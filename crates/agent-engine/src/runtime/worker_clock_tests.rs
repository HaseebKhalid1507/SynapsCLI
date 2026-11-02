//! D obligations at the runtime (`docs/specs/subagent-context-rollover.md`
//! §5, slice 5): a worker runtime on its run clock never renews its
//! deadline at a rollover, never turns its expiry into a time checkpoint,
//! and re-checks cancellation and the deadline before its successor's first
//! request. Each case runs a session runtime as the control, through the
//! real stream against a local Anthropic stand-in. The stand-in counts the
//! conversation's requests apart from a rollover's summary request (the
//! summary goes out between the seal and the head checkpoint).
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Router};
use futures::StreamExt;
use serde_json::json;

use crate::runtime::budget::{RunClock, TurnBudget, TurnRole};
use crate::SharedMessage;
use agent_core::core::config::ContextManagementMode;
use agent_core::{BudgetDimension, SessionEvent, StreamEvent, TurnOutcome};

/// What the stand-in answers a request with.
#[derive(Clone, Copy)]
enum Reply {
    /// End the turn with text.
    Text,
    /// Call `read` (the turn goes on to another round).
    ToolUse,
}

type Plan = Arc<dyn Fn(usize) -> (Reply, Duration) + Send + Sync>;

#[derive(Clone)]
struct Mock {
    seen: Seen,
    plan: Plan,
    summary_delay: Duration,
}

/// What the stand-in was asked: conversation requests (streamed turns) and
/// rollover summary requests (the cold one is not streamed; the warm one
/// carries the host's rollover instruction).
#[derive(Clone, Default)]
struct Seen {
    turns: Arc<AtomicUsize>,
    summaries: Arc<AtomicUsize>,
}

impl Seen {
    fn turns(&self) -> usize {
        self.turns.load(Ordering::SeqCst)
    }

    fn summaries(&self) -> usize {
        self.summaries.load(Ordering::SeqCst)
    }

    fn reset(&self) {
        self.turns.store(0, Ordering::SeqCst);
        self.summaries.store(0, Ordering::SeqCst);
    }
}

fn is_summary(body: &str) -> bool {
    !body.contains("\"stream\":true") || body.contains("[Host request at a context rollover")
}

