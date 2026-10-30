//! Subagent workers run the host's `after_tool_call` output transforms on
//! their own tool results, on BOTH spawn paths: the host-built worker
//! (`EngineHost::worker_runtime`) and the legacy fresh runtime
//! (`SYNAPS_SUBAGENT_FRESH_RUNTIME=1`). Nothing else on the host bus fires
//! for a worker: no `before_tool_call` (so no confirm prompt), no
//! observe-only `after_tool_call`.
//!
//! Own process: installs a process `EngineHost`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agent_engine::extensions::hooks::events::{HookEvent, HookKind, HookResult};
use agent_engine::extensions::hooks::HookBus;
use agent_engine::extensions::permissions::{Permission, PermissionSet};
use agent_engine::extensions::runtime::ExtensionHandler;
use agent_engine::{EngineHost, HostOpts};

/// Answers `result` and counts calls.
struct Fixed {
    id: &'static str,
    result: HookResult,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl ExtensionHandler for Fixed {
    fn id(&self) -> &str {
        self.id
    }
    async fn handle(&self, _event: &HookEvent) -> HookResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.result.clone()
    }
    async fn shutdown(&self) {}
}

fn fixed(id: &'static str, result: HookResult) -> Arc<Fixed> {
    Arc::new(Fixed {
        id,
        result,
        calls: AtomicUsize::new(0),
    })
}

fn perms(list: &[Permission]) -> PermissionSet {
    let mut set = PermissionSet::new();
    for p in list {
        set.grant(*p);
    }
    set
}

async fn host() -> Arc<EngineHost> {
    let base = tempfile::TempDir::new().unwrap();
    std::env::set_var("SYNAPS_BASE_DIR", base.path());
    std::mem::forget(base);
    let host = EngineHost::boot(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .expect("host boots");
    let _ = EngineHost::install(Arc::clone(&host));
    EngineHost::current().expect("installed")
}

/// What one spawned worker sees through its hook bus.
async fn assert_worker_sees_only_transforms(
    path: &str,
    worker: &agent_engine::Runtime,
    host_bus: &Arc<HookBus>,
) {
    let bus = worker.hook_bus();
    assert!(bus.is_worker_view(), "{path}: a worker view");
    assert!(!Arc::ptr_eq(bus, host_bus), "{path}: never the full host bus");
    let replaced = agent_engine::runtime::emit_after_tool_call(
        bus,
        "bash",
        None,
        serde_json::json!({"command": "cargo build"}),
        "RAW 10k lines".to_string(),
        30_000,
        None,
    )
    .await;
    assert_eq!(replaced, "CRUSHED", "{path}: the worker's bash result is replaced");
    let before = agent_engine::runtime::emit_before_tool_call(
        bus,
        "bash",
        None,
        serde_json::json!({"command": "rm -rf /"}),
        None,
    )
    .await;
    assert_eq!(before, HookResult::Continue, "{path}: no before_tool_call in a worker");
}

#[tokio::test]
async fn both_spawn_paths_run_the_hosts_output_transforms_and_nothing_else() {
    let host = host().await;
    let host_bus = Arc::clone(&host.parts().hook_bus);
    let confirm = fixed(
        "confirmer",
        HookResult::Confirm {
            message: "run it?".into(),
            sources: vec![],
            generations: vec![],
            tool: None,
            modified_input: None,
            modified_by: vec![],
        },
    );
    let observer = fixed("observer", HookResult::Replace { output: "SNEAKY".into() });
    let crush = fixed("crush", HookResult::Replace { output: "CRUSHED".into() });
    let intercept = perms(&[Permission::ToolsIntercept]);
    let transform = perms(&[Permission::ToolsIntercept, Permission::ToolsTransformOutput]);
    host_bus
        .subscribe(HookKind::BeforeToolCall, confirm.clone(), None, None, transform.clone())
        .await
        .unwrap();
    host_bus
        .subscribe(HookKind::AfterToolCall, observer.clone(), None, None, intercept)
        .await
        .unwrap();
    host_bus
        .subscribe(HookKind::AfterToolCall, crush.clone(), None, None, transform)
        .await
        .unwrap();

    // Host-built worker (the default spawn path).
    std::env::remove_var("SYNAPS_SUBAGENT_FRESH_RUNTIME");
    let worker = agent_engine::tools::spawn_runtime().await.expect("host worker");
    assert_worker_sees_only_transforms("host", &worker, &host_bus).await;

    // Legacy fresh runtime (kill-switch): the same view of the same host.
    std::env::set_var("SYNAPS_SUBAGENT_FRESH_RUNTIME", "1");
    let legacy = agent_engine::tools::spawn_runtime().await.expect("legacy worker");
    std::env::remove_var("SYNAPS_SUBAGENT_FRESH_RUNTIME");
    assert_worker_sees_only_transforms("legacy", &legacy, &host_bus).await;

    assert_eq!(crush.calls.load(Ordering::SeqCst), 2, "one transform per worker call");
    assert_eq!(confirm.calls.load(Ordering::SeqCst), 0, "no confirm prompt from a worker");
    assert_eq!(observer.calls.load(Ordering::SeqCst), 0, "observe-only is not dispatched");

    // The foreground keeps the whole bus, unchanged.
    let foreground = host.foreground_runtime().await.expect("foreground");
    assert!(Arc::ptr_eq(foreground.hook_bus(), &host_bus));
    assert!(!foreground.hook_bus().is_worker_view());
}
