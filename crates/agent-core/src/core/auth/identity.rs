//! Provider-generic seat identity resolution.
//!
//! One entry point, [`resolve_seat`], answers "which provider account does
//! this access token belong to?" so the login guard, the `auth identify`
//! backfill and the listing all key duplicate detection on the same value:
//!
//! - **OpenAI Codex** — read off the JWT (`chatgpt_account_id` claim, email
//!   from the profile claim). No network.
//! - **Anthropic** — tokens are opaque; one read-only `GET` to the pinned
//!   profile endpoint ([`super::anthropic_profile`]). Seat = `account.uuid`.
//! - **Every other provider** — no trustworthy identity is exposed;
//!   [`SeatResolution::Unsupported`].
//!
//! Nothing here reads or writes `auth.json`, and no variant ever carries
//! token material. A failure is reported, never turned into a guess.

use super::provider::OAuthProviderId;

/// Identity of the seat an access token belongs to. `account_id` is the
/// provider's FULL account id (the value stored as `accountId` and hashed by
/// [`super::seat_fingerprint`]); `identity` is a display-only label such as
/// an email. Neither is a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSeat {
    pub account_id: String,
    pub identity: Option<String>,
}

/// Outcome of [`resolve_seat`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SeatResolution {
    Resolved(ResolvedSeat),
    /// The provider (or this particular token) exposes no identity. A
    /// duplicate of another slot cannot be detected.
    Unsupported,
    /// The provider does expose an identity but it could not be obtained
    /// (network, upstream status, parse). The message is secret-free.
    Failed(String),
}

/// True when [`resolve_seat`] can, in principle, return
/// [`SeatResolution::Resolved`] for `provider`. Used by listings to point at
/// slots whose identity is still unverified.
pub fn supports_seat_identity(provider: OAuthProviderId) -> bool {
    matches!(
        provider,
        OAuthProviderId::Anthropic | OAuthProviderId::OpenAiCodex
    )
}

