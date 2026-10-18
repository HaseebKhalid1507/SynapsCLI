//! `on_session_start` scheduling on one host.
//!
//! Every call goes through one gate, one at a time: each extension is a
//! single process that answers in order, so hooks fired together (a reload
//! restores every session at once, and their first turns can follow at
//! once) would queue there, and the tail would pass the handler timeout and
//! lose its injection for the whole session. A hook that a turn, or the
//! session's own creation, waits for is served before background ones.
//!
//! The priority is strict, with no aging: when the gate frees, the first
//! foreground hook in line goes next, and background hooks go in arrival
//! order only while no foreground hook waits. So background hooks of idle
//! sessions can wait as long as foreground hooks keep arriving; a background
//! hook moves up as soon as its own session's turn or keep-warm ping needs
//! it (`StartTicket::prefer`). A finite reload wave drains either way.
//!
//! `startup.extensions_ready_timeout_secs` bounds the wait for extension
//! discovery only, not the wait at this gate: a session created with
//! `await_extensions = true` (`run_at_create`) then waits for the hook that
//! is running and for any older foreground hooks in line, each call bounded
//! by the handler timeouts of its extensions (5 s per handler).
//!
//! The gate serializes the extensions' side too: a call already made is not
//! abandoned when its session ends or parks, so the gate passes on only once
//! the call has returned, and the next hook never queues behind it in the
//! same extension process. (A session that ends drops the injection that
//! call brings.) The limit that remains is the handler timeout itself: a
//! handler that runs past it fails open for its session, and its extension
//! may still be working on the call when the next hook is sent.

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
    /// The session has ended: nothing reads its injection any more.
    ended: AtomicBool,
}

impl StartTicket {
    pub(crate) fn new(foreground: bool) -> Arc<Self> {
        Arc::new(Self {
            foreground: AtomicBool::new(foreground),
            phase: AtomicU8::new(WAITING),
            ended: AtomicBool::new(false),
        })
    }

    /// The session is ending (see [`SessionStart::end`]).
    fn end(&self) {
        self.ended.store(true, Ordering::SeqCst);
    }

    fn has_ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst)
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

    /// The session is ending: call this before `on_session_end` clears the
    /// session's keyed injection (`finish`); dropping does the same. A hook
    /// not yet called leaves the line. One already called is not abandoned:
    /// its task finishes the call while it holds the gate (bounded by the
    /// handler timeout, as any hook call), then drops the injection it got,
    /// since nothing reads it now. That task holds only the host, the hook
    /// bus, the journal id and its ticket: no actor handle and no lease, so
    /// it keeps nothing of the session alive, and it ends with the process.
    pub(crate) fn end(&mut self) {
        if let Self::Queued { task, ticket } = self {
            ticket.end();
            if ticket.withdraw() {
                task.abort();
            }
        }
    }

    /// The session parks: a hook not yet called leaves the line (nothing
    /// held, nothing queued) and waits for the unpark. One already called
    /// finishes, holding the gate, and counts as run.
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
    /// As [`Self::end`]: a call already made finishes in its task (the
    /// handle is dropped, which detaches it); one not yet made is withdrawn.
    fn drop(&mut self) {
        self.end();
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
    call_in_line(host.session_start_gate(), &ticket, || {
        emit_keyed(&bus, &journal_id, &ticket)
    })
    .await
}

/// The hook, keyed on the session's current journal id. If the session
/// ended meanwhile, the injection is dropped again: either the session's
/// own end clear (`on_session_end`, after [`StartTicket::end`]) comes after
/// this set, or this check sees the end, since both go through the bus's
/// injection lock.
async fn emit_keyed(bus: &Arc<HookBus>, journal_id: &arc_swap::ArcSwap<String>, ticket: &StartTicket) {
    let id = (**journal_id.load()).clone();
    #[cfg(any(test, feature = "testing"))]
    testing::note_call(&id);
    crate::extensions::loader::emit_session_start(bus, &id).await;
    if ticket.has_ended() {
        bus.clear_session_injection(&id).await;
    }
}

