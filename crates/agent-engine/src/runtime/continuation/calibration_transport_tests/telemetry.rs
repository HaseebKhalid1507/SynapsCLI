//! Metadata-only context telemetry: one `context rollover` per committed
//! rollover, one `context window settled` per window, rate-limited
//! `context rollover deferred`. Captured with a thread-local layer.
//!
//! These turns stream text through the bounded stream relay, whose byte
//! counters are process-global; they share the relay tests' serial key so
//! the relay's exact-count assertions never see their traffic.
use super::*;
use std::collections::BTreeMap;
use tracing_subscriber::layer::SubscriberExt;

#[derive(Clone, Debug)]
struct Captured {
    message: String,
    fields: BTreeMap<String, String>,
}

#[derive(Clone, Default)]
struct Capture(Arc<std::sync::Mutex<Vec<Captured>>>);

struct Fields<'a>(&'a mut BTreeMap<String, String>);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().target() != "synaps::context" {
            return;
        }
        let mut fields = BTreeMap::new();
        event.record(&mut Fields(&mut fields));
        let message = fields.remove("message").unwrap_or_default();
        self.0.lock().unwrap().push(Captured { message, fields });
    }
}

impl Capture {
    fn named(&self, message: &str) -> Vec<Captured> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.message == message)
            .cloned()
            .collect()
    }
    fn all(&self) -> Vec<Captured> {
        self.0.lock().unwrap().clone()
    }
}

#[derive(Clone, Default)]
struct Lines(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Field capture plus the formatted lines a log reader would see.
fn capture_with_lines() -> (Capture, Lines, tracing::subscriber::DefaultGuard) {
    use tracing_subscriber::Layer as _;
    let capture = Capture::default();
    let lines = Lines::default();
    let writer = lines.clone();
    let fmt = tracing_subscriber::fmt::layer()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .without_time()
        .with_filter(
            tracing_subscriber::filter::Targets::new()
                .with_target("synaps::context", tracing::Level::TRACE),
        );
    let guard = tracing::subscriber::set_default(
        tracing_subscriber::registry()
            .with(capture.clone())
            .with(fmt),
    );
    (capture, lines, guard)
}

fn capture() -> (Capture, tracing::subscriber::DefaultGuard) {
    let (capture, _lines, guard) = capture_with_lines();
    (capture, guard)
}

/// A caller-supplied session id carrying a path, a credential, content and
/// control characters. None of it may reach a log.
const HOSTILE_SESSION: &str =
    "/home/alice/.ssh/id_rsa sk-ant-api03-SENTINELKEY\nINJECTED=1\u{1b}[31m\u{7} my secret plan";
const HOSTILE_NEEDLES: &[&str] = &[
    "/home/alice",
    "id_rsa",
    "sk-ant",
    "SENTINELKEY",
    "INJECTED",
    "secret plan",
    "\u{1b}",
    "\u{7}",
];

/// The id is replaced by a 12-hex keyed reference in every event and line.
fn assert_session_is_opaque(capture: &Capture, lines: &Lines) {
    let events = capture.all();
    assert!(!events.is_empty());
    for event in &events {
        assert!(!event.fields.contains_key("session"), "{event:?}");
        let r = &event.fields["session_ref"];
        assert!(
            r.len() == 12 && r.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "{event:?}"
        );
        for value in event.fields.values() {
            assert!(!value.contains('\n'), "{event:?}");
        }
    }
    let first = &events[0].fields["session_ref"];
    assert!(events.iter().all(|e| &e.fields["session_ref"] == first));
    assert_metadata_only(capture, HOSTILE_NEEDLES);
    let out = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
    assert_eq!(out.lines().count(), events.len(), "{out}");
    for needle in HOSTILE_NEEDLES {
        assert!(!out.contains(needle), "line leaks {needle:?}: {out}");
    }
}

fn num(event: &Captured, key: &str) -> u64 {
    event
        .fields
        .get(key)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("{key} is numeric: {event:?}"))
}

