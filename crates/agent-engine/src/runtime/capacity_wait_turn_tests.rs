//! Capacity waits through a whole turn (`Runtime::run_stream_tracked`): a
//! scripted broker installed for this session's auth state only (in place
//! of the local one), a loopback Anthropic stub, real time (a paused clock
//! would fire the HTTP client's own timers while it waits on a socket).
//! - Review B3: a completed tool round, then a cooling selection at the
//!   round top: the turn waits, then continues; the tool ran once and the
//!   next request carries its result.
//! - Review S1: Esc during a turn-start wait ends the turn with its history
//!   kept, exactly one Done and no Error, and nothing sent.
//! - Phase 2: a turn that ends on a reset-based capacity stop (at its
//!   start or at a round top) leaves the stop in its completion handle:
//!   resume at the reset, the kind. Nothing else does.

use super::capacity_wait::{CapacityWaitPolicy, Jitter};
use super::types::{AuthState, CapacityStopAt, TurnCompletion};
use crate::auth::{
    AccessToken, AccountSelector, BrokerError, CredentialBroker, CredentialRef, CredentialSource,
    OAuthProviderId, PinnedToken, ProxyByteStream, ProxyRequest, ProxyResponse,
};
use agent_core::auth::{AccountFailure, AccountFailureKind, SelectionFailure, SelectionFailureKind};
use axum::{extract::State, routing::post, Router};
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const MODEL: &str = "claude-sonnet-4-6";
const FRESH: u64 = 9_999_999_999_999;

fn claude4() -> CredentialRef {
    CredentialRef::new(
        OAuthProviderId::Anthropic,
        crate::auth::Account::parse("claude4").unwrap(),
    )
}

/// The only account, claude4: automatic selection, limited from `limit()`
/// on. While limited, its cooldown is active and a selection fails with
/// every account cooling down until `reset_ms`; it is back a second before
/// that stated reset, so a check at the reset finds it.
#[derive(Default)]
struct ScriptBroker {
    /// The stated reset (epoch ms); 0: not limited.
    reset_ms: AtomicU64,
    selections: AtomicUsize,
    /// Fail as a usage read that failed (capacity unproven) instead.
    unproven: std::sync::atomic::AtomicBool,
    /// Refuse to re-vend the pinned seat (`access_token_for_auto`), as a
    /// login whose refresh is held would.
    no_revend: std::sync::atomic::AtomicBool,
    /// Re-vends of the pinned seat asked for (`access_token_for_auto`).
    revends: AtomicUsize,
}

impl ScriptBroker {
    fn limit(&self, reset_ms: u64) {
        self.reset_ms.store(reset_ms, Ordering::SeqCst);
    }

    /// The stated reset while the account is still limited.
    fn limited(&self) -> Option<u64> {
        let reset = self.reset_ms.load(Ordering::SeqCst);
        (reset != 0 && crate::epoch_millis() + 1_000 < reset).then_some(reset)
    }

    fn selections(&self) -> usize {
        self.selections.load(Ordering::SeqCst)
    }

