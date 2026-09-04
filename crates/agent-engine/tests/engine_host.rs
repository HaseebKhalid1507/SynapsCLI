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

    // A second, different host is rejected and never replaces the first.
    let other = EngineHost::boot(HostOpts {
        profile: None,
        no_extensions: true,
    })
    .await
    .unwrap();
    assert!(EngineHost::install(Arc::clone(&other)).is_err());
    assert!(Arc::ptr_eq(&a, &EngineHost::current().unwrap()));
    // Re-installing the same host is Ok (idempotent).
    assert!(EngineHost::install(Arc::clone(&a)).is_ok());
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
