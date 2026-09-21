//! G1/G2 boundary proof for multiple accounts per OAuth provider.
//!
//! * Two mock Codex accounts coexist in one `auth.json`; the local broker
//!   vends A/B independently, an explicit unknown slot is rejected without
//!   fallback, and the Codex `chatgpt-account-id` header is derived from the
//!   same credential as the bearer.
//! * A remote client talking to an HTTP broker selects A/B independently,
//!   caches per `(account, endpoint, machine principal)`, and maps 404/400.
//! * `auto` selection uses fresh read-only usage and honours cooldowns.
//!
//! Runs as an integration binary so the `SYNAPS_BASE_DIR` override cannot
//! interfere with unrelated unit tests. No real credential is ever touched.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use agent_core::auth::{
    Account, AccountPolicy, AccountSelector, BrokerError, CredentialBroker, CredentialRef,
    LocalBroker, OAuthProviderId, ProxyMethod, ProxyRequest, RemoteBroker, TokenCache,
};
use axum::extract::Query;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::json;

const FAR_FUTURE: u64 = 4_102_444_800_000; // year 2100: never refreshes

fn codex_jwt(account_id: &str) -> String {
    let payload = json!({ "https://api.openai.com/auth": { "chatgpt_account_id": account_id } });
    format!(
        "h.{}.s",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    )
}

fn oauth(access: &str, refresh: &str, account_id: Option<&str>) -> serde_json::Value {
    let mut v = json!({ "type": "oauth", "access": access, "refresh": refresh, "expires": FAR_FUTURE });
    if let Some(id) = account_id {
        v["accountId"] = json!(id);
    }
    v
}

/// Throwaway credential store with a default + named Codex slot and a named
/// Anthropic pair (for the usage-driven auto test).
fn install_store() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    agent_core::config::set_base_dir_for_tests(home.path().to_path_buf());
    std::fs::write(
        home.path().join("auth.json"),
        json!({
            "openai-codex": oauth(&codex_jwt("acct-default"), "refresh-default-SECRET", Some("acct-default")),
            "openai-codex@astra2": oauth(&codex_jwt("acct-astra2"), "refresh-astra2-SECRET", Some("acct-astra2")),
            "anthropic@a": oauth("anthropic-a-access", "refresh-anth-a-SECRET", None),
            "anthropic@b": oauth("anthropic-b-access", "refresh-anth-b-SECRET", None),
            "groq": { "type": "api_key", "key": "gsk-static-SECRET" }
        })
        .to_string(),
    )
    .unwrap();
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

fn named(provider: OAuthProviderId, label: &str) -> CredentialRef {
    CredentialRef::new(provider, Account::named(label).unwrap())
}

