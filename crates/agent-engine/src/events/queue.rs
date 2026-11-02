use super::types::{Event, Severity};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// How many arrival headers the queue keeps for waiters (see [`Arrival`]).
pub const ARRIVAL_RING: usize = 256;
/// The longest header field an arrival keeps, in bytes.
const ARRIVAL_FIELD: usize = 128;

/// What a waiter may see of an event the queue accepted: the routing
/// header the event tag shows the model (id, type, source, severity), and
/// nothing else. Never its text or data: event bodies stay in one place,
/// the queue. An arrival promises nothing about the payload's delivery:
/// a priority push can still evict the event before the drain.
#[derive(Debug, Clone, PartialEq)]
pub struct Arrival {
    /// 1-based, monotonic per queue, in accepted-enqueue order.
    pub seq: u64,
    pub id: String,
    pub content_type: String,
    /// `source.source_type`: what the model sees as `source="…"`.
    pub source: String,
    pub severity: Option<Severity>,
    /// The drain may put this event in the conversation (the same
    /// persistence gate the drain applies). A display-only event never
    /// reaches the model, so a waiter must neither match nor show it.
    pub reaches_model: bool,
}

/// A header field as the event tag renders it: at most
/// [`ARRIVAL_FIELD`] bytes, cut at a char boundary, `"` as `'`.
fn header_field(field: &str) -> String {
    let mut end = field.len().min(ARRIVAL_FIELD);
    while !field.is_char_boundary(end) {
        end -= 1;
    }
    field[..end].replace('"', "'")
}

impl Arrival {
    fn of(event: &Event) -> Self {
        Self {
            seq: 0,
            id: header_field(&event.id),
            content_type: header_field(&event.content.content_type),
            source: header_field(&event.source.source_type),
            severity: event.content.severity.clone(),
            reaches_model: agent_core::disclosure::may_persist(
                event.content.disclosure.unwrap_or_default(),
            ),
        }
    }
}

/// Arrivals newer than a waiter's position.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ArrivalsSince {
    pub arrivals: Vec<Arrival>,
    /// Some arrivals after the position already left the ring: arrival
    /// information is unavailable for them (not "a match happened").
    pub overflow: bool,
    /// The newest seq seen; pass it back as the next position.
    pub last_seq: u64,
}

/// Why a session is quiescing: a running `wait` returns
/// `interrupted`, and a turn starts no new provider round after a tool
/// round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quiesce {
    /// The daemon is reloading (set by the reload's prepare step).
    Reload,
    /// The host checkpoints the session.
    Checkpoint,
}

impl Quiesce {
    pub fn as_str(self) -> &'static str {
        match self {
            Quiesce::Reload => "reload",
            Quiesce::Checkpoint => "checkpoint",
        }
    }
}

/// One quiesce flag per session queue. Writable from outside the actor:
/// the session handle holds a clone, so the daemon sets it without a
/// command and without unparking anyone.
pub type QuiesceSlot = Arc<tokio::sync::watch::Sender<Option<Quiesce>>>;

struct ArrivalLog {
    ring: VecDeque<Arrival>,
    seq: u64,
}

pub struct EventQueue {
    inner: Mutex<VecDeque<Event>>,
    capacity: usize,
    notify: tokio::sync::Notify,
    /// Every accepted event, as a header, for waiters (`wait` tool). The
    /// drainer pops `inner`, so a waiter cannot watch the queue itself.
    /// Lock order: `inner`, then `arrivals`.
    arrivals: Mutex<ArrivalLog>,
    arrived: tokio::sync::watch::Sender<u64>,
    /// Events a priority push evicted from a full queue.
    evicted: AtomicU64,
    quiesce: QuiesceSlot,
    /// Why the running turn's stream last stopped at a round boundary for
    /// a quiesce (`runtime/stream.rs`), until the session's actor takes it
    /// ([`Self::take_quiesce_stop`]).
    quiesce_stop: Mutex<Option<Quiesce>>,
    /// The session this queue delivers to, for the intake logs (the event
    /// socket, the inbox); set when they start. Empty until then.
    session: Mutex<String>,
}