    fn revends(&self) -> usize {
        self.revends.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl CredentialBroker for ScriptBroker {
    async fn access_token(&self, _p: OAuthProviderId) -> Result<AccessToken, BrokerError> {
        Err(BrokerError::Denied("legacy path must not be used".into()))
    }
    fn account_selector(&self, _p: OAuthProviderId) -> AccountSelector {
        AccountSelector::Auto
    }
    fn active_cooldown(&self, _cred: &CredentialRef) -> Option<u64> {
        self.limited()
    }
    async fn access_token_for(&self, _cred: &CredentialRef) -> Result<AccessToken, BrokerError> {
        Ok(AccessToken {
            token: "tok-claude4".into(),
            expires: FRESH,
        })
    }
    async fn access_token_for_auto(
        &self,
        cred: &CredentialRef,
    ) -> Result<AccessToken, BrokerError> {
        self.revends.fetch_add(1, Ordering::SeqCst);
        if self.no_revend.load(Ordering::SeqCst) {
            return Err(BrokerError::Denied(
                "scripted: the seat cannot be re-vended".into(),
            ));
        }
        self.access_token_for(cred).await
    }
    async fn access_token_pinned_for(
        &self,
        _provider: OAuthProviderId,
        _model: Option<&str>,
    ) -> Result<PinnedToken, BrokerError> {
        self.selections.fetch_add(1, Ordering::SeqCst);
        if let Some(reset) = self.limited() {
            return Err(BrokerError::NoAccountAvailable {
                provider: "anthropic".into(),
                reason: "scripted".into(),
                failure: Some(SelectionFailure {
                    kind: if self.unproven.load(Ordering::SeqCst) {
                        SelectionFailureKind::UsageReadFailed
                    } else {
                        SelectionFailureKind::AllCoolingDown
                    },
                    accounts: vec![AccountFailure {
                        account: "claude4".into(),
                        kind: AccountFailureKind::Exhausted,
                        http_status: None,
                        retry_after_s: None,
                        until_ms: Some(reset),
                        cooling_down: true,
                    }],
                    earliest_reset_ms: Some(reset),
                    usage_read_retry_ms: None,
                }),
            });
        }
        Ok(PinnedToken {
            credential: claude4(),
            token: AccessToken {
                token: "tok-claude4".into(),
                expires: FRESH,
            },
        })
    }
    async fn proxy(&self, _r: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
        Err(BrokerError::Denied("not implemented in stub".into()))
    }
    async fn proxy_stream(&self, _r: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
        Err(BrokerError::Denied("not implemented in stub".into()))
    }
    async fn anthropic_usage(&self) -> Result<Value, BrokerError> {
        Err(BrokerError::Denied("not implemented in stub".into()))
    }
    async fn capabilities(&self) -> Result<Vec<agent_core::auth::ProviderStatus>, BrokerError> {
        Ok(vec![])
    }
}

fn sse(events: Vec<Value>) -> String {
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

fn message_start() -> Value {
    json!({"type":"message_start","message":{"id":"msg_cw","type":"message","role":"assistant","content":[],"model":MODEL,"stop_reason":null,"stop_sequence":null}})
}

/// One call to `capacity_probe`.
fn tool_sse() -> String {
    sse(vec![
        message_start(),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_probe","name":"capacity_probe","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null}}),
        json!({"type":"message_stop"}),
    ])
}

fn text_sse() -> String {
    sse(vec![
        message_start(),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"all done"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null}}),
        json!({"type":"message_stop"}),
    ])
}

/// The loopback provider: the first request answers with a tool call and
/// then limits claude4 (another session met its limit meanwhile); later
/// ones answer with text. Bodies and arrival times recorded.
#[derive(Clone)]
struct Mock {
    broker: Arc<ScriptBroker>,
    /// The stated reset the first answer sets, this long from then.
    limit_for: Option<Duration>,
    seen: Arc<Mutex<Vec<(Instant, String)>>>,
}

impl Mock {
    fn bodies(&self) -> Vec<String> {
        self.seen.lock().unwrap().iter().map(|(_, b)| b.clone()).collect()
    }
    fn arrivals(&self) -> Vec<Instant> {
        self.seen.lock().unwrap().iter().map(|(t, _)| *t).collect()
    }
}

async fn respond(State(m): State<Mock>, body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    let first = {
        let mut seen = m.seen.lock().unwrap();
        seen.push((Instant::now(), body));
        seen.len() == 1
    };
    let sse = if first {
        if let Some(d) = m.limit_for {
            m.broker.limit(crate::epoch_millis() + d.as_millis() as u64);
        }
        tool_sse()
    } else {
        text_sse()
    };
    (
        axum::http::StatusCode::OK,
        [("content-type", "text/event-stream")],
        sse,
    )
        .into_response()
}

async fn spawn(broker: Arc<ScriptBroker>, limit_for: Option<Duration>) -> (String, Mock) {
    let mock = Mock {
        broker,
        limit_for,
        seen: Arc::default(),
    };
    let app = Router::new()
        .route("/v1/messages", post(respond))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), mock)
}

/// Counts its runs.
struct Probe(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl crate::tools::Tool for Probe {
    fn name(&self) -> &str {
        "capacity_probe"
    }
    fn description(&self) -> &str {
        "counts its runs"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object"})
    }
    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }
    async fn execute(&self, _params: Value, _ctx: crate::tools::ToolContext) -> crate::Result<String> {
        let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(format!("probe-ran-{n}"))
    }
}