/// Wait for `ticket`'s turn at `gate`, then make the call while holding it.
/// `false` if the ticket was withdrawn before its turn came (not called).
async fn call_in_line<F, Fut>(gate: &SessionStartGate, ticket: &Arc<StartTicket>, call: F) -> bool
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let permit = gate.acquire(ticket).await;
    if !ticket.begin() {
        return false;
    }
    call().await;
    drop(permit);
    true
}

/// `on_session_start` for a session that waits for it at create: through
/// the same gate, ahead of background hooks.
pub(crate) async fn run_at_create(host: &EngineHost, bus: &Arc<HookBus>, session_id: &str) {
    let ticket = StartTicket::new(true);
    let permit = host.session_start_gate().acquire(&ticket).await;
    #[cfg(any(test, feature = "testing"))]
    testing::note_call(session_id);
    crate::extensions::loader::emit_session_start(bus, session_id).await;
    drop(permit);
}

/// Test-only controls (`cfg(test)` / the `testing` feature). Set only by
/// code, like `cache_keepwarm::testing::IntervalOverride`: no binary input
/// (env, config, argv) reaches them.
///
/// Both controls are scoped to one session, named by its journal id (the
/// key its hook gets): only `#[serial]` tests are serialized, and the other
/// tests of the same binary run their own sessions meanwhile, so an
/// unscoped flag or count could be taken or moved by them. The requeue slot
/// holds one session at a time, so tests that set it must be `#[serial]`
/// with each other.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::collections::BTreeMap;
    use std::sync::{Mutex, MutexGuard};

    static REQUEUE_AFTER_TURN: Mutex<Option<String>> = Mutex::new(None);
    static CALLS: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The next turn of `session` to end puts a fresh `on_session_start`
    /// back in line, as if it had not run yet. With the keep-warm period
    /// that turn starts, this reaches a ping that is due while the hook is
    /// pending, which no production path does today: a period needs an
    /// accepted request, and that request waited for the hook. One-shot.
    pub fn requeue_after_next_turn(session: &str) {
        *lock(&REQUEUE_AFTER_TURN) = Some(session.to_string());
    }

    /// Consume the request above if it names `session` (its actor, at a
    /// turn's end).
    pub(crate) fn take_requeue(session: &str) -> bool {
        let mut slot = lock(&REQUEUE_AFTER_TURN);
        if slot.as_deref() == Some(session) {
            *slot = None;
            true
        } else {
            false
        }
    }

    /// `on_session_start` calls made through the gate for `session`.
    pub fn calls(session: &str) -> u64 {
        lock(&CALLS).get(session).copied().unwrap_or(0)
    }

    pub(super) fn note_call(session: &str) {
        *lock(&CALLS).entry(session.to_string()).or_insert(0) += 1;
    }

    /// Holds the host's session-start gate while alive: every hook in line
    /// waits (not yet called, so a park can still withdraw it).
    #[must_use = "the gate is released when the hold drops"]
    pub struct GateHold(#[allow(dead_code)] super::GatePermit);

    /// Take the gate now, ahead of background hooks (waits for a running one).
    pub async fn hold_gate(host: &crate::host::EngineHost) -> GateHold {
        let ticket = super::StartTicket::new(true);
        GateHold(host.session_start_gate().acquire(&ticket).await)
    }
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

    /// A hook in line at `gate` whose extension is still working on it once
    /// called: `called` fires when the call is made, and the call returns
    /// when `release` fires (or is dropped).
    struct Blocking {
        called: oneshot::Receiver<()>,
        release: oneshot::Sender<()>,
    }

    fn blocking_start(gate: &Arc<SessionStartGate>) -> (SessionStart, Blocking) {
        let ticket = StartTicket::new(false);
        let (called_tx, called) = oneshot::channel();
        let (release, release_rx) = oneshot::channel::<()>();
        let (gate, t) = (Arc::clone(gate), Arc::clone(&ticket));
        let task = tokio::spawn(async move {
            call_in_line(&gate, &t, move || async move {
                let _ = called_tx.send(());
                let _ = release_rx.await;
            })
            .await
        });
        (SessionStart::Queued { task, ticket }, Blocking { called, release })
    }

    /// Long enough for an aborted task to be dropped and its turn at the
    /// gate to pass on (the current-thread test runtime runs it meanwhile).
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    /// A session that ends while its extension still works on the hook does
    /// not hand the gate on: the next hook would queue behind that call in
    /// the same extension process and could pass the handler timeout.
    #[tokio::test]
    async fn a_hook_already_called_keeps_the_gate_when_its_session_ends() {
        let gate = Arc::new(SessionStartGate::new());
        let order = Arc::new(Mutex::new(Vec::new()));
        let (ending, mut hook) = blocking_start(&gate);
        (&mut hook.called).await.expect("the hook is called");
        let next = waiter(&gate, StartTicket::new(false), "next", &order);
        until_queued(&gate, 1).await;

        drop(ending);
        settle().await;
        assert!(
            order.lock().unwrap().is_empty(),
            "the next hook waits for the call in flight"
        );

        let _ = hook.release.send(());
        next.await.unwrap();
        assert_eq!(*order.lock().unwrap(), vec!["next"]);
    }

    /// The same for a park: the call in flight finishes, then the gate
    /// passes on, and the session's hook counts as run.
    #[tokio::test]
    async fn a_hook_already_called_keeps_the_gate_when_its_session_parks() {
        let gate = Arc::new(SessionStartGate::new());
        let order = Arc::new(Mutex::new(Vec::new()));
        let (mut parking, mut hook) = blocking_start(&gate);
        (&mut hook.called).await.expect("the hook is called");
        let next = waiter(&gate, StartTicket::new(false), "next", &order);
        until_queued(&gate, 1).await;

        parking.withdraw_for_park();
        settle().await;
        assert!(
            matches!(parking, SessionStart::Queued { .. }),
            "a running hook is not withdrawn"
        );
        assert!(order.lock().unwrap().is_empty(), "the next hook waits");

        let _ = hook.release.send(());
        next.await.unwrap();
        assert_eq!(*order.lock().unwrap(), vec!["next"]);
        let SessionStart::Queued { task, .. } = &mut parking else {
            unreachable!()
        };
        assert!(task.await.unwrap(), "the hook ran");
    }

    /// A hook not called yet leaves the line when its session ends: it is
    /// never called, and the gate passes on.
    #[tokio::test]
    async fn a_hook_not_yet_called_leaves_the_line_when_its_session_ends() {
        let gate = Arc::new(SessionStartGate::new());
        let held = gate.acquire(&StartTicket::new(false)).await;
        let (ending, hook) = blocking_start(&gate);
        until_queued(&gate, 1).await;

        drop(ending);
        drop(held);
        let free = tokio::time::timeout(
            Duration::from_secs(2),
            gate.acquire(&StartTicket::new(false)),
        )
        .await;
        assert!(free.is_ok(), "the gate is free again");
        assert!(hook.called.await.is_err(), "the hook was never called");
    }

    /// An `on_session_start` handler that injects `INJECTED`, answering only
    /// once released: an extension still working on the call.
    struct GatedHook {
        called: Mutex<Option<oneshot::Sender<()>>>,
        release: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
    }

    #[async_trait::async_trait]
    impl crate::extensions::runtime::ExtensionHandler for GatedHook {
        fn id(&self) -> &str {
            "gated"
        }

        async fn handle(
            &self,
            _event: &crate::extensions::hooks::events::HookEvent,
        ) -> crate::extensions::hooks::events::HookResult {
            if let Some(tx) = self.called.lock().unwrap().take() {
                let _ = tx.send(());
            }
            if let Some(rx) = self.release.lock().await.take() {
                let _ = rx.await;
            }
            crate::extensions::hooks::events::HookResult::Inject {
                content: INJECTED.into(),
            }
        }

        async fn shutdown(&self) {}
    }

    const INJECTED: &str = "session-start context";

    /// A session `id` whose hook is in line at `gate` on the production
    /// path (`emit_keyed`), with a handler that waits for its release.
    async fn keyed_start(
        gate: &Arc<SessionStartGate>,
        id: &str,
    ) -> (SessionStart, Arc<HookBus>, Blocking) {
        let bus = Arc::new(HookBus::new());
        let (called_tx, called) = oneshot::channel();
        let (release, release_rx) = oneshot::channel::<()>();
        let mut permissions = crate::extensions::permissions::PermissionSet::new();
        permissions.grant(crate::extensions::permissions::Permission::SessionLifecycle);
        bus.subscribe(
            crate::extensions::hooks::events::HookKind::OnSessionStart,
            Arc::new(GatedHook {
                called: Mutex::new(Some(called_tx)),
                release: tokio::sync::Mutex::new(Some(release_rx)),
            }),
            None,
            None,
            permissions,
        )
        .await
        .expect("subscribe");
        let ticket = StartTicket::new(false);
        let journal_id = Arc::new(arc_swap::ArcSwap::from_pointee(id.to_string()));
        let (gate, b, t) = (Arc::clone(gate), Arc::clone(&bus), Arc::clone(&ticket));
        let task = tokio::spawn(async move {
            call_in_line(&gate, &t, || emit_keyed(&b, &journal_id, &t)).await
        });
        (SessionStart::Queued { task, ticket }, bus, Blocking { called, release })
    }

    /// Wait until the gate is free: the call in flight has fully returned.
    async fn gate_free(gate: &SessionStartGate) {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            gate.acquire(&StartTicket::new(false)),
        )
        .await
        .expect("the gate frees");
    }

    /// The session's end, as `finish` does it: the ticket first, then
    /// `on_session_end`, which clears the keyed injection.
    async fn end_session(start: &mut SessionStart, bus: &Arc<HookBus>, id: &str) {
        start.end();
        crate::extensions::loader::emit_session_end(bus, id, None, Duration::from_secs(1)).await;
    }

    /// Control: a session that does not end keeps what its hook injected.
    #[tokio::test]
    async fn a_live_session_keeps_its_injection() {
        let gate = Arc::new(SessionStartGate::new());
        let (start, bus, mut hook) = keyed_start(&gate, "live").await;
        (&mut hook.called).await.expect("the hook is called");
        let _ = hook.release.send(());
        gate_free(&gate).await;
        assert_eq!(bus.session_injection_for("live").await.as_deref(), Some(INJECTED));
        drop(start);
    }

    /// The hook answers after the session ended (its end clear already ran):
    /// the hook's own task drops the injection.
    #[tokio::test]
    async fn an_injection_that_lands_after_the_end_is_dropped() {
        let gate = Arc::new(SessionStartGate::new());
        let (mut start, bus, mut hook) = keyed_start(&gate, "late").await;
        (&mut hook.called).await.expect("the hook is called");
        end_session(&mut start, &bus, "late").await;
        drop(start);
        let _ = hook.release.send(());
        gate_free(&gate).await;
        assert_eq!(bus.session_injection_for("late").await, None);
    }

    /// The hook answers between the ticket's end and the end clear: the
    /// task drops it at once, and the end clear finds nothing.
    #[tokio::test]
    async fn an_injection_that_lands_during_the_end_is_dropped() {
        let gate = Arc::new(SessionStartGate::new());
        let (mut start, bus, mut hook) = keyed_start(&gate, "during").await;
        (&mut hook.called).await.expect("the hook is called");
        start.end();
        let _ = hook.release.send(());
        gate_free(&gate).await;
        assert_eq!(bus.session_injection_for("during").await, None);
        crate::extensions::loader::emit_session_end(&bus, "during", None, Duration::from_secs(1)).await;
        assert_eq!(bus.session_injection_for("during").await, None);
    }

    /// The hook answered before the session ended (its actor had not taken
    /// the result yet): the session's end clear drops it.
    #[tokio::test]
    async fn an_injection_that_landed_before_the_end_is_dropped() {
        let gate = Arc::new(SessionStartGate::new());
        let (mut start, bus, mut hook) = keyed_start(&gate, "early").await;
        (&mut hook.called).await.expect("the hook is called");
        let _ = hook.release.send(());
        gate_free(&gate).await;
        assert_eq!(bus.session_injection_for("early").await.as_deref(), Some(INJECTED));
        end_session(&mut start, &bus, "early").await;
        assert_eq!(bus.session_injection_for("early").await, None);
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
