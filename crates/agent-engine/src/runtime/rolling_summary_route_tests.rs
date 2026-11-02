// Rolling-summary transport tests, included into
// `rolling_summary::transport_tests::route_tests` (its helpers and mocks):
// redirects, fail-closed redirect-free clients, usage reported by failed
// replies, the remote broker (proxied routes skipped, Anthropic vended), the
// summary model's selection, and notices. Every endpoint is a loopback mock.
use std::sync::atomic::{AtomicUsize, Ordering};

/// What one settled summary update left behind.
struct Settled {
    reserved: f64,
    measured: f64,
    summary: Option<String>,
    /// Why no summary was attempted, when a reason was given.
    skip: Option<super::super::SummarySkip>,
    /// (input, output, model) of every usage event the update booked.
    usages: Vec<(u64, u64, Option<String>)>,
}

async fn settle_events(rt: &crate::Runtime) -> Settled {
    settle_with(&rt.rollover_summarizer().expect("summarizer")).await
}

/// A summarizer for `model` built directly, bypassing the model selection
/// (and so its remote-proxy skip): the remote broker mock is the only
/// loopback route to a Responses endpoint, used to test how failed replies
/// are booked. Production never sends a bounded summary through the proxy.
fn direct_summarizer(rt: &crate::Runtime, model: &str) -> super::super::RolloverSummarizer {
    let runtime = rt.clone();
    let call_model = model.to_owned();
    super::super::RolloverSummarizer::new(
        Arc::new(move |messages, max_output_tokens, gate| {
            let runtime = runtime.clone();
            let model = call_model.clone();
            Box::pin(async move {
                runtime
                    .rollover_summary_call(&model, messages, max_output_tokens, gate)
                    .await
            })
        }),
        rt.compaction_policy(),
        model.to_owned(),
    )
}

async fn settle_with(summarizer: &super::super::RolloverSummarizer) -> Settled {
    settle_after(summarizer, None).await
}

/// [`settle_with`] with a previous summary to carry.
async fn settle_after(
    summarizer: &super::super::RolloverSummarizer,
    previous: Option<&str>,
) -> Settled {
    let usage = crate::runtime::budget::UsageCounters::default();
    let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let dropped: Vec<SharedMessage> = (0..20).map(event).collect();
    let archive = "a".repeat(32);
    let out = super::super::update(
        summarizer,
        super::super::SummaryRequest {
            logical_id: "session",
            previous,
            dropped: &dropped,
            window: 2,
            archive_id: &archive,
            cap_tokens: SUMMARY,
            timeout: Duration::from_secs(30),
            cancel: &Default::default(),
            accounting: Some(super::super::SummaryAccounting {
                usage: &usage,
                max_cost_usd: None,
                events: &events,
                tap: None,
                ledger: None,
            }),
            snapshot: None,
            run_deadline: None,
        },
    )
    .await;
    drop(events);
    let mut usages = Vec::new();
    while let Some(event) = rx.recv().await {
        if let crate::StreamEvent::Session(crate::SessionEvent::Usage {
            input_tokens,
            output_tokens,
            model,
            ..
        }) = event
        {
            usages.push((input_tokens, output_tokens, model));
        }
    }
    Settled {
        reserved: usage.reserved_usd(),
        measured: usage.cost_usd(),
        skip: out.as_ref().err().copied().flatten(),
        summary: out.ok(),
        usages,
    }
}

/// Reads one complete HTTP request (headers and `content-length` body).
async fn read_request(socket: &mut tokio::net::TcpStream) -> usize {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 65_536];
    let body_start = loop {
        let Ok(n) = socket.read(&mut chunk).await else {
            return buf.len();
        };
        if n == 0 {
            return buf.len();
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..body_start]).to_ascii_lowercase();
    let length: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    while buf.len() < body_start + length {
        let Ok(n) = socket.read(&mut chunk).await else {
            break;
        };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    buf.len()
}

