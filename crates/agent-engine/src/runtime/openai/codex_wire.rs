//! Codex direct-transport wire helpers: legible HTTP 4xx errors and the
//! opt-in OpenAI-hosted `web_search` tool (gating + visibility).
//!
//! Kept out of `stream.rs` so the transport only carries call sites.

use crate::runtime::openai::catalog::ExecutionRole;
use crate::runtime::types::StreamEvent;
use serde_json::Value;
use tokio::sync::mpsc;

// ─── HTTP 4xx error bodies ───────────────────────────────────────────────────

/// Bytes of a Codex 4xx body read for classification. Large enough for the
/// JSON envelope to parse (a truncated body yields no detail, never an error).
pub(crate) const CODEX_ERROR_BODY_CAP: usize = 8 * 1024;

/// Classified Codex non-2xx body. Spec §5.1 holds: provider prose is never
/// kept. `kind` / `code` / `param` are identifier-shaped tokens; the
/// explanation shown to the user is Synaps-authored static text selected by
/// them (and by one classification-only boolean).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct CodexHttpErrorDetail {
    pub kind: Option<String>,
    pub code: Option<String>,
    pub param: Option<String>,
    /// The backend refused the model for this ChatGPT account (entitlement).
    /// Derived from the message as a boolean only; the text is dropped.
    pub account_entitlement: bool,
}

const EXPLAIN_ENTITLEMENT: &str = "this model isn't available on the selected ChatGPT account; \
     switch account (`synaps auth use --provider openai-codex`) or model (/model).";
const EXPLAIN_MODEL_NOT_FOUND: &str =
    "the backend doesn't know this model for this account; pick another with /model.";
const EXPLAIN_REASONING: &str =
    "this model rejected the requested reasoning setting; pick another level with /effort.";
const EXPLAIN_TOOLS_WEB_SEARCH: &str = "the model rejected a request tool; hosted web_search \
     (SYNAPS_CODEX_WEB_SEARCH) was attached and is the likely cause.";
const EXPLAIN_TOOLS: &str =
    "the backend rejected a request tool definition for this model; try another model with /model.";
const EXPLAIN_PARAMETER: &str =
    "the backend rejected a request parameter for this model; try another model with /model.";

impl CodexHttpErrorDetail {
    /// Parse a (possibly truncated) body. Accepts the OpenAI envelope
    /// (`{"error":{"type","code","param","message"}}`) and the ChatGPT
    /// backend's FastAPI form (`{"detail": "..."}`).
    pub(crate) fn from_body(body: &[u8]) -> Self {
        let Ok(v) = serde_json::from_slice::<Value>(body) else {
            return Self::default();
        };
        let err = v.get("error").filter(|e| e.is_object());
        let field = |name: &str| {
            err.and_then(|e| e.get(name))
                .or_else(|| v.get(name))
                .and_then(Value::as_str)
        };
        // Provider prose: classification-only boolean, never retained.
        let account_entitlement = field("message")
            .or_else(|| v.get("detail").and_then(Value::as_str))
            .is_some_and(|m| {
                m.to_ascii_lowercase()
                    .contains("not supported when using codex with a chatgpt account")
            });
        Self {
            kind: field("type").and_then(sanitize_identifier),
            code: field("code").and_then(sanitize_identifier),
            param: field("param").and_then(sanitize_param),
            account_entitlement,
        }
    }

    /// The backend rejected a request tool: a `tools` param, or a code that
    /// names a tool rejection.
    pub(crate) fn rejects_hosted_tool(&self) -> bool {
        let param = self.param.as_deref().unwrap_or("");
        param == "tools"
            || param.starts_with("tools[")
            || param.starts_with("tools.")
            || matches!(
                self.code.as_deref(),
                Some(
                    "unsupported_tool"
                        | "tool_not_supported"
                        | "unsupported_tool_type"
                        | "invalid_tool_type"
                )
            )
    }

