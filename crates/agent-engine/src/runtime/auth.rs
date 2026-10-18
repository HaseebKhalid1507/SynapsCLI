use super::types::AuthState;
use crate::auth::{
    broker_from_source, is_expired_with_margin, CredentialSource, OAuthProviderId, TokenCache,
    DEFAULT_MARGIN_MS,
};
use crate::{Result, RuntimeError};
use reqwest::Client;
use std::sync::Arc;
use tokio::sync::RwLock;

pub(super) struct AuthMethods;

/// True if `model` routes to the Anthropic path (not OpenAI/codex/local). Used
/// to skip the Anthropic pre-stream refresh for non-Anthropic models — they
/// resolve their own provider auth (incl. via the broker), so fetching an
/// Anthropic token first is wasteful and would FAIL on a codex-only Remote
/// broker. (#158 C4/#7)
pub(super) fn model_is_anthropic(model: &str) -> bool {
    crate::runtime::openai::resolve_route(model)
        .is_some_and(|route| route.wire == crate::runtime::openai::WireProtocol::AnthropicMessages)
}

impl AuthMethods {
    /// Reset `AuthState` so a Remote client never *uses* or *holds* a credential
    /// seeded from a local `auth.json`. Called from `apply_config` when the
    /// source is Remote: clears the access token, drops any refresh token
    /// (invariant 1), and forces the next `refresh_if_needed` to fetch from the
    /// broker. `auth_type` stays "oauth" so the Local early-return is not taken.
    pub(super) fn scrub_for_remote(s: &mut AuthState) {
        s.auth_token.clear();
        s.auth_type = "oauth".to_string();
        s.refresh_token = None;
        s.token_expires = None;
    }

