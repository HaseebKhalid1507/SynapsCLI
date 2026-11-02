//! A oneshot whose parent stops waiting still ends its worker's
//! orchestration handle.
//!
//! The `subagent` tool runs its worker on its own thread and waits for the
//! result. A cancelled parent turn drops that wait: the dropped shutdown
//! sender cancels the worker, but nothing ended the worker's handle in the
//! delegation policy, so it stayed Running for good. It held a concurrency
//! slot, counted in the session's pending workers and kept the completion
//! accounting waiting for a reconcile that no call would make.
//!
//! [`DropReap`] is armed once the worker is running and disarmed when the
//! result arrives; the caller's normal path is unchanged. Dropped while
//! armed, it hands the policy, the handle id and the worker's thread to a
//! detached reaper. The reaper waits for the worker thread to end, at most
//! [`REAP_BOUND`], and then finishes the handle as Failed ("parent turn
//! cancelled"; the policy has no cancelled terminal). A worker that did not
//! stop in time is finished anyway, with one WARN line, so it cannot hold
//! the slot forever. The dropped call publishes no Usage, no SubagentDone
//! and writes no subagent log: its caller is gone.
//!
//! The reaper holds only the policy, the id, the join handle and the run's
//! spend lease (no session, no runtime) and is an ordinary thread: process
//! exit ends it.
//!
//! Child spend: whenever the reaper ends a handle while the worker may
//! still be running (at the bound, or with no reaper thread to wait), it
//! first seals the run's lease. The parent may then stop counting on the
//! worker; a report the worker still makes charges nothing in the session
//! and is logged as late spend. A worker that stopped has already ended its
//! run, and so has a worker thread that ends its own handle (a handed end,
//! below): that is its last act.
//!
//! A oneshot with a plan (slice 8c, dark) also publishes its run's final
//! record, which may outlast its caller's wait (§5). Its run reports how
//! that stands ([`TerminalPhase`]). While the record may still land the
//! handle stays pending, and the one lifecycle that knows when the write
//! has ended, the worker thread itself, ends it: the thread drives the
//! write after sending its result, and its last act ([`WorkerEnd::exit`])
//! ends the handle when it was handed the end. A caller that got its
//! result while the write goes on hands it the end ([`DropReap::hand_over`]);
//! so does a dropped call whose run is writing its final, from the drop
//! when the write has outlasted the run's bound, or when no reaper thread
//! can be started, and from its reaper otherwise. Nothing waits on a
//! second thread for the write, so no failure to start one can end the
//! handle early. One atomic state ([`WorkerEnd`]) makes the end exactly
//! once across the caller, the reaper and the worker. A run without a
//! record write in flight keeps L's paths, including the inline end when
//! no reaper thread can be started.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use agent_core::orchestration::WorkerTerminal;

use crate::orchestration::OrchestrationRuntime;
use crate::runtime::child_spend::RunLease;
use crate::tools::subagent::launch::TerminalPhase;

/// How long a dropped oneshot's reaper waits for its worker thread to end
/// before it finishes the handle anyway.
pub(super) const REAP_BOUND: Duration = Duration::from_secs(30);

/// How often the reaper looks at the worker thread. A std thread has no
/// join with a timeout, so the reaper polls `is_finished`.
const REAP_POLL: Duration = Duration::from_millis(25);

/// The reason a dropped oneshot's handle ends with.
pub(super) const PARENT_CANCELLED: &str = "parent turn cancelled";

/// Who ends a oneshot's handle, exactly once.
///
/// `OPEN` → `GONE` when the worker thread exits without having been handed
/// the end; `OPEN` → `HANDED` when the end is handed to it; `HANDED` →
/// `ENDED` only by the worker thread's exit; `OPEN` or `GONE` → `ENDED` by
/// a caller or reaper that claims it. A handed end is never claimed from
/// outside: the worker ends it once its write has.
#[derive(Default)]
pub(super) struct WorkerEnd {
    state: AtomicU8,
    /// The call was dropped: the handle ends as Failed
    /// ([`PARENT_CANCELLED`]), not with the run's own terminal.
    dropped: AtomicBool,
}

const OPEN: u8 = 0;
const HANDED: u8 = 1;
const GONE: u8 = 2;
const ENDED: u8 = 3;

