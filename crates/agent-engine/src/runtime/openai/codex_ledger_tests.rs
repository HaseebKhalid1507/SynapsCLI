//! The Codex transport's request-ledger hook
//! (`docs/specs/subagent-context-rollover.md` §6, slice 6, dark): a
//! ledgered Codex request books each HTTP send as its own attempt, reports
//! the attempt's usage as that attempt's (`UsageAttempt`, never also the
//! plain `Usage`), settles it by its terminal report, and keeps every other
//! attempt an open liability. Without a ledger nothing changes: the same
//! request bytes, the same plain `Usage`.
//!
//! The Codex route has no base-URL override, so these call the transport
//! directly against a loopback stand-in.
use std::sync::{Arc, Mutex};

use agent_core::auth::{AccessToken, BrokerError, ProxyByteStream, ProxyRequest, ProxyResponse};
use agent_core::LiabilityState;
use async_trait::async_trait;
use axum::{http::StatusCode, response::IntoResponse, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

use super::*;
use crate::runtime::types::SessionEvent;
use crate::tools::ledger_record::{Amount, Ledger, LedgerEntry, RequestPurpose};
use crate::tools::request_ledger::UsageReports;
use crate::tools::RequestLedgerHandle;

const MODEL: &str = "openai-codex/gpt-5.6-sol";

/// A JWT-shaped token carrying the ChatGPT account-id claim.
fn token() -> String {
    let payload = json!({ "https://api.openai.com/auth": { "chatgpt_account_id": "acct_test" } });
    format!("h.{}.s", URL_SAFE_NO_PAD.encode(payload.to_string()))
}

/// A broker that vends the token and nothing else.
struct TokenBroker;

#[async_trait]
impl crate::auth::CredentialBroker for TokenBroker {
    async fn access_token(
        &self,
        _p: agent_core::auth::OAuthProviderId,
    ) -> Result<AccessToken, BrokerError> {
        Ok(AccessToken {
            token: token(),
            expires: u64::MAX,
        })
    }
    async fn proxy(&self, _request: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
        Err(BrokerError::Denied("not in this test".into()))
    }
    async fn proxy_stream(&self, _request: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
        Err(BrokerError::Denied("not in this test".into()))
    }
    async fn anthropic_usage(&self) -> Result<Value, BrokerError> {
        Err(BrokerError::Denied("not in this test".into()))
    }
    async fn capabilities(&self) -> Result<Vec<agent_core::auth::ProviderStatus>, BrokerError> {
        Ok(vec![])
    }
}

type Bodies = Arc<Mutex<Vec<bytes::Bytes>>>;

/// A Codex stand-in answering its `n`th request with `reply(n)`; returns its
/// base URL and the request bodies it received.
async fn stand_in(
    reply: impl Fn(usize) -> axum::response::Response + Clone + Send + Sync + 'static,
) -> (String, Bodies) {
    let bodies: Bodies = Arc::default();
    let record = Arc::clone(&bodies);
    let app = Router::new().route(
        "/codex/responses",
        axum::routing::post(move |body: bytes::Bytes| {
            let record = Arc::clone(&record);
            let reply = reply.clone();
            async move {
                let n = {
                    let mut seen = record.lock().unwrap();
                    seen.push(body);
                    seen.len() - 1
                };
                reply(n)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), bodies)
}

fn sse(events: &[Value]) -> axum::response::Response {
    let body: String = events
        .iter()
        .map(|e| format!("data: {e}\n\n"))
        .chain(std::iter::once("data: [DONE]\n\n".to_owned()))
        .collect();
    (StatusCode::OK, [("content-type", "text/event-stream")], body).into_response()
}

/// A stream cut after `events`: no terminal event, no `[DONE]`.
fn cut(events: &[Value]) -> axum::response::Response {
    let body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
    (StatusCode::OK, [("content-type", "text/event-stream")], body).into_response()
}

fn status(code: StatusCode) -> axum::response::Response {
    (
        code,
        [("content-type", "application/json")],
        "{\"error\":{\"message\":\"no\"}}",
    )
        .into_response()
}

fn text() -> Value {
    json!({"type":"response.output_text.delta","delta":"hello"})
}

/// A terminal event `kind` reporting `input` tokens (of which `cached` were
/// read from the cache) and `output` tokens.
fn report(kind: &str, input: u64, cached: u64, output: u64) -> Value {
    json!({"type": kind, "response": {"usage": {
        "input_tokens": input,
        "input_tokens_details": {"cached_tokens": cached},
        "output_tokens": output,
    }}})
}

/// A complete response: text, then its one usage report.
fn ok() -> axum::response::Response {
    sse(&[text(), report("response.completed", 10, 4, 3)])
}

/// What one Codex request emitted and booked.
struct Booked {
    result: Result<Value, Box<dyn std::error::Error + Send + Sync>>,
    events: Vec<StreamEvent>,
    ledger: Option<RequestLedgerHandle>,
}

impl Booked {
    fn entries(&self) -> Vec<LedgerEntry> {
        self.ledger
            .as_ref()
            .expect("a ledgered request")
            .snapshot()
            .entries()
            .cloned()
            .collect()
    }

    fn plain_usage(&self) -> Vec<(u64, u64, u64)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Session(SessionEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cache_read_input_tokens,
                    ..
                }) => Some((*input_tokens, *output_tokens, *cache_read_input_tokens)),
                _ => None,
            })
            .collect()
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
}