    /// Check if the Anthropic access token is expired and re-vend it through
    /// the credential broker if needed.
    ///
    /// One path for both sources: `broker_from_source` yields the in-process
    /// `LocalBroker` (single-flight refresh + atomic auth.json persistence
    /// behind the boundary) or the authenticated `RemoteBroker`. In BOTH cases
    /// this layer receives an access token + expiry only and never holds a
    /// refresh token — there is no direct-read fallback.
    pub(super) async fn refresh_if_needed(
        auth: Arc<RwLock<AuthState>>,
        client: &Client,
        source: &CredentialSource,
        cache: &TokenCache,
    ) -> Result<()> {
        // Fast path: in-memory token still fresh?
        let recorded_mtime = {
            let auth_guard = auth.read().await;
            // A non-OAuth local auth mode (e.g. a stubbed api_key harness)
            // never contacts the broker. Remote sources always resolve via
            // the broker regardless of the seeded auth_type.
            if !source.is_remote() && auth_guard.auth_type != "oauth" {
                return Ok(());
            }
            match auth_guard.token_expires {
                Some(exp) if !auth_guard.auth_token.is_empty() => {
                    if source.is_remote() {
                        // Must use the SAME predicate as the remote cache
                        // (is_expired_with_margin + DEFAULT_MARGIN_MS) so the
                        // fast-path and TokenCache agree on freshness (board #1).
                        if !is_expired_with_margin(exp, DEFAULT_MARGIN_MS) {
                            return Ok(());
                        }
                        None
                    } else if crate::epoch_millis() < exp {
                        // Fresh by expiry; the mtime check below decides.
                        Some(auth_guard.auth_file_mtime)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        };
        // Read lock dropped here — the stat(2) below never runs under it.
        // Local only: auth.json rotated externally since we loaded? One stat
        // per request.
        if let Some(recorded) = recorded_mtime {
            if !auth_file_changed(recorded) {
                return Ok(());
            }
        }

        tracing::info!("Refreshing auth token via credential broker");
        let mtime = local_mtime_for(source);
        let broker = broker_from_source(source, cache, client.clone());
        let tok = broker
            .access_token(OAuthProviderId::Anthropic)
            .await
            .map_err(refresh_error)?;
        Self::store_token(&auth, tok, mtime).await;
        Ok(())
    }

    /// Recover from a provider-rejected (HTTP 401) Anthropic token, or an
    /// explicit `/auth reload`: drop the in-memory token and re-vend.
    ///
    /// * Remote: invalidate the client `TokenCache` and re-run
    ///   [`Self::refresh_if_needed`] — identical to the pre-existing C1 path.
    /// * Local (OAuth): ask the `LocalBroker` for a token *after rejection of*
    ///   the one we held. It re-reads auth.json under the refresh gate; if the
    ///   file already has a different token (external rotation) that is used,
    ///   otherwise the refresh token is spent to force a new access token
    ///   (revoked-but-unexpired case).
    ///
    /// No lock is held across an await on the broker. The rejected token is
    /// passed to the broker only for an equality check and never logged.
    ///
    /// `sent` is the exact token the rejected request carried (`None` for an
    /// explicit `/auth reload`, meaning "whatever we hold now"). For Local, if shared
    /// state already holds a *different* non-empty token, a concurrent stream
    /// has reloaded since this request was built: reuse it, spend nothing.
    pub(super) async fn reload_after_rejection(
        auth: Arc<RwLock<AuthState>>,
        client: &Client,
        source: &CredentialSource,
        cache: &TokenCache,
        sent: Option<&str>,
    ) -> Result<()> {
        let rejected = {
            let mut g = auth.write().await;
            if !source.is_remote() && g.auth_type != "oauth" {
                return Err(RuntimeError::Auth(
                    "Anthropic auth is not OAuth-managed in this session; nothing to reload."
                        .to_string(),
                ));
            }
            // Local only: the remote path is kept byte-for-byte as before
            // (always invalidate + refetch; the broker owns rotation).
            if let (Some(sent), false) = (sent, source.is_remote()) {
                if !g.auth_token.is_empty() && g.auth_token != sent {
                    return Ok(());
                }
            }
            g.token_expires = None;
            g.auth_file_mtime = None;
            let held = std::mem::take(&mut g.auth_token);
            sent.map_or(held, str::to_string)
        };
        if source.is_remote() {
            cache.invalidate("anthropic");
            return Self::refresh_if_needed(auth, client, source, cache).await;
        }

        tracing::info!("Reloading Anthropic credentials after rejection");
        let mtime = local_mtime_for(source);
        let broker = broker_from_source(source, cache, client.clone());
        let res = if rejected.is_empty() {
            broker.access_token(OAuthProviderId::Anthropic).await
        } else {
            broker
                .access_token_after_rejection(OAuthProviderId::Anthropic, &rejected)
                .await
        };
        drop(rejected);
        let tok = res.map_err(refresh_error)?;
        Self::store_token(&auth, tok, mtime).await;
        Ok(())
    }

    /// Publish a freshly vended token to the shared state so all clones
    /// (including spawned stream tasks) see it. Never a refresh token.
    async fn store_token(
        auth: &Arc<RwLock<AuthState>>,
        tok: crate::auth::AccessToken,
        mtime: Option<std::time::SystemTime>,
    ) {
        let mut auth_guard = auth.write().await;
        auth_guard.auth_token = tok.token;
        auth_guard.auth_type = "oauth".to_string();
        auth_guard.refresh_token = None;
        auth_guard.token_expires = Some(tok.expires);
        auth_guard.auth_file_mtime = mtime;
    }
}

fn refresh_error(e: crate::auth::BrokerError) -> RuntimeError {
    RuntimeError::Auth(format!(
        "Token refresh failed: {}. Run `synaps login` to re-authenticate, or check auth.remote_endpoint / broker reachability.", e
    ))
}

/// Current mtime of the local auth.json, if it can be stat'ed.
fn auth_file_mtime_now() -> Option<std::time::SystemTime> {
    std::fs::metadata(crate::auth::auth_file_path())
        .and_then(|m| m.modified())
        .ok()
}

/// Local source: stat auth.json BEFORE loading, so a write racing the load
/// can only cause one extra (cheap) reload, never a missed one.
fn local_mtime_for(source: &CredentialSource) -> Option<std::time::SystemTime> {
    if source.is_remote() {
        None
    } else {
        auth_file_mtime_now()
    }
}

/// True when we recorded an mtime at load time and the file's current mtime
/// differs. An unreadable/missing file does NOT invalidate a working token.
fn auth_file_changed(recorded: Option<std::time::SystemTime>) -> bool {
    // `recorded.and_then` short-circuits: no stat when nothing was recorded.
    match (recorded, recorded.and_then(|_| auth_file_mtime_now())) {
        (Some(then), Some(now)) => then != now,
        _ => false,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::TokenCache;

    /// Tiny in-test broker that always returns `token_json` at GET /token.
    async fn spawn_broker(token_json: &'static str) -> String {
        use axum::{routing::get, Router};
        let app = Router::new().route("/token", get(move || async move { token_json.to_string() }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn empty_auth() -> Arc<RwLock<AuthState>> {
        Arc::new(RwLock::new(AuthState {
            auth_token: String::new(),
            auth_type: "none".into(),
            refresh_token: Some("SHOULD-BE-CLEARED".into()),
            token_expires: None,
            auth_file_mtime: None,
        }))
    }

    #[tokio::test]
    async fn remote_source_fetches_and_populates_auth_state() {
        let url = spawn_broker(r#"{"access_token":"sk-broker-xyz","expires":9999999999999}"#).await;
        let source = CredentialSource::Remote {
            endpoint: url,
            machine_token: "m".into(),
        };
        let cache = TokenCache::new();
        let auth = empty_auth();
        AuthMethods::refresh_if_needed(Arc::clone(&auth), &Client::new(), &source, &cache)
            .await
            .unwrap();
        let g = auth.read().await;
        assert_eq!(g.auth_token, "sk-broker-xyz");
        assert_eq!(g.auth_type, "oauth");
        assert_eq!(
            g.refresh_token, None,
            "Remote must clear any refresh token (invariant)"
        );
        assert_eq!(g.token_expires, Some(9_999_999_999_999));
    }

    #[tokio::test]
    async fn remote_source_fast_path_when_token_still_fresh() {
        let url = spawn_broker(r#"{"access_token":"sk-1","expires":9999999999999}"#).await;
        let source = CredentialSource::Remote {
            endpoint: url,
            machine_token: "m".into(),
        };
        let cache = TokenCache::new();
        let auth = empty_auth();
        // First call fetches + sets a far-future expiry.
        AuthMethods::refresh_if_needed(Arc::clone(&auth), &Client::new(), &source, &cache)
            .await
            .unwrap();
        // Second call: in-memory token is fresh -> fast path returns without refetch.
        AuthMethods::refresh_if_needed(Arc::clone(&auth), &Client::new(), &source, &cache)
            .await
            .unwrap();
        assert_eq!(auth.read().await.auth_token, "sk-1");
    }

    #[tokio::test]
    async fn remote_fast_path_refetches_a_token_inside_the_margin() {
        // AuthState holds a token that is valid but within the 5-min refetch
        // margin. The fast path must NOT serve it — it must refetch (board #1).
        let url = spawn_broker(r#"{"access_token":"sk-FRESH","expires":9999999999999}"#).await;
        let source = CredentialSource::Remote {
            endpoint: url,
            machine_token: "m".into(),
        };
        let cache = TokenCache::new();
        let near = crate::epoch_millis() + 2 * 60 * 1000; // 2 min — inside the 5-min margin
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-STALE".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(near),
            auth_file_mtime: None,
        }));
        AuthMethods::refresh_if_needed(Arc::clone(&auth), &Client::new(), &source, &cache)
            .await
            .unwrap();
        assert_eq!(
            auth.read().await.auth_token,
            "sk-FRESH",
            "a token inside the refetch margin must be refetched, not served stale"
        );
    }

    #[tokio::test]
    async fn local_source_api_key_is_noop_never_contacts_broker() {
        // Local + non-oauth auth_type hits the original early-return: no broker,
        // no error, state untouched. (Local path byte-for-byte unchanged.)
        let source = CredentialSource::Local;
        let cache = TokenCache::new();
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "key".into(),
            auth_type: "api_key".into(),
            refresh_token: None,
            token_expires: None,
            auth_file_mtime: None,
        }));
        AuthMethods::refresh_if_needed(Arc::clone(&auth), &Client::new(), &source, &cache)
            .await
            .unwrap();
        let g = auth.read().await;
        assert_eq!(g.auth_token, "key");
        assert_eq!(g.auth_type, "api_key");
    }

    // ── Local source: auth.json rotation + 401 recovery ─────────────────────

    fn write_creds_with_mtime(base: &std::path::Path, access: &str, mtime_secs: u64) {
        let path = base.join("auth.json");
        let body = serde_json::json!({
            "anthropic": {
                "type": "oauth",
                "refresh": "rt-local",
                "access": access,
                "expires": 9_999_999_999_999u64,
            }
        });
        std::fs::write(&path, body.to_string()).unwrap();
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime_secs);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn unloaded_local_oauth() -> Arc<RwLock<AuthState>> {
        // Mirrors `Runtime::from_parts`: empty token, expiry 0 → first call loads.
        Arc::new(RwLock::new(AuthState {
            auth_token: String::new(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(0),
            auth_file_mtime: None,
        }))
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn local_fast_path_reloads_when_auth_json_mtime_changes() {
        let base = crate::test_env::BaseDirGuard::new();
        let (src, cache, client) = (CredentialSource::Local, TokenCache::new(), Client::new());
        let auth = unloaded_local_oauth();

        write_creds_with_mtime(base.path(), "sk-A", 1_000_000);
        AuthMethods::refresh_if_needed(Arc::clone(&auth), &client, &src, &cache)
            .await
            .unwrap();
        assert_eq!(auth.read().await.auth_token, "sk-A");
        assert!(auth.read().await.auth_file_mtime.is_some());

        // Same mtime, different content: fast path trusts memory (no re-read).
        write_creds_with_mtime(base.path(), "sk-B", 1_000_000);
        AuthMethods::refresh_if_needed(Arc::clone(&auth), &client, &src, &cache)
            .await
            .unwrap();
        assert_eq!(
            auth.read().await.auth_token,
            "sk-A",
            "unchanged mtime must keep the one-stat fast path"
        );

        // External rotation bumps mtime → next request reloads.
        write_creds_with_mtime(base.path(), "sk-C", 2_000_000);
        AuthMethods::refresh_if_needed(Arc::clone(&auth), &client, &src, &cache)
            .await
            .unwrap();
        assert_eq!(auth.read().await.auth_token, "sk-C");
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn local_fast_path_keeps_token_when_auth_json_disappears() {
        let base = crate::test_env::BaseDirGuard::new();
        let (src, cache, client) = (CredentialSource::Local, TokenCache::new(), Client::new());
        let auth = unloaded_local_oauth();
        write_creds_with_mtime(base.path(), "sk-A", 1_000_000);
        AuthMethods::refresh_if_needed(Arc::clone(&auth), &client, &src, &cache)
            .await
            .unwrap();
        std::fs::remove_file(base.path().join("auth.json")).unwrap();
        AuthMethods::refresh_if_needed(Arc::clone(&auth), &client, &src, &cache)
            .await
            .expect("a vanished auth.json must not break a working session");
        assert_eq!(auth.read().await.auth_token, "sk-A");
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn reload_after_rejection_uses_rotated_disk_token_without_refresh() {
        let base = crate::test_env::BaseDirGuard::new();
        write_creds_with_mtime(base.path(), "sk-new", 1_000_000);
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-old".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
        }));
        // A client that cannot reach anything: proves no network refresh ran.
        let offline = Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .build()
            .unwrap();
        AuthMethods::reload_after_rejection(
            Arc::clone(&auth),
            &offline,
            &CredentialSource::Local,
            &TokenCache::new(),
            None,
        )
        .await
        .unwrap();
        let g = auth.read().await;
        assert_eq!(g.auth_token, "sk-new");
        assert_eq!(g.refresh_token, None, "engine never holds a refresh token");
        assert!(g.auth_file_mtime.is_some());
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn reload_after_rejection_forces_refresh_when_disk_token_was_rejected() {
        // Revoked-but-unexpired: auth.json still holds the exact token the
        // provider just rejected. The broker must NOT hand it back; it must
        // spend the refresh token. The client is offline, so the forced
        // refresh fails — which is the observable proof it was attempted —
        // and the rejected token is not reinstated.
        let base = crate::test_env::BaseDirGuard::new();
        write_creds_with_mtime(base.path(), "sk-revoked", 1_000_000);
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-revoked".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
        }));
        let offline = Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .build()
            .unwrap();
        let err = AuthMethods::reload_after_rejection(
            Arc::clone(&auth),
            &offline,
            &CredentialSource::Local,
            &TokenCache::new(),
            None,
        )
        .await
        .expect_err("forced refresh must be attempted (and fail offline)");
        let msg = err.to_string();
        assert!(msg.contains("Token refresh failed"), "{msg}");
        for secret in ["sk-revoked", "rt-local"] {
            assert!(!msg.contains(secret), "error leaked a credential: {msg}");
        }
        let g = auth.read().await;
        assert!(
            g.auth_token.is_empty(),
            "rejected token must not be reinstated"
        );
        assert_eq!(g.token_expires, None);
        // Failed refresh persists nothing.
        let disk = std::fs::read_to_string(base.path().join("auth.json")).unwrap();
        assert!(disk.contains("sk-revoked"));
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn late_401_for_old_token_reuses_concurrent_reload_and_spends_nothing() {
        // Stream A already reloaded to sk-new (memory + disk). Stream B's 401
        // was for sk-old. B must reuse sk-new — NOT report it as rejected,
        // which would force a second refresh-token spend.
        let base = crate::test_env::BaseDirGuard::new();
        write_creds_with_mtime(base.path(), "sk-new", 1_000_000);
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-new".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
        }));
        let offline = Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .build()
            .unwrap();
        AuthMethods::reload_after_rejection(
            Arc::clone(&auth),
            &offline,
            &CredentialSource::Local,
            &TokenCache::new(),
            Some("sk-old"),
        )
        .await
        .expect("a concurrent reload already fixed it: no refresh attempted");
        assert_eq!(auth.read().await.auth_token, "sk-new");
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn late_401_while_other_reload_in_flight_uses_sent_token_as_rejected() {
        // Memory is empty (A's reload in flight) but disk already has sk-new.
        // B reports the token it SENT (sk-old): disk differs → reuse, no refresh.
        let base = crate::test_env::BaseDirGuard::new();
        write_creds_with_mtime(base.path(), "sk-new", 1_000_000);
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: String::new(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: None,
            auth_file_mtime: None,
        }));
        let offline = Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .build()
            .unwrap();
        AuthMethods::reload_after_rejection(
            Arc::clone(&auth),
            &offline,
            &CredentialSource::Local,
            &TokenCache::new(),
            Some("sk-old"),
        )
        .await
        .unwrap();
        assert_eq!(auth.read().await.auth_token, "sk-new");
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn failed_forced_refresh_then_user_relogin_recovers_next_request() {
        let base = crate::test_env::BaseDirGuard::new();
        write_creds_with_mtime(base.path(), "sk-revoked", 1_000_000);
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-revoked".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
        }));
        let (src, cache) = (CredentialSource::Local, TokenCache::new());
        let offline = Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .build()
            .unwrap();
        assert!(AuthMethods::reload_after_rejection(
            Arc::clone(&auth),
            &offline,
            &src,
            &cache,
            Some("sk-revoked")
        )
        .await
        .is_err());
        write_creds_with_mtime(base.path(), "sk-fixed", 2_000_000); // `synaps login`
        AuthMethods::refresh_if_needed(Arc::clone(&auth), &offline, &src, &cache)
            .await
            .unwrap();
        assert_eq!(auth.read().await.auth_token, "sk-fixed");
    }

    #[test]
    fn auth_state_debug_redacts_tokens() {
        let s = AuthState {
            auth_token: "sk-SECRET".into(),
            auth_type: "oauth".into(),
            refresh_token: Some("rt-SECRET".into()),
            token_expires: Some(1),
            auth_file_mtime: None,
        };
        let d = format!("{s:?}");
        assert!(!d.contains("SECRET"), "{d}");
    }

    #[tokio::test]
    async fn reload_after_rejection_remote_invalidates_cache_and_refetches() {
        let url = spawn_broker(r#"{"access_token":"sk-remote-new","expires":9999999999999}"#).await;
        let source = CredentialSource::Remote {
            endpoint: url,
            machine_token: "m".into(),
        };
        let cache = TokenCache::new();
        cache.put(
            "anthropic",
            crate::auth::BrokerToken {
                access_token: "sk-remote-old".into(),
                expires: 9_999_999_999_999,
                ttl_ms: None,
            },
        );
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-remote-old".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
        }));
        AuthMethods::reload_after_rejection(
            Arc::clone(&auth),
            &Client::new(),
            &source,
            &cache,
            None,
        )
        .await
        .unwrap();
        let g = auth.read().await;
        assert_eq!(g.auth_token, "sk-remote-new");
        assert_eq!(g.auth_file_mtime, None, "remote never tracks a local mtime");
    }

    #[tokio::test]
    async fn reload_after_rejection_remote_ignores_sent_shortcut_and_refetches() {
        // Remote behaviour is unchanged from the pre-existing C1 path: even if
        // shared state already holds a different token, invalidate + refetch.
        let url = spawn_broker(r#"{"access_token":"sk-remote-new","expires":9999999999999}"#).await;
        let source = CredentialSource::Remote {
            endpoint: url,
            machine_token: "m".into(),
        };
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-other".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
        }));
        AuthMethods::reload_after_rejection(
            Arc::clone(&auth),
            &Client::new(),
            &source,
            &TokenCache::new(),
            Some("sk-sent"),
        )
        .await
        .unwrap();
        assert_eq!(auth.read().await.auth_token, "sk-remote-new");
    }

    #[tokio::test]
    async fn reload_after_rejection_local_api_key_is_rejected_not_reloaded() {
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "key".into(),
            auth_type: "api_key".into(),
            refresh_token: None,
            token_expires: None,
            auth_file_mtime: None,
        }));
        let err = AuthMethods::reload_after_rejection(
            Arc::clone(&auth),
            &Client::new(),
            &CredentialSource::Local,
            &TokenCache::new(),
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("nothing to reload"), "{err}");
        assert_eq!(auth.read().await.auth_token, "key", "state untouched");
    }

    #[test]
    fn scrub_for_remote_clears_local_credential_and_refresh_token() {
        let mut s = AuthState {
            auth_token: "local-token".into(),
            auth_type: "oauth".into(),
            refresh_token: Some("LEAKED-REFRESH".into()),
            token_expires: Some(123),
            auth_file_mtime: None,
        };
        AuthMethods::scrub_for_remote(&mut s);
        assert!(s.auth_token.is_empty(), "access token must be cleared");
        assert_eq!(
            s.refresh_token, None,
            "refresh token must be dropped (invariant 1)"
        );
        assert_eq!(
            s.token_expires, None,
            "expiry must be cleared to force a broker fetch"
        );
        assert_eq!(
            s.auth_type, "oauth",
            "auth_type stays oauth so Local early-return is skipped"
        );
    }

    /// Write Anthropic OAuth creds with an explicit expiry (ms since epoch).
    fn write_creds_expiring(base: &std::path::Path, access: &str, expires_ms: u64) {
        let body = serde_json::json!({
            "anthropic": {
                "type": "oauth",
                "refresh": "rt-local",
                "access": access,
                "expires": expires_ms,
            }
        });
        std::fs::write(base.join("auth.json"), body.to_string()).unwrap();
    }

    /// Stand-in for ANOTHER synaps process mid-refresh: hold the per-provider
    /// cross-process refresh flock (flock is per open file description, so
    /// this contends exactly like a foreign process would).
    fn hold_foreign_refresh_lock(base: &std::path::Path) -> std::fs::File {
        use fs4::fs_std::FileExt;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(base.join("auth.json.anthropic.refresh.lock"))
            .unwrap();
        assert!(FileExt::try_lock_exclusive(&f).unwrap());
        f
    }

    fn offline_client() -> Client {
        Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .build()
            .unwrap()
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn forced_refresh_waits_on_cross_process_lock_and_adopts_rotated_token() {
        // Revocation hits two processes at once. The other one holds the
        // refresh lock and rotates auth.json; we must wait for it and adopt
        // its token instead of spending the same refresh token. The client
        // is offline, so success proves no network refresh happened here.
        let base = crate::test_env::BaseDirGuard::new();
        write_creds_expiring(base.path(), "sk-revoked", 9_999_999_999_999);
        let lock = hold_foreign_refresh_lock(base.path());
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-revoked".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
        }));
        let task = {
            let auth = Arc::clone(&auth);
            tokio::spawn(async move {
                AuthMethods::reload_after_rejection(
                    auth,
                    &offline_client(),
                    &CredentialSource::Local,
                    &TokenCache::new(),
                    Some("sk-revoked"),
                )
                .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !task.is_finished(),
            "must wait for the foreign refresh lock"
        );
        write_creds_expiring(base.path(), "sk-rotated", 9_999_999_999_999);
        drop(lock);
        task.await.unwrap().expect("adopts the rotated token");
        assert_eq!(auth.read().await.auth_token, "sk-rotated");
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn expiry_refresh_waits_on_cross_process_lock_and_adopts_rotated_token() {
        // Two processes whose token expires together: the other one holds
        // the refresh lock and rotates auth.json. The normal expiry path must
        // wait and adopt its token rather than spend the same refresh token.
        // Offline client: success proves no network refresh happened here.
        let base = crate::test_env::BaseDirGuard::new();
        write_creds_expiring(base.path(), "sk-expired", 1_000);
        let lock = hold_foreign_refresh_lock(base.path());
        let auth = unloaded_local_oauth();
        let task = {
            let auth = Arc::clone(&auth);
            tokio::spawn(async move {
                AuthMethods::refresh_if_needed(
                    auth,
                    &offline_client(),
                    &CredentialSource::Local,
                    &TokenCache::new(),
                )
                .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !task.is_finished(),
            "expiry refresh must wait for the foreign refresh lock"
        );
        write_creds_expiring(base.path(), "sk-rotated", 9_999_999_999_999);
        drop(lock);
        task.await.unwrap().expect("adopts the rotated token");
        assert_eq!(auth.read().await.auth_token, "sk-rotated");
    }
}
