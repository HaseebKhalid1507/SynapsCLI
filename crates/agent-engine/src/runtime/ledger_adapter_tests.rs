//! The transports' request-ledger hooks
//! (`docs/specs/subagent-context-rollover.md` §6, slice 6, dark): a turn on a
//! runtime with a request ledger (a v2 worker run) books each upstream
//! attempt in the run's ledger, reports its usage as that attempt's
//! (`UsageAttempt`, never also the plain `Usage`), and reports an attempt
//! whose spend is not measured as a liability. The requests themselves are
//! the same bytes the off gate pins.
use std::sync::Arc;

use futures::StreamExt;

use super::*;
use crate::tools::request_ledger::UsageReports;
use crate::tools::ledger_record::{Amount, Ledger, LedgerEntry, RequestPurpose};
use crate::tools::{LedgerSlot, RequestLedgerHandle};
use crate::{SessionEvent, StreamEvent};
use agent_core::LiabilityState;

/// What a ledgered turn emitted and booked.
struct Booked {
    events: Vec<StreamEvent>,
    ledger: RequestLedgerHandle,
}

impl Booked {
    fn entries(&self) -> Vec<LedgerEntry> {
        self.ledger.snapshot().entries().cloned().collect()
    }

    fn plain_usage(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Session(SessionEvent::Usage { .. })))
            .count()
    }

    /// The attempts' usage events as (request ID, input, output).
    fn attempts(&self) -> Vec<(String, u64, u64)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Session(SessionEvent::UsageAttempt { request, usage }) => Some((
                    request.request_id.clone(),
                    usage.input_tokens,
                    usage.output_tokens,
                )),
                _ => None,
            })
            .collect()
    }

    /// The liability events as (request ID, state).
    fn liabilities(&self) -> Vec<(String, LiabilityState)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Session(SessionEvent::UsageLiability { request, state, .. }) => {
                    Some((request.request_id.clone(), *state))
                }
                _ => None,
            })
            .collect()
    }

    fn error(&self) -> Option<String> {
        self.events.iter().find_map(|e| match e {
            StreamEvent::Session(SessionEvent::Error(e)) => Some(format!("{e:?}")),
            _ => None,
        })
    }
}

/// A runtime with the run's ledger installed, as a v2 worker run's.
fn ledgered_runtime(model: &str, ledger: &RequestLedgerHandle) -> crate::Runtime {
    let mut rt = crate::Runtime::new_headless();
    rt.set_model(model.to_owned());
    rt.set_api_retries(1);
    rt.set_tools(crate::tools::ToolRegistry::from_tools_for_tests(vec![
        Arc::new(crate::tools::ReadTool),
        Arc::new(crate::tools::LsTool),
    ]));
    rt.continuation.lock().unwrap().config.mode = ContextManagementMode::Off;
    let slot = LedgerSlot::default();
    rt.arm_request_ledger(slot.clone());
    assert!(slot.install(ledger.clone()));
    rt
}

async fn ledgered_turn(model: &str, ledger: RequestLedgerHandle) -> Booked {
    turn_on(ledgered_runtime(model, &ledger), ledger).await
}

/// One turn on `rt`, whose ledger is `ledger`.
async fn turn_on(rt: crate::Runtime, ledger: RequestLedgerHandle) -> Booked {
    let mut stream = rt
        .run_stream_with_messages(history(), Default::default(), None, None, false)
        .await;
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    Booked { events, ledger }
}

/// Run one ledgered turn on `model` against `reply` (the route pointed at
/// the stand-in through `env`); the requests the stand-in received must be
/// the off gate's fixture `gate_name` (`None`: nothing to compare).
async fn run(
    model: &str,
    env: &'static str,
    path: &str,
    gate_name: Option<&str>,
    reply: impl Fn(usize) -> axum::response::Response + Clone + Send + Sync + 'static,
) -> (Booked, usize) {
    let _base = crate::test_env::BaseDirGuard::new();
    let (url, seen) = stand_in(reply).await;
    let _url = crate::test_env::EnvVarGuard::set(env, &format!("{url}{path}"));
    let booked = ledgered_turn(model, RequestLedgerHandle::new(model)).await;
    let seen = seen.lock().unwrap();
    if let Some(name) = gate_name {
        check(&format!("{name}_requests.txt"), &render_requests(&seen));
    }
    (booked, seen.len())
}