/// A loopback endpoint that reads each whole request and answers it with
/// `307` to `location`. Returns its `http://host:port` and the request
/// bytes it received.
async fn spawn_redirector(location: String) -> (String, Arc<AtomicUsize>) {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let received = Arc::new(AtomicUsize::new(0));
    let seen = received.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let seen = seen.clone();
            let location = location.clone();
            tokio::spawn(async move {
                seen.fetch_add(read_request(&mut socket).await, Ordering::SeqCst);
                let reply = format!(
                    "HTTP/1.1 307 Temporary Redirect\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                );
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (format!("http://{addr}"), received)
}

/// An endpoint answering every request with HTTP 400 and an Anthropic-style
/// invalid-request body; counts the requests.
async fn spawn_rejecting() -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let app = Router::new().fallback(move || {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            (
                StatusCode::BAD_REQUEST,
                [("content-type", "application/json")],
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"no"}}"#,
            )
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hits)
}

fn closed_port() -> String {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = closed.local_addr().unwrap();
    drop(closed);
    format!("http://{addr}")
}

/// A redirect is never followed by a bounded summary request: whatever the
/// redirect points at (a refused port, an error), the first hop received
/// the request, so its spend stays unknown and the reservation is kept. A
/// direct refused connection still releases it.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn a_redirected_summary_keeps_its_reservation() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (rejecting, rejected) = spawn_rejecting().await;
    {
        // Routed (`local/…`) summary through the broker.
        let (_mock, _g) = spawn(Reply::Status(500)).await;
        for target in [
            format!("{}/v1/chat/completions", closed_port()),
            format!("{rejecting}/v1/chat/completions"),
        ] {
            let (url, received) = spawn_redirector(target.clone()).await;
            let _l = crate::test_env::EnvVarGuard::set("LOCAL_ENDPOINT", &format!("{url}/v1"));
            let (reserved, measured) = settle(&local_runtime(), Duration::from_secs(30)).await;
            assert!(received.load(Ordering::SeqCst) > 0, "the first hop got it");
            assert!(
                reserved > 0.0 && measured == 0.0,
                "routed, redirect to {target}: spend unknown, reservation kept"
            );
        }
        let _l =
            crate::test_env::EnvVarGuard::set("LOCAL_ENDPOINT", &format!("{}/v1", closed_port()));
        let (reserved, _) = settle(&local_runtime(), Duration::from_secs(30)).await;
        assert_eq!(reserved, 0.0, "routed, direct refusal: nothing sent");
    }
    {
        // Direct Anthropic Messages summary.
        for target in [
            format!("{}/v1/messages", closed_port()),
            format!("{rejecting}/v1/messages"),
        ] {
            let (url, received) = spawn_redirector(target.clone()).await;
            let _g = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
            let (reserved, measured) =
                settle(&runtime(Some(SUMMARY)), Duration::from_secs(30)).await;
            assert!(received.load(Ordering::SeqCst) > 0, "the first hop got it");
            assert!(
                reserved > 0.0 && measured == 0.0,
                "Anthropic, redirect to {target}: spend unknown, reservation kept"
            );
        }
        let _g = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &closed_port());
        let (reserved, _) = settle(&runtime(Some(SUMMARY)), Duration::from_secs(30)).await;
        assert_eq!(reserved, 0.0, "Anthropic, direct refusal: nothing sent");
    }
    assert_eq!(
        rejected.load(Ordering::SeqCst),
        0,
        "no redirect was followed"
    );
}

/// A mock of the remote credential broker: records each `/proxy` request
/// and its machine token (answered by upstream path), and vends an
/// Anthropic access token on `/token`, counting those calls.
#[derive(Clone, Default)]
struct RemoteBroker {
    requests: Arc<Mutex<Vec<Value>>>,
    tokens: Arc<Mutex<Vec<String>>>,
    responses: Arc<Mutex<String>>,
    vended: Arc<AtomicUsize>,
}

const CHAT_REPLY: &str = concat!(
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"## Window 2\\nREMOTE-CHAT\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":410,\"completion_tokens\":41,\"total_tokens\":451}}\n\n",
    "data: [DONE]\n\n",
);

fn responses_reply(events: &[Value]) -> String {
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

fn responses_text() -> String {
    responses_reply(&[
        json!({"type":"response.output_text.delta","delta":"## Window 2\nREMOTE-RESPONSES"}),
        json!({"type":"response.completed","response":{"usage":{"input_tokens":420,"output_tokens":42}}}),
    ])
}

async fn spawn_remote_broker(responses: String) -> (RemoteBroker, String) {
    let mock = RemoteBroker {
        responses: Arc::new(Mutex::new(responses)),
        ..Default::default()
    };
    let app =
        Router::new()
            .route(
                "/proxy",
                post(
                    |State(m): State<RemoteBroker>,
                     headers: axum::http::HeaderMap,
                     body: String| async move {
                        let request: Value = serde_json::from_str(&body).unwrap();
                        m.tokens.lock().unwrap().push(
                            headers
                                .get("authorization")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("")
                                .to_string(),
                        );
                        let reply = if request["path"] == "/responses" {
                            m.responses.lock().unwrap().clone()
                        } else {
                            CHAT_REPLY.to_string()
                        };
                        m.requests.lock().unwrap().push(request);
                        (
                            StatusCode::OK,
                            [("content-type", "text/event-stream")],
                            reply,
                        )
                    },
                ),
            )
            .route(
                "/token",
                axum::routing::get(|State(m): State<RemoteBroker>| async move {
                    m.vended.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        r#"{"access_token":"sk-ant-vended-test","expires":9999999999999}"#,
                    )
                }),
            )
            .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (mock, format!("http://{addr}"))
}

