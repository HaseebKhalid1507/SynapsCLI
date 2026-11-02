//! F obligations (§11; slice 4 durability): publication states of one run's
//! checkpoint owner under injected faults. Every case ends by resolving the
//! disk the way a resume does (`record_file::load` + `resolve`).
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::*;
use crate::tools::subagent::record::{
    resolve, Expectations, Resolution, ResolveInput, RunId, WriterState,
};
use crate::tools::subagent::record_file;
use crate::tools::subagent::record_store::{writer_probe, RunOwnership};
use crate::tools::subagent::test_fixtures::{
    gate, head, run, write_root, Fault, FaultFs, Op, WriteRoot,
};

const HEAD: &str = "sa_5.head.json";
const FINAL: &str = "sa_5.json";

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

struct Run {
    r: WriteRoot,
    fs: Arc<FaultFs>,
    id: RunId,
    cp: WorkerCheckpoint,
}

/// A checkpoint owner of a fresh run with its spawn head committed.
async fn started(n: u8) -> Run {
    let r = write_root();
    let fs = FaultFs::new(r.open());
    let id = run(n);
    let owner = RunOwnership::acquire(fs.dir(), "sa_5", &id).unwrap();
    let mut cp = WorkerCheckpoint::new(fs.clone(), owner);
    assert_eq!(cp.state(), PublicationState::NoHead);
    assert!(
        !cp.may_infer(),
        "no request before the spawn head is durable"
    );
    assert_eq!(
        cp.publish_head(head(&id, 0, "spawn"), soon())
            .await
            .unwrap(),
        PublicationState::Committed(0)
    );
    assert!(cp.may_infer());
    Run { r, fs, id, cp }
}

/// What a resume of this location would select.
fn on_disk(r: &WriteRoot) -> Resolution {
    let loaded = record_file::load(&r.location(RecordKind::Final)).unwrap();
    resolve(ResolveInput {
        head: loaded.head,
        final_record: loaded.final_record,
        writer: WriterState::Quiescent,
        expect: Expectations {
            handle_id: "sa_5".into(),
            run_id: None,
            binding: None,
        },
    })
}

fn selected(r: &WriteRoot) -> (RecordKind, u64, String) {
    match on_disk(r) {
        Resolution::Authoritative { kind, record, .. } => (
            kind,
            record.generation,
            record.history.last().unwrap()["content"]
                .as_str()
                .unwrap()
                .to_owned(),
        ),
        other => panic!("expected an authoritative record, got {other:?}"),
    }
}

fn final_of(id: &RunId, last: &str) -> WorkerRecord {
    let mut rec = head(id, 0, last);
    rec.kind = RecordKind::Final;
    rec
}

#[tokio::test]
async fn f1_temp_write_failure_saves_the_latest_history_at_g() {
    let mut t = started(41).await;
    t.fs.inject(Op::WriteTemp, "", Fault::Fail);
    assert_eq!(
        t.cp.publish_head(head(&t.id, 1, "successor"), soon())
            .await
            .unwrap(),
        PublicationState::PrePublicationFailure {
            committed: Some(0),
            attempted: 1
        }
    );
    assert!(
        !t.cp.may_infer(),
        "no successor request after a failed head"
    );
    // The final carries the latest in-memory history, as generation 0.
    assert_eq!(
        t.cp.finalize(final_of(&t.id, "rounds after g"))
            .await
            .unwrap(),
        FinalOutcome::Durable { head_removed: true }
    );
    assert_eq!(t.cp.state(), PublicationState::Final);
    assert_eq!(
        selected(&t.r),
        (RecordKind::Final, 0, "rounds after g".into())
    );
    assert!(!t.r.dir.join(HEAD).exists());
}

#[tokio::test]
async fn f2_rename_ok_sync_failed_is_uncertain_then_republished() {
    let mut t = started(42).await;
    t.fs.inject(Op::SyncDir, "", Fault::Fail);
    assert_eq!(
        t.cp.publish_head(head(&t.id, 1, "successor"), soon())
            .await
            .unwrap(),
        PublicationState::Uncertain { attempted: 1 }
    );
    assert!(
        !t.cp.may_infer(),
        "zero successor requests before a durable ACK"
    );
    assert_eq!(
        t.cp.finalize(final_of(&t.id, "x")).await.unwrap_err(),
        CheckpointError::PublicationUncertain
    );
    let before = t.fs.log().len();
    assert!(t.cp.quiesce(soon()).await);
    assert_eq!(
        t.cp.reconcile().await.unwrap(),
        PublicationState::Committed(1)
    );
    // Adopted only after a durable republication of the same head.
    assert_eq!(
        t.fs.log()[before..],
        ["write".to_owned(), format!("rename {HEAD}"), "sync".into()]
    );
    assert!(t.cp.may_infer());
    assert_eq!(selected(&t.r), (RecordKind::Head, 1, "successor".into()));
}

