//! Meta Muse broker boundary.
//!
//! * The Muse proxy pins the origin, allowlists only `POST /v1/responses` and
//!   `GET /muse-code/models`, and attaches the minted Model API key — never
//!   the Meta OAuth token stored as `refresh`.
//! * A 401 on the minted key runs the broker's rejected-token recovery once
//!   (here: a peer already re-minted, so the fresh key is adopted with no
//!   network mint) and retries exactly once — never a loop.
//! * `META_API_KEY` overrides the account login for policy-selected requests
//!   (official CLI semantics); an explicit account stays explicit, and an env
//!   key is never "recovered".
//!
//! Runs as an integration binary so the `SYNAPS_BASE_DIR` override cannot
//! interfere with unrelated unit tests. No real credential is ever touched.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_core::auth::{AccountPolicy, BrokerError, CredentialBroker, LocalBroker, ProxyMethod, ProxyRequest};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use serde_json::json;

const FAR_FUTURE: u64 = 4_102_444_800_000; // year 2100: never re-mints proactively

fn muse_slot(key: &str) -> serde_json::Value {
    json!({ "type": "oauth", "access": key, "refresh": "META-OAUTH-SECRET", "expires": FAR_FUTURE })
}

fn write_store(home: &std::path::Path, slots: serde_json::Value) {
    std::fs::write(home.join("auth.json"), slots.to_string()).unwrap();
}

fn install_store(slots: serde_json::Value) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    agent_core::config::set_base_dir_for_tests(home.path().to_path_buf());
    write_store(home.path(), slots);
    std::fs::write(home.path().join("config"), "").unwrap();
    home
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn responses_request(provider: &str) -> ProxyRequest {
    ProxyRequest {
        provider: provider.into(),
        method: ProxyMethod::Post,
        path: "/v1/responses".into(),
        body: Some(json!({ "model": "muse-wonder-1.0", "input": "hi", "stream": false })),
        stream: false,
        body_bytes: None,
    }
}

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<(String, String)>>>);

impl Seen {
    fn push(&self, headers: &HeaderMap) {
        let h = |k: &str| {
            headers
                .get(k)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string()
        };
        self.0
            .lock()
            .unwrap()
            .push((h("authorization"), h("user-agent")));
    }
    fn auths(&self) -> Vec<String> {
        self.0.lock().unwrap().iter().map(|(a, _)| a.clone()).collect()
    }
}

