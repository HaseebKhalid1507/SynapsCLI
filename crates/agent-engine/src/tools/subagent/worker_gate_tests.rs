// Item G: a worker's completion gate counts only its own delegation
// subtree. Included at the end of start.rs as `mod worker_gate`.
//
// Before the fix, a worker's runtime ran the session-wide completion gate:
// once the parent read one worker without reconciling it, any other worker
// of that parent failed when it gave its final answer ("completion blocked:
// ... require collection/reconciliation"), and so did a worker resumed from
// the read one. The worker cases run the real start, collect and resume
// tools against a loopback provider stand-in; the policy cases pin the
// gate each runtime sees.

use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use agent_core::orchestration::{CompletionGate, WorkerTerminal};
use axum::body::Bytes;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Router;
use serde_json::{json, Value};

use crate::tools::{SubagentRegistry, Tool};

fn sse_events(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

fn usage() -> Value {
    json!({"input_tokens":12,"output_tokens":3,"cache_read_input_tokens":0,"cache_creation_input_tokens":0})
}

fn message_start(n: usize) -> Value {
    json!({"type":"message_start","message":{"id":format!("msg_g_{n}"),"type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":usage()}})
}

/// A reply that asks for the `read` tool on a path that does not exist.
fn tool_use_reply(n: usize) -> Response {
    let body = sse_events(&[
        message_start(n),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":format!("toolu_g_{n}"),"name":"read","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"/nonexistent/worker-gate.txt\"}"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":usage()}),
        json!({"type":"message_stop"}),
    ]);
    (StatusCode::OK, [("content-type", "text/event-stream")], body).into_response()
}

/// A reply that answers in text and ends the turn.
fn text_reply(text: &str, n: usize) -> Response {
    let body = sse_events(&[
        message_start(n),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":usage()}),
        json!({"type":"message_stop"}),
    ]);
    (StatusCode::OK, [("content-type", "text/event-stream")], body).into_response()
}

/// The stand-in. A request whose body names "task B" first gets a tool
/// call; its follow-up (with the tool result) waits for `release` before it
/// is answered. Every other request is answered at once.
async fn stand_in(release: Arc<tokio::sync::Notify>) -> String {
    let count = Arc::new(AtomicUsize::new(0));
    let app = Router::new().fallback(move |body: Bytes| {
        let release = Arc::clone(&release);
        let count = Arc::clone(&count);
        async move {
            let n = count.fetch_add(1, AtomicOrdering::SeqCst);
            let text = String::from_utf8_lossy(&body).into_owned();
            if text.contains("task B") {
                if !text.contains("tool_result") {
                    return tool_use_reply(n);
                }
                release.notified().await;
                return text_reply("B is done", n);
            }
            text_reply("done", n)
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    url
}

/// One parent: its worker registry and its orchestration policy, shared by
/// every tool call it makes, as one session's are.
struct Parent {
    registry: Arc<Mutex<SubagentRegistry>>,
    policy: Arc<crate::orchestration::OrchestrationRuntime>,
}

impl Parent {
    fn new() -> Self {
        let policy = crate::tools::test_helpers::create_tool_context()
            .capabilities
            .orchestration
            .expect("the test context has an orchestration policy");
        Self { registry: Arc::new(Mutex::new(SubagentRegistry::new())), policy }
    }

    fn context(&self) -> crate::tools::ToolContext {
        let mut ctx = crate::tools::test_helpers::create_tool_context();
        ctx.capabilities.subagent_registry = Some(Arc::clone(&self.registry));
        ctx.capabilities.orchestration = Some(Arc::clone(&self.policy));
        ctx
    }

    async fn start(&self, task: &str) -> String {
        let out = SubagentStartTool
            .execute(
                json!({"name": "g", "system_prompt": "You are a worker.", "task": task, "timeout": 30}),
                self.context(),
            )
            .await
            .expect("the worker starts");
        handle_of(&out)
    }

    async fn resume(&self, handle_id: &str) -> String {
        let out = crate::tools::subagent::SubagentResumeTool
            .execute(
                json!({"handle_id": handle_id, "instructions": "Go on.", "timeout": 30}),
                self.context(),
            )
            .await
            .expect("the worker resumes");
        handle_of(&out)
    }

    /// An inspection read: collect without reconciled=true.
    async fn inspect(&self, handle_id: &str) -> Value {
        self.collect(json!({ "handle_id": handle_id })).await
    }

    async fn reconcile(&self, handle_id: &str) -> Value {
        self.collect(json!({ "handle_id": handle_id, "reconciled": true })).await
    }

    async fn collect(&self, params: Value) -> Value {
        let body = crate::tools::subagent::SubagentCollectTool
            .execute(params, self.context())
            .await
            .expect("collect");
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("{body}: {e}"))
    }

    /// Wait until `handle_id`'s worker thread has ended.
    async fn finished(&self, handle_id: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let done = self
                .registry
                .lock()
                .unwrap()
                .get(handle_id)
                .and_then(|h| h.thread_handle.as_ref().map(|t| t.is_finished()))
                .expect("the thread handle is attached before the tool returns");
            if done {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "{handle_id} ends");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}

fn handle_of(result: &str) -> String {
    let value: Value = serde_json::from_str(result).unwrap_or_else(|e| panic!("{result}: {e}"));
    value["handle_id"].as_str().expect("a handle").to_owned()
}

/// A scratch base directory with a stored stand-in Anthropic login, and the
/// provider pointed at the loopback stand-in.
fn scene(url: &str) -> (crate::test_env::BaseDirGuard, crate::test_env::EnvVarGuard) {
    let base = crate::test_env::BaseDirGuard::new();
    std::fs::write(
        base.path().join("auth.json"),
        json!({"anthropic":{"type":"oauth","refresh":"g-refresh","access":"g-access","expires":9_999_999_999_999u64}})
            .to_string(),
    )
    .unwrap();
    let url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", url);
    (base, url)
}

fn assert_completed(body: &Value, what: &str) {
    assert_eq!(body["status"], "completed", "{what}: {body:#}");
    assert!(body.get("error").is_none(), "{what}: {body:#}");
}

/// The probe: the parent reads A without reconciling it while B still
/// runs; B then gives its final answer and completes.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn g1_a_siblings_inspection_read_does_not_fail_a_worker() {
    let release = Arc::new(tokio::sync::Notify::new());
    let url = stand_in(Arc::clone(&release)).await;
    let _scene = scene(&url);
    let parent = Parent::new();
    let a = parent.start("task A: answer at once").await;
    let b = parent.start("task B: read, then answer").await;
    parent.finished(&a).await;
    let read = parent.inspect(&a).await;
    assert_completed(&read, "A");
    release.notify_one();
    parent.finished(&b).await;
    let b_body = parent.reconcile(&b).await;
    assert_completed(&b_body, "B after A's inspection read");
    assert_eq!(b_body["output"], "B is done");
    // The parent's own gate still names A until A is reconciled.
    assert_eq!(
        parent.policy.completion_gate(),
        CompletionGate::Blocked { workers: vec![a.clone()] }
    );
    parent.reconcile(&a).await;
    assert_eq!(parent.policy.completion_gate(), CompletionGate::Allowed);
}

/// A worker resumed from one the parent only read (not reconciled) completes.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn g2_a_worker_resumed_after_an_inspection_read_completes() {
    let url = stand_in(Arc::new(tokio::sync::Notify::new())).await;
    let _scene = scene(&url);
    let parent = Parent::new();
    let first = parent.start("task A: answer at once").await;
    parent.finished(&first).await;
    assert_completed(&parent.inspect(&first).await, "the first run");
    let second = parent.resume(&first).await;
    parent.finished(&second).await;
    let body = parent.reconcile(&second).await;
    assert_completed(&body, "the resumed run");
    assert_eq!(body["output"], "done");
}

/// A policy with workers reserved under `parent` (None = the foreground)
/// and authorized, all running.
fn policy_with(edges: &[(&str, Option<&str>)]) -> crate::orchestration::OrchestrationRuntime {
    let policy = crate::orchestration::OrchestrationRuntime::baseline(
        agent_core::prompt::QualifiedModelId::parse("anthropic/claude-sonnet-4-6").unwrap(),
        8,
        64,
    )
    .unwrap()
    .with_max_depth(4)
    .unwrap();
    for (id, parent) in edges {
        policy.reserve_delegation(id, *parent).unwrap();
        // Authorizing dispatches the worker and marks it running.
        policy.authorize(id, "anthropic/claude-sonnet-4-6").unwrap();
    }
    policy
}

fn blocked(ids: &[&str]) -> CompletionGate {
    CompletionGate::Blocked { workers: ids.iter().map(|s| s.to_string()).collect() }
}

/// The foreground's gate is the session-wide one, unchanged: it counts
/// every worker read but not reconciled, at any depth.
#[test]
fn g3_the_foregrounds_gate_still_counts_every_worker() {
    let policy = policy_with(&[("sa_a", None), ("sa_b", None), ("sa_c", Some("sa_b"))]);
    policy.terminal_and_collect("sa_a", WorkerTerminal::Completed).unwrap();
    policy.terminal_and_collect("sa_c", WorkerTerminal::Completed).unwrap();
    assert_eq!(policy.completion_gate_for(None), policy.completion_gate());
    assert_eq!(policy.completion_gate_for(None), blocked(&["sa_a", "sa_c"]));
}

/// A worker is still blocked by its own child read but not reconciled,
/// also once that child's thread has ended and its tree edge is released;
/// a sibling of that worker is not.
#[test]
fn g4_a_worker_is_still_blocked_by_its_own_child() {
    let policy = policy_with(&[("sa_w", None), ("sa_s", None), ("sa_c", Some("sa_w"))]);
    policy.release_delegation("sa_c", Some("sa_w"));
    policy.terminal_and_collect("sa_c", WorkerTerminal::Completed).unwrap();
    assert_eq!(policy.completion_gate_for(Some("sa_w")), blocked(&["sa_c"]));
    assert_eq!(policy.completion_gate_for(Some("sa_s")), CompletionGate::Allowed);
    assert_eq!(policy.completion_gate_for(Some("sa_c")), CompletionGate::Allowed);
    policy.reconcile("sa_c").unwrap();
    assert_eq!(policy.completion_gate_for(Some("sa_w")), CompletionGate::Allowed);
}

/// Two levels: a grandchild read but not reconciled blocks its own parent
/// worker (and that worker's own parent, whose subtree it is in), never its
/// parent's siblings or the siblings' children.
#[test]
fn g5_a_grandchild_blocks_only_its_own_line() {
    let policy = policy_with(&[
        ("sa_top", None),
        ("sa_mid", Some("sa_top")),
        ("sa_mid2", Some("sa_top")),
        ("sa_leaf", Some("sa_mid")),
        ("sa_leaf2", Some("sa_mid2")),
        ("sa_other", None),
    ]);
    policy.terminal_and_collect("sa_leaf", WorkerTerminal::Completed).unwrap();
    assert_eq!(policy.completion_gate_for(Some("sa_mid")), blocked(&["sa_leaf"]));
    assert_eq!(policy.completion_gate_for(Some("sa_top")), blocked(&["sa_leaf"]));
    for unrelated in ["sa_mid2", "sa_leaf2", "sa_other", "sa_leaf"] {
        assert_eq!(
            policy.completion_gate_for(Some(unrelated)),
            CompletionGate::Allowed,
            "{unrelated}"
        );
    }
    assert_eq!(policy.completion_gate_for(None), blocked(&["sa_leaf"]));
}

/// A middle worker that ended while its own child still ran (a running child
/// never blocks), was reconciled by its parent and retired by the reaper
/// leaves that child in its parent's line: the grandchild, once read but not
/// reconciled, still blocks the grandparent and never the grandparent's
/// other children.
#[test]
fn g6_a_retired_middle_worker_leaves_its_child_in_the_line() {
    let policy = policy_with(&[
        ("sa_top", None),
        ("sa_mid", Some("sa_top")),
        ("sa_mid2", Some("sa_top")),
        ("sa_leaf", Some("sa_mid")),
    ]);
    policy.release_delegation("sa_mid", Some("sa_top"));
    policy.terminal_and_collect("sa_mid", WorkerTerminal::Completed).unwrap();
    policy.reconcile("sa_mid").unwrap();
    policy.retire_reconciled(&std::collections::HashSet::from(["sa_mid".to_string()]));
    assert_eq!(policy.completion_gate_for(Some("sa_top")), CompletionGate::Allowed);
    policy.terminal_and_collect("sa_leaf", WorkerTerminal::Completed).unwrap();
    assert_eq!(policy.completion_gate_for(Some("sa_top")), blocked(&["sa_leaf"]));
    assert_eq!(policy.completion_gate_for(Some("sa_mid2")), CompletionGate::Allowed);
    assert_eq!(policy.completion_gate_for(None), blocked(&["sa_leaf"]));
    policy.reconcile("sa_leaf").unwrap();
    assert_eq!(policy.completion_gate_for(Some("sa_top")), CompletionGate::Allowed);
}

// ── Review r1 fixes ─────────────────────────────────────────────────────

impl Parent {
    /// A parent whose policy admits `concurrent` running workers at a time.
    fn with_concurrency(concurrent: usize) -> Self {
        let policy = crate::orchestration::OrchestrationRuntime::baseline(
            agent_core::prompt::QualifiedModelId::parse("anthropic/claude-sonnet-4-6").unwrap(),
            concurrent,
            64,
        )
        .unwrap();
        Self { registry: Arc::new(Mutex::new(SubagentRegistry::new())), policy: Arc::new(policy) }
    }

    async fn try_start(&self, task: &str, model: Option<&str>) -> std::result::Result<String, String> {
        let mut params =
            json!({"name": "g", "system_prompt": "You are a worker.", "task": task, "timeout": 30});
        if let Some(model) = model {
            params["model"] = json!(model);
        }
        SubagentStartTool
            .execute(params, self.context())
            .await
            .map(|out| handle_of(&out))
            .map_err(|e| e.to_string())
    }

    async fn try_resume(&self, handle_id: &str) -> std::result::Result<String, String> {
        crate::tools::subagent::SubagentResumeTool
            .execute(
                json!({"handle_id": handle_id, "instructions": "Go on.", "timeout": 30}),
                self.context(),
            )
            .await
            .map(|out| handle_of(&out))
            .map_err(|e| e.to_string())
    }
}

/// A start refused after its tree edge was reserved (an invalid model, a
/// model the policy does not offer, the concurrency limit) leaves no owner
/// entry behind, however often it is tried.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn g7_a_refused_start_leaves_no_owner_behind() {
    let release = Arc::new(tokio::sync::Notify::new());
    let url = stand_in(Arc::clone(&release)).await;
    let _scene = scene(&url);
    let parent = Parent::with_concurrency(1);
    let before = parent.policy.owner_entries();
    for n in 0..25 {
        for model in ["not a model", "openai/gpt-not-offered"] {
            let err = parent
                .try_start(&format!("task R{n}"), Some(model))
                .await
                .expect_err("the model is refused");
            assert!(err.contains("dispatch_denied"), "{err}");
            assert!(!err.contains("concurrency_limit"), "{err}");
        }
    }
    assert_eq!(parent.policy.owner_entries(), before, "a refused model leaves no owner");
    assert_eq!(parent.policy.delegation_descendants(), 0);

    // B holds the one slot; every further start is refused at capacity.
    let b = parent.start("task B: read, then answer").await;
    let held = parent.policy.owner_entries();
    assert_eq!(held, before + 1, "B's own entry");
    for n in 0..25 {
        let err = parent.try_start(&format!("task C{n}"), None).await.expect_err("at capacity");
        assert!(err.contains("concurrency_limit"), "{err}");
    }
    assert_eq!(parent.policy.owner_entries(), held, "a start refused at capacity leaves no owner");
    assert_eq!(parent.policy.delegation_descendants(), 1);
    release.notify_one();
    parent.finished(&b).await;
    assert_completed(&parent.reconcile(&b).await, "B");
}

/// The same for a resume refused after its tree edge was reserved.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn g8_a_refused_resume_leaves_no_owner_behind() {
    let release = Arc::new(tokio::sync::Notify::new());
    let url = stand_in(Arc::clone(&release)).await;
    let _scene = scene(&url);
    let parent = Parent::with_concurrency(1);
    let a = parent.start("task A: answer at once").await;
    parent.finished(&a).await;
    assert_completed(&parent.reconcile(&a).await, "A");
    let b = parent.start("task B: read, then answer").await;
    let held = parent.policy.owner_entries();
    for _ in 0..25 {
        let err = parent.try_resume(&a).await.expect_err("at capacity");
        assert!(err.contains("concurrency_limit"), "{err}");
    }
    assert_eq!(parent.policy.owner_entries(), held, "a refused resume leaves no owner");
    assert_eq!(parent.policy.delegation_descendants(), 1);
    release.notify_one();
    parent.finished(&b).await;
    assert_completed(&parent.reconcile(&b).await, "B");
}

