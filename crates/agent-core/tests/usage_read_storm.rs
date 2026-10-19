//! The usage-endpoint 429 wall: failed usage reads are held and shared
//! process-wide, and a recent good reading keeps an account selectable
//! while its usage read fails for a transient reason.
//!
//! Integration binary (serial): `SYNAPS_BASE_DIR` is redirected to a
//! throwaway store per test, and the fake usage endpoint is loopback only.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_core::auth::{
    AccountFailureKind, AccountPolicy, AccountSelector, BrokerError, CredentialBroker, LocalBroker,
    OAuthProviderId, SelectionFailure, SelectionFailureKind,
};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

const FAR_FUTURE: u64 = 4_102_444_800_000; // year 2100: never refreshes
const HOUR_S: u64 = 3600;

/// Throwaway store with one Anthropic slot per label; slot `x` sends the
/// bearer `x-access`.
fn install(labels: &[&str]) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    agent_core::config::set_base_dir_for_tests(home.path().to_path_buf());
    let mut data = serde_json::Map::new();
    for label in labels {
        data.insert(
            format!("anthropic@{label}"),
            json!({ "type": "oauth", "access": format!("{label}-access"),
                    "refresh": format!("{label}-refresh-SECRET"), "expires": FAR_FUTURE }),
        );
    }
    std::fs::write(
        home.path().join("auth.json"),
        serde_json::Value::Object(data).to_string(),
    )
    .unwrap();
    std::fs::write(home.path().join("config"), "").unwrap();
    home
}

/// How the fake usage endpoint answers one account.
#[derive(Clone, Copy, Debug)]
enum Answer {
    /// Seven-day window at `used` percent, resetting in `reset_in_s`.
    Ok { used: f64, reset_in_s: u64 },
    Status(u16),
}

/// Fake usage endpoint: per-account answers, per-account request counts,
/// optional answer delay (to hold a read in flight).
#[derive(Clone, Default)]
struct Upstream {
    answers: Arc<Mutex<HashMap<String, Answer>>>,
    hits: Arc<Mutex<HashMap<String, usize>>>,
    delay_ms: Arc<AtomicU64>,
}

impl Upstream {
    fn set(&self, label: &str, answer: Answer) {
        self.answers.lock().unwrap().insert(label.into(), answer);
    }
    fn all(&self, labels: &[&str], answer: Answer) {
        for label in labels {
            self.set(label, answer);
        }
    }
    fn hits(&self, label: &str) -> usize {
        self.hits.lock().unwrap().get(label).copied().unwrap_or(0)
    }

