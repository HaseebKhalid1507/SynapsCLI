//! Worker runtimes built by `EngineHost` keep `Runtime::new()`'s values where
//! it matters for a subagent that runs on its own throwaway tokio runtime:
//! no progressive disclosure (its registry has no activation tools) and its
//! own HTTP client (hyper parks connection drivers on the runtime that
//! opened them).
//!
//! Own process: boots a host with `progressive_tool_disclosure = true`.

use std::sync::Arc;

use agent_engine::{EngineHost, HostOpts, Tool, ToolContext, ToolRegistry};
use serde_json::{json, Value};

struct ExtTool;

#[async_trait::async_trait]
impl Tool for ExtTool {
    fn name(&self) -> &str {
        "alpha:do_thing"
    }
    fn description(&self) -> &str {
        "ext"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
    async fn execute(&self, _p: Value, _c: ToolContext) -> agent_engine::Result<String> {
        Ok("ok".into())
    }
    fn extension_id(&self) -> Option<&str> {
        Some("alpha")
    }
}

async fn host_with_disclosure_on() -> Arc<EngineHost> {
    static INIT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    INIT.get_or_init(|| async {
        let home = std::env::temp_dir().join(format!("synaps-worker-iso-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config"), "progressive_tool_disclosure = true\n").unwrap();
        agent_engine::config::set_base_dir_for_tests(home);
        let h = EngineHost::boot(HostOpts {
            profile: None,
            no_extensions: true,
        })
        .await
        .expect("host boot");
        // Simulate an extension registering into the shared catalog.
        h.parts().tools.write().await.register(Arc::new(ExtTool));
        assert!(EngineHost::install(h).is_ok(), "first install");
    })
    .await;
    EngineHost::current().expect("installed")
}

#[tokio::test]
async fn worker_ignores_host_progressive_disclosure_and_keeps_extension_tools() {
    let h = host_with_disclosure_on().await;
    assert!(h.parts().progressive_tool_disclosure, "host has the flag on");
    let fg = h.foreground_runtime().await.unwrap();
    assert!(fg.progressive_tool_disclosure(), "foreground follows config");

    let w = h.worker_runtime().await.unwrap();
    assert!(
        !w.progressive_tool_disclosure(),
        "worker must be `Runtime::new()`'s value: disclosure off"
    );

    // With the flag off the worker sends its full registry schema, which
    // must still carry the extension tool on top of the builtin set.
    let core = ToolRegistry::without_subagent().tools_schema().len();
    let reg = w.tools_shared();
    let reg = reg.read().await;
    let schema = reg.tools_schema();
    assert!(
        schema.len() > core,
        "worker schema {} must exceed builtin-only {}",
        schema.len(),
        core
    );
    assert!(reg.get("alpha:do_thing").is_some(), "extension tool present");
}


#[tokio::test]
async fn worker_has_its_own_http_client_pool() {
    let h = host_with_disclosure_on().await;
    let fg = h.foreground_runtime().await.unwrap();
    let w1 = h.worker_runtime().await.unwrap();
    let w2 = h.worker_runtime().await.unwrap();
    let fg_id = fg.http_client_pool_id();
    assert_ne!(fg_id, w1.http_client_pool_id(), "worker pool != host pool");
    assert_ne!(fg_id, w2.http_client_pool_id(), "worker pool != host pool");
    assert_ne!(
        w1.http_client_pool_id(),
        w2.http_client_pool_id(),
        "each worker owns its pool"
    );
    // Foregrounds keep sharing the host's client.
    let fg2 = h.foreground_runtime().await.unwrap();
    assert_eq!(fg_id, fg2.http_client_pool_id());
}
