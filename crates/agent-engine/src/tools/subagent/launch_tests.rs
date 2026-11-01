//! Launching a reactive worker's run (§3.2–3.3, §5; slice 4 drive wiring,
//! dark): with worker rollover off nothing of it changes; with it on, the
//! spawn head is durable before the first inference and the final after the
//! drive; whatever stops the run before its first inference starts none; and
//! no v1 archive is ever written beside a run's v2 records. Every case reads
//! the disk back the way a resume does. Run IDs are random: ownership is
//! process-wide.
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use futures::StreamExt;
use serde_json::{json, Value};

use super::*;
use crate::memory_backend::MemoryBinding;
use crate::runtime::subagent::{
    SubagentArchiveMeta, SubagentHandle, SubagentRegistry, SubagentState, SubagentStatus,
};
use crate::tools::subagent::archive::{read_archive, resume_messages};
use crate::tools::subagent::drive::{WorkerDrive, WorkerStream};
use crate::tools::subagent::ledger::{AttemptEnd, UsageReports};
use crate::tools::subagent::lineage::{successor_lineage, worker_logical_id, SuccessorLineage};
use crate::tools::subagent::persist::ROLLOVER_OFF;
use crate::tools::subagent::record::{
    Amount, ArchiveId, Binding, Candidate, FinalRecord, MeasuredUsage, RecordKind, RequestPurpose,
    RunId, WorkerMeta, WorkerRecord, WriterState,
};
use crate::tools::subagent::record_file;
use crate::tools::subagent::record_store::{
    try_lifecycle_exclusive, writer_probe, LifecycleAttempt, RecordDir, RecordFs,
};
use crate::tools::subagent::test_fixtures::{
    self as fx, gate, head, meta, private_file, record, write_root, Fault, FaultFs, Op, WriteRoot,
    PROCESS_DIR,
};
use crate::{CancellationToken, SessionEvent, SharedMessage, StreamEvent};
use agent_core::config::{MemoryBackendConfig, MemoryBackendKind};
use agent_core::core::context_head::ContextHeadReceipt;
use agent_core::memory::store::ProjectScope;

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

/// The model of the runs these tests launch: one whose route books every
/// request in a run's ledger (§6), so a run can be a v2 run.
const BOOKED_MODEL: &str = "claude-sonnet-4-6";

/// The fixture's worker, on [`BOOKED_MODEL`].
fn worker_meta() -> WorkerMeta {
    WorkerMeta {
        model: BOOKED_MODEL.into(),
        ..meta()
    }
}

fn plan(r: &WriteRoot, binding: &MemoryBinding, lineage: Option<SuccessorLineage>) -> LaunchPlan {
    LaunchPlan {
        root: r.root.clone(),
        process_dir: PROCESS_DIR.into(),
        handle_id: "sa_5".into(),
        session: Some("conv-5".into()),
        binding: binding.clone(),
        meta: worker_meta(),
        lineage,
        ledger: Default::default(),
        context: Ok(worker_context()),
        grant: None,
        // A fresh lineage armed with an empty registry: its v2 run's
        // runtime runs on the owned-history tools alone (8g).
        tools: Some(WorkerTools {
            registry: Some(crate::ToolRegistry::empty()),
            disabled: Vec::new(),
        }),
        evicted_windows: 0,
    }
}

/// The parent settings a fixture worker inherits: context management
/// `auto` with the default thresholds, resolved for [`BOOKED_MODEL`].
fn worker_context() -> crate::tools::WorkerContext {
    auto_policy().resolve(BOOKED_MODEL).expect("the defaults fit")
}

/// A parent's live settings: `auto`, default thresholds, nothing else set.
fn auto_policy() -> crate::tools::WorkerContextPolicy {
    crate::tools::WorkerContextPolicy {
        context_management: agent_core::config::ContextManagementConfig {
            mode: agent_core::config::ContextManagementMode::Auto,
            ..Default::default()
        },
        summary_model: None,
        summary_model_invalid: false,
        compaction_model: None,
        compaction_mode: Default::default(),
        compaction_exclusions: Vec::new(),
        window_override: None,
        operator_rollover: agent_core::config::ContextRollover::Inherit,
        resume_refuse_tokens: None,
        worker_target_tokens: None,
    }
}

/// A worker's drive and state as start/resume build them: launch identity
/// and the opening messages are in the state before the run.
fn drive(
    timeout_secs: u64,
    opening: &[SharedMessage],
) -> (WorkerDrive, Arc<RwLock<SubagentState>>, CancellationToken) {
    let state = Arc::new(RwLock::new(SubagentState::new()));
    {
        let mut s = state.write().unwrap();
        s.archive_meta = Some(SubagentArchiveMeta::from(meta()));
        s.history = Some(opening.to_vec());
    }
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

/// What the runtime stand-in was started with (`None`: never started).
type Started = Arc<Mutex<Option<Vec<SharedMessage>>>>;

/// A runtime stand-in that replays `events` once started, after running
/// `at_start` (a test's look at the disk at the first inference).
fn runtime(
    events: Vec<StreamEvent>,
    at_start: impl FnOnce() + 'static,
) -> (
    Started,
    impl FnOnce(Vec<SharedMessage>) -> std::future::Ready<WorkerStream>,
) {
    let started: Started = Arc::default();
    let seen = Arc::clone(&started);
    let start = move |opening: Vec<SharedMessage>| {
        at_start();
        *seen.lock().unwrap() = Some(opening);
        std::future::ready(Box::pin(futures::stream::iter(events)) as WorkerStream)
    };
    (started, start)
}

fn done() -> StreamEvent {
    StreamEvent::Session(SessionEvent::Done)
}

fn text(t: &str) -> StreamEvent {
    StreamEvent::Llm(crate::LlmEvent::Text(t.into()))
}

fn history_event(messages: Vec<SharedMessage>) -> StreamEvent {
    StreamEvent::Session(SessionEvent::MessageHistory(messages))
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

fn real(dir: RecordDir) -> Arc<dyn RecordFs> {
    Arc::new(dir)
}

/// A wrap that injects `fault` at the first `op` on `target`.
fn faulty(op: Op, target: &'static str, fault: Fault) -> impl Fn(RecordDir) -> Arc<dyn RecordFs> {
    let fault = Mutex::new(Some(fault));
    move |dir| {
        let fs = FaultFs::new(dir);
        if let Some(fault) = fault.lock().unwrap().take() {
            fs.inject(op, target, fault);
        }
        fs
    }
}

fn head_on_disk(r: &WriteRoot) -> Option<WorkerRecord> {
    match record_file::load(&r.location(RecordKind::Final)).unwrap().head {
        Candidate::Present(record) => Some(record),
        Candidate::Absent => None,
        Candidate::Unreadable(e) => panic!("unreadable head: {e}"),
    }
}

fn v2_final(r: &WriteRoot) -> WorkerRecord {
    match record_file::load(&r.location(RecordKind::Final))
        .unwrap()
        .final_record
    {
        Candidate::Present(FinalRecord::V2(record)) => *record,
        Candidate::Present(FinalRecord::V1(_)) => panic!("a v1 archive took the final's name"),
        Candidate::Absent => panic!("no final record"),
        Candidate::Unreadable(e) => panic!("unreadable final: {e}"),
    }
}

/// A memory binding that cannot confine retrieval to a lineage: an Axel
/// service that cannot even be started.
fn unsupported_binding(base: &std::path::Path) -> MemoryBinding {
    MemoryBinding::for_test(
        base.to_path_buf(),
        ProjectScope::for_root(&fx::project_dir(base, "repo")).unwrap(),
        &MemoryBackendConfig {
            kind: MemoryBackendKind::Axel,
            executable: Some(base.join("no-such-service")),
            brain: Some(base.join("brain.r8")),
            user_scope: false,
        },
    )
}

// ── Off ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn l1_without_a_plan_the_run_is_the_v1_run() {
    let r = write_root();
    let (event, rx) = checkpoint("worker-run:any".into(), task());
    let latest = msgs(vec![user("task"), assistant("done")]);
    let (started, start) = runtime(
        vec![event, text("done"), history_event(latest.clone()), done()],
        || {},
    );
    let (d, state, _cancel) = drive(0, &task());
    let result = run_worker_with(d, task(), None, start, &real).await.unwrap();
    assert_eq!(result.text, "done");
    assert_eq!(started.lock().unwrap().as_deref(), Some(task().as_slice()));
    assert_eq!(rx.await.unwrap(), Err(ROLLOVER_OFF.into()));
    let s = state.read().unwrap();
    assert!(!s.v2_records, "the finalizer keeps writing the v1 archive");
    assert_eq!(s.history.as_deref(), Some(latest.as_slice()));
    assert_eq!((&s.archive_path, &s.archive_error), (&None, &None));
    assert!(!r.dir.exists(), "nothing was created below the root");
}

#[tokio::test]
async fn l2_an_unsupported_binding_or_an_unclaimable_run_runs_with_rollover_off() {
    // The binding cannot confine retrieval: no record, no lock, no
    // directory; the receipt of a checkpoint is refused as when off.
    let r = write_root();
    let (event, rx) = checkpoint("worker-run:any".into(), task());
    let (started, start) = runtime(vec![event, done()], || {});
    let (d, state, _cancel) = drive(0, &task());
    let base = r._tmp.path().canonicalize().unwrap();
    let binding = unsupported_binding(&base);
    run_worker_with(d, task(), Some(plan(&r, &binding, None)), start, &real)
        .await
        .unwrap();
    assert!(started.lock().unwrap().is_some(), "the worker still runs");
    assert_eq!(rx.await.unwrap(), Err(ROLLOVER_OFF.into()));
    assert!(!state.read().unwrap().v2_records);
    assert!(!r.dir.exists());

    // A root that is not private refuses the record directory.
    let store = fx::legacy();
    let r = write_root();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&r.root, std::fs::Permissions::from_mode(0o757)).unwrap();
    }
    let (started, start) = runtime(vec![done()], || {});
    let (d, state, _cancel) = drive(0, &task());
    run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &real)
        .await
        .unwrap();
    assert!(started.lock().unwrap().is_some());
    assert!(!state.read().unwrap().v2_records);
    assert!(!r.dir.exists());
}

#[tokio::test]
async fn l3_a_busy_lifecycle_lock_runs_the_worker_with_rollover_off() {
    let store = fx::legacy();
    let r = write_root();
    drop(r.open());
    let pruning = match try_lifecycle_exclusive(&r.root) {
        LifecycleAttempt::Held(guard) => guard,
        other => panic!("expected the lifecycle lock, got {other:?}"),
    };
    let (started, start) = runtime(vec![done()], || {});
    let (d, state, _cancel) = drive(0, &task());
    let began = std::time::Instant::now();
    run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &real)
        .await
        .unwrap();
    // Bounded by the startup wait, then a normal start without records.
    assert!(began.elapsed() < Duration::from_secs(30));
    assert!(started.lock().unwrap().is_some(), "never a failed start");
    assert!(!state.read().unwrap().v2_records);
    assert!(!r.dir.join("sa_5.lock").exists(), "no run lock was taken");
    assert!(head_on_disk(&r).is_none());
    drop(pruning);
}

/// The lifecycle lock held as pruning holds it, so a claim waits.
fn pruning(r: &WriteRoot) -> crate::tools::subagent::record_store::LifecycleGuard {
    drop(r.open());
    match try_lifecycle_exclusive(&r.root) {
        LifecycleAttempt::Held(guard) => guard,
        other => panic!("expected the lifecycle lock, got {other:?}"),
    }
}

