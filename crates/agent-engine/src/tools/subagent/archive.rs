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
//! come from a process-wide counter, so one directory per process image
//! cannot collide (a daemon reload starts a new directory: the uuid suffix
//! is per image, and the reload carries the counter forward). Directories
//! older than [`KEEP_DAYS`] are pruned on first use.

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
    #[serde(default)]
    pub error: Option<String>,
    pub output: String,
    pub history: Vec<Value>,
    /// Wall-clock start (Unix ms) of the run's last accepted Anthropic
    /// request (see `SubagentState::cache_anchor_unix_ms`). Absent from
    /// archives written before cold-resume compaction, and from runs that
    /// never had a request accepted.
    #[serde(default)]
    pub cache_anchor_unix_ms: Option<u64>,
}

/// Borrowing twin of [`SubagentArchive`] so writing never deep-clones history.
#[derive(Serialize)]
struct SubagentArchiveRef<'a> {
    version: u32,
    handle_id: &'a str,
    meta: &'a SubagentArchiveMeta,
    status: &'a str,
    error: Option<&'a str>,
    output: &'a str,
    history: &'a [crate::SharedMessage],
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_anchor_unix_ms: Option<u64>,
}

/// This process's archive directory (created lazily by [`write_archive_anchored`]).
pub fn process_dir() -> PathBuf {
    process_dir_for_base(&crate::config::base_dir())
}

