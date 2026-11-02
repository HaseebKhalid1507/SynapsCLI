//! F obligations at the drive (§11; slice 4 drive wiring, dark): a worker
//! run's spawn head, its rollover heads acknowledged only once durable, the
//! deadline barrier, and the terminal final. Every case reads the disk back
//! the way a resume does (`record_file::load` + `resolve`). Run IDs are
//! random: ownership is process-wide.
use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures::StreamExt as _;
use serde_json::{json, Value};
use tokio::sync::oneshot::error::TryRecvError;
use tokio::time::Instant;

use super::*;
use crate::runtime::continuation::MARKER;
use crate::runtime::subagent::SubagentState;
use crate::tools::subagent::archive::resume_messages;
use crate::tools::subagent::checkpoint::PublicationState;
use crate::tools::subagent::drive::WorkerDrive;
use crate::tools::subagent::lineage::{successor_lineage, worker_logical_id};
use crate::tools::subagent::record::{
    resolve, ArchiveId, Expectations, RecordKind, Resolution, ResolveInput, RunId, WriterState,
};
use crate::tools::subagent::record_file;
use crate::tools::subagent::record_store::{
    try_lifecycle_exclusive, LifecycleAttempt, OwnershipError, RecordDir,
};
use crate::tools::subagent::test_fixtures::{
    gate, head, marker_message, meta, offline_binding, private_file, record, write_root, Fault,
    FaultFs, Op, WriteRoot, PROCESS_DIR,
};
use crate::{CancellationToken, SessionEvent, SharedMessage, StreamEvent};
use agent_core::core::context_head::ContextHeadReceipt;

const HEAD: &str = "sa_5.head.json";
const FINAL: &str = "sa_5.json";

fn msgs(values: Vec<Value>) -> Vec<SharedMessage> {
    values.into_iter().map(Arc::new).collect()
}

fn user(text: &str) -> Value {
    json!({"role": "user", "content": text})
}

fn assistant(text: &str) -> Value {
    json!({"role": "assistant", "content": text})
}

/// The opening messages of a fresh run.
fn task() -> Vec<SharedMessage> {
    msgs(vec![user("task")])
}

fn arch(n: u8) -> ArchiveId {
    ArchiveId::parse(&format!("{n:02x}").repeat(16)).unwrap()
}

/// The successor window a rollover hands over: its marker, then `last`.
fn successor(archive: &ArchiveId, window: u64, last: &str) -> Vec<SharedMessage> {
    msgs(vec![marker_message(archive, window), assistant(last)])
}

fn template(run: &RunId) -> WorkerRecord {
    start_template(run.clone(), "sa_5", offline_binding(), meta(), &task()).unwrap()
}

async fn claim_run(r: &WriteRoot, run: &RunId) -> Result<(RecordDir, RunOwnership), ClaimError> {
    claim(
        r.root.clone(),
        PROCESS_DIR.into(),
        "sa_5".into(),
        run.clone(),
        Duration::from_secs(2),
        CancellationToken::new(),
    )
    .await
}

/// A fresh run claimed and begun: its spawn head is committed.
async fn begun(
    r: &WriteRoot,
    deadline: Option<Instant>,
) -> (Arc<FaultFs>, RunId, WorkerPersistence) {
    let id = RunId::random();
    let (dir, owner) = claim_run(r, &id).await.unwrap();
    let fs = FaultFs::new(dir);
    match WorkerPersistence::begin(fs.clone(), owner, template(&id), deadline).await {
        Begin::On(p) => (fs, id, *p),
        Begin::Off(why) | Begin::Refused(why) => panic!("expected rollover on: {why}"),
    }
}

fn checkpoint(
    session_id: String,
    messages: Vec<SharedMessage>,
) -> (
    StreamEvent,
    tokio::sync::oneshot::Receiver<Result<(), String>>,
) {
    let (receipt, rx) = ContextHeadReceipt::channel();
    (
        StreamEvent::Session(SessionEvent::ContextHeadCheckpoint {
            session_id,
            messages,
            receipt,
        }),
        rx,
    )
}

fn drive(
    timeout_secs: u64,
) -> (
    WorkerDrive,
    Arc<RwLock<SubagentState>>,
    CancellationToken,
) {
    let state = Arc::new(RwLock::new(SubagentState::new()));
    let cancel = CancellationToken::new();
    (
        WorkerDrive {
            state: Arc::clone(&state),
            tx_events: None,
            subagent_id: 5,
            label: "worker".into(),
            model: "provider/model-b".into(),
            timeout_secs,
            run_clock: None,
            cancel: cancel.clone(),
            task: "task".into(),
            first_request: None,
            lease: None,
            lease_binds_rounds: false,
            shape: crate::tools::subagent::drive::DriveShape::Reactive,
        },
        state,
        cancel,
    )
}

fn done() -> StreamEvent {
    StreamEvent::Session(SessionEvent::Done)
}

fn history_event(messages: Vec<SharedMessage>) -> StreamEvent {
    StreamEvent::Session(SessionEvent::MessageHistory(messages))
}

/// The record a resume of this location would select.
fn selected_record(r: &WriteRoot) -> (RecordKind, WorkerRecord) {
    let loaded = record_file::load(&r.location(RecordKind::Final)).unwrap();
    match resolve(ResolveInput {
        head: loaded.head,
        final_record: loaded.final_record,
        writer: WriterState::Quiescent,
        expect: Expectations {
            handle_id: "sa_5".into(),
            run_id: None,
            binding: None,
        },
    }) {
        Resolution::Authoritative { kind, record, .. } => (kind, *record),
        other => panic!("expected an authoritative record, got {other:?}"),
    }
}

/// What a resume of this location would select: kind, generation and the
/// last message's content.
fn selected(r: &WriteRoot) -> (RecordKind, u64, String) {
    let (kind, record) = selected_record(r);
    (
        kind,
        record.generation,
        record.history.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .to_owned(),
    )
}

fn spawn_log() -> Vec<String> {
    vec!["write".into(), format!("rename {HEAD}"), "sync".into()]
}

// ── Templates ─────────────────────────────────────────────────────────────

#[test]
fn p1_a_start_template_is_a_valid_spawn_head() {
    let id = RunId::random();
    let t = template(&id);
    assert_eq!(t.kind, RecordKind::Head);
    assert_eq!(t.generation, 0);
    assert_eq!(t.window_ordinal, 1);
    assert_eq!(t.rollovers_this_run, 0);
    assert!(t.windows.is_empty() && t.inherited_windows.is_empty());
    assert!(t.predecessor.is_none());
    assert_eq!(t.history, vec![user("task")]);
    t.validate(RecordKind::Head).unwrap();
    // A fresh run's history holds no context marker.
    let marked = msgs(vec![marker_message(&arch(1), 2), user("task")]);
    assert!(start_template(id, "sa_5", offline_binding(), meta(), &marked).is_err());
}

