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
    TestClock, TokenCache,
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
const LABELS: [&str; 7] = ["a", "b", "c", "d", "e", "f", "g"];

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
    /// This reply, from a read that takes this many seconds on the test
    /// clock ([`spawn_clocked_usage`]).
    Slow(u64, Box<Reply>),
    /// This reply, held back while the flag is set (the request is logged
    /// when it arrives).
    Held(Arc<AtomicBool>, Box<Reply>),
}

type Script = Arc<Mutex<HashMap<&'static str, Reply>>>;

/// Usage mock keyed by the bearer's account (`a` / `b`).
async fn spawn_usage(script: Script) -> String {
    spawn_counted_usage(script).await.0
}

/// [`spawn_usage`] plus the number of requests it has answered.
async fn spawn_counted_usage(script: Script) -> (String, Arc<AtomicUsize>) {
    let (url, hits, _) = spawn_logged_usage(script).await;
    (url, hits)
}

/// The accounts the usage mock was asked about, in request order.
type ReadLog = Arc<Mutex<Vec<&'static str>>>;

/// [`spawn_counted_usage`] plus whose usage each request read.
async fn spawn_logged_usage(script: Script) -> (String, Arc<AtomicUsize>, ReadLog) {
    spawn_clocked_usage(script, None).await
}

