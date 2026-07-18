//! External proof harness for expired subagent tombstones.
//!
//! This test lives outside `agent-engine` and drives the public runtime/tool
//! interfaces without provider traffic.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

struct TrackingAllocator;

static LIVE_ALLOCATED_BYTES: AtomicIsize = AtomicIsize::new(0);

#[global_allocator]
static GLOBAL_ALLOCATOR: TrackingAllocator = TrackingAllocator;

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            LIVE_ALLOCATED_BYTES.fetch_add(layout.size() as isize, Ordering::SeqCst);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc_zeroed(layout);
        if !ptr.is_null() {
            LIVE_ALLOCATED_BYTES.fetch_add(layout.size() as isize, Ordering::SeqCst);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        LIVE_ALLOCATED_BYTES.fetch_sub(layout.size() as isize, Ordering::SeqCst);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = System.realloc(ptr, layout, new_size);
        if !new_ptr.is_null() {
            LIVE_ALLOCATED_BYTES
                .fetch_add(new_size as isize - layout.size() as isize, Ordering::SeqCst);
        }
        new_ptr
    }
}

use agent_core::orchestration::CompletionGate;
use agent_core::prompt::QualifiedModelId;
use agent_engine::orchestration::OrchestrationRuntime;
use agent_engine::runtime::subagent::{
    reap_finished, SubagentHandle, SubagentRegistry, SubagentState, SubagentStatus,
    TOMBSTONE_OUTPUT_MAX_BYTES,
};
use agent_engine::tools::{
    SubagentCollectTool, Tool, ToolCapabilities, ToolChannels, ToolContext, ToolLimits,
};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

fn tool_context(
    registry: Arc<Mutex<SubagentRegistry>>,
    orchestration: Arc<OrchestrationRuntime>,
) -> ToolContext {
    ToolContext {
        channels: ToolChannels {
            tx_delta: None,
            tx_events: None,
        },
        capabilities: ToolCapabilities {
            watcher_exit_path: None,
            tool_register_tx: None,
            session_manager: None,
            subagent_registry: Some(registry),
            event_queue: None,
            secret_prompt: None,
            orchestration: Some(orchestration),
        },
        limits: ToolLimits {
            max_tool_output: 30_000,
            max_tool_buffer: 256 * 1024,
            bash_timeout: 30,
            bash_max_timeout: 300,
            subagent_timeout: 120,
        },
    }
}

#[tokio::test]
async fn expired_worker_is_bounded_transport_free_collectible_and_reconcilable() {
    const HANDLE_ID: &str = "sa_oracle_expired";
    const MODEL: &str = "anthropic/claude-sonnet-4-6";

    let foreground = QualifiedModelId::parse(MODEL).expect("qualified model");
    let orchestration =
        Arc::new(OrchestrationRuntime::baseline(foreground, 1, 1).expect("bounded policy runtime"));
    orchestration
        .authorize(HANDLE_ID, MODEL)
        .expect("authorize worker before registering runtime handle");

    let state = Arc::new(RwLock::new(SubagentState::new()));
    {
        let mut state = state.write().expect("state lock");
        state.status = SubagentStatus::Completed;
        // The production TTL is 15 minutes. Backdate terminal state so the public
        // production reaper sees a genuinely expired worker without sleeping.
        state.finished_at = Some(
            Instant::now()
                .checked_sub(std::time::Duration::from_secs(901))
                .expect("backdated terminal instant"),
        );
        state.partial_text = format!("{}{}", "x".repeat(80_000), "🦀 terminal tail");
        state.tool_log = vec!["large tool log".repeat(10_000)];
        state.conversation_state = vec![json!({
            "role": "assistant",
            "content": "large resumable context".repeat(10_000)
        })];
    }

    let (steer_tx, mut steer_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (_result_tx, result_rx) = oneshot::channel();
    let live_bytes_with_heavy_worker = LIVE_ALLOCATED_BYTES.load(Ordering::SeqCst);
    let handle = SubagentHandle::new(
        HANDLE_ID.into(),
        1,
        "oracle-worker".into(),
        "bounded tombstone proof".into(),
        MODEL.into(),
        "system".into(),
        30,
        state,
        Some(steer_tx),
        Some(shutdown_tx),
        Some(result_rx),
    );

    let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
    registry.lock().expect("registry lock").register(handle);

    assert!(matches!(
        orchestration.completion_gate(),
        CompletionGate::Blocked { .. }
    ));

    // Drive the public production reaper seam.
    reap_finished(&registry, Some(orchestration.as_ref()));

    {
        let registry = registry.lock().expect("registry lock");
        let tombstone = registry
            .get(HANDLE_ID)
            .expect("unreconciled expired worker must remain addressable");

        // Bounded retained output, valid UTF-8, with large execution context gone.
        let output = tombstone.partial_output();
        assert!(output.len() <= TOMBSTONE_OUTPUT_MAX_BYTES);
        assert!(output.ends_with("🦀 terminal tail"));
        assert!(tombstone.tool_log().is_empty());
        assert!(tombstone.conversation_state().is_empty());

        // Public behavior proves all execution transports are gone.
        assert!(tombstone.is_tombstone());
        assert!(tombstone.steer("must fail after expiry").is_err());
    }
    assert!(
        shutdown_rx.await.is_err(),
        "shutdown sender must be dropped"
    );
    assert!(
        steer_rx.recv().await.is_none(),
        "steering sender must be dropped"
    );
    let live_bytes_with_tombstone = LIVE_ALLOCATED_BYTES.load(Ordering::SeqCst);
    assert!(
        live_bytes_with_heavy_worker - live_bytes_with_tombstone > 300_000,
        "expiry must release the large tool log and resumable context; before={live_bytes_with_heavy_worker}, after={live_bytes_with_tombstone}"
    );

    let response = SubagentCollectTool
        .execute(
            json!({"handle_id": HANDLE_ID, "reconciled": true}),
            tool_context(registry.clone(), orchestration.clone()),
        )
        .await
        .expect("retained tombstone remains collectible");
    let response: Value = serde_json::from_str(&response).expect("collect JSON");
    assert_eq!(response["status"], "completed");
    assert_ne!(response["status"], "expired");
    assert!(response["output"].as_str().unwrap().len() <= TOMBSTONE_OUTPUT_MAX_BYTES);
    assert_eq!(orchestration.completion_gate(), CompletionGate::Allowed);

    // Once collection/reconciliation removes the policy need, normal GC removes
    // the tombstone entirely: retention cannot grow forever.
    reap_finished(&registry, Some(orchestration.as_ref()));
    assert!(registry
        .lock()
        .expect("registry lock")
        .get(HANDLE_ID)
        .is_none());
}
