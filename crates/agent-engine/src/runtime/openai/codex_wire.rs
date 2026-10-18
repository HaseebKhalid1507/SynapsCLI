//! Codex direct-transport wire helpers: legible HTTP 4xx errors and the
//! opt-in OpenAI-hosted `web_search` tool (gating + visibility).
//!
//! Kept out of `stream.rs` so the transport only carries call sites.

use crate::runtime::openai::catalog::ExecutionRole;
use crate::runtime::types::StreamEvent;
use serde_json::Value;
use tokio::sync::mpsc;

// ─── HTTP 4xx error bodies ───────────────────────────────────────────────────

/// Bytes of a Codex 4xx body read for diagnosis. Large enough for the JSON
/// envelope to parse (a truncated body yields no detail, never an error).
pub(crate) const CODEX_ERROR_BODY_CAP: usize = 8 * 1024;
/// Max chars of provider `message` shown to the user / logged.
const MESSAGE_CAP: usize = 200;
/// Quoted spans longer than this are redacted: short ones are parameter
/// values (`'ultra'`, `'gpt-5.3-codex-spark'`); long ones can be echoed
/// request content.
const QUOTED_SPAN_CAP: usize = 48;
/// Unbroken runs this long are treated as secret-shaped (tokens, keys,
/// base64) and redacted.
const SECRET_RUN_MIN: usize = 32;

/// Sanitized detail from a Codex non-2xx JSON body. Every field is either a
/// vetted identifier shape or a capped, redacted message — never raw bytes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct CodexHttpErrorDetail {
    pub kind: Option<String>,
    pub code: Option<String>,
    pub param: Option<String>,
    pub message: Option<String>,
    /// The backend refused the model for this ChatGPT account (entitlement).
    pub account_entitlement: bool,
}

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
        let ident = |raw: Option<&str>| {
            raw.map(|s| super::stream::sanitize_error_identifier(Some(s)))
                .filter(|s| !matches!(*s, "unsafe_or_freeform" | "unlisted_identifier"))
                .map(str::to_string)
        };
        let raw_message = field("message").or_else(|| v.get("detail").and_then(Value::as_str));
        Self {
            kind: ident(field("type")),
            code: ident(field("code")),
            param: field("param").and_then(sanitize_param),
            account_entitlement: raw_message.is_some_and(|m| {
                let m = m.to_ascii_lowercase();
                m.contains("not supported when using codex with a chatgpt account")
            }),
            message: raw_message.and_then(sanitize_provider_message),
        }
    }

    fn is_empty(&self) -> bool {
        self.kind.is_none() && self.code.is_none() && self.param.is_none() && self.message.is_none()
    }

    /// Trace terminal code: `http_<status>` plus sanitized identifiers only
    /// (`http_400:invalid_request_error:unsupported_value`). No free text.
    pub(crate) fn trace_code(&self, status: u16) -> String {
        let mut code = format!("http_{status}");
        for part in [&self.kind, &self.code].into_iter().flatten() {
            code.push(':');
            code.push_str(part);
        }
        code
    }

    /// User-visible error: keeps the `codex request failed: <status>` prefix
    /// (net.rs classifies on it) and appends the sanitized detail.
    pub(crate) fn user_message(&self, label: &str, status: reqwest::StatusCode) -> String {
        let mut out = format!("{label} request failed: {status}");
        if self.is_empty() {
            return out;
        }
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
        if let Some(message) = &self.message {
            out.push_str(": ");
            out.push_str(message);
        }
        if self.account_entitlement {
            out.push_str(
                " — this model isn't available on the selected ChatGPT account; switch account \
                 (`synaps auth use --provider openai-codex`) or model (/model).",
            );
        }
        out
    }
}

/// `param` is a request JSON path (`reasoning.effort`, `tools[3].type`).
fn sanitize_param(raw: &str) -> Option<String> {
    let ok = !raw.is_empty()
        && raw.len() <= 64
        && raw.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'[' | b']')
        });
    ok.then(|| raw.to_string())
}

