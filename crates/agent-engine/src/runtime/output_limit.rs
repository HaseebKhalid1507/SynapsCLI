//! Bounded continuation after a response stops at the output-token limit
//! (`output_limit_continuations`).
//!
//! A response cut off at the output limit that leaves the agent nothing to
//! execute used to end the turn silently: nobody sends "continue" in
//! headless/daemon automation or inside a subagent, so the work just stopped.
//! The turn loop now appends one short host note and continues, at most
//! `output_limit_continuations` times IN A ROW (`0` = the old behaviour): a
//! response that is not cut off and has its tool calls run resets the count,
//! so a long worker that hits the limit now and then never runs out.
//!
//! Transports report the output limit on the response envelope as the
//! top-level `"stop_reason": "max_tokens"` (Anthropic `max_tokens`,
//! OpenAI-compatible Chat Completions `finish_reason: "length"`, Gemini
//! `MAX_TOKENS`, an extension provider's `max_tokens`). History only ever
//! takes the envelope's `content`, so the key never reaches a request.
//! Codex/Responses `response.incomplete` stays a provider error.

use agent_core::core::host_note;
use serde_json::{json, Value};

/// The envelope `stop_reason` every transport uses for the output limit.
pub(crate) const OUTPUT_LIMIT_STOP_REASON: &str = "max_tokens";

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

/// The host note the turn loop appends (append-only, plain user-role text
/// like the other harness notes): when the limit cut a tool call
/// (`cut_tool`), the note naming it and saying it was NOT run; otherwise
/// the plain "continue from where it stopped" note.
pub(crate) fn continuation_message(cut_tool: Option<&str>) -> crate::SharedMessage {
    let text = match cut_tool {
        Some(tool) => host_note::output_limit_tool_note_text(tool),
        None => host_note::OUTPUT_LIMIT_NOTE.to_string(),
    };
    std::sync::Arc::new(json!({"role": "user", "content": text}))
}

/// " while writing a `write` call (not run)" for notices; the name is quoted
/// only when it is a plausible tool name.
fn cut_call_phrase(cut_tool: Option<&str>) -> String {
    match cut_tool {
        Some(tool) if host_note::is_plausible_tool_name(tool) => {
            format!(" while writing a `{tool}` call (not run)")
        }
        Some(_) => " while writing a tool call (not run)".to_string(),
        None => String::new(),
    }
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
        && block["signature"].as_str().is_none_or(|s| s.is_empty())
}

/// What the turn loop does with one finished response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputLimitStep {
    /// Not cut off at the output limit, cancelled, or the feature is off:
    /// the response is untouched and the turn runs exactly as before.
    Unchanged,
    /// Cut off, but complete tool calls remain: they run as normal.
    /// Unfinished blocks were removed; a cut call is reported after the
    /// results with the tool note.
    RunTools,
    /// Cut off with nothing to execute: unfinished blocks were removed; the
    /// loop appends [`continuation_message`] and continues.
    Continue,
    /// Cut off with nothing to execute and the consecutive continuations
    /// used up: unfinished blocks were removed (nothing half-written runs or
    /// reaches history) and the turn ends with
    /// [`OutputLimitContinuation::exhausted_notice`].
    Exhausted,
}

/// What [`OutputLimitContinuation::inspect`] removed from a cut-off response.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Unfinished {
    /// Ids of the tool calls cut off mid-input: their UI cards get
    /// [`UNFINISHED_TOOL_CALL`].
    pub(crate) tool_ids: Vec<String>,
    /// The call the limit cut (the last unfinished one), by the name the
    /// model wrote; `None` when no call was cut. Only ever quoted through
    /// the plausible-name check.
    pub(crate) cut_tool: Option<String>,
}

/// Consecutive output-limit continuations, against the configured cap.
#[derive(Debug)]
pub(crate) struct OutputLimitContinuation {
    cap: u32,
    consecutive: u32,
}

impl OutputLimitContinuation {
    pub(crate) fn new(cap: u32) -> Self {
        Self { cap, consecutive: 0 }
    }

    pub(crate) fn cap(&self) -> u32 {
        self.cap
    }

    pub(crate) fn consecutive(&self) -> u32 {
        self.consecutive
    }