    /// Synaps-authored explanation for a known (type, code, param) shape.
    /// web_search is blamed for a tool rejection only when it was attached.
    pub(crate) fn explanation(&self, web_search_attached: bool) -> Option<&'static str> {
        let code = self.code.as_deref();
        let param = self.param.as_deref().unwrap_or("");
        if self.account_entitlement {
            return Some(EXPLAIN_ENTITLEMENT);
        }
        if code == Some("model_not_found") {
            return Some(EXPLAIN_MODEL_NOT_FOUND);
        }
        if param == "reasoning" || param.starts_with("reasoning.") {
            return Some(EXPLAIN_REASONING);
        }
        if self.rejects_hosted_tool() {
            return Some(if web_search_attached {
                EXPLAIN_TOOLS_WEB_SEARCH
            } else {
                EXPLAIN_TOOLS
            });
        }
        match code {
            Some("unsupported_parameter" | "unknown_parameter") => Some(EXPLAIN_PARAMETER),
            Some("unsupported_value" | "invalid_value") if !param.is_empty() => {
                Some(EXPLAIN_PARAMETER)
            }
            _ => None,
        }
    }

    /// Trace terminal code: `http_<status>` plus sanitized identifiers only
    /// (`http_400:invalid_request_error:unsupported_value`).
    pub(crate) fn trace_code(&self, status: u16) -> String {
        let mut code = format!("http_{status}");
        for part in [&self.kind, &self.code].into_iter().flatten() {
            code.push(':');
            code.push_str(part);
        }
        code
    }

    /// User-visible error: keeps the `codex request failed: <status>` prefix
    /// (net.rs / session_driver classify on it), then identifiers, then our
    /// static explanation. Safe to log and to hand to a parent model.
    pub(crate) fn user_message(
        &self,
        label: &str,
        status: reqwest::StatusCode,
        web_search_attached: bool,
    ) -> String {
        let mut out = format!("{label} request failed: {status}");
        let ids: Vec<&str> = [&self.kind, &self.code]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect();
        if !ids.is_empty() || self.param.is_some() {
            out.push_str(" [");
            out.push_str(&ids.join("/"));
            if let Some(param) = &self.param {
                if !ids.is_empty() {
                    out.push_str(", ");
                }
                out.push_str("param ");
                out.push_str(param);
            }
            out.push(']');
        }
        if let Some(explanation) = self.explanation(web_search_attached) {
            out.push_str(" — ");
            out.push_str(explanation);
        }
        out
    }
}

/// Too many digits look like an id, hash or key, not an enum value.
fn digit_heavy(s: &str) -> bool {
    s.bytes().filter(u8::is_ascii_digit).count() > 4
}

/// `error.type` / `error.code`: lowercase snake-case enum values only.
fn sanitize_identifier(raw: &str) -> Option<String> {
    let s = super::stream::sanitize_error_identifier(Some(raw));
    let ok = s == raw && s.len() <= 48 && !digit_heavy(s);
    ok.then(|| s.to_string())
}

/// `param` is a request JSON path (`reasoning.effort`, `tools[3].type`).
fn sanitize_param(raw: &str) -> Option<String> {
    let ok = !raw.is_empty()
        && raw.len() <= 48
        && !digit_heavy(raw)
        && raw.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'[' | b']')
        });
    ok.then(|| raw.to_string())
}

/// Unicode format characters (bidi overrides/isolates, zero-width, BOM,
/// soft hyphen) that can make a displayed line differ from its bytes.
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// One display line: no control or format characters, single spaces.
fn one_line(raw: &str) -> String {
    let flat: String = raw
        .chars()
        .filter(|c| !is_format_char(*c))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    flat.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cap_chars(s: &str, cap: usize) -> String {
    if s.chars().count() > cap {
        let mut out: String = s.chars().take(cap).collect();
        out.push('…');
        out
    } else {
        s.to_string()
    }
}

// ─── Hosted web_search: gating ───────────────────────────────────────────────

/// Workers (subagents) get the hosted tool only when this is also truthy.
pub(crate) const CODEX_WEB_SEARCH_WORKERS_ENV: &str = "SYNAPS_CODEX_WEB_SEARCH_WORKERS";

/// Text-only model; never sent hosted tools.
const SPARK: &str = "gpt-5.3-codex-spark";

/// Models whose backend rejected the hosted tool this process (learned from
/// a `param: tools` 400). Synaps sends full Responses requests, so upstream's
/// `use_responses_lite` (a request-format switch) is not a gate here.
static HOSTED_SEARCH_REJECTED: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
    std::sync::Mutex::new(None);

/// Remember that `model_id` rejected hosted web_search. Returns whether this
/// is new (so the caller shows its notice once).
pub(crate) fn record_web_search_rejected(model_id: &str) -> bool {
    HOSTED_SEARCH_REJECTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(Default::default)
        .insert(model_id.to_string())
}

fn web_search_rejected(model_id: &str) -> bool {
    HOSTED_SEARCH_REJECTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .is_some_and(|m| m.contains(model_id))
}

/// Why hosted web_search isn't attached for a model, or `None` if it may be.
pub(crate) fn codex_web_search_unsupported_reason(model_id: &str) -> Option<&'static str> {
    if model_id == SPARK {
        return Some("the built-in table marks gpt-5.3-codex-spark text-only (no hosted tools)");
    }
    web_search_rejected(model_id)
        .then_some("the backend rejected hosted web_search for this model earlier in this process")
}

