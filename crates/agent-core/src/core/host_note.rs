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

/// How the note names a tool whose name isn't a plausible tool name.
pub const UNRECOGNISED_TOOL: &str = "(an unrecognised tool)";

/// A name the note may quote: what tool wire names look like
/// (`ToolRegistry` exposes `[A-Za-z0-9_-]{1,128}`), plus `.` and `:` for
/// runtime-style names. The name comes straight from the model's call, so
/// anything else (a backtick, a newline, an overlong string) is never
/// copied into a user-role message: it reads [`UNRECOGNISED_TOOL`].
pub fn is_plausible_tool_name(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

/// The note a cancel records for tool calls the interrupted response made
/// when the engine never reported back on them: the engine's canceled
/// `tool_result` wording, hedged to "may have already started" because the
/// host can't tell whether the call began. One line.
pub fn tool_in_flight_note_text(phrase: &str, tools: &[String]) -> String {
    let names = tools
        .iter()
        .map(|t| if is_plausible_tool_name(t) { format!("`{t}`") } else { UNRECOGNISED_TOOL.into() })
        .collect::<Vec<_>>()
        .join(", ");
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
/// one or more names that are each a backticked plausible tool name or
/// [`UNRECOGNISED_TOOL`], and nothing else. Checked by regenerating the
/// note from the names it contains, so there is no free-form part. `None`
/// for anything else.
pub fn tool_in_flight_note_label(text: &str) -> Option<&str> {
    if text.contains(['\n', '\r']) {
        return None;
    }
    for phrase in CANCEL_PHRASES {
        for (call, was) in [("tool call", "was"), ("tool calls", "were")] {
            let Some(rest) = text.strip_prefix(&format!("[{phrase} while the {call} ")) else {
                continue;
            };
            let Some(end) = rest.find(&format!(" {was} running. ")) else {
                continue;
            };
            // A plausible name has no space or comma, so ", " splits items.
            let tools: Option<Vec<String>> = rest[..end]
                .split(", ")
                .map(|item| match item.strip_prefix('`').and_then(|i| i.strip_suffix('`')) {
                    Some(name) if is_plausible_tool_name(name) => Some(name.to_string()),
                    // Regenerates as the fallback item.
                    None if item == UNRECOGNISED_TOOL => Some(String::new()),
                    _ => None,
                })
                .collect();
            if tools.is_some_and(|tools| tool_in_flight_note_text(phrase, &tools) == text) {
                return Some(&text[1..text.len() - 1]);
            }
        }
    }
    None
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

    /// A tool name comes straight from the model's call: one that isn't a
    /// plausible tool name (a backtick, a newline, too long) never reaches
    /// the note; it reads "(an unrecognised tool)", and the note is still
    /// recognised exactly.
    #[test]
    fn implausible_tool_names_become_the_fallback_and_stay_recognised() {
        for bad in ["a`b".to_string(), "a\nb".into(), "a".repeat(200), String::new(), "rm -rf /".into()] {
            let note = tool_in_flight_note_text("Canceled by user", std::slice::from_ref(&bad));
            assert_eq!(
                note,
                "[Canceled by user while the tool call (an unrecognised tool) was running. It \
                 may have already started and partially applied its effects.]",
                "{bad:?}"
            );
            assert!(tool_in_flight_note_label(&note).is_some(), "{bad:?}");
        }
        let mixed = tool_in_flight_note_text("Canceled", &["read".into(), "x`y".into()]);
        assert_eq!(
            mixed,
            "[Canceled while the tool calls `read`, (an unrecognised tool) were running. They \
             may have already started and partially applied their effects.]"
        );
        assert!(tool_in_flight_note_label(&mixed).is_some());
        // Plausible names, including MCP/extension style, are kept.
        let kept = tool_in_flight_note_text("Canceled", &["mcp__fs.read:v2-x".into(), "a".repeat(128)]);
        assert!(kept.contains("`mcp__fs.read:v2-x`") && kept.contains(&format!("`{}`", "a".repeat(128))));
        assert!(tool_in_flight_note_label(&kept).is_some());
        // A hand-written name that isn't plausible is not the note.
        let forged = "[Canceled while the tool call `a b` was running. It may have already started \
                      and partially applied its effects.]";
        assert_eq!(tool_in_flight_note_label(forged), None);
    }
}