/// A cancel while the run waits to be claimed: it falls back to v1, starts
/// no inference and finishes through the finalizer as any v1 run.
#[tokio::test]
async fn l18_a_cancel_during_the_claim_starts_no_inference() {
    let store = fx::legacy();
    let r = write_root();
    let held = pruning(&r);
    let (started, start) = runtime(vec![done()], || {});
    let (d, state, cancel) = drive(0, &task());
    let control = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(
        run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &real),
        control
    );
    let err = result.unwrap_err();
    assert!(err.contains("cancelled before the first inference"), "{err}");
    assert!(started.lock().unwrap().is_none(), "no inference");
    assert!(!state.read().unwrap().v2_records);
    assert!(head_on_disk(&r).is_none());
    drop(held);
    state.write().unwrap().status = SubagentStatus::Failed(err);
    crate::tools::subagent::finalize::finalize_subagent_in(
        &state,
        None,
        "sa_5",
        5,
        "worker",
        std::time::Instant::now(),
        None,
        &r.dir,
    );
    let path = state.read().unwrap().archive_path.clone().unwrap();
    assert_eq!(read_archive(&path).unwrap().status, "failed");
}

/// The run deadline passes before the spawn head is published (while the
/// claim waits for the lifecycle lock, or once it got it): no inference,
/// timed out as a v1 run.
#[tokio::test]
async fn l19_a_deadline_passed_before_publication_starts_no_inference() {
    // The claim gives up (2 s startup wait) after the 1 s deadline.
    let store = fx::legacy();
    let r = write_root();
    let held = pruning(&r);
    let (started, start) = runtime(vec![done()], || {});
    let (d, state, _cancel) = drive(1, &task());
    let result = run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &real)
        .await
        .unwrap();
    assert!(result.timed_out);
    assert!(started.lock().unwrap().is_none(), "no inference");
    assert_eq!(state.read().unwrap().status, SubagentStatus::TimedOut);
    assert!(!state.read().unwrap().v2_records);
    drop(held);

    // The lock is released after the deadline: the claim succeeds and the
    // spawn head is refused unstarted (or the claim gave up: either way).
    let r = write_root();
    let held = pruning(&r);
    let (started, start) = runtime(vec![done()], || {});
    let (d, state, _cancel) = drive(1, &task());
    let control = async move {
        tokio::time::sleep(Duration::from_millis(1300)).await;
        drop(held);
    };
    let (result, ()) = tokio::join!(
        run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &real),
        control
    );
    assert!(result.unwrap().timed_out);
    assert!(started.lock().unwrap().is_none(), "no inference");
    assert!(!state.read().unwrap().v2_records);
    assert!(head_on_disk(&r).is_none());
}

/// A run that falls back to v1 keeps the run deadline taken before its
/// launch; it does not start a fresh one.
#[tokio::test]
async fn l20_a_fallback_keeps_the_original_run_deadline() {
    let r = write_root();
    let base = r._tmp.path().canonicalize().unwrap();
    let binding = unsupported_binding(&base);
    let (d, _state, cancel) = drive(3600, &task());
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = Arc::clone(&started);
    let start = move |_opening: Vec<SharedMessage>, _context: Option<WorkerStart>| {
        seen.store(true, std::sync::atomic::Ordering::SeqCst);
        // Runs until the drive cancels it.
        let stop = cancel.clone();
        std::future::ready(Box::pin(futures::stream::once(async move {
            stop.cancelled().await;
            done()
        })) as WorkerStream)
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        run_worker_until(d, task(), plan(&r, &binding, None), start, &real, Some(deadline)),
    )
    .await
    .expect("timed out at the run deadline, not after a fresh hour")
    .unwrap();
    assert!(result.timed_out);
    assert!(started.load(std::sync::atomic::Ordering::SeqCst));
}

// ── On ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn l4_a_fresh_run_publishes_its_spawn_head_before_inference_and_its_final_after() {
    let store = fx::legacy();
    let r = write_root();
    let latest = msgs(vec![user("task"), assistant("done")]);
    let location = r.location(RecordKind::Final);
    let at_start = move || {
        // Durable before the first inference: generation 0 of a fresh run.
        let loaded = record_file::load(&location).unwrap();
        let Candidate::Present(head) = loaded.head else {
            panic!("no spawn head at the first inference");
        };
        assert_eq!((head.generation, head.window_ordinal), (0, 1));
        assert!(head.predecessor.is_none());
        assert_eq!(head.history, vec![user("task")]);
        assert!(matches!(loaded.final_record, Candidate::Absent));
    };
    let (started, start) = runtime(
        vec![text("done"), history_event(latest.clone()), done()],
        at_start,
    );
    let (d, state, _cancel) = drive(0, &task());
    let result = run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &real)
        .await
        .unwrap();
    assert_eq!(result.text, "done");
    assert!(started.lock().unwrap().is_some());

    let fin = v2_final(&r);
    assert_eq!(fin.handle_id, "sa_5");
    assert_eq!(fin.generation, 0);
    assert_eq!((fin.status.as_str(), fin.output.as_str()), ("completed", "done"));
    assert_eq!(
        fin.history,
        latest.iter().map(|m| m.as_ref().clone()).collect::<Vec<_>>()
    );
    assert_eq!(fin.binding, Binding::of_memory(&store.binding).unwrap());
    assert_eq!(fin.meta, worker_meta());
    assert!(head_on_disk(&r).is_none(), "the head is removed after the final");
    assert_eq!(
        writer_probe(&r.location(RecordKind::Final)),
        WriterState::Quiescent,
        "ownership released"
    );

    let s = state.read().unwrap();
    assert!(s.v2_records);
    assert_eq!(s.archive_path, Some(r.dir.join(FINAL)));
    assert_eq!(
        s.v2_final_run.as_deref(),
        Some(fin.run_id.as_str()),
        "collect reads it as this run's final"
    );
    assert_eq!(s.archive_error, None);
    assert!(s.history.is_none(), "released once the final is durable");
}

#[tokio::test]
async fn l5_a_resumed_run_continues_its_predecessor_lineage() {
    let store = fx::legacy();
    let r = write_root();
    let pred = record(
        &RunId::random(),
        fx::current(&store.binding),
        vec![],
        &[arch(1), arch(2)],
        None,
    );
    let lineage = successor_lineage(&pred, RecordKind::Final).unwrap();
    let opening = resume_messages(pred.history.clone(), "continue");
    let (_started, start) = runtime(vec![done()], || {});
    let (d, _state, _cancel) = drive(0, &opening);
    run_worker_with(
        d,
        opening.clone(),
        Some(plan(&r, &store.binding, Some(lineage.clone()))),
        start,
        &real,
    )
    .await
    .unwrap();
    let fin = v2_final(&r);
    assert_ne!(fin.run_id, pred.run_id, "a resume is a new run");
    assert_eq!(fin.predecessor, Some(lineage.predecessor));
    assert_eq!(fin.inherited_windows, lineage.inherited_windows);
    assert_eq!(fin.window_ordinal, lineage.window_ordinal);
    assert!(fin.windows.is_empty());
    assert_eq!(
        fin.history,
        opening.iter().map(|m| m.as_ref().clone()).collect::<Vec<_>>()
    );
}

/// Storage ceilings (§9): a resume inherits its predecessor's windows by
/// reference; it seals nothing of its own until it rolls over, so it takes
/// no worker history slot however many windows it inherits.
#[tokio::test]
async fn l5b_a_resumed_lineage_takes_no_new_worker_slot() {
    let store = fx::legacy();
    let r = write_root();
    let pred_run = RunId::random();
    let own = [
        fx::seal_run(&store.binding, &pred_run, "window one").await,
        fx::seal_run(&store.binding, &pred_run, "window two").await,
    ];
    let scope = ProjectScope::for_root(&store.base.join("project-a")).unwrap();
    let segments = || {
        agent_core::context_archive::export_in(&store.base, scope.key())
            .unwrap()
            .len()
    };
    assert_eq!(segments(), 2);
    let pred = record(&pred_run, fx::current(&store.binding), vec![], &own, None);
    let lineage = successor_lineage(&pred, RecordKind::Final).unwrap();
    let opening = resume_messages(pred.history.clone(), "continue");
    let (_started, start) = runtime(vec![done()], || {});
    let (d, _state, _cancel) = drive(0, &opening);
    run_worker_with(
        d,
        opening.clone(),
        Some(plan(&r, &store.binding, Some(lineage))),
        start,
        &real,
    )
    .await
    .unwrap();
    let fin = v2_final(&r);
    assert_eq!(fin.inherited_windows.len(), 2);
    assert!(fin.windows.is_empty());
    assert_eq!(segments(), 2, "inheriting windows seals nothing");
}

#[tokio::test]
async fn l6_a_rollover_head_is_published_under_the_run_logical_id() {
    let store = fx::legacy();
    let r = write_root();
    // The runtime asks under the logical ID of the run it was given; the
    // stand-in reads it back from the spawn head at its first inference.
    let location = r.location(RecordKind::Final);
    let rx_slot: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<_>>>> = Arc::default();
    let slot = Arc::clone(&rx_slot);
    let start = move |_opening: Vec<SharedMessage>| {
        let Candidate::Present(head) = record_file::load(&location).unwrap().head else {
            panic!("no spawn head");
        };
        let next = msgs(vec![
            fx::marker_message(&arch(1), 2),
            assistant("window two"),
        ]);
        let (event, rx) = checkpoint(worker_logical_id(&head.run_id), next.clone());
        *slot.lock().unwrap() = Some(rx);
        let events = vec![event, history_event(next), done()];
        std::future::ready(Box::pin(futures::stream::iter(events)) as WorkerStream)
    };
    let (d, _state, _cancel) = drive(0, &task());
    run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &real)
        .await
        .unwrap();
    let rx = rx_slot.lock().unwrap().take().unwrap();
    assert_eq!(rx.await.unwrap(), Ok(()));
    let fin = v2_final(&r);
    assert_eq!((fin.generation, fin.window_ordinal), (1, 2));
    assert_eq!(fin.windows.len(), 1);
    assert_eq!(fin.history.last().unwrap()["content"], "window two");
}

#[tokio::test]
async fn l7_an_unpublished_spawn_head_runs_the_worker_as_v1() {
    let store = fx::legacy();
    let r = write_root();
    let (started, start) = runtime(vec![text("done"), done()], || {});
    let (d, state, _cancel) = drive(0, &task());
    let wrap = faulty(Op::WriteTemp, "", Fault::Fail);
    run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &wrap)
        .await
        .unwrap();
    assert!(started.lock().unwrap().is_some());
    assert!(!state.read().unwrap().v2_records);
    assert!(head_on_disk(&r).is_none());
    // The finalizer writes the v1 archive as it always did.
    state.write().unwrap().status = SubagentStatus::Completed;
    crate::tools::subagent::finalize::finalize_subagent_in(
        &state,
        None,
        "sa_5",
        5,
        "worker",
        std::time::Instant::now(),
        None,
        &r.dir,
    );
    let path = state.read().unwrap().archive_path.clone().unwrap();
    assert_eq!(read_archive(&path).unwrap().status, "completed");
}