/// Typed terminal Codex 4xx: the identifiers-only message, plus whether the
/// backend rejected a request tool (so the caller can drop hosted
/// web_search and retry once).
#[derive(Debug)]
pub(crate) struct CodexRequestRejected {
    pub message: String,
    pub hosted_tool_rejected: bool,
}

impl std::fmt::Display for CodexRequestRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CodexRequestRejected {}

/// After a send failed: if hosted web_search was attached and the backend
/// rejected a request tool, remember that for this model, show a one-time
/// notice, clear `attached` and return `Ok(())` so the caller resends once
/// without it. A second rejection finds `attached == false` and is returned
/// as the error, so the retry cannot loop.
pub(crate) fn hosted_search_retry(
    error: Box<dyn std::error::Error + Send + Sync>,
    attached: &mut bool,
    model_id: &str,
    tx: &mpsc::UnboundedSender<StreamEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let rejected = error
        .downcast_ref::<CodexRequestRejected>()
        .is_some_and(|e| e.hosted_tool_rejected);
    if !*attached || !rejected {
        return Err(error);
    }
    *attached = false;
    tracing::warn!(
        model = model_id,
        "codex rejected hosted web_search; retrying once without it"
    );
    if record_web_search_rejected(model_id) {
        let _ = tx.send(StreamEvent::Session(crate::SessionEvent::Notice(format!(
            "🔎 Codex web_search was rejected by {model_id}; continuing without it."
        ))));
    }
    Ok(())
}

/// Drop the hosted web_search entry from a built Codex request body.
pub(crate) fn strip_web_search_tool(body: &mut Value) {
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        tools.retain(|t| t.get("type").and_then(Value::as_str) != Some("web_search"));
    }
}

#[cfg(test)]
thread_local! {
    static ENV_OVERRIDE: std::cell::RefCell<Option<Vec<(String, String)>>> =
        const { std::cell::RefCell::new(None) };
}

