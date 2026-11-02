//! Record publication and run ownership (§3.2–3.3; slice 4 durability).
//! Run IDs here use `run(131..)`: ownership is process-wide, and the
//! checkpoint tests own `run(41..)` concurrently.
use std::os::unix::fs::{symlink, MetadataExt as _};

use super::*;
use crate::tools::subagent::record::{Candidate, FinalRecord, RecordKind};
use crate::tools::subagent::record_file;
use crate::tools::subagent::archive::PruneOutcome;
use crate::tools::subagent::test_fixtures::{
    head, run, write_root, Fault, FaultFs, ForkedChild, Op, PROCESS_DIR,
};
use std::os::fd::AsRawFd as _;
use std::time::{Duration, Instant};

/// The archive's pruning age.
const KEEP: Duration = Duration::from_secs(14 * 24 * 3600);

/// Make `path` look 30 days old to pruning.
fn age(path: &Path) {
    let old = std::time::SystemTime::now() - Duration::from_secs(30 * 24 * 3600);
    std::fs::File::open(path)
        .unwrap()
        .set_modified(old)
        .unwrap();
}

/// The root's lifecycle lock held exclusively through another open file
/// description (another process's pruning).
fn hold_exclusive(root: &Path) -> LifecycleGuard {
    match try_lifecycle_exclusive(root) {
        LifecycleAttempt::Held(guard) => guard,
        other => panic!("expected the lifecycle lock, got {other:?}"),
    }
}

/// The root's lifecycle lock held shared through another open file
/// description (another process's starting owner).
fn hold_shared(root: &Path) -> std::fs::File {
    let file = std::fs::File::open(root.join(".lifecycle.lock")).unwrap();
    // SAFETY: valid fd.
    assert_eq!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
        0
    );
    file
}

fn temps(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".tmp-"))
        .collect()
}

fn final_bytes(last: &str) -> Vec<u8> {
    let mut rec = head(&run(1), 0, last);
    rec.kind = RecordKind::Final;
    serde_json::to_vec(&rec).unwrap()
}

fn final_text(r: &crate::tools::subagent::test_fixtures::WriteRoot) -> String {
    match record_file::load(&r.location(RecordKind::Final))
        .unwrap()
        .final_record
    {
        Candidate::Present(FinalRecord::V2(rec)) => rec.history.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .to_owned(),
        other => panic!("expected a v2 final, got {other:?}"),
    }
}

#[test]
fn s1_publication_is_durable_private_and_leaves_no_temp() {
    let r = write_root();
    let dir = r.open();
    assert_eq!(
        publish(&dir, "sa_5.json", &final_bytes("one")),
        Publication::Durable
    );
    let meta = std::fs::metadata(r.dir.join("sa_5.json")).unwrap();
    assert_eq!(meta.mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(&r.dir).unwrap().mode() & 0o777, 0o700);
    assert!(temps(&r.dir).is_empty());
    // What the writer publishes passes the reader's checks.
    assert_eq!(final_text(&r), "one");
    assert_eq!(
        publish(&dir, "sa_5.json", &final_bytes("two")),
        Publication::Durable
    );
    assert_eq!(final_text(&r), "two");
}

#[test]
fn s2_each_failing_step_has_its_outcome() {
    let r = write_root();
    let fs = FaultFs::new(r.open());
    assert_eq!(
        publish(fs.as_ref(), "sa_5.json", &final_bytes("old")),
        Publication::Durable
    );
    let cases: [(Op, Fault, bool, &str); 4] = [
        // (fault, uncertain?, contents now on disk)
        (Op::WriteTemp, Fault::Fail, false, "old"),
        (Op::Rename, Fault::Fail, true, "old"),
        (Op::Rename, Fault::DoThenFail, true, "new"),
        (Op::SyncDir, Fault::Fail, true, "new"),
    ];
    for (op, fault, uncertain, on_disk) in cases {
        if on_disk == "new" {
            assert_eq!(
                publish(fs.as_ref(), "sa_5.json", &final_bytes("old")),
                Publication::Durable
            );
        }
        fs.inject(op, "", fault);
        let outcome = publish(fs.as_ref(), "sa_5.json", &final_bytes("new"));
        match (&outcome, uncertain) {
            (Publication::Uncertain(_), true) | (Publication::NotPublished(_), false) => {}
            _ => panic!("{op:?}: {outcome:?}"),
        }
        assert_eq!(final_text(&r), on_disk, "{op:?}");
        assert!(temps(&r.dir).is_empty(), "{op:?}: no temp file left");
    }
}

