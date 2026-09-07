//! Bounded, process-local feedback for one caller-owned foreground turn.
//!
//! The caller must filter ownership, call `begin_turn` before observing, and call
//! `finish` only after success. Session/history, agent, and partial tool events
//! are never inspected. Starting again abandons any unfinished observation.
//!
//! This is a byte-fingerprint heuristic, not proof of progress or equality:
//! `DefaultHasher` collisions can produce false repeats, and its fingerprints
//! are neither cryptographic nor stable across Rust versions. Only bounded
//! model/effort metadata and private hash state survive calls; no response/input
//! content is retained, logged, exported, or read from storage.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::io::{self, Write};

use serde_json::Value;
use synaps_cli::{LlmEvent, StreamEvent};

const RECENT_SIGNATURES: usize = 4;
const MAX_BYTES: usize = 1024 * 1024;
const MAX_TOOL_EVENTS: usize = 1024;
const MAX_JSON_NODES: usize = 16 * 1024;
const MAX_JSON_DEPTH: usize = 64;
const MAX_SCOPE_BYTES: usize = 4096;

#[derive(Default)]
pub(super) struct Tracker {
    // Exact, bounded metadata: do not normalize or fingerprint the scope key.
    scope: Option<(Box<str>, Box<str>)>,
    recent: [Option<u64>; RECENT_SIGNATURES],
    next: usize,
    current: Option<Turn>,
}

impl Tracker {
    /// Begin an owned turn, discarding any unfinished (failed/interrupted) turn.
    /// Changing either exact scope string clears the comparison window, even
    /// when the new turn is subsequently abandoned. Oversized metadata is unknown.
    pub(super) fn begin_turn(&mut self, model: &str, effort: &str) {
        self.current = None;
        if model.len() > MAX_SCOPE_BYTES || effort.len() > MAX_SCOPE_BYTES - model.len() {
            self.scope = None;
            self.clear_recent();
            self.current = Some(Turn {
                unknown: true,
                ..Turn::default()
            });
            return;
        }

        let same_scope = self
            .scope
            .as_ref()
            .is_some_and(|(m, e)| m.as_ref() == model && e.as_ref() == effort);
        if !same_scope {
            self.clear_recent();
            self.scope = Some((model.into(), effort.into()));
        }
        self.current = Some(Turn::default());
    }

    /// Observe only final LLM output; everything outside an active turn is ignored.
    pub(super) fn observe(&mut self, event: &StreamEvent) {
        let Some(turn) = self.current.as_mut() else {
            return;
        };
        if turn.unknown {
            return;
        }
        if let StreamEvent::Llm(event) = event {
            if turn.observe(event).is_none() {
                // Never compare a truncated prefix, nor recover within this turn.
                turn.unknown = true;
            }
        }
    }

    /// Commit a successful turn once. Repeated calls without `begin_turn` return
    /// `unknown` and cannot insert another signature. Empty/unknown turns do not
    /// enter or clear the ring; their non-repeated labels break a caller's streak.
    pub(super) fn finish(&mut self) -> &'static str {
        let Some(mut turn) = self.current.take() else {
            return "unknown";
        };
        if turn.unknown {
            return "unknown";
        }

        let signature = if turn.tool_events != 0 {
            // Any final tool event, including a standalone result, is meaningful.
            // Accompanying assistant prose is excluded from this signature.
            turn.tools.write_u64(turn.tool_events as u64);
            turn.tools.finish()
        } else if turn.meaningful_text {
            // One length for the entire byte stream, never one per chunk.
            turn.text.write_u64(turn.text_bytes as u64);
            turn.text.finish()
        } else {
            return "empty";
        };

        let repeated = self.recent.contains(&Some(signature));
        self.recent[self.next] = Some(signature);
        self.next = (self.next + 1) % RECENT_SIGNATURES;
        if repeated {
            "repeated"
        } else {
            "changed"
        }
    }

    fn clear_recent(&mut self) {
        self.recent = [None; RECENT_SIGNATURES];
        self.next = 0;
    }
}

#[derive(Clone)]
struct Turn {
    text: DefaultHasher,
    tools: DefaultHasher,
    text_bytes: usize,
    meaningful_text: bool,
    tool_events: usize,
    bytes_left: usize,
    json_nodes_left: usize,
    unknown: bool,
    response_baseline: Option<Box<Turn>>,
}