#[tokio::test]
async fn l8_an_undecided_spawn_head_starts_no_inference_and_no_v1_archive() {
    let store = fx::legacy();
    let r = write_root();
    drop(r.open());
    // The disk holds a head that is neither the run's nor absent.
    let foreign = serde_json::to_vec(&head(&RunId::random(), 0, "foreign")).unwrap();
    private_file(&r.dir.join(HEAD), &foreign);
    let (started, start) = runtime(vec![done()], || {});
    let (d, state, _cancel) = drive(0, &task());
    let wrap = faulty(Op::Rename, HEAD, Fault::Fail);
    let err = run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &wrap)
        .await
        .unwrap_err();
    assert!(err.contains("nothing was started"), "{err}");
    assert!(started.lock().unwrap().is_none(), "no inference");
    assert!(state.read().unwrap().v2_records);
    state.write().unwrap().status = SubagentStatus::Failed(err);
    crate::tools::subagent::finalize::finalize_subagent_in(
        &state,
        None,
        "sa_5",
        5,
        "worker",
        std::time::Instant::now(),
        None,
        &r.dir,
    );
    let s = state.read().unwrap();
    assert!(!r.dir.join(FINAL).exists(), "no v1 archive beside the head");
    assert!(s.archive_path.is_none());
    assert!(s.archive_error.is_some());
    assert!(s.history.is_some(), "history retained in memory");
}

#[tokio::test]
async fn l9_the_deadline_or_a_cancel_during_launch_starts_no_inference() {
    // Deadline: the spawn head is held past a 1 s run deadline.
    let store = fx::legacy();
    let r = write_root();
    let (g, fault) = gate();
    let (started, start) = runtime(vec![done()], || {});
    let (d, state, _cancel) = drive(1, &task());
    let wrap = faulty(Op::Rename, HEAD, fault);
    let control = async {
        g.reached.await.unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        g.open.send(()).unwrap();
    };
    let (result, ()) = tokio::join!(
        run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &wrap),
        control
    );
    assert!(result.unwrap().timed_out);
    assert!(started.lock().unwrap().is_none(), "no inference");
    assert_eq!(state.read().unwrap().status, SubagentStatus::TimedOut);
    let fin = v2_final(&r);
    assert_eq!((fin.generation, fin.status.as_str()), (0, "timed_out"));

    // Cancel: the worker is cancelled while its spawn head is written.
    let r = write_root();
    let (g, fault) = gate();
    let (started, start) = runtime(vec![done()], || {});
    let (d, state, cancel) = drive(0, &task());
    let wrap = faulty(Op::Rename, HEAD, fault);
    let control = async {
        g.reached.await.unwrap();
        cancel.cancel();
        g.open.send(()).unwrap();
    };
    let (result, ()) = tokio::join!(
        run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &wrap),
        control
    );
    let err = result.unwrap_err();
    assert!(err.contains("cancelled before the first inference"), "{err}");
    assert!(started.lock().unwrap().is_none(), "no inference");
    assert!(state.read().unwrap().v2_records);
    let fin = v2_final(&r);
    assert_eq!(fin.generation, 0);
    assert_eq!(fin.status, "failed");
    assert!(head_on_disk(&r).is_none());
}

#[tokio::test]
async fn l10_a_cancel_requested_run_finalizes_as_cancelled() {
    let store = fx::legacy();
    let r = write_root();
    let (_started, start) = runtime(vec![text("partial"), done()], || {});
    let (d, state, _cancel) = drive(0, &task());
    state.write().unwrap().cancel_requested = true;
    run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &real)
        .await
        .unwrap();
    let fin = v2_final(&r);
    assert_eq!((fin.status.as_str(), fin.output.as_str()), ("cancelled", "partial"));
}

// ── The v1 hand-over ──────────────────────────────────────────────────────

#[tokio::test]
async fn l11_the_finalizer_and_recovery_never_write_a_v1_archive_for_a_v2_run() {
    let store = fx::legacy();
    let r = write_root();
    let (_started, start) = runtime(vec![text("done"), done()], || {});
    let (d, state, _cancel) = drive(0, &task());
    run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &real)
        .await
        .unwrap();
    let published = std::fs::read(r.dir.join(FINAL)).unwrap();
    state.write().unwrap().status = SubagentStatus::Completed;
    crate::tools::subagent::finalize::finalize_subagent_in(
        &state,
        None,
        "sa_5",
        5,
        "worker",
        std::time::Instant::now(),
        None,
        &r.dir,
    );
    assert_eq!(std::fs::read(r.dir.join(FINAL)).unwrap(), published);
    {
        let s = state.read().unwrap();
        assert!(s.finalization_ready);
        assert_eq!(s.archive_path, Some(r.dir.join(FINAL)));
        assert_eq!(s.archive_error, None);
    }

    // A v2 run whose final failed keeps its history in memory; recovery
    // never writes a v1 archive under the final's name.
    {
        let mut s = state.write().unwrap();
        s.archive_path = None;
        s.archive_error = Some("publication uncertain".into());
        s.history = Some(task());
    }
    std::fs::remove_file(r.dir.join(FINAL)).unwrap();
    crate::tools::subagent::archive::retry_failed_archive(&state, "sa_5");
    assert!(!r.dir.join(FINAL).exists());
    assert_eq!(
        state.read().unwrap().archive_error.as_deref(),
        Some("publication uncertain")
    );
}

#[test]
fn l12_a_v2_run_that_ended_without_its_final_reports_it() {
    // A panic dropped the run before `finish`: the head stays the crash
    // checkpoint and the finalizer writes nothing.
    let r = write_root();
    drop(r.open());
    let state = Arc::new(RwLock::new(SubagentState::new()));
    {
        let mut s = state.write().unwrap();
        s.archive_meta = Some(SubagentArchiveMeta::from(meta()));
        s.history = Some(task());
        s.v2_records = true;
        s.status = SubagentStatus::Failed("panic: boom".into());
    }
    crate::tools::subagent::finalize::finalize_subagent_in(
        &state,
        None,
        "sa_5",
        5,
        "worker",
        std::time::Instant::now(),
        None,
        &r.dir,
    );
    let s = state.read().unwrap();
    assert!(!r.dir.join(FINAL).exists());
    assert!(s.archive_path.is_none());
    let error = s.archive_error.clone().unwrap();
    assert!(error.contains("final record"), "{error}");
    assert!(s.history.is_some());
}

/// Train30 item 4a: a run a daemon reload stops whose final record is not
/// published (its temp write fails, as with EMFILE during the drain) keeps
/// its head as the crash checkpoint, and its state says where; the
/// finalizer warns with the error instead of saying it was archived.
#[tokio::test(flavor = "current_thread")]
async fn l22_a_reload_stop_whose_final_was_not_published_keeps_its_head_and_warns() {
    let (log, _guard) = crate::tools::test_helpers::DaemonLog::capture();
    let store = fx::legacy();
    let r = write_root();
    let records: Records = Arc::default();
    let wrap = recorded(&records);
    let inject = Arc::clone(&records);
    let (_started, start) = runtime(vec![text("partial"), done()], move || {
        // The spawn head is durable by now: the next temp write, the
        // final's, fails.
        inject
            .lock()
            .unwrap()
            .as_ref()
            .expect("the run's record directory")
            .inject(Op::WriteTemp, "", Fault::Fail);
    });
    let (d, state, _cancel) = drive(0, &task());
    {
        let mut s = state.write().unwrap();
        s.cancel_requested = true;
        s.stopped_by_reload = true;
    }
    run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &wrap)
        .await
        .unwrap();
    assert!(!r.dir.join(FINAL).exists(), "the final was not published");
    let head = head_on_disk(&r).expect("the head stays: the crash checkpoint");
    assert_eq!(head.status, "running");
    let error = {
        let s = state.read().unwrap();
        assert!(s.v2_records);
        assert_eq!(s.archive_path, None);
        assert_eq!(
            s.v2_head,
            Some(r.dir.join(HEAD)),
            "the state says where its crash checkpoint is"
        );
        s.archive_error.clone().unwrap()
    };
    assert!(error.contains("final record not published"), "{error}");

    crate::tools::subagent::finalize::finalize_subagent_in(
        &state,
        None,
        "sa_5",
        5,
        "worker",
        std::time::Instant::now(),
        None,
        &r.dir,
    );
    let lines = log.lines();
    let finalizer: Vec<_> = lines
        .iter()
        .filter(|l| l.target.ends_with("subagent::finalize"))
        .collect();
    let warned: Vec<_> = finalizer
        .iter()
        .filter(|l| l.level == tracing::Level::WARN)
        .collect();
    assert_eq!(warned.len(), 1, "{lines:#?}");
    let warning = &warned[0].message;
    assert!(warning.contains(&error), "names the error: {warning}");
    assert!(
        warning.contains("crash checkpoint") && warning.contains(HEAD),
        "{warning}"
    );
    assert!(
        !finalizer.iter().any(|l| l.message.contains("archived")),
        "never says archived: {lines:#?}"
    );
}

// ── Collect ───────────────────────────────────────────────────────────────

/// A registry whose handle `id` was reaped, leaving the record at `path`
/// (a v2 final of `v2_run`, or a v1 archive).
fn reaped(id: &str, path: std::path::PathBuf, v2_run: Option<&RunId>) -> Arc<Mutex<SubagentRegistry>> {
    let state = Arc::new(RwLock::new(SubagentState::new()));
    {
        let mut s = state.write().unwrap();
        s.status = SubagentStatus::Completed;
        s.finished_at = Some(std::time::Instant::now());
        s.archive_path = Some(path);
        s.v2_final_run = v2_run.map(|run| run.as_str().to_owned());
    }
    let (steer_tx, _steer_rx) = tokio::sync::mpsc::unbounded_channel();
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let (_result_tx, result_rx) = tokio::sync::oneshot::channel();
    let mut handle = SubagentHandle::new(
        id.into(),
        5,
        "worker".into(),
        "task".into(),
        "provider/model-b".into(),
        "s".into(),
        60,
        state,
        Some(steer_tx),
        Some(shutdown_tx),
        Some(result_rx),
    );
    handle.mark_collected();
    let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
    {
        let mut reg = registry.lock().unwrap();
        reg.register(handle);
        reg.cleanup_finished_with_ttl(Duration::ZERO);
        assert!(reg.get(id).is_none(), "reaped");
    }
    registry
}

/// `subagent_collect` of `id` with reaped records below `root`.
async fn collect(
    registry: Arc<Mutex<SubagentRegistry>>,
    id: &str,
    root: &std::path::Path,
) -> Result<Value, String> {
    let mut ctx = crate::tools::test_helpers::create_tool_context();
    ctx.capabilities.subagent_registry = Some(registry);
    ctx.capabilities.orchestration = None;
    crate::tools::subagent::collect::SubagentCollectTool
        .execute_in(json!({ "handle_id": id }), ctx, root.to_path_buf())
        .await
        .map(|body| serde_json::from_str(&body).unwrap())
        .map_err(|e| e.to_string())
}

fn assert_no_result(outcome: Result<Value, String>) {
    match outcome {
        Err(e) => assert!(e.contains("No subagent found"), "{e}"),
        Ok(body) => panic!("not a result, yet collected: {body}"),
    }
}

/// A valid v2 final of `run` for `sa_5`.
fn final_of(run: &RunId) -> WorkerRecord {
    let mut fin = record(run, fx::offline_binding(), vec![], &[], None);
    fin.status = "timed_out".into();
    fin.output = "[TIMED OUT] latest progress".into();
    fin
}

fn v1_archive(r: &WriteRoot) -> std::path::PathBuf {
    crate::tools::subagent::archive::write_archive(
        &r.dir,
        "sa_5",
        &SubagentArchiveMeta::from(meta()),
        "completed",
        None,
        "v1 out",
        &task(),
    )
    .unwrap()
}