/// What handing the end to the worker came to.
#[must_use]
enum HandOver {
    /// The worker thread ends the handle when its write has ended.
    ToWorker,
    /// The worker thread had already gone (its write is over): the one who
    /// handed it over ends the handle now.
    EndNow,
    /// The handle has ended already.
    Ended,
}

impl WorkerEnd {
    fn cas(&self, from: u8, to: u8) -> Result<u8, u8> {
        self.state
            .compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire)
    }

    /// The worker thread's last act: end the handle if it was handed the
    /// end, otherwise record that it has gone so whoever ends the handle
    /// next may do so at once. `terminal` is the run's own, as its caller
    /// would have read it from the result.
    pub(super) fn exit(&self, policy: &OrchestrationRuntime, id: &str, terminal: WorkerTerminal) {
        match self.cas(OPEN, GONE) {
            Ok(_) => {}
            Err(HANDED) => {
                if self.cas(HANDED, ENDED).is_ok() {
                    let ending = if self.dropped.load(Ordering::Acquire) {
                        Ending::Dropped
                    } else {
                        Ending::Settled(terminal)
                    };
                    finish(policy, id, ending, Worker::Stopped);
                }
            }
            // Ended already: a reaper finished it at its bound.
            Err(_) => {}
        }
        #[cfg(test)]
        exited::record(id);
    }

    fn hand_over(&self) -> HandOver {
        match self.cas(OPEN, HANDED) {
            Ok(_) => HandOver::ToWorker,
            Err(GONE) => match self.cas(GONE, ENDED) {
                Ok(_) => HandOver::EndNow,
                Err(_) => HandOver::Ended,
            },
            Err(_) => HandOver::Ended,
        }
    }

    /// Claim the end for a caller or a reaper. Never a handed end: the
    /// worker ends that one.
    fn claim(&self) -> bool {
        self.cas(OPEN, ENDED).is_ok() || self.cas(GONE, ENDED).is_ok()
    }

    fn worker_gone(&self) -> bool {
        self.state.load(Ordering::Acquire) == GONE
    }
}

/// Ends a running oneshot's handle if its caller drops the wait.
pub(super) struct DropReap {
    armed: Option<Armed>,
}

struct Armed {
    policy: Arc<OrchestrationRuntime>,
    id: String,
    thread: JoinHandle<()>,
    bound: Duration,
    /// The run's spend lease, sealed if the worker may outlive its handle.
    seal: Option<RunLease>,
    /// How the run's terminal publication stands (a run with a plan).
    terminal: Option<Arc<TerminalPhase>>,
    end: Arc<WorkerEnd>,
}

impl DropReap {
    /// Arm for worker `id`, running on `thread`, whose last act is
    /// `end.exit(..)`, its spend under `seal`.
    pub(super) fn arm(
        policy: Arc<OrchestrationRuntime>,
        id: String,
        thread: JoinHandle<()>,
        end: Arc<WorkerEnd>,
        seal: Option<RunLease>,
    ) -> Self {
        Self::arm_within(policy, id, thread, end, REAP_BOUND, seal)
    }

    fn arm_within(
        policy: Arc<OrchestrationRuntime>,
        id: String,
        thread: JoinHandle<()>,
        end: Arc<WorkerEnd>,
        bound: Duration,
        seal: Option<RunLease>,
    ) -> Self {
        Self {
            armed: Some(Armed {
                policy,
                id,
                thread,
                bound,
                seal,
                terminal: None,
                end,
            }),
        }
    }

    /// The worker's run reports how its terminal publication stands in
    /// `phase`: a dropped call's handle does not end while the run's final
    /// record may still be written.
    pub(super) fn with_terminal(mut self, phase: Arc<TerminalPhase>) -> Self {
        if let Some(armed) = self.armed.as_mut() {
            armed.terminal = Some(phase);
        }
        self
    }

