// Slice 9 item (a): the fallback notice end to end (spec §6 "The caller is
// told", §12 item 9). A launch that falls back to rollover off tells its
// caller why in one bounded line, "rollover disabled for this worker:
// <reason>", wherever the caller looks while the handle lives:
// subagent_status (running and finished), subagent_collect (running and
// terminal) and the completion event (its text and its data); a oneshot says
// it after its output. A reaped handle's collect and status read its v1
// archive, which carries no notice (a known limit, §6).
//
// The case is the live sidecar's at enablement: an Axel service with the
// older capabilities (no `history_owned`, lineage_tests a8), on which the
// memory binding cannot confine retrieval to a lineage. The parent's
// settings are `auto` and resolve for the worker's model, so that is the
// reason the launch gives. Each case runs the real tools on a real registry
// and orchestration policy, the worker runtimes built as in production,
// against a loopback Anthropic stand-in. Since slice 9 (its `e4`) the cases
// call the tools' production entries (`execute`), with worker rollover on:
// the parent's auto settings make each run want a plan, and each tool puts
// its records at this process's place under a scratch base itself.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Router;
use serde_json::{json, Value};

use super::record::{Candidate, FinalRecord, RecordKind};
use super::record_file;
use super::test_fixtures::{older_capabilities, FakeAxel};
use crate::runtime::subagent::SubagentRegistry;
use crate::tools::{Tool, ToolContext};
use crate::{AgentEvent, SessionEvent, StreamEvent};

/// What every surface says of a worker that fell back on an older Axel.
const NOTICE: &str =
    "rollover disabled for this worker: the memory binding cannot confine retrieval to a lineage";

/// The worker's model: one whose route books its requests (§6), so the
/// launch gets as far as the memory binding.
const MODEL: &str = "anthropic/claude-sonnet-4-6";

// ── The stand-in ────────────────────────────────────────────────────────

/// A loopback Anthropic stand-in. Conversation request `n` (from 0, across
/// the test) is answered "done" once it is released ([`Stand::release`]),
/// so a worker can be looked at while it is running.
#[derive(Clone)]
struct Stand {
    /// Requests that have arrived so far.
    arrived: Arc<tokio::sync::watch::Sender<usize>>,
    /// Requests below this are answered.
    released: Arc<tokio::sync::watch::Sender<usize>>,
}

impl Stand {
    /// Wait until request `n` has arrived: its worker's launch is decided
    /// (the launch comes before the first inference).
    async fn arrived(&self, n: usize) {
        let mut arrived = self.arrived.subscribe();
        tokio::time::timeout(Duration::from_secs(60), arrived.wait_for(|a| *a > n))
            .await
            .unwrap_or_else(|_| panic!("request {n} arrives"))
            .unwrap();
    }

    /// Answer request `n` (and every one before it).
    fn release(&self, n: usize) {
        self.released.send_modify(|r| *r = (*r).max(n + 1));
    }

    /// Answer every request at once.
    fn release_all(&self) {
        self.released.send_replace(usize::MAX);
    }

    fn requests(&self) -> usize {
        *self.arrived.borrow()
    }
}