impl Default for Turn {
    fn default() -> Self {
        let mut text = DefaultHasher::new();
        let mut tools = DefaultHasher::new();
        // Domain separation for text-only and tool-based signatures.
        text.write_u8(1);
        tools.write_u8(2);
        Self {
            text,
            tools,
            text_bytes: 0,
            meaningful_text: false,
            tool_events: 0,
            bytes_left: MAX_BYTES,
            json_nodes_left: MAX_JSON_NODES,
            unknown: false,
            response_baseline: None,
        }
    }
}

impl Turn {
    fn observe(&mut self, event: &LlmEvent) -> Option<()> {
        match event {
            LlmEvent::ResponseStart => {
                self.response_baseline = None;
                self.response_baseline = Some(Box::new(self.clone()));
            }
            LlmEvent::ResponseReset => {
                if let Some(baseline) = self.response_baseline.take() {
                    *self = *baseline;
                }
            }
            LlmEvent::Text(text) => {
                // Count all final text against the budget, even when tools make
                // it irrelevant to equality. Ignored event types cost no budget.
                reserve(&mut self.bytes_left, text.len())?;
                self.text_bytes += text.len();
                self.meaningful_text |= !text.trim().is_empty();
                // DefaultHasher's incremental byte writes concatenate chunks;
                // Hash::hash(str) would instead frame each chunk separately.
                self.text.write(text.as_bytes());
            }
            LlmEvent::ToolUse {
                tool_name, input, ..
            } => {
                self.tool_event()?;
                self.tools.write_u8(1); // Call: framed name + framed JSON digest.
                self.field(tool_name)?;

                // Check borrowed shape/lengths BEFORE sorting or serializing.
                // This bounds recursion, allocation, string scanning, and total
                // JSON traversal across the turn, not just the bytes written.
                let mut lower_bound_left = self.bytes_left;
                validate_json(input, 0, &mut lower_bound_left, &mut self.json_nodes_left)?;
                let mut writer = HashWriter::new(self.bytes_left);
                write_canonical_json(&mut writer, input).ok()?;
                reserve(&mut self.bytes_left, writer.written)?;
                self.tools.write_u64(writer.written as u64);
                self.tools.write_u64(writer.hash.finish());
            }
            LlmEvent::ToolResult { result, .. } => {
                self.tool_event()?;
                self.tools.write_u8(2); // Result, in observed order; no ID lookup.
                self.field(result)?;
            }
            LlmEvent::Thinking(_)
            | LlmEvent::ToolUseStart { .. }
            | LlmEvent::ToolUseDelta { .. }
            | LlmEvent::ToolResultDelta { .. } => {}
        }
        Some(())
    }

    fn tool_event(&mut self) -> Option<()> {
        if self.tool_events == MAX_TOOL_EVENTS {
            return None;
        }
        self.tool_events += 1;
        Some(())
    }

    fn field(&mut self, value: &str) -> Option<()> {
        reserve(&mut self.bytes_left, value.len())?;
        self.tools.write_u64(value.len() as u64);
        self.tools.write(value.as_bytes());
        Some(())
    }
}

fn reserve(remaining: &mut usize, amount: usize) -> Option<()> {
    *remaining = remaining.checked_sub(amount)?;
    Some(())
}