#[test]
fn s3_ownership_is_exclusive_in_process_and_by_its_lock() {
    let r = write_root();
    let dir = r.open();
    let (a, b) = (run(131), run(132));
    let owned = RunOwnership::acquire(&dir, "sa_5", &a).unwrap();
    assert_eq!(
        RunOwnership::acquire(&dir, "sa_6", &a).unwrap_err(),
        OwnershipError::AlreadyOwned
    );
    // Another run on the same handle (another owner of the lock file).
    assert_eq!(
        RunOwnership::acquire(&dir, "sa_5", &b).unwrap_err(),
        OwnershipError::LockHeld
    );
    assert_eq!(
        writer_probe(&r.location(RecordKind::Final)),
        WriterState::MayPublish
    );
    assert_eq!(
        writer_probe(&r.location(RecordKind::Head)),
        WriterState::MayPublish
    );
    drop(owned);
    assert_eq!(
        writer_probe(&r.location(RecordKind::Final)),
        WriterState::Quiescent
    );
    let again = RunOwnership::acquire(&dir, "sa_5", &b).unwrap();
    assert_eq!(again.run_id(), &b);
}

#[test]
fn s4_probe_is_quiescent_without_a_lock_and_fails_closed_on_links() {
    let r = write_root();
    // No process directory yet.
    assert_eq!(
        writer_probe(&r.location(RecordKind::Final)),
        WriterState::Quiescent
    );
    let dir = r.open();
    assert_eq!(
        writer_probe(&r.location(RecordKind::Final)),
        WriterState::Quiescent
    );
    // A lock file that is a link cannot be checked.
    let target = r.dir.join("elsewhere");
    std::fs::write(&target, b"").unwrap();
    symlink(&target, r.dir.join("sa_5.lock")).unwrap();
    assert_eq!(
        writer_probe(&r.location(RecordKind::Final)),
        WriterState::MayPublish
    );
    assert!(dir_has_live_writer(&r.dir));
    drop(dir);
}

#[test]
fn s5_pruning_keeps_a_directory_whose_run_lock_is_held() {
    let r = write_root();
    let dir = r.open();
    assert!(!dir_has_live_writer(&r.dir), "no lock files");
    let owned = RunOwnership::acquire(&dir, "sa_5", &run(133)).unwrap();
    assert!(dir_has_live_writer(&r.dir));
    // An old directory with a live owner survives the age sweep; without
    // one it goes.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 24 * 3600);
    let age_out = || {
        std::fs::File::open(&r.dir)
            .unwrap()
            .set_modified(old)
            .unwrap();
        crate::tools::subagent::archive::prune_older_than(
            &r.root,
            std::time::Duration::from_secs(14 * 24 * 3600),
        );
    };
    age_out();
    assert!(r.dir.exists(), "a held run lock keeps its directory");
    drop(owned);
    assert!(!dir_has_live_writer(&r.dir), "a released lock file is free");
    age_out();
    assert!(!r.dir.exists());
}

#[test]
fn s6_resume_sees_a_held_lock_of_another_process_directory() {
    let r = write_root();
    let dir = r.open();
    let registry = crate::runtime::subagent::SubagentRegistry::new();
    let location = r.location(RecordKind::Final);
    let own = "1-00000000-0000-4000-8000-000000000001";
    assert_ne!(own, PROCESS_DIR);
    let state = || crate::tools::subagent::admission::writer_state(&location, own, &registry);
    assert_eq!(state(), WriterState::Quiescent);
    let owned = RunOwnership::acquire(&dir, "sa_5", &run(134)).unwrap();
    assert_eq!(state(), WriterState::MayPublish);
    drop(owned);
    assert_eq!(state(), WriterState::Quiescent);
}

#[test]
fn s7_a_lock_file_that_is_not_a_private_regular_file_is_refused() {
    let r = write_root();
    let dir = r.open();
    let fifo = std::ffi::CString::new(r.dir.join("sa_5.lock").to_str().unwrap()).unwrap();
    // SAFETY: NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(matches!(
        RunOwnership::acquire(&dir, "sa_5", &run(135)),
        Err(OwnershipError::Io(_))
    ));
    assert_eq!(
        writer_probe(&r.location(RecordKind::Final)),
        WriterState::MayPublish,
        "an uncheckable lock fails closed"
    );
    std::fs::create_dir(r.dir.join("sa_6.lock")).unwrap();
    assert!(matches!(
        RunOwnership::acquire(&dir, "sa_6", &run(136)),
        Err(OwnershipError::Io(_))
    ));
}

