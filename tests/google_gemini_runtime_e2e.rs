//! Zero-network Google Gemini Code Assist runtime harness.
//!
//! Wires a real `LocalBroker` to a fake token endpoint + fake
//! `cloudcode-pa.googleapis.com` host, primes `auth.json` with a stored
//! Google refresh token, and asserts:
//!
//! - The broker sends the correct bearer, path, and body.
//! - The runtime decodes SSE text deltas + tool calls in order.
//! - Redirects returned by the fake Code Assist host are NOT followed.
//! - Path-allowlist enforcement blocks non-reviewed same-host methods.

use agent_core::auth::{
    google_gemini::PROVIDER, save_provider_auth, LocalBroker, OAuthCredentials, ProxyMethod,
    ProxyRequest,
};
use agent_engine::runtime::google_gemini::{stream_gemini, ChatTurn, GeminiStreamEvent};
use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::post,
    Router,
};
use futures::StreamExt;
use serial_test::serial;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

// ── Fixtures ─────────────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct SeenReq(Arc<Mutex<Vec<(String, String, String)>>>); // (path, auth, body)

async fn capture_streaming(
    State(seen): State<SeenReq>,
    headers: HeaderMap,
    req: axum::extract::Request,
) -> Response {
    let path = req.uri().path().to_string();
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body_bytes = axum::body::to_bytes(req.into_body(), 1 << 20)
        .await
        .unwrap();
    let body = String::from_utf8(body_bytes.to_vec()).unwrap();
    seen.0.lock().unwrap().push((path, auth, body));
    // SSE frames — three text deltas, one tool call, and a finish reason.
    let sse = concat!(
        "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello \"}]}}]}}\n\n",
        "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"world\"}]}}]}}\n\n",
        "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"count\",\"args\":{\"n\":3}},\"thoughtSignature\":\"fixture-signature\"}]},\"finishReason\":\"STOP\"}]}}\n\n",
        "data: [DONE]\n\n",
    );
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from(sse))
        .unwrap()
}