/// A headless runtime on automatic selection with claude4 bound, capacity
/// waits on with no jitter, the probe tool registered.
async fn runtime(runs: &Arc<AtomicUsize>) -> crate::Runtime {
    let mut rt = crate::Runtime::new_headless();
    rt.set_model(MODEL.to_string());
    rt.set_api_retries(1);
    rt.capacity_wait = Some(CapacityWaitPolicy {
        jitter: Jitter::Fixed(Duration::ZERO),
        ..Default::default()
    });
    *rt.auth.write().await = AuthState {
        auth_token: "tok-claude4".into(),
        auth_type: "oauth".into(),
        refresh_token: None,
        token_expires: Some(FRESH),
        auth_file_mtime: None,
        bound_credential: Some(super::auth::anthropic_binding(
            &CredentialSource::Local,
            &claude4(),
        )),
    };
    rt.tools.write().await.register(Arc::new(Probe(Arc::clone(runs))));
    rt
}

#[derive(Default, Debug)]
struct Seen {
    notices: Vec<String>,
    errors: Vec<String>,
    history: Option<Vec<crate::SharedMessage>>,
    done: usize,
}

/// Drain the turn; `on_notice` runs once, at the first notice.
async fn drain(
    mut stream: std::pin::Pin<Box<dyn futures::Stream<Item = crate::StreamEvent> + Send>>,
    on_notice: impl FnOnce(),
) -> Seen {
    use agent_core::{SessionEvent as S, StreamEvent as E};
    let mut seen = Seen::default();
    let mut on_notice = Some(on_notice);
    while let Some(event) = tokio::time::timeout(Duration::from_secs(60), stream.next())
        .await
        .expect("the turn makes progress")
    {
        match event {
            E::Session(S::Notice(n)) => {
                seen.notices.push(n);
                if let Some(f) = on_notice.take() {
                    f();
                }
            }
            E::Session(S::Error(e)) => seen.errors.push(e.message),
            E::Session(S::MessageHistory(h)) => seen.history = Some(h),
            E::Session(S::Done) => seen.done += 1,
            _ => {}
        }
    }
    seen
}

fn user(text: &str) -> Vec<crate::SharedMessage> {
    vec![Arc::new(json!({"role":"user","content":text}))]
}

fn has_text(history: &[crate::SharedMessage], needle: &str) -> bool {
    history.iter().any(|m| m.to_string().contains(needle))
}

/// Review B3: the round-top wait. A tool round completes; at the next
/// round top the pinned seat is cooling and no account has capacity. The
/// turn waits for the reset (one notice), then sends the NEXT request on
/// the same seat. The tool ran exactly once, the second request carries its
/// result, nothing was sent twice, and the turn ends normally.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
// `stream_relay` too: a real turn runs a real bounded stream relay, whose
// byte counters are process-wide (relay.rs's flood test reads them).
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_round_top_wait_after_a_tool_round_continues_with_its_result() {
    let _base = crate::test_env::BaseDirGuard::new();
    let broker = Arc::new(ScriptBroker::default());
    let (url, mock) = spawn(Arc::clone(&broker), Some(Duration::from_secs(7))).await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let runs = Arc::new(AtomicUsize::new(0));
    let rt = runtime(&runs).await;
    let _installed = super::turn_broker_testing::install_for(&rt.auth, broker.clone());

    let completion = TurnCompletion::new();
    let stream = rt
        .run_stream_tracked(
            user("probe: run the tool once"),
            CancellationToken::new(),
            None,
            None,
            false,
            completion.clone(),
        )
        .await;
    let seen = drain(stream, || {}).await;

    assert!(seen.errors.is_empty(), "no error: {seen:?}");
    assert_eq!(seen.done, 1, "{seen:?}");
    assert!(completion.completed(), "the turn reached its normal end");
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the tool ran exactly once");
    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 2, "one request per round, none resent: {bodies:?}");
    assert!(!bodies[0].contains("tool_result"), "{}", bodies[0]);
    assert!(bodies[1].contains("\"tool_result\""), "{}", bodies[1]);
    assert!(bodies[1].contains("toolu_probe"), "{}", bodies[1]);
    assert!(bodies[1].contains("probe-ran-1"), "the next request carries the result: {}", bodies[1]);
    let arrivals = mock.arrivals();
    let waited = arrivals[1] - arrivals[0];
    assert!(waited >= Duration::from_secs(5), "the round top waited: {waited:?}");
    assert_eq!(seen.notices.len(), 1, "{:?}", seen.notices);
    assert!(
        seen.notices[0].starts_with("⏸ Every Anthropic account is cooling down"),
        "{}",
        seen.notices[0]
    );
    assert!(seen.notices[0].ends_with(", then continuing. Esc stops."), "{}", seen.notices[0]);
    assert_eq!(
        broker.selections(),
        2,
        "the turn start's selection and the round top's failed one; the seat is kept after"
    );
    let history = seen.history.expect("the final history");
    assert!(has_text(&history, "probe-ran-1"), "{history:?}");
    assert!(has_text(&history, "all done"), "{history:?}");
    assert_eq!(rt.auth.read().await.auth_token, "tok-claude4");
    assert_eq!(completion.take_capacity_stop(), None, "a turn that carried on is no stop");
}