/// [`spawn_logged_usage`] where a [`Reply::Slow`] read moves `clock` on by
/// its time before it answers.
async fn spawn_clocked_usage(
    script: Script,
    clock: Option<TestClock>,
) -> (String, Arc<AtomicUsize>, ReadLog) {
    let hits = Arc::new(AtomicUsize::new(0));
    let log = ReadLog::default();
    let (counter, reads) = (hits.clone(), log.clone());
    let app = Router::new().route(
        "/usage",
        get(move |headers: HeaderMap| {
            let script = script.clone();
            let reads = reads.clone();
            let clock = clock.clone();
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
                reads.lock().unwrap().push(account);
                let scripted = script.lock().unwrap().get(account).cloned();
                let reply = match scripted {
                    Some(Reply::Slow(secs, reply)) => {
                        if let Some(clock) = &clock {
                            clock.advance(std::time::Duration::from_secs(secs));
                        }
                        Some(*reply)
                    }
                    Some(Reply::Held(closed, reply)) => {
                        while closed.load(Ordering::SeqCst) {
                            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                        }
                        Some(*reply)
                    }
                    other => other,
                };
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
                    Some(Reply::Slow(..) | Reply::Held(..)) | None => {
                        StatusCode::NOT_FOUND.into_response()
                    }
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
    (format!("http://{addr}/usage"), hits, log)
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

/// The incident shape: the usage endpoint errors for one account and
/// rate-limits the other (second: a usage 429 pauses every later read). The
/// plan carries each account's cause; the error and the WARN say "usage read
/// failed", with each kind and HTTP status.
#[tokio::test]
#[serial_test::serial]
async fn usage_read_failures_keep_their_kind_status_and_retry_after() {
    let _home = install_store();
    let script: Script = Arc::default();
    script.lock().unwrap().insert("a", Reply::Status(503));
    script.lock().unwrap().insert("b", Reply::RateLimited(120));
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
        cause_of("b"),
        json!({"kind": "usage_rate_limited", "http_status": 429, "retry_after_s": 120}),
        "{selection}"
    );
    assert_eq!(
        cause_of("a"),
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
        warn.contains("accounts=a:usage_upstream:503,b:usage_rate_limited:429"),
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

/// Review fix 5: while a cooldown is active, a turn asks for its account's
/// cooldown before every request. The stored login is read once per
/// version of auth.json, not per lookup or twice per account per
/// selection; a write (a rotation) is read again, and the cooldown still
/// follows the login.
#[tokio::test]
#[serial_test::serial]
async fn cooldown_lookups_read_the_stored_login_once_per_version_of_auth_json() {
    let home = install_accounts(&[("a", FAR_FUTURE), ("b", FAR_FUTURE)]);
    let script: Script = Arc::default();
    for label in ["a", "b"] {
        script.lock().unwrap().insert(label, roomy());
    }
    let broker = auto_broker(&spawn_usage(script).await);
    // Nothing cooling: no read at all.
    assert_eq!(broker.active_cooldown(&anthropic("a")), None);
    assert_eq!(broker.cooldown_seat_loads_for_tests(), 0);

    let until = agent_core::epoch_millis() + 3_600_000;
    broker
        .report_cooldown(&anthropic("a"), Some(until), "anthropic_quota_exhausted")
        .await
        .unwrap();
    let after_report = broker.cooldown_seat_loads_for_tests();
    assert_eq!(after_report, 1, "a's login, read once");
    for _ in 0..10 {
        assert_eq!(broker.active_cooldown(&anthropic("a")), Some(until));
        assert_eq!(broker.active_cooldown(&anthropic("b")), None);
    }
    assert_eq!(broker.cooldown_seat_loads_for_tests(), 2, "plus b's, once");
    // A selection looks both accounts up without reading either again.
    assert_eq!(turn(&broker).await, anthropic("b"));
    broker.plan(OAuthProviderId::Anthropic, None).await.unwrap();
    assert_eq!(broker.cooldown_seat_loads_for_tests(), 2);

    // Another process rotates a's login: a new version, read again.
    std::thread::sleep(std::time::Duration::from_millis(5));
    store_tokens(
        &home,
        "a",
        "rotated-anthropic-a-access",
        "refresh-anth-a-rotated-SECRET",
        FAR_FUTURE,
    );
    assert_eq!(broker.active_cooldown(&anthropic("a")), Some(until));
    assert_eq!(broker.cooldown_seat_loads_for_tests(), 3);
    assert_eq!(broker.active_cooldown(&anthropic("a")), Some(until));
    assert_eq!(broker.cooldown_seat_loads_for_tests(), 3);
}

/// Review fix 1: `clear_cooldown` drops a cooldown only for a request sent
/// after it was reported (one sent before proves nothing about it), and
/// only the account's own.
#[tokio::test]
#[serial_test::serial]
async fn a_cooldown_is_cleared_only_by_a_request_sent_after_it() {
    let _home = install_accounts(&[("a", FAR_FUTURE), ("b", FAR_FUTURE)]);
    let broker = auto_broker(&spawn_usage(Arc::default()).await);
    let until = agent_core::epoch_millis() + 3_600_000;
    let before = agent_core::epoch_millis() - 1_000;
    for label in ["a", "b"] {
        broker
            .report_cooldown(&anthropic(label), Some(until), "anthropic_quota_exhausted")
            .await
            .unwrap();
    }
    broker.clear_cooldown(&anthropic("a"), before);
    assert_eq!(broker.active_cooldown(&anthropic("a")), Some(until), "sent before the report");
    broker.clear_cooldown(&anthropic("a"), agent_core::epoch_millis());
    assert_eq!(broker.active_cooldown(&anthropic("a")), None);
    assert_eq!(broker.active_cooldown(&anthropic("b")), Some(until), "b's stays");
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

/// A selection reads `a`'s usage (ok) and `b`'s (429) and a serves; then a
/// is reported cooling down and a second selection fails (b's 429 is held,
/// a is not read while cooling). The report shows the cooldown with its
/// reason, each read's kind, HTTP status and time, and the failed
/// selection. Building it makes no usage request, and neither its JSON nor
/// its text carries token material, the email, the account id or its
/// prefix, or a seat identity.
#[tokio::test]
#[serial_test::serial]
async fn auth_state_reports_memory_without_requests_or_secrets() {
    let _home = install_identified_store();
    let script: Script = Arc::default();
    script.lock().unwrap().insert(
        "a",
        Reply::Body(
            json!({"five_hour": {"utilization": 10.0, "resets_at": "2099-01-01T00:00:00Z"}}),
        ),
    );
    script.lock().unwrap().insert("b", Reply::RateLimited(120));
    let (usage_url, hits) = spawn_counted_usage(script).await;
    let broker = auto_broker(&usage_url);
    let served = broker
        .access_token_pinned(OAuthProviderId::Anthropic)
        .await
        .expect("a has capacity");
    assert_eq!(served.credential, anthropic("a"));
    let until = agent_core::epoch_millis() + 3_600_000;
    broker
        .report_cooldown(&anthropic("a"), Some(until), "anthropic_quota_exhausted")
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
        a["cooldown"],
        json!({"until_ms": until, "reason": "anthropic_quota_exhausted"}),
        "{a}"
    );
    assert!(a["last_usage"].get("failure").is_none(), "{a}");
    let read_at = a["last_usage"]["at_ms"]
        .as_u64()
        .expect("a's read is recorded");
    assert_eq!(a["last_usage_ok_ms"], read_at, "{a}");
    let b = account("b");
    assert_eq!(b["identity"], "rotating", "{b}");
    assert_eq!(
        b["last_usage"]["failure"],
        json!({"kind": "usage_rate_limited", "http_status": 429, "retry_after_s": 120}),
        "{b}"
    );
    assert!(b.get("last_usage_ok_ms").is_none(), "{b}");
    assert!(b.get("cooldown").is_none(), "{b}");
    assert!(read_at <= report.generated_at_ms && report.generated_at_ms - read_at < 60_000);
    let selection = &value["selections"][0];
    assert_eq!(selection["provider"], "anthropic", "{value}");
    assert_eq!(selection["failure"]["kind"], "usage_read_failed", "{value}");

    let text = render_text(&report);
    for needle in [
        "usage_rate_limited, HTTP 429, retry after 120s, ",
        " ago",
        "anthropic_quota_exhausted",
        "failed: usage_read_failed (a:cooling_down,b:usage_rate_limited:429)",
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

/// Review fix 3: a retryable refresh failure (here the token endpoint is
/// overloaded) of the chosen account does not move the selection: it says
/// nothing against the account, and moving would cost the turn its cache.
/// The selection fails with the refresh's own kind (not a selection
/// failure such as `usage_read_failed`) after one refresh attempt; b is
/// never vended.
#[tokio::test]
#[serial_test::serial]
async fn a_transient_refresh_failure_of_the_chosen_account_fails_without_moving() {
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
    let err = second.expect_err("a's refresh failed; b is not tried");
    let BrokerError::Refresh(f) = &err else {
        panic!("the refresh's own failure: {err:?}");
    };
    assert_eq!(f.kind, agent_core::auth::RefreshFailureKind::Transient, "{err:?}");
    assert!(err.to_string().contains("account 'a'"), "{err}");
    assert_eq!(token_hits.load(Ordering::SeqCst), 1, "one refresh attempt, for a");
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
    assert!(!log.contains("selecting again"), "{log}");
    assert_secret_free(&log);
    let state = serde_json::to_value(broker.auth_state(&anthropic_query()).unwrap()).unwrap();
    assert!(
        state.to_string().contains("refresh_transient"),
        "auth state records the vend failure: {state}"
    );
}

/// Review fix 3, one account: a served, then its token expired (its
/// reading stays cached, so the vend must refresh) and the token endpoint
/// is overloaded. The selection reports the refresh's own kind
/// (`refresh_transient`), not a selection failure (`usage_read_failed`).
#[tokio::test]
#[serial_test::serial]
async fn the_only_accounts_transient_refresh_failure_keeps_its_kind() {
    let home = install_accounts(&[("a", FAR_FUTURE)]);
    let script: Script = Arc::default();
    script.lock().unwrap().insert("a", roomy());
    let (token_url, token_hits) = spawn_token(503, r#"{"error":"overloaded"}"#).await;
    let broker =
        auto_broker(&spawn_usage(script).await).with_anthropic_token_url_for_tests(token_url);
    assert_eq!(turn(&broker).await, anthropic("a"));
    expire_stored(&home, "a");
    let err = tokio::time::timeout(
        SELECTION_DEADLINE,
        broker.access_token_pinned(OAuthProviderId::Anthropic),
    )
    .await
    .expect("the selection terminates")
    .expect_err("a's refresh fails");
    assert_eq!(token_hits.load(Ordering::SeqCst), 1);
    let BrokerError::Refresh(f) = &err else {
        panic!("the refresh's own kind, not a selection failure: {err:?}");
    };
    assert_eq!(f.kind, agent_core::auth::RefreshFailureKind::Transient, "{err:?}");
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
    // A retryable failure does not move the selection (review fix 3).
    lookup_without_b(&broker)
        .await
        .expect_err("a's refresh fails");
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
    spawn_held_token_with(StatusCode::BAD_REQUEST, INVALID_GRANT).await
}

/// [`spawn_held_token`] answering every refresh with `status` and `body`.
async fn spawn_held_token_with(status: StatusCode, body: &'static str) -> HeldToken {
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
                (status, [("content-type", "application/json")], body).into_response()
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

    /// Answer every refresh request, now and from now on.
    fn open(&self) {
        self.closed.store(false, Ordering::SeqCst);
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

// ── A2: usage read clocks ───────────────────────────────────────────────────────

const NEVER_RESETS: &str = "2099-01-01T00:00:00Z";

/// A usage reading whose five-hour window is used up until `reset`.
fn used_up(reset: &str) -> Reply {
    Reply::Body(json!({
        "five_hour": {"utilization": 100.0, "resets_at": reset},
        "seven_day": {"utilization": 10.0, "resets_at": NEVER_RESETS}
    }))
}

/// Scratch accounts with room, a usage mock that logs whose usage each
/// request reads, and an automatic broker on the default read clocks that
/// takes its time from a test clock (the clock moves only when a test
/// advances it, or a [`Reply::Slow`] read does).
struct Clocked {
    _home: tempfile::TempDir,
    script: Script,
    broker: LocalBroker,
    log: ReadLog,
    clock: TestClock,
    usage_url: String,
    /// The broker's usage-read table writes the saved readings (the
    /// daemon's), and so does a reloaded one.
    saving: bool,
}

impl Clocked {
    /// What the usage mock answers for `label` from now on.
    fn answer(&self, label: &'static str, reply: Reply) {
        self.script.lock().unwrap().insert(label, reply);
    }

    /// A new broker with an empty usage-read table, as after a daemon
    /// reload: the same store, usage mock and clock, nothing in memory. A
    /// saving one takes the lease from the current broker, as a reloaded
    /// daemon does (the old one stops writing).
    fn reloaded(&self) -> LocalBroker {
        let broker = auto_broker(&self.usage_url).with_test_clock(&self.clock);
        if self.saving {
            self.broker.stop_saving_usage_readings();
            broker.with_new_saving_usage_table()
        } else {
            broker.with_new_usage_table()
        }
    }
}

/// [`clocked`] as the daemon: every login carries an account id
/// (`acct-<label>`; a reading of a login without one is never saved), and
/// the broker's usage-read table writes the saved readings.
async fn clocked_saving(labels: &[&'static str]) -> Clocked {
    let mut t = clocked(labels).await;
    for label in labels {
        set_account_id(&t, label, &format!("acct-{label}"));
    }
    t.saving = true;
    t.broker = auto_broker(&t.usage_url)
        .with_test_clock(&t.clock)
        .with_new_saving_usage_table();
    t
}

/// Seven accounts `a`..`g` ([`clocked_saving`]).
async fn seven_saving() -> Clocked {
    clocked_saving(&LABELS).await
}

/// [`Clocked`] with one account per label, listed in this order.
async fn clocked(labels: &[&'static str]) -> Clocked {
    let accounts: Vec<_> = labels.iter().map(|label| (*label, FAR_FUTURE)).collect();
    let home = install_accounts(&accounts);
    let script: Script = Arc::default();
    for label in labels {
        script.lock().unwrap().insert(*label, roomy());
    }
    let clock = TestClock::default();
    let (usage_url, _, log) = spawn_clocked_usage(script.clone(), Some(clock.clone())).await;
    Clocked {
        _home: home,
        script,
        broker: auto_broker(&usage_url).with_test_clock(&clock),
        log,
        clock,
        usage_url,
        saving: false,
    }
}

/// Seven accounts `a`..`g` ([`clocked`]). `a` is first in the listing, so
/// it serves the first turn.
async fn seven_with_room() -> Clocked {
    clocked(&LABELS).await
}

/// [`clocked`] with the pinned account's reading reused for 60 s, as near
/// its limit, so the next turn start reads it again: for the tests of what
/// a turn start does once that read finds it used up (a switch).
async fn clocked_pinned_60s(labels: &[&'static str]) -> Clocked {
    let mut t = clocked(labels).await;
    t.broker = auto_broker(&t.usage_url)
        .with_test_clock(&t.clock)
        .with_pinned_snapshot_age(later(60));
    t
}

/// Seven accounts `a`..`g` ([`clocked_pinned_60s`]).
async fn seven_pinned_60s() -> Clocked {
    clocked_pinned_60s(&LABELS).await
}

/// One turn start: the account that serves and whose usage it read.
async fn turn_reads(broker: &LocalBroker, log: &ReadLog) -> (CredentialRef, Vec<&'static str>) {
    let mark = log.lock().unwrap().len();
    let served = turn(broker).await;
    let reads = log.lock().unwrap()[mark..].to_vec();
    (served, reads)
}

fn later(secs: u64) -> std::time::Duration {
    std::time::Duration::from_secs(secs)
}

/// `ms` (epoch ms) as an RFC 3339 time, for a reset in a usage reply.
fn rfc3339(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .unwrap()
        .to_rfc3339()
}

/// A turn start reads the account it is pinned to once its reading is past
/// the pinned reuse age (here 60 s, as near its limit), and no other account
/// until that one is due (10 min and up): not every account at every turn
/// start.
#[tokio::test]
#[serial_test::serial]
async fn a_turn_start_reads_the_pinned_account_and_only_the_idle_ones_that_are_due() {
    let t = seven_pinned_60s().await;
    let mut per_turn = Vec::new();
    for _ in 0..3 {
        let (served, reads) = turn_reads(&t.broker, &t.log).await;
        assert_eq!(served, anthropic("a"));
        per_turn.push(reads);
        t.clock.advance(later(61));
    }
    assert_eq!(per_turn[0], LABELS, "the first turn start reads every account");
    assert_eq!(per_turn[1..], [["a"], ["a"]], "then only the pinned one");
}

/// The idle accounts' clocks are spread over the listing: after 10 min
/// they are read again one or two per turn start, never all at once.
#[tokio::test]
#[serial_test::serial]
async fn idle_accounts_are_read_again_at_different_turn_starts() {
    let t = seven_with_room().await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    let mut idle = Vec::new();
    for _ in 0..26 {
        t.clock.advance(later(30));
        let (served, reads) = turn_reads(&t.broker, &t.log).await;
        assert_eq!(served, anthropic("a"));
        idle.push(reads.into_iter().filter(|l| *l != "a").collect::<Vec<_>>());
    }
    assert!(
        idle[..20].iter().all(Vec::is_empty),
        "no idle account is read again within 10 min: {idle:?}"
    );
    assert_eq!(
        idle.concat(),
        ["b", "c", "d", "e", "f", "g"],
        "each idle account is read again once: {idle:?}"
    );
    assert!(
        idle.iter().all(|reads| reads.len() <= 2),
        "at most two idle reads per turn start: {idle:?}"
    );
    assert_eq!(
        idle.iter().filter(|reads| !reads.is_empty()).count(),
        4,
        "over four turn starts: {idle:?}"
    );
}

/// An idle account whose reading reported a reset that has passed since
/// is read again at the next turn start, before its clock is due. The
/// pinned account, whose reading shows room, is not due either (10 min).
#[tokio::test]
#[serial_test::serial]
async fn an_idle_account_whose_reset_has_passed_is_read_again() {
    let t = seven_with_room().await;
    let soon = rfc3339(t.clock.now_ms() + 60_000);
    t.answer("b", used_up(&soon));
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.answer("b", roomy());

    // b's reset passes; its reading is 6 min old, under its clock.
    t.clock.advance(later(6 * 60));
    let (served, reads) = turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, anthropic("a"));
    assert_eq!(reads, ["b"], "b's reset has passed; a and c..g are not due");
}

/// A switch never rests on a reading older than 60 s: the account the
/// selection would switch to is read again first, and nobody else is.
#[tokio::test]
#[serial_test::serial]
async fn a_switch_reads_the_account_it_switches_to_again_first() {
    let t = seven_pinned_60s().await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.answer("a", used_up(NEVER_RESETS));

    t.clock.advance(later(90));
    let (served, reads) = turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, anthropic("b"));
    assert_eq!(reads, ["a", "b"]);
}

/// When the fresh read of the account a switch would go to contradicts its
/// old reading (it is used up now), the choice is redone on the fresh
/// reading, and the next account is read fresh too before it is used.
#[tokio::test]
#[serial_test::serial]
async fn a_switch_never_rests_on_an_old_reading_a_fresh_one_contradicts() {
    let t = seven_pinned_60s().await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    for label in ["a", "b"] {
        t.answer(label, used_up(NEVER_RESETS));
    }

    t.clock.advance(later(90));
    let (served, reads) = turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, anthropic("c"), "b's 90 s old reading said room");
    assert_eq!(reads, ["a", "b", "c"]);
}

/// Grace and the held failed read work as before: the pinned account's
/// read is rate-limited, its last good reading keeps it, no idle account
/// is read for it, and the next turn start sends no request at all.
#[tokio::test]
#[serial_test::serial]
async fn a_rate_limited_pinned_read_is_graced_without_reading_the_idle_accounts() {
    let t = seven_pinned_60s().await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.answer("a", Reply::RateLimited(120));

    t.clock.advance(later(90));
    let (served, reads) = turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, anthropic("a"), "graced on its last good reading");
    assert_eq!(reads, ["a"], "the rate-limited read only");
    let (served, reads) = turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, anthropic("a"));
    assert!(reads.is_empty(), "the failed read is held: {reads:?}");
}

// ── A2 r2: one read per account per selection; no stale "no capacity" ──────

const HOUR_MS: u64 = 60 * 60 * 1000;

/// A reading with room whose weekly budget resets at `reset_ms`: within
/// 24 h it is an urgent reset, and the soonest one is chosen first.
fn room_until(reset_ms: u64) -> Reply {
    Reply::Body(json!({
        "five_hour": {"utilization": 10.0, "resets_at": NEVER_RESETS},
        "seven_day": {"utilization": 10.0, "resets_at": rfc3339(reset_ms)}
    }))
}

/// A reading whose five-hour window is used up, with this `resets_at`.
fn used_up_at(resets_at: serde_json::Value) -> Reply {
    Reply::Body(json!({
        "five_hour": {"utilization": 100.0, "resets_at": resets_at},
        "seven_day": {"utilization": 10.0, "resets_at": NEVER_RESETS}
    }))
}

/// One turn start that terminates: what the vend gave and whose usage it
/// read.
async fn try_turn_reads(
    broker: &LocalBroker,
    log: &ReadLog,
) -> (Result<CredentialRef, String>, Vec<&'static str>) {
    let mark = log.lock().unwrap().len();
    let served = tokio::time::timeout(
        SELECTION_DEADLINE,
        broker.access_token_pinned(OAuthProviderId::Anthropic),
    )
    .await
    .expect("the selection terminates")
    .map(|token| token.credential)
    .map_err(|e| e.to_string());
    let reads = log.lock().unwrap()[mark..].to_vec();
    (served, reads)
}

/// A slow read never makes a selection read an account twice: the pinned
/// account, read at the start, is not read again when the selection has
/// run past 60 s by the time it re-ranks.
#[tokio::test]
#[serial_test::serial]
async fn a_slow_read_never_makes_a_selection_read_an_account_twice() {
    let t = seven_pinned_60s().await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.answer("a", used_up(NEVER_RESETS));
    t.answer("b", Reply::Slow(61, Box::new(used_up(NEVER_RESETS))));

    t.clock.advance(later(90));
    let (served, reads) = turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, anthropic("c"), "b's 90 s old reading said room");
    assert_eq!(reads, ["a", "b", "c"], "each account is read once");
}

/// The account a selection comes back to, after its fresh read lost the
/// choice to older readings that their own fresh reads then contradicted,
/// is chosen on that fresh read: it is not read again, however long the
/// other reads took.
#[tokio::test]
#[serial_test::serial]
async fn a_winner_the_selection_comes_back_to_is_not_read_again() {
    let t = clocked_pinned_60s(&["a", "b", "c", "d"]).await;
    let start = t.clock.now_ms();
    for (label, hours) in [("a", 1), ("b", 2), ("c", 3), ("d", 4)] {
        t.answer(label, room_until(start + hours * HOUR_MS));
    }
    assert_eq!(turn(&t.broker).await, anthropic("a"), "the soonest reset");
    // Now a and d are used up, b has room with no urgent reset, and c is
    // used up and slow to answer.
    t.answer("a", used_up(NEVER_RESETS));
    t.answer("b", roomy());
    t.answer("c", Reply::Slow(61, Box::new(used_up(NEVER_RESETS))));
    t.answer("d", used_up(NEVER_RESETS));

    t.clock.advance(later(90));
    let (served, reads) = turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, anthropic("b"), "the one account with room");
    assert_eq!(reads, ["a", "b", "c", "d"], "each account is read once");
}

/// Before a selection reports no capacity, it reads again, once, each
/// account it rejected on a reading over 60 s old with no reset ahead: here
/// b, used up with no reset to wait for, has room again and serves.
#[tokio::test]
#[serial_test::serial]
async fn an_account_used_up_without_a_reset_is_read_again_before_no_capacity() {
    let t = clocked_pinned_60s(&["a", "b"]).await;
    t.answer("b", used_up_at(json!(null)));
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.answer("a", used_up(NEVER_RESETS));
    t.answer("b", roomy());

    t.clock.advance(later(90));
    let (served, reads) = try_turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, Ok(anthropic("b")), "b has room again");
    assert_eq!(reads, ["a", "b"], "b is read again, once");
}

/// The same for a reading whose reset had already passed when it was taken
/// (no evidence of a new window, so its reset never makes it out of date).
#[tokio::test]
#[serial_test::serial]
async fn an_account_used_up_past_its_reset_is_read_again_before_no_capacity() {
    let t = clocked_pinned_60s(&["a", "b"]).await;
    let passed = rfc3339(t.clock.now_ms() - 60_000);
    t.answer("b", used_up_at(json!(passed)));
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.answer("a", used_up(NEVER_RESETS));
    t.answer("b", roomy());

    t.clock.advance(later(90));
    let (served, reads) = try_turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, Ok(anthropic("b")), "b has room again");
    assert_eq!(reads, ["a", "b"], "b is read again, once");
}

/// But not an account whose reading names a reset still ahead for the
/// window it is used up on: it can't have room before that reset, and the
/// reading is read again once the reset has passed. The selection reports
/// no capacity without reading it.
#[tokio::test]
#[serial_test::serial]
async fn an_account_used_up_until_a_reset_ahead_is_not_read_again_before_no_capacity() {
    let t = clocked_pinned_60s(&["a", "b"]).await;
    let ahead = rfc3339(t.clock.now_ms() + HOUR_MS);
    t.answer("b", used_up_at(json!(ahead)));
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.answer("a", used_up(NEVER_RESETS));
    t.answer("b", roomy());

    t.clock.advance(later(90));
    let (served, reads) = try_turn_reads(&t.broker, &t.log).await;
    let err = served.expect_err("b waits for its reset");
    assert!(err.contains("no account with proven capacity"), "{err}");
    assert_eq!(reads, ["a"], "b is not read again");
}

/// A cache hit that was under 60 s old when the selection took it, but is
/// over 60 s old once the selection chooses (another account's read was
/// slow), is read again before it wins: here a, read 59 s before the turn,
/// while b's read takes 61 s.
#[tokio::test]
#[serial_test::serial]
async fn a_cache_hit_that_ages_past_60s_during_the_selection_is_read_again_before_it_wins() {
    let t = clocked_pinned_60s(&["a", "b"]).await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    // a is read again at 630 s (the pinned 60 s); b's reading (11 min for
    // the second of two accounts) is not due yet.
    t.clock.advance(later(630));
    assert_eq!(turn_reads(&t.broker, &t.log).await, (anthropic("a"), vec!["a"]));
    // 59 s later a's reading is a cache hit and b is due; b's read takes 61 s.
    t.clock.advance(later(59));
    t.answer("b", Reply::Slow(61, Box::new(roomy())));
    let (served, reads) = turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, anthropic("a"));
    assert_eq!(reads, ["b", "a"], "a's reading was 120 s old when it won");
}

/// The same before no capacity: a's reading, used up with no reset and 59 s
/// old when the selection took it, is 120 s old once b's slow read ends, so
/// the check before no capacity reads it again, and a has room again.
#[tokio::test]
#[serial_test::serial]
async fn a_negative_cache_hit_that_ages_past_60s_is_read_again_before_no_capacity() {
    let t = clocked(&["a", "b"]).await;
    let ahead = rfc3339(t.clock.now_ms() + 24 * HOUR_MS);
    t.answer("b", used_up_at(json!(ahead)));
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    // At 630 s a is read again: used up, no reset. b's reading (used up
    // until a reset a day ahead) is not due: no capacity.
    t.clock.advance(later(630));
    t.answer("a", used_up_at(json!(null)));
    let (served, reads) = try_turn_reads(&t.broker, &t.log).await;
    assert!(served.is_err(), "{served:?}");
    assert_eq!(reads, ["a"]);
    // 59 s later a has room again; b is due and its read takes 61 s.
    t.clock.advance(later(59));
    t.answer("a", roomy());
    t.answer("b", Reply::Slow(61, Box::new(used_up_at(json!(ahead)))));
    let (served, reads) = try_turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, Ok(anthropic("a")), "a has room again");
    assert_eq!(reads, ["b", "a"], "a's reading was 120 s old at the check");
}

// ── Usage-read throttle: saved readings, one pause for every account, the
// pinned account's 10 min ─────────────────────────────────────────────────────

/// A reading of the pinned account near its limit: its five-hour window is
/// 92 % used.
fn near_limit() -> Reply {
    Reply::Body(json!({
        "five_hour": {"utilization": 92.0, "resets_at": NEVER_RESETS},
        "seven_day": {"utilization": 10.0, "resets_at": NEVER_RESETS}
    }))
}

/// The saved usage readings beside `t`'s store.
fn saved_readings(t: &Clocked) -> std::path::PathBuf {
    t._home.path().join("usage-last-good.json")
}

/// Give `label`'s stored login this provider account id.
fn set_account_id(t: &Clocked, label: &str, account_id: &str) {
    let path = t._home.path().join("auth.json");
    let mut store: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    store[format!("anthropic@{label}")]["accountId"] = json!(account_id);
    std::fs::write(&path, store.to_string()).unwrap();
}

/// One usage 429 pauses every usage read until its Retry-After: the other
/// accounts, all due, are not read one after another into the same limit.
/// The selection serves on the last readings, and once the pause is over
/// each account is read once.
#[tokio::test]
#[serial_test::serial]
async fn one_usage_429_pauses_every_usage_read_until_its_retry_after() {
    let t = seven_with_room().await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    // 13 min later every account is due, and the endpoint limits a's read.
    t.clock.advance(later(13 * 60));
    t.answer("a", Reply::RateLimited(120));
    let (served, reads) = turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, anthropic("a"), "on its last good reading");
    assert_eq!(reads, ["a"], "no other account is read into the limit");

    t.answer("a", roomy());
    t.clock.advance(later(119));
    let (served, reads) = turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, anthropic("a"));
    assert!(reads.is_empty(), "still paused: {reads:?}");
    t.clock.advance(later(2));
    let (served, reads) = turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, anthropic("a"));
    assert_eq!(reads, LABELS, "the pause is over: each account is read once");
}

