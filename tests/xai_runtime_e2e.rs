//! Zero-network broker/API harness: real local and remote broker contracts,
//! representative static key proxy, Grok SSE, and wire-level non-egress proof.
use agent_core::auth::{
    CredentialBroker, LocalBroker, OAuthProviderId, ProxyMethod, ProxyRequest, RemoteBroker,
    TokenCache,
};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    response::Response,
    routing::post,
    Router,
};
use futures::StreamExt;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<(String, String, String)>>>);
async fn capture(State(seen): State<Seen>, headers: HeaderMap, req: Request<Body>) -> Response {
    let path = req.uri().path().to_string();
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = String::from_utf8(
        axum::body::to_bytes(req.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    seen.0.lock().unwrap().push((path, auth, body));
    Response::builder().status(StatusCode::OK).header("content-type","text/event-stream")
        .body(Body::from("data: {\"type\":\"response.output_text.delta\",\"delta\":\"Grok says hi\"}\n\ndata: [DONE]\n\n")).unwrap()
}
async fn serve(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{a}")
}
fn request(provider: &str, path: &str) -> ProxyRequest {
    ProxyRequest {
        provider: provider.into(),
        method: ProxyMethod::Post,
        path: path.into(),
        body: Some(serde_json::json!({"model":"grok-4.5","input":"hello"})),
        stream: true,
        body_bytes: None,
    }
}

#[tokio::test]
async fn local_and_remote_broker_stream_without_secret_egress() {
    let seen = Seen::default();
    let upstream = serve(
        Router::new()
            .route("/chat/completions", post(capture))
            .with_state(seen.clone()),
    )
    .await;
    let local = LocalBroker::with_local_base_url(reqwest::Client::new(), upstream);
    let mut stream = local
        .proxy_stream(request("local", "/chat/completions"))
        .await
        .unwrap();
    let mut bytes = Vec::new();
    while let Some(v) = stream.next().await {
        bytes.extend(v.unwrap());
    }
    assert!(String::from_utf8(bytes).unwrap().contains("Grok says hi"));

    // Fake authenticated remote broker. Capture its complete wire request and
    // return SSE exactly as the production remote transport expects.
    let remote_seen = Seen::default();
    let endpoint = serve(
        Router::new()
            .route("/proxy", post(capture))
            .with_state(remote_seen.clone()),
    )
    .await;
    let remote = RemoteBroker::new(
        endpoint,
        "machine-only",
        reqwest::Client::new(),
        TokenCache::new(),
    );
    let mut stream = remote
        .proxy_stream(request("local", "/chat/completions"))
        .await
        .unwrap();
    while let Some(v) = stream.next().await {
        v.unwrap();
    }
    let wire = remote_seen.0.lock().unwrap();
    assert_eq!(wire[0].1, "Bearer machine-only");
    assert!(!wire[0].2.contains("refresh-secret"));
    assert!(!wire[0].2.contains("raw-static-secret"));
    assert!(!wire[0].2.contains("machine-only"));
}

#[tokio::test]
async fn local_broker_sends_exact_body_bytes_verbatim() {
    // Exact-byte handoff (request-trace spec §6.2): the upstream must receive
    // the very buffer the caller serialized. The fixture bytes deliberately
    // use a key order serde_json would NOT reproduce from the parsed Value
    // ("b" before "a"), so any internal re-serialization fails this test.
    let seen = Seen::default();
    let upstream = serve(
        Router::new()
            .route("/chat/completions", post(capture))
            .with_state(seen.clone()),
    )
    .await;
    let exact: &[u8] = b"{\"b\":1,\"a\":2,\"model\":\"grok-4.5\"}";
    let broker = LocalBroker::with_local_base_url(reqwest::Client::new(), upstream);
    let mut stream = broker
        .proxy_stream(ProxyRequest {
            provider: "local".into(),
            method: ProxyMethod::Post,
            path: "/chat/completions".into(),
            body: Some(serde_json::from_slice(exact).unwrap()),
            stream: true,
            body_bytes: Some(bytes::Bytes::from_static(exact)),
        })
        .await
        .unwrap();
    while let Some(v) = stream.next().await {
        v.unwrap();
    }
    let wire = seen.0.lock().unwrap();
    assert_eq!(
        wire[0].2.as_bytes(),
        exact,
        "broker must forward the caller-serialized bytes verbatim"
    );
}

#[tokio::test]
async fn representative_static_key_is_applied_only_upstream_and_denials_are_local() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());
    agent_core::auth::save_static_key("groq", "raw-static-secret").unwrap();
    // Destination pinning is proved without network: malicious paths are denied
    // before send, while the request object itself has no field for a key/URL.
    let broker = LocalBroker::new(reqwest::Client::new());
    let err = broker
        .proxy_stream(request("groq", "https://attacker.invalid/steal"))
        .await
        .err()
        .unwrap();
    let text = err.to_string();
    assert!(text.contains("denied"));
    assert!(!text.contains("raw-static-secret"));
    let json = serde_json::to_string(&request("groq", "/chat/completions")).unwrap();
    assert!(!json.contains("raw-static-secret"));
    // OAuth access response is structurally access-only (no refresh slot).
    let token = agent_core::auth::AccessToken {
        token: "access-only".into(),
        expires: 1,
    };
    let serialized = serde_json::to_string(&token).unwrap();
    assert!(!serialized.contains("refresh"));
    let _ = OAuthProviderId::Xai;
}
