//! Checkpoint-1 boundary proof: the Anthropic usage operation is brokered.
//!
//! * LocalBroker resolves the OAuth access token from broker-owned storage
//!   and attaches it behind the boundary; the caller receives usage JSON only.
//! * RemoteBroker presents machine auth to `GET /usage` and never sends or
//!   receives provider token material.
//!
//! Runs as an integration binary so the `SYNAPS_BASE_DIR` override cannot
//! interfere with unrelated unit tests.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use agent_core::auth::{CredentialBroker, LocalBroker, RemoteBroker, TokenCache};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn usage_payload() -> serde_json::Value {
    json!({
        "five_hour": { "utilization": 42.0, "resets_at": "2099-01-01T00:00:00Z" },
        "seven_day": { "utilization": 10.0, "resets_at": "2099-01-01T00:00:00Z" }
    })
}

/// Local path: the broker reads auth.json (inside the boundary), attaches the
/// bearer token + oauth beta header itself, and vends usage JSON with no
/// token fields. The caller never touched auth.json.
#[tokio::test]
#[serial_test::serial]
async fn local_broker_executes_usage_with_broker_resolved_token() {
    // Broker-owned credential store in a throwaway home.
    let home = tempfile::tempdir().unwrap();
    agent_core::config::set_base_dir_for_tests(home.path().to_path_buf());
    let far_future = 4_102_444_800_000u64; // year 2100, never refreshes
    std::fs::write(
        home.path().join("auth.json"),
        json!({
            "anthropic": {
                "type": "oauth",
                "refresh": "refresh-secret-MUST-NOT-EGRESS",
                "access": "access-token-abc",
                "expires": far_future
            }
        })
        .to_string(),
    )
    .unwrap();

    // Fake Anthropic usage endpoint that records the auth headers it saw.
    let seen: Arc<Mutex<(String, String)>> = Arc::new(Mutex::new((String::new(), String::new())));
    let seen2 = seen.clone();
    let app = Router::new().route(
        "/api/oauth/usage",
        get(move |headers: HeaderMap| {
            let seen = seen2.clone();
            async move {
                let h = |k: &str| {
                    headers
                        .get(k)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string()
                };
                *seen.lock().unwrap() = (h("authorization"), h("anthropic-beta"));
                Json(usage_payload())
            }
        }),
    );
    let upstream = spawn(app).await;

    let broker = LocalBroker::new(reqwest::Client::new())
        .with_anthropic_usage_url(format!("{upstream}/api/oauth/usage"));
    let usage = broker.anthropic_usage().await.expect("usage must succeed");

    // The broker resolved and attached the token behind the boundary.
    let (auth_header, beta) = seen.lock().unwrap().clone();
    assert_eq!(auth_header, "Bearer access-token-abc");
    assert_eq!(beta, "oauth-2025-04-20");

    // The vended payload is usage data only — no token material egresses.
    assert_eq!(usage["five_hour"]["utilization"], 42.0);
    let raw = usage.to_string();
    assert!(!raw.contains("access-token-abc"));
    assert!(!raw.contains("refresh-secret"));
}

/// Remote path: the client presents its machine token to the broker's typed
/// /usage endpoint and gets usage JSON back. No provider token appears in the
/// request or the response.
#[tokio::test]
async fn remote_broker_fetches_usage_with_machine_auth_only() {
    let seen_headers: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen_headers.clone();
    let app = Router::new().route(
        "/usage",
        get(move |headers: HeaderMap| {
            let seen = seen2.clone();
            async move {
                *seen.lock().unwrap() = headers
                    .iter()
                    .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect();
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if auth != "Bearer machine-token-1" {
                    return (
                        axum::http::StatusCode::UNAUTHORIZED,
                        Json(json!({ "error": "bad machine auth" })),
                    );
                }
                (axum::http::StatusCode::OK, Json(usage_payload()))
            }
        }),
    );
    let endpoint = spawn(app).await;

    let broker = RemoteBroker::new(
        endpoint.clone(),
        "machine-token-1",
        reqwest::Client::new(),
        TokenCache::new(),
    );
    let usage = broker.anthropic_usage().await.expect("usage must succeed");
    assert_eq!(usage["seven_day"]["utilization"], 10.0);

    // Egress audit: the only credential sent is the machine token; there is
    // no provider access/refresh token anywhere in the request.
    let headers = seen_headers.lock().unwrap().clone();
    let auth_values: Vec<&str> = headers
        .iter()
        .filter(|(k, _)| k.contains("auth") || k.contains("token") || k.contains("key"))
        .map(|(_, v)| v.as_str())
        .collect();
    assert_eq!(auth_values, vec!["Bearer machine-token-1"]);

    // Wrong machine token → typed Unauthorized, no usage data.
    let bad = RemoteBroker::new(
        endpoint,
        "wrong-token",
        reqwest::Client::new(),
        TokenCache::new(),
    );
    let err = bad.anthropic_usage().await.expect_err("must be denied");
    assert!(matches!(err, agent_core::auth::BrokerError::Unauthorized));
}
