//! Task 25 — tool-call ledger and interrupted-side-effect handling
//! (spec §8.3), end-to-end through the real `Runtime` stream loop.
//!
//! Ledger states are monotonic:
//!   planned -> authorized -> started -> committed -> result_recorded
//!
//! If cancellation lands after a possible side effect (the call has
//! STARTED) but before the result is recorded, a NonIdempotent call must
//! surface `TurnOutcome::InterruptedAfterSideEffect { call_id }` and must
//! NEVER be automatically rerun. Idempotent/read-only calls stay retryable
//! and surface a plain cancellation.

#[path = "support/phase2/mod.rs"]
mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serial_test::serial;
use support::*;
use tokio_util::sync::CancellationToken;

use synaps_cli::runtime::{Runtime, SessionEvent, StreamEvent};
use synaps_cli::tools::catalog::ToolEffect;
use synaps_cli::tools::{Tool, ToolContext, ToolOrigin};
use synaps_cli::{Result, TurnOutcome, Value};

// ── SSE fixture: a single tool call, then a terminating turn ─────────────────

fn sse_one_call(name: &str, id: &str) -> String {
    format!(
        concat!(
            "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_l1\",\"type\":\"message\",",
            "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
            "\"stop_sequence\":null,\"usage\":{{\"input_tokens\":10,\"output_tokens\":0,",
            "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}}}\n\n",
            "data: {{\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"{name}\"}}}}\n\n",
            "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
            "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\",",
            "\"stop_sequence\":null}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":5,",
            "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}\n\n",
            "data: {{\"type\":\"message_stop\"}}\n\n",
        ),
        id = id,
        name = name,
    )
}

// ── stub tool: commit a side effect, then have cancellation land ─────────────

/// A tool that records exactly one execution, "commits" a side effect, then
/// trips the shared cancellation token and stalls — so the runtime observes
/// cancellation while this call is in-flight (started, side effect possible,
/// result not yet recorded). Auto-rerun would bump `exec_count` past 1.
struct CommitThenCancelTool {
    name: String,
    effect: ToolEffect,
    cancel: CancellationToken,
    exec_count: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CommitThenCancelTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    fn description(&self) -> &str {
        "commit then cancel"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type":"object"})
    }
    fn effect(&self) -> ToolEffect {
        self.effect
    }
    async fn execute(&self, _p: Value, _c: ToolContext) -> Result<String> {
        self.exec_count.fetch_add(1, Ordering::SeqCst);
        // The side effect has now "committed". Cancellation lands here,
        // immediately after the side effect but before result recording.
        self.cancel.cancel();
        // Stall so the runtime's select! observes the cancellation instead
        // of our Ok return.
        tokio::time::sleep(Duration::from_secs(10)).await;
        Ok("committed".into())
    }
}

async fn runtime_with(tool: Arc<dyn Tool>) -> Runtime {
    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    {
        let shared = rt.tools_shared();
        let mut registry = shared.write().await;
        registry.register(tool);
    }
    rt
}

/// Drive one turn against a caller-owned cancellation token (so a tool can
/// trip it mid-execute), returning every observed event.
async fn drive_with_cancel(
    rt: &Runtime,
    prompt: &str,
    cancel: CancellationToken,
) -> Vec<StreamEvent> {
    use futures::StreamExt;
    let mut stream = rt.run_stream(prompt.to_string(), cancel).await;
    let mut events = Vec::new();
    while let Some(ev) = tokio::time::timeout(Duration::from_secs(30), stream.next())
        .await
        .expect("turn hung beyond 30 s")
    {
        let done = matches!(ev, StreamEvent::Session(SessionEvent::Done));
        events.push(ev);
        if done {
            break;
        }
    }
    events
}

fn terminal_outcome(events: &[StreamEvent]) -> Option<TurnOutcome> {
    events.iter().find_map(|e| match e {
        StreamEvent::Session(SessionEvent::Error(err)) => Some(err.outcome.clone()),
        _ => None,
    })
}

// ── scenarios ────────────────────────────────────────────────────────────────

/// Cancellation immediately after a committed NonIdempotent call yields
/// `InterruptedAfterSideEffect { call_id }` and the tool is NOT rerun.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn cancel_after_committed_nonidempotent_yields_interrupted_no_rerun() {
    let _guard = HomeGuard::new();
    let body: &'static str = Box::leak(sse_one_call("commit_stub", "toolu_c1").into_boxed_str());
    let bodies: &'static [&'static str] = Box::leak(Box::new([body, ANTHROPIC_SSE]));
    let (url, _hits, _) = spawn_stub(Script::SeqSse(bodies)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let cancel = CancellationToken::new();
    let exec_count = Arc::new(AtomicUsize::new(0));
    let rt = runtime_with(Arc::new(CommitThenCancelTool {
        name: "commit_stub".into(),
        effect: ToolEffect::NonIdempotent,
        cancel: cancel.clone(),
        exec_count: Arc::clone(&exec_count),
    }))
    .await;

    let events = drive_with_cancel(&rt, "commit once", cancel).await;

    assert_eq!(
        exec_count.load(Ordering::SeqCst),
        1,
        "the NonIdempotent side effect must never be automatically rerun"
    );
    let outcome = terminal_outcome(&events)
        .expect("an interrupted-after-side-effect turn must surface a typed outcome");
    assert_eq!(
        outcome,
        TurnOutcome::InterruptedAfterSideEffect {
            call_id: "toolu_c1".to_string()
        },
        "cancellation after a committed NonIdempotent call is InterruptedAfterSideEffect"
    );
}

/// A read-only call interrupted the same way stays a plain cancellation
/// (retry remains safe) — it must NOT be misreported as an interrupted
/// side effect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn cancel_during_readonly_call_is_plain_cancellation() {
    let _guard = HomeGuard::new();
    let body: &'static str = Box::leak(sse_one_call("ro_stub", "toolu_r9").into_boxed_str());
    let bodies: &'static [&'static str] = Box::leak(Box::new([body, ANTHROPIC_SSE]));
    let (url, _hits, _) = spawn_stub(Script::SeqSse(bodies)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let cancel = CancellationToken::new();
    let exec_count = Arc::new(AtomicUsize::new(0));
    let rt = runtime_with(Arc::new(CommitThenCancelTool {
        name: "ro_stub".into(),
        effect: ToolEffect::ReadOnly,
        cancel: cancel.clone(),
        exec_count: Arc::clone(&exec_count),
    }))
    .await;

    let events = drive_with_cancel(&rt, "read once", cancel).await;

    assert_eq!(exec_count.load(Ordering::SeqCst), 1, "one execution");
    // No InterruptedAfterSideEffect for a read-only call.
    match terminal_outcome(&events) {
        None => {}
        Some(TurnOutcome::Canceled) => {}
        other => panic!("read-only cancellation must not be an interrupted side effect: {other:?}"),
    }
}
