//! Metadata-only Codex terminal diagnostics. Never retains provider text or IDs.
//! Counts are bounded observations, not a second response parser. Nothing here
//! changes dispatch, retry, output, accounting, or account selection.
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug, Default)]
pub(crate) struct Context {
    pub session: Option<String>,
    pub window: Option<u64>,
}

tokio::task_local! {
    pub(crate) static CONTEXT: Context;
}

const LIMIT: usize = 128;
const SAMPLE_EVERY: u64 = 64;
static SUCCESSES: AtomicU64 = AtomicU64::new(0);

/// Fixed bins; unknown wire strings are never copied into diagnostics.
const ITEM_TYPES: [&str; 8] = [
    "message",
    "reasoning",
    "function_call",
    "web_search_call",
    "computer_call",
    "file_search_call",
    "custom_tool_call",
    "other",
];
const PART_TYPES: [&str; 5] = [
    "output_text",
    "refusal",
    "reasoning_text",
    "summary_text",
    "other",
];

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Inventory {
    items: [u64; 8],
    parts: [u64; 5],
    text_bytes: u64,
    refusal_bytes: u64,
    truncated: bool,
}

fn bin<const N: usize>(value: Option<&str>, names: &[&str; N]) -> usize {
    names
        .iter()
        .position(|s| Some(*s) == value)
        .unwrap_or(N - 1)
}

impl Inventory {
    fn item(&mut self, item: &Value) {
        let kind = item.get("type").and_then(Value::as_str);
        let i = bin(kind, &ITEM_TYPES);
        self.items[i] = self.items[i].saturating_add(1);
        // Only the schema-defined content/summary arrays; never recursively
        // inspect tool arguments, encrypted reasoning, URLs, or arbitrary keys.
        for key in ["content", "summary"] {
            if let Some(parts) = item.get(key).and_then(Value::as_array) {
                self.truncated |= parts.len() > LIMIT;
                for part in parts.iter().take(LIMIT) {
                    let kind = part.get("type").and_then(Value::as_str);
                    let i = bin(kind, &PART_TYPES);
                    self.parts[i] = self.parts[i].saturating_add(1);
                    if kind == Some("output_text") {
                        self.text_bytes = self.text_bytes.saturating_add(
                            part.get("text")
                                .and_then(Value::as_str)
                                .map_or(0, |s| s.len() as u64),
                        );
                    } else if kind == Some("refusal") {
                        self.refusal_bytes = self.refusal_bytes.saturating_add(
                            part.get("refusal")
                                .and_then(Value::as_str)
                                .map_or(0, |s| s.len() as u64),
                        );
                    }
                }
            }
        }
    }

    fn output(output: Option<&Value>) -> Self {
        let mut out = Self::default();
        if let Some(items) = output.and_then(Value::as_array) {
            out.truncated = items.len() > LIMIT;
            for item in items.iter().take(LIMIT) {
                out.item(item);
            }
        }
        out
    }
}

fn status(value: Option<&str>) -> &'static str {
    match value {
        Some("completed") => "completed",
        Some("incomplete") => "incomplete",
        Some("failed") => "failed",
        Some("cancelled") => "cancelled",
        Some("in_progress") => "in_progress",
        Some("queued") => "queued",
        None => "absent",
        _ => "other",
    }
}
fn reason(value: Option<&str>) -> &'static str {
    match value {
        Some("max_output_tokens") => "max_output_tokens",
        Some("content_filter") => "content_filter",
        None => "absent",
        _ => "other",
    }
}

fn error_code(value: Option<&str>) -> &'static str {
    match value {
        Some("context_length_exceeded") => "context_length_exceeded",
        Some("max_tokens_exceeded") => "max_tokens_exceeded",
        Some("input_too_long") => "input_too_long",
        Some("request_too_large") => "request_too_large",
        Some("invalid_request_error") => "invalid_request_error",
        Some("server_error") => "server_error",
        Some("rate_limit_exceeded") => "rate_limit_exceeded",
        Some("insufficient_quota") => "insufficient_quota",
        None => "absent",
        _ => "other",
    }
}

/// Local host labels only. Reject an invalid/oversized label in full, rather
/// than copying an arbitrary prefix that might itself contain private text.
fn label(value: &str) -> &str {
    if !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:@/".contains(&b))
    {
        value
    } else {
        "unavailable"
    }
}

