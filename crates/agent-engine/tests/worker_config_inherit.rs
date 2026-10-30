//! Subagent workers of an installed `EngineHost` run under the host's
//! config on BOTH `spawn_runtime` branches (host-built `worker_runtime` and
//! the `SYNAPS_SUBAGENT_FRESH_RUNTIME=1` legacy runtime), on every spawn
//! path (oneshot, start, resume):
//! * G1: no `disabled_tools` entry is offered or runs (a worker's `bash`
//!   call is refused, `bash` never executes);
//! * G5: the cache-diagnostics object, a chain of the worker's own, and on
//!   OAuth no beta header;
//! * G6: gzip at the configured level;
//! * G7: a `reload_config` that changes `disabled_tools` rebuilds the
//!   cached worker registry.
//!
//! The no-host twin is `tools::subagent::worker_inherit_wire_tests`; both
//! share `support/worker_wire.rs`. Own process: installs a process
//! `EngineHost` and points `SYNAPS_BASE_DIR` at a private dir.

#[path = "support/worker_wire.rs"]
mod worker_wire;

use std::sync::{Arc, Mutex};

use agent_engine::runtime::subagent::{SubagentRegistry, SubagentStatus};
use agent_engine::tools::{
    SubagentResumeTool, SubagentStartTool, SubagentTool, Tool, ToolCapabilities, ToolChannels,
    ToolContext, ToolLimits,
};
use agent_engine::{EngineHost, HostOpts};
use serde_json::{json, Value};
use worker_wire::Seen;

const MODEL: &str = "anthropic/claude-sonnet-4-6";

fn ctx(registry: &Arc<Mutex<SubagentRegistry>>) -> ToolContext {
    let foreground = agent_core::prompt::QualifiedModelId::parse(MODEL).unwrap();
    ToolContext {
        channels: ToolChannels {
            tx_delta: None,
            tx_events: None,
        },
        capabilities: ToolCapabilities {
            session_allow_all: None,
            launch_cancel: None,
            memory_backend: None,
            telemetry: None,
            watcher_exit_path: None,
            tool_register_tx: None,
            session_manager: None,
            subagent_registry: Some(Arc::clone(registry)),
            event_queue: None,
            delegation_parent: None,
            codex_parent_plan: None,
            secret_prompt: None,
            orchestration: Some(Arc::new(
                agent_engine::orchestration::OrchestrationRuntime::baseline(foreground, 8, 64)
                    .unwrap(),
            )),
            tool_activation: None,
            mcp_leases: None,
            extension_leases: None,
            memory_context: None,
            cwd: None,
            env: None,
            env_stripped: Vec::new(),
            env_warned: Default::default(),
            session_id: None,
        },
        limits: ToolLimits {
            max_tool_output: 30000,
            max_tool_buffer: 256 * 1024,
            bash_timeout: 30,
            bash_max_timeout: 300,
            subagent_timeout: 300,
        },
    }
}

fn task() -> Value {
    // Over gzip's minimum body size, whatever the tool set.
    let prompt = format!("test worker\n{}", "padding ".repeat(3000));
    json!({"system_prompt": prompt, "task": "run it", "model": MODEL, "timeout": 60})
}

fn take(seen: &Arc<Mutex<Vec<Seen>>>) -> Vec<Seen> {
    std::mem::take(&mut *seen.lock().unwrap())
}