#[tokio::test]
async fn l13_collect_reads_the_final_of_a_known_v2_run() {
    let r = write_root();
    fx::private_dir(&r.dir);
    let run = RunId::random();
    private_file(&r.dir.join(FINAL), &serde_json::to_vec(&final_of(&run)).unwrap());
    let body = collect(reaped("sa_5", r.dir.join(FINAL), Some(&run)), "sa_5", &r.root)
        .await
        .unwrap();
    assert_eq!(body["collected"], true);
    assert_eq!(body["status"], "timed_out");
    assert_eq!(body["output"], "[TIMED OUT] latest progress");
    assert_eq!(body["model"], "provider/model-b");
    assert_eq!(body["archive_path"], json!(r.dir.join(FINAL)));

    // A v1 handle reads its v1 archive as before.
    let r = write_root();
    fx::private_dir(&r.dir);
    let path = v1_archive(&r);
    let body = collect(reaped("sa_5", path, None), "sa_5", &r.root).await.unwrap();
    assert_eq!((body["status"].as_str(), body["output"].as_str()), (Some("completed"), Some("v1 out")));
}

/// With worker rollover off every handle is a v1 handle: a valid v2 final
/// in place of its archive changes nothing.
#[tokio::test]
async fn l15_a_v1_handle_whose_archive_became_a_v2_final_keeps_its_outcome() {
    let r = write_root();
    fx::private_dir(&r.dir);
    let run = RunId::random();
    private_file(&r.dir.join(FINAL), &serde_json::to_vec(&final_of(&run)).unwrap());
    assert_no_result(collect(reaped("sa_5", r.dir.join(FINAL), None), "sa_5", &r.root).await);
}

#[tokio::test]
async fn l16_a_known_v2_run_reads_only_its_own_bounded_final() {
    let run = RunId::random();

    // A head is not a result, even the run's own.
    let r = write_root();
    fx::private_dir(&r.dir);
    private_file(&r.dir.join(HEAD), &serde_json::to_vec(&head(&run, 0, "x")).unwrap());
    assert_no_result(collect(reaped("sa_5", r.dir.join(HEAD), Some(&run)), "sa_5", &r.root).await);
    assert_no_result(collect(reaped("sa_5", r.dir.join(FINAL), Some(&run)), "sa_5", &r.root).await);

    // Another run's final.
    let r = write_root();
    fx::private_dir(&r.dir);
    let other = RunId::random();
    private_file(&r.dir.join(FINAL), &serde_json::to_vec(&final_of(&other)).unwrap());
    assert_no_result(collect(reaped("sa_5", r.dir.join(FINAL), Some(&run)), "sa_5", &r.root).await);

    // A v1 archive under the final's name: never read as v1 for a v2 run.
    let r = write_root();
    fx::private_dir(&r.dir);
    let path = v1_archive(&r);
    assert_eq!(path, r.dir.join(FINAL));
    assert_no_result(collect(reaped("sa_5", path, Some(&run)), "sa_5", &r.root).await);

    // An oversized final is refused by the bounded reader before any read
    // (sparse: nothing is allocated either way).
    let r = write_root();
    fx::private_dir(&r.dir);
    private_file(&r.dir.join(FINAL), b"");
    std::fs::OpenOptions::new()
        .write(true)
        .open(r.dir.join(FINAL))
        .unwrap()
        .set_len(record_file::MAX_RECORD_FILE_BYTES + 1)
        .unwrap();
    assert_no_result(collect(reaped("sa_5", r.dir.join(FINAL), Some(&run)), "sa_5", &r.root).await);
}

/// The reaped record is read on a blocking worker: while the read waits,
/// this (single-threaded) executor keeps running other tasks.
#[tokio::test]
async fn l17_collect_reads_a_reaped_record_off_the_executor() {
    let r = write_root();
    fx::private_dir(&r.dir);
    let run = RunId::random();
    let mut fin = final_of(&run);
    fin.handle_id = "sa_17".into();
    private_file(&r.dir.join("sa_17.json"), &serde_json::to_vec(&fin).unwrap());
    let registry = reaped("sa_17", r.dir.join("sa_17.json"), Some(&run));
    let (entered, open) = crate::tools::subagent::collect::read_gate::gate("sa_17");
    let root = r.root.clone();
    let task = tokio::spawn(async move { collect(registry, "sa_17", &root).await });
    entered.await.unwrap();
    assert_eq!(tokio::spawn(async { 7 }).await.unwrap(), 7, "the executor runs");
    open.send(()).unwrap();
    let body = task.await.unwrap().unwrap();
    assert_eq!(body["status"], "timed_out");
}

/// `subagent_status` of `id` with reaped records below `root`.
async fn status_of(
    registry: Arc<Mutex<SubagentRegistry>>,
    id: &str,
    root: &std::path::Path,
) -> Result<Value, String> {
    let mut ctx = crate::tools::test_helpers::create_tool_context();
    ctx.capabilities.subagent_registry = Some(registry);
    ctx.capabilities.orchestration = None;
    crate::tools::subagent::status::SubagentStatusTool
        .execute_in(json!({ "handle_id": id }), ctx, root.to_path_buf())
        .await
        .map(|body| serde_json::from_str(&body).unwrap())
        .map_err(|e| e.to_string())
}

/// Refused, and said so: never "not found" for a record that is there.
fn assert_status_refused(outcome: Result<Value, String>) {
    match outcome {
        Err(e) => assert!(
            e.contains("Subagent 'sa_5' is no longer live and its record")
                && e.contains("could not be read")
                && !e.contains("No subagent found"),
            "{e}"
        ),
        Ok(body) => panic!("not a status, yet read: {body}"),
    }
}

/// `subagent_status` of a reaped v2 handle reads the final of the run it
/// recorded, through collect's reader: the same result collect returns.
#[tokio::test]
async fn status_after_reap_reads_the_final_of_the_recorded_v2_run() {
    let r = write_root();
    fx::private_dir(&r.dir);
    let run = RunId::random();
    private_file(&r.dir.join(FINAL), &serde_json::to_vec(&final_of(&run)).unwrap());
    let registry = reaped("sa_5", r.dir.join(FINAL), Some(&run));
    let body = status_of(Arc::clone(&registry), "sa_5", &r.root).await.unwrap();
    assert_eq!(body["handle_id"], "sa_5");
    assert_eq!(body["agent_name"], "worker", "the final's own meta names it");
    assert_eq!(body["status"], "timed_out");
    assert_eq!(body["partial_output"], "[TIMED OUT] latest progress");
    assert_eq!(body["model"], "provider/model-b");
    assert_eq!(body["error"], Value::Null, "a v2 final keeps no failure text");
    assert_eq!(body["archived"], true);
    assert_eq!(body["stopped_by_reload"], false);
    assert_eq!(body["archive_path"], json!(r.dir.join(FINAL)));
    assert!(body["note"].as_str().unwrap().starts_with("Not running: finished and reaped."));
    let collected = collect(registry, "sa_5", &r.root).await.unwrap();
    assert!(body["rolloverWindows"].is_object(), "{body}");
    assert_eq!(body["rolloverWindows"], collected["rolloverWindows"]);
    assert_eq!(body["status"], collected["status"]);
}

/// Another run's final at the recorded path is not this handle's result:
/// status refuses it as collect does.
#[tokio::test]
async fn status_after_reap_refuses_another_runs_final() {
    let r = write_root();
    fx::private_dir(&r.dir);
    let (run, other) = (RunId::random(), RunId::random());
    private_file(&r.dir.join(FINAL), &serde_json::to_vec(&final_of(&other)).unwrap());
    let registry = reaped("sa_5", r.dir.join(FINAL), Some(&run));
    assert_status_refused(status_of(Arc::clone(&registry), "sa_5", &r.root).await);
    assert_no_result(collect(registry, "sa_5", &r.root).await);
}

/// The recorded run's own valid final, reached through a symlink, is
/// refused: no links below the root.
#[tokio::test]
async fn status_after_reap_refuses_a_final_reached_through_a_symlink() {
    let r = write_root();
    fx::private_dir(&r.dir);
    let run = RunId::random();
    let target = r.dir.join("elsewhere.json");
    private_file(&target, &serde_json::to_vec(&final_of(&run)).unwrap());
    std::os::unix::fs::symlink(&target, r.dir.join(FINAL)).unwrap();
    let registry = reaped("sa_5", r.dir.join(FINAL), Some(&run));
    let outcome = status_of(Arc::clone(&registry), "sa_5", &r.root).await;
    assert!(
        outcome.as_ref().is_err_and(|e| e.contains("symlink")),
        "{outcome:?}"
    );
    assert_status_refused(outcome);
    assert_no_result(collect(registry, "sa_5", &r.root).await);
}

/// A daemon reload carries the run a reaped v2 handle recorded: after the
/// round trip (`reload_carry`, the reload record's JSON, `adopt_carried`)
/// the next image's status and collect both read that run's final. A
/// record an older image carried names no run: the v1 reader refuses the
/// final there (fails closed), and says why.
#[tokio::test]
async fn status_and_collect_read_the_recorded_v2_final_after_a_reload() {
    use crate::runtime::subagent::CarriedWorker;
    let r = write_root();
    fx::private_dir(&r.dir);
    let run = RunId::random();
    private_file(&r.dir.join(FINAL), &serde_json::to_vec(&final_of(&run)).unwrap());
    let carried = reaped("sa_5", r.dir.join(FINAL), Some(&run))
        .lock()
        .unwrap()
        .reload_carry(&Default::default());
    let back: Vec<CarriedWorker> =
        serde_json::from_slice(&serde_json::to_vec(&carried).unwrap()).unwrap();
    assert_eq!(back, carried);
    let w = back.iter().find(|w| w.id == "sa_5").unwrap();
    assert_eq!(w.v2_final_run.as_deref(), Some(run.as_str()));
    let adopted = |w: &CarriedWorker| {
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        registry.lock().unwrap().adopt_carried(w);
        registry
    };

    let next = adopted(w);
    let body = status_of(Arc::clone(&next), "sa_5", &r.root).await.unwrap();
    assert_eq!(body["status"], "timed_out");
    assert_eq!(body["partial_output"], "[TIMED OUT] latest progress");
    assert_eq!(body["model"], "provider/model-b");
    assert_eq!(body["agent_name"], "worker");
    assert!(body["rolloverWindows"].is_object(), "{body}");
    let collected = collect(next, "sa_5", &r.root).await.unwrap();
    assert_eq!(collected["status"], "timed_out");
    assert_eq!(collected["output"], "[TIMED OUT] latest progress");
    assert_eq!(collected["model"], "provider/model-b");

    let older = CarriedWorker { v2_final_run: None, ..w.clone() };
    let outcome = status_of(adopted(&older), "sa_5", &r.root).await;
    assert!(
        outcome.as_ref().is_err_and(|e| e.contains("unsupported subagent archive version 2")),
        "{outcome:?}"
    );
    assert_status_refused(outcome);
    assert_no_result(collect(adopted(&older), "sa_5", &r.root).await);
}

#[tokio::test]
async fn l14_a_missing_history_root_is_created_private() {
    use std::os::unix::fs::PermissionsExt;
    let store = fx::legacy();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("subagent-history");
    let r = WriteRoot {
        dir: root.join(PROCESS_DIR),
        root,
        _tmp: tmp,
    };
    let (_started, start) = runtime(vec![done()], || {});
    let (d, state, _cancel) = drive(0, &task());
    run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &real)
        .await
        .unwrap();
    let mode = std::fs::metadata(&r.root).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    assert!(state.read().unwrap().v2_records);
    assert_eq!(v2_final(&r).status, "completed");
}