/// Ownership is per run: a child the earlier run started, read but not
/// reconciled, does not block the resumed run (a new handle with an empty
/// subtree). It still blocks the earlier run's line and the foreground, also
/// once the earlier run is retired and the child has passed to its owner.
#[test]
fn g9_a_resumed_run_does_not_inherit_the_earlier_runs_children() {
    let policy = policy_with(&[("sa_src", None), ("sa_kid", Some("sa_src"))]);
    // The earlier run ended; the parent resumes it as a new handle.
    policy.release_delegation("sa_src", None);
    policy.reserve_delegation("sa_res", None).unwrap();
    policy.authorize("sa_res", "anthropic/claude-sonnet-4-6").unwrap();
    policy.terminal_and_collect("sa_kid", WorkerTerminal::Completed).unwrap();
    assert_eq!(policy.completion_gate_for(Some("sa_res")), CompletionGate::Allowed);
    assert_eq!(policy.completion_gate_for(Some("sa_src")), blocked(&["sa_kid"]));
    assert_eq!(policy.completion_gate_for(None), blocked(&["sa_kid"]));
    policy.terminal_and_collect("sa_src", WorkerTerminal::Completed).unwrap();
    policy.reconcile("sa_src").unwrap();
    policy.retire_reconciled(&std::collections::HashSet::from(["sa_src".to_string()]));
    assert_eq!(policy.completion_gate_for(Some("sa_res")), CompletionGate::Allowed);
    assert_eq!(policy.completion_gate_for(None), blocked(&["sa_kid"]));
}

