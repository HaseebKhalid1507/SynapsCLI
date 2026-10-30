//! Read dedupe: replace a repeated `read` result with a short stub when the
//! identical result is still in the live history.
//!
//! The decision is made where a tool result is turned into history (next to
//! the [`super::tool_spill`] virtualiser), against the current `messages`
//! plus the results already built for the current batch — never against a
//! private memory of what the model has seen. A stub is produced only when
//! an earlier `read` of the same resolved path with the same `offset`/`limit`
//! still sits in history and its history text is byte-identical to the final
//! (post-virtualisation) text this result would add. So after a rollover or
//! compaction removed the earlier read, after the file changed, or when the
//! virtualised previews differ, the model gets the full result.
//!
//! The stub and the write/edit notes are pure functions of their inputs: no
//! timestamps or randomness, so they are cache-safe once written.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::SharedMessage;

/// Identity of a `read` call: resolved path plus the effective range the
/// `read` tool applies (`offset` defaults to 0; `limit` absent = default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReadKey {
    path: PathBuf,
    offset: u64,
    limit: Option<u64>,
}

impl ReadKey {
    /// Key for a builtin `read` input; `None` without a string `path`.
    pub(super) fn from_input(input: &Value, cwd: Option<&Path>) -> Option<Self> {
        let path = input.get("path")?.as_str()?;
        Some(Self {
            path: crate::tools::resolve_path_in(path, cwd),
            offset: input.get("offset").and_then(Value::as_u64).unwrap_or(0),
            limit: input.get("limit").and_then(Value::as_u64),
        })
    }
}

/// What read dedupe may do to one call's history entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Shaping {
    /// A builtin `read`: may become an "unchanged" stub.
    Read(ReadKey),
    /// A builtin `write` / `edit`: gets its no-need-to-re-read note.
    Mutation(&'static str),
}

impl Shaping {
    /// The shaping a call is eligible for, decided from its (post-hook)
    /// input before it runs. `None` when dedupe is off or the tool is not
    /// the builtin `read` / `write` / `edit`. The caller must apply it only
    /// to a successful, plain-text, not-cancelled result.
    pub(super) fn for_call(
        enabled: bool,
        runtime_name: &str,
        origin: &crate::tools::ToolOrigin,
        input: &Value,
        cwd: Option<&Path>,
    ) -> Option<Self> {
        if !enabled || !matches!(origin, crate::tools::ToolOrigin::Builtin) {
            return None;
        }
        match runtime_name {
            "read" => ReadKey::from_input(input, cwd).map(Shaping::Read),
            "write" => Some(Shaping::Mutation("write")),
            "edit" => Some(Shaping::Mutation("edit")),
            _ => None,
        }
    }
}

/// Applies `shaping` to the final history `content` of a successful call.
///
/// Returns the content history should carry and, for a deduped read, the
/// bytes the stub kept out of history. A read is stubbed only when an
/// identical earlier result is live (see [`find_unchanged_read`]) and the
/// stub is actually shorter than the content it replaces.
pub(super) fn shape(
    shaping: &Shaping,
    messages: &[SharedMessage],
    pending: &[Value],
    wire_name: &str,
    cwd: Option<&Path>,
    content: Value,
) -> (Value, Option<usize>) {
    match shaping {
        Shaping::Read(key) => {
            let Some(id) = find_unchanged_read(messages, pending, wire_name, key, cwd, &content)
            else {
                return (content, None);
            };
            let stub = unchanged_stub(&id);
            let replaced = content.as_str().map_or(0, str::len);
            if replaced <= stub.len() {
                return (content, None);
            }
            let saved = replaced - stub.len();
            (Value::String(stub), Some(saved))
        }
        Shaping::Mutation(name) => match content {
            Value::String(text) => (Value::String(with_mutation_note(name, text)), None),
            other => (other, None),
        },
    }
}

/// The deterministic stub that replaces a deduped read result.
pub(super) fn unchanged_stub(earlier_id: &str) -> String {
    format!(
        "[File unchanged since your earlier read ({earlier_id}) — that result is still \
         in your context above and is current. Use it instead of re-reading.]"
    )
}

