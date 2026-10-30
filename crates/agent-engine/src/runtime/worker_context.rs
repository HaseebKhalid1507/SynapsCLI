//! Automatic context rollover for subagent workers.
//!
//! A worker is one brief followed by hundreds of assistant / tool_result
//! rounds, and before this its context only ever grew: every round re-read the
//! whole history at the cache-read price, and a worker that reached the window
//! died on a terminal "prompt is too long". The foreground rollover
//! (`continuation`) does not fit workers: its successor keeps user requests
//! (a worker has one), it seals evicted history into the session's memory
//! backend, and it needs a frontend to acknowledge the new head.
//!
//! This is the worker's host-only variant. The decision is the foreground's
//! policy engine unchanged (`assess_context`, `ContextThresholds`,
//! `target_tokens`, `successor_budget`); only the successor and where evicted
//! history goes differ:
//!
//! - the successor keeps the brief (message 0) verbatim, later orchestrator
//!   instructions verbatim (within a share of the target), a host-written note,
//!   and the most recent rounds as a contiguous suffix cut at an assistant
//!   message that no tool call/result group straddles. Assistant messages are
//!   never edited, so signed thinking blocks travel verbatim;
//! - the evicted messages are written as a plain-text transcript into the
//!   worker's archive directory (`<handle>.window-<N>.md`, 0600), the same
//!   place, permissions and retention as the finalize archive. The worker can
//!   grep/read it; nothing goes to the memory backend.
//!
//! See docs/design/subagent-rollover.md.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_core::config::{ContextManagementConfig, ContextManagementMode, SynapsConfig};
use agent_core::core::context_policy::ContextThresholds;
use serde_json::{json, Value};

use super::continuation::{
    message_tokens, neutral_note, tool_groups, SuccessorBudget, TokenCalibration,
    DROPPED_INDEX_TOKENS, PINNED_SHARE_PERCENT,
};
use crate::SharedMessage;

/// Host-only key on the rollover note (stripped from every provider wire like
/// every `_synaps_*` key): `{"schema", "window", "transcripts": [...]}`.
pub(crate) const MARKER_KEY: &str = "_synaps_worker_context";
pub(crate) const SCHEMA: &str = "synaps-worker-context/1";

/// Longest input preview per tool call in the note's index.
const INDEX_PREVIEW_CHARS: usize = 100;
/// Room reserved for the note around its bounded index.
const NOTE_FRAME_TOKENS: u64 = 600;
/// Earlier windows the note lists by path; older ones are named by directory
/// so a long-lived worker's note stays bounded.
const NOTE_LISTED_WINDOWS: usize = 12;
/// A rollover must shrink the estimate by more than this to be worth a cache
/// rewrite (the foreground's no-shrink rule).
const MIN_SHRINK_TOKENS: u64 = 1024;

/// What config says about worker rollover. Set on every worker runtime by
/// `apply_subagent_runtime_policy`; inert until a transcript sink is attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerContextSettings {
    pub enabled: bool,
    pub rollover_tokens: u64,
}

impl WorkerContextSettings {
    pub fn from_config(config: &SynapsConfig) -> Self {
        Self {
            enabled: config.subagent_context_management == ContextManagementMode::Auto,
            rollover_tokens: config.subagent_rollover_tokens,
        }
    }

    /// The policy for a model window: the foreground's policy config with
    /// the rollover band lowered to `rollover_tokens` (the window's own band
    /// wins when it is lower), pressure and target derived exactly as the
    /// foreground derives them, and no Plan/WrapUp extensions (workers report
    /// no phases). `None` when off or the window cannot hold a valid policy.
    pub fn policy(&self, window: u64) -> Option<WorkerPolicy> {
        if !self.enabled {
            return None;
        }
        let base = ContextManagementConfig {
            mode: ContextManagementMode::Auto,
            finish_rounds: 0,
            ..Default::default()
        };
        let band = ContextThresholds::resolve(&base, window).ok()?.rollover_tokens;
        let config = ContextManagementConfig {
            rollover_tokens: Some(self.rollover_tokens.min(band).max(2)),
            ..base
        };
        let thresholds = ContextThresholds::resolve(&config, window).ok()?;
        Some(WorkerPolicy { config, thresholds })
    }
}

/// A resolved worker policy for one model window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerPolicy {
    pub config: ContextManagementConfig,
    pub thresholds: ContextThresholds,
}

impl WorkerPolicy {
    pub fn target_tokens(&self) -> u64 {
        self.thresholds.target_tokens(&self.config)
    }

    /// Message budget for the successor (estimator units), exactly as the
    /// foreground sizes its successor.
    pub fn successor_budget(
        &self,
        overhead_tokens: u64,
        calibration: Option<TokenCalibration>,
    ) -> SuccessorBudget {
        super::continuation::successor_budget(
            self.target_tokens(),
            self.thresholds.pressure_tokens,
            overhead_tokens,
            calibration,
        )
    }

    /// Whether rolling over at hard capacity can make real room. Hard
    /// capacity is the footprint `context_window - required_next_round`,
    /// where the next round's reserves are its output envelope, thinking, the
    /// next tool result and the safety margin. When that line is at or below
    /// this policy's successor target the window is dominated by its
    /// reserves: no successor of the size the policy aims for passes hard
    /// admission, and any successor the window does hold is a fraction of
    /// it, back at hard capacity a few rounds later. On a 200k Opus window
    /// the reserves are about 168k (128k of it the output envelope), so hard
    /// capacity sat near 32k against a 93k target, and workers rolled over
    /// every few tool calls to successors of about 9k. `false`: do not roll
    /// over at hard capacity; send the request as before worker rollover
    /// existed and let the provider judge it.
    pub fn hard_capacity_has_room(&self, context_window: u64, required_next_round: u64) -> bool {
        context_window.saturating_sub(required_next_round) > self.target_tokens()
    }
}

