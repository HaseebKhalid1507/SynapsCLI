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
    let slots: Vec<_> = labels.iter().map(|label| (*label, None, "1")).collect();
    install_slots(&slots)
}

/// Like [`install`], for slots `(label, account id, refresh generation)`.
fn install_slots(slots: &[(&str, Option<&str>, &str)]) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    agent_core::config::set_base_dir_for_tests(home.path().to_path_buf());
    write_slots(home.path(), slots);
    std::fs::write(home.path().join("config"), "").unwrap();
    home
}

/// (Re)write the store: a new refresh generation is a token rotation, a new
/// account id a re-login of the slot to another account.
fn write_slots(dir: &std::path::Path, slots: &[(&str, Option<&str>, &str)]) {
    let mut data = serde_json::Map::new();
    for (label, account_id, generation) in slots {
        let mut slot = json!({ "type": "oauth", "access": format!("{label}-access"),
                               "refresh": format!("{label}-refresh-{generation}-SECRET"),
                               "expires": FAR_FUTURE });
        if let Some(id) = account_id {
            slot["accountId"] = json!(id);
        }
        data.insert(format!("anthropic@{label}"), slot);
    }
    std::fs::write(dir.join("auth.json"), serde_json::Value::Object(data).to_string()).unwrap();
}

/// Codex access token carrying `account_id` (the usage read's account header).
fn codex_jwt(account_id: &str) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    let payload = json!({ "https://api.openai.com/auth": { "chatgpt_account_id": account_id } });
    format!("h.{}.s", URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap()))
}