async fn finish(registry: &Arc<Mutex<SubagentRegistry>>, handle_id: &str) {
    let mut status = None;
    for _ in 0..600 {
        status = registry.lock().unwrap().get(handle_id).map(|h| h.status());
        if status != Some(SubagentStatus::Running) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(status, Some(SubagentStatus::Completed), "{handle_id}");
}

/// The requests of the oneshot, start and resume workers, in that order.
async fn run_three_paths(seen: &Arc<Mutex<Vec<Seen>>>) -> [Vec<Seen>; 3] {
    let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
    let out = SubagentTool
        .execute(task(), ctx(&registry))
        .await
        .expect("subagent");
    assert!(out.contains("done"), "{out}");
    let oneshot = take(seen);
    let out = SubagentStartTool
        .execute(task(), ctx(&registry))
        .await
        .expect("subagent_start");
    let handle = serde_json::from_str::<Value>(&out).unwrap()["handle_id"]
        .as_str()
        .unwrap()
        .to_owned();
    finish(&registry, &handle).await;
    let start = take(seen);
    let out = SubagentResumeTool
        .execute(
            json!({"handle_id": handle, "instructions": "again", "timeout": 60}),
            ctx(&registry),
        )
        .await
        .expect("subagent_resume");
    let resumed = serde_json::from_str::<Value>(&out).unwrap()["handle_id"]
        .as_str()
        .unwrap()
        .to_owned();
    finish(&registry, &resumed).await;
    [oneshot, start, take(seen)]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_workers_inherit_disabled_tools_diagnostics_and_gzip_on_both_branches() {
    let base = tempfile::TempDir::new().unwrap();
    std::fs::write(base.path().join("auth.json"), worker_wire::OAUTH_AUTH_JSON).unwrap();
    std::fs::write(base.path().join("config"), worker_wire::LOCKED_CONFIG).unwrap();
    agent_engine::config::set_base_dir_for_tests(base.path().to_path_buf());
    let marker = base.path().join("bash-ran");
    let (url, seen) = worker_wire::spawn_provider(&marker, false).await;
    for key in [
        "SYNAPS_AUTH_ENDPOINT",
        "SYNAPS_MACHINE_TOKEN",
        "SYNAPS_SUBAGENT_FRESH_RUNTIME",
        "SYNAPS_GZIP_REQUEST_BODIES",
        "ANTHROPIC_API_KEY",
    ] {
        std::env::remove_var(key);
    }
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let host = EngineHost::boot(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .expect("host boots");
    assert!(EngineHost::install(host).is_ok());
    let host = EngineHost::current().expect("installed");
    // The foreground's lockdown, for reference: the workers must match it.
    let foreground = host.parts().tools.read().await.tools_schema();
    for name in worker_wire::DISABLED {
        assert!(!foreground.iter().any(|t| t["name"] == name), "{name}");
    }

    // Host-built worker (the default branch).
    let runs = run_three_paths(&seen).await;
    worker_wire::assert_worker_requests("host", &runs, &marker);

    // Legacy fresh runtime under the same host.
    std::env::set_var("SYNAPS_SUBAGENT_FRESH_RUNTIME", "1");
    let runs = run_three_paths(&seen).await;
    std::env::remove_var("SYNAPS_SUBAGENT_FRESH_RUNTIME");
    worker_wire::assert_worker_requests("legacy", &runs, &marker);

    // G5 fallback on the host branch: a refused diagnostics object is
    // dropped and the worker still completes.
    let (refusing, refused) = worker_wire::spawn_provider(&marker, true).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &refusing);
    let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
    let out = SubagentTool
        .execute(task(), ctx(&registry))
        .await
        .expect("subagent");
    assert!(out.contains("done"), "{out}");
    worker_wire::assert_diagnostics_refusal_falls_back(&take(&refused));
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    // G7: a reload that changes `disabled_tools` rebuilds the cached worker
    // registry on both branches; the catalog generation alone did not move.
    let generation = host.parts().tools.read().await.catalog().generation();
    let cached = host.worker_registry().await;
    assert!(Arc::ptr_eq(
        &cached.tools_schema(),
        &host.worker_registry().await.tools_schema()
    ));
    std::fs::write(base.path().join("config"), "disabled_tools = read, find\n").unwrap();
    host.reload_config();
    assert_eq!(
        host.parts().tools.read().await.catalog().generation(),
        generation
    );
    let reloaded = host.worker_registry().await;
    assert!(!Arc::ptr_eq(
        &cached.tools_schema(),
        &reloaded.tools_schema()
    ));
    for branch in ["host", "legacy"] {
        if branch == "legacy" {
            std::env::set_var("SYNAPS_SUBAGENT_FRESH_RUNTIME", "1");
        }
        let worker = agent_engine::tools::spawn_runtime().await.expect(branch);
        std::env::remove_var("SYNAPS_SUBAGENT_FRESH_RUNTIME");
        let tools = worker.tools_shared();
        let tools = tools.read().await;
        for name in ["read", "find"] {
            assert!(
                tools.get(name).is_none(),
                "{branch}: {name} after the reload"
            );
        }
        assert!(tools.get("ls").is_some(), "{branch}: ls is back");
        assert_eq!(
            tools.get("bash").is_some(),
            cfg!(feature = "command-exec"),
            "{branch}: bash is back"
        );
    }
    assert!(!marker.exists());
}
