// Request-ledger tests for the warm summary, included into
// `rolling_summary::transport_tests::warm_tests::ledger_tests`
// (`docs/specs/subagent-context-rollover.md` §6, slice 6, dark). A warm
// summary on a v2 worker run is one `rollover_summary` request of the run,
// reserved right before it is sent. It is measured by its full report and
// released when it provably was not sent (the transport says so, or its
// unpublished body was withdrawn); otherwise it stays open. A cold summary
// after an unsent warm attempt is a request of its own, and a run at its cap
// sends neither. Through the stream, a v2 worker runtime books its summary
// in its own run ledger. Every endpoint is the loopback mock.
use crate::runtime::rolling_summary::{SummaryPath, SummarySkip};
use crate::tools::ledger_record::{Amount, Ledger, LedgerEntry, MeasuredUsage, RequestPurpose};
use crate::tools::request_ledger::UsageReports;
use crate::tools::{LedgerSlot, RequestLedgerHandle};
use agent_core::{LiabilityState, UsagePurpose};

const LEDGER_MODEL: &str = "claude-sonnet-4-6";

/// What one ledgered summary did.
struct Booked {
    out: crate::runtime::rolling_summary::Summarized,
    events: Vec<agent_core::StreamEvent>,
    /// The turn's counters' allowance still held afterwards.
    reserved: f64,
}

impl Booked {
    /// The attempts' usage events as (request ID, purpose, input, output).
    fn attempts(&self) -> Vec<(String, UsagePurpose, u64, u64)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                agent_core::StreamEvent::Session(agent_core::SessionEvent::UsageAttempt {
                    request,
                    usage,
                }) => Some((
                    request.request_id.clone(),
                    request.purpose,
                    usage.input_tokens,
                    usage.output_tokens,
                )),
                _ => None,
            })
            .collect()
    }

    /// The liability events as (request ID, estimate, state).
    fn liabilities(&self) -> Vec<(String, u64, LiabilityState)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                agent_core::StreamEvent::Session(agent_core::SessionEvent::UsageLiability {
                    request,
                    estimate_micro_usd,
                    state,
                }) => Some((request.request_id.clone(), *estimate_micro_usd, *state)),
                _ => None,
            })
            .collect()
    }

    fn plain_usage(&self) -> usize {
        self.events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    agent_core::StreamEvent::Session(agent_core::SessionEvent::Usage { .. })
                )
            })
            .count()
    }
}

/// A warm transport from an async closure.
fn warm_call<F, Fut>(f: F) -> ws::WarmCall
where
    F: Fn(Arc<ws::RequestRecord>, ws::WarmSend, tokio_util::sync::CancellationToken) -> Fut
        + Send
        + Sync
        + 'static,
    Fut: std::future::Future<Output = ws::WarmReply> + Send + 'static,
{
    Arc::new(move |record, send, cancel| Box::pin(f(record, send, cancel)))
}

fn booked(ledger: &RequestLedgerHandle) -> Vec<LedgerEntry> {
    ledger.snapshot().entries().cloned().collect()
}

fn id(entry: &LedgerEntry) -> String {
    entry.request_id.as_str().to_owned()
}

