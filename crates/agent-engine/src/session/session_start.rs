//! `on_session_start` scheduling on one host.
//!
//! Every call goes through one gate, one at a time: each extension is a
//! single process that answers in order, so hooks fired together (a reload
//! restores every session at once, and their first turns can follow at
//! once) would queue there, and the tail would pass the handler timeout and
//! lose its injection for the whole session. A hook that a turn, or the
//! session's own creation, waits for is served before background ones.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::extensions::hooks::HookBus;
use crate::host::EngineHost;

const WAITING: u8 = 0;
const RUNNING: u8 = 1;
const WITHDRAWN: u8 = 2;

/// One session's place in line at the gate.
pub(crate) struct StartTicket {
    /// A turn (or the session's creation) waits for this hook.
    foreground: AtomicBool,
    phase: AtomicU8,
}

impl StartTicket {
    pub(crate) fn new(foreground: bool) -> Arc<Self> {
        Arc::new(Self {
            foreground: AtomicBool::new(foreground),
            phase: AtomicU8::new(WAITING),
        })
    }

    /// A turn now waits for this hook: it goes ahead of background hooks
    /// (no effect once its turn at the gate has come).
    pub(crate) fn prefer(&self) {
        self.foreground.store(true, Ordering::Release);
    }

    fn is_foreground(&self) -> bool {
        self.foreground.load(Ordering::Acquire)
    }