/// The pinned account is not read again within 10 min while its reading
/// shows room: a turn start every minute reads nothing until then, and at
/// 10 min only the pinned account is due.
#[tokio::test]
#[serial_test::serial]
async fn the_pinned_account_is_not_read_again_within_10_min_while_it_shows_room() {
    let t = seven_with_room().await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    let mut reads = Vec::new();
    for _ in 0..9 {
        t.clock.advance(later(61));
        let (served, read) = turn_reads(&t.broker, &t.log).await;
        assert_eq!(served, anthropic("a"));
        reads.extend(read);
    }
    assert!(reads.is_empty(), "nothing is read within 10 min: {reads:?}");
    t.clock.advance(later(60));
    assert_eq!(
        turn_reads(&t.broker, &t.log).await,
        (anthropic("a"), vec!["a"]),
        "a is due at 10 min, the others later"
    );
}

/// Near its limit (a window at 90 % or more) the pinned account is read
/// again once its reading is over 60 s old.
#[tokio::test]
#[serial_test::serial]
async fn a_pinned_account_near_its_limit_is_read_again_after_60s() {
    let t = clocked(&["a"]).await;
    t.answer("a", near_limit());
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(59));
    assert_eq!(turn_reads(&t.broker, &t.log).await, (anthropic("a"), vec![]));
    t.clock.advance(later(2));
    assert_eq!(turn_reads(&t.broker, &t.log).await, (anthropic("a"), vec!["a"]));
}

/// A reload keeps the readings: a new broker with nothing in memory serves
/// on the saved ones and reads no account whose reading is under 10 min old.
#[tokio::test]
#[serial_test::serial]
async fn a_reload_sends_no_usage_read_for_readings_under_10_min_old() {
    let t = seven_saving().await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(2 * 60));
    let broker = t.reloaded();
    let (served, reads) = turn_reads(&broker, &t.log).await;
    assert_eq!(served, anthropic("a"));
    assert!(reads.is_empty(), "{reads:?}");
}

/// The incident shape: right after a reload the endpoint answers 429 for
/// every account. The first 429 pauses the reads, and the selection serves
/// on the saved readings (13 min old, inside the grace) instead of failing.
#[tokio::test]
#[serial_test::serial]
async fn after_a_reload_a_usage_429_for_every_account_still_serves_on_the_saved_readings() {
    let t = seven_saving().await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(13 * 60));
    for label in LABELS {
        t.answer(label, Reply::RateLimited(90));
    }
    let broker = t.reloaded();
    let (served, reads) = try_turn_reads(&broker, &t.log).await;
    assert_eq!(served, Ok(anthropic("a")));
    assert_eq!(reads, ["a"], "one 429, then the pause");
}

/// A saved reading counts only for the account it was read from: after a
/// re-login of `a` to another account id, a reload and a 429 for every
/// account, `a` is not served on `a`'s old reading; `b` is, on its own.
#[tokio::test]
#[serial_test::serial]
async fn a_saved_reading_never_serves_another_account_logged_into_the_slot() {
    let t = clocked_saving(&["a", "b"]).await;
    set_account_id(&t, "a", "acct-a-first");
    set_account_id(&t, "b", "acct-b-first");
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(13 * 60));
    set_account_id(&t, "a", "acct-a-second");
    for label in ["a", "b"] {
        t.answer(label, Reply::RateLimited(90));
    }
    let broker = t.reloaded();
    let (served, reads) = try_turn_reads(&broker, &t.log).await;
    assert_eq!(served, Ok(anthropic("b")), "a's reading was another account's");
    assert_eq!(reads, ["a"]);
}

/// A saved reading never makes a cooling account eligible: after a reload,
/// `a` (cooling) is not served on its saved reading; `b` is.
#[tokio::test]
#[serial_test::serial]
async fn a_saved_reading_never_serves_a_cooling_account() {
    let t = clocked_saving(&["a", "b"]).await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(2 * 60));
    let broker = t.reloaded();
    broker
        .report_cooldown(
            &anthropic("a"),
            Some(t.clock.now_ms() + HOUR_MS),
            "anthropic_quota_exhausted",
        )
        .await
        .unwrap();
    let (served, _) = try_turn_reads(&broker, &t.log).await;
    assert_eq!(served, Ok(anthropic("b")));
}

/// The saved readings hold numbers, times, the slot and the account
/// binding: no token, no account id, no credential digest.
#[tokio::test]
#[serial_test::serial]
async fn the_saved_readings_hold_no_token_account_id_or_credential_digest() {
    let t = clocked_saving(&["a", "b"]).await;
    set_account_id(&t, "a", "acct-a-first");
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    let saved = std::fs::read_to_string(saved_readings(&t)).expect("the readings are saved");
    assert!(saved.contains("\"version\""), "{saved}");
    assert_secret_free(&saved);
    for leak in ["access", "refresh", "acct-", "cred:", "id:"] {
        assert!(!saved.contains(leak), "{leak} in {saved}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(saved_readings(&t)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

/// Saving is best effort: with the store's directory read-only the
/// readings can't be saved, and the selection still works. (Unix: a
/// read-only directory by mode bits.)
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial]
async fn a_failed_save_never_fails_a_selection() {
    use std::os::unix::fs::PermissionsExt;
    let t = clocked_saving(&["a", "b"]).await;
    let dir = t._home.path().to_path_buf();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let (served, reads) = try_turn_reads(&t.broker, &t.log).await;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(served, Ok(anthropic("a")));
    assert_eq!(reads, ["a", "b"]);
    assert!(!saved_readings(&t).exists());
}

/// A saved reading is reused only for the account it was read from: after a
/// re-login of `a` to another account id and a reload, `a`'s 2-min-old
/// saved reading is not reused, so `a` is read; `b`'s is reused.
#[tokio::test]
#[serial_test::serial]
async fn a_saved_reading_is_not_reused_for_another_account_logged_into_the_slot() {
    let t = clocked_saving(&["a", "b"]).await;
    set_account_id(&t, "a", "acct-a-first");
    set_account_id(&t, "b", "acct-b-first");
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(2 * 60));
    set_account_id(&t, "a", "acct-a-second");
    let broker = t.reloaded();
    assert_eq!(turn_reads(&broker, &t.log).await, (anthropic("a"), vec!["a"]));
}

/// After a reload a saved reading whose reset has passed since is read
/// again, as one in memory is: here `b`, used up until a reset 60 s after
/// its read, is read 2 min later; `a` (room) is not.
#[tokio::test]
#[serial_test::serial]
async fn after_a_reload_a_saved_reading_whose_reset_has_passed_is_read_again() {
    let t = clocked_saving(&["a", "b"]).await;
    let soon = rfc3339(t.clock.now_ms() + 60_000);
    t.answer("b", used_up(&soon));
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.answer("b", roomy());
    t.clock.advance(later(2 * 60));
    let broker = t.reloaded();
    assert_eq!(turn_reads(&broker, &t.log).await, (anthropic("a"), vec!["b"]));
}

/// The pause outlives a reload: 60 s into a 120 s pause the new broker sends
/// no usage read and serves on the saved readings (14 min old, inside the
/// grace).
#[tokio::test]
#[serial_test::serial]
async fn a_pause_outlives_a_reload() {
    let t = seven_saving().await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(13 * 60));
    t.answer("a", Reply::RateLimited(120));
    assert_eq!(turn_reads(&t.broker, &t.log).await, (anthropic("a"), vec!["a"]));
    t.clock.advance(later(60));
    let broker = t.reloaded();
    let (served, reads) = try_turn_reads(&broker, &t.log).await;
    assert_eq!(served, Ok(anthropic("a")), "on the saved readings");
    assert!(reads.is_empty(), "still paused: {reads:?}");
}

/// A reported limit drops the account's saved reading too, so its old
/// "room" never outlives the limit: after a reload `a` is read again (used
/// up now) and `b` serves on its own saved reading.
#[tokio::test]
#[serial_test::serial]
async fn a_reported_limit_drops_the_saved_reading() {
    let t = clocked_saving(&["a", "b"]).await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.broker
        .report_cooldown(
            &anthropic("a"),
            Some(agent_core::epoch_millis() + HOUR_MS),
            "anthropic_quota_exhausted",
        )
        .await
        .unwrap();
    t.answer("a", used_up(NEVER_RESETS));
    t.clock.advance(later(2 * 60));
    let broker = t.reloaded();
    assert_eq!(turn_reads(&broker, &t.log).await, (anthropic("b"), vec!["a"]));
}

// ── Usage-read throttle, round 2 ─────────────────────────────────────────────

/// A refresh of `b`'s token that the token endpoint accepts.
const REFRESHED_B: &str = r#"{"access_token":"refreshed-anthropic-b-access","refresh_token":"refresh-anth-b-rotated-SECRET","expires_in":3600}"#;

/// p1-r1 B4: the pause is checked again when a read is about to go out,
/// after its token refresh. `b`'s read waits for its refresh; meanwhile
/// `a`'s read gets a usage 429, which pauses every usage read; when `b`'s
/// refresh ends, `b`'s read sends nothing.
#[tokio::test]
#[serial_test::serial]
async fn a_read_that_waited_for_its_token_refresh_sends_nothing_once_a_429_paused_the_reads() {
    let t = clocked(&["a", "b"]).await;
    expire_stored(&t._home, "b");
    let token = spawn_held_token_with(StatusCode::OK, REFRESHED_B).await;
    let broker = Arc::new(
        auto_broker(&t.usage_url)
            .with_test_clock(&t.clock)
            .with_new_usage_table()
            .with_anthropic_token_url_for_tests(token.url.clone()),
    );
    let reading_b = {
        let broker = broker.clone();
        tokio::spawn(async move { broker.usage(&anthropic("b")).await })
    };
    token.arrived(1).await;
    t.answer("a", Reply::RateLimited(120));
    assert!(broker.usage(&anthropic("a")).await.is_err(), "a's usage 429");
    token.open();
    let b = tokio::time::timeout(SELECTION_DEADLINE, reading_b)
        .await
        .expect("b's read ends")
        .unwrap();
    assert!(b.is_err(), "b's read is answered by the pause: {b:?}");
    assert_eq!(*t.log.lock().unwrap(), ["a"], "b's read went out during the pause");
}

