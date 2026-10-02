//! Boot discovery for MCP servers without cached descriptors (#427).
//!
//! A configured server with no fingerprint-matching descriptor-cache entry
//! used to contribute no tools, and the cache was only written after a leased
//! call, which needs a tool to call: a newly added server stayed invisible
//! forever. `setup_lazy_mcp` now lists such a server once at boot, records it,
//! and registers its tools. Own test binary: it points `SYNAPS_BASE_DIR` at a
//! temp dir for the whole process.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_engine::mcp::{descriptors, setup_lazy_mcp};
use agent_engine::ToolRegistry;
use serde_json::json;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

#[tokio::test]
async fn new_server_is_discovered_once_at_boot_and_its_tools_registered() {
    let base = tempfile::tempdir().unwrap();
    std::env::set_var("SYNAPS_BASE_DIR", base.path());
    std::env::remove_var("SYNAPS_MCP_CACHE_WRITEBACK");
    let spy = base.path().join("spy.log");
    let tools_json = base.path().join("tools.json");
    std::fs::write(
        &tools_json,
        json!([{
            "name": "echo_tool",
            "description": "Echo the input.",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}
        }])
        .to_string(),
    )
    .unwrap();
    let mcp = json!({"mcpServers": {"fresh": {
        "command": "python3",
        "args": [fixture("mcp_fixture_server.py").display().to_string()],
        "env": {
            "MCP_FIXTURE_SPY": spy.display().to_string(),
            "MCP_FIXTURE_TOOLS_JSON": tools_json.display().to_string(),
            "MCP_FIXTURE_MODE": "ok"
        }
    }}});
    std::fs::write(base.path().join("mcp.json"), mcp.to_string()).unwrap();
    assert!(!descriptors::default_cache_path().exists(), "no cache yet");

    // First boot: no cache entry, so the server is listed once and recorded.
    let registry = Arc::new(tokio::sync::RwLock::new(ToolRegistry::empty()));
    let registered = setup_lazy_mcp(&registry).await;
    assert_eq!(registered, 1, "the discovered tool is registered");
    assert!(registry.read().await.get("ext__fresh__echo_tool").is_some());
    let cache = descriptors::load_default_cache().expect("cache written by discovery");
    assert_eq!(cache.servers["fresh"].tools.len(), 1);
    let spawns_after_first = std::fs::read_to_string(&spy).unwrap_or_default();

    // Second boot: the fingerprint-matching entry is used; nothing is spawned.
    let registry = Arc::new(tokio::sync::RwLock::new(ToolRegistry::empty()));
    assert_eq!(setup_lazy_mcp(&registry).await, 1);
    assert_eq!(
        std::fs::read_to_string(&spy).unwrap_or_default(),
        spawns_after_first,
        "a cached server is not started again at boot"
    );
}