fn measured(entry: &LedgerEntry) -> (u64, u64) {
    match &entry.amount {
        Amount::Measured { usage } => (usage.input_tokens, usage.output_tokens),
        other => panic!("{} is not measured: {other:?}", entry.request_id.as_str()),
    }
}

fn is_open(entry: &LedgerEntry) -> bool {
    matches!(entry.amount, Amount::Liability { estimate_micro_usd } if estimate_micro_usd > 0)
}

/// L15: a streamed Anthropic turn books its one attempt, measured from the
/// attempt's one cumulative report (`message_delta`), reported as the
/// attempt's usage and never also as plain usage; the request is the same
/// bytes as without a ledger.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn la1_an_anthropic_attempt_is_measured_from_its_one_report() {
    let (b, sent) = run(
        "claude-sonnet-4-6",
        "SYNAPS_ANTHROPIC_BASE_URL",
        "",
        Some("anthropic"),
        |_| stream_ok(anthropic_sse()),
    )
    .await;
    assert_eq!(sent, 1);
    assert_eq!(b.error(), None);
    let entries = b.entries();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(measured(&entries[0]), (12, 3));
    assert_eq!(entries[0].purpose, RequestPurpose::Conversation);
    assert_eq!(entries[0].model, "claude-sonnet-4-6");
    match &entries[0].amount {
        Amount::Measured { usage } => {
            assert_eq!((usage.cache_read_tokens, usage.cache_write_tokens), (4, 5));
            assert_eq!((usage.cache_write_5m, usage.cache_write_1h), (Some(5), Some(0)));
        }
        _ => unreachable!(),
    }
    let id = entries[0].request_id.as_str().to_owned();
    assert_eq!(b.attempts(), vec![(id, 12, 3)]);
    assert_eq!(b.plain_usage(), 0, "never both");
    assert!(b.liabilities().is_empty());
}

/// L15: a rate-limited attempt is not proof of zero spend: it stays an open
/// liability (and says so); its retry is its own measured attempt.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn la2_a_rate_limited_attempt_stays_open_and_its_retry_is_its_own() {
    let (b, sent) = run(
        "claude-sonnet-4-6",
        "SYNAPS_ANTHROPIC_BASE_URL",
        "",
        Some("anthropic_retry"),
        |n| match n {
            0 => rate_limited(anthropic_429()),
            _ => stream_ok(anthropic_sse()),
        },
    )
    .await;
    assert_eq!(sent, 2);
    assert_eq!(b.error(), None);
    let entries = b.entries();
    assert_eq!(entries.len(), 2, "one entry per attempt: {entries:?}");
    let (first, second) = (&entries[0], &entries[1]);
    assert!(is_open(first), "{first:?}");
    assert_eq!(measured(second), (12, 3));
    assert_eq!(
        b.liabilities(),
        vec![(first.request_id.as_str().to_owned(), LiabilityState::Open)]
    );
    assert_eq!(b.attempts(), vec![(second.request_id.as_str().to_owned(), 12, 3)]);
    assert_eq!(b.plain_usage(), 0);
}