/// Test seam: replace the process env this thread's Codex requests see.
#[cfg(test)]
pub(crate) fn override_env_for_test(pairs: &[(&str, &str)]) -> EnvOverrideGuard {
    ENV_OVERRIDE.with(|o| {
        *o.borrow_mut() = Some(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    });
    EnvOverrideGuard
}

#[cfg(test)]
pub(crate) struct EnvOverrideGuard;

#[cfg(test)]
impl Drop for EnvOverrideGuard {
    fn drop(&mut self) {
        ENV_OVERRIDE.with(|o| *o.borrow_mut() = None);
    }
}

/// The engine process environment (overridable per thread in tests).
pub(crate) fn engine_env(key: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(v) = ENV_OVERRIDE.with(|o| {
        o.borrow()
            .as_ref()
            .map(|pairs| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()))
    }) {
        return v;
    }
    std::env::var(key).ok()
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WebSearchDecision {
    /// Not requested.
    Off,
    Attach(Value),
    /// Requested, but this role never gets it (internal, or worker without
    /// the worker opt-in).
    RoleExcluded,
    /// Requested, but this model can't use it.
    ModelUnsupported(&'static str),
}

fn truthy(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("on" | "true" | "1" | "yes")
    )
}

/// Decide hosted web_search for one request. `env` is the process
/// environment (injected for tests).
pub(crate) fn web_search_decision(
    model_id: &str,
    role: ExecutionRole,
    env: impl Fn(&str) -> Option<String>,
) -> WebSearchDecision {
    let Some(spec) =
        super::stream::codex_web_search_tool(env(super::stream::CODEX_WEB_SEARCH_ENV).as_deref())
    else {
        return WebSearchDecision::Off;
    };
    let role_ok = match role {
        ExecutionRole::Foreground => true,
        ExecutionRole::Worker => truthy(env(CODEX_WEB_SEARCH_WORKERS_ENV).as_deref()),
        ExecutionRole::Internal => false,
    };
    if !role_ok {
        return WebSearchDecision::RoleExcluded;
    }
    match codex_web_search_unsupported_reason(model_id) {
        Some(reason) => WebSearchDecision::ModelUnsupported(reason),
        None => WebSearchDecision::Attach(spec),
    }
}

/// The hosted tool spec for one Codex request, or `None`.
pub(crate) fn codex_web_search_for_request(
    model_id: &str,
    role: ExecutionRole,
    env: impl Fn(&str) -> Option<String>,
) -> Option<Value> {
    match web_search_decision(model_id, role, env) {
        WebSearchDecision::Attach(spec) => Some(spec),
        other => {
            tracing::debug!(model = model_id, role = role.as_str(), decision = ?other, "codex web_search not attached");
            None
        }
    }
}

/// The session-level line about hosted web_search for the foreground model
/// (qualified id), or `None` when it isn't requested or the model isn't
/// Codex. The session actor shows it when it changes, so once per session
/// per model.
pub(crate) fn session_web_search_notice(
    qualified_model: &str,
    env: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    // The router decides what is Codex; the prefix is the fallback for ids
    // it doesn't resolve.
    let model_id = match super::resolve_route(qualified_model) {
        Some(route) if route.wire == super::WireProtocol::CodexResponses => route.model,
        Some(_) => return None,
        None => qualified_model.strip_prefix("openai-codex/")?.to_string(),
    };
    let model_id = model_id.as_str();
    let workers = truthy(env(CODEX_WEB_SEARCH_WORKERS_ENV).as_deref());
    match web_search_decision(model_id, ExecutionRole::Foreground, &env) {
        WebSearchDecision::Attach(spec) => {
            let mode = if spec.get("external_web_access") == Some(&Value::Bool(false)) {
                "cached"
            } else {
                "live"
            };
            Some(format!(
                "🔎 Codex web_search is on ({mode}{}): {} was read from the engine process \
                 environment (the daemon's, when attached). Searches run on OpenAI's servers; \
                 hooks can't gate them.",
                if workers { ", workers too" } else { "" },
                super::stream::CODEX_WEB_SEARCH_ENV
            ))
        }
        WebSearchDecision::ModelUnsupported(reason) => Some(format!(
            "🔎 Codex web_search was requested ({}) but is not attached for {model_id}: {reason}.",
            super::stream::CODEX_WEB_SEARCH_ENV
        )),
        WebSearchDecision::Off | WebSearchDecision::RoleExcluded => None,
    }
}

// ─── Hosted web_search: visibility ───────────────────────────────────────────

/// Queries shown before collapsing the rest into "+k more".
const MAX_SHOWN_QUERIES: usize = 5;
/// Per-query / per-target display cap (chars).
const TARGET_CAP: usize = 200;
const CALL_MARK: &str = " web_search (";

/// Display text for a hosted `web_search_call` item's action. Every query is
/// shown (user-visible egress), capped at [`MAX_SHOWN_QUERIES`].
fn web_search_target(action: Option<&Value>) -> Option<String> {
    let action = action?;
    let s = |k: &str| {
        action
            .get(k)
            .and_then(Value::as_str)
            .map(|v| cap_chars(&one_line(v), TARGET_CAP))
            .filter(|v| !v.is_empty())
    };
    if let (Some(pattern), Some(url)) = (s("pattern"), s("url")) {
        return Some(format!("'{pattern}' in {url}"));
    }
    let mut queries: Vec<String> = action
        .get("queries")
        .and_then(Value::as_array)
        .map(|qs| {
            qs.iter()
                .filter_map(Value::as_str)
                .map(|v| cap_chars(&one_line(v), TARGET_CAP))
                .filter(|v| !v.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if let Some(q) = s("query") {
        if !queries.contains(&q) {
            queries.insert(0, q);
        }
    }
    if !queries.is_empty() {
        let more = queries.len().saturating_sub(MAX_SHOWN_QUERIES);
        let mut text = queries
            .into_iter()
            .take(MAX_SHOWN_QUERIES)
            .collect::<Vec<_>>()
            .join(" | ");
        if more > 0 {
            text.push_str(&format!(" (+{more} more)"));
        }
        return Some(text);
    }
    s("url").or_else(|| s("pattern"))
}

/// Surface a completed hosted `web_search_call` output item as a transcript
/// notice (with the query: the user must see egress) and a metadata-only log
/// record (never the query text). No-op for any other item type.
pub(crate) fn announce_web_search_call(
    provider_label: &str,
    item: &Value,
    tx: &mpsc::UnboundedSender<StreamEvent>,
) {
    if item.get("type").and_then(Value::as_str) != Some("web_search_call") {
        return;
    }
    let ident = |v: Option<&Value>, absent: &'static str| {
        v.and_then(Value::as_str)
            .map(|s| super::stream::sanitize_error_identifier(Some(s)).to_string())
            .unwrap_or_else(|| absent.to_string())
    };
    let status = ident(item.get("status"), "absent");
    let action = item.get("action");
    let action_kind = ident(action.and_then(|a| a.get("type")), "search");
    let str_len = |k: &str| {
        action
            .and_then(|a| a.get(k))
            .and_then(Value::as_str)
            .map_or(0, |v| v.chars().count())
    };
    let query_lens: Vec<usize> = action
        .and_then(|a| a.get("queries"))
        .and_then(Value::as_array)
        .map(|qs| {
            qs.iter()
                .filter_map(Value::as_str)
                .map(|q| q.chars().count())
                .collect()
        })
        .unwrap_or_default();
    tracing::info!(
        provider = provider_label,
        status = %status,
        action = %action_kind,
        query_len = str_len("query"),
        query_count = query_lens.len(),
        query_lens = ?query_lens,
        url_len = str_len("url"),
        pattern_len = str_len("pattern"),
        "provider-hosted web_search call (query text withheld from logs)"
    );
    let text = match web_search_target(action) {
        Some(t) => format!("🔎 {provider_label}{CALL_MARK}{action_kind}, {status}): {t}"),
        None => format!("🔎 {provider_label}{CALL_MARK}{action_kind}, {status})"),
    };
    let _ = tx.send(StreamEvent::Session(crate::SessionEvent::Notice(text)));
}

/// Forward a worker's web_search notice to the parent stream, tagged with
/// the worker label, so worker egress is as visible as foreground egress.
pub(crate) fn forward_worker_web_search(
    worker_label: &str,
    event: &StreamEvent,
    parent: Option<&mpsc::UnboundedSender<StreamEvent>>,
) {
    let (Some(parent), StreamEvent::Session(crate::SessionEvent::Notice(text))) = (parent, event)
    else {
        return;
    };
    let Some(rest) = text.strip_prefix("🔎 ") else {
        return;
    };
    if !rest.contains(CALL_MARK) {
        return;
    }
    let label = cap_chars(&one_line(worker_label), 40);
    let _ = parent.send(StreamEvent::Session(crate::SessionEvent::Notice(format!(
        "🔎 [worker {label}] {rest}"
    ))));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn detail(body: &str) -> CodexHttpErrorDetail {
        CodexHttpErrorDetail::from_body(body.as_bytes())
    }

    fn bad_request() -> reqwest::StatusCode {
        reqwest::StatusCode::BAD_REQUEST
    }

    #[test]
    fn invalid_reasoning_effort_400_maps_to_static_text() {
        let d = detail(
            r#"{"error":{"message":"Unsupported value: 'reasoning.effort' does not support 'max' with this model.","type":"invalid_request_error","param":"reasoning.effort","code":"unsupported_value"}}"#,
        );
        assert_eq!(
            d.trace_code(400),
            "http_400:invalid_request_error:unsupported_value"
        );
        assert_eq!(
            d.user_message("codex", bad_request(), false),
            format!(
                "codex request failed: 400 Bad Request \
                 [invalid_request_error/unsupported_value, param reasoning.effort] — {EXPLAIN_REASONING}"
            )
        );
    }

    #[test]
    fn unsupported_model_on_chatgpt_account_gets_static_hint() {
        let d = detail(
            r#"{"detail":"The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account."}"#,
        );
        assert!(d.account_entitlement);
        assert_eq!(d.trace_code(400), "http_400");
        let msg = d.user_message("codex", bad_request(), false);
        assert_eq!(
            msg,
            format!("codex request failed: 400 Bad Request — {EXPLAIN_ENTITLEMENT}")
        );
        assert!(!msg.contains("gpt-6.1-sol"), "no provider prose: {msg}");
    }

    #[test]
    fn disallowed_hosted_tool_400_maps_to_static_text() {
        let d = detail(
            r#"{"error":{"message":"Tool 'web_search' is not supported with gpt-5.3-codex-spark.","type":"invalid_request_error","param":"tools","code":null}}"#,
        );
        assert_eq!(d.trace_code(400), "http_400:invalid_request_error");
        assert_eq!(
            d.user_message("codex", bad_request(), false),
            format!(
                "codex request failed: 400 Bad Request [invalid_request_error, param tools] — {EXPLAIN_TOOLS}"
            )
        );
    }

    #[test]
    fn unsupported_parameter_maps_to_static_text() {
        let d = detail(
            r#"{"error":{"message":"Unsupported parameter: 'temperature' is not supported with this model.","type":"invalid_request_error","param":"temperature","code":"unsupported_parameter"}}"#,
        );
        let msg = d.user_message("codex", bad_request(), false);
        assert!(msg.ends_with(EXPLAIN_PARAMETER), "{msg}");
        assert!(!msg.contains("not supported with this model"), "{msg}");
    }

    #[test]
    fn unknown_combination_shows_identifiers_only() {
        let d = detail(
            r#"{"error":{"message":"Something novel happened to your request.","type":"invalid_request_error","code":"brand_new_code"}}"#,
        );
        assert_eq!(
            d.user_message("codex", bad_request(), false),
            "codex request failed: 400 Bad Request [invalid_request_error/brand_new_code]"
        );
    }

    #[test]
    fn secret_shaped_identifiers_are_dropped() {
        let body = json!({"error": {
            "type": "Invalid Request ECHO",
            "code": "a3f9c2b18e7d4f60",
            "param": "input[12345].content",
        }})
        .to_string();
        let d = detail(&body);
        assert_eq!(d, CodexHttpErrorDetail::default());
        assert_eq!(
            d.user_message("codex", bad_request(), false),
            "codex request failed: 400 Bad Request"
        );
        let d = detail(r#"{"error":{"type":"sk-ABCDEF","code":"Bearer"}}"#);
        assert_eq!(d.kind, None);
        assert_eq!(d.code, None);
    }

    #[test]
    fn non_json_or_truncated_bodies_keep_the_bare_status() {
        for body in [
            "<html>Bad Request</html>",
            r#"{"error":{"message":"trunc"#,
            "",
        ] {
            let d = detail(body);
            assert_eq!(d, CodexHttpErrorDetail::default());
            assert_eq!(
                d.user_message("codex", bad_request(), false),
                "codex request failed: 400 Bad Request"
            );
        }
    }

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn web_search_gated_by_role_and_model() {
        use ExecutionRole::{Foreground, Internal, Worker};
        let on = env_of(&[("SYNAPS_CODEX_WEB_SEARCH", "live")]);
        let ws = json!({"type": "web_search"});
        assert_eq!(
            web_search_decision("gpt-5.5", Foreground, &on),
            WebSearchDecision::Attach(ws.clone())
        );
        assert_eq!(
            web_search_decision("gpt-5.5", Foreground, env_of(&[])),
            WebSearchDecision::Off
        );
        assert!(matches!(
            web_search_decision(SPARK, Foreground, &on),
            WebSearchDecision::ModelUnsupported(_)
        ));
        // Responses-Lite rows upstream still get it: Synaps sends full Responses.
        assert_eq!(
            web_search_decision("gpt-5.6-sol", Foreground, &on),
            WebSearchDecision::Attach(ws.clone())
        );
        assert_eq!(
            web_search_decision("gpt-5.5", Worker, &on),
            WebSearchDecision::RoleExcluded
        );
        assert_eq!(
            web_search_decision("gpt-5.5", Internal, &on),
            WebSearchDecision::RoleExcluded
        );
        let workers = env_of(&[
            ("SYNAPS_CODEX_WEB_SEARCH", "cached"),
            ("SYNAPS_CODEX_WEB_SEARCH_WORKERS", "1"),
        ]);
        assert_eq!(
            codex_web_search_for_request("gpt-5.5", Worker, &workers),
            Some(json!({"type": "web_search", "external_web_access": false}))
        );
        assert_eq!(
            codex_web_search_for_request("gpt-5.5", Internal, &workers),
            None
        );
    }

    #[test]
    fn learned_rejection_is_per_model_and_spark_reason_names_the_table() {
        let on = env_of(&[("SYNAPS_CODEX_WEB_SEARCH", "on")]);
        let fg = ExecutionRole::Foreground;
        assert!(codex_web_search_for_request("gpt-zz-rej-1", fg, &on).is_some());
        assert!(record_web_search_rejected("gpt-zz-rej-1"));
        assert!(!record_web_search_rejected("gpt-zz-rej-1"), "only new once");
        assert_eq!(codex_web_search_for_request("gpt-zz-rej-1", fg, &on), None);
        assert!(codex_web_search_for_request("gpt-zz-rej-2", fg, &on).is_some());
        assert!(codex_web_search_unsupported_reason(SPARK)
            .unwrap()
            .contains("built-in table"));
        assert!(codex_web_search_unsupported_reason("gpt-zz-rej-1")
            .unwrap()
            .contains("rejected"));
    }

    #[test]
    fn hosted_search_retry_happens_once_and_only_for_tool_rejections() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let rejected = || -> Box<dyn std::error::Error + Send + Sync> {
            Box::new(CodexRequestRejected {
                message: "codex request failed: 400 Bad Request".into(),
                hosted_tool_rejected: true,
            })
        };
        let mut attached = true;
        assert!(hosted_search_retry(rejected(), &mut attached, "gpt-zz-retry-1", &tx).is_ok());
        assert!(!attached);
        // Second rejection: nothing attached any more, so it is the error.
        assert!(hosted_search_retry(rejected(), &mut attached, "gpt-zz-retry-1", &tx).is_err());
        // Other errors, or nothing attached, never retry.
        let mut attached = true;
        let other: Box<dyn std::error::Error + Send + Sync> = "codex request failed: 401".into();
        assert!(hosted_search_retry(other, &mut attached, "gpt-zz-retry-2", &tx).is_err());
        let not_tools: Box<dyn std::error::Error + Send + Sync> = Box::new(CodexRequestRejected {
            message: "x".into(),
            hosted_tool_rejected: false,
        });
        assert!(hosted_search_retry(not_tools, &mut attached, "gpt-zz-retry-2", &tx).is_err());
        assert!(attached);
        assert_eq!(
            notices(&mut rx),
            vec![
                "🔎 Codex web_search was rejected by gpt-zz-retry-1; continuing without it."
                    .to_string()
            ]
        );
    }

    #[test]
    fn tool_rejection_blames_web_search_only_when_attached() {
        let d = detail(r#"{"error":{"type":"invalid_request_error","param":"tools[2]"}}"#);
        assert!(d.rejects_hosted_tool());
        assert!(d
            .user_message("codex", bad_request(), true)
            .ends_with(EXPLAIN_TOOLS_WEB_SEARCH));
        let generic = d.user_message("codex", bad_request(), false);
        assert!(generic.ends_with(EXPLAIN_TOOLS), "{generic}");
        assert!(!generic.contains("web_search"), "{generic}");
        assert!(detail(r#"{"error":{"code":"unsupported_tool"}}"#).rejects_hosted_tool());
        assert!(!detail(r#"{"error":{"param":"reasoning.effort"}}"#).rejects_hosted_tool());
        let mut body = json!({"tools":[{"type":"function","name":"bash"},{"type":"web_search"}]});
        strip_web_search_tool(&mut body);
        assert_eq!(body, json!({"tools":[{"type":"function","name":"bash"}]}));
    }

    #[test]
    fn session_notice_resolves_codex_through_the_router() {
        let on = env_of(&[("SYNAPS_CODEX_WEB_SEARCH", "live")]);
        assert!(session_web_search_notice("openai-codex/gpt-5.6-sol", &on)
            .unwrap()
            .contains("is on (live)"));
        assert_eq!(session_web_search_notice("xai-auth/grok-4", &on), None);
        assert_eq!(session_web_search_notice("claude-opus-4-7", &on), None);
    }

    #[test]
    fn session_notice_reports_on_or_dropped() {
        let on = env_of(&[("SYNAPS_CODEX_WEB_SEARCH", "live")]);
        let n = session_web_search_notice("openai-codex/gpt-5.5", &on).unwrap();
        assert!(n.contains("is on (live)") && n.contains("daemon"), "{n}");
        assert!(!n.contains('\n'));
        let n = session_web_search_notice("openai-codex/gpt-5.3-codex-spark", &on).unwrap();
        assert!(n.contains("not attached for gpt-5.3-codex-spark"), "{n}");
        let w = env_of(&[
            ("SYNAPS_CODEX_WEB_SEARCH", "cached"),
            ("SYNAPS_CODEX_WEB_SEARCH_WORKERS", "yes"),
        ]);
        let n = session_web_search_notice("openai-codex/gpt-5.5", &w).unwrap();
        assert!(n.contains("(cached, workers too)"), "{n}");
        assert_eq!(
            session_web_search_notice("openai-codex/gpt-5.5", env_of(&[])),
            None
        );
        assert_eq!(session_web_search_notice("anthropic/claude-x", &on), None);
    }

    fn notices(rx: &mut mpsc::UnboundedReceiver<StreamEvent>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let StreamEvent::Session(crate::SessionEvent::Notice(t)) = ev {
                out.push(t);
            }
        }
        out
    }

    #[test]
    fn web_search_call_items_render_every_query_and_find_in_page() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let call =
            |action: Value| json!({"type":"web_search_call","status":"completed","action":action});
        announce_web_search_call(
            "Codex",
            &call(json!({"type":"search","query":"ram dass\npuja chat"})),
            &tx,
        );
        announce_web_search_call("Codex", &json!({"type":"function_call","name":"bash"}), &tx);
        announce_web_search_call(
            "Codex",
            &call(json!({"type":"search","queries":["a","SECRET-2","SECRET-3","d","e","f","g"]})),
            &tx,
        );
        announce_web_search_call(
            "Codex",
            &call(
                json!({"type":"find_in_page","url":"https://example.com/a","pattern":"tok\u{202e}en"}),
            ),
            &tx,
        );
        announce_web_search_call(
            "Codex",
            &call(json!({"type":"open_page","url":"https://example.com/b"})),
            &tx,
        );
        assert_eq!(
            notices(&mut rx),
            vec![
                "🔎 Codex web_search (search, completed): ram dass puja chat".to_string(),
                "🔎 Codex web_search (search, completed): a | SECRET-2 | SECRET-3 | d | e (+2 more)"
                    .to_string(),
                "🔎 Codex web_search (find_in_page, completed): 'token' in https://example.com/a"
                    .to_string(),
                "🔎 Codex web_search (open_page, completed): https://example.com/b".to_string(),
            ]
        );
    }

    #[test]
    fn web_search_log_is_metadata_only() {
        use std::sync::{Arc, Mutex};
        #[derive(Clone)]
        struct W(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for W {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = W(Arc::clone(&buf));
        // The production default filter: `agent_engine=debug`.
        let sub = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("agent_engine=debug"))
            .with_writer(move || sink.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(sub);
        let (tx, _rx) = mpsc::unbounded_channel();
        announce_web_search_call(
            "Codex",
            &json!({"type":"web_search_call","status":"completed",
                    "action":{"type":"search","query":"QZSECRET one","queries":["QZSECRET two","x"]}}),
            &tx,
        );
        let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            logs.contains("provider-hosted web_search call"),
            "record must pass the default filter: {logs}"
        );
        assert!(
            logs.contains("query_count=2") && logs.contains("query_len=12"),
            "{logs}"
        );
        assert!(!logs.contains("QZSECRET"), "query text in logs: {logs}");
    }

    #[test]
    fn worker_web_search_notices_reach_the_parent_tagged() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let call = StreamEvent::Session(crate::SessionEvent::Notice(
            "🔎 Codex web_search (search, completed): q".into(),
        ));
        forward_worker_web_search("researcher", &call, Some(&tx));
        // Other notices (retry text, the session line) are not forwarded.
        for other in [
            "⏳ retrying",
            "🔎 Codex web_search is on (live): …",
            "retry: upstream body",
        ] {
            let ev = StreamEvent::Session(crate::SessionEvent::Notice(other.into()));
            forward_worker_web_search("researcher", &ev, Some(&tx));
        }
        forward_worker_web_search("researcher", &call, None);
        assert_eq!(
            notices(&mut rx),
            vec!["🔎 [worker researcher] Codex web_search (search, completed): q".to_string()]
        );
    }
}