/// Review S1 (the end-to-end ask): every account is cooling down until a
/// reset 2 h away, so the turn waits at its start, in the spawned turn
/// task. Esc during the wait ends the turn: its history is kept (the
/// user's message is in it), exactly one Done, no Error, nothing sent and
/// no further selection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
// `stream_relay` too, as above.
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn esc_during_a_turn_start_wait_keeps_the_history_and_ends_with_one_done() {
    let _base = crate::test_env::BaseDirGuard::new();
    let broker = Arc::new(ScriptBroker::default());
    broker.limit(crate::epoch_millis() + 2 * 3600 * 1000);
    let (url, mock) = spawn(Arc::clone(&broker), None).await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let runs = Arc::new(AtomicUsize::new(0));
    let rt = runtime(&runs).await;
    let _installed = super::turn_broker_testing::install_for(&rt.auth, broker.clone());

    let cancel = CancellationToken::new();
    let completion = TurnCompletion::new();
    let stream = rt
        .run_stream_tracked(
            user("probe: please wait for capacity"),
            cancel.clone(),
            None,
            None,
            false,
            completion.clone(),
        )
        .await;
    let esc = cancel.clone();
    let seen = drain(stream, move || esc.cancel()).await;

    assert!(seen.errors.is_empty(), "Esc is not an error: {seen:?}");
    assert_eq!(seen.done, 1, "{seen:?}");
    assert!(!completion.completed(), "a canceled turn didn't reach its end");
    assert_eq!(seen.notices.len(), 1, "{:?}", seen.notices);
    assert!(seen.notices[0].contains("; waiting until "), "{}", seen.notices[0]);
    let history = seen.history.expect("the history is published");
    assert!(has_text(&history, "probe: please wait for capacity"), "{history:?}");
    assert!(mock.bodies().is_empty(), "nothing was sent");
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(broker.selections(), 1, "no selection after Esc");
    assert_eq!(completion.take_capacity_stop(), None, "Esc is no capacity stop");
}

const DAY_MS: u64 = 24 * 3600 * 1000;

/// One turn, drained to its end, and its completion handle.
async fn one_turn(rt: &crate::Runtime, text: &str) -> (Seen, TurnCompletion) {
    let completion = TurnCompletion::new();
    let stream = rt
        .run_stream_tracked(
            user(text),
            CancellationToken::new(),
            None,
            None,
            false,
            completion.clone(),
        )
        .await;
    (drain(stream, || {}).await, completion)
}

/// The turn ended as today: one Error, one Done, no wait notice.
fn ended_as_today(seen: &Seen) {
    assert_eq!(seen.errors.len(), 1, "{seen:?}");
    assert_eq!(seen.done, 1, "{seen:?}");
    assert!(!seen.notices.iter().any(|n| n.starts_with("⏸ ")), "no wait: {:?}", seen.notices);
}