/// At hard capacity the configured target may leave the successor little room
/// to grow (a window whose reserves put hard capacity below the rollover band
/// reaches it first; see [`WorkerPolicy::hard_capacity_has_room`] for one
/// that cannot hold the target at all). Aim for two thirds of the history
/// room the next round leaves, so the successor both fits and can grow.
pub fn hard_capacity_budget(
    budget: SuccessorBudget,
    context_window: u64,
    required_next_round: u64,
    overhead_tokens: u64,
) -> SuccessorBudget {
    let room = context_window
        .saturating_sub(required_next_round)
        .saturating_sub(overhead_tokens);
    SuccessorBudget {
        history_tokens: budget.history_tokens.min(room * 2 / 3),
    }
}

/// Called after every successful worker rollover (the spawn path updates the
/// worker's state and its subagent panel line).
pub type RolloverObserver = Arc<dyn Fn(&WorkerRolloverReport) + Send + Sync>;

/// Where one worker's evicted history goes. Attached per spawn: the archive
/// directory and the worker's handle id.
#[derive(Clone)]
pub struct WorkerTranscriptSink {
    pub dir: PathBuf,
    pub stem: String,
    pub observer: Option<RolloverObserver>,
}

impl std::fmt::Debug for WorkerTranscriptSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerTranscriptSink")
            .field("dir", &self.dir)
            .field("stem", &self.stem)
            .field("observer", &self.observer.is_some())
            .finish()
    }
}

/// A worker runtime's rollover wiring. Active only with both halves: config
/// on, and a sink to preserve what is evicted. Without a sink nothing is
/// evicted (nothing may be lost), so the worker behaves as before.
#[derive(Debug, Clone, Default)]
pub struct WorkerContext {
    pub settings: Option<WorkerContextSettings>,
    pub sink: Option<WorkerTranscriptSink>,
}

impl WorkerContext {
    /// The resolved policy and sink, when rollover is active for `window`.
    pub fn resolve(&self, window: u64) -> Option<(WorkerPolicy, WorkerTranscriptSink)> {
        let policy = self.settings?.policy(window)?;
        Some((policy, self.sink.clone()?))
    }
}

/// What one rollover did (observer, notice, status).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRolloverReport {
    /// The window the worker continues in (2 after the first rollover).
    pub window: u64,
    /// Estimated full request footprint before and after.
    pub before_tokens: u64,
    pub after_tokens: u64,
    pub evicted_messages: usize,
    /// The transcript this rollover wrote.
    pub transcript: PathBuf,
    /// Every earlier window's transcript, oldest first, this one last.
    pub transcripts: Vec<PathBuf>,
}

impl WorkerRolloverReport {
    /// The worker stream's notice.
    pub fn notice(&self) -> String {
        format!(
            "Worker context rolled over to window {}: ~{}k → ~{}k tokens; {} earlier messages saved to {}.",
            self.window,
            self.before_tokens / 1000,
            self.after_tokens / 1000,
            self.evicted_messages,
            self.transcript.display()
        )
    }

    /// One line for the subagent panel.
    pub fn step(&self) -> String {
        format!(
            "↻ context window {}: ~{}k → ~{}k tokens",
            self.window,
            self.before_tokens / 1000,
            self.after_tokens / 1000
        )
    }
}

/// An earlier window's transcript, as recorded in the note's metadata.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TranscriptRef {
    pub window: u64,
    pub path: String,
    pub messages: usize,
}

/// The current window (1 before any rollover) and every recorded earlier
/// transcript, from the host metadata on `messages`: the rollover note, or
/// the single message of a cold resume (which carries it on message 0).
pub fn window_state(messages: &[SharedMessage]) -> (u64, Vec<TranscriptRef>) {
    let mut window = 1;
    let mut seen = BTreeSet::new();
    let mut transcripts = Vec::new();
    for marker in messages
        .iter()
        .filter_map(|m| m.get(MARKER_KEY))
        .filter(|m| m["schema"] == SCHEMA)
    {
        window = window.max(marker["window"].as_u64().unwrap_or(1));
        for t in marker["transcripts"].as_array().into_iter().flatten() {
            if let Ok(t) = serde_json::from_value::<TranscriptRef>(t.clone()) {
                if seen.insert((t.window, t.path.clone())) {
                    transcripts.push(t);
                }
            }
        }
    }
    transcripts.sort_by_key(|t| t.window);
    (window, transcripts)
}

/// The host metadata to carry onto a message that replaces a rolled history
/// (cold resume), so later rollovers keep numbering and listing earlier
/// windows. `None` when `history` never rolled over.
pub fn carried_marker(history: &[Value]) -> Option<Value> {
    let shared: Vec<SharedMessage> = history
        .iter()
        .filter(|m| m.get(MARKER_KEY).is_some())
        .map(|m| Arc::new(m.clone()))
        .collect();
    if shared.is_empty() {
        return None;
    }
    let (window, transcripts) = window_state(&shared);
    Some(json!({"schema": SCHEMA, "window": window, "transcripts": transcripts}))
}

fn is_note(message: &Value) -> bool {
    message.get(MARKER_KEY).is_some()
}

fn has_tool_result(message: &Value) -> bool {
    message["content"]
        .as_array()
        .is_some_and(|b| b.iter().any(|b| b["type"] == "tool_result"))
}

/// Host bookkeeping that never travels as an instruction.
fn is_host_bookkeeping(message: &Value) -> bool {
    is_note(message)
        || message.get("_synaps_context").is_some()
        || crate::engine::interrupt::is_tool_in_flight_note(message)
        || message["content"]
            .as_str()
            .is_some_and(crate::engine::interrupt::is_interruption_marker)
}

