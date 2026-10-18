//! Meta Muse broker boundary.
//!
//! * The Muse proxy pins the origin, allowlists only `POST /v1/responses` and
//!   `GET /muse-code/models`, and attaches the minted Model API key — never
//!   the Meta OAuth token stored as `refresh`.
//! * A 401 on the minted key runs the broker's rejected-token recovery once
//!   (here: a peer already re-minted, so the fresh key is adopted with no
//!   network mint) and retries exactly once — never a loop.
//! * Every broker here points the Muse key mint at a loopback fake
//!   (`with_muse_mint_url_for_tests`): no test can reach api.meta.ai.
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

/// A loopback URL nothing listens on: an accidental mint fails locally.
async fn dead_mint() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}/muse-code/key")
}

/// Muse broker against a fake upstream and a fake (or dead) key mint.
fn muse_broker(upstream: String, mint: String) -> LocalBroker {
    LocalBroker::new(reqwest::Client::new())
        .with_account_policy(AccountPolicy::new())
        .with_muse_base_url_for_tests(upstream)
        .with_muse_mint_url_for_tests(mint)
}

/// Fake key mint: records each request's bearer and answers the next
/// scripted `(status, body)` (the last one repeats).
#[derive(Clone)]
struct Mint {
    bearers: Arc<Mutex<Vec<String>>>,
    replies: Arc<Vec<(StatusCode, String)>>,
}

impl Mint {
    fn new(replies: &[(StatusCode, &str)]) -> Self {
        Self {
            bearers: Arc::default(),
            replies: Arc::new(replies.iter().map(|(s, b)| (*s, b.to_string())).collect()),
        }
    }
    fn calls(&self) -> usize {
        self.bearers.lock().unwrap().len()
    }
    async fn spawn(&self) -> String {
        let mint = self.clone();
        let app = Router::new().route(
            "/muse-code/key",
            post(move |headers: HeaderMap| {
                let mint = mint.clone();
                async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    let n = {
                        let mut b = mint.bearers.lock().unwrap();
                        b.push(auth);
                        b.len()
                    };
                    let (status, body) = mint.replies[(n - 1).min(mint.replies.len() - 1)].clone();
                    (status, body).into_response()
                }
            }),
        );
        format!("{}/muse-code/key", spawn(app).await)
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
    let broker = muse_broker(upstream, dead_mint().await);

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
    let broker = muse_broker(upstream, dead_mint().await);

    let resp = broker.proxy(responses_request("muse")).await.unwrap();
    assert_eq!(resp.status, 401);
    assert_eq!(seen.auths().len(), 2, "one recovery retry, never a loop");
}

/// Upstream that rejects every key and never rotates storage: the stored
/// key stays the rejected one, so recovery would have to re-mint.
fn always_401_upstream(seen: Seen) -> Router {
    Router::new().route(
        "/v1/responses",
        post(move |headers: HeaderMap| {
            let seen = seen.clone();
            async move {
                seen.push(&headers);
                (StatusCode::UNAUTHORIZED, UPSTREAM_401_BODY).into_response()
            }
        }),
    )
}

/// Another process re-minted the Muse key moments ago: it left its refresh
/// time in the account's lock file, exactly as the refresh path does.
fn stamp_recent_refresh(home: &std::path::Path, storage_key: &str) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    std::fs::write(
        home.join(format!("auth.json.{storage_key}.refresh.lock")),
        now_ms.to_string(),
    )
    .unwrap();
}

const UPSTREAM_401_BODY: &str = r#"{"error":{"code":"invalid_api_key","type":"authentication_error"}}"#;

/// The Muse 401 re-mint runs through the single locked refresh path and its
/// per-account forced-refresh cooldown: this account was just re-minted by
/// another process (stamp in `auth.json.muse.refresh.lock`), so no request
/// mints a new key — none reaches the network mint (which would fail here).
/// The caller gets the ORIGINAL upstream 401 (status and body), like the
/// env-key path, and every request makes exactly one upstream call. A
/// permanent 401 therefore never mints on every request.
#[tokio::test]
#[serial_test::serial]
async fn muse_permanent_401_does_not_mint_inside_the_cooldown() {
    std::env::remove_var("META_API_KEY");
    let home = install_store(json!({ "muse": muse_slot("KEY-1") }));
    stamp_recent_refresh(home.path(), "muse");
    let seen = Seen::default();
    let upstream = spawn(always_401_upstream(seen.clone())).await;
    let broker = muse_broker(upstream, dead_mint().await);

    for n in 1..=3 {
        let resp = broker
            .proxy(responses_request("muse"))
            .await
            .expect("refused re-mint returns the upstream 401, not a broker error");
        assert_eq!(resp.status, 401);
        assert_eq!(resp.body, UPSTREAM_401_BODY);
        assert_eq!(seen.auths().len(), n, "one upstream call, no retry, per request");
    }
    assert!(seen.auths().iter().all(|a| a == "Bearer KEY-1"), "no new key was minted");
}

