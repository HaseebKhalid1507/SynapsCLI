//! `before_message` inject for one turn (#414).
//!
//! The hook runs once for each new user message of a turn (the prompt, a
//! steer, a pushed event), right before that message is first sent. What it
//! returns is split by part:
//!
//! - **saved** parts become fenced text blocks on that message, in durable
//!   history, before any request carries it. From then on they are ordinary
//!   history (every later request, the session, rollover archives,
//!   compaction) and their bytes never change.
//! - **forget** parts join the turn's forget set. The set is rendered once
//!   into one turn-only block, attached at the tail of every request of the
//!   turn (`stream::attach_turn_context`), and never written anywhere
//!   durable. A marker per extension is saved on the message instead, at the
//!   same moment as the saved parts, so no durable byte changes later.
//! - A saved part over [`SAVED_PART_CAP`] is handled as forget (warn log).
//!
//! Framing and detectors: `agent_core::core::extension_context`.

use std::sync::Arc;

use agent_core::core::extension_context as ec;
use serde_json::{json, Value};

use crate::extensions::hooks::events::InjectPart;
use crate::SharedMessage;

/// Largest saved part (bytes); a bigger one is shown for the turn only.
pub(super) const SAVED_PART_CAP: usize = 16 * 1024;

/// The turn's forget set and its rendered tail block.
#[derive(Debug, Default)]
pub(super) struct TurnInjection {
    forget: Vec<String>,
    tail: Option<String>,
}

impl TurnInjection {
    /// The turn-only block for this turn's requests (guard framing, no
    /// leading separator), if any forget content arrived.
    pub(super) fn tail(&self) -> Option<&str> {
        self.tail.as_deref()
    }

    /// Apply one hook run's `parts` to `message`, the user message the hook
    /// ran for, which no request has carried yet. Saved parts, then one
    /// marker per extension that gave forget content, are appended to it;
    /// forget parts join the turn set in arrival order (earlier ones stay).
    /// The message is marked hooked (`ec::HOOKED_KEY`) with the number of
    /// blocks added, also when there were none (a `Continue`, a timeout):
    /// it is never hooked again.
    pub(super) fn absorb(&mut self, message: &mut SharedMessage, parts: Vec<InjectPart>) {
        let mut saved = Vec::new();
        let mut marked: Vec<String> = Vec::new();
        let mut forgot = false;
        for part in parts {
            let source = part.source.as_deref().unwrap_or("extension");
            if part.content.trim().is_empty() {
                tracing::warn!(extension = source, "before_message inject part is empty; skipped");
                continue;
            }
            let over_cap = !part.forget && part.content.len() > SAVED_PART_CAP;
            if over_cap {
                tracing::warn!(
                    extension = source,
                    bytes = part.content.len(),
                    cap = SAVED_PART_CAP,
                    "before_message saved inject part over the cap: shown for this turn only, not saved"
                );
            }
            if part.forget || over_cap {
                if !marked.iter().any(|s| s == source) {
                    marked.push(source.to_string());
                }
                self.forget.push(part.content);
                forgot = true;
            } else {
                saved.push(ec::saved_block_text(source, &part.content));
            }
        }
        let blocks: Vec<String> = saved
            .into_iter()
            .chain(marked.iter().map(|s| ec::marker_text(s)))
            .collect();
        let added = if blocks.is_empty() { 0 } else { append_text_blocks(message, blocks) };
        ec::set_hooked(Arc::make_mut(message), added);
        if forgot {
            self.tail = Some(ec::guard_turn(&self.forget.join("\n\n")));
        }
    }
}

/// The user's text the hook is given for `message`, when the hook should run
/// for it: a user message with text of its own (string content, or its
/// first text block) that was never hooked (`ec::HOOKED_KEY`). One that was
/// is never hooked again, whatever the hook returned then: a retried,
/// resent or resumed turn would otherwise add to bytes already sent. Its
/// forget content is not shown again either; the marker stays.
///
/// Host-made user messages (interruption markers, tool-in-flight notes, the
/// rollover continuation marker) are never targets: saving onto them would
/// turn their string content into blocks their detectors no longer match.
// Deviation (#414): before, the hook fired for any newest user text on every
// round, host notes included; those got only request-tail context.
pub(super) fn hook_target_text(message: &Value) -> Option<String> {
    if message["role"].as_str() != Some("user")
        || message.get("_synaps_context").is_some()
        || ec::hooked(message).is_some()
        || agent_core::core::host_note::is_tool_in_flight_note(message)
    {
        return None;
    }
    match &message["content"] {
        Value::String(text) if crate::engine::interrupt::is_interruption_marker(text) => None,
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => blocks
            .iter()
            .find(|b| b["type"].as_str() == Some("text"))
            .and_then(|b| b["text"].as_str())
            .map(str::to_owned),
        _ => None,
    }
}

