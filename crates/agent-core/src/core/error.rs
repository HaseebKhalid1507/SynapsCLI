use thiserror::Error;

#[derive(Error, Debug)]
pub enum RuntimeError {
    #[error("API error: {0}")]
    Api(#[from] reqwest::Error),
    #[error("{0}")]
    ApiStatus(String),
    #[error("Auth error: {0}")]
    Auth(String),
    #[error("Config error: {0}")]
    Config(String),
    #[error("Session error: {0}")]
    Session(String),
    #[error("Tool execution failed: {0}")]
    Tool(String),
    #[error("Request timed out")]
    Timeout,
    #[error("Operation canceled")]
    Canceled,
}

/// Translate an Anthropic API error response into a human-actionable message.
///
/// SECURITY (spec §5.1): the response `body` — including any nested
/// `error.message` / `error.type` — is UNTRUSTED. A hostile or misconfigured
/// provider can echo the entire request (prompts, system text, tool schemas,
/// credentials) inside it, so no body-derived text is ever reproduced in the
/// returned message. The body is used only for *classification* (matching
/// against fixed, vetted patterns); output is built exclusively from static
/// guidance plus the numeric status.
pub fn humanize_api_error(status: u16, body: &str) -> String {
    humanize_api_error_with_reset(status, body, None)
}

/// Anthropic wire error types we recognise. Matching one lets the message
/// name the class via OUR static string — never the provider's bytes.
const VETTED_ERROR_TYPES: &[&str] = &[
    "invalid_request_error",
    "authentication_error",
    "permission_error",
    "not_found_error",
    "request_too_large",
    "rate_limit_error",
    "api_error",
    "overloaded_error",
    "billing_error",
    "timeout_error",
];

/// Extract `error.type` from an Anthropic error envelope and map it onto a
/// vetted static label. Returns `None` for anything unrecognised — the
/// untrusted value itself is never surfaced.
fn vetted_error_type(body: &str) -> Option<&'static str> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let ty = v.get("error")?.get("type")?.as_str()?;
    VETTED_ERROR_TYPES.iter().find(|t| **t == ty).copied()
}

/// Classification-only peek at the untrusted error body: true when it
/// contains the given fixed pattern. The body text itself is never emitted.
fn body_mentions(body: &str, pattern: &str) -> bool {
    body.contains(pattern)
}

/// Map an untrusted provider error-type string onto a vetted static label.
///
/// Used by streaming/sync callers that already parsed the envelope: the
/// returned `&'static str` is OUR constant, safe to log/display; the input
/// itself must never be reproduced.
pub fn sanitize_error_type(ty: &str) -> Option<&'static str> {
    VETTED_ERROR_TYPES.iter().find(|t| **t == ty).copied()
}

/// Like [`humanize_api_error`] but surfaces a known rate-limit reset time in
/// the 429 message so the failure is honest rather than cryptic.
/// `reset_hint` is a human-readable duration string, e.g. `"47s"`.
pub fn humanize_api_error_with_reset(status: u16, body: &str, reset_hint: Option<&str>) -> String {
    // Vetted static class label, e.g. " [api_error]" — safe because it is
    // one of OUR constants, selected (not copied) via the untrusted body.
    let kind = vetted_error_type(body)
        .map(|t| format!(" [{t}]"))
        .unwrap_or_default();

    match status {
        529 => "Anthropic is overloaded right now. Retries exhausted — wait a minute and try again.".to_string(),
        429 => {
            if let Some(reset) = reset_hint {
                format!(
                    "Rate limit exhausted — retries used up while waiting for reset (next window in {}). \
                     Try again shortly, or switch models with /model.",
                    reset
                )
            } else {
                format!("Rate limited by Anthropic (HTTP 429{kind}). Wait for the limit to reset, or switch models with /model.")
            }
        }
        401 => "Authentication rejected. Run `synaps login` to re-authenticate.".to_string(),
        403 => format!("Access denied (HTTP 403{kind}). Your account may not have access to this model."),
        404 => format!("Model or endpoint not found (HTTP 404{kind}). Check the model name with /model."),
        413 => "Request too large. Run /compact to shrink the conversation, or reduce tool output sizes.".to_string(),
        400 if body_mentions(body, "Consumer Terms") =>
            "Anthropic requires accepting updated Consumer Terms: sign in at claude.ai with this account, accept the terms, then retry.".to_string(),
        400 if body_mentions(body, "extended-cache-ttl") =>
            "Bad request (HTTP 400) — your account may not support 1h cache TTL; set cache_ttl = 5m in config.".to_string(),
        400 if body_mentions(body, "prompt is too long") || body_mentions(body, "max_tokens") || body_mentions(body, "context") =>
            "Context window exceeded (HTTP 400). Run /compact to shrink the conversation.".to_string(),
        400 => format!("Bad request (HTTP 400{kind}). Provider error details withheld — they can echo request content."),
        500 | 502 | 503 => format!("Anthropic server error (HTTP {status}{kind}). Retries exhausted — usually transient, try again shortly."),
        _ => format!("API error (HTTP {status}{kind}). Provider error details withheld — they can echo request content."),
    }
}