#[derive(Debug)]
pub(crate) struct Observation {
    event: &'static str,
    status: &'static str,
    reason: &'static str,
    error_code: &'static str,
    added: Inventory,
    added_items: usize,
    part_added: [u64; 5],
    done: Inventory,
    terminal: Inventory,
    done_items: usize,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    delta_count: u64,
    delta_bytes: u64,
    done_text_bytes: u64,
    refusal_events: u64,
    reasoning_events: u64,
}
impl Default for Observation {
    fn default() -> Self {
        Self {
            event: "absent",
            status: "absent",
            reason: "absent",
            error_code: "absent",
            added: Inventory::default(),
            added_items: 0,
            part_added: [0; 5],
            done: Inventory::default(),
            terminal: Inventory::default(),
            done_items: 0,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            reasoning_tokens: None,
            delta_count: 0,
            delta_bytes: 0,
            done_text_bytes: 0,
            refusal_events: 0,
            reasoning_events: 0,
        }
    }
}
impl Observation {
    pub fn observe(&mut self, event: &Value) {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "response.output_item.added" => {
                if self.added_items < LIMIT {
                    if let Some(item) = event.get("item") {
                        self.added.item(item);
                        self.added_items += 1;
                    }
                } else {
                    self.added.truncated = true;
                }
            }
            "response.content_part.added" | "response.reasoning_summary_part.added" => {
                let kind = event.pointer("/part/type").and_then(Value::as_str);
                let i = bin(kind, &PART_TYPES);
                self.part_added[i] = self.part_added[i].saturating_add(1);
            }
            "response.output_item.done" => {
                if self.done_items < LIMIT {
                    if let Some(item) = event.get("item") {
                        self.done.item(item);
                        self.done_items += 1;
                    }
                } else {
                    self.done.truncated = true;
                }
            }
            "response.output_text.delta" => {
                if let Some(text) = event.get("delta").and_then(Value::as_str) {
                    self.delta_count = self.delta_count.saturating_add(1);
                    self.delta_bytes = self.delta_bytes.saturating_add(text.len() as u64);
                }
            }
            "response.output_text.done" => {
                self.done_text_bytes = self.done_text_bytes.saturating_add(
                    event
                        .get("text")
                        .and_then(Value::as_str)
                        .map_or(0, |s| s.len() as u64),
                );
            }
            "response.completed"
            | "response.done"
            | "response.incomplete"
            | "response.failed"
            | "error" => {
                self.event = match kind {
                    "response.completed" => "completed",
                    "response.done" => "done",
                    "response.incomplete" => "incomplete",
                    "response.failed" => "failed",
                    _ => "error",
                };
                let response = event.get("response");
                self.error_code = error_code(
                    event
                        .pointer("/response/error/code")
                        .or_else(|| event.pointer("/error/code"))
                        .or_else(|| event.get("code"))
                        .or_else(|| event.pointer("/response/error/type"))
                        .or_else(|| event.pointer("/error/type"))
                        .and_then(Value::as_str),
                );
                self.status = status(
                    response
                        .and_then(|r| r.get("status"))
                        .and_then(Value::as_str),
                );
                self.reason = reason(
                    response
                        .and_then(|r| r.pointer("/incomplete_details/reason"))
                        .and_then(Value::as_str),
                );
                self.terminal = Inventory::output(response.and_then(|r| r.get("output")));
                let usage = response
                    .and_then(|r| r.get("usage"))
                    .or_else(|| event.get("usage"));
                let count = |key| usage.and_then(|u| u.get(key)).and_then(Value::as_u64);
                // Replace, don't accumulate repeated terminal totals. Missing
                // counters stay absent, rather than being reported as zero.
                self.input_tokens = count("input_tokens");
                self.output_tokens = count("output_tokens");
                self.total_tokens = count("total_tokens");
                self.reasoning_tokens = usage
                    .and_then(|u| u.pointer("/output_tokens_details/reasoning_tokens"))
                    .and_then(Value::as_u64);
            }
            _ if kind.starts_with("response.refusal.") => {
                self.refusal_events = self.refusal_events.saturating_add(1)
            }
            _ if kind.starts_with("response.reasoning") => {
                self.reasoning_events = self.reasoning_events.saturating_add(1)
            }
            _ => {}
        }
    }

    pub fn http_rejected(code: Option<&str>) -> Self {
        Self {
            event: "http_rejected",
            status: "failed",
            error_code: error_code(code),
            ..Self::default()
        }
    }

    pub fn emit(&self, request: &Request, account: &str, usable: bool) {
        if self.event == "absent"
            || (usable
                && !SUCCESSES
                    .fetch_add(1, Ordering::Relaxed)
                    .is_multiple_of(SAMPLE_EVERY))
        {
            return;
        }
        tracing::debug!(
            event = "codex_terminal_metadata", schema = 1,
            request_id = %request.id, session_id = %request.session,
            account = label(account), usable_output = usable, success_sample_every = SAMPLE_EVERY,
            terminal_event = self.event, response_status = self.status, incomplete_reason = self.reason,
            provider_error_code = self.error_code,
            item_type_bins = ?ITEM_TYPES, part_type_bins = ?PART_TYPES,
            added_items = ?self.added.items, added_parts = ?self.added.parts,
            part_added_events = ?self.part_added, added_inventory_truncated = self.added.truncated,
            done_items = ?self.done.items, done_parts = ?self.done.parts,
            terminal_items = ?self.terminal.items, terminal_parts = ?self.terminal.parts,
            done_inventory_truncated = self.done.truncated, terminal_inventory_truncated = self.terminal.truncated,
            done_item_text_bytes = self.done.text_bytes, terminal_text_bytes = self.terminal.text_bytes,
            done_refusal_bytes = self.done.refusal_bytes, terminal_refusal_bytes = self.terminal.refusal_bytes,
            text_delta_count = self.delta_count, text_delta_bytes = self.delta_bytes,
            text_done_event_bytes = self.done_text_bytes,
            refusal_events = self.refusal_events, reasoning_events = self.reasoning_events,
            input_tokens = ?self.input_tokens, output_tokens = ?self.output_tokens,
            total_tokens = ?self.total_tokens, reasoning_tokens = ?self.reasoning_tokens,
            request_token_estimate = request.estimated_tokens, estimate_kind = "wire_bytes_div4_ceil",
            request_bytes = request.bytes, runtime_window = ?request.context.window,
            catalog_window = ?request.catalog_window, effort = request.effort,
            "Codex terminal metadata"
        );
    }
}