// ── D: the run clock (§5) ─────────────────────────────────────────────────

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// A runtime stand-in that works until cancelled, then publishes its
/// opening as its history (the runtime's cancel path) and ends.
fn works_until_cancelled(
    cancel: &CancellationToken,
) -> impl FnOnce(Vec<SharedMessage>) -> std::future::Ready<WorkerStream> {
    let cancel = cancel.clone();
    move |opening| {
        let stream = futures::stream::once(async move {
            cancel.cancelled().await;
            history_event(opening)
        });
        std::future::ready(Box::pin(stream) as WorkerStream)
    }
}

/// The runtime's stop on the run clock: its history, then the typed error.
fn wall_clock_stop() -> StreamEvent {
    StreamEvent::Session(SessionEvent::Error(agent_core::TurnError::budget(
        agent_core::BudgetDimension::WallClock,
    )))
}

/// D: min of both deadlines. A run with a plan stops at the earlier of its
/// timeout and the worker's `max_elapsed`, both from one run start; a
/// timeout of zero is none. On paused time: the clock moves only to the
/// next timer once nothing else can run (never while a record write runs
/// on a blocking worker), so the stop lands exactly at the earlier
/// deadline whatever the runner's scheduling.
#[tokio::test(start_paused = true)]
async fn d12_the_run_deadline_is_the_earlier_of_timeout_and_max_elapsed() {
    let store = fx::legacy();
    for (timeout, max_elapsed) in [(10, Some(1)), (1, Some(10)), (0, Some(1))] {
        let r = write_root();
        let (mut d, _state, cancel) = drive(timeout, &task());
        d.run_clock = Some(RunClock::start(
            (timeout > 0).then(|| secs(timeout)),
            max_elapsed.map(secs),
        ));
        let began = tokio::time::Instant::now();
        let result = tokio::time::timeout(
            secs(5),
            run_worker_with(
                d,
                task(),
                Some(plan(&r, &store.binding, None)),
                works_until_cancelled(&cancel),
                &real,
            ),
        )
        .await
        .expect("stopped at the earlier deadline")
        .unwrap();
        assert!(result.timed_out);
        let ran = began.elapsed();
        assert!(ran >= secs(1) && ran < secs(2), "{timeout} {max_elapsed:?}: {ran:?}");
        assert!(
            result.text.starts_with("[TIMED OUT after 1s"),
            "{timeout} {max_elapsed:?}: {}",
            result.text
        );
        assert_eq!(v2_final(&r).status, "timed_out");
    }
}

/// D: the runtime checks the same deadline before each request; its stop
/// on it is the run timing out, not a failure. Off (no plan) the same
/// event keeps its v1 outcome.
#[tokio::test]
async fn d13_a_runtime_stop_on_the_run_deadline_is_the_run_timing_out() {
    let store = fx::legacy();
    let r = write_root();
    let latest = msgs(vec![user("task"), assistant("partial")]);
    let events = || vec![text("partial"), history_event(latest.clone()), wall_clock_stop()];
    let (_started, start) = runtime(events(), || {});
    let (mut d, state, _cancel) = drive(300, &task());
    d.run_clock = Some(RunClock::start(Some(secs(300)), None));
    let result = run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &real)
        .await
        .unwrap();
    assert!(result.timed_out);
    assert!(result.text.starts_with("[TIMED OUT after 300s"), "{}", result.text);
    assert_eq!(state.read().unwrap().status, SubagentStatus::TimedOut);
    let fin = v2_final(&r);
    assert_eq!(fin.status, "timed_out");
    assert_eq!(fin.history.last().unwrap()["content"], "partial");

    let (_started, start) = runtime(events(), || {});
    let (d, state, _cancel) = drive(300, &task());
    let err = run_worker_with(d, task(), None, start, &real)
        .await
        .unwrap_err();
    // Off, the v1 outcome is the live one: 7fdd5a4d's drive words a
    // provider error as `[category]: message`.
    assert_eq!(
        err,
        "provider request failed [budget_exceeded dimension=WallClock]: turn budget exhausted (wall_clock)"
    );
    assert_eq!(state.read().unwrap().status, SubagentStatus::Running);
}

/// D: the run clock is armed only for a run with a plan, on its runtime,
/// from the runtime's worker budget; without a plan the runtime keeps its
/// per-turn clock (rollover off changes nothing).
#[tokio::test]
async fn d14_a_run_clock_is_armed_only_for_a_run_with_a_plan() {
    let store = fx::legacy();
    let r = write_root();
    let worker = || {
        let mut rt = crate::Runtime::new_headless();
        rt.set_turn_budget(crate::runtime::budget::TurnBudget {
            max_elapsed: Some(secs(2)),
            ..crate::runtime::budget::TurnBudget::for_role(
                crate::runtime::budget::TurnRole::Worker,
            )
        });
        rt
    };
    let mut rt = worker();
    let before = rt.turn_budget().clone();
    assert_eq!(arm_run_clock(None, &mut rt, 10), None);
    assert_eq!(rt.turn_budget(), &before, "off: nothing changes");

    let plan = plan(&r, &store.binding, None);
    for (timeout, limit) in [(10, 2), (1, 1), (0, 2)] {
        let mut rt = worker();
        let clock = arm_run_clock(Some(&plan), &mut rt, timeout).unwrap();
        assert_eq!(clock.limit(), Some(secs(limit)), "timeout {timeout}");
        assert_eq!(rt.turn_budget().run_clock, Some(clock));
    }
}

// ── Request ledger (§6) ───────────────────────────────────────────────────

/// G: what a runtime that has the run's ledger books at its first
/// inference: one measured conversation attempt.
fn book_one(rt: &crate::Runtime) {
    let ledger = rt.request_ledger().expect("the runtime has the run's ledger");
    let mut attempt = ledger.reserve(7, UsageReports::Cumulative).unwrap();
    attempt.observe(MeasuredUsage {
        input_tokens: 3,
        output_tokens: 2,
        ..MeasuredUsage::default()
    });
    assert_eq!(attempt.finish(), AttemptEnd::Measured);
}

/// G: a runtime gets a request ledger only through the launch, only for a
/// run with a plan, and only once the run is a v2 run. It is the run's own
/// ledger: what the runtime books is in the run's final, and the clone a
/// stream runs on shares it.
#[tokio::test]
async fn g1_only_a_v2_run_gives_its_runtime_the_run_ledger() {
    let mut rt = crate::Runtime::new_headless();
    arm_request_ledger(None, &mut rt);
    assert!(rt.request_ledger().is_none(), "without a plan nothing is armed");

    let store = fx::legacy();
    let r = write_root();
    let plan = plan(&r, &store.binding, None);
    let mut rt = crate::Runtime::new_headless();
    arm_request_ledger(Some(&plan), &mut rt);
    assert!(rt.request_ledger().is_none(), "none before the launch decides");
    let rt = Arc::new(rt);
    let seen = Arc::clone(&rt);
    let at_start = move || {
        book_one(&seen);
        let clone = (*seen).clone();
        assert!(clone.request_ledger().is_some(), "a clone shares the slot");
    };
    let latest = msgs(vec![user("task"), assistant("done")]);
    let (started, start) = runtime(
        vec![text("done"), history_event(latest), done()],
        at_start,
    );
    let (d, state, _cancel) = drive(0, &task());
    run_worker_with(d, task(), Some(plan), start, &real)
        .await
        .unwrap();
    assert!(started.lock().unwrap().is_some());
    assert!(state.read().unwrap().v2_records);
    let fin = v2_final(&r);
    let booked: Vec<_> = fin.ledger.entries().collect();
    assert_eq!(booked.len(), 1, "{booked:?}");
    assert_eq!(booked[0].purpose, RequestPurpose::Conversation);
    assert_eq!(booked[0].model, worker_meta().model);
    assert!(
        matches!(&booked[0].amount, Amount::Measured { usage } if usage.input_tokens == 3),
        "{booked:?}"
    );
}

/// G: a run that falls back to rollover off (here: a binding that cannot
/// confine retrieval) leaves its runtime without a ledger, so its requests
/// dispatch as a v1 worker's.
#[tokio::test]
async fn g2_a_fallback_to_rollover_off_leaves_the_runtime_without_a_ledger() {
    let r = write_root();
    let base = r._tmp.path().canonicalize().unwrap();
    let binding = unsupported_binding(&base);
    let plan = plan(&r, &binding, None);
    let mut rt = crate::Runtime::new_headless();
    arm_request_ledger(Some(&plan), &mut rt);
    let rt = Arc::new(rt);
    let seen = Arc::clone(&rt);
    let had: Arc<Mutex<Option<bool>>> = Arc::default();
    let had_at_start = Arc::clone(&had);
    let at_start = move || {
        *had_at_start.lock().unwrap() = Some(seen.request_ledger().is_some());
    };
    let (_started, start) = runtime(vec![done()], at_start);
    let (d, state, _cancel) = drive(0, &task());
    run_worker_with(d, task(), Some(plan), start, &real)
        .await
        .unwrap();
    assert!(!state.read().unwrap().v2_records);
    assert_eq!(*had.lock().unwrap(), Some(false), "started, without a ledger");
    assert!(rt.request_ledger().is_none());
}

/// G: a runtime built any other way (a session's, one-shot's, rpc's,
/// agent's) has no request ledger, and the launch is the only code that
/// arms one or installs a run's ledger. A lexical tripwire over the
/// sources: it catches another file naming those calls or setting the
/// options' ledger, not data flow (a copy of another request's options
/// passes it); the call-graph audit and the behavioural tests (g1, g2, the
/// oneshot test lg1, the off gate) carry that.
#[test]
fn g3_only_the_launch_arms_or_fills_a_runtimes_ledger() {
    assert!(crate::Runtime::new_headless().request_ledger().is_none());
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut arms = Vec::new();
    let mut installs = Vec::new();
    let mut ledgered_options = Vec::new();
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !name.ends_with(".rs") || name.ends_with("_tests.rs") {
                continue;
            }
            let rel = path.strip_prefix(&src).unwrap().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(&path).unwrap();
            for line in text.lines() {
                let code = line.trim();
                if code.starts_with("//") {
                    continue;
                }
                if code.contains(".arm_request_ledger(") {
                    arms.push(rel.clone());
                }
                if code.contains(".install(") && code.contains("ledger") {
                    installs.push(rel.clone());
                }
                if code.starts_with("request_ledger:")
                    && !code.starts_with("request_ledger: None")
                    && !code.starts_with("request_ledger: Option<")
                {
                    ledgered_options.push(format!("{rel}: {code}"));
                }
            }
        }
    }
    assert_eq!(arms, vec!["tools/subagent/launch.rs".to_owned()]);
    assert_eq!(installs, vec!["tools/subagent/launch.rs".to_owned()]);
    ledgered_options.sort();
    assert_eq!(
        ledgered_options,
        vec![
            "runtime/mod.rs: request_ledger: self.request_ledger(),".to_owned(),
            "runtime/mod.rs: request_ledger: self.request_ledger.clone(),".to_owned(),
        ],
        "only the stream turn's options (and the runtime's clone) carry a ledger"
    );
}

// ── Root relocation (§3.3) ────────────────────────────────────────────────