#[tokio::test]
async fn f3_receipt_loss_blocks_inference_and_finalization() {
    // Lost after the rename: the disk head is the candidate.
    let mut t = started(43).await;
    t.fs.inject(Op::SyncDir, "", Fault::Panic);
    assert_eq!(
        t.cp.publish_head(head(&t.id, 1, "successor"), soon())
            .await
            .unwrap(),
        PublicationState::Uncertain { attempted: 1 }
    );
    assert!(!t.cp.may_infer());
    assert_eq!(
        t.cp.finalize(final_of(&t.id, "x")).await.unwrap_err(),
        CheckpointError::PublicationUncertain
    );
    assert_eq!(
        t.cp.reconcile().await.unwrap(),
        PublicationState::Committed(1)
    );

    // Lost before the rename: the disk still has g, which is kept, and the
    // candidate is only reported.
    let mut t = started(44).await;
    t.fs.inject(Op::WriteTemp, "", Fault::Panic);
    t.cp.publish_head(head(&t.id, 1, "successor"), soon())
        .await
        .unwrap();
    assert!(!t.cp.may_infer());
    assert_eq!(
        t.cp.reconcile().await.unwrap(),
        PublicationState::KeptAfterUncertain { committed: Some(0) }
    );
    assert!(!t.cp.may_infer(), "the run finalizes from g");
    let report = std::fs::read(t.r.dir.join("sa_5.report.json")).unwrap();
    assert!(String::from_utf8_lossy(&report).contains("successor"));
    assert_eq!(
        t.cp.finalize(final_of(&t.id, "in-memory after g+1"))
            .await
            .unwrap(),
        FinalOutcome::Durable { head_removed: true }
    );
    // Built from g as is, not from the unpublished in-memory state.
    assert_eq!(selected(&t.r), (RecordKind::Final, 0, "spawn".into()));
}

#[tokio::test]
async fn f4_crash_after_rename_recovers_the_head() {
    let mut t = started(45).await;
    assert_eq!(
        t.cp.publish_head(head(&t.id, 1, "successor"), soon())
            .await
            .unwrap(),
        PublicationState::Committed(1)
    );
    let location = t.r.location(RecordKind::Final);
    assert_eq!(
        writer_probe(&location),
        WriterState::MayPublish,
        "live owner"
    );
    // The process dies here: no final; its lock goes with it.
    drop(t.cp);
    assert_eq!(writer_probe(&location), WriterState::Quiescent);
    match on_disk(&t.r) {
        Resolution::Authoritative {
            kind: RecordKind::Head,
            record,
            loss_warning: true,
        } => assert_eq!(record.generation, 1),
        other => panic!("expected the head as a crash checkpoint, got {other:?}"),
    }
}

#[tokio::test]
async fn f5_final_ambiguity_keeps_the_head_until_a_durable_republication() {
    let mut t = started(46).await;
    t.fs.inject(Op::SyncDir, "", Fault::Fail);
    assert!(matches!(
        t.cp.finalize(final_of(&t.id, "final")).await.unwrap(),
        FinalOutcome::Uncertain(_)
    ));
    assert_eq!(t.cp.state(), PublicationState::FinalUncertain);
    assert!(t.r.dir.join(HEAD).exists(), "the head is kept");
    // The resolver may select that final; the owner has not acknowledged it.
    assert_eq!(selected(&t.r), (RecordKind::Final, 0, "final".into()));
    let before = t.fs.log().len();
    assert_eq!(
        t.cp.retry_final().await.unwrap(),
        FinalOutcome::Durable { head_removed: true }
    );
    // Republished durably before the head goes.
    assert_eq!(
        t.fs.log()[before..],
        [
            "write".to_owned(),
            format!("rename {FINAL}"),
            "sync".into(),
            format!("remove {HEAD}"),
            "sync".into()
        ]
    );
    assert!(!t.r.dir.join(HEAD).exists());

    // A final that failed before its rename can simply be retried.
    let mut t = started(47).await;
    t.fs.inject(Op::WriteTemp, "", Fault::Fail);
    assert!(matches!(
        t.cp.finalize(final_of(&t.id, "final")).await.unwrap(),
        FinalOutcome::NotPublished(_)
    ));
    assert!(!t.r.dir.join(FINAL).exists());
    assert_eq!(
        t.cp.finalize(final_of(&t.id, "final")).await.unwrap(),
        FinalOutcome::Durable { head_removed: true }
    );
}