/// p1-r1 B5: a reading of a login without an account id is never saved, so
/// another login without an id in the same slot inherits nothing after a
/// reload: both accounts are read again.
#[tokio::test]
#[serial_test::serial]
async fn a_login_without_an_account_id_gets_no_saved_reading() {
    let mut t = clocked(&["a", "b"]).await;
    t.saving = true;
    t.broker = t.reloaded();
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(2 * 60));
    relogin(&t._home, "a", "second", None);
    let broker = t.reloaded();
    assert_eq!(
        turn_reads(&broker, &t.log).await,
        (anthropic("a"), vec!["a", "b"]),
        "a reading of a login without an id was reused after the reload"
    );
}

/// p1-r1 B6: a refused token refresh drops the account's saved reading. `a`
/// is chosen on its cached reading, its refresh is refused, and the turn
/// goes to `b`. After `a` logs in again (the same account) and a reload,
/// `a`'s old reading is neither reused nor a grace for its usage 429.
#[tokio::test]
#[serial_test::serial]
async fn a_refused_refresh_drops_the_saved_reading() {
    let t = clocked_saving(&["a", "b"]).await;
    let (token_url, refreshes) = spawn_token(400, INVALID_GRANT).await;
    t.broker.stop_saving_usage_readings();
    let broker = auto_broker(&t.usage_url)
        .with_test_clock(&t.clock)
        .with_new_saving_usage_table()
        .with_anthropic_token_url_for_tests(token_url.clone());
    assert_eq!(turn(&broker).await, anthropic("a"));
    expire_stored(&t._home, "a");
    t.clock.advance(later(30));
    assert_eq!(turn_reads(&broker, &t.log).await, (anthropic("b"), vec![]));
    assert_eq!(refreshes.load(Ordering::SeqCst), 1, "a's refresh was refused");
    let saved = std::fs::read_to_string(saved_readings(&t)).unwrap();
    assert!(!saved.contains("anthropic@a|"), "a's reading is still saved: {saved}");
    assert!(saved.contains("anthropic@b|"), "{saved}");

    relogin(&t._home, "a", "again", Some("acct-a"));
    t.answer("a", Reply::RateLimited(90));
    broker.stop_saving_usage_readings();
    let reloaded = auto_broker(&t.usage_url)
        .with_test_clock(&t.clock)
        .with_new_saving_usage_table()
        .with_anthropic_token_url_for_tests(token_url);
    let mark = t.log.lock().unwrap().len();
    let served = lookup_without_b(&reloaded).await;
    assert!(served.is_err(), "a served on its refused login's reading: {served:?}");
    assert_eq!(t.log.lock().unwrap()[mark..], ["a"]);
}

/// p1-r1 B6, the other refusal paths: a refused refresh of an account the
/// user chose (`access_token_for`) and of a rejected token's recovery
/// (`recover_rejected_token`) drop the account's reading too, saved copy
/// included. Neither is held: the next call asks the token endpoint again.
#[tokio::test]
#[serial_test::serial]
async fn a_refused_explicit_or_recovery_refresh_drops_the_saved_reading_and_holds_nothing() {
    let t = clocked_saving(&["a", "b", "c"]).await;
    let (token_url, refreshes) = spawn_token(400, INVALID_GRANT).await;
    t.broker.stop_saving_usage_readings();
    let broker = auto_broker(&t.usage_url)
        .with_test_clock(&t.clock)
        .with_new_saving_usage_table()
        .with_anthropic_token_url_for_tests(token_url);
    assert_eq!(turn(&broker).await, anthropic("a"));
    let saved = || std::fs::read_to_string(saved_readings(&t)).unwrap();
    for label in ["a", "b", "c"] {
        assert!(saved().contains(&format!("anthropic@{label}|")), "{}", saved());
    }

    expire_stored(&t._home, "b");
    for _ in 0..2 {
        assert!(broker.access_token_for(&anthropic("b")).await.is_err());
    }
    assert_eq!(refreshes.load(Ordering::SeqCst), 2, "an explicit account is never held");
    assert!(!saved().contains("anthropic@b|"), "b's reading is still saved: {}", saved());

    assert!(broker
        .recover_rejected_token(&anthropic("c"), "anthropic-c-access")
        .await
        .is_err());
    assert_eq!(refreshes.load(Ordering::SeqCst), 3);
    assert!(!saved().contains("anthropic@c|"), "c's reading is still saved: {}", saved());
    assert!(saved().contains("anthropic@a|"), "{}", saved());
}

/// p1-r1 SF4: the writer saves only the slots the store still lists: an
/// account removed from the store leaves the saved readings at the next
/// write.
#[tokio::test]
#[serial_test::serial]
async fn an_account_removed_from_the_store_leaves_the_saved_readings() {
    let t = clocked_saving(&["a", "b"]).await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    let saved = || std::fs::read_to_string(saved_readings(&t)).unwrap();
    assert!(saved().contains("anthropic@b|"), "{}", saved());
    let path = t._home.path().join("auth.json");
    let mut store: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    store.as_object_mut().unwrap().remove("anthropic@b");
    std::fs::write(&path, store.to_string()).unwrap();
    // 11 min later the pinned `a` is due: its new reading is written,
    // without `b`.
    t.clock.advance(later(11 * 60));
    assert_eq!(turn_reads(&t.broker, &t.log).await, (anthropic("a"), vec!["a"]));
    assert!(!saved().contains("anthropic@b|"), "b's reading is still saved: {}", saved());
    assert!(saved().contains("anthropic@a|"), "{}", saved());
}

// ── Usage-read throttle, round 3 ─────────────────────────────────────────────

