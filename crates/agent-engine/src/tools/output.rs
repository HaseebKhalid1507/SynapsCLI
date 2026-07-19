//! Task 26 — bounded tool-output delta channels with an explicit
//! backpressure/coalescing policy (spec §8.4).
//!
//! High-volume tool output used to ride an UNBOUNDED `mpsc` queue from the
//! producing tool to a detached forwarding task; a fast producer with a slow
//! consumer could therefore grow RSS without limit. This module replaces
//! that path with:
//!
//! - a **bounded channel** ([`DELTA_CHANNEL_CAPACITY`] chunks, each retained
//!   chunk capped at [`DELTA_MAX_CHUNK_BYTES`]);
//! - an explicit **coalescing overflow buffer** (at most
//!   [`DELTA_COALESCE_CAP_BYTES`] bytes): when the channel is full, chunks
//!   merge into one pending byte-capped buffer instead of queueing without
//!   bound;
//! - an explicit **drop policy** beyond the coalesce cap, with exact
//!   dropped/coalesced byte and chunk counters ([`OutputCounters`]);
//! - a **UI-preview budget** enforced by the forwarding task at production
//!   time via [`agent_core::BoundedText`] — the downstream (unbounded) UI
//!   event queue can never receive more than the budget from one tool call;
//! - **cancellation** that terminates the forwarding task and thereby
//!   closes the channel, releasing producers: a send after close is a
//!   counted drop, never a block.
//!
//! Policy choice (documented, deliberate): producers are NEVER blocked on
//! the UI consumer. The delta lane is a preview lane — full fidelity for
//! model history is owned by the tool's own bounded result buffer — so the
//! bounded policy here is coalesce-then-drop with exact accounting rather
//! than producer backpressure, which would let a stalled UI consumer stall
//! tool execution and cancellation.
//!
//! Memory invariant: bytes retained in flight are bounded by
//! `DELTA_CHANNEL_CAPACITY * DELTA_MAX_CHUNK_BYTES + DELTA_COALESCE_CAP_BYTES`
//! per tool call, and always equal `produced - forwarded - dropped` in the
//! counters (coalesced bytes are informational overlap, not a third bucket).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

/// Maximum chunks queued in the bounded delta channel.
pub const DELTA_CHANNEL_CAPACITY: usize = 64;

/// Per-chunk retention cap: a single produced chunk larger than this keeps
/// only a UTF-8-safe prefix (the cut tail is a counted drop).
pub const DELTA_MAX_CHUNK_BYTES: usize = 64 * 1024;

/// Byte cap of the coalescing overflow buffer used while the channel is full.
pub const DELTA_COALESCE_CAP_BYTES: usize = 64 * 1024;

/// Default per-tool-call UI-preview byte budget enforced by the forwarder.
/// Matches the historical `max_tool_buffer` bound (256 KiB) so bounded tool
/// results remain visually identical.
pub const DEFAULT_UI_PREVIEW_BYTES: usize = 256 * 1024;

// ── Budgets ─────────────────────────────────────────────────────────────────

/// Independent per-call output byte budgets (spec §8.4). The UI-preview
/// lane (streamed deltas + the final `ToolResult` event) and the
/// model-history lane (the `tool_result` block content) are bounded
/// SEPARATELY: tightening one never widens or narrows the other. Both are
/// enforced through [`agent_core::BoundedText`] at the point the bounded
/// text is produced — never by materializing the full output first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputBudgets {
    /// Total bytes one tool call may contribute to the UI event lane.
    pub ui_preview_bytes: usize,
    /// Total bytes one tool call's result may occupy in model history.
    pub model_history_bytes: usize,
}

impl OutputBudgets {
    /// Runtime defaults: the compiled UI-preview budget alongside the
    /// configured model-history budget (`max_tool_output`).
    pub fn for_limits(max_tool_output: usize) -> Self {
        Self {
            ui_preview_bytes: DEFAULT_UI_PREVIEW_BYTES,
            model_history_bytes: max_tool_output,
        }
    }