    /// Classify `response`. Unless it is [`OutputLimitStep::Unchanged`], the
    /// blocks the provider never finished (a tool call cut off mid-input,
    /// thinking without a signature) are removed from its content and
    /// reported in [`Unfinished`]. An unchanged response is left
    /// byte-identical.
    pub(crate) fn inspect(&self, response: &mut Value, cancelled: bool) -> (OutputLimitStep, Unfinished) {
        if self.cap == 0 || cancelled || !hit_output_limit(response) {
            return (OutputLimitStep::Unchanged, Unfinished::default());
        }
        let Some(content) = response.get_mut("content").and_then(Value::as_array_mut) else {
            return (OutputLimitStep::Unchanged, Unfinished::default());
        };
        let unfinished = Unfinished {
            tool_ids: content
                .iter()
                .filter(|b| is_unfinished_tool_use(b))
                .filter_map(|b| b["id"].as_str().map(str::to_string))
                .collect(),
            cut_tool: content
                .iter()
                .rev()
                .find(|b| is_unfinished_tool_use(b))
                .map(|b| b["name"].as_str().unwrap_or_default().to_string()),
        };
        content.retain(|b| !is_unfinished_tool_use(b) && !is_unsigned_thinking(b));
        let step = if content.iter().any(|b| b["type"].as_str() == Some("tool_use")) {
            OutputLimitStep::RunTools
        } else if self.consecutive < self.cap {
            OutputLimitStep::Continue
        } else {
            OutputLimitStep::Exhausted
        };
        (step, unfinished)
    }

    /// Spend one continuation; the visible notice for it.
    pub(crate) fn take(&mut self) -> String {
        self.consecutive = self.consecutive.saturating_add(1).min(self.cap);
        format!(
            "↻ response hit the output limit — continuing ({}/{})",
            self.consecutive, self.cap
        )
    }

    /// A response that was not cut off had its tool calls run: the run of
    /// consecutive cut-offs is over.
    pub(crate) fn reset(&mut self) {
        self.consecutive = 0;
    }

    /// Notice for a cut-off response once the consecutive continuations are
    /// spent; the turn ends.
    pub(crate) fn exhausted_notice(&self, cut_tool: Option<&str>) -> String {
        format!(
            "response hit the output limit{} — automatic continuations used up ({}/{}); the turn ends here. Send a message to continue.",
            cut_call_phrase(cut_tool),
            self.consecutive,
            self.cap
        )
    }

    /// Notice for a cut-off response that left nothing to keep on a route
    /// that cannot take a note straight after a user-role message; the turn
    /// ends.
    pub(crate) fn ended_notice(cut_tool: Option<&str>) -> String {
        let what = match cut_tool {
            Some(_) => cut_call_phrase(cut_tool),
            None => " before writing anything usable".to_string(),
        };
        format!("response hit the output limit{what}; the turn ends here. Send a message to continue.")
    }

