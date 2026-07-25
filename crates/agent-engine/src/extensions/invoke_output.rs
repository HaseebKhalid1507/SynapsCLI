//! CP-11 fix-3: bounded, budgeted collection of `command.invoke` output.
//!
//! ## Threat model
//!
//! An interactive plugin command streams `command.output` / `task.*`
//! notifications to the host while its `command.invoke` JSON-RPC response
//! is pending. The pre-fix architecture handed the producer an
//! `mpsc::UnboundedSender<InvokeCommandEvent>` whose consumer (the TUI)
//! drained it only AFTER the call resolved — so a hostile or
//! malfunctioning extension could park an aggregate flood of output
//! bytes in host memory for the whole invocation window. The 4 MiB
//! per-frame reader cap bounds individual frames, the 120 s invoke
//! timeout bounds duration, and the one-in-flight user action bounds
//! concurrency — none of them bounds aggregate queued bytes.
//!
//! ## Architecture
//!
//! A naive bounded-channel substitution would deadlock: the invocation
//! loop awaits sends into the sink while the TUI's post-hoc drain waits
//! for the call to resolve. Instead, the sink is BOUNDED
//! ([`INVOKE_EVENT_QUEUE_CAPACITY`]) and paired with an EAGERLY
//! CONCURRENT collector that runs alongside the invocation (see
//! `ExtensionManager::invoke_command_collected`, which joins both
//! futures). The collector consumes unconditionally — it never waits on
//! anything but the channel — so producers are always paced, never
//! parked behind an absent consumer, and budgets are enforced at
//! PRODUCTION time:
//!
//! - retained payload bytes are capped by
//!   [`InvokeOutputBudget::max_retained_payload_bytes`], with the final
//!   partially-fitting text event truncated UTF-8-safely via
//!   [`agent_core::BoundedText`];
//! - retained events are capped by
//!   [`InvokeOutputBudget::max_retained_events`];
//! - terminal/control semantics are preserved past budget exhaustion
//!   through a small dedicated control reserve: the first
//!   `CommandOutputEvent::Done` marker is always retained, and a
//!   `task.done` for a task whose `task.start` was retained is retained
//!   (at most once per task id) so no retained task is left dangling;
//! - everything else past budget is dropped WHOLE with exact byte/event
//!   accounting; dropped `Error` events are additionally counted so
//!   callers can surface error visibility (see
//!   [`InvokeOutputReport::limit_notice`]).
//!
//! Truncation/coalescing policy is deterministic: head retention in
//! arrival order; only `Text`/`System`/`Error` content is partially
//! truncated (single-string payloads); `Table` and `task.*` events never
//! fit partially — they are retained whole or dropped whole.
//!
//! Cancellation/timeout/drop safety: the sink is single-ownership (not
//! `Clone`); when the invocation future is dropped (caller cancellation,
//! 120 s timeout, transport failure) the sink drops with it, the
//! collector's channel closes and [`InvokeOutputCollector::collect`]
//! returns promptly, and any producer blocked on an awaited send is
//! released by the dropped receiver. No detached tasks are involved.
//!
//! Counter identities (asserted by tests):
//!
//! - `consumed_payload_bytes == retained + truncated + dropped` bytes;
//! - `consumed_events == retained_events + dropped_events`
//!   (truncated events are retained);
//! - `produced_* >= consumed_*`, equal while every send succeeds;
//! - `retained_payload_bytes <= max_retained_payload_bytes +
//!   control_reserve_bytes` at every instant.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::extensions::commands::CommandOutputEvent;
use crate::extensions::runtime::InvokeCommandEvent;
use crate::extensions::tasks::TaskEvent;

/// Bounded producer→collector handoff capacity, in events. Small on
/// purpose: with an eagerly-draining collector, channel-parked bytes are
/// scheduling-transient; capacity only smooths bursts (mirrors
/// `NOTIFICATION_QUEUE_CAPACITY` in the transport underneath).
pub const INVOKE_EVENT_QUEUE_CAPACITY: usize = 8;