/// An orchestrator instruction in the evicted range, as it should travel:
/// a plain user message as is (images included); from a tool_result carrier
/// with text (resume merges its instructions there) only the text blocks,
/// since a tool_result cannot travel without its tool_use.
fn carried_instruction(message: &SharedMessage) -> Option<SharedMessage> {
    if message["role"] != "user" || is_host_bookkeeping(message) {
        return None;
    }
    match &message["content"] {
        Value::String(s) if !s.trim().is_empty() => Some(Arc::clone(message)),
        Value::Array(blocks) if !has_tool_result(message) => (!blocks.is_empty())
            .then(|| Arc::clone(message)),
        Value::Array(blocks) => {
            let text: Vec<Value> = blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter(|b| b["text"].as_str().is_some_and(|t| !t.trim().is_empty()))
                .map(|b| json!({"type": "text", "text": b["text"]}))
                .collect();
            (!text.is_empty()).then(|| Arc::new(json!({"role": "user", "content": text})))
        }
        _ => None,
    }
}

/// A rollover ready to apply: write `transcript` to `transcript_path`, then
/// replace the history with `successor`.
#[derive(Debug)]
pub struct RolloverPlan {
    pub successor: Vec<SharedMessage>,
    pub transcript: String,
    pub transcript_path: PathBuf,
    /// The window the successor starts.
    pub window: u64,
    pub evicted_messages: usize,
    pub transcripts: Vec<TranscriptRef>,
    /// Indices (in the planned history) the transcript renders.
    evicted_indices: Vec<usize>,
}

/// Plan a rollover of `messages` into `budget` (message tokens, estimator
/// units). `transcript_path(window)` names the file the evicted window is
/// saved to. `None` when nothing can be evicted (no assistant round before
/// the last one, or no valid cut); the caller treats that as no-shrink.
///
/// The successor is: the brief (message 0, always, even over budget); later
/// orchestrator instructions from the evicted range, newest first within
/// [`PINNED_SHARE_PERCENT`] of the budget; the host note; and the longest
/// contiguous suffix that starts at an assistant message no tool group
/// straddles and fits the rest of the budget. The last assistant message and
/// everything after it are always kept.
pub fn plan_rollover(
    messages: &[SharedMessage],
    budget: SuccessorBudget,
    before_tokens: u64,
    transcript_path: impl Fn(u64) -> PathBuf,
) -> Option<RolloverPlan> {
    let n = messages.len();
    if n < 3 || messages[0]["role"] != "user" {
        return None;
    }
    let last_assistant = messages.iter().rposition(|m| m["role"] == "assistant")?;
    let after_notes = messages
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, m)| is_note(m))
        .map(|(i, _)| i + 1)
        .max()
        .unwrap_or(1);

    // A cut at k is valid when no tool call/result group has members on
    // both sides: no group with lo < k <= hi.
    let groups = tool_groups(messages);
    let mut extent: HashMap<usize, (usize, usize)> = HashMap::new();
    for (i, g) in groups.iter().enumerate() {
        let e = extent.entry(*g).or_insert((i, i));
        e.0 = e.0.min(i);
        e.1 = e.1.max(i);
    }
    let mut straddled = vec![false; n + 1];
    for &(lo, hi) in extent.values() {
        for flag in straddled.iter_mut().take(hi + 1).skip(lo + 1) {
            *flag = true;
        }
    }
    let candidates: Vec<usize> = (after_notes.max(1)..=last_assistant)
        .filter(|&k| messages[k]["role"] == "assistant" && !straddled[k])
        .collect();
    let &must_keep = candidates.last()?;

    let costs: Vec<u64> = messages.iter().map(message_tokens).collect();
    let mut suffix = vec![0u64; n + 1];
    for i in (0..n).rev() {
        suffix[i] = suffix[i + 1] + costs[i];
    }
    let total = budget.history_tokens;
    let share = total.min(u64::MAX / 100) * PINNED_SHARE_PERCENT / 100;
    let instructions: Vec<(usize, SharedMessage, u64)> = (1..must_keep)
        .filter_map(|i| carried_instruction(&messages[i]).map(|m| (i, m)))
        .map(|(i, m)| {
            let cost = message_tokens(&m);
            (i, m, cost)
        })
        .collect();
    let instruction_reserve = instructions.iter().map(|(_, _, c)| c).sum::<u64>().min(share);
    let (_, prior) = window_state(messages);
    let note_reserve =
        DROPPED_INDEX_TOKENS + NOTE_FRAME_TOKENS + 60 * (prior.len() as u64 + 1);
    let available =
        total.saturating_sub(costs[0] + instruction_reserve + note_reserve);
    // First cut whose suffix fits the reserve estimate; then measure the
    // real successor (the note grows with the index and every earlier
    // window) and step to the next boundary while it is over target.
    let first = candidates
        .iter()
        .position(|&k| suffix[k] <= available)
        .unwrap_or(candidates.len() - 1);
    let (window, prior_transcripts) = window_state(messages);
    let path = transcript_path(window);
    let mut plan = None;
    for &cut in &candidates[first..] {
        let built = build_successor(
            messages,
            cut,
            after_notes,
            &instructions,
            share,
            window,
            &prior_transcripts,
            &path,
            before_tokens,
            &costs,
        )?;
        let size = super::context::estimate_history(&built.successor);
        plan = Some(built);
        if size <= total || cut == must_keep {
            if size > total && total != u64::MAX {
                tracing::warn!(
                    successor_tokens = size,
                    target_tokens = total,
                    "worker rollover must-keep tail exceeds the successor target; keeping it whole"
                );
            }
            break;
        }
    }
    let mut plan = plan?;
    let evicted: Vec<usize> = std::mem::take(&mut plan.evicted_indices);
    plan.transcript = render_transcript(messages, &evicted, window, before_tokens);
    Some(plan)
}

