//! Request-construction helpers for Anthropic API calls.
//!
//! Extracted from `api.rs`. Holds auth/beta header builders shared by the
//! streaming and non-streaming code paths. All methods are added to
//! `ApiMethods` via an additional `impl` block.

use std::sync::Arc;
use tokio::sync::RwLock;

use super::api::{ApiMethods, ApiOptions};
use super::types::AuthState;

impl ApiMethods {
    /// Build the auth header for Anthropic requests.
    /// Returns `(header_name, header_value, auth_type)`.
    pub(super) async fn build_auth_header(
        auth: &Arc<RwLock<AuthState>>,
    ) -> (String, String, String) {
        let (auth_token, auth_type) = {
            let a = auth.read().await;
            (a.auth_token.clone(), a.auth_type.clone())
        };
        let (name, value) = if auth_type == "oauth" {
            ("authorization".to_string(), format!("Bearer {}", auth_token))
        } else {
            ("x-api-key".to_string(), auth_token)
        };
        (name, value, auth_type)
    }

    /// Build the `anthropic-beta` header value. Returns `None` when no beta
    /// flags apply.
    pub(super) fn build_beta_header(
        auth_type: &str,
        options: &ApiOptions,
        model: &str,
    ) -> Option<String> {
        let mut betas: Vec<&str> = Vec::new();
        if auth_type == "oauth" {
            betas.push("claude-code-20250219");
            betas.push("oauth-2025-04-20");
        }
        if options.use_1m_context && crate::core::models::model_supports_1m(model) {
            betas.push("context-1m-2025-08-07");
        }
        if betas.is_empty() {
            None
        } else {
            Some(betas.join(","))
        }
    }
}