#[tokio::test]
#[serial_test::serial]
async fn local_broker_addresses_accounts_independently_and_never_falls_back() {
    let _home = install_store();
    let broker = LocalBroker::new(reqwest::Client::new()).with_account_policy(AccountPolicy::new());

    // Legacy call: default slot.
    let default = broker.access_token(OAuthProviderId::OpenAiCodex).await.unwrap();
    assert_eq!(default.token, codex_jwt("acct-default"));

    // Explicit slots.
    let a = broker
        .access_token_for(&CredentialRef::default_for(OAuthProviderId::OpenAiCodex))
        .await
        .unwrap();
    let b = broker
        .access_token_for(&named(OAuthProviderId::OpenAiCodex, "astra2"))
        .await
        .unwrap();
    assert_eq!(a.token, codex_jwt("acct-default"));
    assert_eq!(b.token, codex_jwt("acct-astra2"));

    // Explicit unknown slot: typed error, no fallback to any other slot.
    let err = broker
        .access_token_for(&named(OAuthProviderId::OpenAiCodex, "missing"))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, BrokerError::UnknownAccount { provider, label } if provider == "openai-codex" && label == "missing"),
        "{err:?}"
    );
    assert!(!err.to_string().contains("SECRET"));

    // Policy selects the named slot for the pinned pair.
    let policy = AccountPolicy::new().with(
        OAuthProviderId::OpenAiCodex,
        AccountSelector::Account(Account::named("astra2").unwrap()),
    );
    let broker = LocalBroker::new(reqwest::Client::new()).with_account_policy(policy);
    let pinned = broker
        .access_token_pinned(OAuthProviderId::OpenAiCodex)
        .await
        .unwrap();
    assert_eq!(pinned.credential.storage_key(), "openai-codex@astra2");
    assert_eq!(pinned.token.token, codex_jwt("acct-astra2"));
    assert_eq!(
        broker.access_token(OAuthProviderId::OpenAiCodex).await.unwrap().token,
        codex_jwt("acct-astra2"),
        "legacy access_token follows the policy"
    );

    // A policy naming a slot that does not exist fails closed.
    let policy = AccountPolicy::new().with(
        OAuthProviderId::OpenAiCodex,
        AccountSelector::Account(Account::named("gone").unwrap()),
    );
    let broker = LocalBroker::new(reqwest::Client::new()).with_account_policy(policy);
    assert!(matches!(
        broker.access_token(OAuthProviderId::OpenAiCodex).await,
        Err(BrokerError::UnknownAccount { .. })
    ));

    // An invalid configured selector fails closed with its source.
    let policy = AccountPolicy::new().with(
        OAuthProviderId::OpenAiCodex,
        AccountSelector::Invalid {
            source: "auth.account.openai-codex".into(),
            reason: "bad".into(),
        },
    );
    let broker = LocalBroker::new(reqwest::Client::new()).with_account_policy(policy);
    let err = broker.access_token(OAuthProviderId::OpenAiCodex).await.unwrap_err();
    assert!(matches!(err, BrokerError::InvalidAccount(_)));
    assert!(err.to_string().contains("auth.account.openai-codex"));
}

#[tokio::test]
#[serial_test::serial]
async fn capabilities_list_accounts_without_secrets_and_mark_selection() {
    let _home = install_store();
    let policy = AccountPolicy::new().with(
        OAuthProviderId::OpenAiCodex,
        AccountSelector::Account(Account::named("astra2").unwrap()),
    );
    let broker = LocalBroker::new(reqwest::Client::new()).with_account_policy(policy);
    let caps = broker.capabilities().await.unwrap();
    let codex = caps.iter().find(|c| c.key == "openai-codex").unwrap();
    assert!(codex.configured);
    let labels: Vec<&str> = codex.accounts.iter().map(|a| a.label.as_str()).collect();
    assert_eq!(labels, vec!["default", "astra2"]);
    assert!(codex.accounts.iter().find(|a| a.label == "astra2").unwrap().selected);
    assert!(!codex.accounts.iter().find(|a| a.label == "default").unwrap().selected);
    assert_eq!(
        codex.accounts[0].account_id_prefix.as_deref(),
        Some("acct-def")
    );
    // Anthropic: policy default → default slot missing → not configured, but accounts listed.
    let anthropic = caps.iter().find(|c| c.key == "anthropic").unwrap();
    assert!(!anthropic.configured);
    assert_eq!(anthropic.accounts.len(), 2);
    let wire = serde_json::to_string(&caps).unwrap();
    for secret in ["SECRET", "anthropic-a-access", "gsk-static", "acct-default\""] {
        assert!(!wire.contains(secret), "leaked {secret}: {wire}");
    }
}

