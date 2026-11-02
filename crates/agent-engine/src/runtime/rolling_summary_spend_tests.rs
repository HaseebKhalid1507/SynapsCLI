// Child spend at the summary settlement (P1e), included into
// `rolling_summary::transport_tests::warm_tests::spend_tests` (its helpers
// and mocks): a worker runtime with a spend tap books each summary once,
// at its settlement, as a `summary` report with the summary's model and
// counts. The summary's own transport books nothing, and a summary that
// never reaches its settlement books nothing (the narrow promise: its
// reservation stays held locally, as today). Every endpoint is a loopback
// mock.
use crate::runtime::child_spend::{
    SinkMessage, SpendKind, SpendReceiver, SpendSink, SpendTap, SpendTokens,
};

/// Give `rt` a worker's spend tap; the session's end of its channel.
fn tapped(rt: &mut crate::Runtime) -> SpendReceiver {
    let (sink, rx) = SpendSink::new();
    let lease = sink.lease("sa_1", "worker").unwrap();
    rt.set_spend_tap(Some(SpendTap::new(lease)));
    rx
}

/// Every report the receiver holds, in order: (kind, model, tokens).
fn reports(rx: &mut SpendReceiver) -> Vec<(SpendKind, String, SpendTokens)> {
    let mut out = Vec::new();
    while let Ok(message) = rx.try_recv() {
        if let SinkMessage::Spend { report, .. } = message {
            out.push((report.kind, report.model, report.tokens));
        }
    }
    out
}

fn of_kind(
    reports: &[(SpendKind, String, SpendTokens)],
    kind: SpendKind,
) -> Vec<(SpendKind, String, SpendTokens)> {
    reports.iter().filter(|r| r.0 == kind).cloned().collect()
}

fn counts(input: u64, output: u64) -> SpendTokens {
    SpendTokens {
        input,
        output,
        ..SpendTokens::default()
    }
}

/// The native screened (cold) summary: one `summary` report with the
/// summary model's counts, and the turn's usage events are the ones a
/// runtime without a tap sends. The stand-in's conversation replies report
/// no usage, so the summary is the only report.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_worker_cold_summary_books_one_summary_report_at_its_settlement() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn(Reply::Text("## Window 1\nBOOKED".into())).await;
    let plain = turn(&runtime(Some(SUMMARY)), history(None), Default::default()).await;
    assert!(plain.errors.is_empty(), "{:?}", plain.errors);
    let sent = mock.summaries.lock().unwrap().len();

    let mut rt = runtime(Some(SUMMARY));
    let mut rx = tapped(&mut rt);
    let out = turn(&rt, history(None), Default::default()).await;
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert!(!out.heads.is_empty(), "the turn rolls over");
    assert!(summary_of(&out.heads[0]).is_some_and(|s| s.contains("BOOKED")));
    assert_eq!(out.usages, plain.usages, "the tap changes no usage event");
    assert_eq!(
        reports(&mut rx),
        vec![(
            SpendKind::Summary,
            "claude-sonnet-4-6".to_string(),
            counts(777, 333)
        )]
    );
    assert_eq!(mock.summaries.lock().unwrap().len(), sent + 1, "one summary request");
}

/// A failed reply that reported its usage is charged it: the settlement
/// books it once, even though the previous summary is carried.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn an_empty_summary_reply_that_reported_usage_books_it_once() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (_mock, _g) = spawn(Reply::Text(String::new())).await;
    let mut rt = runtime(Some(SUMMARY));
    let mut rx = tapped(&mut rt);
    let out = turn(&rt, history(None), Default::default()).await;
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(
        reports(&mut rx),
        vec![(
            SpendKind::Summary,
            "claude-sonnet-4-6".to_string(),
            counts(777, 333)
        )]
    );
}

/// Usage without input and output counts is not a report: nothing is
/// booked (the reservation stays the local charge). Explicit zeros are a
/// report and book a zero-cost summary.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_summary_without_usable_usage_books_nothing_and_explicit_zeros_book_zero() {
    let _base = crate::test_env::BaseDirGuard::new();
    for (usage, expected) in [
        (None, vec![]),
        (Some(json!(null)), vec![]),
        (Some(json!({})), vec![]),
        (Some(json!({"input_tokens": 700})), vec![]),
        (Some(json!({"output_tokens": 50})), vec![]),
        (
            Some(json!({"input_tokens": 0, "output_tokens": 0})),
            vec![(
                SpendKind::Summary,
                "claude-sonnet-4-6".to_string(),
                counts(0, 0),
            )],
        ),
    ] {
        let (_mock, _g) = spawn(Reply::Usage(usage.clone())).await;
        let mut rt = runtime(Some(SUMMARY));
        let mut rx = tapped(&mut rt);
        let out = turn(&rt, history(None), Default::default()).await;
        assert!(out.errors.is_empty(), "{usage:?}: {:?}", out.errors);
        assert_eq!(reports(&mut rx), expected, "{usage:?}");
    }
}

