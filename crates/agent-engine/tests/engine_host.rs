//! `EngineHost` — process-global parts built once; runtimes borrow them.

use std::sync::Arc;

use agent_engine::config;
use agent_engine::{EngineHost, HostOpts};

async fn host() -> Arc<EngineHost> {
    static INIT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    INIT.get_or_init(|| async {
        let home = std::env::temp_dir().join(format!("synaps-engine-host-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        config::set_base_dir_for_tests(home);
        let h = EngineHost::boot(HostOpts {
            profile: None,
            no_extensions: true,
        })
        .await
        .expect("host boot");
        assert!(EngineHost::install(h).is_ok(), "first install");
    })
    .await;
    EngineHost::current().expect("installed")
}

#[tokio::test]
async fn boot_twice_returns_same_host() {
    let a = host().await;
    let b = EngineHost::current().unwrap();
    assert!(Arc::ptr_eq(&a, &b));

    // A second, different host is rejected and never replaces the first —
    // and never touches the routing static (boot() writes no statics; only
    // the winning install() does).
    let routing_before = agent_engine::runtime::openai::extension_manager_for_routing().unwrap();
    assert!(Arc::ptr_eq(&routing_before, a.ext_manager()));
    let other = EngineHost::boot(HostOpts {
        profile: None,
        no_extensions: true,
    })
    .await
    .unwrap();
    let rejected = EngineHost::install(Arc::clone(&other)).expect_err("rejected");
    assert!(Arc::ptr_eq(&rejected, &other));
    assert!(Arc::ptr_eq(&a, &EngineHost::current().unwrap()));
    let routing_after = agent_engine::runtime::openai::extension_manager_for_routing().unwrap();
    assert!(Arc::ptr_eq(&routing_after, a.ext_manager()), "static untouched");
    // Re-installing the same host is Ok (idempotent).
    assert!(EngineHost::install(Arc::clone(&a)).is_ok());
    // boot_and_install returns the installed host, not a fresh one.
    let again = EngineHost::boot_and_install(HostOpts {
        profile: None,
        no_extensions: true,
    })
    .await
    .unwrap();
    assert!(Arc::ptr_eq(&a, &again));
    drop(other); // process state is clean: nothing points at the loser
}

#[tokio::test]
async fn foreground_runtime_shares_tools_and_hookbus() {
    let h = host().await;
    let r1 = h.foreground_runtime().await.unwrap();
    let r2 = h.foreground_runtime().await.unwrap();
    assert!(Arc::ptr_eq(&r1.tools_shared(), &h.parts().tools));
    assert!(Arc::ptr_eq(&r2.tools_shared(), &h.parts().tools));
    assert!(Arc::ptr_eq(r1.hook_bus(), &h.parts().hook_bus));
    assert!(Arc::ptr_eq(r2.hook_bus(), &h.parts().hook_bus));
    // Independent runtimes never share tool-session identity.
    assert_ne!(r1.host_tool_session_id(), r2.host_tool_session_id());
    // Unkeyed until boot resolves a session.
    assert_eq!(r1.session_id(), None);
}

#[tokio::test]
async fn worker_runtime_fresh_hookbus_and_private_registry() {
    let h = host().await;
    let w = h.worker_runtime().await.unwrap();
    assert!(!Arc::ptr_eq(w.hook_bus(), &h.parts().hook_bus));
    assert!(!Arc::ptr_eq(&w.tools_shared(), &h.parts().tools));
    // Worker registry excludes the recursive subagent tools.
    let reg = w.tools_shared();
    let reg = reg.read().await;
    assert!(reg.get("subagent").is_none());
    assert!(reg.get("bash").is_some());
}

#[tokio::test]
async fn worker_registry_cached_by_generation() {
    let h = host().await;
    let a = h.worker_registry().await;
    let b = h.worker_registry().await;
    // Same generation → same cached template (schema Arc shared).
    assert!(Arc::ptr_eq(&a.tools_schema(), &b.tools_schema()));
    assert_eq!(a.catalog().generation(), b.catalog().generation());

    // Mutate the shared catalog → generation bump → rebuilt.
    let before = h.parts().tools.read().await.catalog().generation();
    h.parts()
        .tools
        .write()
        .await
        .disable(&["ls".to_string()]);
    let after = h.parts().tools.read().await.catalog().generation();
    assert!(after > before);
    // Rebuilt (new template), then cached again at the new generation.
    // (`without_subagent_with_extensions` rebuilds builtins from scratch, so
    // a disabled builtin is not reflected — pre-existing behaviour.)
    let c = h.worker_registry().await;
    assert!(!Arc::ptr_eq(&a.tools_schema(), &c.tools_schema()));
    let d = h.worker_registry().await;
    assert!(Arc::ptr_eq(&c.tools_schema(), &d.tools_schema()));
}

#[tokio::test]
async fn runtime_new_is_still_fully_fresh() {
    let h = host().await;
    let r = agent_engine::Runtime::new().await.unwrap();
    assert!(!Arc::ptr_eq(&r.tools_shared(), &h.parts().tools));
    assert!(!Arc::ptr_eq(r.hook_bus(), &h.parts().hook_bus));
}

/// C2: `extensions_ready()` resolves immediately when no loader was
/// dispatched, waits for a dispatched loader's `Finished`, and is immediate
/// afterwards. Uses the installed host (no plugins under the temp home, so
/// the walk itself is trivial) — the seam is the loader↔host handshake.
#[tokio::test]
async fn extensions_ready_tracks_the_loader() {
    let h = host().await;
    // No loader dispatched → immediate.
    tokio::time::timeout(std::time::Duration::from_millis(200), h.extensions_ready())
        .await
        .expect("no loader → immediate");

    // Hold the manager write lock so the dispatched loader cannot finish;
    // a waiter must now block until we release it.
    let guard = h.ext_manager().write().await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let loader = agent_engine::extensions::loader::spawn_discover_and_load(
        Arc::clone(h.ext_manager()),
        tx,
        Some("sess-ready".to_string()),
    );
    let waiter = {
        let h = Arc::clone(&h);
        tokio::spawn(async move {
            h.extensions_ready().await;
            std::time::Instant::now()
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!waiter.is_finished(), "waiter must block while discovery is pending");
    let released = std::time::Instant::now();
    drop(guard);
    loader.await.unwrap();
    let woke = waiter.await.unwrap();
    assert!(woke >= released, "woke only after the loader finished");
    assert!(matches!(
        rx.recv().await,
        Some(agent_engine::extensions::loader::ExtensionLoaderEvent::Started)
    ));
    // Afterwards: immediate, and the manager records discovery as done.
    tokio::time::timeout(std::time::Duration::from_millis(200), h.extensions_ready())
        .await
        .expect("ready → immediate");
    assert!(h.ext_manager().read().await.discovery_done().is_some());
}