/// One rollover summary of turn 1's second message through `summarize()`,
/// warm through `call` (the runtime's own warm transport when `None`),
/// booked in `ledger`.
async fn summarize_booked(
    rt: &crate::Runtime,
    first: &Turn,
    call: Option<ws::WarmCall>,
    ledger: Option<&RequestLedgerHandle>,
    timeout: Duration,
) -> Booked {
    let summarizer = rt.rollover_summarizer().expect("summarizer");
    assert!(summarizer.warm().is_some(), "the warm path is configured");
    let mut warm = rt.warm_summarizer();
    if let Some(call) = call {
        warm.call = call;
    }
    let usage = crate::runtime::budget::UsageCounters::default();
    let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let archive = "a".repeat(32);
    let dropped = vec![first.history[1].clone()];
    let indices = [1usize];
    let cancel = tokio_util::sync::CancellationToken::new();
    let state = rt.continuation.clone();
    let logical_id = state.lock().unwrap().logical_id.clone();
    let messages = first.history.clone();
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        crate::runtime::rolling_summary::summarize(
            &summarizer,
            crate::runtime::rolling_summary::SummaryRequest {
                logical_id: &logical_id,
                previous: None,
                dropped: &dropped,
                window: 1,
                archive_id: &archive,
                cap_tokens: SUMMARY,
                timeout,
                cancel: &cancel,
                accounting: Some(crate::runtime::rolling_summary::SummaryAccounting {
                    usage: &usage,
                    max_cost_usd: None,
                    events: &events,
                    tap: None,
                    ledger,
                }),
                snapshot: Some(ws::SnapshotGate::new(&state)),
                run_deadline: None,
            },
            Some(ws::WarmContext {
                warm: &warm,
                state: &state,
                messages: &messages,
                dropped: &indices,
                newest_run_start: None,
            }),
        ),
    )
    .await
    .expect("the summary is bounded");
    drop(events);
    let mut seen = Vec::new();
    while let Ok(event) = rx.try_recv() {
        seen.push(event);
    }
    Booked {
        out,
        events: seen,
        reserved: usage.reserved_usd(),
    }
}

/// A warm summary is one `rollover_summary` request, measured by its full
/// report (cache reads included), reported as that attempt's usage and never
/// also as the plain usage.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn sw1_a_warm_summary_is_measured_as_its_own_attempt() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    let (first, _record) = warmed(&rt).await;
    let ledger = RequestLedgerHandle::new(LEDGER_MODEL);
    let b = summarize_booked(&rt, &first, None, Some(&ledger), Duration::from_secs(30)).await;
    assert!(b.out.text.is_ok(), "{:?}", b.out.text);
    assert_eq!(b.out.report.path, SummaryPath::Warm);
    assert_eq!((mock.warm_requests(), mock.cold_requests()), (1, 0));
    let entries = booked(&ledger);
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].purpose, RequestPurpose::RolloverSummary);
    assert_eq!(entries[0].model, LEDGER_MODEL);
    assert_eq!(
        entries[0].amount,
        Amount::Measured {
            usage: MeasuredUsage {
                input_tokens: 60,
                output_tokens: 250,
                cache_read_tokens: 45_000,
                cache_write_tokens: 0,
                cache_write_5m: None,
                cache_write_1h: None,
            }
        }
    );
    assert_eq!(
        b.attempts(),
        vec![(id(&entries[0]), UsagePurpose::RolloverSummary, 60, 250)]
    );
    assert_eq!(b.plain_usage(), 0, "never also the plain usage");
    assert!(b.liabilities().is_empty(), "{:?}", b.liabilities());
}