fn remote(mut rt: crate::Runtime, endpoint: &str) -> crate::Runtime {
    rt.credential_source = crate::auth::CredentialSource::Remote {
        endpoint: endpoint.to_string(),
        machine_token: "machine-token".to_string(),
    };
    rt
}

/// The remote broker's daemon dispatches proxied requests on an ordinary,
/// redirect-following broker, so a single upstream attempt cannot be
/// guaranteed there: a summary whose route would be proxied is skipped
/// before anything is sent or reserved, for `auto` and explicit models
/// alike, with or without credentials that would also work locally.
#[tokio::test]
// Whole turns go through the stream relay, whose counters the relay flood
// test reads globally.
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn remote_proxied_summaries_are_skipped_before_any_request() {
    use super::super::SummarySkip::*;
    let _base = crate::test_env::BaseDirGuard::new();
    let (anthropic, _g) = spawn(Reply::Status(500)).await;
    let (local_bodies, _l) = spawn_local(Local::Reply("## Window 2\nLOCAL-ENDPOINT")).await;
    let (broker, endpoint) = spawn_remote_broker(responses_text()).await;
    for local_key in ["", "gsk_local_conflicting_key"] {
        let _k = crate::test_env::EnvVarGuard::set("GROQ_API_KEY", local_key);
        // (session model, compaction model, explicit summary model)
        let cases: [(&str, Option<&str>, Option<&str>); 5] = [
            (
                "github-copilot/gpt-5-mini",
                Some("openai-codex/gpt-5.5"),
                None,
            ),
            ("local/test-model", Some("openai-codex/gpt-5.5"), None),
            (
                "claude-sonnet-4-6",
                None,
                Some("groq/llama-3.3-70b-versatile"),
            ),
            ("claude-sonnet-4-6", None, Some("local/test-model")),
            ("claude-sonnet-4-6", None, Some("github-copilot/gpt-5-mini")),
        ];
        for (session, compaction, explicit) in cases {
            let mut rt = runtime(Some(SUMMARY));
            rt.set_model(session.to_string());
            rt.set_compaction_model(compaction.map(str::to_owned));
            rt.set_summary_model(explicit.map(str::to_owned));
            let rt = remote(rt, &endpoint);
            assert!(
                matches!(rt.rollover_summarizer(), Err(Some(RemoteProxy))),
                "{session} {compaction:?} {explicit:?}"
            );
        }
        // auto with a proxied compaction model: the session's Anthropic
        // model (credential vending, direct Messages) needs no fallback.
        let mut rt = runtime(Some(SUMMARY));
        rt.set_compaction_model(Some("groq/llama-3.3-70b-versatile".to_string()));
        let rt = remote(rt, &endpoint);
        assert_eq!(
            rt.summary_model_choice(),
            Ok(("claude-sonnet-4-6".to_string(), None))
        );
        // auto with a proxied session model: the Anthropic compaction model,
        // said why.
        let mut rt = runtime(Some(SUMMARY));
        rt.set_model("local/test-model".to_string());
        let rt = remote(rt, &endpoint);
        assert_eq!(
            rt.summary_model_choice(),
            Ok((
                "claude-sonnet-4-6".to_string(),
                Some(CompactionModelForRemoteProxy)
            ))
        );
    }

    // End to end: the conversation runs (Anthropic, vended credential), the
    // proxied summary is skipped with one notice, and nothing is proxied.
    let mut rt = runtime(Some(SUMMARY));
    rt.set_summary_model(Some("groq/llama-3.3-70b-versatile".to_string()));
    let rt = remote(rt, &endpoint);
    let first = turn(&rt, history(Some((PREVIOUS, 3))), Default::default()).await;
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    let notice = rollover_notice(&first);
    assert!(
        notice.contains("Rolling summary skipped: the remote credential broker can't yet guarantee a single attempt on the summary model's route.")
            && notice.contains("carried forward unchanged"),
        "{notice}"
    );
    assert!(
        first
            .usages
            .iter()
            .all(|(_, _, m)| m.as_deref() != Some("groq/llama-3.3-70b-versatile")),
        "nothing booked for the summary: {:?}",
        first.usages
    );
    let again = turn(&rt, history(Some((PREVIOUS, 3))), Default::default()).await;
    assert!(
        !rollover_notice(&again).contains("Rolling summary skipped"),
        "said once"
    );
    assert!(
        broker.requests.lock().unwrap().is_empty(),
        "no proxied request"
    );
    assert!(
        broker.vended.load(Ordering::SeqCst) > 0,
        "the conversation's credential was vended"
    );
    assert!(
        anthropic.summaries.lock().unwrap().is_empty(),
        "no summary request"
    );
    assert!(
        local_bodies.lock().unwrap().is_empty(),
        "no local endpoint contacted"
    );
}