#[test]
fn p2_a_resume_template_continues_the_predecessor_window() {
    let pred_run = RunId::random();
    let pred = record(&pred_run, offline_binding(), vec![], &[arch(1), arch(2)], None);
    let lineage = successor_lineage(&pred, RecordKind::Final).unwrap();
    let opening = resume_messages(pred.history.clone(), "continue");
    let id = RunId::random();
    let t = resume_template(
        id.clone(),
        "sa_6",
        offline_binding(),
        meta(),
        lineage.clone(),
        &opening,
    )
    .unwrap();
    t.validate(RecordKind::Head).unwrap();
    assert_eq!(t.generation, 0);
    assert_eq!(t.inherited_windows.len(), 2);
    assert!(t.windows.is_empty());
    assert_eq!(t.window_ordinal, 3);
    assert_eq!(t.predecessor.as_ref().unwrap().run_id, pred_run);
    // A shortened history must keep its current marker.
    let unmarked = msgs(vec![user("continue")]);
    assert!(
        resume_template(id, "sa_6", offline_binding(), meta(), lineage, &unmarked).is_err()
    );
}

// ── Claiming and the spawn head ───────────────────────────────────────────

#[tokio::test]
async fn p3_begin_publishes_the_spawn_head_first() {
    let r = write_root();
    let (fs, _id, p) = begun(&r, None).await;
    assert_eq!(fs.log(), spawn_log());
    assert_eq!(p.state(), PublicationState::Committed(0));
    assert!(r.dir.join("sa_5.lock").exists(), "the run lock is held");
    assert_eq!(selected(&r), (RecordKind::Head, 0, "task".into()));
}

#[tokio::test]
async fn p4_a_claim_that_cannot_own_the_run_takes_nothing() {
    let r = write_root();
    drop(r.open());
    // Pruning holds the lifecycle lock past the startup wait.
    let pruning = match try_lifecycle_exclusive(&r.root) {
        LifecycleAttempt::Held(guard) => guard,
        other => panic!("expected the lifecycle lock, got {other:?}"),
    };
    let id = RunId::random();
    let busy = claim(
        r.root.clone(),
        PROCESS_DIR.into(),
        "sa_5".into(),
        id.clone(),
        Duration::from_millis(100),
        CancellationToken::new(),
    )
    .await
    .map(drop);
    assert_eq!(busy, Err(ClaimError::Owner(OwnershipError::LifecycleBusy)));
    assert!(!r.dir.join("sa_5.lock").exists(), "no run lock was taken");
    // A cancelled claim stops waiting at once.
    let cancel = CancellationToken::new();
    cancel.cancel();
    let started = std::time::Instant::now();
    let cancelled = claim(
        r.root.clone(),
        PROCESS_DIR.into(),
        "sa_5".into(),
        id.clone(),
        Duration::from_secs(60),
        cancel,
    )
    .await
    .map(drop);
    assert_eq!(cancelled, Err(ClaimError::Owner(OwnershipError::Cancelled)));
    assert!(started.elapsed() < Duration::from_secs(10));
    drop(pruning);
    // The run is owned once; another run of the same handle finds its lock.
    let first = claim_run(&r, &id).await.unwrap();
    assert_eq!(
        claim_run(&r, &id).await.map(drop),
        Err(ClaimError::Owner(OwnershipError::AlreadyOwned))
    );
    assert_eq!(
        claim_run(&r, &RunId::random()).await.map(drop),
        Err(ClaimError::Owner(OwnershipError::LockHeld))
    );
    drop(first);
    assert!(claim_run(&r, &id).await.is_ok(), "released with its owner");
}

#[tokio::test]
async fn p5_an_unpublished_spawn_head_runs_with_rollover_off() {
    let r = write_root();
    let id = RunId::random();
    let (dir, owner) = claim_run(&r, &id).await.unwrap();
    let fs = FaultFs::new(dir);
    fs.inject(Op::WriteTemp, "", Fault::Fail);
    assert!(matches!(
        WorkerPersistence::begin(fs.clone(), owner, template(&id), None).await,
        Begin::Off(_)
    ));
    assert!(!r.dir.join(HEAD).exists(), "nothing reached its name");
    assert!(claim_run(&r, &id).await.is_ok(), "ownership was released");
}

#[tokio::test]
async fn p6_an_uncertain_spawn_head_is_decided_before_the_run_starts() {
    // Renamed, then the rename reported failure: adopted once republished.
    let r = write_root();
    let id = RunId::random();
    let (dir, owner) = claim_run(&r, &id).await.unwrap();
    let fs = FaultFs::new(dir);
    fs.inject(Op::Rename, HEAD, Fault::DoThenFail);
    match WorkerPersistence::begin(fs.clone(), owner, template(&id), None).await {
        Begin::On(p) => assert_eq!(p.state(), PublicationState::Committed(0)),
        Begin::Off(why) | Begin::Refused(why) => panic!("expected rollover on: {why}"),
    }
    assert_eq!(selected(&r), (RecordKind::Head, 0, "task".into()));

    // Never renamed: nothing of it is on disk, so the run goes on without.
    let r = write_root();
    let id = RunId::random();
    let (dir, owner) = claim_run(&r, &id).await.unwrap();
    let fs = FaultFs::new(dir);
    fs.inject(Op::Rename, HEAD, Fault::Fail);
    assert!(matches!(
        WorkerPersistence::begin(fs.clone(), owner, template(&id), None).await,
        Begin::Off(_)
    ));
    assert!(!r.dir.join(HEAD).exists());
    assert!(claim_run(&r, &id).await.is_ok());

    // The disk holds a head that is neither: the run must not start.
    let r = write_root();
    let id = RunId::random();
    let (dir, owner) = claim_run(&r, &id).await.unwrap();
    let foreign = serde_json::to_vec(&head(&RunId::random(), 0, "foreign")).unwrap();
    private_file(&r.dir.join(HEAD), &foreign);
    let fs = FaultFs::new(dir);
    fs.inject(Op::Rename, HEAD, Fault::Fail);
    assert!(matches!(
        WorkerPersistence::begin(fs.clone(), owner, template(&id), None).await,
        Begin::Refused(_)
    ));
}

// ── Rollover heads ────────────────────────────────────────────────────────

#[tokio::test]
async fn f1_a_head_is_acknowledged_only_once_durable() {
    let r = write_root();
    let (fs, id, mut p) = begun(&r, None).await;
    let (g, fault) = gate();
    fs.inject(Op::Rename, HEAD, fault);
    let next = successor(&arch(1), 2, "window two");
    let (event, mut rx) = checkpoint(worker_logical_id(&id), next.clone());
    let mut later = next.clone();
    later.push(Arc::new(assistant("after")));
    let go_on = Arc::new(tokio::sync::Notify::new());
    let go_on_s = Arc::clone(&go_on);
    let later_s = later.clone();
    // The runtime sends nothing more until the receipt is answered.
    let stream = futures::stream::iter(vec![event])
        .chain(futures::stream::once(async move {
            go_on_s.notified().await;
            history_event(later_s)
        }))
        .chain(futures::stream::iter(vec![done()]));
    let (d, state, _cancel) = drive(0);
    let control = async {
        g.reached.await.unwrap();
        assert!(
            matches!(rx.try_recv(), Err(TryRecvError::Empty)),
            "no acknowledgement before the head is durable"
        );
        assert_eq!(selected(&r).1, 0, "still the spawn head");
        g.open.send(()).unwrap();
        assert_eq!((&mut rx).await.unwrap(), Ok(()));
        assert_eq!(
            selected(&r),
            (RecordKind::Head, 1, "window two".into()),
            "durable before the acknowledgement"
        );
        go_on.notify_one();
    };
    let (result, ()) = tokio::join!(d.run_with(Box::pin(stream), Some(&mut p)), control);
    assert!(!result.unwrap().timed_out);
    assert_eq!(p.state(), PublicationState::Committed(1));
    let latest = state.read().unwrap().history.clone().unwrap();
    assert_eq!(latest, later);

    let t = p.finish("completed", "done", Some(&latest)).await;
    assert_eq!(t.archive_error, None);
    assert_eq!(t.archive_path, Some(r.dir.join(FINAL)));
    assert_eq!(selected(&r), (RecordKind::Final, 1, "after".into()));
    assert!(!r.dir.join(HEAD).exists(), "head removed after the final");
}