/// The Codex proxy pins the slot via `provider@label`; bearer and
/// `chatgpt-account-id` are derived from the SAME credential.
#[tokio::test]
#[serial_test::serial]
async fn codex_proxy_pairs_bearer_and_account_header_from_one_credential() {
    let _home = install_store();
    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let app = Router::new().route(
        "/codex/models",
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
                seen.lock()
                    .unwrap()
                    .push((h("authorization"), h("chatgpt-account-id")));
                Json(json!({ "models": [] }))
            }
        }),
    );
    let upstream = spawn(app).await;
    let broker = LocalBroker::new(reqwest::Client::new())
        .with_account_policy(AccountPolicy::new())
        .with_openai_codex_base_url_for_tests(upstream);
    for (provider, expected_id) in [
        ("openai-codex", "acct-default"),
        ("openai-codex@astra2", "acct-astra2"),
        ("openai-codex@default", "acct-default"),
    ] {
        let resp = broker
            .proxy(ProxyRequest {
                provider: provider.into(),
                method: ProxyMethod::Get,
                path: "/codex/models?client_version=1".into(),
                body: None,
                stream: false,
                body_bytes: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{provider}");
        let (auth, account) = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!(auth, format!("Bearer {}", codex_jwt(expected_id)), "{provider}");
        assert_eq!(account, expected_id, "{provider}");
    }
    // Unknown pinned slot and malformed labels are rejected before any request.
    let before = seen.lock().unwrap().len();
    for provider in ["openai-codex@missing", "openai-codex@Bad Label", "openai-codex@"] {
        let err = broker
            .proxy(ProxyRequest {
                provider: provider.into(),
                method: ProxyMethod::Get,
                path: "/codex/models?client_version=1".into(),
                body: None,
                stream: false,
                body_bytes: None,
            })
            .await
            .unwrap_err();
        assert!(
            matches!(err, BrokerError::UnknownAccount { .. } | BrokerError::InvalidAccount(_)),
            "{provider}: {err:?}"
        );
    }
    assert_eq!(seen.lock().unwrap().len(), before, "no upstream call for rejected pins");
}

/// Fake `synaps auth-broker` token endpoint: serves distinct tokens per
/// account and records requests; 404 for unknown, 400 for malformed.
fn fake_broker(hits: Arc<Mutex<Vec<String>>>) -> Router {
    #[derive(serde::Deserialize)]
    struct Q {
        provider: String,
        account: Option<String>,
    }
    Router::new().route(
        "/token",
        get(move |headers: HeaderMap, Query(q): Query<Q>| {
            let hits = hits.clone();
            async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if auth != "Bearer machine-A" && auth != "Bearer machine-B" {
                    return (StatusCode::UNAUTHORIZED, Json(json!({"error":"bad machine auth"})));
                }
                let account = q.account.clone().expect("explicit slot requests always carry account");
                hits.lock().unwrap().push(format!("{auth}|{}|{account}", q.provider));
                match account.as_str() {
                    "default" | "astra2" => (
                        StatusCode::OK,
                        Json(json!({
                            "access_token": format!("tok-{}-{account}", &auth[7..]),
                            "expires": FAR_FUTURE,
                            "ttl_ms": 3_600_000u64,
                            "account": account,
                        })),
                    ),
                    "missing" => (StatusCode::NOT_FOUND, Json(json!({"error":"unknown account"}))),
                    _ => (StatusCode::BAD_REQUEST, Json(json!({"error":"invalid account"}))),
                }
            }
        }),
    )
}