/// The summary model's reply to a cold (non-streamed) summary request.
fn summary_message() -> String {
    json!({"id":"s","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[{"type":"text","text":"## Window 1\nSUMMARY"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}).to_string()
}

fn sse(reply: Reply, n: usize) -> String {
    let usage = json!({"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0});
    let start = json!({"type":"message_start","message":{"id":format!("msg_{n}"),"type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":usage}});
    let (block, delta, stop) = match reply {
        Reply::Text => (
            json!({"type":"text","text":""}),
            json!({"type":"text_delta","text":"ok"}),
            "end_turn",
        ),
        Reply::ToolUse => (
            json!({"type":"tool_use","id":format!("toolu_{n}"),"name":"read","input":{}}),
            json!({"type":"input_json_delta","partial_json":"{\"path\":\"/nonexistent/synaps-worker-clock\"}"}),
            "tool_use",
        ),
    };
    [
        start,
        json!({"type":"content_block_start","index":0,"content_block":block}),
        json!({"type":"content_block_delta","index":0,"delta":delta}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":stop,"stop_sequence":null},"usage":usage}),
        json!({"type":"message_stop"}),
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .collect()
}

/// Serves Anthropic SSE: `plan` sees the conversation request's index and
/// says what to answer and how long to take; a summary request is answered
/// at once. Returns the base URL and what was asked.
async fn serve(plan: impl Fn(usize) -> (Reply, Duration) + Send + Sync + 'static) -> (String, Seen) {
    serve_with(plan, Duration::ZERO).await
}

/// [`serve`], answering a summary request after `summary_delay`.
async fn serve_with(
    plan: impl Fn(usize) -> (Reply, Duration) + Send + Sync + 'static,
    summary_delay: Duration,
) -> (String, Seen) {
    let mock = Mock {
        seen: Seen::default(),
        plan: Arc::new(plan),
        summary_delay,
    };
    let seen = mock.seen.clone();
    let app = Router::new()
        .route(
            "/v1/messages",
            post(|State(m): State<Mock>, body: String| async move {
                if is_summary(&body) {
                    m.seen.summaries.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(m.summary_delay).await;
                    if !body.contains("\"stream\":true") {
                        return (
                            StatusCode::OK,
                            [("content-type", "application/json")],
                            summary_message(),
                        )
                            .into_response();
                    }
                    return (
                        StatusCode::OK,
                        [("content-type", "text/event-stream")],
                        sse(Reply::Text, usize::MAX),
                    )
                        .into_response();
                }
                let n = m.seen.turns.fetch_add(1, Ordering::SeqCst);
                let (reply, delay) = (m.plan)(n);
                tokio::time::sleep(delay).await;
                (
                    StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    sse(reply, n),
                )
                    .into_response()
            }),
        )
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

/// A runtime with context management on and a `max_elapsed` turn limit:
/// a session's, or (`worker`) a worker's on its run clock without a
/// timeout of its own (the limit is the run's).
fn runtime(max_elapsed: Option<Duration>, worker: bool) -> crate::Runtime {
    let mut rt = crate::Runtime::new_headless();
    rt.set_model("claude-sonnet-4-6".to_string());
    rt.set_api_retries(1);
    rt.set_context_window(Some(500_000));
    rt.continuation.lock().unwrap().config.mode = ContextManagementMode::Auto;
    let role = if worker {
        TurnRole::Worker
    } else {
        TurnRole::Foreground
    };
    rt.set_turn_budget(TurnBudget {
        max_elapsed,
        ..TurnBudget::for_role(role)
    });
    if worker {
        rt.fix_run_clock(None);
    }
    rt
}

fn event(i: usize) -> SharedMessage {
    Arc::new(json!({"role":"user","content":format!(
        "<event id=\"e{i}\" type=\"worker_report\" severity=\"low\" source=\"worker-{}\">report {i}: {}</event>",
        i % 4,
        "x".repeat(1_100)
    )}))
}

/// Low soft thresholds so `big_history` forces a rollover before the
/// first request.
fn low_thresholds(rt: &crate::Runtime) {
    let mut s = rt.continuation.lock().unwrap();
    s.config.pressure_tokens = Some(60_000);
    s.config.rollover_tokens = Some(80_000);
    s.config.target_tokens = Some(25_000);
}

fn big_history() -> Vec<SharedMessage> {
    let mut m = vec![Arc::new(json!({"role":"user","content":"charter"}))];
    m.extend((0..260).map(event));
    m.push(Arc::new(json!({"role":"user","content":"next"})));
    m
}

fn small_history() -> Vec<SharedMessage> {
    vec![Arc::new(json!({"role":"user","content":"task"}))]
}

/// What a turn did, as its consumer saw it.
#[derive(Default)]
struct Outcome {
    heads: usize,
    notices: Vec<String>,
    errors: Vec<agent_core::TurnError>,
}

impl Outcome {
    fn stopped_on_its_wall_clock(&self) -> bool {
        self.errors.iter().any(|e| {
            e.outcome
                == TurnOutcome::BudgetExceeded {
                    dimension: BudgetDimension::WallClock,
                }
        })
    }

    fn renewed_its_clock(&self) -> bool {
        self.notices
            .iter()
            .any(|n| n.contains("fresh wall-clock allowance"))
    }
}

/// Run one turn over `messages`; each context-head checkpoint is answered
/// by `answer` (it may wait, cancel, then acknowledge or not).
async fn turn<F, Fut>(
    rt: &crate::Runtime,
    messages: Vec<SharedMessage>,
    cancel: tokio_util::sync::CancellationToken,
    answer: F,
) -> Outcome
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<()>>,
{
    let mut stream = rt
        .run_stream_with_messages(messages, cancel, None, None, false)
        .await;
    let mut out = Outcome::default();
    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::Session(SessionEvent::Error(e)) => out.errors.push(e),
            StreamEvent::Session(SessionEvent::Notice(n)) => out.notices.push(n),
            StreamEvent::Session(SessionEvent::ContextHeadCheckpoint { receipt, .. }) => {
                out.heads += 1;
                receipt.complete(answer().await);
            }
            _ => {}
        }
    }
    out
}

fn ack_after(wait: Duration) -> impl Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>>>> {
    move || {
        Box::pin(async move {
            tokio::time::sleep(wait).await;
            Ok(())
        })
    }
}

/// A worker runtime on a controlled run clock with a `max_elapsed` limit
/// (far beyond any real run): the test advances the clock past its
/// deadline at the phase it wants, so no scheduling decides the outcome.
fn controlled_worker(max_elapsed: Duration) -> (crate::Runtime, RunClock) {
    let mut rt = runtime(Some(max_elapsed), true);
    let clock = rt.turn_budget().run_clock.expect("a run clock").controlled();
    let mut budget = rt.turn_budget().clone();
    budget.run_clock = Some(clock);
    rt.set_turn_budget(budget);
    (rt, clock)
}

const HOUR: Duration = Duration::from_secs(3600);

/// D: no elapsed reset. The worker's head is saved past its deadline (the
/// clock is moved past it while the save is outstanding): its committed
/// rollover renews nothing, and its successor sends no request.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn d3_a_committed_worker_rollover_keeps_the_run_deadline() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (url, seen) = serve(|_| (Reply::Text, Duration::ZERO)).await;
    let _g = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let (worker, clock) = controlled_worker(HOUR);
    low_thresholds(&worker);
    let out = turn(&worker, big_history(), Default::default(), move || {
        clock.advance(HOUR + Duration::from_secs(1));
        async { Ok(()) }
    })
    .await;
    assert_eq!(out.heads, 1);
    assert_eq!(worker.continuation.lock().unwrap().window, 2, "committed");
    assert_eq!(seen.summaries(), 1, "summarized before the deadline");
    assert_eq!(seen.turns(), 0, "no successor request");
    assert!(out.stopped_on_its_wall_clock(), "{:?}", out.errors);
    assert!(!out.renewed_its_clock(), "{:?}", out.notices);
    assert!(
        out.notices.iter().any(|n| n.contains("run deadline is unchanged")),
        "{:?}",
        out.notices
    );
}

/// D: no time-checkpoint rollover. The worker's deadline passes while its
/// first request is answered (the stand-in moves the clock past it before
/// replying): it stops before its next request, with no head.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn d4_a_worker_deadline_is_never_a_time_checkpoint() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (worker, clock) = controlled_worker(HOUR);
    let (url, seen) = serve(move |n| {
        if n == 0 {
            clock.advance(HOUR + Duration::from_secs(1));
            (Reply::ToolUse, Duration::ZERO)
        } else {
            (Reply::Text, Duration::ZERO)
        }
    })
    .await;
    let _g = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let out = turn(&worker, small_history(), Default::default(), ack_after(Duration::ZERO)).await;
    assert_eq!(out.heads, 0, "no rollover on the run deadline");
    assert_eq!(seen.turns(), 1, "no request past the deadline");
    assert_eq!(seen.summaries(), 0);
    assert!(out.stopped_on_its_wall_clock(), "{:?}", out.errors);
}

/// Smoke (real time, loopback): the same as d3 with a session runtime as
/// the control, whose committed rollover renews its clock and sends the
/// successor's request. Its 1.3 s save against 1 s limits depends on the
/// runner's scheduling; d3 is the proof.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn d3_smoke_session_renews_and_worker_keeps_its_deadline() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (url, seen) = serve(|_| (Reply::Text, Duration::ZERO)).await;
    let _g = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let slow_save = ack_after(Duration::from_millis(1_300));

    let session = runtime(Some(Duration::from_secs(1)), false);
    low_thresholds(&session);
    let out = turn(&session, big_history(), Default::default(), &slow_save).await;
    assert_eq!(out.heads, 1);
    assert_eq!(seen.turns(), 1, "{:?}", out.errors);
    assert!(out.renewed_its_clock() && out.errors.is_empty());

    seen.reset();
    let worker = runtime(Some(Duration::from_secs(1)), true);
    low_thresholds(&worker);
    let out = turn(&worker, big_history(), Default::default(), &slow_save).await;
    assert_eq!(out.heads, 1);
    assert_eq!(seen.turns(), 0, "no successor request");
    assert!(out.stopped_on_its_wall_clock(), "{:?}", out.errors);
    assert!(!out.renewed_its_clock(), "{:?}", out.notices);
}

/// Smoke (real time, loopback): the same as d4 with a session runtime as
/// the control, which archives its head at the time limit and continues in
/// a fresh segment. Its 1.3 s reply against 1 s limits depends on the
/// runner's scheduling; d4 is the proof.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn d4_smoke_session_time_checkpoints_and_worker_stops() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (url, seen) = serve(|n| match n {
        0 => (Reply::ToolUse, Duration::from_millis(1_300)),
        _ => (Reply::Text, Duration::ZERO),
    })
    .await;
    let _g = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let ack = ack_after(Duration::ZERO);

    let session = runtime(Some(Duration::from_secs(1)), false);
    let out = turn(&session, small_history(), Default::default(), &ack).await;
    assert_eq!(out.heads, 1, "a time checkpoint: {:?}", out.errors);
    assert_eq!(seen.turns(), 2);
    assert!(out.errors.is_empty(), "{:?}", out.errors);

    seen.reset();
    let worker = runtime(Some(Duration::from_secs(1)), true);
    let out = turn(&worker, small_history(), Default::default(), &ack).await;
    assert_eq!(out.heads, 0, "no rollover on the run deadline");
    assert_eq!(seen.turns(), 1, "no request past the deadline");
    assert!(out.stopped_on_its_wall_clock(), "{:?}", out.errors);
}