#[test]
fn s8_release_unlocks_even_while_an_inherited_descriptor_stays_open() {
    // A child forked while the run was owned shares the lock's open file
    // description; a `dup` shares it the same way. Closing our descriptor
    // alone would leave the lock held through the other reference.
    let r = write_root();
    let dir = r.open();
    let owned = RunOwnership::acquire(&dir, "sa_5", &run(137)).unwrap();
    let inherited = owned.lock.as_ref().unwrap().duplicate().unwrap();
    drop(owned);
    assert_eq!(
        writer_probe(&r.location(RecordKind::Final)),
        WriterState::Quiescent,
        "released explicitly, not only by the last close"
    );
    assert!(RunOwnership::acquire(&dir, "sa_5", &run(138)).is_ok());
    drop(inherited);
}

#[test]
fn s9_a_child_forked_while_the_run_is_owned_does_not_keep_it_owned() {
    let r = write_root();
    let dir = r.open();
    let location = r.location(RecordKind::Final);
    let owned = RunOwnership::acquire(&dir, "sa_5", &run(139)).unwrap();
    // Blocked before exec, holding a copy of the lock's description.
    let mut child = ForkedChild::spawn();
    drop(owned);
    assert_eq!(writer_probe(&location), WriterState::Quiescent);
    let again = RunOwnership::acquire(&dir, "sa_5", &run(140));
    assert!(
        again.is_ok(),
        "reacquired while the child is alive: {again:?}"
    );
    drop(again);
    assert!(child.release_and_reap());
}

#[test]
fn s10_a_child_forked_during_a_probe_does_not_keep_its_shared_lock() {
    let r = write_root();
    let dir = r.open();
    let location = r.location(RecordKind::Final);
    // A free lock file, so the probe takes its shared lock.
    drop(RunOwnership::acquire(&dir, "sa_5", &run(141)).unwrap());
    let child = std::rc::Rc::new(std::cell::RefCell::new(None));
    let slot = child.clone();
    seams::on_probe_shared(Some(Box::new(move || {
        *slot.borrow_mut() = Some(ForkedChild::spawn());
    })));
    let state = writer_probe(&location);
    seams::on_probe_shared(None);
    assert_eq!(state, WriterState::Quiescent);
    let mut child = child
        .borrow_mut()
        .take()
        .expect("forked while the probe held its lock");
    let owned = RunOwnership::acquire(&dir, "sa_5", &run(142));
    assert!(
        owned.is_ok(),
        "acquired while the child is alive: {owned:?}"
    );
    drop(owned);
    assert!(child.release_and_reap());
}

#[test]
fn s11_opening_a_process_directory_syncs_the_root_before_use() {
    let r = write_root();
    seams::take_ops();
    drop(r.open());
    assert_eq!(seams::take_ops(), ["mkdir", "open", "sync root"]);
    // An existing directory too: its creator may have died before syncing.
    drop(r.open());
    assert_eq!(seams::take_ops(), ["mkdir (exists)", "open", "sync root"]);
    // A failed root sync yields no publishable directory.
    let other = write_root();
    seams::fail_root_sync();
    let opened = RecordDir::open_or_create(&other.root, PROCESS_DIR);
    assert!(matches!(opened, Err(RecordFileError::Io(_))), "{opened:?}");
}

