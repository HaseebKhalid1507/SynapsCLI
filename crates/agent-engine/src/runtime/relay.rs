//! CP-11 fix-2 (A): bounded caller-facing stream boundary.
//!
//! `Runtime::run_stream_with_messages` produces [`StreamEvent`]s from an
//! internal turn task. Delivery to the CALLER is routed through this relay,
//! which enforces a fixed retention policy so a slow or absent consumer can
//! never make the runtime retain unbounded provider bytes:
//!
//! - the caller-facing channel is BOUNDED ([`RELAY_CHANNEL_CAPACITY`]
//!   events, each delta payload chunked to [`RELAY_FORWARD_CHUNK_BYTES`]);
//! - high-volume preview deltas (`LlmEvent::Text` / `Thinking` /
//!   `ToolUseDelta` / `ToolResultDelta`) are COALESCED once retention
//!   crosses [`RELAY_COALESCE_THRESHOLD_BYTES`] and DROPPED oldest-first
//!   (exact byte accounting) once retention would exceed
//!   [`RELAY_DELTA_RETAINED_BUDGET_BYTES`];
//! - semantic terminal/control/tool-call events (tool_use, tool_result,
//!   session history, errors, Done, agent events) are NEVER dropped or
//!   reordered — their volume is structurally bounded by the turn budget
//!   (calls × `max_tool_output`, rounds, one history per turn);
//! - a dropped caller stream cancels the turn token so provider tasks are
//!   released instead of streaming into the void.
//!
//! The internal producer hop stays an unbounded sender for API stability,
//! but the relay drains it EAGERLY (it never awaits caller capacity while
//! input is available), so retention there is scheduling-transient, not
//! caller-dependent; all caller-dependent retention is governed by the
//! budget above.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::runtime::types::{LlmEvent, StreamEvent};

/// Bounded caller-facing channel capacity, in events.
pub const RELAY_CHANNEL_CAPACITY: usize = 64;
/// Maximum delta payload bytes retained inside the relay while the caller
/// is not consuming. Beyond this, oldest preview bytes are dropped.
pub const RELAY_DELTA_RETAINED_BUDGET_BYTES: usize = 256 * 1024;
/// Retained-delta level above which adjacent same-class deltas coalesce.
/// Below it, event granularity is preserved for prompt consumers.
pub const RELAY_COALESCE_THRESHOLD_BYTES: usize = 64 * 1024;
/// Maximum delta payload bytes per forwarded event (large coalesced runs
/// are re-chunked), bounding bytes parked in the caller channel itself.
pub const RELAY_FORWARD_CHUNK_BYTES: usize = 16 * 1024;

static PRODUCED_DELTA_BYTES: AtomicU64 = AtomicU64::new(0);
static FORWARDED_DELTA_BYTES: AtomicU64 = AtomicU64::new(0);
static DROPPED_DELTA_BYTES: AtomicU64 = AtomicU64::new(0);
static RETAINED_DELTA_BYTES: AtomicU64 = AtomicU64::new(0);
static PEAK_RETAINED_DELTA_BYTES: AtomicU64 = AtomicU64::new(0);
static ACTIVE_RELAYS: AtomicU64 = AtomicU64::new(0);

/// Point-in-time view of the global relay accounting. `produced == forwarded
/// + dropped + retained` holds at quiescence; `retained` returns to zero
/// when every relay has terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelaySnapshot {
    pub produced_delta_bytes: u64,
    pub forwarded_delta_bytes: u64,
    pub dropped_delta_bytes: u64,
    pub retained_delta_bytes: u64,
    pub peak_retained_delta_bytes: u64,
    pub active_relays: u64,
}