/// Throwaway store with one Codex slot `label` on account `account_id`.
fn install_codex(label: &str, account_id: &str) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    agent_core::config::set_base_dir_for_tests(home.path().to_path_buf());
    let data = json!({
        (format!("openai-codex@{label}")): { "type": "oauth", "access": codex_jwt(account_id),
            "refresh": format!("{label}-refresh-SECRET"), "expires": FAR_FUTURE,
            "accountId": account_id }
    });
    std::fs::write(home.path().join("auth.json"), data.to_string()).unwrap();
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
/// optional answer delay (to hold a read in flight). Anthropic reads are
/// keyed by the bearer label (`x-access` -> `x`); Codex reads by their
/// account header, and get a Codex-shaped body.
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
                    let codex_account = headers
                        .get("chatgpt-account-id")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    let label = codex_account.clone().unwrap_or_else(|| {
                        bearer
                            .trim_start_matches("Bearer ")
                            .trim_end_matches("-access")
                            .to_string()
                    });
                    *up.hits.lock().unwrap().entry(label.clone()).or_default() += 1;
                    let delay = up.delay_ms.load(Ordering::SeqCst);
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                    let answer = up.answers.lock().unwrap().get(&label).copied();
                    match answer.unwrap_or(Answer::Status(500)) {
                        Answer::Ok { used, reset_in_s } if codex_account.is_some() => Json(json!({
                            "rate_limit": { "allowed": true, "limit_reached": false,
                                "primary_window": { "used_percent": used,
                                    "limit_window_seconds": 7 * 24 * HOUR_S,
                                    "reset_after_seconds": reset_in_s } }
                        }))
                        .into_response(),
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

/// Grace is bound to the account the good reading came from. Identified
/// slots (`synaps login` stores the profile's account id when the lookup
/// succeeds; `synaps auth identify` backfills it): a refresh rotation (same
/// account id) keeps it; the same slot re-logged to another account id while
/// its read is held is never chosen on the previous account's reading, so
/// the other account (graced on its own) serves.
#[tokio::test]
#[serial_test::serial]
async fn a_slot_re_logged_to_another_account_is_never_graced_on_the_old_reading() {
    let home = install_slots(&[("a", Some("acct-a"), "1"), ("b", Some("acct-b"), "1")]);
    let up = Upstream::default();
    up.set("a", URGENT);
    up.set("b", LATER);
    let url = up.spawn().await;
    let broker = auto_broker(&url);
    assert_eq!(chosen(&broker).await.unwrap(), "a");

    write_slots(home.path(), &[("a", Some("acct-a"), "2"), ("b", Some("acct-b"), "1")]);
    up.all(&["a", "b"], LIMITED);
    past_fresh_age().await;
    assert_eq!(
        chosen(&broker).await.unwrap(),
        "a",
        "the same account after a token rotation is still graced"
    );
    assert_eq!((up.hits("a"), up.hits("b")), (2, 2));

    write_slots(home.path(), &[("a", Some("acct-other"), "3"), ("b", Some("acct-b"), "1")]);
    assert_eq!(chosen(&broker).await.unwrap(), "b");
    assert_eq!(chosen(&auto_broker(&url)).await.unwrap(), "b", "a new broker too");
    assert_eq!((up.hits("a"), up.hits("b")), (2, 2), "no request inside the hold");
}

/// Unidentified Anthropic slots (no account id stored: an older login, or a
/// profile lookup that failed) can't tell a refresh rotation from a re-login
/// to another account, so grace holds only for the exact credential the
/// reading came from. A re-login of the slot (new refresh material) is never
/// graced on the previous occupant's reading; losing grace after a rotation
/// is the accepted cost.
#[tokio::test]
#[serial_test::serial]
async fn an_unidentified_slot_is_graced_only_for_the_credential_that_was_read() {
    let home = install_slots(&[("a", None, "1"), ("b", None, "1")]);
    let up = Upstream::default();
    up.set("a", URGENT);
    up.set("b", LATER);
    let url = up.spawn().await;
    let broker = auto_broker(&url);
    assert_eq!(chosen(&broker).await.unwrap(), "a");

    up.all(&["a", "b"], LIMITED);
    past_fresh_age().await;
    assert_eq!(
        chosen(&broker).await.unwrap(),
        "a",
        "the same credential is graced on its own reading"
    );

    // Slot `a` now holds other refresh material (a re-login to another
    // Anthropic account looks exactly like this without an account id).
    write_slots(home.path(), &[("a", None, "2"), ("b", None, "1")]);
    assert_eq!(chosen(&broker).await.unwrap(), "b");
    assert_eq!(chosen(&auto_broker(&url)).await.unwrap(), "b", "a new broker too");
    assert_eq!((up.hits("a"), up.hits("b")), (2, 2), "no request inside the hold");
}

/// Codex: a good reading that does not list the requested model (a generic
/// window does not prove entitlement to it) keeps that model ineligible
/// when the read then fails and the reading is graced; the same graced
/// reading still serves a request without that model.
#[tokio::test]
#[serial_test::serial]
async fn a_codex_model_missing_from_the_good_reading_stays_ineligible_under_grace() {
    const ASTRA: &str = "gpt-6-astra";
    let codex = OAuthProviderId::OpenAiCodex;
    let _home = install_codex("x", "acct-x");
    let up = Upstream::default();
    up.set("acct-x", LATER);
    let broker = LocalBroker::new(reqwest::Client::new())
        .with_usage_endpoint_override(up.spawn().await)
        .with_max_snapshot_age(Duration::from_secs(1));
    let label = |p: agent_core::auth::PinnedToken| p.credential.account.label_str().to_string();
    assert!(broker.access_token_auto(codex, Some(ASTRA)).await.is_err(), "unproven model");
    assert_eq!(label(broker.access_token_auto(codex, None).await.unwrap()), "x");

    up.set("acct-x", LIMITED);
    past_fresh_age().await;
    let err = broker.access_token_auto(codex, Some(ASTRA)).await.unwrap_err();
    assert!(matches!(err, BrokerError::NoAccountAvailable { .. }), "{err:?}");
    assert_eq!(
        label(broker.access_token_auto(codex, None).await.unwrap()),
        "x",
        "graced for a request without that model"
    );
    assert_eq!(up.hits("acct-x"), 2, "one good read, one held 429");
}

/// `synaps auth state --clear` is the escape hatch from a held usage read:
/// it drops the slot's hold (and last good reading), so the next selection
/// reads usage again at once instead of waiting out the backoff. Clearing
/// one account leaves the other's hold alone.
#[tokio::test]
#[serial_test::serial]
async fn auth_state_clear_releases_a_held_usage_read() {
    let _home = install(&["a", "b"]);
    let up = Upstream::default();
    up.all(&["a", "b"], Answer::Status(503));
    let url = up.spawn().await;
    let broker = auto_broker(&url);
    assert!(chosen(&broker).await.is_err(), "no good reading yet");
    assert_eq!((up.hits("a"), up.hits("b")), (1, 1));

    up.all(&["a", "b"], URGENT);
    past_fresh_age().await;
    assert!(chosen(&broker).await.is_err(), "held: no request inside the hold");
    assert_eq!((up.hits("a"), up.hits("b")), (1, 1));

    let query = agent_core::auth::AuthStateQuery {
        provider: Some("anthropic".into()),
        account: Some("b".into()),
        clear: true,
    };
    broker.auth_state(&query).unwrap();
    assert_eq!(
        chosen(&broker).await.expect("the cleared slot reads usage again"),
        "b"
    );
    assert_eq!((up.hits("a"), up.hits("b")), (1, 2), "only the cleared slot was read");

    // Every broker in the process shares the table: a clear through one
    // releases the hold for all of them.
    let query = agent_core::auth::AuthStateQuery {
        provider: Some("anthropic".into()),
        ..Default::default()
    };
    broker.auth_state(&agent_core::auth::AuthStateQuery { clear: true, ..query }).unwrap();
    assert_eq!(chosen(&auto_broker(&url)).await.unwrap(), "a");
    assert_eq!((up.hits("a"), up.hits("b")), (2, 3));
}
