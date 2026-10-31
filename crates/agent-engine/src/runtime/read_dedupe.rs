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
//!
//! A rollover keeps the newest messages, so it can keep a stub and drop the
//! read it names. The stub's own wording says to read again in that case,
//! and both successor builders (foreground `continuation::successor`,
//! worker `worker_context::build_successor`) also rewrite such a stub to
//! [`DROPPED_STUB`] via [`rewrite_dangling_stubs`]. The cut changes the
//! prefix anyway, so the rewrite costs no extra cache.

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
    /// A builtin `write` / `edit` / `multi_edit`: gets its no-need-to-re-read note.
    Mutation(&'static str),
}

impl Shaping {
    /// The shaping a call is eligible for, decided from its (post-hook)
    /// input before it runs. `None` when dedupe is off or the tool is not
    /// the builtin `read` / `write` / `edit` / `multi_edit`. The caller must apply it only
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
            "multi_edit" => Some(Shaping::Mutation("multi_edit")),
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

/// Everything in a stub before the earlier call's id.
const STUB_PREFIX: &str = "[File unchanged since your earlier read (";

/// The deterministic stub that replaces a deduped read result. It stays
/// true even if a later rollover drops the result it names.
pub(super) fn unchanged_stub(earlier_id: &str) -> String {
    format!(
        "{STUB_PREFIX}{earlier_id}). That result is current; use it instead of \
         re-reading. If it's no longer in your context, read the file again.]"
    )
}

/// The earlier tool_use_id `text` names, when `text` is exactly a stub
/// [`unchanged_stub`] wrote (not a prefix match: the whole text must equal
/// the stub for that id).
fn stub_target(text: &str) -> Option<&str> {
    let id = text.strip_prefix(STUB_PREFIX)?.split(')').next()?;
    (!id.is_empty() && unchanged_stub(id) == text).then_some(id)
}

/// What a stub becomes when a rollover dropped the result it names. Fixed
/// text: it never claims the content is in context.
pub(super) const DROPPED_STUB: &str =
    "[Earlier file content was dropped at a context rollover; read the file again if you need it.]";

/// Rewrites, in a rollover successor, every dedupe stub whose named
/// `tool_use_id` has no `tool_result` left in `successor` to
/// [`DROPPED_STUB`]. Only user `tool_result` blocks are touched (never
/// assistant messages); a message with nothing to rewrite keeps its `Arc`.
/// Idempotent. Returns how many stubs were rewritten.
pub(super) fn rewrite_dangling_stubs(successor: &mut [SharedMessage]) -> usize {
    let is_result = |b: &Value| b["type"] == "tool_result";
    let retained: std::collections::HashSet<String> = successor
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter(|b| is_result(b))
        .filter_map(|b| b["tool_use_id"].as_str().map(str::to_owned))
        .collect();
    let dangling = |b: &Value| {
        is_result(b)
            && b["content"]
                .as_str()
                .and_then(stub_target)
                .is_some_and(|id| !retained.contains(id))
    };
    let mut rewritten = 0;
    for message in successor.iter_mut() {
        if message["role"] != "user"
            || !message["content"]
                .as_array()
                .is_some_and(|blocks| blocks.iter().any(dangling))
        {
            continue;
        }
        let mut edited = (**message).clone();
        for block in edited["content"].as_array_mut().into_iter().flatten() {
            if dangling(block) {
                block["content"] = Value::String(DROPPED_STUB.to_string());
                rewritten += 1;
            }
        }
        *message = std::sync::Arc::new(edited);
    }
    rewritten
}

/// Host-written history text (a stub or a rewritten stub): never an
/// original `read` result, so never a dedupe candidate.
fn is_host_text(text: &str) -> bool {
    text == DROPPED_STUB || stub_target(text).is_some()
}

/// One honest line appended to a successful `write` result.
pub(super) const WRITE_NOTE: &str =
    "The file now holds exactly the content you wrote; no need to re-read it to verify.";

/// One honest line appended to a successful `edit` result.
pub(super) const EDIT_NOTE: &str =
    "The edit was applied exactly as given; no need to re-read the file to verify.";

/// One honest line appended to a successful `multi_edit` result. Its result
/// already lists every edit, skip and newline/line-ending adjustment.
pub(super) const MULTI_EDIT_NOTE: &str =
    "Every edit above was applied as reported; no need to re-read the files to verify.";

/// Appends the write/edit note for builtin `write`/`edit`/`multi_edit`
/// successes.
/// Returns `content` unchanged for every other tool.
pub(super) fn with_mutation_note(runtime_name: &str, content: String) -> String {
    let note = match runtime_name {
        "write" => WRITE_NOTE,
        "edit" => EDIT_NOTE,
        "multi_edit" => MULTI_EDIT_NOTE,
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
        if result["content"].as_str() != Some(new_text) || is_host_text(new_text) {
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
    fn stub_text_is_pinned_and_parses_back_exactly() {
        let stub = unchanged_stub("toolu_01AbC");
        assert_eq!(
            stub,
            "[File unchanged since your earlier read (toolu_01AbC). That result is current; \
             use it instead of re-reading. If it's no longer in your context, read the file \
             again.]"
        );
        // Short enough for the shorter-than-the-result rule to matter only
        // for tiny files: a real Anthropic id is 30 bytes.
        assert!(unchanged_stub(&"x".repeat(30)).len() < 200);
        assert_eq!(stub_target(&stub), Some("toolu_01AbC"));
        // Exact match only: no prefix, suffix or near-miss counts.
        assert_eq!(stub_target(&format!("{stub} ")), None);
        assert_eq!(stub_target(&stub[..stub.len() - 1]), None);
        assert_eq!(stub_target(STUB_PREFIX), None);
        assert_eq!(stub_target(&unchanged_stub("")), None, "an id is required");
        assert_eq!(stub_target(DROPPED_STUB), None);
        assert!(is_host_text(&stub) && is_host_text(DROPPED_STUB));
        assert!(!is_host_text("1\tfn a() {}\n"));
    }

    #[test]
    fn rollover_rewrites_only_stubs_whose_original_is_gone() {
        let input = json!({"path": "a.rs"});
        let kept_original = results_msg(vec![result("toolu_1", json!("1\tfn a() {}\n"))]);
        let calls = use_msg(&[
            ("toolu_2", "read", input.clone()),
            ("toolu_3", "read", input.clone()),
            ("toolu_4", "bash", json!({})),
        ]);
        let stubs = results_msg(vec![
            // Names a read the cut dropped: rewritten.
            result("toolu_2", json!(unchanged_stub("toolu_0"))),
            // Names a read that is still here: untouched.
            result("toolu_3", json!(unchanged_stub("toolu_1"))),
            // Plain text that merely mentions the stub wording: untouched.
            result(
                "toolu_4",
                json!(format!("echo {}", unchanged_stub("toolu_0"))),
            ),
        ]);
        let untouched = results_msg(vec![result("toolu_5", json!(unchanged_stub("toolu_1")))]);
        let mut successor = vec![
            use_msg(&[("toolu_1", "read", input.clone())]),
            kept_original.clone(),
            calls.clone(),
            stubs.clone(),
            untouched.clone(),
        ];
        assert_eq!(rewrite_dangling_stubs(&mut successor), 1);
        let blocks = successor[3]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["content"], json!(DROPPED_STUB));
        assert_eq!(
            blocks[0]["tool_use_id"], "toolu_2",
            "the block keeps its pairing"
        );
        assert_eq!(blocks[1], stubs["content"][1]);
        assert_eq!(blocks[2], stubs["content"][2]);
        // Every other message is the same Arc: assistant turns never edited.
        assert!(Arc::ptr_eq(&successor[1], &kept_original));
        assert!(Arc::ptr_eq(&successor[2], &calls));
        assert!(Arc::ptr_eq(&successor[4], &untouched));
        // Idempotent and byte-stable across later rollovers.
        let once = successor.clone();
        assert_eq!(rewrite_dangling_stubs(&mut successor), 0);
        assert_eq!(
            serde_json::to_string(&successor).unwrap(),
            serde_json::to_string(&once).unwrap()
        );
        // Nothing to rewrite: no message is cloned.
        let mut clean = vec![calls.clone(), kept_original.clone()];
        assert_eq!(rewrite_dangling_stubs(&mut clean), 0);
        assert!(Arc::ptr_eq(&clean[1], &kept_original));
    }

    #[test]
    fn host_written_text_is_never_a_dedupe_candidate() {
        let input = json!({"path": "a.rs"});
        for text in [DROPPED_STUB.to_string(), unchanged_stub("toolu_0")] {
            let messages = vec![
                use_msg(&[("toolu_1", "read", input.clone())]),
                results_msg(vec![result("toolu_1", json!(text))]),
            ];
            assert_eq!(find(&messages, &[], &input, &text), None, "{text}");
        }
    }

    /// `multi_edit` changes files like `edit`/`write`, so it gets the same
    /// treatment: a mutation note, never a read stub.
    #[test]
    fn multi_edit_is_a_mutation_with_its_own_note() {
        let input = json!({"edits": [{"path": "a.rs", "op": "append", "text": "x"}]});
        let builtin = crate::tools::ToolOrigin::Builtin;
        assert_eq!(
            Shaping::for_call(true, "multi_edit", &builtin, &input, Some(Path::new("/work"))),
            Some(Shaping::Mutation("multi_edit"))
        );
        assert_eq!(
            with_mutation_note("multi_edit", "multi_edit: 1 file(s) written".into()),
            format!("multi_edit: 1 file(s) written\n\n{MULTI_EDIT_NOTE}")
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
