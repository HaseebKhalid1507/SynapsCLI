use super::*;
use serde_json::json;

fn meta() -> SubagentArchiveMeta {
    SubagentArchiveMeta {
        agent_name: "worker".into(),
        model: "provider/model-b".into(),
        system_prompt: "You are a worker.".into(),
        timeout_secs: 300,
        cwd: None,
        thinking: None,
        watch_usage: None,
    }
}

fn binding() -> Binding {
    Binding {
        backend: BackendKind::Legacy,
        scope_digest: scope(0xab),
    }
}

fn scope(byte: u8) -> String {
    format!("sha256:{}", format!("{byte:02x}").repeat(32))
}

fn archive_id(n: u8) -> ArchiveId {
    ArchiveId::parse(&format!("{n:02x}").repeat(16)).unwrap()
}

fn record(run: &RunId, kind: RecordKind, generation: u64) -> WorkerRecord {
    let history = vec![
        json!({"role": "user", "content": format!("task at generation {generation}")}),
        json!({"role": "assistant", "content": "working"}),
    ];
    WorkerRecord {
        version: RECORD_VERSION,
        run_id: run.clone(),
        handle_id: "sa_7".into(),
        kind,
        generation,
        history_digest: HistoryDigest::of(&history).unwrap(),
        binding: binding(),
        predecessor: None,
        windows: (1..=generation)
            .map(|w| WindowRef {
                window: w,
                archive_id: archive_id(w as u8),
            })
            .collect(),
        inherited_windows: Vec::new(),
        window_ordinal: generation + 1,
        rollovers_this_run: generation,
        ledger: Ledger::default(),
        meta: meta().into(),
        status: "completed".into(),
        output: "done".into(),
        history,
    }
}

fn expect() -> Expectations {
    Expectations {
        handle_id: "sa_7".into(),
        run_id: None,
        binding: None,
    }
}

fn input(head: Candidate<WorkerRecord>, final_record: Candidate<FinalRecord>) -> ResolveInput {
    ResolveInput {
        head,
        final_record,
        writer: WriterState::Quiescent,
        expect: expect(),
    }
}

fn v2_final(r: WorkerRecord) -> Candidate<FinalRecord> {
    Candidate::Present(FinalRecord::V2(Box::new(r)))
}

fn v1_bytes() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "version": 1,
        "handle_id": "sa_7",
        "meta": meta(),
        "status": "completed",
        "output": "legacy output",
        "history": [{"role": "user", "content": "legacy task"}],
    }))
    .unwrap()
}

fn unresolved(resolution: Resolution) -> Unresolved {
    match resolution {
        Resolution::Unresolved(reason) => reason,
        other => panic!("expected unresolved, got {other:?}"),
    }
}

fn authoritative(resolution: Resolution) -> (RecordKind, WorkerRecord, bool) {
    match resolution {
        Resolution::Authoritative {
            kind,
            record,
            loss_warning,
        } => (kind, *record, loss_warning),
        other => panic!("expected an authoritative record, got {other:?}"),
    }
}

