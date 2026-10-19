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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

/// Formats into `capture` through the daemon log file's own filter:
/// `with_crate_directives` over the ERROR default that an unset `RUST_LOG`
/// gives, exactly what `init_logging` builds. A line these tests find is a
/// line synaps.log keeps.
fn capture_subscriber(capture: &Capture) -> impl tracing::Subscriber + Send + Sync {
    use tracing_subscriber::filter::{EnvFilter, LevelFilter};
    let unset = EnvFilter::builder()
        .with_default_directive(LevelFilter::ERROR.into())
        .parse_lossy("");
    let sink = capture.0.clone();
    tracing_subscriber::fmt()
        .with_env_filter(agent_core::logging::with_crate_directives(unset))
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
/// limit) > quota_unknown. A cooling account is not read: it is cooling down.
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
    // unproven, not absent. On its own usage endpoint: a failed read is
    // held process-wide per slot and endpoint, so the held 429 of `b` must
    // not stand in for the reads below.
    let held: Script = Arc::default();
    held.lock().unwrap().insert("a", Reply::Status(401));
    held.lock().unwrap().insert("b", Reply::RateLimited(30));
    auto_broker(&spawn_usage(held).await)
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
    script.lock().unwrap().insert("a", Reply::Status(401));
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
    // and it is not read (S3): it is cooling down, nothing else.
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
        warn.contains("accounts=a:usage_unauthorized:401,b:cooling_down"),
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
    assert!(warn.contains("accounts=a:exhausted,b:cooling_down"), "{warn}");

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
        warn.contains("accounts=a:window_unknown,b:cooling_down"),
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

/// S3 (a), replacing r2 blocker 1's test: an account whose cooldown is
/// active is not read. Every account cooling, one whose usage read would be
/// refused (401), one rate-limited (429) and one whose expired token the
/// (loopback) token endpoint would refuse: no usage request and no token
/// request at all; the kind is all_cooling_down, each account is cooling
/// down with its own time, and the earliest time is the earliest cooldown
/// end. `auth state` shows no read or refresh for any of them.
#[tokio::test]
#[serial_test::serial]
async fn every_account_cooling_down_is_reported_without_a_read_or_refresh() {
    let _home = install_accounts(&[("a", FAR_FUTURE), ("b", FAR_FUTURE), ("c", EXPIRED)]);
    let script: Script = Arc::default();
    script.lock().unwrap().insert("a", Reply::Status(401));
    script.lock().unwrap().insert("b", Reply::RateLimited(90));
    script.lock().unwrap().insert("c", roomy());
    let (usage_url, usage_hits) = spawn_counted_usage(script).await;
    let (token_url, token_hits) = spawn_token(400, INVALID_GRANT).await;
    let broker = auto_broker(&usage_url).with_anthropic_token_url_for_tests(token_url);
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
    assert_eq!(usage_hits.load(Ordering::SeqCst), 0, "no usage request");
    assert_eq!(token_hits.load(Ordering::SeqCst), 0, "no token request");
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
                {"account": "a", "kind": "cooling_down", "until_ms": until_a, "cooling_down": true},
                {"account": "b", "kind": "cooling_down", "until_ms": until_b, "cooling_down": true},
                {"account": "c", "kind": "cooling_down", "until_ms": until_c, "cooling_down": true},
            ],
            "earliest_reset_ms": until_b,
        })
    );
    assert_secret_free(&err.to_string());
    let log = capture.take();
    let warn = selection_warn(&log);
    assert!(warn.contains("kind=all_cooling_down"), "{warn}");
    assert!(
        warn.contains("accounts=a:cooling_down,b:cooling_down,c:cooling_down"),
        "{warn}"
    );
    assert_secret_free(&log);

    for label in ["a", "b", "c"] {
        let account = account_state(&broker, label);
        assert!(account.get("last_usage").is_none(), "{account}");
        assert!(account.get("last_refresh").is_none(), "{account}");
        assert!(account["cooldown"].is_object(), "{account}");
    }
    assert_eq!(usage_hits.load(Ordering::SeqCst), 0);
    assert_eq!(token_hits.load(Ordering::SeqCst), 0);
}

/// S3 (a): with a cooling and b roomy, selections and the preview read only
/// b's usage and b serves. After `auth state --clear` of a (its cooldown
/// gone), a is read again.
#[tokio::test]
#[serial_test::serial]
async fn a_cooling_account_is_not_read_until_its_cooldown_is_gone() {
    let _home = install_accounts(&[("a", FAR_FUTURE), ("b", FAR_FUTURE)]);
    let script: Script = Arc::default();
    for label in ["a", "b"] {
        script.lock().unwrap().insert(label, roomy());
    }
    let (usage_url, usage_hits) = spawn_counted_usage(script).await;
    let broker = auto_broker(&usage_url);
    let until = agent_core::epoch_millis() + 3_600_000;
    broker
        .report_cooldown(&anthropic("a"), Some(until), "anthropic_quota_exhausted")
        .await
        .unwrap();

    assert_eq!(turn(&broker).await, anthropic("b"));
    assert_eq!(usage_hits.load(Ordering::SeqCst), 1, "only b is read");
    let err = lookup_without_b(&broker)
        .await
        .expect_err("a is cooling down");
    assert!(
        matches!(err, BrokerError::NoAccountAvailable { .. }),
        "{err:?}"
    );
    let plan = broker.plan(OAuthProviderId::Anthropic, None).await.unwrap();
    assert_eq!(plan.rows[0].credential, anthropic("b"), "{plan:?}");
    assert_eq!(
        usage_hits.load(Ordering::SeqCst),
        1,
        "a is never read while cooling (b's reading is cached)"
    );
    assert!(account_state(&broker, "a").get("last_usage").is_none());

    broker
        .auth_state(&AuthStateQuery {
            account: Some("a".into()),
            clear: true,
            ..anthropic_query()
        })
        .unwrap();
    assert_eq!(
        lookup_without_b(&broker).await.expect("a serves again"),
        anthropic("a")
    );
    assert_eq!(usage_hits.load(Ordering::SeqCst), 2, "a is read again");
}

