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
    #[error("Network request timed out while {0}. Check your connection and try again.")]
    NetworkTimeout(&'static str),
    #[error("Request timed out")]
    Timeout,
    #[error("Operation canceled")]
    Canceled,
}

impl RuntimeError {
    /// Actionable transport text, without a potentially credential-bearing URL.
    pub fn user_message(&self) -> String {
        match self {
            Self::Api(error) => humanize_network_error(error),
            _ => self.to_string(),
        }
    }
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

/// Wire error classes we recognise on broker-proxied (OpenAI-compatible)
/// non-2xx responses. Superset of [`VETTED_ERROR_TYPES`]: it adds the
/// OpenAI/Kimi/xAI-style `error.type` / `error.code` values so a proxied
/// failure can be named via OUR static label — never the provider's bytes.
///
/// INVARIANT: no label may contain `':'`. Engine-side
/// `redact_provider_proxy_error` truncates the transport message at the
/// first `':'` after the `provider request failed: {status}` marker, so a
/// colon in a label would silently strip it.
pub const VETTED_PROXY_ERROR_TYPES: &[&str] = &[
    // Anthropic classes (kept in sync with `VETTED_ERROR_TYPES`).
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
    // OpenAI-compatible / Kimi / xAI classes.
    "access_terminated_error",
    "invalid_authentication_error",
    "insufficient_quota",
    "quota_exceeded",
    "server_error",
    "model_not_found",
    "invalid_api_key",
];

/// Classify an untrusted proxied error body onto a vetted static label.
///
/// Parses `body` as JSON and reads, in order, `error.type`, `error.code`,
/// then top-level `type` / `code` (each must be a JSON string). Returns OUR
/// constant only on an exact match; the untrusted value itself is never
/// surfaced. Non-JSON, oversized-truncated, or unrecognised bodies yield
/// `None` — classification never fails loudly.
pub fn vetted_proxy_error_label(body: &str) -> Option<&'static str> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let candidates = [
        v.get("error").and_then(|e| e.get("type")),
        v.get("error").and_then(|e| e.get("code")),
        v.get("type"),
        v.get("code"),
    ];
    let label = candidates
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .find_map(|ty| VETTED_PROXY_ERROR_TYPES.iter().find(|t| **t == ty).copied());
    label
}

/// User-actionable message for a broker-proxied non-2xx response when the
/// (status, vetted label) pair is one we understand; `None` otherwise so the
/// caller keeps the raw `provider request failed: …` prefix.
///
/// `provider` is a runtime-owned key (e.g. `kimi-code`, `xai-auth`) — never
/// provider bytes. `label` must be one of OUR constants (typically from
/// [`vetted_proxy_error_label`]); an unknown label is treated as absent.
pub fn humanize_proxy_status_error(
    provider: &str,
    status: u16,
    label: Option<&str>,
) -> Option<String> {
    let label = label.and_then(|l| VETTED_PROXY_ERROR_TYPES.iter().find(|t| **t == l).copied());
    match (status, label?) {
        (403, "access_terminated_error") => {
            let mut msg = format!(
                "{provider}: usage quota exhausted (HTTP 403). Your plan's usage window is used up — \
                 wait for the reset or upgrade, or switch models with /model."
            );
            if provider == "kimi-code" {
                msg.push_str(
                    " Kimi Code quotas are weekly; the Kimi membership page shows the reset time.",
                );
            }
            Some(msg)
        }
        (401, "invalid_authentication_error" | "authentication_error" | "invalid_api_key") => {
            Some(format!(
                "{provider}: authentication rejected (HTTP 401). Run `synaps login --provider {provider}`."
            ))
        }
        (429, "rate_limit_error") => Some(format!(
            "{provider}: rate limited (HTTP 429). Wait for the limit to reset, or switch models with /model."
        )),
        (429, "insufficient_quota" | "quota_exceeded") => Some(format!(
            "{provider}: usage quota exhausted (HTTP 429). Add credit or wait for the quota to reset, \
             or switch models with /model."
        )),
        _ => None,
    }
}

