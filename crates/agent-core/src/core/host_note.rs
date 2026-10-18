//! Host-authored notes the session records into history as plain user-role
//! text, recognised by their exact generated wording (like the interruption
//! markers in `synaps_engine::engine::interrupt`), never by a host key.
//!
//! A host key (`_synaps_context`) would make a rolled-back 0.10.2 take the
//! note for a context-archive marker and refuse every turn under Axel
//! memory, so the note stays ordinary text on disk and on the wire. Readers
//! that must not treat it as something the user said (turn starts, archive
//! projection, memory capture) ask [`is_tool_in_flight_note`].

use serde_json::Value;

/// Every cancel phrase a note can lead with: the engine's
/// `CancelCause::phrase` values, plus the generic one for reasons without a
/// live cancel. The engine pins that its phrases are all listed here.
pub const CANCEL_PHRASES: &[&str] = &[
    "Canceled by user",
    "Canceled (session cost cap reached)",
    "Canceled (Synaps restarted)",
    "Canceled (session stopped by the host)",
    "Canceled (session driver revoked)",
    "Canceled",
];

/// The note a cancel records for tool calls the interrupted response made
/// when the engine never reported back on them: the engine's canceled
/// `tool_result` wording, hedged to "may have already started" because the
/// host can't tell whether the call began. One line.
pub fn tool_in_flight_note_text(phrase: &str, tools: &[String]) -> String {
    let names = tools.iter().map(|t| format!("`{t}`")).collect::<Vec<_>>().join(", ");
    let (call, was, it, its) = if tools.len() == 1 {
        ("tool call", "was", "It", "its")
    } else {
        ("tool calls", "were", "They", "their")
    };
    format!(
        "[{phrase} while the {call} {names} {was} running. {it} may have already started and \
         partially applied {its} effects.]"
    )
}

/// `text` without its brackets iff it is exactly a note
/// [`tool_in_flight_note_text`] generates: a single line, a known phrase,
/// one or more non-empty backticked tool names, and nothing else. Checked
/// by regenerating the note from the names it contains, so there is no
/// free-form part. `None` for anything else.
pub fn tool_in_flight_note_label(text: &str) -> Option<&str> {
    if text.contains(['\n', '\r']) || !text.starts_with('[') || !text.ends_with(']') {
        return None;
    }
    let mut parts = text.split('`');
    parts.next()?; // the lead-in before the first name
    let mut tools = Vec::new();
    while let (Some(name), Some(_between)) = (parts.next(), parts.next()) {
        if name.is_empty() {
            return None;
        }
        tools.push(name.to_string());
    }
    if tools.is_empty() {
        return None;
    }
    CANCEL_PHRASES
        .iter()
        .any(|phrase| tool_in_flight_note_text(phrase, &tools) == text)
        .then(|| &text[1..text.len() - 1])
}

/// A user-role history message whose string content is exactly a
/// tool-in-flight note.
pub fn is_tool_in_flight_note(message: &Value) -> bool {
    message["role"] == "user"
        && message["content"]
            .as_str()
            .is_some_and(|text| tool_in_flight_note_label(text).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn generated_notes_round_trip_and_nothing_else_matches() {
        for phrase in CANCEL_PHRASES {
            for tools in [vec!["bash".to_string()], vec!["read".into(), "edit".into(), "bash".into()]] {
                let note = tool_in_flight_note_text(phrase, &tools);
                assert_eq!(tool_in_flight_note_label(&note), Some(&note[1..note.len() - 1]));
                assert!(is_tool_in_flight_note(&json!({"role": "user", "content": note})));
            }
        }
        let note = tool_in_flight_note_text("Canceled by user", &["bash".into()]);
        for text in [
            note.replace("running.", "running.\n"),
            note.replace("`bash`", "`bash` `x`"),
            note.replace("`bash`", "`bash"),
            format!(" {note}"),
            "[]".into(),
            "``".into(),
        ] {
            assert_eq!(tool_in_flight_note_label(&text), None, "{text:?}");
        }
        assert!(!is_tool_in_flight_note(&json!({"role": "assistant", "content": note})));
    }
}