impl EventQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
            notify: tokio::sync::Notify::new(),
            arrivals: Mutex::new(ArrivalLog {
                ring: VecDeque::with_capacity(ARRIVAL_RING),
                seq: 0,
            }),
            arrived: tokio::sync::watch::channel(0).0,
            evicted: AtomicU64::new(0),
            quiesce: Arc::new(tokio::sync::watch::channel(None).0),
            quiesce_stop: Mutex::new(None),
            session: Mutex::new(String::new()),
        }
    }

    /// Name the session this queue delivers to (for the intake logs).
    pub fn set_session_label(&self, session_id: &str) {
        *self.session.lock().unwrap_or_else(|p| p.into_inner()) = session_id.to_string();
    }

    /// The session this queue delivers to (empty when never named).
    pub fn session_label(&self) -> String {
        self.session.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Stamp an accepted event's arrival. Called with `inner` held, so seq
    /// order is enqueue order and a refused push stamps nothing.
    fn stamp(&self, mut arrival: Arrival) -> u64 {
        let mut log = self.arrivals.lock().unwrap_or_else(|p| p.into_inner());
        log.seq += 1;
        arrival.seq = log.seq;
        if log.ring.len() == ARRIVAL_RING {
            log.ring.pop_front();
        }
        log.ring.push_back(arrival);
        log.seq
    }

    /// The seq of the newest arrival (0 before any).
    pub fn arrival_seq(&self) -> u64 {
        self.arrivals.lock().unwrap_or_else(|p| p.into_inner()).seq
    }

    /// Subscribe, then read the position: an arrival after the returned
    /// seq always marks the receiver changed, so a waiter that checks
    /// [`Self::arrivals_since`] before each wait loses no notification.
    pub fn watch_arrivals(&self) -> (tokio::sync::watch::Receiver<u64>, u64) {
        let rx = self.arrived.subscribe();
        (rx, self.arrival_seq())
    }

    /// Arrivals with a seq greater than `after`, oldest first.
    pub fn arrivals_since(&self, after: u64) -> ArrivalsSince {
        let log = self.arrivals.lock().unwrap_or_else(|p| p.into_inner());
        let oldest = log.ring.front().map_or(log.seq + 1, |a| a.seq);
        ArrivalsSince {
            arrivals: log.ring.iter().filter(|a| a.seq > after).cloned().collect(),
            overflow: after + 1 < oldest,
            last_seq: log.seq.max(after),
        }
    }

    /// The session's quiesce flag, for its handle.
    pub fn quiesce_slot(&self) -> QuiesceSlot {
        Arc::clone(&self.quiesce)
    }

    /// Set (or clear, with `None`) the quiesce flag.
    pub fn set_quiesce(&self, reason: Option<Quiesce>) {
        self.quiesce.send_replace(reason);
    }

    /// Why the session is quiescing, if it is.
    pub fn quiesced(&self) -> Option<Quiesce> {
        *self.quiesce.borrow()
    }

    /// The running turn's stream stopped at a round boundary because the
    /// session was quiescing for `reason`. The actor learns it from the
    /// turn's end ([`Self::take_quiesce_stop`]), even when the quiesce has
    /// lifted by then.
    pub fn note_quiesce_stop(&self, reason: Quiesce) {
        *self.quiesce_stop.lock().unwrap_or_else(|p| p.into_inner()) = Some(reason);
    }

    /// Why the turn's stream stopped for a quiesce, if it did (and forget
    /// it).
    pub fn take_quiesce_stop(&self) -> Option<Quiesce> {
        self.quiesce_stop.lock().unwrap_or_else(|p| p.into_inner()).take()
    }

    /// Test seam (`testing` builds only; never in a release build): with
    /// `SYNAPS_TEST_HOLD_DONE_AFTER_QUIESCE_STOP` set, a turn that ends while
    /// its session is quiescing sends its `Done` only once the quiesce has
    /// lifted (or after 30 s), so a test can deliver a command between a
    /// quiesce stop and the turn's end.
    #[cfg(feature = "testing")]
    pub async fn hold_done_after_quiesce_stop_for_test(&self) {
        if std::env::var_os("SYNAPS_TEST_HOLD_DONE_AFTER_QUIESCE_STOP").is_none()
            || self.quiesced().is_none()
        {
            return;
        }
        let mut rx = self.quiesce.subscribe();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            rx.wait_for(|q| q.is_none()),
        )
        .await;
    }

    /// Subscribe, then read: a change after the returned value always
    /// marks the receiver changed (the same rule as [`Self::watch_arrivals`]).
    pub fn watch_quiesce(&self) -> (tokio::sync::watch::Receiver<Option<Quiesce>>, Option<Quiesce>) {
        let mut rx = self.quiesce.subscribe();
        let now = *rx.borrow_and_update();
        (rx, now)
    }

    /// Events a priority push has evicted so far.
    pub fn evictions(&self) -> u64 {
        self.evicted.load(Ordering::Relaxed)
    }

    /// Push an event. Critical goes to front, High goes after existing Criticals,
    /// everything else goes to the back. Returns Err if queue is full.
    pub fn push(&self, event: Event) -> Result<(), String> {
        let mut q = self.inner.lock().unwrap();
        if q.len() >= self.capacity {
            return Err(format!("event queue full (capacity {})", self.capacity));
        }
        let arrival = Arrival::of(&event);
        let sev = event.content.severity.clone();
        match sev {
            Some(Severity::Critical) => q.push_front(event),
            Some(Severity::High) => {
                // insert after trailing Critical events at the front
                let mut idx = 0;
                while idx < q.len() && matches!(q[idx].content.severity, Some(Severity::Critical)) {
                    idx += 1;
                }
                q.insert(idx, event);
            }
            _ => q.push_back(event),
        }
        let seq = self.stamp(arrival);
        drop(q);
        self.arrived.send_replace(seq);
        self.notify.notify_one();
        Ok(())
    }

    /// Force push to front regardless of severity. Evicts the oldest (back) if full.
    pub fn push_priority(&self, event: Event) {
        let mut q = self.inner.lock().unwrap();
        if q.len() >= self.capacity {
            if let Some(evicted) = q.back() {
                tracing::warn!("event queue full — evicting event id={}", evicted.id);
            }
            if q.pop_back().is_some() {
                self.evicted.fetch_add(1, Ordering::Relaxed);
            }
        }
        let arrival = Arrival::of(&event);
        q.push_front(event);
        let seq = self.stamp(arrival);
        drop(q);
        self.arrived.send_replace(seq);
        self.notify.notify_one();
    }

    pub fn pop(&self) -> Option<Event> {
        self.inner.lock().unwrap().pop_front()
    }

    pub fn peek(&self) -> Option<Event> {
        self.inner.lock().unwrap().front().cloned()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    /// Wait until an event is pushed. Use in tokio::select! for instant wake.
    pub fn notified(&self) -> impl std::future::Future<Output = ()> + '_ {
        self.notify.notified()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }

    /// Whether a `push` would be refused right now.
    pub fn is_full(&self) -> bool {
        self.inner.lock().unwrap().len() >= self.capacity
    }

    /// Clone of every queued event, front first; the queue is untouched.
    pub fn snapshot(&self) -> Vec<Event> {
        self.inner.lock().unwrap().iter().cloned().collect()
    }

    pub fn drain(&self) -> Vec<Event> {
        let mut q = self.inner.lock().unwrap();
        q.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(text: &str, sev: Option<Severity>) -> Event {
        Event::simple("test", text, sev)
    }

    #[test]
    fn push_pop_fifo_for_medium() {
        let q = EventQueue::new(10);
        q.push(ev("a", Some(Severity::Medium))).unwrap();
        q.push(ev("b", Some(Severity::Low))).unwrap();
        q.push(ev("c", None)).unwrap();
        assert_eq!(q.pop().unwrap().content.text, "a");
        assert_eq!(q.pop().unwrap().content.text, "b");
        assert_eq!(q.pop().unwrap().content.text, "c");
        assert!(q.is_empty());
    }

    #[test]
    fn critical_jumps_to_front() {
        let q = EventQueue::new(10);
        q.push(ev("a", Some(Severity::Medium))).unwrap();
        q.push(ev("b", Some(Severity::Medium))).unwrap();
        q.push(ev("CRIT", Some(Severity::Critical))).unwrap();
        assert_eq!(q.pop().unwrap().content.text, "CRIT");
        assert_eq!(q.pop().unwrap().content.text, "a");
    }

    #[test]
    fn high_sits_after_critical() {
        let q = EventQueue::new(10);
        q.push(ev("med", Some(Severity::Medium))).unwrap();
        q.push(ev("c1", Some(Severity::Critical))).unwrap();
        q.push(ev("c2", Some(Severity::Critical))).unwrap();
        q.push(ev("high", Some(Severity::High))).unwrap();
        // Order should be: c2, c1, high, med
        assert_eq!(q.pop().unwrap().content.text, "c2");
        assert_eq!(q.pop().unwrap().content.text, "c1");
        assert_eq!(q.pop().unwrap().content.text, "high");
        assert_eq!(q.pop().unwrap().content.text, "med");
    }

    #[test]
    fn capacity_limit() {
        let q = EventQueue::new(2);
        q.push(ev("a", None)).unwrap();
        q.push(ev("b", None)).unwrap();
        assert!(q.push(ev("c", None)).is_err());
    }

    #[test]
    fn drain_takes_all() {
        let q = EventQueue::new(10);
        q.push(ev("a", None)).unwrap();
        q.push(ev("b", None)).unwrap();
        let all = q.drain();
        assert_eq!(all.len(), 2);
        assert!(q.is_empty());
    }

    #[test]
    fn peek_does_not_remove() {
        let q = EventQueue::new(10);
        q.push(ev("a", None)).unwrap();
        assert_eq!(q.peek().unwrap().content.text, "a");
        assert_eq!(q.len(), 1);
    }

    // --- arrivals (wait tool) ---

    fn never_persist(text: &str) -> Event {
        let mut e = ev(text, Some(Severity::High));
        e.content.disclosure = Some(agent_core::disclosure::DisclosureClass::NeverPersist);
        e
    }

    #[test]
    fn arrivals_follow_enqueue_order_and_keep_headers_only() {
        let q = EventQueue::new(10);
        let mut first = ev("secret body", Some(Severity::Low));
        first.content.data = Some(serde_json::json!({"k": "v"}));
        q.push(first.clone()).unwrap();
        q.push(ev("b", Some(Severity::Critical))).unwrap();
        let since = q.arrivals_since(0);
        assert!(!since.overflow);
        assert_eq!(since.last_seq, 2);
        let seqs: Vec<u64> = since.arrivals.iter().map(|a| a.seq).collect();
        assert_eq!(seqs, vec![1, 2], "enqueue order, not queue order");
        let a = &since.arrivals[0];
        assert_eq!(a.id, first.id);
        assert_eq!(a.content_type, first.content.content_type);
        assert_eq!(a.source, "test");
        assert_eq!(a.severity, Some(Severity::Low));
        assert!(a.reaches_model);
        let shown = format!("{a:?}");
        assert!(
            !shown.contains("secret body") && !shown.contains("\"k\""),
            "{shown}"
        );
        assert_eq!(q.arrivals_since(1).arrivals.len(), 1);
        assert_eq!(q.arrivals_since(2).arrivals.len(), 0);
    }

    #[test]
    fn a_refused_push_stamps_no_arrival() {
        let q = EventQueue::new(1);
        q.push(ev("a", None)).unwrap();
        assert!(q.push(ev("b", None)).is_err());
        assert_eq!(q.arrival_seq(), 1);
        assert_eq!(q.arrivals_since(0).arrivals.len(), 1);
    }

    #[test]
    fn a_priority_push_into_a_full_queue_counts_its_eviction() {
        let q = EventQueue::new(1);
        q.push(ev("a", None)).unwrap();
        assert_eq!(q.evictions(), 0);
        q.push_priority(ev("b", None));
        assert_eq!(q.evictions(), 1);
        assert_eq!(q.len(), 1);
        assert_eq!(q.arrival_seq(), 2, "the evicted event's arrival stays");
    }

    #[test]
    fn a_position_older_than_the_ring_reports_overflow() {
        let q = EventQueue::new(ARRIVAL_RING + 64);
        for i in 0..ARRIVAL_RING + 10 {
            q.push(ev(&i.to_string(), None)).unwrap();
        }
        let since = q.arrivals_since(0);
        assert!(since.overflow);
        assert_eq!(since.arrivals.len(), ARRIVAL_RING);
        assert_eq!(since.last_seq, (ARRIVAL_RING + 10) as u64);
        assert!(!q.arrivals_since(10).overflow, "seq 11 is still held");
        assert!(q.arrivals_since(9).overflow);
    }

    #[test]
    fn header_fields_are_bounded_and_quoted_like_the_event_tag() {
        let q = EventQueue::new(4);
        let mut e = ev("x", None);
        e.source.source_type = format!("{}\"é", "s".repeat(126));
        e.content.content_type = "a\"b".into();
        q.push(e).unwrap();
        let a = &q.arrivals_since(0).arrivals[0];
        assert_eq!(a.content_type, "a'b");
        assert!(a.source.len() <= 128, "{}", a.source.len());
        assert!(
            a.source.ends_with('\''),
            "cut at a char boundary: {}",
            a.source
        );
    }

    #[test]
    fn a_never_persist_event_does_not_reach_the_model() {
        let q = EventQueue::new(4);
        q.push(never_persist("x")).unwrap();
        assert!(!q.arrivals_since(0).arrivals[0].reaches_model);
    }

    #[tokio::test]
    async fn quiesce_is_shared_with_the_slot_and_wakes_a_watcher() {
        let q = EventQueue::new(4);
        assert_eq!(q.quiesced(), None);
        let (mut rx, now) = q.watch_quiesce();
        assert_eq!(now, None);
        let slot = q.quiesce_slot();
        slot.send_replace(Some(Quiesce::Reload));
        assert!(rx.has_changed().unwrap(), "a set after watching is seen");
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), Some(Quiesce::Reload));
        assert_eq!(q.quiesced(), Some(Quiesce::Reload), "one flag, two writers");
        q.set_quiesce(None);
        assert_eq!(*slot.borrow(), None);
        assert_eq!(Quiesce::Checkpoint.as_str(), "checkpoint");
    }

    #[test]
    fn a_quiesce_stop_outlives_the_lift_until_taken_once() {
        let q = EventQueue::new(4);
        assert_eq!(q.take_quiesce_stop(), None);
        q.set_quiesce(Some(Quiesce::Reload));
        q.note_quiesce_stop(Quiesce::Reload);
        q.set_quiesce(None);
        assert_eq!(q.take_quiesce_stop(), Some(Quiesce::Reload), "the lift keeps it");
        assert_eq!(q.take_quiesce_stop(), None, "taken once");
    }

    #[tokio::test]
    async fn an_arrival_after_watching_always_marks_the_receiver_changed() {
        let q = EventQueue::new(4);
        q.push(ev("before", None)).unwrap();
        let (mut rx, at) = q.watch_arrivals();
        assert_eq!(at, 1);
        assert!(!rx.has_changed().unwrap(), "nothing new yet");
        q.push(ev("after", None)).unwrap();
        assert!(rx.has_changed().unwrap());
        rx.changed().await.unwrap();
        assert_eq!(q.arrivals_since(at).arrivals.len(), 1);
    }
}
