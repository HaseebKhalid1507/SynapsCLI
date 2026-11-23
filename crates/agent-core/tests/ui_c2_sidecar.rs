//! The C2 row's own tests of the `.ui` sidecar (`agent-ui/architecture.md`
//! v3.2 §7): a round trip, oversize and malformed files. The crash and
//! fault tests are `ui_c2_exit_gate.rs`.

use agent_core::core::session_surfaces::*;
use agent_core::core::ui_types::*;
use serde_json::json;

const SID: &str = "20261123-120000-a1b2";
const LINEAGE: &str = "0123456789abcdef0123456789abcdef";
const HX: &str = "00112233445566778899aabbccddeeff";
const AT: &str = "2026-11-23T11:00:00+00:00";

fn card(n: u8) -> SurfaceState {
    SurfaceState {
        id: format!("s_{n:016x}"),
        call_id: format!("toolu_{n}"),
        rev: 1,
        contract: format!("c1:{}", "0".repeat(32)),
        catalog: 1,
        mode: WaitMode::Live,
        phase: Phase::Open,
        offers: vec![Verb::Answer, Verb::Skip],
        doc: serde_json::from_value(json!({
            "question": "Which approach?",
            "options": [{"label": "Refactor"}, {"label": "Rewrite"}],
        }))
        .unwrap(),
        origin: Origin {
            turn: None,
            task: "Cut the 0.13 release".into(),
            stopped: None,
        },
        epoch: 0,
        resume: false,
        actions: Vec::new(),
        raised_at: AT.into(),
        downgraded: None,
        answer: None,
    }
}

fn accepted(card: u8, n: u8, note: Option<String>) -> Op {
    let values = Answer {
        choice: Some(Choice::One("Refactor".into())),
        note,
        fields: None,
    };
    Op::Accept(Box::new(Accepted {
        record: LedgerRecord {
            answer_id: format!("a_{n:016x}"),
            card: format!("s_{card:016x}"),
            rev: 1,
            status: AnswerStatus::Answered,
            values: values.clone(),
            wake: true,
            epoch: 0,
            delivery: Delivery::Queued,
            q: "Which approach?".into(),
            asked: None,
        },
        action: LogEntry {
            action_id: format!("act-{n}"),
            hash: action_hash(Verb::Answer, 0, 0, &values),
            rev: 1,
        },
        by: "desk".into(),
        at: AT.into(),
        offers: Vec::new(),
    }))
}

/// An image with something in every part: cards (open and answered), the
/// ledger at each delivery step, a tombstone, the settled mark, a
/// compaction in progress.
fn full_image() -> Sidecar {
    let mut s = Sidecar::new(SID, LINEAGE, ROOT_BASE);
    for n in 1..=3 {
        apply(&mut s, &Op::Create(Box::new(card(n))));
    }
    apply(&mut s, &accepted(1, 1, None));
    apply(&mut s, &accepted(2, 2, Some("and keep the tests".into())));
    apply(
        &mut s,
        &Op::Deliver {
            answer_id: format!("a_{:016x}", 1),
            to: Delivery::InContext,
        },
    );
    apply(
        &mut s,
        &Op::Settle {
            answer_id: format!("a_{:016x}", 1),
        },
    );
    apply(
        &mut s,
        &Op::Close {
            card: format!("s_{:016x}", 3),
        },
    );
    s.seq = 7;
    s.epoch = 2;
    s.compacting = Some(Compacting {
        hx: HX.into(),
        from: HandoffFrom {
            session_id: SID.into(),
            base: ROOT_BASE.into(),
            seq: 7,
        },
        to: HandoffTo {
            session_id: "20261123-120500-c3d4".into(),
            base: ROOT_BASE.into(),
        },
    });
    s
}

/// What is written reads back the same, field for field, and again to the
/// same bytes.
#[test]
fn sidecar_round_trip() {
    let s = full_image();
    assert!(!s.cards.is_empty() && !s.ledger.is_empty(), "{s:?}");
    assert!(s.newest_settled.is_some(), "{s:?}");
    let bytes = encode_sidecar(&s).unwrap();
    let back = decode_sidecar(&bytes).unwrap();
    assert_eq!(back, s);
    assert_eq!(encode_sidecar(&back).unwrap(), bytes);
    assert_eq!(peek_seq(&bytes), Some(7));
    let fs = ModelFs::new();
    fs.put(&sidecar_name(SID), &bytes);
    assert_eq!(load(&*fs, SID).unwrap(), Loaded::Image(Box::new(s)));
}