    /// Hard upper bound retained by the UI delta lane itself, independent
    /// of the consumer and configured preview budget.
    pub const fn max_ui_retained_bytes() -> u64 {
        (DELTA_CHANNEL_CAPACITY * DELTA_MAX_CHUNK_BYTES + DELTA_COALESCE_CAP_BYTES) as u64
    }
}

/// Model-history production result. `text` is always a UTF-8 prefix at most
/// the configured model-history budget; the original is represented only by
/// exact byte accounting, never retained in full.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedOutput {
    pub text: String,
    pub original_bytes: usize,
    pub retained_bytes: usize,
    pub truncated: bool,
}

/// Bound a final result string for the UI event lane, appending a
/// metadata-only truncation marker when anything was cut. UTF-8-safe and
/// greedy via [`agent_core::BoundedText`].
pub fn bounded_preview(s: &str, max_bytes: usize) -> String {
    let bounded = agent_core::BoundedText::new(s, max_bytes);
    if !bounded.truncated {
        return bounded.text;
    }
    format!(
        "{}\n\n[preview truncated — {} of {} bytes shown]",
        bounded.text, bounded.retained_bytes, bounded.original_bytes
    )
}

// ── Spill artifact ──────────────────────────────────────────────────────────

/// Opt-in spill policy: capture the FULL produced delta stream (before any
/// cap/coalesce/drop decision) into a private (`0600`, T4 helpers) artifact
/// under `dir`, so bounded previews never mean lost data.
#[derive(Debug, Clone)]
pub struct SpillPolicy {
    pub dir: PathBuf,
}

/// Point-in-time spill outcome. Metadata only: path, byte count, failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpillReport {
    pub path: PathBuf,
    pub bytes: u64,
    pub failed: bool,
}

/// Lazily opened private spill file. First write creates the directory
/// (`0700`) and the file (`0600`, symlink-refusing, append-only). An I/O
/// failure degrades ONCE (warn + `failed` flag) and disables further
/// writes; it can never fail the tool call or block the stream.
struct SpillState {
    path: PathBuf,
    dir: PathBuf,
    file: Mutex<Option<std::fs::File>>,
    opened: AtomicBool,
    bytes: AtomicU64,
    failed: AtomicBool,
}

impl std::fmt::Debug for SpillState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpillState")
            .field("path", &self.path)
            .field("bytes", &self.bytes.load(Ordering::Relaxed))
            .field("failed", &self.failed.load(Ordering::Relaxed))
            .finish()
    }
}

impl SpillState {
    fn new(policy: SpillPolicy) -> Self {
        static SPILL_SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SPILL_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = policy
            .dir
            .join(format!("tool-output-{}-{seq}.log", std::process::id()));
        Self {
            path,
            dir: policy.dir,
            file: Mutex::new(None),
            opened: AtomicBool::new(false),
            bytes: AtomicU64::new(0),
            failed: AtomicBool::new(false),
        }
    }

    fn report(&self) -> SpillReport {
        SpillReport {
            path: self.path.clone(),
            bytes: self.bytes.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
        }
    }

    fn fail(&self, action: &str, err: &dyn std::fmt::Display) {
        if !self.failed.swap(true, Ordering::Relaxed) {
            // Metadata only: our own artifact path and error, never content.
            tracing::warn!(
                path = %self.path.display(),
                error = %err,
                "tool-output spill {action} failed — artifact disabled for this call"
            );
        }
    }

    fn write(&self, chunk: &str) {
        if self.failed.load(Ordering::Relaxed) {
            return;
        }
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if file.is_none() {
            if self.opened.swap(true, Ordering::Relaxed) {
                return; // a previous open failed
            }
            if let Err(err) = agent_core::core::private_fs::ensure_private_dir(&self.dir) {
                self.fail("dir create", &err);
                return;
            }
            match agent_core::core::private_fs::open_private_append(&self.path) {
                Ok(handle) => *file = Some(handle),
                Err(err) => {
                    self.fail("open", &err);
                    return;
                }
            }
        }
        if let Some(handle) = file.as_mut() {
            use std::io::Write as _;
            match handle.write_all(chunk.as_bytes()) {
                Ok(()) => {
                    self.bytes.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                }
                Err(err) => {
                    self.fail("write", &err);
                    *file = None;
                }
            }
        }
    }
}