#[tokio::test]
async fn f2_a_refused_checkpoint_publishes_nothing() {
    let r = write_root();
    let (fs, id, mut p) = begun(&r, None).await;
    let own = worker_logical_id(&id);
    let bad_archive = msgs(vec![json!({
        "role": "user",
        "_synaps_context": {"schema": MARKER, "archive": "not-an-archive", "window": 2},
        "content": "x"
    })]);
    let cases: Vec<(&str, String, Vec<SharedMessage>)> = vec![
        (
            "another run's logical id",
            worker_logical_id(&RunId::random()),
            successor(&arch(3), 2, "x"),
        ),
        ("no marker", own.clone(), msgs(vec![user("x")])),
        (
            "two markers",
            own.clone(),
            msgs(vec![
                marker_message(&arch(3), 2),
                marker_message(&arch(4), 2),
            ]),
        ),
        ("a skipped window", own.clone(), successor(&arch(3), 3, "x")),
        ("the current window", own.clone(), successor(&arch(3), 1, "x")),
        ("a malformed archive id", own.clone(), bad_archive),
    ];
    for (why, session_id, messages) in cases {
        let (receipt, rx) = ContextHeadReceipt::channel();
        let step = p
            .on_head(&session_id, &messages, &receipt, &CancellationToken::new())
            .await;
        assert!(matches!(step, HeadStep::Refused(_)), "{why}: {step:?}");
        assert!(rx.await.unwrap().is_err(), "{why}");
        assert_eq!(fs.log(), spawn_log(), "{why}: nothing written");
        assert_eq!(p.state(), PublicationState::Committed(0), "{why}");
    }
    // An unstarted checkpoint after cancellation is refused too.
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (receipt, rx) = ContextHeadReceipt::channel();
    let step = p
        .on_head(&own, &successor(&arch(3), 2, "x"), &receipt, &cancel)
        .await;
    assert!(matches!(step, HeadStep::Refused(_)), "{step:?}");
    assert!(rx.await.unwrap().is_err());
    assert_eq!(fs.log(), spawn_log());
}

#[tokio::test]
async fn f3_a_pre_publication_failure_finalizes_the_latest_history_at_g() {
    let r = write_root();
    let (fs, id, mut p) = begun(&r, None).await;
    fs.inject(Op::WriteTemp, "", Fault::Fail);
    let (receipt, rx) = ContextHeadReceipt::channel();
    let step = p
        .on_head(
            &worker_logical_id(&id),
            &successor(&arch(1), 2, "never"),
            &receipt,
            &CancellationToken::new(),
        )
        .await;
    assert!(matches!(step, HeadStep::Refused(_)), "{step:?}");
    assert!(rx.await.unwrap().is_err());
    assert_eq!(
        p.state(),
        PublicationState::PrePublicationFailure {
            committed: Some(0),
            attempted: 1
        }
    );
    // Rounds completed after the spawn head are not lost.
    let latest = msgs(vec![user("task"), assistant("round one"), assistant("round two")]);
    let t = p.finish("failed", "partial", Some(&latest)).await;
    assert_eq!(t.archive_error, None);
    assert_eq!(selected(&r), (RecordKind::Final, 0, "round two".into()));
}

#[tokio::test]
async fn f4_an_uncertain_head_is_reconciled_before_the_final() {
    let r = write_root();
    let (fs, id, mut p) = begun(&r, None).await;
    fs.inject(Op::Rename, HEAD, Fault::DoThenFail);
    let (receipt, rx) = ContextHeadReceipt::channel();
    let step = p
        .on_head(
            &worker_logical_id(&id),
            &successor(&arch(1), 2, "window two"),
            &receipt,
            &CancellationToken::new(),
        )
        .await;
    assert!(matches!(step, HeadStep::Refused(_)), "{step:?}");
    assert!(rx.await.unwrap().is_err(), "uncertain is never acknowledged");
    assert_eq!(p.state(), PublicationState::Uncertain { attempted: 1 });
    // The runtime never took the successor: its history is the old window,
    // which no longer matches the adopted head's lineage.
    let old = msgs(vec![user("task"), assistant("old window")]);
    let t = p.finish("failed", "stopped", Some(&old)).await;
    assert_eq!(t.archive_error, None);
    assert_eq!(selected(&r), (RecordKind::Final, 1, "window two".into()));
}

#[tokio::test]
async fn f5_a_lost_receipt_keeps_the_committed_head() {
    let r = write_root();
    let (fs, id, mut p) = begun(&r, None).await;
    fs.inject(Op::Rename, HEAD, Fault::Panic);
    let (receipt, rx) = ContextHeadReceipt::channel();
    let step = p
        .on_head(
            &worker_logical_id(&id),
            &successor(&arch(1), 2, "lost"),
            &receipt,
            &CancellationToken::new(),
        )
        .await;
    assert!(matches!(step, HeadStep::Refused(_)), "{step:?}");
    assert!(rx.await.unwrap().is_err());
    assert_eq!(p.state(), PublicationState::Uncertain { attempted: 1 });
    let latest = msgs(vec![user("task"), assistant("round one")]);
    let t = p.finish("failed", "stopped", Some(&latest)).await;
    assert_eq!(t.archive_error, None);
    // Built from the kept head, not from evidence past an uncertain write.
    assert_eq!(selected(&r), (RecordKind::Final, 0, "task".into()));
    assert!(r.dir.join("sa_5.report.json").exists());
}