/// D: cancel after the acknowledgement (no deadline: timeout zero). The
/// successor is committed and the run stops before its first request.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn d5_a_cancel_after_the_acknowledgement_sends_no_successor_request() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (url, seen) = serve(|_| (Reply::Text, Duration::ZERO)).await;
    let _g = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let worker = runtime(None, true);
    assert_eq!(worker.turn_budget().run_clock.unwrap().deadline(), None);
    low_thresholds(&worker);
    let cancel = tokio_util::sync::CancellationToken::new();
    let c = cancel.clone();
    let out = turn(&worker, big_history(), cancel, move || {
        let c = c.clone();
        async move {
            c.cancel();
            Ok(())
        }
    })
    .await;
    assert_eq!(out.heads, 1);
    assert_eq!(seen.summaries(), 1, "summarized before the cancel");
    assert_eq!(seen.turns(), 0, "no successor request");
    assert!(!out.stopped_on_its_wall_clock(), "{:?}", out.errors);
    assert_eq!(worker.continuation.lock().unwrap().window, 2, "committed");
}

/// D: a cancel that lands while the window is sealed stops the rollover
/// before any checkpoint: nothing to acknowledge, no request.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn d6_a_cancel_at_the_seal_stops_before_any_checkpoint() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (url, seen) = serve(|_| (Reply::Text, Duration::ZERO)).await;
    let _g = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let worker = runtime(None, true);
    low_thresholds(&worker);
    let cancel = tokio_util::sync::CancellationToken::new();
    let logical_id = worker.continuation.lock().unwrap().logical_id.clone();
    let sealing =
        crate::runtime::continuation::seal_seam::cancel_at_seal(&logical_id, cancel.clone());
    let out = turn(&worker, big_history(), cancel, ack_after(Duration::ZERO)).await;
    drop(sealing);
    assert_eq!(out.heads, 0, "nothing was asked to be saved");
    assert_eq!(seen.turns(), 0);
    assert_eq!(seen.summaries(), 0, "no summary after the cancel");
    assert_eq!(worker.continuation.lock().unwrap().window, 1, "nothing committed");
}