/// Classification-only peek at the untrusted error body: true when it
/// contains the given fixed pattern. The body text itself is never emitted.
fn body_mentions(body: &str, pattern: &str) -> bool {
    body.contains(pattern)
}

/// Why a provider refused a request that asked for the extended (1M)
/// context window. Selected from the untrusted error body by fixed
/// patterns; only [`LongContextRefusal::code`] (our constant) is ever
/// logged or shown, never the body.
///
/// The exact response Anthropic sends when an account that isn't entitled
/// to the 1M window goes past the native window is not known. Two classes
/// are recognised: the documented "prompt is too long" overflow, and a
/// narrow long-context entitlement class (a 400/403/429 whose body names
/// long or extended context together with a plan or entitlement phrase).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LongContextRefusal {
    /// HTTP 400 "prompt is too long": the input exceeded the window the
    /// provider actually applied.
    PromptTooLong,
    /// The provider said long-context use isn't available to this account
    /// or plan.
    Entitlement,
}

impl LongContextRefusal {
    /// Vetted class code for logs: our constant, never provider text.
    pub fn code(self) -> &'static str {
        match self {
            Self::PromptTooLong => "prompt_too_long",
            Self::Entitlement => "long_context_entitlement",
        }
    }
}

/// Phrases naming the extended context window in a refusal body.
const LONG_CONTEXT_SUBJECTS: &[&str] = &[
    "long context",
    "long-context",
    "context-1m",
    "1m context",
    "extended context",
];

/// Phrases naming an entitlement or plan limit in a refusal body.
const LONG_CONTEXT_ENTITLEMENT: &[&str] = &[
    "entitle",
    " plan",
    "not available",
    "extra usage",
    "not enabled",
    "not eligible",
];

/// Classify a failed request's status and untrusted body as a long-context
/// refusal, or `None` for any other failure. Classification only: the body
/// is matched against fixed patterns and never copied out.
///
/// Callers must still check that the request carried the 1M beta and that
/// its input was over the native window; a plain overflow without the beta
/// is not an entitlement signal.
pub fn classify_long_context_refusal(status: u16, body: &str) -> Option<LongContextRefusal> {
    let body = body.to_ascii_lowercase();
    if status == 400 && body_mentions(&body, "prompt is too long") {
        return Some(LongContextRefusal::PromptTooLong);
    }
    if !matches!(status, 400 | 403 | 429) {
        return None;
    }
    let subject = LONG_CONTEXT_SUBJECTS
        .iter()
        .any(|pattern| body_mentions(&body, pattern));
    let entitlement = LONG_CONTEXT_ENTITLEMENT
        .iter()
        .any(|pattern| body_mentions(&body, pattern));
    (subject && entitlement).then_some(LongContextRefusal::Entitlement)
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
        // Anthropic's whole-request image rule: past 20 images in one request,
        // every image must be at most 2000 px per side. The runtime trims the
        // outgoing copy (runtime::image_limits); this names it if it ever slips.
        400 if body_mentions(body, "many-image") =>
            "Too many large images (HTTP 400): once a request holds more than 20 images, Anthropic accepts only images up to 2000 px per side. Run /compact to drop old images from the conversation.".to_string(),
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
        "Network request failed. Check your connection and try again.".to_string()
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

/// Most levels of a cause chain the transport helpers below inspect.
const TRANSPORT_CHAIN_MAX_DEPTH: usize = 16;

/// `std::io::ErrorKind`s a transport failure is classed by, checked first:
/// the OS's own verdict beats any wrapper's text.
const TRANSPORT_IO_CLASSES: &[(std::io::ErrorKind, &str)] = &[
    (std::io::ErrorKind::ConnectionReset, "connection_reset"),
    (std::io::ErrorKind::BrokenPipe, "broken_pipe"),
    (std::io::ErrorKind::ConnectionAborted, "connection_aborted"),
    (std::io::ErrorKind::ConnectionRefused, "connection_refused"),
    (std::io::ErrorKind::NotConnected, "not_connected"),
    (std::io::ErrorKind::TimedOut, "timed_out"),
    (std::io::ErrorKind::UnexpectedEof, "unexpected_eof"),
];

/// Lower-cased `Display` fragments a transport failure is classed by when no
/// level carries a known `io::ErrorKind`. The hyper 1.x texts are those of
/// `hyper::Error`'s `Display` (`src/error.rs`): `IncompleteMessage` is the
/// stale pooled keep-alive connection closed under a request.
const TRANSPORT_TEXT_CLASSES: &[(&str, &str)] = &[
    ("connection closed before message completed", "incomplete_message"),
    ("connection reset", "connection_reset"),
    ("broken pipe", "broken_pipe"),
    ("error writing a body to connection", "body_write"),
    ("error reading a body from connection", "body_read"),
    ("operation was canceled", "canceled"),
    ("connection refused", "connection_refused"),
    ("dns error", "dns"),
    ("failed to lookup address", "dns"),
    ("timed out", "timed_out"),
];

/// The levels of `e`'s cause chain, `e` first, at most
/// [`TRANSPORT_CHAIN_MAX_DEPTH`].
fn chain_levels<'a>(
    e: &'a (dyn std::error::Error + 'static),
) -> Vec<&'a (dyn std::error::Error + 'static)> {
    let mut levels = Vec::new();
    let mut level = Some(e);
    while let Some(current) = level {
        if levels.len() == TRANSPORT_CHAIN_MAX_DEPTH {
            break;
        }
        levels.push(current);
        level = current.source();
    }
    levels
}