/// A cheap lower bound on encoded bytes, plus strict node/depth bounds. No clone
/// of JSON or allocation proportional to unchecked input size. A node requires
/// at least one encoded byte; raw string/key lengths are further lower bounds.
fn validate_json(
    value: &Value,
    depth: usize,
    bytes_left: &mut usize,
    nodes_left: &mut usize,
) -> Option<()> {
    if depth > MAX_JSON_DEPTH {
        return None;
    }
    reserve(nodes_left, 1)?;
    reserve(bytes_left, 1)?;
    match value {
        Value::String(text) => reserve(bytes_left, text.len())?,
        Value::Array(values) => {
            if values.len() > *nodes_left {
                return None;
            }
            for value in values {
                validate_json(value, depth + 1, bytes_left, nodes_left)?;
            }
        }
        Value::Object(values) => {
            if values.len() > *nodes_left {
                return None;
            }
            for (key, value) in values {
                reserve(bytes_left, key.len())?;
                validate_json(value, depth + 1, bytes_left, nodes_left)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Some(())
}

/// In-memory, all-or-error writer: an overflow never yields a usable prefix.
/// Actual serialized bytes (including escaping) consume the remaining budget.
struct HashWriter {
    hash: DefaultHasher,
    left: usize,
    written: usize,
}

impl HashWriter {
    fn new(left: usize) -> Self {
        Self {
            hash: DefaultHasher::new(),
            left,
            written: 0,
        }
    }
}

impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        reserve(&mut self.left, bytes.len())
            .ok_or_else(|| io::Error::other("feedback byte budget exceeded"))?;
        self.hash.write(bytes);
        self.written += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Compact canonical JSON: recursively sorted object keys, ordered arrays,
/// serde string escaping and serde_json's number spelling (not numeric semantic
/// normalization). Call ONLY after validate_json. Sorting holds borrowed entries
/// bounded by MAX_JSON_NODES across all active recursion frames, never raw copies.
fn write_canonical_json(writer: &mut HashWriter, value: &Value) -> io::Result<()> {
    match value {
        Value::Null => writer.write_all(b"null"),
        Value::Bool(true) => writer.write_all(b"true"),
        Value::Bool(false) => writer.write_all(b"false"),
        // Display is serde_json's JSON number representation and writes directly
        // to our budget. Unlike to_string(), it cannot allocate a huge temporary
        // if another dependency enables serde_json's arbitrary_precision feature.
        Value::Number(number) => write!(writer, "{number}"),
        Value::String(text) => write_json_string(writer, text),
        Value::Array(values) => {
            writer.write_all(b"[")?;
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    writer.write_all(b",")?;
                }
                write_canonical_json(writer, value)?;
            }
            writer.write_all(b"]")
        }
        Value::Object(values) => {
            // Do not depend on whether serde_json's preserve_order is enabled.
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
            writer.write_all(b"{")?;
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index != 0 {
                    writer.write_all(b",")?;
                }
                write_json_string(writer, key)?;
                writer.write_all(b":")?;
                write_canonical_json(writer, value)?;
            }
            writer.write_all(b"}")
        }
    }
}