/// S4: a login without a provider account id has a seat derived from its
/// token, so a refresh changes it; its cooldown is kept under the slot too
/// and survives the rotation: `a` stays cooling, is neither chosen nor
/// read, and `auth state` still shows the cooldown. It also survives the
/// login gaining an account id (`auth identify`); `auth state --clear`
/// drops it. (A login without an id can't be told apart from a new login
/// by another account: a_new_login_into_the_label_inherits_the_cooldown_of_a_login_without_an_id.)
#[tokio::test]
#[serial_test::serial]
async fn a_cooldown_survives_a_token_rotation_of_a_login_without_an_account_id() {
    let home = install_accounts(&[("a", FAR_FUTURE), ("b", FAR_FUTURE)]);
    let script: Script = Arc::default();
    for label in ["a", "b"] {
        script.lock().unwrap().insert(label, roomy());
    }
    let (usage_url, usage_hits) = spawn_counted_usage(script).await;
    let broker = auto_broker(&usage_url);
    let until = agent_core::epoch_millis() + 3_600_000;
    broker
        .report_cooldown(&anthropic("a"), Some(until), "anthropic_quota_exhausted")
        .await
        .unwrap();
    // Another process refreshes a's login: new access and refresh token.
    store_tokens(
        &home,
        "a",
        "rotated-anthropic-a-access",
        "refresh-anth-a-rotated-SECRET",
        FAR_FUTURE,
    );

    assert_eq!(account_state(&broker, "a")["cooldown"]["until_ms"], until);
    assert_eq!(broker.active_cooldown(&anthropic("a")), Some(until));
    assert_eq!(broker.active_cooldown(&anthropic("b")), None);
    assert_eq!(turn(&broker).await, anthropic("b"));
    assert!(
        matches!(
            lookup_without_b(&broker).await,
            Err(BrokerError::NoAccountAvailable { .. })
        ),
        "a is still cooling down"
    );
    assert_eq!(usage_hits.load(Ordering::SeqCst), 1, "a is not read");

    // The same login gains its account id.
    let path = home.path().join("auth.json");
    let mut root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    root["anthropic@a"]["accountId"] = json!(ACCOUNT_ID_A);
    std::fs::write(&path, root.to_string()).unwrap();
    assert_eq!(broker.active_cooldown(&anthropic("a")), Some(until));

    broker
        .auth_state(&AuthStateQuery {
            account: Some("a".into()),
            clear: true,
            ..anthropic_query()
        })
        .unwrap();
    assert_eq!(broker.active_cooldown(&anthropic("a")), None);
    assert_eq!(
        lookup_without_b(&broker).await.expect("a serves again"),
        anthropic("a")
    );
}

/// Log `label` in again: new tokens, and `account_id` (None: no id).
fn relogin(home: &tempfile::TempDir, label: &str, tag: &str, account_id: Option<&str>) {
    store_tokens(
        home,
        label,
        &format!("{tag}-anthropic-{label}-access"),
        &format!("refresh-anth-{label}-{tag}-SECRET"),
        FAR_FUTURE,
    );
    let path = home.path().join("auth.json");
    let mut root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let entry = root[format!("anthropic@{label}")].as_object_mut().unwrap();
    match account_id {
        Some(id) => entry.insert("accountId".into(), json!(id)),
        None => entry.remove("accountId"),
    };
    std::fs::write(&path, root.to_string()).unwrap();
}