/// Cloneable read handle onto a call's spill artifact state.
#[derive(Debug, Clone)]
pub struct SpillHandle {
    state: Arc<SpillState>,
}

impl SpillHandle {
    pub fn report(&self) -> SpillReport {
        self.state.report()
    }
}

// ── Counters ────────────────────────────────────────────────────────────────

/// Exact per-call output accounting. All methods are lock-free and cheap;
/// metadata only (byte/chunk counts, never content).
#[derive(Debug, Default)]
pub struct OutputCounters {
    produced_chunks: AtomicU64,
    produced_bytes: AtomicU64,
    forwarded_chunks: AtomicU64,
    forwarded_bytes: AtomicU64,
    coalesced_chunks: AtomicU64,
    coalesced_bytes: AtomicU64,
    dropped_chunks: AtomicU64,
    dropped_bytes: AtomicU64,
    model_history_truncated_chunks: AtomicU64,
    model_history_dropped_bytes: AtomicU64,
}

/// Point-in-time copy of [`OutputCounters`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OutputCountersSnapshot {
    /// Chunks/bytes handed to [`DeltaSender::send`] by the producer.
    pub produced_chunks: u64,
    pub produced_bytes: u64,
    /// Chunks/bytes actually emitted to the UI consumer.
    pub forwarded_chunks: u64,
    pub forwarded_bytes: u64,
    /// Chunks/bytes merged through the overflow buffer (still delivered
    /// unless separately counted as dropped).
    pub coalesced_chunks: u64,
    pub coalesced_bytes: u64,
    /// Chunks that lost at least one byte / bytes never delivered to the UI
    /// (coalesce-cap cuts, per-chunk cap cuts, budget cuts, closed-channel
    /// sends).
    pub dropped_chunks: u64,
    pub dropped_bytes: u64,
    /// Model-history chunks that lost at least one byte and exact bytes cut
    /// while producing the independently bounded history prefix.
    pub model_history_truncated_chunks: u64,
    pub model_history_dropped_bytes: u64,
}

impl OutputCountersSnapshot {
    /// Bytes currently retained in flight (channel + overflow buffer):
    /// `produced - forwarded - dropped`.
    pub fn retained_bytes(&self) -> u64 {
        self.produced_bytes
            .saturating_sub(self.forwarded_bytes)
            .saturating_sub(self.dropped_bytes)
    }
}

