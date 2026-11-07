//! Bounded continuation after a response stops at the output-token limit
//! (`output_limit_continuations`).
//!
//! A response cut off at the output limit that leaves the agent nothing to
//! execute used to end the turn silently: nobody sends "continue" in
//! headless/daemon automation or inside a subagent, so the work just stopped.
//! The turn loop now appends one short host note and continues, at most
//! `output_limit_continuations` times per user turn (`0` = the old behaviour).
//!
//! Transports report the output limit on the response envelope as the
//! top-level `"stop_reason": "max_tokens"` (Anthropic `max_tokens`,
//! OpenAI-compatible `finish_reason: "length"`, Gemini `MAX_TOKENS`, an
//! extension provider's `max_tokens`). History only ever takes the
//! envelope's `content`, so the key never reaches a request.

use serde_json::{json, Value};

/// The envelope `stop_reason` every transport uses for the output limit.
pub(crate) const OUTPUT_LIMIT_STOP_REASON: &str = "max_tokens";

/// Host note appended (append-only, like the other harness notes) when a
/// cut-off response leaves nothing to execute.
pub(crate) const CONTINUATION_PROMPT: &str = "[Host harness note, not a new user request] Your previous response was cut off at the output-token limit. Continue from where it stopped. Do long work through tool calls and keep each response shorter.";

/// UI-only result for a tool call the limit cut off mid-input. The call
/// never enters history and never runs; this closes its card.
pub(crate) const UNFINISHED_TOOL_CALL: &str =
    "not run: the response hit the output-token limit while this call was being written";

/// Mark a response envelope as cut off at the output limit.
pub(crate) fn mark_output_limit(response: &mut Value) {
    if let Some(envelope) = response.as_object_mut() {
        envelope.insert("stop_reason".into(), json!(OUTPUT_LIMIT_STOP_REASON));
    }
}

/// Whether a response envelope stopped at the output limit.
pub(crate) fn hit_output_limit(response: &Value) -> bool {
    response.get("stop_reason").and_then(Value::as_str) == Some(OUTPUT_LIMIT_STOP_REASON)
}

/// The continuation message the turn loop appends.
pub(crate) fn continuation_message() -> crate::SharedMessage {
    std::sync::Arc::new(json!({"role": "user", "content": CONTINUATION_PROMPT}))
}

/// A `tool_use` whose input was still being written: its JSON did not parse
/// (`__parse_error`, set by the Anthropic and OpenAI-compatible transports).
fn is_unfinished_tool_use(block: &Value) -> bool {
    block["type"].as_str() == Some("tool_use") && block["input"].get("__parse_error").is_some()
}

/// Thinking without a signature cannot be replayed to the provider (the
/// cancellation path drops it for the same reason).
fn is_unsigned_thinking(block: &Value) -> bool {
    block["type"].as_str() == Some("thinking")
        && !block["signature"].as_str().is_some_and(|s| !s.is_empty())
}

/// What the turn loop does with one finished response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputLimitStep {
    /// Not cut off at the output limit, cancelled, or the feature is off:
    /// the response is untouched and the turn runs exactly as before.
    Unchanged,
    /// Cut off, but complete tool calls remain: they run as normal, without
    /// any extra message. Unfinished blocks were removed.
    RunTools,
    /// Cut off with nothing to execute: unfinished blocks were removed; the
    /// loop appends [`continuation_message`] and continues.
    Continue,
    /// Cut off with nothing to execute and no continuations left: the
    /// response is untouched and the turn proceeds as before.
    Exhausted,
}

/// Per-user-turn continuation counter.
#[derive(Debug)]
pub(crate) struct OutputLimitContinuation {
    cap: u32,
    used: u32,
}

impl OutputLimitContinuation {
    pub(crate) fn new(cap: u32) -> Self {
        Self { cap, used: 0 }
    }

    pub(crate) fn cap(&self) -> u32 {
        self.cap
    }

    pub(crate) fn used(&self) -> u32 {
        self.used
    }

    /// Classify `response`. For [`OutputLimitStep::RunTools`] and
    /// [`OutputLimitStep::Continue`] the blocks the provider never finished
    /// (a tool call cut off mid-input, thinking without a signature) are
    /// removed from its content; their tool ids are returned so the UI can
    /// close those cards. Otherwise the response is left byte-identical.
    pub(crate) fn inspect(
        &self,
        response: &mut Value,
        cancelled: bool,
    ) -> (OutputLimitStep, Vec<String>) {
        if self.cap == 0 || cancelled || !hit_output_limit(response) {
            return (OutputLimitStep::Unchanged, Vec::new());
        }
        let Some(content) = response.get("content").and_then(Value::as_array) else {
            return (OutputLimitStep::Unchanged, Vec::new());
        };
        let has_complete_tool_use = content
            .iter()
            .any(|b| b["type"].as_str() == Some("tool_use") && !is_unfinished_tool_use(b));
        let step = if has_complete_tool_use {
            OutputLimitStep::RunTools
        } else if self.used < self.cap {
            OutputLimitStep::Continue
        } else {
            return (OutputLimitStep::Exhausted, Vec::new());
        };
        let dropped: Vec<String> = content
            .iter()
            .filter(|b| is_unfinished_tool_use(b))
            .filter_map(|b| b["id"].as_str().map(str::to_string))
            .collect();
        if content
            .iter()
            .any(|b| is_unfinished_tool_use(b) || is_unsigned_thinking(b))
        {
            if let Some(content) = response.get_mut("content").and_then(Value::as_array_mut) {
                content.retain(|b| !is_unfinished_tool_use(b) && !is_unsigned_thinking(b));
            }
        }
        (step, dropped)
    }