/// Append `texts` as text blocks, coercing string content to blocks first
/// (an empty string to none: an empty text block is an Anthropic 400).
/// Returns how many were appended.
fn append_text_blocks(message: &mut SharedMessage, texts: Vec<String>) -> usize {
    let msg = Arc::make_mut(message);
    if let Some(text) = msg["content"].as_str().map(str::to_owned) {
        msg["content"] = if text.is_empty() {
            json!([])
        } else {
            json!([{"type": "text", "text": text}])
        };
    }
    match msg["content"].as_array_mut() {
        Some(blocks) => {
            let n = texts.len();
            blocks.extend(texts.into_iter().map(|text| json!({"type": "text", "text": text})));
            n
        }
        None => {
            tracing::warn!(
                "before_message saved context dropped: user message content is neither string nor array"
            );
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(content: Value) -> SharedMessage {
        Arc::new(json!({"role": "user", "content": content}))
    }

    #[test]
    fn saved_parts_then_one_marker_per_extension() {
        let mut msg = user(json!("hi"));
        let mut turn = TurnInjection::default();
        turn.absorb(
            &mut msg,
            vec![
                InjectPart::saved("S1").from_source("a"),
                InjectPart::forget("F1").from_source("a"),
                InjectPart::forget("F2").from_source("a"),
                InjectPart::forget("F3").from_source("b"),
                InjectPart::saved("S2").from_source("b"),
            ],
        );
        let texts: Vec<String> = msg["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["text"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            texts,
            vec![
                "hi".to_string(),
                ec::saved_block_text("a", "S1"),
                ec::saved_block_text("b", "S2"),
                ec::marker_text("a"),
                ec::marker_text("b"),
            ]
        );
        assert_eq!(turn.tail(), Some(ec::guard_turn("F1\n\nF2\n\nF3").as_str()));
    }

    #[test]
    fn empty_and_whitespace_parts_are_a_no_op() {
        let mut msg = user(json!("hi"));
        let before = msg.clone();
        let mut turn = TurnInjection::default();
        turn.absorb(
            &mut msg,
            vec![InjectPart::saved("  \n"), InjectPart::forget(""), InjectPart::saved("")],
        );
        assert_eq!(msg["content"], before["content"], "nothing saved");
        assert_eq!(ec::hooked(&msg), Some(0), "but it was hooked");
        assert_eq!(hook_target_text(&msg), None, "and never is again");
        assert_eq!(turn.tail(), None);
    }

    #[test]
    fn over_cap_saved_part_is_turn_only_with_a_marker() {
        let big = "x".repeat(SAVED_PART_CAP + 1);
        let exact = "y".repeat(SAVED_PART_CAP);
        let mut msg = user(json!("hi"));
        let mut turn = TurnInjection::default();
        turn.absorb(
            &mut msg,
            vec![
                InjectPart::saved(big.clone()).from_source("a"),
                InjectPart::saved(exact.clone()).from_source("b"),
            ],
        );
        let content = &msg["content"];
        assert_eq!(content[1]["text"], ec::saved_block_text("b", &exact));
        assert_eq!(content[2]["text"], ec::marker_text("a"));
        assert_eq!(content.as_array().unwrap().len(), 3);
        assert_eq!(turn.tail(), Some(ec::guard_turn(&big).as_str()));
    }

    #[test]
    fn later_forget_parts_join_earlier_ones() {
        let mut first = user(json!("prompt"));
        let mut steer = user(json!("steer"));
        let mut turn = TurnInjection::default();
        turn.absorb(&mut first, vec![InjectPart::forget("one").from_source("a")]);
        turn.absorb(&mut steer, vec![InjectPart::forget("two").from_source("b")]);
        assert_eq!(turn.tail(), Some(ec::guard_turn("one\n\ntwo").as_str()));
        assert_eq!(steer["content"][1]["text"], ec::marker_text("b"));
        assert_eq!(first["content"][1]["text"], ec::marker_text("a"));
    }

    #[test]
    fn hook_target_is_the_users_own_text() {
        assert_eq!(hook_target_text(&user(json!("go"))).as_deref(), Some("go"));
        let tool = user(json!([{"type": "tool_result", "tool_use_id": "t", "content": "x"}]));
        assert_eq!(hook_target_text(&tool), None);
        let image = user(json!([{"type": "image"}, {"type": "text", "text": "look"}]));
        assert_eq!(hook_target_text(&image).as_deref(), Some("look"));
        let assistant = Arc::new(json!({"role": "assistant", "content": "x"}));
        assert_eq!(hook_target_text(&assistant), None);
        // Already hooked: never again, whatever it added.
        let mut hooked = user(json!("go"));
        TurnInjection::default().absorb(&mut hooked, vec![InjectPart::forget("f").from_source("a")]);
        assert_eq!(ec::hooked(&hooked), Some(1));
        assert_eq!(hook_target_text(&hooked), None);
        let mut nothing = user(json!("go"));
        TurnInjection::default().absorb(&mut nothing, Vec::new());
        assert_eq!(hook_target_text(&nothing), None);
        // Text that only looks like extension output is the user's.
        let look_alike = user(json!([{"type": "image"}, {"type": "text", "text": ec::marker_text("axel")}]));
        assert_eq!(hook_target_text(&look_alike).as_deref(), Some(ec::marker_text("axel").as_str()));
    }
}