/// S4 review: the slot can't tell "the same account gained an id" from
/// "another account logged in". A login WITHOUT an account id cools down;
/// a new login into that label by a different account (with an id)
/// inherits the cooldown through the slot until `auth state --clear` frees
/// it: it errs toward not using the account. Contrast: the cooldown of a
/// login WITH an id is not inherited by a login with another id.
#[tokio::test]
#[serial_test::serial]
async fn a_new_login_into_the_label_inherits_the_cooldown_of_a_login_without_an_id() {
    const OTHER_ACCOUNT_ID: &str = "9a8b7c6d-5e4f-4a2b-8c1d-0e9f8a7b6c5d";
    let home = install_accounts(&[("a", FAR_FUTURE), ("b", FAR_FUTURE)]);
    let script: Script = Arc::default();
    for label in ["a", "b"] {
        script.lock().unwrap().insert(label, roomy());
    }
    let broker = auto_broker(&spawn_usage(script).await);
    let until = agent_core::epoch_millis() + 3_600_000;
    broker
        .report_cooldown(&anthropic("a"), Some(until), "anthropic_quota_exhausted")
        .await
        .unwrap();

    relogin(&home, "a", "other", Some(OTHER_ACCOUNT_ID));
    assert_eq!(broker.active_cooldown(&anthropic("a")), Some(until));
    assert_eq!(account_state(&broker, "a")["cooldown"]["until_ms"], until);
    assert!(
        matches!(
            lookup_without_b(&broker).await,
            Err(BrokerError::NoAccountAvailable { .. })
        ),
        "the new login of a is still cooling down"
    );

    broker
        .auth_state(&AuthStateQuery {
            account: Some("a".into()),
            clear: true,
            ..anthropic_query()
        })
        .unwrap();
    assert_eq!(broker.active_cooldown(&anthropic("a")), None);
    assert_eq!(
        lookup_without_b(&broker)
            .await
            .expect("a serves after the clear"),
        anthropic("a")
    );

    // Contrast: this login has an id; a login with another id starts clean.
    broker
        .report_cooldown(&anthropic("a"), Some(until), "anthropic_quota_exhausted")
        .await
        .unwrap();
    assert_eq!(broker.active_cooldown(&anthropic("a")), Some(until));
    relogin(&home, "a", "third", Some(ACCOUNT_ID_A));
    assert_eq!(broker.active_cooldown(&anthropic("a")), None);
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

/// The auth lines these tests read reach the daemon log through its own
/// filter (`capture_subscriber`), and that capture is not a pass-through:
/// in the same subscriber a WARN at a target outside the crate directives
/// is dropped, while an ERROR there is kept (the unset-`RUST_LOG`
/// default). Also the one test that reads the `usage read needs action`
/// WARN, which tells the user what to fix for an unreadable stored login.
#[tokio::test]
#[serial_test::serial]
async fn auth_log_lines_pass_the_daemon_log_filter_and_a_foreign_warn_does_not() {
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
    {
        let _guard = tracing::subscriber::set_default(capture_subscriber(&capture));
        broker
            .access_token_pinned(OAuthProviderId::Anthropic)
            .await
            .unwrap_err();
        tracing::warn!(target: "unrelated_crate::x", "foreign_warn_control");
        tracing::error!(target: "unrelated_crate::x", "foreign_error_control");
    }
    let log = capture.take();
    assert!(
        selection_warn(&log).contains("kind=action_required"),
        "{log}"
    );
    let action: Vec<_> = log
        .lines()
        .filter(|l| l.contains("usage read needs action"))
        .collect();
    assert_eq!(action.len(), 1, "{log}");
    assert!(
        action[0].contains("WARN")
            && action[0].contains("credential=anthropic@b")
            && action[0].contains("kind=credential_store"),
        "{log}"
    );
    assert!(!log.contains("foreign_warn_control"), "{log}");
    assert!(log.contains("foreign_error_control"), "{log}");
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

/// A selection reads `a`'s usage (429) and `b`'s (ok) and b serves; then b
/// is reported cooling down and a second selection fails (a's 429 is held,
/// b is not read while cooling). The report shows the cooldown with its
/// reason, each read's kind, HTTP status and time, and the failed
/// selection. Building it makes no usage request, and neither its JSON nor
/// its text carries token material, the email, the account id or its
/// prefix, or a seat identity.
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
    let served = broker
        .access_token_pinned(OAuthProviderId::Anthropic)
        .await
        .expect("b has capacity");
    assert_eq!(served.credential, anthropic("b"));
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
    assert_eq!(reads, 2, "one usage read per account, none while held or cooling");

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

// ── S2: failover across accounts ────────────────────────────────────────────

const INVALID_GRANT: &str =
    r#"{"error":"invalid_grant","error_description":"Refresh token revoked"}"#;

/// Bound for a selection that must terminate (a re-selection loop would
/// otherwise hang the test instead of failing it).
const SELECTION_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// Mark `label`'s stored access token expired, keeping both tokens and so
/// its seat: a cached reading still applies, and vending it must refresh.
fn expire_stored(home: &tempfile::TempDir, label: &str) {
    let path = home.path().join("auth.json");
    let mut root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    root[format!("anthropic@{label}")]["expires"] = json!(EXPIRED);
    std::fs::write(&path, root.to_string()).unwrap();
}

/// A request that already used some accounts asks for another: the broker
/// selects among the rest and never vends one it was told to avoid.
#[tokio::test]
#[serial_test::serial]
async fn excluding_accounts_a_request_already_used_never_vends_one_of_them() {
    let _home = install_accounts(&[("a", FAR_FUTURE), ("b", FAR_FUTURE), ("c", FAR_FUTURE)]);
    let script: Script = Arc::default();
    for label in ["a", "b", "c"] {
        script.lock().unwrap().insert(label, roomy());
    }
    let broker = auto_broker(&spawn_usage(script).await);
    let p = OAuthProviderId::Anthropic;

    let first = broker
        .access_token_pinned_excluding(p, None, &[anthropic("a")])
        .await
        .expect("b and c have capacity");
    assert_ne!(first.credential, anthropic("a"));
    let tried = [anthropic("a"), first.credential.clone()];
    let second = broker
        .access_token_pinned_excluding(p, None, &tried)
        .await
        .expect("one account is left");
    assert!(
        !tried.contains(&second.credential),
        "{:?}",
        second.credential
    );
    let all = [anthropic("a"), anthropic("b"), anthropic("c")];
    let err = broker
        .access_token_pinned_excluding(p, None, &all)
        .await
        .unwrap_err();
    assert!(
        matches!(err, BrokerError::NoAccountAvailable { .. }),
        "{err:?}"
    );
}

/// The accounts a request used were reported cooling down: the failure of
/// a lookup that avoids them still says they are cooling down (with their
/// times), not merely "excluded".
#[tokio::test]
#[serial_test::serial]
async fn attempted_accounts_that_are_cooling_down_stay_cooling_down_in_the_failure() {
    let _home = install_accounts(&[("a", FAR_FUTURE), ("b", FAR_FUTURE)]);
    let script: Script = Arc::default();
    for label in ["a", "b"] {
        script.lock().unwrap().insert(label, roomy());
    }
    let broker = auto_broker(&spawn_usage(script).await);
    let now = agent_core::epoch_millis();
    let (until_a, until_b) = (now + 3_600_000, now + 2 * 3_600_000);
    for (label, until) in [("a", until_a), ("b", until_b)] {
        broker
            .report_cooldown(&anthropic(label), Some(until), "anthropic_quota_exhausted")
            .await
            .unwrap();
    }

    let err = broker
        .access_token_pinned_excluding(
            OAuthProviderId::Anthropic,
            None,
            &[anthropic("a"), anthropic("b")],
        )
        .await
        .unwrap_err();
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
                {"account": "a", "kind": "cooling_down", "until_ms": until_a, "cooling_down": true},
                {"account": "b", "kind": "cooling_down", "until_ms": until_b, "cooling_down": true},
            ],
            "earliest_reset_ms": until_a,
        })
    );
}

