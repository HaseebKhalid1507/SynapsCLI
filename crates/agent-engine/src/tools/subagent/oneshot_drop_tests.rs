// Included in the oneshot gate as `mod dropped`: a oneshot whose parent turn
// is cancelled while it waits. A cancelled turn drops the tool call
// (`await_tool_call` selects on the turn's cancel), so the call's own code
// never runs again. These tests drop the real `subagent` call against a
// loopback stand-in and check what is left behind.
//
// The base dir and the provider URL are process-wide, and a test elsewhere
// may send a request or write a log while one of these runs. So the
// stand-in answers by the task in the request, the tests count only their
// own requests and look only for their own log.

use std::time::{Duration, Instant};

use super::super::reap::{exited, finished};
use crate::orchestration::OrchestrationRuntime;
use crate::StreamEvent;

/// A policy that admits `concurrent` workers at a time.
fn policy(concurrent: usize) -> Arc<OrchestrationRuntime> {
    let foreground = agent_core::prompt::QualifiedModelId::parse("anthropic/claude-sonnet-4-6").unwrap();
    Arc::new(OrchestrationRuntime::baseline(foreground, concurrent, 64).unwrap())
}

/// The task of a call whose request the stand-in holds open.
const HOLD: &str = "Wait for the drop test.";
/// The task of a call the stand-in answers ("gate ok").
const ANSWER: &str = "Answer the drop test.";

fn params(task: &str) -> Value {
    json!({
        "name": "dropped",
        "system_prompt": "You are the drop test's worker.",
        "task": task,
        "timeout": 30,
    })
}

fn asks(body: &Bytes, task: &str) -> bool {
    String::from_utf8_lossy(body).contains(task)
}

/// A stand-in that records every request and answers each by its body.
async fn stand_in_by(reply: impl Fn(&Bytes) -> Response + Clone + Send + Sync + 'static) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let record = Arc::clone(&seen);
    let app = Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
        let record = Arc::clone(&record);
        let reply = reply.clone();
        async move {
            let response = reply(&body);
            record.lock().unwrap().push(Received { method, uri, headers, body });
            response
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), seen)
}

/// Held for a HOLD call, answered in text for anything else.
fn hold_or_answer(body: &Bytes) -> Response {
    if asks(body, HOLD) {
        held_reply(0)
    } else {
        text_reply(0)
    }
}

/// The base dir with a stand-in Anthropic login, and the provider pointed
/// at the stand-in.
struct Scene {
    base: crate::test_env::BaseDirGuard,
    _url: crate::test_env::EnvVarGuard,
    seen: Seen,
    policy: Arc<OrchestrationRuntime>,
}

async fn scene(
    concurrent: usize,
    reply: impl Fn(&Bytes) -> Response + Clone + Send + Sync + 'static,
) -> Scene {
    let base = crate::test_env::BaseDirGuard::new();
    std::fs::write(
        base.path().join("auth.json"),
        json!({"anthropic":{"type":"oauth","refresh":"gate-refresh","access":"gate-access","expires":9_999_999_999_999u64}})
            .to_string(),
    )
    .unwrap();
    let (url, seen) = stand_in_by(reply).await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    Scene {
        base,
        _url,
        seen,
        policy: policy(concurrent),
    }
}

impl Scene {
    /// A tool context on the scene's policy, its events into a channel.
    fn context(&self) -> (crate::tools::ToolContext, tokio::sync::mpsc::UnboundedReceiver<StreamEvent>) {
        let mut ctx = crate::tools::test_helpers::create_tool_context();
        ctx.capabilities.orchestration = Some(Arc::clone(&self.policy));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.channels.tx_events = Some(tx);
        (ctx, rx)
    }