/// Upstream that rejects `KEY-1`. On that rejection it simulates a peer
/// process re-minting (storage now holds `KEY-2`) — so recovery adopts the
/// peer's key under the refresh locks instead of minting over the network.
fn rotating_upstream(seen: Seen, home: PathBuf, always_401: bool) -> Router {
    Router::new().route(
        "/v1/responses",
        post(move |headers: HeaderMap| {
            let seen = seen.clone();
            let home = home.clone();
            async move {
                seen.push(&headers);
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                if always_401 || auth == "Bearer KEY-1" {
                    let n = seen.0.lock().unwrap().len();
                    write_store(&home, json!({ "muse": muse_slot(&format!("KEY-{}", n + 1)) }));
                    return (
                        StatusCode::UNAUTHORIZED,
                        r#"{"error":{"code":"invalid_api_key","type":"authentication_error"}}"#,
                    )
                        .into_response();
                }
                (StatusCode::OK, r#"{"id":"resp_1","status":"completed"}"#).into_response()
            }
        }),
    )
}

#[tokio::test]
#[serial_test::serial]
async fn muse_401_adopts_fresh_key_and_retries_exactly_once() {
    std::env::remove_var("META_API_KEY");
    let home = install_store(json!({ "muse": muse_slot("KEY-1") }));
    let seen = Seen::default();
    let upstream = spawn(rotating_upstream(seen.clone(), home.path().to_path_buf(), false)).await;
    let broker = LocalBroker::new(reqwest::Client::new())
        .with_account_policy(AccountPolicy::new())
        .with_muse_base_url_for_tests(upstream);

    let resp = broker.proxy(responses_request("muse")).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(seen.auths(), vec!["Bearer KEY-1", "Bearer KEY-2"]);
    // The Meta OAuth token (stored as `refresh`) never reaches the API host.
    assert!(seen.auths().iter().all(|a| !a.contains("META-OAUTH")));
    let ua = seen.0.lock().unwrap()[0].1.clone();
    assert!(ua.starts_with("SynapsCLI/"), "{ua}");
}

#[tokio::test]
#[serial_test::serial]
async fn muse_persistent_401_is_returned_after_one_retry() {
    std::env::remove_var("META_API_KEY");
    let home = install_store(json!({ "muse": muse_slot("KEY-1") }));
    let seen = Seen::default();
    let upstream = spawn(rotating_upstream(seen.clone(), home.path().to_path_buf(), true)).await;
    let broker = LocalBroker::new(reqwest::Client::new())
        .with_account_policy(AccountPolicy::new())
        .with_muse_base_url_for_tests(upstream);

    let resp = broker.proxy(responses_request("muse")).await.unwrap();
    assert_eq!(resp.status, 401);
    assert_eq!(seen.auths().len(), 2, "one recovery retry, never a loop");
}

#[tokio::test]
#[serial_test::serial]
async fn muse_env_key_overrides_login_and_is_never_recovered() {
    // No muse login at all: the env key alone is enough.
    let home = install_store(json!({}));
    std::env::set_var("META_API_KEY", "ENV-KEY");
    let seen = Seen::default();
    let upstream = spawn(rotating_upstream(seen.clone(), home.path().to_path_buf(), true)).await;
    let broker = LocalBroker::new(reqwest::Client::new())
        .with_account_policy(AccountPolicy::new())
        .with_muse_base_url_for_tests(upstream);

    let resp = broker.proxy(responses_request("muse")).await.unwrap();
    assert_eq!(resp.status, 401);
    assert_eq!(seen.auths(), vec!["Bearer ENV-KEY"], "env keys are not re-minted");

    // An explicit account stays explicit even with the env override set.
    let err = broker
        .proxy(responses_request("muse@work"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, BrokerError::UnknownAccount { .. }),
        "{err:?}"
    );
    std::env::remove_var("META_API_KEY");
}

#[tokio::test]
#[serial_test::serial]
async fn muse_catalog_get_is_allowed_and_mint_logout_are_denied() {
    std::env::remove_var("META_API_KEY");
    let _home = install_store(json!({ "muse": muse_slot("KEY-1") }));
    let seen = Seen::default();
    let seen2 = seen.clone();
    let app = Router::new().route(
        "/muse-code/models",
        get(move |headers: HeaderMap| {
            let seen = seen2.clone();
            async move {
                seen.push(&headers);
                r#"{"models":[]}"#
            }
        }),
    );
    let upstream = spawn(app).await;
    let broker = LocalBroker::new(reqwest::Client::new())
        .with_account_policy(AccountPolicy::new())
        .with_muse_base_url_for_tests(upstream);
    let resp = broker
        .proxy(ProxyRequest {
            provider: "muse".into(),
            method: ProxyMethod::Get,
            path: "/muse-code/models".into(),
            body: None,
            stream: false,
            body_bytes: None,
        })
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(seen.auths(), vec!["Bearer KEY-1"]);

    for (method, path) in [
        (ProxyMethod::Post, "/muse-code/key"),
        (ProxyMethod::Post, "/muse-code/logout"),
        (ProxyMethod::Get, "/v1/responses"),
        (ProxyMethod::Post, "/muse-code/models"),
        (ProxyMethod::Post, "/v1/chat/completions"),
    ] {
        let err = broker
            .proxy(ProxyRequest {
                provider: "muse".into(),
                method,
                path: path.into(),
                body: None,
                stream: false,
                body_bytes: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, BrokerError::Denied(_)), "{path}: {err:?}");
    }
    assert_eq!(seen.auths().len(), 1, "denied requests never reach upstream");
}
