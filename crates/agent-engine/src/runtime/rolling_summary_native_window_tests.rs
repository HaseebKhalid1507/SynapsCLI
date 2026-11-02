// The 1M entitlement fallback (`runtime::native_window`), included into
// `rolling_summary::transport_tests::warm_tests::native_window_tests`: a
// stream request that carried the 1M beta, over the native 200k window, and
// was refused with a long-context class latches the session's model to its
// native window, rolls over there with a cold summary and is retried once.
// The provider's real reply to an account without the 1M entitlement is not
// known: every refusal here is synthetic. Every endpoint is a loopback mock.

/// The session's model: 1M-capable, native window 200k.
const MODEL: &str = "claude-sonnet-4-6";
/// The 1M context beta.
const BETA_1M: &str = "context-1m-2025-08-07";
/// The fallback's `synaps::context` line.
const FALLBACK: &str = "context window fallback to native";
/// The fallback's notice.
const FALLBACK_NOTICE: &str = "didn't accept the 1M context window";
/// A synthetic overflow refusal, shaped like the documented 400.
const TOO_LONG: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 215000 tokens > 200000 maximum"}}"#;
/// A synthetic long-context entitlement refusal.
const NOT_ENTITLED: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"The long context beta is not available for this subscription."}}"#;

/// Refuses conversation requests that carry the 1M beta (and, when
/// `native_too`, those without it) with `refusal`; answers every other
/// conversation request, and a cold summary, like the warm mock.
#[derive(Clone, Default)]
struct NativeMock {
    requests: Arc<Mutex<Vec<Seen>>>,
    refusal: Arc<Mutex<Option<(u16, String)>>>,
    native_too: Arc<Mutex<bool>>,
}

impl NativeMock {
    /// Conversation (streaming) requests, in order.
    fn turns(&self) -> Vec<Seen> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.body.contains("\"stream\":true"))
            .cloned()
            .collect()
    }

    /// Non-streaming requests: the cold summary path.
    fn cold(&self) -> Vec<Seen> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|s| !s.body.contains("\"stream\":true"))
            .cloned()
            .collect()
    }

    fn warm_requests(&self) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.body.contains(WARM_MARK))
            .count()
    }
}

fn carries_beta(seen: &Seen) -> bool {
    seen.headers
        .get("anthropic-beta")
        .is_some_and(|betas| betas.contains(BETA_1M))
}

async fn spawn_native(status: u16, refusal: &str) -> (NativeMock, crate::test_env::EnvVarGuard) {
    let mock = NativeMock::default();
    *mock.refusal.lock().unwrap() = Some((status, refusal.to_owned()));
    let app = Router::new()
        .route(
            "/v1/messages",
            post(
                |State(m): State<NativeMock>, headers: axum::http::HeaderMap, body: String| async move {
                    let headers = headers
                        .iter()
                        .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
                        .collect();
                    let seen = Seen { body, headers };
                    let stream = seen.body.contains("\"stream\":true");
                    let refuse = stream && (carries_beta(&seen) || *m.native_too.lock().unwrap());
                    m.requests.lock().unwrap().push(seen);
                    let json_type = [("content-type", "application/json")];
                    if !stream {
                        let reply = json!({"id":"s","type":"message","role":"assistant","model":MODEL,"content":[{"type":"text","text":"## Window 1\nCOLD-SUMMARY"}],"stop_reason":"end_turn","usage":{"input_tokens":30_000,"output_tokens":300}});
                        return (StatusCode::OK, json_type, reply.to_string()).into_response();
                    }
                    let refusal = m.refusal.lock().unwrap().clone();
                    if let Some((status, body)) = refusal.filter(|_| refuse) {
                        return (StatusCode::from_u16(status).unwrap(), json_type, body)
                            .into_response();
                    }
                    let usage = json!({"input_tokens":100,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0});
                    (
                        StatusCode::OK,
                        [("content-type", "text/event-stream")],
                        streamed("ok", usage, 5),
                    )
                        .into_response()
                },
            ),
        )
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let guard =
        crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &format!("http://{addr}"));
    (mock, guard)
}