/// A warm attempt the transport did not send is released; the cold
/// summary that follows is a request of its own, measured by its report.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn sw2_an_unsent_warm_attempt_is_released_and_the_cold_one_is_its_own() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    let (first, _record) = warmed(&rt).await;
    let ledger = RequestLedgerHandle::new(LEDGER_MODEL);
    let unsent = warm_call(|_record, _send, _cancel| async {
        ws::WarmReply {
            terminal: ws::WarmTerminal::NotSent(ws::WarmMiss::PreSend),
            usage: None,
            dispatched: false,
            failure: None,
            stop: None,
        }
    });
    let b = summarize_booked(&rt, &first, Some(unsent), Some(&ledger), Duration::from_secs(30))
        .await;
    assert!(
        b.out.text.as_ref().is_ok_and(|t| t.contains("COLD-SUMMARY")),
        "{:?}",
        b.out.text
    );
    assert_eq!((mock.warm_requests(), mock.cold_requests()), (0, 1));
    let entries = booked(&ledger);
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert!(entries
        .iter()
        .all(|e| e.purpose == RequestPurpose::RolloverSummary && e.model == LEDGER_MODEL));
    let released: Vec<_> = entries
        .iter()
        .filter(|e| e.amount == Amount::Released {})
        .collect();
    let measured: Vec<_> = entries
        .iter()
        .filter(|e| matches!(e.amount, Amount::Measured { .. }))
        .collect();
    assert_eq!((released.len(), measured.len()), (1, 1), "{entries:?}");
    assert_eq!(
        measured[0].amount,
        Amount::Measured {
            usage: MeasuredUsage {
                input_tokens: 30_000,
                output_tokens: 300,
                ..MeasuredUsage::default()
            }
        }
    );
    let liabilities = b.liabilities();
    assert_eq!(liabilities.len(), 1, "{:?}", b.events);
    assert_eq!(liabilities[0].0, id(released[0]));
    assert_eq!(liabilities[0].2, LiabilityState::Released);
    assert_eq!(
        b.attempts(),
        vec![(id(measured[0]), UsagePurpose::RolloverSummary, 30_000, 300)]
    );
    assert_eq!(b.plain_usage(), 0);
}

/// A warm summary that was sent but did not report every counter keeps its
/// liability open.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn sw3_a_warm_reply_without_full_usage_stays_open() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    *mock.kind.lock().unwrap() = WarmKind::PartialUsage;
    let rt = runtime(Some(SUMMARY));
    let (first, _record) = warmed(&rt).await;
    let ledger = RequestLedgerHandle::new(LEDGER_MODEL);
    let b = summarize_booked(&rt, &first, None, Some(&ledger), Duration::from_secs(30)).await;
    assert!(b.out.text.is_ok(), "{:?}", b.out.text);
    let entries = booked(&ledger);
    assert_eq!(entries.len(), 1, "{entries:?}");
    let Amount::Liability { estimate_micro_usd } = entries[0].amount else {
        panic!("open: {:?}", entries[0].amount);
    };
    assert!(estimate_micro_usd > 0);
    assert_eq!(
        b.liabilities(),
        vec![(id(&entries[0]), estimate_micro_usd, LiabilityState::Open)]
    );
    assert!(b.attempts().is_empty() && b.plain_usage() == 0);
}

/// A warm attempt that times out is released only when its body was never
/// published (withdrawn: nothing was sent); once published it stays open.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn sw4_a_timed_out_warm_attempt_is_released_only_when_never_published() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    let (first, _record) = warmed(&rt).await;
    let unpublished =
        warm_call(|_record, _send, _cancel| futures::future::pending::<ws::WarmReply>());
    let published = warm_call(|_record, send: ws::WarmSend, _cancel| async move {
        assert!(send.published.publish(), "the body may go out");
        futures::future::pending::<ws::WarmReply>().await
    });
    for (case, call, expected) in [
        ("unpublished", unpublished, LiabilityState::Released),
        ("published", published, LiabilityState::Open),
    ] {
        let ledger = RequestLedgerHandle::new(LEDGER_MODEL);
        let b = summarize_booked(&rt, &first, Some(call), Some(&ledger), Duration::from_millis(50))
            .await;
        assert!(b.out.text.is_err(), "{case}");
        assert_eq!(b.out.report.terminal, Some("timeout"), "{case}");
        let entries = booked(&ledger);
        assert_eq!(entries.len(), 1, "{case}: {entries:?}");
        match expected {
            LiabilityState::Released => assert_eq!(entries[0].amount, Amount::Released {}),
            _ => assert!(
                matches!(entries[0].amount, Amount::Liability { .. }),
                "{case}: {:?}",
                entries[0].amount
            ),
        }
        let liabilities = b.liabilities();
        assert_eq!(liabilities.len(), 1, "{case}: {:?}", b.events);
        assert_eq!(liabilities[0].2, expected, "{case}");
        assert!(b.attempts().is_empty() && b.plain_usage() == 0, "{case}");
    }
    assert_eq!((mock.warm_requests(), mock.cold_requests()), (0, 0));
}

