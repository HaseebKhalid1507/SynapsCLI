//! A v2 worker run's screened (cold) rollover summary in the run's request
//! ledger (`docs/specs/subagent-context-rollover.md` §6, slice 6, dark).
//!
//! The summary's one attempt is reserved right before it is sent, under
//! purpose `rollover_summary`, for the amount the summary itself reserves.
//! It is settled by the summary transport's typed spend: measured when
//! reported, released when provably not sent or rejected before generation
//! by a first-party endpoint, otherwise open. Its usage is reported as the
//! attempt's (`UsageAttempt`, never also the plain `Usage`); the turn's
//! usage counters book it as before. A run at its request cap sends no
//! summary. Without a ledger nothing changes.
use super::*;
use crate::tools::ledger_record::{Amount, Ledger, LedgerEntry, MeasuredUsage, RequestPurpose};
use crate::tools::request_ledger::{self, UsageReports};
use crate::tools::RequestLedgerHandle;
use crate::{SessionEvent, StreamEvent};
use agent_core::{LiabilityState, UsageCounts, UsagePurpose, UsageRequest};
use futures::future::BoxFuture;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_util::sync::CancellationToken;

/// A fake summary transport: one reply per call, given the call's cancel.
type Transport = Box<dyn Fn(&CancellationToken) -> BoxFuture<'static, SummaryReply> + Send + Sync>;

const MODEL: &str = "claude-sonnet-4-6";
const CAP: u64 = 4_000;
const WINDOW: u64 = 3;

/// What one screened summary did.
struct Cold {
    out: std::result::Result<String, Option<SummarySkip>>,
    report: SummaryReport,
    /// Summary requests the transport was asked to send.
    calls: usize,
    events: Vec<StreamEvent>,
    /// The turn's usage counters afterwards: still reserved, and measured.
    reserved: f64,
    cost: f64,
    /// What the summary reserves for its request, in micro-USD (rounded
    /// up): the liability it is booked with.
    estimate_micro_usd: u64,
}

impl Cold {
    /// The plain usage events as (input, output, model).
    fn plain_usage(&self) -> Vec<(u64, u64, Option<String>)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Session(SessionEvent::Usage {
                    input_tokens,
                    output_tokens,
                    model,
                    ..
                }) => Some((*input_tokens, *output_tokens, model.clone())),
                _ => None,
            })
            .collect()
    }

    fn attempts(&self) -> Vec<(UsageRequest, UsageCounts)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Session(SessionEvent::UsageAttempt { request, usage }) => {
                    Some((request.clone(), usage.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn liabilities(&self) -> Vec<(UsageRequest, u64, LiabilityState)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Session(SessionEvent::UsageLiability {
                    request,
                    estimate_micro_usd,
                    state,
                }) => Some((request.clone(), *estimate_micro_usd, *state)),
                _ => None,
            })
            .collect()
    }
}

fn entries(ledger: &RequestLedgerHandle) -> Vec<LedgerEntry> {
    ledger.snapshot().entries().cloned().collect()
}

fn dropped() -> Vec<SharedMessage> {
    (0..40)
        .map(|i| {
            Arc::new(json!({"role":"user","content":format!("request {i} {}", "x".repeat(2_000))}))
        })
        .collect()
}

fn archive() -> String {
    "a".repeat(32)
}

/// What the summary of [`dropped`] reserves on `model`, in micro-USD.
fn estimate_for(model: &str) -> u64 {
    let transcript = project("session", &dropped(), &Default::default()).unwrap();
    let body = prompt(None, &transcript, WINDOW, &archive(), CAP);
    request_ledger::micro_usd_ceil(estimated_cost_usd(model, &body, CAP))
}

/// A transport answering every request with `text` and `spend`.
fn answer(
    text: std::result::Result<&'static str, ()>,
    spend: Spend,
) -> impl Fn(&CancellationToken) -> BoxFuture<'static, SummaryReply> + Send + Sync + 'static {
    move |_: &CancellationToken| -> BoxFuture<'static, SummaryReply> {
        let spend = spend.clone();
        Box::pin(async move {
            SummaryReply {
                text: text
                    .map(str::to_owned)
                    .map_err(|()| crate::RuntimeError::ApiStatus("refused".into())),
                spend,
                failure: None,
                stop: None,
            }
        })
    }
}

/// A transport that never answers.
fn silent(_: &CancellationToken) -> BoxFuture<'static, SummaryReply> {
    Box::pin(futures::future::pending())
}

/// A transport that cancels the summary while its request is out.
fn cancelling(cancel: &CancellationToken) -> BoxFuture<'static, SummaryReply> {
    cancel.cancel();
    Box::pin(futures::future::pending())
}