/// Default invocation-local retained payload byte budget. Command output
/// is chat-transcript text; 256 KiB comfortably covers legitimate
/// commands while capping hostile floods.
pub const INVOKE_RETAINED_BYTE_BUDGET: usize = 256 * 1024;

/// Default invocation-local retained event budget (guards against floods
/// of many tiny events — e.g. hostile `task.start` storms — that stay
/// under the byte budget).
pub const INVOKE_RETAINED_EVENT_BUDGET: usize = 1024;

/// Byte reserve usable ONLY by post-budget control events (`Done`
/// markers, `task.done` completion for already-retained tasks) so
/// terminal semantics survive budget exhaustion. Its consumers are
/// structurally bounded: at most one `task.done` per retained task id
/// plus one `Done` marker.
pub const INVOKE_CONTROL_RESERVE_BYTES: usize = 16 * 1024;

/// Invocation-local budgets applied by the collector at production time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvokeOutputBudget {
    /// Max payload bytes retained for post-invoke consumption
    /// (excluding the control reserve).
    pub max_retained_payload_bytes: usize,
    /// Max events retained for post-invoke consumption (control-reserve
    /// retentions may exceed this by their structurally bounded count).
    pub max_retained_events: usize,
    /// Byte reserve for post-budget control retention.
    pub control_reserve_bytes: usize,
}

impl Default for InvokeOutputBudget {
    fn default() -> Self {
        Self {
            max_retained_payload_bytes: INVOKE_RETAINED_BYTE_BUDGET,
            max_retained_events: INVOKE_RETAINED_EVENT_BUDGET,
            control_reserve_bytes: INVOKE_CONTROL_RESERVE_BYTES,
        }
    }
}

impl InvokeOutputBudget {
    /// Effectively unlimited budgets. MUTATION ORACLE ONLY: tests use
    /// this to prove the bounded-retention assertions are sensitive to
    /// the budget (disabling it must retain the whole flood). Production
    /// call sites must use [`InvokeOutputBudget::default`].
    pub fn unlimited() -> Self {
        Self {
            max_retained_payload_bytes: usize::MAX,
            max_retained_events: usize::MAX,
            control_reserve_bytes: 0,
        }
    }
}

/// Exact invocation-local accounting. See module docs for identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InvokeOutputCounters {
    /// Events/payload bytes the producer side handed to the sink
    /// (counted BEFORE the send, so a closed channel still counts).
    pub produced_events: u64,
    pub produced_payload_bytes: u64,
    /// Events/payload bytes the collector received from the channel.
    pub consumed_events: u64,
    pub consumed_payload_bytes: u64,
    /// Events/payload bytes retained for post-invoke consumption
    /// (truncated events count here with their KEPT byte length).
    pub retained_events: u64,
    pub retained_payload_bytes: u64,
    /// Events partially truncated to fit the byte budget, and the exact
    /// bytes trimmed from them.
    pub truncated_events: u64,
    pub truncated_payload_bytes: u64,
    /// Events dropped whole, and their exact payload bytes.
    pub dropped_events: u64,
    pub dropped_payload_bytes: u64,
    /// Subset of `dropped_events` that were `CommandOutputEvent::Error`
    /// — surfaced so error visibility survives the drop policy.
    pub dropped_error_events: u64,
    /// Whether the producer emitted a `CommandOutputEvent::Done`
    /// terminal marker.
    pub saw_done: bool,
}

/// Everything retained from one `command.invoke` output stream, plus
/// exact accounting.
#[derive(Debug)]
pub struct InvokeOutputReport {
    /// Retained events in arrival order (post truncation policy).
    pub events: Vec<InvokeCommandEvent>,
    pub counters: InvokeOutputCounters,
}

impl InvokeOutputReport {
    /// True when any budget limit affected the output.
    pub fn is_limited(&self) -> bool {
        self.counters.dropped_events > 0 || self.counters.truncated_events > 0
    }