#[tokio::test]
async fn remote_broker_selects_accounts_independently_with_principal_scoped_cache() {
    let hits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let endpoint = spawn(fake_broker(hits.clone())).await;
    let cache = TokenCache::new();
    let policy_b = AccountPolicy::new().with(
        OAuthProviderId::OpenAiCodex,
        AccountSelector::Account(Account::named("astra2").unwrap()),
    );
    let client_a = RemoteBroker::new(&endpoint, "machine-A", reqwest::Client::new(), cache.clone())
        .with_account_policy(AccountPolicy::new());
    let client_a_named = RemoteBroker::new(&endpoint, "machine-A", reqwest::Client::new(), cache.clone())
        .with_account_policy(policy_b.clone());
    let client_b = RemoteBroker::new(&endpoint, "machine-B", reqwest::Client::new(), cache.clone())
        .with_account_policy(policy_b);

    // Policy-driven pinned pairs on one principal.
    let p = client_a.access_token_pinned(OAuthProviderId::OpenAiCodex).await.unwrap();
    assert_eq!(p.credential.storage_key(), "openai-codex");
    assert_eq!(p.token.token, "tok-machine-A-default");
    let p = client_a_named.access_token_pinned(OAuthProviderId::OpenAiCodex).await.unwrap();
    assert_eq!(p.credential.storage_key(), "openai-codex@astra2");
    assert_eq!(p.token.token, "tok-machine-A-astra2");
    // Same account, cached: no second broker round-trip.
    let before = hits.lock().unwrap().len();
    let again = client_a
        .access_token_for(&named(OAuthProviderId::OpenAiCodex, "astra2"))
        .await
        .unwrap();
    assert_eq!(again.token, "tok-machine-A-astra2");
    assert_eq!(hits.lock().unwrap().len(), before, "cache hit expected");
    // A different machine principal at the same endpoint must NOT reuse A's cache.
    let p = client_b.access_token_pinned(OAuthProviderId::OpenAiCodex).await.unwrap();
    assert_eq!(p.token.token, "tok-machine-B-astra2");
    assert_eq!(hits.lock().unwrap().len(), before + 1);
    // Unknown / malformed explicit slots: typed, no fallback, not cached.
    assert!(matches!(
        client_a
            .access_token_for(&named(OAuthProviderId::OpenAiCodex, "missing"))
            .await,
        Err(BrokerError::UnknownAccount { .. })
    ));
    // Provider-wide invalidation clears every scoped key.
    cache.invalidate("openai-codex");
    let before = hits.lock().unwrap().len();
    client_a
        .access_token_for(&named(OAuthProviderId::OpenAiCodex, "astra2"))
        .await
        .unwrap();
    assert_eq!(hits.lock().unwrap().len(), before + 1, "refetch after invalidate");
    // Wire log shows no machine token in the cache key material.
    let dbg = format!("{cache:?}");
    assert!(!dbg.contains("machine-A") && !dbg.contains("machine-B"), "{dbg}");
    // Explicit default-slot requests always carry `account=default`.
    assert!(hits
        .lock()
        .unwrap()
        .iter()
        .any(|h| h == "Bearer machine-A|openai-codex|default"));
}

/// Broker whose HOST policy points at a named slot (or an old broker that
/// ignores the `account` parameter): an explicit default request must never
/// end up caching another slot's token under `default`, and a named request
/// against a broker that cannot honour it is rejected, not served.
fn policy_broker(mode: &'static str, hits: Arc<Mutex<Vec<String>>>) -> Router {
    #[derive(serde::Deserialize)]
    struct Q {
        provider: String,
        account: Option<String>,
    }
    Router::new().route(
        "/token",
        get(move |Query(q): Query<Q>| {
            let hits = hits.clone();
            async move {
                hits.lock()
                    .unwrap()
                    .push(format!("{}|{}", q.provider, q.account.clone().unwrap_or_default()));
                let token = |slot: &str| json!({ "access_token": format!("tok-{slot}"), "expires": FAR_FUTURE, "ttl_ms": 3_600_000u64 });
                match mode {
                    // New broker: host policy selects `astra2` when no account is given.
                    "host-named" => {
                        let slot = q.account.clone().unwrap_or_else(|| "astra2".into());
                        let mut body = token(&slot);
                        body["account"] = json!(slot);
                        (StatusCode::OK, Json(body))
                    }
                    // Old broker: ignores `account`, always the bare slot, no `account` field.
                    "old" => (StatusCode::OK, Json(token("default"))),
                    // Misbehaving broker: says it served another slot than asked.
                    _ => {
                        let mut body = token("astra2");
                        body["account"] = json!("astra2");
                        (StatusCode::OK, Json(body))
                    }
                }
            }
        }),
    )
}