impl OutputCounters {
    pub fn note_produced(&self, bytes: usize) {
        self.produced_chunks.fetch_add(1, Ordering::Relaxed);
        self.produced_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn note_forwarded(&self, bytes: usize) {
        self.forwarded_chunks.fetch_add(1, Ordering::Relaxed);
        self.forwarded_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn note_coalesced(&self, bytes: usize) {
        self.coalesced_chunks.fetch_add(1, Ordering::Relaxed);
        self.coalesced_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn note_dropped(&self, bytes: usize, chunks: u64) {
        self.dropped_chunks.fetch_add(chunks, Ordering::Relaxed);
        self.dropped_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn note_model_history_truncated(&self, bytes: usize) {
        self.model_history_truncated_chunks
            .fetch_add(1, Ordering::Relaxed);
        self.model_history_dropped_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> OutputCountersSnapshot {
        OutputCountersSnapshot {
            produced_chunks: self.produced_chunks.load(Ordering::Relaxed),
            produced_bytes: self.produced_bytes.load(Ordering::Relaxed),
            forwarded_chunks: self.forwarded_chunks.load(Ordering::Relaxed),
            forwarded_bytes: self.forwarded_bytes.load(Ordering::Relaxed),
            coalesced_chunks: self.coalesced_chunks.load(Ordering::Relaxed),
            coalesced_bytes: self.coalesced_bytes.load(Ordering::Relaxed),
            dropped_chunks: self.dropped_chunks.load(Ordering::Relaxed),
            dropped_bytes: self.dropped_bytes.load(Ordering::Relaxed),
            model_history_truncated_chunks: self
                .model_history_truncated_chunks
                .load(Ordering::Relaxed),
            model_history_dropped_bytes: self.model_history_dropped_bytes.load(Ordering::Relaxed),
        }
    }
}

// ── Channel ─────────────────────────────────────────────────────────────────

struct DeltaShared {
    overflow: Mutex<String>,
    notify: tokio::sync::Notify,
    counters: Arc<OutputCounters>,
    spill: Option<Arc<SpillState>>,
    model_history: Mutex<ModelHistoryState>,
}

#[derive(Debug)]
struct ModelHistoryState {
    text: String,
    original_bytes: usize,
    budget: usize,
    truncated: bool,
}

impl ModelHistoryState {
    fn new(budget: usize) -> Self {
        Self {
            text: String::with_capacity(budget.min(64 * 1024)),
            original_bytes: 0,
            budget,
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &str, counters: &OutputCounters) {
        self.original_bytes = self.original_bytes.saturating_add(chunk.len());
        let room = self.budget.saturating_sub(self.text.len());
        let bounded = agent_core::BoundedText::new(chunk, room);
        self.text.push_str(&bounded.text);
        if bounded.truncated {
            self.truncated = true;
            counters.note_model_history_truncated(
                bounded
                    .original_bytes
                    .saturating_sub(bounded.retained_bytes),
            );
        }
    }

    fn snapshot(&self) -> BoundedOutput {
        BoundedOutput {
            text: self.text.clone(),
            original_bytes: self.original_bytes,
            retained_bytes: self.text.len(),
            truncated: self.truncated,
        }
    }
}

/// Producer half handed to tools as `ToolChannels::tx_delta`. `send` never
/// blocks and never fails: full channels coalesce, closed channels drop —
/// both exactly counted.
pub struct DeltaSender {
    tx: tokio::sync::mpsc::Sender<String>,
    shared: Arc<DeltaShared>,
}

/// Consumer half: drains the bounded channel and the coalescing overflow
/// buffer in production order.
pub struct DeltaReceiver {
    rx: tokio::sync::mpsc::Receiver<String>,
    shared: Arc<DeltaShared>,
}

/// Cloneable observation handle for one tool call's independently bounded
/// outputs and exact counters.
#[derive(Clone)]
pub struct OutputHandle {
    shared: Arc<DeltaShared>,
}

impl OutputHandle {
    pub fn counters(&self) -> Arc<OutputCounters> {
        Arc::clone(&self.shared.counters)
    }

    pub fn model_history(&self) -> BoundedOutput {
        self.shared
            .model_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot()
    }

    pub fn spill_report(&self) -> Option<SpillReport> {
        self.shared.spill.as_ref().map(|state| state.report())
    }
}

/// One bounded delta channel pair plus its shared output handle.
pub struct ToolDeltaChannel {
    pub sender: DeltaSender,
    pub receiver: DeltaReceiver,
    output: OutputHandle,
}

impl ToolDeltaChannel {
    pub fn output_handle(&self) -> OutputHandle {
        self.output.clone()
    }
}

/// Construct a bounded delta channel with fresh counters and default budgets.
pub fn delta_channel() -> ToolDeltaChannel {
    delta_channel_with_budgets(OutputBudgets::for_limits(DEFAULT_UI_PREVIEW_BYTES), None)
}

/// Backward-compatible constructor for callers selecting only spill policy.
pub fn delta_channel_with(spill: Option<SpillPolicy>) -> ToolDeltaChannel {
    delta_channel_with_budgets(OutputBudgets::for_limits(DEFAULT_UI_PREVIEW_BYTES), spill)
}

/// Construct one output producer with independent UI/model-history budgets.
pub fn delta_channel_with_budgets(
    budgets: OutputBudgets,
    spill: Option<SpillPolicy>,
) -> ToolDeltaChannel {
    let (tx, rx) = tokio::sync::mpsc::channel(DELTA_CHANNEL_CAPACITY);
    let shared = Arc::new(DeltaShared {
        overflow: Mutex::new(String::new()),
        notify: tokio::sync::Notify::new(),
        counters: Arc::new(OutputCounters::default()),
        spill: spill.map(|policy| Arc::new(SpillState::new(policy))),
        model_history: Mutex::new(ModelHistoryState::new(budgets.model_history_bytes)),
    });
    ToolDeltaChannel {
        sender: DeltaSender {
            tx,
            shared: Arc::clone(&shared),
        },
        receiver: DeltaReceiver {
            rx,
            shared: Arc::clone(&shared),
        },
        output: OutputHandle { shared },
    }
}

/// Append `chunk` to the byte-capped overflow buffer at a valid UTF-8
/// boundary; the cut remainder is a counted drop.
fn coalesce_into(overflow: &mut String, chunk: &str, counters: &OutputCounters) {
    let room = DELTA_COALESCE_CAP_BYTES.saturating_sub(overflow.len());
    let bounded = agent_core::BoundedText::new(chunk, room);
    if bounded.retained_bytes > 0 {
        overflow.push_str(&bounded.text);
        counters.note_coalesced(bounded.retained_bytes);
    }
    if bounded.truncated {
        counters.note_dropped(bounded.original_bytes - bounded.retained_bytes, 1);
    }
}

impl DeltaSender {
    /// Shared counters handle (also visible from the receiver/forwarder).
    pub fn counters(&self) -> Arc<OutputCounters> {
        Arc::clone(&self.shared.counters)
    }

    /// Read handle onto this call's spill artifact, when policy enabled one.
    pub fn spill_handle(&self) -> Option<SpillHandle> {
        self.shared.spill.as_ref().map(|state| SpillHandle {
            state: Arc::clone(state),
        })
    }

    /// Non-blocking bounded send. Policy, in order:
    /// 1. account production (and spill the FULL chunk when enabled —
    ///    before any cap/coalesce/drop decision);
    /// 2. cap the retained chunk at [`DELTA_MAX_CHUNK_BYTES`] (UTF-8-safe);
    /// 3. if the overflow buffer is non-empty, coalesce there (preserves
    ///    production order — channel items are always older than overflow
    ///    content);
    /// 4. else try the bounded channel; on Full coalesce, on Closed count a
    ///    drop and return immediately (the producer is RELEASED, never
    ///    blocked, once the consumer is gone).
    pub fn send(&self, chunk: String) {
        let counters = &self.shared.counters;
        counters.note_produced(chunk.len());
        if chunk.is_empty() {
            return;
        }
        if let Some(spill) = &self.shared.spill {
            spill.write(&chunk);
        }
        self.shared
            .model_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(&chunk, counters);
        let chunk = if chunk.len() > DELTA_MAX_CHUNK_BYTES {
            let bounded = agent_core::BoundedText::new(&chunk, DELTA_MAX_CHUNK_BYTES);
            counters.note_dropped(bounded.original_bytes - bounded.retained_bytes, 1);
            bounded.text
        } else {
            chunk
        };
        {
            let mut overflow = self
                .shared
                .overflow
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !overflow.is_empty() {
                coalesce_into(&mut overflow, &chunk, counters);
                drop(overflow);
                self.shared.notify.notify_one();
                return;
            }
        }
        match self.tx.try_send(chunk) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(chunk)) => {
                let mut overflow = self
                    .shared
                    .overflow
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                coalesce_into(&mut overflow, &chunk, counters);
                drop(overflow);
                self.shared.notify.notify_one();
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(chunk)) => {
                counters.note_dropped(chunk.len(), 1);
            }
        }
    }
}

impl DeltaReceiver {
    pub fn counters(&self) -> Arc<OutputCounters> {
        Arc::clone(&self.shared.counters)
    }

    fn drain_overflow(&self) -> Option<String> {
        let mut overflow = self
            .shared
            .overflow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if overflow.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut *overflow))
        }
    }

    /// Non-blocking drain of everything currently pending, coalesced into
    /// one batch in production order (channel first, then overflow).
    pub fn try_drain(&mut self) -> Option<String> {
        let mut batch = String::new();
        while let Ok(chunk) = self.rx.try_recv() {
            batch.push_str(&chunk);
        }
        if let Some(overflow) = self.drain_overflow() {
            batch.push_str(&overflow);
        }
        if batch.is_empty() {
            None
        } else {
            Some(batch)
        }
    }

    /// Await the next coalesced batch; `None` once every sender is dropped
    /// and all pending content (including overflow) is drained.
    pub async fn recv(&mut self) -> Option<String> {
        loop {
            if let Some(batch) = self.try_drain() {
                return Some(batch);
            }
            tokio::select! {
                maybe = self.rx.recv() => match maybe {
                    Some(first) => {
                        let mut batch = first;
                        while let Ok(chunk) = self.rx.try_recv() {
                            batch.push_str(&chunk);
                        }
                        if let Some(overflow) = self.drain_overflow() {
                            batch.push_str(&overflow);
                        }
                        return Some(batch);
                    }
                    None => return self.drain_overflow(),
                },
                _ = self.shared.notify.notified() => continue,
            }
        }
    }
}

// ── UI forwarder ────────────────────────────────────────────────────────────

/// Live UI forwarding tasks — a metadata-only diagnostic gauge the leak
/// harness asserts against after cancellation.
static ACTIVE_UI_FORWARDERS: AtomicUsize = AtomicUsize::new(0);

/// Number of UI forwarding tasks currently alive in this process.
pub fn active_ui_forwarder_count() -> usize {
    ACTIVE_UI_FORWARDERS.load(Ordering::SeqCst)
}

/// Spawn the forwarding task for one tool call: drains the bounded channel,
/// enforces the UI-preview byte budget at production time (UTF-8-safe via
/// `BoundedText`), and terminates on cancellation or channel close. On
/// termination the receiver drops, closing the channel and RELEASING every
/// producer (their sends become counted drops).
pub fn spawn_ui_forwarder(
    mut receiver: DeltaReceiver,
    ui_budget_bytes: usize,
    cancel: CancellationToken,
    emit: impl Fn(String) + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    ACTIVE_UI_FORWARDERS.fetch_add(1, Ordering::SeqCst);
    tokio::spawn(async move {
        /// Decrements the gauge on every exit path (return, cancel, panic).
        struct Gauge;
        impl Drop for Gauge {
            fn drop(&mut self) {
                ACTIVE_UI_FORWARDERS.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _gauge = Gauge;
        let counters = receiver.counters();
        let mut forwarded_total: usize = 0;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                maybe = receiver.recv() => match maybe {
                    None => break,
                    Some(batch) => {
                        let room = ui_budget_bytes.saturating_sub(forwarded_total);
                        let bounded = agent_core::BoundedText::new(&batch, room);
                        if bounded.retained_bytes > 0 {
                            forwarded_total += bounded.retained_bytes;
                            counters.note_forwarded(bounded.retained_bytes);
                            emit(bounded.text);
                        }
                        if bounded.truncated {
                            counters.note_dropped(
                                bounded.original_bytes - bounded.retained_bytes,
                                1,
                            );
                        }
                    }
                },
            }
        }
        // Cancellation may win while bytes remain in the bounded queue.
        // Receiver drop releases the producer; account every such byte as a
        // cancellation drop before the task exits.
        let retained = counters.snapshot().retained_bytes();
        if retained > 0 {
            counters.note_dropped(retained as usize, 1);
        }
        // Receiver drops here: the channel closes and producers are
        // released — a post-cancel send is a counted drop, never a block.
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A fast producer with NO consumer keeps retained bytes bounded by the
    /// channel + coalesce caps; everything beyond is a counted drop, and the
    /// accounting is exact (`produced == retained + forwarded + dropped`).
    #[tokio::test]
    async fn burst_without_consumer_bounds_retained_bytes_exactly() {
        let channel = delta_channel();
        let sender = channel.sender;
        let counters = sender.counters();
        let chunk = "x".repeat(1024);
        for _ in 0..10_000 {
            sender.send(chunk.clone());
        }
        let snap = counters.snapshot();
        assert_eq!(snap.produced_bytes, 10_000 * 1024);
        assert_eq!(snap.produced_chunks, 10_000);
        assert_eq!(snap.forwarded_bytes, 0);
        let ceiling = (DELTA_CHANNEL_CAPACITY * 1024 + DELTA_COALESCE_CAP_BYTES) as u64;
        assert!(
            snap.retained_bytes() <= ceiling,
            "retained {} exceeds ceiling {ceiling}",
            snap.retained_bytes()
        );
        assert!(snap.dropped_bytes > 0, "beyond-cap bytes must be dropped");
        assert!(snap.coalesced_chunks > 0, "full channel must coalesce");
        assert_eq!(
            snap.produced_bytes,
            snap.retained_bytes() + snap.forwarded_bytes + snap.dropped_bytes,
            "accounting must be exact"
        );
    }

    /// Whatever survives the coalesce/drop policy is a PREFIX of the
    /// produced stream: order is preserved and nothing is reordered across
    /// the channel/overflow split.
    #[tokio::test]
    async fn drained_output_is_an_ordered_prefix_of_production() {
        let mut channel = delta_channel();
        let mut full = String::new();
        for i in 0..2_000 {
            let chunk = format!("[chunk-{i:05}]");
            full.push_str(&chunk);
            channel.sender.send(chunk);
        }
        let mut received = String::new();
        while let Some(batch) = channel.receiver.try_drain() {
            received.push_str(&batch);
        }
        assert!(
            full.starts_with(&received),
            "received content must be an ordered prefix of production"
        );
        let snap = channel.sender.counters().snapshot();
        assert_eq!(
            received.len() as u64,
            snap.produced_bytes - snap.dropped_bytes,
            "drained bytes must equal produced minus dropped"
        );
    }

    /// An oversized single chunk retains only a bounded UTF-8-safe prefix.
    #[tokio::test]
    async fn oversized_chunk_is_capped_at_production() {
        let mut channel = delta_channel();
        // Multibyte char straddling the cap: BoundedText must cut safely.
        let big = "é".repeat(DELTA_MAX_CHUNK_BYTES); // 2 bytes each => 2x cap
        channel.sender.send(big.clone());
        let batch = channel.receiver.try_drain().expect("prefix retained");
        assert!(batch.len() <= DELTA_MAX_CHUNK_BYTES);
        assert!(big.starts_with(&batch));
        let snap = channel.sender.counters().snapshot();
        assert_eq!(snap.dropped_bytes, (big.len() - batch.len()) as u64);
    }

    /// A dropped receiver RELEASES the producer: sends return immediately
    /// as counted drops.
    #[tokio::test]
    async fn closed_channel_releases_producer_with_counted_drops() {
        let channel = delta_channel();
        drop(channel.receiver);
        let counters = channel.sender.counters();
        channel.sender.send("after close".to_string());
        let snap = counters.snapshot();
        assert_eq!(snap.dropped_bytes, "after close".len() as u64);
        assert_eq!(snap.dropped_chunks, 1);
        assert_eq!(snap.retained_bytes(), 0);
    }

    /// The forwarder enforces the UI budget (UTF-8-safe), counts the cut as
    /// dropped, and terminates when the producer hangs up.
    #[tokio::test]
    async fn forwarder_enforces_ui_budget_and_ends_on_close() {
        let channel = delta_channel();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let handle = spawn_ui_forwarder(
            channel.receiver,
            10,
            CancellationToken::new(),
            move |batch| {
                let _ = out_tx.send(batch);
            },
        );
        channel.sender.send("hello ".to_string());
        // Give the forwarder time to drain the first batch so the budget
        // boundary is exercised across separate emissions too.
        tokio::time::sleep(Duration::from_millis(50)).await;
        channel.sender.send("world!!".to_string());
        drop(channel.sender);
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("forwarder must terminate on close")
            .expect("forwarder must not panic");
        let mut emitted = String::new();
        while let Ok(batch) = out_rx.try_recv() {
            emitted.push_str(&batch);
        }
        assert_eq!(emitted.len(), 10, "UI budget must be exact");
        assert!("hello world!!".starts_with(&emitted));
    }

    /// Cancellation terminates the forwarding task (gauge returns to its
    /// baseline) and releases the producer (post-cancel sends are counted
    /// drops on a closed channel).
    #[tokio::test]
    async fn cancellation_closes_forwarder_and_releases_producer() {
        let baseline = active_ui_forwarder_count();
        let channel = delta_channel();
        let cancel = CancellationToken::new();
        let handle = spawn_ui_forwarder(channel.receiver, usize::MAX, cancel.clone(), |_| {});
        channel.sender.send("before cancel".to_string());
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("forwarder must terminate on cancellation")
            .expect("forwarder must not panic");
        // Gauge returns to baseline (poll: other tests may run in parallel).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while active_ui_forwarder_count() > baseline && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            active_ui_forwarder_count() <= baseline,
            "no forwarding task may remain alive after cancellation"
        );
        let counters = channel.sender.counters();
        let before = counters.snapshot();
        assert_eq!(
            before.retained_bytes(),
            0,
            "cancellation must account every queued/coalesced byte as dropped"
        );
        channel.sender.send("after cancel".to_string());
        let after = counters.snapshot();
        assert_eq!(
            after.dropped_bytes - before.dropped_bytes,
            "after cancel".len() as u64,
            "post-cancel sends must be released as counted drops"
        );
    }

    /// UI and model-history are separate production-time lanes: changing
    /// either budget cannot change the other lane's exact retained prefix.
    #[tokio::test]
    async fn ui_and_model_history_budgets_are_independent() {
        let budgets = OutputBudgets {
            ui_preview_bytes: 7,
            model_history_bytes: 11,
        };
        let channel = delta_channel_with_budgets(budgets, None);
        let output = channel.output_handle();
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let forwarder = spawn_ui_forwarder(
            channel.receiver,
            budgets.ui_preview_bytes,
            CancellationToken::new(),
            move |chunk| {
                let _ = ui_tx.send(chunk);
            },
        );
        channel.sender.send("abcdefgh".to_string());
        channel.sender.send("ijklmnop".to_string());
        drop(channel.sender);
        forwarder.await.expect("forwarder");

        let mut ui = String::new();
        while let Ok(chunk) = ui_rx.try_recv() {
            ui.push_str(&chunk);
        }
        let history = output.model_history();
        assert_eq!(ui, "abcdefg");
        assert_eq!(history.text, "abcdefghijk");
        assert!(history.truncated);
        assert_eq!(history.original_bytes, 16);
        assert_eq!(history.retained_bytes, 11);
    }

    /// A synthetic 1 GiB producer uses fixed-size generated chunks (never a
    /// real 1 GiB file/string); retained model history stays at its budget
    /// and every cut byte/chunk is reported exactly.
    #[tokio::test]
    async fn synthetic_one_gib_production_is_bounded_without_materializing_full_output() {
        const GIB: u64 = 1024 * 1024 * 1024;
        const CHUNK: usize = 64 * 1024;
        let budgets = OutputBudgets {
            ui_preview_bytes: 1024,
            model_history_bytes: 4096,
        };
        let channel = delta_channel_with_budgets(budgets, None);
        let output = channel.output_handle();
        let chunk = "x".repeat(CHUNK);
        for _ in 0..(GIB / CHUNK as u64) {
            channel.sender.send(chunk.clone());
        }
        let history = output.model_history();
        let snap = output.counters().snapshot();
        assert_eq!(history.original_bytes as u64, GIB);
        assert_eq!(history.retained_bytes, budgets.model_history_bytes);
        assert_eq!(snap.model_history_dropped_bytes, GIB - 4096);
        assert_eq!(snap.model_history_truncated_chunks, GIB / CHUNK as u64);
        assert!(snap.retained_bytes() <= OutputBudgets::max_ui_retained_bytes());
    }

    /// Spill is opt-in, captures the exact full production stream before
    /// truncation/drop, and T4 creates both directory and artifact privately.
    #[cfg(unix)]
    #[tokio::test]
    async fn optional_spill_is_exact_and_private_0600() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let dir = tmp.path().join("spill");
        let channel = delta_channel_with_budgets(
            OutputBudgets {
                ui_preview_bytes: 2,
                model_history_bytes: 3,
            },
            Some(SpillPolicy { dir: dir.clone() }),
        );
        let output = channel.output_handle();
        channel.sender.send("hello".to_string());
        channel.sender.send("🌟world".to_string());
        let report = output.spill_report().expect("spill enabled");

        assert!(!report.failed);
        assert_eq!(report.bytes, "hello🌟world".len() as u64);
        assert_eq!(
            std::fs::read_to_string(&report.path).unwrap(),
            "hello🌟world"
        );
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&report.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