/// Wait until the usage mock has been asked about `label` `n` times.
async fn asked(log: &ReadLog, label: &str, n: usize) {
    let deadline = tokio::time::Instant::now() + SELECTION_DEADLINE;
    while log.lock().unwrap().iter().filter(|l| **l == label).count() < n {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no usage request {n} for {label}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// p1-r2 B1 (R3-3): a usage read of `a` in flight when a limit is reported
/// for `a` is discarded when it lands: not used, cached or saved. Its line
/// is a usage-read line with `outcome=superseded` and the account label;
/// it is no observation (`auth state` keeps the last good read); after a
/// reload `a` has no saved reading, so it is read. It starts no pause (`b`
/// is read next) and no hold (`a`'s next read goes out).
#[tokio::test]
#[serial_test::serial]
async fn a_reading_in_flight_when_a_limit_is_reported_is_discarded() {
    let t = clocked_saving(&["a", "b"]).await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    let closed = Arc::new(AtomicBool::new(true));
    t.answer("a", Reply::Held(closed.clone(), Box::new(roomy())));
    let capture = Capture::default();
    let a = anthropic("a");
    let (read, ()) = {
        let _guard = tracing::subscriber::set_default(capture_subscriber(&capture));
        tokio::join!(t.broker.usage(&a), async {
            asked(&t.log, "a", 2).await;
            t.broker
                .report_cooldown(
                    &anthropic("a"),
                    Some(agent_core::epoch_millis() + HOUR_MS),
                    "anthropic_quota_exhausted",
                )
                .await
                .unwrap();
            closed.store(false, Ordering::SeqCst);
        })
    };
    assert!(read.is_err(), "the reading that landed after the limit was used: {read:?}");
    let log = capture.take();
    let line = log
        .lines()
        .find(|l| l.contains("outcome=superseded"))
        .unwrap_or_else(|| panic!("no superseded line:\n{log}"));
    assert!(line.contains("usage read") && line.contains("account=a"), "{line}");
    assert!(!line.contains("usage_rate_limited"), "{line}");
    assert_secret_free(&log);
    assert!(
        account_state(&t.broker, "a")["last_usage"]
            .get("failure")
            .is_none(),
        "the discard was recorded as a failed read"
    );
    let saved = std::fs::read_to_string(saved_readings(&t)).unwrap();
    assert!(!saved.contains("anthropic@a|"), "a's reading was saved: {saved}");
    assert!(saved.contains("anthropic@b|"), "{saved}");

    // A minute later (the saved readings keep the mock's own read times,
    // which a load at an earlier clock time would reject as future-dated).
    t.clock.advance(later(60));
    let reloaded = t.reloaded();
    let reads = {
        let _guard = tracing::subscriber::set_default(capture_subscriber(&capture));
        turn_reads(&reloaded, &t.log).await
    };
    assert_eq!(reads, (anthropic("a"), vec!["a"]), "{}", capture.take());
    let mark = t.log.lock().unwrap().len();
    assert!(t.broker.usage(&anthropic("b")).await.is_ok(), "the discard paused the reads");
    assert!(t.broker.usage(&anthropic("a")).await.is_ok(), "the discard held a");
    assert_eq!(t.log.lock().unwrap()[mark..], ["b", "a"]);
}

/// p1-r2 B1 (R3-3), a refusal: the same with a refused token refresh of
/// `a` (the user's explicit use of `a`) while `a`'s usage read is in
/// flight: the landed reading is not saved, and after a reload `a` is read
/// again, not served on it.
#[tokio::test]
#[serial_test::serial]
async fn a_reading_in_flight_when_the_login_is_refused_is_discarded() {
    let t = clocked_saving(&["a", "b"]).await;
    let (token_url, refreshes) = spawn_token(400, INVALID_GRANT).await;
    t.broker.stop_saving_usage_readings();
    let broker = auto_broker(&t.usage_url)
        .with_test_clock(&t.clock)
        .with_new_saving_usage_table()
        .with_anthropic_token_url_for_tests(token_url);
    assert_eq!(turn(&broker).await, anthropic("a"));
    let closed = Arc::new(AtomicBool::new(true));
    t.answer("a", Reply::Held(closed.clone(), Box::new(roomy())));
    let a = anthropic("a");
    let (read, refused) = tokio::join!(broker.usage(&a), async {
        asked(&t.log, "a", 2).await;
        expire_stored(&t._home, "a");
        let refused = broker.access_token_for(&anthropic("a")).await;
        closed.store(false, Ordering::SeqCst);
        refused
    });
    assert!(refused.is_err());
    assert_eq!(refreshes.load(Ordering::SeqCst), 1, "a's refresh was refused");
    assert!(read.is_err(), "the reading that landed after the refusal was used: {read:?}");
    let saved = std::fs::read_to_string(saved_readings(&t)).unwrap();
    assert!(!saved.contains("anthropic@a|"), "a's reading was saved: {saved}");
    assert!(saved.contains("anthropic@b|"), "{saved}");

    // A reload (R4-4): a's reading is not served again. With a's login
    // valid again (only its expiry moves back: the same tokens, the same
    // account), the reloaded selection reads a; b's saved reading, under
    // 10 min old, is reused. (The clock moves on a minute: the saved
    // readings keep the mock's own read times.)
    broker.stop_saving_usage_readings();
    let path = t._home.path().join("auth.json");
    let mut root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    root["anthropic@a"]["expires"] = json!(FAR_FUTURE);
    std::fs::write(&path, root.to_string()).unwrap();
    t.clock.advance(later(60));
    let (token_url, refreshes) = spawn_token(400, INVALID_GRANT).await;
    let reloaded = auto_broker(&t.usage_url)
        .with_test_clock(&t.clock)
        .with_new_saving_usage_table()
        .with_anthropic_token_url_for_tests(token_url);
    assert_eq!(turn_reads(&reloaded, &t.log).await, (anthropic("a"), vec!["a"]));
    assert_eq!(refreshes.load(Ordering::SeqCst), 0, "a's login was refreshed");
}

/// p1-r2 B2 (R3-3): a refusal through one broker ends every broker's cached
/// copy of the reading. Two brokers on one usage-read table (as in one
/// daemon) both serve on the saved readings, so both cache `a`'s. A refused
/// recovery of `a`'s token through one drops `a`'s reading; the other's
/// next selection does not reuse its cached copy: it reads `a`.
#[tokio::test]
#[serial_test::serial]
async fn a_refusal_through_one_broker_ends_the_other_brokers_cached_reading() {
    let t = clocked_saving(&["a", "b"]).await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(2 * 60));
    let (token_url, refreshes) = spawn_token(400, INVALID_GRANT).await;
    let one = auto_broker(&t.usage_url)
        .with_test_clock(&t.clock)
        .with_new_usage_table()
        .with_anthropic_token_url_for_tests(token_url.clone());
    let two = auto_broker(&t.usage_url)
        .with_test_clock(&t.clock)
        .sharing_usage_table_with(&one)
        .with_anthropic_token_url_for_tests(token_url);
    assert_eq!(turn_reads(&one, &t.log).await, (anthropic("a"), vec![]));
    assert_eq!(turn_reads(&two, &t.log).await, (anthropic("a"), vec![]));
    assert!(one
        .recover_rejected_token(&anthropic("a"), "anthropic-a-access")
        .await
        .is_err());
    assert_eq!(refreshes.load(Ordering::SeqCst), 1, "a's recovery was refused");
    assert_eq!(
        turn_reads(&two, &t.log).await,
        (anthropic("a"), vec!["a"]),
        "the other broker reused its copy of a's dropped reading"
    );
}

/// p1-r3 B1 (R4-1): a refusal that lands while a selection waits on another
/// account's read ends the selection's own copy of the refused account's
/// reading. Two brokers on one usage-read table (as in one daemon): Y reads
/// `a` (room), then waits on `b`'s read; meanwhile X's refused recovery of
/// `a` drops `a`'s reading. Once `b`'s read lands, Y does not choose `a` on
/// its copy, and does not read `a` again in this selection: it chooses `b`.
#[tokio::test]
#[serial_test::serial]
async fn a_refusal_while_a_selection_waits_on_another_read_ends_its_copy_of_the_reading() {
    let t = clocked(&["a", "b"]).await;
    let (token_url, refreshes) = spawn_token(400, INVALID_GRANT).await;
    let x = auto_broker(&t.usage_url)
        .with_test_clock(&t.clock)
        .with_new_usage_table()
        .with_anthropic_token_url_for_tests(token_url.clone());
    let y = auto_broker(&t.usage_url)
        .with_test_clock(&t.clock)
        .sharing_usage_table_with(&x)
        .with_anthropic_token_url_for_tests(token_url);
    let closed = Arc::new(AtomicBool::new(true));
    t.answer("b", Reply::Held(closed.clone(), Box::new(roomy())));
    let (served, refused) = tokio::join!(turn_reads(&y, &t.log), async {
        asked(&t.log, "b", 1).await;
        let refused = x
            .recover_rejected_token(&anthropic("a"), "anthropic-a-access")
            .await;
        closed.store(false, Ordering::SeqCst);
        refused
    });
    assert!(refused.is_err());
    assert_eq!(refreshes.load(Ordering::SeqCst), 1, "a's recovery was refused");
    assert_eq!(
        served,
        (anthropic("b"), vec!["a", "b"]),
        "Y chose a on its copy of a's dropped reading, or read a again"
    );
}

/// p1-r3 B1 (R4-1), a later ranking pass: `a`'s reading, taken in the
/// selection's first pass, is dropped while the second pass reads `b` again
/// (the first pass chose `b` on a 90 s old reading), and `b` turns out used
/// up. The second pass does not choose `a` on the first pass's copy: with
/// no other account, the selection reports no capacity, reading nothing
/// more.
#[tokio::test]
#[serial_test::serial]
async fn a_refusal_during_a_later_ranking_pass_ends_the_selections_copy_of_the_reading() {
    let t = clocked(&["a", "b"]).await;
    let (token_url, refreshes) = spawn_token(400, INVALID_GRANT).await;
    let y = auto_broker(&t.usage_url)
        .with_test_clock(&t.clock)
        .with_pinned_snapshot_age(later(60))
        .with_new_usage_table()
        .with_anthropic_token_url_for_tests(token_url.clone());
    let x = auto_broker(&t.usage_url)
        .with_test_clock(&t.clock)
        .sharing_usage_table_with(&y)
        .with_anthropic_token_url_for_tests(token_url);
    let start = t.clock.now_ms();
    t.answer("a", room_until(start + 2 * HOUR_MS));
    t.answer("b", room_until(start + 3 * HOUR_MS));
    assert_eq!(
        turn_reads(&y, &t.log).await,
        (anthropic("a"), vec!["a", "b"]),
        "the soonest reset"
    );
    // 90 s later a (pinned, 60 s) is read again: room, but until a later
    // reset than b's 90 s old reading names. So the first pass chooses b,
    // and b is read again before it wins: it is used up now.
    t.clock.advance(later(90));
    t.answer("a", room_until(start + 4 * HOUR_MS));
    let closed = Arc::new(AtomicBool::new(true));
    t.answer("b", Reply::Held(closed.clone(), Box::new(used_up(NEVER_RESETS))));
    let ((served, reads), refused) = tokio::join!(try_turn_reads(&y, &t.log), async {
        asked(&t.log, "b", 2).await;
        let refused = x
            .recover_rejected_token(&anthropic("a"), "anthropic-a-access")
            .await;
        closed.store(false, Ordering::SeqCst);
        refused
    });
    assert!(refused.is_err());
    assert_eq!(refreshes.load(Ordering::SeqCst), 1, "a's recovery was refused");
    assert!(
        served.is_err(),
        "the second pass chose a on its copy of a's dropped reading: {served:?}"
    );
    assert_eq!(reads, ["a", "b"], "a was read again in the selection");
}

/// p1-r4 B1 (R5-1): an account whose re-read a selection has queued, and
/// whose reading is dropped before the next pass looks it up, is not read
/// again and not chosen: it counts as failed for the rest of the selection.
/// Two brokers on one usage-read table: Y's first pass finds `a` and `b`
/// used up with no reset to wait for, on 90 s old readings, so before it
/// reports no capacity it queues both re-reads; while the next pass reads
/// `a` again, X's refused recovery of `b` drops `b`'s reading. Y then
/// neither reads `b` (which has room by now) nor chooses it. (The re-read
/// queued for a switch target goes the same way; the selection queues two
/// re-reads here so that one is in flight before the other is looked up.)
#[tokio::test]
#[serial_test::serial]
async fn a_refusal_after_a_reread_is_queued_ends_the_selections_copy_without_a_read() {
    let t = clocked(&["a", "b"]).await;
    let (token_url, refreshes) = spawn_token(400, INVALID_GRANT).await;
    let y = auto_broker(&t.usage_url)
        .with_test_clock(&t.clock)
        .with_new_usage_table()
        .with_anthropic_token_url_for_tests(token_url.clone());
    let x = auto_broker(&t.usage_url)
        .with_test_clock(&t.clock)
        .sharing_usage_table_with(&y)
        .with_anthropic_token_url_for_tests(token_url);
    t.answer("a", used_up_at(json!(null)));
    t.answer("b", used_up_at(json!(null)));
    let (served, reads) = try_turn_reads(&y, &t.log).await;
    assert!(served.is_err(), "both used up: {served:?}");
    assert_eq!(reads, ["a", "b"]);

    t.clock.advance(later(90));
    let closed = Arc::new(AtomicBool::new(true));
    t.answer("a", Reply::Held(closed.clone(), Box::new(used_up_at(json!(null)))));
    t.answer("b", roomy());
    let ((served, reads), refused) = tokio::join!(try_turn_reads(&y, &t.log), async {
        asked(&t.log, "a", 2).await;
        let refused = x
            .recover_rejected_token(&anthropic("b"), "anthropic-b-access")
            .await;
        closed.store(false, Ordering::SeqCst);
        refused
    });
    assert!(refused.is_err());
    assert_eq!(refreshes.load(Ordering::SeqCst), 1, "b's recovery was refused");
    assert!(
        served.is_err(),
        "Y chose b after b's reading was dropped: {served:?}"
    );
    assert_eq!(reads, ["a"], "b was read again after its reading was dropped");
}

/// p1-r4 (R5-4): a limit reported while a selection waits on another
/// account's read ends the selection's copy of the limited account's
/// reading, and the account still counts as cooling down until the known
/// time: with the other account used up until a later reset, the selection
/// reports all_cooling_down, the earliest time being the cooldown's end.
#[tokio::test]
#[serial_test::serial]
async fn a_limit_reported_mid_selection_still_reports_all_cooling_down() {
    let t = clocked(&["a", "b"]).await;
    let now = t.clock.now_ms();
    let (until_a, reset_b) = (now + HOUR_MS, now + 2 * HOUR_MS);
    let closed = Arc::new(AtomicBool::new(true));
    t.answer(
        "b",
        Reply::Held(closed.clone(), Box::new(used_up_at(json!(rfc3339(reset_b))))),
    );
    let (served, reported) = tokio::join!(
        tokio::time::timeout(
            SELECTION_DEADLINE,
            t.broker.access_token_pinned(OAuthProviderId::Anthropic)
        ),
        async {
            asked(&t.log, "b", 1).await;
            let reported = t
                .broker
                .report_cooldown(&anthropic("a"), Some(until_a), "anthropic_quota_exhausted")
                .await;
            closed.store(false, Ordering::SeqCst);
            reported
        }
    );
    reported.unwrap();
    let err = served.expect("the selection terminates").unwrap_err();
    let BrokerError::NoAccountAvailable {
        failure: Some(failure),
        ..
    } = &err
    else {
        panic!("typed selection failure expected: {err:?}");
    };
    let failure = serde_json::to_value(failure).unwrap();
    assert_eq!(failure["kind"], "all_cooling_down", "{failure}");
    assert_eq!(failure["earliest_reset_ms"], json!(until_a), "{failure}");
    assert_eq!(failure["accounts"][0]["account"], "a", "{failure}");
    assert_eq!(failure["accounts"][0]["cooling_down"], true, "{failure}");
    assert_eq!(failure["accounts"][0]["until_ms"], json!(until_a), "{failure}");
    assert_eq!(*t.log.lock().unwrap(), ["a", "b"], "a was read again");
}

/// p1-r2 SF1 (R3-4): pruning loads the saved readings first. After a reload
/// with `b` gone from the store, the first selection prunes `b`'s saved
/// reading before `a`'s new one is written, so the file has no `b`. Logged
/// in again, `b` has no reading: it is read, and saved again.
#[tokio::test]
#[serial_test::serial]
async fn a_reload_prunes_a_removed_account_before_its_first_save() {
    let t = clocked_saving(&["a", "b"]).await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    let saved = || std::fs::read_to_string(saved_readings(&t)).unwrap();
    assert!(saved().contains("anthropic@b|"), "{}", saved());
    let path = t._home.path().join("auth.json");
    let with_b = std::fs::read_to_string(&path).unwrap();
    let mut store: serde_json::Value = serde_json::from_str(&with_b).unwrap();
    store.as_object_mut().unwrap().remove("anthropic@b");
    std::fs::write(&path, store.to_string()).unwrap();
    t.clock.advance(later(11 * 60));
    let broker = t.reloaded();
    assert_eq!(turn_reads(&broker, &t.log).await, (anthropic("a"), vec!["a"]));
    assert!(!saved().contains("anthropic@b|"), "b's reading was written back: {}", saved());
    std::fs::write(&path, with_b).unwrap();
    t.clock.advance(later(30));
    assert_eq!(turn_reads(&broker, &t.log).await, (anthropic("a"), vec!["b"]));
    assert!(saved().contains("anthropic@b|"), "{}", saved());
}

/// SF3, the pinned account's default 10 min: a limit on its message request
/// switches the turn to `b`, read fresh first (its 2 min old reading never
/// decides a switch); the cooling account is not read.
#[tokio::test]
#[serial_test::serial]
async fn at_the_default_age_a_reported_limit_switches_to_an_account_read_fresh() {
    let t = clocked(&["a", "b"]).await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(2 * 60));
    t.broker
        .report_cooldown(
            &anthropic("a"),
            Some(t.clock.now_ms() + HOUR_MS),
            "anthropic_quota_exhausted",
        )
        .await
        .unwrap();
    assert_eq!(turn_reads(&t.broker, &t.log).await, (anthropic("b"), vec!["b"]));
}

/// SF3: at the default age a reset that has passed since the pinned
/// account's reading reads it again (2 min in, not 10); it is used up now,
/// so the turn switches to `b`, read fresh first.
#[tokio::test]
#[serial_test::serial]
async fn at_the_default_age_a_passed_reset_reads_the_pinned_account_and_a_switch_reads_its_target() {
    let t = clocked(&["a", "b"]).await;
    t.answer("a", room_until(t.clock.now_ms() + 90_000));
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(2 * 60));
    let ahead = rfc3339(t.clock.now_ms() + 24 * HOUR_MS);
    t.answer("a", used_up_at(json!(ahead)));
    assert_eq!(
        turn_reads(&t.broker, &t.log).await,
        (anthropic("b"), vec!["a", "b"])
    );
}

/// One turn start for `model`: the account that serves and whose usage it
/// read.
async fn model_turn_reads(
    broker: &LocalBroker,
    log: &ReadLog,
    model: &str,
) -> (CredentialRef, Vec<&'static str>) {
    let mark = log.lock().unwrap().len();
    let served = tokio::time::timeout(
        SELECTION_DEADLINE,
        broker.access_token_pinned_for(OAuthProviderId::Anthropic, Some(model)),
    )
    .await
    .expect("the selection terminates")
    .expect("an account serves")
    .credential;
    let reads = log.lock().unwrap()[mark..].to_vec();
    (served, reads)
}

/// SF3: near its limit means a window of the requested model at 90 % or
/// more. `a`'s Sonnet window is at 95 %: an Opus turn keeps the 10 min
/// reuse, a Sonnet turn reads `a` again after 60 s.
#[tokio::test]
#[serial_test::serial]
async fn near_its_limit_means_on_a_window_of_the_requested_model() {
    const OPUS: &str = "claude-opus-4-7";
    const SONNET: &str = "claude-sonnet-4-5";
    let t = clocked(&["a"]).await;
    t.answer(
        "a",
        Reply::Body(json!({
            "five_hour": {"utilization": 10.0, "resets_at": NEVER_RESETS},
            "seven_day": {"utilization": 10.0, "resets_at": NEVER_RESETS},
            "seven_day_sonnet": {"utilization": 95.0, "resets_at": NEVER_RESETS}
        })),
    );
    assert_eq!(
        model_turn_reads(&t.broker, &t.log, OPUS).await,
        (anthropic("a"), vec!["a"])
    );
    assert_eq!(
        model_turn_reads(&t.broker, &t.log, SONNET).await,
        (anthropic("a"), vec![])
    );
    t.clock.advance(later(61));
    assert_eq!(
        model_turn_reads(&t.broker, &t.log, OPUS).await,
        (anthropic("a"), vec![]),
        "the Sonnet window is not Opus's"
    );
    assert_eq!(
        model_turn_reads(&t.broker, &t.log, SONNET).await,
        (anthropic("a"), vec!["a"]),
        "Sonnet's own window is near its limit"
    );
}

/// SF3: the pinned account's cached reading, 599 s old when the selection
/// starts, is over 10 min old once `b`'s read (its reset has passed) takes
/// 2 s, so `a` is read again before it serves.
#[tokio::test]
#[serial_test::serial]
async fn at_the_default_age_a_cached_reading_that_turns_10_min_old_during_the_selection_is_read_again(
) {
    let t = clocked(&["a", "b"]).await;
    let soon = rfc3339(t.clock.now_ms() + 60_000);
    t.answer("b", used_up(&soon));
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(599));
    t.answer("b", Reply::Slow(2, Box::new(roomy())));
    assert_eq!(
        turn_reads(&t.broker, &t.log).await,
        (anthropic("a"), vec!["b", "a"])
    );
}

/// SF3: the pause holds the reads a passed reset or a near limit would
/// otherwise force: `a` near its limit and `b` past its reset are not read
/// while `c`'s usage 429 pauses the reads (`a` serves on its last reading);
/// once the pause is over they are.
#[tokio::test]
#[serial_test::serial]
async fn a_pause_suppresses_the_reads_a_passed_reset_or_a_near_limit_would_force() {
    let t = clocked(&["a", "b", "c"]).await;
    let soon = rfc3339(t.clock.now_ms() + 30_000);
    t.answer("a", near_limit());
    t.answer("b", used_up(&soon));
    t.answer("c", Reply::RateLimited(120));
    let (served, reads) = try_turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, Ok(anthropic("a")));
    assert_eq!(reads, ["a", "b", "c"]);
    t.answer("b", roomy());
    t.clock.advance(later(61));
    let (served, reads) = try_turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, Ok(anthropic("a")), "on a's last reading");
    assert!(reads.is_empty(), "paused: {reads:?}");
    t.answer("c", roomy());
    t.clock.advance(later(60));
    let (served, reads) = try_turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, Ok(anthropic("a")));
    // b (its reset passed) and c (no reading) in the first pass, then a
    // (near its limit) before it serves.
    assert_eq!(reads, ["b", "c", "a"], "the pause is over");
}