#[tokio::test]
async fn remote_explicit_default_is_never_served_from_host_policy_or_mismatched_slot() {
    // Host policy = named: explicit default still gets the default slot, and
    // the wire request always carries `account=default`.
    let hits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let endpoint = spawn(policy_broker("host-named", hits.clone())).await;
    let client = RemoteBroker::new(&endpoint, "m", reqwest::Client::new(), TokenCache::new())
        .with_account_policy(AccountPolicy::new());
    let tok = client
        .access_token_for(&CredentialRef::default_for(OAuthProviderId::OpenAiCodex))
        .await
        .unwrap();
    assert_eq!(tok.token, "tok-default");
    assert_eq!(client.access_token(OAuthProviderId::OpenAiCodex).await.unwrap().token, "tok-default");
    assert!(hits.lock().unwrap().iter().all(|h| h == "openai-codex|default"), "{:?}", hits.lock().unwrap());
    // Legacy policy-free fetch (bare provider) is the only call that lets the host decide.
    let fetcher = agent_core::auth::BrokerClient::new(&endpoint, "m");
    let legacy = agent_core::auth::resolve_remote(&fetcher, &TokenCache::new(), "openai-codex", 0)
        .await
        .unwrap();
    assert_eq!(legacy.access_token, "tok-astra2");

    // Old broker (ignores account, no `account` in response): default OK, named REJECTED and not cached.
    let endpoint = spawn(policy_broker("old", Arc::new(Mutex::new(Vec::new())))).await;
    let cache = TokenCache::new();
    let client = RemoteBroker::new(&endpoint, "m", reqwest::Client::new(), cache.clone())
        .with_account_policy(AccountPolicy::new());
    assert_eq!(
        client
            .access_token_for(&CredentialRef::default_for(OAuthProviderId::OpenAiCodex))
            .await
            .unwrap()
            .token,
        "tok-default"
    );
    let err = client
        .access_token_for(&named(OAuthProviderId::OpenAiCodex, "astra2"))
        .await
        .unwrap_err();
    assert!(matches!(err, BrokerError::Transport(_)), "{err:?}");
    assert!(err.to_string().contains("different account"), "{err}");
    let dbg = format!("{cache:?}");
    assert!(!dbg.contains("openai-codex@astra2"), "mismatched token must not be cached: {dbg}");
    // Same via the pinned/policy path.
    let policy = AccountPolicy::new().with(
        OAuthProviderId::OpenAiCodex,
        AccountSelector::Account(Account::named("astra2").unwrap()),
    );
    let client = RemoteBroker::new(&endpoint, "m", reqwest::Client::new(), TokenCache::new())
        .with_account_policy(policy);
    assert!(client.access_token(OAuthProviderId::OpenAiCodex).await.is_err());

    // Misbehaving broker answering `default` with another slot: rejected.
    let endpoint = spawn(policy_broker("mismatch", Arc::new(Mutex::new(Vec::new())))).await;
    let cache = TokenCache::new();
    let client = RemoteBroker::new(&endpoint, "m", reqwest::Client::new(), cache.clone())
        .with_account_policy(AccountPolicy::new());
    let err = client
        .access_token_for(&CredentialRef::default_for(OAuthProviderId::OpenAiCodex))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("different account"), "{err}");
    assert!(!format!("{cache:?}").contains("openai-codex"));
}