/// The successor for a cut at `cut` (see [`plan_rollover`]); `None` when the
/// cut evicts nothing that is not carried anyway.
#[allow(clippy::too_many_arguments)]
fn build_successor(
    messages: &[SharedMessage],
    cut: usize,
    after_notes: usize,
    instructions: &[(usize, SharedMessage, u64)],
    share: u64,
    window: u64,
    prior_transcripts: &[TranscriptRef],
    path: &Path,
    before_tokens: u64,
    costs: &[u64],
) -> Option<RolloverPlan> {
    // Instructions evicted by this cut, newest first within the share.
    let mut pinned = Vec::new();
    let mut unpinned = Vec::new();
    let mut used = 0u64;
    for (i, m, cost) in instructions.iter().rev().filter(|(i, _, _)| *i < cut) {
        if used + cost <= share {
            used += cost;
            pinned.push((*i, Arc::clone(m)));
        } else {
            unpinned.push(*i);
        }
    }
    pinned.reverse();
    unpinned.reverse();

    // Saved and counted: what this window added before the cut. Messages
    // before the old note are instructions an earlier rollover carried; they
    // are already in an earlier window's transcript.
    let evicted: Vec<usize> = (after_notes..cut)
        .filter(|&i| !is_note(&messages[i]))
        .collect();
    if evicted.is_empty() || evicted.iter().all(|i| pinned.iter().any(|(p, _)| p == i)) {
        return None;
    }
    let mut transcripts = prior_transcripts.to_vec();
    transcripts.push(TranscriptRef {
        window,
        path: path.display().to_string(),
        messages: evicted.len(),
    });
    let evicted_tokens: u64 = evicted.iter().map(|&i| costs[i]).sum();
    let kept = messages.len() - cut;
    let note = note_message(NoteFacts {
        window: window + 1,
        before_tokens,
        evicted: evicted.len(),
        evicted_tokens,
        kept,
        carried_instructions: !pinned.is_empty(),
        transcripts: &transcripts,
        tool_index: tool_index(messages, &evicted),
        unpinned: &unpinned
            .iter()
            .map(|&i| (i, preview(&messages[i])))
            .collect::<Vec<_>>(),
    });

    let mut successor = Vec::with_capacity(2 + pinned.len() + kept);
    successor.push(Arc::clone(&messages[0]));
    successor.extend(pinned.into_iter().map(|(_, m)| m));
    successor.push(Arc::new(note));
    successor.extend(messages[cut..].iter().cloned());
    Some(RolloverPlan {
        successor,
        transcript: String::new(),
        transcript_path: path.to_path_buf(),
        window: window + 1,
        evicted_messages: evicted.len(),
        transcripts,
        evicted_indices: evicted,
    })
}

/// Whether `plan` is worth applying: it must shrink the history estimate by
/// more than [`MIN_SHRINK_TOKENS`] and keep every tool call paired as it was.
pub fn plan_is_productive(messages: &[SharedMessage], plan: &RolloverPlan) -> bool {
    let before = super::context::estimate_history(messages);
    let after = super::context::estimate_history(&plan.successor);
    after.saturating_add(MIN_SHRINK_TOKENS) < before
        && unpaired_tool_ids(&plan.successor).is_subset(&unpaired_tool_ids(messages))
}

