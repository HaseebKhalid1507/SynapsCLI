//! Typed "no account with proven capacity" failures.
//!
//! A failed usage read must not vanish into a bare "quota unknown": the
//! selection plan, the broker error and one WARN line name each account's
//! failure kind and HTTP status, and the top-level kind tells "capacity
//! unproven" (a usage read failed) apart from "login rejected", "every
//! account is limited", "reads proved nothing" and "no login at all".
//!
//! Loopback usage mock and a throwaway credential store only. Every token
//! expires in 2100, so no refresh (and no request to a real token endpoint)
//! can happen.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent_core::auth::auth_state::render_text;
use agent_core::auth::{
    broker_from_source, local_auth_state, AccountPolicy, AccountSelector, AuthStateQuery,
    BrokerError, CredentialBroker, CredentialRef, CredentialSource, LocalBroker, OAuthProviderId,
    TokenCache,
};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

const FAR_FUTURE: u64 = 4_102_444_800_000; // year 2100: never refreshes

fn oauth(access: &str, refresh: &str) -> serde_json::Value {
    json!({ "type": "oauth", "access": access, "refresh": refresh, "expires": FAR_FUTURE })
}

/// Scratch store with two Anthropic accounts, `a` and `b`.
fn install_store() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    agent_core::config::set_base_dir_for_tests(home.path().to_path_buf());
    std::fs::write(
        home.path().join("auth.json"),
        json!({
            "anthropic@a": oauth("anthropic-a-access", "refresh-anth-a-SECRET"),
            "anthropic@b": oauth("anthropic-b-access", "refresh-anth-b-SECRET"),
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(home.path().join("config"), "").unwrap();
    home
}

/// Labels the scratch stores and the usage mock know.
const LABELS: [&str; 6] = ["a", "b", "c", "d", "e", "f"];

/// Expired in 1970: any use refreshes first.
const EXPIRED: u64 = 1_000;

/// Scratch store with one Anthropic account per `(label, expires)`; every
/// access token is `anthropic-<label>-access`.
fn install_accounts(accounts: &[(&str, u64)]) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    agent_core::config::set_base_dir_for_tests(home.path().to_path_buf());
    let mut root = serde_json::Map::new();
    for (label, expires) in accounts {
        assert!(LABELS.contains(label), "unknown fixture label {label}");
        let mut entry = oauth(
            &format!("anthropic-{label}-access"),
            &format!("refresh-anth-{label}-SECRET"),
        );
        entry["expires"] = json!(expires);
        root.insert(format!("anthropic@{label}"), entry);
    }
    std::fs::write(
        home.path().join("auth.json"),
        serde_json::Value::Object(root).to_string(),
    )
    .unwrap();
    std::fs::write(home.path().join("config"), "").unwrap();
    home
}

/// Loopback token endpoint answering every refresh with `status` and
/// `body`, plus the number of refreshes it has answered.
async fn spawn_token(status: u16, body: &'static str) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let app = Router::new().route(
        "/v1/oauth/token",
        axum::routing::post(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async move {
                (
                    StatusCode::from_u16(status).unwrap(),
                    [("content-type", "application/json")],
                    body,
                )
                    .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/v1/oauth/token"), hits)
}

/// A usage reading with room in every window.
fn roomy() -> Reply {
    let reset = "2099-01-01T00:00:00Z";
    Reply::Body(json!({
        "five_hour": {"utilization": 10.0, "resets_at": reset},
        "seven_day": {"utilization": 10.0, "resets_at": reset}
    }))
}

/// What the mock answers for one account's token.
#[derive(Clone)]
enum Reply {
    /// 429 with `Retry-After: <secs>`.
    RateLimited(u64),
    Status(u16),
    /// 200 with this body.
    Body(serde_json::Value),
}

type Script = Arc<Mutex<HashMap<&'static str, Reply>>>;

/// Usage mock keyed by the bearer's account (`a` / `b`).
async fn spawn_usage(script: Script) -> String {
    spawn_counted_usage(script).await.0
}

/// [`spawn_usage`] plus the number of requests it has answered.
async fn spawn_counted_usage(script: Script) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let app = Router::new().route(
        "/usage",
        get(move |headers: HeaderMap| {
            let script = script.clone();
            counter.fetch_add(1, Ordering::SeqCst);
            async move {
                let bearer = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let account = LABELS
                    .into_iter()
                    .find(|l| bearer.ends_with(&format!("anthropic-{l}-access")))
                    .unwrap_or("unknown");
                let reply = script.lock().unwrap().get(account).cloned();
                let response: Response = match reply {
                    Some(Reply::RateLimited(secs)) => (
                        StatusCode::TOO_MANY_REQUESTS,
                        [("retry-after", secs.to_string())],
                        "slow down",
                    )
                        .into_response(),
                    Some(Reply::Status(code)) => {
                        (StatusCode::from_u16(code).unwrap(), "upstream says no").into_response()
                    }
                    Some(Reply::Body(body)) => Json(body).into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                };
                response
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/usage"), hits)
}

fn auto_broker(usage_url: &str) -> LocalBroker {
    LocalBroker::new(reqwest::Client::new())
        .with_account_policy(
            AccountPolicy::new().with(OAuthProviderId::Anthropic, AccountSelector::Auto),
        )
        .with_usage_endpoint_override(usage_url.to_string())
}

fn anthropic(label: &str) -> CredentialRef {
    CredentialRef::new(
        OAuthProviderId::Anthropic,
        agent_core::auth::Account::named(label).unwrap(),
    )
}

/// In-process sink for formatted tracing output.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn take(&self) -> String {
        String::from_utf8(std::mem::take(&mut *self.0.lock().unwrap())).unwrap()
    }
}

struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_subscriber(capture: &Capture) -> impl tracing::Subscriber + Send + Sync {
    let sink = capture.0.clone();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || CaptureWriter(sink.clone()))
        .with_ansi(false)
        .finish()
}

/// The one `auth_selection_failed` WARN in `log`, failing unless there is
/// exactly one.
fn selection_warn(log: &str) -> String {
    let lines: Vec<_> = log
        .lines()
        .filter(|l| l.contains("auth_selection_failed"))
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "exactly one selection WARN per failure:\n{log}"
    );
    assert!(
        lines[0].contains("WARN"),
        "selection failure logs at WARN: {}",
        lines[0]
    );
    lines[0].to_string()
}

fn assert_secret_free(text: &str) {
    for secret in ["SECRET", "anthropic-a-access", "anthropic-b-access"] {
        assert!(!text.contains(secret), "secret material in {text}");
    }
}

/// JR's incident shape: the usage endpoint rate-limits one account and errors
/// for the other. The plan carries each account's cause; the error and the
/// WARN say "usage read failed", with each kind and HTTP status.
#[tokio::test]
#[serial_test::serial]
async fn usage_read_failures_keep_their_kind_status_and_retry_after() {
    let _home = install_store();
    let script: Script = Arc::default();
    script.lock().unwrap().insert("a", Reply::RateLimited(120));
    script.lock().unwrap().insert("b", Reply::Status(503));
    let broker = auto_broker(&spawn_usage(script.clone()).await);

    let plan = broker.plan(OAuthProviderId::Anthropic, None).await.unwrap();
    let selection = serde_json::to_value(&plan.selection).unwrap();
    assert_eq!(selection["outcome"], "no_capacity", "{selection}");
    let cause_of = |label: &str| {
        selection["rejections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["credential"] == format!("anthropic@{label}"))
            .map(|r| r["cause"].clone())
            .unwrap()
    };
    assert_eq!(
        cause_of("a"),
        json!({"kind": "usage_rate_limited", "http_status": 429, "retry_after_s": 120}),
        "{selection}"
    );
    assert_eq!(
        cause_of("b"),
        json!({"kind": "usage_upstream", "http_status": 503}),
        "{selection}"
    );

    let capture = Capture::default();
    let err = {
        let _guard = tracing::subscriber::set_default(capture_subscriber(&capture));
        broker
            .access_token_pinned(OAuthProviderId::Anthropic)
            .await
            .unwrap_err()
    };
    let text = err.to_string();
    for needle in [
        "usage_rate_limited",
        "HTTP 429",
        "retry after 120s",
        "usage_upstream",
        "HTTP 503",
    ] {
        assert!(text.contains(needle), "{needle} missing from: {text}");
    }
    assert_secret_free(&text);
    let log = capture.take();
    let warn = selection_warn(&log);
    assert!(warn.contains("provider=anthropic"), "{warn}");
    assert!(warn.contains("kind=usage_read_failed"), "{warn}");
    assert!(
        warn.contains("accounts=a:usage_rate_limited:429,b:usage_upstream:503"),
        "{warn}"
    );
    assert_secret_free(&log);
}

/// The top-level kind, first match wins: no_credentials > usage_read_failed >
/// credential_rejected (every account not cooling down needs a login) >
/// action_required (every such account needs a login or an action outside
/// synaps, at least one an action) > all_cooling_down (cooling down or at a
/// limit) > quota_unknown. A cooling account keeps its own read failure.
#[tokio::test]
#[serial_test::serial]
async fn selection_failure_kind_follows_the_priority_rules() {
    let home = install_store();
    let script: Script = Arc::default();
    let usage_url = spawn_usage(script.clone()).await;
    let broker = auto_broker(&usage_url);
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture_subscriber(&capture));

    // A transient read failure outranks a rejected login: capacity is
    // unproven, not absent.
    script.lock().unwrap().insert("a", Reply::Status(401));
    script.lock().unwrap().insert("b", Reply::RateLimited(30));
    broker
        .access_token_pinned(OAuthProviderId::Anthropic)
        .await
        .unwrap_err();
    let warn = selection_warn(&capture.take());
    assert!(warn.contains("kind=usage_read_failed"), "{warn}");
    assert!(
        warn.contains("accounts=a:usage_unauthorized:401,b:usage_rate_limited:429"),
        "{warn}"
    );

    // Every account's login rejected.
    script.lock().unwrap().insert("b", Reply::Status(403));
    let err = broker
        .access_token_pinned(OAuthProviderId::Anthropic)
        .await
        .unwrap_err();
    let warn = selection_warn(&capture.take());
    assert!(warn.contains("kind=credential_rejected"), "{warn}");
    assert!(
        warn.contains("accounts=a:usage_unauthorized:401,b:usage_unauthorized:403"),
        "{warn}"
    );
    assert!(err.to_string().contains("usage_unauthorized"), "{err}");

    // A cooling account doesn't count against "every account needs a login",
    // and its own refused read stays visible next to the cooldown.
    broker
        .report_cooldown(&anthropic("b"), Some(FAR_FUTURE), "test limit")
        .await
        .unwrap();
    capture.take();
    broker
        .access_token_pinned(OAuthProviderId::Anthropic)
        .await
        .unwrap_err();
    let warn = selection_warn(&capture.take());
    assert!(warn.contains("kind=credential_rejected"), "{warn}");
    assert!(
        warn.contains("accounts=a:usage_unauthorized:401,b:usage_unauthorized:403:cooling"),
        "{warn}"
    );

    // Cooling down or at a window limit everywhere.
    let reset = "2099-01-01T00:00:00Z";
    script.lock().unwrap().insert(
        "a",
        Reply::Body(json!({
            "five_hour": {"utilization": 100.0, "resets_at": reset},
            "seven_day": {"utilization": 40.0, "resets_at": reset}
        })),
    );
    broker
        .access_token_pinned(OAuthProviderId::Anthropic)
        .await
        .unwrap_err();
    let warn = selection_warn(&capture.take());
    assert!(warn.contains("kind=all_cooling_down"), "{warn}");
    assert!(
        warn.contains("accounts=a:exhausted,b:usage_unauthorized:403:cooling"),
        "{warn}"
    );

    // Reads succeeded but proved nothing. A new broker (with `b` cooled
    // again): the step above left a good snapshot of `a` that is still
    // fresh, and a selection reuses a fresh snapshot instead of reading.
    let broker = auto_broker(&usage_url);
    broker
        .report_cooldown(&anthropic("b"), Some(FAR_FUTURE), "test limit")
        .await
        .unwrap();
    capture.take();
    script.lock().unwrap().insert(
        "a",
        Reply::Body(json!({"five_hour": {"utilization": null, "resets_at": reset}})),
    );
    broker
        .access_token_pinned(OAuthProviderId::Anthropic)
        .await
        .unwrap_err();
    let warn = selection_warn(&capture.take());
    assert!(warn.contains("kind=quota_unknown"), "{warn}");
    assert!(
        warn.contains("accounts=a:window_unknown,b:usage_unauthorized:403:cooling"),
        "{warn}"
    );

    // No Anthropic login at all.
    std::fs::write(home.path().join("auth.json"), json!({}).to_string()).unwrap();
    broker
        .access_token_pinned(OAuthProviderId::Anthropic)
        .await
        .unwrap_err();
    let log = capture.take();
    let warn = selection_warn(&log);
    assert!(warn.contains("kind=no_credentials"), "{warn}");
    assert_secret_free(&log);
}

/// A preview (`plan`) is not a selection failure: no WARN.
#[tokio::test]
#[serial_test::serial]
async fn plan_preview_logs_no_selection_failure() {
    let _home = install_store();
    let script: Script = Arc::default();
    script.lock().unwrap().insert("a", Reply::Status(500));
    script.lock().unwrap().insert("b", Reply::Status(500));
    let broker = auto_broker(&spawn_usage(script).await);
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture_subscriber(&capture));
    broker.plan(OAuthProviderId::Anthropic, None).await.unwrap();
    let log = capture.take();
    assert!(!log.contains("auth_selection_failed"), "{log}");
}

/// r2 blocker 1: a cooldown does not erase what the account's last read or
/// refresh said. Every account cooling, one with a refused usage read
/// (401), one rate-limited (429, Retry-After 90) and one whose expired token
/// the (loopback) token endpoint refused (400 invalid_grant): the kind stays
/// all_cooling_down, each account keeps its own kind and status beside its
/// cooldown, and the earliest time is the earliest cooldown end.
#[tokio::test]
#[serial_test::serial]
async fn a_cooldown_keeps_each_accounts_read_or_refresh_failure_beside_it() {
    let _home = install_accounts(&[("a", FAR_FUTURE), ("b", FAR_FUTURE), ("c", EXPIRED)]);
    let script: Script = Arc::default();
    script.lock().unwrap().insert("a", Reply::Status(401));
    script.lock().unwrap().insert("b", Reply::RateLimited(90));
    let (token_url, token_hits) = spawn_token(
        400,
        r#"{"error":"invalid_grant","error_description":"Refresh token revoked"}"#,
    )
    .await;
    let broker =
        auto_broker(&spawn_usage(script).await).with_anthropic_token_url_for_tests(token_url);
    let now = agent_core::epoch_millis();
    let (until_a, until_b, until_c) = (now + 3 * 3_600_000, now + 3_600_000, now + 2 * 3_600_000);
    for (label, until) in [("a", until_a), ("b", until_b), ("c", until_c)] {
        broker
            .report_cooldown(&anthropic(label), Some(until), "anthropic_quota_exhausted")
            .await
            .unwrap();
    }

    let capture = Capture::default();
    let err = {
        let _guard = tracing::subscriber::set_default(capture_subscriber(&capture));
        broker
            .access_token_pinned(OAuthProviderId::Anthropic)
            .await
            .unwrap_err()
    };
    assert!(
        token_hits.load(Ordering::SeqCst) >= 1,
        "c's refresh was tried"
    );
    let BrokerError::NoAccountAvailable {
        failure: Some(failure),
        ..
    } = &err
    else {
        panic!("typed selection failure expected: {err:?}");
    };
    assert_eq!(
        serde_json::to_value(failure).unwrap(),
        json!({
            "kind": "all_cooling_down",
            "accounts": [
                {"account": "a", "kind": "usage_unauthorized", "http_status": 401,
                 "until_ms": until_a, "cooling_down": true},
                {"account": "b", "kind": "usage_rate_limited", "http_status": 429,
                 "retry_after_s": 90, "until_ms": until_b, "cooling_down": true},
                {"account": "c", "kind": "refresh_rejected", "http_status": 400,
                 "until_ms": until_c, "cooling_down": true},
            ],
            "earliest_reset_ms": until_b,
        })
    );
    assert_secret_free(&err.to_string());
    let log = capture.take();
    let warn = selection_warn(&log);
    assert!(warn.contains("kind=all_cooling_down"), "{warn}");
    assert!(
        warn.contains(
            "accounts=a:usage_unauthorized:401:cooling,b:usage_rate_limited:429:cooling,\
             c:refresh_rejected:400:cooling"
        ),
        "{warn}"
    );
    assert_secret_free(&log);

    // The report shows c's refused exchange with the endpoint's status,
    // without another exchange.
    let hits = token_hits.load(Ordering::SeqCst);
    let report = serde_json::to_value(broker.auth_state(&anthropic_query()).unwrap()).unwrap();
    let c = report["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["label"] == "c")
        .unwrap()
        .clone();
    assert_eq!(
        c["last_refresh"]["failure"],
        json!({"kind": "refresh_rejected", "http_status": 400}),
        "{c}"
    );
    assert_eq!(token_hits.load(Ordering::SeqCst), hits);
}

/// r2 blocker 2 / item 5: automatic selection on an unreadable store fails
/// with the store's error (naming the file, never its content) and exactly
/// one auth_selection_failed WARN of kind credential_store, recorded for
/// `auth state`. A preview on the same store logs no selection failure.
#[tokio::test]
#[serial_test::serial]
async fn an_unreadable_store_fails_automatic_selection_with_one_store_warn() {
    let home = install_store();
    let (usage_url, hits) = spawn_counted_usage(Arc::default()).await;
    let broker = auto_broker(&usage_url);
    let store = home.path().join("auth.json");
    let good = std::fs::read(&store).unwrap();
    std::fs::write(
        &store,
        r#"{"anthropic@a": {"type": "oauth", "refresh": "refresh-anth-a-SECRET""#,
    )
    .unwrap();

    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture_subscriber(&capture));
    let err = broker
        .access_token_pinned(OAuthProviderId::Anthropic)
        .await
        .unwrap_err();
    match &err {
        BrokerError::Credential(msg) => {
            assert!(
                msg.contains("Failed to parse") && msg.contains("auth.json"),
                "{msg}"
            )
        }
        other => panic!("the store error itself expected: {other:?}"),
    }
    let log = capture.take();
    let warn = selection_warn(&log);
    assert!(warn.contains("provider=anthropic"), "{warn}");
    assert!(warn.contains("kind=credential_store"), "{warn}");
    assert_secret_free(&err.to_string());
    assert_secret_free(&log);

    broker
        .plan(OAuthProviderId::Anthropic, None)
        .await
        .unwrap_err();
    let log = capture.take();
    assert!(!log.contains("auth_selection_failed"), "a preview: {log}");
    assert_eq!(hits.load(Ordering::SeqCst), 0, "no usage read");

    std::fs::write(&store, good).unwrap();
    let report = serde_json::to_value(broker.auth_state(&anthropic_query()).unwrap()).unwrap();
    let selections = report["selections"].as_array().unwrap();
    assert_eq!(selections.len(), 1, "{report}");
    assert_eq!(
        selections[0]["failure"]["kind"], "credential_store",
        "{report}"
    );
}

/// r2 blocker 2: one account's stored login can't be read (a malformed
/// entry) and the other is cooling down. That is an action for the user,
/// never "quota unknown".
#[tokio::test]
#[serial_test::serial]
async fn an_unreadable_stored_login_makes_selection_action_required_not_quota_unknown() {
    let home = install_store();
    let store = home.path().join("auth.json");
    let mut root: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&store).unwrap()).unwrap();
    root["anthropic@b"]["expires"] = json!("soon");
    std::fs::write(&store, root.to_string()).unwrap();
    let script: Script = Arc::default();
    script.lock().unwrap().insert("a", roomy());
    let broker = auto_broker(&spawn_usage(script).await);
    broker
        .report_cooldown(
            &anthropic("a"),
            Some(agent_core::epoch_millis() + 3_600_000),
            "anthropic_quota_exhausted",
        )
        .await
        .unwrap();

    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture_subscriber(&capture));
    let err = broker
        .access_token_pinned(OAuthProviderId::Anthropic)
        .await
        .unwrap_err();
    let warn = selection_warn(&capture.take());
    assert!(warn.contains("kind=action_required"), "{warn}");
    assert!(
        warn.contains("accounts=a:cooling_down,b:credential_store"),
        "{warn}"
    );
    let text = err.to_string();
    assert!(text.contains("credential_store"), "{text}");
    assert_secret_free(&text);
}