/// A vetted static class for a transport failure (`connection_reset`,
/// `incomplete_message`, `broken_pipe`, …, else `other`), for log fields.
///
/// A known `std::io::ErrorKind` anywhere in the chain wins; otherwise the
/// deepest level whose `Display` holds a known fragment names it. Only our
/// own labels are returned, never error text.
pub fn transport_failure_class(e: &(dyn std::error::Error + 'static)) -> &'static str {
    let levels = chain_levels(e);
    for level in &levels {
        if let Some(io) = level.downcast_ref::<std::io::Error>() {
            let kind = io.kind();
            if let Some((_, class)) = TRANSPORT_IO_CLASSES.iter().find(|(k, _)| *k == kind) {
                return class;
            }
        }
    }
    for level in levels.iter().rev() {
        let text = level.to_string().to_ascii_lowercase();
        if let Some((_, class)) = TRANSPORT_TEXT_CLASSES
            .iter()
            .find(|(fragment, _)| text.contains(fragment))
        {
            return class;
        }
    }
    "other"
}

/// The causes BELOW `e` (its `source()` chain, rendered as
/// [`error_chain_string`] does), with every `scheme://…` token replaced by
/// `[url]`; `None` when `e` has no source.
///
/// The top level is skipped: for a `reqwest::Error` it is only the kind
/// (`error sending request`) plus ` for url (…)` (reqwest `src/error.rs`
/// `Display`), and the classification flags already carry the kind. The
/// result is otherwise unredacted and uncapped: callers that log it pass it
/// through their secret scrub first, then cap it.
pub fn transport_cause_chain(e: &(dyn std::error::Error + 'static)) -> Option<String> {
    let levels = chain_levels(e);
    let source = levels.get(1)?;
    Some(strip_urls(&error_chain_string(*source)))
}

/// Replace every `scheme://…` token (up to whitespace, a quote, `<`, `>` or a
/// backtick) with `[url]`. A `)` ending the token is kept so
/// `for url (https://…)` stays balanced; the rest of the token, query
/// string included, never survives.
fn strip_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("://") {
        let scheme_start = rest[..pos]
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            .last()
            .map_or(pos, |(i, _)| i);
        out.push_str(&rest[..scheme_start]);
        out.push_str("[url]");
        let after = &rest[pos + 3..];
        let mut end = after
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`'))
            .unwrap_or(after.len());
        if after[..end].ends_with(')') {
            end -= 1;
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
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

    const KIMI_403_BODY: &str = r#"{"error":{"type":"access_terminated_error","message":"You've reached your weekly (7-day) usage limit. Your quota will reset when the current 7-day window ends."}}"#;

    #[test]
    fn proxy_labels_never_contain_colon_and_are_superset_of_anthropic() {
        for label in VETTED_PROXY_ERROR_TYPES {
            assert!(
                !label.contains(':'),
                "label {label:?} would be truncated by the engine"
            );
            assert!(
                !label.contains('['),
                "label {label:?} breaks the [label] suffix"
            );
        }
        for label in VETTED_ERROR_TYPES {
            assert!(
                VETTED_PROXY_ERROR_TYPES.contains(label),
                "proxy list must be a superset: missing {label}"
            );
        }
    }

    #[test]
    fn proxy_label_reads_error_type() {
        assert_eq!(
            vetted_proxy_error_label(KIMI_403_BODY),
            Some("access_terminated_error")
        );
    }

    #[test]
    fn proxy_label_falls_back_to_error_code_then_top_level() {
        assert_eq!(
            vetted_proxy_error_label(r#"{"error":{"code":"invalid_api_key","message":"x"}}"#),
            Some("invalid_api_key")
        );
        // error.type present but unvetted → fall through to error.code.
        assert_eq!(
            vetted_proxy_error_label(r#"{"error":{"type":"weird","code":"insufficient_quota"}}"#),
            Some("insufficient_quota")
        );
        assert_eq!(
            vetted_proxy_error_label(r#"{"type":"server_error"}"#),
            Some("server_error")
        );
        assert_eq!(
            vetted_proxy_error_label(r#"{"code":"model_not_found"}"#),
            Some("model_not_found")
        );
    }

    #[test]
    fn proxy_label_rejects_non_string_non_json_and_hostile_values() {
        assert_eq!(vetted_proxy_error_label(r#"{"error":{"type":42}}"#), None);
        assert_eq!(
            vetted_proxy_error_label(r#"{"error":{"code":["a"]}}"#),
            None
        );
        assert_eq!(
            vetted_proxy_error_label(r#"{"error":{"type":"<script>ECHO:secret"}}"#),
            None
        );
        // Prefix/suffix variants must not match — exact only.
        assert_eq!(
            vetted_proxy_error_label(r#"{"error":{"type":"access_terminated_error2"}}"#),
            None
        );
        assert_eq!(
            vetted_proxy_error_label(r#"{"error":{"type":"Access_Terminated_Error"}}"#),
            None
        );
        assert_eq!(vetted_proxy_error_label("<html>403 Forbidden</html>"), None);
        assert_eq!(vetted_proxy_error_label(""), None);
        // Truncated JSON (oversized body cut at the cap) must not parse.
        assert_eq!(vetted_proxy_error_label(&KIMI_403_BODY[..40]), None);
    }

    #[test]
    fn humanize_proxy_403_access_terminated_is_quota_not_auth() {
        let msg = humanize_proxy_status_error("kimi-code", 403, Some("access_terminated_error"))
            .expect("known combo");
        assert!(
            msg.starts_with("kimi-code: usage quota exhausted (HTTP 403)"),
            "got: {msg}"
        );
        assert!(msg.contains("/model"), "got: {msg}");
        assert!(msg.contains("weekly"), "kimi-specific hint missing: {msg}");
        assert!(msg.contains("membership page"), "got: {msg}");
        assert!(
            !msg.contains("login"),
            "quota must not be misreported as auth: {msg}"
        );

        let generic = humanize_proxy_status_error("xai-auth", 403, Some("access_terminated_error"))
            .expect("known combo");
        assert!(
            generic.starts_with("xai-auth: usage quota exhausted (HTTP 403)"),
            "got: {generic}"
        );
        assert!(
            !generic.contains("weekly"),
            "kimi hint leaked to other provider: {generic}"
        );
    }

    #[test]
    fn humanize_proxy_401_points_to_provider_login() {
        for label in [
            "invalid_authentication_error",
            "authentication_error",
            "invalid_api_key",
        ] {
            let msg = humanize_proxy_status_error("xai-auth", 401, Some(label)).expect(label);
            assert!(msg.contains("HTTP 401"), "got: {msg}");
            assert!(
                msg.contains("synaps login --provider xai-auth"),
                "got: {msg}"
            );
        }
    }

    #[test]
    fn humanize_proxy_429_distinguishes_rate_limit_from_quota() {
        let rl = humanize_proxy_status_error("groq", 429, Some("rate_limit_error")).unwrap();
        let q1 = humanize_proxy_status_error("groq", 429, Some("insufficient_quota")).unwrap();
        let q2 = humanize_proxy_status_error("groq", 429, Some("quota_exceeded")).unwrap();
        assert!(rl.contains("rate limited"), "got: {rl}");
        assert!(q1.contains("quota exhausted"), "got: {q1}");
        assert_eq!(q1, q2);
        assert_ne!(rl, q1);
    }

    #[test]
    fn humanize_proxy_unknown_combos_return_none() {
        assert_eq!(humanize_proxy_status_error("kimi-code", 403, None), None);
        assert_eq!(
            humanize_proxy_status_error("kimi-code", 403, Some("rate_limit_error")),
            None
        );
        assert_eq!(
            humanize_proxy_status_error("kimi-code", 500, Some("server_error")),
            None
        );
        assert_eq!(
            humanize_proxy_status_error("kimi-code", 401, Some("access_terminated_error")),
            None
        );
        // Unvetted label strings are treated as absent, never echoed.
        assert_eq!(
            humanize_proxy_status_error("kimi-code", 403, Some("<script>ECHO")),
            None
        );
    }

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
    fn long_context_refusal_classifies_prompt_too_long_only_on_400() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 230145 tokens > 200000 maximum"}}"#;
        assert_eq!(
            classify_long_context_refusal(400, body),
            Some(LongContextRefusal::PromptTooLong)
        );
        assert_eq!(
            classify_long_context_refusal(400, &body.to_uppercase()),
            Some(LongContextRefusal::PromptTooLong)
        );
        for status in [403, 413, 429, 500, 529] {
            assert_eq!(classify_long_context_refusal(status, body), None, "{status}");
        }
    }

    #[test]
    fn long_context_refusal_classifies_the_narrow_entitlement_class() {
        let bodies = [
            r#"{"error":{"type":"invalid_request_error","message":"The long context beta is not available for this subscription."}}"#,
            r#"{"error":{"message":"Your organization is not entitled to use context-1m-2025-08-07"}}"#,
            r#"{"error":{"message":"Extended context requires extra usage on your plan"}}"#,
            r#"{"error":{"message":"1M context is not enabled for this account"}}"#,
        ];
        for body in bodies {
            for status in [400, 403, 429] {
                assert_eq!(
                    classify_long_context_refusal(status, body),
                    Some(LongContextRefusal::Entitlement),
                    "{status} {body}"
                );
            }
            for status in [401, 404, 413, 500, 529] {
                assert_eq!(classify_long_context_refusal(status, body), None, "{status}");
            }
        }
    }

    #[test]
    fn long_context_refusal_ignores_unrelated_failures() {
        let bodies = [
            // Other 400s, including ones the humanizer files under "context".
            r#"{"error":{"type":"invalid_request_error","message":"max_tokens: 64000 > 32000"}}"#,
            r#"{"error":{"message":"messages: context management field invalid"}}"#,
            r#"{"error":{"message":"At least one of the image dimensions exceed max allowed size for many-image requests"}}"#,
            r#"{"error":{"message":"extended-cache-ttl is not available"}}"#,
            // A subject without an entitlement phrase, and the reverse.
            r#"{"error":{"message":"long context request malformed"}}"#,
            r#"{"error":{"message":"This feature is not available on your plan"}}"#,
            // An ordinary rate limit.
            r#"{"error":{"type":"rate_limit_error","message":"Number of request tokens has exceeded your per-minute rate limit"}}"#,
            "",
        ];
        for body in bodies {
            for status in [400, 403, 429] {
                assert_eq!(classify_long_context_refusal(status, body), None, "{status} {body}");
            }
        }
    }

    #[test]
    fn long_context_refusal_codes_are_fixed_constants() {
        assert_eq!(LongContextRefusal::PromptTooLong.code(), "prompt_too_long");
        assert_eq!(LongContextRefusal::Entitlement.code(), "long_context_entitlement");
    }

    #[test]
    fn test_humanize_400_many_image_names_the_limit() {
        let msg = humanize_api_error(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.433.content.1.image.source.base64.data: At least one of the image dimensions exceed max allowed size for many-image requests: 2000 pixels"}}"#,
        );
        assert!(msg.contains("20 images"), "got: {msg}");
        assert!(msg.contains("2000 px"), "got: {msg}");
        assert!(msg.contains("/compact"), "got: {msg}");
        assert!(
            !msg.contains("messages.433"),
            "untrusted detail leaked: {msg}"
        );
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

    // ── transport_failure_class / transport_cause_chain ─────────────────────

    /// One level of a constructed cause chain: fixed text, optional source.
    #[derive(Debug)]
    struct Level(String, Option<Box<dyn std::error::Error + Send + Sync>>);

    impl std::fmt::Display for Level {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl std::error::Error for Level {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1
                .as_deref()
                .map(|e| e as &(dyn std::error::Error + 'static))
        }
    }

    /// `texts` top first, over an optional io leaf.
    fn chain(
        texts: &[&str],
        leaf: Option<std::io::Error>,
    ) -> Box<dyn std::error::Error + Send + Sync> {
        let mut source: Option<Box<dyn std::error::Error + Send + Sync>> =
            leaf.map(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>);
        for text in texts.iter().rev() {
            source = Some(Box::new(Level(text.to_string(), source.take())));
        }
        source.expect("at least one level")
    }

    const SEND_TOP: &str = "error sending request for url (https://api.example.com/v1/messages)";

    #[test]
    fn transport_failure_class_prefers_an_io_kind_anywhere_in_the_chain() {
        let reset = chain(
            &[SEND_TOP, "client error (SendRequest)", "connection error"],
            Some(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "Connection reset by peer (os error 54)",
            )),
        );
        assert_eq!(transport_failure_class(&*reset), "connection_reset");
        // The io kind beats a wrapper's text ("error writing a body…").
        let pipe = chain(
            &[SEND_TOP, "client error (SendRequest)", "error writing a body to connection"],
            Some(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "Broken pipe (os error 32)",
            )),
        );
        assert_eq!(transport_failure_class(&*pipe), "broken_pipe");
    }

    #[test]
    fn transport_failure_class_reads_hyper_text_and_falls_back_to_other() {
        let stale = chain(
            &[
                SEND_TOP,
                "client error (SendRequest)",
                "connection closed before message completed",
            ],
            None,
        );
        assert_eq!(transport_failure_class(&*stale), "incomplete_message");
        let canceled = chain(
            &[SEND_TOP, "client error (Canceled)", "operation was canceled"],
            None,
        );
        assert_eq!(transport_failure_class(&*canceled), "canceled");
        let unknown = chain(
            &[SEND_TOP, "something new"],
            Some(std::io::Error::other("unclassified")),
        );
        assert_eq!(transport_failure_class(&*unknown), "other");
    }

    #[test]
    fn transport_cause_chain_skips_the_top_level_and_cuts_every_url() {
        let e = chain(
            &[
                "error sending request for url (https://api.example.com/v1/messages?beta=true)",
                "client error (SendRequest)",
                "proxy sent https://login.example.com/cb?code=abc123 then (http://h.example/p?key=v)",
            ],
            None,
        );
        assert_eq!(
            transport_cause_chain(&*e).as_deref(),
            Some("client error (SendRequest): proxy sent [url] then ([url])")
        );
        assert_eq!(transport_cause_chain(&Inner), None, "no source, no chain");
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
