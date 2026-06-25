use std::sync::Arc;
use tokio::sync::RwLock;
use crate::{Result, RuntimeError};
use reqwest::Client;
use crate::auth::{BrokerClient, CredentialSource, TokenCache, resolve_remote, DEFAULT_MARGIN_MS};
use super::types::{AuthState, PiAuth};

pub(super) struct AuthMethods;

impl AuthMethods {
    /// Check if the OAuth token is expired and refresh it if needed.
    ///
    /// Two credential sources:
    /// - `Remote` (#157): fetch a short-lived access token from the broker.
    ///   Independent of the local auth.json. NEVER holds a refresh token,
    ///   NEVER writes auth.json, NEVER refreshes client-side.
    /// - `Local` (default): the original Pi-style file-locked refresh below.
    ///
    /// Local path detail (unchanged):
    /// - Acquires exclusive lock on auth.json
    /// - Re-reads inside the lock (another instance may have refreshed)
    /// - Refreshes via API only if still expired
    /// - Writes back atomically and releases lock
    ///
    /// Multiple SynapsCLI instances (or Avante/Jade) can safely call this
    /// simultaneously — they'll serialize on the lock and only one will
    /// actually hit the token endpoint.
    pub(super) async fn refresh_if_needed(
        auth: Arc<RwLock<AuthState>>,
        client: &Client,
        source: &CredentialSource,
        cache: &TokenCache,
    ) -> Result<()> {
        // ── Remote credential source: resolve via the broker. ──
        if let CredentialSource::Remote { .. } = source {
            // Fast path: in-memory broker token still fresh?
            {
                let auth_guard = auth.read().await;
                if let Some(exp) = auth_guard.token_expires {
                    if !auth_guard.auth_token.is_empty() && crate::epoch_millis() < exp {
                        return Ok(());
                    }
                }
            }
            let broker = BrokerClient::from_source(source)
                .expect("CredentialSource::Remote always yields a BrokerClient");
            let tok = resolve_remote(&broker, cache, "anthropic", DEFAULT_MARGIN_MS)
                .await
                .map_err(|e| RuntimeError::Auth(format!(
                    "Broker token fetch failed: {}. Check auth.remote_endpoint, the machine token, and broker reachability.", e
                )))?;
            let mut auth_guard = auth.write().await;
            auth_guard.auth_token = tok.access_token;
            auth_guard.auth_type = "oauth".to_string();
            auth_guard.refresh_token = None; // invariant: clients never hold a refresh token
            auth_guard.token_expires = Some(tok.expires);
            return Ok(());
        }

        // ── Local credential source (default — unchanged behavior). ──
        // Fast path: read lock to check expiry without blocking writers
        {
            let auth_guard = auth.read().await;
            if auth_guard.auth_type != "oauth" {
                return Ok(());
            }

            let in_memory_expired = match auth_guard.token_expires {
                Some(exp) => {
                    let now = crate::epoch_millis();
                    now >= exp
                }
                None => false,
            };

            if !in_memory_expired {
                return Ok(());
            }
        }
        // Read lock dropped here

        tracing::info!("Token needs refresh, checking...");

        // Slow path: delegate to auth.rs which handles locking, re-read,
        // conditional refresh, and persistence.
        tracing::info!("Refreshing auth token");
        let creds = crate::auth::ensure_fresh_token(client)
            .await
            .map_err(|e| RuntimeError::Auth(format!(
                "Token refresh failed: {}. Run `synaps login` to re-authenticate.", e
            )))?;

        // Update shared auth state so all clones (including spawned stream tasks)
        // immediately see the fresh token.
        {
            let mut auth_guard = auth.write().await;
            auth_guard.auth_token = creds.access;
            auth_guard.refresh_token = Some(creds.refresh);
            auth_guard.token_expires = Some(creds.expires);
        }

        Ok(())
    }
    
    pub(super) fn get_auth_token() -> Result<(String, String, Option<String>, Option<u64>)> {
        // Try auth.json via the auth module
        if let Ok(Some(auth_file)) = crate::auth::load_auth() {
            let creds = &auth_file.anthropic;
            if creds.auth_type == "oauth" && !creds.access.is_empty() {
                return Ok((
                    creds.access.clone(),
                    "oauth".to_string(),
                    Some(creds.refresh.clone()),
                    Some(creds.expires),
                ));
            }
        }

        // Legacy: try the old PiAuth struct format (in case auth.json has optional fields)
        let auth_path = crate::config::resolve_read_path("auth.json");

        if auth_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&auth_path) {
                if let Ok(auth) = serde_json::from_str::<PiAuth>(&content) {
                    let creds = &auth.anthropic;
                    if let (true, Some(access)) = (creds.auth_type == "oauth", creds.access.as_ref()) {
                        return Ok((
                            access.clone(),
                            "oauth".to_string(),
                            creds.refresh.clone(),
                            creds.expires,
                        ));
                    }
                }
            }
        }

        // Fall back to env var
        if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
            return Ok((api_key, "api_key".to_string(), None, None));
        }
        
        // No Anthropic credentials — allow startup anyway for non-Anthropic providers.
        // Auth will fail lazily on the first actual Anthropic API call.
        Ok(("".to_string(), "none".to_string(), None, None))
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
        let source = CredentialSource::Remote { endpoint: url, machine_token: "m".into() };
        let cache = TokenCache::new();
        let auth = empty_auth();
        AuthMethods::refresh_if_needed(Arc::clone(&auth), &Client::new(), &source, &cache)
            .await
            .unwrap();
        let g = auth.read().await;
        assert_eq!(g.auth_token, "sk-broker-xyz");
        assert_eq!(g.auth_type, "oauth");
        assert_eq!(g.refresh_token, None, "Remote must clear any refresh token (invariant)");
        assert_eq!(g.token_expires, Some(9_999_999_999_999));
    }

    #[tokio::test]
    async fn remote_source_fast_path_when_token_still_fresh() {
        let url = spawn_broker(r#"{"access_token":"sk-1","expires":9999999999999}"#).await;
        let source = CredentialSource::Remote { endpoint: url, machine_token: "m".into() };
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
}
