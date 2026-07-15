//! Zero-network Google Gemini OAuth harness.
//!
//! Exercises `login_with` end-to-end against a loopback OAuth token endpoint
//! and a driver "browser" that completes the loopback callback. No production
//! Google host is contacted at any point.

use agent_core::auth::google_gemini::{
    build_authorize_url, credentials_from_token_response, login_with, parse_pasted_callback,
    redirect_uri, refresh_with_endpoint, GeminiAuthError, CALLBACK_HOST, CALLBACK_PATH,
    CLIENT_ID, CLIENT_SECRET, PROVIDER,
};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Form, Router,
};
use serial_test::serial;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

// ── Local token-endpoint fake ────────────────────────────────────────────────

#[derive(Clone, Default)]
struct SeenForms(Arc<Mutex<Vec<HashMap<String, String>>>>);

async fn ok_token(
    State(seen): State<SeenForms>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    seen.0.lock().unwrap().push(form.clone());
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        r#"{"access_token":"aaa","refresh_token":"rrr","expires_in":3600,"token_type":"Bearer","scope":"https://www.googleapis.com/auth/cloud-platform"}"#,
    )
        .into_response()
}

async fn bad_request_token(
    State(_): State<SeenForms>,
    Form(_form): Form<HashMap<String, String>>,
) -> Response {
    (
        StatusCode::BAD_REQUEST,
        [("content-type", "application/json")],
        r#"{"error":"invalid_grant"}"#,
    )
        .into_response()
}

async fn refresh_reissues_access_only(
    State(seen): State<SeenForms>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    seen.0.lock().unwrap().push(form.clone());
    // Google's refresh grant typically omits refresh_token; broker must carry
    // the previous refresh forward — this fake proves the carry-over path.
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        r#"{"access_token":"new-access","expires_in":1800,"token_type":"Bearer"}"#,
    )
        .into_response()
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/token")
}

fn free_loopback_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

// ── E2E: full login flow ─────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn login_completes_via_loopback_and_stores_atomically() {
    let seen = SeenForms::default();
    let token_url = serve(
        Router::new()
            .route("/token", post(ok_token))
            .with_state(seen.clone()),
    )
    .await;

    let port = free_loopback_port();

    // Isolated auth home so save_provider_auth is unattended-safe.
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());

    let creds = login_with(
        port,
        &token_url,
        /* allow_http_token_endpoint = */ true,
        |auth_url| {
            let url = url::Url::parse(auth_url).unwrap();
            let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
            let state = q.get("state").cloned().unwrap();
            let redirect_uri = q.get("redirect_uri").cloned().unwrap();
            // Complete the callback like a real browser would.

tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let cb = format!("{redirect_uri}?code=abc123&state={state}");
                let _ = reqwest::Client::new().get(&cb).send().await;
            });
            Ok(())
        },
    )
    .await
    .expect("login must complete");

    assert_eq!(creds.access, "aaa");
    assert_eq!(creds.refresh, "rrr");
    assert_eq!(creds.auth_type, "oauth");

    // Verify the token exchange body: authorization_code grant, PKCE verifier,
    // correct client_id, correct redirect_uri.
    let forms = seen.0.lock().unwrap().clone();
    assert_eq!(forms.len(), 1);
    let f = &forms[0];
    assert_eq!(f.get("grant_type").map(String::as_str), Some("authorization_code"));
    assert_eq!(f.get("code").map(String::as_str), Some("abc123"));
    assert_eq!(f.get("client_id").map(String::as_str), Some(CLIENT_ID));
    // Installed-app secret is required for the token exchange but never logged.
    assert_eq!(f.get("client_secret").map(String::as_str), Some(CLIENT_SECRET));
    assert!(f.get("code_verifier").map(|v| !v.is_empty()).unwrap_or(false));
    assert_eq!(
        f.get("redirect_uri").map(String::as_str),
        Some(redirect_uri(port).as_str())
    );

    // Atomic storage under the canonical provider key.
    let stored = agent_core::auth::load_provider_auth(PROVIDER)
        .expect("load_provider_auth ok")
        .expect("credentials stored on disk");
    assert_eq!(stored.access, "aaa");
    assert_eq!(stored.refresh, "rrr");
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn login_surfaces_token_error_without_leaking_secrets() {
    let seen = SeenForms::default();
    let token_url = serve(
        Router::new()
            .route("/token", post(bad_request_token))
            .with_state(seen.clone()),
    )
    .await;

    let port = free_loopback_port();
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());

    let err = login_with(port, &token_url, true, |auth_url| {
        let url = url::Url::parse(auth_url).unwrap();
        let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
        let state = q["state"].clone();
        let redirect_uri = q["redirect_uri"].clone();

tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let cb = format!("{redirect_uri}?code=abc&state={state}");
            let _ = reqwest::Client::new().get(&cb).send().await;
        });
        Ok(())
    })
    .await
    .expect_err("HTTP 400 from token endpoint must surface an error");

    assert!(err.contains("google-gemini"));
    assert!(err.contains("400"));
    // No client secret, no code, no state.
    assert!(!err.contains(CLIENT_SECRET));
    assert!(!err.contains("abc"));
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn refresh_grant_carries_forward_previous_refresh_token() {
    let seen = SeenForms::default();
    let token_url = serve(
        Router::new()
            .route("/token", post(refresh_reissues_access_only))
            .with_state(seen.clone()),
    )
    .await;

    let client = reqwest::Client::new();
    let refreshed = refresh_with_endpoint(&client, &token_url, true, "old-refresh")
        .await
        .expect("refresh with omitted refresh_token must carry the old one");
    assert_eq!(refreshed.access, "new-access");
    assert_eq!(refreshed.refresh, "old-refresh");

    // Verify wire form used the refresh grant.
    let form = seen.0.lock().unwrap().last().cloned().unwrap();
    assert_eq!(form.get("grant_type").map(String::as_str), Some("refresh_token"));
    assert_eq!(form.get("refresh_token").map(String::as_str), Some("old-refresh"));
    assert_eq!(form.get("client_id").map(String::as_str), Some(CLIENT_ID));
    assert_eq!(form.get("client_secret").map(String::as_str), Some(CLIENT_SECRET));
}

// ── Structural sanity: pure helpers still hold under harness compile ─────────

#[test]
fn pure_helpers_are_reachable_from_harness() {
    let url = build_authorize_url("cc", "st", 45289);
    assert!(url.contains(CLIENT_ID));
    // Scopes are percent-encoded in the query string; check the tail token.
    assert!(url.contains("cloud-platform"));
    assert!(url.contains("state=st"));
    assert!(url.contains("code_challenge=cc"));

    assert!(parse_pasted_callback(
        &format!("http://{CALLBACK_HOST}:45289{CALLBACK_PATH}?code=c&state=st"),
        "st",
        45289
    )
    .is_some());

    // credentials_from_token_response happy path exposed publicly for tests.
    let creds = credentials_from_token_response(
        r#"{"access_token":"a","refresh_token":"r","expires_in":10}"#,
        None,
        true,
    )
    .unwrap();
    assert_eq!(creds.access, "a");
    assert!(matches!(
        credentials_from_token_response("{}", None, true),
        Err(GeminiAuthError::EmptyAccessToken)
    ));
}
