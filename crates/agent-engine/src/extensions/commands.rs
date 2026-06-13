//! Plugin interactive command (`/command`) output event types and parser.
//!
//! Phase B Phase 2 contract — see
//! `docs/plans/2026-05-03-extension-contracts-for-rich-plugins.md`.
//!
//! Wire shape (`command.output` JSON-RPC notification params):
//!
//! ```jsonc
//! {
//!   "request_id": "abc-123",
//!   "event": { "kind": "text"|"system"|"error"|"table"|"done", ... }
//! }
//! ```
//!
//! Synaps subscribes to `command.output` notifications matching the
//! caller-issued `request_id` after invoking `command.invoke`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single structured event emitted by an interactive plugin command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandOutputEvent {
    /// Markdown-rendered chat text.
    Text { content: String },
    /// System-style chat message (wrapped/dimmed in the UI).
    System { content: String },
    /// Error chat message.
    Error { content: String },
    /// A tabular result.
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    /// End-of-stream marker.
    Done,
}

/// Parsed `command.output` notification frame.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandOutputFrame {
    pub request_id: String,
    pub event: CommandOutputEvent,
}

/// Parse a `command.output` JSON-RPC notification's `params`.
///
/// Accepts both `{"request_id": "...", "event": {"kind": "..."}}` and
/// the flat shape `{"request_id": "...", "kind": "...", ...}` for tolerance.
pub fn parse_command_output(params: &Value) -> Result<CommandOutputFrame, String> {
    let obj = params
        .as_object()
        .ok_or_else(|| "command.output params must be a JSON object".to_string())?;

    let request_id = obj
        .get("request_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "command.output missing request_id".to_string())?
        .to_string();
    if request_id.is_empty() {
        return Err("command.output request_id must be non-empty".to_string());
    }

    let event_value: Value = match obj.get("event") {
        Some(v) => v.clone(),
        None => {
            // Flat shape: rebuild a nested object containing all non-request_id keys.
            let mut clone = obj.clone();
            clone.remove("request_id");
            Value::Object(clone)
        }
    };

    let event = parse_command_output_event(&event_value)?;
    Ok(CommandOutputFrame { request_id, event })
}

/// Parse just the `event` payload (`{"kind": "...", ...}`).
pub fn parse_command_output_event(event: &Value) -> Result<CommandOutputEvent, String> {
    let obj = event
        .as_object()
        .ok_or_else(|| "command.output event must be a JSON object".to_string())?;
    let kind = obj
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "command.output event missing 'kind'".to_string())?;

    match kind {
        "text" => {
            let content = obj
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "command.output text event missing 'content'".to_string())?
                .to_string();
            Ok(CommandOutputEvent::Text { content })
        }
        "system" => {
            let content = obj
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "command.output system event missing 'content'".to_string())?
                .to_string();
            Ok(CommandOutputEvent::System { content })
        }
        "error" => {
            let content = obj
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "command.output error event missing 'content'".to_string())?
                .to_string();
            if content.is_empty() {
                return Err("command.output error content must be non-empty".to_string());
            }
            Ok(CommandOutputEvent::Error { content })
        }
        "table" => {
            let headers = obj
                .get("headers")
                .and_then(Value::as_array)
                .ok_or_else(|| "command.output table missing 'headers' array".to_string())?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| "command.output table header must be string".to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            let rows = obj
                .get("rows")
                .and_then(Value::as_array)
                .ok_or_else(|| "command.output table missing 'rows' array".to_string())?
                .iter()
                .map(|row| {
                    row.as_array()
                        .ok_or_else(|| "command.output table row must be array".to_string())?
                        .iter()
                        .map(|cell| {
                            cell.as_str()
                                .map(str::to_string)
                                .ok_or_else(|| "command.output table cell must be string".to_string())
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(CommandOutputEvent::Table { headers, rows })
        }
        "done" => Ok(CommandOutputEvent::Done),
        other => Err(format!("unknown command.output event kind: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_text_event_nested() {
        let v = json!({"request_id": "r1", "event": {"kind": "text", "content": "hello"}});
        let frame = parse_command_output(&v).unwrap();
        assert_eq!(frame.request_id, "r1");
        assert_eq!(
            frame.event,
            CommandOutputEvent::Text { content: "hello".into() }
        );
    }

    #[test]
    fn parses_text_event_flat() {
        let v = json!({"request_id": "r2", "kind": "text", "content": "hi"});
        let frame = parse_command_output(&v).unwrap();
        assert_eq!(frame.request_id, "r2");
        assert_eq!(
            frame.event,
            CommandOutputEvent::Text { content: "hi".into() }
        );
    }

    #[test]
    fn parses_system_error_done() {
        let frame = parse_command_output(
            &json!({"request_id":"x","event":{"kind":"system","content":"sys"}}),
        )
        .unwrap();
        assert!(matches!(frame.event, CommandOutputEvent::System { .. }));

        let frame = parse_command_output(
            &json!({"request_id":"x","event":{"kind":"error","content":"oops"}}),
        )
        .unwrap();
        assert!(matches!(frame.event, CommandOutputEvent::Error { .. }));

        let frame = parse_command_output(&json!({"request_id":"x","event":{"kind":"done"}})).unwrap();
        assert_eq!(frame.event, CommandOutputEvent::Done);
    }

    #[test]
    fn parses_table() {
        let v = json!({
            "request_id": "t",
            "event": {
                "kind": "table",
                "headers": ["id", "size"],
                "rows": [["tiny", "75 MB"], ["base", "142 MB"]]
            }
        });
        let frame = parse_command_output(&v).unwrap();
        match frame.event {
            CommandOutputEvent::Table { headers, rows } => {
                assert_eq!(headers, vec!["id".to_string(), "size".to_string()]);
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0], vec!["tiny".to_string(), "75 MB".to_string()]);
            }
            other => panic!("expected Table, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_request_id() {
        let v = json!({"event": {"kind": "done"}});
        assert!(parse_command_output(&v).is_err());
    }

    #[test]
    fn rejects_empty_request_id() {
        let v = json!({"request_id": "", "event": {"kind": "done"}});
        assert!(parse_command_output(&v).is_err());
    }

    #[test]
    fn rejects_unknown_kind() {
        let v = json!({"request_id": "x", "event": {"kind": "weird"}});
        let err = parse_command_output(&v).unwrap_err();
        assert!(err.contains("unknown"));
    }

    #[test]
    fn rejects_text_without_content() {
        let v = json!({"request_id":"x","event":{"kind":"text"}});
        assert!(parse_command_output(&v).is_err());
    }

    #[test]
    fn rejects_error_with_empty_content() {
        let v = json!({"request_id":"x","event":{"kind":"error","content":""}});
        assert!(parse_command_output(&v).is_err());
    }

    #[test]
    fn rejects_table_with_non_string_cell() {
        let v = json!({
            "request_id":"x",
            "event":{"kind":"table","headers":["a"],"rows":[[1]]}
        });
        assert!(parse_command_output(&v).is_err());
    }
}