/// Phase 2, E1: every account is cooling down until a reset 3 days away,
/// beyond the wait's bound. The turn ends at its start as today (nothing
/// sent), and its completion holds the stop: resume at that reset, and the
/// selection failure's kind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_turn_start_stop_beyond_the_bound_is_marked_with_its_reset() {
    let _base = crate::test_env::BaseDirGuard::new();
    let broker = Arc::new(ScriptBroker::default());
    let reset = crate::epoch_millis() + 3 * DAY_MS;
    broker.limit(reset);
    let (url, mock) = spawn(Arc::clone(&broker), None).await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let runs = Arc::new(AtomicUsize::new(0));
    let rt = runtime(&runs).await;
    let _installed = super::turn_broker_testing::install_for(&rt.auth, broker.clone());

    let (seen, completion) = one_turn(&rt, "probe: stopped at the turn start").await;

    ended_as_today(&seen);
    assert!(mock.bodies().is_empty(), "nothing was sent");
    assert_eq!(
        completion.take_capacity_stop(),
        Some(CapacityStopAt { resume_at_ms: reset, kind: "all_cooling_down" })
    );
}

/// Phase 2, E1b (ruling 4): with `capacity_wait` off the turn never waits,
/// and a reset-based stop is marked all the same, even one whose reset (2 h
/// away) the wait would have reached.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn with_capacity_wait_off_a_turn_start_stop_is_marked_at_once() {
    let _base = crate::test_env::BaseDirGuard::new();
    let broker = Arc::new(ScriptBroker::default());
    let reset = crate::epoch_millis() + 2 * 3600 * 1000;
    broker.limit(reset);
    let (url, mock) = spawn(Arc::clone(&broker), None).await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let runs = Arc::new(AtomicUsize::new(0));
    let mut rt = runtime(&runs).await;
    rt.capacity_wait = None;
    let _installed = super::turn_broker_testing::install_for(&rt.auth, broker.clone());

    let (seen, completion) = one_turn(&rt, "probe: capacity_wait off").await;

    ended_as_today(&seen);
    assert!(mock.bodies().is_empty(), "nothing was sent");
    assert_eq!(
        completion.take_capacity_stop(),
        Some(CapacityStopAt { resume_at_ms: reset, kind: "all_cooling_down" })
    );
}

/// Phase 2, E3 at the turn level (`capacity_wait` off, so nothing waits):
/// only reset-based stops inside the 8-day horizon are marked. A reset 9
/// days away is bad data; unproven capacity beside a reset 3 days away is
/// no reset-based stop (ruling); unproven capacity beside a reset 2 h away
/// is (the mixed case).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_turn_start_marks_only_reset_based_stops_inside_the_horizon() {
    let _base = crate::test_env::BaseDirGuard::new();
    let broker = Arc::new(ScriptBroker::default());
    let (url, mock) = spawn(Arc::clone(&broker), None).await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let runs = Arc::new(AtomicUsize::new(0));
    let mut rt = runtime(&runs).await;
    rt.capacity_wait = None;
    let _installed = super::turn_broker_testing::install_for(&rt.auth, broker.clone());

    broker.limit(crate::epoch_millis() + 9 * DAY_MS);
    let (seen, completion) = one_turn(&rt, "probe: beyond the horizon").await;
    ended_as_today(&seen);
    assert_eq!(completion.take_capacity_stop(), None, "a reset 9 days away is not marked");

    broker.unproven.store(true, Ordering::SeqCst);
    broker.limit(crate::epoch_millis() + 3 * DAY_MS);
    let (seen, completion) = one_turn(&rt, "probe: unproven, reset in 3 days").await;
    ended_as_today(&seen);
    assert_eq!(completion.take_capacity_stop(), None, "unproven beyond the bound is not marked");

    let reset = crate::epoch_millis() + 2 * 3600 * 1000;
    broker.limit(reset);
    let (seen, completion) = one_turn(&rt, "probe: unproven, reset in 2 h").await;
    ended_as_today(&seen);
    assert_eq!(
        completion.take_capacity_stop(),
        Some(CapacityStopAt { resume_at_ms: reset, kind: "usage_read_failed" }),
        "the mixed case is reset-based"
    );
    assert!(mock.bodies().is_empty(), "nothing was sent");
}