/// Over its bounds, a sidecar is never written (and never truncated to
/// fit); a file over them reads as unreadable, never as an image, and one
/// past the read bound is not read at all.
#[test]
fn sidecar_oversize() {
    let mut s = full_image();
    let big = "x".repeat(LEDGER_MAX_VALUE_BYTES);
    apply(&mut s, &Op::Create(Box::new(card(4))));
    apply(&mut s, &accepted(4, 4, Some(big)));
    assert!(
        matches!(
            encode_sidecar(&s),
            Err(SidecarError::Bounds(_)) | Err(SidecarError::TooLarge(_))
        ),
        "{s:?}"
    );
    let mut s = Sidecar::new(SID, LINEAGE, ROOT_BASE);
    for n in 0..=(ATTACHED_MAX_STATES as u8) {
        s.cards.push(CardRecord {
            state: card(n),
            log: Vec::new(),
        });
    }
    assert!(matches!(encode_sidecar(&s), Err(SidecarError::Bounds(_))));
    let mut padded = encode_sidecar(&full_image()).unwrap();
    padded.pop();
    padded.extend(std::iter::repeat_n(b' ', SIDECAR_MAX_BYTES));
    padded.push(b'}');
    assert!(matches!(
        decode_sidecar(&padded),
        Err(SidecarError::TooLarge(_))
    ));
    let fs = ModelFs::new();
    fs.put(&sidecar_name(SID), &padded);
    assert!(matches!(load(&*fs, SID).unwrap(), Loaded::Unreadable(_)));
    fs.put(&sidecar_name(SID), &vec![b' '; SIDECAR_READ_MAX_BYTES + 1]);
    assert!(matches!(load(&*fs, SID).unwrap(), Loaded::Unreadable(_)));
}

/// A file that isn't a sidecar of this version, or whose identity fields
/// are out of shape, is unreadable: a class, not an I/O failure.
#[test]
fn sidecar_malformed() {
    let good = serde_json::to_value(full_image()).unwrap();
    let mut cases: Vec<Vec<u8>> = vec![
        b"".to_vec(),
        b"not json".to_vec(),
        b"[]".to_vec(),
        b"{\"v\":1}".to_vec(),
    ];
    for (field, value) in [
        ("v", json!(2)),
        ("lineage", json!("not-hex")),
        ("session_id", json!("../escape")),
        ("base", json!("x".repeat(BASE_MAX_BYTES + 1))),
        ("seq", json!(-1)),
        ("unknown_field", json!(true)),
    ] {
        let mut v = good.clone();
        v[field] = value;
        cases.push(serde_json::to_vec(&v).unwrap());
    }
    let mut v = good.clone();
    v["cards"][1]["state"]["id"] = v["cards"][0]["state"]["id"].clone();
    cases.push(serde_json::to_vec(&v).unwrap());
    let mut v = good;
    v["ledger"][0]["answer_id"] = json!("a_short");
    cases.push(serde_json::to_vec(&v).unwrap());
    let fs = ModelFs::new();
    for (k, bytes) in cases.iter().enumerate() {
        assert!(
            decode_sidecar(bytes).is_err(),
            "case {k} decoded: {}",
            String::from_utf8_lossy(bytes)
        );
        fs.put(&sidecar_name(SID), bytes);
        assert!(
            matches!(load(&*fs, SID).unwrap(), Loaded::Unreadable(_)),
            "case {k}"
        );
    }
}

/// A session's files are deleted sidecar first (compaction rollback, a
/// session's deletion): when the sidecar can't go, its history stays, so a
/// delete cut short never leaves cards without their history.
#[test]
fn sidecar_deleted_first() {
    use agent_core::core::session::Session;
    use agent_core::core::session_journal::{
        delete_session_files_in_dir, journal_path, save_session_in_dir, SessionPersistence,
    };
    let tmp = tempfile::TempDir::new().unwrap();
    let mut s = Session::new("claude-sonnet-4-6", "medium", Some("harness prompt"));
    s.api_messages.push(std::sync::Arc::new(
        json!({"role": "user", "content": "hello"}),
    ));
    save_session_in_dir(tmp.path(), &s, SessionPersistence::Journal).unwrap();
    let snapshot = tmp.path().join(format!("{}.json", s.id));
    let sidecar = tmp.path().join(sidecar_name(&s.id));
    std::fs::write(&sidecar, b"{}").unwrap();
    delete_session_files_in_dir(tmp.path(), &s.id).unwrap();
    assert!(!sidecar.exists() && !snapshot.exists());
    assert!(!journal_path(tmp.path(), &s.id).exists());
    // The sidecar can't be unlinked (a directory in its place): the delete
    // fails before it touches the history.
    save_session_in_dir(tmp.path(), &s, SessionPersistence::Journal).unwrap();
    std::fs::create_dir(&sidecar).unwrap();
    std::fs::write(sidecar.join("keep"), b"x").unwrap();
    assert!(delete_session_files_in_dir(tmp.path(), &s.id).is_err());
    assert!(snapshot.exists() || journal_path(tmp.path(), &s.id).exists());
    std::fs::remove_dir_all(&sidecar).unwrap();
    delete_session_files_in_dir(tmp.path(), &s.id).unwrap();
    assert!(!snapshot.exists() && !journal_path(tmp.path(), &s.id).exists());
}
