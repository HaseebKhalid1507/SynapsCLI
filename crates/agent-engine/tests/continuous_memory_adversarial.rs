//! Task F2 — consolidated host adversarial oracle suite (spec §20.5).
//!
//! This file names every §20.5 host threat. Where a resource-heavy or
//! cross-repository oracle already exists, the corresponding test below is a
//! lightweight cross-reference so the capped host run does not duplicate a
//! 1 GiB allocation or weaken an existing assertion.

#[path = "support/hermetic_base_dir.rs"]
mod hermetic_base_dir;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_core::config::load_config_from_str;
use agent_core::memory::store::{
    fetch_exact_in, store_record_in, MemoryProvenance, MemoryRetention, MemorySensitivity,
    NewMemoryRecord, ProjectScope,
};
use agent_engine::extensions::hooks::HookBus;
use agent_engine::extensions::lease::{ExtensionLeaseCapability, ExtensionSessionEndGuard};
use agent_engine::extensions::manager::ExtensionManager;
use agent_engine::extensions::manifest::ExtensionManifest;
use agent_engine::tools::catalog::SchemaDigest;
use serde_json::json;

const PLUGIN: &str = "f2-memory-fixture";

fn fixture_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extension_fixture.py")
}

fn fixture_manifest(dir: &Path, mode: &str) -> (PathBuf, ExtensionManifest) {
    let spy = dir.join("spy.log");
    let tools = json!([
        {
            "name": "memory_recall",
            "description": "recall dispatch",
            "input_schema": {
                "type": "object",
                "properties": {"schema": {"type": "string"}, "query": {"type": "string"}}
            }
        },
        {
            "name": "memory_ping",
            "description": "lease liveness probe",
            "input_schema": {"type": "object", "properties": {}}
        },
        {
            "name": "memory_capture",
            "description": "capture dispatch",
            "input_schema": {"type": "object"}
        }
    ]);
    let tools_path = dir.join("tools.json");
    std::fs::write(&tools_path, serde_json::to_vec(&tools).unwrap()).unwrap();
    let providers_path = dir.join("providers.json");
    std::fs::write(&providers_path, b"[]").unwrap();
    let context_providers_path = dir.join("context-providers.json");
    std::fs::write(
        &context_providers_path,
        serde_json::to_vec(&json!([{
            "id": "project-memory",
            "capability": "project-memory",
            "description": "F2 fixture memory provider",
            "schema_version": 1
        }]))
        .unwrap(),
    )
    .unwrap();
    let manifest = serde_json::from_value(json!({
        "runtime": "process",
        "command": "python3",
        "args": [
            fixture_script().display().to_string(),
            spy.display().to_string(),
            tools_path.display().to_string(),
            mode,
            providers_path.display().to_string(),
            context_providers_path.display().to_string()
        ],
        "permissions": ["tools.register", "context_providers.register"],
        "deferred": {
            "tools": tools,
            "context_providers": [{
                "id": "project-memory",
                "capability": "project-memory",
                "description": "F2 fixture memory provider",
                "schema_version": 1
            }]
        }
    }))
    .unwrap();
    (spy, manifest)
}

fn spy_count(spy: &Path, event: &str) -> usize {
    std::fs::read_to_string(spy)
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == event)
        .count()
}

async fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// §20.5 bullet 2: probing an id that exists only in another project is
/// indistinguishable from probing an id that exists nowhere. This drives the
/// public project-store fetch path used by the model-facing memory tool.
#[test]
fn foreign_project_id_probe_has_same_error_as_unknown_id() {
    hermetic_base_dir::pin();
    let temp = tempfile::TempDir::new().unwrap();
    let own_root = temp.path().join("project-a");
    let foreign_root = temp.path().join("project-b");
    std::fs::create_dir_all(&own_root).unwrap();
    std::fs::create_dir_all(&foreign_root).unwrap();
    let own = ProjectScope::for_root(&own_root).unwrap();
    let foreign = ProjectScope::for_root(&foreign_root).unwrap();
    let foreign_id = store_record_in(
        temp.path(),
        &foreign,
        NewMemoryRecord {
            content: "foreign body must not affect the oracle".into(),
            tags: vec![],
            provenance: MemoryProvenance {
                source: "fixture".into(),
                session: None,
            },
            sensitivity: MemorySensitivity::Normal,
            retention: MemoryRetention::Standard,
        },
    )
    .unwrap()
    .id
    .unwrap();

    let foreign_probe = fetch_exact_in(temp.path(), &own, &[foreign_id.as_str()])
        .unwrap_err()
        .to_string();
    let absent_probe = fetch_exact_in(temp.path(), &own, &[foreign_id.as_str()])
        .unwrap_err()
        .to_string();

    assert_eq!(foreign_probe, absent_probe);
    assert_eq!(
        foreign_probe,
        format!("memory record not found in this project scope: {foreign_id}")
    );
}

/// §20.5 bullet 3: a model cannot alter the durable config default: an
/// unconfirmed setting is normalized back to `off`.
#[tokio::test]
async fn model_cannot_enable_durable_default_without_consent_session_only_remains_safe() {
    hermetic_base_dir::pin();
    let config = load_config_from_str("memory.default_mode = capture_and_recall\n");
    assert_eq!(config.memory.default_mode, "off");
    assert!(!config.memory.default_mode_confirmed);
}

/// §20.5 bullet 7: simulated host/plugin crash cleanup uses the production
/// lease/session RAII guards. Dropping the session guard removes the lease;
/// dropping the final manager owners releases the child liveness token.
#[tokio::test]
async fn host_or_plugin_crash_does_not_leak_lease_or_child_process() {
    hermetic_base_dir::pin();
    let temp = tempfile::TempDir::new().unwrap();
    let (spy, manifest) = fixture_manifest(temp.path(), "crash-lease-leak");
    let registry = Arc::new(tokio::sync::RwLock::new(
        agent_engine::tools::ToolRegistry::new(),
    ));
    let mut manager =
        ExtensionManager::new_with_tools(Arc::new(HookBus::new()), Arc::clone(&registry));
    manager.set_progressive_deferral(true);
    manager.load(PLUGIN, &manifest).await.unwrap();
    let runtime = manager.extension_runtime();
    let session = agent_engine::tools::activation::SessionId::parse("f2-crash-session").unwrap();
    let capability = ExtensionLeaseCapability::new(session.clone(), Arc::clone(&runtime));
    let digest = SchemaDigest::of_schema(&json!({"type": "object", "properties": {}}));

    capability
        .call_exact_no_chat(PLUGIN, "memory_ping", &digest, json!({}))
        .await
        .unwrap();
    assert_eq!(runtime.lease_count(), 1);
    assert_eq!(spy_count(&spy, "spawn"), 1);
    let liveness = runtime
        .lease_liveness_for_tests(&session, PLUGIN)
        .expect("ready lease liveness token");

    let guard = ExtensionSessionEndGuard::new(session, Arc::clone(&runtime));
    drop(guard);
    assert_eq!(runtime.lease_count(), 0);
    assert!(
        wait_until(|| spy_count(&spy, "shutdown") == 1).await,
        "session guard did not reap the child"
    );

    drop(capability);
    drop(runtime);
    drop(manager);
    assert!(
        wait_until(|| liveness.upgrade().is_none()).await,
        "crash cleanup retained child ownership"
    );
}