/// Resolve a base explicitly without mutating process environment (also used by tests).
pub(super) fn process_dir_for_base(base: &Path) -> PathBuf {
    static SUFFIX: OnceLock<String> = OnceLock::new();
    static PRUNED: OnceLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> = OnceLock::new();
    let suffix = SUFFIX.get_or_init(|| format!("{}-{}", std::process::id(), uuid::Uuid::new_v4()));
    let root = base.join("subagent-history");
    let mut pruned = PRUNED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if pruned.insert(root.clone()) {
        prune_older_than(
            &root,
            std::time::Duration::from_secs(KEEP_DAYS * 24 * 60 * 60),
        );
    }
    root.join(suffix)
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

/// [`write_archive_anchored`] with no cache anchor. Tests only: production
/// archives always record the anchor.
#[cfg(test)]
pub fn write_archive(
    dir: &Path,
    handle_id: &str,
    meta: &SubagentArchiveMeta,
    status: &str,
    error: Option<&str>,
    output: &str,
    history: &[crate::SharedMessage],
) -> std::io::Result<PathBuf> {
    write_archive_anchored(dir, handle_id, meta, status, error, output, history, None)
}

/// Write one worker's archive into `dir` (mode 0700 dir / 0600 file: tool
/// output can be sensitive). Atomic: written to a temp name, then renamed.
/// `cache_anchor_unix_ms` records when the run's prompt cache was last
/// written, so a later resume can tell whether replaying it would be a cache
/// read or a full rewrite.
#[allow(clippy::too_many_arguments)]
pub fn write_archive_anchored(
    dir: &Path,
    handle_id: &str,
    meta: &SubagentArchiveMeta,
    status: &str,
    error: Option<&str>,
    output: &str,
    history: &[crate::SharedMessage],
    cache_anchor_unix_ms: Option<u64>,
) -> std::io::Result<PathBuf> {
    create_private_dir(dir)?;
    let path = dir.join(format!("{handle_id}.json"));

    let body = serde_json::to_vec(&SubagentArchiveRef {
        version: ARCHIVE_VERSION,
        handle_id,
        meta,
        status,
        error,
        output,
        history,
        cache_anchor_unix_ms,
    })
    .map_err(std::io::Error::other)?;
    // NamedTempFile uses an unpredictable, create_new private file in this directory.
    use std::io::Write;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(&body)?;
    tmp.as_file().sync_all()?;
    tmp.persist(&path).map_err(|e| e.error)?;
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(path)
}

/// Write the plain-text transcript of a worker's evicted context window
/// (`runtime::worker_context`) next to its resume archive: the same private
/// directory (0700), a private file (0600), written atomically.
pub(crate) fn write_window_transcript(path: &Path, body: &str) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("transcript path has no directory"))?;
    create_private_dir(dir)?;
    use std::io::Write;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(body.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
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

/// Archive captured history, releasing it only after a successful durable write.
/// On failure keep the only full copy in state; cleanup must not discard it.
pub fn archive_finished_worker(
    state: &std::sync::RwLock<SubagentState>,
    handle_id: &str,
    dir: &Path,
) {
    let (meta, status, error, output, history, anchor) = {
        let mut s = state.write().unwrap_or_else(|p| p.into_inner());
        let Some(meta) = s.archive_meta.clone() else {
            if s.history.is_some() {
                s.archive_error =
                    Some("missing archive metadata; history retained in memory".into());
            }
            return;
        };
        // Startup failures may never emit MessageHistory; still write the
        // archive so the failure diagnostic survives, but mark it unresumable.
        let history = s.history.clone();
        (
            meta,
            s.status.as_str().to_string(),
            s.status.failure_reason().map(str::to_owned),
            s.partial_text.clone(),
            history,
            s.cache_anchor_unix_ms,
        )
    };
    let had_history = history.is_some();
    let history = history.unwrap_or_default();
    let outcome = write_archive_anchored(
        dir,
        handle_id,
        &meta,
        &status,
        error.as_deref(),
        &output,
        &history,
        anchor,
    );
    let mut s = state.write().unwrap_or_else(|p| p.into_inner());
    match outcome {
        Ok(path) => {
            s.archive_path = Some(path);
            s.archive_error = (!had_history).then(|| {
                "worker exited without a full history checkpoint; resume unavailable".into()
            });
            s.history = None;
        }
        Err(e) => {
            tracing::warn!("subagent {handle_id}: could not write resume archive: {e}");
            s.archive_error = Some(if had_history {
                format!("archive write failed: {e}; full history retained in memory")
            } else {
                format!("archive write failed: {e}; worker exited without a full history checkpoint")
            });
        }
    }
}

/// Explicit terminal collect/resume is also a recovery attempt after disk repair.
/// Callers hold the registry lock; finalization is already complete, so retries
/// cannot race the worker's original write or each other.
pub(super) fn retry_failed_archive(state: &std::sync::RwLock<SubagentState>, handle_id: &str) {
    let dir = {
        let s = state.read().unwrap_or_else(|p| p.into_inner());
        if s.finalization_ready
            && s.archive_error.is_some()
            && s.history.is_some()
            && s.archive_meta.is_some()
        {
            s.archive_dir.clone()
        } else {
            None
        }
    };
    if let Some(dir) = dir {
        archive_finished_worker(state, handle_id, &dir);
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

// ── Cold-resume compaction ──────────────────────────────────────────────────
//
// Workers run on the 5-minute cache TTL (#110). A resume after that window
// replays the whole archived transcript into a cold cache, rewriting every
// token at the cache-write price (measured: 250k–715k tokens, up to ~$3.50 per
// resume). Past a size threshold, a cold resume therefore starts from one
// compact user message instead: the original task, the last steps, the final
// report and the new instructions.

/// The TTL every worker request is written with (see
/// `apply_subagent_runtime_policy`).
pub const WORKER_CACHE_TTL_SECS: u64 = 300;
/// Treat the cache as cold this long before the TTL runs out: the resumed
/// request needs time to upload, and the provider's clock starts at its own
/// request start, not ours.
pub const COLD_MARGIN_SECS: u64 = 15;
const COLD_TASK_CHARS: usize = 40_000;
const COLD_TAIL_CHARS: usize = 60_000;
const COLD_REPORT_CHARS: usize = 40_000;
const COLD_TOTAL_CHARS: usize = 160_000;
const COLD_TOOL_INPUT_CHARS: usize = 600;
const COLD_TOOL_RESULT_CHARS: usize = 1_500;
/// One huge text turn must not eat the whole tail budget on its own.
const COLD_TAIL_TEXT_CHARS: usize = 6_000;

/// Unix milliseconds of a cache anchor's wall clock.
pub fn anchor_unix_ms(anchor: crate::runtime::keepwarm::CacheAnchor) -> Option<u64> {
    anchor
        .wall
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// Record a finished run's last accepted request on its state, for the
/// archive and for a later resume's cold check. Called after the drive
/// returns, before the finalizer archives.
pub(super) fn record_cache_anchor(
    state: &std::sync::RwLock<SubagentState>,
    anchor: Option<crate::runtime::keepwarm::CacheAnchor>,
) {
    if let Some(ms) = anchor.and_then(anchor_unix_ms) {
        state
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .cache_anchor_unix_ms = Some(ms);
    }
}

/// Age of a cache anchor at `now`. An anchor in the future (clock step)
/// counts as fresh.
pub fn anchor_age(anchor_unix_ms: u64, now: std::time::SystemTime) -> std::time::Duration {
    let at = std::time::UNIX_EPOCH + std::time::Duration::from_millis(anchor_unix_ms);
    now.duration_since(at).unwrap_or_default()
}

/// Cache age of an archived run: its recorded anchor, else the archive
/// file's mtime (written at finalization, after the last request), else
/// unknown.
pub fn archive_cache_age(
    anchor_unix_ms: Option<u64>,
    archive_path: Option<&Path>,
    now: std::time::SystemTime,
) -> Option<std::time::Duration> {
    if let Some(ms) = anchor_unix_ms {
        return Some(anchor_age(ms, now));
    }
    let mtime = std::fs::metadata(archive_path?).and_then(|m| m.modified()).ok()?;
    Some(now.duration_since(mtime).unwrap_or_default())
}

/// Whether a prompt cache of this age has (nearly) expired. Unknown age
/// counts as cold: a needless compaction loses detail the worker can re-read,
/// a needless full replay costs a full rewrite.
pub fn cache_is_cold(age: Option<std::time::Duration>) -> bool {
    age.is_none_or(|age| age.as_secs() >= WORKER_CACHE_TTL_SECS - COLD_MARGIN_SECS)
}

/// The same conservative estimator the context manager uses, over the
/// serialized messages.
pub fn estimate_history_tokens<T: Serialize>(history: &[T]) -> u64 {
    serde_json::to_string(history)
        .map(|s| crate::runtime::context::conservative_token_estimate(&s))
        .unwrap_or(0)
}

/// What [`cold_resume_messages`] needs to say about the run it replaces.
#[derive(Debug, Clone, Copy)]
pub struct ColdResumeInfo {
    pub messages: usize,
    pub est_tokens: u64,
    pub cache_age_secs: Option<u64>,
}

/// What a compacted resume reports back to the parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CompactedResume {
    pub messages: usize,
    pub est_tokens: u64,
    pub kept_est_tokens: u64,
    pub cache_age_secs: Option<u64>,
}

/// How `subagent_resume` may rebuild the opening context.
#[derive(Debug, Clone, Copy)]
pub struct ResumePolicy {
    /// `subagent_resume.full_history`: always replay everything.
    pub full_history: bool,
    /// `subagent_cold_resume_compact_tokens`; 0 disables compaction.
    pub compact_tokens: u64,
}

impl ResumePolicy {
    /// Today's behaviour: the full transcript, always.
    #[cfg(test)]
    pub const FULL: Self = Self {
        full_history: true,
        compact_tokens: 0,
    };
}

/// Opening messages of a resumed run: the full replay ([`resume_messages`]),
/// or, when the prior run's cache is cold and its history at least
/// `policy.compact_tokens`, the compact context of [`cold_resume_messages`].
pub fn plan_resume_messages(
    history: Vec<Value>,
    instructions: &str,
    cache_age: Option<std::time::Duration>,
    policy: ResumePolicy,
) -> (Vec<crate::SharedMessage>, Option<CompactedResume>) {
    if policy.full_history || policy.compact_tokens == 0 || !cache_is_cold(cache_age) {
        return (resume_messages(history, instructions), None);
    }
    let est_tokens = estimate_history_tokens(&history);
    if est_tokens < policy.compact_tokens {
        return (resume_messages(history, instructions), None);
    }
    let info = ColdResumeInfo {
        messages: history.len(),
        est_tokens,
        cache_age_secs: cache_age.map(|a| a.as_secs()),
    };
    match cold_resume_messages(&history, instructions, &info) {
        Some(messages) => {
            let kept_est_tokens = estimate_history_tokens(&messages);
            (
                messages,
                Some(CompactedResume {
                    messages: info.messages,
                    est_tokens,
                    kept_est_tokens,
                    cache_age_secs: info.cache_age_secs,
                }),
            )
        }
        None => (resume_messages(history, instructions), None),
    }
}

/// Cut `text` to `max` chars with a marker saying how much went.
fn cut_chars(text: &str, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push_str(&format!(" [… cut {} chars]", total - max));
    out
}

/// Plain text of a message's `content` (string, or its text blocks).
fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"].as_str() == Some("text"))
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Text of a tool_result's `content` (string or blocks; images named only).
fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().map(str::to_owned),
                Some("image") => Some("[image]".to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// One line per kept block of `message`, in order. Thinking is dropped: it
/// is the model's scratch work, and signed blocks cannot be re-sent out of
/// their original turn anyway.
fn render_message_lines(message: &Value) -> Vec<String> {
    let role = message["role"].as_str().unwrap_or("user");
    let mut lines = Vec::new();
    match &message["content"] {
        Value::String(s) if !s.trim().is_empty() => {
            lines.push(format!("{role}: {}", cut_chars(s, COLD_TAIL_TEXT_CHARS)));
        }
        Value::Array(blocks) => {
            for block in blocks {
                match block["type"].as_str() {
                    Some("text") => {
                        let text = block["text"].as_str().unwrap_or("");
                        if !text.trim().is_empty() {
                            lines.push(format!(
                                "{role}: {}",
                                cut_chars(text, COLD_TAIL_TEXT_CHARS)
                            ));
                        }
                    }
                    Some("tool_use") => {
                        let name = block["name"].as_str().unwrap_or("?");
                        let input = block["input"].to_string();
                        lines.push(format!(
                            "→ {name}: {}",
                            cut_chars(&input, COLD_TOOL_INPUT_CHARS)
                        ));
                    }
                    Some("tool_result") => {
                        let text = tool_result_text(&block["content"]);
                        let error = if block["is_error"].as_bool() == Some(true) {
                            " (error)"
                        } else {
                            ""
                        };
                        lines.push(format!(
                            "← result{error}: {}",
                            cut_chars(&text, COLD_TOOL_RESULT_CHARS)
                        ));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    lines
}

/// A compact opening context for resuming a worker whose prompt cache has
/// expired: exactly one user message (so no tool_use/tool_result pairing can
/// break) holding the original task, the tail of the run as plain text, the
/// final report and the new instructions. `None` when it cannot help: no
/// assistant turn to report, or no smaller than the full history.
pub fn cold_resume_messages(
    history: &[Value],
    instructions: &str,
    info: &ColdResumeInfo,
) -> Option<Vec<crate::SharedMessage>> {
    let last_assistant = history
        .iter()
        .rposition(|m| m["role"].as_str() == Some("assistant"))?;
    let first_user = history
        .iter()
        .position(|m| m["role"].as_str() == Some("user"))
        .filter(|&i| i < last_assistant);

    let age = match info.cache_age_secs {
        Some(secs) => format!(
            "its prompt cache expired (last written {} min ago)",
            secs.div_ceil(60)
        ),
        None => "its prompt cache has expired".to_string(),
    };
    let header = format!(
        "[Resumed by the orchestrator. Your earlier run in this task ({} messages, about {} \
         tokens) is not replayed in full: {age}, so replaying it would rewrite all {} tokens. \
         Below are your original task, the last steps of that run and your final report, then \
         the new instructions. Re-read any file you need; don't assume details that aren't \
         shown here.]",
        info.messages, info.est_tokens, info.est_tokens
    );
    let task = first_user
        .map(|i| content_text(&history[i]["content"]))
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| "(not recorded)".to_string());
    let task = format!(
        "## Your original task\n\n{}",
        cut_chars(&task, COLD_TASK_CHARS)
    );
    let final_message = &history[last_assistant];
    let mut report = content_text(&final_message["content"]);
    if report.trim().is_empty() {
        // The run stopped mid-step (e.g. timed out on a tool call): show
        // what it was doing instead of an empty report.
        report = render_message_lines(final_message).join("\n");
    }
    if report.trim().is_empty() {
        report = "(no text)".to_string();
    }
    let report = format!(
        "## Your final report\n\n{}",
        cut_chars(&report, COLD_REPORT_CHARS)
    );
    let note = format!(
        "[Resumed by the orchestrator. Everything above summarizes your own earlier work in \
         this task; continue from where it stopped.]\n\n{instructions}"
    );

    let fixed: usize = [&header, &task, &report, &note]
        .iter()
        .map(|s| s.chars().count())
        .sum();
    let budget = COLD_TAIL_CHARS.min(COLD_TOTAL_CHARS.saturating_sub(fixed + 64));
    let mut kept: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut cut = false;
    'walk: for (i, message) in history.iter().enumerate().rev() {
        if Some(i) == first_user || i == last_assistant {
            continue;
        }
        for line in render_message_lines(message).into_iter().rev() {
            let len = line.chars().count() + 1;
            if used + len > budget {
                cut = true;
                break 'walk;
            }
            used += len;
            kept.push(line);
        }
    }
    kept.reverse();
    let mut tail = String::from("## The last steps of your earlier run\n\n");
    if cut {
        tail.push_str("(earlier steps omitted)\n");
    }
    if kept.is_empty() && !cut {
        tail.push_str("(none)\n");
    }
    for line in kept {
        tail.push_str(&line);
        tail.push('\n');
    }

    let blocks = [header, task, tail, report, note];
    // Not smaller than replaying everything: the compaction only loses detail.
    if crate::runtime::context::conservative_token_estimate(&blocks.concat()) >= info.est_tokens {
        return None;
    }
    let content: Vec<Value> = blocks
        .into_iter()
        .map(|text| serde_json::json!({"type": "text", "text": text}))
        .collect();
    Some(vec![std::sync::Arc::new(serde_json::json!({
        "role": "user",
        "content": content
    }))])
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

/// Backdate a test file or directory. `File::open` is read-only: Unix lets a
/// read-only handle change times, Windows wants write-attribute access, and
/// opens a directory only with backup semantics.
#[cfg(test)]
fn set_mtime_for_test(path: &Path, time: std::time::SystemTime) {
    let mut options = std::fs::File::options();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Win32 FILE_WRITE_ATTRIBUTES and FILE_FLAG_BACKUP_SEMANTICS.
        const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        // `access_mode` replaces the read flag above.
        options
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    }
    options.open(path).unwrap().set_modified(time).unwrap();
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
            cwd: None,
            thinking: None,
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
        let path = write_archive(
            &sub,
            "sa_7",
            &meta(),
            "timed_out",
            Some("worker timed out"),
            "out",
            &history(),
        )
        .unwrap();
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
    fn launch_roots_and_early_failure_seeds_survive_finalization_and_retry() {
        let roots = tempfile::tempdir().unwrap();
        let base_a = roots.path().join("a");
        let base_b = roots.path().join("b");
        let dir_a = process_dir_for_base(&base_a);
        let dir_b = process_dir_for_base(&base_b);
        assert_ne!(dir_a, dir_b);
        assert_eq!(dir_a.file_name(), dir_b.file_name());
        assert_eq!(process_dir_for_base(&base_a), dir_a);

        // Both launch forms, before any runtime emits MessageHistory. The resumed
        // seed includes actual tool-use/result pairs, not a summary of prior work.
        for (index, seed) in [
            vec![Arc::new(
                json!({"role": "user", "content": "original opening task"}),
            )],
            resume_messages(
                history().iter().map(|m| (**m).clone()).collect(),
                "continue",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let captured = dir_a.join(index.to_string());
            std::fs::create_dir_all(&dir_a).unwrap();
            std::fs::write(&captured, b"block archive directory").unwrap();
            let state = Arc::new(RwLock::new(SubagentState::new()));
            {
                let mut s = state.write().unwrap();
                s.archive_meta = Some(meta());
                s.archive_dir = Some(captured.clone());
                s.history = Some(seed.clone());
                // Simulate startup failure before the stream exists.
                s.status = crate::runtime::subagent::SubagentStatus::Failed("init failed".into());
                assert_eq!(
                    s.public_status(),
                    crate::runtime::subagent::SubagentStatus::Running
                );
            }
            // Resolving a later launch under B must not redirect A's finalizer.
            assert_eq!(process_dir_for_base(&base_b), dir_b);
            super::super::finalize::finalize_subagent(
                &state,
                None,
                "sa_seed",
                1,
                "inline",
                std::time::Instant::now(),
                None,
            );
            {
                let s = state.read().unwrap();
                assert!(s.finalization_ready);
                assert!(s.archive_error.is_some());
                assert_eq!(s.history.as_ref().unwrap(), &seed);
            }
            std::fs::remove_file(&captured).unwrap();
            retry_failed_archive(&state, "sa_seed");
            let s = state.read().unwrap();
            assert!(s.archive_error.is_none());
            assert!(s.history.is_none());
            let path = s.archive_path.as_ref().unwrap();
            assert!(path.starts_with(&captured));
            assert_eq!(
                read_archive(path).unwrap().history,
                seed.iter().map(|m| (**m).clone()).collect::<Vec<_>>()
            );
            assert!(checked_archive_path_in(
                path.to_str().unwrap(),
                &base_b.join("subagent-history")
            )
            .is_err());
        }
        assert!(
            !dir_b.exists(),
            "neither finalizer nor retry writes to the later root"
        );
    }

    #[test]
    fn review_202_failed_archive_keeps_recovery_history() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("not-a-directory");
        std::fs::write(&blocked, b"blocked").unwrap();
        let state = RwLock::new(SubagentState::new());
        {
            let mut s = state.write().unwrap();
            s.history = Some(history());
            s.archive_meta = Some(meta());
        }
        archive_finished_worker(&state, "sa_review", &blocked);
        {
            let s = state.read().unwrap();
            assert_eq!(s.history.as_ref().unwrap(), &history());
            assert!(s.archive_error.is_some());
            assert!(s.archive_path.is_none());
        }
        // Explicit recovery can retry the same stable archive id without losing content.
        archive_finished_worker(&state, "sa_review", dir.path());
        let s = state.read().unwrap();
        assert!(s.archive_error.is_none());
        assert!(s.history.is_none());
        assert_eq!(
            read_archive(s.archive_path.as_ref().unwrap())
                .unwrap()
                .history
                .len(),
            3
        );
    }

    #[test]
    fn missing_metadata_does_not_take_history() {
        let dir = tempfile::tempdir().unwrap();
        let state = RwLock::new(SubagentState::new());
        state.write().unwrap().history = Some(history());
        archive_finished_worker(&state, "sa_missing", dir.path());
        let s = state.read().unwrap();
        assert_eq!(s.history.as_ref().unwrap(), &history());
        assert!(s.archive_error.is_some());
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
    fn finalizer_without_history_preserves_startup_error() {
        let dir = tempfile::tempdir().unwrap();
        let state = RwLock::new(SubagentState::new());
        {
            let mut state = state.write().unwrap();
            state.archive_meta = Some(meta());
            state.status =
                crate::runtime::subagent::SubagentStatus::Failed("startup failed".into());
        }
        archive_finished_worker(&state, "sa_10", dir.path());
        let path = state.read().unwrap().archive_path.clone().unwrap();
        let archive = read_archive(&path).unwrap();
        assert_eq!(archive.error.as_deref(), Some("startup failed"));
        assert!(archive.history.is_empty());
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
        let ok = write_archive(
            &sub,
            "sa_1",
            &meta(),
            "failed",
            Some("failed"),
            "",
            &history(),
        )
        .unwrap();
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
        set_mtime_for_test(&root.path().join("old"), old_time);
        prune_older_than(root.path(), std::time::Duration::from_secs(14 * 86400));
        assert!(!root.path().join("old").exists());
        assert!(root.path().join("new").exists());
    }
}

#[cfg(test)]
mod cold_resume_tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, RwLock};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn unix_ms(t: SystemTime) -> u64 {
        t.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
    }

    fn meta() -> SubagentArchiveMeta {
        SubagentArchiveMeta {
            agent_name: "inline".into(),
            model: "anthropic/claude-opus-5-5".into(),
            system_prompt: "be a builder".into(),
            timeout_secs: 3600,
            cwd: None,
            thinking: None,
        }
    }

    /// Task, `rounds` tool steps with bulky results, then a final report.
    fn big_history(rounds: usize) -> Vec<Value> {
        let mut h = vec![json!({"role": "user", "content": "TASK-MARK build the plate"})];
        for i in 0..rounds {
            h.push(json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "SECRET-THOUGHT", "signature": "sig"},
                {"type": "text", "text": format!("step {i}")},
                {"type": "tool_use", "id": format!("t{i}"), "name": "bash",
                 "input": {"command": "x".repeat(2000)}}
            ]}));
            h.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": format!("t{i}"),
                 "content": "y".repeat(5000), "is_error": i % 2 == 0}
            ]}));
        }
        h.push(json!({"role": "assistant", "content": [
            {"type": "redacted_thinking", "data": "REDACTED-MARK"},
            {"type": "text", "text": "FINAL-REPORT all done"}
        ]}));
        h
    }

    fn info_for(history: &[Value]) -> ColdResumeInfo {
        ColdResumeInfo {
            messages: history.len(),
            est_tokens: estimate_history_tokens(history),
            cache_age_secs: Some(3600),
        }
    }

    fn joined_text(messages: &[crate::SharedMessage]) -> String {
        messages[0]["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["text"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn cold_detection_from_anchor_and_archive_mtime() {
        let now = SystemTime::now();
        let fresh = archive_cache_age(Some(unix_ms(now - Duration::from_secs(10))), None, now);
        assert!(!cache_is_cold(fresh), "fresh anchor is warm");
        let edge = archive_cache_age(Some(unix_ms(now - Duration::from_secs(280))), None, now);
        assert!(!cache_is_cold(edge), "inside the margin is still warm");
        let old = archive_cache_age(Some(unix_ms(now - Duration::from_secs(290))), None, now);
        assert!(cache_is_cold(old), "older than TTL minus margin is cold");
        let future = archive_cache_age(Some(unix_ms(now + Duration::from_secs(60))), None, now);
        assert!(!cache_is_cold(future), "clock step forward counts as fresh");

        let dir = tempfile::tempdir().unwrap();
        let path = write_archive(dir.path(), "sa_m", &meta(), "completed", None, "", &[]).unwrap();
        let fresh_mtime = archive_cache_age(None, Some(&path), SystemTime::now());
        assert!(!cache_is_cold(fresh_mtime), "no anchor + fresh mtime is warm");
        set_mtime_for_test(&path, SystemTime::now() - Duration::from_secs(3600));
        let old_mtime = archive_cache_age(None, Some(&path), SystemTime::now());
        assert!(cache_is_cold(old_mtime), "no anchor + old mtime is cold");
        // The anchor wins over the mtime.
        let anchored = archive_cache_age(Some(unix_ms(SystemTime::now())), Some(&path), now);
        assert!(!cache_is_cold(anchored));
        // Nothing to date it by: cold.
        assert_eq!(archive_cache_age(None, None, now), None);
        assert!(cache_is_cold(None));
        let missing = dir.path().join("gone.json");
        assert!(cache_is_cold(archive_cache_age(None, Some(&missing), now)));
    }

    #[test]
    fn anchor_is_recorded_as_unix_ms() {
        let anchor = crate::runtime::keepwarm::CacheAnchor {
            mono: tokio::time::Instant::now(),
            wall: UNIX_EPOCH + Duration::from_millis(5_000),
        };
        assert_eq!(anchor_unix_ms(anchor), Some(5_000));
        let state = RwLock::new(SubagentState::new());
        record_cache_anchor(&state, None);
        assert_eq!(state.read().unwrap().cache_anchor_unix_ms, None);
        record_cache_anchor(&state, Some(anchor));
        assert_eq!(state.read().unwrap().cache_anchor_unix_ms, Some(5_000));
    }

    #[test]
    fn threshold_and_overrides_pick_full_or_compacted() {
        let cold = Some(Duration::from_secs(600));
        let warm = Some(Duration::from_secs(30));
        let on = ResumePolicy {
            full_history: false,
            compact_tokens: 1_000,
        };
        let big = big_history(10);
        assert!(estimate_history_tokens(&big) > 1_000);

        let (messages, compacted) = plan_resume_messages(big.clone(), "next", cold, on);
        let c = compacted.expect("big + cold compacts");
        assert_eq!(messages.len(), 1);
        assert_eq!(c.messages, big.len());
        assert_eq!(c.est_tokens, estimate_history_tokens(&big));
        assert!(c.kept_est_tokens < c.est_tokens);
        assert_eq!(c.cache_age_secs, Some(600));

        let small = vec![
            json!({"role": "user", "content": "task"}),
            json!({"role": "assistant", "content": "done"}),
        ];
        let (messages, compacted) = plan_resume_messages(small.clone(), "next", cold, on);
        assert!(compacted.is_none(), "small + cold replays in full");
        assert_eq!(messages.len(), 3);

        let (messages, compacted) = plan_resume_messages(big.clone(), "next", warm, on);
        assert!(compacted.is_none(), "big + warm replays in full");
        assert_eq!(messages.len(), big.len() + 1);

        let off = ResumePolicy {
            full_history: false,
            compact_tokens: 0,
        };
        assert!(plan_resume_messages(big.clone(), "next", cold, off).1.is_none());
        assert!(plan_resume_messages(big.clone(), "next", None, off).1.is_none());

        let full = ResumePolicy {
            full_history: true,
            compact_tokens: 1_000,
        };
        assert!(plan_resume_messages(big.clone(), "next", cold, full).1.is_none());
        assert!(plan_resume_messages(big, "next", cold, ResumePolicy::FULL).1.is_none());
    }

    #[test]
    fn compact_context_is_one_user_message_in_order() {
        let history = big_history(3);
        let messages = cold_resume_messages(&history, "NEXT-INSTR go on", &info_for(&history))
            .expect("compacts");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        let blocks = messages[0]["content"].as_array().unwrap();
        assert!(blocks.iter().all(|b| b["type"] == "text"));
        let text = joined_text(&messages);
        let pos = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle} missing from {text}"))
        };
        assert!(pos("[Resumed by the orchestrator.") < pos("## Your original task"));
        assert!(pos("## Your original task") < pos("TASK-MARK"));
        assert!(pos("TASK-MARK") < pos("## The last steps of your earlier run"));
        assert!(pos("## The last steps of your earlier run") < pos("assistant: step 0"));
        assert!(pos("assistant: step 0") < pos("assistant: step 2"));
        assert!(pos("assistant: step 2") < pos("## Your final report"));
        assert!(pos("## Your final report") < pos("FINAL-REPORT all done"));
        assert!(pos("FINAL-REPORT all done") < pos("NEXT-INSTR go on"));
        assert!(text.contains(&format!("{} messages", history.len())));
        assert!(text.contains("60 min ago"), "{text}");
        assert!(!text.contains("SECRET-THOUGHT"), "thinking dropped");
        assert!(!text.contains("REDACTED-MARK"), "redacted thinking dropped");
        assert!(!text.contains("(earlier steps omitted)"), "nothing cut");
        // Tool calls and results rendered and cut.
        assert!(text.contains("→ bash: {\"command\":\"xxx"));
        assert!(text.contains("← result (error): yyy"));
        assert!(text.contains("← result: yyy"));
        assert!(text.contains("[… cut "));
        assert!(!text.contains(&"x".repeat(601)));
        assert!(!text.contains(&"y".repeat(1501)));
        // The task and the final report sit in their own sections, not the tail.
        assert_eq!(text.matches("TASK-MARK").count(), 1);
        assert_eq!(text.matches("FINAL-REPORT").count(), 1);
    }

    #[test]
    fn long_history_tail_is_budgeted_and_capped() {
        let history = big_history(200);
        let messages =
            cold_resume_messages(&history, "next", &info_for(&history)).expect("compacts");
        let text = joined_text(&messages);
        assert!(text.contains("(earlier steps omitted)"));
        assert!(text.contains("assistant: step 199"), "newest steps kept");
        assert!(!text.contains("assistant: step 0\n"), "oldest steps dropped");
        let total: usize = messages[0]["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["text"].as_str().unwrap().chars().count())
            .sum();
        assert!(total <= COLD_TOTAL_CHARS, "{total}");
        let tail_start = text.find("## The last steps").unwrap();
        let tail_end = text.find("## Your final report").unwrap();
        let tail_chars = text[tail_start..tail_end].chars().count();
        assert!(tail_chars <= COLD_TAIL_CHARS + 200, "{tail_chars}");
    }

    #[test]
    fn huge_task_and_report_are_cut() {
        let history = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "t".repeat(100_000)}]}),
            json!({"role": "assistant", "content": "r".repeat(100_000)}),
        ];
        let messages =
            cold_resume_messages(&history, "next", &info_for(&history)).expect("compacts");
        let text = joined_text(&messages);
        assert!(text.contains("[… cut 60000 chars]"));
        assert!(!text.contains(&"t".repeat(40_001)));
        assert!(!text.contains(&"r".repeat(40_001)));
    }

    #[test]
    fn fallback_without_assistant_or_when_not_smaller() {
        let no_assistant = vec![json!({"role": "user", "content": "x".repeat(10_000)})];
        assert!(cold_resume_messages(&no_assistant, "next", &info_for(&no_assistant)).is_none());
        let tiny = vec![
            json!({"role": "user", "content": "task"}),
            json!({"role": "assistant", "content": "done"}),
        ];
        assert!(
            cold_resume_messages(&tiny, "next", &info_for(&tiny)).is_none(),
            "header alone outweighs a tiny history"
        );
        // And through the planner: a fallback is a full replay.
        let policy = ResumePolicy {
            full_history: false,
            compact_tokens: 1,
        };
        let (messages, compacted) =
            plan_resume_messages(no_assistant, "next", Some(Duration::from_secs(900)), policy);
        assert!(compacted.is_none());
        assert_eq!(messages.len(), 1);
        assert!(messages[0]["content"].is_array(), "instructions merged");
    }

    #[test]
    fn archive_round_trips_cache_anchor_and_reads_old_archives() {
        let dir = tempfile::tempdir().unwrap();
        let history: Vec<crate::SharedMessage> =
            vec![Arc::new(json!({"role": "user", "content": "task"}))];
        let path = write_archive_anchored(
            dir.path(),
            "sa_a",
            &meta(),
            "completed",
            None,
            "out",
            &history,
            Some(1_234),
        )
        .unwrap();
        assert_eq!(read_archive(&path).unwrap().cache_anchor_unix_ms, Some(1_234));
        let path = write_archive(dir.path(), "sa_b", &meta(), "completed", None, "", &history)
            .unwrap();
        assert_eq!(read_archive(&path).unwrap().cache_anchor_unix_ms, None);

        let old = json!({
            "version": ARCHIVE_VERSION,
            "handle_id": "sa_old",
            "meta": meta(),
            "status": "completed",
            "error": null,
            "output": "",
            "history": [{"role": "user", "content": "task"}]
        });
        let old_path = dir.path().join("sa_old.json");
        std::fs::write(&old_path, serde_json::to_vec(&old).unwrap()).unwrap();
        let a = read_archive(&old_path).unwrap();
        assert_eq!(a.cache_anchor_unix_ms, None);
        assert_eq!(a.history.len(), 1);

        // The finalizer carries the state's anchor into the archive.
        let state = RwLock::new(SubagentState::new());
        {
            let mut s = state.write().unwrap();
            s.archive_meta = Some(meta());
            s.history = Some(history);
            s.cache_anchor_unix_ms = Some(42);
        }
        archive_finished_worker(&state, "sa_c", dir.path());
        let written = state.read().unwrap().archive_path.clone().expect("archived");
        assert_eq!(read_archive(&written).unwrap().cache_anchor_unix_ms, Some(42));
    }
}