    /// Human-readable limit notice with exact totals, or `None` when the
    /// output was fully retained. `severity_error` is set when dropped
    /// events included `Error` events (callers should surface the notice
    /// on their error channel so error visibility is preserved).
    pub fn limit_notice(&self) -> Option<InvokeOutputNotice> {
        if !self.is_limited() {
            return None;
        }
        let c = &self.counters;
        let mut message = format!(
            "command output limited by invocation budget: produced {} bytes in {} events; \
             retained {} bytes in {} events; truncated {} bytes across {} events; \
             dropped {} bytes across {} events",
            c.produced_payload_bytes,
            c.produced_events,
            c.retained_payload_bytes,
            c.retained_events,
            c.truncated_payload_bytes,
            c.truncated_events,
            c.dropped_payload_bytes,
            c.dropped_events,
        );
        if c.dropped_error_events > 0 {
            message.push_str(&format!(
                " (including {} error events)",
                c.dropped_error_events
            ));
        }
        Some(InvokeOutputNotice {
            severity_error: c.dropped_error_events > 0,
            message,
        })
    }
}

/// Limit notice for UI surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvokeOutputNotice {
    pub message: String,
    /// True when error events were dropped — surface as an error.
    pub severity_error: bool,
}

/// Payload byte weight of one event: the UTF-8 byte length of every
/// producer-controlled string it carries. `Done` is a zero-byte marker.
pub fn event_payload_bytes(event: &InvokeCommandEvent) -> usize {
    match event {
        InvokeCommandEvent::Output(CommandOutputEvent::Text { content })
        | InvokeCommandEvent::Output(CommandOutputEvent::System { content })
        | InvokeCommandEvent::Output(CommandOutputEvent::Error { content }) => content.len(),
        InvokeCommandEvent::Output(CommandOutputEvent::Table { headers, rows }) => {
            headers.iter().map(String::len).sum::<usize>()
                + rows
                    .iter()
                    .map(|r| r.iter().map(String::len).sum::<usize>())
                    .sum::<usize>()
        }
        InvokeCommandEvent::Output(CommandOutputEvent::Done) => 0,
        InvokeCommandEvent::Task(TaskEvent::Start { id, label, .. }) => id.len() + label.len(),
        InvokeCommandEvent::Task(TaskEvent::Update { id, message, .. }) => {
            id.len() + message.as_deref().map(str::len).unwrap_or(0)
        }
        InvokeCommandEvent::Task(TaskEvent::Log { id, line }) => id.len() + line.len(),
        InvokeCommandEvent::Task(TaskEvent::Done { id, error }) => {
            id.len() + error.as_deref().map(str::len).unwrap_or(0)
        }
    }
}

/// Shared produced-side meter (producer counts before each send).
#[derive(Debug, Default)]
struct InvokeMeter {
    produced_events: AtomicU64,
    produced_payload_bytes: AtomicU64,
}

/// Error returned by [`InvokeEventSink::send`] when the collector (and
/// its channel) is gone. Producers treat this exactly like the old
/// closed-unbounded-sink case: stop forwarding, let the call finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvokeSinkClosed;

/// Bounded, metered producer handle for `command.invoke` events.
///
/// Single-ownership on purpose (no `Clone`): dropping the invocation
/// future drops the only sender, which closes the collector's channel —
/// the property cancellation safety relies on.
#[derive(Debug)]
pub struct InvokeEventSink {
    tx: mpsc::Sender<InvokeCommandEvent>,
    meter: Arc<InvokeMeter>,
}

impl InvokeEventSink {
    /// Awaited, backpressured send. Counts the event as produced whether
    /// or not the collector is still there.
    pub async fn send(&self, event: InvokeCommandEvent) -> Result<(), InvokeSinkClosed> {
        self.meter.produced_events.fetch_add(1, Ordering::SeqCst);
        self.meter
            .produced_payload_bytes
            .fetch_add(event_payload_bytes(&event) as u64, Ordering::SeqCst);
        self.tx.send(event).await.map_err(|_| InvokeSinkClosed)
    }