fn write_json_string(writer: &mut HashWriter, text: &str) -> io::Result<()> {
    // text.len() has already been bounded, so serde cannot scan a giant string
    // before reaching the writer's budget check. Never retain serializer errors.
    serde_json::to_writer(writer, text)
        .map_err(|_| io::Error::other("feedback JSON serialization failed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use synaps_cli::{AgentEvent, SessionEvent};

    fn text(value: &str) -> StreamEvent {
        StreamEvent::Llm(LlmEvent::Text(value.into()))
    }

    fn call(id: &str, name: &str, input: Value) -> StreamEvent {
        StreamEvent::Llm(LlmEvent::ToolUse {
            tool_name: name.into(),
            tool_id: id.into(),
            input,
        })
    }

    fn result(id: &str, output: &str) -> StreamEvent {
        StreamEvent::Llm(LlmEvent::ToolResult {
            tool_id: id.into(),
            result: output.into(),
        })
    }

    #[test]
    fn retry_feedback_ignores_discarded_attempt_but_keeps_earlier_tools() {
        let mut tracker = Tracker::default();
        let previous = call("p", "read", json!({"path":"before"}));
        let after = call("q", "write", json!({"path":"after"}));
        assert_eq!(
            run(&mut tracker, &[previous.clone(), after.clone()]),
            "changed"
        );
        assert_eq!(
            run(
                &mut tracker,
                &[
                    previous,
                    StreamEvent::Llm(LlmEvent::ResponseStart),
                    call("bad", "bash", json!({"command":"not executed"})),
                    text("discard me"),
                    StreamEvent::Llm(LlmEvent::ResponseReset),
                    StreamEvent::Llm(LlmEvent::ResponseStart),
                    after,
                ]
            ),
            "repeated"
        );
    }

    fn run(tracker: &mut Tracker, events: &[StreamEvent]) -> &'static str {
        tracker.begin_turn("model", "effort");
        for event in events {
            tracker.observe(event);
        }
        tracker.finish()
    }

    fn scoped(tracker: &mut Tracker, model: &str, effort: &str) -> &'static str {
        tracker.begin_turn(model, effort);
        tracker.observe(&text("same output"));
        tracker.finish()
    }

    #[test]
    fn first_changed_then_repeated_and_changed() {
        let mut tracker = Tracker::default();
        assert_eq!(run(&mut tracker, &[text("A")]), "changed");
        assert_eq!(run(&mut tracker, &[text("A")]), "repeated");
        assert_eq!(run(&mut tracker, &[text("B")]), "changed");
        assert_eq!(run(&mut tracker, &[text("B")]), "repeated");
    }

    #[test]
    fn recognizes_a_b_a_and_keeps_exactly_four_successful_signatures() {
        let mut tracker = Tracker::default();
        for value in ["A", "B"] {
            assert_eq!(run(&mut tracker, &[text(value)]), "changed");
        }
        assert_eq!(run(&mut tracker, &[text("A")]), "repeated");

        let mut tracker = Tracker::default();
        for value in ["A", "B", "C", "D"] {
            assert_eq!(run(&mut tracker, &[text(value)]), "changed");
        }
        assert_eq!(run(&mut tracker, &[text("A")]), "repeated");
        assert_eq!(run(&mut tracker, &[text("E")]), "changed");
        assert_eq!(run(&mut tracker, &[text("B")]), "changed");
    }

    #[test]
    fn ring_counts_turns_not_distinct_signatures() {
        let mut tracker = Tracker::default();
        assert_eq!(run(&mut tracker, &[text("A")]), "changed");
        assert_eq!(run(&mut tracker, &[text("B")]), "changed");
        for _ in 0..3 {
            assert_eq!(run(&mut tracker, &[text("B")]), "repeated");
        }
        assert_eq!(run(&mut tracker, &[text("A")]), "changed");
    }

    #[test]
    fn arbitrary_tool_ids_and_accompanying_prose_are_ignored() {
        let mut tracker = Tracker::default();
        assert_eq!(
            run(
                &mut tracker,
                &[
                    text("before"),
                    call("old-id", "read", json!({"path": "a"})),
                    text("between"),
                    result("old-id", "contents"),
                    text("after"),
                ],
            ),
            "changed"
        );
        assert_eq!(
            run(
                &mut tracker,
                &[
                    call("new-id", "read", json!({"path": "a"})),
                    result("unrelated-result-id", "contents"),
                    text("completely different explanation"),
                ],
            ),
            "repeated"
        );
        assert_eq!(
            run(
                &mut tracker,
                &[
                    call("", "read", json!({"path": "a"})),
                    result("", "contents"),
                ],
            ),
            "repeated"
        );
    }

    #[test]
    fn changed_tool_names_inputs_or_results_are_changed() {
        let mut tracker = Tracker::default();
        for (name, input, output) in [
            ("read", json!({"path": "a"}), "one"),
            ("read", json!({"path": "b"}), "one"),
            ("read", json!({"path": "b"}), "two"),
            ("other", json!({"path": "b"}), "two"),
        ] {
            assert_eq!(
                run(
                    &mut tracker,
                    &[call("id", name, input), result("id", output)]
                ),
                "changed"
            );
        }
    }

    #[test]
    fn unique_results_never_look_repeated_just_because_calls_repeat() {
        let mut tracker = Tracker::default();
        for index in 0..12 {
            assert_eq!(
                run(
                    &mut tracker,
                    &[
                        call("id", "poll", json!({})),
                        result("id", &format!("output {index}")),
                    ],
                ),
                "changed"
            );
        }
    }

    #[test]
    fn canonicalizes_object_keys_recursively_but_not_array_order_or_types() {
        let mut tracker = Tracker::default();
        let a: Value = serde_json::from_str(r#"{"z":[{"b":2,"a":1}],"a":true}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":true,"z":[{"a":1,"b":2}]}"#).unwrap();
        assert_eq!(run(&mut tracker, &[call("a", "tool", a)]), "changed");
        assert_eq!(run(&mut tracker, &[call("b", "tool", b)]), "repeated");
        for input in [
            json!([1, 2]),
            json!([2, 1]),
            json!(["2", "1"]),
            json!("[2,1]"),
        ] {
            assert_eq!(run(&mut tracker, &[call("id", "tool", input)]), "changed");
        }
    }

    #[test]
    fn tool_event_order_is_significant() {
        let mut tracker = Tracker::default();
        let a = call("a", "one", json!(null));
        let b = call("b", "two", json!(null));
        let x = result("a", "x");
        let y = result("b", "y");
        assert_eq!(
            run(&mut tracker, &[a.clone(), b.clone(), x.clone(), y.clone()]),
            "changed"
        );
        assert_eq!(
            run(&mut tracker, &[b.clone(), a.clone(), x.clone(), y.clone()]),
            "changed"
        );
        assert_eq!(run(&mut tracker, &[b, a, y, x]), "changed");
    }

    #[test]
    fn type_tags_and_field_lengths_prevent_concatenation_aliases() {
        let mut tracker = Tracker::default();
        assert_eq!(run(&mut tracker, &[text("abc")]), "changed");
        assert_eq!(run(&mut tracker, &[result("", "abc")]), "changed");
        assert_eq!(
            run(&mut tracker, &[result("", "ab"), result("", "c")]),
            "changed"
        );
        assert_eq!(
            run(&mut tracker, &[result("", "a"), result("", "bc")]),
            "changed"
        );
        assert_eq!(run(&mut tracker, &[call("", "ab", json!("c"))]), "changed");
        assert_eq!(run(&mut tracker, &[call("", "a", json!("bc"))]), "changed");
    }

    #[test]
    fn text_chunk_boundaries_do_not_affect_signature() {
        let mut tracker = Tracker::default();
        let whole = " \r\nCafé 🦀\n";
        assert_eq!(run(&mut tracker, &[text(whole)]), "changed");
        for split in whole
            .char_indices()
            .map(|(index, _)| index)
            .chain([whole.len()])
        {
            assert_eq!(
                run(
                    &mut tracker,
                    &[text(&whole[..split]), text(""), text(&whole[split..])]
                ),
                "repeated"
            );
        }
        let chunks: Vec<_> = whole.chars().map(|ch| text(&ch.to_string())).collect();
        assert_eq!(run(&mut tracker, &chunks), "repeated");
    }

    #[test]
    fn nonempty_text_uses_exact_bytes_not_trimmed_or_normalized_bytes() {
        let mut tracker = Tracker::default();
        for value in ["answer", " answer", "answer ", "Answer", "answer\n"] {
            assert_eq!(run(&mut tracker, &[text(value)]), "changed");
        }
    }

    #[test]
    fn thinking_notices_history_agent_events_and_deltas_are_ignored() {
        let mut tracker = Tracker::default();
        // Synthetic history is supplied but never traversed by the tracker.
        let ignored = [
            StreamEvent::Llm(LlmEvent::Thinking("private reasoning".into())),
            StreamEvent::Llm(LlmEvent::ToolUseStart {
                tool_name: "unfinished".into(),
                tool_id: "id".into(),
            }),
            StreamEvent::Llm(LlmEvent::ToolUseDelta {
                tool_id: "id".into(),
                delta: "{incomplete".into(),
            }),
            StreamEvent::Llm(LlmEvent::ToolResultDelta {
                tool_id: "id".into(),
                delta: "partial output".into(),
            }),
            StreamEvent::Session(SessionEvent::Notice("notice".into())),
            StreamEvent::Session(SessionEvent::MessageHistory(vec![std::sync::Arc::new(
                json!({
                    "role": "user", "content": "user goal/history is not output"
                }),
            )])),
            StreamEvent::Agent(AgentEvent::SteeringDelivered {
                message: "new goal".into(),
            }),
            StreamEvent::Session(SessionEvent::Done),
        ];
        assert_eq!(run(&mut tracker, &ignored), "empty");
        assert_eq!(run(&mut tracker, &[text("answer")]), "changed");
        tracker.begin_turn("model", "effort");
        tracker.observe(&text("ans"));
        for event in &ignored {
            tracker.observe(event);
        }
        tracker.observe(&text("wer"));
        assert_eq!(tracker.finish(), "repeated");
    }

    #[test]
    fn empty_or_whitespace_only_output_never_becomes_repeated() {
        let mut tracker = Tracker::default();
        for _ in 0..6 {
            assert_eq!(run(&mut tracker, &[]), "empty");
            assert_eq!(
                run(&mut tracker, &[text(""), text(" \t\r\n\u{2003}")]),
                "empty"
            );
            assert_eq!(
                run(
                    &mut tracker,
                    &[StreamEvent::Llm(LlmEvent::Thinking("alone".into()))]
                ),
                "empty"
            );
        }
        assert_eq!(run(&mut tracker, &[text("A")]), "changed");
        for _ in 0..6 {
            assert_eq!(run(&mut tracker, &[]), "empty");
        }
        assert_eq!(run(&mut tracker, &[text("A")]), "repeated");
    }

    #[test]
    fn final_tools_are_meaningful_even_with_empty_output() {
        let mut tracker = Tracker::default();
        assert_eq!(
            run(&mut tracker, &[call("", "tool", json!(null))]),
            "changed"
        );
        assert_eq!(
            run(&mut tracker, &[call("new", "tool", json!(null))]),
            "repeated"
        );
        assert_eq!(run(&mut tracker, &[result("", "")]), "changed");
        assert_eq!(run(&mut tracker, &[result("new", "")]), "repeated");
    }

    #[test]
    fn exact_model_and_effort_changes_reset_the_ring() {
        let mut tracker = Tracker::default();
        assert_eq!(scoped(&mut tracker, "model", "high"), "changed");
        assert_eq!(scoped(&mut tracker, "model", "high"), "repeated");
        for (model, effort) in [
            ("Model", "high"),
            ("model", "high"),
            ("model", "HIGH"),
            ("model", "high "),
            ("model", "high"),
            ("ab", "c"),
            ("a", "bc"),
        ] {
            assert_eq!(scoped(&mut tracker, model, effort), "changed");
        }
    }

    #[test]
    fn oversized_scope_is_unknown_without_retaining_unbounded_metadata() {
        let mut tracker = Tracker::default();
        assert_eq!(scoped(&mut tracker, "model", "effort"), "changed");
        assert_eq!(
            scoped(&mut tracker, &"m".repeat(MAX_SCOPE_BYTES + 1), ""),
            "unknown"
        );
        assert_eq!(
            scoped(&mut tracker, "m", &"e".repeat(MAX_SCOPE_BYTES)),
            "unknown"
        );
        assert_eq!(scoped(&mut tracker, "model", "effort"), "changed");
    }

    #[test]
    fn finish_and_observe_outside_a_turn_do_not_account_twice() {
        let mut tracker = Tracker::default();
        tracker.observe(&text("unowned"));
        assert_eq!(tracker.finish(), "unknown");
        assert_eq!(run(&mut tracker, &[text("A")]), "changed");
        assert_eq!(run(&mut tracker, &[text("B")]), "changed");
        for _ in 0..8 {
            tracker.observe(&text("unowned"));
            assert_eq!(tracker.finish(), "unknown");
        }
        assert_eq!(run(&mut tracker, &[text("C")]), "changed");
        assert_eq!(run(&mut tracker, &[text("D")]), "changed");
        assert_eq!(run(&mut tracker, &[text("A")]), "repeated");
        assert_eq!(run(&mut tracker, &[text("unowned")]), "changed");
    }

    #[test]
    fn interrupted_turn_is_discarded_without_losing_completed_signatures() {
        let mut tracker = Tracker::default();
        assert_eq!(run(&mut tracker, &[text("completed")]), "changed");
        tracker.begin_turn("model", "effort");
        tracker.observe(&text("interrupted"));
        // No finish on failure; the next begin discards that observation.
        assert_eq!(run(&mut tracker, &[text("completed")]), "repeated");
        assert_eq!(run(&mut tracker, &[text("interrupted")]), "changed");
        tracker.begin_turn("different model", "effort");
        assert_eq!(run(&mut tracker, &[text("completed")]), "changed");
    }

    #[test]
    fn text_budget_boundary_and_overflow_do_not_compare_truncated_prefixes() {
        let mut tracker = Tracker::default();
        let at_limit = text(&"x".repeat(MAX_BYTES));
        assert_eq!(
            run(&mut tracker, std::slice::from_ref(&at_limit)),
            "changed"
        );
        assert_eq!(
            run(&mut tracker, std::slice::from_ref(&at_limit)),
            "repeated"
        );
        for _ in 0..6 {
            // This non-repeated label breaks a prior repeated streak. Neither
            // overflow nor its otherwise matching prefix enters the ring.
            assert_eq!(run(&mut tracker, &[at_limit.clone(), text("x")]), "unknown");
        }
        assert_eq!(
            run(&mut tracker, std::slice::from_ref(&at_limit)),
            "repeated"
        );
        assert_eq!(
            run(&mut tracker, &[text(&"x".repeat(MAX_BYTES + 1))]),
            "unknown"
        );
    }

    #[test]
    fn tool_event_limit_is_inclusive_and_overflow_is_sticky() {
        let mut tracker = Tracker::default();
        for expected in ["changed", "repeated"] {
            tracker.begin_turn("model", "effort");
            for _ in 0..MAX_TOOL_EVENTS {
                tracker.observe(&result("ignored", ""));
            }
            assert_eq!(tracker.finish(), expected);
        }
        tracker.begin_turn("model", "effort");
        for _ in 0..=MAX_TOOL_EVENTS {
            tracker.observe(&result("ignored", ""));
        }
        tracker.observe(&text("cannot recover"));
        assert_eq!(tracker.finish(), "unknown");
        assert_eq!(run(&mut tracker, &[text("cannot recover")]), "changed");
    }

    #[test]
    fn byte_budget_is_shared_by_text_names_inputs_and_results() {
        let mut tracker = Tracker::default();
        let events = [
            text(&"x".repeat(MAX_BYTES - 7)),
            call("id", "t", json!(null)), // 1 byte name + 4 bytes JSON.
            result("id", "ok"),           // 2 bytes result: exactly the limit.
        ];
        assert_eq!(run(&mut tracker, &events), "changed");
        tracker.begin_turn("model", "effort");
        for event in &events {
            tracker.observe(event);
        }
        tracker.observe(&text("!"));
        assert_eq!(tracker.finish(), "unknown");
        assert_eq!(
            run(&mut tracker, &[result("", &"x".repeat(MAX_BYTES + 1))]),
            "unknown"
        );
        assert_eq!(
            run(
                &mut tracker,
                &[call("", &"x".repeat(MAX_BYTES + 1), json!(null))]
            ),
            "unknown"
        );
    }

    #[test]
    fn ignored_content_does_not_consume_the_budget() {
        let mut tracker = Tracker::default();
        let ignored = StreamEvent::Llm(LlmEvent::Thinking("x".repeat(MAX_BYTES + 1)));
        assert_eq!(run(&mut tracker, &[ignored, text("answer")]), "changed");
        assert_eq!(run(&mut tracker, &[text("answer")]), "repeated");
    }

    #[test]
    fn oversized_json_and_escaped_serialization_overflow_are_unknown() {
        let mut tracker = Tracker::default();
        for input in [
            Value::String("x".repeat(MAX_BYTES + 1)),
            // Raw text passes preflight, but JSON escaping exceeds the budget.
            Value::String("\n".repeat(MAX_BYTES / 2)),
        ] {
            assert_eq!(run(&mut tracker, &[call("", "tool", input)]), "unknown");
        }
        assert_eq!(
            run(&mut tracker, &[call("", "tool", json!("small"))]),
            "changed"
        );
        assert_eq!(
            run(&mut tracker, &[call("", "tool", json!("small"))]),
            "repeated"
        );
    }

    #[test]
    fn json_depth_and_total_node_limits_fail_closed() {
        let mut tracker = Tracker::default();
        let mut nested = Value::Null;
        for _ in 0..MAX_JSON_DEPTH {
            nested = Value::Array(vec![nested]);
        }
        assert_eq!(
            run(&mut tracker, &[call("", "tool", nested.clone())]),
            "changed"
        );
        assert_eq!(
            run(
                &mut tracker,
                &[call("", "tool", Value::Array(vec![nested]))]
            ),
            "unknown"
        );
        assert_eq!(
            run(
                &mut tracker,
                &[call(
                    "",
                    "tool",
                    Value::Array(vec![Value::Null; MAX_JSON_NODES])
                )]
            ),
            "unknown"
        );
        let half = call(
            "",
            "tool",
            Value::Array(vec![Value::Null; MAX_JSON_NODES / 2]),
        );
        assert_eq!(run(&mut tracker, &[half.clone(), half]), "unknown");
    }
}