/// S2 (ruled: any refresh kind): a refused refresh of the automatically
/// chosen account no longer ends the selection while another account has
/// proven capacity. The chosen account counts as failed, with its refresh
/// kind, for the rest of that selection: one refresh attempt, one WARN
/// naming it, no selection failure, and the other account serves.
#[tokio::test]
#[serial_test::serial]
async fn a_refused_refresh_of_the_chosen_account_moves_the_selection_to_another() {
    let home = install_accounts(&[("a", FAR_FUTURE), ("b", FAR_FUTURE)]);
    let script: Script = Arc::default();
    for label in ["a", "b"] {
        script.lock().unwrap().insert(label, roomy());
    }
    let (token_url, token_hits) = spawn_token(400, INVALID_GRANT).await;
    let broker =
        auto_broker(&spawn_usage(script).await).with_anthropic_token_url_for_tests(token_url);
    let p = OAuthProviderId::Anthropic;
    let first = broker.access_token_pinned(p).await.unwrap();
    assert_eq!(first.credential, anthropic("a"));
    // a's reading stays cached; its token expires before the next vend.
    expire_stored(&home, "a");

    let capture = Capture::default();
    let second = {
        let _guard = tracing::subscriber::set_default(capture_subscriber(&capture));
        tokio::time::timeout(SELECTION_DEADLINE, broker.access_token_pinned(p))
            .await
            .expect("the selection terminates")
    };
    let second = second.expect("b has proven capacity");
    assert_eq!(second.credential, anthropic("b"));
    assert_eq!(
        token_hits.load(Ordering::SeqCst),
        1,
        "one refresh attempt, for a"
    );
    let log = capture.take();
    let refused: Vec<_> = log
        .lines()
        .filter(|l| l.contains("token refresh failed"))
        .collect();
    assert_eq!(refused.len(), 1, "{log}");
    assert!(
        refused[0].contains("WARN")
            && refused[0].contains("credential=anthropic@a")
            && refused[0].contains("kind=refresh_rejected"),
        "{log}"
    );
    assert!(
        !log.contains("auth_selection_failed"),
        "the selection did not fail: {log}"
    );
    assert_secret_free(&log);
    // `auth state --account a` says why a was passed over, without another
    // exchange.
    let query = AuthStateQuery {
        account: Some("a".into()),
        ..anthropic_query()
    };
    let report = serde_json::to_value(broker.auth_state(&query).unwrap()).unwrap();
    let a = report["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["label"] == "a")
        .unwrap_or_else(|| panic!("no a in {report}"))
        .clone();
    assert_eq!(
        a["last_refresh"]["failure"],
        json!({"kind": "refresh_rejected", "http_status": 400}),
        "{a}"
    );
    assert_eq!(token_hits.load(Ordering::SeqCst), 1);
    assert_secret_free(&report.to_string());
}

/// S2 (ruled: ANY refresh kind, not only a refused login): a transient
/// refresh failure of the chosen account (the token endpoint answers 503)
/// also moves the selection to another account with proven capacity. One
/// refresh attempt, one WARN with the transient kind, no selection failure.
#[tokio::test]
#[serial_test::serial]
async fn a_transient_refresh_failure_of_the_chosen_account_also_moves_the_selection() {
    let home = install_accounts(&[("a", FAR_FUTURE), ("b", FAR_FUTURE)]);
    let script: Script = Arc::default();
    for label in ["a", "b"] {
        script.lock().unwrap().insert(label, roomy());
    }
    let (token_url, token_hits) = spawn_token(503, r#"{"error":"overloaded"}"#).await;
    let broker =
        auto_broker(&spawn_usage(script).await).with_anthropic_token_url_for_tests(token_url);
    let p = OAuthProviderId::Anthropic;
    assert_eq!(
        broker.access_token_pinned(p).await.unwrap().credential,
        anthropic("a")
    );
    expire_stored(&home, "a");

    let capture = Capture::default();
    let second = {
        let _guard = tracing::subscriber::set_default(capture_subscriber(&capture));
        tokio::time::timeout(SELECTION_DEADLINE, broker.access_token_pinned(p))
            .await
            .expect("the selection terminates")
    };
    let second = second.expect("b has proven capacity");
    assert_eq!(second.credential, anthropic("b"));
    assert_eq!(
        token_hits.load(Ordering::SeqCst),
        1,
        "one refresh attempt, for a"
    );
    let log = capture.take();
    let failed: Vec<_> = log
        .lines()
        .filter(|l| l.contains("token refresh failed"))
        .collect();
    assert_eq!(failed.len(), 1, "{log}");
    assert!(
        failed[0].contains("WARN")
            && failed[0].contains("credential=anthropic@a")
            && failed[0].contains("kind=refresh_transient"),
        "{log}"
    );
    assert!(
        !log.contains("auth_selection_failed"),
        "the selection did not fail: {log}"
    );
    assert_secret_free(&log);
}

/// Every chosen account's refresh is refused: the selection ends (no loop)
/// with the classified failure, each account keeping its refresh kind, both
/// labels needing a login and no reset time to wait for. One refresh per
/// account, one selection WARN.
#[tokio::test]
#[serial_test::serial]
async fn every_chosen_accounts_refused_refresh_ends_the_selection_naming_each_login() {
    let home = install_accounts(&[("a", FAR_FUTURE), ("b", FAR_FUTURE)]);
    let script: Script = Arc::default();
    for label in ["a", "b"] {
        script.lock().unwrap().insert(label, roomy());
    }
    let (token_url, token_hits) = spawn_token(400, INVALID_GRANT).await;
    let broker =
        auto_broker(&spawn_usage(script).await).with_anthropic_token_url_for_tests(token_url);
    let p = OAuthProviderId::Anthropic;
    assert_eq!(
        broker.access_token_pinned(p).await.unwrap().credential,
        anthropic("a")
    );
    expire_stored(&home, "a");
    expire_stored(&home, "b");

    let capture = Capture::default();
    let result = {
        let _guard = tracing::subscriber::set_default(capture_subscriber(&capture));
        tokio::time::timeout(SELECTION_DEADLINE, broker.access_token_pinned(p))
            .await
            .expect("the selection terminates")
    };
    let err = result.unwrap_err();
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
            "kind": "credential_rejected",
            "accounts": [
                {"account": "a", "kind": "refresh_rejected", "http_status": 400},
                {"account": "b", "kind": "refresh_rejected", "http_status": 400},
            ],
        })
    );
    assert_eq!(failure.login_labels(), vec!["a", "b"]);
    assert_eq!(
        token_hits.load(Ordering::SeqCst),
        2,
        "one refresh per account"
    );
    let log = capture.take();
    let warn = selection_warn(&log);
    assert!(warn.contains("kind=credential_rejected"), "{warn}");
    assert!(
        warn.contains("accounts=a:refresh_rejected:400,b:refresh_rejected:400"),
        "{warn}"
    );
    assert_secret_free(&log);
    assert_secret_free(&err.to_string());
}