#[tokio::test]
async fn f6_a_deadline_during_publication_takes_the_barrier() {
    let r = write_root();
    // Room for the spawn head; the gated rollover head then outlasts it.
    let deadline = Instant::now() + Duration::from_secs(1);
    let (fs, id, mut p) = begun(&r, Some(deadline)).await;
    assert_eq!(p.deadline(), Some(deadline));
    let (g, fault) = gate();
    fs.inject(Op::Rename, HEAD, fault);
    let (event, rx) = checkpoint(worker_logical_id(&id), successor(&arch(1), 2, "late"));
    let (d, _state, cancel) = drive(300);
    let cancel_s = cancel.clone();
    let old = msgs(vec![user("task"), assistant("old window")]);
    let old_s = old.clone();
    // Like the runtime's cancel path: the history comes once cancelled.
    let stream = futures::stream::iter(vec![event]).chain(futures::stream::once(async move {
        cancel_s.cancelled().await;
        history_event(old_s)
    }));
    let control = async {
        g.reached.await.unwrap();
        cancel.cancelled().await;
        let answered = tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .expect("answered while the write still waits")
            .unwrap();
        assert!(answered.is_err(), "never acknowledged: {answered:?}");
        assert_eq!(selected(&r).1, 0, "the late head has not landed yet");
        g.open.send(()).unwrap();
    };
    let (result, ()) = tokio::join!(d.run_with(Box::pin(stream), Some(&mut p)), control);
    assert!(result.unwrap().timed_out);
    // The barrier joined the late write before anything else ran.
    assert_eq!(p.state(), PublicationState::Committed(1));
    let t = p.finish("timed_out", "report", Some(&old)).await;
    assert_eq!(t.archive_error, None);
    assert_eq!(selected(&r), (RecordKind::Final, 1, "late".into()));
}

#[tokio::test]
async fn f7_rollover_off_fails_the_receipt_explicitly() {
    let (d, _state, _cancel) = drive(0);
    let (event, rx) = checkpoint(
        worker_logical_id(&RunId::random()),
        successor(&arch(1), 2, "x"),
    );
    d.run(Box::pin(futures::stream::iter(vec![event, done()])))
        .await
        .unwrap();
    assert_eq!(rx.await, Ok(Err("worker rollover is off".to_owned())));
}

#[tokio::test]
async fn f8_an_uncertain_final_is_retried_and_an_unpublished_one_keeps_the_head() {
    let r = write_root();
    let (fs, _id, p) = begun(&r, None).await;
    fs.inject(Op::Rename, FINAL, Fault::DoThenFail);
    let t = p.finish("completed", "done", Some(&task())).await;
    assert_eq!(t.archive_error, None);
    assert_eq!(t.archive_path, Some(r.dir.join(FINAL)));
    assert_eq!(selected(&r), (RecordKind::Final, 0, "task".into()));
    assert!(!r.dir.join(HEAD).exists());

    let r = write_root();
    let (fs, _id, p) = begun(&r, None).await;
    fs.inject(Op::WriteTemp, "", Fault::Fail);
    let t = p.finish("completed", "done", Some(&task())).await;
    assert_eq!(t.archive_path, None);
    assert!(t.archive_error.is_some());
    // The head stays: the run's crash checkpoint.
    assert_eq!(selected(&r), (RecordKind::Head, 0, "task".into()));
}

#[tokio::test]
async fn f9_an_unstarted_checkpoint_after_the_deadline_is_refused() {
    let r = write_root();
    let deadline = Instant::now() + Duration::from_millis(500);
    let (fs, id, mut p) = begun(&r, Some(deadline)).await;
    tokio::time::sleep_until(deadline + Duration::from_millis(20)).await;
    let (receipt, rx) = ContextHeadReceipt::channel();
    let step = p
        .on_head(
            &worker_logical_id(&id),
            &successor(&arch(1), 2, "x"),
            &receipt,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(step, HeadStep::DeadlineBarrier);
    assert!(rx.await.unwrap().is_err());
    assert_eq!(fs.log(), spawn_log(), "nothing started");
    assert_eq!(p.state(), PublicationState::Committed(0));
}

// ── D: cancellation and the deadline at every stage of a rollover (§5) ────

/// As `subagent_cancel` does: the request, then the worker's token.
fn cancel_like_subagent_cancel(state: &RwLock<SubagentState>, cancel: &CancellationToken) {
    state.write().unwrap().cancel_requested = true;
    cancel.cancel();
}

/// The runtime stand-in: `events`, then, once cancelled, `history` (the
/// runtime's cancel path publishes its history and ends).
fn then_on_cancel(
    events: Vec<StreamEvent>,
    cancel: &CancellationToken,
    history: Vec<SharedMessage>,
) -> crate::tools::subagent::drive::WorkerStream {
    let cancel = cancel.clone();
    Box::pin(
        futures::stream::iter(events).chain(futures::stream::once(async move {
            cancel.cancelled().await;
            history_event(history)
        })),
    )
}

fn deadline_of(timeout_secs: u64) -> Option<Instant> {
    (timeout_secs > 0).then(|| Instant::now() + Duration::from_secs(timeout_secs))
}

fn old_window() -> Vec<SharedMessage> {
    msgs(vec![user("task"), assistant("old window")])
}

fn usage(input: u64, output: u64) -> StreamEvent {
    StreamEvent::Session(SessionEvent::Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_creation_5m: None,
        cache_creation_1h: None,
        model: None,
    })
}

/// D: a cancel while the runtime seals or summarizes the window: no
/// checkpoint reaches the drive, nothing more is written, and the final
/// keeps the committed head with the latest history (timeout zero too).
#[tokio::test]
async fn d7_a_cancel_before_the_checkpoint_writes_nothing_more() {
    for timeout in [0, 300] {
        let r = write_root();
        let (fs, _id, mut p) = begun(&r, deadline_of(timeout)).await;
        let (d, state, cancel) = drive(timeout);
        cancel_like_subagent_cancel(&state, &cancel);
        let stream = then_on_cancel(vec![], &cancel, old_window());
        assert!(!d.run_with(stream, Some(&mut p)).await.unwrap().timed_out);
        assert_eq!(fs.log(), spawn_log(), "timeout {timeout}");
        let t = p.finish("cancelled", "stopped", Some(&old_window())).await;
        assert_eq!(t.archive_error, None);
        assert_eq!(selected(&r), (RecordKind::Final, 0, "old window".into()));
    }
}

/// D: a checkpoint already queued when the cancel lands is refused before
/// anything is written (timeout zero too).
#[tokio::test]
async fn d8_a_checkpoint_queued_when_the_cancel_lands_is_refused() {
    for timeout in [0, 300] {
        let r = write_root();
        let (fs, id, mut p) = begun(&r, deadline_of(timeout)).await;
        let (event, rx) = checkpoint(worker_logical_id(&id), successor(&arch(1), 2, "never"));
        let (d, state, cancel) = drive(timeout);
        cancel_like_subagent_cancel(&state, &cancel);
        let stream = then_on_cancel(vec![event], &cancel, old_window());
        d.run_with(stream, Some(&mut p)).await.unwrap();
        let answer = rx.await.unwrap();
        assert!(
            answer.as_ref().is_err_and(|e| e.contains("cancelled")),
            "{answer:?}"
        );
        assert_eq!(fs.log(), spawn_log(), "timeout {timeout}");
        let t = p.finish("cancelled", "stopped", Some(&old_window())).await;
        assert_eq!(t.archive_error, None);
        assert_eq!(selected(&r), (RecordKind::Final, 0, "old window".into()));
    }
}