/// Anthropic under a remote broker is credential vending plus one direct
/// Messages request from this process (on the redirect-free client): the
/// summary still goes out, once, and nothing is proxied.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn a_remote_anthropic_summary_uses_a_vended_credential_and_dispatches_once() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (anthropic, _g) = spawn(Reply::Text("## Window 2\nVENDED-SUMMARY".into())).await;
    let (broker, endpoint) = spawn_remote_broker(responses_text()).await;
    let rt = remote(runtime(Some(SUMMARY)), &endpoint);
    // The session's turn bound its seat; the summary uses that binding.
    rt.refresh_if_needed().await.expect("the turn's credential");
    let out = settle_events(&rt).await;
    assert!(
        out.summary
            .as_deref()
            .is_some_and(|s| s.contains("VENDED-SUMMARY")),
        "{:?}",
        out.summary
    );
    assert_eq!(anthropic.summaries.lock().unwrap().len(), 1, "one attempt");
    assert!(
        broker.vended.load(Ordering::SeqCst) > 0,
        "credential vended"
    );
    assert!(
        broker.requests.lock().unwrap().is_empty(),
        "nothing proxied"
    );
    assert_eq!(out.reserved, 0.0, "settled from the reported usage");
}

/// Under `auto`, a cold rollover summary keeps the session's bound seat: a
/// stale token is re-vended by name (no selection) and the one request goes
/// out on it; a 401 is not recovered; a seat that can no longer be vended
/// (or none bound) means no request and nothing reserved, the previous
/// summary carried. The binding never moves to another seat.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn a_cold_summary_under_auto_keeps_the_bound_seat() {
    use crate::runtime::background_seat_tests as seat;
    let _base = crate::test_env::BaseDirGuard::new();
    let _auto = crate::test_env::EnvVarGuard::set("SYNAPS_ACCOUNT_ANTHROPIC", "auto");
    let (daemon, endpoint) = seat::SeatDaemon::spawn().await;

    // Stale token: the bound seat re-vended by name, one request on it.
    {
        let (upstream, _url) = seat::spawn_upstream(200).await;
        let rt = seat::on_remote(runtime(Some(SUMMARY)), &endpoint);
        seat::bind(&rt, Some("work"), seat::STALE).await;
        let before = seat::shared_auth(&rt).await;
        let out = settle_events(&rt).await;
        assert!(
            out.summary
                .as_deref()
                .is_some_and(|s| s.contains("BOUND-SEAT")),
            "{:?}",
            out.summary
        );
        assert_eq!(upstream.auths(), vec!["Bearer tok-work"], "the re-vended token");
        assert_eq!(daemon.vends(), vec!["work"], "no selection");
        assert_eq!(
            seat::shared_auth(&rt).await,
            before,
            "the re-vended token is the request's own: shared auth untouched"
        );
    }

    // A 401: the summary's failure, no recovery vend, no retry.
    {
        let (upstream, _url) = seat::spawn_upstream(401).await;
        let rt = seat::on_remote(runtime(Some(SUMMARY)), &endpoint);
        seat::bind(&rt, Some("work"), seat::FAR).await;
        let before = seat::binding(&rt).await;
        let out = settle_events(&rt).await;
        assert!(out.summary.is_none(), "{:?}", out.summary);
        assert_eq!(upstream.auths(), vec!["Bearer tok-held-work"], "one request");
        assert_eq!(daemon.vends(), vec!["work"], "no recovery vend");
        assert_eq!(seat::binding(&rt).await, before);
    }

    // The seat can no longer be vended, or none is bound: not sent, and a
    // previous summary is not replaced (see the next test for the head).
    daemon.forget("work");
    let (upstream, _url) = seat::spawn_upstream(200).await;
    for label in [Some("work"), None] {
        let rt = seat::on_remote(runtime(Some(SUMMARY)), &endpoint);
        seat::bind(&rt, label, seat::STALE).await;
        let before = seat::binding(&rt).await;
        let summarizer = rt.rollover_summarizer().expect("summarizer");
        let out = settle_after(&summarizer, Some(PREVIOUS)).await;
        assert!(out.summary.is_none(), "{label:?}: {:?}", out.summary);
        assert_eq!(out.reserved, 0.0, "{label:?}: nothing reserved");
        assert_eq!(out.measured, 0.0, "{label:?}");
        assert!(out.usages.is_empty(), "{label:?}: {:?}", out.usages);
        assert_eq!(seat::binding(&rt).await, before, "{label:?}");
    }
    assert!(upstream.auths().is_empty(), "nothing sent");
    assert_eq!(
        daemon.vends(),
        vec!["work", "work"],
        "the bound seat only, never a selection"
    );
}