    /// Wait for the stand-in to receive a request with `task`.
    async fn wait_for(&self, task: &str) {
        let waited = Instant::now();
        while !self.seen.lock().unwrap().iter().any(|r| asks(&r.body, task)) {
            assert!(waited.elapsed() < Duration::from_secs(30), "the worker sends its request");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Wait (at most `bound`) for worker `id`'s handle to end.
    async fn ended_within(&self, id: &str, bound: Duration) -> bool {
        let waited = Instant::now();
        while self.policy.is_unreconciled(id) {
            if waited.elapsed() > bound {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        true
    }

    fn pending(&self) -> Vec<String> {
        let registry = Arc::new(Mutex::new(crate::runtime::subagent::SubagentRegistry::new()));
        crate::runtime::subagent::pending_workers(&registry, Some(&self.policy))
            .into_iter()
            .map(|w| format!("{} ({})", w.id, w.state))
            .collect()
    }

    /// The subagent logs written for this file's calls ("dropped").
    fn logs(&self) -> Vec<String> {
        std::fs::read_dir(self.base.path().join("logs").join("subagents"))
            .map(|dir| dir.map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_else(|_| Vec::new())
            .into_iter()
            .filter(|name: &String| name.contains("dropped"))
            .collect()
    }
}

/// Everything the channel holds now.
fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<StreamEvent>) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

/// The orchestration id of the oneshot that announced itself in `events`.
fn started(events: &[StreamEvent]) -> String {
    events
        .iter()
        .find_map(|e| match e {
            StreamEvent::Agent(crate::AgentEvent::SubagentStart { subagent_id, .. }) => {
                Some(format!("sa_{subagent_id}"))
            }
            _ => None,
        })
        .expect("the oneshot announced itself")
}

/// What a finished call publishes to its parent, if any is in `events`.
fn published(events: &[StreamEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|e| {
            matches!(
                e,
                StreamEvent::Session(crate::SessionEvent::Usage { .. })
                    | StreamEvent::Agent(crate::AgentEvent::SubagentDone { .. })
            )
        })
        .map(|e| format!("{e:?}"))
        .collect()
}

/// A dropped oneshot: the call is dropped while its worker's request is
/// held, as a cancelled turn drops it.
struct Dropped {
    scene: Scene,
    id: String,
    events: tokio::sync::mpsc::UnboundedReceiver<StreamEvent>,
    pending_before: Vec<String>,
}

async fn dropped(concurrent: usize) -> Dropped {
    let scene = scene(concurrent, hold_or_answer).await;
    let (ctx, mut events) = scene.context();
    let call = tokio::spawn(async move { SubagentTool.execute(params(HOLD), ctx).await });
    scene.wait_for(HOLD).await;
    let id = started(&drain(&mut events));
    assert!(scene.policy.is_unreconciled(&id), "the worker is running");
    let pending_before = scene.pending();
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled(), "the call was dropped, not finished");
    Dropped {
        scene,
        id,
        events,
        pending_before,
    }
}

/// The regression: once the dropped call's worker has stopped, its handle
/// has ended and the parent admits new work under max_concurrent 1.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn d1_a_dropped_oneshot_ends_its_handle_and_frees_its_slot() {
    let d = dropped(1).await;
    assert!(
        d.scene.ended_within(&d.id, Duration::from_secs(15)).await,
        "the dropped oneshot's handle {} is still unreconciled: {:?}",
        d.id,
        d.scene.policy.unreconciled_runtime_handles()
    );
    assert_eq!(finished::count(&d.id), 1, "ended exactly once");

    let (ctx, mut events) = d.scene.context();
    let next = SubagentTool.execute(params(ANSWER), ctx).await;
    let next = next.expect("the parent admits the next oneshot under max_concurrent 1");
    assert_eq!(next, "[subagent:dropped] gate ok");
    let next_id = started(&drain(&mut events));
    assert_eq!(finished::count(&next_id), 0, "a call that got its result ends its own handle");
    assert!(d.scene.policy.unreconciled_runtime_handles().is_empty());
}

/// The dropped call publishes nothing to its parent: no Usage, no
/// SubagentDone and no subagent log, also after its handle has ended.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn d2_a_dropped_oneshot_publishes_no_usage_done_or_log() {
    let mut d = dropped(1).await;
    assert!(
        d.scene.ended_within(&d.id, Duration::from_secs(15)).await,
        "the dropped oneshot's handle {} has ended",
        d.id
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(published(&drain(&mut d.events)), Vec::<String>::new());
    assert_eq!(d.scene.logs(), Vec::<String>::new(), "no subagent log for the dropped call");
}

/// A rollover's pending-worker manifest lists the running oneshot (as
/// "expired, not reconciled": a oneshot has no reactive handle) and no
/// longer lists it once the dropped call's handle has ended.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn d3_the_rollover_manifest_no_longer_lists_a_dropped_oneshot() {
    let d = dropped(1).await;
    assert_eq!(d.pending_before, vec![format!("{} (expired, not reconciled)", d.id)]);
    let waited = Instant::now();
    while !d.scene.pending().is_empty() && waited.elapsed() < Duration::from_secs(15) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(d.scene.pending(), Vec::<String>::new(), "a dropped oneshot is not carried as pending");
}

/// The race: the worker has finished and sent its result, but the call is
/// dropped before it is polled again, so the call's own finish never runs.
/// The handle still ends, exactly once, at the drop, and nothing is
/// published.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn d4_a_drop_after_the_worker_finished_ends_the_handle_once() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let reply = {
        let gate = Arc::clone(&gate);
        move |body: &Bytes| {
            if !asks(body, ANSWER) {
                return text_reply(0);
            }
            let gate = Arc::clone(&gate);
            let body = sse_events(&[
                message_start(0),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"gate ok"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":usage()}),
                json!({"type":"message_stop"}),
            ]);
            let stream = futures::stream::once(async move {
                let _open = gate.acquire().await.unwrap();
                Ok::<_, std::io::Error>(Bytes::from(body))
            });
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
    };
    let scene = scene(1, reply).await;
    let (ctx, mut events) = scene.context();
    let mut call = Box::pin(SubagentTool.execute(params(ANSWER), ctx));
    let waited = Instant::now();
    while !scene.seen.lock().unwrap().iter().any(|r| asks(&r.body, ANSWER)) {
        assert!(futures::poll!(call.as_mut()).is_pending(), "the call waits for its worker");
        assert!(waited.elapsed() < Duration::from_secs(30), "the worker sends its request");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let id = started(&drain(&mut events));
    // Let the reply through and wait until the worker thread has sent its
    // result and taken its last act; the call is not polled again.
    gate.add_permits(1);
    let waited = Instant::now();
    while !exited::seen(&id) {
        assert!(waited.elapsed() < Duration::from_secs(30), "the worker finishes");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(scene.policy.is_unreconciled(&id), "only the call ends the handle while it lives");
    drop(call);

    assert!(
        !scene.policy.is_unreconciled(&id),
        "a worker that had finished is ended at the drop"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(finished::count(&id), 1, "exactly once");
    assert_eq!(published(&drain(&mut events)), Vec::<String>::new());
    assert_eq!(scene.logs(), Vec::<String>::new());
}

// ── This reaper and #255's `Drop for SubagentHandle` (c2f665b0) ─────────────
//
// Both act when something is dropped under a running worker, on different
// records. The reaper ends a oneshot's handle in the delegation policy
// (Failed, "parent turn cancelled"). `Drop for SubagentHandle` flags
// `cancel_requested` on a registry worker's state, so that worker's own
// finalizer labels it cancelled; it ends nothing in the policy. A oneshot
// never registers a `SubagentHandle`, and only a oneshot arms the reaper:
// these two tests check that neither ends or relabels the other's worker.

/// A dropped oneshot whose context carries a subagent registry: the
/// registry stays empty (no handle there for #255's drop to flag), the
/// reaper ends the policy handle once, and a second end is refused.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn d5_a_dropped_oneshot_leaves_the_registry_alone_and_ends_once() {
    let scene = scene(1, hold_or_answer).await;
    let registry = Arc::new(Mutex::new(crate::runtime::subagent::SubagentRegistry::new()));
    let (mut ctx, mut events) = scene.context();
    ctx.capabilities.subagent_registry = Some(Arc::clone(&registry));
    let call = tokio::spawn(async move { SubagentTool.execute(params(HOLD), ctx).await });
    scene.wait_for(HOLD).await;
    let id = started(&drain(&mut events));
    assert_eq!(registry.lock().unwrap().iter_mut_handles().count(), 0, "a running oneshot has no registry handle");
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled(), "the call was dropped, not finished");
    assert!(
        scene.ended_within(&id, Duration::from_secs(15)).await,
        "the dropped oneshot's handle {id} has ended"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(finished::count(&id), 1, "the reaper ended it once");
    assert_eq!(registry.lock().unwrap().iter_mut_handles().count(), 0, "still no registry handle");
    assert!(
        scene
            .policy
            .finish_one_shot(&id, agent_core::orchestration::WorkerTerminal::Failed)
            .is_err(),
        "an ended handle cannot be ended (or relabelled) again"
    );
}

/// A registry worker's handle dropped while it runs (#255): its state is
/// flagged cancelled for its own finalizer, and the oneshot reaper does
/// not touch it: its policy handle is still open, waiting for that
/// finalizer, and the reaper never counted it.
#[test]
fn d6_a_dropped_registry_worker_is_flagged_and_never_reaped() {
    let id = "sa_reaper_vs_registry_drop";
    let policy = policy(1);
    policy.resolve_and_authorize(id, None).unwrap();
    policy.mark_starting(id).unwrap();
    policy.mark_running(id).unwrap();
    let state = Arc::new(std::sync::RwLock::new(crate::runtime::subagent::SubagentState::new()));
    let (steer_tx, _steer_rx) = tokio::sync::mpsc::unbounded_channel();
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let (_result_tx, result_rx) = tokio::sync::oneshot::channel();
    let handle = crate::runtime::subagent::SubagentHandle::new(
        id.to_string(),
        0,
        "dropped".to_string(),
        "test task".to_string(),
        "claude-sonnet-4-6".to_string(),
        "system prompt".to_string(),
        30,
        Arc::clone(&state),
        Some(steer_tx),
        Some(shutdown_tx),
        Some(result_rx),
    );
    let mut registry = crate::runtime::subagent::SubagentRegistry::new();
    registry.register(handle);
    drop(registry);
    assert!(state.read().unwrap().cancel_requested, "#255: a killed run is a cancel");
    assert!(policy.is_unreconciled(id), "the drop ends nothing in the policy");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(finished::count(id), 0, "the oneshot reaper never ends a registry worker");
    assert!(policy.is_unreconciled(id));
}