/// SF3: the burst limits itself. When the pause ends with all seven
/// accounts due and the first read is limited again, that is the one read.
#[tokio::test]
#[serial_test::serial]
async fn when_the_pause_ends_and_the_first_read_is_limited_again_no_other_account_is_read() {
    let t = seven_with_room().await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(13 * 60));
    t.answer("a", Reply::RateLimited(120));
    assert_eq!(turn_reads(&t.broker, &t.log).await, (anthropic("a"), vec!["a"]));
    t.clock.advance(later(121));
    let (served, reads) = try_turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, Ok(anthropic("a")), "on a's last good reading");
    assert_eq!(reads, ["a"], "one read in total");
}

// ── Part 2: the last known reading in a long usage-read failure ──────────────

/// `reply`'s body with Anthropic's paid extra usage switched on or off, in
/// the shape the usage endpoint reports it.
fn extra_usage(reply: Reply, enabled: bool) -> Reply {
    let Reply::Body(mut body) = reply else {
        panic!("a body reply");
    };
    body["extra_usage"] = if enabled {
        json!({
            "is_enabled": true,
            "monthly_limit": 5000,
            "used_credits": 0.0,
            "utilization": 0.0
        })
    } else {
        json!({
            "is_enabled": false,
            "monthly_limit": null,
            "used_credits": null,
            "utilization": null
        })
    };
    Reply::Body(body)
}

/// One turn start that fails: the typed selection failure (JSON) and whose
/// usage it read.
async fn failed_turn(
    broker: &LocalBroker,
    log: &ReadLog,
) -> (serde_json::Value, Vec<&'static str>) {
    let mark = log.lock().unwrap().len();
    let result = tokio::time::timeout(
        SELECTION_DEADLINE,
        broker.access_token_pinned(OAuthProviderId::Anthropic),
    )
    .await
    .expect("the selection terminates");
    let err = match result {
        Ok(token) => panic!("{} served", token.credential),
        Err(err) => err,
    };
    let BrokerError::NoAccountAvailable {
        failure: Some(failure),
        ..
    } = &err
    else {
        panic!("typed selection failure expected: {err:?}");
    };
    let reads = log.lock().unwrap()[mark..].to_vec();
    (serde_json::to_value(failure).unwrap(), reads)
}

/// `label`'s entry in a selection failure (JSON).
fn failed_account<'f>(failure: &'f serde_json::Value, label: &str) -> &'f serde_json::Value {
    failure["accounts"]
        .as_array()
        .and_then(|accounts| accounts.iter().find(|a| a["account"] == label))
        .unwrap_or_else(|| panic!("no {label} in {failure}"))
}

/// The test clock's time rounded down to a whole second, plus `ms`: a reset
/// that survives the RFC 3339 round trip exactly.
fn whole_second(t: &Clocked, ms: u64) -> u64 {
    t.clock.now_ms() / 1000 * 1000 + ms
}

/// S1: a window that was used up when last read, and whose reset has
/// passed since, has started again. In a usage 429 wall the account serves
/// on that reading instead of failing with `usage_read_failed`.
#[tokio::test]
#[serial_test::serial]
async fn a_window_whose_reset_passed_during_a_429_wall_is_room() {
    let t = clocked(&["a"]).await;
    let reset = whole_second(&t, 5 * 60_000);
    t.answer("a", used_up_at(json!(rfc3339(reset))));
    let (failure, reads) = failed_turn(&t.broker, &t.log).await;
    assert_eq!(failure["kind"], "all_cooling_down", "{failure}");
    assert_eq!(reads, ["a"]);

    t.clock.advance(later(6 * 60));
    t.answer("a", Reply::RateLimited(120));
    let (served, reads) = try_turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, Ok(anthropic("a")), "its window started again at the reset");
    assert_eq!(reads, ["a"], "read again once its reset passed: the 429");
}

/// S2: a window used up until a reset still ahead keeps the account out in
/// a usage 429 wall, and says until when: the failure is
/// `all_cooling_down` with that reset, not `usage_read_failed`.
#[tokio::test]
#[serial_test::serial]
async fn a_spent_window_with_its_reset_ahead_reads_as_cooling_down_in_a_429_wall() {
    let t = clocked(&["a"]).await;
    let reset = whole_second(&t, 2 * HOUR_MS);
    t.answer("a", used_up_at(json!(rfc3339(reset))));
    let (failure, _) = failed_turn(&t.broker, &t.log).await;
    assert_eq!(failure["kind"], "all_cooling_down", "{failure}");

    t.clock.advance(later(10 * 60 + 1));
    t.answer("a", Reply::RateLimited(120));
    let (failure, reads) = failed_turn(&t.broker, &t.log).await;
    assert_eq!(reads, ["a"], "due at 10 min: the 429");
    assert_eq!(failure["kind"], "all_cooling_down", "{failure}");
    assert_eq!(failure["earliest_reset_ms"], json!(reset), "{failure}");
    let a = failed_account(&failure, "a");
    assert_eq!(a["kind"], "exhausted", "{failure}");
    assert_eq!(a["until_ms"], json!(reset), "{failure}");
}

/// S2, every account: each one's last reading is used up until a reset
/// still ahead, and the endpoint answers 429. The failure is
/// `all_cooling_down` with the earliest of their resets.
#[tokio::test]
#[serial_test::serial]
async fn all_spent_graced_accounts_give_the_earliest_reset() {
    let t = clocked(&["a", "b", "c"]).await;
    let resets = [
        ("a", whole_second(&t, 3 * HOUR_MS)),
        ("b", whole_second(&t, HOUR_MS)),
        ("c", whole_second(&t, 2 * HOUR_MS)),
    ];
    for (label, reset) in resets {
        t.answer(label, used_up_at(json!(rfc3339(reset))));
    }
    let (failure, reads) = failed_turn(&t.broker, &t.log).await;
    assert_eq!(failure["kind"], "all_cooling_down", "{failure}");
    assert_eq!(reads, ["a", "b", "c"]);

    t.clock.advance(later(13 * 60));
    for (label, _) in resets {
        t.answer(label, Reply::RateLimited(120));
    }
    let (failure, reads) = failed_turn(&t.broker, &t.log).await;
    assert_eq!(reads, ["a"], "one 429, then the pause");
    assert_eq!(failure["kind"], "all_cooling_down", "{failure}");
    assert_eq!(failure["earliest_reset_ms"], json!(resets[1].1), "{failure}");
    for (label, reset) in resets {
        let account = failed_account(&failure, label);
        assert_eq!(account["kind"], "exhausted", "{failure}");
        assert_eq!(account["until_ms"], json!(reset), "{failure}");
    }
}

/// S4: past the 30-minute grace, the pinned account still serves on its
/// last reading in a usage 429 wall when that reading shows its paid extra
/// usage switched off (a spent window then answers with a limit, never a
/// bill). `b`, whose extra usage is on, would not.
#[tokio::test]
#[serial_test::serial]
async fn past_30_min_a_pinned_account_with_extra_usage_off_serves_a_429_wall() {
    let t = clocked(&["a", "b"]).await;
    t.answer("a", extra_usage(roomy(), false));
    t.answer("b", extra_usage(roomy(), true));
    assert_eq!(turn(&t.broker).await, anthropic("a"));

    t.clock.advance(later(45 * 60));
    for label in ["a", "b"] {
        t.answer(label, Reply::RateLimited(120));
    }
    let (served, reads) = try_turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, Ok(anthropic("a")), "a's extra usage is off");
    assert_eq!(reads, ["a"], "one 429, then the pause");
}

/// S4 through the pause: the pinned account's extra usage is on, so its
/// 45-minute-old reading no longer counts; `b`'s says nothing about extra
/// usage; `c` and `d` have it off. `c` serves, on a read the 429's pause
/// held back (a usage 429 too).
#[tokio::test]
#[serial_test::serial]
async fn past_30_min_a_paused_read_of_an_account_with_extra_usage_off_serves() {
    let t = clocked(&["a", "b", "c", "d"]).await;
    t.answer("a", extra_usage(roomy(), true));
    t.answer("c", extra_usage(roomy(), false));
    t.answer("d", extra_usage(roomy(), false));
    assert_eq!(turn(&t.broker).await, anthropic("a"));

    t.clock.advance(later(45 * 60));
    for label in ["a", "b", "c", "d"] {
        t.answer(label, Reply::RateLimited(120));
    }
    let (served, reads) = try_turn_reads(&t.broker, &t.log).await;
    assert_eq!(served, Ok(anthropic("c")), "the first account with extra usage off");
    assert_eq!(reads, ["a"], "one 429, then the pause");
}

/// N: near its limit, the pinned account keeps the 10-minute age when its
/// extra usage is off (filling up gives a message 429 and a failover, never
/// a bill): it is not read again at 2 or 9 minutes, and is at 10.
#[tokio::test]
#[serial_test::serial]
async fn a_near_limit_pinned_account_with_extra_usage_off_keeps_the_10_min_age() {
    let t = clocked(&["a"]).await;
    t.answer("a", extra_usage(near_limit(), false));
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(2 * 60));
    assert_eq!(turn_reads(&t.broker, &t.log).await, (anthropic("a"), vec![]));
    t.clock.advance(later(7 * 60));
    assert_eq!(turn_reads(&t.broker, &t.log).await, (anthropic("a"), vec![]));
    t.clock.advance(later(61));
    assert_eq!(turn_reads(&t.broker, &t.log).await, (anthropic("a"), vec!["a"]));
}

/// F: the saved readings keep the extra usage flag. After a reload 45
/// minutes later, in a usage 429 wall, `a` (off) serves on its saved
/// reading; `b` (on) would not.
#[tokio::test]
#[serial_test::serial]
async fn saved_readings_keep_the_extra_usage_flag_across_a_reload() {
    let t = clocked_saving(&["a", "b"]).await;
    t.answer("a", extra_usage(roomy(), false));
    t.answer("b", extra_usage(roomy(), true));
    assert_eq!(turn(&t.broker).await, anthropic("a"));

    t.clock.advance(later(45 * 60));
    for label in ["a", "b"] {
        t.answer(label, Reply::RateLimited(120));
    }
    let broker = t.reloaded();
    let (served, reads) = try_turn_reads(&broker, &t.log).await;
    assert_eq!(served, Ok(anthropic("a")), "the flag came back from the saved readings");
    assert_eq!(reads, ["a"], "one 429, then the pause");
}

/// F (guard, review r1): the flag of the later saved reading rules. `a` is
/// read with extra usage off, then on; `b` on, then off; each pair saved by
/// the daemon of its time. After a reload and a 45-minute usage 429 wall,
/// `b` serves on its later reading, and `a`, first in the listing, does not.
#[tokio::test]
#[serial_test::serial]
async fn the_later_saved_reading_s_extra_usage_flag_rules_after_a_reload() {
    let t = clocked_saving(&["a", "b"]).await;
    t.answer("a", extra_usage(roomy(), false));
    t.answer("b", extra_usage(roomy(), true));
    assert_eq!(turn(&t.broker).await, anthropic("a"));

    // Past every reuse age, so the next daemon reads both again.
    t.clock.advance(later(13 * 60));
    t.answer("a", extra_usage(roomy(), true));
    t.answer("b", extra_usage(roomy(), false));
    let second = t.reloaded();
    let (served, mut reads) = turn_reads(&second, &t.log).await;
    assert_eq!(served, anthropic("a"));
    reads.sort_unstable();
    assert_eq!(reads, ["a", "b"], "both read again");

    t.clock.advance(later(45 * 60));
    for label in ["a", "b"] {
        t.answer(label, Reply::RateLimited(120));
    }
    second.stop_saving_usage_readings();
    let third = t.reloaded();
    let (served, reads) = try_turn_reads(&third, &t.log).await;
    assert_eq!(served, Ok(anthropic("b")), "b's later reading shows extra usage off, a's on");
    assert_eq!(reads, ["a"], "one 429, then the pause");
}

/// S3 (guard): a window used up with no reset keeps the account out with
/// the read failure, as before: `usage_read_failed`.
#[tokio::test]
#[serial_test::serial]
async fn a_spent_window_without_a_reset_stays_a_failed_read_in_a_429_wall() {
    let t = clocked(&["a"]).await;
    t.answer("a", used_up_at(json!(null)));
    let (failure, _) = failed_turn(&t.broker, &t.log).await;
    assert_ne!(failure["kind"], "usage_read_failed", "{failure}");

    t.clock.advance(later(10 * 60 + 1));
    t.answer("a", Reply::RateLimited(120));
    let (failure, reads) = failed_turn(&t.broker, &t.log).await;
    assert_eq!(reads, ["a"]);
    assert_eq!(failure["kind"], "usage_read_failed", "{failure}");
    assert_eq!(failed_account(&failure, "a")["kind"], "usage_rate_limited", "{failure}");
}

/// S3 (guard): so does a window whose reset had already passed when it was
/// read (no evidence of a new window).
#[tokio::test]
#[serial_test::serial]
async fn a_spent_window_past_its_reset_when_read_stays_a_failed_read_in_a_429_wall() {
    let t = clocked(&["a"]).await;
    let passed = rfc3339(t.clock.now_ms() - 60_000);
    t.answer("a", used_up_at(json!(passed)));
    let (failure, _) = failed_turn(&t.broker, &t.log).await;
    assert_ne!(failure["kind"], "usage_read_failed", "{failure}");

    t.clock.advance(later(10 * 60 + 1));
    t.answer("a", Reply::RateLimited(120));
    let (failure, reads) = failed_turn(&t.broker, &t.log).await;
    assert_eq!(reads, ["a"]);
    assert_eq!(failure["kind"], "usage_read_failed", "{failure}");
}