/// Translate a reqwest transport error into a human-actionable message.
pub fn humanize_network_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "Request to api.anthropic.com timed out. Check your connection and try again.".to_string()
    } else if e.is_connect() {
        "Could not reach api.anthropic.com (connection failed). Check your network, DNS, or proxy settings.".to_string()
    } else if e.is_body() || e.is_decode() {
        "Connection lost mid-response. Partial reply kept — send again to continue.".to_string()
    } else {
        format!("Network error: {}", e)
    }
}

/// Render an error's full cause chain (`Display` of every level, `: `-joined).
///
/// `format!("{e}")` on a `reqwest::Error` prints only the top level — e.g.
/// `error sending request for url (…)` — and silently drops the source that
/// says *why* (`operation timed out`, `dns error`, `connection refused`, …).
/// That cost a real postmortem hours; always surface the chain.
pub fn error_chain_string(e: &(dyn std::error::Error + 'static)) -> String {
    let mut msg = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        let cause_str = cause.to_string();
        // Some wrappers already embed their source's Display; skip duplicates.
        if !msg.contains(&cause_str) {
            msg.push_str(": ");
            msg.push_str(&cause_str);
        }
        source = cause.source();
    }
    msg
}

/// Host-aware variant of [`humanize_network_error`] for non-Anthropic
/// providers (OpenAI Codex, Groq, local OpenAI-compat endpoints, …).
///
/// Unlike the Anthropic-specific helper this derives the host from the
/// failing request's URL and preserves the underlying cause chain, so
/// `chatgpt.com` failures are never misattributed and the *reason*
/// (timeout vs connect vs mid-stream drop) survives into the UI.
pub fn humanize_provider_network_error(e: &reqwest::Error) -> String {
    let host = e
        .url()
        .and_then(|u| u.host_str())
        .map(String::from)
        .unwrap_or_else(|| "the provider endpoint".to_string());
    let chain = error_chain_string(e);
    if e.is_timeout() {
        format!(
            "Request to {host} timed out — usually transient; check your connection and try again. [{chain}]"
        )
    } else if e.is_connect() {
        format!(
            "Could not reach {host} (connection failed). Check your network, DNS, or proxy settings. [{chain}]"
        )
    } else if e.is_body() || e.is_decode() {
        format!("Connection to {host} lost mid-response — usually transient; try again. [{chain}]")
    } else {
        format!("Network error talking to {host}: {chain}")
    }
}

