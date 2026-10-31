//! Tool-round keep-warm (`cache_keepwarm_tools`, `runtime::tool_keepwarm`)
//! reaches the workers of an installed `EngineHost` on BOTH `spawn_runtime`
//! branches (host-built `worker_runtime` and the
//! `SYNAPS_SUBAGENT_FRESH_RUNTIME=1` legacy runtime), on every spawn path
//! (oneshot, start, resume): a `bash` call that outlives the cache TTL is
//! kept warm by repeated pings that replay the round's request, and the
//! request after it reads the whole prefix; with the key off (a host
//! `reload_config`) the same workers rewrite it.
//!
//! The no-host twin is `tools::subagent::worker_tool_keepwarm_tests`; both
//! share `support/cache_sim.rs`. Own process: installs a process
//! `EngineHost` and points `SYNAPS_BASE_DIR` at a private dir.
#![cfg(all(unix, feature = "command-exec"))]

#[path = "support/cache_sim.rs"]
mod cache_sim;

use std::sync::{Arc, Mutex};

use agent_engine::runtime::subagent::{SubagentRegistry, SubagentStatus};
use agent_engine::runtime::tool_keepwarm::testing::IntervalOverride;
use agent_engine::tools::{
    SubagentResumeTool, SubagentStartTool, SubagentTool, Tool, ToolCapabilities, ToolChannels,
    ToolContext, ToolLimits,
};
use agent_engine::{EngineHost, HostOpts};
use cache_sim::{Req, Sim};
use serde_json::{json, Value};

const MODEL: &str = "anthropic/claude-sonnet-4-6";

const OAUTH_AUTH_JSON: &str = r#"{"anthropic": {"type": "oauth", "refresh": "synthetic", "access": "synthetic-access", "expires": 9999999999999}}"#;

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
    let prompt = format!("test worker\n{}", "padding ".repeat(3000));
    json!({"system_prompt": prompt, "task": "run it", "model": MODEL, "timeout": 60})
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

async fn oneshot(sim: &Arc<Mutex<Sim>>) -> Vec<Req> {
    let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
    let out = SubagentTool
        .execute(task(), ctx(&registry))
        .await
        .expect("subagent");
    assert!(out.contains("done"), "{out}");
    cache_sim::take(sim)
}

/// The requests of the oneshot, start and resume workers, in that order.
async fn run_three_paths(sim: &Arc<Mutex<Sim>>) -> [Vec<Req>; 3] {
    let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
    let oneshot = oneshot(sim).await;
    let out = SubagentStartTool
        .execute(task(), ctx(&registry))
        .await
        .expect("subagent_start");
    let handle = serde_json::from_str::<Value>(&out).unwrap()["handle_id"]
        .as_str()
        .unwrap()
        .to_owned();
    finish(&registry, &handle).await;
    let start = cache_sim::take(sim);
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
    [oneshot, start, cache_sim::take(sim)]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_workers_keep_a_long_tool_round_warm_on_both_branches() {
    let base = tempfile::TempDir::new().unwrap();
    std::fs::write(base.path().join("auth.json"), OAUTH_AUTH_JSON).unwrap();
    std::fs::write(base.path().join("config"), "").unwrap();
    agent_engine::config::set_base_dir_for_tests(base.path().to_path_buf());
    let (url, sim) = cache_sim::spawn(
        cache_sim::Script::kept_warm(2),
        "bash",
        json!({"command": cache_sim::WORKER_COMMAND}),
    )
    .await;
    for key in [
        "SYNAPS_AUTH_ENDPOINT",
        "SYNAPS_MACHINE_TOKEN",
        "SYNAPS_SUBAGENT_FRESH_RUNTIME",
        "ANTHROPIC_API_KEY",
    ] {
        std::env::remove_var(key);
    }
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let _interval = IntervalOverride::set(cache_sim::WORKER_INTERVAL);

    let host = EngineHost::boot(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .expect("host boots");
    assert!(EngineHost::install(host).is_ok());
    let host = EngineHost::current().expect("installed");

    for branch in ["host", "legacy"] {
        if branch == "legacy" {
            std::env::set_var("SYNAPS_SUBAGENT_FRESH_RUNTIME", "1");
        }
        let runs = run_three_paths(&sim).await;
        std::env::remove_var("SYNAPS_SUBAGENT_FRESH_RUNTIME");
        for (path, requests) in ["oneshot", "start", "resume"].iter().zip(&runs) {
            cache_sim::assert_worker_kept_warm(&format!("{branch} {path}"), requests);
        }
        // Stopped with the rounds: nothing once the workers are done.
        tokio::time::sleep(cache_sim::WORKER_INTERVAL * 3).await;
        assert!(cache_sim::take(&sim).is_empty(), "a ping after the workers");
    }

    // Control: the key off through the host's config, on both branches.
    std::fs::write(base.path().join("config"), "cache_keepwarm_tools = off\n").unwrap();
    sim.lock().unwrap().script = cache_sim::Script::outlives_ttl();
    host.reload_config();
    for branch in ["host", "legacy"] {
        if branch == "legacy" {
            std::env::set_var("SYNAPS_SUBAGENT_FRESH_RUNTIME", "1");
        }
        let requests = oneshot(&sim).await;
        std::env::remove_var("SYNAPS_SUBAGENT_FRESH_RUNTIME");
        cache_sim::assert_worker_rewrites(&format!("{branch} off"), &requests);
    }
}