/// `auto`: fresh read-only usage picks the account with headroom; exhausted,
/// cooled-down and unsupported accounts are never selected; no capacity =
/// typed failure, not a token.
#[tokio::test]
#[serial_test::serial]
async fn auto_selection_uses_fresh_usage_and_honours_cooldowns() {
    let _home = install_store();
    // Fake Anthropic usage: token A is exhausted, token B has headroom.
    let app = Router::new().route(
        "/api/oauth/usage",
        get(|headers: HeaderMap| async move {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let util = if auth.ends_with("anthropic-a-access") { 100.0 } else { 20.0 };
            Json(json!({
                "five_hour": { "utilization": util, "resets_at": "2099-01-01T00:00:00Z" },
                "seven_day": { "utilization": util, "resets_at": "2099-01-01T00:00:00Z" }
            }))
        }),
    );
    let upstream = spawn(app).await;
    let policy = AccountPolicy::new().with(OAuthProviderId::Anthropic, AccountSelector::Auto);
    let broker = LocalBroker::new(reqwest::Client::new())
        .with_account_policy(policy)
        .with_usage_endpoint_override(format!("{upstream}/api/oauth/usage"));

    let pinned = broker.access_token_pinned(OAuthProviderId::Anthropic).await.unwrap();
    assert_eq!(pinned.credential.storage_key(), "anthropic@b");
    assert_eq!(pinned.token.token, "anthropic-b-access");
    assert_eq!(
        broker.account_selector(OAuthProviderId::Anthropic),
        AccountSelector::Auto
    );

    // A limit report benches B; with A exhausted nothing is eligible → fail closed.
    broker
        .report_cooldown(&named(OAuthProviderId::Anthropic, "b"), None, "test")
        .await
        .unwrap();
    let err = broker.access_token_pinned(OAuthProviderId::Anthropic).await.unwrap_err();
    assert!(matches!(err, BrokerError::NoAccountAvailable { .. }), "{err:?}");
    assert!(!err.to_string().contains("SECRET"));
    let rows = broker.accounts(OAuthProviderId::Anthropic).await.unwrap();
    assert!(rows.iter().find(|r| r.label == "b").unwrap().cooldown_until.is_some());

    // Explicit selection is unaffected by cooldowns (never overridden).
    let b = broker
        .access_token_for(&named(OAuthProviderId::Anthropic, "b"))
        .await
        .unwrap();
    assert_eq!(b.token, "anthropic-b-access");

    // A provider without a usage adapter can never prove capacity.
    let policy = AccountPolicy::new().with(OAuthProviderId::GitHubCopilot, AccountSelector::Auto);
    let broker = LocalBroker::new(reqwest::Client::new()).with_account_policy(policy);
    assert!(matches!(
        broker.access_token_pinned(OAuthProviderId::GitHubCopilot).await,
        Err(BrokerError::NoAccountAvailable { .. })
    ));
}

/// A fresh cached reading for A must not authorize B after re-login under the
/// same label, including replacement while the usage request is in flight.
#[tokio::test]
#[serial_test::serial]
async fn auto_selection_discards_replaced_seats_and_removed_slots() {
    use std::sync::atomic::{AtomicBool, Ordering};
    for replace_during_read in [false, true] {
        let home = install_store();
        let path = home.path().join("auth.json");
        let replace = Arc::new(AtomicBool::new(replace_during_read));
        let handler_path = path.clone();
        let app = Router::new().route(
            "/usage",
            get(move |headers: HeaderMap| {
                let path = handler_path.clone();
                let replace = replace.clone();
                async move {
                    let auth = headers.get("authorization").unwrap().to_str().unwrap();
                    let original_b = auth.ends_with("anthropic-b-access");
                    if original_b && replace.swap(false, Ordering::SeqCst) {
                        let mut data: serde_json::Value =
                            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                        data["anthropic@b"] =
                            oauth("replacement-access", "replacement-refresh", None);
                        std::fs::write(&path, data.to_string()).unwrap();
                    }
                    let used = if original_b { 20.0 } else { 100.0 };
                    Json(
                        json!({"five_hour":{"utilization":used,"resets_at":"2099-01-01T00:00:00Z"},
                    "seven_day":{"utilization":used,"resets_at":"2099-01-01T00:00:00Z"}}),
                    )
                }
            }),
        );
        let upstream = spawn(app).await;
        let broker = LocalBroker::new(reqwest::Client::new())
            .with_account_policy(
                AccountPolicy::new().with(OAuthProviderId::Anthropic, AccountSelector::Auto),
            )
            .with_usage_endpoint_override(format!("{upstream}/usage"));
        if !replace_during_read {
            let pinned = broker
                .access_token_pinned(OAuthProviderId::Anthropic)
                .await
                .unwrap();
            assert_eq!(pinned.token.token, "anthropic-b-access");
            // Same alias now holds an exhausted, independently identified seat.
            let mut data: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            data["anthropic@b"] = oauth("replacement-access", "replacement-refresh", None);
            std::fs::write(&path, data.to_string()).unwrap();
        }
        assert!(matches!(
            broker.access_token_pinned(OAuthProviderId::Anthropic).await,
            Err(BrokerError::NoAccountAvailable { .. })
        ));
        let mut data: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        data.as_object_mut().unwrap().remove("anthropic@b");
        std::fs::write(&path, data.to_string()).unwrap();
        assert!(matches!(
            broker.access_token_pinned(OAuthProviderId::Anthropic).await,
            Err(BrokerError::NoAccountAvailable { .. })
        ));
    }
}