/// A run at its request cap sends no summary, warm or cold: it is skipped
/// with its reason and the turn's reservation is returned.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn sw5_a_run_at_its_cap_sends_no_warm_or_cold_summary() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    let (first, _record) = warmed(&rt).await;
    let ledger = RequestLedgerHandle::new(LEDGER_MODEL);
    for _ in 0..Ledger::MAX_ENTRIES {
        ledger
            .reserve(1, UsageReports::Cumulative)
            .unwrap()
            .release();
    }
    let b = summarize_booked(&rt, &first, None, Some(&ledger), Duration::from_secs(30)).await;
    assert!(
        matches!(b.out.text, Err(Some(SummarySkip::RunFull))),
        "{:?}",
        b.out.text
    );
    assert_eq!((mock.warm_requests(), mock.cold_requests()), (0, 0));
    assert_eq!(b.reserved, 0.0, "the turn's reservation is returned");
    assert!(b.events.is_empty(), "{:?}", b.events);
    assert_eq!(booked(&ledger).len(), Ledger::MAX_ENTRIES);
}

/// Through the stream: a v2 worker runtime (its run's ledger installed)
/// books its rollover summary in its own run ledger, under its own purpose,
/// next to its conversation requests.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn sw6_a_v2_runtime_books_its_rollover_summary_in_its_run_ledger() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let mut rt = runtime(Some(SUMMARY));
    let slot = LedgerSlot::default();
    rt.arm_request_ledger(slot.clone());
    assert!(slot.install(RequestLedgerHandle::new(LEDGER_MODEL)));
    let ledger = rt.request_ledger().expect("the run's ledger");
    let (_first, second) = two_turns(&rt).await;
    assert!(summary_of(&second.heads[0]).is_some(), "turn 2 is summarized");
    assert_eq!(mock.warm_requests(), 1);
    let entries = booked(&ledger);
    let summaries: Vec<_> = entries
        .iter()
        .filter(|e| e.purpose == RequestPurpose::RolloverSummary)
        .collect();
    assert_eq!(summaries.len(), 1, "{entries:?}");
    assert!(
        matches!(summaries[0].amount, Amount::Measured { .. }),
        "{:?}",
        summaries[0].amount
    );
    assert!(
        entries
            .iter()
            .filter(|e| e.purpose == RequestPurpose::Conversation)
            .count()
            >= 2,
        "{entries:?}"
    );
}

/// Through the stream and the real routed transport: a cold summary whose
/// endpoint refuses the connection was provably not sent, so a v2 runtime's
/// ledger releases it (the summary gate's `cold_not_sent` case, ledgered).
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn sw7_a_cold_summary_whose_endpoint_refuses_the_connection_is_released() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (_mock, _g) = spawn_warm().await;
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let _local = crate::test_env::EnvVarGuard::set(
        "LOCAL_ENDPOINT",
        &format!("http://127.0.0.1:{closed}/v1"),
    );
    let mut rt = runtime(Some(SUMMARY));
    rt.set_summary_model(Some("local/test-model".to_owned()));
    let slot = LedgerSlot::default();
    rt.arm_request_ledger(slot.clone());
    assert!(slot.install(RequestLedgerHandle::new(LEDGER_MODEL)));
    let ledger = rt.request_ledger().expect("the run's ledger");
    let (_first, _second) = two_turns(&rt).await;
    let entries = booked(&ledger);
    let summaries: Vec<_> = entries
        .iter()
        .filter(|e| e.purpose == RequestPurpose::RolloverSummary)
        .collect();
    assert_eq!(summaries.len(), 1, "{entries:?}");
    assert_eq!(summaries[0].model, "local/test-model");
    assert_eq!(summaries[0].amount, Amount::Released {}, "{:?}", summaries[0].amount);
}
