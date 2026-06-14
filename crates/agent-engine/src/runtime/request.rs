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
    ///
    /// The extended-cache-ttl token is added ONLY for API-key auth: the live
    /// probe confirmed 1h TTL is honored bare over OAuth, and the OAuth beta
    /// set is part of the pool-routing fingerprint — we do not perturb it for
    /// a feature that works without it (spec §3.4).
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
        if auth_type != "oauth" && options.cache_ttl != crate::core::config::CacheTtl::FiveMinutes {
            betas.push("extended-cache-ttl-2025-04-11");
        }
        if betas.is_empty() {
            None
        } else {
            Some(betas.join(","))
        }
    }
}

#[cfg(test)]
mod beta_header_tests {
    use super::*;
    use crate::core::config::CacheTtl;

    fn opts(ttl: CacheTtl) -> ApiOptions {
        ApiOptions { cache_ttl: ttl, ..Default::default() }
    }

    const MODEL: &str = "claude-sonnet-4-6";

    #[test]
    fn api_key_5m_emits_no_header() {
        // DEFAULT MUST BE INVISIBLE: no header where there was none before.
        assert_eq!(ApiMethods::build_beta_header("api_key", &opts(CacheTtl::FiveMinutes), MODEL), None);
    }

    #[test]
    fn api_key_1h_and_hybrid_emit_extended_ttl_beta() {
        for ttl in [CacheTtl::OneHour, CacheTtl::Hybrid] {
            assert_eq!(
                ApiMethods::build_beta_header("api_key", &opts(ttl), MODEL).as_deref(),
                Some("extended-cache-ttl-2025-04-11"),
                "under {ttl:?}"
            );
        }
    }

    #[test]
    fn oauth_beta_set_unperturbed_by_cache_ttl() {
        // OAuth sends no new beta token in ANY mode — its beta set is part
        // of the pool-routing fingerprint and 1h works bare (live-probed).
        for ttl in [CacheTtl::FiveMinutes, CacheTtl::OneHour, CacheTtl::Hybrid] {
            assert_eq!(
                ApiMethods::build_beta_header("oauth", &opts(ttl), MODEL).as_deref(),
                Some("claude-code-20250219,oauth-2025-04-20"),
                "under {ttl:?}"
            );
        }
    }

    #[test]
    fn extended_ttl_comma_joins_with_1m_context() {
        let options = ApiOptions {
            use_1m_context: true,
            cache_ttl: CacheTtl::OneHour,
            ..Default::default()
        };
        // claude-sonnet-4-6 supports 1M — precondition assert so this test
        // fails LOUDLY (not silently no-ops) if the model table changes.
        assert!(
            crate::core::models::model_supports_1m(MODEL),
            "fixture model fell out of the 1M table — pick a 1M-capable fixture"
        );
        assert_eq!(
            ApiMethods::build_beta_header("api_key", &options, MODEL).as_deref(),
            Some("context-1m-2025-08-07,extended-cache-ttl-2025-04-11"),
        );
    }
}