/// HTTP client with the policy identity lookups (and the token refresh that
/// may precede them) require: no redirects, bounded connect/total timeouts.
pub fn identity_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(concat!("SynapsCLI/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| "could not build HTTP client".to_string())
}

/// Resolve the seat behind `access_token` for `provider`. `endpoint_override`
/// is a loopback-only test seam for the network-backed providers.
pub async fn resolve_seat(
    provider: OAuthProviderId,
    access_token: &str,
    client: &reqwest::Client,
    endpoint_override: Option<&str>,
) -> SeatResolution {
    match provider {
        OAuthProviderId::OpenAiCodex => {
            match super::openai_codex::extract_account_id(access_token) {
                Some(account_id) => SeatResolution::Resolved(ResolvedSeat {
                    account_id,
                    identity: super::openai_codex::extract_email(access_token),
                }),
                None => SeatResolution::Unsupported,
            }
        }
        OAuthProviderId::Anthropic => {
            match super::anthropic_profile::fetch_anthropic_profile(
                client,
                access_token,
                endpoint_override,
            )
            .await
            {
                Ok(profile) => SeatResolution::Resolved(ResolvedSeat {
                    account_id: profile.account_uuid,
                    identity: profile.email,
                }),
                Err(msg) => SeatResolution::Failed(msg),
            }
        }
        OAuthProviderId::Xai
        | OAuthProviderId::GitHubCopilot
        | OAuthProviderId::GoogleGemini
        | OAuthProviderId::KimiCode => SeatResolution::Unsupported,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

    fn jwt(claims: serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        format!("{header}.{payload}.sig")
    }

    fn client() -> reqwest::Client {
        identity_http_client().unwrap()
    }

    #[test]
    fn identity_support_matrix() {
        assert!(supports_seat_identity(OAuthProviderId::Anthropic));
        assert!(supports_seat_identity(OAuthProviderId::OpenAiCodex));
        for p in [
            OAuthProviderId::Xai,
            OAuthProviderId::GitHubCopilot,
            OAuthProviderId::GoogleGemini,
            OAuthProviderId::KimiCode,
        ] {
            assert!(!supports_seat_identity(p), "{p}");
        }
    }

    #[tokio::test]
    async fn codex_resolves_from_jwt_without_network() {
        let token = jwt(serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct_123"},
            "https://api.openai.com/profile": {"email": "codex@example.com"}
        }));
        // A dead override proves no request is made for Codex.
        let out = resolve_seat(
            OAuthProviderId::OpenAiCodex,
            &token,
            &client(),
            Some("http://127.0.0.1:9/never"),
        )
        .await;
        assert_eq!(
            out,
            SeatResolution::Resolved(ResolvedSeat {
                account_id: "acct_123".into(),
                identity: Some("codex@example.com".into()),
            })
        );
        // Same values the login flow used to derive directly.
        assert_eq!(
            super::super::openai_codex::extract_email(&token).as_deref(),
            Some("codex@example.com")
        );
        // Email absent → identity None, seat still resolved.
        let token = jwt(serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct_123"}
        }));
        assert_eq!(
            resolve_seat(OAuthProviderId::OpenAiCodex, &token, &client(), None).await,
            SeatResolution::Resolved(ResolvedSeat {
                account_id: "acct_123".into(),
                identity: None,
            })
        );
    }

    #[tokio::test]
    async fn codex_without_claim_and_other_providers_are_unsupported() {
        let token = jwt(serde_json::json!({
            "https://api.openai.com/profile": {"email": "codex@example.com"}
        }));
        assert_eq!(
            resolve_seat(OAuthProviderId::OpenAiCodex, &token, &client(), None).await,
            SeatResolution::Unsupported
        );
        assert_eq!(
            resolve_seat(OAuthProviderId::OpenAiCodex, "not-a-jwt", &client(), None).await,
            SeatResolution::Unsupported
        );
        for p in [
            OAuthProviderId::Xai,
            OAuthProviderId::GitHubCopilot,
            OAuthProviderId::GoogleGemini,
            OAuthProviderId::KimiCode,
        ] {
            assert_eq!(
                resolve_seat(p, "opaque", &client(), Some("http://127.0.0.1:9/never")).await,
                SeatResolution::Unsupported,
                "{p}"
            );
        }
    }

    async fn spawn(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn anthropic_resolves_account_uuid_and_email_from_profile() {
        use axum::routing::get;
        let app = axum::Router::new().route(
            "/profile",
            get(|| async {
                (
                    [("content-type", "application/json")],
                    r#"{"account":{"uuid":"b8a1448d-0000-4000-8000-00000000000a","email":"jr.a@example.com"},"organization":{"uuid":"3895e3cf"}}"#,
                )
            }),
        );
        let url = spawn(app).await;
        let out = resolve_seat(
            OAuthProviderId::Anthropic,
            "anthropic-access-token",
            &client(),
            Some(&format!("{url}/profile")),
        )
        .await;
        assert_eq!(
            out,
            SeatResolution::Resolved(ResolvedSeat {
                account_id: "b8a1448d-0000-4000-8000-00000000000a".into(),
                identity: Some("jr.a@example.com".into()),
            })
        );
    }

    #[tokio::test]
    async fn anthropic_failure_is_reported_without_secrets() {
        use axum::{http::StatusCode, routing::get};
        let app = axum::Router::new().route(
            "/profile",
            get(|| async { (StatusCode::BAD_GATEWAY, "UPSTREAM-SECRET-BODY") }),
        );
        let url = spawn(app).await;
        let out = resolve_seat(
            OAuthProviderId::Anthropic,
            "SECRET-ACCESS-TOKEN",
            &client(),
            Some(&format!("{url}/profile")),
        )
        .await;
        let SeatResolution::Failed(msg) = out else {
            panic!("expected Failed, got {out:?}");
        };
        assert!(msg.contains("HTTP 502"), "{msg}");
        assert!(!msg.contains("SECRET"), "{msg}");
        // Non-loopback override never sends.
        let out = resolve_seat(
            OAuthProviderId::Anthropic,
            "SECRET-ACCESS-TOKEN",
            &client(),
            Some("https://evil.example/profile"),
        )
        .await;
        assert!(matches!(out, SeatResolution::Failed(m) if m.contains("loopback")));
    }
}