#[tokio::test]
async fn f6_head_delete_failure_is_harmless() {
    let mut t = started(48).await;
    t.fs.inject(Op::Remove, HEAD, Fault::Fail);
    assert_eq!(
        t.cp.finalize(final_of(&t.id, "final")).await.unwrap(),
        FinalOutcome::Durable {
            head_removed: false
        }
    );
    assert!(t.r.dir.join(HEAD).exists());
    // The final supersedes a head of its generation.
    assert_eq!(selected(&t.r), (RecordKind::Final, 0, "final".into()));
}

#[tokio::test]
async fn f7_late_rename_after_timeout_is_not_overwritten() {
    let mut t = started(49).await;
    let (g, fault) = gate();
    t.fs.inject(Op::Rename, HEAD, fault);
    let deadline = Instant::now() + Duration::from_millis(200);
    assert_eq!(
        t.cp.publish_head(head(&t.id, 1, "late successor"), deadline)
            .await
            .unwrap(),
        PublicationState::Pending(PendingOp::Head(1))
    );
    // A timed-out waiter authorizes nothing.
    assert!(!t.cp.may_infer());
    assert_eq!(
        t.cp.finalize(final_of(&t.id, "too early"))
            .await
            .unwrap_err(),
        CheckpointError::Pending
    );
    assert_eq!(
        t.cp.reconcile().await.unwrap_err(),
        CheckpointError::Pending
    );
    assert!(
        !t.cp
            .quiesce(Instant::now() + Duration::from_millis(50))
            .await
    );
    // The pending task still owns the run.
    let location = t.r.location(RecordKind::Final);
    assert_eq!(writer_probe(&location), WriterState::MayPublish);
    assert!(
        !t.fs.log().iter().any(|e| e.contains(FINAL)),
        "no final yet"
    );
    // The late rename lands; joining the saved task decides.
    g.open.send(()).unwrap();
    assert!(t.cp.quiesce(soon()).await);
    assert_eq!(t.cp.state(), PublicationState::Committed(1));
    assert_eq!(
        t.cp.finalize(final_of(&t.id, "late successor"))
            .await
            .unwrap(),
        FinalOutcome::Durable { head_removed: true }
    );
    assert_eq!(
        selected(&t.r),
        (RecordKind::Final, 1, "late successor".into())
    );
}

