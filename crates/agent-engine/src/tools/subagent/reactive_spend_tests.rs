// Child spend (P1f), included in the worker gate tests as `mod spend`. A
// reactive worker in a host with a sink books its requests through its own
// run's tap; a resume is a new run (a new nonce under the new handle); a
// worker a run delegates names that run as its parent; and a launch whose
// delegating run is sealed, or whose session is ending, is refused before
// it launches, leaving no owner entry, no handle and no slot behind.

use std::time::{Duration, Instant};

use crate::runtime::child_spend::{SinkMessage, SpendKind, SpendParent, SpendReceiver, SpendSink};

impl Parent {
    fn context_with(&self, spend: Option<SpendParent>) -> crate::tools::ToolContext {
        let mut ctx = self.context();
        ctx.capabilities.spend = spend;
        ctx
    }

    async fn start_with(&self, spend: Option<SpendParent>, task: &str) -> std::result::Result<String, String> {
        SubagentStartTool
            .execute(
                json!({"name": "g", "system_prompt": "You are a worker.", "task": task, "timeout": 30}),
                self.context_with(spend),
            )
            .await
            .map(|out| handle_of(&out))
            .map_err(|e| e.to_string())
    }

    async fn resume_with(&self, spend: Option<SpendParent>, handle_id: &str) -> std::result::Result<String, String> {
        crate::tools::subagent::SubagentResumeTool
            .execute(
                json!({"handle_id": handle_id, "instructions": "Go on.", "timeout": 30}),
                self.context_with(spend),
            )
            .await
            .map(|out| handle_of(&out))
            .map_err(|e| e.to_string())
    }
}

/// The session's messages up to and including the first run terminal,
/// waiting at most 20 s for it.
async fn until_terminal(rx: &mut SpendReceiver) -> Vec<SinkMessage> {
    let mut out = Vec::new();
    let waited = Instant::now();
    loop {
        while let Ok(message) = rx.try_recv() {
            let terminal = matches!(
                message,
                SinkMessage::RunSealed { .. } | SinkMessage::RunEnded { .. }
            );
            out.push(message);
            if terminal {
                return out;
            }
        }
        assert!(waited.elapsed() < Duration::from_secs(20), "the run ends: {out:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// One answered request booked under worker `handle`'s run (under
/// `parent`), then the run's end. Returns the run's nonce.
fn assert_one_request(messages: &[SinkMessage], handle: &str, parent: Option<&str>) -> u64 {
    let [SinkMessage::Spend { run, seq: 0, report }, SinkMessage::RunEnded { run: ended, next_seq: 1 }] =
        messages
    else {
        panic!("one report, then the run's end: {messages:?}");
    };
    assert_eq!(run.handle, handle);
    assert_eq!(run.parent_handle.as_deref(), parent);
    assert_eq!(run.agent, "g");
    assert_eq!(ended, run, "the same run ends");
    assert_eq!(report.kind, SpendKind::Turn);
    assert_eq!((report.tokens.input, report.tokens.output), (12, 3), "as the stand-in reported");
    assert!(report.model.ends_with("claude-sonnet-4-6"), "{report:?}");
    run.run_nonce
}

/// A started worker books its request under its own run, which ends with
/// the worker; resumed, it books under a new run of the new handle.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn rs1_a_reactive_worker_books_under_its_run_and_a_resume_is_a_new_run() {
    let release = Arc::new(tokio::sync::Notify::new());
    let url = stand_in(Arc::clone(&release)).await;
    let _scene = scene(&url);
    let parent = Parent::new();
    let (sink, mut rx) = SpendSink::new();
    let session = || Some(SpendParent::Session(sink.clone()));

    let a = parent.start_with(session(), "task A: answer at once").await.expect("starts");
    parent.finished(&a).await;
    let first = assert_one_request(&until_terminal(&mut rx).await, &a, None);
    assert_completed(&parent.reconcile(&a).await, "A");

    let b = parent.resume_with(session(), &a).await.expect("resumes");
    assert_ne!(b, a, "a resume is a new handle");
    parent.finished(&b).await;
    let second = assert_one_request(&until_terminal(&mut rx).await, &b, None);
    assert_ne!(second, first, "a resume is a new run");
    assert_completed(&parent.reconcile(&b).await, "B");
    assert_eq!(sink.producers(), 0);
    assert!(rx.try_recv().is_err(), "nothing more");
}

/// A worker that a run delegates books under its own run, which names the
/// delegating run as its parent; the delegating run books nothing for it.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn rs2_a_delegated_worker_books_once_under_its_own_run() {
    let release = Arc::new(tokio::sync::Notify::new());
    let url = stand_in(Arc::clone(&release)).await;
    let _scene = scene(&url);
    let parent = Parent::new();
    let (sink, mut rx) = SpendSink::new();
    let outer = sink.lease("sa_outer", "outer").unwrap();

    let a = parent
        .start_with(Some(SpendParent::Run(outer.clone())), "task A: answer at once")
        .await
        .expect("starts");
    parent.finished(&a).await;
    assert_one_request(&until_terminal(&mut rx).await, &a, Some("sa_outer"));
    assert_completed(&parent.reconcile(&a).await, "A");
    assert!(outer.is_live());
    drop(outer);
    match rx.try_recv() {
        Ok(SinkMessage::RunEnded { run, next_seq: 0 }) => assert_eq!(run.handle, "sa_outer"),
        other => panic!("the delegating run ends with no report of its own: {other:?}"),
    }
}

/// A start or resume whose delegating run is sealed, or whose session is
/// ending, is refused before launch: no owner entry, no handle, no request,
/// no slot (the next start is admitted under max_concurrent 1).
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn rs3_a_refused_lease_leaves_nothing_behind() {
    let release = Arc::new(tokio::sync::Notify::new());
    let url = stand_in(Arc::clone(&release)).await;
    let _scene = scene(&url);
    let parent = Parent::with_concurrency(1);
    let a = parent.start("task A: answer at once").await;
    parent.finished(&a).await;
    assert_completed(&parent.reconcile(&a).await, "A");
    let owners = parent.policy.owner_entries();

    let (sink, mut rx) = SpendSink::new();
    let sealed = sink.lease("sa_sealed", "outer").unwrap();
    assert!(sealed.seal());
    let (closing, _closing_rx) = SpendSink::new();
    closing.begin_closing();
    for _ in 0..10 {
        for (spend, reason) in [
            (SpendParent::Run(sealed.clone()), "parent run sealed"),
            (SpendParent::Session(closing.clone()), "session ending"),
        ] {
            let err = parent.start_with(Some(spend.clone()), "task C").await.expect_err("refused");
            assert!(err.contains(reason), "{err}");
            let err = parent.resume_with(Some(spend), &a).await.expect_err("refused");
            assert!(err.contains(reason), "{err}");
        }
    }
    assert_eq!(parent.policy.owner_entries(), owners, "a refused lease leaves no owner");
    assert_eq!(parent.policy.delegation_descendants(), 0);
    assert!(parent.policy.unreconciled_runtime_handles().is_empty());
    assert_eq!(closing.producers(), 0);

    let next = parent
        .start_with(Some(SpendParent::Session(sink.clone())), "task D: answer at once")
        .await
        .expect("admitted under max_concurrent 1");
    parent.finished(&next).await;
    let sealed_end = until_terminal(&mut rx).await;
    assert!(
        matches!(&sealed_end[..], [SinkMessage::RunSealed { next_seq: 0, .. }]),
        "{sealed_end:?}"
    );
    assert_one_request(&until_terminal(&mut rx).await, &next, None);
    assert_completed(&parent.reconcile(&next).await, "D");
}