pub(crate) struct Request {
    id: uuid::Uuid,
    context: Context,
    session: String,
    bytes: u64,
    estimated_tokens: u64,
    catalog_window: Option<u64>,
    effort: &'static str,
}
impl Request {
    /// One ID per answered HTTP attempt, including retries; no provider IDs.
    pub fn new(bytes: usize, model: &str, effort: &str) -> Self {
        let context = CONTEXT.try_with(Clone::clone).unwrap_or_default();
        let session = label(context.session.as_deref().unwrap_or("unavailable")).to_owned();
        let catalog_window =
            super::catalog::capability_cache::get(model).and_then(|m| m.context_tokens);
        let effort = match effort {
            "omitted" => "omitted",
            "none" => "none",
            "minimal" => "minimal",
            "low" => "low",
            "medium" => "medium",
            "high" => "high",
            "xhigh" => "xhigh",
            _ => "other",
        };
        let bytes = bytes as u64;
        Self {
            id: uuid::Uuid::new_v4(),
            context,
            session,
            bytes,
            estimated_tokens: bytes.saturating_add(3) / 4,
            catalog_window,
            effort,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inventories_completed_output_without_retaining_content() {
        let mut obs = Observation::default();
        obs.observe(&json!({"type":"response.completed","response":{"status":"completed","output":[
            {"type":"reasoning","encrypted_content":"DO_NOT_LOG","summary":[{"type":"summary_text","text":"DO_NOT_LOG"}]},
            {"type":"message","content":[{"type":"output_text","text":"é"},{"type":"refusal","refusal":"DO_NOT_LOG"}]},
            {"type":"function_call","arguments":"DO_NOT_LOG"}, {"type":"DO_NOT_LOG"}
        ],"usage":{"input_tokens":20,"output_tokens":10,"total_tokens":30,"output_tokens_details":{"reasoning_tokens":8}}}}));
        assert_eq!(obs.terminal.items, [1, 1, 1, 0, 0, 0, 0, 1]);
        assert_eq!(obs.terminal.parts, [1, 1, 0, 1, 0]);
        assert_eq!(obs.terminal.text_bytes, 2);
        assert_eq!(obs.reasoning_tokens, Some(8));
        assert_eq!(obs.total_tokens, Some(30));
        assert!(!format!("{obs:?}").contains("DO_NOT_LOG"));
    }

    #[test]
    fn terminal_metadata_is_allowlisted_and_last_report_wins() {
        let mut obs = Observation::default();
        obs.observe(&json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"usage":{"output_tokens":99}}}));
        assert_eq!(obs.reason, "max_output_tokens");
        assert_eq!(obs.output_tokens, Some(99));
        obs.observe(&json!({"type":"response.completed","response":{"status":"PRIVATE","incomplete_details":{"reason":"PRIVATE"}}}));
        assert_eq!(obs.status, "other");
        assert_eq!(obs.reason, "other");
        assert_eq!(obs.output_tokens, None);
        assert!(!format!("{obs:?}").contains("PRIVATE"));
        assert_eq!(label("token\nsecret"), "unavailable");
        assert_eq!(label(&"x".repeat(129)), "unavailable");
        assert_eq!(label("openai-codex@codex4"), "openai-codex@codex4");
    }

    #[test]
    fn zero_text_reasoning_and_refusal_events_are_distinct() {
        let mut obs = Observation::default();
        obs.observe(&json!({"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"PRIVATE"}}));
        obs.observe(&json!({"type":"response.content_part.added","part":{"type":"refusal","refusal":"PRIVATE"}}));
        assert_eq!(obs.added.items[1], 1);
        assert_eq!(obs.part_added[1], 1);
        for e in [
            json!({"type":"response.reasoning_summary_text.delta","delta":"PRIVATE"}),
            json!({"type":"response.refusal.done","refusal":"PRIVATE"}),
            json!({"type":"response.output_text.done","text":""}),
            json!({"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":""}]}}),
        ] {
            obs.observe(&e);
        }
        assert_eq!(obs.delta_count, 0);
        assert_eq!(obs.done_text_bytes, 0);
        assert_eq!(obs.done.items[0], 1);
        assert_eq!((obs.reasoning_events, obs.refusal_events), (1, 1));
        assert!(!format!("{obs:?}").contains("PRIVATE"));
    }

    #[test]
    fn emitted_record_is_metadata_only_with_fixed_fields() {
        use tracing_subscriber::layer::SubscriberExt;
        let log = crate::tools::test_helpers::DaemonLog::default();
        let _second = tracing::Dispatch::new(tracing_subscriber::registry());
        let subscriber = tracing_subscriber::registry().with(log.clone());
        let _capture = tracing::subscriber::set_default(subscriber);
        let mut obs = Observation::default();
        obs.observe(&json!({"type":"response.completed","response":{
            "id":"PRIVATE_RESPONSE", "status":"PRIVATE_STATUS",
            "incomplete_details":{"reason":"PRIVATE_REASON"},
            "output":[{"type":"message","content":[{"type":"output_text","text":"PRIVATE_TEXT"},{"type":"refusal","refusal":"PRIVATE_REFUSAL"}]}],
            "usage":{"output_tokens":7,"output_tokens_details":{"reasoning_tokens":7}}
        }}));
        let request = Request::new(9, "unknown-model", "PRIVATE_EFFORT");
        obs.emit(&request, "openai-codex@codex4", false);
        let lines = log.lines();
        assert_eq!(lines.len(), 1);
        let fields = &lines[0].fields;
        assert_eq!(fields["response_status"], "other");
        assert_eq!(fields["incomplete_reason"], "other");
        assert_eq!(fields["effort"], "other");
        assert_eq!(fields["account"], "openai-codex@codex4");
        assert_eq!(fields["request_token_estimate"], "3");
        assert!(fields.contains_key("terminal_items"));
        assert!(fields.contains_key("runtime_window"));
        assert!(fields.contains_key("catalog_window"));
        assert!(!format!("{lines:?}").contains("PRIVATE_"));
        // Every unusable terminal is recorded, not just the sampled successes.
        obs.emit(&request, "openai-codex@codex4", false);
        assert_eq!(log.lines().len(), 2);
    }

    #[tokio::test]
    async fn request_context_is_scoped_and_restored() {
        let outside = Request::new(0, "unknown-model", "high");
        assert_eq!(outside.session, "unavailable");
        CONTEXT
            .scope(
                Context {
                    session: Some("session-1".into()),
                    window: Some(1050000),
                },
                async {
                    let a = Request::new(8, "unknown-model", "high");
                    let b = Request::new(8, "unknown-model", "high");
                    assert_eq!(a.session, "session-1");
                    assert_eq!(a.context.window, Some(1050000));
                    assert_ne!(a.id, b.id);
                    assert_eq!(a.estimated_tokens, 2);
                },
            )
            .await;
        assert_eq!(
            Request::new(0, "unknown-model", "high").session,
            "unavailable"
        );
    }

    #[test]
    fn size_refusals_are_allowlisted_for_http_and_sse() {
        let http = Observation::http_rejected(Some("context_length_exceeded"));
        assert_eq!(http.error_code, "context_length_exceeded");
        assert_eq!(http.event, "http_rejected");
        assert_eq!(
            Observation::http_rejected(Some("PRIVATE_CODE")).error_code,
            "other"
        );
        let mut sse = Observation::default();
        sse.observe(&json!({"type":"response.failed","response":{"error":{"code":"context_length_exceeded","message":"PRIVATE"}}}));
        assert_eq!(sse.error_code, "context_length_exceeded");
        assert!(!format!("{sse:?}").contains("PRIVATE"));
    }

    #[test]
    fn inventories_are_bounded() {
        let part = json!({"type":"output_text","text":"x"});
        let item = json!({"type":"message","content":vec![part; LIMIT+1]});
        let inv = Inventory::output(Some(&json!(vec![item; LIMIT + 1])));
        assert!(inv.truncated);
        assert_eq!(inv.items[0], LIMIT as u64);
        assert_eq!(inv.parts[0], (LIMIT * LIMIT) as u64);
    }
}
