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
        if trimmed.len() > 200 { format!("{}…", &trimmed[..200]) } else { trimmed.to_string() }
    });

    match status {
        529 => "Anthropic is overloaded right now. Retries exhausted — wait a minute and try again.".to_string(),
        429 => format!("Rate limited by Anthropic ({}). Wait for the limit to reset, or switch models with /model.", detail),
        401 => "Authentication rejected. Run `synaps login` to re-authenticate, or check ANTHROPIC_API_KEY.".to_string(),
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