/// S4 (guard): past 30 minutes, readings with extra usage on, or that say
/// nothing about it, no longer count: `usage_read_failed`, as before.
#[tokio::test]
#[serial_test::serial]
async fn past_30_min_extra_usage_on_or_unknown_stops_at_the_grace() {
    let t = clocked(&["a", "b"]).await;
    t.answer("a", extra_usage(roomy(), true));
    assert_eq!(turn(&t.broker).await, anthropic("a"));

    t.clock.advance(later(45 * 60));
    for label in ["a", "b"] {
        t.answer(label, Reply::RateLimited(120));
    }
    let (failure, reads) = failed_turn(&t.broker, &t.log).await;
    assert_eq!(reads, ["a"]);
    assert_eq!(failure["kind"], "usage_read_failed", "{failure}");
}

/// S4 (guard): only a usage 429 outlasts the grace. A 503 from the usage
/// endpoint, 45 minutes on, keeps an account with extra usage off out.
#[tokio::test]
#[serial_test::serial]
async fn past_30_min_a_usage_5xx_stops_at_the_grace_even_with_extra_usage_off() {
    let t = clocked(&["a"]).await;
    t.answer("a", extra_usage(roomy(), false));
    assert_eq!(turn(&t.broker).await, anthropic("a"));

    t.clock.advance(later(45 * 60));
    t.answer("a", Reply::Status(503));
    let (failure, reads) = failed_turn(&t.broker, &t.log).await;
    assert_eq!(reads, ["a"]);
    assert_eq!(failure["kind"], "usage_read_failed", "{failure}");
    assert_eq!(failed_account(&failure, "a")["kind"], "usage_upstream", "{failure}");
}

/// S4 (guard): a reported limit drops the reading, so its flag can't bring
/// the account back: after the cooldown, in a 429 wall 45 minutes on, `a`
/// (extra usage off) has no reading to serve on.
#[tokio::test]
#[serial_test::serial]
async fn a_dropped_reading_with_extra_usage_off_never_serves_a_429_wall() {
    let t = clocked(&["a", "b"]).await;
    t.answer("a", extra_usage(roomy(), false));
    t.answer("b", extra_usage(roomy(), true));
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.broker
        .report_cooldown(
            &anthropic("a"),
            Some(t.clock.now_ms() + 60_000),
            "anthropic_quota_exhausted",
        )
        .await
        .unwrap();

    t.clock.advance(later(45 * 60));
    for label in ["a", "b"] {
        t.answer(label, Reply::RateLimited(120));
    }
    let (failure, reads) = failed_turn(&t.broker, &t.log).await;
    assert_eq!(reads, ["a"]);
    assert_eq!(failure["kind"], "usage_read_failed", "{failure}");
}

/// N (guard): near its limit with extra usage on, the pinned account is
/// read again after 60 s, as before.
#[tokio::test]
#[serial_test::serial]
async fn a_near_limit_pinned_account_with_extra_usage_on_is_read_again_after_60s() {
    let t = clocked(&["a"]).await;
    t.answer("a", extra_usage(near_limit(), true));
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(61));
    assert_eq!(turn_reads(&t.broker, &t.log).await, (anthropic("a"), vec!["a"]));
}

/// Fresh readings are judged as before: a fresh reading with extra usage
/// off and a window used up until a reset ahead keeps the account out.
#[tokio::test]
#[serial_test::serial]
async fn a_fresh_spent_reading_with_extra_usage_off_is_still_exhausted() {
    let t = clocked(&["a"]).await;
    let reset = whole_second(&t, HOUR_MS);
    t.answer("a", extra_usage(used_up_at(json!(rfc3339(reset))), false));
    let (failure, reads) = failed_turn(&t.broker, &t.log).await;
    assert_eq!(reads, ["a"]);
    assert_eq!(failure["kind"], "all_cooling_down", "{failure}");
    assert_eq!(failure["earliest_reset_ms"], json!(reset), "{failure}");
}

// ── synaps status: the last known usage ──────────────────────────────────────

/// `synaps status`'s broker: another process, with a usage-read table of
/// its own, that loads the daemon's saved readings and never writes them.
fn status_broker(t: &Clocked) -> LocalBroker {
    auto_broker(&t.usage_url)
        .with_test_clock(&t.clock)
        .with_new_usage_table()
}

/// What `synaps status` gets for one account.
type Shown = Result<agent_core::auth::UsageSnapshot, Box<agent_core::auth::UsageReadFailure>>;

/// What `synaps status` gets for `label`, and whose usage it read for it.
async fn status_reads(t: &Clocked, broker: &LocalBroker, label: &str) -> (Shown, Vec<&'static str>) {
    let mark = t.log.lock().unwrap().len();
    let shown = broker.usage_or_last_known(&anthropic(label)).await;
    let reads = t.log.lock().unwrap()[mark..].to_vec();
    (shown, reads)
}

/// The rate-limit part of a status read that failed.
fn limited(shown: Shown) -> agent_core::auth::RateLimitedRead {
    shown
        .expect_err("no reading")
        .rate_limited
        .expect("a rate-limited read")
}

/// A usage 429 on status's own read shows the account's last good reading
/// (the daemon's saved one, at the time it was read) and when this process
/// reads the account again; asking again within that time sends nothing.
/// The 429 also starts the pause: the next account is not read, and shows
/// its own last good reading and the pause's end.
#[tokio::test]
#[serial_test::serial]
async fn a_usage_429_in_status_shows_the_last_good_reading_and_the_pause_sends_nothing() {
    let t = clocked_saving(&["a", "b"]).await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(13 * 60));
    t.answer("a", Reply::RateLimited(90));
    let status = status_broker(&t);

    let (shown, reads) = status_reads(&t, &status, "a").await;
    assert_eq!(reads, ["a"]);
    let a = limited(shown);
    let now = t.clock.now_ms();
    assert!(
        a.retry_at_ms >= now + 90_000,
        "the broker's retry time, in {} ms",
        a.retry_at_ms.saturating_sub(now)
    );
    let last_good = a.last_good.expect("a's saved reading");
    assert!(
        now.saturating_sub(last_good.observed_at) >= 12 * 60 * 1000,
        "it keeps the time it was read"
    );
    let (shown, reads) = status_reads(&t, &status, "a").await;
    assert!(reads.is_empty(), "held: {reads:?}");
    assert_eq!(limited(shown).retry_at_ms, a.retry_at_ms);

    let (shown, reads) = status_reads(&t, &status, "b").await;
    assert!(reads.is_empty(), "paused: {reads:?}");
    let b = limited(shown);
    assert_eq!(b.retry_at_ms, a.retry_at_ms, "the pause's end");
    assert!(b.last_good.is_some(), "b's saved reading");
}

/// The last good reading a 429 shows in status is only one read from the
/// account stored in the slot now: after a re-login of `a` to another
/// account id there is none (the 429 and its retry time still show).
#[tokio::test]
#[serial_test::serial]
async fn a_usage_429_in_status_never_shows_another_account_s_reading() {
    let t = clocked_saving(&["a"]).await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(13 * 60));
    set_account_id(&t, "a", "acct-a-second");
    t.answer("a", Reply::RateLimited(90));
    let (shown, reads) = status_reads(&t, &status_broker(&t), "a").await;
    assert_eq!(reads, ["a"]);
    let a = limited(shown);
    assert!(a.retry_at_ms > t.clock.now_ms());
    assert!(a.last_good.is_none(), "{:?}", a.last_good);
}

/// Any other failed read keeps today's failure in status: a usage 5xx
/// gives no retry time and no last good reading, and the error is the one
/// `usage` gives (here for the held 5xx, without a request).
#[tokio::test]
#[serial_test::serial]
async fn a_usage_5xx_in_status_shows_no_last_good_reading() {
    let t = clocked_saving(&["a"]).await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(13 * 60));
    t.answer("a", Reply::Status(503));
    let status = status_broker(&t);
    let (shown, reads) = status_reads(&t, &status, "a").await;
    assert_eq!(reads, ["a"]);
    let failure = shown.expect_err("a 503 is no reading");
    assert!(failure.rate_limited.is_none(), "{:?}", failure.rate_limited);
    let mark = t.log.lock().unwrap().len();
    let usage = status.usage(&anthropic("a")).await.expect_err("held");
    assert_eq!(t.log.lock().unwrap().len(), mark, "held: no request");
    assert_eq!(usage.to_string(), failure.error.to_string());
}

/// Status shows a saved reading younger than the broker's own reuse age
/// for the account, and sends no request for it: 5 min old with room
/// (10 min), and 5 min old near its limit with extra usage off (10 min).
#[tokio::test]
#[serial_test::serial]
async fn status_shows_a_saved_reading_under_its_reuse_age_without_a_read() {
    let t = clocked_saving(&["a", "b"]).await;
    t.answer("b", extra_usage(near_limit(), false));
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(5 * 60));
    let status = status_broker(&t);
    for label in ["a", "b"] {
        let (shown, reads) = status_reads(&t, &status, label).await;
        assert!(reads.is_empty(), "{label}: {reads:?}");
        let reading = shown.expect("the saved reading");
        assert!(
            t.clock.now_ms().saturating_sub(reading.observed_at) >= 4 * 60 * 1000,
            "{label}: at the time it was read"
        );
    }
}

/// Status reads an account whose saved reading is past the broker's reuse
/// age for it: near its limit with extra usage unknown or on (60 s), one
/// whose reset has passed since, and, at 11 min, one with room.
#[tokio::test]
#[serial_test::serial]
async fn status_reads_an_account_whose_saved_reading_is_past_its_reuse_age() {
    let t = clocked_saving(&["a", "b", "c", "d"]).await;
    t.answer("b", near_limit());
    t.answer("c", extra_usage(near_limit(), true));
    let soon = rfc3339(t.clock.now_ms() + 60_000);
    t.answer("d", used_up(&soon));
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(2 * 60));
    let status = status_broker(&t);
    for (label, read) in [("a", false), ("b", true), ("c", true), ("d", true)] {
        let (shown, reads) = status_reads(&t, &status, label).await;
        shown.expect("a reading");
        assert_eq!(reads.len(), usize::from(read), "{label} at 2 min: {reads:?}");
    }
    t.clock.advance(later(9 * 60));
    let (shown, reads) = status_reads(&t, &status_broker(&t), "a").await;
    shown.expect("a reading");
    assert_eq!(reads, ["a"], "a at 11 min");
}

/// A status run writes nothing: through a read with room, a usage 429 and
/// a paused read, the saved readings keep their bytes and their time, and
/// nothing is added beside them.
#[tokio::test]
#[serial_test::serial]
async fn a_status_run_never_changes_the_saved_readings() {
    let t = clocked_saving(&["a", "b", "c"]).await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(13 * 60));
    t.answer("b", Reply::RateLimited(90));
    let modified = || {
        std::fs::metadata(saved_readings(&t))
            .unwrap()
            .modified()
            .unwrap()
    };
    let (before, mtime) = (snapshot_dir(t._home.path()), modified());
    let status = status_broker(&t);
    let mut reads = Vec::new();
    for label in ["a", "b", "c"] {
        let (shown, read) = status_reads(&t, &status, label).await;
        reads.extend(read);
        assert_eq!(shown.is_ok(), label == "a", "{label}");
    }
    assert_eq!(reads, ["a", "b"], "c is paused");
    assert_eq!(snapshot_dir(t._home.path()), before);
    assert_eq!(modified(), mtime);
}

/// During the saved pause, status shows a saved reading under its reuse age
/// in the rate-limited form: that reading as the last good one, the pause's
/// end, and no request. Without a pause the same reading is shown as a
/// reading (the control). The row doesn't turn on the reading's age.
#[tokio::test]
#[serial_test::serial]
async fn during_the_saved_pause_status_shows_a_young_reading_as_rate_limited() {
    let t = clocked_saving(&["a", "b"]).await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(60));
    let (shown, reads) = status_reads(&t, &status_broker(&t), "a").await;
    assert!(reads.is_empty(), "no pause: {reads:?}");
    let young = shown.expect("a's saved reading, with no pause");

    // The daemon's own read of b gets a usage 429: the pause, saved.
    t.answer("b", Reply::RateLimited(90));
    t.broker.usage(&anthropic("b")).await.expect_err("a 429");
    t.clock.advance(later(60));
    let (shown, reads) = status_reads(&t, &status_broker(&t), "a").await;
    assert!(reads.is_empty(), "paused: {reads:?}");
    let a = limited(shown);
    assert!(a.retry_at_ms > t.clock.now_ms(), "the pause's end");
    assert_eq!(a.last_good, Some(young), "a's saved reading");
}

/// The same once status's own usage 429 starts the pause: b's saved reading
/// is past its reuse age (near its limit, 2 min old), so b is read and gets
/// a 429; a's 2-min-old saved reading then shows as rate-limited, with that
/// same reading as the last good one, the pause's end and no request.
#[tokio::test]
#[serial_test::serial]
async fn after_a_usage_429_in_status_a_young_reading_shows_as_rate_limited() {
    let t = clocked_saving(&["a", "b"]).await;
    t.answer("b", near_limit());
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(2 * 60));
    let (shown, reads) = status_reads(&t, &status_broker(&t), "a").await;
    assert!(reads.is_empty(), "no pause yet: {reads:?}");
    let saved = shown.expect("a's saved reading, before b's 429");

    t.answer("b", Reply::RateLimited(90));
    let status = status_broker(&t);
    let (shown, reads) = status_reads(&t, &status, "b").await;
    assert_eq!(reads, ["b"]);
    let b = limited(shown);

    let (shown, reads) = status_reads(&t, &status, "a").await;
    assert!(reads.is_empty(), "paused: {reads:?}");
    let a = limited(shown);
    assert_eq!(a.retry_at_ms, b.retry_at_ms, "the pause's end");
    assert!(a.retry_at_ms > t.clock.now_ms(), "the pause is still on");
    let last_good = a.last_good.expect("a's saved reading");
    assert_eq!(last_good, saved, "exactly a's saved reading");
    // The saved reading keeps the time the usage-read table recorded (the
    // wall clock at the read, a little after the test clock's start), so
    // about 2 min ago: well before this run.
    assert!(
        t.clock.now_ms().saturating_sub(last_good.observed_at) >= 60 * 1000,
        "at the time it was read"
    );
}