#[test]
fn s12_an_acquire_during_pruning_is_busy_and_then_finds_its_directory_gone() {
    let r = write_root();
    let dir = r.open();
    // A stale, free lock file: the probe finds no live writer.
    drop(RunOwnership::acquire(&dir, "sa_5", &run(143)).unwrap());
    age(&r.dir);
    let dir = std::rc::Rc::new(dir);
    let in_hook = dir.clone();
    let during = std::rc::Rc::new(std::cell::RefCell::new(None));
    let slot = during.clone();
    seams::before_prune_delete(Some(Box::new(move |_path: &Path| {
        // Between the scan and the delete a starting owner gets no lock.
        let acquired = RunOwnership::acquire_within(
            &in_hook,
            "sa_5",
            &run(144),
            Duration::from_millis(50),
            &|| false,
        )
        .map(drop);
        *slot.borrow_mut() = Some(acquired);
    })));
    let outcome = crate::tools::subagent::archive::prune_older_than(&r.root, KEEP);
    seams::before_prune_delete(None);
    assert_eq!(outcome, PruneOutcome::Pruned(1));
    assert_eq!(
        during.borrow_mut().take().expect("the hook ran"),
        Err(OwnershipError::LifecycleBusy)
    );
    assert!(!r.dir.exists(), "pruned");
    let acquired = RunOwnership::acquire(&dir, "sa_5", &run(144)).map(drop);
    assert!(
        matches!(acquired, Err(OwnershipError::Io(_))),
        "a removed directory is not owned: {acquired:?}"
    );
}

#[test]
fn s13_an_existing_lock_file_must_be_private_and_single_linked() {
    use std::os::unix::fs::PermissionsExt as _;
    let r = write_root();
    let dir = r.open();
    let at = |handle: &str| {
        record_file::location_of(&r.root, PROCESS_DIR, handle, RecordKind::Final).unwrap()
    };
    let lock = r.dir.join("sa_5.lock");
    std::fs::write(&lock, b"").unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        RunOwnership::acquire(&dir, "sa_5", &run(145)),
        Err(OwnershipError::Io(_))
    ));
    assert_eq!(writer_probe(&at("sa_5")), WriterState::MayPublish);
    let lock = r.dir.join("sa_6.lock");
    std::fs::write(&lock, b"").unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::hard_link(&lock, r.dir.join("elsewhere")).unwrap();
    assert!(matches!(
        RunOwnership::acquire(&dir, "sa_6", &run(146)),
        Err(OwnershipError::Io(_))
    ));
    assert_eq!(writer_probe(&at("sa_6")), WriterState::MayPublish);
}

// ── The lifecycle lock never waits unboundedly ──────────────────────────

#[test]
fn l1_startup_is_busy_after_its_deadline_and_leaves_nothing_behind() {
    let r = write_root();
    let dir = r.open();
    let pruning = hold_exclusive(&r.root);
    let started = Instant::now();
    let acquired = RunOwnership::acquire_within(
        &dir,
        "sa_5",
        &run(147),
        Duration::from_millis(200),
        &|| false,
    )
    .map(drop);
    let waited = started.elapsed();
    assert_eq!(acquired, Err(OwnershipError::LifecycleBusy));
    assert!(waited >= Duration::from_millis(200), "{waited:?}");
    assert!(waited < Duration::from_secs(10), "{waited:?}");
    assert!(!r.dir.join("sa_5.lock").exists(), "no run lock was taken");
    // Nothing of the attempt is left: the run is free once pruning ends.
    drop(pruning);
    assert!(RunOwnership::acquire(&dir, "sa_5", &run(147)).is_ok());
}