/// A session at `context_window = 1m`, its bands left to the defaults
/// (250k/400k at 1M, 140k/180k at the native 200k).
fn runtime_1m() -> crate::Runtime {
    let mut rt = runtime(Some(SUMMARY));
    rt.set_context_window(Some(1_000_000));
    {
        let mut s = rt.continuation.lock().unwrap();
        s.config.pressure_tokens = None;
        s.config.rollover_tokens = None;
    }
    rt
}

/// A history estimated just over the native 200k window: the 1M window
/// sends it without a rollover.
fn over_native() -> Vec<SharedMessage> {
    let base = crate::runtime::context::estimate_history(&history_with(None, 0));
    let each = crate::runtime::context::estimate_history(&[event(9_999)]);
    let history = history_with(None, ((210_000 - base) / each) as usize);
    let tokens = crate::runtime::context::estimate_history(&history);
    assert!(
        (205_000..225_000).contains(&tokens),
        "fixture: {tokens} estimated tokens"
    );
    history
}

/// One turn on a 1M session over the native window, its 1M request
/// refused by the mock with `status`: one fallback (log, notice, latch), a
/// rollover at the native window with a cold summary, and one retry that
/// no longer carries the beta.
async fn assert_falls_back(
    mock: &NativeMock,
    events: &ContextEvents,
    status: u16,
    class: &str,
) -> (crate::Runtime, Turn) {
    let rt = runtime_1m();
    let out = turn(&rt, over_native(), Default::default()).await;
    assert!(out.errors.is_empty(), "{status}: {:?}", out.errors);

    // One typed line: metadata only, never the provider's text.
    let lines = events.logged(FALLBACK);
    assert_eq!(lines.len(), 1, "{status}: {lines:?}");
    let line = &lines[0];
    assert_eq!(line["status"], status.to_string());
    assert_eq!(line["class"], class);
    assert_eq!(line["window_from_tokens"], "1000000");
    assert_eq!(line["window_to_tokens"], "200000");
    assert!(
        line.values()
            .all(|v| !v.contains("too long") && !v.contains("subscription")),
        "{line:?}"
    );

    // One notice, naming the window the session continues at.
    let notices: Vec<_> = out
        .notices
        .iter()
        .filter(|n| n.contains(FALLBACK_NOTICE))
        .collect();
    assert_eq!(notices.len(), 1, "{status}: {:?}", out.notices);
    assert!(notices[0].contains("200k"), "{}", notices[0]);
    assert!(!notices[0].contains("subscription") && !notices[0].contains("too long"));

    // One rollover, sized for the native window, its summary cold.
    let rollover = events.rollover();
    let field = |key: &str| rollover.get(key).map(String::as_str);
    assert_eq!(field("trigger"), Some("native_window"));
    assert_eq!(field("summary_attempted_path"), Some("cold"));
    assert_eq!(field("summary"), Some("written"));
    assert!(number(&rollover, "footprint_tokens") > 200_000);
    assert!(number(&rollover, "history_budget_tokens") < 200_000);
    assert_eq!(out.heads.len(), 1, "one rollover head");
    let summary = summary_of(&out.heads[0]).expect("a summary");
    assert!(summary.contains("COLD-SUMMARY"), "{summary}");
    assert_eq!(mock.warm_requests(), 0, "the warm path resends the refused prefix");
    let cold = mock.cold();
    assert_eq!(cold.len(), 1, "one cold summary request");
    assert!(!carries_beta(&cold[0]), "the summary never asks for the 1M window");

    // The refused request, then exactly one retry, without the beta, on the
    // rolled-over context.
    let turns = mock.turns();
    assert_eq!(turns.len(), 2, "{status}: the refused request and one retry");
    assert!(carries_beta(&turns[0]), "fixture: the refused request asked for 1M");
    assert!(!carries_beta(&turns[1]), "{:?}", turns[1].headers);
    assert!(turns[1].body.contains("COLD-SUMMARY"));
    assert!(turns[1].body.len() < turns[0].body.len() / 2);

    // The session's model is latched; the configured window is kept.
    assert_eq!(rt.context_window(), 200_000);
    assert!(!rt.uses_1m_context());
    assert_eq!(rt.context_window_override, Some(1_000_000));
    (rt, out)
}