/// D: a cancel while a started save is blocked (before its rename) or
/// after its rename (before the directory sync) is never raced: the
/// receipt is refused while the write still waits, the barrier joins the
/// write and records its outcome, and the run stops. With no deadline
/// (timeout zero) as with one.
#[tokio::test]
async fn d9_a_cancel_during_a_started_save_waits_on_its_barrier() {
    for (op, timeout) in [
        (Op::WriteTemp, 0),
        (Op::WriteTemp, 300),
        (Op::SyncDir, 0),
        (Op::SyncDir, 300),
    ] {
        let r = write_root();
        let (fs, id, mut p) = begun(&r, deadline_of(timeout)).await;
        let (g, fault) = gate();
        fs.inject(op, "", fault);
        let (event, rx) = checkpoint(worker_logical_id(&id), successor(&arch(1), 2, "late"));
        let (d, state, cancel) = drive(timeout);
        let stream = then_on_cancel(vec![event], &cancel, old_window());
        let control = async {
            g.reached.await.unwrap();
            cancel_like_subagent_cancel(&state, &cancel);
            let answered = tokio::time::timeout(Duration::from_secs(10), rx)
                .await
                .expect("answered while the write still waits")
                .unwrap();
            assert!(answered.is_err(), "never acknowledged: {answered:?}");
            g.open.send(()).unwrap();
        };
        let (result, ()) = tokio::join!(d.run_with(stream, Some(&mut p)), control);
        assert!(!result.unwrap().timed_out, "{op:?} timeout {timeout}");
        // The barrier joined the started write and recorded its outcome.
        assert_eq!(p.state(), PublicationState::Committed(1), "{op:?} timeout {timeout}");
        let t = p.finish("cancelled", "stopped", Some(&old_window())).await;
        assert_eq!(t.archive_error, None);
        assert_eq!(selected(&r), (RecordKind::Final, 1, "late".into()));
    }
}

/// D: a cancel after the acknowledgement: the successor's head is durable,
/// the runtime stops before its first request and publishes the new
/// window's history, which the final carries (timeout zero too).
#[tokio::test]
async fn d10_a_cancel_after_the_acknowledgement_finalizes_the_new_window() {
    for timeout in [0, 300] {
        let r = write_root();
        let (_fs, id, mut p) = begun(&r, deadline_of(timeout)).await;
        let next = successor(&arch(1), 2, "window two");
        let (event, rx) = checkpoint(worker_logical_id(&id), next.clone());
        let (d, state, cancel) = drive(timeout);
        let stream = then_on_cancel(vec![event], &cancel, next.clone());
        let control = async {
            assert_eq!(rx.await.unwrap(), Ok(()));
            cancel_like_subagent_cancel(&state, &cancel);
        };
        let (result, ()) = tokio::join!(d.run_with(stream, Some(&mut p)), control);
        assert!(!result.unwrap().timed_out);
        let t = p.finish("cancelled", "stopped", Some(&next)).await;
        assert_eq!(t.archive_error, None);
        assert_eq!(selected(&r), (RecordKind::Final, 1, "window two".into()));
    }
}

/// D: past the deadline the drive cancels and drains the worker. The usage
/// the runtime reports while draining is booked, and a checkpoint it asks
/// for then is refused before anything is written.
#[tokio::test]
async fn d11_a_deadline_drain_books_usage_and_refuses_a_late_checkpoint() {
    let r = write_root();
    let (fs, id, mut p) = begun(&r, deadline_of(1)).await;
    let (event, rx) = checkpoint(worker_logical_id(&id), successor(&arch(1), 2, "late"));
    let (d, _state, cancel) = drive(300);
    let drained = vec![usage(1_000, 50), event, history_event(old_window())];
    let c = cancel.clone();
    let stream = futures::stream::iter(vec![usage(10, 5)]).chain(
        futures::stream::once(async move { c.cancelled().await })
            .flat_map(move |()| futures::stream::iter(drained.clone())),
    );
    let result = d.run_with(Box::pin(stream), Some(&mut p)).await.unwrap();
    assert!(result.timed_out);
    assert_eq!((result.input_tokens, result.output_tokens), (1_010, 55));
    let answer = rx.await.expect("answered explicitly");
    assert!(
        answer.as_ref().is_err_and(|e| e.contains("deadline")),
        "{answer:?}"
    );
    assert_eq!(fs.log(), spawn_log(), "nothing started after the deadline");

    // A v1 run (no records) books its drained usage the same way.
    let (d, _state, cancel) = drive(1);
    let c = cancel.clone();
    let stream = futures::stream::iter(vec![usage(10, 5)]).chain(
        futures::stream::once(async move { c.cancelled().await })
            .flat_map(|()| futures::stream::iter(vec![usage(1_000, 50)])),
    );
    let result = d.run(Box::pin(stream)).await.unwrap();
    assert!(result.timed_out);
    assert_eq!((result.input_tokens, result.output_tokens), (1_010, 55));
}

// ── The run's request ledger (slice 6) ────────────────────────────────────

use crate::tools::subagent::ledger::{RequestLedgerHandle, UsageReports};
use crate::tools::subagent::record::{Amount, MeasuredUsage, RequestId, RequestPurpose};

/// One attempt on `ledger` that reports `input`/`output` tokens and ends.
fn settled(ledger: &RequestLedgerHandle, input: u64, output: u64) -> RequestId {
    let mut attempt = ledger.reserve(1, UsageReports::Cumulative).unwrap();
    attempt.observe(MeasuredUsage {
        input_tokens: input,
        output_tokens: output,
        ..MeasuredUsage::default()
    });
    let id = attempt.request_id().clone();
    attempt.finish();
    id
}

/// The amount and purpose `record`'s ledger holds for `id`.
fn booked(record: &WorkerRecord, id: &RequestId) -> (Amount, RequestPurpose) {
    let e = record
        .ledger
        .entries()
        .find(|e| &e.request_id == id)
        .unwrap_or_else(|| panic!("{} is not in the record's ledger", id.as_str()));
    (e.amount.clone(), e.purpose)
}

fn measured(input: u64, output: u64) -> Amount {
    Amount::Measured {
        usage: MeasuredUsage {
            input_tokens: input,
            output_tokens: output,
            ..MeasuredUsage::default()
        },
    }
}

fn provider_failure() -> StreamEvent {
    StreamEvent::Session(SessionEvent::Error(agent_core::TurnError::budget(
        agent_core::BudgetDimension::ProviderRounds,
    )))
}

