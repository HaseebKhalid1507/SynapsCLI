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
        {
            let auth_guard = auth.read().await;
            // A non-OAuth local auth mode (e.g. a stubbed api_key harness)
            // never contacts the broker. Remote sources always resolve via
            // the broker regardless of the seeded auth_type.
            if !source.is_remote() && auth_guard.auth_type != "oauth" {
                return Ok(());
            }
            if let Some(exp) = auth_guard.token_expires {
                let still_fresh = if source.is_remote() {
                    // Must use the SAME predicate as the remote cache
                    // (is_expired_with_margin + DEFAULT_MARGIN_MS) so the
                    // fast-path and TokenCache agree on freshness (board #1).
                    !auth_guard.auth_token.is_empty()
                        && !is_expired_with_margin(exp, DEFAULT_MARGIN_MS)
                } else {
                    !auth_guard.auth_token.is_empty() && crate::epoch_millis() < exp
                };
                if still_fresh {
                    return Ok(());
                }
            }
        }
        // Read lock dropped here

        tracing::info!("Refreshing auth token via credential broker");
        let broker = broker_from_source(source, cache, client.clone());
        let tok = broker
            .access_token(OAuthProviderId::Anthropic)
            .await
            .map_err(|e| RuntimeError::Auth(format!(
                "Token refresh failed: {}. Run `synaps login` to re-authenticate, or check auth.remote_endpoint / broker reachability.", e
            )))?;

        // Update shared auth state so all clones (including spawned stream
        // tasks) immediately see the fresh token. Never a refresh token.
        {
            let mut auth_guard = auth.write().await;
            auth_guard.auth_token = tok.token;
            auth_guard.auth_type = "oauth".to_string();
            auth_guard.refresh_token = None;
            auth_guard.token_expires = Some(tok.expires);
        }

        Ok(())
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
        }));
        AuthMethods::refresh_if_needed(Arc::clone(&auth), &Client::new(), &source, &cache)
            .await
            .unwrap();
        let g = auth.read().await;
        assert_eq!(g.auth_token, "key");
        assert_eq!(g.auth_type, "api_key");
    }

    #[test]
    fn scrub_for_remote_clears_local_credential_and_refresh_token() {
        let mut s = AuthState {
            auth_token: "local-token".into(),
            auth_type: "oauth".into(),
            refresh_token: Some("LEAKED-REFRESH".into()),
            token_expires: Some(123),
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
}