/// Under `auto`, a whole turn whose rollover's cold summary cannot be sent
/// (the bound seat's token is stale and the seat can no longer be vended)
/// carries the nonempty previous summary into the successor head
/// unchanged; no summary request goes out and the turn completes.
#[tokio::test]
// Whole turns go through the stream relay, whose counters the relay flood
// test reads globally.
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn an_unsent_cold_summary_carries_the_previous_summary() {
    use crate::runtime::background_seat_tests as seat;
    let _base = crate::test_env::BaseDirGuard::new();
    let _auto = crate::test_env::EnvVarGuard::set("SYNAPS_ACCOUNT_ANTHROPIC", "auto");
    let (daemon, endpoint) = seat::SeatDaemon::spawn().await;
    let (upstream, _url) = seat::spawn_upstream(200).await;
    // The turn binds the selected seat on a token that is already stale;
    // that seat then cannot be re-vended by name.
    daemon.expire_at(seat::STALE);
    daemon.forget(seat::SELECTED);
    let rt = seat::on_remote(runtime(Some(SUMMARY)), &endpoint);

    let out = turn(&rt, history(Some((PREVIOUS, 3))), Default::default()).await;

    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert!(!out.heads.is_empty(), "the turn rolled over");
    assert_eq!(
        summary_of(&out.heads[0]).as_deref(),
        Some(PREVIOUS),
        "carried unchanged"
    );
    assert!(
        daemon.vends().iter().any(|a| a == seat::SELECTED),
        "the bound seat was tried by name: {:?}",
        daemon.vends()
    );
    assert_eq!(upstream.summaries(), 0, "no summary request");
    assert!(!upstream.auths().is_empty(), "the conversation went out");
    assert_eq!(
        seat::binding(&rt).await.1,
        seat::bound_to(&rt, seat::SELECTED),
        "the turn's own binding"
    );
}

/// A cold summary whose bound seat is re-vended while a foreground turn
/// start rebinds the session to another seat never restores the old seat:
/// the rebind survives, and the summary went out on the old seat's
/// re-vended token (or was not sent and reserved nothing).
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn a_cold_summary_vend_never_undoes_a_concurrent_foreground_rebind() {
    use crate::runtime::background_seat_tests as seat;
    let _base = crate::test_env::BaseDirGuard::new();
    let _auto = crate::test_env::EnvVarGuard::set("SYNAPS_ACCOUNT_ANTHROPIC", "auto");
    let (daemon, endpoint) = seat::SeatDaemon::spawn().await;
    let (upstream, _url) = seat::spawn_upstream(200).await;
    let rt = seat::on_remote(runtime(Some(SUMMARY)), &endpoint);
    seat::bind(&rt, Some("work"), seat::STALE).await;

    let out = seat::during_a_foreground_rebind(&rt, &daemon, "work", settle_events(&rt)).await;

    assert_eq!(daemon.vends(), vec!["work", "auto"]);
    assert_eq!(
        seat::binding(&rt).await,
        (format!("tok-{}", seat::SELECTED), seat::bound_to(&rt, seat::SELECTED)),
        "the foreground rebind survives"
    );
    if upstream.auths().is_empty() {
        assert!(out.summary.is_none() && out.reserved == 0.0 && out.usages.is_empty());
    } else {
        assert_eq!(upstream.auths(), vec!["Bearer tok-work"], "the old seat's token");
    }
}

/// An explicit selector with no seat bound yet: the cold summary is not
/// sent, nothing is vended, bound, reserved or booked.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn a_cold_summary_with_an_unbound_explicit_selector_is_not_sent() {
    use crate::runtime::background_seat_tests as seat;
    let _base = crate::test_env::BaseDirGuard::new();
    let _work = crate::test_env::EnvVarGuard::set("SYNAPS_ACCOUNT_ANTHROPIC", "work");
    let (daemon, endpoint) = seat::SeatDaemon::spawn().await;
    let (upstream, _url) = seat::spawn_upstream(200).await;
    let rt = seat::on_remote(runtime(Some(SUMMARY)), &endpoint);
    seat::bind(&rt, None, seat::STALE).await;
    let before = seat::shared_auth(&rt).await;

    let out = settle_events(&rt).await;

    assert!(out.summary.is_none(), "{:?}", out.summary);
    assert_eq!(out.reserved, 0.0, "nothing reserved");
    assert_eq!(out.measured, 0.0);
    assert!(out.usages.is_empty(), "{:?}", out.usages);
    assert!(daemon.vends().is_empty(), "no vend");
    assert!(upstream.auths().is_empty(), "nothing sent");
    assert_eq!(seat::shared_auth(&rt).await, before, "nothing bound");
}