// ── `synaps auth state` ─────────────────────────────────────────────────────

const ACCOUNT_ID_A: &str = "4f1c2b9e-0d3a-4e5f-9a8b-7c6d5e4f3a2b";
const EMAIL_A: &str = "owner-a@example.com";

/// Scratch store for `auth state`: `a` records its provider account id and
/// an email; `b` records neither.
fn install_identified_store() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    agent_core::config::set_base_dir_for_tests(home.path().to_path_buf());
    let mut a = oauth("anthropic-a-access", "refresh-anth-a-SECRET");
    a["accountId"] = json!(ACCOUNT_ID_A);
    a["identity"] = json!(EMAIL_A);
    std::fs::write(
        home.path().join("auth.json"),
        json!({
            "anthropic@a": a,
            "anthropic@b": oauth("anthropic-b-access", "refresh-anth-b-SECRET"),
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(home.path().join("config"), "").unwrap();
    home
}

fn anthropic_query() -> AuthStateQuery {
    AuthStateQuery {
        provider: Some("anthropic".into()),
        ..AuthStateQuery::default()
    }
}

/// Every file in `dir` with its bytes.
fn snapshot_dir(dir: &std::path::Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let bytes = if e.file_type().unwrap().is_file() {
                std::fs::read(e.path()).unwrap()
            } else {
                Vec::new()
            };
            (e.file_name(), bytes)
        })
        .collect();
    files.sort();
    files
}