/// L15: a stream the provider ends with an overload error is its own
/// attempt; what its `message_start` reported is a running count, so the
/// attempt stays open (its usage is still reported as before, as the
/// attempt's); the retry is measured.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn la3_a_stream_retry_is_one_entry_per_attempt() {
    let (b, sent) = run(
        "claude-sonnet-4-6",
        "SYNAPS_ANTHROPIC_BASE_URL",
        "",
        Some("anthropic_stream_retry"),
        |n| match n {
            0 => stream_ok(anthropic_overloaded_sse()),
            _ => stream_ok(anthropic_sse()),
        },
    )
    .await;
    assert_eq!(sent, 2);
    assert_eq!(b.error(), None);
    let entries = b.entries();
    assert_eq!(entries.len(), 2, "{entries:?}");
    let (first, second) = (&entries[0], &entries[1]);
    assert!(is_open(first), "{first:?}");
    assert_eq!(measured(second), (12, 3));
    assert_eq!(
        b.attempts(),
        vec![
            (first.request_id.as_str().to_owned(), 12, 0),
            (second.request_id.as_str().to_owned(), 12, 3),
        ]
    );
    assert_eq!(
        b.liabilities(),
        vec![(first.request_id.as_str().to_owned(), LiabilityState::Open)]
    );
    assert_eq!(b.plain_usage(), 0);
}

/// L5/L20: the conversation client follows redirects, so a refused
/// connection is no proof that nothing was sent (a first hop may have
/// received the request, as the broker types it): every attempt stays an
/// open liability, and each retry is its own attempt.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn la4_a_refused_connection_stays_open_on_a_redirecting_client() {
    let _base = crate::test_env::BaseDirGuard::new();
    // A port nobody listens on.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let model = "claude-sonnet-4-6";
    let b = ledgered_turn(model, RequestLedgerHandle::new(model)).await;
    assert!(b.error().is_some(), "the turn fails");
    let entries = b.entries();
    assert_eq!(entries.len(), 2, "two attempts (one retry): {entries:?}");
    assert!(entries.iter().all(is_open), "{entries:?}");
    assert_eq!(
        b.liabilities(),
        entries
            .iter()
            .map(|e| (e.request_id.as_str().to_owned(), LiabilityState::Open))
            .collect::<Vec<_>>()
    );
    assert!(b.attempts().is_empty());
}

/// L6: a run at its request cap sends nothing more: the turn ends with the
/// typed reason and the provider sees no request.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn la5_a_run_at_its_cap_sends_no_request() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (url, seen) = stand_in(|_| stream_ok(anthropic_sse())).await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let model = "claude-sonnet-4-6";
    let ledger = RequestLedgerHandle::new(model);
    for _ in 0..Ledger::MAX_ENTRIES {
        ledger.reserve(1, UsageReports::Cumulative).unwrap().release();
    }
    let b = ledgered_turn(model, ledger).await;
    assert_eq!(seen.lock().unwrap().len(), 0, "nothing was sent");
    let error = b.error().expect("the turn fails");
    assert!(
        error.contains("this run reached 4096 provider requests"),
        "{error}"
    );
    assert_eq!(b.entries().len(), Ledger::MAX_ENTRIES);
    assert!(b.attempts().is_empty() && b.liabilities().is_empty());
}