/// Hosts must not relocate an active history root. If one is moved anyway
/// while a run is live, the run's writes follow the process directory it
/// opened, while the location it reports is still spelled under the old
/// root: collect of that location then refuses, never reads another record,
/// also after a fresh root appears in the old place.
#[tokio::test]
async fn l21_a_root_moved_under_a_live_run_reads_back_as_a_refusal() {
    let store = fx::legacy();
    let r = write_root();
    let moved = r.root.with_file_name("subagent-history.moved");
    let (from, to) = (r.root.clone(), moved.clone());
    let (_started, start) = runtime(
        vec![
            text("done"),
            history_event(msgs(vec![user("task"), assistant("done")])),
            done(),
        ],
        move || std::fs::rename(&from, &to).unwrap(),
    );
    let (d, state, _cancel) = drive(0, &task());
    run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &real)
        .await
        .unwrap();

    // The final followed the open directory to the moved root ...
    let at_moved =
        record_file::location_of(&moved, PROCESS_DIR, "sa_5", RecordKind::Final).unwrap();
    let Candidate::Present(FinalRecord::V2(fin)) =
        record_file::load(&at_moved).unwrap().final_record
    else {
        panic!("the final is not below the moved root");
    };
    assert_eq!(fin.output, "done");
    // ... while the run reports it under the old root, where nothing is.
    let (path, run) = {
        let s = state.read().unwrap();
        (s.archive_path.clone().unwrap(), s.v2_final_run.clone().unwrap())
    };
    assert_eq!(path, r.dir.join(FINAL));
    assert_eq!(run, fin.run_id.as_str());
    assert!(!r.root.exists());
    assert_no_result(collect(reaped("sa_5", path.clone(), Some(&fin.run_id)), "sa_5", &r.root).await);

    // A fresh root in the old place: nothing of this run there either.
    fx::private_dir(&r.root);
    fx::private_dir(&r.dir);
    private_file(
        &r.dir.join(FINAL),
        &serde_json::to_vec(&final_of(&RunId::random())).unwrap(),
    );
    assert_no_result(collect(reaped("sa_5", path, Some(&fin.run_id)), "sa_5", &r.root).await);
}

// ── Routes that can't book their requests (§6) ────────────────────────────

/// A run on `model` with a store whose binding confines retrieval: only the
/// model decides whether it can be a v2 run. Returns whether the worker
/// started, whether its runtime had a request ledger at its first
/// inference, the receipt of its checkpoint, its state and its root.
async fn run_on(
    model: &str,
) -> (
    bool,
    Option<bool>,
    Result<(), String>,
    Arc<RwLock<SubagentState>>,
    WriteRoot,
) {
    let store = fx::legacy();
    let r = write_root();
    let mut plan = plan(&r, &store.binding, None);
    plan.meta.model = model.into();
    let mut rt = crate::Runtime::new_headless();
    arm_request_ledger(Some(&plan), &mut rt);
    let rt = Arc::new(rt);
    let seen = Arc::clone(&rt);
    let had: Arc<Mutex<Option<bool>>> = Arc::default();
    let had_at_start = Arc::clone(&had);
    let (event, rx) = checkpoint("worker-run:any".into(), task());
    let (started, start) = runtime(vec![event, done()], move || {
        *had_at_start.lock().unwrap() = Some(seen.request_ledger().is_some());
    });
    let (d, state, _cancel) = drive(0, &task());
    run_worker_with(d, task(), Some(plan), start, &real)
        .await
        .unwrap();
    let started = started.lock().unwrap().is_some();
    let had = *had.lock().unwrap();
    (started, had, rx.await.unwrap(), state, r)
}

/// §6: a v2 run books every request it sends in its ledger. A worker whose
/// route can't (the cloud routes, OpenAI Responses, an extension provider)
/// runs with rollover off from the start: no record, lock or directory, no
/// ledger; it runs as a v1 worker, and the caller is told why.
#[tokio::test]
async fn lr1_a_worker_whose_route_cannot_book_runs_with_rollover_off() {
    let mut models: Vec<String> = crate::auth::cloud::cloud_provider_descriptors()
        .iter()
        .map(|d| format!("{}/some-model", d.id))
        .collect();
    models.extend(["xai-auth/grok-4.5".into(), "some-plugin:provider:model".into()]);
    for model in &models {
        let (started, had_ledger, receipt, state, r) = run_on(model).await;
        assert!(started, "{model}: the worker still runs");
        assert_eq!(had_ledger, Some(false), "{model}: without a ledger");
        assert_eq!(receipt, Err(ROLLOVER_OFF.into()), "{model}");
        let s = state.read().unwrap();
        assert!(!s.v2_records, "{model}: a v1 run");
        assert!(!r.dir.exists(), "{model}: nothing below the root");
        assert_eq!(
            s.rollover_notice.as_deref(),
            Some(
                format!(
                    "rollover disabled for this worker: '{model}' can't book its requests in a \
                     run's request ledger"
                )
                .as_str()
            ),
        );
    }
    // The same run on a route that books is a v2 run, and nobody is told
    // anything.
    let (started, had_ledger, receipt, state, r) = run_on(BOOKED_MODEL).await;
    assert!(started);
    assert_eq!(had_ledger, Some(true));
    assert_ne!(receipt, Err(ROLLOVER_OFF.into()));
    let s = state.read().unwrap();
    assert!(s.v2_records);
    assert!(r.dir.exists());
    assert_eq!(s.rollover_notice, None);
}

/// §6 tripwire: cloud routes are unbooked because no worker request can
/// reach them (every worker request carries tools; a cloud route refuses
/// tools before any broker or network). A cloud provider that starts
/// accepting tools while its route still does not book requests breaks
/// that premise.
#[test]
fn lr2_cloud_routes_stay_unbooked_only_while_they_refuse_tools() {
    for d in crate::auth::cloud::cloud_provider_descriptors() {
        let model = format!("{}/some-model", d.id);
        assert!(
            !d.supports_tools || crate::runtime::openai::books_requests(&model),
            "{} now accepts tools, so a worker's requests can reach it, but its route does not \
             book requests in a run's ledger: add the cloud ledger hook \
             (runtime/cloud_invoke.rs) or confirm the launch's rollover-off gate covers it, \
             and update spec §6 ('cloud routes are unbooked'), before enabling worker rollover",
            d.id
        );
    }
}

/// The notice is one bounded plain line, whatever the reason says.
#[test]
fn lr3_the_notice_is_bounded_and_plain() {
    let reason = format!("a\nb\u{1b}[31m{}", "x".repeat(10_000));
    let notice = rollover_off_notice(&reason);
    assert!(notice.starts_with("rollover disabled for this worker: a b"), "{notice}");
    assert!(!notice.chars().any(char::is_control), "{notice:?}");
    assert!(
        notice.chars().count()
            <= "rollover disabled for this worker: ".chars().count() + NOTICE_REASON_CHARS + 1,
        "{}",
        notice.chars().count()
    );
    assert!(notice.ends_with('…'));
    assert_eq!(
        rollover_off_notice("short"),
        "rollover disabled for this worker: short"
    );
}

/// Only a fallback tells the caller: a run without a plan (rollover off
/// everywhere) says nothing; any fallback says why.
#[tokio::test]
async fn lr4_only_a_fallback_tells_the_caller() {
    let (_started, start) = runtime(vec![done()], || {});
    let (d, state, _cancel) = drive(0, &task());
    run_worker_with(d, task(), None, start, &real).await.unwrap();
    assert_eq!(state.read().unwrap().rollover_notice, None);

    let r = write_root();
    let base = r._tmp.path().canonicalize().unwrap();
    let binding = unsupported_binding(&base);
    let (_started, start) = runtime(vec![done()], || {});
    let (d, state, _cancel) = drive(0, &task());
    run_worker_with(d, task(), Some(plan(&r, &binding, None)), start, &real)
        .await
        .unwrap();
    assert_eq!(
        state.read().unwrap().rollover_notice.as_deref(),
        Some(
            "rollover disabled for this worker: the memory binding cannot confine retrieval to \
             a lineage"
        )
    );
}

fn notice_handle(id: &str, status: SubagentStatus, notice: Option<&str>) -> SubagentHandle {
    let state = Arc::new(RwLock::new(SubagentState::new()));
    {
        let mut s = state.write().unwrap();
        s.partial_text = "done".into();
        if status != SubagentStatus::Running {
            s.finished_at = Some(std::time::Instant::now());
        }
        s.status = status;
        s.rollover_notice = notice.map(str::to_owned);
    }
    let (steer_tx, _steer_rx) = tokio::sync::mpsc::unbounded_channel();
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let (_result_tx, result_rx) = tokio::sync::oneshot::channel();
    SubagentHandle::new(
        id.into(),
        5,
        "worker".into(),
        "task".into(),
        BOOKED_MODEL.into(),
        "s".into(),
        60,
        state,
        Some(steer_tx),
        Some(shutdown_tx),
        Some(result_rx),
    )
}

async fn tool_body(
    tool: &dyn crate::tools::Tool,
    registry: &Arc<Mutex<SubagentRegistry>>,
    id: &str,
) -> Value {
    let mut ctx = crate::tools::test_helpers::create_tool_context();
    ctx.capabilities.subagent_registry = Some(Arc::clone(registry));
    ctx.capabilities.orchestration = None;
    serde_json::from_str(&tool.execute(json!({"handle_id": id}), ctx).await.unwrap()).unwrap()
}

/// The caller sees the notice where it looks: status, collect (running or
/// ended) and the completion event. Without one, none of them changes.
#[tokio::test]
async fn lr5_status_collect_and_completion_carry_the_notice() {
    const NOTICE: &str = "rollover disabled for this worker: why";
    let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
    {
        let mut reg = registry.lock().unwrap();
        reg.register(notice_handle("sa_n1", SubagentStatus::Running, Some(NOTICE)));
        reg.register(notice_handle("sa_n2", SubagentStatus::Completed, Some(NOTICE)));
        reg.register(notice_handle("sa_n3", SubagentStatus::Running, None));
        reg.register(notice_handle("sa_n4", SubagentStatus::Completed, None));
    }
    let status = crate::tools::subagent::SubagentStatusTool;
    let collect = crate::tools::subagent::collect::SubagentCollectTool;
    for id in ["sa_n1", "sa_n2"] {
        assert_eq!(tool_body(&status, &registry, id).await["rolloverNotice"], NOTICE, "{id}");
        assert_eq!(tool_body(&collect, &registry, id).await["rolloverNotice"], NOTICE, "{id}");
    }
    for id in ["sa_n3", "sa_n4"] {
        for body in [
            tool_body(&status, &registry, id).await,
            tool_body(&collect, &registry, id).await,
        ] {
            assert!(body.get("rolloverNotice").is_none(), "{id}: {body}");
        }
    }

    let completion = |notice: Option<&str>| {
        crate::tools::subagent::finalize::build_completion_event(
            "sa_n2",
            5,
            "worker",
            &SubagentStatus::Completed,
            "done",
            1.0,
            None,
            notice,
            0,
        )
    };
    let with = completion(Some(NOTICE));
    assert!(with.content.text.contains(&format!(" Note: {NOTICE}.")), "{}", with.content.text);
    assert_eq!(with.content.data.as_ref().unwrap()["rollover_notice"], NOTICE);
    let without = completion(None);
    assert_eq!(
        without.content.text,
        "Subagent 'worker' (sa_n2) finished with status 'completed' after 1.0s. Call \
         subagent_collect with handle_id \"sa_n2\" to retrieve the full result. Preview: done"
    );
    assert!(without.content.data.as_ref().unwrap().get("rollover_notice").is_none());

    // The finalizer passes the run's notice on.
    let queue = Arc::new(crate::events::EventQueue::new(10));
    let state = Arc::clone(&notice_handle("sa_n5", SubagentStatus::Completed, Some(NOTICE)).state);
    let dir = tempfile::tempdir().unwrap();
    crate::tools::subagent::finalize::finalize_subagent_in(
        &state,
        Some(&queue),
        "sa_n5",
        5,
        "worker",
        std::time::Instant::now(),
        None,
        dir.path(),
    );
    let ev = queue.pop().unwrap();
    assert_eq!(ev.content.data.unwrap()["rollover_notice"], NOTICE);
}