fn sse_events(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

/// A reply that answers "done" and ends the turn.
fn done_reply(n: usize) -> Response {
    let usage = json!({"input_tokens":12,"output_tokens":3,"cache_read_input_tokens":0,"cache_creation_input_tokens":0});
    let body = sse_events(&[
        json!({"type":"message_start","message":{"id":format!("msg_notice_{n}"),"type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":usage}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":usage}),
        json!({"type":"message_stop"}),
    ]);
    (StatusCode::OK, [("content-type", "text/event-stream")], body).into_response()
}

async fn answer(State(s): State<Stand>) -> Response {
    let mut n = 0;
    s.arrived.send_modify(|a| {
        n = *a;
        *a += 1;
    });
    let mut released = s.released.subscribe();
    let _ = released.wait_for(|r| *r > n).await;
    done_reply(n)
}

async fn stand_in() -> (String, Stand) {
    let stand = Stand {
        arrived: Arc::new(tokio::sync::watch::channel(0).0),
        released: Arc::new(tokio::sync::watch::channel(0).0),
    };
    let app = Router::new().fallback(answer).with_state(stand.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), stand)
}

// ── The parent ──────────────────────────────────────────────────────────

/// The parent's live settings: `auto`, its defaults.
fn auto_policy() -> crate::tools::WorkerContextPolicy {
    crate::tools::WorkerContextPolicy {
        context_management: agent_core::config::ContextManagementConfig {
            mode: agent_core::config::ContextManagementMode::Auto,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// One parent and its world: a scratch base with a stored Anthropic login
/// (a stand-in credential, never refreshed) that the worker's broker reads,
/// the stand-in, the older Axel the parent's memory is bound to, and the
/// parent's worker registry, orchestration policy and event queue, shared by
/// every tool call it makes (as one session's are).
struct Parent {
    base: crate::test_env::BaseDirGuard,
    stand: Stand,
    _url: crate::test_env::EnvVarGuard,
    axel: FakeAxel,
    registry: Arc<Mutex<SubagentRegistry>>,
    policy: Arc<crate::orchestration::OrchestrationRuntime>,
    queue: Arc<crate::events::EventQueue>,
}

impl Parent {
    async fn new() -> Self {
        let base = crate::test_env::BaseDirGuard::new();
        std::fs::write(
            base.path().join("auth.json"),
            json!({"anthropic":{"type":"oauth","refresh":"notice-refresh","access":"notice-access","expires":9_999_999_999_999u64}})
                .to_string(),
        )
        .unwrap();
        let (url, stand) = stand_in().await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let axel = FakeAxel::new(json!({"capabilities": older_capabilities()}));
        let policy = crate::tools::test_helpers::create_tool_context()
            .capabilities
            .orchestration
            .expect("the test context has an orchestration policy");
        // The parent's settings resolve for the worker: the fallback is the
        // binding's, not theirs.
        assert!(super::launch::worker_context(Some(&auto_policy()), MODEL).is_ok());
        Self {
            base,
            stand,
            _url,
            axel,
            registry: Arc::new(Mutex::new(SubagentRegistry::new())),
            policy,
            queue: Arc::new(crate::events::EventQueue::new(16)),
        }
    }

    /// A tool context of this parent: its registry, policy, completion
    /// queue, memory binding and live settings.
    fn context(&self) -> ToolContext {
        let mut ctx = crate::tools::test_helpers::create_tool_context();
        ctx.capabilities.subagent_registry = Some(Arc::clone(&self.registry));
        ctx.capabilities.orchestration = Some(Arc::clone(&self.policy));
        ctx.capabilities.event_queue = Some(Arc::clone(&self.queue));
        ctx.capabilities.memory_backend = Some(self.axel.binding.clone());
        ctx.capabilities.worker_context = Some(auto_policy());
        ctx
    }

    /// subagent_start through its production entry; its handle.
    async fn start(&self) -> String {
        let reply = super::SubagentStartTool
            .execute(
                json!({
                    "name": "notice",
                    "system_prompt": "You are the notice test's worker.",
                    "task": "Say done.",
                    "model": MODEL,
                    "timeout": 120,
                }),
                self.context(),
            )
            .await
            .expect("the worker starts");
        handle_of(&reply)
    }

    /// subagent_resume of `prior` through its production entry (the
    /// archive root, the parent's binding); the new handle.
    async fn resume(&self, prior: &str) -> String {
        let reply = super::SubagentResumeTool
            .execute(
                json!({"handle_id": prior, "instructions": "Say done again.", "timeout": 120}),
                self.context(),
            )
            .await
            .expect("the worker resumes");
        handle_of(&reply)
    }

    /// `tool`'s body for `params`.
    async fn body(&self, tool: &dyn Tool, params: Value) -> Value {
        let reply = tool
            .execute(params, self.context())
            .await
            .unwrap_or_else(|e| panic!("{}: {e}", tool.name()));
        serde_json::from_str(&reply).unwrap_or_else(|e| panic!("{}: {reply}: {e}", tool.name()))
    }

    async fn status(&self, handle: &str) -> Value {
        self.body(&super::SubagentStatusTool, json!({"handle_id": handle}))
            .await
    }

    async fn collect(&self, handle: &str) -> Value {
        self.body(
            &super::SubagentCollectTool,
            json!({"handle_id": handle, "reconciled": true}),
        )
        .await
    }

    /// Wait until `handle`'s worker thread has ended (its finalizer has
    /// run).
    async fn finished(&self, handle: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let done = self
                .registry
                .lock()
                .unwrap()
                .get(handle)
                .and_then(|h| h.thread_handle.as_ref().map(|t| t.is_finished()))
                .expect("the thread handle is attached before the tool returns");
            if done {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "{handle} ends");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The handle's surfaces while its worker is held at its first request:
    /// status and collect say it is running, with the notice and nothing
    /// of a v2 run.
    async fn assert_running(&self, handle: &str) {
        let status = self.status(handle).await;
        assert_eq!(status["status"], "running", "{status}");
        assert_notice(&status);
        let collect = self.collect(handle).await;
        assert_eq!(collect["status"], "running", "{collect}");
        assert_notice(&collect);
    }

    /// The handle's completion event, its only one: its text and its data
    /// carry the notice; `resumed_from` names the run it continues.
    fn assert_completion(&self, handle: &str, resumed_from: Option<&str>) {
        let event = self.queue.pop().expect("a completion event");
        assert!(self.queue.pop().is_none(), "one completion event");
        let text = &event.content.text;
        assert!(
            text.starts_with(&format!(
                "Subagent 'notice' ({handle}) finished with status 'completed' after "
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!(" Note: {NOTICE}. Call subagent_collect with handle_id")),
            "{text}"
        );
        let data = event.content.data.expect("completion data");
        assert_eq!(data["handle_id"], handle, "{data}");
        assert_eq!(data["status"], "completed", "{data}");
        assert_eq!(data["rollover_notice"], NOTICE, "{data}");
        assert_eq!(data.get("resumed_from").and_then(Value::as_str), resumed_from, "{data}");
        for key in ["rollover_windows", "rollover_notices", "rollover_notices_dropped"] {
            assert!(data.get(key).is_none(), "{key} in {data}");
        }
    }

    /// The finished handle's surfaces: status and the terminal collect say
    /// it completed, with its output and the notice.
    async fn assert_finished(&self, handle: &str) {
        let status = self.status(handle).await;
        assert_eq!(status["status"], "completed", "{status}");
        assert_notice(&status);
        let collect = self.collect(handle).await;
        assert_eq!(collect["status"], "completed", "{collect}");
        assert_eq!(collect["output"], "done", "{collect}");
        assert_notice(&collect);
    }

    /// Nothing but these handles' v1 archives is in this process's records
    /// directory: no head, no v2 final, no lock.
    fn assert_only_v1_archives(&self, handles: &[&str]) {
        let dir = super::archive::process_dir();
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        let mut want: Vec<String> = handles.iter().map(|h| format!("{h}.json")).collect();
        want.sort();
        assert_eq!(names, want, "{}", dir.display());
        for handle in handles {
            let location = record_file::location_of(
                &super::archive::archive_root(),
                super::archive::process_dir_name(),
                handle,
                RecordKind::Final,
            )
            .unwrap();
            let loaded = record_file::load(&location).unwrap();
            assert!(matches!(loaded.head, Candidate::Absent), "{handle}: no head record");
            assert!(
                matches!(loaded.final_record, Candidate::Present(FinalRecord::V1(_))),
                "{handle}: its record is a v1 archive"
            );
        }
    }

    /// The older Axel was only asked for its capabilities.
    fn assert_only_capabilities_asked(&self) {
        let ops = self.axel.ops();
        assert!(!ops.is_empty(), "the launch asked the binding");
        assert!(ops.iter().all(|op| op == "capabilities"), "{ops:?}");
    }
}

fn handle_of(reply: &str) -> String {
    let value: Value = serde_json::from_str(reply).unwrap_or_else(|e| panic!("{reply}: {e}"));
    value["handle_id"].as_str().expect("a handle").to_owned()
}

/// A status or collect body of a fallback: the notice, and none of a v2
/// run's counters or notices.
fn assert_notice(body: &Value) {
    assert_eq!(body["rolloverNotice"], NOTICE, "{body}");
    for key in ["rolloverWindows", "rolloverNotices", "rolloverNoticesDropped"] {
        assert!(body.get(key).is_none(), "{key} in {body}");
    }
}

// ── Reactive workers ────────────────────────────────────────────────────

/// n1: a started worker that falls back to rollover off shows the notice
/// in subagent_status and subagent_collect while it runs and once it has
/// finished, and in its completion event's text and data; nothing of a v2
/// run is written, and its v1 archive is.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn n1_a_start_that_falls_back_shows_the_notice_in_status_collect_and_completion() {
    let parent = Parent::new().await;
    let first = parent.start().await;
    parent.stand.arrived(0).await;
    parent.assert_running(&first).await;
    parent.stand.release(0);
    parent.finished(&first).await;
    parent.assert_completion(&first, None);
    parent.assert_finished(&first).await;
    assert_eq!(parent.stand.requests(), 1);
    parent.assert_only_v1_archives(&[&first]);
    parent.assert_only_capabilities_asked();
}

/// n2: a resume of that worker falls back the same way, and its new handle
/// shows the notice on every surface; its own v1 archive is written beside
/// the first, and nothing of a v2 run.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn n2_a_resume_of_that_worker_shows_it_on_its_new_handle() {
    let parent = Parent::new().await;
    let first = parent.start().await;
    parent.stand.release(0);
    parent.finished(&first).await;
    parent.assert_completion(&first, None);
    parent.assert_finished(&first).await;

    let second = parent.resume(&first).await;
    assert_ne!(second, first);
    parent.stand.arrived(1).await;
    parent.assert_running(&second).await;
    parent.stand.release(1);
    parent.finished(&second).await;
    parent.assert_completion(&second, Some(&first));
    parent.assert_finished(&second).await;
    assert_eq!(parent.stand.requests(), 2);
    parent.assert_only_v1_archives(&[&first, &second]);
    parent.assert_only_capabilities_asked();
}

// ── The oneshot ─────────────────────────────────────────────────────────

/// What one oneshot returned and sent its parent.
struct Oneshot {
    result: String,
    events: Vec<StreamEvent>,
}

impl Parent {
    /// The oneshot to its end through its production entry, with the
    /// caller's `context_rollover` (`rollover`; `None` leaves it out).
    async fn oneshot(&self, rollover: Option<&str>) -> Oneshot {
        let mut ctx = self.context();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.channels.tx_events = Some(tx);
        let mut params = json!({
            "name": "notice",
            "system_prompt": "You are the notice test's worker.",
            "task": "Say done.",
            "model": MODEL,
            "timeout": 120,
        });
        if let Some(rollover) = rollover {
            params["context_rollover"] = json!(rollover);
        }
        let result = super::SubagentTool
            .execute(params, ctx)
            .await
            .expect("a oneshot returns its outcome as text");
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        Oneshot { result, events }
    }

    /// The oneshot log files written so far, read and removed.
    fn take_logs(&self) -> String {
        let dir = self.base.path().join("logs").join("subagents");
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .map(|d| d.map(|e| e.unwrap().path()).collect())
            .unwrap_or_default();
        entries.sort();
        let mut log = String::new();
        for path in entries {
            log.push_str(&std::fs::read_to_string(&path).unwrap());
            std::fs::remove_file(&path).unwrap();
        }
        log
    }
}

impl Oneshot {
    fn usage_events(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Session(SessionEvent::Usage { .. })))
            .count()
    }

    fn done_previews(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Agent(AgentEvent::SubagentDone { result_preview, .. }) => {
                    Some(result_preview.clone())
                }
                _ => None,
            })
            .collect()
    }
}

/// n3: a oneshot whose run falls back to rollover off answers as one
/// without a plan (here: its caller's opt-out), then says why after its
/// output, in its result, its completion preview and its log; it writes no
/// records.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn n3_a_oneshot_that_falls_back_says_why_after_its_output() {
    let parent = Parent::new().await;
    parent.stand.release_all();
    let unplanned = parent.oneshot(Some("off")).await;
    let unplanned_log = parent.take_logs();
    let planned = parent.oneshot(None).await;
    let planned_log = parent.take_logs();
    let trailer = format!("\n\n[rollover]: {NOTICE}");
    assert_eq!(planned.result, format!("{}{trailer}", unplanned.result));
    assert_eq!(planned.usage_events(), 1);
    assert_eq!(planned.done_previews(), [format!("done{trailer}")]);
    assert!(unplanned_log.contains("## Result\n\ndone\n"), "{unplanned_log}");
    assert!(planned_log.contains(&format!("## Result\n\ndone{trailer}\n")), "{planned_log}");
    assert_eq!(parent.stand.requests(), 2);
    let dir = super::archive::process_dir();
    assert!(
        std::fs::read_dir(&dir).map_or(true, |mut entries| entries.next().is_none()),
        "nothing of a fallback is in {}",
        dir.display()
    );
    parent.assert_only_capabilities_asked();
}