fn reported(input: u64, output: u64) -> Spend {
    Spend::Reported(json!({
        "input_tokens": input,
        "output_tokens": output,
        "cache_read_input_tokens": 0,
        "cache_creation_input_tokens": 0,
    }))
}

/// One screened summary of [`dropped`] on `model` through
/// `update_reporting`, booked in `ledger`, its transport answering with
/// `transport`.
async fn cold<F>(
    model: &str,
    ledger: Option<&RequestLedgerHandle>,
    timeout: Duration,
    transport: F,
) -> Cold
where
    F: Fn(&CancellationToken) -> BoxFuture<'static, SummaryReply> + Send + Sync + 'static,
{
    let cancel = CancellationToken::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let (counted, token) = (calls.clone(), cancel.clone());
    let summarizer = RolloverSummarizer::new(
        Arc::new(move |_messages, _ceiling, _gate| {
            counted.fetch_add(1, Ordering::SeqCst);
            transport(&token)
        }),
        Default::default(),
        model.into(),
    )
    .with_bounded_probe(|| true);
    let dropped = dropped();
    let archive = archive();
    let usage = super::super::budget::UsageCounters::default();
    let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut report = SummaryReport::default();
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        update_reporting(
            &summarizer,
            SummaryRequest {
                logical_id: "session",
                previous: None,
                dropped: &dropped,
                window: WINDOW,
                archive_id: &archive,
                cap_tokens: CAP,
                timeout,
                cancel: &cancel,
                accounting: Some(SummaryAccounting {
                    usage: &usage,
                    max_cost_usd: None,
                    events: &events,
                    tap: None,
                    ledger,
                }),
                snapshot: None,
                run_deadline: None,
            },
            &mut report,
            Limits::from_now(timeout),
        ),
    )
    .await
    .expect("the summary is bounded");
    drop(events);
    let mut seen = Vec::new();
    while let Ok(event) = rx.try_recv() {
        seen.push(event);
    }
    Cold {
        out,
        report,
        calls: calls.load(Ordering::SeqCst),
        events: seen,
        reserved: usage.reserved_usd(),
        cost: usage.cost_usd(),
        estimate_micro_usd: estimate_for(model),
    }
}

/// A reported summary is one `rollover_summary` request of the run,
/// measured by its report, and reported as that attempt's usage, never also
/// as the plain usage. The turn's counters book it as before. A second
/// summary is a request of its own.
#[tokio::test]
async fn sb1_a_reported_summary_is_measured_as_its_own_attempt() {
    let ledger = RequestLedgerHandle::new(MODEL);
    let c = cold(
        MODEL,
        Some(&ledger),
        Duration::from_secs(30),
        answer(Ok("## Window 3\nSUMMARY"), reported(30_000, 300)),
    )
    .await;
    assert!(c.out.is_ok(), "{:?}", c.out);
    assert_eq!(c.calls, 1);
    let booked = entries(&ledger);
    assert_eq!(booked.len(), 1, "{booked:?}");
    assert_eq!(booked[0].purpose, RequestPurpose::RolloverSummary);
    assert_eq!(booked[0].model, MODEL);
    assert_eq!(
        booked[0].amount,
        Amount::Measured {
            usage: MeasuredUsage {
                input_tokens: 30_000,
                output_tokens: 300,
                ..MeasuredUsage::default()
            }
        }
    );
    let attempts = c.attempts();
    assert_eq!(attempts.len(), 1, "{:?}", c.events);
    let (request, usage) = &attempts[0];
    assert_eq!(request.request_id, booked[0].request_id.as_str());
    assert_eq!(request.purpose, UsagePurpose::RolloverSummary);
    assert_eq!(request.model, MODEL);
    assert_eq!((usage.input_tokens, usage.output_tokens), (30_000, 300));
    assert!(c.plain_usage().is_empty(), "never also the plain usage");
    assert!(c.liabilities().is_empty(), "{:?}", c.liabilities());
    assert!(c.cost > 0.0, "the turn's counters book it");
    assert_eq!(c.reserved, 0.0);
    assert!(c.report.settled_usd.is_some_and(|usd| usd > 0.0));

    let again = cold(
        MODEL,
        Some(&ledger),
        Duration::from_secs(30),
        answer(Ok("## Window 3\nAGAIN"), reported(10, 20)),
    )
    .await;
    assert!(again.out.is_ok());
    let booked = entries(&ledger);
    assert_eq!(booked.len(), 2, "one request per summary: {booked:?}");
    assert_ne!(booked[0].request_id, booked[1].request_id);
}