async fn redirect_leaker(
    State(_): State<SeenReq>,
    _headers: HeaderMap,
    _req: axum::extract::Request,
) -> Response {
    // If the broker followed this, the bearer token would leak to a foreign
    // host. Test asserts the request fails instead.
    Response::builder()
        .status(StatusCode::TEMPORARY_REDIRECT)
        .header("location", "https://evil.example/steal")
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn spawn_upstream(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn fresh_credentials() -> OAuthCredentials {
    // Not expired: expires ~1h in the future.
    let expires_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 60 * 60 * 1000;
    OAuthCredentials {
        auth_type: "oauth".into(),
        refresh: "test-refresh".into(),
        access: "ya29.fake-access".into(),
        expires: expires_ms,
        account_id: Some("user@example.com".into()),
    }
}

fn seeded_broker_with_base(base_url: &str) -> LocalBroker {
    // Override the pinned cloudcode-pa base URL via the broker constructor —
    // added in this slice below.
    LocalBroker::with_google_gemini_base_url_for_tests(reqwest::Client::new(), base_url)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn broker_streams_gemini_generate_content_end_to_end() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());
    save_provider_auth(PROVIDER, &fresh_credentials()).unwrap();

    let seen = SeenReq::default();
    let upstream = spawn_upstream(
        Router::new()
            .route("/v1internal:streamGenerateContent", post(capture_streaming))
            .with_state(seen.clone()),
    )
    .await;

    let broker = seeded_broker_with_base(&upstream);
    let cancel = CancellationToken::new();
    let mut stream = stream_gemini(
        &broker,
        "gemini-2.5-pro",
        Some("proj-abc".into()),
        Some("be brief".into()),
        &[
            ChatTurn::User { text: "hi".into() },
            ChatTurn::Assistant {
                text: "hello".into(),
            },
            ChatTurn::User {
                text: "count to 3".into(),
            },
        ],
        &[],
        cancel,
    )
    .await
    .expect("stream must open");

    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.expect("no stream errors"));
    }

    // Text deltas, tool call, then two Finish events (STOP + [DONE] sentinel).
    let mut text = String::new();
    let mut tool_names = Vec::new();
    let mut tool_signatures = Vec::new();
    let mut finishes = 0;
    for ev in &events {
        match ev {
            GeminiStreamEvent::TextDelta(t) => text.push_str(t),
            GeminiStreamEvent::ToolCall(c) => {
                tool_names.push(c.name.clone());
                tool_signatures.push(c.thought_signature.clone());
            }
            GeminiStreamEvent::Finish { .. } => finishes += 1,
            GeminiStreamEvent::Usage(_) | GeminiStreamEvent::Ignored => {}
        }
    }
    assert_eq!(text, "Hello world");
    assert_eq!(tool_names, vec!["count".to_string()]);
    assert_eq!(tool_signatures, vec![Some("fixture-signature".into())]);
    assert!(finishes >= 1);

    // Verify wire request: bearer used the broker-vended access token; path
    // was the exact reviewed method; body carried the CA envelope with model
    // + project + Vertex-style contents.
    let seen = seen.0.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let (path, auth, body) = &seen[0];
    assert!(path.starts_with("/v1internal:streamGenerateContent"));
    assert_eq!(auth, "Bearer ya29.fake-access");
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["model"], "gemini-2.5-pro");
    assert_eq!(body["project"], "proj-abc");
    assert_eq!(body["request"]["contents"][0]["role"], "user");
    assert_eq!(
        body["request"]["contents"][2]["parts"][0]["text"],
        "count to 3"
    );
    assert_eq!(
        body["request"]["systemInstruction"]["parts"][0]["text"],
        "be brief"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn broker_refuses_redirect_from_pinned_cloudcode_pa_host() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());
    save_provider_auth(PROVIDER, &fresh_credentials()).unwrap();

    let seen = SeenReq::default();
    let upstream = spawn_upstream(
        Router::new()
            .route("/v1internal:streamGenerateContent", post(redirect_leaker))
            .with_state(seen.clone()),
    )
    .await;
    let broker = seeded_broker_with_base(&upstream);
    let res = stream_gemini(
        &broker,
        "gemini-2.5-pro",
        None,
        None,
        &[ChatTurn::User {
            text: "leak?".into(),
        }],
        &[],
        CancellationToken::new(),
    )
    .await;
    // Either the stream opens and the 307 body ends immediately (no follow),
    // or the broker surfaces a transport error. In both cases the fake
    // 'evil.example' target is NEVER contacted (we don't run one) and no
    // subsequent bytes carrying the bearer arrive elsewhere.
    match res {
        Ok(mut stream) => {
            // Empty body means the decoder should just see EOF with no events.
            let mut events = Vec::new();
            let deadline = Duration::from_secs(2);
            let _ = tokio::time::timeout(deadline, async {
                while let Some(ev) = stream.next().await {
                    events.push(ev);
                }
            })
            .await;
            for ev in &events {
                assert!(
                    !matches!(ev, Ok(GeminiStreamEvent::TextDelta(_)))
                        && !matches!(ev, Ok(GeminiStreamEvent::ToolCall(_))),
                    "no content must arrive from a redirect-only response"
                );
            }
        }
        Err(err) => {
            let msg = format!("{err}");
            assert!(!msg.contains("evil.example"));
            assert!(!msg.contains("ya29.fake-access"));
        }
    }
}

#[tokio::test]
async fn proxy_denies_non_allowlisted_cloudcode_pa_paths() {
    for bad in [
        "/v1internal:listExperiments",
        "/v1internal:generateContent",
        "/v1internal:setCodeAssistGlobalUserSetting",
        "/v2/models",
    ] {
        let req = ProxyRequest {
            provider: "google-gemini".into(),
            method: ProxyMethod::Post,
            path: bad.into(),
            body: Some(serde_json::json!({})),
            stream: false,
            body_bytes: None,
        };
        assert!(
            req.validate().is_err(),
            "{bad} must be denied by validate()"
        );
    }
}

#[tokio::test]
async fn oversized_request_body_is_denied_before_egress() {
    let big =
        serde_json::json!({ "junk": "x".repeat(agent_core::auth::MAX_PROXY_REQUEST_BYTES + 1) });
    let req = ProxyRequest {
        provider: "google-gemini".into(),
        method: ProxyMethod::Post,
        path: "/v1internal:streamGenerateContent".into(),
        body: Some(big),
        stream: true,
        body_bytes: None,
    };
    assert!(req.validate().is_err());
}