/// A re-selection after the chosen account's refused refresh reuses the
/// selection's earlier failures: c's failed usage read is not repeated (a
/// and b come from the reading cache). c's read is refused (401), not
/// transient: a transient failure is held by the usage-read table anyway,
/// so only a refused read shows the selection's own memory.
#[tokio::test]
#[serial_test::serial]
async fn a_reselection_does_not_repeat_an_earlier_failed_read() {
    let home = install_accounts(&[("a", FAR_FUTURE), ("b", FAR_FUTURE), ("c", FAR_FUTURE)]);
    let script: Script = Arc::default();
    script.lock().unwrap().insert("a", roomy());
    script.lock().unwrap().insert("b", roomy());
    script.lock().unwrap().insert("c", Reply::Status(401));
    let (usage_url, usage_hits) = spawn_counted_usage(script).await;
    let (token_url, token_hits) = spawn_token(400, INVALID_GRANT).await;
    let broker = auto_broker(&usage_url).with_anthropic_token_url_for_tests(token_url);
    let p = OAuthProviderId::Anthropic;
    assert_eq!(
        broker.access_token_pinned(p).await.unwrap().credential,
        anthropic("a")
    );
    expire_stored(&home, "a");
    let before = usage_hits.load(Ordering::SeqCst);

    let second = tokio::time::timeout(SELECTION_DEADLINE, broker.access_token_pinned(p))
        .await
        .expect("the selection terminates")
        .expect("b has proven capacity");
    assert_eq!(second.credential, anthropic("b"));
    assert_eq!(
        usage_hits.load(Ordering::SeqCst) - before,
        1,
        "c is read once in this selection"
    );
    assert_eq!(token_hits.load(Ordering::SeqCst), 1);
}

// ── S2: the refused-refresh hold ────────────────────────────────────────────

/// The hold window the ruling fixes (15 min).
const HOLD_MS: u64 = 15 * 60 * 1000;

/// a and b have capacity and a served the first turn; then a's token
/// expires (same tokens, so its reading stays cached and vending it must
/// refresh). The token endpoint answers every refresh with `status`/`body`.
async fn a_served_then_expired(
    status: u16,
    body: &'static str,
) -> (tempfile::TempDir, LocalBroker, Arc<AtomicUsize>) {
    let home = install_accounts(&[("a", FAR_FUTURE), ("b", FAR_FUTURE)]);
    let script: Script = Arc::default();
    for label in ["a", "b"] {
        script.lock().unwrap().insert(label, roomy());
    }
    let (token_url, token_hits) = spawn_token(status, body).await;
    let broker =
        auto_broker(&spawn_usage(script).await).with_anthropic_token_url_for_tests(token_url);
    assert_eq!(turn(&broker).await, anthropic("a"));
    expire_stored(&home, "a");
    (home, broker, token_hits)
}

/// One automatic Anthropic vend that terminates and succeeds: the account
/// that serves.
async fn turn(broker: &LocalBroker) -> CredentialRef {
    tokio::time::timeout(
        SELECTION_DEADLINE,
        broker.access_token_pinned(OAuthProviderId::Anthropic),
    )
    .await
    .expect("the selection terminates")
    .expect("an account serves")
    .credential
}

/// A failover lookup of a request that already used b (stickiness can't
/// keep b): only a is left.
async fn lookup_without_b(broker: &LocalBroker) -> Result<CredentialRef, BrokerError> {
    tokio::time::timeout(
        SELECTION_DEADLINE,
        broker.access_token_pinned_excluding(OAuthProviderId::Anthropic, None, &[anthropic("b")]),
    )
    .await
    .expect("the selection terminates")
    .map(|t| t.credential)
}

/// The lookup failed because a needs a login, a refused refresh.
fn assert_a_needs_login(result: Result<CredentialRef, BrokerError>) -> BrokerError {
    let err = result.expect_err("a is the only account left and its login is refused");
    let BrokerError::NoAccountAvailable {
        failure: Some(failure),
        ..
    } = &err
    else {
        panic!("typed selection failure expected: {err:?}");
    };
    assert_eq!(failure.login_labels(), vec!["a"], "{failure:?}");
    let a = failure.accounts.iter().find(|f| f.account == "a").unwrap();
    assert_eq!(
        serde_json::to_value(a).unwrap(),
        json!({"account": "a", "kind": "refresh_rejected", "http_status": 400})
    );
    err
}