/// Status's reuse age is inclusive, as the broker's own: a saved reading
/// exactly 10 min old is shown without a request, and one 10 min and 1 ms
/// old is read.
#[tokio::test]
#[serial_test::serial]
async fn status_reuses_a_reading_exactly_its_reuse_age_old_and_reads_one_past_it() {
    let t = clocked_saving(&["a"]).await;
    assert_eq!(turn(&t.broker).await, anthropic("a"));
    t.clock.advance(later(5 * 60));
    let status = status_broker(&t);
    let (shown, reads) = status_reads(&t, &status, "a").await;
    assert!(reads.is_empty(), "5 min: {reads:?}");
    // The reading's time comes from the read, not the test clock's start:
    // move the clock to exactly its reuse age.
    let observed_at = shown.expect("the saved reading").observed_at;
    let exactly = observed_at + 10 * 60 * 1000 - t.clock.now_ms();
    t.clock.advance(std::time::Duration::from_millis(exactly));
    let (shown, reads) = status_reads(&t, &status, "a").await;
    assert!(reads.is_empty(), "exactly the age: {reads:?}");
    let reading = shown.expect("the saved reading");
    assert_eq!(
        t.clock.now_ms().saturating_sub(reading.observed_at),
        10 * 60 * 1000,
        "exactly the age"
    );
    t.clock.advance(std::time::Duration::from_millis(1));
    let (shown, reads) = status_reads(&t, &status, "a").await;
    shown.expect("a reading");
    assert_eq!(reads, ["a"], "the age and 1 ms");
}

/// A status run with no saved readings and no lock beside the store stays
/// a reader: through a read with room, a usage 429 and a paused read,
/// neither file appears and the store's directory is unchanged.
#[tokio::test]
#[serial_test::serial]
async fn a_status_run_with_no_saved_readings_creates_no_file() {
    let t = clocked(&["a", "b", "c"]).await;
    for label in ["a", "b", "c"] {
        set_account_id(&t, label, &format!("acct-{label}"));
    }
    t.answer("b", Reply::RateLimited(90));
    let lock = t._home.path().join("usage-last-good.lock");
    assert!(!saved_readings(&t).exists(), "no saved readings to start");
    assert!(!lock.exists(), "no lock to start");
    let before = snapshot_dir(t._home.path());
    let status = status_broker(&t);
    let mut reads = Vec::new();
    for label in ["a", "b", "c"] {
        let (shown, read) = status_reads(&t, &status, label).await;
        reads.extend(read);
        assert_eq!(shown.is_ok(), label == "a", "{label}");
    }
    assert_eq!(reads, ["a", "b"], "c is paused");
    assert!(!saved_readings(&t).exists(), "no saved readings");
    assert!(!lock.exists(), "no lock");
    assert_eq!(snapshot_dir(t._home.path()), before);
}

// ── A4: Fable's weekly limit, and a limit of Fable's requests only ───────────

const OPUS_MODEL: &str = "claude-opus-5-5";
const FABLE_MODEL: &str = "claude-fable-5-1";

/// A usage reading with the shared windows at `all` % and Fable's weekly
/// limit, in the `limits` list, at `fable` % (with the repeats of the shared
/// windows the list also carries).
fn with_fable(all: f64, fable: f64) -> Reply {
    Reply::Body(json!({
        "five_hour": {"utilization": all, "resets_at": NEVER_RESETS},
        "seven_day": {"utilization": all, "resets_at": NEVER_RESETS},
        "limits": [
            {"kind": "session", "percent": all, "resets_at": NEVER_RESETS},
            {"kind": "weekly_all", "percent": all, "resets_at": NEVER_RESETS},
            {"kind": "weekly_scoped", "scope": {"model": {"display_name": "Fable"}},
             "percent": fable, "resets_at": NEVER_RESETS}
        ]
    }))
}

/// One automatic vend for `model`: the account that serves, or the error.
async fn try_model_turn(
    broker: &LocalBroker,
    model: Option<&str>,
) -> Result<CredentialRef, BrokerError> {
    tokio::time::timeout(
        SELECTION_DEADLINE,
        broker.access_token_pinned_for(OAuthProviderId::Anthropic, model),
    )
    .await
    .expect("the selection terminates")
    .map(|token| token.credential)
}

/// Report a limit of `label`'s Fable requests until `until_ms`.
async fn cool_for_fable(broker: &LocalBroker, label: &str, until_ms: u64) {
    broker
        .report_model_cooldown(
            &anthropic(label),
            "fable",
            Some(until_ms),
            "anthropic_quota_exhausted",
        )
        .await
        .unwrap();
}

/// A spent Fable limit stops the account's Fable requests only: Opus is
/// served by it, Fable by the account with room in Fable's limit.
#[tokio::test]
#[serial_test::serial]
async fn a_spent_fable_limit_stops_fable_requests_and_no_other_model() {
    let t = clocked(&["a", "b"]).await;
    t.answer("a", with_fable(40.0, 100.0));
    t.answer("b", with_fable(40.0, 5.0));
    let (served, _) = model_turn_reads(&t.broker, &t.log, OPUS_MODEL).await;
    assert_eq!(served, anthropic("a"), "Opus isn't under Fable's limit");
    let (served, _) = model_turn_reads(&t.broker, &t.log, FABLE_MODEL).await;
    assert_eq!(served, anthropic("b"), "a's Fable limit is spent");
}

/// Fable's limit is inside the shared one: an account whose shared windows
/// are spent serves neither Opus nor Fable, whatever room Fable's own limit
/// shows.
#[tokio::test]
#[serial_test::serial]
async fn a_spent_shared_limit_stops_fable_even_with_room_in_fables_own() {
    let t = clocked(&["a", "b"]).await;
    t.answer("a", with_fable(100.0, 5.0));
    t.answer("b", with_fable(10.0, 5.0));
    for model in [OPUS_MODEL, FABLE_MODEL] {
        let (served, _) = model_turn_reads(&t.broker, &t.log, model).await;
        assert_eq!(served, anthropic("b"), "{model}");
    }
}

/// A limit of a's Fable requests keeps a for Opus, on its kept reading
/// with no read, and stops a for Fable although that reading shows room in
/// Fable's limit (the cooldown wins), without reading a for it. A limit of
/// the whole account still drops the reading, as before: once it is over,
/// a is read again.
#[tokio::test]
#[serial_test::serial]
async fn a_fable_only_cooldown_stops_fable_requests_and_keeps_the_reading() {
    let t = clocked(&["a", "b"]).await;
    for label in ["a", "b"] {
        t.answer(label, with_fable(10.0, 10.0));
    }
    let (served, _) = model_turn_reads(&t.broker, &t.log, OPUS_MODEL).await;
    assert_eq!(served, anthropic("a"));
    cool_for_fable(&t.broker, "a", agent_core::epoch_millis() + 3_600_000).await;
    t.clock.advance(later(60));

    let (served, reads) = model_turn_reads(&t.broker, &t.log, OPUS_MODEL).await;
    assert_eq!(served, anthropic("a"), "Opus keeps a");
    assert!(reads.is_empty(), "on the kept reading: {reads:?}");
    let (served, reads) = model_turn_reads(&t.broker, &t.log, FABLE_MODEL).await;
    assert_eq!(served, anthropic("b"), "the cooldown wins over the reading's room");
    assert!(!reads.contains(&"a"), "a is not read for Fable: {reads:?}");

    t.broker
        .report_cooldown(
            &anthropic("a"),
            Some(t.clock.now_ms() + 30_000),
            "anthropic_quota_exhausted",
        )
        .await
        .unwrap();
    t.clock.advance(later(2 * 60));
    let (served, reads) = model_turn_reads(&t.broker, &t.log, OPUS_MODEL).await;
    assert_eq!(served, anthropic("a"));
    assert!(reads.contains(&"a"), "the whole-account limit dropped a's reading: {reads:?}");
}

/// A limit of the whole account (what a Fable request refused with unknown
/// headers reports, A4 r3) keeps the account from Opus requests too: Opus
/// is served by another account until it is over.
#[tokio::test]
#[serial_test::serial]
async fn a_whole_account_limit_stops_opus_requests_too() {
    let t = clocked(&["a", "b"]).await;
    let (first, _) = model_turn_reads(&t.broker, &t.log, OPUS_MODEL).await;
    t.broker
        .report_cooldown(
            &first,
            Some(t.clock.now_ms() + HOUR_MS),
            "anthropic_quota_exhausted",
        )
        .await
        .unwrap();
    let (served, _) = model_turn_reads(&t.broker, &t.log, OPUS_MODEL).await;
    assert_ne!(served, first, "the account cooling for every model is not chosen");
}

/// Every account cooling for Fable: a Fable request fails as
/// all_cooling_down with the earliest of those cooldowns, reading nothing,
/// and an Opus request is served.
#[tokio::test]
#[serial_test::serial]
async fn every_account_cooling_for_fable_fails_fable_requests_as_all_cooling_down() {
    let t = clocked(&["a", "b"]).await;
    for label in ["a", "b"] {
        t.answer(label, with_fable(10.0, 10.0));
    }
    let now = agent_core::epoch_millis();
    let (until_a, until_b) = (now + 2 * 3_600_000, now + 3_600_000);
    cool_for_fable(&t.broker, "a", until_a).await;
    cool_for_fable(&t.broker, "b", until_b).await;
    let err = try_model_turn(&t.broker, Some(FABLE_MODEL))
        .await
        .expect_err("every account is cooling for Fable");
    let BrokerError::NoAccountAvailable {
        failure: Some(failure),
        ..
    } = &err
    else {
        panic!("typed selection failure expected: {err:?}");
    };
    let failure = serde_json::to_value(failure).unwrap();
    assert_eq!(failure["kind"], "all_cooling_down", "{failure}");
    assert_eq!(failure["earliest_reset_ms"], until_b, "{failure}");
    assert!(t.log.lock().unwrap().is_empty(), "nothing read for Fable");
    let served = try_model_turn(&t.broker, Some(OPUS_MODEL))
        .await
        .map_err(|e| e.to_string());
    assert_eq!(served, Ok(anthropic("a")), "Opus is served");
}

/// R2: a selection without a model (the CLI's token, the keeper) fails
/// closed, as for any model window: an account whose Fable limit is spent,
/// or that is cooling for Fable, is not chosen.
#[tokio::test]
#[serial_test::serial]
async fn a_selection_without_a_model_counts_a_fable_limit() {
    let t = clocked(&["a", "b"]).await;
    t.answer("a", with_fable(10.0, 100.0));
    t.answer("b", with_fable(10.0, 5.0));
    assert_eq!(turn(&t.broker).await, anthropic("b"), "a's Fable limit is spent");

    let t = clocked(&["a", "b"]).await;
    for label in ["a", "b"] {
        t.answer(label, with_fable(10.0, 5.0));
    }
    cool_for_fable(&t.broker, "a", agent_core::epoch_millis() + 3_600_000).await;
    assert_eq!(turn(&t.broker).await, anthropic("b"), "a is cooling for Fable");
}

/// R1: Fable's window is saved with its model scope (the scope is in the
/// saved bytes; a loader, this one or an older binary's, doesn't derive it
/// from the id), and a reload gives it back as Fable's window only: Fable
/// is still stopped and Opus served, with no read.
#[tokio::test]
#[serial_test::serial]
async fn the_saved_readings_keep_fables_window_scoped_to_fable() {
    let t = clocked_saving(&["a", "b"]).await;
    t.answer("a", with_fable(10.0, 100.0));
    t.answer("b", with_fable(10.0, 5.0));
    let (served, _) = model_turn_reads(&t.broker, &t.log, OPUS_MODEL).await;
    assert_eq!(served, anthropic("a"));
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(saved_readings(&t)).unwrap()).unwrap();
    let fable: Vec<&serde_json::Value> = saved["readings"]
        .as_object()
        .expect("saved readings")
        .values()
        .flat_map(|reading| reading["snapshot"]["windows"].as_array().unwrap())
        .filter(|window| window["id"] == "seven_day_fable")
        .collect();
    assert_eq!(fable.len(), 2, "{saved}");
    for window in fable {
        assert_eq!(window["scope"], json!({"kind": "model", "model": "fable"}), "{saved}");
    }

    // Half a minute on: the saved readings are not newer than the clock.
    t.clock.advance(later(30));
    let broker = t.reloaded();
    let (served, reads) = model_turn_reads(&broker, &t.log, FABLE_MODEL).await;
    assert_eq!(served, anthropic("b"), "a's saved Fable limit is spent");
    assert!(reads.is_empty(), "{reads:?}");
    let (served, reads) = model_turn_reads(&broker, &t.log, OPUS_MODEL).await;
    assert_eq!(served, anthropic("a"), "and limits Fable only");
    assert!(reads.is_empty(), "{reads:?}");
}

/// R5: `auth state --clear` of an account ends a limit of its Fable
/// requests too, as it ends a limit of the whole account.
#[tokio::test]
#[serial_test::serial]
async fn auth_state_clear_ends_a_fable_only_cooldown() {
    let t = clocked(&["a", "b"]).await;
    for label in ["a", "b"] {
        t.answer(label, with_fable(10.0, 5.0));
    }
    cool_for_fable(&t.broker, "a", agent_core::epoch_millis() + 3_600_000).await;
    let without_b = [anthropic("b")];
    let fable_without_b = t
        .broker
        .access_token_pinned_excluding(OAuthProviderId::Anthropic, Some(FABLE_MODEL), &without_b)
        .await;
    assert!(fable_without_b.is_err(), "a is cooling for Fable");
    t.broker
        .auth_state(&AuthStateQuery {
            account: Some("a".into()),
            clear: true,
            ..anthropic_query()
        })
        .unwrap();
    let served = t
        .broker
        .access_token_pinned_excluding(OAuthProviderId::Anthropic, Some(FABLE_MODEL), &without_b)
        .await
        .map(|token| token.credential)
        .map_err(|e| e.to_string());
    assert_eq!(served, Ok(anthropic("a")), "a serves Fable again");
}