/// A summary cancelled after it was sent never reaches its settlement:
/// nothing is booked, now or later.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_cancelled_summary_books_nothing() {
    let _base = crate::test_env::BaseDirGuard::new();
    let cancel = tokio_util::sync::CancellationToken::new();
    let (mock, _g) = spawn(Reply::CancelThenSlow(
        cancel.clone(),
        Duration::from_secs(20),
    ))
    .await;
    let mut rt = runtime(Some(SUMMARY));
    let mut rx = tapped(&mut rt);
    let out = turn(&rt, history(None), cancel).await;
    assert!(out.heads.is_empty(), "no head after a cancel");
    assert_eq!(mock.summaries.lock().unwrap().len(), 1, "the summary was sent");
    assert_eq!(reports(&mut rx), vec![]);
    // Nothing settles after the turn either.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(reports(&mut rx), vec![]);
}

/// The routed summary: its transport (the router) books nothing; its
/// settlement books one `summary` report at the summary model.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_routed_worker_summary_books_once_at_its_settlement() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (_mock, _g) = spawn(Reply::Status(500)).await;
    let (_bodies, _l) = spawn_local(Local::Reply("## Window 1\nROUTED")).await;
    let mut rt = local_runtime();
    let mut rx = tapped(&mut rt);
    let out = turn(&rt, history(None), Default::default()).await;
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert!(summary_of(&out.heads[0]).is_some_and(|s| s.contains("ROUTED")));
    let summary_usage: Vec<_> = out
        .usages
        .iter()
        .filter(|(_, _, m)| m.as_deref() == Some("local/test-model"))
        .collect();
    assert_eq!(summary_usage, vec![&(500, 60, Some("local/test-model".to_string()))]);
    assert_eq!(
        reports(&mut rx),
        vec![(
            SpendKind::Summary,
            "local/test-model".to_string(),
            counts(500, 60)
        )],
        "one report: the routed transport books none of its own"
    );
}

/// The warm summary: its request goes through the native transport with
/// no tap, so the only report for it is the settlement's, with the warm
/// reply's full usage. Each conversation request still books its own
/// `turn` report.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_worker_warm_summary_books_one_summary_report_at_its_settlement() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let mut rt = runtime(Some(SUMMARY));
    let mut rx = tapped(&mut rt);
    let (_first, second) = two_turns(&rt).await;
    assert!(summary_of(&second.heads[0]).is_some_and(|s| s.contains("WARM-SUMMARY")));
    assert_eq!(mock.warm_requests(), 1, "one warm request");
    assert_eq!(mock.cold_requests(), 0, "no cold request");

    let booked = reports(&mut rx);
    let warm_usage: Vec<_> = second
        .usages
        .iter()
        .filter(|(i, o, _)| (*i, *o) == (60, 250))
        .collect();
    assert_eq!(warm_usage.len(), 1, "{:?}", second.usages);
    let model = warm_usage[0].2.clone().expect("the summary's model");
    assert_eq!(
        of_kind(&booked, SpendKind::Summary),
        vec![(
            SpendKind::Summary,
            model,
            SpendTokens {
                input: 60,
                output: 250,
                cache_read: 45_000,
                cache_creation: 0,
                cache_creation_5m: None,
                cache_creation_1h: None,
            }
        )]
    );
    let conversation = mock
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.body.contains("\"stream\":true") && !s.body.contains(WARM_MARK))
        .count();
    let turns = of_kind(&booked, SpendKind::Turn);
    assert_eq!(turns.len(), conversation, "{booked:?}");
    assert!(
        turns.iter().all(|(_, _, t)| (t.input, t.output) == (100, 5)),
        "no turn report is the warm request's: {booked:?}"
    );
}

/// A warm reply without full usage is not settled: nothing is booked for
/// the summary, and its transport books nothing either.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_warm_summary_without_full_usage_books_nothing() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    *mock.kind.lock().unwrap() = WarmKind::PartialUsage;
    let mut rt = runtime(Some(SUMMARY));
    let mut rx = tapped(&mut rt);
    let (_first, second) = two_turns(&rt).await;
    assert!(summary_of(&second.heads[0]).is_some_and(|s| s.contains("WARM-PARTIAL")));
    let booked = reports(&mut rx);
    assert_eq!(of_kind(&booked, SpendKind::Summary), vec![], "{booked:?}");
    assert!(
        of_kind(&booked, SpendKind::Turn)
            .iter()
            .all(|(_, _, t)| (t.input, t.output) == (100, 5)),
        "{booked:?}"
    );
}