/// One honest line appended to a successful `write` result.
pub(super) const WRITE_NOTE: &str =
    "The file now holds exactly the content you wrote; no need to re-read it to verify.";

/// One honest line appended to a successful `edit` result.
pub(super) const EDIT_NOTE: &str =
    "The edit was applied exactly as given; no need to re-read the file to verify.";

/// Appends the write/edit note for builtin `write`/`edit` successes.
/// Returns `content` unchanged for every other tool.
pub(super) fn with_mutation_note(runtime_name: &str, content: String) -> String {
    let note = match runtime_name {
        "write" => WRITE_NOTE,
        "edit" => EDIT_NOTE,
        _ => return content,
    };
    if content.is_empty() {
        note.to_string()
    } else {
        format!("{content}\n\n{note}")
    }
}

/// Finds an earlier `read` result that makes `new_content` redundant.
///
/// * `wire_name` — the API name of the current call's tool; only earlier
///   `tool_use` blocks with the same name are considered.
/// * `key` — the current call's [`ReadKey`].
/// * `new_content` — the exact `content` value this result would add to
///   history (after virtualisation). Only plain strings dedupe.
/// * `pending` — `tool_result` blocks already built for the current batch
///   (they precede this one in the user message that will be pushed).
///
/// Returns the earlier `tool_use_id` whose result in history is a
/// non-error string byte-identical to `new_content`.
pub(super) fn find_unchanged_read(
    messages: &[SharedMessage],
    pending: &[Value],
    wire_name: &str,
    key: &ReadKey,
    cwd: Option<&Path>,
    new_content: &Value,
) -> Option<String> {
    let new_text = new_content.as_str()?;
    if new_text.is_empty() {
        return None;
    }
    // Every earlier tool_result, most recent first: the pending batch, then
    // user messages in history.
    let history_results = messages.iter().rev().flat_map(|m| {
        let blocks = if m["role"] == "user" {
            m["content"].as_array().map(|a| a.as_slice()).unwrap_or(&[])
        } else {
            &[]
        };
        blocks.iter().rev()
    });
    let candidates = pending.iter().rev().chain(history_results);
    for result in candidates {
        if result["type"] != "tool_result" || result["is_error"] == Value::Bool(true) {
            continue;
        }
        if result["content"].as_str() != Some(new_text) {
            continue;
        }
        let Some(id) = result["tool_use_id"].as_str() else {
            continue;
        };
        let Some(input) = find_tool_use_input(messages, id, wire_name) else {
            continue;
        };
        if ReadKey::from_input(input, cwd).as_ref() == Some(key) {
            return Some(id.to_string());
        }
    }
    None
}