    async fn spawn(&self) -> String {
        let up = self.clone();
        let app = Router::new().route(
            "/usage",
            get(move |headers: HeaderMap| {
                let up = up.clone();
                async move {
                    let bearer = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    let label = bearer
                        .trim_start_matches("Bearer ")
                        .trim_end_matches("-access")
                        .to_string();
                    *up.hits.lock().unwrap().entry(label.clone()).or_default() += 1;
                    let delay = up.delay_ms.load(Ordering::SeqCst);
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                    let answer = up.answers.lock().unwrap().get(&label).copied();
                    match answer.unwrap_or(Answer::Status(500)) {
                        Answer::Ok { used, reset_in_s } => {
                            let at = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(
                                (agent_core::epoch_millis() + reset_in_s * 1000) as i64,
                            )
                            .unwrap()
                            .to_rfc3339();
                            Json(json!({ "seven_day": { "utilization": used, "resets_at": at } }))
                                .into_response()
                        }
                        Answer::Status(s) => StatusCode::from_u16(s).unwrap().into_response(),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/usage")
    }
}

/// Auto-selecting broker on the fake endpoint. Readings count as fresh for
/// one second, so a test re-reads after `past_fresh_age`.
fn auto_broker(url: &str) -> LocalBroker {
    LocalBroker::new(reqwest::Client::new())
        .with_account_policy(AccountPolicy::new().with(OAuthProviderId::Anthropic, AccountSelector::Auto))
        .with_usage_endpoint_override(url)
        .with_max_snapshot_age(Duration::from_secs(1))
}

async fn past_fresh_age() {
    tokio::time::sleep(Duration::from_millis(1100)).await;
}

async fn chosen(broker: &LocalBroker) -> Result<String, BrokerError> {
    broker
        .access_token_pinned(OAuthProviderId::Anthropic)
        .await
        .map(|p| p.credential.account.label_str().to_string())
}

const URGENT: Answer = Answer::Ok {
    used: 10.0,
    reset_in_s: 6 * HOUR_S,
};
const LATER: Answer = Answer::Ok {
    used: 10.0,
    reset_in_s: 48 * HOUR_S,
};
const LIMITED: Answer = Answer::Status(429);

/// The wall: the usage endpoint answers 429 for every account. An account
/// with a good reading from minutes ago stays selectable (it was a real
/// request each time), and the chosen account stays chosen.
#[tokio::test]
#[serial_test::serial]
async fn every_usage_read_rate_limited_with_a_recent_good_reading_still_selects() {
    let _home = install(&["a", "b"]);
    let up = Upstream::default();
    up.set("a", URGENT);
    up.set("b", LATER);
    let broker = auto_broker(&up.spawn().await);
    assert_eq!(chosen(&broker).await.unwrap(), "a");

    up.all(&["a", "b"], LIMITED);
    past_fresh_age().await;
    assert_eq!(
        chosen(&broker)
            .await
            .expect("a usage-endpoint 429 must not end the turn while a recent good reading exists"),
        "a"
    );
    assert_eq!((up.hits("a"), up.hits("b")), (2, 2), "the failed reads were real requests");
}

/// No good reading to fall back on: the selection refuses, typed as a
/// usage-read failure on every account with the time reads resume (the
/// held 2-minute backoff), and each account's part names its read failure.
/// A refused login on one account keeps its login advice.
#[tokio::test]
#[serial_test::serial]
async fn every_usage_read_failing_and_no_good_reading_refuses_with_the_read_time() {
    /// The typed failure of a refused selection.
    fn failure_of(err: &BrokerError) -> &SelectionFailure {
        match err {
            BrokerError::NoAccountAvailable {
                failure: Some(f), ..
            } => f,
            other => panic!("{other:?}"),
        }
    }
    fn kinds(f: &SelectionFailure) -> Vec<(String, AccountFailureKind, Option<u16>)> {
        let by_label: std::collections::BTreeMap<_, _> = f
            .accounts
            .iter()
            .map(|a| (a.account.clone(), (a.kind, a.http_status)))
            .collect();
        by_label
            .into_iter()
            .map(|(label, (kind, status))| (label, kind, status))
            .collect()
    }

    let _home = install(&["a", "b"]);
    let up = Upstream::default();
    up.all(&["a", "b"], LIMITED);
    let broker = auto_broker(&up.spawn().await);
    let before = agent_core::epoch_millis();
    let err = chosen(&broker).await.unwrap_err();
    let after = agent_core::epoch_millis();
    let f = failure_of(&err);
    assert_eq!(f.kind, SelectionFailureKind::UsageReadFailed);
    assert_eq!(
        kinds(f),
        vec![
            ("a".into(), AccountFailureKind::UsageRateLimited, Some(429)),
            ("b".into(), AccountFailureKind::UsageRateLimited, Some(429)),
        ]
    );
    let retry = f
        .usage_read_retry_ms
        .expect("every account failed on a transient usage read");
    assert!(
        retry >= before + 120_000 && retry <= after + 120_000,
        "reads resume after the first 2-minute hold: {retry} in [{before}, {after}] + 120s"
    );
    assert!(f.login_labels().is_empty(), "{f:?}");
    assert!(!err.to_string().contains("SECRET"));

    // A 5xx on one account: still a held read failure with a read time.
    let _home = install(&["a", "b"]);
    let up = Upstream::default();
    up.set("a", LIMITED);
    up.set("b", Answer::Status(503));
    let broker = auto_broker(&up.spawn().await);
    let err = chosen(&broker).await.unwrap_err();
    let f = failure_of(&err);
    assert_eq!(f.kind, SelectionFailureKind::UsageReadFailed);
    assert_eq!(
        kinds(f),
        vec![
            ("a".into(), AccountFailureKind::UsageRateLimited, Some(429)),
            ("b".into(), AccountFailureKind::UsageUpstream, Some(503)),
        ]
    );
    assert!(f.usage_read_retry_ms.is_some(), "a 503 is held too: {f:?}");

    // A refused login on one account: the read failure still has its read
    // time, and only the refused account needs a login.
    let _home = install(&["a", "b"]);
    let up = Upstream::default();
    up.set("a", LIMITED);
    up.set("b", Answer::Status(401));
    let broker = auto_broker(&up.spawn().await);
    let err = chosen(&broker).await.unwrap_err();
    let f = failure_of(&err);
    assert_eq!(f.kind, SelectionFailureKind::UsageReadFailed, "{f:?}");
    assert_eq!(f.login_labels(), vec!["b"], "{f:?}");
    assert!(f.usage_read_retry_ms.is_some(), "{f:?}");
}

/// A good reading that showed an exhausted window never stands in for a
/// failed read: the other account (graced on its own good reading) is
/// chosen even though the exhausted one would reset first.
#[tokio::test]
#[serial_test::serial]
async fn a_good_reading_with_an_exhausted_window_is_never_graced() {
    let _home = install(&["a", "b"]);
    let up = Upstream::default();
    up.set(
        "a",
        Answer::Ok {
            used: 100.0,
            reset_in_s: 6 * HOUR_S,
        },
    );
    up.set("b", LATER);
    let url = up.spawn().await;
    let broker = auto_broker(&url);
    assert_eq!(chosen(&broker).await.unwrap(), "b");

    up.all(&["a", "b"], LIMITED);
    past_fresh_age().await;
    assert_eq!(chosen(&broker).await.unwrap(), "b");
    // A new broker in the same process (no sticky seat; the reads are held
    // and answered from the table): still b, never a.
    let other = auto_broker(&url);
    assert_eq!(chosen(&other).await.unwrap(), "b");
}

/// Concurrent failing reads of one account from TWO brokers in one process
/// send ONE request; a later read inside the hold (same or a new broker)
/// sends none. Concurrent successful reads are shared the same way.
#[tokio::test]
#[serial_test::serial]
async fn concurrent_failing_reads_from_two_brokers_send_one_request_per_account() {
    let _home = install(&["a", "b"]);
    let up = Upstream::default();
    up.all(&["a", "b"], LIMITED);
    up.delay_ms.store(300, Ordering::SeqCst);
    let url = up.spawn().await;
    let (one, two) = (auto_broker(&url), auto_broker(&url));
    let (p1, p2) = tokio::join!(
        one.plan(OAuthProviderId::Anthropic, None),
        two.plan(OAuthProviderId::Anthropic, None)
    );
    assert_eq!((up.hits("a"), up.hits("b")), (1, 1), "one request per account");
    for plan in [p1.unwrap(), p2.unwrap()] {
        assert!(plan.rows.iter().all(|r| r.rank.is_none()), "both saw the failure");
    }
    past_fresh_age().await;
    assert!(chosen(&one).await.is_err());
    let three = auto_broker(&url);
    assert!(chosen(&three).await.is_err());
    assert_eq!(
        (up.hits("a"), up.hits("b")),
        (1, 1),
        "a held failed read sends no request, whichever broker asks"
    );

    let _home = install(&["a", "b"]);
    let up = Upstream::default();
    up.set("a", URGENT);
    up.set("b", LATER);
    up.delay_ms.store(300, Ordering::SeqCst);
    let url = up.spawn().await;
    let (one, two) = (auto_broker(&url), auto_broker(&url));
    let (c1, c2) = tokio::join!(chosen(&one), chosen(&two));
    assert_eq!((c1.unwrap(), c2.unwrap()), ("a".to_string(), "a".to_string()));
    assert_eq!((up.hits("a"), up.hits("b")), (1, 1), "a read in flight is shared");
}
