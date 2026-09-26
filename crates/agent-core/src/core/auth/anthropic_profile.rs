//! Anthropic OAuth profile lookup: the seat identity behind a Claude
//! credential.
//!
//! Anthropic access tokens are opaque (no JWT claims), so unlike Codex the
//! account behind a credential cannot be read off the token. The only source
//! is the read-only `GET /api/oauth/profile` endpoint, which returns the
//! `account.uuid` (the quota-bearing entity) plus display metadata.
//!
//! Same design rules as [`super::usage`]:
//!
//! - **Read-only, pinned destination.** One `GET` to a constant URL. The only
//!   override is a loopback-only test seam that is rejected for any other
//!   host, so a bearer token can never be coaxed toward another destination.
//! - **Bounded time and body.** Per-request timeout; the body is streamed
//!   into a capped buffer and dropped on overflow.
//! - **Sanitized errors.** No error string ever contains the token, the URL
//!   query, or a byte of an upstream body. Parse errors use a fixed
//!   vocabulary — never raw upstream key names or values.
//! - **No token material in the result.** [`AnthropicProfile`] holds only
//!   identity fields.

use std::time::Duration;

use serde_json::Value;

/// Pinned Anthropic OAuth profile endpoint.
pub const ANTHROPIC_PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";

/// Beta header every Anthropic OAuth API call carries. Must stay identical to
/// the (private) constant of the same name in `usage.rs`.
pub const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";

/// Total time budget for one profile request.
pub const PROFILE_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Hard cap on a buffered profile body. Real payloads are under 2 KiB.
pub const PROFILE_MAX_BODY_BYTES: usize = 256 * 1024;

/// Upper bound on any identity string accepted from the profile (uuids are
/// 36 chars; emails are at most 254). Longer values are treated as malformed.
const MAX_FIELD_LEN: usize = 254;

/// Non-secret identity of the Claude account behind an OAuth credential.
/// `account_uuid` is the seat identity used for duplicate detection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnthropicProfile {
    /// `account.uuid` — the quota-bearing entity. Two slots holding the same
    /// value are two refresh owners for one seat.
    pub account_uuid: String,
    /// `account.email`, display only.
    pub email: Option<String>,
    /// `organization.uuid`, informational (dedupe is on `account_uuid`).
    pub organization_uuid: Option<String>,
    /// `organization.organization_type` (e.g. `claude_max`).
    pub organization_type: Option<String>,
    pub has_claude_max: Option<bool>,
    pub has_claude_pro: Option<bool>,
}

/// Bounded, trimmed, non-empty string at `path`; `None` for null/missing/
/// non-string/empty. Over-long values are `None` as well (never truncated
/// into a *different* identity).
fn bounded_string(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= MAX_FIELD_LEN)
        .map(str::to_string)
}

/// Pure parser for the profile body. Fails closed when `account.uuid` is
/// missing, empty, non-string or over-long; every other field is optional
/// and unknown/null fields are ignored.
pub fn parse_anthropic_profile(body: &str) -> Result<AnthropicProfile, String> {
    if body.len() > PROFILE_MAX_BODY_BYTES {
        return Err(format!(
            "profile body exceeds {PROFILE_MAX_BODY_BYTES} bytes"
        ));
    }
    let root: Value =
        serde_json::from_str(body).map_err(|_| "profile body is not valid JSON".to_string())?;
    let root = root
        .as_object()
        .ok_or_else(|| "profile body is not a JSON object".to_string())?;
    let account = root
        .get("account")
        .and_then(Value::as_object)
        .ok_or_else(|| "profile has no account object".to_string())?;
    let account_uuid = match account.get("uuid") {
        None | Some(Value::Null) => return Err("profile account has no uuid".into()),
        Some(Value::String(_)) => bounded_string(account.get("uuid"))
            .ok_or_else(|| "profile account uuid is empty or over-long".to_string())?,
        Some(_) => return Err("profile account uuid is not a string".into()),
    };
    let organization = root.get("organization").and_then(Value::as_object);
    Ok(AnthropicProfile {
        account_uuid,
        email: bounded_string(account.get("email")),
        organization_uuid: bounded_string(organization.and_then(|o| o.get("uuid"))),
        organization_type: bounded_string(organization.and_then(|o| o.get("organization_type"))),
        has_claude_max: account.get("has_claude_max").and_then(Value::as_bool),
        has_claude_pro: account.get("has_claude_pro").and_then(Value::as_bool),
    })
}