/// An Anthropic stream whose `message_start` reports `start` and whose
/// `message_delta` reports `delta` (each a usage object as sent).
fn anthropic_sse_reporting(start: serde_json::Value, delta: serde_json::Value) -> String {
    [
        json!({"type":"message_start","message":{"id":"msg_gate","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":start}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"gate ok"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":delta}),
        json!({"type":"message_stop"}),
    ]
    .iter()
    .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
    .collect()
}

/// One ledgered Anthropic turn against a stream reporting `start` / `delta`.
async fn anthropic_reporting(start: serde_json::Value, delta: serde_json::Value) -> Booked {
    let sse = anthropic_sse_reporting(start, delta);
    let (b, sent) = run(
        "claude-sonnet-4-6",
        "SYNAPS_ANTHROPIC_BASE_URL",
        "",
        None,
        move |_| stream_ok(sse.clone()),
    )
    .await;
    assert_eq!(sent, 1);
    assert_eq!(b.error(), None);
    assert_eq!(b.entries().len(), 1, "{:?}", b.entries());
    b
}

fn cache_counts(entry: &LedgerEntry) -> (u64, u64) {
    match &entry.amount {
        Amount::Measured { usage } => (usage.cache_read_tokens, usage.cache_write_tokens),
        other => panic!("{} is not measured: {other:?}", entry.request_id.as_str()),
    }
}

/// L3: an output count reported neither by `message_start` nor by
/// `message_delta` is unknown, not zero: the attempt stays open. Its report
/// is still the attempt's, as it was reported (zero-filled for display).
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn la6_a_report_without_the_output_count_stays_open() {
    let usage = json!({"input_tokens":12,"cache_read_input_tokens":4,"cache_creation_input_tokens":5});
    let b = anthropic_reporting(usage.clone(), usage).await;
    let entry = &b.entries()[0];
    assert!(is_open(entry), "{entry:?}");
    let id = entry.request_id.as_str().to_owned();
    assert_eq!(b.liabilities(), vec![(id.clone(), LiabilityState::Open)]);
    assert_eq!(b.attempts(), vec![(id, 12, 0)]);
    assert_eq!(b.plain_usage(), 0);
}

/// L3: a final report that states every count as zero is a measured zero,
/// not an unknown: the attempt settles at no spend. (An all-zero report
/// is no usage to display, as before: no usage event.)
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn la7_an_explicit_all_zero_report_is_measured_as_zero() {
    let usage = json!({"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0});
    let b = anthropic_reporting(usage.clone(), usage).await;
    let entry = &b.entries()[0];
    assert_eq!(measured(entry), (0, 0));
    assert_eq!(cache_counts(entry), (0, 0));
    assert!(b.liabilities().is_empty());
    assert!(b.attempts().is_empty());
    assert_eq!(b.plain_usage(), 0);
}

/// L3: the output count on `message_start` is a running count; an explicit
/// zero on `message_delta` is the final one and the attempt settles on it.
/// The display event is the unchanged legacy emission (which falls back to
/// the start's count for a zero), so only the ledger differs from it.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn la8_an_explicit_zero_output_on_the_delta_is_the_final_count() {
    let start = json!({"input_tokens":12,"output_tokens":3,"cache_read_input_tokens":4,"cache_creation_input_tokens":5});
    let delta = json!({"input_tokens":12,"output_tokens":0,"cache_read_input_tokens":4,"cache_creation_input_tokens":5});
    let b = anthropic_reporting(start, delta).await;
    let entry = &b.entries()[0];
    assert_eq!(measured(entry), (12, 0));
    assert_eq!(cache_counts(entry), (4, 5));
    assert!(b.liabilities().is_empty());
    let id = entry.request_id.as_str().to_owned();
    assert_eq!(b.attempts(), vec![(id, 12, 3)], "display unchanged");
}

/// L3: a sparse `message_delta` carrying only the final output count settles
/// with the counts `message_start` stated for the rest.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn la9_a_sparse_delta_settles_with_the_starts_other_counts() {
    let start = json!({"input_tokens":12,"output_tokens":1,"cache_read_input_tokens":4,"cache_creation_input_tokens":5});
    let b = anthropic_reporting(start, json!({"output_tokens":3})).await;
    let entry = &b.entries()[0];
    assert_eq!(measured(entry), (12, 3));
    assert_eq!(cache_counts(entry), (4, 5));
    assert!(b.liabilities().is_empty());
}

/// L3: a cache count reported by neither event is unknown: the attempt
/// stays open (the price needs it).
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn la10_a_report_without_a_cache_count_stays_open() {
    let usage = json!({"input_tokens":12,"output_tokens":3,"cache_read_input_tokens":4});
    let b = anthropic_reporting(usage.clone(), usage).await;
    let entry = &b.entries()[0];
    assert!(is_open(entry), "{entry:?}");
    let id = entry.request_id.as_str().to_owned();
    assert_eq!(b.liabilities(), vec![(id.clone(), LiabilityState::Open)]);
    assert_eq!(b.attempts(), vec![(id, 12, 3)]);
}

/// A Chat Completions stream with two usage reports: a running count, then
/// the request's whole usage (some compatible servers report usage on
/// several chunks; each is the running total).
fn chat_two_reports_sse() -> String {
    [
        json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"gate "},"finish_reason":null}],"usage":{"prompt_tokens":12,"completion_tokens":1,"total_tokens":13,"prompt_tokens_details":{"cached_tokens":4}}}),
        json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}),
        json!({"id":"c","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":4}}}),
    ]
    .iter()
    .map(|c| format!("data: {c}\n\n"))
    .chain(std::iter::once("data: [DONE]\n\n".to_owned()))
    .collect()
}