/// A request that carried the 1M beta, over the native window, refused
/// with the documented "prompt is too long" 400: the session falls back to
/// the native window and retries once. The latch outlives the turn and is
/// never written to config.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_prompt_too_long_refusal_of_the_1m_window_falls_back_and_retries_once() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (events, _trace) = ContextEvents::install();
    let (mock, _g) = spawn_native(400, TOO_LONG).await;
    let (rt, first) = assert_falls_back(&mock, &events, 400, "prompt_too_long").await;

    // The next turn stays at the native window: no beta, no new fallback.
    let mut next = first.history.clone();
    next.push(msg(json!({"role":"user","content":"carry on"})));
    let second = turn(&rt, next, Default::default()).await;
    assert!(second.errors.is_empty(), "{:?}", second.errors);
    assert!(second.heads.is_empty(), "no second rollover");
    let turns = mock.turns();
    assert_eq!(turns.len(), 3);
    assert!(!carries_beta(&turns[2]), "{:?}", turns[2].headers);
    assert_eq!(events.logged(FALLBACK).len(), 1);
    assert!(!second.notices.iter().any(|n| n.contains(FALLBACK_NOTICE)));
    events.rollover();

    // Runtime state only: nothing was written to config.
    assert!(!agent_core::config::resolve_read_path("config").exists());
    let s = rt.continuation.lock().unwrap();
    assert_eq!((s.config.pressure_tokens, s.config.rollover_tokens), (None, None));
    assert_eq!(rt.context_window_override, Some(1_000_000));
}

/// The narrow long-context entitlement class (synthetic bodies) falls back
/// the same way on a 400, a 403 or a 429; the 429 burns no retries.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_long_context_entitlement_refusal_falls_back_the_same_way() {
    let _base = crate::test_env::BaseDirGuard::new();
    for status in [403, 429, 400] {
        let (events, _trace) = ContextEvents::install();
        let (mock, _g) = spawn_native(status, NOT_ENTITLED).await;
        assert_falls_back(&mock, &events, status, "long_context_entitlement").await;
    }
}

/// After the fallback the retry is refused again: the turn ends with the
/// error as before. One latch, one rollover, one retry: no loop.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_refusal_after_the_fallback_ends_the_turn() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (events, _trace) = ContextEvents::install();
    let (mock, _g) = spawn_native(400, TOO_LONG).await;
    *mock.native_too.lock().unwrap() = true;
    let rt = runtime_1m();
    let out = turn(&rt, over_native(), Default::default()).await;
    assert_eq!(out.errors.len(), 1, "{:?}", out.errors);
    let turns = mock.turns();
    assert_eq!(turns.len(), 2, "the refused request and one retry");
    assert!(carries_beta(&turns[0]) && !carries_beta(&turns[1]));
    assert_eq!(events.logged(FALLBACK).len(), 1);
    assert_eq!(events.rollover().get("trigger").map(String::as_str), Some("native_window"));
    assert_eq!(
        out.notices
            .iter()
            .filter(|n| n.contains(FALLBACK_NOTICE))
            .count(),
        1
    );
    assert_eq!(rt.context_window(), 200_000);
}