/// L: the run's ledger (the worker's model, conversation) is snapshotted
/// into every head and into its final, open liabilities and a same-model
/// summary's requests included; the spawn head's is empty.
#[tokio::test]
async fn l10_heads_and_the_final_snapshot_the_runs_ledger() {
    let r = write_root();
    let (_fs, id, mut p) = begun(&r, None).await;
    assert!(selected_record(&r).1.ledger.is_empty(), "no request yet");
    let ledger = p.ledger().clone();
    assert_eq!(
        (ledger.model(), ledger.purpose()),
        ("provider/model-b", RequestPurpose::Conversation)
    );
    let first = settled(&ledger, 10, 5);
    let open = ledger.reserve(20, UsageReports::Cumulative).unwrap();
    let open_id = open.request_id().clone();
    drop(open);

    let next = successor(&arch(1), 2, "window two");
    let (event, rx) = checkpoint(worker_logical_id(&id), next.clone());
    let stream = futures::stream::iter(vec![event, history_event(next.clone()), done()]);
    let (d, _state, _cancel) = drive(0);
    d.run_with(Box::pin(stream), Some(&mut p)).await.unwrap();
    assert_eq!(rx.await.unwrap(), Ok(()));
    let (kind, head) = selected_record(&r);
    assert_eq!((kind, head.generation), (RecordKind::Head, 1));
    assert_eq!(head.ledger, ledger.snapshot());
    assert_eq!(head.ledger.len(), 2);

    let summary = ledger.for_requests("provider/model-b", RequestPurpose::RolloverSummary);
    let summarized = settled(&summary, 30, 3);
    let t = p.finish("completed", "done", Some(&next)).await;
    assert_eq!(t.archive_error, None);
    let (kind, fin) = selected_record(&r);
    assert_eq!(kind, RecordKind::Final);
    assert_eq!(fin.ledger, ledger.snapshot());
    assert_eq!(booked(&fin, &first), (measured(10, 5), RequestPurpose::Conversation));
    assert_eq!(
        booked(&fin, &open_id),
        (
            Amount::Liability {
                estimate_micro_usd: 20
            },
            RequestPurpose::Conversation
        ),
        "unknown spend is kept"
    );
    assert_eq!(
        booked(&fin, &summarized),
        (measured(30, 3), RequestPurpose::RolloverSummary)
    );
}

/// L: on a provider error a v2 run drains its stream (bounded) before it
/// finalizes, so what settles after the error and the runtime's last
/// history reach the final. A v1 run still ends at the error, as before.
#[tokio::test]
async fn l11_an_error_drains_a_v2_run_before_its_final() {
    let r = write_root();
    let (_fs, _id, mut p) = begun(&r, None).await;
    let ledger = p.ledger().clone();
    let latest = msgs(vec![user("task"), assistant("partial")]);
    let after_error = |ledger: Option<RequestLedgerHandle>, polled: Arc<std::sync::atomic::AtomicBool>| {
        let latest = latest.clone();
        futures::stream::iter(vec![provider_failure()]).chain(futures::stream::once(async move {
            polled.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(ledger) = &ledger {
                settled(ledger, 40, 4);
            }
            history_event(latest)
        }))
    };
    let polled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (d, state, _cancel) = drive(0);
    let err = d
        .run_with(
            Box::pin(after_error(Some(ledger.clone()), Arc::clone(&polled))),
            Some(&mut p),
        )
        .await
        .unwrap_err();
    assert!(err.contains("provider request failed"), "{err}");
    assert!(polled.load(std::sync::atomic::Ordering::SeqCst), "drained");
    let history = state.read().unwrap().history.clone();
    assert_eq!(history.as_deref(), Some(latest.as_slice()));
    let t = p.finish("failed", "", history.as_deref()).await;
    assert_eq!(t.archive_error, None);
    let (_, fin) = selected_record(&r);
    assert_eq!(fin.ledger.len(), 1);
    assert_eq!(fin.ledger, ledger.snapshot());
    assert!(
        fin.ledger
            .entries()
            .all(|e| e.amount == measured(40, 4)),
        "the attempt that settled after the error"
    );
    assert_eq!(fin.history.last().unwrap()["content"], "partial");

    // A v1 run (no records) ends at the error without reading on.
    let polled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (d, _state, _cancel) = drive(0);
    let err = d
        .run(Box::pin(after_error(None, Arc::clone(&polled))))
        .await
        .unwrap_err();
    assert!(err.contains("provider request failed"), "{err}");
    assert!(!polled.load(std::sync::atomic::Ordering::SeqCst), "v1 unchanged");
}

/// L: producer drop. A runtime that goes away mid-attempt (a panic or a
/// dropped stream) leaves that attempt open in the final: unknown spend.
#[tokio::test]
async fn l12_a_producer_dropped_mid_attempt_leaves_its_spend_open_in_the_final() {
    let r = write_root();
    let (_fs, _id, mut p) = begun(&r, None).await;
    let ledger = p.ledger().clone();
    let producer = ledger.clone();
    let stream = futures::stream::once(async move {
        let mut attempt = producer.reserve(55, UsageReports::Cumulative).unwrap();
        attempt.observe(MeasuredUsage {
            input_tokens: 9,
            ..MeasuredUsage::default()
        });
        drop(attempt);
        history_event(old_window())
    });
    let (d, state, _cancel) = drive(0);
    d.run_with(Box::pin(stream), Some(&mut p)).await.unwrap();
    let history = state.read().unwrap().history.clone();
    p.finish("completed", "", history.as_deref()).await;
    let (_, fin) = selected_record(&r);
    assert_eq!(fin.ledger.len(), 1);
    assert!(fin.ledger.entries().all(|e| e.amount
        == Amount::Liability {
            estimate_micro_usd: 55
        }));
}

/// L: drain. Past the deadline the drive cancels and drains the worker;
/// an attempt that settles while it drains is in the final. (The deadline
/// has passed when the drive starts: no wall clock decides.)
#[tokio::test]
async fn l13_an_attempt_settled_during_the_deadline_drain_is_in_the_final() {
    let r = write_root();
    let (_fs, _id, mut p) = begun(&r, None).await;
    let ledger = p.ledger().clone();
    let (d, state, cancel) = drive(0);
    let c = cancel.clone();
    let producer = ledger.clone();
    let stream = futures::stream::once(async move {
        c.cancelled().await;
        settled(&producer, 70, 7);
        history_event(old_window())
    });
    let result = d
        .run_until(Box::pin(stream), Some(&mut p), Some(Instant::now()))
        .await
        .unwrap();
    assert!(result.timed_out);
    let history = state.read().unwrap().history.clone();
    p.finish("timed_out", &result.text, history.as_deref()).await;
    let (_, fin) = selected_record(&r);
    assert_eq!(fin.ledger.len(), 1);
    assert!(fin.ledger.entries().all(|e| e.amount == measured(70, 7)));
}

// ── Strict r1 B-B2: a watch worker's final carries the lease it used ─────

use crate::tools::subagent::record::WorkerWatchUsage;
use crate::tools::subagent::watch_admission::{WatchLease, WatchUsage};

/// A watch lease of 65 s and 10 rounds.
fn watch_cfg() -> crate::config::WatchConfig {
    crate::config::WatchConfig {
        lease_secs: 65,
        lease_rounds: 10,
        ..Default::default()
    }
}

/// What the worker used before this run: 5 s and 1 round, so this run's
/// lease is 60 s and 9 rounds.
fn prior() -> WatchUsage {
    WatchUsage { secs: 5, rounds: 1 }
}

/// A watch run begun as a planned watch worker's is: its spawn head
/// carries the usage it started with.
async fn begun_watch(r: &WriteRoot) -> (Arc<FaultFs>, RunId, WorkerPersistence) {
    let id = RunId::random();
    let (dir, owner) = claim_run(r, &id).await.unwrap();
    let fs = FaultFs::new(dir);
    let mut t = template(&id);
    t.meta.watch_usage = Some(prior().into());
    match WorkerPersistence::begin(fs.clone(), owner, t, None).await {
        Begin::On(p) => (fs, id, *p),
        Begin::Off(why) | Begin::Refused(why) => panic!("expected rollover on: {why}"),
    }
}