/// After a cooldown report and a selection that read `a`'s usage (429) and
/// `b`'s (ok), the report shows the cooldown with its reason, each read's
/// kind, HTTP status and time, and the failed selection. Building it makes
/// no usage request, and neither its JSON nor its text carries token
/// material, the email, the account id or its prefix, or a seat identity.
#[tokio::test]
#[serial_test::serial]
async fn auth_state_reports_memory_without_requests_or_secrets() {
    let _home = install_identified_store();
    let script: Script = Arc::default();
    script.lock().unwrap().insert("a", Reply::RateLimited(120));
    script.lock().unwrap().insert(
        "b",
        Reply::Body(
            json!({"five_hour": {"utilization": 10.0, "resets_at": "2099-01-01T00:00:00Z"}}),
        ),
    );
    let (usage_url, hits) = spawn_counted_usage(script).await;
    let broker = auto_broker(&usage_url);
    let until = agent_core::epoch_millis() + 3_600_000;
    broker
        .report_cooldown(&anthropic("b"), Some(until), "anthropic_quota_exhausted")
        .await
        .unwrap();
    broker
        .access_token_pinned(OAuthProviderId::Anthropic)
        .await
        .unwrap_err();
    let reads = hits.load(Ordering::SeqCst);
    assert_eq!(reads, 2, "one usage read per account");

    let report = broker.auth_state(&anthropic_query()).unwrap();
    assert_eq!(
        hits.load(Ordering::SeqCst),
        reads,
        "auth state made a usage request"
    );
    assert!(report.broker_active);
    let value = serde_json::to_value(&report).unwrap();
    let account = |label: &str| {
        value["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["label"] == label)
            .unwrap_or_else(|| panic!("no {label} in {value}"))
            .clone()
    };
    let a = account("a");
    assert_eq!(a["identity"], "stable", "{a}");
    assert_eq!(a["access_expires_ms"], FAR_FUTURE, "{a}");
    assert_eq!(
        a["last_usage"]["failure"],
        json!({"kind": "usage_rate_limited", "http_status": 429, "retry_after_s": 120}),
        "{a}"
    );
    assert!(a.get("last_usage_ok_ms").is_none(), "{a}");
    assert!(a.get("cooldown").is_none(), "{a}");
    let b = account("b");
    assert_eq!(b["identity"], "rotating", "{b}");
    assert_eq!(
        b["cooldown"],
        json!({"until_ms": until, "reason": "anthropic_quota_exhausted"}),
        "{b}"
    );
    assert!(b["last_usage"].get("failure").is_none(), "{b}");
    let read_at = b["last_usage"]["at_ms"]
        .as_u64()
        .expect("b's read is recorded");
    assert_eq!(b["last_usage_ok_ms"], read_at, "{b}");
    assert!(read_at <= report.generated_at_ms && report.generated_at_ms - read_at < 60_000);
    let selection = &value["selections"][0];
    assert_eq!(selection["provider"], "anthropic", "{value}");
    assert_eq!(selection["failure"]["kind"], "usage_read_failed", "{value}");

    let text = render_text(&report);
    for needle in [
        "usage_rate_limited, HTTP 429, retry after 120s, ",
        " ago",
        "anthropic_quota_exhausted",
        "failed: usage_read_failed (a:usage_rate_limited:429,b:cooling_down)",
    ] {
        assert!(text.contains(needle), "{needle:?} missing from:\n{text}");
    }
    let summaries = broker.accounts(OAuthProviderId::Anthropic).await.unwrap();
    let a_row = summaries.iter().find(|s| s.label == "a").unwrap();
    let fingerprint = a_row
        .seat_fingerprint
        .clone()
        .expect("a has a seat fingerprint");
    let prefix = a_row.account_id_prefix.clone().expect("a has an id prefix");
    for out in [value.to_string(), text] {
        assert_secret_free(&out);
        for leak in [
            ACCOUNT_ID_A,
            EMAIL_A,
            fingerprint.as_str(),
            prefix.as_str(),
            "id:",
            "cred:",
        ] {
            assert!(!out.contains(leak), "{leak:?} in {out}");
        }
    }
    assert_eq!(hits.load(Ordering::SeqCst), reads);
}

/// `clear` for one account drops that account's cooldown in the broker
/// requests use (not just in the report), keeps the other account's, logs
/// one INFO line naming the account and writes nothing in the store. With
/// no broker built yet, the report lists the accounts with nothing
/// recorded; an unknown label is an error, never "all accounts".
#[tokio::test]
#[serial_test::serial]
async fn auth_state_clear_drops_one_accounts_memory_and_writes_nothing() {
    let home = install_identified_store();
    let cache = TokenCache::new();
    let report = local_auth_state(&cache, &anthropic_query(), false).unwrap();
    assert!(!report.live && !report.broker_active, "{report:?}");
    assert_eq!(report.accounts.len(), 2, "{report:?}");
    assert!(report
        .accounts
        .iter()
        .all(|a| a.cooldown.is_none() && a.last_usage.is_none()));

    let broker = broker_from_source(&CredentialSource::Local, &cache, reqwest::Client::new());
    let until = agent_core::epoch_millis() + 3_600_000;
    for label in ["a", "b"] {
        broker
            .report_cooldown(&anthropic(label), Some(until), "anthropic_quota_exhausted")
            .await
            .unwrap();
    }
    let store_before = snapshot_dir(home.path());

    let capture = Capture::default();
    let clear_b = AuthStateQuery {
        account: Some("b".into()),
        clear: true,
        ..anthropic_query()
    };
    let report = {
        let _guard = tracing::subscriber::set_default(capture_subscriber(&capture));
        local_auth_state(&cache, &clear_b, true).unwrap()
    };
    assert!(report.live && report.broker_active, "{report:?}");
    assert_eq!(report.cleared, vec!["anthropic:b".to_string()]);
    assert_eq!(report.accounts.len(), 1, "{report:?}");
    assert!(report.accounts[0].cooldown.is_none(), "{report:?}");
    let log = capture.take();
    let lines: Vec<_> = log
        .lines()
        .filter(|l| l.contains("auth_state_cleared"))
        .collect();
    assert_eq!(lines.len(), 1, "{log}");
    assert!(
        lines[0].contains("INFO") && lines[0].contains("accounts=anthropic:b"),
        "{log}"
    );

    let rows = broker.accounts(OAuthProviderId::Anthropic).await.unwrap();
    let cooldown_of = |label: &str| {
        rows.iter()
            .find(|r| r.label == label)
            .unwrap()
            .cooldown_until
    };
    assert_eq!(cooldown_of("a"), Some(until), "a keeps its cooldown");
    assert_eq!(
        cooldown_of("b"),
        None,
        "b's cooldown is gone from the broker"
    );
    assert_eq!(
        snapshot_dir(home.path()),
        store_before,
        "clear wrote to the store dir"
    );

    let unknown = AuthStateQuery {
        account: Some("zzz".into()),
        clear: true,
        ..anthropic_query()
    };
    assert!(
        matches!(
            local_auth_state(&cache, &unknown, true),
            Err(BrokerError::UnknownAccount { .. })
        ),
        "an unknown label clears nothing"
    );
    let rows = broker.accounts(OAuthProviderId::Anthropic).await.unwrap();
    assert!(rows.iter().any(|r| r.cooldown_until == Some(until)));

    let clear_all = AuthStateQuery {
        clear: true,
        ..anthropic_query()
    };
    let report = local_auth_state(&cache, &clear_all, true).unwrap();
    assert_eq!(
        report.cleared,
        vec!["anthropic:a".to_string(), "anthropic:b".to_string()]
    );
    let rows = broker.accounts(OAuthProviderId::Anthropic).await.unwrap();
    assert!(rows.iter().all(|r| r.cooldown_until.is_none()), "{rows:?}");
    assert_eq!(snapshot_dir(home.path()), store_before);
}

/// r2 blocker 3: a cooldown's reason reaches the report only as a closed
/// kind. Known reasons keep their name; anything else (here a token-shaped
/// string, an email and a seat-identity-shaped string) is `other`, and none
/// of it appears in the JSON, the text or the Debug form.
#[tokio::test]
#[serial_test::serial]
async fn auth_state_reports_cooldown_reason_kinds_never_the_callers_string() {
    const TOKEN_REASON: &str = "sk-ant-oat01-Qm9vQ2mZ7pL4kR8sT1uW3yA6bC0dE5fGhIjKl";
    const EMAIL_REASON: &str = "victim@example.com";
    const SEAT_REASON: &str = "cred:3f9a1c7e5b2d4f60a8c1e3b5d7f9a2c4";
    let _home = install_accounts(&[
        ("a", FAR_FUTURE),
        ("b", FAR_FUTURE),
        ("c", FAR_FUTURE),
        ("d", FAR_FUTURE),
        ("e", FAR_FUTURE),
        ("f", FAR_FUTURE),
    ]);
    let (usage_url, hits) = spawn_counted_usage(Arc::default()).await;
    let broker = auto_broker(&usage_url);
    let until = agent_core::epoch_millis() + 3_600_000;
    let reasons = [
        ("a", TOKEN_REASON, "other"),
        ("b", EMAIL_REASON, "other"),
        ("c", SEAT_REASON, "other"),
        (
            "d",
            "anthropic_quota_exhausted",
            "anthropic_quota_exhausted",
        ),
        ("e", "codex_quota_exhausted", "codex_quota_exhausted"),
        ("f", "client_report", "client_report"),
    ];
    for (label, reason, _) in reasons {
        broker
            .report_cooldown(&anthropic(label), Some(until), reason)
            .await
            .unwrap();
    }
    let report = broker.auth_state(&anthropic_query()).unwrap();
    let value = serde_json::to_value(&report).unwrap();
    for (label, _, kind) in reasons {
        let account = value["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["label"] == label)
            .unwrap();
        assert_eq!(
            account["cooldown"],
            json!({"until_ms": until, "reason": kind}),
            "{account}"
        );
    }
    let text = render_text(&report);
    assert!(
        text.contains("other") && text.contains("client_report"),
        "{text}"
    );
    for out in [value.to_string(), text, format!("{report:?}")] {
        for leak in [
            TOKEN_REASON,
            "Qm9vQ2mZ",
            EMAIL_REASON,
            "victim",
            SEAT_REASON,
            "3f9a1c7e5b2d4f60",
        ] {
            assert!(!out.contains(leak), "{leak:?} in {out}");
        }
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0, "no usage read");
}

/// r2 item 10: neither the report nor a clear (account-scoped or
/// provider-wide) refreshes an expired token or reads usage, and neither
/// writes the store. Multi-threaded on purpose: a report path that blocked
/// on a refresh would run here (and hit the loopback token endpoint)
/// instead of panicking.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn auth_state_and_clear_never_refresh_an_expired_token_or_read_usage() {
    let home = install_accounts(&[("a", FAR_FUTURE), ("c", EXPIRED)]);
    let script: Script = Arc::default();
    script.lock().unwrap().insert("a", roomy());
    let (usage_url, usage_hits) = spawn_counted_usage(script).await;
    let (token_url, token_hits) = spawn_token(503, r#"{"error":"overloaded"}"#).await;
    let broker = auto_broker(&usage_url).with_anthropic_token_url_for_tests(token_url);
    // One selection: a's usage is read, c's refresh is tried (503) and a
    // is chosen. That leaves something to report and to clear.
    let pinned = broker
        .access_token_pinned(OAuthProviderId::Anthropic)
        .await
        .unwrap();
    assert_eq!(pinned.credential, anthropic("a"));
    let (usage_before, token_before) = (
        usage_hits.load(Ordering::SeqCst),
        token_hits.load(Ordering::SeqCst),
    );
    assert!(token_before >= 1, "the selection tried c's refresh");
    let store_before = snapshot_dir(home.path());

    let report = serde_json::to_value(broker.auth_state(&anthropic_query()).unwrap()).unwrap();
    let c = report["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["label"] == "c")
        .unwrap()
        .clone();
    assert_eq!(c["access_expires_ms"], EXPIRED, "{c}");
    assert_eq!(
        c["last_refresh"]["failure"]["kind"], "refresh_transient",
        "{c}"
    );
    for query in [
        AuthStateQuery {
            account: Some("c".into()),
            clear: true,
            ..anthropic_query()
        },
        AuthStateQuery {
            clear: true,
            ..anthropic_query()
        },
    ] {
        broker.auth_state(&query).unwrap();
    }
    assert_eq!(
        usage_hits.load(Ordering::SeqCst),
        usage_before,
        "a usage read"
    );
    assert_eq!(token_hits.load(Ordering::SeqCst), token_before, "a refresh");
    assert_eq!(snapshot_dir(home.path()), store_before, "a store write");
}

/// r2 item 9: `--account b` shows only the selections that chose or
/// considered b. Selection m1 chose a while b was cooling (b was not a
/// failure there, so it is not shown); selection m2 failed with both.
#[tokio::test]
#[serial_test::serial]
async fn auth_state_account_filter_keeps_only_selections_that_chose_or_considered_it() {
    const M1: &str = "claude-opus-4-7";
    const M2: &str = "claude-sonnet-4-5";
    let _home = install_store();
    let script: Script = Arc::default();
    script.lock().unwrap().insert("a", roomy());
    script.lock().unwrap().insert("b", roomy());
    let broker = auto_broker(&spawn_usage(script).await);
    let until = agent_core::epoch_millis() + 3_600_000;
    broker
        .report_cooldown(&anthropic("b"), Some(until), "anthropic_quota_exhausted")
        .await
        .unwrap();
    let pinned = broker
        .access_token_pinned_for(OAuthProviderId::Anthropic, Some(M1))
        .await
        .unwrap();
    assert_eq!(pinned.credential, anthropic("a"));
    broker
        .report_cooldown(&anthropic("a"), Some(until), "anthropic_quota_exhausted")
        .await
        .unwrap();
    broker
        .access_token_pinned_for(OAuthProviderId::Anthropic, Some(M2))
        .await
        .unwrap_err();

    let models = |account: Option<&str>| -> Vec<String> {
        let query = AuthStateQuery {
            account: account.map(str::to_string),
            ..anthropic_query()
        };
        let mut models: Vec<String> = broker
            .auth_state(&query)
            .unwrap()
            .selections
            .iter()
            .map(|s| s.model.clone().unwrap_or_default())
            .collect();
        models.sort();
        models
    };
    assert_eq!(models(None), vec![M1, M2]);
    assert_eq!(models(Some("a")), vec![M1, M2]);
    assert_eq!(models(Some("b")), vec![M2]);
}