/// Input of the assistant `tool_use` block `id` named `wire_name`.
fn find_tool_use_input<'a>(
    messages: &'a [SharedMessage],
    id: &str,
    wire_name: &str,
) -> Option<&'a Value> {
    messages
        .iter()
        .rev()
        .filter(|m| m["role"] == "assistant")
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .find(|b| b["type"] == "tool_use" && b["id"] == id)
        .filter(|b| b["name"] == wire_name)
        .map(|b| &b["input"])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn use_msg(calls: &[(&str, &str, Value)]) -> SharedMessage {
        let content: Vec<Value> = calls
            .iter()
            .map(|(id, name, input)| {
                json!({"type": "tool_use", "id": id, "name": name, "input": input})
            })
            .collect();
        Arc::new(json!({"role": "assistant", "content": content}))
    }

    fn result(id: &str, content: Value) -> Value {
        json!({"type": "tool_result", "tool_use_id": id, "content": content})
    }

    fn results_msg(results: Vec<Value>) -> SharedMessage {
        Arc::new(json!({"role": "user", "content": results}))
    }

    fn key(input: &Value) -> ReadKey {
        ReadKey::from_input(input, Some(Path::new("/work"))).unwrap()
    }

    fn find(
        messages: &[SharedMessage],
        pending: &[Value],
        input: &Value,
        text: &str,
    ) -> Option<String> {
        find_unchanged_read(
            messages,
            pending,
            "read",
            &key(input),
            Some(Path::new("/work")),
            &json!(text),
        )
    }

    #[test]
    fn identical_read_in_history_is_found() {
        let input = json!({"path": "src/a.rs"});
        let messages = vec![
            use_msg(&[("toolu_1", "read", input.clone())]),
            results_msg(vec![result("toolu_1", json!("1\tfn a() {}"))]),
            use_msg(&[("toolu_2", "read", input.clone())]),
        ];
        assert_eq!(
            find(&messages, &[], &input, "1\tfn a() {}").as_deref(),
            Some("toolu_1")
        );
    }

    #[test]
    fn relative_and_absolute_paths_resolve_to_the_same_key() {
        let messages = vec![
            use_msg(&[("toolu_1", "read", json!({"path": "/work/src/a.rs"}))]),
            results_msg(vec![result("toolu_1", json!("x"))]),
        ];
        assert_eq!(
            find(
                &messages,
                &[],
                &json!({"path": "src/a.rs", "offset": 0}),
                "x"
            )
            .as_deref(),
            Some("toolu_1")
        );
    }

    #[test]
    fn changed_content_range_or_tool_is_not_deduped() {
        let input = json!({"path": "a.rs"});
        let messages = vec![
            use_msg(&[
                ("toolu_1", "read", input.clone()),
                ("toolu_2", "read", json!({"path": "a.rs", "offset": 10})),
                ("toolu_3", "grep", input.clone()),
            ]),
            results_msg(vec![
                result("toolu_1", json!("old")),
                result("toolu_2", json!("new")),
                result("toolu_3", json!("new")),
            ]),
        ];
        assert_eq!(find(&messages, &[], &input, "new"), None);
        assert_eq!(
            find(&messages, &[], &json!({"path": "a.rs", "limit": 5}), "old"),
            None
        );
    }

    #[test]
    fn evicted_earlier_read_is_not_deduped() {
        // After rollover/compaction the earlier tool_use/result pair is gone.
        let input = json!({"path": "a.rs"});
        let messages = vec![
            Arc::new(json!({"role": "user", "content": "[summary of earlier work]"})),
            use_msg(&[("toolu_2", "read", input.clone())]),
        ];
        assert_eq!(find(&messages, &[], &input, "x"), None);
        // A result whose tool_use was evicted does not count either.
        let orphan = vec![results_msg(vec![result("toolu_1", json!("x"))])];
        assert_eq!(find(&orphan, &[], &input, "x"), None);
    }

    #[test]
    fn errors_rich_and_empty_results_are_never_deduped() {
        let input = json!({"path": "a.rs"});
        let mut err = result("toolu_1", json!("x"));
        err["is_error"] = json!(true);
        let rich = json!([{"type": "text", "text": "x"}]);
        let messages = vec![
            use_msg(&[
                ("toolu_1", "read", input.clone()),
                ("toolu_2", "read", input.clone()),
            ]),
            results_msg(vec![
                err,
                result("toolu_2", rich.clone()),
                result("toolu_3", json!("")),
            ]),
        ];
        assert_eq!(find(&messages, &[], &input, "x"), None);
        assert_eq!(find(&messages, &[], &input, ""), None);
        let found = find_unchanged_read(
            &messages,
            &[],
            "read",
            &key(&input),
            Some(Path::new("/work")),
            &rich,
        );
        assert_eq!(found, None);
    }

    #[test]
    fn pending_batch_results_count_and_win_over_history() {
        let input = json!({"path": "a.rs"});
        let messages = vec![
            use_msg(&[("toolu_1", "read", input.clone())]),
            results_msg(vec![result("toolu_1", json!("x"))]),
            use_msg(&[
                ("toolu_2", "read", input.clone()),
                ("toolu_3", "read", input.clone()),
            ]),
        ];
        let pending = vec![result("toolu_2", json!("x"))];
        assert_eq!(
            find(&messages, &pending, &input, "x").as_deref(),
            Some("toolu_2")
        );
    }

    #[test]
    fn earlier_stub_is_skipped_for_the_original() {
        let input = json!({"path": "a.rs"});
        let messages = vec![
            use_msg(&[("toolu_1", "read", input.clone())]),
            results_msg(vec![result("toolu_1", json!("x"))]),
            use_msg(&[("toolu_2", "read", input.clone())]),
            results_msg(vec![result("toolu_2", json!(unchanged_stub("toolu_1")))]),
        ];
        assert_eq!(
            find(&messages, &[], &input, "x").as_deref(),
            Some("toolu_1")
        );
    }

    #[test]
    fn only_builtin_read_write_edit_are_shaped_and_only_when_on() {
        use crate::tools::ToolOrigin;
        let input = json!({"path": "a.rs"});
        let cwd = Some(Path::new("/work"));
        let builtin = ToolOrigin::Builtin;
        assert_eq!(
            Shaping::for_call(true, "read", &builtin, &input, cwd),
            Some(Shaping::Read(key(&input)))
        );
        assert_eq!(
            Shaping::for_call(true, "write", &builtin, &input, cwd),
            Some(Shaping::Mutation("write"))
        );
        assert_eq!(
            Shaping::for_call(true, "edit", &builtin, &input, cwd),
            Some(Shaping::Mutation("edit"))
        );
        assert_eq!(
            Shaping::for_call(false, "read", &builtin, &input, cwd),
            None,
            "off"
        );
        assert_eq!(Shaping::for_call(true, "bash", &builtin, &input, cwd), None);
        assert_eq!(
            Shaping::for_call(true, "read", &builtin, &json!({}), cwd),
            None,
            "no path"
        );
        let mcp = ToolOrigin::Mcp {
            server_id: "s".into(),
            server_tool_name: "read".into(),
        };
        assert_eq!(
            Shaping::for_call(true, "read", &mcp, &input, cwd),
            None,
            "not the builtin"
        );
    }

    #[test]
    fn shape_stubs_only_when_the_stub_is_shorter() {
        let input = json!({"path": "a.rs"});
        let big = "1\tfn a() {}\n".repeat(40);
        let messages = vec![
            use_msg(&[
                ("toolu_1", "read", input.clone()),
                ("toolu_2", "read", input.clone()),
            ]),
            results_msg(vec![
                result("toolu_1", json!(big)),
                result("toolu_2", json!("1\tx\n")),
            ]),
        ];
        let cwd = Some(Path::new("/work"));
        let read = Shaping::Read(key(&input));
        let (content, saved) = shape(&read, &messages, &[], "read", cwd, json!(big));
        assert_eq!(content, json!(unchanged_stub("toolu_1")));
        assert_eq!(saved, Some(big.len() - unchanged_stub("toolu_1").len()));
        // A tiny file: the stub would be longer than the result.
        let (content, saved) = shape(&read, &messages, &[], "read", cwd, json!("1\tx\n"));
        assert_eq!((content, saved), (json!("1\tx\n"), None));
        let write = Shaping::Mutation("write");
        let (content, saved) = shape(&write, &messages, &[], "write", cwd, json!("Wrote 1 lines"));
        assert_eq!(
            (content, saved),
            (json!(format!("Wrote 1 lines\n\n{WRITE_NOTE}")), None)
        );
    }

    #[test]
    fn stub_and_notes_are_deterministic() {
        assert_eq!(unchanged_stub("toolu_9"), unchanged_stub("toolu_9"));
        assert!(unchanged_stub("toolu_9").contains("toolu_9"));
        assert_eq!(
            with_mutation_note("write", "Wrote 3 bytes".into()),
            format!("Wrote 3 bytes\n\n{WRITE_NOTE}")
        );
        assert_eq!(with_mutation_note("edit", String::new()), EDIT_NOTE);
        assert_eq!(with_mutation_note("bash", "ok".into()), "ok");
    }
}