/// A watch oneshot's drive under this run's lease (its caller's limit is
/// the lease's 60 s, so a stop at the deadline is the lease's).
fn watch_drive() -> (
    WorkerDrive,
    Arc<RwLock<SubagentState>>,
    CancellationToken,
) {
    let (mut d, state, cancel) = drive(60);
    d.shape = crate::tools::subagent::drive::DriveShape::Oneshot;
    d.lease = Some((WatchLease::remaining(&watch_cfg(), prior()).unwrap(), prior()));
    (d, state, cancel)
}

/// The usage the final on disk carries, and the lease a resume from it gets
/// (what `decide_dispatch` does with a prior usage).
fn final_watch(r: &WriteRoot) -> (WorkerWatchUsage, Result<WatchLease, String>) {
    let (kind, fin) = selected_record(r);
    assert_eq!(kind, RecordKind::Final);
    let used = fin
        .meta
        .watch_usage
        .expect("a watch worker's final carries its usage");
    (used, WatchLease::remaining(&watch_cfg(), used.into()))
}

#[tokio::test]
async fn b2_a_watch_oneshot_stopped_on_its_lease_has_no_lease_left_in_its_final() {
    let r = write_root();
    let (_fs, _id, mut p) = begun_watch(&r).await;
    let (d, state, cancel) = watch_drive();
    let c = cancel.clone();
    let stream = futures::stream::once(async move {
        c.cancelled().await;
        history_event(msgs(vec![user("task"), assistant("still waiting")]))
    });
    let result = d
        .run_until(Box::pin(stream), Some(&mut p), Some(Instant::now()))
        .await
        .unwrap();
    assert!(result.timed_out);
    let history = state.read().unwrap().history.clone();
    p.finish("timed_out", &result.text, history.as_deref()).await;
    let (used, next) = final_watch(&r);
    // The 5 s before, plus this run's 60 s spent whole; 1 + 1 rounds.
    assert_eq!(used, WorkerWatchUsage { secs: 65, rounds: 2 });
    let refusal = next.unwrap_err();
    assert!(refusal.starts_with("watch lease expired"), "{refusal}");
    assert!(refusal.contains("65s of 65s"), "{refusal}");
}

#[tokio::test]
async fn b2_a_watch_run_expired_before_its_first_inference_has_no_lease_left_in_its_final() {
    let r = write_root();
    let (_fs, _id, mut p) = begun_watch(&r).await;
    let (d, state, _cancel) = watch_drive();
    let result = d.expire_before_start(Some(&mut p)).await;
    assert!(result.timed_out);
    let history = state.read().unwrap().history.clone();
    p.finish("timed_out", &result.text, history.as_deref()).await;
    let (used, next) = final_watch(&r);
    assert_eq!(used, WorkerWatchUsage { secs: 65, rounds: 1 });
    assert!(next.unwrap_err().contains("65s of 65s"));
}

#[tokio::test]
async fn b2_a_watch_run_that_ends_inside_its_lease_leaves_a_resume_only_the_rest() {
    let r = write_root();
    let (_fs, _id, mut p) = begun_watch(&r).await;
    let (d, state, _cancel) = watch_drive();
    let stream = futures::stream::iter(vec![
        history_event(msgs(vec![user("task"), assistant("not yet")])),
        done(),
    ]);
    d.run_until(Box::pin(stream), Some(&mut p), None)
        .await
        .unwrap();
    let history = state.read().unwrap().history.clone();
    p.finish("completed", "not yet", history.as_deref()).await;
    let (used, next) = final_watch(&r);
    // This run's round and its rounded-up seconds, on top of the 5 s and
    // 1 round before it.
    assert_eq!(used.rounds, 2);
    assert!((6..65).contains(&used.secs), "{used:?}");
    let lease = next.unwrap();
    assert_eq!((lease.secs, lease.rounds), (65 - used.secs, 8));
    assert!(lease.secs < 60, "never this run's fresh lease again: {used:?}");
}

#[tokio::test]
async fn b2_a_final_built_from_the_kept_head_keeps_the_runs_watch_charge() {
    let r = write_root();
    let (fs, id, mut p) = begun_watch(&r).await;
    fs.inject(Op::Rename, HEAD, Fault::Panic);
    let (receipt, rx) = ContextHeadReceipt::channel();
    let step = p
        .on_head(
            &worker_logical_id(&id),
            &successor(&arch(1), 2, "lost"),
            &receipt,
            &CancellationToken::new(),
        )
        .await;
    assert!(matches!(step, HeadStep::Refused(_)), "{step:?}");
    assert!(rx.await.unwrap().is_err());
    p.charge_watch(WatchUsage {
        secs: 65,
        rounds: 3,
    });
    let latest = msgs(vec![user("task"), assistant("round one")]);
    let t = p.finish("timed_out", "stopped", Some(&latest)).await;
    assert_eq!(t.archive_error, None);
    // Built from the kept head (generation 0), with the run's charge.
    let (kind, fin) = selected_record(&r);
    assert_eq!((kind, fin.generation), (RecordKind::Final, 0));
    assert_eq!(
        fin.meta.watch_usage,
        Some(WorkerWatchUsage {
            secs: 65,
            rounds: 3
        })
    );
}

// ── Owned history (8g) ──────────────────────────────────────────────────

/// A fresh v2 run `id`'s own grant, on a store that confines retrieval,
/// given to its persistence.
async fn grant_for(
    p: &mut WorkerPersistence,
    id: &RunId,
) -> (crate::tools::subagent::test_fixtures::Store, Arc<OwnedHistoryGrant>) {
    let store = crate::tools::subagent::test_fixtures::legacy();
    let support = crate::tools::subagent::lineage::worker_rollover_support(&store.binding)
        .await
        .expect("a legacy store confines retrieval");
    let grant = OwnedHistoryGrant::fresh(store.binding.clone(), support, id.clone());
    p.grow_into(Arc::clone(&grant));
    (store, grant)
}

