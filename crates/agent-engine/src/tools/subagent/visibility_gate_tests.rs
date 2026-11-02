// The worker-visibility gate (slice 8a): included at the end of
// launch_tests.rs as `mod visibility_gate`. Fixtures in
// tests/fixtures/visibility_gate/ were written with VISIBILITY_GATE_WRITE=1
// at the slice-8a base (2ad187ce, before the notice ring and the run
// counters), twice, with no difference between the two writes.
//
// A v1 worker (no plan: worker rollover off) whose runtime reports notices,
// progress, tools, a refused context checkpoint, usage and an answer: its
// SubagentUpdate events, its state after the drive, its result, and the bodies of
// subagent_status, subagent_collect and the completion event are what they
// were before slice 8a: subagent_collect while it still runs, once it has
// finished, and after its handle was reaped (read from its v1 archive).
// Normalization: elapsed/duration seconds -> <secs>, uuids -> <id>,
// RFC 3339 timestamps -> <ts>, the temporary archive directory -> <dir>;
// nothing else. The running and reaped collect fixtures were added after the
// first five, written the same way at the same base.
//
// Provenance on the dev port: all seven fixtures are re-pinned (e68127e6) to
// pin B, 8402fb73, written with VISIBILITY_GATE_WRITE=1 at 562c7e79 (the
// port's s7, 8abd4dab, plus this file: a reference before slice 8a on the
// dev line, not the live base), twice, the second write identical to the
// first. The 2ad187ce recording is history.

use crate::tools::Tool;

const GATE_DIR: &str = "tests/fixtures/visibility_gate";

fn notice(text: &str) -> StreamEvent {
    StreamEvent::Session(SessionEvent::Notice(text.into()))
}

fn tool_start(name: &str) -> StreamEvent {
    StreamEvent::Llm(crate::LlmEvent::ToolUseStart {
        tool_name: name.into(),
        tool_id: "t1".into(),
    })
}

fn tool_use(name: &str, input: Value) -> StreamEvent {
    StreamEvent::Llm(crate::LlmEvent::ToolUse {
        tool_name: name.into(),
        tool_id: "t1".into(),
        input,
    })
}

fn tool_result(text: &str) -> StreamEvent {
    StreamEvent::Llm(crate::LlmEvent::ToolResult {
        tool_id: "t1".into(),
        result: text.into(),
        memory_only: false,
        client_tool: None,
    })
}

fn usage(input: u64, output: u64) -> StreamEvent {
    StreamEvent::Session(SessionEvent::Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_creation_5m: None,
        cache_creation_1h: None,
        model: Some(BOOKED_MODEL.into()),
    })
}

/// The run the gate pins: every kind of event the drive reads, including
/// the notices a rollover-capable runtime emits.
fn gate_events(checkpoint_event: StreamEvent) -> Vec<StreamEvent> {
    vec![
        text("looking"),
        notice("Context is getting large: about 150000 tokens in use."),
        tool_start("read"),
        tool_use("read", json!({"path": "src/lib.rs"})),
        tool_result("fn main() {}"),
        usage(100, 20),
        notice(
            "Continued automatically in context window 2 with a fresh wall-clock allowance; earlier \
             eligible source evidence remains searchable. Other resource limits remain unchanged. \
             No rolling summary.",
        ),
        checkpoint_event,
        notice("Rolling summary skipped: the session model has no enforceable output ceiling."),
        text("done"),
        usage(50, 10),
        history_event(msgs(vec![user("task"), assistant("looking"), assistant("done")])),
        done(),
    ]
}

fn stable(text: &str) -> String {
    let secs = regex::Regex::new(r#"("elapsed_secs"|"duration_secs"):\s*[0-9.]+"#).unwrap();
    let after = regex::Regex::new(r"after [0-9]+\.[0-9]s").unwrap();
    let uuid = regex::Regex::new(
        r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
    )
    .unwrap();
    let ts = regex::Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})").unwrap();
    let t = secs.replace_all(text, "$1: <secs>");
    let t = after.replace_all(&t, "after <secs>");
    let t = uuid.replace_all(&t, "<id>");
    ts.replace_all(&t, "<ts>").into_owned()
}

fn gate_check(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(GATE_DIR).join(name);
    if std::env::var_os("VISIBILITY_GATE_WRITE").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    if expected != actual {
        let line = expected
            .lines()
            .zip(actual.lines())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| expected.lines().count().min(actual.lines().count()));
        let cut = |s: &str| -> String {
            s.lines().nth(line).unwrap_or("<end>").chars().take(400).collect()
        };
        panic!(
            "{name} differs from the fixture at line {}:\n  fixture: {}\n  now:     {}",
            line + 1,
            cut(&expected),
            cut(actual)
        );
    }
}