fn config(base_url: &str) -> ProviderConfig {
    ProviderConfig {
        base_url: base_url.to_owned(),
        model: "gpt-5.6-sol".to_owned(),
        provider: "openai-codex".to_owned(),
    }
}

/// One Codex request against `base_url`, booked in `ledger` when set.
async fn request(
    base_url: &str,
    max_retries: u32,
    ledger: Option<RequestLedgerHandle>,
    cancel: &tokio_util::sync::CancellationToken,
    tx: &mpsc::UnboundedSender<StreamEvent>,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let broker: Arc<dyn crate::auth::CredentialBroker> = Arc::new(TokenBroker);
    call_codex_stream_inner(
        &config(base_url),
        &reqwest::Client::new(),
        &broker,
        &[],
        &Some("test".to_owned()),
        &[],
        tx,
        None,
        None,
        agent_core::reasoning::ReasoningLevel::Medium,
        crate::runtime::openai::catalog::CodexRequestRole::Foreground,
        cancel,
        max_retries,
        &crate::runtime::trace::TraceContext::disabled(),
        crate::runtime::child_spend::RouteSpend::NONE,
        ledger.as_ref(),
    )
    .await
}

async fn run(base_url: &str, max_retries: u32, ledger: Option<RequestLedgerHandle>) -> Booked {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let cancel = tokio_util::sync::CancellationToken::new();
    let result = request(base_url, max_retries, ledger.clone(), &cancel, &tx).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    Booked {
        result,
        events,
        ledger,
    }
}

async fn ledgered(base_url: &str, max_retries: u32) -> Booked {
    run(base_url, max_retries, Some(RequestLedgerHandle::new(MODEL))).await
}

fn measured(entry: &LedgerEntry) -> (u64, u64, u64) {
    match &entry.amount {
        Amount::Measured { usage } => (usage.input_tokens, usage.output_tokens, usage.cache_read_tokens),
        other => panic!("{} is not measured: {other:?}", entry.request_id.as_str()),
    }
}

fn is_open(entry: &LedgerEntry) -> bool {
    matches!(entry.amount, Amount::Liability { estimate_micro_usd } if estimate_micro_usd > 0)
}

fn id(entry: &LedgerEntry) -> String {
    entry.request_id.as_str().to_owned()
}

/// The off gate for this route: without a ledger the usage is the plain
/// `Usage` it has always been, and a ledger changes no byte of the request.
#[tokio::test]
async fn lc0_without_a_ledger_nothing_changes_and_a_ledger_changes_no_request_byte() {
    let (plain_url, plain_bodies) = stand_in(|_| ok()).await;
    let plain = run(&plain_url, 0, None).await;
    assert!(plain.result.is_ok(), "{:?}", plain.result.err());
    assert_eq!(plain.plain_usage(), vec![(6, 3, 4)]);
    assert!(plain.attempts().is_empty() && plain.liabilities().is_empty());

    let (url, bodies) = stand_in(|_| ok()).await;
    let booked = ledgered(&url, 0).await;
    assert!(booked.result.is_ok(), "{:?}", booked.result.err());
    assert_eq!(*bodies.lock().unwrap(), *plain_bodies.lock().unwrap());
    assert_eq!(bodies.lock().unwrap().len(), 1);
}

