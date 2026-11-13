//! History repair after a failed turn, shared by the session actor and the
//! subagent archive.

/// Repair conversation history after a turn failure.
///
/// Only messages appended by the ACTIVE turn (index >= `turn_baseline`) may
/// be removed, and only while the trailing message is invalid history — an
/// assistant message with unmatched `tool_use` blocks or empty content.
/// Valid partial output (assistant text, completed `tool_result` messages)
/// and everything pre-existing (including a trailing user prompt) survive.
pub fn repair_history_after_failure(
    messages: &mut Vec<crate::SharedMessage>,
    turn_baseline: usize,
) {
    fn trailing_is_valid(msg: &serde_json::Value) -> bool {
        match msg["role"].as_str() {
            // User messages (text prompts or tool_result blocks) are always
            // a valid trailing state.
            Some("user") => true,
            Some("assistant") => match &msg["content"] {
                serde_json::Value::String(text) => !text.is_empty(),
                serde_json::Value::Array(blocks) => {
                    !blocks.is_empty()
                        && !blocks
                            .iter()
                            .any(|b| b["type"].as_str() == Some("tool_use"))
                }
                _ => false,
            },
            _ => false,
        }
    }

    while messages.len() > turn_baseline {
        match messages.last() {
            Some(last) if trailing_is_valid(last) => break,
            _ => {
                messages.pop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn msg(v: serde_json::Value) -> crate::SharedMessage {
        Arc::new(v)
    }

    /// T3 criterion 4: a pre-existing trailing user message (the prompt that
    /// started the turn) must never be removed by failure repair.
    #[test]
    fn error_repair_keeps_preexisting_trailing_user_message() {
        let mut messages = vec![msg(json!({"role": "user", "content": "hello"}))];
        repair_history_after_failure(&mut messages, 1);
        assert_eq!(
            messages.len(),
            1,
            "pre-existing trailing user message must survive turn failure"
        );
        assert_eq!(messages[0]["content"], "hello");
    }

    /// T3 criterion 2: partial assistant output and completed tool results
    /// appended by the active turn must survive an unrecovered failure.
    #[test]
    fn error_repair_keeps_partial_assistant_output_and_tool_results() {
        let mut messages = vec![
            msg(json!({"role": "user", "content": "do the thing"})),
            msg(json!({"role": "assistant", "content": [
                {"type": "text", "text": "working on it"},
                {"type": "tool_use", "id": "tu_1", "name": "bash", "input": {}},
            ]})),
            msg(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_1", "content": "ok"},
            ]})),
            msg(json!({"role": "assistant", "content": [
                {"type": "text", "text": "partial answer before the failure"},
            ]})),
        ];
        repair_history_after_failure(&mut messages, 1);
        assert_eq!(
            messages.len(),
            4,
            "valid partial assistant output and completed tool results must survive"
        );
        assert_eq!(
            messages[3]["content"][0]["text"],
            "partial answer before the failure"
        );
    }

    /// A turn-appended trailing assistant message with unmatched tool_use
    /// blocks is invalid history and must be removed by repair.
    #[test]
    fn error_repair_drops_turn_appended_unmatched_tool_use() {
        let mut messages = vec![
            msg(json!({"role": "user", "content": "do the thing"})),
            msg(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_1", "name": "bash", "input": {}},
            ]})),
        ];
        repair_history_after_failure(&mut messages, 1);
        assert_eq!(
            messages.len(),
            1,
            "unmatched trailing tool_use appended by the turn must be removed"
        );
        assert_eq!(messages[0]["content"], "do the thing");
    }
}
