//! `memory.default_mode` (spec §12) applies to every foreground session the
//! host builds — including each fresh one — but never to worker runtimes.
//!
//! Own process: boots a host whose config confirms `capture_and_recall`.

use std::sync::Arc;

use agent_engine::runtime::memory_context::{DurableStatus, MemoryContextMode};
use agent_engine::{EngineHost, HostOpts};

async fn host_with_default_mode() -> Arc<EngineHost> {
    static INIT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    INIT.get_or_init(|| async {
        let home =
            std::env::temp_dir().join(format!("synaps-memory-default-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config"),
            "memory.default_mode = capture_and_recall\nmemory.default_mode_confirmed = true\n",
        )
        .unwrap();
        agent_engine::config::set_base_dir_for_tests(home);
        let h = EngineHost::boot(HostOpts {
            profile: None,
            no_extensions: true,
            mcp_discovery: agent_engine::McpDiscovery::Inline,
        })
        .await
        .expect("host boot");
        assert!(EngineHost::install(h).is_ok(), "first install");
    })
    .await;
    EngineHost::current().expect("installed")
}

fn mode_of(status: &DurableStatus) -> Option<MemoryContextMode> {
    match status {
        DurableStatus::Active { mode, .. } => Some(*mode),
        DurableStatus::Off => None,
    }
}

#[tokio::test]
async fn every_foreground_session_starts_in_the_confirmed_default_mode() {
    let h = host_with_default_mode().await;
    let fg1 = h.foreground_runtime().await.unwrap();
    let fg2 = h.foreground_runtime().await.unwrap();
    for fg in [&fg1, &fg2] {
        assert_eq!(
            mode_of(&fg.memory_context_status().durable),
            Some(MemoryContextMode::CaptureAndRecall)
        );
    }
    // Sessions are independent: turning one off leaves the other on.
    fg1.memory_context_disable();
    assert_eq!(fg1.memory_context_status().durable, DurableStatus::Off);
    assert_eq!(
        mode_of(&fg2.memory_context_status().durable),
        Some(MemoryContextMode::CaptureAndRecall)
    );
}

#[tokio::test]
async fn workers_do_not_inherit_the_default_mode() {
    let h = host_with_default_mode().await;
    let w = h.worker_runtime().await.unwrap();
    assert_eq!(w.memory_context_status().durable, DurableStatus::Off);
}