    /// The caller has its result, but the worker thread is still writing
    /// its run's final record (the write outlasted the caller's wait, §5).
    /// The worker thread ends the handle as `terminal` once the write has
    /// ended, however long it takes; until then it stays pending, as the
    /// record may still land. The caller does not finish the handle itself
    /// unless the thread has already gone. Nothing else is started.
    pub(super) fn hand_over(mut self, terminal: WorkerTerminal) {
        let Armed {
            policy, id, end, ..
        } = self
            .armed
            .take()
            .expect("a oneshot's reap guard is handed over once");
        if let HandOver::EndNow = end.hand_over() {
            finish(&policy, &id, Ending::Settled(terminal), Worker::Stopped);
        }
    }

    /// The caller has its result and its run wrote nothing past it: hand
    /// the worker thread back. Nothing is reaped; the caller joins the
    /// thread and finishes the handle as before
    /// ([`Disarmed::join_and_claim`]).
    pub(super) fn disarm(mut self) -> Disarmed {
        let armed = self
            .armed
            .take()
            .expect("a oneshot's reap guard is disarmed once");
        Disarmed {
            thread: armed.thread,
            end: armed.end,
        }
    }
}

/// A disarmed guard: the caller joins the worker thread and ends the handle.
pub(super) struct Disarmed {
    thread: JoinHandle<()>,
    end: Arc<WorkerEnd>,
}

impl Disarmed {
    /// Join the worker thread; then whether the caller ends the handle
    /// (it does, once: nothing else was handed the end).
    pub(super) fn join_and_claim(self) -> bool {
        let Self { thread, end } = self;
        let _ = thread.join();
        end.claim()
    }
}

impl Drop for DropReap {
    fn drop(&mut self) {
        let Some(Armed {
            policy,
            id,
            thread,
            bound,
            seal,
            terminal,
            end,
        }) = self.armed.take()
        else {
            return;
        };
        end.dropped.store(true, Ordering::Release);
        // A worker that has already stopped is finished here: nothing to
        // wait for.
        if end.worker_gone() || thread.is_finished() {
            if thread.is_finished() {
                let _ = thread.join();
            }
            if end.claim() {
                finish(&policy, &id, Ending::Dropped, Worker::Stopped);
            }
            return;
        }
        // A run whose final record's write outlasted its bound: the worker
        // thread ends the handle once the write has.
        if terminal.as_deref().is_some_and(TerminalPhase::detached) {
            if let HandOver::EndNow = end.hand_over() {
                finish(&policy, &id, Ending::Dropped, Worker::Stopped);
            }
            return;
        }
        let fallback = (
            Arc::clone(&policy),
            id.clone(),
            Arc::clone(&end),
            terminal.clone(),
            seal.clone(),
        );
        let spawned = if spawn_fails(&id) {
            Err(std::io::Error::other("reaper spawn failure (test)"))
        } else {
            spawn_named("subagent-reap", move || {
                reap(&policy, &id, thread, bound, terminal.as_deref(), &end, seal)
            })
        };
        if spawned.is_err() {
            let (policy, id, end, terminal, seal) = fallback;
            if terminal.as_deref().is_some_and(TerminalPhase::finalizing) {
                // Its run's final record may still land and nothing can
                // wait for it here: the worker thread ends the handle once
                // the write has.
                if let HandOver::EndNow = end.hand_over() {
                    finish(&policy, &id, Ending::Dropped, Worker::Stopped);
                }
            } else if end.claim() {
                // No thread to wait on and no record being written: finish
                // now. The worker was told to stop (its shutdown sender
                // goes with the dropped call), but nothing waits for it, so
                // its run is sealed first.
                seal_run(seal.as_ref());
                finish(&policy, &id, Ending::Dropped, Worker::Unwatched);
            }
        }
    }
}