/// L15: a Codex attempt is measured from its one terminal report (input
/// net of its cached slice, as reported downstream), reported as the
/// attempt's usage and never also as plain usage.
#[tokio::test]
async fn lc1_a_codex_attempt_is_measured_from_its_terminal_report() {
    let (url, bodies) = stand_in(|_| ok()).await;
    let b = ledgered(&url, 0).await;
    assert!(b.result.is_ok(), "{:?}", b.result.err());
    assert_eq!(bodies.lock().unwrap().len(), 1);
    let entries = b.entries();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(measured(&entries[0]), (6, 3, 4));
    assert_eq!(entries[0].purpose, RequestPurpose::Conversation);
    assert_eq!(entries[0].model, MODEL);
    assert_eq!(b.attempts(), vec![(id(&entries[0]), 6, 3)]);
    assert!(b.plain_usage().is_empty(), "never both");
    assert!(b.liabilities().is_empty());
}

/// L15: the Responses stream states a response's whole usage on its
/// terminal event; a repeated terminal report is the same response's, so
/// the attempt settles with the last one, never the sum.
#[tokio::test]
async fn lc2_a_repeated_terminal_report_settles_with_the_last() {
    let (url, _) = stand_in(|_| {
        sse(&[
            text(),
            report("response.completed", 10, 4, 3),
            report("response.done", 12, 4, 5),
        ])
    })
    .await;
    let b = ledgered(&url, 0).await;
    assert!(b.result.is_ok(), "{:?}", b.result.err());
    let entries = b.entries();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(measured(&entries[0]), (8, 5, 4), "the last report, not the sum");
    assert!(b.liabilities().is_empty());
}

/// L5/L15: every HTTP send is its own attempt. A rate-limited send is no
/// proof of zero spend: it stays an open liability (and says so); the retry
/// is its own measured attempt.
#[tokio::test]
async fn lc3_each_send_of_a_retried_request_is_its_own_attempt() {
    let (url, bodies) = stand_in(|n| match n {
        0 => status(StatusCode::TOO_MANY_REQUESTS),
        _ => ok(),
    })
    .await;
    let b = ledgered(&url, 1).await;
    assert!(b.result.is_ok(), "{:?}", b.result.err());
    assert_eq!(bodies.lock().unwrap().len(), 2);
    let entries = b.entries();
    assert_eq!(entries.len(), 2, "one entry per send: {entries:?}");
    assert!(is_open(&entries[0]), "{:?}", entries[0]);
    assert_eq!(measured(&entries[1]), (6, 3, 4));
    assert_eq!(b.liabilities(), vec![(id(&entries[0]), LiabilityState::Open)]);
    assert_eq!(b.attempts(), vec![(id(&entries[1]), 6, 3)]);
    assert!(b.plain_usage().is_empty());
}

/// L5/L20: an error status, and a refused connection on this
/// redirect-following client, prove nothing about spend: each stays open.
#[tokio::test]
async fn lc4_an_error_status_or_a_refused_connection_stays_open() {
    let (url, _) = stand_in(|_| status(StatusCode::INTERNAL_SERVER_ERROR)).await;
    let b = ledgered(&url, 0).await;
    assert!(b.result.is_err());
    let entries = b.entries();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert!(is_open(&entries[0]), "{:?}", entries[0]);
    assert_eq!(b.liabilities(), vec![(id(&entries[0]), LiabilityState::Open)]);

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let closed = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let b = ledgered(&closed, 0).await;
    assert!(b.result.is_err());
    let entries = b.entries();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert!(is_open(&entries[0]), "{:?}", entries[0]);
    assert_eq!(b.liabilities(), vec![(id(&entries[0]), LiabilityState::Open)]);
    assert!(b.attempts().is_empty());
}

/// L15: a stream that ends without its terminal event reported nothing
/// final: the attempt stays open; the retry is its own measured attempt.
#[tokio::test]
async fn lc5_a_stream_without_its_terminal_event_stays_open() {
    let (url, bodies) = stand_in(|n| match n {
        0 => cut(&[json!({"type":"response.created","response":{"usage":null}})]),
        _ => ok(),
    })
    .await;
    let b = ledgered(&url, 1).await;
    assert!(b.result.is_ok(), "{:?}", b.result.err());
    assert_eq!(bodies.lock().unwrap().len(), 2);
    let entries = b.entries();
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert!(is_open(&entries[0]), "{:?}", entries[0]);
    assert_eq!(measured(&entries[1]), (6, 3, 4));
    assert_eq!(b.liabilities(), vec![(id(&entries[0]), LiabilityState::Open)]);
}