/// No field of any context event carries message content.
fn assert_metadata_only(capture: &Capture, needles: &[&str]) {
    for event in capture.all() {
        for (key, value) in &event.fields {
            for needle in needles {
                assert!(
                    !value.contains(needle),
                    "{key} leaks content {needle:?}: {event:?}"
                );
            }
        }
    }
}

/// Answers every request with usage equal to the request's size proxy and
/// remembers the proxies in call order.
async fn recording_mock() -> (
    Arc<std::sync::Mutex<Vec<u64>>>,
    crate::test_env::EnvVarGuard,
) {
    let proxies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let p = proxies.clone();
    let (url, _mock) = spawn(Arc::new(move |_, proxy| {
        p.lock().unwrap().push(proxy);
        (200, Some(json!({"input_tokens":proxy,"output_tokens":3,"cache_read_input_tokens":0,"cache_creation_input_tokens":0})))
    }))
    .await;
    let guard = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    (proxies, guard)
}

fn follow_up(history: &[SharedMessage]) -> Vec<SharedMessage> {
    let mut next = history.to_vec();
    next.push(Arc::new(json!({"role":"user","content":"again"})));
    next
}

fn bound_runtime(session: &str) -> crate::Runtime {
    let rt = runtime();
    rt.reset_context_continuation(session, &[]);
    rt.continuation.lock().unwrap().config.mode = ContextManagementMode::Auto;
    low_thresholds(&rt);
    rt
}

#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_committed_rollover_emits_one_metadata_event_then_one_settle() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (proxies, _g) = recording_mock().await;
    let rt = bound_runtime("20260916-101112-a3f9");
    let (capture, _sub) = capture();

    let out = turn_with(&rt, big_history(), Default::default()).await;
    assert_eq!(out.heads.len(), 1, "exactly one rollover");

    let rollovers = capture.named("context rollover");
    assert_eq!(rollovers.len(), 1, "{:?}", capture.all());
    let r = &rollovers[0];
    assert_eq!(r.fields["session"], "20260916-101112-a3f9");
    assert_eq!(num(r, "window_from"), 1);
    assert_eq!(num(r, "window_to"), 2);
    assert_eq!(r.fields["trigger"], "rollover_band");
    let archive = &r.fields["archive"];
    assert!(
        !archive.is_empty() && archive.chars().all(|c| c.is_ascii_alphanumeric()),
        "{r:?}"
    );
    assert_eq!(num(r, "target_tokens"), 25_000);
    for key in [
        "footprint_tokens",
        "target_history_tokens",
        "history_budget_tokens",
        "history_target_tokens",
        "overhead_tokens",
        "successor_tokens",
        "retained_messages",
        "dropped_messages",
        "must_keep_tokens",
        "continuation_tokens",
        "summary_tokens",
    ] {
        num(r, key);
    }
    assert!(num(r, "footprint_tokens") > num(r, "successor_tokens"));
    assert!(num(r, "dropped_messages") > 0);
    assert_eq!(
        num(r, "retained_messages") + num(r, "dropped_messages"),
        big_history().len() as u64
    );
    assert!(num(r, "successor_tokens") > num(r, "overhead_tokens"));
    assert!(
        ["written", "carried_forward", "skipped"].contains(&r.fields["summary"].as_str()),
        "{r:?}"
    );
    assert!(r.fields.contains_key("summary_reason"), "{r:?}");
    assert_eq!(num(r, "workers_carried"), 0, "no worker pending");
    assert!(!r.fields.contains_key("measured"), "a measured line: {r:?}");

    // The first provider-reported request of the new window settles it.
    let settled = capture.named("context window settled");
    assert_eq!(settled.len(), 1, "{:?}", capture.all());
    let s = &settled[0];
    assert_eq!(s.fields["session"], "20260916-101112-a3f9");
    assert_eq!(num(s, "window"), 2);
    assert_eq!(num(s, "rounds_since_rollover"), 1);
    assert_eq!(num(s, "successor_tokens"), num(r, "successor_tokens"));
    assert_eq!(num(s, "target_tokens"), 25_000);
    let last = *proxies.lock().unwrap().last().unwrap();
    assert_eq!(num(s, "provider_context_tokens"), last);
    num(s, "request_tokens");

    // A later request in the same window settles nothing new.
    turn_with(&rt, follow_up(&out.history), Default::default()).await;
    assert_eq!(capture.named("context window settled").len(), 1);
    assert_eq!(capture.named("context rollover").len(), 1);
    assert!(capture.named("context rollover deferred").is_empty());

    assert_metadata_only(&capture, &["charter", "worker_report", "xxxxxxxx", "again"]);
}