    /// Notice for a cut-off response whose complete calls ran: the cut one
    /// did not, and the model is told so.
    pub(crate) fn cut_call_notice(cut_tool: &str) -> String {
        format!(
            "response hit the output limit{}; the complete calls ran",
            cut_call_phrase(Some(cut_tool))
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

    fn none() -> Unfinished {
        Unfinished::default()
    }

    fn cut_write(ids: &[&str]) -> Unfinished {
        Unfinished {
            tool_ids: ids.iter().map(|s| s.to_string()).collect(),
            cut_tool: Some("write".into()),
        }
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
        assert_eq!(limit.inspect(&mut response, false), (OutputLimitStep::Unchanged, none()));
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
            assert_eq!(limit.inspect(&mut response, false), (OutputLimitStep::Continue, none()));
            assert_eq!(response, original, "nothing unfinished to drop");
            assert_eq!(
                limit.take(),
                format!("↻ response hit the output limit — continuing ({n}/2)")
            );
        }
        let mut response = original.clone();
        assert_eq!(limit.inspect(&mut response, false), (OutputLimitStep::Exhausted, none()));
        assert_eq!(response, original, "nothing unfinished to drop");
        assert_eq!(
            limit.exhausted_notice(None),
            "response hit the output limit — automatic continuations used up (2/2); the turn ends here. Send a message to continue."
        );
    }

    /// The count is of consecutive cut-offs: a round that ran its tools
    /// without being cut off starts it again.
    #[test]
    fn reset_starts_the_consecutive_count_again() {
        let mut limit = OutputLimitContinuation::new(1);
        let original = cut(json!([{"type":"text","text":"I will now"}]));
        assert_eq!(limit.inspect(&mut original.clone(), false).0, OutputLimitStep::Continue);
        limit.take();
        assert_eq!(limit.inspect(&mut original.clone(), false).0, OutputLimitStep::Exhausted);
        limit.reset();
        assert_eq!(limit.consecutive(), 0);
        assert_eq!(limit.inspect(&mut original.clone(), false).0, OutputLimitStep::Continue);
        assert_eq!(limit.take(), "↻ response hit the output limit — continuing (1/1)");
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
        let (step, unfinished) = limit.inspect(&mut response, false);
        assert_eq!(step, OutputLimitStep::Continue);
        assert_eq!(unfinished, cut_write(&["t9"]));
        assert_eq!(
            response["content"],
            json!([
                {"type":"thinking","thinking":"signed","signature":"sig"},
                {"type":"text","text":"Writing the file now."},
            ])
        );
    }

    /// Past the cap, nothing half-written stays either: the partial call
    /// never reaches history, so it can never run as a parse-error round.
    #[test]
    fn exhausted_drops_unfinished_blocks_too() {
        let mut limit = OutputLimitContinuation::new(1);
        limit.take();
        let mut response = cut(json!([
            {"type":"text","text":"Writing the file now."},
            {"type":"thinking","thinking":"cut","signature":""},
            partial_tool("t9"),
        ]));
        let (step, unfinished) = limit.inspect(&mut response, false);
        assert_eq!(step, OutputLimitStep::Exhausted);
        assert_eq!(unfinished, cut_write(&["t9"]));
        assert_eq!(response["content"], json!([{"type":"text","text":"Writing the file now."}]));
        assert_eq!(
            limit.exhausted_notice(unfinished.cut_tool.as_deref()),
            "response hit the output limit while writing a `write` call (not run) — automatic continuations used up (1/1); the turn ends here. Send a message to continue."
        );
    }

    #[test]
    fn thinking_without_any_signature_field_is_dropped() {
        let limit = OutputLimitContinuation::new(1);
        let mut response = cut(json!([{"type":"thinking","thinking":"no sig field"}]));
        assert_eq!(limit.inspect(&mut response, false), (OutputLimitStep::Continue, none()));
        assert_eq!(response["content"], json!([]));
    }

    #[test]
    fn complete_tool_calls_run_and_only_the_partial_one_is_dropped() {
        let limit = OutputLimitContinuation::new(1);
        let complete = json!({"type":"tool_use","id":"t1","name":"bash","input":{"command":"ls"}});
        let mut response = cut(json!([complete.clone(), partial_tool("t2")]));
        let (step, unfinished) = limit.inspect(&mut response, false);
        assert_eq!(step, OutputLimitStep::RunTools);
        assert_eq!(unfinished, cut_write(&["t2"]));
        assert_eq!(response["content"], json!([complete]));
    }

    #[test]
    fn complete_tool_calls_run_even_with_no_continuations_left() {
        let mut limit = OutputLimitContinuation::new(1);
        limit.take();
        let complete = json!({"type":"tool_use","id":"t1","name":"bash","input":{"command":"ls"}});
        let mut response = cut(json!([complete.clone()]));
        assert_eq!(limit.inspect(&mut response, false), (OutputLimitStep::RunTools, none()));
        assert_eq!(response["content"], json!([complete]));
    }

    /// The note names the call the limit cut (the last one); a name that
    /// isn't a plausible tool name reads "a tool call". Text or thinking
    /// only gets the plain note.
    #[test]
    fn the_note_names_the_cut_call_or_falls_back_to_a_generic_one() {
        let limit = OutputLimitContinuation::new(1);
        let mut response = cut(json!([
            {"type":"tool_use","id":"t1","name":"read","input":{"__parse_error":"EOF"}},
            {"type":"tool_use","id":"t2","name":"edit","input":{"__parse_error":"EOF"}},
        ]));
        let (_, unfinished) = limit.inspect(&mut response, false);
        assert_eq!(unfinished.tool_ids, vec!["t1".to_string(), "t2".to_string()]);
        assert_eq!(unfinished.cut_tool.as_deref(), Some("edit"));
        let note = continuation_message(unfinished.cut_tool.as_deref());
        assert_eq!(note["role"], "user");
        let text = note["content"].as_str().unwrap();
        assert!(text.contains("while writing a `edit` call; that call was incomplete and was NOT run"), "{text}");
        assert!(host_note::is_output_limit_note(&note));

        for bad in ["rm -rf /`", "", "a\nb"] {
            let mut response = cut(json!([
                {"type":"tool_use","id":"t1","name":bad,"input":{"__parse_error":"EOF"}},
            ]));
            let (_, unfinished) = limit.inspect(&mut response, false);
            let note = continuation_message(unfinished.cut_tool.as_deref());
            let text = note["content"].as_str().unwrap();
            assert!(text.contains("while writing a tool call; that call was incomplete"), "{text}");
            assert!(!text.contains('`'), "{text}");
            assert!(host_note::is_output_limit_note(&note));
            assert!(OutputLimitContinuation::ended_notice(unfinished.cut_tool.as_deref())
                .contains("while writing a tool call (not run)"));
        }

        let plain = continuation_message(None);
        assert_eq!(plain["content"], host_note::OUTPUT_LIMIT_NOTE);
        assert!(host_note::is_output_limit_note(&plain));
        assert_eq!(
            OutputLimitContinuation::ended_notice(None),
            "response hit the output limit before writing anything usable; the turn ends here. Send a message to continue."
        );
    }
}