    /// Whether the collector side is gone.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// Eagerly concurrent consumer for one invocation's event stream.
#[derive(Debug)]
pub struct InvokeOutputCollector {
    rx: mpsc::Receiver<InvokeCommandEvent>,
    meter: Arc<InvokeMeter>,
    budget: InvokeOutputBudget,
}

/// Create the bounded sink/collector pair for one `command.invoke`.
pub fn invoke_event_channel(
    budget: InvokeOutputBudget,
) -> (InvokeEventSink, InvokeOutputCollector) {
    let (tx, rx) = mpsc::channel(INVOKE_EVENT_QUEUE_CAPACITY);
    let meter = Arc::new(InvokeMeter::default());
    (
        InvokeEventSink {
            tx,
            meter: Arc::clone(&meter),
        },
        InvokeOutputCollector { rx, meter, budget },
    )
}

impl InvokeOutputCollector {
    /// Drain the channel until the sink drops, applying the budget policy
    /// to every event as it is produced. Must run CONCURRENTLY with the
    /// invocation future (e.g. `tokio::join!`) — it is the consumer that
    /// keeps bounded producers from parking.
    pub async fn collect(mut self) -> InvokeOutputReport {
        let mut state = CollectorState::new(self.budget);
        while let Some(event) = self.rx.recv().await {
            state.ingest(event);
        }
        state.finish(&self.meter)
    }
}

/// Budget-enforcing retention state. Separated from the channel so the
/// policy is unit-testable synchronously.
struct CollectorState {
    budget: InvokeOutputBudget,
    events: Vec<InvokeCommandEvent>,
    counters: InvokeOutputCounters,
    /// Ordinary-budget bytes used (excludes control reserve).
    ordinary_bytes: usize,
    /// Control-reserve bytes used.
    control_bytes: usize,
    /// Task ids whose `task.start` is retained.
    started_task_ids: std::collections::HashSet<String>,
    /// Task ids whose `task.done` is already retained.
    done_task_ids: std::collections::HashSet<String>,
}

impl CollectorState {
    fn new(budget: InvokeOutputBudget) -> Self {
        Self {
            budget,
            events: Vec::new(),
            counters: InvokeOutputCounters::default(),
            ordinary_bytes: 0,
            control_bytes: 0,
            started_task_ids: std::collections::HashSet::new(),
            done_task_ids: std::collections::HashSet::new(),
        }
    }

    fn ingest(&mut self, event: InvokeCommandEvent) {
        let bytes = event_payload_bytes(&event);
        self.counters.consumed_events += 1;
        self.counters.consumed_payload_bytes += bytes as u64;

        match event {
            InvokeCommandEvent::Output(CommandOutputEvent::Done) => {
                if self.counters.saw_done {
                    // Duplicate terminal markers carry no information.
                    self.drop_whole(0, false);
                } else {
                    self.counters.saw_done = true;
                    // Zero-byte control retention: always fits.
                    self.retain_control(InvokeCommandEvent::Output(CommandOutputEvent::Done), 0);
                }
            }
            InvokeCommandEvent::Task(TaskEvent::Done { id, error }) => {
                let within_ordinary = self.fits_ordinary(bytes);
                let already_done = self.done_task_ids.contains(&id);
                if within_ordinary && !already_done {
                    self.done_task_ids.insert(id.clone());
                    self.retain_ordinary(
                        InvokeCommandEvent::Task(TaskEvent::Done { id, error }),
                        bytes,
                    );
                } else if !already_done
                    && self.started_task_ids.contains(&id)
                    && self.control_bytes + bytes <= self.budget.control_reserve_bytes
                {
                    // Past budget, but the task's start is retained:
                    // preserve completion so no retained task dangles.
                    self.done_task_ids.insert(id.clone());
                    self.retain_control(
                        InvokeCommandEvent::Task(TaskEvent::Done { id, error }),
                        bytes,
                    );
                } else {
                    self.drop_whole(bytes, false);
                }
            }
            event => self.ingest_ordinary(event, bytes),
        }
    }

