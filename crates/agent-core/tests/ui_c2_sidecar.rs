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
    accepted_at(card, n, note, AT, Vec::new())
}

fn accepted_at(card: u8, n: u8, note: Option<String>, at: &str, offers: Vec<Verb>) -> Op {
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
        at: at.into(),
        offers,
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

/// T20 and eviction (§4.4): revisable receipts beyond the newest eight, or
/// answered more than a day ago, are made final and keep their ledger
/// records; only a final card whose answers are settled leaves for a
/// tombstone, and only when the cards are over what `Attached.surfaces`
/// carries.
#[test]
fn retained_limits_close_then_evict() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-11-23T12:00:00+00:00")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let mut s = Sidecar::new(SID, LINEAGE, ROOT_BASE);
    // Ten revisable receipts answered n minutes ago (n = 1..=10), and one
    // answered 30 h ago.
    for n in 1..=11u8 {
        apply(&mut s, &Op::Create(Box::new(card(n))));
        let at = if n == 11 {
            now - chrono::Duration::hours(30)
        } else {
            now - chrono::Duration::minutes(n as i64)
        };
        apply(
            &mut s,
            &accepted_at(n, n, None, &at.to_rfc3339(), vec![Verb::Revise]),
        );
    }
    for n in 1..=11u8 {
        apply(
            &mut s,
            &Op::Settle {
                answer_id: format!("a_{n:016x}"),
            },
        );
    }
    let ledger = s.ledger.clone();
    enforce_limits(&mut s, now);
    let phase = |s: &Sidecar, n: u8| s.card(&format!("s_{n:016x}")).map(|c| c.phase);
    // The oldest three (30 h, 10 min, 9 min) are final; the newest eight
    // stay revisable.
    for n in [11, 10, 9] {
        assert_eq!(phase(&s, n), Some(Phase::Closed), "card {n}");
        assert!(s.card(&format!("s_{n:016x}")).unwrap().offers.is_empty());
    }
    for n in 1..=8 {
        assert_eq!(phase(&s, n), Some(Phase::Answered), "card {n}");
    }
    assert_eq!(s.ledger, ledger, "T20 never touches the ledger");
    assert!(s.tombstones.is_empty(), "eleven cards fit: {s:?}");
    // Two more open cards: thirteen. The oldest final, settled card (30 h)
    // leaves for a tombstone with its log; nothing revisable or open does.
    for n in 12..=13u8 {
        apply(&mut s, &Op::Create(Box::new(card(n))));
    }
    enforce_limits(&mut s, now);
    assert_eq!(s.cards.len(), ATTACHED_MAX_STATES);
    assert_eq!(phase(&s, 11), None);
    assert_eq!(s.tombstones.len(), 1);
    assert_eq!(s.tombstones[0].id, format!("s_{:016x}", 11));
    assert_eq!(s.tombstones[0].log.len(), 1);
    assert!(encode_sidecar(&s).is_ok());
    // An unsettled final card stays a card: the ledger's card is never
    // evicted.
    let mut t = s.clone();
    t.ledger
        .iter_mut()
        .for_each(|r| r.delivery = Delivery::Queued);
    apply(&mut t, &Op::Create(Box::new(card(14))));
    enforce_limits(&mut t, now);
    assert_eq!(
        t.cards.len(),
        ATTACHED_MAX_STATES + 1,
        "nothing settled and final to evict"
    );
}