/// A complete Chat Completions stream whose usage report lacks the output
/// count.
fn chat_partial_report_sse() -> String {
    [
        json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"gate ok"},"finish_reason":"stop"}]}),
        json!({"id":"c","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":12,"total_tokens":12}}),
    ]
    .iter()
    .map(|c| format!("data: {c}\n\n"))
    .chain(std::iter::once("data: [DONE]\n\n".to_owned()))
    .collect()
}

const CHAT: &str = "local/test-model";

/// L16: a Chat Completions attempt is measured from its last usage report
/// (cumulative: the request's whole usage), reported as the attempt's and
/// never also as plain usage; the request is the off gate's bytes.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn lo1_a_chat_attempt_is_measured_from_its_report() {
    let (b, sent) = run(CHAT, "LOCAL_ENDPOINT", "/v1", Some("openai_chat"), |_| {
        stream_ok(chat_sse())
    })
    .await;
    assert_eq!(sent, 1);
    assert_eq!(b.error(), None);
    let entries = b.entries();
    assert_eq!(entries.len(), 1, "{entries:?}");
    // prompt_tokens includes the cached slice: 12 = 8 input + 4 cache read.
    assert_eq!(measured(&entries[0]), (8, 3));
    match &entries[0].amount {
        Amount::Measured { usage } => assert_eq!(usage.cache_read_tokens, 4),
        _ => unreachable!(),
    }
    assert_eq!(entries[0].purpose, RequestPurpose::Conversation);
    let id = entries[0].request_id.as_str().to_owned();
    assert_eq!(b.attempts(), vec![(id, 8, 3)]);
    assert_eq!(b.plain_usage(), 0, "never both");
    assert!(b.liabilities().is_empty());
}

/// L16: several usage reports in one attempt are running totals: the
/// attempt is settled once, with the last, never with their sum.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn lo2_several_chat_reports_settle_with_the_last() {
    let (b, sent) = run(CHAT, "LOCAL_ENDPOINT", "/v1", None, |_| {
        stream_ok(chat_two_reports_sse())
    })
    .await;
    assert_eq!(sent, 1);
    assert_eq!(b.error(), None);
    let entries = b.entries();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(measured(&entries[0]), (8, 3), "the last report, not the sum");
    let id = entries[0].request_id.as_str().to_owned();
    assert_eq!(b.attempts(), vec![(id.clone(), 8, 1), (id, 8, 3)]);
    assert_eq!(b.plain_usage(), 0);
    assert!(b.liabilities().is_empty());
}

/// L15: a stream cut off before it completed is its own attempt, open
/// (whatever it reported is no final count); its retry is measured.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn lo3_a_cut_chat_stream_stays_open_and_its_retry_is_its_own() {
    let (b, sent) = run(
        CHAT,
        "LOCAL_ENDPOINT",
        "/v1",
        Some("openai_chat_stream_retry"),
        |n| match n {
            0 => stream_ok(chat_cut_sse()),
            _ => stream_ok(chat_sse()),
        },
    )
    .await;
    assert_eq!(sent, 2);
    assert_eq!(b.error(), None);
    let entries = b.entries();
    assert_eq!(entries.len(), 2, "{entries:?}");
    let (first, second) = (&entries[0], &entries[1]);
    assert!(is_open(first), "{first:?}");
    assert_eq!(measured(second), (8, 3));
    assert_eq!(
        b.liabilities(),
        vec![(first.request_id.as_str().to_owned(), LiabilityState::Open)]
    );
    assert_eq!(b.attempts(), vec![(second.request_id.as_str().to_owned(), 8, 3)]);
    assert_eq!(b.plain_usage(), 0);
}