/// Sets the injected bounded-client failure for this thread (the test's
/// runtime thread) and resets it on drop.
struct BoundedClientFails;

impl BoundedClientFails {
    fn set() -> Self {
        crate::runtime::BOUNDED_CLIENT_FAILS_FOR_TEST.with(|fails| fails.set(true));
        Self
    }
}

impl Drop for BoundedClientFails {
    fn drop(&mut self) {
        crate::runtime::BOUNDED_CLIENT_FAILS_FOR_TEST.with(|fails| fails.set(false));
    }
}

/// When no redirect-free client can be built a bounded summary fails
/// closed: it is skipped with a one-time notice before anything is sent or
/// reserved, and even past that check the Anthropic leg sends nothing (not
/// dispatched, reservation released) instead of using another client.
#[tokio::test]
// Whole turns go through the stream relay, whose counters the relay flood
// test reads globally.
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn without_a_redirect_free_client_no_summary_is_sent() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (anthropic, _g) = spawn(Reply::Text("## Window 2\nMUST-NOT-BE-SENT".into())).await;
    let _fails = BoundedClientFails::set();
    let rt = runtime(Some(SUMMARY));

    let first = turn(&rt, history(Some((PREVIOUS, 3))), Default::default()).await;
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    let notice = rollover_notice(&first);
    assert!(
        notice.contains(
            "Rolling summary skipped: no HTTP client that refuses redirects could be set up"
        ) && notice.contains("carried forward unchanged"),
        "{notice}"
    );
    let again = turn(&rt, history(Some((PREVIOUS, 3))), Default::default()).await;
    assert!(
        !rollover_notice(&again).contains("Rolling summary skipped"),
        "said once"
    );

    // Past the readiness check: the transport itself refuses to send.
    let summarizer = rt
        .rollover_summarizer()
        .expect("summarizer")
        .with_bounded_probe(|| true);
    let usage = crate::runtime::budget::UsageCounters::default();
    let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
    let dropped: Vec<SharedMessage> = (0..20).map(event).collect();
    let archive = "a".repeat(32);
    let out = super::super::update(
        &summarizer,
        super::super::SummaryRequest {
            logical_id: "session",
            previous: None,
            dropped: &dropped,
            window: 2,
            archive_id: &archive,
            cap_tokens: SUMMARY,
            timeout: Duration::from_secs(30),
            cancel: &Default::default(),
            accounting: Some(super::super::SummaryAccounting {
                usage: &usage,
                max_cost_usd: None,
                events: &events,
                tap: None,
                ledger: None,
            }),
            snapshot: None,
            run_deadline: None,
        },
    )
    .await;
    assert!(matches!(out, Err(None)), "{out:?}");
    assert_eq!(usage.reserved_usd(), 0.0, "not dispatched: released");
    assert_eq!(usage.cost_usd(), 0.0);
    assert!(
        anthropic.summaries.lock().unwrap().is_empty(),
        "no summary request on any client"
    );
}

/// A Responses reply that fails (incomplete at the cap, failed, or completed
/// without text) but reports its final usage is charged that usage: the
/// usage is booked, the reservation released, and the previous summary
/// carried forward. Without a usable report the reservation is kept.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn a_failed_responses_summary_books_the_usage_it_reported() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (_anthropic, _g) = spawn(Reply::Status(500)).await;
    let usage = json!({"input_tokens":900,"output_tokens":120,"input_tokens_details":{"cached_tokens":100}});
    let partial = json!({"input_tokens":900});
    let cases = [
        (
            "incomplete at the cap",
            vec![
                json!({"type":"response.output_text.delta","delta":"## Window 2\nCUT"}),
                json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":usage}}),
            ],
            true,
        ),
        (
            "failed",
            vec![
                json!({"type":"response.failed","response":{"error":{"code":"invalid_prompt"},"usage":usage}}),
            ],
            true,
        ),
        (
            "completed without text",
            vec![json!({"type":"response.completed","response":{"usage":usage}})],
            true,
        ),
        (
            "failed without usage",
            vec![json!({"type":"response.failed","response":{"error":{"code":"invalid_prompt"}}})],
            false,
        ),
        (
            "incomplete with a partial report",
            vec![json!({"type":"response.incomplete","response":{"usage":partial}})],
            false,
        ),
    ];
    for (case, events, reported) in cases {
        let (broker, endpoint) = spawn_remote_broker(responses_reply(&events)).await;
        let rt = remote(runtime(Some(SUMMARY)), &endpoint);
        let out = settle_with(&direct_summarizer(&rt, "github-copilot/gpt-5-mini")).await;
        assert_eq!(broker.requests.lock().unwrap().len(), 1, "{case}");
        assert!(out.summary.is_none(), "{case}: carried forward");
        if reported {
            assert_eq!(out.reserved, 0.0, "{case}: reservation released");
            assert!(out.measured > 0.0, "{case}: usage booked");
            assert_eq!(
                out.usages,
                vec![(800, 120, Some("github-copilot/gpt-5-mini".to_string()))],
                "{case}"
            );
        } else {
            assert!(
                out.reserved > 0.0,
                "{case}: spend unknown, reservation kept"
            );
            assert_eq!(out.measured, 0.0, "{case}");
            assert!(out.usages.is_empty(), "{case}");
        }
    }
}