    fn fits_ordinary(&self, bytes: usize) -> bool {
        (self.counters.retained_events as usize) < self.budget.max_retained_events
            && self
                .ordinary_bytes
                .checked_add(bytes)
                .is_some_and(|total| total <= self.budget.max_retained_payload_bytes)
    }

    fn ingest_ordinary(&mut self, event: InvokeCommandEvent, bytes: usize) {
        let is_error = is_error_event(&event);
        if (self.counters.retained_events as usize) >= self.budget.max_retained_events {
            self.drop_whole(bytes, is_error);
            return;
        }
        let remaining = self
            .budget
            .max_retained_payload_bytes
            .saturating_sub(self.ordinary_bytes);
        if bytes <= remaining {
            if let InvokeCommandEvent::Task(TaskEvent::Start { id, .. }) = &event {
                self.started_task_ids.insert(id.clone());
            }
            self.retain_ordinary(event, bytes);
            return;
        }
        // Partially fitting single-string text payloads truncate
        // UTF-8-safely to exactly the remaining budget; everything else
        // drops whole (Tables/task events have no deterministic partial
        // representation).
        if remaining > 0 {
            if let Some((rebuild, content)) = truncatable_content(event) {
                let bounded = agent_core::BoundedText::new(&content, remaining);
                let kept = bounded.retained_bytes;
                debug_assert!(bounded.truncated);
                self.counters.truncated_events += 1;
                self.counters.truncated_payload_bytes += (bytes - kept) as u64;
                self.retain_ordinary(rebuild(bounded.text), kept);
                return;
            }
            // Not partially representable — fell through to a whole drop.
        }
        self.drop_whole(bytes, is_error);
    }

    fn retain_ordinary(&mut self, event: InvokeCommandEvent, kept_bytes: usize) {
        self.ordinary_bytes += kept_bytes;
        self.counters.retained_events += 1;
        self.counters.retained_payload_bytes += kept_bytes as u64;
        self.events.push(event);
    }

    fn retain_control(&mut self, event: InvokeCommandEvent, kept_bytes: usize) {
        self.control_bytes += kept_bytes;
        self.counters.retained_events += 1;
        self.counters.retained_payload_bytes += kept_bytes as u64;
        self.events.push(event);
    }

    fn drop_whole(&mut self, bytes: usize, is_error: bool) {
        self.counters.dropped_events += 1;
        self.counters.dropped_payload_bytes += bytes as u64;
        if is_error {
            self.counters.dropped_error_events += 1;
        }
    }