#[tokio::test]
#[serial_test::serial]
async fn chooser_ranks_deadlines_keeps_sticky_seat_and_preview_is_non_spending() {
    use agent_core::auth::{quota_policy::Strategy, AutoSelectionPolicy};
    use std::sync::atomic::{AtomicU64, Ordering};
    let home = install_store();
    let before = std::fs::read(home.path().join("auth.json")).unwrap();
    let a_reset_hours = Arc::new(AtomicU64::new(60));
    let reset = a_reset_hours.clone();
    let app = Router::new().route("/usage", get(move |headers: HeaderMap| {
        let reset = reset.clone();
        async move {
            let a = headers.get("authorization").unwrap().to_str().unwrap().ends_with("anthropic-a-access");
            let hours = if a { reset.load(Ordering::SeqCst) } else { 48 };
            let time = chrono::DateTime::<chrono::Utc>::from_timestamp_millis((agent_core::epoch_millis() + hours * 3_600_000) as i64).unwrap().to_rfc3339();
            Json(json!({"seven_day": {"utilization": if a { 5.0 } else { 70.0 }, "resets_at": time}, "extra_usage": {"utilization": null, "is_enabled": false}}))
        }
    }));
    let upstream = spawn(app).await;
    let broker = LocalBroker::new(reqwest::Client::new())
        .with_account_policy(
            AccountPolicy::new().with(OAuthProviderId::Anthropic, AccountSelector::Auto),
        )
        .with_usage_endpoint_override(format!("{upstream}/usage"))
        .with_max_snapshot_age(std::time::Duration::from_secs(1));
    let plan = broker.plan(OAuthProviderId::Anthropic, None).await.unwrap();
    assert!(plan.current.is_none());
    assert_eq!(plan.rows[0].credential.account.label_str(), "b");
    assert_eq!(plan.rows[0].rank, Some(1));
    assert_eq!(plan.rows[1].rank, Some(2));
    assert!(
        broker
            .plan(OAuthProviderId::Anthropic, None)
            .await
            .unwrap()
            .current
            .is_none(),
        "preview must not pin a winner"
    );
    let first = broker
        .access_token_pinned(OAuthProviderId::Anthropic)
        .await
        .unwrap();
    assert_eq!(first.credential.account.label_str(), "b");
    a_reset_hours.store(36, Ordering::SeqCst); // A is sooner now, same tier.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert_eq!(
        broker
            .access_token_pinned(OAuthProviderId::Anthropic)
            .await
            .unwrap()
            .credential,
        first.credential
    );
    a_reset_hours.store(12, Ordering::SeqCst); // Urgent A preempts healthy B.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert_eq!(
        broker
            .access_token_pinned(OAuthProviderId::Anthropic)
            .await
            .unwrap()
            .credential
            .account
            .label_str(),
        "a"
    );
    broker
        .report_cooldown(
            &named(OAuthProviderId::Anthropic, "a"),
            None,
            "synthetic_limit",
        )
        .await
        .unwrap();
    let cooldown = broker
        .accounts(OAuthProviderId::Anthropic)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.label == "a")
        .unwrap()
        .cooldown_until;
    let plan = broker.plan(OAuthProviderId::Anthropic, None).await.unwrap();
    assert_eq!(plan.rows[0].credential.account.label_str(), "b");
    assert_eq!(
        plan.current.unwrap().account.label_str(),
        "a",
        "preview did not repin away from cooling seat"
    );
    assert_eq!(
        broker
            .accounts(OAuthProviderId::Anthropic)
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.label == "a")
            .unwrap()
            .cooldown_until,
        cooldown
    );
    assert_eq!(
        std::fs::read(home.path().join("auth.json")).unwrap(),
        before,
        "valid-token preview and vend do not rewrite credentials"
    );
    a_reset_hours.store(60, Ordering::SeqCst);
    let other = LocalBroker::new(reqwest::Client::new())
        .with_auto_policy(AutoSelectionPolicy {
            strategy: Strategy::LowestUtilization,
            ..Default::default()
        })
        .with_usage_endpoint_override(format!("{upstream}/usage"));
    assert_eq!(
        other
            .plan(OAuthProviderId::Anthropic, None)
            .await
            .unwrap()
            .rows[0]
            .credential
            .account
            .label_str(),
        "a"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn runtime_broker_cache_retains_limits_but_separates_profiles_and_principals() {
    use agent_core::auth::{broker_from_source, CredentialSource};
    let _home = install_store();
    let cache = TokenCache::new();
    let a = broker_from_source(&CredentialSource::Local, &cache, reqwest::Client::new());
    let b = broker_from_source(
        &CredentialSource::Local,
        &cache.clone(),
        reqwest::Client::new(),
    );
    assert!(Arc::ptr_eq(&a, &b));
    a.report_cooldown(&named(OAuthProviderId::Anthropic, "a"), None, "limit")
        .await
        .unwrap();
    assert!(b
        .accounts(OAuthProviderId::Anthropic)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.label == "a")
        .unwrap()
        .cooldown_until
        .is_some());
    let _other_home = install_store();
    let c = broker_from_source(&CredentialSource::Local, &cache, reqwest::Client::new());
    assert!(!Arc::ptr_eq(&b, &c));
    assert!(c
        .accounts(OAuthProviderId::Anthropic)
        .await
        .unwrap()
        .iter()
        .all(|r| r.cooldown_until.is_none()));
    let remote = broker_from_source(
        &CredentialSource::Remote {
            endpoint: "http://127.0.0.1:1".into(),
            machine_token: "synthetic".into(),
        },
        &cache,
        reqwest::Client::new(),
    );
    assert!(!Arc::ptr_eq(&c, &remote));
}