/// Nothing fell back: no line, no rollover, no notice, no latch, and the
/// session keeps `window`.
fn assert_no_fallback(rt: &crate::Runtime, out: &Turn, events: &ContextEvents, window: u64) {
    assert_eq!(out.errors.len(), 1, "the turn ends as before: {:?}", out.errors);
    assert!(events.logged(FALLBACK).is_empty());
    assert!(events.logged("context rollover").is_empty());
    assert!(
        !out.notices.iter().any(|n| n.contains(FALLBACK_NOTICE)),
        "{:?}",
        out.notices
    );
    assert!(!rt.native_window.is_latched(MODEL));
    assert_eq!(rt.context_window(), window);
}

/// Without the 1M beta an overflow is an overflow: the turn ends as before.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn an_overflow_without_the_1m_beta_does_not_fall_back() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (events, _trace) = ContextEvents::install();
    let (mock, _g) = spawn_native(400, TOO_LONG).await;
    *mock.native_too.lock().unwrap() = true;
    // A 500k window: no beta is sent.
    let rt = runtime(Some(SUMMARY));
    set_thresholds(&rt, 300_000, 400_000);
    let out = turn(&rt, over_native(), Default::default()).await;
    let turns = mock.turns();
    assert_eq!(turns.len(), 1);
    assert!(!carries_beta(&turns[0]));
    assert_no_fallback(&rt, &out, &events, 500_000);
}

/// At or below the native window a refusal is not an entitlement signal,
/// even with the beta.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_refusal_below_the_native_window_does_not_fall_back() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (events, _trace) = ContextEvents::install();
    let (mock, _g) = spawn_native(400, TOO_LONG).await;
    let rt = runtime_1m();
    let out = turn(&rt, history_with(None, 20), Default::default()).await;
    let turns = mock.turns();
    assert_eq!(turns.len(), 1);
    assert!(carries_beta(&turns[0]));
    assert_no_fallback(&rt, &out, &events, 1_000_000);
    assert!(rt.uses_1m_context());
}

/// Any other refusal of a 1M request over the native window ends the turn
/// as before.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn an_unrelated_refusal_does_not_fall_back() {
    let _base = crate::test_env::BaseDirGuard::new();
    for (status, body) in [
        (
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"synthetic"}}"#,
        ),
        (
            403,
            r#"{"type":"error","error":{"type":"permission_error","message":"This feature is not available on your plan"}}"#,
        ),
    ] {
        let (events, _trace) = ContextEvents::install();
        let (mock, _g) = spawn_native(status, body).await;
        let rt = runtime_1m();
        let out = turn(&rt, over_native(), Default::default()).await;
        let turns = mock.turns();
        assert_eq!(turns.len(), 1, "{status}");
        assert!(carries_beta(&turns[0]));
        assert_no_fallback(&rt, &out, &events, 1_000_000);
        assert!(rt.uses_1m_context());
    }
}

/// The latch is held per (session, model): clones are the same session, a
/// fresh runtime (a subagent) is not; another model starts unlatched and
/// switching back keeps it. The configured window is never changed.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn the_latch_is_held_per_session_and_model() {
    let _base = crate::test_env::BaseDirGuard::new();
    let mut rt = runtime_1m();
    let clone = rt.clone();
    let fresh = runtime_1m();
    assert!(rt.native_window.latch(MODEL));
    assert!(!clone.native_window.latch(MODEL), "one latch per model");
    assert!(!clone.uses_1m_context());
    assert_eq!(clone.context_window(), 200_000);
    assert!(fresh.uses_1m_context());
    assert_eq!(fresh.context_window(), 1_000_000);
    rt.set_model("claude-opus-4-6".to_string());
    assert!(rt.uses_1m_context());
    assert_eq!(rt.context_window(), 1_000_000);
    rt.set_model(MODEL.to_string());
    assert!(!rt.uses_1m_context());
    assert_eq!(rt.context_window(), 200_000);
    assert_eq!(rt.context_window_override, Some(1_000_000));
}