pub type Result<T> = std::result::Result<T, RuntimeError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_humanize_529_overloaded() {
        let msg = humanize_api_error(
            529,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        );
        assert!(msg.contains("overloaded"), "got: {msg}");
        assert!(!msg.contains('{'), "raw JSON leaked: {msg}");
    }

    #[test]
    fn test_humanize_401_points_to_login() {
        let msg = humanize_api_error(401, r#"{"error":{"message":"invalid x-api-key"}}"#);
        assert!(msg.contains("synaps login"), "got: {msg}");
    }

    #[test]
    fn test_humanize_400_context_suggests_compact() {
        let msg = humanize_api_error(
            400,
            r#"{"error":{"message":"prompt is too long: 250000 tokens"}}"#,
        );
        assert!(msg.contains("/compact"), "got: {msg}");
    }

    #[test]
    fn test_humanize_400_cache_ttl_names_config_key() {
        let msg = humanize_api_error(
            400,
            r#"{"error":{"message":"The extended-cache-ttl-2025-04-11 beta is not enabled for this account"}}"#,
        );
        assert!(msg.contains("cache_ttl = 5m"), "got: {msg}");
    }

    #[test]
    fn test_humanize_unknown_status_names_status_but_withholds_detail() {
        // Provider `error.message` is untrusted (can echo the request) —
        // only the status and vetted guidance may appear.
        let msg = humanize_api_error(418, r#"{"error":{"message":"teapot"}}"#);
        assert!(msg.contains("418"), "got: {msg}");
        assert!(!msg.contains("teapot"), "untrusted detail leaked: {msg}");
    }

    #[test]
    fn test_humanize_non_json_body_truncated() {
        let long_body = "x".repeat(500);
        let msg = humanize_api_error(418, &long_body);
        assert!(msg.len() < 300, "not truncated: {} chars", msg.len());
    }

    /// BUG-1 regression: byte-slicing at offset 200 panics when a multibyte
    /// char (e.g. the 4-byte emoji 🔥) straddles that boundary.
    /// Craft a body where the emoji starts at byte 198 (i.e. bytes 198-201),
    /// so `&trimmed[..200]` would land in the middle of a char → panic.
    /// The fixed code must NOT panic and must truncate cleanly at a char boundary.
    #[test]
    fn test_humanize_multibyte_boundary_no_panic() {
        // 198 ASCII bytes + 🔥 (4 bytes, positions 198-201) + filler to exceed 200 total
        let body = format!("{}{}{}", "a".repeat(198), "🔥", "b".repeat(100));
        assert!(body.len() > 200, "precondition: body must exceed 200 bytes");
        // This must NOT panic — the bug causes a panic on unpatched code
        let msg = humanize_api_error(418, &body);
        // The result must be valid UTF-8 (it's a &str / String, always is if no panic)
        // and must contain the truncated prefix (not the raw emoji bytes mid-char)
        assert!(msg.contains("418"), "status should appear: {msg}");
        // The body portion forwarded must be ≤ 200 bytes (emoji trimmed at boundary)
        // After truncation at 198 bytes (the last safe boundary before 200), we get
        // 198 'a's — confirm no garbled bytes leaked through
        assert!(!msg.contains('\u{FFFD}'), "replacement char leaked: {msg}");
    }

    // ── error_chain_string: full cause chain, not just the top Display ──────

    #[derive(Debug)]
    struct Outer(Inner);
    #[derive(Debug)]
    struct Inner;

    impl std::fmt::Display for Outer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "error sending request for url (https://example.com/x)")
        }
    }
    impl std::fmt::Display for Inner {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "operation timed out")
        }
    }
    impl std::error::Error for Outer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }
    impl std::error::Error for Inner {}

    #[test]
    fn error_chain_string_joins_all_sources() {
        let msg = error_chain_string(&Outer(Inner));
        assert!(
            msg.contains("error sending request") && msg.contains("operation timed out"),
            "chain must include top-level AND source: {msg}"
        );
    }

    #[test]
    fn error_chain_string_single_level_has_no_separator_suffix() {
        let msg = error_chain_string(&Inner);
        assert_eq!(msg, "operation timed out");
    }

    // ── humanize_provider_network_error: host-aware transport messaging ─────

    /// Listener that accepts connections but never responds → client-side
    /// timeout while awaiting response headers (the incident failure mode).
    #[tokio::test]
    async fn humanize_provider_timeout_names_host_and_says_transient() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Keep the listener alive but never accept/respond.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(100))
            .build()
            .unwrap();
        let err = client
            .post(format!("http://{addr}/codex/responses"))
            .send()
            .await
            .expect_err("must time out");
        drop(listener);
        assert!(err.is_timeout(), "precondition: {err:?}");
        let msg = humanize_provider_network_error(&err);
        assert!(msg.contains("127.0.0.1"), "must name the host: {msg}");
        assert!(msg.contains("timed out"), "must say timed out: {msg}");
        assert!(msg.contains("transient"), "must flag transience: {msg}");
        assert!(
            !msg.contains("api.anthropic.com"),
            "must not claim the Anthropic host: {msg}"
        );
    }

    #[tokio::test]
    async fn humanize_provider_connect_error_names_host() {
        // Bind then drop → guaranteed-refused port.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let client = reqwest::Client::new();
        let err = client
            .post(format!("http://{addr}/codex/responses"))
            .send()
            .await
            .expect_err("must fail to connect");
        assert!(err.is_connect(), "precondition: {err:?}");
        let msg = humanize_provider_network_error(&err);
        assert!(msg.contains("127.0.0.1"), "must name the host: {msg}");
        assert!(
            msg.contains("Could not reach"),
            "must describe connection failure: {msg}"
        );
    }

    // ── Phase 1 holdout regression: hostile provider echoes the request ─────
    //
    // A hostile/misconfigured provider can put the ENTIRE request body
    // (user messages, system prompts, tool schemas, credentials) into
    // `error.message`. spec §5.1 / Task 1: no raw-content sentinel may
    // appear in logs or user-visible errors at any level. The humanizer
    // must therefore never echo any body-derived text.

    const HOSTILE_SENTINEL: &str = "HOLDOUT-SENTINEL-http500-77aa";

    /// Anthropic-shaped error envelope whose message echoes a request body.
    fn hostile_json_body() -> String {
        format!(
            r#"{{"type":"error","error":{{"type":"api_error","message":"ECHOED:{{\"messages\":[{{\"role\":\"user\",\"content\":\"{} tell me things\"}}],\"system\":[{{\"text\":\"You are a secret system prompt\"}}],\"tools\":[{{\"name\":\"bash\",\"input_schema\":{{\"properties\":{{}}}}}}]}}"}}}}"#,
            HOSTILE_SENTINEL
        )
    }

    /// Request-shaped markers that must never surface in a humanized error.
    fn assert_no_request_content(msg: &str, ctx: &str) {
        assert!(
            !msg.contains(HOSTILE_SENTINEL),
            "{ctx}: sentinel leaked: {msg}"
        );
        assert!(!msg.contains("ECHOED"), "{ctx}: echoed body leaked: {msg}");
        assert!(
            !msg.contains("input_schema") && !msg.contains("secret system prompt"),
            "{ctx}: request-shaped content leaked: {msg}"
        );
    }

    #[test]
    fn hostile_error_message_never_leaks_for_any_status() {
        let body = hostile_json_body();
        for status in [400u16, 403, 404, 429, 500, 418] {
            let msg = humanize_api_error(status, &body);
            assert_no_request_content(&msg, &format!("status {status}"));
            // Still actionable: names the status class.
            assert!(
                msg.contains(&status.to_string())
                    || msg.contains("Rate limited")
                    || msg.contains("Access denied")
                    || msg.contains("not found"),
                "status {status}: message not actionable: {msg}"
            );
        }
    }

    #[test]
    fn hostile_429_with_reset_hint_keeps_timing_but_not_body() {
        let msg = humanize_api_error_with_reset(429, &hostile_json_body(), Some("47s"));
        assert_no_request_content(&msg, "429+reset");
        assert!(msg.contains("47s"), "reset timing must survive: {msg}");
    }

    #[test]
    fn hostile_non_json_body_never_leaks() {
        // Non-JSON hostile content (e.g. text/plain echo of the request).
        let body = format!(
            "raw echo: {} {{\"messages\":[...],\"system\":[...]}} {}",
            HOSTILE_SENTINEL,
            "x".repeat(300)
        );
        for status in [400u16, 403, 404, 429, 500, 418] {
            let msg = humanize_api_error(status, &body);
            assert_no_request_content(&msg, &format!("non-json status {status}"));
        }
    }

    #[test]
    fn hostile_error_type_field_is_not_echoed_verbatim() {
        // `error.type` is attacker-controlled too — only vetted static
        // labels may be reproduced.
        let body = format!(
            r#"{{"error":{{"type":"{} injected-type","message":"m"}}}}"#,
            HOSTILE_SENTINEL
        );
        for status in [400u16, 403, 404, 429, 500, 418] {
            let msg = humanize_api_error(status, &body);
            assert_no_request_content(&msg, &format!("type-field status {status}"));
        }
    }

    #[test]
    fn test_runtime_error_display() {
        assert_eq!(
            format!("{}", RuntimeError::Auth("bad token".into())),
            "Auth error: bad token"
        );

        assert_eq!(
            format!("{}", RuntimeError::Config("missing".into())),
            "Config error: missing"
        );

        assert_eq!(
            format!("{}", RuntimeError::Tool("failed".into())),
            "Tool execution failed: failed"
        );

        assert_eq!(
            format!("{}", RuntimeError::Session("not found".into())),
            "Session error: not found"
        );

        assert_eq!(format!("{}", RuntimeError::Timeout), "Request timed out");

        assert_eq!(format!("{}", RuntimeError::Canceled), "Operation canceled");
    }

    #[test]
    fn test_runtime_error_to_string() {
        assert_eq!(
            RuntimeError::Auth("bad token".into()).to_string(),
            "Auth error: bad token"
        );

        assert_eq!(
            RuntimeError::Config("missing".into()).to_string(),
            "Config error: missing"
        );

        assert_eq!(
            RuntimeError::Tool("failed".into()).to_string(),
            "Tool execution failed: failed"
        );

        assert_eq!(
            RuntimeError::Session("not found".into()).to_string(),
            "Session error: not found"
        );

        assert_eq!(RuntimeError::Timeout.to_string(), "Request timed out");

        assert_eq!(RuntimeError::Canceled.to_string(), "Operation canceled");
    }
}