    fn finish(mut self, meter: &InvokeMeter) -> InvokeOutputReport {
        self.counters.produced_events = meter.produced_events.load(Ordering::SeqCst);
        self.counters.produced_payload_bytes = meter.produced_payload_bytes.load(Ordering::SeqCst);
        debug_assert!(
            self.counters.retained_payload_bytes
                <= (self.budget.max_retained_payload_bytes as u64)
                    .saturating_add(self.budget.control_reserve_bytes as u64),
            "retained bytes must never exceed budget + control reserve",
        );
        InvokeOutputReport {
            events: self.events,
            counters: self.counters,
        }
    }
}

fn is_error_event(event: &InvokeCommandEvent) -> bool {
    matches!(
        event,
        InvokeCommandEvent::Output(CommandOutputEvent::Error { .. })
    )
}

/// For single-string text payloads, return the content plus a rebuilder
/// applied after truncation. `None` for events with no deterministic
/// partial representation.
#[allow(clippy::type_complexity)]
fn truncatable_content(
    event: InvokeCommandEvent,
) -> Option<(fn(String) -> InvokeCommandEvent, String)> {
    match event {
        InvokeCommandEvent::Output(CommandOutputEvent::Text { content }) => Some((
            |content| InvokeCommandEvent::Output(CommandOutputEvent::Text { content }),
            content,
        )),
        InvokeCommandEvent::Output(CommandOutputEvent::System { content }) => Some((
            |content| InvokeCommandEvent::Output(CommandOutputEvent::System { content }),
            content,
        )),
        InvokeCommandEvent::Output(CommandOutputEvent::Error { content }) => Some((
            |content| InvokeCommandEvent::Output(CommandOutputEvent::Error { content }),
            content,
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(content: &str) -> InvokeCommandEvent {
        InvokeCommandEvent::Output(CommandOutputEvent::Text {
            content: content.to_string(),
        })
    }

    fn budget(bytes: usize, events: usize, reserve: usize) -> InvokeOutputBudget {
        InvokeOutputBudget {
            max_retained_payload_bytes: bytes,
            max_retained_events: events,
            control_reserve_bytes: reserve,
        }
    }

    fn ingest_all(
        budget: InvokeOutputBudget,
        events: Vec<InvokeCommandEvent>,
    ) -> InvokeOutputReport {
        let mut state = CollectorState::new(budget);
        let meter = InvokeMeter::default();
        for ev in events {
            meter.produced_events.fetch_add(1, Ordering::SeqCst);
            meter
                .produced_payload_bytes
                .fetch_add(event_payload_bytes(&ev) as u64, Ordering::SeqCst);
            state.ingest(ev);
        }
        state.finish(&meter)
    }

    fn assert_conservation(report: &InvokeOutputReport) {
        let c = &report.counters;
        assert_eq!(
            c.consumed_payload_bytes,
            c.retained_payload_bytes + c.truncated_payload_bytes + c.dropped_payload_bytes,
            "byte conservation"
        );
        assert_eq!(
            c.consumed_events,
            c.retained_events + c.dropped_events,
            "event conservation"
        );
        assert_eq!(report.events.len() as u64, c.retained_events);
    }

    #[test]
    fn under_budget_output_is_retained_verbatim() {
        let events = vec![
            text("hello"),
            text("world"),
            InvokeCommandEvent::Output(CommandOutputEvent::Done),
        ];
        let report = ingest_all(InvokeOutputBudget::default(), events.clone());
        assert_eq!(report.events, events);
        assert!(!report.is_limited());
        assert!(report.limit_notice().is_none());
        assert!(report.counters.saw_done);
        assert_eq!(report.counters.retained_payload_bytes, 10);
        assert_eq!(report.counters.produced_payload_bytes, 10);
        assert_conservation(&report);
    }

    #[test]
    fn head_retention_truncates_boundary_event_then_drops_rest() {
        // Budget 10: "abcdef" (6) fits, "ghijkl" (6) truncates to 4, rest drops.
        let report = ingest_all(
            budget(10, 100, 0),
            vec![text("abcdef"), text("ghijkl"), text("mnopqr")],
        );
        assert_eq!(report.events, vec![text("abcdef"), text("ghij")]);
        let c = &report.counters;
        assert_eq!(c.retained_payload_bytes, 10);
        assert_eq!(c.truncated_events, 1);
        assert_eq!(c.truncated_payload_bytes, 2);
        assert_eq!(c.dropped_events, 1);
        assert_eq!(c.dropped_payload_bytes, 6);
        assert_conservation(&report);
    }

    #[test]
    fn truncation_is_utf8_char_boundary_safe() {
        // "éé" is 4 bytes; budget 3 keeps only the first 'é' (2 bytes).
        let report = ingest_all(budget(3, 100, 0), vec![text("éé")]);
        assert_eq!(report.events, vec![text("é")]);
        assert_eq!(report.counters.retained_payload_bytes, 2);
        assert_eq!(report.counters.truncated_payload_bytes, 2);
        assert_conservation(&report);
    }

    #[test]
    fn event_budget_caps_tiny_event_floods() {
        let flood: Vec<_> = (0..500).map(|i| text(&format!("{i}"))).collect();
        let report = ingest_all(budget(usize::MAX, 16, 0), flood);
        assert_eq!(report.counters.retained_events, 16);
        assert_eq!(report.counters.dropped_events, 484);
        assert_conservation(&report);
    }

    #[test]
    fn first_done_marker_survives_budget_exhaustion() {
        let report = ingest_all(
            budget(4, 100, 1024),
            vec![
                text("aaaa"),
                text("bbbb"),
                InvokeCommandEvent::Output(CommandOutputEvent::Done),
                InvokeCommandEvent::Output(CommandOutputEvent::Done),
            ],
        );
        assert!(report.counters.saw_done);
        assert_eq!(
            report.events,
            vec![
                text("aaaa"),
                InvokeCommandEvent::Output(CommandOutputEvent::Done)
            ]
        );
        // The duplicate Done was dropped (zero bytes).
        assert_eq!(report.counters.dropped_events, 2);
        assert_conservation(&report);
    }

    #[test]
    fn task_done_for_retained_start_survives_budget_exhaustion() {
        let start = InvokeCommandEvent::Task(TaskEvent::Start {
            id: "t1".into(),
            label: "L".into(),
            kind: crate::extensions::tasks::TaskKind::Download,
        });
        let done = InvokeCommandEvent::Task(TaskEvent::Done {
            id: "t1".into(),
            error: None,
        });
        let report = ingest_all(
            budget(3, 100, 1024),
            vec![start.clone(), text("xxxxxxxx"), done.clone(), done.clone()],
        );
        // start (3 bytes) fits; flood text dropped; first done retained
        // via control reserve; duplicate done dropped.
        assert_eq!(report.events, vec![start, done]);
        assert_eq!(report.counters.dropped_events, 2);
        assert_conservation(&report);
    }

    #[test]
    fn task_done_for_unknown_task_does_not_consume_control_reserve() {
        let done = InvokeCommandEvent::Task(TaskEvent::Done {
            id: "ghost".into(),
            error: None,
        });
        let report = ingest_all(budget(0, 0, 1024), vec![done]);
        assert!(report.events.is_empty());
        assert_eq!(report.counters.dropped_events, 1);
        assert_conservation(&report);
    }

    #[test]
    fn dropped_error_events_are_counted_and_flagged_in_notice() {
        let report = ingest_all(
            budget(2, 100, 0),
            vec![
                text("xx"),
                InvokeCommandEvent::Output(CommandOutputEvent::Error {
                    content: "boom".into(),
                }),
            ],
        );
        assert_eq!(report.counters.dropped_error_events, 1);
        let notice = report.limit_notice().expect("limited");
        assert!(notice.severity_error);
        assert!(notice.message.contains("including 1 error events"));
        assert_conservation(&report);
    }

    /// MUTATION ORACLE: with the budget disabled, the collector retains
    /// the ENTIRE flood — proving the bounded-retention assertions in the
    /// adversarial tests are sensitive to the budget (restoring post-hoc
    /// unbounded behavior or disabling the budget must fail them).
    #[test]
    fn disabled_budget_retains_entire_flood() {
        let flood: Vec<_> = (0..1000).map(|_| text(&"z".repeat(1024))).collect();
        let report = ingest_all(InvokeOutputBudget::unlimited(), flood);
        let c = &report.counters;
        assert_eq!(c.retained_payload_bytes, 1_024_000);
        assert_eq!(c.retained_events, 1000);
        assert_eq!(c.dropped_events, 0);
        assert_eq!(c.truncated_events, 0);
        assert!(
            c.retained_payload_bytes > INVOKE_RETAINED_BYTE_BUDGET as u64,
            "an unlimited budget must visibly violate the production ceiling"
        );
        assert_conservation(&report);
    }

    /// Producer counting is send-attempt based: a closed collector still
    /// counts produced events, so produced >= consumed holds.
    #[tokio::test]
    async fn produced_counts_survive_closed_collector() {
        let (sink, collector) = invoke_event_channel(InvokeOutputBudget::default());
        sink.send(text("a")).await.expect("open");
        // Race-free: collector still holds the channel; drop it now.
        drop(collector);
        assert!(matches!(sink.send(text("bb")).await, Err(InvokeSinkClosed)));
        assert!(sink.is_closed());
    }
}