/// True for `http(s)://127.0.0.1…`, `localhost`, `[::1]`.
fn is_loopback_url(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

fn resolve_endpoint(override_url: Option<&str>) -> Result<String, String> {
    match override_url {
        None => Ok(ANTHROPIC_PROFILE_URL.to_string()),
        Some(u) if is_loopback_url(u) => Ok(u.to_string()),
        Some(_) => Err("profile endpoint override must be a loopback URL".into()),
    }
}

/// Coarse transport class; never the URL, the token or a body.
fn classify_reqwest_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "profile request timed out".into()
    } else if e.is_connect() {
        "profile request could not connect".into()
    } else if e.is_body() || e.is_decode() {
        "profile response body could not be read".into()
    } else {
        "profile request failed".into()
    }
}

/// Stream the body into a capped buffer. Fail closed on overflow.
async fn read_body_capped(resp: reqwest::Response, cap: usize) -> Result<String, String> {
    use futures::StreamExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| classify_reqwest_error(&e))?;
        if buf.len().saturating_add(chunk.len()) > cap {
            return Err(format!("profile body exceeds {cap} bytes"));
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| "profile body is not UTF-8".to_string())
}

/// `GET` the profile for `access_token` and parse it. `endpoint_override` is
/// a loopback-only test seam. The caller's `client` should follow no
/// redirects (see [`super::identity::identity_http_client`]); a 3xx is an
/// error here regardless. Errors never contain the token or upstream bytes.
pub async fn fetch_anthropic_profile(
    client: &reqwest::Client,
    access_token: &str,
    endpoint_override: Option<&str>,
) -> Result<AnthropicProfile, String> {
    let url = resolve_endpoint(endpoint_override)?;
    let resp = client
        .get(&url)
        .timeout(PROFILE_REQUEST_TIMEOUT)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("anthropic-beta", ANTHROPIC_OAUTH_BETA)
        .send()
        .await
        .map_err(|e| classify_reqwest_error(&e))?;
    let status = resp.status().as_u16();
    // Non-2xx bodies are upstream-controlled and may echo the request: dropped unread.
    match status {
        200..=299 => {}
        300..=399 => {
            drop(resp);
            return Err(format!("profile request was redirected (HTTP {status})"));
        }
        401 | 403 => {
            drop(resp);
            return Err(format!("profile request was rejected (HTTP {status})"));
        }
        429 => {
            drop(resp);
            return Err("profile request was rate limited (HTTP 429)".into());
        }
        _ => {
            drop(resp);
            return Err(format!("profile request failed (HTTP {status})"));
        }
    }
    let body = read_body_capped(resp, PROFILE_MAX_BODY_BYTES).await?;
    parse_anthropic_profile(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Redacted live shape (values replaced). Two accounts, distinct uuids.
    const PROFILE_A: &str = r#"{
      "account": {"uuid":"b8a1448d-0000-4000-8000-00000000000a","full_name":"A Person","display_name":"A","email":"jr.a@example.com","has_claude_max":true,"has_claude_pro":false,"created_at":"2026-02-01T00:00:00Z"},
      "organization": {"uuid":"3895e3cf-0000-4000-8000-00000000000a","name":"A's Organization","organization_type":"claude_max","billing_type":"stripe_subscription","rate_limit_tier":"default_claude_max_5x","seat_tier":null,"has_extra_usage_enabled":true,"subscription_status":"active","some_future_field":null},
      "application": {"uuid":"9d1c250a-e61b-44d9-88ed-5944d1962f5e","name":"Claude Code","slug":"claude-code"},
      "enabled_plugins": []
    }"#;
    const PROFILE_B: &str = r#"{
      "account": {"uuid":"c9b2559e-0000-4000-8000-00000000000b","email":"jr.b@example.com","has_claude_max":false,"has_claude_pro":true},
      "organization": {"uuid":"4906f4d0-0000-4000-8000-00000000000b","organization_type":"claude_pro"}
    }"#;

    #[test]
    fn parses_redacted_live_shape_for_two_distinct_accounts() {
        let a = parse_anthropic_profile(PROFILE_A).unwrap();
        assert_eq!(a.account_uuid, "b8a1448d-0000-4000-8000-00000000000a");
        assert_eq!(a.email.as_deref(), Some("jr.a@example.com"));
        assert_eq!(
            a.organization_uuid.as_deref(),
            Some("3895e3cf-0000-4000-8000-00000000000a")
        );
        assert_eq!(a.organization_type.as_deref(), Some("claude_max"));
        assert_eq!(a.has_claude_max, Some(true));
        assert_eq!(a.has_claude_pro, Some(false));

        let b = parse_anthropic_profile(PROFILE_B).unwrap();
        assert_ne!(
            a.account_uuid, b.account_uuid,
            "distinct accounts, distinct seats"
        );
        assert_ne!(a.organization_uuid, b.organization_uuid);
        assert_eq!(b.has_claude_pro, Some(true));

        // Same body twice → same seat identity (what the dedupe relies on).
        assert_eq!(parse_anthropic_profile(PROFILE_A).unwrap(), a);
    }

    #[test]
    fn missing_or_bad_account_uuid_is_an_error() {
        for body in [
            r#"{"account":{"email":"x@example.com"}}"#,
            r#"{"account":{"uuid":null}}"#,
            r#"{"account":{"uuid":""}}"#,
            r#"{"account":{"uuid":"   "}}"#,
            r#"{"account":{"uuid":42}}"#,
            r#"{"account":{"uuid":{"nested":"SECRET-VALUE"}}}"#,
            r#"{"account":"not-an-object"}"#,
            r#"{"organization":{"uuid":"3895e3cf"}}"#,
            r#"[]"#,
            r#"not json"#,
            "",
        ] {
            let err = parse_anthropic_profile(body).unwrap_err();
            assert!(
                !err.contains("SECRET-VALUE"),
                "error must not echo values: {err}"
            );
            assert!(
                !err.contains("3895e3cf"),
                "error must not echo values: {err}"
            );
        }
        let long = "x".repeat(MAX_FIELD_LEN + 1);
        let body = format!(r#"{{"account":{{"uuid":"{long}"}}}}"#);
        assert!(
            parse_anthropic_profile(&body).is_err(),
            "over-long uuid is malformed"
        );
    }

    #[test]
    fn null_and_missing_optionals_are_tolerated() {
        let p = parse_anthropic_profile(
            r#"{"account":{"uuid":"b8a1448d","email":null,"has_claude_max":null,"has_claude_pro":"yes"},"organization":null,"unknown":{"deep":[1,2,3]}}"#,
        )
        .unwrap();
        assert_eq!(p.account_uuid, "b8a1448d");
        assert_eq!(p.email, None);
        assert_eq!(p.organization_uuid, None);
        assert_eq!(p.organization_type, None);
        assert_eq!(p.has_claude_max, None);
        assert_eq!(p.has_claude_pro, None, "non-bool is unknown, not coerced");

        let p = parse_anthropic_profile(r#"{"account":{"uuid":" b8a1448d "}}"#).unwrap();
        assert_eq!(p.account_uuid, "b8a1448d", "trimmed");
        // Over-long optional fields are dropped, never truncated into a different value.
        let long = "e".repeat(MAX_FIELD_LEN + 1);
        let p = parse_anthropic_profile(&format!(
            r#"{{"account":{{"uuid":"b8a1448d","email":"{long}"}}}}"#
        ))
        .unwrap();
        assert_eq!(p.email, None);
    }

    #[test]
    fn oversize_body_is_rejected_before_parsing() {
        let padding = " ".repeat(PROFILE_MAX_BODY_BYTES);
        let body = format!(r#"{{"account":{{"uuid":"b8a1448d"}}}}{padding}"#);
        let err = parse_anthropic_profile(&body).unwrap_err();
        assert!(err.contains("exceeds"), "{err}");
    }

    #[test]
    fn endpoint_override_is_loopback_only() {
        assert_eq!(resolve_endpoint(None).unwrap(), ANTHROPIC_PROFILE_URL);
        assert!(resolve_endpoint(Some("http://127.0.0.1:1/p")).is_ok());
        assert!(resolve_endpoint(Some("http://localhost:1/p")).is_ok());
        assert!(resolve_endpoint(Some("http://[::1]:1/p")).is_ok());
        for bad in [
            "https://evil.example/profile",
            "http://10.0.0.1/profile",
            "ftp://127.0.0.1/profile",
            "127.0.0.1",
            "",
        ] {
            assert!(resolve_endpoint(Some(bad)).is_err(), "{bad}");
        }
    }

    // ── Loopback HTTP ────────────────────────────────────────────────────────

    async fn spawn(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn fetch_sends_bearer_and_beta_header_and_parses() {
        use axum::{extract::Request, routing::get};
        let app = axum::Router::new().route(
            "/profile",
            get(|req: Request| async move {
                let h = req.headers();
                assert_eq!(
                    h.get("authorization").unwrap(),
                    "Bearer anthropic-access-token"
                );
                assert_eq!(h.get("anthropic-beta").unwrap(), ANTHROPIC_OAUTH_BETA);
                assert_eq!(h.get("accept").unwrap(), "application/json");
                ([("content-type", "application/json")], PROFILE_A)
            }),
        );
        let url = spawn(app).await;
        let p = fetch_anthropic_profile(
            &client(),
            "anthropic-access-token",
            Some(&format!("{url}/profile")),
        )
        .await
        .unwrap();
        assert_eq!(p.account_uuid, "b8a1448d-0000-4000-8000-00000000000a");
        assert_eq!(p.email.as_deref(), Some("jr.a@example.com"));
    }

    #[tokio::test]
    async fn fetch_rejects_non_loopback_override_before_sending() {
        let err = fetch_anthropic_profile(&client(), "tok", Some("https://evil.example/profile"))
            .await
            .unwrap_err();
        assert!(err.contains("loopback"), "{err}");
    }

    #[tokio::test]
    async fn fetch_error_statuses_never_echo_body_or_token() {
        use axum::{http::StatusCode, routing::get};
        let app = axum::Router::new()
            .route(
                "/401",
                get(|| async { (StatusCode::UNAUTHORIZED, "UPSTREAM-SECRET-BODY") }),
            )
            .route(
                "/500",
                get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "UPSTREAM-SECRET-BODY") }),
            )
            .route(
                "/302",
                get(|| async {
                    (
                        StatusCode::FOUND,
                        [("location", "https://evil.example/")],
                        "",
                    )
                }),
            )
            .route(
                "/429",
                get(|| async { (StatusCode::TOO_MANY_REQUESTS, "") }),
            );
        let url = spawn(app).await;
        for (path, needle) in [
            ("401", "rejected"),
            ("500", "HTTP 500"),
            ("302", "redirected"),
            ("429", "rate limited"),
        ] {
            let err = fetch_anthropic_profile(
                &client(),
                "SECRET-ACCESS-TOKEN",
                Some(&format!("{url}/{path}")),
            )
            .await
            .unwrap_err();
            assert!(err.contains(needle), "{path}: {err}");
            assert!(!err.contains("UPSTREAM-SECRET-BODY"), "{path}: {err}");
            assert!(!err.contains("SECRET-ACCESS-TOKEN"), "{path}: {err}");
        }
    }

    #[tokio::test]
    async fn fetch_caps_oversize_body() {
        use axum::routing::get;
        let big = format!(
            r#"{{"account":{{"uuid":"b8a1448d"}},"pad":"{}"}}"#,
            "x".repeat(PROFILE_MAX_BODY_BYTES)
        );
        let app = axum::Router::new().route("/profile", get(move || async move { big }));
        let url = spawn(app).await;
        let err = fetch_anthropic_profile(&client(), "tok", Some(&format!("{url}/profile")))
            .await
            .unwrap_err();
        assert!(err.contains("exceeds"), "{err}");
    }
}