/// Wait for the worker thread to end, at most `bound` unless its run's final
/// record is being written then, and finish. A run whose write outlasts its
/// own bound hands the end to its worker thread.
fn reap(
    policy: &OrchestrationRuntime,
    id: &str,
    thread: JoinHandle<()>,
    bound: Duration,
    terminal: Option<&TerminalPhase>,
    end: &WorkerEnd,
    seal: Option<RunLease>,
) {
    let deadline = Instant::now() + bound;
    loop {
        if end.worker_gone() || thread.is_finished() {
            if end.claim() {
                finish(policy, id, Ending::Dropped, Worker::Stopped);
            }
            return;
        }
        if terminal.is_some_and(TerminalPhase::detached) {
            // The write goes on in the worker thread, which ends the handle
            // once it has.
            if let HandOver::EndNow = end.hand_over() {
                finish(policy, id, Ending::Dropped, Worker::Stopped);
            }
            return;
        }
        // A run writing its final record waits at most its own bound before
        // it detaches the write: until then the reaper waits too.
        if Instant::now() >= deadline && !terminal.is_some_and(TerminalPhase::finalizing) {
            // A worker still running past the bound keeps its thread
            // (dropping the join handle detaches it); its result has nowhere
            // to go, and its run is sealed before the handle ends.
            if end.claim() {
                seal_run(seal.as_ref());
                finish(policy, id, Ending::Dropped, Worker::Running(bound));
            }
            return;
        }
        std::thread::sleep(REAP_POLL);
    }
}

/// Start a reaper thread. Under test, [`spawn_fault`] makes it fail on the
/// calling thread, as under thread exhaustion.
fn spawn_named(
    name: &str,
    f: impl FnOnce() + Send + 'static,
) -> std::io::Result<JoinHandle<()>> {
    #[cfg(test)]
    if spawn_fault::active() {
        return Err(std::io::Error::other("injected: no thread"));
    }
    std::thread::Builder::new().name(name.into()).spawn(f)
}

/// Why the guard, not the caller, ends a handle.
#[derive(Clone, Copy)]
enum Ending {
    /// The call was dropped: Failed ([`PARENT_CANCELLED`]).
    Dropped,
    /// The caller got its result while the run's final record was still
    /// being written: the run's own terminal.
    Settled(WorkerTerminal),
}

/// Seal the run's spend publication (idempotent; nothing without a lease).
fn seal_run(seal: Option<&RunLease>) {
    if let Some(lease) = seal {
        lease.seal();
    }
}

/// Whether a test made the reaper's thread spawn fail for `id`.
#[cfg(test)]
fn spawn_fails(id: &str) -> bool {
    finished::spawn_fails(id)
}

#[cfg(not(test))]
fn spawn_fails(_id: &str) -> bool {
    false
}

/// How the worker thread stood when its handle was ended.
#[derive(Clone, Copy)]
enum Worker {
    /// It had stopped, or it is the worker thread ending its own handle.
    Stopped,
    /// It was still running at the bound.
    Running(Duration),
    /// No reaper thread could be started, so nothing waited for it.
    Unwatched,
}

/// End the handle: Failed for a dropped call, with the reason in the log
/// line; the run's own terminal for a settled one. Callers claim it first,
/// so it runs once.
fn finish(policy: &OrchestrationRuntime, id: &str, ending: Ending, worker: Worker) {
    // Recorded first, so a test that sees the handle ended sees the count.
    #[cfg(test)]
    finished::record(id);
    let terminal = match ending {
        Ending::Dropped => WorkerTerminal::Failed,
        Ending::Settled(terminal) => terminal,
    };
    let outcome = policy.finish_one_shot(id, terminal);
    match (ending, worker, outcome) {
        (Ending::Dropped, Worker::Stopped, Ok(())) => {
            tracing::info!("subagent {id} ended: {PARENT_CANCELLED}")
        }
        (Ending::Dropped, Worker::Running(bound), Ok(())) => tracing::warn!(
            "subagent {id} ended: {PARENT_CANCELLED}; worker did not stop within {}s",
            bound.as_secs()
        ),
        (Ending::Dropped, Worker::Unwatched, Ok(())) => tracing::warn!(
            "subagent {id} ended: {PARENT_CANCELLED}; no reaper thread, worker not awaited"
        ),
        (Ending::Dropped, _, Err(error)) => {
            tracing::warn!("subagent {id} ({PARENT_CANCELLED}) could not be ended: {error}")
        }
        (Ending::Settled(_), _, Ok(())) => {
            tracing::info!("subagent {id} ended once its final record was written")
        }
        (Ending::Settled(_), _, Err(error)) => {
            tracing::warn!("subagent {id} could not be ended after its final record: {error}")
        }
    }
}

/// Test-only: makes the reaper threads this thread starts fail to start.
#[cfg(test)]
pub(super) mod spawn_fault {
    use std::cell::Cell;