// ── The parent's context settings (§2, §7.2; slice 7) ─────────────────────

/// What a configured runtime stand-in was started with: its opening and
/// the worker context it was given (`None`: never started).
type StartedWith = Arc<Mutex<Option<(Vec<SharedMessage>, Option<WorkerStart>)>>>;

/// [`runtime`] for a `start` that is also given the worker context.
fn configured_runtime(
    events: Vec<StreamEvent>,
) -> (
    StartedWith,
    impl FnOnce(
        Vec<SharedMessage>,
        Option<WorkerStart>,
    ) -> std::future::Ready<WorkerStream>,
) {
    let started: StartedWith = Arc::default();
    let seen = Arc::clone(&started);
    let start = move |opening: Vec<SharedMessage>, context: Option<WorkerStart>| {
        *seen.lock().unwrap() = Some((opening, context));
        std::future::ready(Box::pin(futures::stream::iter(events)) as WorkerStream)
    };
    (started, start)
}

/// A run with `context` as its plan's resolved parent settings, on
/// `model`. Returns what its runtime was started with, its state and root.
async fn run_with_context(
    model: &str,
    context: Result<crate::tools::WorkerContext, String>,
) -> (
    Option<(Vec<SharedMessage>, Option<WorkerStart>)>,
    Arc<RwLock<SubagentState>>,
    WriteRoot,
) {
    let store = fx::legacy();
    let r = write_root();
    let mut plan = plan(&r, &store.binding, None);
    plan.meta.model = model.into();
    plan.context = context;
    let (started, start) = configured_runtime(vec![done()]);
    let (d, state, _cancel) = drive(0, &task());
    run_worker_configured_with(d, task(), Some(plan), start, &real)
        .await
        .unwrap();
    let started = started.lock().unwrap().take();
    (started, state, r)
}

/// m0: without a plan (worker rollover off, or a parent whose context
/// management isn't `auto`) the runtime is started with nothing inherited,
/// and nobody is told anything.
#[tokio::test]
async fn w1_without_a_plan_the_runtime_inherits_nothing() {
    let (started, start) = configured_runtime(vec![done()]);
    let (d, state, _cancel) = drive(0, &task());
    run_worker_configured_with(d, task(), None, start, &real)
        .await
        .unwrap();
    let (opening, context) = started.lock().unwrap().take().expect("started");
    assert_eq!(opening, task());
    assert_eq!(context, None);
    let s = state.read().unwrap();
    assert!(!s.v2_records);
    assert_eq!(s.rollover_notice, None);
}

/// A v2 run's runtime is started with the parent's settings resolved for
/// it; thresholds that fit need no notice.
#[tokio::test]
async fn w2_a_v2_runs_runtime_starts_with_the_resolved_settings() {
    let (started, state, r) = run_with_context(BOOKED_MODEL, Ok(worker_context())).await;
    let (_, start) = started.expect("started");
    assert_eq!(start.map(|s| s.context), Some(worker_context()));
    let s = state.read().unwrap();
    assert!(s.v2_records);
    assert!(r.dir.exists());
    assert_eq!(s.rollover_notice, None);
}

/// 8c: a v2 run's runtime starts bound to its run: the logical ID of the
/// run its records name, which every head it checkpoints must carry and
/// its windows are sealed under; never another run's. A run without a
/// plan and every fallback start with no binding (`w1`, `w4`, `w5`).
#[tokio::test]
async fn w8_a_v2_runs_runtime_starts_bound_to_its_run() {
    let (started, _state, r) = run_with_context(BOOKED_MODEL, Ok(worker_context())).await;
    let start = started.expect("started").1.expect("a v2 run's start");
    let fin = v2_final(&r);
    assert_eq!(start.logical_id, worker_logical_id(&fin.run_id));
    let (again, _state, r2) = run_with_context(BOOKED_MODEL, Ok(worker_context())).await;
    let other = again.expect("started").1.expect("a v2 run's start");
    assert_eq!(other.logical_id, worker_logical_id(&v2_final(&r2).run_id));
    assert_ne!(start.logical_id, other.logical_id, "each run its own");
}

/// m10: thresholds reset to the worker's defaults keep the run a v2 run,
/// and the caller is told why in the rollover-notice field.
#[tokio::test]
async fn w3_reset_thresholds_keep_rollover_on_and_tell_the_caller() {
    let policy = crate::tools::WorkerContextPolicy {
        context_management: agent_core::config::ContextManagementConfig {
            mode: agent_core::config::ContextManagementMode::Auto,
            pressure_tokens: Some(600_000),
            rollover_tokens: Some(800_000),
            ..Default::default()
        },
        ..auto_policy()
    };
    let context = policy.resolve(BOOKED_MODEL).expect("the defaults fit");
    let notice = context.notice.clone().expect("a reset");
    assert!(notice.starts_with("rollover thresholds reset to this worker's defaults: "));
    let (started, state, _r) = run_with_context(BOOKED_MODEL, Ok(context.clone())).await;
    assert_eq!(started.expect("started").1.map(|s| s.context), Some(context));
    let s = state.read().unwrap();
    assert!(s.v2_records, "rollover stays on");
    assert_eq!(s.rollover_notice.as_deref(), Some(notice.as_str()));
}

/// m10: settings even the defaults don't fit run the worker with rollover
/// off, before anything is written, with nothing inherited; the caller is
/// told why.
#[tokio::test]
async fn w4_settings_that_cannot_resolve_run_the_worker_with_rollover_off() {
    let policy = crate::tools::WorkerContextPolicy {
        window_override: Some(crate::tools::InheritedWindow {
            tokens: 1,
            model: BOOKED_MODEL.into(),
        }),
        ..auto_policy()
    };
    let why = policy.resolve(BOOKED_MODEL).expect_err("the defaults don't fit a 1-token window");
    let (started, state, r) = run_with_context(BOOKED_MODEL, Err(why.clone())).await;
    assert_eq!(started.expect("the worker still runs").1, None);
    let s = state.read().unwrap();
    assert!(!s.v2_records);
    assert!(!r.dir.exists(), "nothing below the root");
    assert_eq!(
        s.rollover_notice.as_deref(),
        Some(format!("rollover disabled for this worker: {why}").as_str())
    );
}

/// Any other fallback to rollover off starts the runtime with nothing
/// inherited either: no inherited window override or thresholds without a
/// v2 run.
#[tokio::test]
async fn w5_a_fallback_to_rollover_off_inherits_nothing() {
    let (started, state, _r) = run_with_context("xai-auth/grok-4.5", Ok(worker_context())).await;
    assert_eq!(started.expect("started").1, None);
    assert!(!state.read().unwrap().v2_records);
}

/// e6 (slice 9): worker rollover is built on. A parent whose live context
/// management is auto gives a worker a plan unless the operator or the
/// caller opts out; a parent whose mode is off, or a tool context without
/// a snapshot, gives none. A worker on a route that can't book its requests
/// still falls back with the notice before anything is written (`lr1`).
#[test]
fn e6_worker_rollover_is_on_for_an_auto_parent_unless_someone_opts_out() {
    use agent_core::config::ContextRollover::{Inherit, Off};
    let off = crate::tools::WorkerContextPolicy::default();
    assert!(wants_rollover(Some(&auto_policy()), Inherit));
    assert!(!wants_rollover(Some(&auto_policy()), Off));
    for policy in [None, Some(&off)] {
        for choice in [Inherit, Off] {
            assert!(!wants_rollover(policy, choice));
        }
    }
    let operator_off = crate::tools::WorkerContextPolicy {
        operator_rollover: Off,
        ..auto_policy()
    };
    assert!(!wants_rollover(Some(&operator_off), Inherit));
    assert_eq!(
        super::worker_context(None, BOOKED_MODEL),
        Err("the parent's context settings are not available".to_owned())
    );
    assert_eq!(super::worker_context(Some(&auto_policy()), BOOKED_MODEL), Ok(worker_context()));
}

/// The worker context is applied only through `Runtime::start_worker_run`
/// (which binds the run first), called only by [`start_v2_runtime`] (which
/// also puts a fresh lineage's runtime on its owned registry, 8g), and
/// that only by the start closures that start, resume and the oneshot give
/// the launch, which call it only with a start the launch handed them (a
/// v2 run). A lexical tripwire like g3, over production code: a
/// `_tests.rs` file and an inline `#[cfg(test)] mod … {` block are tests.
///
/// PARITY (E9): the merge's pin
/// `e9_upstreams_worker_rollover_runs_only_where_ours_is_inactive` (in
/// `tools/subagent/mod.rs`'s `worker_rollover_tests`) applies a worker
/// context to a bare runtime on purpose; the scan skips inline test
/// modules, so that test call is not a start path.
#[test]
fn w7_only_the_worker_start_closures_apply_the_worker_context() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut applies = Vec::new();
    let mut starts = Vec::new();
    let mut v2_starts = Vec::new();
    let mut v2_defs = Vec::new();
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !name.ends_with(".rs") || name.ends_with("_tests.rs") {
                continue;
            }
            let rel = path.strip_prefix(&src).unwrap().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(&path).unwrap();
            // An inline test module runs from its `#[cfg(test)]`-marked
            // `mod … {` line to its closing `}` at column 0.
            let mut after_cfg_test = false;
            let mut in_test_module = false;
            for line in text.lines() {
                if in_test_module {
                    in_test_module = line != "}";
                    continue;
                }
                let code = line.trim();
                if std::mem::take(&mut after_cfg_test)
                    && !line.starts_with(char::is_whitespace)
                    && code.contains("mod ")
                    && code.ends_with('{')
                {
                    in_test_module = true;
                    continue;
                }
                after_cfg_test = code == "#[cfg(test)]";
                if code.starts_with("//") {
                    continue;
                }
                if code.contains(".apply_worker_context(") {
                    applies.push(rel.clone());
                }
                if code.contains(".start_worker_run(") {
                    starts.push(rel.clone());
                }
                // Every use of the name, not only a call: a stray call in
                // launch.rs, a function pointer or an alias counts too. The
                // one definition is in launch.rs.
                for (at, _) in code.match_indices("start_v2_runtime") {
                    let ident = |c: char| c.is_alphanumeric() || c == '_';
                    let before = code[..at].chars().next_back();
                    let after = code[at + "start_v2_runtime".len()..].chars().next();
                    if before.is_some_and(ident) || after.is_some_and(ident) {
                        continue;
                    }
                    if code[..at].ends_with("fn ") {
                        v2_defs.push(rel.clone());
                    } else {
                        v2_starts.push(rel.clone());
                    }
                }
            }
        }
    }
    applies.sort();
    starts.sort();
    v2_starts.sort();
    assert_eq!(applies, vec!["runtime/mod.rs".to_owned()]);
    assert_eq!(starts, vec!["tools/subagent/launch.rs".to_owned()]);
    assert_eq!(v2_defs, vec!["tools/subagent/launch.rs".to_owned()]);
    assert_eq!(
        v2_starts,
        vec![
            "tools/subagent/oneshot.rs".to_owned(),
            "tools/subagent/resume.rs".to_owned(),
            "tools/subagent/start.rs".to_owned(),
        ]
    );
}

// ── A oneshot's terminal publication (§5; slice 8c) ───────────────────────