/// A summary that was provably not sent, or that a first-party endpoint
/// rejected before generation, is released: nothing is owed.
#[tokio::test]
async fn sb2_a_summary_not_sent_or_rejected_before_generation_is_released() {
    for spend in [Spend::NotDispatched, Spend::RejectedBeforeGeneration] {
        let ledger = RequestLedgerHandle::new(MODEL);
        let c = cold(
            MODEL,
            Some(&ledger),
            Duration::from_secs(30),
            answer(Err(()), spend.clone()),
        )
        .await;
        assert!(c.out.is_err(), "{spend:?}");
        let booked = entries(&ledger);
        assert_eq!(booked.len(), 1, "{spend:?}: {booked:?}");
        assert_eq!(booked[0].purpose, RequestPurpose::RolloverSummary);
        assert_eq!(booked[0].amount, Amount::Released {}, "{spend:?}");
        let liabilities = c.liabilities();
        assert_eq!(liabilities.len(), 1, "{spend:?}: {:?}", c.events);
        assert_eq!(liabilities[0].0.request_id, booked[0].request_id.as_str());
        assert_eq!(liabilities[0].0.purpose, UsagePurpose::RolloverSummary);
        assert_eq!(liabilities[0].1, c.estimate_micro_usd);
        assert_eq!(liabilities[0].2, LiabilityState::Released);
        assert!(c.attempts().is_empty() && c.plain_usage().is_empty());
        assert_eq!((c.reserved, c.cost), (0.0, 0.0), "{spend:?}");
    }
}

/// A summary whose spend is unknown keeps its liability open, for what the
/// summary reserved: a reply without a usable report, a timeout, a cancel
/// while its request is out. On paused time the preparation takes none of
/// the 20 ms timeout, so it expires with the request out, never before the
/// request is sent.
#[tokio::test(start_paused = true)]
async fn sb3_an_unknown_spend_stays_open() {
    let cases: [(&str, Duration, Transport); 3] = [
        (
            "no report",
            Duration::from_secs(30),
            Box::new(answer(Ok("## Window 3\nSUMMARY"), Spend::Unknown)),
        ),
        ("timeout", Duration::from_millis(20), Box::new(silent)),
        ("cancel", Duration::from_secs(30), Box::new(cancelling)),
    ];
    for (case, timeout, transport) in cases {
        let ledger = RequestLedgerHandle::new(MODEL);
        let c = cold(MODEL, Some(&ledger), timeout, move |cancel: &CancellationToken| transport(cancel)).await;
        assert_eq!(c.calls, 1, "{case}");
        let booked = entries(&ledger);
        assert_eq!(booked.len(), 1, "{case}: {booked:?}");
        assert_eq!(
            booked[0].amount,
            Amount::Liability {
                estimate_micro_usd: c.estimate_micro_usd
            },
            "{case}"
        );
        let liabilities = c.liabilities();
        assert_eq!(liabilities.len(), 1, "{case}: {:?}", c.events);
        assert_eq!(liabilities[0].0.request_id, booked[0].request_id.as_str());
        assert_eq!(liabilities[0].1, c.estimate_micro_usd, "{case}");
        assert_eq!(liabilities[0].2, LiabilityState::Open, "{case}");
        assert!(c.attempts().is_empty() && c.plain_usage().is_empty(), "{case}");
        assert!(c.reserved > 0.0, "{case}: the turn keeps its reservation");
    }
}

/// A run at its request cap sends no summary: the summary is skipped with
/// its reason, the turn's reservation is returned, and the ledger and the
/// stream are unchanged.
#[tokio::test]
async fn sb4_a_run_at_its_cap_sends_no_summary() {
    let ledger = RequestLedgerHandle::new(MODEL);
    for _ in 0..Ledger::MAX_ENTRIES {
        ledger
            .reserve(1, UsageReports::Cumulative)
            .unwrap()
            .release();
    }
    let c = cold(
        MODEL,
        Some(&ledger),
        Duration::from_secs(30),
        answer(Ok("## Window 3\nSUMMARY"), reported(30_000, 300)),
    )
    .await;
    assert_eq!(c.calls, 0, "nothing was sent");
    assert_eq!(c.out, Err(Some(SummarySkip::RunFull)));
    assert_eq!(c.report.path, SummaryPath::None);
    assert_eq!((c.reserved, c.cost), (0.0, 0.0));
    assert!(c.events.is_empty(), "{:?}", c.events);
    assert_eq!(entries(&ledger).len(), Ledger::MAX_ENTRIES);
    let skip = SummarySkip::RunFull;
    assert!(skip.is_skip());
    assert_eq!(skip.code(), "run_full");
    assert_eq!(
        skip.notice(MODEL, false),
        format!(
            "Rolling summary skipped: this run reached {} provider requests.",
            Ledger::MAX_ENTRIES
        )
    );
}