fn running_worker() -> crate::runtime::subagent::SubagentHandle {
    use crate::runtime::subagent::{SubagentHandle, SubagentState};
    SubagentHandle::new(
        "sa_busy".into(),
        0,
        "test-agent".into(),
        "secret worker task".into(),
        "claude-sonnet-4-6".into(),
        "s".into(),
        300,
        Arc::new(std::sync::RwLock::new(SubagentState::new())),
        None,
        None,
        None,
    )
}

#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn pending_workers_defer_is_logged_once_per_window() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (_proxies, _g) = recording_mock().await;
    let rt = bound_runtime(HOSTILE_SESSION);
    rt.subagent_registry()
        .lock()
        .unwrap()
        .register(running_worker());
    let (capture, lines, _sub) = capture_with_lines();

    // The reminder rides the request only; no saved history carries it.
    let reminder = "waiting briefly for pending workers";
    let persisted = |out: &Turn| {
        out.heads
            .iter()
            .chain(std::iter::once(&out.history))
            .flatten()
            .any(|m| m.to_string().contains(reminder))
    };
    for _ in 0..crate::runtime::continuation::WORKER_ROLLOVER_GRACE_ROUNDS {
        let out = turn_with(&rt, big_history(), Default::default()).await;
        assert!(out.heads.is_empty(), "no rollover during the grace");
        assert!(!persisted(&out), "the reminder is request-only");
    }
    let deferred = capture.named("context rollover deferred");
    assert_eq!(deferred.len(), 1, "once per window: {:?}", capture.all());
    let d = &deferred[0];
    assert_eq!(d.fields["reason"], "workers_pending");
    assert_eq!(num(d, "workers"), 1);
    assert_eq!(num(d, "window"), 1);
    assert!(num(d, "footprint_tokens") >= 80_000, "{d:?}");
    assert!(capture.named("context rollover").is_empty());

    // The grace is spent: the next round rolls over and carries the worker.
    let out = turn_with(&rt, big_history(), Default::default()).await;
    assert_eq!(out.heads.len(), 1, "rolls over after the grace");
    assert!(!persisted(&out), "the reminder is request-only");
    assert!(out.heads[0][0]["content"].as_str().unwrap().contains("sa_busy"));
    let rollovers = capture.named("context rollover");
    assert_eq!(rollovers.len(), 1, "{:?}", capture.all());
    assert_eq!(num(&rollovers[0], "workers_carried"), 1);
    assert_eq!(
        capture.named("context rollover deferred").len(),
        1,
        "the rollover itself logs no deferral"
    );
    assert_metadata_only(
        &capture,
        &["secret worker task", "worker_report", "charter"],
    );
    assert_session_is_opaque(&capture, &lines);
}

/// Errors and acknowledged heads of one turn; unlike `turn_with`, a refused
/// turn is returned, not a test failure.
async fn turn_outcome(
    rt: &crate::Runtime,
    messages: Vec<SharedMessage>,
) -> (Vec<String>, Vec<Vec<SharedMessage>>) {
    use futures::StreamExt;
    let mut stream = rt
        .run_stream_with_messages(messages, Default::default(), None, None, false)
        .await;
    let (mut errors, mut heads) = (Vec::new(), Vec::new());
    while let Some(event) = stream.next().await {
        match event {
            agent_core::StreamEvent::Session(agent_core::SessionEvent::Error(e)) => {
                errors.push(format!("{e:?}"))
            }
            agent_core::StreamEvent::Session(agent_core::SessionEvent::ContextHeadCheckpoint {
                messages,
                receipt,
                ..
            }) => {
                heads.push(messages);
                receipt.complete(Ok(()));
            }
            _ => {}
        }
    }
    (errors, heads)
}