/// g5 (8g): only a durable head grows the run's grant, by the window it
/// adds, and before its receipt is acknowledged. A refused head, one that
/// failed before publication and an uncertain one add nothing; a second
/// durable head adds its own window and keeps the first.
#[tokio::test]
async fn g5_the_grant_grows_only_with_a_durable_head() {
    for why in ["refused", "failed", "uncertain"] {
        let r = write_root();
        let (fs, id, mut p) = begun(&r, None).await;
        let (_store, grant) = grant_for(&mut p, &id).await;
        let session = match why {
            "refused" => worker_logical_id(&RunId::random()),
            "failed" => {
                fs.inject(Op::WriteTemp, "", Fault::Fail);
                worker_logical_id(&id)
            }
            _ => {
                fs.inject(Op::Rename, HEAD, Fault::DoThenFail);
                worker_logical_id(&id)
            }
        };
        let (receipt, rx) = ContextHeadReceipt::channel();
        let step = p
            .on_head(
                &session,
                &successor(&arch(1), 2, "window two"),
                &receipt,
                &CancellationToken::new(),
            )
            .await;
        assert!(matches!(step, HeadStep::Refused(_)), "{why}: {step:?}");
        assert!(rx.await.unwrap().is_err(), "{why}");
        assert_eq!(grant.allow(), Vec::<String>::new(), "{why}: nothing granted");
    }

    let r = write_root();
    let (fs, id, mut p) = begun(&r, None).await;
    let (_store, grant) = grant_for(&mut p, &id).await;
    let own = worker_logical_id(&id);
    let (g, fault) = gate();
    fs.inject(Op::Rename, HEAD, fault);
    let (receipt, mut rx) = ContextHeadReceipt::channel();
    let next = successor(&arch(1), 2, "window two");
    let cancel = CancellationToken::new();
    let control = async {
        g.reached.await.unwrap();
        assert_eq!(grant.allow(), Vec::<String>::new(), "not before the head is durable");
        g.open.send(()).unwrap();
        assert_eq!((&mut rx).await.unwrap(), Ok(()));
        assert_eq!(grant.allow(), [arch(1).as_str()], "granted by the acknowledgement");
    };
    let (step, ()) = tokio::join!(
        p.on_head(&own, &next, &receipt, &cancel),
        control
    );
    assert!(matches!(step, HeadStep::Committed), "{step:?}");
    let (receipt, rx) = ContextHeadReceipt::channel();
    let step = p
        .on_head(
            &own,
            &successor(&arch(2), 3, "window three"),
            &receipt,
            &CancellationToken::new(),
        )
        .await;
    assert!(matches!(step, HeadStep::Committed), "{step:?}");
    assert_eq!(rx.await.unwrap(), Ok(()));
    assert_eq!(grant.allow(), [arch(1).as_str(), arch(2).as_str()]);
    assert!(grant.allows(arch(2).as_str()) && !grant.allows(arch(3).as_str()));
}

/// g9 (8g): the window is in the grant at the moment the durable head's
/// receipt completes, not after it. The completion is what wakes the
/// runtime, which on a multi-thread executor may send its next request
/// before `on_head` returns, so the probe looks at the grant from inside the
/// acknowledgement itself.
#[tokio::test]
async fn g9_the_window_is_granted_when_the_receipt_completes() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let r = write_root();
    let (_fs, id, mut p) = begun(&r, None).await;
    let (_store, grant) = grant_for(&mut p, &id).await;
    let seen: Rc<RefCell<Vec<Vec<String>>>> = Rc::default();
    let _armed = ack_probe::set({
        let (grant, seen) = (Arc::clone(&grant), Rc::clone(&seen));
        move || seen.borrow_mut().push(grant.allow())
    });
    let own = worker_logical_id(&id);
    for (n, last) in [(1, "window two"), (2, "window three")] {
        let (receipt, rx) = ContextHeadReceipt::channel();
        let step = p
            .on_head(
                &own,
                &successor(&arch(n), u64::from(n) + 1, last),
                &receipt,
                &CancellationToken::new(),
            )
            .await;
        assert!(matches!(step, HeadStep::Committed), "{step:?}");
        assert_eq!(rx.await.unwrap(), Ok(()));
    }
    assert_eq!(
        *seen.borrow(),
        [
            vec![arch(1).as_str().to_owned()],
            vec![arch(1).as_str().to_owned(), arch(2).as_str().to_owned()],
        ],
        "each acknowledgement already sees its own window"
    );
}

/// g10 (8g): a head that would take the lineage past its 128 references is
/// refused before anything is written: its receipt fails (which stops the
/// runtime before further inference), the committed head stays, and the
/// grant is unchanged.
#[tokio::test]
async fn g10_a_head_past_the_lineage_cap_is_refused_and_grows_nothing() {
    use crate::tools::subagent::record::MAX_LINEAGE_REFS;

    // A resumed run that inherits exactly the cap: valid, at 128 refs.
    let full: Vec<ArchiveId> = (1..=MAX_LINEAGE_REFS as u8).map(arch).collect();
    let pred = record(&RunId::random(), offline_binding(), vec![], &full, None);
    let lineage = successor_lineage(&pred, RecordKind::Final).unwrap();
    let opening = resume_messages(pred.history.clone(), "continue");
    let r = write_root();
    let id = RunId::random();
    let (dir, owner) = claim_run(&r, &id).await.unwrap();
    let fs = FaultFs::new(dir);
    let t = resume_template(id.clone(), "sa_5", offline_binding(), meta(), lineage, &opening)
        .unwrap();
    assert_eq!(t.lineage_ref_count(), MAX_LINEAGE_REFS);
    let mut p = match WorkerPersistence::begin(fs.clone(), owner, t, None).await {
        Begin::On(p) => *p,
        Begin::Off(why) | Begin::Refused(why) => panic!("expected rollover on: {why}"),
    };
    // Its grant holds the admitted lineage.
    let (_store, grant) = grant_for(&mut p, &id).await;
    for a in &full {
        grant.grow(a);
    }
    let before = grant.allow();
    assert_eq!(before.len(), MAX_LINEAGE_REFS);

    let next = arch(MAX_LINEAGE_REFS as u8 + 1);
    let (receipt, rx) = ContextHeadReceipt::channel();
    let step = p
        .on_head(
            &worker_logical_id(&id),
            &successor(&next, MAX_LINEAGE_REFS as u64 + 2, "one window too many"),
            &receipt,
            &CancellationToken::new(),
        )
        .await;
    match &step {
        HeadStep::Refused(why) => assert!(why.contains("exceed the cap"), "{why}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(rx.await.unwrap().is_err(), "the runtime is stopped");
    assert_eq!(fs.log(), spawn_log(), "nothing written");
    assert_eq!(p.state(), PublicationState::Committed(0));
    assert_eq!(grant.allow(), before, "the grant is unchanged");
    assert!(!grant.allows(next.as_str()));
}

/// g11 (8g): granting a window twice keeps one entry.
#[tokio::test]
async fn g11_a_window_granted_twice_is_listed_once() {
    let r = write_root();
    let (_fs, id, mut p) = begun(&r, None).await;
    let (_store, grant) = grant_for(&mut p, &id).await;
    grant.grow(&arch(2));
    grant.grow(&arch(1));
    grant.grow(&arch(2));
    assert_eq!(grant.allow(), [arch(1).as_str(), arch(2).as_str()]);
}

/// Storage ceilings (§9): pruning's 14-day age-out never takes the
/// directory of a run that is still publishing, however old it looks; once
/// the run has ended, the same directory ages out like any other.
#[tokio::test]
async fn an_active_run_directory_survives_14_day_pruning() {
    let r = write_root();
    let (_fs, _id, p) = begun(&r, None).await;
    let old = std::time::SystemTime::now() - Duration::from_secs(30 * 24 * 3600);
    let age_out = || {
        for path in [r.dir.join(HEAD), r.dir.clone()] {
            std::fs::File::open(&path)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        crate::tools::subagent::archive::prune_older_than(
            &r.root,
            Duration::from_secs(14 * 24 * 3600),
        )
    };
    age_out();
    assert!(r.dir.exists(), "a publishing run keeps its directory");
    assert_eq!(selected(&r), (RecordKind::Head, 0, "task".into()));
    // The run ends without a final (its head is the record): the run lock
    // is released and nothing keeps the directory any more.
    drop(p);
    age_out();
    assert!(!r.dir.exists(), "an ended run's old directory ages out");
}