/// Without a ledger (every session; workers while rollover is off) the
/// summary reports exactly as before: the plain usage, nothing else. On
/// paused time the 20 ms timeout expires with the request out.
#[tokio::test(start_paused = true)]
async fn sb5_without_a_ledger_the_summary_reports_as_before() {
    let c = cold(
        MODEL,
        None,
        Duration::from_secs(30),
        answer(Ok("## Window 3\nSUMMARY"), reported(30_000, 300)),
    )
    .await;
    assert!(c.out.is_ok());
    assert_eq!(c.events.len(), 1, "{:?}", c.events);
    assert!(
        matches!(
            &c.events[0],
            StreamEvent::Session(SessionEvent::Usage {
                input_tokens: 30_000,
                output_tokens: 300,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_creation_5m: None,
                cache_creation_1h: None,
                model: Some(model),
            }) if model == MODEL
        ),
        "{:?}",
        c.events
    );
    for (spend, timeout) in [
        (Spend::NotDispatched, Duration::from_secs(30)),
        (Spend::Unknown, Duration::from_secs(30)),
    ] {
        let c = cold(MODEL, None, timeout, answer(Ok("## Window 3\nS"), spend.clone())).await;
        assert!(c.events.is_empty(), "{spend:?}: {:?}", c.events);
    }
    let c = cold(MODEL, None, Duration::from_millis(20), silent).await;
    assert_eq!(c.calls, 1, "timeout: the request was sent before it timed out");
    assert!(c.events.is_empty(), "timeout: {:?}", c.events);
}

/// A summary model whose route's usage reports are not established (a cloud
/// route) keeps its liability open even when it reports: the report is the
/// attempt's usage, the liability stays.
#[tokio::test]
async fn sb6_a_summary_model_that_cant_book_stays_open_even_when_reported() {
    let descriptor = crate::auth::cloud::cloud_provider_descriptors()
        .first()
        .expect("a cloud route");
    let model = format!("{}/some-model", descriptor.id);
    assert!(
        !crate::runtime::openai::books_requests(&model),
        "{model} is a route that can't book"
    );
    let ledger = RequestLedgerHandle::new(MODEL);
    let c = cold(
        &model,
        Some(&ledger),
        Duration::from_secs(30),
        answer(Ok("## Window 3\nSUMMARY"), reported(30_000, 300)),
    )
    .await;
    assert!(c.out.is_ok(), "{:?}", c.out);
    let booked = entries(&ledger);
    assert_eq!(booked.len(), 1, "{booked:?}");
    assert_eq!(booked[0].purpose, RequestPurpose::RolloverSummary);
    assert_eq!(booked[0].model, model);
    assert_eq!(
        booked[0].amount,
        Amount::Liability {
            estimate_micro_usd: c.estimate_micro_usd
        }
    );
    let attempts = c.attempts();
    assert_eq!(attempts.len(), 1, "{:?}", c.events);
    assert_eq!((attempts[0].1.input_tokens, attempts[0].1.output_tokens), (30_000, 300));
    let liabilities = c.liabilities();
    assert_eq!(liabilities.len(), 1);
    assert_eq!(liabilities[0].2, LiabilityState::Open);
    assert!(c.plain_usage().is_empty());
}

/// Tripwire: no rollover summary goes to the Codex route. Its output
/// ceiling is not enforceable, so `summary_model_unusable` refuses it for
/// every summarizer (explicit, and the session or compaction model under
/// `auto`); the warm path is native Anthropic only. That is why a summary
/// never settles from the Codex reply's usage, which keeps the last
/// non-empty report where a ledgered conversation attempt settles on the
/// last terminal event. If this fails, settle a ledgered Codex summary from
/// the decoder's last terminal report before allowing it.
#[test]
fn sb7_a_codex_model_is_never_a_summary_model() {
    let codex = "openai-codex/gpt-5.1-codex-mini";
    let route = crate::runtime::openai::resolve_route(codex).expect("a Codex route");
    assert_eq!(route.wire, crate::runtime::openai::WireProtocol::CodexResponses);
    assert!(
        !crate::runtime::openai::output_ceiling_enforceable(&route),
        "a Codex summary would settle from a stale report: see this test's doc"
    );
    let mut rt = crate::Runtime::new_headless();
    rt.set_model(codex.into());
    rt.set_summary_model(Some(codex.into()));
    assert_eq!(
        rt.summary_model_choice().err(),
        Some(SummarySkip::NoEnforceableCeiling)
    );
}