pub fn stream_relay_snapshot() -> RelaySnapshot {
    RelaySnapshot {
        produced_delta_bytes: PRODUCED_DELTA_BYTES.load(Ordering::SeqCst),
        forwarded_delta_bytes: FORWARDED_DELTA_BYTES.load(Ordering::SeqCst),
        dropped_delta_bytes: DROPPED_DELTA_BYTES.load(Ordering::SeqCst),
        retained_delta_bytes: RETAINED_DELTA_BYTES.load(Ordering::SeqCst),
        peak_retained_delta_bytes: PEAK_RETAINED_DELTA_BYTES.load(Ordering::SeqCst),
        active_relays: ACTIVE_RELAYS.load(Ordering::SeqCst),
    }
}

/// Droppable preview payload length, or `None` for semantic events.
fn delta_len(event: &StreamEvent) -> Option<usize> {
    match event {
        StreamEvent::Llm(LlmEvent::Text(s)) | StreamEvent::Llm(LlmEvent::Thinking(s)) => {
            Some(s.len())
        }
        StreamEvent::Llm(LlmEvent::ToolUseDelta { delta, .. })
        | StreamEvent::Llm(LlmEvent::ToolResultDelta { delta, .. }) => Some(delta.len()),
        _ => None,
    }
}

/// Merge `incoming` into `tail` when both are the SAME preview class (and
/// same call id for tool deltas). Returns false when not coalescible.
fn try_coalesce(tail: &mut StreamEvent, incoming: &StreamEvent) -> bool {
    match (tail, incoming) {
        (StreamEvent::Llm(LlmEvent::Text(a)), StreamEvent::Llm(LlmEvent::Text(b))) => {
            a.push_str(b);
            true
        }
        (StreamEvent::Llm(LlmEvent::Thinking(a)), StreamEvent::Llm(LlmEvent::Thinking(b))) => {
            a.push_str(b);
            true
        }
        (
            StreamEvent::Llm(LlmEvent::ToolUseDelta {
                tool_id: ia,
                delta: da,
            }),
            StreamEvent::Llm(LlmEvent::ToolUseDelta {
                tool_id: ib,
                delta: db,
            }),
        ) if ia == ib => {
            da.push_str(db);
            true
        }
        (
            StreamEvent::Llm(LlmEvent::ToolResultDelta {
                tool_id: ia,
                delta: da,
            }),
            StreamEvent::Llm(LlmEvent::ToolResultDelta {
                tool_id: ib,
                delta: db,
            }),
        ) if ia == ib => {
            da.push_str(db);
            true
        }
        _ => false,
    }
}

/// Largest char boundary ≤ `n` (never 0 unless the string is empty or the
/// first char is longer than `n`, in which case the smallest boundary > 0
/// is used so progress is always made).
fn split_boundary(s: &str, n: usize) -> usize {
    let mut i = n.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    if i == 0 && !s.is_empty() {
        i = 1;
        while i < s.len() && !s.is_char_boundary(i) {
            i += 1;
        }
    }
    i
}

/// Trim AT LEAST `want` bytes from the front of `s` (char-boundary safe);
/// returns the exact number of bytes removed.
fn trim_front(s: &mut String, want: usize) -> usize {
    let mut cut = want.min(s.len());
    while cut < s.len() && !s.is_char_boundary(cut) {
        cut += 1;
    }
    s.drain(..cut);
    cut
}

fn payload_mut(event: &mut StreamEvent) -> Option<&mut String> {
    match event {
        StreamEvent::Llm(LlmEvent::Text(s)) | StreamEvent::Llm(LlmEvent::Thinking(s)) => Some(s),
        StreamEvent::Llm(LlmEvent::ToolUseDelta { delta, .. })
        | StreamEvent::Llm(LlmEvent::ToolResultDelta { delta, .. }) => Some(delta),
        _ => None,
    }
}