#[test]
fn l2_a_cancelled_startup_stops_waiting() {
    let r = write_root();
    let dir = r.open();
    let _pruning = hold_exclusive(&r.root);
    let checks = std::sync::atomic::AtomicUsize::new(0);
    let cancelled = || checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 3;
    let started = Instant::now();
    let acquired =
        RunOwnership::acquire_within(&dir, "sa_5", &run(148), Duration::from_secs(60), &cancelled)
            .map(drop);
    assert_eq!(acquired, Err(OwnershipError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(checks.load(std::sync::atomic::Ordering::SeqCst) >= 4, "checked while waiting");
}

#[test]
fn l3_pruning_skips_a_held_lifecycle_lock_and_deletes_nothing() {
    let r = write_root();
    drop(r.open());
    age(&r.dir);
    // Held exclusively (another pruning) or shared (a starting owner).
    let pruning = hold_exclusive(&r.root);
    assert_eq!(
        crate::tools::subagent::archive::prune_older_than(&r.root, KEEP),
        PruneOutcome::Busy
    );
    drop(pruning);
    let starting = hold_shared(&r.root);
    assert_eq!(
        crate::tools::subagent::archive::prune_older_than(&r.root, KEEP),
        PruneOutcome::Busy
    );
    assert!(r.dir.exists(), "nothing was deleted");
    drop(starting);
    assert_eq!(
        crate::tools::subagent::archive::prune_older_than(&r.root, KEEP),
        PruneOutcome::Pruned(1)
    );
    assert!(!r.dir.exists());
}

#[test]
fn l4_an_unusable_lifecycle_lock_is_refused_and_kept() {
    use std::os::unix::fs::PermissionsExt as _;
    let lock = |r: &crate::tools::subagent::test_fixtures::WriteRoot| r.root.join(".lifecycle.lock");
    /// What the bad lock file is, and how to make it at a path.
    type BadLock<'a> = (&'a str, &'a dyn Fn(&Path));
    let cases: [BadLock; 4] = [
        ("group-readable", &|path| {
            std::fs::write(path, b"").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }),
        ("hard-linked", &|path| {
            std::fs::write(path, b"").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::hard_link(path, path.with_extension("other")).unwrap();
        }),
        ("a directory", &|path| std::fs::create_dir(path).unwrap()),
        ("a symlink", &|path| {
            let target = path.with_extension("target");
            std::fs::write(&target, b"").unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
            symlink(&target, path).unwrap();
        }),
    ];
    for (n, (what, make)) in cases.into_iter().enumerate() {
        let r = write_root();
        let dir = r.open();
        age(&r.dir);
        make(&lock(&r));
        assert!(
            matches!(
                crate::tools::subagent::archive::prune_older_than(&r.root, KEEP),
                PruneOutcome::Invalid(_)
            ),
            "{what}"
        );
        assert!(r.dir.exists(), "{what}: nothing was deleted");
        let acquired =
            RunOwnership::acquire(&dir, "sa_5", &run(149 + n as u8)).map(drop);
        assert!(matches!(acquired, Err(OwnershipError::Io(_))), "{what}: {acquired:?}");
        assert!(
            std::fs::symlink_metadata(lock(&r)).is_ok(),
            "{what}: never repaired or removed"
        );
    }
}

#[test]
fn l4b_an_unusable_root_is_told_apart_from_a_bad_lock_file() {
    use std::os::unix::fs::PermissionsExt as _;
    let r = write_root();
    drop(r.open());
    age(&r.dir);
    // The root cannot be opened: removing its lock file would not help.
    std::fs::set_permissions(&r.root, std::fs::Permissions::from_mode(0o000)).unwrap();
    let outcome = crate::tools::subagent::archive::prune_older_than(&r.root, KEEP);
    std::fs::set_permissions(&r.root, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        matches!(outcome, PruneOutcome::RootUnusable(_)),
        "{outcome:?}"
    );
    assert!(r.dir.exists(), "nothing was deleted");
    assert!(
        matches!(
            try_lifecycle_exclusive(&r.root),
            LifecycleAttempt::Held(_)
        ),
        "the lock file itself was fine"
    );
}

/// A root that opens but forbids creating the lock file: the root is the
/// problem (there is no lock file to remove), not a bad lock file.
#[test]
fn l4c_a_root_that_forbids_creating_the_lock_is_an_unusable_root() {
    use std::os::unix::fs::PermissionsExt as _;
    let r = write_root();
    drop(r.open());
    age(&r.dir);
    let lock = r.root.join(".lifecycle.lock");
    let _ = std::fs::remove_file(&lock);
    std::fs::set_permissions(&r.root, std::fs::Permissions::from_mode(0o500)).unwrap();
    let attempt = try_lifecycle_exclusive(&r.root);
    let outcome = crate::tools::subagent::archive::prune_older_than(&r.root, KEEP);
    std::fs::set_permissions(&r.root, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(attempt, LifecycleAttempt::RootUnusable(_)), "{attempt:?}");
    assert!(
        matches!(outcome, PruneOutcome::RootUnusable(_)),
        "{outcome:?}"
    );
    assert!(r.dir.exists(), "nothing was deleted");
    assert!(std::fs::symlink_metadata(&lock).is_err(), "nothing was created");
}

#[test]
fn l5_a_late_wake_up_never_acquires_past_the_deadline() {
    let r = write_root();
    let dir = r.open();
    let pruning = std::cell::RefCell::new(Some(hold_exclusive(&r.root)));
    let wait = Duration::from_millis(100);
    let started = Instant::now();
    // Asked before each attempt: once the wait is half over, oversleep past
    // the deadline (a preempted poll) and let pruning end meanwhile.
    let cancelled = || {
        if started.elapsed() >= wait / 2 && pruning.borrow().is_some() {
            std::thread::sleep(wait);
            pruning.borrow_mut().take();
        }
        false
    };
    let acquired =
        RunOwnership::acquire_within(&dir, "sa_5", &run(149), wait, &cancelled).map(drop);
    assert_eq!(acquired, Err(OwnershipError::LifecycleBusy));
    assert!(pruning.borrow().is_none(), "the lock was free at the late attempt");
    assert!(!r.dir.join("sa_5.lock").exists(), "no run lock was taken");
}

#[test]
fn l6_the_first_attempt_is_made_at_once_even_without_a_wait() {
    let r = write_root();
    let dir = r.open();
    let asked = std::sync::atomic::AtomicUsize::new(0);
    let count = || {
        asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        false
    };
    // Free: one immediate attempt takes it.
    let owner = RunOwnership::acquire_within(&dir, "sa_5", &run(150), Duration::ZERO, &count);
    assert!(owner.is_ok(), "{owner:?}");
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    drop(owner);
    // Held: busy after that one attempt, without sleeping.
    let _pruning = hold_exclusive(&r.root);
    asked.store(0, std::sync::atomic::Ordering::SeqCst);
    let started = Instant::now();
    let busy = RunOwnership::acquire_within(&dir, "sa_5", &run(150), Duration::ZERO, &count)
        .map(drop);
    assert_eq!(busy, Err(OwnershipError::LifecycleBusy));
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// Where a record is read back from comes from the root the directory was
/// opened at (canonicalized then, on a blocking worker): answering it later
/// touches no filesystem, so it can run on the executor.
#[test]
fn a_record_location_is_answered_without_the_filesystem() {
    let r = write_root();
    let dir = r.open();
    let expected = r.location(RecordKind::Final).path();
    // The root is gone from its path: resolving it again would fail.
    std::fs::rename(&r.root, r.root.with_file_name("moved-history")).unwrap();
    let location = dir.location("sa_5", RecordKind::Final).unwrap();
    assert_eq!(location.path(), expected);
    assert_eq!(
        dir.location("sa_5", RecordKind::Head).unwrap().path(),
        expected.with_file_name("sa_5.head.json")
    );
    // Names are still checked.
    assert!(dir.location("sa_x", RecordKind::Final).is_err());
}

/// The run's writer-table entry, if it has one.
fn writer_of(run_id: &RunId) -> Option<WriterEntry> {
    writers_snapshot().into_iter().find(|e| &e.run_id == run_id)
}

/// Spec §5: the writer table (a daemon reload's view of the runs this
/// process owns) has an entry exactly while the run's ownership lives, and
/// it says what the owner last noted.
#[test]
fn s12_the_writer_table_lives_and_dies_with_the_ownership() {
    let r = write_root();
    let dir = r.open();
    let id = run(151);
    assert_eq!(writer_of(&id), None);
    let owned = RunOwnership::acquire(&dir, "sa_5", &id).unwrap();
    let fresh = writer_of(&id).expect("an owned run is in the table");
    assert_eq!(fresh.handle, "sa_5");
    assert_eq!(fresh.session, None);
    assert_eq!(fresh.state, PublicationState::NoHead);
    assert_eq!(fresh.verdict(), WriterVerdict::Starting, "a launch has no head yet");

    owned.label_session(Some("s-1"));
    owned.note_state(None, PublicationState::Pending(PendingOp::Head(0)));
    let writing = writer_of(&id).unwrap();
    assert_eq!(writing.session.as_deref(), Some("s-1"));
    assert_eq!(writing.verdict(), WriterVerdict::Publishing);
    owned.note_state(Some(0), PublicationState::Committed(0));
    assert_eq!(writer_of(&id).unwrap().verdict(), WriterVerdict::Durable);

    // A persistence task still holding the ownership keeps the entry.
    let held = std::sync::Arc::new(owned);
    let task = std::sync::Arc::clone(&held);
    drop(held);
    assert!(writer_of(&id).is_some(), "an orphaned write still owns the run");
    drop(task);
    assert_eq!(writer_of(&id), None, "released with the last holder");
}

#[test]
fn s13_a_forced_reload_marks_its_waived_handles() {
    mark_waived(["sa_901".to_string(), "sa_902".to_string()]);
    assert!(record_waived("sa_901") && record_waived("sa_902"));
    assert!(!record_waived("sa_903"));
    mark_waived(Vec::<String>::new());
    assert!(!record_waived("sa_901"), "the next reload's set replaces it");
}