/// D: no summary past the deadline. The run deadline passes while the
/// window is sealed (the clock is moved past it at the seal): no summary
/// request goes out, and no successor request either.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn d7_a_deadline_passed_at_the_seal_sends_no_summary_request() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (url, seen) = serve(|_| (Reply::Text, Duration::ZERO)).await;
    let _g = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let (worker, clock) = controlled_worker(HOUR);
    low_thresholds(&worker);
    let logical_id = worker.continuation.lock().unwrap().logical_id.clone();
    let sealing = crate::runtime::continuation::seal_seam::run_at_seal(&logical_id, move || {
        clock.advance(HOUR + Duration::from_secs(1));
    });
    let out = turn(&worker, big_history(), Default::default(), ack_after(Duration::ZERO)).await;
    drop(sealing);
    assert_eq!(seen.summaries(), 0, "no summary request past the deadline");
    assert_eq!(seen.turns(), 0, "no successor request");
    assert!(out.stopped_on_its_wall_clock(), "{:?}", out.errors);
}

/// D: the summary waits at most until the run deadline. With one second
/// left at the seal and a summary model that answers after 30 s (inside
/// its own 60 s timeout), the summary is given up at the deadline and the
/// run stops there, not when the reply comes.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn d8_a_summary_waits_at_most_until_the_run_deadline() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (url, seen) = serve_with(|_| (Reply::Text, Duration::ZERO), Duration::from_secs(30)).await;
    let _g = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let (worker, clock) = controlled_worker(HOUR);
    low_thresholds(&worker);
    worker.continuation.lock().unwrap().config.summary_timeout_secs = 60;
    let logical_id = worker.continuation.lock().unwrap().logical_id.clone();
    let sealing = crate::runtime::continuation::seal_seam::run_at_seal(&logical_id, move || {
        // One second left, measured from the clock's own now: the real time
        // the turn took to reach the seal (seconds under a loaded suite)
        // must not eat the second.
        let left = clock.remaining().expect("a run limit");
        clock.advance(left.saturating_sub(Duration::from_secs(1)));
    });
    let started = std::time::Instant::now();
    let out = turn(&worker, big_history(), Default::default(), ack_after(Duration::ZERO)).await;
    let took = started.elapsed();
    drop(sealing);
    assert_eq!(seen.summaries(), 1, "the summary was sent before the deadline");
    assert!(took < Duration::from_secs(20), "bounded by the run deadline: {took:?}");
    assert_eq!(seen.turns(), 0, "no successor request");
    assert!(out.stopped_on_its_wall_clock(), "{:?}", out.errors);
}