/// Rebuild the same delta variant carrying `chunk`.
fn with_payload(event: &StreamEvent, chunk: String) -> StreamEvent {
    match event {
        StreamEvent::Llm(LlmEvent::Text(_)) => StreamEvent::Llm(LlmEvent::Text(chunk)),
        StreamEvent::Llm(LlmEvent::Thinking(_)) => StreamEvent::Llm(LlmEvent::Thinking(chunk)),
        StreamEvent::Llm(LlmEvent::ToolUseDelta { tool_id, .. }) => {
            StreamEvent::Llm(LlmEvent::ToolUseDelta {
                tool_id: tool_id.clone(),
                delta: chunk,
            })
        }
        StreamEvent::Llm(LlmEvent::ToolResultDelta { tool_id, .. }) => {
            StreamEvent::Llm(LlmEvent::ToolResultDelta {
                tool_id: tool_id.clone(),
                delta: chunk,
            })
        }
        _ => unreachable!("with_payload is only called for delta events"),
    }
}

struct RelayState {
    pending: VecDeque<StreamEvent>,
    /// Delta payload bytes currently held in `pending` (mirrors the global
    /// RETAINED gauge contribution of THIS relay).
    retained: usize,
}

impl RelayState {
    fn new() -> Self {
        ACTIVE_RELAYS.fetch_add(1, Ordering::SeqCst);
        Self {
            pending: VecDeque::new(),
            retained: 0,
        }
    }

    fn ingest(&mut self, event: StreamEvent) {
        if let Some(bytes) = delta_len(&event) {
            PRODUCED_DELTA_BYTES.fetch_add(bytes as u64, Ordering::SeqCst);
            // Make room FIRST: the retained gauge must never exceed the
            // fixed budget, even between two statements.
            self.make_room_for(bytes);
            let mut event = event;
            if bytes > RELAY_DELTA_RETAINED_BUDGET_BYTES {
                // A single oversized delta: keep only its newest bytes.
                let payload = payload_mut(&mut event).expect("delta payload");
                let cut = trim_front(payload, bytes - RELAY_DELTA_RETAINED_BUDGET_BYTES);
                DROPPED_DELTA_BYTES.fetch_add(cut as u64, Ordering::SeqCst);
            }
            let kept = delta_len(&event).expect("delta payload");
            self.add_retained(kept);
            let coalesced = self.retained > RELAY_COALESCE_THRESHOLD_BYTES
                && self
                    .pending
                    .back_mut()
                    .is_some_and(|tail| try_coalesce(tail, &event));
            if !coalesced {
                self.pending.push_back(event);
            }
        } else {
            self.pending.push_back(event);
        }
    }

    fn add_retained(&mut self, bytes: usize) {
        self.retained += bytes;
        let now = RETAINED_DELTA_BYTES.fetch_add(bytes as u64, Ordering::SeqCst) + bytes as u64;
        PEAK_RETAINED_DELTA_BYTES.fetch_max(now, Ordering::SeqCst);
    }

    fn sub_retained(&mut self, bytes: usize) {
        self.retained -= bytes;
        RETAINED_DELTA_BYTES.fetch_sub(bytes as u64, Ordering::SeqCst);
    }

    /// Drop OLDEST preview bytes (whole events, then a front-trim of the
    /// oldest survivor) until `incoming` more bytes fit the fixed budget.
    /// Semantic events are never touched.
    fn make_room_for(&mut self, incoming: usize) {
        let need = incoming.min(RELAY_DELTA_RETAINED_BUDGET_BYTES);
        while self.retained + need > RELAY_DELTA_RETAINED_BUDGET_BYTES {
            let Some(index) = self
                .pending
                .iter()
                .position(|event| delta_len(event).is_some())
            else {
                break;
            };
            let over = self.retained + need - RELAY_DELTA_RETAINED_BUDGET_BYTES;
            let len = delta_len(&self.pending[index]).expect("droppable by position");
            if len <= over {
                self.pending.remove(index);
                self.sub_retained(len);
                DROPPED_DELTA_BYTES.fetch_add(len as u64, Ordering::SeqCst);
            } else {
                let payload = payload_mut(&mut self.pending[index]).expect("delta payload");
                let cut = trim_front(payload, over);
                self.sub_retained(cut);
                DROPPED_DELTA_BYTES.fetch_add(cut as u64, Ordering::SeqCst);
            }
        }
    }