/// A valid explicit summary model is used even when the compaction model is
/// configured empty.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn an_explicit_summary_model_works_without_a_compaction_model() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (_anthropic, _g) = spawn(Reply::Status(500)).await;
    let (bodies, _l) = spawn_local(Local::Reply("## Window 2\nEXPLICIT-ONLY")).await;
    let mut rt = runtime(Some(SUMMARY));
    rt.set_compaction_model(Some(String::new()));
    rt.set_summary_model(Some("local/test-model".to_string()));
    let out = settle_events(&rt).await;
    assert!(out
        .summary
        .as_deref()
        .is_some_and(|s| s.contains("EXPLICIT-ONLY")));
    assert_eq!(bodies.lock().unwrap().len(), 1);

    // auto with an empty compaction model still uses the session's model;
    // only the fallback is gone.
    let mut rt = runtime(Some(SUMMARY));
    rt.set_compaction_model(Some(String::new()));
    let summarizer = rt.rollover_summarizer().expect("the session's model");
    assert_eq!(summarizer.model(), "claude-sonnet-4-6");
    assert_eq!(summarizer.notice(), None);
}

/// auto whose session model is proxied by a remote broker falls back to
/// the Anthropic compaction model and says why. That summary is a
/// background request on native Anthropic: it needs a seat the session
/// already bound on this source. With none (a session on a proxied model
/// never binds one), it is skipped before anything is reserved, with a
/// typed reason, and no credential is vended; with one, it is sent on that
/// seat (a named vend, never a selection).
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn auto_fallback_to_the_compaction_model_needs_a_bound_seat() {
    use crate::runtime::background_seat_tests as seat;
    let _base = crate::test_env::BaseDirGuard::new();
    let _auto = crate::test_env::EnvVarGuard::set("SYNAPS_ACCOUNT_ANTHROPIC", "auto");
    let (daemon, endpoint) = seat::SeatDaemon::spawn().await;
    let (upstream, _url) = seat::spawn_upstream(200).await;
    let fallback = || {
        let mut rt = runtime(Some(SUMMARY));
        rt.set_model("local/test-model".to_string());
        let rt = seat::on_remote(rt, &endpoint);
        let summarizer = rt.rollover_summarizer().expect("the compaction model");
        assert_eq!(summarizer.model(), "claude-sonnet-4-6");
        assert_eq!(
            summarizer.notice(),
            Some(super::super::SummarySkip::CompactionModelForRemoteProxy)
        );
        assert!(
            summarizer.warm().is_none(),
            "warm only for the session's model"
        );
        (rt, summarizer)
    };

    // No seat bound: skipped before reservation, nothing vended or sent.
    let (rt, summarizer) = fallback();
    seat::bind(&rt, None, seat::FAR).await;
    let before = seat::shared_auth(&rt).await;
    let out = settle_after(&summarizer, Some(PREVIOUS)).await;
    assert_eq!(out.skip, Some(super::super::SummarySkip::NoBoundSeat));
    assert!(out.summary.is_none(), "{:?}", out.summary);
    assert_eq!(out.reserved, 0.0, "nothing reserved");
    assert_eq!(out.measured, 0.0);
    assert!(out.usages.is_empty(), "{:?}", out.usages);
    assert!(daemon.vends().is_empty(), "no vend");
    assert!(upstream.auths().is_empty(), "nothing sent");
    assert_eq!(seat::shared_auth(&rt).await, before);
    let notice = super::super::SummarySkip::NoBoundSeat.notice("claude-sonnet-4-6", false);
    assert!(
        notice.starts_with("Rolling summary skipped: ")
            && notice.contains("no Anthropic account bound"),
        "{notice}"
    );

    // A seat bound on this source: sent on it, re-vended by name.
    let (rt, summarizer) = fallback();
    seat::bind(&rt, Some("work"), seat::STALE).await;
    let before = seat::shared_auth(&rt).await;
    let out = settle_with(&summarizer).await;
    assert!(
        out.summary
            .as_deref()
            .is_some_and(|s| s.contains("BOUND-SEAT")),
        "{:?}",
        out.summary
    );
    assert_eq!(out.skip, None);
    assert_eq!(daemon.vends(), vec!["work"], "a named vend, no selection");
    assert_eq!(upstream.auths(), vec!["Bearer tok-work"]);
    assert_eq!(upstream.summaries(), 1, "one summary request");
    assert_eq!(seat::shared_auth(&rt).await, before, "shared auth untouched");
}