/// L15: an error status on this route (a broker-proxied provider) is no
/// evidence of zero spend: the attempt stays open.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn lo4_a_rate_limited_chat_attempt_stays_open() {
    let (b, sent) = run(
        CHAT,
        "LOCAL_ENDPOINT",
        "/v1",
        Some("openai_chat_rate_limit"),
        |_| rate_limited(chat_429()),
    )
    .await;
    assert_eq!(sent, 1);
    assert!(b.error().is_some(), "the turn fails");
    let entries = b.entries();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert!(is_open(&entries[0]), "{entries:?}");
    assert_eq!(
        b.liabilities(),
        vec![(entries[0].request_id.as_str().to_owned(), LiabilityState::Open)]
    );
    assert!(b.attempts().is_empty());
}

/// L3: a report without the output count does not state the attempt's
/// spend: the attempt stays open (the report is still the attempt's).
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn lo5_a_chat_report_without_its_output_count_stays_open() {
    let (b, _) = run(CHAT, "LOCAL_ENDPOINT", "/v1", None, |_| {
        stream_ok(chat_partial_report_sse())
    })
    .await;
    assert_eq!(b.error(), None);
    let entries = b.entries();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert!(is_open(&entries[0]), "{entries:?}");
    let id = entries[0].request_id.as_str().to_owned();
    assert_eq!(b.attempts(), vec![(id.clone(), 12, 0)]);
    assert_eq!(b.liabilities(), vec![(id, LiabilityState::Open)]);
    assert_eq!(b.plain_usage(), 0);
}

/// L5/L20: the broker's typed pre-send failure (here: no connection to a
/// remote broker) proves nothing reached the provider: released.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn lo6_a_typed_pre_send_broker_failure_is_released() {
    let _base = crate::test_env::BaseDirGuard::new();
    let ledger = RequestLedgerHandle::new(CHAT);
    let mut rt = ledgered_runtime(CHAT, &ledger);
    rt.credential_source = crate::auth::CredentialSource::Remote {
        endpoint: closed_port_url(),
        machine_token: "not-a-real-token".into(),
    };
    let b = turn_on(rt, ledger).await;
    assert!(b.error().is_some(), "the turn fails");
    let entries = b.entries();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].amount, Amount::Released {});
    assert_eq!(
        b.liabilities(),
        vec![(entries[0].request_id.as_str().to_owned(), LiabilityState::Released)]
    );
    assert!(b.attempts().is_empty());
}

/// §6 backstop: a route whose transport does not book its attempts yet
/// refuses a ledgered request before anything is sent (a v2 run never
/// sends a request its ledger cannot account for). The broker is a closed
/// port, so nothing could leave this host either way.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn lb1_an_unhooked_route_refuses_a_ledgered_request() {
    for model in [
        "xai-auth/grok-4.5",
        "aws-bedrock/anthropic.claude-sonnet-4-6",
        "some-plugin:provider:model",
    ] {
        let _base = crate::test_env::BaseDirGuard::new();
        let ledger = RequestLedgerHandle::new(model);
        let mut rt = ledgered_runtime(model, &ledger);
        let source = crate::auth::CredentialSource::Remote {
            endpoint: closed_port_url(),
            machine_token: "not-a-real-token".into(),
        };
        rt.credential_source = source.clone();
        // A fresh token, so a route the turn takes for Anthropic's (an
        // extension provider's ID) gets past the token check to the route.
        // Bound to the default seat of this source, as the live auth's
        // binding check needs for its fast path (unbound, it re-vends
        // through the broker, here a closed port).
        *rt.auth.write().await = crate::runtime::types::AuthState {
            auth_token: "test-token".into(),
            auth_type: "api_key".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
            bound_credential: Some(crate::runtime::auth::anthropic_binding(
                &source,
                &crate::auth::CredentialRef::default_for(crate::auth::OAuthProviderId::Anthropic),
            )),
        };
        let b = turn_on(rt, ledger).await;
        let error = b.error().expect("the turn fails");
        assert!(
            error.contains("can't book its requests in the run's request ledger"),
            "{model}: {error}"
        );
        assert!(b.entries().is_empty(), "{model}: nothing reserved");
        assert!(b.attempts().is_empty() && b.liabilities().is_empty());
    }
}