/// A reply that calls the oneshot `subagent` tool.
fn oneshot_call_reply(n: usize) -> Response {
    let input =
        json!({"system_prompt": "You are an inner worker.", "task": "inner job", "timeout": 30})
            .to_string();
    let body = sse_events(&[
        message_start(n),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":format!("toolu_g_{n}"),"name":"subagent","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":usage()}),
        json!({"type":"message_stop"}),
    ]);
    (StatusCode::OK, [("content-type", "text/event-stream")], body).into_response()
}

/// The stand-in for g10: the outer worker's first request gets a oneshot
/// call, the oneshot's worker answers, then the outer worker answers with
/// the tool's result in hand.
async fn nesting_stand_in() -> String {
    let count = Arc::new(AtomicUsize::new(0));
    let app = Router::new().fallback(move |body: Bytes| {
        let count = Arc::clone(&count);
        async move {
            let n = count.fetch_add(1, AtomicOrdering::SeqCst);
            let text = String::from_utf8_lossy(&body).into_owned();
            if text.contains("task O") {
                if text.contains("tool_result") {
                    return text_reply("O is done", n);
                }
                return oneshot_call_reply(n);
            }
            text_reply("inner done", n)
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    url
}

/// End to end: a reactive worker's runtime (the shared policy installed, its
/// own handle as delegation parent) runs a oneshot, then gives its final
/// answer, and completes with no block. A worker's registry has no subagent
/// tools on this line, so the test hands its runtime the oneshot tool.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn g10_a_worker_that_ran_a_oneshot_completes() {
    use futures::StreamExt;
    let url = nesting_stand_in().await;
    let _scene = scene(&url);
    let parent = Parent::new();
    // The outer worker's handle, reserved and authorized as the start tool does.
    parent.policy.reserve_delegation("sa_outer", None).unwrap();
    parent.policy.authorize("sa_outer", "anthropic/claude-sonnet-4-6").unwrap();

    let config = crate::config::load_config();
    let mut runtime = crate::Runtime::new().await.expect("a runtime");
    runtime.set_tools(crate::ToolRegistry::from_tools_for_tests(vec![Arc::new(
        crate::tools::subagent::SubagentTool,
    )]));
    runtime.apply_auth_config(&config);
    crate::tools::subagent::apply_subagent_runtime_policy(
        &mut runtime,
        &config,
        None,
        None,
        None,
        &crate::tools::subagent::WorkerWorkspace::default(),
    );
    runtime.set_model("anthropic/claude-sonnet-4-6".to_string());
    runtime.install_worker_orchestration(Arc::clone(&parent.policy));
    runtime.set_delegation_parent(Some("sa_outer".to_string()));

    let messages = vec![Arc::new(json!({"role": "user", "content": "task O: run a oneshot, then answer"}))];
    let mut stream = runtime
        .run_stream_with_messages(messages, crate::CancellationToken::new(), None, None, false)
        .await;
    let (mut text, mut errors, mut done) = (String::new(), Vec::new(), false);
    while let Some(event) = tokio::time::timeout(std::time::Duration::from_secs(60), stream.next())
        .await
        .expect("the worker's stream moves")
    {
        match event {
            crate::StreamEvent::Llm(crate::LlmEvent::Text(t)) => text.push_str(&t),
            crate::StreamEvent::Session(crate::SessionEvent::Error(e)) => errors.push(e.message),
            crate::StreamEvent::Session(crate::SessionEvent::Done) => {
                done = true;
                break;
            }
            _ => {}
        }
    }
    assert!(errors.is_empty(), "{errors:?}");
    assert!(done, "the worker's turn ends");
    assert!(text.contains("O is done"), "{text}");
    assert_eq!(parent.policy.completion_gate_for(Some("sa_outer")), CompletionGate::Allowed);
}
