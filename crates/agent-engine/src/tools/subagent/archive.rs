//! Resume archives for reactive subagents.
//!
//! A worker's real conversation (every assistant turn, tool call and tool
//! result) used to live only inside its runtime. When the worker timed out,
//! failed or was collected, `subagent_resume` had nothing but the task text and
//! a short final message, so every resumed worker started over from scratch.
//! And once the registry reaped the handle (at the end of the turn after a
//! collect, or 15 minutes after finishing), resume and collect failed outright.
//!
//! The finalizer now writes `<base>/subagent-history/<process>/<handle>.json`
//! with the worker's identity, terminal status, output text and full message
//! history. Resume continues that history; collect falls back to it. Handle ids
//! come from a process-wide counter, so one directory per process cannot
//! collide. Directories older than [`KEEP_DAYS`] are pruned on first use.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::runtime::subagent::{SubagentArchiveMeta, SubagentState};

pub const ARCHIVE_VERSION: u32 = 1;
const KEEP_DAYS: u64 = 14;
/// How much of the latest response a timeout report carries.
const REPORT_RESPONSE_TAIL_CHARS: usize = 4000;
/// How many of the most recent tool calls a timeout report lists.
const REPORT_RECENT_TOOLS: usize = 20;

/// A worker archive as read back from disk.
#[derive(Debug, Deserialize)]
pub struct SubagentArchive {
    pub version: u32,
    pub meta: SubagentArchiveMeta,
    pub status: String,
    pub output: String,
    pub history: Vec<Value>,
}

/// Borrowing twin of [`SubagentArchive`] so writing never deep-clones history.
#[derive(Serialize)]
struct SubagentArchiveRef<'a> {
    version: u32,
    handle_id: &'a str,
    meta: &'a SubagentArchiveMeta,
    status: &'a str,
    output: &'a str,
    history: &'a [crate::SharedMessage],
}

/// This process's archive directory (created lazily by [`write_archive`]).
pub fn process_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let root = crate::config::base_dir().join("subagent-history");
        prune_older_than(
            &root,
            std::time::Duration::from_secs(KEEP_DAYS * 24 * 60 * 60),
        );
        root.join(format!(
            "{}-{}",
            chrono::Utc::now().format("%Y%m%d-%H%M%S"),
            std::process::id()
        ))
    })
    .clone()
}

fn prune_older_than(root: &Path, age: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|elapsed| elapsed > age);
        if old && entry.path().is_dir() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Write one worker's archive into `dir` (mode 0700 dir / 0600 file: tool