#[tokio::test]
#[serial_test::serial]
async fn alias_cannot_bypass_cooldown_and_relogin_does_not_inherit_it() {
    let home = install_store();
    let path = home.path().join("auth.json");
    let mut root: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    root["anthropic@a"]["accountId"] = json!("one-seat");
    root["anthropic@b"]["accountId"] = json!("one-seat");
    std::fs::write(&path, root.to_string()).unwrap();
    let broker = LocalBroker::new(reqwest::Client::new());
    broker
        .report_cooldown(&named(OAuthProviderId::Anthropic, "a"), None, "limit")
        .await
        .unwrap();
    assert!(broker
        .accounts(OAuthProviderId::Anthropic)
        .await
        .unwrap()
        .iter()
        .all(|r| r.cooldown_until.is_some()));
    root["anthropic@b"]["accountId"] = json!("new-seat");
    std::fs::write(&path, root.to_string()).unwrap();
    assert!(broker
        .accounts(OAuthProviderId::Anthropic)
        .await
        .unwrap()
        .iter()
        .find(|r| r.label == "b")
        .unwrap()
        .cooldown_until
        .is_none());
}

#[tokio::test]
#[serial_test::serial]
async fn generic_codex_headroom_does_not_authorize_a_model_not_reported_available() {
    let _home = install_store();
    let app = Router::new().route("/usage", get(|| async {
        Json(json!({"plan_type":"free", "rate_limit":{"limit_reached":false,"primary_window":{"used_percent":0,"limit_window_seconds":2592000,"reset_after_seconds":2592000}}}))
    }));
    let upstream = spawn(app).await;
    let broker = LocalBroker::new(reqwest::Client::new())
        .with_usage_endpoint_override(format!("{upstream}/usage"));
    assert!(broker
        .plan(OAuthProviderId::OpenAiCodex, Some("gpt-6-astra"))
        .await
        .unwrap()
        .selection
        .selected()
        .is_none());
    assert!(broker
        .plan(OAuthProviderId::OpenAiCodex, None)
        .await
        .unwrap()
        .selection
        .selected()
        .is_some());
}