/// L15: a completed response without usable output (retried) was still
/// answered and reported: it is measured by its report, and its retry is
/// its own attempt.
#[tokio::test]
async fn lc6_a_completed_attempt_without_output_is_measured() {
    let (url, _) = stand_in(|n| match n {
        0 => sse(&[report("response.completed", 7, 0, 0)]),
        _ => ok(),
    })
    .await;
    let b = ledgered(&url, 1).await;
    assert!(b.result.is_ok(), "{:?}", b.result.err());
    let entries = b.entries();
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert_eq!(measured(&entries[0]), (7, 0, 0));
    assert_eq!(measured(&entries[1]), (6, 3, 4));
    assert_eq!(
        b.attempts(),
        vec![(id(&entries[0]), 7, 0), (id(&entries[1]), 6, 3)]
    );
    assert!(b.liabilities().is_empty());
}

/// L15: a terminal failure that reports its usage (an incomplete response)
/// is measured by that report although the request fails; one that reports
/// none stays open.
#[tokio::test]
async fn lc7_a_terminal_failure_is_measured_by_its_report_or_stays_open() {
    let (url, _) = stand_in(|_| sse(&[text(), report("response.incomplete", 5, 0, 2)])).await;
    let b = ledgered(&url, 0).await;
    assert!(b.result.is_err());
    let entries = b.entries();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(measured(&entries[0]), (5, 2, 0));
    assert_eq!(b.attempts(), vec![(id(&entries[0]), 5, 2)]);
    assert!(b.liabilities().is_empty());

    let (url, _) = stand_in(|_| {
        sse(&[json!({"type":"response.failed","response":{"error":{"type":"server_error"}}})])
    })
    .await;
    let b = ledgered(&url, 0).await;
    assert!(b.result.is_err());
    let entries = b.entries();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert!(is_open(&entries[0]), "{:?}", entries[0]);
    assert_eq!(b.liabilities(), vec![(id(&entries[0]), LiabilityState::Open)]);
}

/// L6: a run at its request cap sends nothing more: the request fails as
/// not dispatched with the typed reason, and the provider sees nothing.
#[tokio::test]
async fn lc8_a_run_at_its_cap_sends_no_request() {
    let (url, bodies) = stand_in(|_| ok()).await;
    let ledger = RequestLedgerHandle::new(MODEL);
    for _ in 0..Ledger::MAX_ENTRIES {
        ledger.reserve(1, UsageReports::Cumulative).unwrap().release();
    }
    let b = run(&url, 1, Some(ledger)).await;
    let error = b.result.as_ref().expect_err("refused");
    assert!(
        error.downcast_ref::<super::super::NotDispatched>().is_some(),
        "{error}"
    );
    assert!(
        error.to_string().contains("this run reached 4096 provider requests"),
        "{error}"
    );
    assert!(bodies.lock().unwrap().is_empty(), "nothing was sent");
    assert_eq!(b.entries().len(), Ledger::MAX_ENTRIES);
    assert!(b.attempts().is_empty() && b.liabilities().is_empty());
}

/// L5: a request cancelled while its answer streams was sent: the attempt
/// stays open and says so.
#[tokio::test]
async fn lc9_a_cancelled_attempt_stays_open() {
    let (url, _) = stand_in(|_| {
        let first = format!("data: {}\n\n", text());
        let body = futures::stream::iter([Ok::<_, std::io::Error>(bytes::Bytes::from(first))])
            .chain(futures::stream::pending());
        (
            StatusCode::OK,
            [("content-type", "text/event-stream")],
            axum::body::Body::from_stream(body),
        )
            .into_response()
    })
    .await;
    let ledger = RequestLedgerHandle::new(MODEL);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut events = Vec::new();
    let canceller = async {
        while let Some(event) = rx.recv().await {
            let started = matches!(&event, StreamEvent::Llm(crate::LlmEvent::Text(_)));
            events.push(event);
            if started {
                cancel.cancel();
                return;
            }
        }
    };
    let (result, ()) = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        async {
            tokio::join!(
                request(&url, 0, Some(ledger.clone()), &cancel, &tx),
                canceller
            )
        },
    )
    .await
    .expect("the cancel ends the request");
    drop(tx);
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    let b = Booked {
        result,
        events,
        ledger: Some(ledger),
    };
    assert!(b.result.is_err());
    let entries = b.entries();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert!(is_open(&entries[0]), "{:?}", entries[0]);
    assert_eq!(b.liabilities(), vec![(id(&entries[0]), LiabilityState::Open)]);
}