/// output can be sensitive). Atomic: written to a temp name, then renamed.
pub fn write_archive(
    dir: &Path,
    handle_id: &str,
    meta: &SubagentArchiveMeta,
    status: &str,
    output: &str,
    history: &[crate::SharedMessage],
) -> std::io::Result<PathBuf> {
    create_private_dir(dir)?;
    let path = dir.join(format!("{handle_id}.json"));
    let tmp = dir.join(format!(".{handle_id}.json.tmp"));
    let body = serde_json::to_vec(&SubagentArchiveRef {
        version: ARCHIVE_VERSION,
        handle_id,
        meta,
        status,
        output,
        history,
    })
    .map_err(std::io::Error::other)?;
    write_private_file(&tmp, &body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Validate a caller-supplied archive path (`subagent_resume.archive_path`, for
/// workers of an earlier or crashed session): `~` expanded, must resolve to a
/// `.json` file under `<base>/subagent-history`.
pub fn checked_archive_path(raw: &str) -> Result<PathBuf, String> {
    checked_archive_path_in(raw, &crate::config::base_dir().join("subagent-history"))
}

fn checked_archive_path_in(raw: &str, root: &Path) -> Result<PathBuf, String> {
    let expanded = match raw.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()
            .ok_or_else(|| "no home directory to expand '~'".to_string())?
            .join(rest),
        None => PathBuf::from(raw),
    };
    let path = expanded
        .canonicalize()
        .map_err(|e| format!("archive_path '{raw}': {e}"))?;
    let root = root
        .canonicalize()
        .map_err(|e| format!("no subagent archives at {}: {e}", root.display()))?;
    if !path.starts_with(&root) || path.extension().and_then(|e| e.to_str()) != Some("json") {
        return Err(format!(
            "archive_path must be a .json archive under {}",
            root.display()
        ));
    }
    Ok(path)
}

pub fn read_archive(path: &Path) -> std::io::Result<SubagentArchive> {
    let bytes = std::fs::read(path)?;
    let archive: SubagentArchive = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    if archive.version != ARCHIVE_VERSION {
        return Err(std::io::Error::other(format!(
            "unsupported subagent archive version {}",
            archive.version
        )));
    }
    Ok(archive)
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn write_private_file(path: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(body)
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, body: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, body)
}

/// Finalizer hook: move the captured history out of `state` into an archive and
/// remember its path. History is released from memory either way, so finished
/// handles stay small. Failures only log: the archive is a recovery aid.
pub fn archive_finished_worker(
    state: &std::sync::RwLock<SubagentState>,
    handle_id: &str,
    dir: &Path,
) {
    let (meta, status, output, history) = {
        let mut s = state.write().unwrap_or_else(|p| p.into_inner());
        let Some(history) = s.history.take() else {
            return;
        };
        let Some(meta) = s.archive_meta.clone() else {
            return;
        };
        (
            meta,
            s.status.as_str().to_string(),
            s.partial_text.clone(),
            history,
        )
    };
    match write_archive(dir, handle_id, &meta, &status, &output, &history) {
        Ok(path) => {
            state
                .write()
                .unwrap_or_else(|p| p.into_inner())
                .archive_path = Some(path);
        }
        Err(e) => tracing::warn!("subagent {handle_id}: could not write resume archive: {e}"),
    }
}

/// Turn an archived history into the opening messages of a resumed run: drop a
/// trailing half-finished assistant turn (unmatched `tool_use`), then add the
/// new instructions as the next user turn (merged into a trailing user message,
/// so roles still alternate).
pub fn resume_messages(history: Vec<Value>, instructions: &str) -> Vec<crate::SharedMessage> {
    let mut messages: Vec<crate::SharedMessage> =
        history.into_iter().map(std::sync::Arc::new).collect();
    crate::engine::stream::repair_history_after_failure(&mut messages, 0);
    let note = format!(
        "[Resumed by the orchestrator. Everything above is your own earlier work in this task; \
         continue from where it stopped.]\n\n{instructions}"
    );
    let text_block = serde_json::json!({"type": "text", "text": note});
    match messages.last().map(|m| m["role"].as_str() == Some("user")) {
        Some(true) => {
            let last = messages.pop().expect("checked non-empty");
            let mut last = std::sync::Arc::unwrap_or_clone(last);
            let blocks = match last["content"].take() {
                Value::String(s) => {
                    vec![serde_json::json!({"type": "text", "text": s}), text_block]
                }
                Value::Array(mut blocks) => {
                    blocks.push(text_block);
                    blocks
                }
                _ => vec![text_block],
            };
            last["content"] = Value::Array(blocks);
            messages.push(std::sync::Arc::new(last));
        }
        _ => messages.push(std::sync::Arc::new(serde_json::json!({
            "role": "user",
            "content": [text_block]
        }))),
    }
    messages
}

/// What a timed-out worker reports: latest progress first, so the completion
/// preview (its first ~300 chars) says where the worker got to, not how it began.
pub fn timeout_report(
    timeout_secs: u64,
    tool_count: u32,
    state: &SubagentState,
    has_history: bool,
) -> String {
    let resume_note = if has_history {
        "The full conversation was kept: subagent_resume continues it where it stopped."
    } else {
        "The conversation could not be captured; subagent_resume will start from the task."
    };
    let mut text = format!(
        "[TIMED OUT after {timeout_secs}s, {tool_count} tool calls. {resume_note}]\n\n[last response]:\n"
    );
    let last = state.last_response_text().trim();
    if last.is_empty() {
        text.push_str("(no text since the last tool call)\n");
    } else {
        let chars = last.chars().count();
        if chars > REPORT_RESPONSE_TAIL_CHARS {
            text.push('…');
            text.extend(last.chars().skip(chars - REPORT_RESPONSE_TAIL_CHARS));
        } else {
            text.push_str(last);
        }
        text.push('\n');
    }
    let log = &state.tool_log;
    if !log.is_empty() {
        let shown = log.len().min(REPORT_RECENT_TOOLS);
        text.push_str(&format!("\n[last {shown} of {} tool calls]:\n", log.len()));
        for line in &log[log.len() - shown..] {
            text.push_str(line);
            text.push('\n');
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, RwLock};

    fn meta() -> SubagentArchiveMeta {
        SubagentArchiveMeta {
            agent_name: "inline".into(),
            model: "anthropic/claude-opus-5-5".into(),
            system_prompt: "be a builder".into(),
            timeout_secs: 3600,
        }
    }

    fn history() -> Vec<crate::SharedMessage> {
        vec![
            Arc::new(json!({"role": "user", "content": "build the plate"})),
            Arc::new(json!({"role": "assistant", "content": [
                {"type": "text", "text": "Reading."},
                {"type": "tool_use", "id": "t1", "name": "read", "input": {"path": "/x"}}
            ]})),
            Arc::new(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "file body"}
            ]})),
        ]
    }

    #[test]
    fn archive_round_trips_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("proc");
        let path = write_archive(&sub, "sa_7", &meta(), "timed_out", "out", &history()).unwrap();
        let a = read_archive(&path).unwrap();
        assert_eq!(a.meta, meta());
        assert_eq!(a.status, "timed_out");
        assert_eq!(a.output, "out");
        assert_eq!(a.history.len(), 3);
        assert_eq!(a.history[2]["content"][0]["content"], "file body");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let dmode = std::fs::metadata(&sub).unwrap().permissions().mode() & 0o777;
            assert_eq!(dmode, 0o700);
        }
        assert!(
            !sub.join(".sa_7.json.tmp").exists(),
            "temp file renamed away"
        );
    }

    #[test]
    fn finalizer_archives_history_and_releases_it() {
        let dir = tempfile::tempdir().unwrap();
        let state = RwLock::new(SubagentState::new());
        {
            let mut s = state.write().unwrap();
            s.history = Some(history());
            s.archive_meta = Some(meta());
            s.partial_text = "done".into();
        }
        archive_finished_worker(&state, "sa_9", dir.path());
        let s = state.read().unwrap();
        assert!(s.history.is_none(), "history released from memory");
        let path = s.archive_path.clone().expect("archive path recorded");
        assert_eq!(read_archive(&path).unwrap().history.len(), 3);
    }

    #[test]
    fn finalizer_without_history_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let state = RwLock::new(SubagentState::new());
        state.write().unwrap().archive_meta = Some(meta());
        archive_finished_worker(&state, "sa_10", dir.path());
        assert!(state.read().unwrap().archive_path.is_none());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn resume_merges_instructions_into_trailing_tool_results() {
        let h: Vec<Value> = history().iter().map(|m| (**m).clone()).collect();
        let msgs = resume_messages(h, "now render pass 3");
        assert_eq!(
            msgs.len(),
            3,
            "merged, not appended: roles keep alternating"
        );
        let last = &msgs[2];
        assert_eq!(last["role"], "user");
        assert_eq!(last["content"][0]["type"], "tool_result");
        assert_eq!(last["content"][1]["type"], "text");
        assert!(last["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("now render pass 3"));
    }

    #[test]
    fn resume_drops_a_dangling_tool_use_then_appends_user_turn() {
        let mut h: Vec<Value> = history().iter().map(|m| (**m).clone()).collect();
        h.push(json!({"role": "assistant", "content": [
            {"type": "text", "text": "Rendering."},
            {"type": "tool_use", "id": "t2", "name": "bash", "input": {"command": "render"}}
        ]}));
        let msgs = resume_messages(h, "continue");
        // The unmatched tool_use turn is gone (it would be a 400), and the
        // instructions are merged into the preceding user turn.
        assert_eq!(msgs.len(), 3);
        assert!(msgs
            .iter()
            .all(|m| !m.to_string().contains("\"id\":\"t2\"")));
        assert_eq!(msgs[2]["role"], "user");
    }

    #[test]
    fn resume_after_assistant_text_appends_user_turn() {
        let mut h: Vec<Value> = history().iter().map(|m| (**m).clone()).collect();
        h.push(json!({"role": "assistant", "content": [{"type": "text", "text": "All done."}]}));
        let msgs = resume_messages(h, "one more fix");
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[4]["role"], "user");
        assert_eq!(msgs[3]["content"][0]["text"], "All done.");
    }

    #[test]
    fn resume_merges_into_plain_string_user_message() {
        let msgs = resume_messages(vec![json!({"role": "user", "content": "task"})], "more");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["content"][0]["text"], "task");
        assert!(msgs[0]["content"][1]["text"]
            .as_str()
            .unwrap()
            .ends_with("more"));
    }

    #[test]
    fn timeout_report_leads_with_latest_progress() {
        let mut s = SubagentState::new();
        s.partial_text = "First words of the run.".into();
        s.last_response_start = s.partial_text.len();
        s.partial_text
            .push_str("Rendering pass 2 now; sheet looks right.");
        s.tool_log = (0..30)
            .map(|i| format!("[tool_use]: bash — step {i}"))
            .collect();
        let r = timeout_report(3500, 30, &s, true);
        let preview: String = r.chars().take(300).collect();
        assert!(preview.contains("TIMED OUT after 3500s"));
        assert!(preview.contains("Rendering pass 2 now"), "{preview}");
        assert!(!r.contains("First words of the run"));
        assert!(r.contains("[last 20 of 30 tool calls]"));
        assert!(r.contains("step 29") && !r.contains("step 9\n"));
        assert!(r.contains("subagent_resume continues it"));
    }

    #[test]
    fn archive_paths_must_stay_under_the_history_root() {
        let root = tempfile::tempdir().unwrap();
        let hist = root.path().join("subagent-history");
        let sub = hist.join("proc");
        let ok = write_archive(&sub, "sa_1", &meta(), "failed", "", &history()).unwrap();
        assert_eq!(
            checked_archive_path_in(ok.to_str().unwrap(), &hist).unwrap(),
            ok.canonicalize().unwrap()
        );
        let outside = root.path().join("x.json");
        std::fs::write(&outside, "{}").unwrap();
        assert!(checked_archive_path_in(outside.to_str().unwrap(), &hist).is_err());
        let sneaky = format!("{}/../x.json", hist.display());
        assert!(checked_archive_path_in(&sneaky, &hist).is_err());
        assert!(checked_archive_path_in(&format!("{}/nope.json", sub.display()), &hist).is_err());
    }

    #[test]
    fn prune_removes_only_old_dirs() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("old")).unwrap();
        std::fs::create_dir(root.path().join("new")).unwrap();
        let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86400);
        std::fs::File::open(root.path().join("old"))
            .unwrap()
            .set_modified(old_time)
            .unwrap();
        prune_older_than(root.path(), std::time::Duration::from_secs(14 * 86400));
        assert!(!root.path().join("old").exists());
        assert!(root.path().join("new").exists());
    }
}