    /// Next event to hand to the caller: semantic events go through whole;
    /// delta payloads larger than the forward chunk are split, with the
    /// remainder kept at the FRONT so order and content are preserved.
    fn next_out(&mut self) -> Option<StreamEvent> {
        let mut event = self.pending.pop_front()?;
        match delta_len(&event) {
            None => Some(event),
            Some(len) if len <= RELAY_FORWARD_CHUNK_BYTES => {
                self.sub_retained(len);
                FORWARDED_DELTA_BYTES.fetch_add(len as u64, Ordering::SeqCst);
                Some(event)
            }
            Some(_) => {
                let payload = payload_mut(&mut event).expect("delta payload");
                let cut = split_boundary(payload, RELAY_FORWARD_CHUNK_BYTES);
                let remainder = payload.split_off(cut);
                let chunk = std::mem::take(payload);
                if !remainder.is_empty() {
                    let rest = with_payload(&event, remainder);
                    self.pending.push_front(rest);
                }
                self.sub_retained(chunk.len());
                FORWARDED_DELTA_BYTES.fetch_add(chunk.len() as u64, Ordering::SeqCst);
                Some(with_payload(&event, chunk))
            }
        }
    }
}

impl Drop for RelayState {
    fn drop(&mut self) {
        // Whatever preview bytes were still parked when the relay ended
        // (caller gone / cancellation) are accounted as dropped so the
        // conservation identity holds and the gauge returns to zero.
        if self.retained > 0 {
            DROPPED_DELTA_BYTES.fetch_add(self.retained as u64, Ordering::SeqCst);
            RETAINED_DELTA_BYTES.fetch_sub(self.retained as u64, Ordering::SeqCst);
            self.retained = 0;
        }
        ACTIVE_RELAYS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Spawn the bounded relay between the runtime's internal event producer and
/// the caller-facing stream. When the caller drops the returned receiver,
/// `cancel` is cancelled so the producing turn (provider requests, tool
/// tasks) is released.
pub(crate) fn spawn_bounded_stream_relay(
    mut rx_in: mpsc::UnboundedReceiver<StreamEvent>,
    cancel: CancellationToken,
) -> mpsc::Receiver<StreamEvent> {
    let (tx_out, rx_out) = mpsc::channel(RELAY_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        let mut state = RelayState::new();
        let mut input_open = true;
        // Per-iteration drain burst: large enough to keep the inner hop
        // empty, small enough to interleave forwarding fairly.
        const DRAIN_BURST: usize = 128;
        loop {
            // 1. Eagerly drain whatever the producer already queued — this
            //    never waits on the caller, so inner-hop retention is
            //    scheduling-transient and policy-bounded here.
            let mut drained = 0;
            while input_open && drained < DRAIN_BURST {
                match rx_in.try_recv() {
                    Ok(event) => {
                        state.ingest(event);
                        drained += 1;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => input_open = false,
                }
            }
            // 2. Forward as much as the caller's bounded capacity allows,
            //    without blocking.
            while !state.pending.is_empty() {
                match tx_out.try_reserve() {
                    Ok(permit) => {
                        if let Some(event) = state.next_out() {
                            permit.send(event);
                        }
                    }
                    Err(mpsc::error::TrySendError::Full(())) => break,
                    Err(mpsc::error::TrySendError::Closed(())) => {
                        // Caller went away: release the producing turn.
                        cancel.cancel();
                        return;
                    }
                }
            }
            // 3. Wait for whichever side can make progress next.
            if !input_open && state.pending.is_empty() {
                return; // producer finished and everything was delivered
            }
            if input_open {
                if state.pending.is_empty() {
                    tokio::select! {
                        maybe = rx_in.recv() => match maybe {
                            Some(event) => state.ingest(event),
                            None => input_open = false,
                        },
                        // Idle producer + departed caller: release the turn
                        // instead of waiting for the next event.
                        _ = tx_out.closed() => {
                            cancel.cancel();
                            return;
                        }
                    }
                } else {
                    tokio::select! {
                        maybe = rx_in.recv() => match maybe {
                            Some(event) => state.ingest(event),
                            None => input_open = false,
                        },
                        permit = tx_out.reserve() => match permit {
                            Ok(permit) => {
                                if let Some(event) = state.next_out() {
                                    permit.send(event);
                                }
                            }
                            Err(_) => {
                                cancel.cancel();
                                return;
                            }
                        },
                    }
                }
            } else {
                // Input closed: flush the remaining backlog at the caller's
                // pace (semantic terminal events must arrive).
                if let Some(event) = state.next_out() {
                    if tx_out.send(event).await.is_err() {
                        cancel.cancel();
                        return;
                    }
                }
            }
        }
    });
    rx_out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::time::{Duration, Instant};

    const CHUNK: usize = 4096;

    fn text(bytes: usize) -> StreamEvent {
        StreamEvent::Llm(LlmEvent::Text("x".repeat(bytes)))
    }

    async fn wait_until(deadline: Duration, mut check: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + deadline;
        while Instant::now() < end {
            if check() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        check()
    }

    /// A hostile flood with a STALLED consumer must keep relay retention
    /// within the fixed budget, account every dropped byte, and conserve
    /// produced == forwarded + dropped once the relay terminates.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial(stream_relay)]
    async fn flood_with_stalled_consumer_keeps_retention_within_fixed_budget() {
        let base = stream_relay_snapshot();
        let (tx, rx_in) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let mut rx_out = spawn_bounded_stream_relay(rx_in, cancel);

        const EVENTS: usize = 10_000; // ~40 MiB of preview deltas
        for _ in 0..EVENTS {
            tx.send(text(CHUNK)).unwrap();
        }
        let produced_target = (EVENTS * CHUNK) as u64;
        assert!(
            wait_until(Duration::from_secs(20), || {
                stream_relay_snapshot().produced_delta_bytes - base.produced_delta_bytes
                    == produced_target
            })
            .await,
            "relay must ingest the full flood without caller progress"
        );
        let during = stream_relay_snapshot();
        assert!(
            during.retained_delta_bytes - base.retained_delta_bytes
                <= RELAY_DELTA_RETAINED_BUDGET_BYTES as u64,
            "stalled-consumer retention must stay within the fixed budget: {}",
            during.retained_delta_bytes - base.retained_delta_bytes
        );
        assert!(
            during.dropped_delta_bytes > base.dropped_delta_bytes,
            "a flood beyond the budget must record dropped preview bytes"
        );

        // Consumer wakes up after producer hangup: bounded total delivery.
        drop(tx);
        let mut delivered = 0usize;
        while let Some(event) = rx_out.recv().await {
            delivered += delta_len(&event).unwrap_or(0);
        }
        assert!(
            delivered
                <= RELAY_DELTA_RETAINED_BUDGET_BYTES
                    + RELAY_CHANNEL_CAPACITY * RELAY_FORWARD_CHUNK_BYTES
                    + CHUNK,
            "late delivery must be bounded by budget + channel capacity, got {delivered}"
        );
        assert!(delivered > 0, "the newest preview bytes must survive");
        assert!(
            wait_until(Duration::from_secs(5), || {
                stream_relay_snapshot().active_relays == base.active_relays
            })
            .await,
            "relay task must terminate"
        );
        let end = stream_relay_snapshot();
        assert_eq!(
            end.produced_delta_bytes - base.produced_delta_bytes,
            (end.forwarded_delta_bytes - base.forwarded_delta_bytes)
                + (end.dropped_delta_bytes - base.dropped_delta_bytes),
            "conservation: every produced preview byte is forwarded or dropped"
        );
        assert_eq!(end.retained_delta_bytes, base.retained_delta_bytes);
    }

    /// Semantic control/terminal/tool-call events are NEVER dropped by the
    /// flood policy and arrive in order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial(stream_relay)]
    async fn semantic_events_survive_flood_in_order() {
        let (tx, rx_in) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let mut rx_out = spawn_bounded_stream_relay(rx_in, cancel);

        let semantic = [
            StreamEvent::Llm(LlmEvent::ToolUseStart {
                tool_name: "bash".into(),
                tool_id: "toolu_s1".into(),
            }),
            StreamEvent::Llm(LlmEvent::ToolUse {
                tool_name: "bash".into(),
                tool_id: "toolu_s1".into(),
                input: serde_json::json!({"command": "true"}),
            }),
            StreamEvent::Llm(LlmEvent::ToolResult {
                tool_id: "toolu_s1".into(),
                result: "ok".into(),
            }),
            StreamEvent::Session(crate::runtime::types::SessionEvent::Done),
        ];
        for event in &semantic {
            for _ in 0..2_000 {
                tx.send(text(CHUNK)).unwrap(); // ~8 MiB flood before each
            }
            tx.send(event.clone()).unwrap();
        }
        drop(tx);

        let mut seen = Vec::new();
        while let Some(event) = rx_out.recv().await {
            match &event {
                StreamEvent::Llm(LlmEvent::ToolUseStart { tool_id, .. }) => {
                    seen.push(format!("start:{tool_id}"))
                }
                StreamEvent::Llm(LlmEvent::ToolUse { tool_id, .. }) => {
                    seen.push(format!("use:{tool_id}"))
                }
                StreamEvent::Llm(LlmEvent::ToolResult { tool_id, .. }) => {
                    seen.push(format!("result:{tool_id}"))
                }
                StreamEvent::Session(crate::runtime::types::SessionEvent::Done) => {
                    seen.push("done".into())
                }
                _ => {}
            }
        }
        assert_eq!(
            seen,
            vec!["start:toolu_s1", "use:toolu_s1", "result:toolu_s1", "done"],
            "every semantic event must survive the flood, in order"
        );
    }

    /// Dropping the caller stream cancels the turn token (releasing the
    /// producing provider tasks) and terminates the relay.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial(stream_relay)]
    async fn caller_drop_cancels_turn_token_and_ends_relay() {
        let base = stream_relay_snapshot();
        let (tx, rx_in) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let rx_out = spawn_bounded_stream_relay(rx_in, cancel.clone());
        drop(rx_out);

        assert!(
            wait_until(Duration::from_secs(5), || cancel.is_cancelled()).await,
            "caller drop must cancel the turn token"
        );
        assert!(
            wait_until(Duration::from_secs(5), || {
                stream_relay_snapshot().active_relays == base.active_relays
            })
            .await,
            "relay must terminate after the caller departs"
        );
        drop(tx);
    }

    /// A promptly-consuming caller keeps full delta granularity: no
    /// coalescing below the threshold, exact payloads, exact order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial(stream_relay)]
    async fn prompt_consumer_preserves_delta_granularity() {
        let (tx, rx_in) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let mut rx_out = spawn_bounded_stream_relay(rx_in, cancel);

        for part in ["alpha ", "beta ", "gamma ", "delta ", "epsilon"] {
            tx.send(StreamEvent::Llm(LlmEvent::Text(part.into())))
                .unwrap();
        }
        drop(tx);
        let mut parts = Vec::new();
        while let Some(event) = rx_out.recv().await {
            match event {
                StreamEvent::Llm(LlmEvent::Text(s)) => parts.push(s),
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert_eq!(
            parts.concat(),
            "alpha beta gamma delta epsilon",
            "prompt consumers must observe complete text"
        );
        assert!(
            parts.len() >= 2,
            "small deltas below the coalesce threshold keep granularity"
        );
    }
}