    /// Spend one continuation; the visible notice for it.
    pub(crate) fn take(&mut self) -> String {
        self.used = self.used.saturating_add(1).min(self.cap);
        format!(
            "↻ response hit the output limit — continuing ({}/{})",
            self.used, self.cap
        )
    }

    /// Notice for a cut-off response once every continuation is spent.
    pub(crate) fn exhausted_notice(&self) -> String {
        format!(
            "response hit the output limit — automatic continuations used up ({}/{}); the turn ends here. Send a message to continue.",
            self.used, self.cap
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cut(content: Value) -> Value {
        json!({"content": content, "stop_reason": "max_tokens"})
    }

    fn partial_tool(id: &str) -> Value {
        json!({"type":"tool_use","id":id,"name":"write","input":{"__parse_error":"invalid tool input JSON: EOF"}})
    }

    #[test]
    fn mark_and_detect_round_trip() {
        let mut v = json!({"content": []});
        assert!(!hit_output_limit(&v));
        mark_output_limit(&mut v);
        assert!(hit_output_limit(&v));
        assert_eq!(v["content"], json!([]), "content untouched");
        for other in ["end_turn", "tool_use", "refusal", "length", "MAX_TOKENS"] {
            assert!(!hit_output_limit(&json!({"content": [], "stop_reason": other})), "{other}");
        }
    }

    #[test]
    fn zero_cap_leaves_every_response_untouched() {
        let limit = OutputLimitContinuation::new(0);
        let original = cut(json!([
            {"type":"thinking","thinking":"half","signature":""},
            partial_tool("t1"),
        ]));
        let mut response = original.clone();
        assert_eq!(limit.inspect(&mut response, false), (OutputLimitStep::Unchanged, vec![]));
        assert_eq!(response, original);
    }

    #[test]
    fn untruncated_or_cancelled_responses_are_untouched() {
        let limit = OutputLimitContinuation::new(2);
        let mut clean = json!({"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"});
        let before = clean.clone();
        assert_eq!(limit.inspect(&mut clean, false).0, OutputLimitStep::Unchanged);
        assert_eq!(clean, before);
        let mut cancelled = cut(json!([partial_tool("t1")]));
        let before = cancelled.clone();
        assert_eq!(limit.inspect(&mut cancelled, true).0, OutputLimitStep::Unchanged);
        assert_eq!(cancelled, before);
    }

    #[test]
    fn text_only_cut_off_continues_until_the_cap_then_is_exhausted() {
        let mut limit = OutputLimitContinuation::new(2);
        let original = cut(json!([{"type":"text","text":"I will now"}]));
        for n in 1..=2 {
            let mut response = original.clone();
            assert_eq!(limit.inspect(&mut response, false), (OutputLimitStep::Continue, vec![]));
            assert_eq!(response, original, "nothing unfinished to drop");
            assert_eq!(
                limit.take(),
                format!("↻ response hit the output limit — continuing ({n}/2)")
            );
        }
        let mut response = original.clone();
        assert_eq!(limit.inspect(&mut response, false), (OutputLimitStep::Exhausted, vec![]));
        assert_eq!(response, original, "exhausted leaves the response as before");
        assert!(limit.exhausted_notice().contains("(2/2)"));
    }

    #[test]
    fn continue_drops_unsigned_thinking_and_partial_tool_calls() {
        let limit = OutputLimitContinuation::new(1);
        let mut response = cut(json!([
            {"type":"thinking","thinking":"signed","signature":"sig"},
            {"type":"text","text":"Writing the file now."},
            {"type":"thinking","thinking":"cut mid-thought","signature":""},
            partial_tool("t9"),
        ]));
        let (step, dropped) = limit.inspect(&mut response, false);
        assert_eq!(step, OutputLimitStep::Continue);
        assert_eq!(dropped, vec!["t9".to_string()]);
        assert_eq!(
            response["content"],
            json!([
                {"type":"thinking","thinking":"signed","signature":"sig"},
                {"type":"text","text":"Writing the file now."},
            ])
        );
    }

    #[test]
    fn thinking_without_any_signature_field_is_dropped() {
        let limit = OutputLimitContinuation::new(1);
        let mut response = cut(json!([{"type":"thinking","thinking":"no sig field"}]));
        assert_eq!(limit.inspect(&mut response, false).0, OutputLimitStep::Continue);
        assert_eq!(response["content"], json!([]));
    }

    #[test]
    fn complete_tool_calls_run_and_only_the_partial_one_is_dropped() {
        let limit = OutputLimitContinuation::new(1);
        let complete = json!({"type":"tool_use","id":"t1","name":"bash","input":{"command":"ls"}});
        let mut response = cut(json!([complete.clone(), partial_tool("t2")]));
        let (step, dropped) = limit.inspect(&mut response, false);
        assert_eq!(step, OutputLimitStep::RunTools);
        assert_eq!(dropped, vec!["t2".to_string()]);
        assert_eq!(response["content"], json!([complete]));
    }

    #[test]
    fn complete_tool_calls_run_even_with_no_continuations_left() {
        let mut limit = OutputLimitContinuation::new(1);
        limit.take();
        let complete = json!({"type":"tool_use","id":"t1","name":"bash","input":{"command":"ls"}});
        let mut response = cut(json!([complete.clone()]));
        assert_eq!(limit.inspect(&mut response, false), (OutputLimitStep::RunTools, vec![]));
        assert_eq!(response["content"], json!([complete]));
    }

    #[test]
    fn continuation_message_is_a_plain_user_note() {
        let message = continuation_message();
        assert_eq!(message["role"], "user");
        assert_eq!(message["content"], CONTINUATION_PROMPT);
        assert!(CONTINUATION_PROMPT.starts_with("[Host harness note, not a new user request]"));
    }
}