#[tokio::test]
async fn f8_a_pending_publication_keeps_the_run_owned_after_its_owner_drops() {
    let mut t = started(50).await;
    let (g, fault) = gate();
    t.fs.inject(Op::Rename, HEAD, fault);
    let deadline = Instant::now() + Duration::from_millis(100);
    t.cp.publish_head(head(&t.id, 1, "pending"), deadline)
        .await
        .unwrap();
    let location = t.r.location(RecordKind::Final);
    drop(t.cp);
    assert_eq!(writer_probe(&location), WriterState::MayPublish);
    assert!(
        RunOwnership::acquire(t.fs.dir(), "sa_5", &t.id).is_err(),
        "not re-owned while its task may publish"
    );
    g.open.send(()).unwrap();
    let released = async {
        while writer_probe(&location) != WriterState::Quiescent {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(30), released)
        .await
        .expect("the lock goes with the finished task");
}

#[tokio::test]
async fn f9_checkpoints_are_this_runs_next_and_unstarted_after_the_deadline() {
    let mut t = started(51).await;
    let other = run(52);
    assert_eq!(
        t.cp.publish_head(head(&other, 1, "x"), soon())
            .await
            .unwrap_err(),
        CheckpointError::WrongRecord("another run")
    );
    assert_eq!(
        t.cp.publish_head(head(&t.id, 2, "x"), soon())
            .await
            .unwrap_err(),
        CheckpointError::WrongRecord("not the next generation")
    );
    let before = t.fs.log().len();
    assert_eq!(
        t.cp.publish_head(head(&t.id, 1, "x"), Instant::now())
            .await
            .unwrap_err(),
        CheckpointError::DeadlinePassed
    );
    assert_eq!(t.fs.log().len(), before, "nothing started");
    assert_eq!(t.cp.state(), PublicationState::Committed(0));
}

/// Poll `operation` until its saved task waits at the gate, then drop only
/// the operation (the waiter), keeping the checkpoint.
async fn drop_at_gate<F: std::future::Future>(
    operation: F,
    reached: tokio::sync::oneshot::Receiver<()>,
) {
    tokio::pin!(operation);
    tokio::select! {
        biased;
        _ = reached => {}
        _ = &mut operation => panic!("the operation finished before its gate"),
    }
}

/// Every operation that needs the checkpoint idle is refused.
async fn all_refused_while_pending(t: &mut Run) {
    assert!(!t.cp.may_infer(), "a dropped waiter authorizes nothing");
    assert_eq!(
        t.cp.publish_head(head(&t.id, 1, "replacement"), soon())
            .await
            .unwrap_err(),
        CheckpointError::Pending
    );
    assert_eq!(
        t.cp.finalize(final_of(&t.id, "replacement"))
            .await
            .unwrap_err(),
        CheckpointError::Pending
    );
    assert_eq!(
        t.cp.retry_final().await.unwrap_err(),
        CheckpointError::Pending
    );
    assert_eq!(
        t.cp.reconcile().await.unwrap_err(),
        CheckpointError::Pending
    );
}

#[tokio::test]
async fn c1_a_dropped_head_waiter_leaves_its_task_as_a_barrier() {
    let mut t = started(53).await;
    let (g, fault) = gate();
    t.fs.inject(Op::Rename, HEAD, fault);
    drop_at_gate(
        t.cp.publish_head(head(&t.id, 1, "successor"), soon()),
        g.reached,
    )
    .await;
    assert_eq!(t.cp.state(), PublicationState::Pending(PendingOp::Head(1)));
    all_refused_while_pending(&mut t).await;
    let before = t.fs.log().len();
    g.open.send(()).unwrap();
    assert!(t.cp.quiesce(soon()).await);
    assert_eq!(
        t.fs.log()[before..],
        [format!("rename {HEAD} (late)"), "sync".into()],
        "only the saved task wrote"
    );
    assert_eq!(t.cp.state(), PublicationState::Committed(1));
    assert!(t.cp.may_infer());
    assert_eq!(selected(&t.r), (RecordKind::Head, 1, "successor".into()));
}

#[tokio::test]
async fn c2_a_dropped_final_or_retry_waiter_leaves_its_task_as_a_barrier() {
    // The first final.
    let mut t = started(54).await;
    let (g, fault) = gate();
    t.fs.inject(Op::Rename, FINAL, fault);
    drop_at_gate(t.cp.finalize(final_of(&t.id, "final")), g.reached).await;
    assert_eq!(t.cp.state(), PublicationState::Pending(PendingOp::Final));
    all_refused_while_pending(&mut t).await;
    assert!(t.r.dir.join(HEAD).exists());
    g.open.send(()).unwrap();
    assert!(t.cp.quiesce(soon()).await);
    assert_eq!(t.cp.state(), PublicationState::Final);
    assert!(!t.r.dir.join(HEAD).exists(), "removed only after the final");
    assert_eq!(selected(&t.r), (RecordKind::Final, 0, "final".into()));

    // A retry of an uncertain final.
    let mut t = started(55).await;
    t.fs.inject(Op::SyncDir, "", Fault::Fail);
    assert!(matches!(
        t.cp.finalize(final_of(&t.id, "final")).await.unwrap(),
        FinalOutcome::Uncertain(_)
    ));
    let (g, fault) = gate();
    t.fs.inject(Op::Rename, FINAL, fault);
    drop_at_gate(t.cp.retry_final(), g.reached).await;
    assert_eq!(t.cp.state(), PublicationState::Pending(PendingOp::Final));
    all_refused_while_pending(&mut t).await;
    assert!(
        t.r.dir.join(HEAD).exists(),
        "kept while the final is unsettled"
    );
    g.open.send(()).unwrap();
    assert!(t.cp.quiesce(soon()).await);
    assert_eq!(t.cp.state(), PublicationState::Final);
    assert!(!t.r.dir.join(HEAD).exists());
}

#[tokio::test]
async fn c3_a_dropped_reconcile_waiter_leaves_its_republication_as_a_barrier() {
    let mut t = started(56).await;
    t.fs.inject(Op::SyncDir, "", Fault::Fail);
    assert_eq!(
        t.cp.publish_head(head(&t.id, 1, "successor"), soon())
            .await
            .unwrap(),
        PublicationState::Uncertain { attempted: 1 }
    );
    let (g, fault) = gate();
    t.fs.inject(Op::Rename, HEAD, fault);
    drop_at_gate(t.cp.reconcile(), g.reached).await;
    assert_eq!(
        t.cp.state(),
        PublicationState::Pending(PendingOp::Republish(1))
    );
    all_refused_while_pending(&mut t).await;
    g.open.send(()).unwrap();
    assert!(t.cp.quiesce(soon()).await);
    assert_eq!(t.cp.state(), PublicationState::Committed(1));
    assert_eq!(selected(&t.r), (RecordKind::Head, 1, "successor".into()));
}

#[tokio::test]
async fn c4_a_dropped_waiter_during_head_removal_leaves_the_removal_as_a_barrier() {
    let mut t = started(57).await;
    let (g, fault) = gate();
    t.fs.inject(Op::Remove, HEAD, fault);
    drop_at_gate(t.cp.finalize(final_of(&t.id, "final")), g.reached).await;
    assert_eq!(
        t.cp.state(),
        PublicationState::Pending(PendingOp::RemoveHead)
    );
    all_refused_while_pending(&mut t).await;
    g.open.send(()).unwrap();
    assert!(t.cp.quiesce(soon()).await);
    assert_eq!(t.cp.state(), PublicationState::Final);
    assert!(!t.r.dir.join(HEAD).exists());
    assert_eq!(selected(&t.r), (RecordKind::Final, 0, "final".into()));
}

#[tokio::test]
async fn c5_a_dropped_waiter_during_the_report_leaves_it_as_a_barrier() {
    let mut t = started(58).await;
    // Lost before the rename: reconciling finds g and reports the candidate.
    t.fs.inject(Op::WriteTemp, "", Fault::Panic);
    t.cp.publish_head(head(&t.id, 1, "successor"), soon())
        .await
        .unwrap();
    let (g, fault) = gate();
    t.fs.inject(Op::Rename, "sa_5.report.json", fault);
    drop_at_gate(t.cp.reconcile(), g.reached).await;
    assert_eq!(t.cp.state(), PublicationState::Pending(PendingOp::Report));
    all_refused_while_pending(&mut t).await;
    assert!(!t.r.dir.join("sa_5.report.json").exists());
    g.open.send(()).unwrap();
    assert!(t.cp.quiesce(soon()).await);
    assert_eq!(
        t.cp.state(),
        PublicationState::KeptAfterUncertain { committed: Some(0) }
    );
    assert!(t.r.dir.join("sa_5.report.json").exists());
    assert_eq!(selected(&t.r), (RecordKind::Head, 0, "spawn".into()));
}

/// Hosts must not relocate an active history root (§3.3). An uncertain
/// head reconciled after the root moved is read under the root's old
/// pathname, where nothing is: the publication stays unresolved (nothing
/// adopted, republished or reported, no inference, no final).
#[tokio::test]
async fn f12_a_reconcile_after_the_root_moved_stays_unresolved() {
    let mut t = started(59).await;
    t.fs.inject(Op::SyncDir, "", Fault::Fail);
    assert_eq!(
        t.cp.publish_head(head(&t.id, 1, "successor"), soon())
            .await
            .unwrap(),
        PublicationState::Uncertain { attempted: 1 }
    );
    let moved = t.r.root.with_file_name("subagent-history.moved");
    std::fs::rename(&t.r.root, &moved).unwrap();
    let before = t.fs.log().len();
    assert_eq!(
        t.cp.reconcile().await.unwrap(),
        PublicationState::Unresolved
    );
    assert_eq!(t.fs.log().len(), before, "nothing written");
    assert!(!t.cp.may_infer());
    assert_eq!(
        t.cp.finalize(final_of(&t.id, "x")).await.unwrap_err(),
        CheckpointError::PublicationUncertain
    );
    assert!(!moved.join(crate::tools::subagent::test_fixtures::PROCESS_DIR)
        .join("sa_5.report.json")
        .exists());
}

/// The writer-table entries of `id` (other tests own runs concurrently).
fn writers_of(id: &RunId) -> Vec<crate::tools::subagent::record_store::WriterEntry> {
    crate::tools::subagent::record_store::writers_snapshot()
        .into_iter()
        .filter(|e| &e.run_id == id)
        .collect()
}

/// Spec §5: a reload's check passes a run with a committed head, and a
/// run whose final is durable even while its head removal is in flight
/// (a failed removal is harmless).
#[tokio::test]
async fn reload_check_passes_a_final_and_a_committed_head() {
    use crate::daemon::worker_check::settle;
    use crate::tools::subagent::record_store::WriterVerdict;
    let committed = started(61).await;
    assert_eq!(writers_of(&committed.id)[0].verdict(), WriterVerdict::Durable);
    assert!(settle(Duration::ZERO, || writers_of(&committed.id)).await.is_empty());

    let mut t = started(62).await;
    let (g, fault) = gate();
    t.fs.inject(Op::Remove, HEAD, fault);
    drop_at_gate(t.cp.finalize(final_of(&t.id, "final")), g.reached).await;
    assert_eq!(
        t.cp.state(),
        PublicationState::Pending(PendingOp::RemoveHead)
    );
    let removing = writers_of(&t.id);
    assert_eq!(removing[0].state, PublicationState::Pending(PendingOp::RemoveHead));
    assert_eq!(removing[0].verdict(), WriterVerdict::Durable);
    assert!(settle(Duration::ZERO, || writers_of(&t.id)).await.is_empty());
    g.open.send(()).unwrap();
    assert!(t.cp.quiesce(soon()).await);
    assert_eq!(writers_of(&t.id)[0].state, PublicationState::Final);
}

/// Spec §5: an owner that gave up waiting for a write (as `finish` does
/// when its quiesce times out: "a record write has not finished") drops
/// the run, but the orphaned write still holds the run's ownership. A
/// reload's check names it as a record write still in flight and refuses;
/// once the write lands and lets go, the check passes.
#[tokio::test]
async fn an_orphaned_write_refuses_a_reload_until_it_lets_go() {
    use crate::daemon::worker_check::{refusal, settle};
    use crate::tools::subagent::record_store::WriterVerdict;
    let mut t = started(63).await;
    let (g, fault) = gate();
    t.fs.inject(Op::Rename, HEAD, fault);
    let deadline = Instant::now() + Duration::from_millis(100);
    assert_eq!(
        t.cp.publish_head(head(&t.id, 1, "pending"), deadline)
            .await
            .unwrap(),
        PublicationState::Pending(PendingOp::Head(1))
    );
    assert!(!t.cp.quiesce(Instant::now() + Duration::from_millis(50)).await);
    drop(t.cp);
    let left = settle(Duration::from_millis(200), || writers_of(&t.id)).await;
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].verdict, WriterVerdict::Publishing);
    let why = refusal(&left);
    assert!(
        why.starts_with("worker records are not durable: sa_5 (a record write is still in flight);"),
        "{why}"
    );
    g.open.send(()).unwrap();
    let released = async {
        while !writers_of(&t.id).is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(30), released)
        .await
        .expect("the entry goes with the finished task");
    assert!(settle(Duration::ZERO, || writers_of(&t.id)).await.is_empty());
}