/// The pending-worker wedge: at hard capacity with one pending reactive
/// worker, every turn used to be refused before any model call. Collecting
/// the worker needs exactly the round being refused, so the session could
/// never recover. A turn there must make progress: roll over, keep the
/// worker (not cancelled), and tell the next window about it.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_hard_stop_with_a_pending_worker_still_makes_progress() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (proxies, _g) = recording_mock().await;
    let mut rt = bound_runtime("20261017-120000-b2c3");
    // Less than one round's reserves left after the history: hard capacity,
    // not just the rollover band.
    rt.set_context_window(Some(140_000));
    rt.subagent_registry()
        .lock()
        .unwrap()
        .register(running_worker());
    let (capture, _sub) = capture();

    let mut history = big_history();
    let (errors, heads) = turn_outcome(&rt, history.clone()).await;
    assert!(errors.is_empty(), "turn refused: {errors:?}");
    assert_eq!(heads.len(), 1, "one rollover at hard capacity");
    let rollovers = capture.named("context rollover");
    assert_eq!(rollovers.len(), 1, "{:?}", capture.all());
    assert_eq!(rollovers[0].fields["trigger"], "hard_capacity");
    assert_eq!(num(&rollovers[0], "workers_carried"), 1);
    assert!(
        capture.named("context rollover deferred").is_empty(),
        "hard capacity never waits"
    );
    assert!(!proxies.lock().unwrap().is_empty(), "a model call was made");
    let carried = heads[0]
        .iter()
        .any(|m| m["content"].as_str().is_some_and(|c| c.contains("sa_busy")));
    assert!(carried, "the successor names the pending worker");
    assert!(
        rt.subagent_registry().lock().unwrap().list_active().iter().any(|(id, _, _)| id == "sa_busy"),
        "the worker keeps running"
    );
    // Later turns in the new window keep working.
    history = follow_up(&heads[0]);
    let (errors, _) = turn_outcome(&rt, history).await;
    assert!(errors.is_empty(), "next turn refused: {errors:?}");
}

#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn an_unproductive_rollover_is_logged_once() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (_proxies, _g) = recording_mock().await;
    let rt = bound_runtime("ephemeral-6f1c2d3e-4b5a-4c6d-8e7f-90a1b2c3d4e5");
    let (capture, _sub) = capture();

    // One request too large to drop: nothing can shrink.
    let history = vec![Arc::new(json!({
        "role":"user",
        "content":format!("unshrinkable {}", "y".repeat(400_000))
    }))];
    for _ in 0..3 {
        let out = turn_with(&rt, history.clone(), Default::default()).await;
        assert!(out.heads.is_empty(), "no rollover can shrink this");
    }
    let deferred = capture.named("context rollover deferred");
    assert_eq!(deferred.len(), 1, "{:?}", capture.all());
    let d = &deferred[0];
    assert_eq!(
        d.fields["session"],
        "ephemeral-6f1c2d3e-4b5a-4c6d-8e7f-90a1b2c3d4e5"
    );
    assert_eq!(d.fields["reason"], "unproductive_no_shrink");
    assert_eq!(num(d, "window"), 1);
    assert_eq!(num(d, "workers"), 0);
    assert!(num(d, "footprint_tokens") >= 80_000, "{d:?}");
    assert!(capture.named("context rollover").is_empty());
    assert_metadata_only(&capture, &["unshrinkable", "yyyyyyyy"]);
}

#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_caller_supplied_session_id_never_reaches_a_log() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (_proxies, _g) = recording_mock().await;
    let rt = bound_runtime(HOSTILE_SESSION);
    let (capture, lines, _sub) = capture_with_lines();
    let out = turn_with(&rt, big_history(), Default::default()).await;
    assert_eq!(out.heads.len(), 1);
    assert_eq!(capture.named("context rollover").len(), 1);
    assert_eq!(capture.named("context window settled").len(), 1);
    assert_session_is_opaque(&capture, &lines);
}

