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
/// Parses the error body (`{"error": {"type": ..., "message": ...}}`) and maps
/// well-known statuses to guidance. Falls back to a trimmed version of the raw
/// body for unknown cases.
pub fn humanize_api_error(status: u16, body: &str) -> String {
    humanize_api_error_with_reset(status, body, None)
}

/// Like [`humanize_api_error`] but surfaces a known rate-limit reset time in
/// the 429 message so the failure is honest rather than cryptic.
/// `reset_hint` is a human-readable duration string, e.g. `"47s"`.
pub fn humanize_api_error_with_reset(status: u16, body: &str, reset_hint: Option<&str>) -> String {
    // Pull the server's message out of the JSON envelope if present.
    let api_msg = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(String::from)
        });
    let detail = api_msg.unwrap_or_else(|| {
        let trimmed = body.trim();
        if trimmed.len() > 200 { format!("{}…", crate::truncate_str(trimmed, 200)) } else { trimmed.to_string() }
    });

    match status {
        529 => "Anthropic is overloaded right now. Retries exhausted — wait a minute and try again.".to_string(),
        429 => {
            if let Some(reset) = reset_hint {
                format!(
                    "Rate limit exhausted — retries used up while waiting for reset (next window in {}). \
                     Try again shortly, or switch models with /model. ({})",
                    reset, detail
                )
            } else {
                format!("Rate limited by Anthropic ({}). Wait for the limit to reset, or switch models with /model.", detail)
            }
        }
        401 => "Authentication rejected. Run `synaps login` to re-authenticate.".to_string(),
        403 => format!("Access denied ({}). Your account may not have access to this model.", detail),
        404 => format!("Model or endpoint not found ({}). Check the model name with /model.", detail),
        413 => "Request too large. Run /compact to shrink the conversation, or reduce tool output sizes.".to_string(),
        400 if detail.contains("extended-cache-ttl") =>
            format!("Bad request ({}) — your account may not support 1h cache TTL; set cache_ttl = 5m in config.", detail),
        400 if detail.contains("prompt is too long") || detail.contains("max_tokens") || detail.contains("context") =>
            format!("Context window exceeded ({}). Run /compact to shrink the conversation.", detail),
        500 | 502 | 503 => format!("Anthropic server error ({} {}). Retries exhausted — usually transient, try again shortly.", status, detail),
        _ => format!("API error {} — {}", status, detail),
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

pub type Result<T> = std::result::Result<T, RuntimeError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_humanize_529_overloaded() {
        let msg = humanize_api_error(529, r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#);
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
        let msg = humanize_api_error(400, r#"{"error":{"message":"prompt is too long: 250000 tokens"}}"#);
        assert!(msg.contains("/compact"), "got: {msg}");
    }

    #[test]
    fn test_humanize_400_cache_ttl_names_config_key() {
        let msg = humanize_api_error(400, r#"{"error":{"message":"The extended-cache-ttl-2025-04-11 beta is not enabled for this account"}}"#);
        assert!(msg.contains("cache_ttl = 5m"), "got: {msg}");
    }

    #[test]
    fn test_humanize_unknown_status_includes_detail() {
        let msg = humanize_api_error(418, r#"{"error":{"message":"teapot"}}"#);
        assert!(msg.contains("418") && msg.contains("teapot"), "got: {msg}");
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

        assert_eq!(
            format!("{}", RuntimeError::Timeout),
            "Request timed out"
        );

        assert_eq!(
            format!("{}", RuntimeError::Canceled),
            "Operation canceled"
        );
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

        assert_eq!(
            RuntimeError::Timeout.to_string(),
            "Request timed out"
        );

        assert_eq!(
            RuntimeError::Canceled.to_string(),
            "Operation canceled"
        );
    }
}