/// §6: the Codex route books its attempts (its own tests drive the
/// transport, which has no base-URL override): a ledgered Codex turn is
/// admitted past the backstop and reserves nothing before its send. The
/// broker is a closed port, so the turn fails at the credential, before
/// any request could be sent.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn lb2_a_codex_turn_is_admitted_and_books_only_at_its_send() {
    let _base = crate::test_env::BaseDirGuard::new();
    let model = "openai-codex/gpt-5.5";
    let ledger = RequestLedgerHandle::new(model);
    let mut rt = ledgered_runtime(model, &ledger);
    rt.credential_source = crate::auth::CredentialSource::Remote {
        endpoint: closed_port_url(),
        machine_token: "not-a-real-token".into(),
    };
    let b = turn_on(rt, ledger).await;
    let error = b.error().expect("the turn fails at the credential");
    assert!(
        !error.contains("can't book its requests in the run's request ledger"),
        "{error}"
    );
    assert!(b.entries().is_empty(), "nothing reserved before a send");
    assert!(b.attempts().is_empty() && b.liabilities().is_empty());
}

/// A loopback URL nobody listens on.
fn closed_port_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    url
}

/// sa_40 item 1, behavioural: a oneshot worker, the sibling closest to a v2
/// run, gets no request ledger. Its runtime is built as `subagent` builds
/// it (`spawn_runtime`, the worker policy, the model) and its stream
/// carries the plain `Usage` and neither ledger event.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn lg1_a_oneshot_workers_stream_carries_no_ledger_events() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (url, seen) = stand_in(|_| stream_ok(anthropic_sse())).await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let model = "claude-sonnet-4-6";
    let mut rt = crate::tools::spawn_runtime().await.expect("a worker runtime");
    crate::tools::apply_subagent_runtime_policy(
        &mut rt,
        &crate::config::load_config(),
        None,
        None,
        None,
        &Default::default(),
    );
    rt.set_model(model.to_owned());
    // Credentials don't matter here: a fresh test token, as a configured
    // worker would have.
    *rt.auth.write().await = crate::runtime::types::AuthState {
        auth_token: "test-token".into(),
        auth_type: "api_key".into(),
        refresh_token: None,
        token_expires: Some(9_999_999_999_999),
        auth_file_mtime: None,
        bound_credential: None,
    };
    assert!(rt.request_ledger().is_none(), "a oneshot runtime has no ledger");
    let mut stream = rt.run_stream("Say ok".into(), crate::CancellationToken::new()).await;
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    assert_eq!(seen.lock().unwrap().len(), 1, "one request");
    let plain: Vec<(u64, u64)> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Session(SessionEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            }) => Some((*input_tokens, *output_tokens)),
            _ => None,
        })
        .collect();
    assert_eq!(plain, vec![(12, 3)], "the plain usage, as before");
    assert!(
        !events.iter().any(|e| matches!(
            e,
            StreamEvent::Session(
                SessionEvent::UsageAttempt { .. } | SessionEvent::UsageLiability { .. }
            )
        )),
        "no ledger event on a oneshot stream"
    );
}