#[test]
fn r1_v2_round_trip_and_v1_legacy() {
    let run = RunId::random();
    let original = record(&run, RecordKind::Final, 2);
    let bytes = serde_json::to_vec(&original).unwrap();
    let FinalRecord::V2(parsed) = parse_final(&bytes).unwrap() else {
        panic!("v2 bytes must parse as v2");
    };
    assert_eq!(*parsed, original);
    assert_eq!(parse_head(&bytes).unwrap(), original);

    let legacy = parse_final(&v1_bytes()).unwrap();
    assert!(matches!(legacy, FinalRecord::V1(_)));
    match resolve(input(Candidate::Absent, Candidate::Present(legacy))) {
        Resolution::Legacy(archive) => assert_eq!(archive.output, "legacy output"),
        other => panic!("v1 final must resolve as legacy, got {other:?}"),
    }
    assert_eq!(
        parse_head(&v1_bytes()).unwrap_err(),
        RecordError::UnsupportedVersion(1),
        "a head is never a v1 archive"
    );
    assert_eq!(
        parse_final(br#"{"version":3}"#).unwrap_err(),
        RecordError::UnsupportedVersion(3)
    );
}

#[test]
fn r2_final_supersedes_only_le_generation() {
    let run = RunId::random();
    for head_generation in [0, 3] {
        let resolution = resolve(input(
            Candidate::Present(record(&run, RecordKind::Head, head_generation)),
            v2_final(record(&run, RecordKind::Final, 3)),
        ));
        let (kind, chosen, warning) = authoritative(resolution);
        assert_eq!(kind, RecordKind::Final);
        assert_eq!(chosen.generation, 3);
        assert!(!warning);
    }
}

#[test]
fn r3_newer_head_than_final_unresolved() {
    let run = RunId::random();
    let reason = unresolved(resolve(input(
        Candidate::Present(record(&run, RecordKind::Head, 4)),
        v2_final(record(&run, RecordKind::Final, 3)),
    )));
    assert_eq!(
        reason,
        Unresolved::HeadNewerThanFinal {
            head: 4,
            final_generation: 3
        }
    );
}

#[test]
fn r4_head_only_crash_checkpoint_warns() {
    let run = RunId::random();
    let (kind, chosen, warning) = authoritative(resolve(input(
        Candidate::Present(record(&run, RecordKind::Head, 2)),
        Candidate::Absent,
    )));
    assert_eq!(kind, RecordKind::Head);
    assert_eq!(chosen.generation, 2);
    assert!(warning, "a head is a crash checkpoint");
}

#[test]
fn r5_final_only_resolves_and_nothing_is_not_found() {
    let run = RunId::random();
    let (kind, _, warning) = authoritative(resolve(input(
        Candidate::Absent,
        v2_final(record(&run, RecordKind::Final, 0)),
    )));
    assert_eq!(kind, RecordKind::Final);
    assert!(!warning);
    assert!(matches!(
        resolve(input(Candidate::Absent, Candidate::Absent)),
        Resolution::NotFound
    ));
}

#[test]
fn r6_mismatched_run_or_handle_unresolved() {
    let reason = unresolved(resolve(input(
        Candidate::Present(record(&RunId::random(), RecordKind::Head, 1)),
        v2_final(record(&RunId::random(), RecordKind::Final, 1)),
    )));
    assert_eq!(reason, Unresolved::RunIdMismatch);

    let run = RunId::random();
    let mut other = expect();
    other.run_id = Some(RunId::random());
    let reason = unresolved(resolve(ResolveInput {
        expect: other,
        ..input(
            Candidate::Absent,
            v2_final(record(&run, RecordKind::Final, 1)),
        )
    }));
    assert_eq!(reason, Unresolved::RunIdMismatch);

    let mut head = record(&run, RecordKind::Head, 1);
    head.handle_id = "sa_8".into();
    let reason = unresolved(resolve(input(Candidate::Present(head), Candidate::Absent)));
    assert_eq!(reason, Unresolved::HandleMismatch);

    // A legacy archive cannot satisfy an expected v2 run.
    let mut wants_run = expect();
    wants_run.run_id = Some(run);
    let reason = unresolved(resolve(ResolveInput {
        expect: wants_run,
        ..input(
            Candidate::Absent,
            Candidate::Present(parse_final(&v1_bytes()).unwrap()),
        )
    }));
    assert_eq!(reason, Unresolved::RunIdMismatch);
}

#[test]
fn r7_digest_kind_or_version_mismatch_unresolved() {
    let run = RunId::random();
    let mut tampered = record(&run, RecordKind::Final, 1);
    tampered
        .history
        .push(json!({"role": "user", "content": "injected"}));
    let reason = unresolved(resolve(input(Candidate::Absent, v2_final(tampered))));
    assert_eq!(
        reason,
        Unresolved::Invalid {
            kind: RecordKind::Final,
            error: RecordError::DigestMismatch
        }
    );

    // A final record found where the head belongs.
    let reason = unresolved(resolve(input(
        Candidate::Present(record(&run, RecordKind::Final, 1)),
        Candidate::Absent,
    )));
    assert_eq!(
        reason,
        Unresolved::Invalid {
            kind: RecordKind::Head,
            error: RecordError::KindMismatch {
                expected: RecordKind::Head,
                found: RecordKind::Final
            }
        }
    );

    let mut wrong_version = record(&run, RecordKind::Head, 1);
    wrong_version.version = 1;
    assert!(matches!(
        unresolved(resolve(input(
            Candidate::Present(wrong_version),
            Candidate::Absent
        ))),
        Unresolved::Invalid {
            error: RecordError::UnsupportedVersion(1),
            ..
        }
    ));

    let mut duplicate = record(&run, RecordKind::Head, 2);
    duplicate.windows[1].window = 1;
    assert!(matches!(
        unresolved(resolve(input(
            Candidate::Present(duplicate),
            Candidate::Absent
        ))),
        Unresolved::Invalid {
            error: RecordError::DuplicateWindow(1),
            ..
        }
    ));

    let mut zero = record(&run, RecordKind::Head, 1);
    zero.windows[0].window = 0;
    assert!(matches!(
        unresolved(resolve(input(Candidate::Present(zero), Candidate::Absent))),
        Unresolved::Invalid {
            error: RecordError::InvalidWindow,
            ..
        }
    ));
}

#[test]
fn r8_writer_may_publish_unresolved() {
    let run = RunId::random();
    for head in [
        Candidate::Absent,
        Candidate::Present(record(&run, RecordKind::Head, 1)),
    ] {
        let reason = unresolved(resolve(ResolveInput {
            writer: WriterState::MayPublish,
            ..input(head, v2_final(record(&run, RecordKind::Final, 1)))
        }));
        assert_eq!(reason, Unresolved::WriterMayPublish);
    }
}

#[test]
fn r9_unreadable_candidates_unresolved() {
    let run = RunId::random();
    let reason = unresolved(resolve(input(
        Candidate::Unreadable("truncated".into()),
        v2_final(record(&run, RecordKind::Final, 5)),
    )));
    assert_eq!(reason, Unresolved::HeadUnreadable("truncated".into()));

    let reason = unresolved(resolve(input(
        Candidate::Present(record(&run, RecordKind::Head, 1)),
        Candidate::Unreadable("bad json".into()),
    )));
    assert_eq!(reason, Unresolved::FinalUnreadable("bad json".into()));
}

#[test]
fn r10_v1_final_with_v2_head_unresolved() {
    let reason = unresolved(resolve(input(
        Candidate::Present(record(&RunId::random(), RecordKind::Head, 1)),
        Candidate::Present(parse_final(&v1_bytes()).unwrap()),
    )));
    assert_eq!(reason, Unresolved::MixedVersions);
}

fn run_id(byte: u8) -> RunId {
    RunId::parse(&format!("{byte:02x}").repeat(16)).unwrap()
}

/// A resumed run: two predecessor runs sealed ordinals 1–3, this run sealed
/// `own` more, and its current window is the next ordinal.
fn chained(kind: RecordKind, own: u64) -> WorkerRecord {
    let mut r = record(&run_id(0x33), kind, own);
    r.predecessor = Some(Predecessor {
        run_id: run_id(0x22),
        kind: RecordKind::Final,
        generation: 0,
        digest: r.history_digest.clone(),
    });
    r.inherited_windows = vec![
        InheritedRef {
            run_id: run_id(0x11),
            window: 1,
            archive_id: archive_id(101),
        },
        InheritedRef {
            run_id: run_id(0x11),
            window: 2,
            archive_id: archive_id(102),
        },
        InheritedRef {
            run_id: run_id(0x22),
            window: 3,
            archive_id: archive_id(103),
        },
    ];
    r.windows = (1..=own)
        .map(|i| WindowRef {
            window: 3 + i,
            archive_id: archive_id(i as u8),
        })
        .collect();
    r.window_ordinal = 4 + own;
    r
}

fn lineage_rejected(r: &WorkerRecord, want: RecordError, why: &str) {
    assert_eq!(r.validate(r.kind), Err(want), "{why}");
}

fn contradiction(e: LineageError) -> RecordError {
    RecordError::Lineage(e)
}

#[test]
fn r11_lineage_cap_and_canonical_mapping() {
    assert_eq!(
        chained(RecordKind::Head, 2).validate(RecordKind::Head),
        Ok(())
    );

    // Exactly at the cap: 127 inherited windows plus one own window.
    let mut r = chained(RecordKind::Head, 1);
    r.inherited_windows = (1..MAX_LINEAGE_REFS as u64)
        .map(|w| InheritedRef {
            run_id: run_id(0x22),
            window: w,
            archive_id: archive_id(w as u8 + 100),
        })
        .collect();
    r.windows = vec![WindowRef {
        window: MAX_LINEAGE_REFS as u64,
        archive_id: archive_id(1),
    }];
    r.window_ordinal = MAX_LINEAGE_REFS as u64 + 1;
    assert_eq!(r.validate(RecordKind::Head), Ok(()));

    // One more sealed window exceeds it.
    r.windows.push(WindowRef {
        window: MAX_LINEAGE_REFS as u64 + 1,
        archive_id: archive_id(2),
    });
    r.rollovers_this_run = 2;
    r.window_ordinal += 1;
    let over = RecordError::LineageCapExceeded(MAX_LINEAGE_REFS + 1);
    assert_eq!(r.validate(RecordKind::Head), Err(over.clone()));
    let reason = unresolved(resolve(input(Candidate::Present(r), Candidate::Absent)));
    assert_eq!(
        reason,
        Unresolved::Invalid {
            kind: RecordKind::Head,
            error: over
        }
    );

    // The physical vectors are bounded, not only the distinct IDs.
    let mut r = chained(RecordKind::Head, 0);
    let copy = r.inherited_windows[0].clone();
    r.inherited_windows = vec![copy; MAX_LINEAGE_REFS + 1];
    lineage_rejected(
        &r,
        RecordError::LineageCapExceeded(MAX_LINEAGE_REFS + 1),
        "repeated physical refs",
    );
}

#[test]
fn lineage_duplicates_and_conflicts_rejected() {
    // Exact duplicate of an inherited ref.
    let mut r = chained(RecordKind::Head, 1);
    let copy = r.inherited_windows[0].clone();
    r.inherited_windows.insert(1, copy);
    lineage_rejected(
        &r,
        RecordError::DuplicateWindow(1),
        "exact inherited duplicate",
    );

    // One archive under two ordinals.
    let mut r = chained(RecordKind::Head, 2);
    r.windows[1].archive_id = r.windows[0].archive_id.clone();
    lineage_rejected(
        &r,
        contradiction(LineageError::DuplicateArchive(archive_id(1))),
        "archive at two own ordinals",
    );

    // One archive owned by this run and by an inherited run.
    let mut r = chained(RecordKind::Head, 1);
    r.windows[0].archive_id = r.inherited_windows[2].archive_id.clone();
    lineage_rejected(
        &r,
        contradiction(LineageError::DuplicateArchive(archive_id(103))),
        "own archive attributed to another run",
    );

    // Inherited archive attributed to two runs.
    let mut r = chained(RecordKind::Head, 1);
    r.inherited_windows[2].archive_id = r.inherited_windows[1].archive_id.clone();
    lineage_rejected(
        &r,
        contradiction(LineageError::DuplicateArchive(archive_id(102))),
        "inherited archive under two owners",
    );

    // One ordinal both inherited and own.
    let mut r = chained(RecordKind::Head, 1);
    r.windows[0].window = 3;
    lineage_rejected(
        &r,
        RecordError::DuplicateWindow(3),
        "ordinal both inherited and own",
    );
}

#[test]
fn lineage_self_and_predecessor_contradictions_rejected() {
    let mut r = chained(RecordKind::Head, 1);
    r.inherited_windows[2].run_id = r.run_id.clone();
    lineage_rejected(
        &r,
        contradiction(LineageError::SelfReference),
        "own run listed as inherited",
    );

    let mut r = chained(RecordKind::Head, 1);
    r.predecessor.as_mut().unwrap().run_id = r.run_id.clone();
    lineage_rejected(
        &r,
        contradiction(LineageError::SelfReference),
        "own run as predecessor",
    );

    let mut r = chained(RecordKind::Head, 1);
    r.predecessor = None;
    lineage_rejected(
        &r,
        contradiction(LineageError::MissingPredecessor),
        "inherited windows without a predecessor",
    );

    // A run of the chain cannot reappear after a later run's windows.
    let mut r = chained(RecordKind::Head, 1);
    r.inherited_windows[0].run_id = run_id(0x22);
    r.inherited_windows[1].run_id = run_id(0x11);
    lineage_rejected(
        &r,
        contradiction(LineageError::ChainOrder),
        "interleaved chain runs",
    );
}

#[test]
fn lineage_ordinals_and_counters_are_consistent() {
    let mut r = chained(RecordKind::Head, 2);
    r.rollovers_this_run = 1;
    lineage_rejected(
        &r,
        contradiction(LineageError::RolloverCount),
        "rollovers_this_run differs from own windows",
    );

    let mut r = chained(RecordKind::Head, 2);
    r.window_ordinal = 9;
    lineage_rejected(
        &r,
        contradiction(LineageError::Ordinal),
        "window_ordinal is not the next ordinal",
    );

    let mut r = chained(RecordKind::Head, 2);
    r.windows[1].window = 7;
    r.window_ordinal = 8;
    lineage_rejected(
        &r,
        contradiction(LineageError::OutOfOrder(7)),
        "gap between own ordinals",
    );

    let mut r = chained(RecordKind::Head, 2);
    r.windows.swap(0, 1);
    lineage_rejected(
        &r,
        contradiction(LineageError::OutOfOrder(5)),
        "own windows out of order",
    );

    let mut r = chained(RecordKind::Head, 1);
    r.inherited_windows.swap(0, 1);
    lineage_rejected(
        &r,
        contradiction(LineageError::OutOfOrder(2)),
        "inherited windows out of order",
    );

    let mut r = chained(RecordKind::Head, 1);
    r.inherited_windows.remove(0);
    lineage_rejected(
        &r,
        contradiction(LineageError::OutOfOrder(2)),
        "inherited ordinals must start at 1",
    );
}

fn ledger_entry(id: &str, amount: Amount) -> LedgerEntry {
    LedgerEntry {
        request_id: RequestId::parse(id).unwrap(),
        purpose: RequestPurpose::Conversation,
        model: "provider/model-c".into(),
        amount,
    }
}

fn measured(input_tokens: u64) -> Amount {
    Amount::Measured {
        usage: MeasuredUsage {
            input_tokens,
            ..Default::default()
        },
    }
}

fn held(estimate_micro_usd: u64) -> Amount {
    Amount::Liability { estimate_micro_usd }
}

#[test]
fn ledger_release_cannot_change_the_held_amount() {
    let mut ledger = Ledger::default();
    ledger.settle(ledger_entry("req-1", held(10))).unwrap();
    let before = ledger.clone();
    let conflicting = serde_json::from_value::<LedgerEntry>(json!({
        "request_id": "req-1",
        "purpose": "conversation",
        "model": "provider/model-c",
        "amount": {"state": "released", "estimate_micro_usd": 999},
    }));
    assert!(conflicting.is_err(), "a release carries no amount");
    assert_eq!(ledger, before);
}

#[test]
fn ledger_release_lifecycle() {
    let released = Amount::Released {};
    let mut ledger = Ledger::default();
    assert_eq!(
        ledger.settle(ledger_entry("req-0", released.clone())),
        Err(SettleError::Conflict),
        "nothing to release"
    );
    assert!(ledger.is_empty());

    ledger.settle(ledger_entry("req-1", held(10))).unwrap();
    assert_eq!(
        ledger.settle(ledger_entry("req-1", released.clone())),
        Ok(Settled::Transitioned)
    );
    assert_eq!(
        ledger.settle(ledger_entry("req-1", released.clone())),
        Ok(Settled::Duplicate)
    );
    let before = ledger.clone();
    for resurrect in [held(10), measured(3)] {
        assert_eq!(
            ledger.settle(ledger_entry("req-1", resurrect)),
            Err(SettleError::Conflict),
            "released is terminal"
        );
        assert_eq!(ledger, before);
    }
    ledger.settle(ledger_entry("req-2", measured(3))).unwrap();
    let before = ledger.clone();
    assert_eq!(
        ledger.settle(ledger_entry("req-2", released)),
        Err(SettleError::Conflict),
        "measured is terminal"
    );
    assert_eq!(ledger, before);

    let bytes = serde_json::to_value(&ledger).unwrap();
    assert_eq!(bytes[0]["amount"], json!({"state": "released"}));
    assert_eq!(serde_json::from_value::<Ledger>(bytes).unwrap(), ledger);
}

/// The `c1` body of one value (after the domain and the one-item array header).
fn c1(value: Value) -> Vec<u8> {
    let bytes = canonical_history_bytes(&[value]).unwrap();
    let header = C1_DOMAIN.len() + 9;
    assert_eq!(&bytes[..C1_DOMAIN.len()], C1_DOMAIN);
    assert_eq!(&bytes[C1_DOMAIN.len()..header], b"a\0\0\0\0\0\0\0\x01");
    bytes[header..].to_vec()
}

fn tagged(tag: u8, payload: [u8; 8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend_from_slice(&payload);
    out
}

#[test]
fn c1_golden_vectors() {
    // Hand-derived bytes, independent of the encoder.
    assert_eq!(
        canonical_history_bytes(&[]).unwrap(),
        b"synaps.worker-history.c1\0a\0\0\0\0\0\0\0\0"
    );
    assert_eq!(c1(json!(null)), b"n");
    assert_eq!(c1(json!(true)), b"t");
    assert_eq!(c1(json!(false)), b"f");
    assert_eq!(c1(json!(1)), tagged(b'u', 1u64.to_be_bytes()));
    assert_eq!(c1(json!(1.0)), tagged(b'd', [0x3f, 0xf0, 0, 0, 0, 0, 0, 0]));
    assert_eq!(c1(json!(0.0)), tagged(b'd', [0; 8]));
    assert_eq!(c1(json!(-0.0)), tagged(b'd', [0x80, 0, 0, 0, 0, 0, 0, 0]));
    assert_eq!(c1(json!(-1)), tagged(b'i', [0xff; 8]));
    assert_eq!(c1(json!(u64::MAX)), tagged(b'u', [0xff; 8]));
    assert_eq!(c1(json!(i64::MAX)), tagged(b'u', i64::MAX.to_be_bytes()));
    assert_eq!(
        c1(json!(i64::MIN)),
        tagged(b'i', [0x80, 0, 0, 0, 0, 0, 0, 0])
    );
    assert_eq!(
        c1(json!(f64::MAX)),
        tagged(b'd', [0x7f, 0xef, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff])
    );
    assert_eq!(c1(json!(5e-324)), tagged(b'd', [0, 0, 0, 0, 0, 0, 0, 1]));
    assert_eq!(
        c1(json!(f64::MIN_POSITIVE)),
        tagged(b'd', [0, 0x10, 0, 0, 0, 0, 0, 0])
    );
    // Strings are length-prefixed UTF-8, verbatim: no escaping.
    assert_eq!(
        c1(json!("\u{e9}\"\n\u{1F600}")),
        b"s\0\0\0\0\0\0\0\x08\xc3\xa9\"\n\xf0\x9f\x98\x80"
    );
    // Keys sort by UTF-8 bytes at every depth: U+E000 before U+1F600
    // (UTF-16 order would differ), "a" before "b".
    assert_eq!(
        c1(json!({"\u{1F600}": [], "\u{e000}": {"b": null, "a": null}})),
        [
            &b"o\0\0\0\0\0\0\0\x02"[..],
            b"\0\0\0\0\0\0\0\x03\xee\x80\x80",
            b"o\0\0\0\0\0\0\0\x02",
            b"\0\0\0\0\0\0\0\x01an",
            b"\0\0\0\0\0\0\0\x01bn",
            b"\0\0\0\0\0\0\0\x04\xf0\x9f\x98\x80",
            b"a\0\0\0\0\0\0\0\0",
        ]
        .concat()
    );
}

fn golden_history() -> Vec<Value> {
    vec![
        json!({
            "role": "user",
            "content": "\u{e9}\"\n\u{1F600}",
            "n": [1, 1.0, -0.0, 0.0, -1, u64::MAX, i64::MIN, f64::MAX, 5e-324,
                  f64::MIN_POSITIVE, null, true, false],
        }),
        json!({"b": {"z": 1, "a": 2}, "a": "x", "\u{1F600}": 1, "\u{e000}": 2}),
    ]
}

#[test]
fn history_digest_golden_and_stable() {
    // Expected values computed outside Rust from the specified encoding.
    assert_eq!(
        HistoryDigest::of(&[]).unwrap().as_str(),
        "sha256-c1:ca2e7834f45060180817140c5d9a1cce2cf9f9dc08b2def6b2aefe7b76f0a87a"
    );
    let history = golden_history();
    let digest = HistoryDigest::of(&history).unwrap();
    assert_eq!(
        digest.as_str(),
        "sha256-c1:3a92d107a44becfb5b71c33d5ecc5cf5134c7df8ae9f01cdfe046a8586520839"
    );
    // Through JSON text and back (the record round trip) nothing changes.
    let text = serde_json::to_vec(&history).unwrap();
    let reparsed: Vec<Value> = serde_json::from_slice(&text).unwrap();
    assert_eq!(HistoryDigest::of(&reparsed).unwrap(), digest);
    // Key insertion order is irrelevant; number kinds and signs are not.
    let mut forward = serde_json::Map::new();
    forward.insert("a".into(), json!(1));
    forward.insert("b".into(), json!(2));
    let mut backward = serde_json::Map::new();
    backward.insert("b".into(), json!(2));
    backward.insert("a".into(), json!(1));
    assert_eq!(
        HistoryDigest::of(&[Value::Object(forward)]).unwrap(),
        HistoryDigest::of(&[Value::Object(backward)]).unwrap()
    );
    let of = |v: Value| HistoryDigest::of(&[v]).unwrap();
    assert_ne!(of(json!(1)), of(json!(1.0)));
    assert_ne!(of(json!(0.0)), of(json!(-0.0)));
    assert_ne!(of(json!("a")), of(json!(["a"])));
    // An integer beyond u64 is a binary64 number once parsed.
    let big: Value = serde_json::from_str("18446744073709551616").unwrap();
    assert_eq!(c1(big), tagged(b'd', 18446744073709551616f64.to_be_bytes()));
    // Depth is bounded.
    let mut deep = json!(null);
    for _ in 0..C1_MAX_DEPTH {
        deep = json!([deep]);
    }
    assert_eq!(
        HistoryDigest::of(&[deep]),
        Err(RecordError::HistoryNotEncodable)
    );
    assert!(HistoryDigest::parse(&format!("sha256:{}", "0".repeat(64))).is_err());
}

#[test]
fn ledger_cap_boundary() {
    let mut ledger = Ledger::default();
    for i in 0..Ledger::MAX_ENTRIES {
        let entry = ledger_entry(&format!("req-{i}"), held(1));
        assert_eq!(ledger.settle(entry), Ok(Settled::Recorded));
    }
    assert_eq!(ledger.len(), Ledger::MAX_ENTRIES);
    let full = ledger.clone();
    assert_eq!(
        ledger.settle(ledger_entry("req-new", measured(1))),
        Err(SettleError::Full)
    );
    assert_eq!(ledger, full, "a rejected entry changes nothing");

    // Redelivery and settlement of existing entries still work at capacity.
    assert_eq!(
        ledger.settle(ledger_entry("req-0", held(1))),
        Ok(Settled::Duplicate)
    );
    assert_eq!(
        ledger.settle(ledger_entry("req-0", measured(5))),
        Ok(Settled::Transitioned)
    );
    assert_eq!(ledger.len(), Ledger::MAX_ENTRIES);

    // Snapshots: exactly the cap parses; one more, or a repeated id, does not.
    let bytes = serde_json::to_vec(&ledger).unwrap();
    assert_eq!(serde_json::from_slice::<Ledger>(&bytes).unwrap(), ledger);
    let mut list: Vec<LedgerEntry> = ledger.clone().into();
    list.push(ledger_entry("req-new", measured(1)));
    let over = serde_json::to_vec(&list).unwrap();
    assert!(serde_json::from_slice::<Ledger>(&over).is_err());
    let mut list: Vec<LedgerEntry> = ledger.into();
    list.truncate(2);
    list[1].request_id = list[0].request_id.clone();
    let doubled = serde_json::to_vec(&list).unwrap();
    assert!(serde_json::from_slice::<Ledger>(&doubled).is_err());
}

#[test]
fn ledger_rejected_updates_never_mutate() {
    let mut ledger = Ledger::default();
    ledger.settle(ledger_entry("req-1", held(10))).unwrap();
    ledger.settle(ledger_entry("req-2", measured(7))).unwrap();
    let before = ledger.clone();
    let mut other_model = ledger_entry("req-1", measured(1));
    other_model.model = "provider/model-d".into();
    let mut other_purpose = ledger_entry("req-1", measured(1));
    other_purpose.purpose = RequestPurpose::RolloverSummary;
    for (entry, want) in [
        (other_model, SettleError::IdentityMismatch),
        (other_purpose, SettleError::IdentityMismatch),
        (ledger_entry("req-1", held(11)), SettleError::Conflict),
        (ledger_entry("req-2", measured(8)), SettleError::Conflict),
        (ledger_entry("req-2", held(1)), SettleError::Conflict),
    ] {
        assert_eq!(ledger.settle(entry), Err(want));
        assert_eq!(ledger, before);
    }
}

#[test]
fn v1_final_never_satisfies_a_binding_expectation() {
    let mut wants = expect();
    wants.binding = Some(binding());
    let reason = unresolved(resolve(ResolveInput {
        expect: wants,
        ..input(
            Candidate::Absent,
            Candidate::Present(parse_final(&v1_bytes()).unwrap()),
        )
    }));
    assert_eq!(reason, Unresolved::BindingMismatch);
}

#[test]
fn directly_built_v1_final_is_revalidated() {
    let FinalRecord::V1(mut archive) = parse_final(&v1_bytes()).unwrap() else {
        panic!("v1 bytes must parse as v1");
    };
    archive.version = RECORD_VERSION;
    let reason = unresolved(resolve(input(
        Candidate::Absent,
        Candidate::Present(FinalRecord::V1(archive)),
    )));
    assert_eq!(
        reason,
        Unresolved::Invalid {
            kind: RecordKind::Final,
            error: RecordError::UnsupportedVersion(u64::from(RECORD_VERSION)),
        }
    );
}

/// Bytes as `archive.rs` writes a v1 archive, kept verbatim.
const V1_FIXTURE: &[u8] = br#"{"version":1,"handle_id":"sa_7","meta":{"agent_name":"worker","model":"provider/model-b","system_prompt":"You are a worker.","timeout_secs":300},"status":"completed","output":"legacy output","history":[{"role":"user","content":"legacy task"},{"role":"assistant","content":[{"type":"text","text":"done"}]}]}"#;

#[test]
fn retained_v1_fixture_resolves_as_legacy() {
    let parsed = parse_final(V1_FIXTURE).unwrap();
    match resolve(input(Candidate::Absent, Candidate::Present(parsed))) {
        Resolution::Legacy(archive) => {
            assert_eq!(archive.history.len(), 2);
            assert_eq!(archive.meta.timeout_secs, 300);
        }
        other => panic!("expected legacy, got {other:?}"),
    }
}

#[test]
fn handles_are_canonical_u64() {
    for good in ["sa_1", "sa_42", "sa_18446744073709551615"] {
        assert!(is_valid_handle_id(good), "{good}");
    }
    for bad in [
        "sa_0",
        "sa_01",
        "sa_007",
        "sa_18446744073709551616",
        "sa_99999999999999999999",
        "sa_+1",
        "sa_-1",
        "sa_ 1",
    ] {
        assert!(!is_valid_handle_id(bad), "{bad}");
    }
}

#[test]
fn binding_scope_is_a_sha256_digest() {
    let run = RunId::random();
    for bad in [
        "scope-0123".to_owned(),
        format!("sha256:{}", "AB".repeat(32)),
        format!("sha256:{}", "ab".repeat(31)),
        "ab".repeat(32),
    ] {
        let mut r = record(&run, RecordKind::Head, 0);
        r.binding.scope_digest = bad.clone();
        assert_eq!(
            r.validate(RecordKind::Head),
            Err(RecordError::InvalidBinding),
            "{bad}"
        );
    }
}

/// Insert `key: value` into the object at `path` of a serialized record.
fn with_extra_field(r: &WorkerRecord, path: &[&str]) -> Vec<u8> {
    let mut value = serde_json::to_value(r).unwrap();
    let mut at = &mut value;
    for step in path {
        at = match step.parse::<usize>() {
            Ok(i) => &mut at[i],
            Err(_) => &mut at[*step],
        };
    }
    at.as_object_mut()
        .expect("path names an object")
        .insert("unexpected".into(), json!(1));
    serde_json::to_vec(&value).unwrap()
}

#[test]
fn unknown_fields_rejected_at_every_typed_level() {
    let mut r = chained(RecordKind::Head, 1);
    r.ledger.settle(ledger_entry("req-1", measured(3))).unwrap();
    r.ledger.settle(ledger_entry("req-2", held(3))).unwrap();
    assert!(parse_head(&serde_json::to_vec(&r).unwrap()).is_ok());
    let paths: [&[&str]; 10] = [
        &[],
        &["binding"],
        &["predecessor"],
        &["windows", "0"],
        &["inherited_windows", "0"],
        &["ledger", "0"],
        &["ledger", "0", "amount"],
        &["ledger", "0", "amount", "usage"],
        &["ledger", "1", "amount"],
        &["meta"],
    ];
    for path in paths {
        assert!(
            parse_head(&with_extra_field(&r, path)).is_err(),
            "unknown field accepted at {path:?}"
        );
    }
    // History stays open: arbitrary message fields are kept verbatim.
    r.history[0]["provider_extra"] = json!({"kept": true});
    r.history_digest = HistoryDigest::of(&r.history).unwrap();
    let bytes = serde_json::to_vec(&r).unwrap();
    assert_eq!(parse_head(&bytes).unwrap(), r);
}

#[test]
fn malformed_fields_rejected_on_parse() {
    let r = record(&RunId::random(), RecordKind::Final, 1);
    let base = serde_json::to_value(&r).unwrap();
    let cases: Vec<(&str, Value)> = vec![
        ("run_id", json!("A".repeat(32))),
        ("run_id", json!("ab")),
        ("history_digest", json!("md5:00")),
        (
            "history_digest",
            json!(format!("sha256:{}", "0".repeat(63))),
        ),
        ("run_id", Value::Null),
        ("generation", json!(-1)),
        ("generation", json!(1.5)),
        ("kind", json!("draft")),
        ("windows", json!([{"window": 1, "archive_id": "ctx-1"}])),
        (
            "ledger",
            json!([{"request_id": "has space", "purpose": "conversation",
                    "model": "m", "amount": {"state": "released"}}]),
        ),
    ];
    for (field, bad) in cases {
        let mut v = base.clone();
        v[field] = bad.clone();
        assert!(
            parse_final(&serde_json::to_vec(&v).unwrap()).is_err(),
            "{field} = {bad} accepted"
        );
    }
    for field in ["run_id", "binding", "history", "meta", "window_ordinal"] {
        let mut v = base.clone();
        v.as_object_mut().unwrap().remove(field);
        assert!(
            parse_final(&serde_json::to_vec(&v).unwrap()).is_err(),
            "missing {field} accepted"
        );
    }
    // A repeated top-level key is rejected, not last-wins.
    let text = serde_json::to_string(&r).unwrap();
    let doubled = format!("{},\"status\":\"failed\"}}", &text[..text.len() - 1]);
    assert!(parse_final(doubled.as_bytes()).is_err());
}

#[test]
fn pair_and_expectation_mismatches_in_both_directions() {
    let a = RunId::random();
    let b = RunId::random();
    for (head_run, final_run) in [(&a, &b), (&b, &a)] {
        for expected in [None, Some(a.clone())] {
            let mut e = expect();
            e.run_id = expected;
            let reason = unresolved(resolve(ResolveInput {
                expect: e,
                ..input(
                    Candidate::Present(record(head_run, RecordKind::Head, 1)),
                    v2_final(record(final_run, RecordKind::Final, 1)),
                )
            }));
            assert_eq!(reason, Unresolved::RunIdMismatch);
        }
    }
    // Either side carrying a foreign handle.
    for head_is_foreign in [true, false] {
        let mut head = record(&a, RecordKind::Head, 1);
        let mut fin = record(&a, RecordKind::Final, 1);
        if head_is_foreign {
            head.handle_id = "sa_8".into();
        } else {
            fin.handle_id = "sa_8".into();
        }
        let reason = unresolved(resolve(input(Candidate::Present(head), v2_final(fin))));
        assert_eq!(reason, Unresolved::HandleMismatch);
    }
    // Either side carrying a foreign binding, with the expectation on one side.
    for head_is_foreign in [true, false] {
        let mut head = record(&a, RecordKind::Head, 1);
        let mut fin = record(&a, RecordKind::Final, 1);
        if head_is_foreign {
            head.binding.scope_digest = scope(0xcd);
        } else {
            fin.binding.scope_digest = scope(0xcd);
        }
        for expected in [None, Some(binding())] {
            let mut e = expect();
            e.binding = expected;
            let reason = unresolved(resolve(ResolveInput {
                expect: e,
                ..input(Candidate::Present(head.clone()), v2_final(fin.clone()))
            }));
            assert_eq!(reason, Unresolved::BindingMismatch);
        }
    }
}

#[test]
fn invalid_side_of_a_pair_is_reported() {
    let run = RunId::random();
    let mut bad_head = record(&run, RecordKind::Head, 1);
    bad_head.history.push(json!("tampered"));
    let reason = unresolved(resolve(input(
        Candidate::Present(bad_head),
        v2_final(record(&run, RecordKind::Final, 1)),
    )));
    assert_eq!(
        reason,
        Unresolved::Invalid {
            kind: RecordKind::Head,
            error: RecordError::DigestMismatch
        }
    );
    let mut bad_final = record(&run, RecordKind::Final, 1);
    bad_final.history.push(json!("tampered"));
    let reason = unresolved(resolve(input(
        Candidate::Present(record(&run, RecordKind::Head, 1)),
        v2_final(bad_final),
    )));
    assert_eq!(
        reason,
        Unresolved::Invalid {
            kind: RecordKind::Final,
            error: RecordError::DigestMismatch
        }
    );
}

#[test]
fn writer_may_publish_takes_precedence() {
    let run = RunId::random();
    let mut bad = record(&run, RecordKind::Head, 1);
    bad.history.push(json!("tampered"));
    let cases: Vec<(Candidate<WorkerRecord>, Candidate<FinalRecord>)> = vec![
        (Candidate::Unreadable("torn".into()), Candidate::Absent),
        (Candidate::Absent, Candidate::Unreadable("bad json".into())),
        (
            Candidate::Unreadable("torn".into()),
            Candidate::Unreadable("bad json".into()),
        ),
        (Candidate::Present(bad), Candidate::Absent),
        (
            Candidate::Present(record(&run, RecordKind::Head, 1)),
            Candidate::Present(parse_final(&v1_bytes()).unwrap()),
        ),
        (Candidate::Absent, Candidate::Absent),
        (
            Candidate::Absent,
            Candidate::Present(parse_final(&v1_bytes()).unwrap()),
        ),
    ];
    for (head, final_record) in cases {
        let reason = unresolved(resolve(ResolveInput {
            writer: WriterState::MayPublish,
            ..input(head, final_record)
        }));
        assert_eq!(reason, Unresolved::WriterMayPublish);
    }
}

#[test]
fn r12_binding_expectation_enforced() {
    let run = RunId::random();
    let mut wants = expect();
    wants.binding = Some(Binding {
        backend: BackendKind::Axel,
        scope_digest: scope(0xab),
    });
    let reason = unresolved(resolve(ResolveInput {
        expect: wants,
        ..input(
            Candidate::Absent,
            v2_final(record(&run, RecordKind::Final, 1)),
        )
    }));
    assert_eq!(reason, Unresolved::BindingMismatch);

    // Head and final of one run must agree even without an expectation.
    let mut head = record(&run, RecordKind::Head, 1);
    head.binding.scope_digest = scope(0xcd);
    let reason = unresolved(resolve(input(
        Candidate::Present(head),
        v2_final(record(&run, RecordKind::Final, 1)),
    )));
    assert_eq!(reason, Unresolved::BindingMismatch);
}

#[test]
fn r13_ledger_settlement_idempotent() {
    let id = RequestId::parse("req-1").unwrap();
    let entry = |purpose, amount| LedgerEntry {
        request_id: id.clone(),
        purpose,
        model: "provider/model-c".into(),
        amount,
    };
    let held = Amount::Liability {
        estimate_micro_usd: 1_500,
    };
    let measured = Amount::Measured {
        usage: MeasuredUsage {
            input_tokens: 900,
            output_tokens: 120,
            ..Default::default()
        },
    };
    let mut ledger = Ledger::default();
    let summary = RequestPurpose::RolloverSummary;
    assert_eq!(
        ledger.settle(entry(summary, held.clone())),
        Ok(Settled::Recorded)
    );
    assert_eq!(
        ledger.settle(entry(summary, held.clone())),
        Ok(Settled::Duplicate)
    );
    assert_eq!(
        ledger.settle(entry(RequestPurpose::Conversation, measured.clone())),
        Err(SettleError::IdentityMismatch),
        "same-model summary usage stays attributed by purpose"
    );
    assert_eq!(
        ledger.settle(entry(summary, measured.clone())),
        Ok(Settled::Transitioned)
    );
    assert_eq!(
        ledger.settle(entry(summary, measured.clone())),
        Ok(Settled::Duplicate)
    );
    assert_eq!(
        ledger.settle(entry(summary, held)),
        Err(SettleError::Conflict),
        "measured is terminal"
    );
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger.entries().next().unwrap().amount, measured);

    // Round trip keeps entries; duplicate IDs in a file are rejected.
    let bytes = serde_json::to_vec(&ledger).unwrap();
    assert_eq!(serde_json::from_slice::<Ledger>(&bytes).unwrap(), ledger);
    let doubled = format!(
        "[{0},{0}]",
        serde_json::to_string(ledger.entries().next().unwrap()).unwrap()
    );
    assert!(serde_json::from_str::<Ledger>(&doubled).is_err());
}

#[test]
fn identifiers_and_file_names_are_validated() {
    assert!(RunId::parse(&"A".repeat(32)).is_err());
    assert!(RunId::parse("short").is_err());
    assert!(HistoryDigest::parse("md5:00").is_err());
    assert!(RequestId::parse("").is_err());
    assert!(RequestId::parse("has space").is_err());
    assert_eq!(
        record_file_name("sa_12", RecordKind::Head).unwrap(),
        "sa_12.head.json"
    );
    assert_eq!(
        record_file_name("sa_12", RecordKind::Final).unwrap(),
        "sa_12.json"
    );
    for bad in ["sa_", "sa_1/..", "../sa_1", "sa_1x", "worker"] {
        assert_eq!(
            record_file_name(bad, RecordKind::Final),
            Err(RecordError::InvalidHandle)
        );
    }
    assert!(serde_json::from_value::<RunId>(json!("not-hex")).is_err());
}

/// Port glue (dev's watch lease, f6000024): a record's meta carries a watch
/// worker's lease used so far, both ways through the shared archive meta,
/// under the record's strict parsing; an ordinary worker's has no field.
#[test]
fn meta_carries_the_watch_lease_strictly() {
    let mut r = chained(RecordKind::Head, 1);
    let plain = serde_json::to_value(&r).unwrap();
    assert!(plain["meta"].get("watch_usage").is_none());
    let parsed = parse_head(&serde_json::to_vec(&r).unwrap()).unwrap();
    assert_eq!(parsed.meta.watch_usage, None);

    let mut archived = meta();
    archived.watch_usage = Some(crate::runtime::subagent::WatchUsage {
        secs: 42,
        rounds: 3,
    });
    r.meta = archived.clone().into();
    let back = parse_head(&serde_json::to_vec(&r).unwrap()).unwrap();
    assert_eq!(back, r);
    assert_eq!(SubagentArchiveMeta::from(back.meta), archived);

    let mut value = serde_json::to_value(&r).unwrap();
    value["meta"]["watch_usage"]["unexpected"] = json!(1);
    assert!(parse_head(&serde_json::to_vec(&value).unwrap()).is_err());
}