/// Tool ids with a call but no result after it, or a result with no call
/// before it. A valid history has none; a successor must add none.
pub fn unpaired_tool_ids(messages: &[SharedMessage]) -> BTreeSet<String> {
    let mut calls = BTreeSet::new();
    let mut unpaired = BTreeSet::new();
    for m in messages {
        for b in m["content"].as_array().into_iter().flatten() {
            match (b["type"].as_str(), m["role"].as_str()) {
                (Some("tool_use"), Some("assistant")) => {
                    if let Some(id) = b["id"].as_str() {
                        calls.insert(id.to_owned());
                        unpaired.insert(id.to_owned());
                    }
                }
                (Some("tool_result"), Some("user")) => {
                    if let Some(id) = b["tool_use_id"].as_str() {
                        if !unpaired.remove(id) || !calls.contains(id) {
                            unpaired.insert(format!("result:{id}"));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    unpaired
}

struct NoteFacts<'a> {
    window: u64,
    before_tokens: u64,
    evicted: usize,
    evicted_tokens: u64,
    kept: usize,
    carried_instructions: bool,
    transcripts: &'a [TranscriptRef],
    tool_index: Vec<String>,
    unpinned: &'a [(usize, String)],
}

fn note_message(f: NoteFacts<'_>) -> Value {
    let mut text = format!(
        "[Worker context rollover — host note, not a new instruction]\n\
         Your context reached about {} tokens, so the host started context window {}. \
         {} earlier messages (about {} tokens) were moved out of your context unchanged; \
         nothing was summarized. Your task (the first message){} and your most recent {} \
         messages are kept verbatim.\n\
         The moved messages are saved as plain text you can search with grep or read by line range:",
        f.before_tokens,
        f.window,
        f.evicted,
        f.evicted_tokens,
        if f.carried_instructions {
            ", later orchestrator instructions"
        } else {
            ""
        },
        f.kept,
    );
    let hidden = f.transcripts.len().saturating_sub(NOTE_LISTED_WINDOWS);
    if let Some(oldest) = f.transcripts.first().filter(|_| hidden > 0) {
        let dir = Path::new(&oldest.path)
            .parent()
            .map(|d| d.display().to_string())
            .unwrap_or_default();
        text.push_str(&format!(
            "\n- windows {}–{}: earlier transcripts (*.window-N.md) in {}",
            oldest.window,
            f.transcripts[hidden - 1].window,
            neutral_note(&dir)
        ));
    }
    for t in &f.transcripts[hidden..] {
        text.push_str(&format!(
            "\n- window {}: {} ({} messages)",
            t.window,
            neutral_note(&t.path),
            t.messages
        ));
    }
    text.push_str(
        "\nLook there only when you need a specific earlier detail (an error, a file path, a \
         decision); do not re-read them wholesale.",
    );
    if !f.unpinned.is_empty() {
        text.push_str("\nEarlier orchestrator messages not kept verbatim (they are in the saved text):");
        for (i, p) in f.unpinned {
            text.push_str(&format!("\n- #{i}: {p}"));
        }
    }
    if !f.tool_index.is_empty() {
        text.push_str("\nTool calls in the moved messages, most recent first:");
        let mut listed = 0;
        for line in &f.tool_index {
            let line = format!("\n- {line}");
            if super::context::conservative_token_estimate(&text)
                + super::context::conservative_token_estimate(&line)
                > DROPPED_INDEX_TOKENS + NOTE_FRAME_TOKENS / 2
            {
                break;
            }
            text.push_str(&line);
            listed += 1;
        }
        if listed < f.tool_index.len() {
            text.push_str(&format!(
                "\n- … and {} older not listed",
                f.tool_index.len() - listed
            ));
        }
    }
    text.push_str(
        "\nContinue the task from where the kept messages leave off; do not redo completed \
         steps.\n[End worker context rollover]",
    );
    let transcripts: Vec<Value> = f
        .transcripts
        .iter()
        .map(|t| serde_json::to_value(t).unwrap_or(Value::Null))
        .collect();
    json!({
        "role": "user",
        MARKER_KEY: {"schema": SCHEMA, "window": f.window, "transcripts": transcripts},
        "content": [{"type": "text", "text": text}],
    })
}

/// `name: <the input's main argument>` for every evicted tool call, newest first.
fn tool_index(messages: &[SharedMessage], evicted: &[usize]) -> Vec<String> {
    let mut lines = Vec::new();
    for &i in evicted.iter().rev() {
        let blocks: Vec<&Value> = messages[i]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "tool_use")
            .collect();
        for b in blocks.into_iter().rev() {
            let name = neutral_note(b["name"].as_str().unwrap_or("?"));
            let input = &b["input"];
            let arg = ["command", "path", "file_path", "pattern", "query", "url", "handle_id"]
                .iter()
                .find_map(|k| input[*k].as_str().map(str::to_owned))
                .unwrap_or_else(|| input.to_string());
            lines.push(format!("{name}: {}", clip(&flat(&arg), INDEX_PREVIEW_CHARS)));
        }
    }
    lines
}

fn flat(text: &str) -> String {
    neutral_note(&text.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn clip(text: &str, chars: usize) -> String {
    let mut out: String = text.chars().take(chars).collect();
    if text.chars().count() > chars {
        out.push('…');
    }
    out
}

fn preview(message: &Value) -> String {
    let text = match &message["content"] {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    };
    clip(&flat(&text), 80)
}

/// Plain text of every evicted message, in order, for grep and ranged read.
/// Thinking is omitted (private reasoning; signed blocks cannot be re-sent out
/// of their turn anyway); images and documents are named only.
pub fn render_transcript(
    messages: &[SharedMessage],
    evicted: &[usize],
    window: u64,
    before_tokens: u64,
) -> String {
    let mut out = format!(
        "# Worker context window {window}: {} messages moved out of context at a rollover\n\
         # (context about {before_tokens} tokens). Thinking is omitted; images are named only.\n",
        evicted.len()
    );
    for &i in evicted {
        let m = &messages[i];
        let role = m["role"].as_str().unwrap_or("user");
        out.push_str(&format!("\n## [{i}] {role}\n"));
        match &m["content"] {
            Value::String(s) => {
                out.push_str(s);
                out.push('\n');
            }
            Value::Array(blocks) => {
                for b in blocks {
                    render_block(&mut out, b);
                }
            }
            _ => {}
        }
    }
    out
}

fn render_block(out: &mut String, b: &Value) {
    match b["type"].as_str() {
        Some("text") => {
            out.push_str(b["text"].as_str().unwrap_or_default());
            out.push('\n');
        }
        Some("tool_use") => {
            let input = serde_json::to_string_pretty(&b["input"]).unwrap_or_default();
            out.push_str(&format!(
                "→ tool_use {} ({})\n{input}\n",
                b["name"].as_str().unwrap_or("?"),
                b["id"].as_str().unwrap_or("?")
            ));
        }
        Some("tool_result") => {
            let error = if b["is_error"] == true { " error" } else { "" };
            out.push_str(&format!(
                "← tool_result{error} ({})\n",
                b["tool_use_id"].as_str().unwrap_or("?")
            ));
            match &b["content"] {
                Value::String(s) => {
                    out.push_str(s);
                    out.push('\n');
                }
                Value::Array(inner) => {
                    for x in inner {
                        render_block(out, x);
                    }
                }
                Value::Null => {}
                other => {
                    out.push_str(&other.to_string());
                    out.push('\n');
                }
            }
        }
        Some("thinking") | Some("redacted_thinking") => {}
        Some(other) => out.push_str(&format!("[{other}]\n")),
        None => {}
    }
}

/// Apply one worker rollover: plan it, refuse one that cannot shrink the
/// context or would start where the next round triggers another rollover
/// (the pressure band below hard capacity, the hard-capacity line at it),
/// save the evicted messages, and return the successor with its report.
/// `hard_capacity` is `Some(line)` for a rollover at hard capacity, where
/// `line` is the footprint at which the next round is at hard capacity
/// (`context_window - required_next_round`). `Ok(None)` = no-shrink (the
/// caller defers like the foreground's unproductive rollover). An error
/// writing the transcript means no rollover: nothing is evicted that was not
/// saved.
pub fn roll_over(
    messages: &[SharedMessage],
    budget: SuccessorBudget,
    before_tokens: u64,
    overhead_tokens: u64,
    policy: &WorkerPolicy,
    sink: &WorkerTranscriptSink,
    hard_capacity: Option<u64>,
) -> std::io::Result<Option<(Vec<SharedMessage>, WorkerRolloverReport)>> {
    let Some(plan) = plan_rollover(messages, budget, before_tokens, |window| {
        transcript_path(&sink.dir, &sink.stem, window)
    }) else {
        return Ok(None);
    };
    if !plan_is_productive(messages, &plan) {
        return Ok(None);
    }
    let after_tokens =
        overhead_tokens.saturating_add(super::context::estimate_history(&plan.successor));
    if after_tokens >= hard_capacity.unwrap_or(policy.thresholds.pressure_tokens) {
        return Ok(None);
    }
    crate::tools::write_window_transcript(
        &plan.transcript_path,
        &plan.transcript,
    )?;
    let report = WorkerRolloverReport {
        window: plan.window,
        before_tokens,
        after_tokens,
        evicted_messages: plan.evicted_messages,
        transcript: plan.transcript_path,
        transcripts: plan
            .transcripts
            .iter()
            .map(|t| PathBuf::from(&t.path))
            .collect(),
    };
    Ok(Some((plan.successor, report)))
}

/// `<dir>/<stem>.window-<window>.md`.
pub fn transcript_path(dir: &Path, stem: &str, window: u64) -> PathBuf {
    let stem: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    dir.join(format!("{stem}.window-{window}.md"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> SharedMessage {
        Arc::new(json!({"role": "user", "content": text}))
    }

    /// One worker round: an assistant tool call (signed thinking first) and
    /// its result, padded to roughly `pad` characters.
    fn round(i: usize, pad: usize) -> [SharedMessage; 2] {
        [
            Arc::new(json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": format!("step {i}"), "signature": format!("sig{i}")},
                {"type": "tool_use", "id": format!("t{i}"), "name": "bash",
                 "input": {"command": format!("cargo test step{i}")}}
            ]})),
            Arc::new(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": format!("t{i}"),
                 "content": format!("output {i} {}", "x".repeat(pad))}
            ]})),
        ]
    }

    fn worker_history(rounds: usize, pad: usize) -> Vec<SharedMessage> {
        let mut m = vec![user("BRIEF: fix the parser")];
        for i in 0..rounds {
            m.extend(round(i, pad));
        }
        m
    }

    fn path(w: u64) -> PathBuf {
        PathBuf::from(format!("/tmp/x/sa_1.window-{w}.md"))
    }

    fn budget(tokens: u64) -> SuccessorBudget {
        SuccessorBudget { history_tokens: tokens }
    }

    #[test]
    fn policy_lowers_the_band_and_derives_pressure_and_target_like_the_foreground() {
        let on = WorkerContextSettings { enabled: true, rollover_tokens: 200_000 };
        let p = on.policy(1_000_000).unwrap();
        assert_eq!(p.thresholds.rollover_tokens, 200_000);
        assert_eq!(p.thresholds.pressure_tokens, 199_999);
        assert_eq!(p.target_tokens(), 100_000);
        assert_eq!(p.config.finish_rounds, 0);
        // A 200k window's own band (180k) wins over a higher setting.
        let p = on.policy(200_000).unwrap();
        assert_eq!(p.thresholds.rollover_tokens, 180_000);
        assert_eq!(p.thresholds.pressure_tokens, 140_000);
        assert_eq!(p.target_tokens(), 93_333);
        assert!(p.target_tokens() < p.thresholds.pressure_tokens);
        let off = WorkerContextSettings { enabled: false, ..on };
        assert!(off.policy(1_000_000).is_none());
        assert!(on.policy(1).is_none(), "a window that holds no policy disables it");
        // Inert without a sink: nothing may be evicted without a place to keep it.
        let ctx = WorkerContext { settings: Some(on), sink: None };
        assert!(ctx.resolve(1_000_000).is_none());
    }

    #[test]
    fn policy_engine_admits_then_rolls_at_the_worker_band() {
        use agent_core::core::context_policy::{assess_context, ContextAction, ContextBudget, ContextState};
        let p = WorkerContextSettings { enabled: true, rollover_tokens: 200_000 }
            .policy(1_000_000)
            .unwrap();
        let at = |used: u64| ContextBudget {
            context_window_tokens: 1_000_000,
            used_tokens: used,
            hard_remaining_tokens: 1_000_000 - used,
            required_next_round_tokens: 150_000,
        };
        let state = ContextState::default();
        assert_eq!(assess_context(&p.config, &state, at(150_000)).action, ContextAction::Continue);
        assert_eq!(assess_context(&p.config, &state, at(200_000)).action, ContextAction::Rollover);
        // Hard capacity is a rollover attempt too (the caller treats HardStop alike).
        assert_eq!(assess_context(&p.config, &state, at(900_000)).action, ContextAction::HardStop);
    }

    #[test]
    fn hard_capacity_aims_inside_the_room_the_window_has_left() {
        let normal = budget(56_000);
        // 120k window, 72k reserved for the next round, 1k fixed overhead.
        assert_eq!(hard_capacity_budget(normal, 120_000, 72_000, 1_000).history_tokens, 31_333);
        // A roomy window keeps the configured target.
        assert_eq!(hard_capacity_budget(normal, 1_000_000, 150_000, 1_000).history_tokens, 56_000);
        assert_eq!(hard_capacity_budget(normal, 10, 72_000, 1_000).history_tokens, 0);
    }

    /// The live thrash (#332): an Opus worker on the 200k default window.
    /// Its next round reserves the 128k output envelope, a 10k tool result,
    /// the 15% safety margin (30k) and the 512 advisory reserve: hard
    /// capacity at 31,488, a third of the 93,333 target. No rollover there
    /// can make room. The same Opus worker on the configured 1m window, and
    /// a Sonnet worker (64k envelope) on 200k, can hold the target.
    #[test]
    fn hard_capacity_has_room_only_when_the_target_fits_below_it() {
        use agent_core::core::context_policy::{assess_context, ContextAction, ContextBudget, ContextState};
        let on = WorkerContextSettings { enabled: true, rollover_tokens: 200_000 };
        let opus_200k = 128_000 + 10_000 + 30_000 + 512;
        let p = on.policy(200_000).unwrap();
        assert_eq!(p.target_tokens(), 93_333);
        // ~33k of context is hard capacity on that window.
        let at_33k = ContextBudget {
            context_window_tokens: 200_000,
            used_tokens: 33_000,
            hard_remaining_tokens: 167_000,
            required_next_round_tokens: opus_200k,
        };
        assert_eq!(assess_context(&p.config, &ContextState::default(), at_33k).action, ContextAction::HardStop);
        assert!(!p.hard_capacity_has_room(200_000, opus_200k));
        assert!(p.hard_capacity_has_room(200_000, 64_000 + 10_000 + 30_000 + 512));
        let opus_1m = 128_000 + 10_000 + 150_000 + 512;
        assert!(on.policy(1_000_000).unwrap().hard_capacity_has_room(1_000_000, opus_1m));
        // A 120k Sonnet window (the old hard-capacity test): 27.5k against 56k.
        let small = on.policy(120_000).unwrap();
        assert_eq!(small.target_tokens(), 56_000);
        assert!(!small.hard_capacity_has_room(120_000, 64_000 + 10_000 + 18_000 + 512));
        // The boundary: a successor at the target must start below the line.
        assert!(!p.hard_capacity_has_room(200_000, 200_000 - 93_333));
        assert!(p.hard_capacity_has_room(200_000, 200_000 - 93_334));
        assert!(!p.hard_capacity_has_room(100_000, 200_000), "reserves over the window");
    }

    /// At hard capacity a successor that would itself be at hard capacity
    /// (its must-keep tail alone is too big) makes no room: refused, as the
    /// pressure band refuses one below hard capacity.
    #[test]
    fn a_successor_at_the_line_that_triggered_it_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = WorkerTranscriptSink { dir: tmp.path().into(), stem: "sa_1".into(), observer: None };
        let policy = WorkerContextSettings { enabled: true, rollover_tokens: 200_000 }
            .policy(200_000)
            .unwrap();
        let mut m = worker_history(20, 3_000);
        m.extend(round(20, 60_000)); // the must-keep round: ~24k tokens
        let before = super::super::context::estimate_history(&m);
        let roll = |line| roll_over(&m, budget(10_000), before, 1_000, &policy, &sink, line).unwrap();
        assert!(roll(Some(20_000)).is_none(), "would start at hard capacity again");
        let (successor, report) = roll(Some(100_000)).expect("room below the line");
        assert!(report.after_tokens < 100_000 && successor.len() < m.len());
        // Below hard capacity the pressure band (140k) is the line.
        assert!(roll(None).is_some());
    }

    #[test]
    fn successor_keeps_brief_and_recent_rounds_within_the_target() {
        let messages = worker_history(40, 4_000);
        let before = super::super::context::estimate_history(&messages);
        let plan = plan_rollover(&messages, budget(20_000), before, path).unwrap();
        let s = &plan.successor;
        assert!(Arc::ptr_eq(&s[0], &messages[0]), "the brief survives verbatim");
        assert!(s[1].get(MARKER_KEY).is_some(), "then the host note");
        assert_eq!(s[2]["role"], "assistant", "the suffix starts at an assistant turn");
        assert!(Arc::ptr_eq(s.last().unwrap(), messages.last().unwrap()));
        // Every retained round is the same Arc: nothing edited (thinking intact).
        for m in &s[2..] {
            assert!(messages.iter().any(|o| Arc::ptr_eq(o, m)));
        }
        assert!(s[2]["content"][0]["type"] == "thinking" && s[2]["content"][0]["signature"].is_string());
        let after = super::super::context::estimate_history(s);
        assert!(after <= 20_000, "target respected: {after}");
        assert!(after < before / 3);
        assert!(unpaired_tool_ids(s).is_empty());
        assert!(plan_is_productive(&messages, &plan));
        assert_eq!(plan.window, 2);
        assert_eq!(plan.evicted_messages, messages.len() - 1 - (s.len() - 2));
        // The transcript holds every evicted round, without thinking.
        assert!(plan.transcript.contains("cargo test step0"));
        assert!(plan.transcript.contains("output 0 "));
        assert!(!plan.transcript.contains("sig0") && !plan.transcript.contains("step 0\n"));
        // The note names the transcript and indexes the evicted calls.
        let note = s[1]["content"][0]["text"].as_str().unwrap();
        assert!(note.contains("/tmp/x/sa_1.window-1.md"), "{note}");
        assert!(note.contains("bash: cargo test step"), "{note}");
        assert_eq!(s[1][MARKER_KEY]["window"], 2);
    }

    #[test]
    fn cut_never_splits_a_tool_group_and_keeps_the_pending_loop() {
        // A parallel round whose second result arrives in a later message
        // straddles the boundary right after it; the cut must move past it.
        let mut m = worker_history(6, 2_000);
        m.push(Arc::new(json!({"role":"assistant","content":[
            {"type":"tool_use","id":"p1","name":"read","input":{"path":"a"}},
            {"type":"tool_use","id":"p2","name":"read","input":{"path":"b"}}]})));
        m.push(Arc::new(json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"p1","content":"A"}]})));
        m.push(Arc::new(json!({"role":"assistant","content":[{"type":"text","text":"hm"}]})));
        m.push(Arc::new(json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"p2","content":"B"}]})));
        m.extend(round(99, 10));
        for tokens in [0, 1_000, 3_000, 6_000, 50_000] {
            let Some(plan) = plan_rollover(&m, budget(tokens), 1, path) else { continue };
            assert!(unpaired_tool_ids(&plan.successor).is_empty(), "budget {tokens}");
            let note = plan.successor.iter().position(|x| x.get(MARKER_KEY).is_some()).unwrap();
            assert_eq!(plan.successor[note + 1]["role"], "assistant");
            assert!(Arc::ptr_eq(plan.successor.last().unwrap(), m.last().unwrap()));
        }
        // Even a zero budget keeps the last assistant turn and its results.
        let plan = plan_rollover(&m, budget(0), 1, path).unwrap();
        assert_eq!(plan.successor.len(), 4, "brief, note, last call, its result");
    }

    #[test]
    fn orchestrator_instructions_survive_and_bookkeeping_does_not() {
        let mut m = worker_history(3, 3_000);
        m.push(user("STEER: also update the docs"));
        // Resume merges its instructions into a tool_result carrier.
        m.extend(round(3, 3_000));
        let mut carrier = (*m.pop().unwrap()).clone();
        carrier["content"].as_array_mut().unwrap().push(
            json!({"type":"text","text":"[Resumed by the orchestrator.] RESUME: finish the tests"}),
        );
        m.push(Arc::new(carrier));
        m.push(Arc::new(json!({"role":"user","content":
            crate::engine::interrupt::InterruptReason::ALL[0].marker()})));
        for i in 4..30 {
            m.extend(round(i, 3_000));
        }
        let plan = plan_rollover(&m, budget(15_000), 1, path).unwrap();
        let s = &plan.successor;
        let texts: Vec<String> = s
            .iter()
            .take_while(|x| x.get(MARKER_KEY).is_none())
            .map(|x| x["content"].to_string())
            .collect();
        assert_eq!(texts.len(), 3, "brief + two carried instructions: {texts:?}");
        assert!(texts[1].contains("STEER: also update the docs"));
        assert!(texts[2].contains("RESUME: finish the tests"));
        assert!(!texts[2].contains("tool_result"), "only the text travels");
        assert!(!s.iter().any(|x| x["content"].as_str().is_some_and(
            crate::engine::interrupt::is_interruption_marker)));
        assert!(unpaired_tool_ids(s).is_empty());
    }

    #[test]
    fn second_rollover_continues_numbering_and_lists_every_window() {
        let messages = worker_history(30, 3_000);
        let first = plan_rollover(&messages, budget(15_000), 1, path).unwrap();
        let mut next = first.successor.clone();
        for i in 100..130 {
            next.extend(round(i, 3_000));
        }
        assert_eq!(window_state(&next).0, 2);
        let second = plan_rollover(&next, budget(15_000), 1, path).unwrap();
        assert_eq!(second.window, 3);
        let s = &second.successor;
        assert!(Arc::ptr_eq(&s[0], &messages[0]), "the brief survives every rollover");
        assert_eq!(s.iter().filter(|x| x.get(MARKER_KEY).is_some()).count(), 1, "one note");
        let note = s[1]["content"][0]["text"].as_str().unwrap();
        assert!(note.contains("window-1.md") && note.contains("window-2.md"), "{note}");
        assert!(!second.transcript.contains("Worker context rollover"), "old note not re-saved");
        let (window, transcripts) = window_state(s);
        assert_eq!(window, 3);
        assert_eq!(transcripts.iter().map(|t| t.window).collect::<Vec<_>>(), vec![1, 2]);
        // A cold resume carries the metadata on its single message.
        let history: Vec<Value> = s.iter().map(|x| (**x).clone()).collect();
        let marker = carried_marker(&history).unwrap();
        assert_eq!(marker["window"], 3);
        assert_eq!(marker["transcripts"].as_array().unwrap().len(), 2);
        assert!(carried_marker(&[json!({"role":"user","content":"x"})]).is_none());
    }

    #[test]
    fn the_note_stays_bounded_however_many_windows_a_worker_has_had() {
        let transcripts: Vec<TranscriptRef> = (1..=40)
            .map(|w| TranscriptRef {
                window: w,
                path: format!("/d/sa_1.window-{w}.md"),
                messages: 9,
            })
            .collect();
        let note = note_message(NoteFacts {
            window: 41,
            before_tokens: 200_000,
            evicted: 9,
            evicted_tokens: 100_000,
            kept: 30,
            carried_instructions: false,
            transcripts: &transcripts,
            tool_index: (0..5_000).map(|i| format!("bash: step {i}")).collect(),
            unpinned: &[],
        });
        let text = note["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("windows 1–28: earlier transcripts (*.window-N.md) in /d"), "{text}");
        assert!(!text.contains("sa_1.window-28.md") && text.contains("sa_1.window-29.md"));
        assert!(text.contains("sa_1.window-40.md"));
        assert!(text.contains("older not listed"));
        let tokens = super::super::context::conservative_token_estimate(text);
        assert!(tokens <= DROPPED_INDEX_TOKENS + NOTE_FRAME_TOKENS, "{tokens}");
    }

    #[test]
    fn nothing_to_evict_is_no_plan_and_small_wins_are_unproductive() {
        assert!(plan_rollover(&[user("brief")], budget(1), 1, path).is_none());
        let one = worker_history(1, 50_000);
        assert!(plan_rollover(&one, budget(1), 1, path).is_none(), "only the must-keep round");
        let two = worker_history(2, 100);
        let plan = plan_rollover(&two, budget(0), 1, path).unwrap();
        assert!(!plan_is_productive(&two, &plan), "a tiny round is not worth a rewrite");
    }

    #[test]
    fn host_metadata_never_reaches_the_wire() {
        let plan = plan_rollover(&worker_history(10, 3_000), budget(5_000), 1, path).unwrap();
        let wire = serde_json::to_string(&super::super::continuation::WireMessages(&plan.successor)).unwrap();
        assert!(!wire.contains(MARKER_KEY));
        assert!(wire.contains("Worker context rollover"));
    }

    #[test]
    fn transcript_path_is_confined_to_the_directory() {
        let p = transcript_path(Path::new("/d"), "../sa 1", 3);
        assert_eq!(p, PathBuf::from("/d/---sa-1.window-3.md"));
    }
}