/// Spec §5 ("why not" a bounded, detached reactive terminal publication):
/// a reactive run's final publishes inline, as today, and while it is in
/// flight its run is in the writer table, so a normal reload waits for it
/// within the check's bound and then refuses, naming it; once the final is
/// durable, the check passes.
#[tokio::test]
async fn a_reactive_terminal_publication_in_flight_blocks_a_normal_reload() {
    use crate::daemon::worker_check::{refusal, settle};
    use crate::tools::subagent::record_store::WriterVerdict;
    let mut t = started(64).await;
    let (g, fault) = gate();
    t.fs.inject(Op::Rename, FINAL, fault);
    drop_at_gate(t.cp.finalize(final_of(&t.id, "final")), g.reached).await;
    assert_eq!(t.cp.state(), PublicationState::Pending(PendingOp::Final));
    let left = settle(Duration::from_millis(200), || writers_of(&t.id)).await;
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].verdict, WriterVerdict::Publishing);
    assert!(
        refusal(&left).starts_with(
            "worker records are not durable: sa_5 (a record write is still in flight);"
        ),
        "{}",
        refusal(&left)
    );
    g.open.send(()).unwrap();
    assert!(t.cp.quiesce(soon()).await);
    assert!(settle(Duration::ZERO, || writers_of(&t.id)).await.is_empty());
    assert_eq!(selected(&t.r), (RecordKind::Final, 0, "final".into()));
}