/// `label`'s entry in the `auth state` report (JSON).
fn account_state(broker: &LocalBroker, label: &str) -> serde_json::Value {
    let report = serde_json::to_value(broker.auth_state(&anthropic_query()).unwrap()).unwrap();
    report["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["label"] == label)
        .unwrap_or_else(|| panic!("no {label} in {report}"))
        .clone()
}

/// Rewrite `label`'s stored tokens; the access token still names the
/// account for the usage mock.
fn store_tokens(home: &tempfile::TempDir, label: &str, access: &str, refresh: &str, expires: u64) {
    let path = home.path().join("auth.json");
    let mut root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let entry = &mut root[format!("anthropic@{label}")];
    entry["access"] = json!(access);
    entry["refresh"] = json!(refresh);
    entry["expires"] = json!(expires);
    std::fs::write(&path, root.to_string()).unwrap();
}

/// S2 hold (2a): a refused refresh of the chosen account is held. Inside the
/// window, with the stored login unchanged, a lookup that has only a left
/// sends no token request and still names a's login; `auth state` shows the
/// held kind, when it was refused and until when, and its text says it is
/// held until the stored login changes.
#[tokio::test]
#[serial_test::serial]
async fn a_refused_refresh_is_held_and_later_lookups_ask_the_provider_nothing() {
    let (_home, broker, token_hits) = a_served_then_expired(400, INVALID_GRANT).await;
    let before = agent_core::epoch_millis();
    assert_eq!(turn(&broker).await, anthropic("b"));
    let after = agent_core::epoch_millis();
    assert_eq!(token_hits.load(Ordering::SeqCst), 1, "a is refreshed once");

    for _ in 0..2 {
        assert_a_needs_login(lookup_without_b(&broker).await);
        assert_eq!(turn(&broker).await, anthropic("b"));
    }
    assert_eq!(
        token_hits.load(Ordering::SeqCst),
        1,
        "no refresh of a while its refusal is held"
    );

    let a = account_state(&broker, "a");
    let hold = &a["refresh_hold"];
    assert_eq!(
        hold["failure"],
        json!({"kind": "refresh_rejected", "http_status": 400}),
        "{a}"
    );
    let refused_at = hold["refused_at_ms"].as_u64().expect("refused_at_ms");
    assert!((before..=after).contains(&refused_at), "{a}");
    assert_eq!(hold["until_ms"].as_u64(), Some(refused_at + HOLD_MS), "{a}");
    assert!(account_state(&broker, "b").get("refresh_hold").is_none());
    let text = render_text(&broker.auth_state(&anthropic_query()).unwrap());
    assert!(
        text.contains("    held      refresh refused (refresh_rejected, HTTP 400), "),
        "{text}"
    );
    assert!(
        text.contains("; held until the stored login changes or "),
        "{text}"
    );
    assert_eq!(token_hits.load(Ordering::SeqCst), 1);
}

/// S2 hold (2b): a change of the stored login ends the hold. A new login
/// (other tokens, still expired) is refreshed once, and refused again it is
/// held again. A refresh by another process (a new access token with the
/// same refresh token) ends it too: a serves with no request of ours.
#[tokio::test]
#[serial_test::serial]
async fn a_changed_stored_login_ends_the_hold() {
    let (home, broker, token_hits) = a_served_then_expired(400, INVALID_GRANT).await;
    assert_eq!(turn(&broker).await, anthropic("b"));
    assert_a_needs_login(lookup_without_b(&broker).await);
    assert_eq!(token_hits.load(Ordering::SeqCst), 1);

    store_tokens(
        &home,
        "a",
        "relogin-anthropic-a-access",
        "refresh-anth-a-SECRET-relogin",
        EXPIRED,
    );
    assert_a_needs_login(lookup_without_b(&broker).await);
    assert_eq!(
        token_hits.load(Ordering::SeqCst),
        2,
        "the new login is refreshed once"
    );
    assert_a_needs_login(lookup_without_b(&broker).await);
    assert_eq!(
        token_hits.load(Ordering::SeqCst),
        2,
        "refused again, it is held again"
    );

    store_tokens(
        &home,
        "a",
        "elsewhere-anthropic-a-access",
        "refresh-anth-a-SECRET-relogin",
        FAR_FUTURE,
    );
    assert_eq!(
        lookup_without_b(&broker).await.expect("a's login works again"),
        anthropic("a")
    );
    assert_eq!(token_hits.load(Ordering::SeqCst), 2);
    assert!(account_state(&broker, "a").get("refresh_hold").is_none());
}

/// S2 hold (2c): a retryable refresh failure (the token endpoint answers
/// 503) is not held: the next lookup refreshes again.
#[tokio::test]
#[serial_test::serial]
async fn a_retryable_refresh_failure_is_not_held() {
    let (_home, broker, token_hits) =
        a_served_then_expired(503, r#"{"error":"overloaded"}"#).await;
    assert_eq!(turn(&broker).await, anthropic("b"));
    assert_eq!(token_hits.load(Ordering::SeqCst), 1);
    assert!(account_state(&broker, "a").get("refresh_hold").is_none());

    lookup_without_b(&broker)
        .await
        .expect_err("a's refresh fails again");
    assert_eq!(
        token_hits.load(Ordering::SeqCst),
        2,
        "a transient failure is retried"
    );
    assert!(account_state(&broker, "a").get("refresh_hold").is_none());
}

/// S2 hold (2d): `auth state --clear` ends the hold, for the account and
/// provider-wide; each clear lets exactly one refresh through.
#[tokio::test]
#[serial_test::serial]
async fn auth_state_clear_ends_the_hold() {
    let (_home, broker, token_hits) = a_served_then_expired(400, INVALID_GRANT).await;
    assert_eq!(turn(&broker).await, anthropic("b"));
    assert_a_needs_login(lookup_without_b(&broker).await);
    assert_eq!(token_hits.load(Ordering::SeqCst), 1, "held before the clear");

    broker
        .auth_state(&AuthStateQuery {
            account: Some("a".into()),
            clear: true,
            ..anthropic_query()
        })
        .unwrap();
    assert_a_needs_login(lookup_without_b(&broker).await);
    assert_eq!(token_hits.load(Ordering::SeqCst), 2, "account clear");
    assert_a_needs_login(lookup_without_b(&broker).await);
    assert_eq!(token_hits.load(Ordering::SeqCst), 2, "held again");

    broker
        .auth_state(&AuthStateQuery {
            clear: true,
            ..anthropic_query()
        })
        .unwrap();
    assert_a_needs_login(lookup_without_b(&broker).await);
    assert_eq!(token_hits.load(Ordering::SeqCst), 3, "provider-wide clear");
}

/// S2 hold (2e): the hold is bounded. One second before the window ends no
/// request; after it exactly one refresh, and a new refusal holds again.
#[tokio::test]
#[serial_test::serial]
async fn the_hold_ends_after_its_window_with_one_retry() {
    let (_home, broker, token_hits) = a_served_then_expired(400, INVALID_GRANT).await;
    assert_eq!(turn(&broker).await, anthropic("b"));
    assert_eq!(token_hits.load(Ordering::SeqCst), 1);

    broker.age_refresh_holds_for_tests(std::time::Duration::from_millis(HOLD_MS - 10_000));
    assert_a_needs_login(lookup_without_b(&broker).await);
    assert_eq!(token_hits.load(Ordering::SeqCst), 1, "inside the window");

    broker.age_refresh_holds_for_tests(std::time::Duration::from_secs(20));
    assert_a_needs_login(lookup_without_b(&broker).await);
    assert_eq!(token_hits.load(Ordering::SeqCst), 2, "one retry after it");
    assert_a_needs_login(lookup_without_b(&broker).await);
    assert_eq!(token_hits.load(Ordering::SeqCst), 2, "refused again: held");
}

/// S2 hold, the usage read's refresh: a's token is expired from the start,
/// so reading its usage refreshes first, and the provider refuses. The
/// refusal is held at this site too: later lookups and `usage` send neither
/// a token request nor a usage request for a, and a held read is not
/// recorded as a new read.
#[tokio::test]
#[serial_test::serial]
async fn a_refused_refresh_before_a_usage_read_is_held_too() {
    let _home = install_accounts(&[("a", EXPIRED), ("b", FAR_FUTURE)]);
    let script: Script = Arc::default();
    for label in ["a", "b"] {
        script.lock().unwrap().insert(label, roomy());
    }
    let (usage_url, usage_hits) = spawn_counted_usage(script).await;
    let (token_url, token_hits) = spawn_token(400, INVALID_GRANT).await;
    let broker = auto_broker(&usage_url).with_anthropic_token_url_for_tests(token_url);
    assert_eq!(turn(&broker).await, anthropic("b"));
    assert_eq!(token_hits.load(Ordering::SeqCst), 1, "a's refresh, refused");
    assert_eq!(usage_hits.load(Ordering::SeqCst), 1, "only b's usage is read");
    let read_at = account_state(&broker, "a")["last_usage"]["at_ms"].clone();
    assert!(read_at.is_u64(), "a's failed read is recorded");

    for _ in 0..2 {
        assert_a_needs_login(lookup_without_b(&broker).await);
    }
    broker
        .usage(&anthropic("a"))
        .await
        .expect_err("a's login is refused");
    assert_eq!(token_hits.load(Ordering::SeqCst), 1, "no refresh while held");
    assert_eq!(usage_hits.load(Ordering::SeqCst), 1, "no usage request for a");
    let a = account_state(&broker, "a");
    assert_eq!(a["last_usage"]["at_ms"], read_at, "held reads are no reads: {a}");
    assert_eq!(
        a["refresh_hold"]["failure"],
        json!({"kind": "refresh_rejected", "http_status": 400}),
        "{a}"
    );
}

/// S2 hold, no secrets (ruling item 3): the digest a hold keeps of the
/// stored login appears in no `auth state` JSON or text, no log line and no
/// error, and neither does any token.
#[tokio::test]
#[serial_test::serial]
async fn a_held_refusal_shows_no_digest_or_token_anywhere() {
    let (_home, broker, _token_hits) = a_served_then_expired(400, INVALID_GRANT).await;
    let capture = Capture::default();
    let err = {
        let _guard = tracing::subscriber::set_default(capture_subscriber(&capture));
        assert_eq!(turn(&broker).await, anthropic("b"));
        assert_a_needs_login(lookup_without_b(&broker).await)
    };
    let digest = broker
        .refresh_hold_digest_for_tests(&anthropic("a"))
        .expect("a's refusal is held");
    assert_eq!(digest.len(), 64, "a sha256 hex digest");
    let report = broker.auth_state(&anthropic_query()).unwrap();
    let json = serde_json::to_value(&report).unwrap().to_string();
    assert!(json.contains("refresh_hold"), "{json}");
    for out in [
        json,
        render_text(&report),
        capture.take(),
        err.to_string(),
        format!("{err:?}"),
    ] {
        assert_secret_free(&out);
        assert!(!out.contains(&digest), "the hold's digest in {out}");
        assert!(!out.contains(&digest[..16]), "a digest prefix in {out}");
    }
}

/// Loopback token endpoint that refuses every refresh (400 invalid_grant)
/// and holds each answer while `closed` is set, so callers of one slot
/// queue behind an exchange in flight.
struct HeldToken {
    url: String,
    hits: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
}

async fn spawn_held_token() -> HeldToken {
    let hits = Arc::new(AtomicUsize::new(0));
    let closed = Arc::new(AtomicBool::new(true));
    let (counter, held) = (hits.clone(), closed.clone());
    let app = Router::new().route(
        "/v1/oauth/token",
        axum::routing::post(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            let held = held.clone();
            async move {
                while held.load(Ordering::SeqCst) {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                (
                    StatusCode::BAD_REQUEST,
                    [("content-type", "application/json")],
                    INVALID_GRANT,
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
    HeldToken {
        url: format!("http://{addr}/v1/oauth/token"),
        hits,
        closed,
    }
}

impl HeldToken {
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    /// Wait until `n` refresh requests have arrived.
    async fn arrived(&self, n: usize) {
        let deadline = tokio::time::Instant::now() + SELECTION_DEADLINE;
        while self.hits() < n {
            assert!(tokio::time::Instant::now() < deadline, "no refresh request {n}");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// Once `n` requests have arrived, give the callers behind the one in
    /// flight time to queue at the slot's gate, then answer everything.
    async fn release_after(&self, n: usize) {
        self.arrived(n).await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        self.closed.store(false, Ordering::SeqCst);
    }
}

/// s2-r1 blocker 1: four ordinary turns at once, over a warm reading of
/// sticky a whose token expired, all choose a while a's refresh is in
/// flight. The hold is decided under a's refresh gate: one token request,
/// and every turn that waited takes the held refusal and selects b. After
/// the window, four lookups at once that have only a left send ONE request
/// again, and all name a's login.
#[tokio::test]
#[serial_test::serial]
async fn concurrent_turns_send_one_refused_refresh_and_the_rest_take_the_hold() {
    let home = install_accounts(&[("a", FAR_FUTURE), ("b", FAR_FUTURE)]);
    let script: Script = Arc::default();
    for label in ["a", "b"] {
        script.lock().unwrap().insert(label, roomy());
    }
    let token = spawn_held_token().await;
    let broker = auto_broker(&spawn_usage(script).await)
        .with_anthropic_token_url_for_tests(token.url.clone());
    token.closed.store(false, Ordering::SeqCst);
    assert_eq!(turn(&broker).await, anthropic("a"));
    assert_eq!(token.hits(), 0, "a's token was fresh");
    token.closed.store(true, Ordering::SeqCst);
    expire_stored(&home, "a");

    let (t1, t2, t3, t4, ()) = tokio::join!(
        turn(&broker),
        turn(&broker),
        turn(&broker),
        turn(&broker),
        token.release_after(1),
    );
    for served in [t1, t2, t3, t4] {
        assert_eq!(served, anthropic("b"));
    }
    assert_eq!(token.hits(), 1, "one refresh of a for four turns");
    assert!(account_state(&broker, "a")["refresh_hold"].is_object());

    token.closed.store(true, Ordering::SeqCst);
    broker.age_refresh_holds_for_tests(std::time::Duration::from_millis(HOLD_MS + 1_000));
    let (l1, l2, l3, l4, ()) = tokio::join!(
        lookup_without_b(&broker),
        lookup_without_b(&broker),
        lookup_without_b(&broker),
        lookup_without_b(&broker),
        token.release_after(2),
    );
    for result in [l1, l2, l3, l4] {
        assert_a_needs_login(result);
    }
    assert_eq!(token.hits(), 2, "one retry after the window, not four");
}

/// s2-r1 backlog 1: the hold is of the login actually refused. While a's
/// refused refresh is in flight, a usage read of a queues at a's gate and
/// a's stored login is rotated (another process refreshed it to tokens
/// that are expired as well). The read re-reads the new login under the
/// gate; nothing holds it, so it is refreshed once and refused, and the
/// hold is for the new login: the next lookup asks the provider nothing.
#[tokio::test]
#[serial_test::serial]
async fn a_rotation_while_a_refresh_waits_holds_the_login_that_was_refused() {
    let home = install_accounts(&[("a", FAR_FUTURE), ("b", FAR_FUTURE)]);
    let script: Script = Arc::default();
    for label in ["a", "b"] {
        script.lock().unwrap().insert(label, roomy());
    }
    let token = spawn_held_token().await;
    let broker = auto_broker(&spawn_usage(script).await)
        .with_anthropic_token_url_for_tests(token.url.clone());
    assert_eq!(turn(&broker).await, anthropic("a"));
    expire_stored(&home, "a");

    let read = async {
        token.arrived(1).await;
        broker.usage(&anthropic("a")).await
    };
    let rotate = async {
        token.arrived(1).await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        store_tokens(
            &home,
            "a",
            "rotated-anthropic-a-access",
            "refresh-anth-a-SECRET-rotated",
            EXPIRED,
        );
        token.closed.store(false, Ordering::SeqCst);
    };
    let (looked_up, read, ()) = tokio::join!(lookup_without_b(&broker), read, rotate);
    assert_a_needs_login(looked_up);
    read.expect_err("the rotated login is refused too");
    assert_eq!(token.hits(), 2, "the old login and the rotated one");

    assert_a_needs_login(lookup_without_b(&broker).await);
    assert_eq!(token.hits(), 2, "the rotated login's refusal is held");
    assert_eq!(
        account_state(&broker, "a")["refresh_hold"]["failure"],
        json!({"kind": "refresh_rejected", "http_status": 400})
    );
}