    thread_local! {
        static FAIL: Cell<bool> = const { Cell::new(false) };
    }

    pub(in crate::tools::subagent) fn set(fail: bool) {
        FAIL.with(|f| f.set(fail));
    }

    pub(super) fn active() -> bool {
        FAIL.with(Cell::get)
    }
}

/// Test-only record of the handles the reaper finished.
#[cfg(test)]
pub(super) mod finished {
    use std::sync::Mutex;

    static FINISHED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    pub(in crate::tools::subagent) fn record(id: &str) {
        FINISHED.lock().unwrap().push(id.to_owned());
    }

    /// How many times the reaper finished `id`.
    pub(in crate::tools::subagent) fn count(id: &str) -> usize {
        FINISHED.lock().unwrap().iter().filter(|seen| *seen == id).count()
    }

    static SPAWN_FAILS: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// Make the reaper's thread spawn fail for `id`.
    pub(in crate::tools::subagent) fn fail_spawn(id: &str) {
        SPAWN_FAILS.lock().unwrap().push(id.to_owned());
    }

    pub(super) fn spawn_fails(id: &str) -> bool {
        SPAWN_FAILS.lock().unwrap().iter().any(|seen| seen == id)
    }
}

/// Test-only record of the worker threads that took their last act.
#[cfg(test)]
pub(super) mod exited {
    use std::sync::Mutex;

    static EXITED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    pub(super) fn record(id: &str) {
        EXITED.lock().unwrap().push(id.to_owned());
    }