/// Upstream that accepts only `good` and rejects every other key.
fn accepts_only(seen: Seen, good: &'static str) -> Router {
    Router::new().route(
        "/v1/responses",
        post(move |headers: HeaderMap| {
            let seen = seen.clone();
            async move {
                seen.push(&headers);
                let ok = headers.get("authorization").and_then(|v| v.to_str().ok())
                    == Some(&format!("Bearer {good}"));
                if ok {
                    (StatusCode::OK, r#"{"id":"resp_1","status":"completed"}"#).into_response()
                } else {
                    (StatusCode::UNAUTHORIZED, UPSTREAM_401_BODY).into_response()
                }
            }
        }),
    )
}

fn stored_muse(home: &std::path::Path) -> serde_json::Value {
    let text = std::fs::read_to_string(home.join("auth.json")).unwrap();
    serde_json::from_str::<serde_json::Value>(&text).unwrap()["muse"].clone()
}

/// (a) A real re-mint: the 401 on the stored key mints a new key from the
/// stored Meta OAuth login (sent only to the mint), persists it, stamps the
/// account's cooldown, and the request is retried once with the new key.
#[tokio::test]
#[serial_test::serial]
async fn muse_401_remints_from_the_fake_mint_and_retries_once() {
    std::env::remove_var("META_API_KEY");
    let home = install_store(json!({ "muse": muse_slot("KEY-1") }));
    let mint = Mint::new(&[(StatusCode::OK, r#"{"api_key":"KEY-MINTED"}"#)]);
    let seen = Seen::default();
    let upstream = spawn(accepts_only(seen.clone(), "KEY-MINTED")).await;
    let broker = muse_broker(upstream, mint.spawn().await);

    let resp = broker.proxy(responses_request("muse")).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(seen.auths(), vec!["Bearer KEY-1", "Bearer KEY-MINTED"]);
    assert_eq!(*mint.bearers.lock().unwrap(), vec!["Bearer META-OAUTH-SECRET"]);
    let stored = stored_muse(home.path());
    assert_eq!(stored["access"], "KEY-MINTED");
    assert_eq!(stored["refresh"], "META-OAUTH-SECRET", "OAuth login is kept");
    let stamp = std::fs::read_to_string(home.path().join("auth.json.muse.refresh.lock")).unwrap();
    assert!(stamp.trim().parse::<u64>().is_ok(), "re-mint stamped the cooldown: {stamp:?}");
}

/// (b) The mint rejects the stored Meta login (401): a credential failure.
/// The caller gets the ORIGINAL upstream 401 (the runtime's "sign in
/// again"), inference is not retried, and nothing is written.
#[tokio::test]
#[serial_test::serial]
async fn muse_mint_401_returns_the_upstream_401_without_retry() {
    std::env::remove_var("META_API_KEY");
    let home = install_store(json!({ "muse": muse_slot("KEY-1") }));
    let mint = Mint::new(&[(StatusCode::UNAUTHORIZED, r#"{"error":"invalid_token"}"#)]);
    let seen = Seen::default();
    let upstream = spawn(always_401_upstream(seen.clone())).await;
    let broker = muse_broker(upstream, mint.spawn().await);

    let resp = broker.proxy(responses_request("muse")).await.unwrap();
    assert_eq!(resp.status, 401);
    assert_eq!(resp.body, UPSTREAM_401_BODY);
    assert_eq!(seen.auths(), vec!["Bearer KEY-1"], "no inference retry");
    assert_eq!(mint.calls(), 1);
    assert_eq!(stored_muse(home.path())["access"], "KEY-1", "nothing persisted");
}

/// (c) Upstream rejects every key: the first 401 mints once and retries
/// once; later requests inside the cooldown get the upstream 401 without
/// minting again. At most one mint per cooldown, however many requests.
#[tokio::test]
#[serial_test::serial]
async fn muse_persistent_401_mints_at_most_once_per_cooldown() {
    std::env::remove_var("META_API_KEY");
    let _home = install_store(json!({ "muse": muse_slot("KEY-1") }));
    let mint = Mint::new(&[
        (StatusCode::OK, r#"{"api_key":"KEY-M1"}"#),
        (StatusCode::OK, r#"{"api_key":"KEY-M2"}"#),
    ]);
    let seen = Seen::default();
    let upstream = spawn(always_401_upstream(seen.clone())).await;
    let broker = muse_broker(upstream, mint.spawn().await);

    for _ in 0..3 {
        let resp = broker.proxy(responses_request("muse")).await.unwrap();
        assert_eq!(resp.status, 401);
    }
    assert_eq!(mint.calls(), 1, "one mint per cooldown");
    assert_eq!(
        seen.auths(),
        vec!["Bearer KEY-1", "Bearer KEY-M1", "Bearer KEY-M1", "Bearer KEY-M1"]
    );
}

/// (d) The re-mint fails for a transient reason (mint 503, or no network
/// at all): not a login problem, so the caller gets a retryable refresh
/// error, not the upstream 401 ("sign in again"). No inference retry, and
/// nothing is written.
#[tokio::test]
#[serial_test::serial]
async fn muse_transient_mint_failure_is_a_retryable_error() {
    use agent_core::auth::RefreshFailureKind;
    std::env::remove_var("META_API_KEY");
    let home = install_store(json!({ "muse": muse_slot("KEY-1") }));
    let mint = Mint::new(&[(StatusCode::SERVICE_UNAVAILABLE, "<html>busy</html>")]);
    for mint_url in [mint.spawn().await, dead_mint().await] {
        let seen = Seen::default();
        let upstream = spawn(always_401_upstream(seen.clone())).await;
        let broker = muse_broker(upstream, mint_url.clone());
        let err = broker.proxy(responses_request("muse")).await.unwrap_err();
        let BrokerError::Refresh(failure) = &err else {
            panic!("{mint_url}: expected a refresh error, got {err:?}");
        };
        assert_eq!(failure.kind, RefreshFailureKind::Transient, "{mint_url}: {err}");
        let text = err.to_string();
        assert!(text.contains("retry"), "{text}");
        assert!(!text.contains("synaps login"), "{text}");
        assert_eq!(seen.auths(), vec!["Bearer KEY-1"], "no inference retry");
        assert_eq!(stored_muse(home.path())["access"], "KEY-1");
    }
    assert_eq!(mint.calls(), 1);
}

/// The mint says the Meta account needs a plan / payment method: a
/// billing problem, not the login. The caller gets that instruction (typed
/// ActionRequired), not the upstream 401 ("sign in again"), and no retry.
#[tokio::test]
#[serial_test::serial]
async fn muse_payment_required_is_not_sign_in_again() {
    use agent_core::auth::RefreshFailureKind;
    std::env::remove_var("META_API_KEY");
    let _home = install_store(json!({ "muse": muse_slot("KEY-1") }));
    let mint = Mint::new(&[(StatusCode::OK, r#"{"require_payment":true}"#)]);
    let seen = Seen::default();
    let upstream = spawn(always_401_upstream(seen.clone())).await;
    let broker = muse_broker(upstream, mint.spawn().await);
    let err = broker.proxy(responses_request("muse")).await.unwrap_err();
    let BrokerError::Refresh(failure) = &err else {
        panic!("expected a refresh error, got {err:?}");
    };
    assert_eq!(failure.kind, RefreshFailureKind::ActionRequired, "{err}");
    assert!(err.to_string().contains("payment method"), "{err}");
    assert_eq!(seen.auths(), vec!["Bearer KEY-1"], "no inference retry");
}

/// The stream path surfaces the same retryable refresh error.
#[tokio::test]
#[serial_test::serial]
async fn muse_stream_transient_mint_failure_is_a_retryable_error() {
    std::env::remove_var("META_API_KEY");
    let _home = install_store(json!({ "muse": muse_slot("KEY-1") }));
    let seen = Seen::default();
    let upstream = spawn(always_401_upstream(seen.clone())).await;
    let broker = muse_broker(upstream, dead_mint().await);
    let err = match broker.proxy_stream(responses_request("muse")).await {
        Ok(_) => panic!("401 must not stream"),
        Err(e) => e,
    };
    assert!(matches!(err, BrokerError::Refresh(_)), "{err:?}");
    assert_eq!(seen.auths().len(), 1);
}

/// `proxy_stream` goes through the same `send`: a refused re-mint surfaces
/// as the upstream 401 status (the stream path's status-only error), not a
/// credential error.
#[tokio::test]
#[serial_test::serial]
async fn muse_stream_refused_remint_surfaces_the_upstream_401() {
    std::env::remove_var("META_API_KEY");
    let home = install_store(json!({ "muse": muse_slot("KEY-1") }));
    stamp_recent_refresh(home.path(), "muse");
    let seen = Seen::default();
    let upstream = spawn(always_401_upstream(seen.clone())).await;
    let broker = muse_broker(upstream, dead_mint().await);

    let err = match broker.proxy_stream(responses_request("muse")).await {
        Ok(_) => panic!("401 must not stream"),
        Err(e) => e,
    };
    assert!(matches!(err, BrokerError::Transport(_)), "{err:?}");
    assert!(err.to_string().contains("401"), "{err}");
    assert_eq!(seen.auths().len(), 1);
}

#[tokio::test]
#[serial_test::serial]
async fn muse_env_key_overrides_login_and_is_never_recovered() {
    // No muse login at all: the env key alone is enough.
    let home = install_store(json!({}));
    std::env::set_var("META_API_KEY", "ENV-KEY");
    let seen = Seen::default();
    let upstream = spawn(rotating_upstream(seen.clone(), home.path().to_path_buf(), true)).await;
    let broker = muse_broker(upstream, dead_mint().await);

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
    let broker = muse_broker(upstream, dead_mint().await);
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
