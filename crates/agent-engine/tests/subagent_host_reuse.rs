//! B2: subagents spawn from the process `EngineHost` — shared credential
//! source / token cache (no `set_global_broker` re-install, no cache eviction),
//! cached worker registry template, one HTTP client. Legacy env restores the
//! fresh-runtime path.

use agent_engine::{EngineHost, HostOpts};
use std::sync::Arc;

async fn host() -> Arc<EngineHost> {
    if let Some(h) = EngineHost::current() {
        return h;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    std::env::set_var("SYNAPS_BASE_DIR", tmp.path());
    std::mem::forget(tmp);
    let h = EngineHost::boot(HostOpts {
        profile: None,
        no_extensions: true,
    })
    .await
    .expect("host boots");
    let _ = EngineHost::install(Arc::clone(&h));
    EngineHost::current().expect("installed")
}

/// One test body: the legacy path re-installs the broker by design, so both
/// halves must run sequentially in this process (cargo runs tests in parallel).
#[tokio::test]
async fn host_spawns_share_broker_and_cached_registry_then_legacy_path() {
    three_spawns_share_broker_and_cached_registry().await;
    legacy_env_builds_fresh_runtime().await;
}

async fn three_spawns_share_broker_and_cached_registry() {
    std::env::remove_var("SYNAPS_SUBAGENT_FRESH_RUNTIME");
    let host = host().await;
    // The foreground runtime is where the broker is installed — exactly once.
    let _fg = host.foreground_runtime().await.expect("foreground runtime");
    let broker_before = agent_core::auth::global_broker();
    let template = host.worker_registry().await;

    let mut workers = Vec::new();
    for _ in 0..3 {
        let rt = agent_engine::tools::spawn_runtime()
            .await
            .expect("worker runtime from host");
        workers.push(rt);
    }

    // The real bug: every legacy spawn re-installed the global broker with a
    // fresh TokenCache, evicting the process-wide cache. Host workers must not.
    let broker_after = agent_core::auth::global_broker();
    assert!(
        Arc::ptr_eq(&broker_before, &broker_after),
        "global broker must not be re-installed by subagent spawns"
    );

    // Registry cache hit: no rebuild while the shared catalog generation is
    // unchanged — the template's schema Arc is handed out, not rebuilt.
    let again = host.worker_registry().await;
    assert!(
        Arc::ptr_eq(&template.tools_schema(), &again.tools_schema()),
        "worker registry must be served from the generation cache"
    );
    for w in &workers {
        let schema = w.tools_shared().read().await.tools_schema();
        assert!(
            Arc::ptr_eq(&template.tools_schema(), &schema),
            "each worker holds a clone of the cached template"
        );
    }
    // Workers never carry the subagent tools themselves.
    for w in &workers {
        let reg = w.tools_shared();
        let reg = reg.read().await;
        assert!(
            !reg.tools_schema().iter().any(|t| t["name"]
                .as_str()
                .is_some_and(|n| n.starts_with("subagent"))),
            "worker registry excludes subagent tools"
        );
    }
}

async fn legacy_env_builds_fresh_runtime() {
    let _host = host().await;
    std::env::set_var("SYNAPS_SUBAGENT_FRESH_RUNTIME", "1");
    assert!(agent_engine::tools::legacy_fresh_runtime());
    let rt = agent_engine::tools::spawn_runtime()
        .await
        .expect("legacy runtime");
    std::env::remove_var("SYNAPS_SUBAGENT_FRESH_RUNTIME");
    // Fresh registry: not the host's cached template.
    let template = _host.worker_registry().await;
    let schema = rt.tools_shared().read().await.tools_schema();
    assert!(
        !Arc::ptr_eq(&template.tools_schema(), &schema),
        "legacy path rebuilds its own registry"
    );
}