mod retention {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, UNIX_EPOCH};

    use agent_core::core::retention::{sweep_at, RetentionPolicy, RetentionRoots};
    use agent_core::core::session::Session;
    use agent_core::core::session_journal::{save_session_in_dir, SessionPersistence};

    const DEAD: &str = "20200101-000000-dead";
    const LIVE: &str = "20990101-000000-live";
    const NOW_MS: u64 = 1_800_000_000_000; // 2027

    fn roots() -> (tempfile::TempDir, RetentionRoots, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let roots = RetentionRoots {
            config_dir: tmp.path().join("config"),
            base_dir: tmp.path().join("base"),
            cache_dir: tmp.path().join("cache"),
        };
        for d in [
            roots.config_dir.join("sessions"),
            roots.config_dir.join("chains"),
            roots.base_dir.join("memory"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::create_dir_all(&roots.cache_dir).unwrap();
        let sessions = roots.config_dir.join("sessions");
        (tmp, roots, sessions)
    }

    fn saved(dir: &Path, id: &str, updated_at: &str) {
        let mut s = Session::new("claude-sonnet-4-6", "medium", Some("harness prompt"));
        s.id = id.to_string();
        s.updated_at = chrono::DateTime::parse_from_rfc3339(updated_at)
            .unwrap()
            .with_timezone(&chrono::Utc);
        save_session_in_dir(dir, &s, SessionPersistence::Json).unwrap();
    }

    /// A file last written in 2020.
    fn old_file(path: &Path) {
        std::fs::write(path, b"{\"v\":1}").unwrap();
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(UNIX_EPOCH + Duration::from_secs(1_577_836_800))
            .unwrap();
    }

    fn age_policy() -> RetentionPolicy {
        RetentionPolicy {
            max_age_days: Some(30),
            max_disk_bytes: None,
            include_notes: false,
        }
    }

    /// A sidecar ages with its session's snapshot, not by its own mtime: a
    /// live session's untouched sidecar stays, a swept session's sidecar
    /// and quarantine go with it. One with no snapshot ages by its mtime.
    #[test]
    fn retention_ages_the_sidecar_with_its_session() {
        let (_tmp, roots, sessions) = roots();
        saved(&sessions, DEAD, "2020-01-01T00:00:00Z");
        saved(&sessions, LIVE, "2099-01-01T00:00:00Z");
        for f in [
            format!("{DEAD}.ui"),
            format!("{DEAD}.ui.quarantine"),
            format!("{LIVE}.ui"),
            "20200101-000000-orph.ui".into(),
        ] {
            old_file(&sessions.join(f));
        }
        sweep_at(&roots, &age_policy(), NOW_MS).unwrap();
        assert!(!sessions.join(format!("{DEAD}.json")).exists());
        assert!(!sessions.join(format!("{DEAD}.ui")).exists());
        assert!(!sessions.join(format!("{DEAD}.ui.quarantine")).exists());
        assert!(sessions.join(format!("{LIVE}.json")).exists());
        assert!(
            sessions.join(format!("{LIVE}.ui")).exists(),
            "a live session's sidecar stays, however old its file"
        );
        assert!(!sessions.join("20200101-000000-orph.ui").exists());
    }

    /// The sidecar goes first: when it can't be unlinked (a directory in its
    /// place), the sweep stops with the session's snapshot still there.
    #[test]
    fn retention_unlinks_the_sidecar_first() {
        let (_tmp, roots, sessions) = roots();
        saved(&sessions, DEAD, "2020-01-01T00:00:00Z");
        let sidecar = sessions.join(format!("{DEAD}.ui"));
        std::fs::create_dir(&sidecar).unwrap();
        std::fs::write(sidecar.join("keep"), b"x").unwrap();
        assert!(sweep_at(&roots, &age_policy(), NOW_MS).is_err());
        assert!(sessions.join(format!("{DEAD}.json")).exists());
    }

    /// The disk budget evicts a snapshot with its sidecar (first), and never
    /// a sidecar on its own while its snapshot stays.
    #[test]
    fn retention_budget_takes_the_sidecar_with_its_snapshot() {
        let (_tmp, roots, sessions) = roots();
        saved(&sessions, DEAD, "2020-01-01T00:00:00Z");
        saved(&sessions, LIVE, "2099-01-01T00:00:00Z");
        for f in [format!("{DEAD}.ui"), format!("{LIVE}.ui")] {
            old_file(&sessions.join(f));
        }
        let size = |f: String| std::fs::metadata(sessions.join(f)).unwrap().len();
        let total: u64 = std::fs::read_dir(&sessions)
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum();
        let budget = total - size(format!("{DEAD}.json")) - size(format!("{DEAD}.ui"));
        let policy = RetentionPolicy {
            max_age_days: None,
            max_disk_bytes: Some(budget),
            include_notes: false,
        };
        sweep_at(&roots, &policy, NOW_MS).unwrap();
        assert!(!sessions.join(format!("{DEAD}.json")).exists());
        assert!(!sessions.join(format!("{DEAD}.ui")).exists());
        assert!(sessions.join(format!("{LIVE}.json")).exists());
        assert!(sessions.join(format!("{LIVE}.ui")).exists());
    }
}