/// L3: a terminal report that states every count as zero is a measured
/// zero, not an unknown. (An all-zero report is no usage to display, as
/// before: no usage event.)
#[tokio::test]
async fn lc10_an_explicit_all_zero_report_is_measured_as_zero() {
    let (url, _) = stand_in(|_| sse(&[text(), report("response.completed", 0, 0, 0)])).await;
    let b = ledgered(&url, 0).await;
    assert!(b.result.is_ok(), "{:?}", b.result.err());
    let entries = b.entries();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(measured(&entries[0]), (0, 0, 0));
    assert!(b.liabilities().is_empty());
    assert!(b.attempts().is_empty() && b.plain_usage().is_empty());
}

/// L3: a terminal report without the output count does not state the
/// attempt's spend: the attempt stays open (the report is still the
/// attempt's, zero-filled for display as before).
#[tokio::test]
async fn lc11_a_report_without_the_output_count_stays_open() {
    let (url, _) = stand_in(|_| {
        sse(&[
            text(),
            json!({"type":"response.completed","response":{"usage":{"input_tokens":10}}}),
        ])
    })
    .await;
    let b = ledgered(&url, 0).await;
    assert!(b.result.is_ok(), "{:?}", b.result.err());
    let entries = b.entries();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert!(is_open(&entries[0]), "{:?}", entries[0]);
    assert_eq!(b.liabilities(), vec![(id(&entries[0]), LiabilityState::Open)]);
    assert_eq!(b.attempts(), vec![(id(&entries[0]), 10, 0)]);
}

/// L3: the attempt settles on the last terminal event's report as sent:
/// a later terminal report that is empty, or states no count the price
/// needs, leaves the attempt open, whatever an earlier one said ("last"
/// is the last, not the last non-empty).
#[tokio::test]
async fn lc12_an_empty_last_terminal_report_leaves_the_attempt_open() {
    for later in [json!({}), json!({"total_tokens": 13})] {
        let (url, _) = stand_in(move |_| {
            sse(&[
                text(),
                report("response.completed", 10, 4, 3),
                json!({"type":"response.done","response":{"usage": later.clone()}}),
            ])
        })
        .await;
        let b = ledgered(&url, 0).await;
        assert!(b.result.is_ok(), "{:?}", b.result.err());
        let entries = b.entries();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(is_open(&entries[0]), "{:?}", entries[0]);
        assert_eq!(b.liabilities(), vec![(id(&entries[0]), LiabilityState::Open)]);
        assert_eq!(b.attempts(), vec![(id(&entries[0]), 6, 3)], "display unchanged");
    }
}

/// L3: a later terminal event without usage (absent or `null`) is no
/// statement of the spend either: it invalidates the earlier report and
/// the attempt stays open. No terminal alias is exempt.
#[tokio::test]
async fn lc13_a_later_terminal_event_without_usage_leaves_the_attempt_open() {
    for later in [
        json!({"type":"response.done","response":{}}),
        json!({"type":"response.done","response":{"usage":null}}),
        json!({"type":"response.done"}),
    ] {
        let (url, _) = stand_in(move |_| {
            sse(&[text(), report("response.completed", 10, 4, 3), later.clone()])
        })
        .await;
        let b = ledgered(&url, 0).await;
        assert!(b.result.is_ok(), "{:?}", b.result.err());
        let entries = b.entries();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(is_open(&entries[0]), "{:?}", entries[0]);
        assert_eq!(b.liabilities(), vec![(id(&entries[0]), LiabilityState::Open)]);
        assert_eq!(b.attempts(), vec![(id(&entries[0]), 6, 3)], "display unchanged");
    }
}