/// The body `tool` returns for `handle_id` (a live handle in `registry`).
async fn tool_body(tool: &dyn Tool, registry: &Arc<Mutex<SubagentRegistry>>, handle_id: &str) -> Value {
    let mut ctx = crate::tools::test_helpers::create_tool_context();
    ctx.capabilities.subagent_registry = Some(Arc::clone(registry));
    ctx.capabilities.orchestration = None;
    let body = tool
        .execute(json!({ "handle_id": handle_id }), ctx)
        .await
        .unwrap_or_else(|e| panic!("{}: {e}", tool.name()));
    serde_json::from_str(&body).unwrap()
}

/// The gate: a v1 worker's visible surfaces with notices in its stream.
#[tokio::test]
async fn visibility_gate_a_v1_workers_surfaces_are_unchanged() {
    let (event, rx) = checkpoint("worker-run:any".into(), task());
    let (started, start) = runtime(gate_events(event), || {});
    let (mut d, state, _cancel) = drive(0, &task());
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    d.tx_events = Some(tx);
    let result = run_worker_with(d, task(), None, start, &real).await.unwrap();
    assert!(started.lock().unwrap().is_some());
    assert_eq!(rx.await.unwrap(), Err(ROLLOVER_OFF.into()));

    // The drive's events to the parent, in order.
    let mut events = String::new();
    while let Ok(event) = updates.try_recv() {
        events.push_str(&format!("{event:?}\n"));
    }
    gate_check("v1_updates.txt", &stable(&events));

    // The state the tools read, and the result.
    {
        let s = state.read().unwrap();
        let view = format!(
            "status: {:?}\npartial_text: {:?}\ntool_log: {:?}\nstep: {:?}\ntools: {}\nrollover_notice: {:?}\nv2_records: {}\nresult: {:?}\n",
            s.status, s.partial_text, s.tool_log, s.step, s.tools, s.rollover_notice, s.v2_records, result
        );
        gate_check("v1_state.txt", &stable(&view));
    }

    // The handle as start.rs registers it, while the worker still runs:
    // the running collect body.
    let (steer_tx, _steer_rx) = tokio::sync::mpsc::unbounded_channel();
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
    registry.lock().unwrap().register(SubagentHandle::new(
        "sa_g1".into(),
        5,
        "worker".into(),
        "task".into(),
        BOOKED_MODEL.into(),
        "s".into(),
        60,
        Arc::clone(&state),
        Some(steer_tx),
        Some(shutdown_tx),
        Some(result_rx),
    ));
    let running = tool_body(
        &crate::tools::subagent::collect::SubagentCollectTool,
        &registry,
        "sa_g1",
    )
    .await;
    gate_check(
        "v1_collect_running.json",
        &stable(&serde_json::to_string_pretty(&running).unwrap()),
    );

    // What start.rs does after the drive (terminal status, the finalizer
    // with its completion event, then the result), and the tools' bodies.
    {
        let mut s = state.write().unwrap();
        s.status = SubagentStatus::Completed;
        s.conversation_state = vec![
            json!({"role": "user", "content": "task"}),
            json!({"role": "assistant", "content": result.text.clone()}),
        ];
    }
    let queue = Arc::new(crate::events::EventQueue::new(10));
    let dir = tempfile::tempdir().unwrap();
    let local = |text: String| text.replace(&dir.path().display().to_string(), "<dir>");
    crate::tools::subagent::finalize::finalize_subagent_in(
        &state,
        Some(&queue),
        "sa_g1",
        5,
        "worker",
        std::time::Instant::now(),
        None,
        dir.path(),
    );
    let _ = result_tx.send(result);
    let completion = queue.pop().unwrap();
    let body = format!(
        "text: {}\ndata: {}\n",
        completion.content.text,
        serde_json::to_string_pretty(&completion.content.data).unwrap()
    );
    gate_check("v1_completion.txt", &stable(&local(body)));
    let status = tool_body(&crate::tools::subagent::SubagentStatusTool, &registry, "sa_g1").await;
    gate_check(
        "v1_status.json",
        &stable(&local(serde_json::to_string_pretty(&status).unwrap())),
    );
    let collect = tool_body(
        &crate::tools::subagent::collect::SubagentCollectTool,
        &registry,
        "sa_g1",
    )
    .await;
    gate_check(
        "v1_collect.json",
        &stable(&local(serde_json::to_string_pretty(&collect).unwrap())),
    );

    // The collected handle is reaped; its result is read from its v1 archive.
    registry
        .lock()
        .unwrap()
        .cleanup_finished_with_ttl(std::time::Duration::ZERO);
    assert!(registry.lock().unwrap().get("sa_g1").is_none(), "the handle was reaped");
    let reaped = tool_body(
        &crate::tools::subagent::collect::SubagentCollectTool,
        &registry,
        "sa_g1",
    )
    .await;
    gate_check(
        "v1_collect_reaped.json",
        &stable(&local(serde_json::to_string_pretty(&reaped).unwrap())),
    );
}
// Shown red by a mutation that routes SessionEvent::Notice in the drive's
// handle() into its progress update (v1_updates.txt and v1_state.txt differ),
// and by a key added to the running collect body and to the reaped collect
// body (v1_collect_running.json, v1_collect_reaped.json).