/// Anthropic SSE frames: a `message_start` reporting `input` context
/// tokens, then either a normal end or an in-stream error of `error_type`.
fn sse_frames(input: u64, error_type: Option<&str>) -> String {
    let usage = json!({"input_tokens":input,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0});
    let start = json!({"type":"message_start","message":{"id":"msg_t","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":usage}});
    let mut frames = vec![start];
    match error_type {
        Some(kind) => frames.push(json!({"type":"error","error":{"type":kind,"message":"synthetic"}})),
        None => frames.extend([
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":usage}),
            json!({"type":"message_stop"}),
        ]),
    }
    frames.iter().map(|e| format!("data: {e}\n\n")).collect()
}

type Frames = Arc<dyn Fn(usize, &str) -> String + Send + Sync>;

/// Serves `frames(index among successor requests, body)` to requests that
/// carry the continuation message, and a normal reply to anything else.
async fn successor_mock(frames: Frames) -> crate::test_env::EnvVarGuard {
    let seen = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        "/v1/messages",
        post(move |body: String| {
            let frames = frames.clone();
            let seen = seen.clone();
            async move {
                let sse = if body.contains("[Context continuation") {
                    frames(seen.fetch_add(1, Ordering::SeqCst), &body)
                } else {
                    sse_frames(1_000, None)
                };
                (StatusCode::OK, [("content-type", "text/event-stream")], sse).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &format!("http://{addr}"))
}

/// Like `turn_with`, but a failed turn is an expected outcome: returns the
/// last published history and whether the turn reported an error.
async fn turn_allowing_failure(
    rt: &crate::Runtime,
    messages: Vec<SharedMessage>,
) -> (Vec<SharedMessage>, bool) {
    let mut stream = rt
        .run_stream_with_messages(messages, Default::default(), None, None, false)
        .await;
    let (mut history, mut failed) = (Vec::new(), false);
    while let Some(event) = stream.next().await {
        match event {
            agent_core::StreamEvent::Session(agent_core::SessionEvent::Error(_)) => failed = true,
            agent_core::StreamEvent::Session(agent_core::SessionEvent::MessageHistory(h)) => {
                history = h
            }
            agent_core::StreamEvent::Session(agent_core::SessionEvent::ContextHeadCheckpoint {
                receipt,
                ..
            }) => receipt.complete(Ok(())),
            _ => {}
        }
    }
    (history, failed)
}

#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn usage_reported_by_a_failed_first_request_settles_the_window_once() {
    let _base = crate::test_env::BaseDirGuard::new();
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let f = fail.clone();
    let _g = successor_mock(Arc::new(move |_, _| {
        if f.load(Ordering::SeqCst) {
            sse_frames(7_777, Some("invalid_request_error"))
        } else {
            sse_frames(8_888, None)
        }
    }))
    .await;
    let rt = bound_runtime("20260916-101115-d6e2");
    let (capture, _sub) = capture();

    let (history, failed) = turn_allowing_failure(&rt, big_history()).await;
    assert!(failed, "the successor's first request fails after usage");
    assert_eq!(capture.named("context rollover").len(), 1);
    let settled = capture.named("context window settled");
    assert_eq!(settled.len(), 1, "{:?}", capture.all());
    assert_eq!(num(&settled[0], "provider_context_tokens"), 7_777);
    assert_eq!(num(&settled[0], "rounds_since_rollover"), 1);

    fail.store(false, Ordering::SeqCst);
    let (_, failed) = turn_allowing_failure(&rt, follow_up(&history)).await;
    assert!(!failed);
    assert_eq!(
        capture.named("context window settled").len(),
        1,
        "once per window"
    );
}

#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_retried_request_settles_on_the_first_reported_attempt() {
    let _base = crate::test_env::BaseDirGuard::new();
    let _g = successor_mock(Arc::new(|n, _| {
        if n == 0 {
            sse_frames(5_555, Some("overloaded_error"))
        } else {
            sse_frames(6_666, None)
        }
    }))
    .await;
    let rt = bound_runtime("20260916-101116-e7f3");
    let (capture, _sub) = capture();

    let (_, failed) = turn_allowing_failure(&rt, big_history()).await;
    assert!(!failed, "the retry answers");
    let settled = capture.named("context window settled");
    assert_eq!(settled.len(), 1, "{:?}", capture.all());
    assert_eq!(num(&settled[0], "provider_context_tokens"), 5_555);
    assert_eq!(num(&settled[0], "rounds_since_rollover"), 1);
}

