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

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
        }
    }
}

// ── Channel ─────────────────────────────────────────────────────────────────

struct DeltaShared {
    overflow: Mutex<String>,
    notify: tokio::sync::Notify,
    counters: Arc<OutputCounters>,
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

/// One bounded delta channel pair plus its shared counters.
pub struct ToolDeltaChannel {
    pub sender: DeltaSender,
    pub receiver: DeltaReceiver,
}

/// Construct a bounded delta channel with fresh counters.
pub fn delta_channel() -> ToolDeltaChannel {
    let (tx, rx) = tokio::sync::mpsc::channel(DELTA_CHANNEL_CAPACITY);
    let shared = Arc::new(DeltaShared {
        overflow: Mutex::new(String::new()),
        notify: tokio::sync::Notify::new(),
        counters: Arc::new(OutputCounters::default()),
    });
    ToolDeltaChannel {
        sender: DeltaSender {
            tx,
            shared: Arc::clone(&shared),
        },
        receiver: DeltaReceiver { rx, shared },
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

    /// Non-blocking bounded send. Policy, in order:
    /// 1. account production;
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
        channel.sender.send("after cancel".to_string());
        let after = counters.snapshot();
        assert_eq!(
            after.dropped_bytes - before.dropped_bytes,
            "after cancel".len() as u64,
            "post-cancel sends must be released as counted drops"
        );
    }
}