/// A oneshot's drive (its words, its drain) with `timeout_secs`.
fn oneshot_drive(
    timeout_secs: u64,
) -> (WorkerDrive, Arc<RwLock<SubagentState>>, CancellationToken) {
    let (mut d, state, cancel) = drive(timeout_secs, &task());
    d.shape = crate::tools::subagent::drive::DriveShape::Oneshot;
    (d, state, cancel)
}

/// The run's record directory as its writes see it, once it has one.
type Records = Arc<Mutex<Option<Arc<FaultFs>>>>;

/// A wrap that keeps the run's record directory in `records`.
fn recorded(records: &Records) -> impl Fn(RecordDir) -> Arc<dyn RecordFs> {
    let records = Arc::clone(records);
    move |dir| {
        let fs = FaultFs::new(dir);
        *records.lock().unwrap() = Some(Arc::clone(&fs));
        fs as Arc<dyn RecordFs>
    }
}

/// A worker that answers in part and then never ends. Its first inference
/// (its spawn head is durable by then) injects `fault` at the next `op` on
/// `target` in the run's record directory.
fn stalls_after_injecting(
    records: &Records,
    op: Op,
    target: &'static str,
    fault: Fault,
) -> impl FnOnce(Vec<SharedMessage>, Option<WorkerStart>) -> std::future::Ready<WorkerStream> {
    let records = Arc::clone(records);
    move |_opening, _context| {
        records
            .lock()
            .unwrap()
            .as_ref()
            .expect("the run's record directory")
            .inject(op, target, fault);
        std::future::ready(Box::pin(
            futures::stream::iter(vec![
                StreamEvent::Llm(crate::LlmEvent::ResponseStart),
                text("partial answer"),
            ])
            .chain(futures::stream::pending()),
        ) as WorkerStream)
    }
}

/// How many times the run's record directory renamed a file to `name`.
fn renames_to(records: &Records, name: &str) -> usize {
    let log = records.lock().unwrap().as_ref().unwrap().log();
    log.iter()
        .filter(|line| *line == &format!("rename {name}") || line.starts_with(&format!("rename {name} ")))
        .count()
}

/// A oneshot whose final record's write is held at `op` (on `target`)
/// after its deadline, with no head write in flight then: its run returns
/// its timed-out result within its timeout and a second and hands back the
/// write, which goes on; nothing says the final failed while it may still
/// land. Once the write is let go the final is durable, published once,
/// the head removed, and the run's state points at it.
async fn held_final_write(op: Op, target: &'static str) {
    let store = fx::legacy();
    let r = write_root();
    let records: Records = Arc::default();
    let (gate, fault) = gate();
    let start = stalls_after_injecting(&records, op, target, fault);
    let (d, state, _cancel) = oneshot_drive(1);
    let phase = Arc::new(TerminalPhase::default());
    let started = std::time::Instant::now();
    let wrap = recorded(&records);
    let (outcome, settling) = tokio::time::timeout(
        Duration::from_secs(5),
        run_oneshot_worker(d, task(), plan(&r, &store.binding, None), start, &wrap, Arc::clone(&phase)),
    )
    .await
    .unwrap_or_else(|_| panic!("{op:?}: the oneshot's run waits for its held final write"));
    let took = started.elapsed();
    assert!(outcome.unwrap().timed_out);
    assert!(took < Duration::from_secs(2), "{op:?}: returns within its timeout and a second: {took:?}");
    let settling = settling.unwrap_or_else(|| panic!("{op:?}: the held write goes on"));
    assert!(phase.finalizing() && phase.detached());
    {
        let s = state.read().unwrap();
        assert!(s.v2_records);
        assert!(s.archive_path.is_none(), "{op:?}: not durable yet");
        assert_eq!(s.archive_error, None, "{op:?}: never reported failed while it may land");
    }
    tokio::time::timeout(Duration::from_secs(10), gate.reached)
        .await
        .expect("the held operation is the final's")
        .unwrap();
    gate.open.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(30), settling)
        .await
        .expect("the write ends once let go")
        .unwrap();
    let s = state.read().unwrap();
    assert!(s.archive_path.is_some(), "{op:?}: the state points at the final");
    assert_eq!(s.archive_error, None);
    assert!(s.v2_final_run.is_some());
    assert_eq!(v2_final(&r).status, "timed_out");
    assert!(head_on_disk(&r).is_none(), "{op:?}: the head removed after the final");
    assert_eq!(renames_to(&records, FINAL), 1, "{op:?}: the final is published once");
}

#[tokio::test]
async fn t1_a_held_final_rename_does_not_hold_a_oneshots_result() {
    held_final_write(Op::Rename, FINAL).await;
}

#[tokio::test]
async fn t2_a_held_final_directory_sync_does_not_hold_a_oneshots_result() {
    held_final_write(Op::SyncDir, "").await;
}

#[tokio::test]
async fn t3_a_held_head_removal_does_not_hold_a_oneshots_result() {
    held_final_write(Op::Remove, HEAD).await;
}

#[tokio::test]
async fn t7_a_held_final_temp_write_does_not_hold_a_oneshots_result() {
    held_final_write(Op::WriteTemp, "").await;
}

/// A oneshot whose final is written in time hands nothing back: the state
/// points at the final when the run returns, as a reactive run's does.
#[tokio::test]
async fn t4_a_final_written_in_time_is_settled_when_the_oneshot_returns() {
    let store = fx::legacy();
    let r = write_root();
    let (_started, start) = runtime(vec![text("done"), done()], || {});
    let (d, state, _cancel) = oneshot_drive(0);
    let phase = Arc::new(TerminalPhase::default());
    let (outcome, settling) = run_oneshot_worker(
        d,
        task(),
        plan(&r, &store.binding, None),
        move |o, _| start(o),
        &real,
        Arc::clone(&phase),
    )
    .await;
    assert_eq!(outcome.unwrap().text, "done");
    assert!(settling.is_none());
    assert!(phase.finalizing() && !phase.detached());
    assert!(state.read().unwrap().archive_path.is_some());
    assert_eq!(v2_final(&r).status, "completed");
}

/// A reactive worker's caller (not waiting for its result) still waits for
/// the run's terminal publication to its end, as before (§5: the same
/// primitive is tracked for 8e and slice 9).
#[tokio::test]
async fn t5_a_reactive_run_still_waits_for_its_final() {
    let store = fx::legacy();
    let r = write_root();
    let records: Records = Arc::default();
    let (gate, fault) = gate();
    let (d, state, cancel) = drive(1, &task());
    // Its worker ends once cancelled at the deadline (a reactive drain
    // waits for it much longer than a oneshot's).
    let start = {
        let records = Arc::clone(&records);
        move |_opening: Vec<SharedMessage>, _context: Option<WorkerStart>| {
            records.lock().unwrap().as_ref().unwrap().inject(Op::Rename, FINAL, fault);
            std::future::ready(Box::pin(
                futures::stream::iter(vec![text("partial answer")]).chain(futures::stream::once(
                    async move {
                        cancel.cancelled().await;
                        done()
                    },
                )),
            ) as WorkerStream)
        }
    };
    let wrap = recorded(&records);
    let plan = plan(&r, &store.binding, None);
    let run = async move { run_worker_configured_with(d, task(), Some(plan), start, &wrap).await };
    let mut run = Box::pin(run);
    tokio::select! {
        _ = &mut run => panic!("a reactive run returned before its final was written"),
        reached = gate.reached => reached.unwrap(),
    }
    tokio::select! {
        _ = &mut run => panic!("a reactive run returned before its final was written"),
        () = tokio::time::sleep(Duration::from_millis(1500)) => {}
    }
    gate.open.send(()).unwrap();
    assert!(run.await.unwrap().timed_out);
    assert!(state.read().unwrap().archive_path.is_some());
    assert_eq!(renames_to(&records, FINAL), 1);
}

/// The bound a oneshot waits for its run's terminal publication: the
/// deadline's grace, at least the minimum, at most the barrier wait.
#[test]
fn t6_a_oneshots_terminal_bound() {
    let now = tokio::time::Instant::now();
    let ms = Duration::from_millis;
    // Its drive drained its worker and ended 500 ms after the deadline.
    assert_eq!(terminal_bound(Some(now - ms(500)), now), now + ms(300));
    // A head write was in flight at the deadline and waited for.
    assert_eq!(terminal_bound(Some(now - ms(5_000)), now), now + TERMINAL_MIN);
    // It ended well before its deadline, or has none.
    assert_eq!(terminal_bound(Some(now + ms(600_000)), now), now + crate::tools::subagent::persist::BARRIER_WAIT);
    assert_eq!(terminal_bound(None, now), now + crate::tools::subagent::persist::BARRIER_WAIT);
    assert_eq!(terminal_bound(Some(now + ms(200)), now), now + ms(200) + TERMINAL_GRACE);
}

/// g7 (8g): owned-history tools that cannot be installed run the worker
/// with rollover off before anything is written, and the caller is told
/// why: the armed registry already holds a builtin of an owned name, or the
/// operator disabled one. A plan whose registry was never armed falls back
/// the same way. Slice 9 (e5): so does a resume admitted with a grant when
/// the operator disabled an owned name (its worker's owned registry lacks
/// it, and a v2 run's context management can't run without it).
#[tokio::test]
async fn g7_an_owned_name_collision_falls_back_before_any_write() {
    let store = fx::legacy();
    let support = worker_rollover_support(&store.binding).await.expect("a legacy store");
    let mut taken = crate::ToolRegistry::empty();
    crate::tools::subagent::retrieval::install_owned_history(
        &mut taken,
        OwnedHistoryGrant::fresh(store.binding.clone(), support.clone(), RunId::random()),
    )
    .unwrap();
    let installed = "rollover disabled for this worker: owned history tools could not be installed: ";
    let cases = [
        (
            "a builtin of an owned name",
            None,
            Some(WorkerTools {
                registry: Some(taken),
                disabled: Vec::new(),
            }),
            installed.to_owned(),
        ),
        (
            "an owned name disabled",
            None,
            Some(WorkerTools {
                registry: Some(crate::ToolRegistry::empty()),
                disabled: vec!["memory_fetch".into()],
            }),
            format!("{installed}the operator disabled memory_fetch"),
        ),
        (
            "a granted resume, an owned name disabled",
            Some(OwnedHistoryGrant::fresh(store.binding.clone(), support, RunId::random())),
            Some(WorkerTools {
                registry: None,
                disabled: vec!["memory_search".into()],
            }),
            format!("{installed}the operator disabled memory_search"),
        ),
        ("never armed", None, None, format!("{installed}the worker's registry was not armed")),
    ];
    for (why, grant, tools, notice) in cases {
        let r = write_root();
        let mut plan = plan(&r, &store.binding, None);
        plan.grant = grant;
        plan.tools = tools;
        let (started, start) = configured_runtime(vec![text("done"), done()]);
        let (d, state, _cancel) = drive(0, &task());
        run_worker_configured_with(d, task(), Some(plan), start, &real)
            .await
            .unwrap();
        let (_, start) = started.lock().unwrap().take().expect("the worker ran");
        assert_eq!(start, None, "{why}: rollover off");
        assert!(!r.dir.exists(), "{why}: nothing written");
        let s = state.read().unwrap();
        assert!(!s.v2_records, "{why}");
        let got = s.rollover_notice.clone().unwrap_or_default();
        assert!(got.starts_with(&notice), "{why}: {got}");
    }
}

mod visibility_gate {
    use super::*;
    include!("visibility_gate_tests.rs");
}

mod visibility {
    use super::*;
    include!("visibility_tests.rs");
}

mod optout_gate {
    include!("optout_gate_tests.rs");
}

mod optout {
    use super::*;
    include!("optout_tests.rs");
}