/// Reduce a provider free-text message to something safe to show: one line,
/// no controls, long quoted spans and secret-shaped runs redacted, capped.
/// A message carrying structure (`{`, `}`, `"`, `<`, `>`, `\`) is dropped
/// whole: that is what an echoed request body or an HTML page looks like,
/// and real Codex error prose uses single quotes.
pub(crate) fn sanitize_provider_message(raw: &str) -> Option<String> {
    if raw.contains(['{', '}', '"', '<', '>', '\\']) {
        return None;
    }
    let flat = one_line(raw);
    let unquoted = redact_long_quoted_spans(&flat);
    let redacted = unquoted
        .split(' ')
        .map(|word| {
            if secret_shaped(word) {
                "[redacted]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let trimmed = redacted.trim();
    (!trimmed.is_empty()).then(|| cap_chars(trimmed))
}

fn one_line(raw: &str) -> String {
    let flat: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    flat.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cap_chars(s: &str) -> String {
    if s.chars().count() > MESSAGE_CAP {
        let mut out: String = s.chars().take(MESSAGE_CAP).collect();
        out.push('…');
        out
    } else {
        s.to_string()
    }
}

fn redact_long_quoted_spans(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find(['\'', '"', '`']) {
        let quote = rest[start..].chars().next().unwrap_or('\'');
        out.push_str(&rest[..=start]);
        let after = &rest[start + 1..];
        let (span, tail) = match after.find(quote) {
            Some(end) => (&after[..end], Some(&after[end + 1..])),
            // Unterminated: everything after the quote may be content.
            None => (after, None),
        };
        if span.chars().count() > QUOTED_SPAN_CAP {
            out.push_str("[redacted]");
        } else {
            out.push_str(span);
        }
        match tail {
            Some(tail) => {
                out.push(quote);
                rest = tail;
            }
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn secret_shaped(word: &str) -> bool {
    let core = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let lower = core.to_ascii_lowercase();
    lower.starts_with("sk-")
        || lower.starts_with("eyj")
        || lower == "bearer"
        || (core.len() >= SECRET_RUN_MIN
            && core.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+' | b'/' | b'=')
            }))
}

// ─── Hosted web_search: gating ───────────────────────────────────────────────

/// Workers (subagents) get the hosted tool only when this is also truthy.
pub(crate) const CODEX_WEB_SEARCH_WORKERS_ENV: &str = "SYNAPS_CODEX_WEB_SEARCH_WORKERS";

/// Models with no search support even without live catalog evidence.
/// `gpt-5.3-codex-spark` is text-only and doesn't take hosted tools.
const STATIC_NO_SEARCH_MODELS: &[&str] = &["gpt-5.3-codex-spark"];

type LiveSearchSupport = std::collections::HashMap<String, bool>;
static LIVE_SEARCH_SUPPORT: std::sync::Mutex<Option<LiveSearchSupport>> =
    std::sync::Mutex::new(None);

/// Record a live `/codex/models` row's explicit `supports_search_tool`.
pub(crate) fn record_live_search_tool_support(slug: &str, supported: bool) {
    LIVE_SEARCH_SUPPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(Default::default)
        .insert(slug.to_string(), supported);
}

/// Whether the hosted `web_search` tool may be attached for `model_id`.
/// Live catalog evidence wins; otherwise a static denylist; unknown models
/// keep the user's opt-in.
pub(crate) fn codex_model_supports_web_search(model_id: &str) -> bool {
    let live = LIVE_SEARCH_SUPPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .and_then(|m| m.get(model_id).copied());
    live.unwrap_or(!STATIC_NO_SEARCH_MODELS.contains(&model_id))
}

fn truthy(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("on" | "true" | "1" | "yes")
    )
}

/// The hosted tool spec for one Codex request, or `None`. `env` is the
/// process environment (injected for tests). Internal turns never get it;
/// workers only with [`CODEX_WEB_SEARCH_WORKERS_ENV`]; models that can't use
/// it never do.
pub(crate) fn codex_web_search_for_request(
    model_id: &str,
    role: ExecutionRole,
    env: impl Fn(&str) -> Option<String>,
) -> Option<Value> {
    let spec =
        super::stream::codex_web_search_tool(env(super::stream::CODEX_WEB_SEARCH_ENV).as_deref())?;
    let role_ok = match role {
        ExecutionRole::Foreground => true,
        ExecutionRole::Worker => truthy(env(CODEX_WEB_SEARCH_WORKERS_ENV).as_deref()),
        ExecutionRole::Internal => false,
    };
    if !role_ok {
        tracing::debug!(
            role = role.as_str(),
            "codex web_search not attached for this role"
        );
        return None;
    }
    if !codex_model_supports_web_search(model_id) {
        tracing::debug!(
            model = model_id,
            "codex web_search not attached: model lacks search support"
        );
        return None;
    }
    Some(spec)
}

// ─── Hosted web_search: visibility ───────────────────────────────────────────

static ANNOUNCED_SESSIONS: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
    std::sync::Mutex::new(None);

/// One-line notice, once per session, that hosted search is on and where the
/// switch was read from. Returns whether it was emitted.
pub(crate) fn announce_web_search_enabled(
    session_key: &str,
    spec: &Value,
    tx: &mpsc::UnboundedSender<StreamEvent>,
) -> bool {
    {
        let mut guard = ANNOUNCED_SESSIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let set = guard.get_or_insert_with(Default::default);
        if set.len() > 4096 {
            set.clear();
        }
        if !set.insert(session_key.to_string()) {
            return false;
        }
    }
    let mode = if spec.get("external_web_access") == Some(&Value::Bool(false)) {
        "cached"
    } else {
        "live"
    };
    let _ = tx.send(StreamEvent::Session(crate::SessionEvent::Notice(format!(
        "🔎 Codex web_search is on ({mode}): {} was read from the engine process environment \
         (the daemon's, when attached). Searches run on OpenAI's servers; hooks can't gate them.",
        super::stream::CODEX_WEB_SEARCH_ENV
    ))));
    true
}

/// Surface a completed hosted `web_search_call` output item as a transcript
/// notice and a log event. No-op for any other item type.
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
    let target = action
        .and_then(|a| {
            a.get("query")
                .or_else(|| a.get("url"))
                .or_else(|| a.get("pattern"))
        })
        .and_then(Value::as_str)
        .or_else(|| {
            action
                .and_then(|a| a.get("queries"))
                .and_then(Value::as_array)
                .and_then(|q| q.first())
                .and_then(Value::as_str)
        })
        .map(|raw| cap_chars(&one_line(raw)))
        .filter(|s| !s.is_empty());
    tracing::info!(
        target: "synaps::web_search",
        provider = provider_label,
        status = %status,
        action = %action_kind,
        query = target.as_deref().unwrap_or(""),
        "provider-hosted web_search call"
    );
    let text = match target {
        Some(q) => format!("🔎 {provider_label} web_search ({action_kind}, {status}): {q}"),
        None => format!("🔎 {provider_label} web_search ({action_kind}, {status})"),
    };
    let _ = tx.send(StreamEvent::Session(crate::SessionEvent::Notice(text)));
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
    fn invalid_reasoning_effort_400_is_legible() {
        let d = detail(
            r#"{"error":{"message":"Unsupported value: 'reasoning.effort' does not support 'max' with this model. Supported values are: 'low', 'medium', 'high', and 'xhigh'.","type":"invalid_request_error","param":"reasoning.effort","code":"unsupported_value"}}"#,
        );
        assert_eq!(
            d.trace_code(400),
            "http_400:invalid_request_error:unsupported_value"
        );
        let msg = d.user_message("codex", bad_request());
        assert!(
            msg.starts_with(
                "codex request failed: 400 Bad Request \
                 [invalid_request_error/unsupported_value, param reasoning.effort]: "
            ),
            "{msg}"
        );
        assert!(msg.contains("does not support 'max'"), "{msg}");
        assert!(!d.account_entitlement);
    }

    #[test]
    fn unsupported_model_on_chatgpt_account_gets_static_hint() {
        let d = detail(
            r#"{"detail":"The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account."}"#,
        );
        assert!(d.account_entitlement);
        assert_eq!(d.trace_code(400), "http_400");
        let msg = d.user_message("codex", bad_request());
        assert!(
            msg.contains(": The 'gpt-6.1-sol' model is not supported"),
            "{msg}"
        );
        assert!(
            msg.contains("synaps auth use --provider openai-codex"),
            "{msg}"
        );
    }

    #[test]
    fn disallowed_hosted_tool_400_is_legible() {
        let d = detail(
            r#"{"error":{"message":"Tool 'web_search' is not supported with gpt-5.3-codex-spark.","type":"invalid_request_error","param":"tools","code":null}}"#,
        );
        assert_eq!(d.trace_code(400), "http_400:invalid_request_error");
        assert_eq!(
            d.user_message("codex", bad_request()),
            "codex request failed: 400 Bad Request [invalid_request_error, param tools]: \
             Tool 'web_search' is not supported with gpt-5.3-codex-spark."
        );
    }

    #[test]
    fn echoed_request_content_and_tokens_are_redacted() {
        let prompt = "please summarise my private notes about the quarterly board meeting";
        let token = "sk-proj-AbCdEf0123456789abcdef0123456789";
        let jwt = "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiIxIn0.c2ln";
        let run = "OAI_TRACE_SENTINEL_5eed_never_persist";
        let body = json!({"error": {
            "message": format!(
                "Invalid 'input[0].content[0].text': '{prompt}'. Key {token} jwt {jwt} {run}\nline2 \u{1b}[31m"
            ),
            "type": "Invalid Request ECHO",
            "code": "sk-ABCDEF",
            "param": "input[0].content[0].text",
        }})
        .to_string();
        let d = detail(&body);
        let msg = d.user_message("codex", bad_request());
        for leak in [prompt, token, jwt, run, "sk-ABCDEF", "ECHO", "\u{1b}", "\n"] {
            assert!(!msg.contains(leak), "leaked {leak:?}: {msg}");
        }
        assert!(msg.contains("'[redacted]'"), "{msg}");
        assert_eq!(d.kind, None);
        assert_eq!(d.code, None);
        assert_eq!(d.param.as_deref(), Some("input[0].content[0].text"));
        assert_eq!(d.trace_code(400), "http_400");
    }

    #[test]
    fn echoed_request_json_drops_the_message_but_keeps_identifiers() {
        let echo = r#"ECHOED:{"messages":[{"role":"user","content":"hi"}],"tools":[]}"#;
        let body = json!({"error": {"message": echo, "type": "invalid_request_error"}}).to_string();
        let d = detail(&body);
        assert_eq!(d.message, None);
        assert_eq!(
            d.user_message("codex", bad_request()),
            "codex request failed: 400 Bad Request [invalid_request_error]"
        );
        assert_eq!(
            sanitize_provider_message("<html><body>Bad</body></html>"),
            None
        );
    }

    #[test]
    fn message_is_capped() {
        let body = json!({"error": {"message": "word ".repeat(200)}}).to_string();
        let m = detail(&body).message.unwrap();
        assert!(m.chars().count() <= MESSAGE_CAP + 1);
        assert!(m.ends_with('…'));
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
                d.user_message("codex", bad_request()),
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
        let ws = Some(json!({"type": "web_search"}));
        assert_eq!(
            codex_web_search_for_request("gpt-5.6-sol", Foreground, &on),
            ws
        );
        // Off unless configured.
        assert_eq!(
            codex_web_search_for_request("gpt-5.6-sol", Foreground, env_of(&[])),
            None
        );
        // Text-only spark never gets it.
        assert_eq!(
            codex_web_search_for_request("gpt-5.3-codex-spark", Foreground, &on),
            None
        );
        // Workers need the explicit worker opt-in; internal turns never.
        assert_eq!(
            codex_web_search_for_request("gpt-5.6-sol", Worker, &on),
            None
        );
        assert_eq!(
            codex_web_search_for_request("gpt-5.6-sol", Internal, &on),
            None
        );
        let workers = env_of(&[
            ("SYNAPS_CODEX_WEB_SEARCH", "cached"),
            ("SYNAPS_CODEX_WEB_SEARCH_WORKERS", "1"),
        ]);
        assert_eq!(
            codex_web_search_for_request("gpt-5.6-sol", Worker, &workers),
            Some(json!({"type": "web_search", "external_web_access": false}))
        );
        assert_eq!(
            codex_web_search_for_request("gpt-5.6-sol", Internal, &workers),
            None
        );
    }

    #[test]
    fn live_catalog_search_support_overrides_static() {
        let on = env_of(&[("SYNAPS_CODEX_WEB_SEARCH", "on")]);
        record_live_search_tool_support("gpt-zz-nosearch-1", false);
        record_live_search_tool_support("gpt-zz-search-1", true);
        let fg = ExecutionRole::Foreground;
        assert_eq!(
            codex_web_search_for_request("gpt-zz-nosearch-1", fg, &on),
            None
        );
        assert!(codex_web_search_for_request("gpt-zz-search-1", fg, &on).is_some());
        // Unknown model keeps the opt-in.
        assert!(codex_model_supports_web_search("gpt-zz-unknown-1"));
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
    fn enabled_notice_once_per_session() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let live = json!({"type": "web_search"});
        let cached = json!({"type": "web_search", "external_web_access": false});
        assert!(announce_web_search_enabled("sess-zz-1", &live, &tx));
        assert!(!announce_web_search_enabled("sess-zz-1", &live, &tx));
        assert!(announce_web_search_enabled("sess-zz-2", &cached, &tx));
        let n = notices(&mut rx);
        assert_eq!(n.len(), 2);
        assert!(
            n[0].contains("(live)") && n[0].contains("daemon"),
            "{}",
            n[0]
        );
        assert!(n[1].contains("(cached)"));
        assert!(!n[0].contains('\n'));
    }

    #[test]
    fn web_search_call_item_becomes_notice_with_query() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        announce_web_search_call(
            "Codex",
            &json!({"type":"web_search_call","id":"ws_1","status":"completed",
                    "action":{"type":"search","query":"ram dass\npuja chat"}}),
            &tx,
        );
        announce_web_search_call("Codex", &json!({"type":"function_call","name":"bash"}), &tx);
        announce_web_search_call(
            "Codex",
            &json!({"type":"web_search_call","status":"completed",
                    "action":{"type":"open_page","url":"https://example.com/a"}}),
            &tx,
        );
        assert_eq!(
            notices(&mut rx),
            vec![
                "🔎 Codex web_search (search, completed): ram dass puja chat".to_string(),
                "🔎 Codex web_search (open_page, completed): https://example.com/a".to_string(),
            ]
        );
    }
}
