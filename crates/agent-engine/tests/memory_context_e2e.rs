//! Task B6 — Phase B end-to-end harness with a REAL fixture extension
//! process (continuous-memory spec §22 Phase B gate: "real provider-turn
//! tests prove bounded per-prompt injection without user/system-role
//! confusion").
//!
//! Uses the checked-in Python extension fixture (Content-Length framed
//! JSON-RPC 2.0 over stdio — the same real protocol the T20 lease tests
//! drive; no sockets, no network, no test doubles). `/memory` was removed,
//! so only the mode-off case is left here; the per-prompt recall runs
//! through the runtime's REAL stream hook
//! (`apply_turn_memory_recall`, the exact function
//! `run_stream_with_messages` calls) with the REAL
//! `ExtensionLeaseCapability` spawn boundary — only the outer provider
//! HTTP call is absent, exactly like the existing fixture-based tests.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_engine::extensions::hooks::HookBus;
use agent_engine::extensions::lease::ExtensionRuntimeManager;
use agent_engine::extensions::manager::ExtensionManager;
use agent_engine::extensions::manifest::ExtensionManifest;
use agent_engine::tools::ToolRegistry;
use agent_engine::{Runtime, SharedMessage};
use serde_json::{json, Value};

const PLUGIN: &str = "memory-fixture-plugin";

// ── fixture plumbing (mirrors extension_lease_lifecycle.rs) ─────────────────

fn fixture_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extension_fixture.py")
}

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("synaps-mem-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn recall_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "schema": {"type": "string"},
            "query": {"type": "string"}
        }
    })
}

fn ping_schema() -> Value {
    json!({"type": "object", "properties": {}})
}

/// Manifest + runtime tool declarations (must match EXACTLY: the lease
/// path validates the runtime registration against the declaration).
fn declared_tools() -> Value {
    json!([
        {"name": "memory_recall", "description": "recall dispatch", "input_schema": recall_schema()},
        {"name": "memory_ping", "description": "lease warm ping", "input_schema": ping_schema()},
        {"name": "memory_capture", "description": "capture dispatch", "input_schema": {"type": "object"}},
    ])
}

/// Task A3 `DeclaredExtensionContextProvider` shape.
fn declared_context_providers() -> Value {
    json!([{
        "id": "project-memory",
        "capability": "project-memory",
        "description": "fixture continuous-memory context provider",
        "schema_version": 1
    }])
}

struct Fixture {
    dir: PathBuf,
    spy: PathBuf,
    manifest: ExtensionManifest,
}

/// Build a fixture whose manifest declares BOTH the deferred recall tools
/// and a `context_providers` entry, with the fixture argv extended by the
/// new optional context-providers slot (argv[5]).
fn fixture(tag: &str, mode: &str) -> Fixture {
    let dir = tmp_dir(tag);
    let spy = dir.join("spy.log");
    let tools_json = dir.join("tools.json");
    std::fs::write(&tools_json, serde_json::to_vec(&declared_tools()).unwrap()).unwrap();
    // Slot 4 (providers) stays in-contract as an empty array so slot 5 can
    // carry the context-provider declarations.
    let providers_json = dir.join("providers.json");
    std::fs::write(&providers_json, b"[]").unwrap();
    let context_providers_json = dir.join("context_providers.json");
    std::fs::write(
        &context_providers_json,
        serde_json::to_vec(&declared_context_providers()).unwrap(),
    )
    .unwrap();
    let manifest: ExtensionManifest = serde_json::from_value(json!({
        "runtime": "process",
        "command": "python3",
        "args": [
            fixture_script().display().to_string(),
            spy.display().to_string(),
            tools_json.display().to_string(),
            mode,
            providers_json.display().to_string(),
            context_providers_json.display().to_string(),
        ],
        "permissions": ["tools.register", "context_providers.register"],
        "deferred": {
            "tools": [
                {"name": "memory_recall", "description": "recall dispatch", "input_schema": recall_schema()},
                {"name": "memory_ping", "description": "lease warm ping", "input_schema": ping_schema()},
                {"name": "memory_capture", "description": "capture dispatch", "input_schema": {"type": "object"}},
            ],
            "context_providers": [{
                "id": "project-memory",
                "capability": "project-memory",
                "description": "fixture continuous-memory context provider",
                "schema_version": 1
            }]
        }
    }))
    .unwrap();
    Fixture { dir, spy, manifest }
}

impl Fixture {
    fn events(&self) -> Vec<String> {
        std::fs::read_to_string(&self.spy)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ── engine assembly ─────────────────────────────────────────────────────────

/// One fully wired engine: progressive-deferral extension manager loaded
/// with the fixture manifest, and a REAL `Runtime` with the extension
/// lease runtime installed (the same wiring `install_extension_runtime`
/// performs at engine boot). Construction spawns NOTHING.
struct Engine {
    runtime: Runtime,
    mgr_runtime: Arc<ExtensionRuntimeManager>,
    _mgr: ExtensionManager,
}

async fn engine(fx: &Fixture) -> Engine {
    let registry = Arc::new(tokio::sync::RwLock::new(ToolRegistry::new()));
    let mut mgr = ExtensionManager::new_with_tools(Arc::new(HookBus::new()), Arc::clone(&registry));
    mgr.set_progressive_deferral(true);
    mgr.load(PLUGIN, &fx.manifest).await.unwrap();
    let mgr_runtime = mgr.extension_runtime();
    let mut runtime = Runtime::new().await.expect("credential-blind construction");
    runtime.install_extension_runtime(Arc::clone(&mgr_runtime));
    Engine {
        runtime,
        mgr_runtime,
        _mgr: mgr,
    }
}

// ── message helpers ─────────────────────────────────────────────────────────

fn shared(values: Vec<Value>) -> Vec<SharedMessage> {
    values.into_iter().map(Arc::new).collect()
}

/// Simulated prompt N of a growing conversation; the trailing element is
/// the REAL new user message, in content-array form so the "never merged
/// into the user's own content array" invariant is observable.
fn history(prompts: usize) -> Vec<SharedMessage> {
    let mut values = Vec::new();
    for turn in 1..prompts {
        values.push(json!({"role": "user", "content": format!("earlier prompt {turn}")}));
        values.push(json!({"role": "assistant", "content": format!("earlier answer {turn}")}));
    }
    values.push(json!({
        "role": "user",
        "content": [{"type": "text", "text": format!("prompt {prompts}: what did we decide about auth?")}]
    }));
    shared(values)
}

fn wire_bytes(messages: &[SharedMessage]) -> Vec<u8> {
    serde_json::to_vec(&messages.iter().map(|m| (**m).clone()).collect::<Vec<_>>()).unwrap()
}

// ── tests ───────────────────────────────────────────────────────────────────

/// (e) Mode Off (the default — no `/memory` command ever ran): an eligible
/// prompt spawns NOTHING. The spy log never even comes into existence.
#[tokio::test]
async fn memory_off_never_spawns_the_fixture() {
    let fx = fixture("off", "ok");
    let engine = engine(&fx).await;

    let mut messages = history(1);
    let before = wire_bytes(&messages);
    engine
        .runtime
        .apply_turn_memory_recall_for_harness(&mut messages)
        .await;
    assert_eq!(wire_bytes(&messages), before);
    assert!(
        !fx.spy.exists() || fx.events().is_empty(),
        "zero fixture processes under mode Off: {:?}",
        fx.events()
    );
    assert_eq!(engine.mgr_runtime.lease_count(), 0);
    fx.cleanup();
}