#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_model_switch_before_settling_drops_the_probe() {
    let _base = crate::test_env::BaseDirGuard::new();
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let f = fail.clone();
    // The successor's first request fails before any usage is reported.
    let _g = successor_mock(Arc::new(move |_, _| {
        if f.load(Ordering::SeqCst) {
            "data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"x\"}}\n\n".into()
        } else {
            sse_frames(4_444, None)
        }
    }))
    .await;
    let mut rt = bound_runtime("20260916-101117-f8a4");
    let (capture, _sub) = capture();
    let (history, failed) = turn_allowing_failure(&rt, big_history()).await;
    assert!(failed);
    assert_eq!(capture.named("context rollover").len(), 1);
    assert!(capture.named("context window settled").is_empty());

    fail.store(false, Ordering::SeqCst);
    rt.set_model("claude-opus-4-7".to_string());
    let (_, failed) = turn_allowing_failure(&rt, follow_up(&history)).await;
    assert!(!failed);
    assert!(
        capture.named("context window settled").is_empty(),
        "another model's footprint is not this rollover's settle: {:?}",
        capture.all()
    );
}

/// A subscriber with no interest in anything, for this thread.
fn quiet() -> tracing::subscriber::DefaultGuard {
    tracing::subscriber::set_default(
        tracing_subscriber::registry().with(tracing_subscriber::filter::Targets::new()),
    )
}

/// A 20-byte caller id whose multi-byte character straddles the session
/// shape's first slicing point: the label must become a reference, never a
/// panic, and the rollover must commit as with telemetry off.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_unicode_session_id_rolls_over_with_tracing_disabled() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (_proxies, _g) = recording_mock().await;
    let id = "1234567\u{e9}12345678901";
    assert_eq!(id.len(), 20);
    let rt = bound_runtime(id);
    let _quiet = quiet();
    let out = turn_with(&rt, big_history(), Default::default()).await;
    assert_eq!(out.heads.len(), 1);
    assert_eq!(rt.continuation.lock().unwrap().window, 2, "committed");
    assert_eq!(out.history.first(), out.heads[0].first(), "published");
}

/// Interest appears only while the frontend is acknowledging the head: the
/// successor was not measured, so the rollover is logged with
/// `measured=false` and no size fields, never as zeros.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn tracing_enabled_during_the_head_ack_reports_no_unmeasured_sizes() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (_proxies, _g) = recording_mock().await;
    let rt = bound_runtime("20260916-101118-a9b5");
    let mut quiet = Some(quiet());
    let mut capture_guard = None;
    let mut stream = rt
        .run_stream_with_messages(big_history(), Default::default(), None, None, false)
        .await;
    while let Some(event) = stream.next().await {
        match event {
            agent_core::StreamEvent::Session(agent_core::SessionEvent::Error(e)) => {
                panic!("turn failed: {e:?}")
            }
            agent_core::StreamEvent::Session(agent_core::SessionEvent::ContextHeadCheckpoint {
                receipt,
                ..
            }) => {
                assert!(capture_guard.is_none(), "one rollover");
                drop(quiet.take());
                capture_guard = Some(capture());
                receipt.complete(Ok(()));
            }
            _ => {}
        }
    }
    let (capture, _sub) = capture_guard.expect("a rollover head");
    let rollovers = capture.named("context rollover");
    assert_eq!(rollovers.len(), 1, "{:?}", capture.all());
    let r = &rollovers[0];
    assert_eq!(
        r.fields.get("measured").map(String::as_str),
        Some("false"),
        "{r:?}"
    );
    for key in ["successor_tokens", "continuation_tokens", "summary_tokens"] {
        assert!(!r.fields.contains_key(key), "unmeasured {key}: {r:?}");
    }
    assert!(num(r, "dropped_messages") > 0, "counts are still real");
    num(r, "overhead_tokens");
    assert!(
        capture.named("context window settled").is_empty(),
        "no probe was armed: {:?}",
        capture.all()
    );
}