    /// The holder is about to call the hook; `false` if it was withdrawn
    /// first. Decides, together with [`Self::withdraw`], exactly once.
    pub(crate) fn begin(&self) -> bool {
        self.phase
            .compare_exchange(WAITING, RUNNING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Take back a hook that has not been called yet; `false` once it runs.
    pub(crate) fn withdraw(&self) -> bool {
        self.phase
            .compare_exchange(WAITING, WITHDRAWN, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

#[derive(Default)]
struct GateState {
    held: bool,
    waiters: VecDeque<Waiter>,
}

struct Waiter {
    ticket: Arc<StartTicket>,
    tx: oneshot::Sender<GatePermit>,
}

fn lock(state: &Mutex<GateState>) -> MutexGuard<'_, GateState> {
    state.lock().unwrap_or_else(|p| p.into_inner())
}

/// The host's one-at-a-time gate for `on_session_start`.
#[derive(Default)]
pub(crate) struct SessionStartGate {
    state: Arc<Mutex<GateState>>,
}

/// The turn at the gate. Dropping it hands the gate to the next waiter: the
/// first foreground one, else the first in arrival order.
pub(crate) struct GatePermit {
    state: Option<Arc<Mutex<GateState>>>,
}

impl SessionStartGate {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Wait for this ticket's turn. Cancel-safe: a dropped wait leaves the
    /// line, and a turn handed to it passes on.
    pub(crate) async fn acquire(&self, ticket: &Arc<StartTicket>) -> GatePermit {
        let rx = {
            let mut st = lock(&self.state);
            if !st.held {
                st.held = true;
                return GatePermit {
                    state: Some(Arc::clone(&self.state)),
                };
            }
            let (tx, rx) = oneshot::channel();
            st.waiters.push_back(Waiter {
                ticket: Arc::clone(ticket),
                tx,
            });
            rx
        };
        // The sender only goes away with a turn sent, so this does not fail
        // while the gate lives; if it ever did, go on without the gate.
        rx.await.unwrap_or(GatePermit { state: None })
    }
}

impl Drop for GatePermit {
    fn drop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        let mut st = lock(&state);
        loop {
            let next = st
                .waiters
                .iter()
                .position(|w| w.ticket.is_foreground())
                .or(if st.waiters.is_empty() { None } else { Some(0) });
            let Some(waiter) = next.and_then(|i| st.waiters.remove(i)) else {
                st.held = false;
                return;
            };
            let permit = GatePermit {
                state: Some(Arc::clone(&state)),
            };
            match waiter.tx.send(permit) {
                Ok(()) => return,
                // That waiter left the line: neutralise the returned turn
                // (no recursive hand-off under the lock), try the next one.
                Err(mut unclaimed) => unclaimed.state = None,
            }
        }
    }
}

/// Where a session's `on_session_start` stands.
pub(crate) enum SessionStart {
    /// It ran in this process (or the session awaited it at create).
    Done,
    /// A task waits for extension discovery and its turn at the gate, then
    /// calls the hook; it yields `true` once the hook ran.
    Queued {
        task: JoinHandle<bool>,
        ticket: Arc<StartTicket>,
    },
    /// Not in line: the session is parked. It joins the line once unparked.
    Pending,
}

impl SessionStart {
    /// Join the line. `discovery` bounds the wait for extension discovery;
    /// the hook is keyed on the session's journal id when it runs.
    pub(crate) fn queue(
        host: &Arc<EngineHost>,
        bus: Arc<HookBus>,
        journal_id: Arc<arc_swap::ArcSwap<String>>,
        discovery: Duration,
    ) -> Self {
        let ticket = StartTicket::new(false);
        let task = tokio::spawn(run_queued(
            Arc::clone(host),
            bus,
            journal_id,
            discovery,
            Arc::clone(&ticket),
        ));
        Self::Queued { task, ticket }
    }

    pub(crate) fn is_done(&self) -> bool {
        matches!(self, Self::Done)
    }

    /// The session parks: a hook not yet called leaves the line (nothing
    /// held, nothing queued) and waits for the unpark. One already running
    /// finishes. (A session that ends drops this, which stops the wait; a
    /// hook already called may still be answered by its extension, and
    /// nothing reads that answer.)
    pub(crate) fn withdraw_for_park(&mut self) {
        if let Self::Queued { task, ticket } = self {
            if ticket.withdraw() {
                task.abort();
                *self = Self::Pending;
            }
        }
    }
}

impl Drop for SessionStart {
    fn drop(&mut self) {
        if let Self::Queued { task, .. } = self {
            task.abort();
        }
    }
}

async fn run_queued(
    host: Arc<EngineHost>,
    bus: Arc<HookBus>,
    journal_id: Arc<arc_swap::ArcSwap<String>>,
    discovery: Duration,
    ticket: Arc<StartTicket>,
) -> bool {
    if tokio::time::timeout(discovery, host.extensions_ready())
        .await
        .is_err()
    {
        tracing::warn!(
            budget_secs = discovery.as_secs(),
            "extensions_ready timed out — on_session_start may miss late extensions"
        );
    }
    let permit = host.session_start_gate().acquire(&ticket).await;
    if !ticket.begin() {
        return false;
    }
    let id = (**journal_id.load()).clone();
    crate::extensions::loader::emit_session_start(&bus, &id).await;
    drop(permit);
    true
}

/// `on_session_start` for a session that waits for it at create: through
/// the same gate, ahead of background hooks.
pub(crate) async fn run_at_create(host: &EngineHost, bus: &Arc<HookBus>, session_id: &str) {
    let ticket = StartTicket::new(true);
    let permit = host.session_start_gate().acquire(&ticket).await;
    crate::extensions::loader::emit_session_start(bus, session_id).await;
    drop(permit);
}

/// How many waiters are in line at `gate` (tests).
#[cfg(test)]
fn queued(gate: &SessionStartGate) -> usize {
    lock(&gate.state).waiters.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    async fn until_queued(gate: &SessionStartGate, n: usize) {
        while queued(gate) < n {
            tokio::task::yield_now().await;
        }
    }

    /// Spawn a waiter that records its name once its turn comes.
    fn waiter(
        gate: &Arc<SessionStartGate>,
        ticket: Arc<StartTicket>,
        name: &'static str,
        order: &Arc<Mutex<Vec<&'static str>>>,
    ) -> JoinHandle<()> {
        let (gate, order) = (Arc::clone(gate), Arc::clone(order));
        tokio::spawn(async move {
            let permit = gate.acquire(&ticket).await;
            order.lock().unwrap().push(name);
            drop(permit);
        })
    }

    #[tokio::test]
    async fn a_foreground_hook_goes_before_earlier_background_ones() {
        let gate = Arc::new(SessionStartGate::new());
        let order = Arc::new(Mutex::new(Vec::new()));
        let held = gate.acquire(&StartTicket::new(false)).await;
        let b1 = waiter(&gate, StartTicket::new(false), "bg1", &order);
        until_queued(&gate, 1).await;
        let b2 = waiter(&gate, StartTicket::new(false), "bg2", &order);
        until_queued(&gate, 2).await;
        let f1 = waiter(&gate, StartTicket::new(true), "fg1", &order);
        until_queued(&gate, 3).await;
        drop(held);
        for t in [b1, b2, f1] {
            t.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), ["fg1", "bg1", "bg2"]);
    }

    #[tokio::test]
    async fn a_hook_a_turn_waits_for_moves_ahead_while_queued() {
        let gate = Arc::new(SessionStartGate::new());
        let order = Arc::new(Mutex::new(Vec::new()));
        let held = gate.acquire(&StartTicket::new(false)).await;
        let b1 = waiter(&gate, StartTicket::new(false), "bg1", &order);
        until_queued(&gate, 1).await;
        let later = StartTicket::new(false);
        let b2 = waiter(&gate, Arc::clone(&later), "bg2", &order);
        until_queued(&gate, 2).await;
        later.prefer();
        drop(held);
        for t in [b1, b2] {
            t.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), ["bg2", "bg1"]);
    }

    #[tokio::test]
    async fn a_waiter_that_left_the_line_is_skipped() {
        let gate = Arc::new(SessionStartGate::new());
        let order = Arc::new(Mutex::new(Vec::new()));
        let held = gate.acquire(&StartTicket::new(false)).await;
        let gone = waiter(&gate, StartTicket::new(true), "gone", &order);
        until_queued(&gate, 1).await;
        let next = waiter(&gate, StartTicket::new(false), "next", &order);
        until_queued(&gate, 2).await;
        gone.abort();
        let _ = gone.await;
        drop(held);
        next.await.unwrap();
        assert_eq!(*order.lock().unwrap(), ["next"]);
        // The gate is free again afterwards.
        let again = tokio::time::timeout(Duration::from_secs(1), gate.acquire(&StartTicket::new(false))).await;
        assert!(again.is_ok(), "the gate stays usable");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn never_two_holders_at_once() {
        let gate = Arc::new(SessionStartGate::new());
        let inside = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<_> = (0..32)
            .map(|i| {
                let (gate, inside, most) = (Arc::clone(&gate), Arc::clone(&inside), Arc::clone(&most));
                tokio::spawn(async move {
                    let permit = gate.acquire(&StartTicket::new(i % 3 == 0)).await;
                    let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    inside.fetch_sub(1, Ordering::SeqCst);
                    drop(permit);
                })
            })
            .collect();
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(most.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_ticket_is_either_run_or_withdrawn_once() {
        let t = StartTicket::new(false);
        assert!(t.withdraw());
        assert!(!t.begin(), "withdrawn: the hook is not called");
        let t = StartTicket::new(false);
        assert!(t.begin());
        assert!(!t.withdraw(), "running: a park cannot take it back");
    }
}
