use std::sync::Arc;
use tokio::sync::RwLock;
use crate::{Result, RuntimeError};
use reqwest::Client;
use super::types::{AuthState, PiAuth};

pub(super) struct AuthMethods;

impl AuthMethods {
    /// Check if the OAuth token is expired and refresh it if needed.
    /// Uses Pi-style file locking for cross-process safety:
    /// - Acquires exclusive lock on auth.json
    /// - Re-reads inside the lock (another instance may have refreshed)
    /// - Refreshes via API only if still expired
    /// - Writes back atomically and releases lock
    ///
    /// Multiple SynapsCLI instances (or Avante/Jade) can safely call this
    /// simultaneously — they'll serialize on the lock and only one will
    /// actually hit the token endpoint.
    pub(super) async fn refresh_if_needed(auth: Arc<RwLock<AuthState>>, client: &Client) -> Result<()> {
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
                "Token refresh failed: {}. Run `login` to re-authenticate.", e
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
        
        Err(RuntimeError::Auth("No Anthropic credentials found. Run `login` to authenticate.".to_string()))
    }
}