    /// Whether worker `id`'s thread has taken its last act.
    pub(in crate::tools::subagent) fn seen(id: &str) -> bool {
        EXITED.lock().unwrap().iter().any(|seen| seen == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A policy with worker `id` running, as a oneshot leaves it.
    fn running(id: &str) -> Arc<OrchestrationRuntime> {
        let foreground =
            agent_core::prompt::QualifiedModelId::parse("anthropic/claude-sonnet-4-6").unwrap();
        let policy = Arc::new(OrchestrationRuntime::baseline(foreground, 1, 64).unwrap());
        policy.resolve_and_authorize(id, None).unwrap();
        policy.mark_starting(id).unwrap();
        policy.mark_running(id).unwrap();
        assert!(policy.is_unreconciled(id));
        policy
    }

    /// A worker thread that runs until `release` is sent (or dropped), then
    /// takes its last act with the run's `terminal`, as a oneshot's does.
    fn held_worker(
        policy: &Arc<OrchestrationRuntime>,
        id: &str,
        terminal: WorkerTerminal,
    ) -> (std::sync::mpsc::Sender<()>, JoinHandle<()>, Arc<WorkerEnd>) {
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let end = Arc::new(WorkerEnd::default());
        let (policy, id, worker_end) = (Arc::clone(policy), id.to_owned(), Arc::clone(&end));
        let thread = std::thread::spawn(move || {
            let _ = wait.recv();
            worker_end.exit(&policy, &id, terminal);
        });
        (release, thread, end)
    }

    fn wait_until(bound: Duration, done: impl Fn() -> bool) -> bool {
        let start = Instant::now();
        while !done() {
            if start.elapsed() > bound {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    /// A run writing its final record, as its phase reports it.
    fn writing(detached: bool) -> Arc<TerminalPhase> {
        let phase = Arc::new(TerminalPhase::default());
        phase.begin_finalizing();
        if detached {
            phase.detach();
        }
        phase
    }

    /// Pending while held, then ended exactly once after release, and the
    /// slot is free (the next worker is admitted under max_concurrent 1).
    fn ends_once_after_release(
        policy: &OrchestrationRuntime,
        id: &str,
        release: std::sync::mpsc::Sender<()>,
    ) {
        std::thread::sleep(Duration::from_millis(300));
        assert!(policy.is_unreconciled(id), "pending while the worker writes its final");
        assert_eq!(finished::count(id), 0);
        release.send(()).unwrap();
        assert!(
            wait_until(Duration::from_secs(10), || !policy.is_unreconciled(id)),
            "ended once the worker stopped"
        );
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(finished::count(id), 1, "exactly once");
        policy
            .resolve_and_authorize(&format!("{id}_next"), None)
            .unwrap();
    }

    /// A worker thread that never ends does not hold its slot: the reaper
    /// finishes the handle at the bound and leaves the thread running. The
    /// reaper is a std thread, so tokio's paused clock cannot drive it; the
    /// bound is injected short instead. The worker's later exit ends
    /// nothing again.
    #[test]
    fn r1_a_worker_that_does_not_stop_is_ended_at_the_bound() {
        let id = "sa_reap_r1";
        let policy = running(id);
        let (release, thread, end) = held_worker(&policy, id, WorkerTerminal::Completed);
        let bound = Duration::from_millis(400);
        let dropped = Instant::now();
        drop(DropReap::arm_within(Arc::clone(&policy), id.to_owned(), thread, end, bound, None));

        std::thread::sleep(Duration::from_millis(150));
        assert!(policy.is_unreconciled(id), "the reaper waits for the worker until the bound");
        assert!(
            wait_until(Duration::from_secs(10), || !policy.is_unreconciled(id)),
            "a worker that never stops is ended at the bound"
        );
        assert!(dropped.elapsed() >= bound, "not before the bound");
        assert_eq!(finished::count(id), 1);
        // The slot is free: the next worker is admitted under max_concurrent 1.
        policy.resolve_and_authorize("sa_reap_r1_next", None).unwrap();
        release.send(()).unwrap();
        assert!(wait_until(Duration::from_secs(10), || exited::seen(id)));
        assert_eq!(finished::count(id), 1, "the late exit ends nothing again");
    }

    /// A worker that stops soon after the drop is ended once it has stopped.
    #[test]
    fn r2_a_worker_that_stops_is_ended_once_it_has() {
        let id = "sa_reap_r2";
        let policy = running(id);
        let (release, thread, end) = held_worker(&policy, id, WorkerTerminal::Completed);
        drop(DropReap::arm(Arc::clone(&policy), id.to_owned(), thread, end, None));
        ends_once_after_release(&policy, id, release);
    }

    /// A worker that had already taken its last act when its caller went
    /// away is ended at the drop itself.
    #[test]
    fn r3_a_worker_that_had_stopped_is_ended_at_the_drop() {
        let id = "sa_reap_r3";
        let policy = running(id);
        let (release, thread, end) = held_worker(&policy, id, WorkerTerminal::Completed);
        release.send(()).unwrap();
        assert!(wait_until(Duration::from_secs(10), || exited::seen(id)));
        drop(DropReap::arm(Arc::clone(&policy), id.to_owned(), thread, end, None));
        assert!(!policy.is_unreconciled(id), "ended at the drop, no wait");
        assert_eq!(finished::count(id), 1);
    }

    /// The caller that got its result disarms the guard: the handle is the
    /// caller's to finish, and nothing reaps it.
    #[test]
    fn r4_a_disarmed_guard_ends_nothing() {
        let id = "sa_reap_r4";
        let policy = running(id);
        let (release, thread, end) = held_worker(&policy, id, WorkerTerminal::Completed);
        let guard = DropReap::arm(Arc::clone(&policy), id.to_owned(), thread, end, None).disarm();
        release.send(()).unwrap();
        assert!(wait_until(Duration::from_secs(10), || exited::seen(id)));
        std::thread::sleep(Duration::from_millis(200));
        assert!(policy.is_unreconciled(id), "the caller finishes its own handle");
        assert_eq!(finished::count(id), 0);
        assert!(guard.join_and_claim(), "the caller's to end");
        policy.finish_one_shot(id, WorkerTerminal::Completed).unwrap();
        assert!(!policy.is_unreconciled(id));
    }

    /// A dropped call whose run is writing its final record when the bound
    /// passes is not ended then: the record may still land. It ends, once,
    /// when the worker thread does.
    #[test]
    fn r5_a_dropped_run_writing_its_final_is_ended_only_once_it_stops() {
        let id = "sa_reap_r5";
        let policy = running(id);
        let (release, thread, end) = held_worker(&policy, id, WorkerTerminal::TimedOut);
        let bound = Duration::from_millis(100);
        drop(
            DropReap::arm_within(Arc::clone(&policy), id.to_owned(), thread, end, bound, None)
                .with_terminal(writing(false)),
        );
        ends_once_after_release(&policy, id, release);
    }

    /// r5 when no reaper thread can be started: the worker thread is handed
    /// the end and the handle stays pending until its write has ended.
    #[test]
    fn r5i_a_dropped_run_writing_its_final_stays_pending_when_no_reaper_can_start() {
        let id = "sa_reap_r5i";
        let policy = running(id);
        let (release, thread, end) = held_worker(&policy, id, WorkerTerminal::TimedOut);
        spawn_fault::set(true);
        drop(
            DropReap::arm(Arc::clone(&policy), id.to_owned(), thread, end, None)
                .with_terminal(writing(false)),
        );
        spawn_fault::set(false);
        ends_once_after_release(&policy, id, release);
    }

    /// A caller that got its result while its run's final record is still
    /// being written hands the end to the worker thread: the handle stays
    /// pending until the thread ends, however long that takes, then ends
    /// once with the run's terminal. Nothing is started, so a failure to
    /// start a thread changes nothing.
    #[test]
    fn r6_a_handed_over_handle_ends_once_its_worker_has() {
        for (id, no_threads) in [("sa_reap_r6", false), ("sa_reap_r6i", true)] {
            let policy = running(id);
            let (release, thread, end) = held_worker(&policy, id, WorkerTerminal::TimedOut);
            spawn_fault::set(no_threads);
            DropReap::arm(Arc::clone(&policy), id.to_owned(), thread, end, None)
                .with_terminal(writing(true))
                .hand_over(WorkerTerminal::TimedOut);
            spawn_fault::set(false);
            ends_once_after_release(&policy, id, release);
        }
    }

    /// A call dropped once its run's write has outlasted the run's bound:
    /// the worker thread is handed the end at the drop (no reaper is
    /// started) and ends the handle as Failed once the write has.
    #[test]
    fn r7_a_call_dropped_while_its_write_goes_on_is_ended_by_its_worker() {
        for (id, no_threads) in [("sa_reap_r7", false), ("sa_reap_r7i", true)] {
            let policy = running(id);
            let (release, thread, end) = held_worker(&policy, id, WorkerTerminal::Completed);
            spawn_fault::set(no_threads);
            drop(
                DropReap::arm(Arc::clone(&policy), id.to_owned(), thread, end, None)
                    .with_terminal(writing(true)),
            );
            spawn_fault::set(false);
            ends_once_after_release(&policy, id, release);
        }
    }

    /// The worker thread that took its last act before its caller handed it
    /// the end has ended nothing; the caller ends the handle, once.
    #[test]
    fn r8_a_hand_over_after_the_worker_has_gone_ends_the_handle_once() {
        let id = "sa_reap_r8";
        let policy = running(id);
        let (release, thread, end) = held_worker(&policy, id, WorkerTerminal::TimedOut);
        release.send(()).unwrap();
        assert!(wait_until(Duration::from_secs(10), || exited::seen(id)));
        assert!(policy.is_unreconciled(id), "a worker that was not handed the end ends nothing");
        DropReap::arm(Arc::clone(&policy), id.to_owned(), thread, end, None)
            .with_terminal(writing(true))
            .hand_over(WorkerTerminal::TimedOut);
        assert!(!policy.is_unreconciled(id), "ended by the hand-over");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(finished::count(id), 1, "exactly once");
    }

    /// A plain v1 call (no record being written) dropped when no reaper
    /// thread can be started is ended at once, as L built it, and the
    /// worker's later exit ends nothing again.
    #[test]
    fn r9_a_plain_dropped_call_without_a_reaper_is_ended_at_once() {
        let id = "sa_reap_r9";
        let policy = running(id);
        let (release, thread, end) = held_worker(&policy, id, WorkerTerminal::Completed);
        spawn_fault::set(true);
        drop(DropReap::arm(Arc::clone(&policy), id.to_owned(), thread, end, None));
        spawn_fault::set(false);
        assert!(!policy.is_unreconciled(id), "ended at the drop: nothing is being written");
        assert_eq!(finished::count(id), 1);
        release.send(()).unwrap();
        assert!(wait_until(Duration::from_secs(10), || exited::seen(id)));
        assert_eq!(finished::count(id), 1, "exactly once");
    }
    include!("reap_spend_tests.rs");
}
