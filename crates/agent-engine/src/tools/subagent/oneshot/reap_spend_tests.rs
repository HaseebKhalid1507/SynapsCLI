// Child spend at the reaper (P1f), included in reap.rs's tests: a reaper
// that ends a handle while the worker may still run seals the worker's run
// first, so a later report charges nothing in the session. A worker that
// stopped has ended its own run, and a disarmed guard seals nothing.

use crate::runtime::child_spend::{
    BookOutcome, NotBooked, SinkMessage, SpendKind, SpendReceiver, SpendSink, SpendTap,
    SpendTokens,
};

/// A live run's lease and the session's end of its channel.
fn run(id: &str) -> (RunLease, SpendReceiver) {
    let (sink, rx) = SpendSink::new();
    (sink.lease(id, "reaped").unwrap(), rx)
}

/// What the session received so far, by kind.
fn received(rx: &mut SpendReceiver) -> Vec<&'static str> {
    let mut out = Vec::new();
    while let Ok(message) = rx.try_recv() {
        out.push(match message {
            SinkMessage::Spend { .. } => "spend",
            SinkMessage::RunSealed { .. } => "sealed",
            SinkMessage::RunEnded { .. } => "ended",
        });
    }
    out
}

/// One report on `lease`'s run, as its worker's tap books it.
fn book(lease: &RunLease) -> BookOutcome {
    SpendTap::new(lease.clone()).book(
        SpendKind::Turn,
        "anthropic/claude-sonnet-4-6",
        SpendTokens {
            input: 1,
            output: 1,
            ..SpendTokens::default()
        },
    )
}

/// A worker still running at the bound: its run is sealed before its
/// handle ends, and what it reports after that is not booked.
#[test]
fn s1_a_worker_ended_at_the_bound_has_its_run_sealed_first() {
    let id = "sa_reap_s1";
    let policy = running(id);
    let (worker, mut rx) = run(id);
    let (release, thread, end) = held_worker(&policy, id, WorkerTerminal::Completed);
    assert_eq!(book(&worker), BookOutcome::Booked { seq: 0 });
    let bound = Duration::from_millis(300);
    drop(DropReap::arm_within(
        Arc::clone(&policy),
        id.to_owned(),
        thread,
        end,
        bound,
        Some(worker.clone()),
    ));
    std::thread::sleep(Duration::from_millis(100));
    assert!(worker.is_live(), "the run publishes until the bound");
    assert!(wait_until(Duration::from_secs(10), || !policy.is_unreconciled(id)));
    assert!(!worker.is_live(), "sealed by the time its handle ends");
    assert_eq!(
        book(&worker),
        BookOutcome::NotBooked(NotBooked::RunNotLive),
        "a report after the seal is late spend"
    );
    drop(worker);
    drop(release);
    assert_eq!(received(&mut rx), vec!["spend", "sealed"], "the seal is the run's one terminal");
}

/// No reaper thread: the handle ends at the drop, and the run is sealed
/// first, since nothing waits for the worker.
#[test]
fn s2_with_no_reaper_thread_the_run_is_sealed_at_the_drop() {
    let id = "sa_reap_s2";
    let policy = running(id);
    let (worker, mut rx) = run(id);
    let (release, thread, end) = held_worker(&policy, id, WorkerTerminal::Completed);
    finished::fail_spawn(id);
    drop(DropReap::arm(
        Arc::clone(&policy),
        id.to_owned(),
        thread,
        end,
        Some(worker.clone()),
    ));
    assert!(!policy.is_unreconciled(id), "ended at the drop");
    assert_eq!(finished::count(id), 1);
    assert!(!worker.is_live(), "sealed");
    assert_eq!(book(&worker), BookOutcome::NotBooked(NotBooked::RunNotLive));
    drop(release);
    drop(worker);
    assert_eq!(received(&mut rx), vec!["sealed"]);
}

/// A worker that stops before the bound ends its own run: its reports are
/// booked, the run ends once its last lease is gone, and nothing is sealed.
#[test]
fn s3_a_worker_that_stops_ends_its_own_run() {
    let id = "sa_reap_s3";
    let policy = running(id);
    let (lease, mut rx) = run(id);
    let worker = lease.clone();
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let thread = std::thread::spawn(move || {
        let _ = wait.recv();
        assert_eq!(book(&worker), BookOutcome::Booked { seq: 0 });
    });
    // A plain thread that never takes a oneshot's last act: the reaper
    // sees it stop by its join handle.
    let end = Arc::new(WorkerEnd::default());
    drop(DropReap::arm(Arc::clone(&policy), id.to_owned(), thread, end, Some(lease)));
    std::thread::sleep(Duration::from_millis(100));
    release.send(()).unwrap();
    assert!(wait_until(Duration::from_secs(10), || !policy.is_unreconciled(id)));
    let mut seen = Vec::new();
    let waited = Instant::now();
    while !seen.contains(&"ended") {
        assert!(
            waited.elapsed() < Duration::from_secs(10),
            "the run ends once the reaper lets its lease go: {seen:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
        seen.extend(received(&mut rx));
    }
    assert_eq!(seen, vec!["spend", "ended"]);
}

/// The caller that got its result disarms the guard: the run is not sealed
/// and ends when its last lease goes.
#[test]
fn s4_a_disarmed_guard_seals_nothing() {
    let id = "sa_reap_s4";
    let policy = running(id);
    let (lease, mut rx) = run(id);
    let (release, thread, end) = held_worker(&policy, id, WorkerTerminal::Completed);
    let guard =
        DropReap::arm(Arc::clone(&policy), id.to_owned(), thread, end, Some(lease.clone()));
    release.send(()).unwrap();
    assert!(guard.disarm().join_and_claim(), "the caller ends the handle");
    assert!(lease.is_live(), "not sealed");
    drop(lease);
    assert_eq!(received(&mut rx), vec!["ended"]);
    policy.finish_one_shot(id, WorkerTerminal::Completed).unwrap();
}