/// Phase 2, E4: a tool round completes, then at the round top every
/// account is cooling down until a reset 3 days away (`capacity_wait` off).
/// The turn ends as today after its one tool run, and its completion holds
/// the stop with that reset.
///
/// PARITY (upstream be408a1cc, kept by the merge): mid-turn, with every
/// account cooling and no wait starting, the pinned seat is KEPT when it
/// can be re-vended (see the keep-seat test below). The stop here is the
/// one left: a pinned seat that cannot be re-vended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_round_top_stop_after_a_tool_round_is_marked_with_its_reset() {
    let _base = crate::test_env::BaseDirGuard::new();
    let broker = Arc::new(ScriptBroker::default());
    let (url, mock) = spawn(Arc::clone(&broker), Some(Duration::from_millis(3 * DAY_MS))).await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let runs = Arc::new(AtomicUsize::new(0));
    let mut rt = runtime(&runs).await;
    rt.capacity_wait = None;
    broker.no_revend.store(true, Ordering::SeqCst);
    let _installed = super::turn_broker_testing::install_for(&rt.auth, broker.clone());

    let (seen, completion) = one_turn(&rt, "probe: run the tool once").await;

    ended_as_today(&seen);
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the tool ran once");
    assert_eq!(mock.bodies().len(), 1, "no request after the round top");
    assert_eq!(
        broker.revends(),
        1,
        "the seat was asked for once, and refused"
    );
    let reset = broker.limited().expect("limited by the first answer");
    assert_eq!(
        completion.take_capacity_stop(),
        Some(CapacityStopAt { resume_at_ms: reset, kind: "all_cooling_down" })
    );
}

/// Keep-seat versus capacity wait (the merge, auth.rs `capacity_wait_with_broker`
/// / `keep_or_end` / `keep_cooling_seat`): a tool round completes, then at
/// the round top EVERY account is cooling down, the pinned one too, until a
/// reset 3 days away. Where no wait starts (`capacity_wait` off, or on with
/// the reset beyond its bound) the turn KEEPS its pinned seat (re-vended
/// once, upstream be408a1cc): no wait notice, no error, the next request
/// goes out on the same seat with the tool's result, the turn ends
/// normally and its completion holds no capacity stop. A wait that does
/// start waits instead (review B3 above: re-selects after the wait).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_round_top_with_every_account_cooling_keeps_the_seat_where_no_wait_starts() {
    for wait_on in [false, true] {
        let _base = crate::test_env::BaseDirGuard::new();
        let broker = Arc::new(ScriptBroker::default());
        let (url, mock) = spawn(Arc::clone(&broker), Some(Duration::from_millis(3 * DAY_MS))).await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let runs = Arc::new(AtomicUsize::new(0));
        let mut rt = runtime(&runs).await;
        if !wait_on {
            rt.capacity_wait = None;
        }
        let _installed = super::turn_broker_testing::install_for(&rt.auth, broker.clone());

        let (seen, completion) = one_turn(&rt, "probe: run the tool once").await;

        assert!(seen.errors.is_empty(), "wait_on={wait_on}: {seen:?}");
        assert_eq!(seen.done, 1, "wait_on={wait_on}: {seen:?}");
        assert!(
            !seen.notices.iter().any(|n| n.starts_with("⏸ ")),
            "wait_on={wait_on}: no wait: {:?}",
            seen.notices
        );
        assert!(
            completion.completed(),
            "wait_on={wait_on}: the turn reached its end"
        );
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "wait_on={wait_on}: the tool ran once"
        );
        let bodies = mock.bodies();
        assert_eq!(bodies.len(), 2, "wait_on={wait_on}: one request per round");
        assert!(
            bodies[1].contains("probe-ran-1"),
            "wait_on={wait_on}: {}",
            bodies[1]
        );
        assert_eq!(
            broker.revends(),
            1,
            "wait_on={wait_on}: the pinned seat re-vended once"
        );
        assert_eq!(
            broker.selections(),
            2,
            "wait_on={wait_on}: the turn start's selection and the round top's failed one"
        );
        assert!(
            broker.limited().is_some(),
            "wait_on={wait_on}: still cooling"
        );
        assert_eq!(rt.auth.read().await.auth_token, "tok-claude4");
        let history = seen.history.expect("the final history");
        assert!(
            has_text(&history, "all done"),
            "wait_on={wait_on}: {history:?}"
        );
        assert_eq!(
            completion.take_capacity_stop(),
            None,
            "wait_on={wait_on}: a turn that kept its seat is no stop"
        );
    }
}