/// A malformed explicit `summary_model` turns the summary off with a
/// one-time notice; it never falls back to `auto` (here a usable compaction
/// model).
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn an_invalid_explicit_summary_model_turns_the_summary_off_with_a_notice() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn(Reply::Text("## Window 1\nAUTO-WOULD-WRITE-THIS".into())).await;
    let mut rt = runtime(Some(SUMMARY));
    rt.set_summary_model_invalid();
    assert_eq!(
        rt.rollover_summarizer().err(),
        Some(Some(super::super::SummarySkip::InvalidSummaryModel))
    );
    let first = turn(&rt, history(Some((PREVIOUS, 3))), Default::default()).await;
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    assert_eq!(summary_of(&first.heads[0]).as_deref(), Some(PREVIOUS));
    let notice = rollover_notice(&first);
    assert!(
        notice.contains(
            "Rolling summary skipped: context_management.summary_model is not auto or a single model id."
        ),
        "{notice}"
    );
    let second = turn(&rt, history(Some((PREVIOUS, 3))), Default::default()).await;
    assert!(
        !rollover_notice(&second).contains("summary_model"),
        "said once"
    );
    assert!(
        mock.summaries.lock().unwrap().is_empty(),
        "no summary request to the auto candidate"
    );
}

/// The fallback notice names the model and never claims a summary that was
/// not produced; it is said once per reason and model, so a later model
/// change is told again.
#[test]
fn fallback_notices_follow_the_model_and_the_outcome() {
    use super::super::{SummaryNotices, SummarySkip::CompactionModelForCeiling};
    assert_eq!(
        CompactionModelForCeiling.notice("claude-sonnet-4-6", false),
        "Attempted rolling summary with claude-sonnet-4-6 (the session model has no enforceable output ceiling)."
    );
    assert_eq!(
        CompactionModelForCeiling.notice("claude-sonnet-4-6", true),
        "Rolling summary model: claude-sonnet-4-6 (the session model has no enforceable output ceiling)."
    );
    let mut noticed = SummaryNotices::default();
    assert!(noticed.first(CompactionModelForCeiling, "claude-sonnet-4-6"));
    assert!(
        !noticed.first(CompactionModelForCeiling, "claude-sonnet-4-6"),
        "said once"
    );
    assert!(
        noticed.first(CompactionModelForCeiling, "claude-opus-4-7"),
        "a model change is told again"
    );
}

#[test]
fn model_ids_are_sanitized_for_display() {
    use super::super::display_model;
    assert_eq!(display_model("local/qwen3"), "local/qwen3");
    assert_eq!(
        display_model("local/evil\u{1b}[31m\nname\t"),
        "local/evil[31mname"
    );
    assert_eq!(display_model(" \u{7}\n"), "(unnamed model)");
    let long = display_model(&format!("local/{}", "m".repeat(200)));
    assert_eq!(long.chars().count(), 80);
    assert!(long.ends_with('…'));
    // Exactly at the limit: no cut; one scalar over: cut.
    assert_eq!(display_model(&"m".repeat(80)), "m".repeat(80));
    assert!(display_model(&"m".repeat(81)).ends_with('…'));
    let notice =
        super::super::SummarySkip::CompactionModelForCeiling.notice("local/a\u{1b}b c", true);
    assert_eq!(
        notice,
        "Rolling summary model: local/abc (the session model has no enforceable output ceiling)."
    );
}

/// Notice keys hold the displayed (sanitized, bounded) model id: ids that
/// display alike are told once, and a huge id stores at most the display
/// length.
#[test]
fn notice_keys_store_the_bounded_display_id() {
    use super::super::{SummaryNotices, SummarySkip};
    let mut noticed = SummaryNotices::default();
    assert!(noticed.first(SummarySkip::RemoteProxy, "local/a\u{1b}b"));
    assert!(
        !noticed.first(SummarySkip::RemoteProxy, "local/ab"),
        "same display"
    );
    assert!(
        noticed.first(SummarySkip::NoBoundedTransport, "local/ab"),
        "other reason"
    );
    let huge = format!("local/{}", "m".repeat(1 << 20));
    assert!(noticed.first(SummarySkip::RemoteProxy, &huge));
    assert!(!noticed.first(SummarySkip::RemoteProxy, &huge));
    let longest = noticed.0.iter().map(|(_, m)| m.chars().count()).max();
    assert_eq!(longest, Some(80), "{:?}", noticed.0.len());
}
