//! Google Gemini (Code Assist) OAuth: authorization-code + PKCE.
//!
//! Experimental. The user-facing endpoints (`accounts.google.com/o/oauth2/v2/auth`,
//! `oauth2.googleapis.com/token`) are official Google OAuth 2.0 surfaces. The
//! Code Assist runtime host (`cloudcode-pa.googleapis.com/v1internal`) that
//! consumes the resulting access token is a product-client-observed integration
//! surface and is treated as **experimental** — it is not described as a stable
//! public third-party API.
//!
//! Credential mapping:
//! - `OAuthCredentials.refresh` = long-lived Google refresh token (broker-owned only)
//! - `OAuthCredentials.access`  = short-lived Google access token
//! - `OAuthCredentials.expires` = access-token expiry (ms, with skew)
//!
//! The long-lived refresh token must never be vended, logged, or placed in
//! broker wire types. This module operates only inside the auth boundary.

use std::time::Duration;

use reqwest::Client;

use super::OAuthCredentials;

// ── Pinned endpoints ─────────────────────────────────────────────────────────

/// Canonical storage / broker id.
pub const PROVIDER: &str = "google-gemini";

/// Google installed-app OAuth authorization endpoint (RFC 6749).
pub const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";

/// Google OAuth 2.0 token endpoint.
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Google userinfo endpoint (for optional post-auth account labeling).
pub const USERINFO_URL: &str = "https://openidconnect.googleapis.com/v1/userinfo";

/// Space-separated scopes. Same values as the official Gemini CLI reference:
/// cloud-platform (Code Assist), userinfo.email, userinfo.profile.
pub const SCOPES: &str = concat!(
    "https://www.googleapis.com/auth/cloud-platform",
    " https://www.googleapis.com/auth/userinfo.email",
    " https://www.googleapis.com/auth/userinfo.profile",
);

/// OAuth installed-app client id observed in the official Gemini CLI source
/// (`packages/core/src/code_assist/oauth2.ts`). Per Google's own installed-app
/// documentation the paired "secret" is not a true secret; we still send it
/// verbatim to the token endpoint but never expose it outside this module.
pub const CLIENT_ID: &str =
    "redacted-google-desktop-client.invalid";

/// See CLIENT_ID note: not a confidential secret in the RFC 6749 sense — it is
/// an installed-app "secret" that the Gemini CLI publishes in its source.
pub const CLIENT_SECRET: &str = "redacted-public-client-value";

/// Loopback callback host; RFC 8252 § 7.3 mandates a literal IP for installed
/// apps. Google explicitly rejects `localhost` for some client types.
pub const CALLBACK_HOST: &str = "127.0.0.1";

/// Callback path — pinned distinct from the shared `/callback` to avoid state
/// bleed with the other loopback providers.
pub const CALLBACK_PATH: &str = "/oauth2callback";

/// Default loopback port for the Gemini callback listener.
pub const CALLBACK_PORT: u16 = 45289;

/// Expiry safety-margin subtracted from `now + expires_in`.
pub const EXPIRY_SKEW_MS: u64 = 5 * 60 * 1000;

/// Connect timeout for auth HTTP.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Full request timeout for auth HTTP.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum response body bytes we will accept from Google OAuth endpoints.
pub const MAX_RESPONSE_BODY_BYTES: usize = 64 * 1024;

// ── Login / refresh stubs — implemented in later slices ──────────────────────

pub async fn login() -> Result<OAuthCredentials, String> {
    Err("google-gemini interactive login is not yet implemented".to_string())
}

pub async fn refresh_token(_client: &Client, _refresh: &str) -> Result<OAuthCredentials, String> {
    Err("google-gemini refresh is not yet implemented".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    #[test]
    fn endpoints_are_https_and_pinned_to_google() {
        for endpoint in [AUTHORIZE_URL, TOKEN_URL, USERINFO_URL] {
            let url = Url::parse(endpoint).expect("valid URL");
            assert_eq!(url.scheme(), "https", "{endpoint} must be https");
            let host = url.host_str().expect("has host");
            assert!(
                host.ends_with("google.com") || host.ends_with("googleapis.com"),
                "{endpoint} must be a google endpoint (got {host})"
            );
        }
    }

    #[test]
    fn scopes_include_cloud_platform_and_userinfo() {
        // Cloud Platform is what unlocks Code Assist; userinfo is required for
        // the eventual account label. Order must be stable so it doesn't drift.
        assert!(SCOPES.contains("https://www.googleapis.com/auth/cloud-platform"));
        assert!(SCOPES.contains("https://www.googleapis.com/auth/userinfo.email"));
        assert!(SCOPES.contains("https://www.googleapis.com/auth/userinfo.profile"));
    }

    #[test]
    fn callback_uses_loopback_ip_literal() {
        // RFC 8252 §7.3: installed apps must use a loopback IP literal, not
        // "localhost", to avoid DNS-based interception.
        assert_eq!(CALLBACK_HOST, "127.0.0.1");
        assert!(CALLBACK_PATH.starts_with('/'));
        assert_ne!(CALLBACK_PATH, "/callback", "must not collide with other providers");
    }

    #[test]
    fn provider_id_matches_typed_registry_key() {
        assert_eq!(PROVIDER, super::super::provider::OAuthProviderId::GoogleGemini.as_str());
    }

    #[tokio::test]
    async fn login_and_refresh_stubs_return_secret_free_error() {
        let err = login().await.unwrap_err();
        assert!(!err.is_empty());
        assert!(!err.contains(CLIENT_SECRET));

        let err = refresh_token(&Client::new(), "irrelevant-refresh").await.unwrap_err();
        assert!(!err.is_empty());
        assert!(!err.contains(CLIENT_SECRET));
        assert!(!err.contains("irrelevant-refresh"));
    }
}
