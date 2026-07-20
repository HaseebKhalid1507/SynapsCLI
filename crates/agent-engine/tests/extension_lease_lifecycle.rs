//! Task 20 Commit B — session-scoped exact extension runtime leases.
//!
//! Uses the checked-in Python extension fixture (Content-Length framed
//! JSON-RPC 2.0, proper JSON parsing; no sockets, no network). Every
//! invariant is observed via the fixture spy log, weak liveness tokens,
//! and typed errors.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_engine::extensions::hooks::HookBus;
use agent_engine::extensions::lease::{ExtensionLeaseCapability, ExtensionSessionEndGuard};
use agent_engine::extensions::lifecycle::dormant_extension_tools;
use agent_engine::extensions::manager::ExtensionManager;
use agent_engine::extensions::manifest::ExtensionManifest;
use agent_engine::tools::activation::{
    activate_exact_for_user, ExecutionGate, SessionId, SessionToolSet,
};
use agent_engine::tools::catalog::{
    DiscoveryIndex, DiscoveryQuery, SchemaDigest, SearchLimits, ToolId,
};
use agent_engine::tools::{ToolCapabilities, ToolChannels, ToolContext, ToolLimits, ToolRegistry};
use serde_json::{json, Value};

const PLUGIN: &str = "fixture-plugin";

// ── fixture plumbing ────────────────────────────────────────────────────────

fn fixture_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extension_fixture.py")
}

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("synaps-ext-lease-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn search_schema() -> Value {
    json!({"type":"object","properties":{"q":{"type":"string"}},"required":["q"]})
}

fn sibling_schema() -> Value {
    json!({"type":"object","properties":{}})
}

/// Runtime registration matching the manifest declarations exactly.
fn advertised_tools() -> Value {
    json!([
        {"name": "search", "description": "deferred search", "input_schema": search_schema()},
        {"name": "sibling", "description": "sibling stays dormant", "input_schema": sibling_schema()},
    ])
}

struct Fixture {
    dir: PathBuf,
    spy: PathBuf,
    manifest: ExtensionManifest,
}

fn fixture(tag: &str, mode: &str, tools: Value) -> Fixture {
    let dir = tmp_dir(tag);
    let spy = dir.join("spy.log");
    let tools_json = dir.join("tools.json");
    std::fs::write(&tools_json, serde_json::to_vec(&tools).unwrap()).unwrap();
    let manifest: ExtensionManifest = serde_json::from_value(json!({
        "runtime": "process",
        "command": "python3",
        "args": [
            fixture_script().display().to_string(),
            spy.display().to_string(),
            tools_json.display().to_string(),
            mode,
        ],
        "permissions": ["tools.register"],
        "deferred": {
            "tools": [
                {"name": "search", "description": "deferred search", "input_schema": search_schema()},
                {"name": "sibling", "description": "sibling stays dormant", "input_schema": sibling_schema()},
            ]
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
    fn count(&self, event: &str) -> usize {
        self.events().iter().filter(|e| e.as_str() == event).count()
    }
    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Progressive manager with the fixture manifest deferred-loaded.
async fn deferred_manager(
    fx: &Fixture,
) -> (ExtensionManager, Arc<tokio::sync::RwLock<ToolRegistry>>) {
    let registry = Arc::new(tokio::sync::RwLock::new(ToolRegistry::new()));
    let mut mgr = ExtensionManager::new_with_tools(Arc::new(HookBus::new()), Arc::clone(&registry));
    mgr.set_progressive_deferral(true);
    mgr.load(PLUGIN, &fx.manifest).await.unwrap();
    (mgr, registry)
}

fn sid() -> SessionId {
    SessionId::parse("task20-lease").unwrap()
}

fn ctx_with(cap: Option<ExtensionLeaseCapability>) -> ToolContext {
    ToolContext {
        channels: ToolChannels {
            tx_delta: None,
            tx_events: None,
        },
        capabilities: ToolCapabilities {
            watcher_exit_path: None,
            tool_register_tx: None,
            session_manager: None,
            subagent_registry: None,
            event_queue: None,
            delegation_parent: None,
            secret_prompt: None,
            orchestration: None,
            tool_activation: None,
            mcp_leases: None,
            extension_leases: cap,
            memory_context: None,
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

fn shared_set_with_activations(
    registry: &ToolRegistry,
) -> agent_engine::tools::activation::SharedSessionToolSet {
    let mut set = SessionToolSet::progressive_core_for_catalog(sid(), registry.catalog());
    activate_exact_for_user(
        &mut set,
        registry.catalog(),
        &ToolId::extension(PLUGIN, "search"),
    )
    .unwrap();
    activate_exact_for_user(
        &mut set,
        registry.catalog(),
        &ToolId::extension(PLUGIN, "sibling"),
    )
    .unwrap();
    Arc::new(std::sync::RwLock::new(set))
}

fn activation_cap_for(
    registry: &ToolRegistry,
    shared: &agent_engine::tools::activation::SharedSessionToolSet,
) -> agent_engine::tools::discovery::ActivationCapability {
    agent_engine::tools::discovery::ActivationCapability::new(
        registry.catalog().clone(),
        Arc::clone(shared),
        agent_engine::tools::activation::ActivationAuthority::Unauthorized,
    )
}

fn ctx_full(
    lease: ExtensionLeaseCapability,
    activation: agent_engine::tools::discovery::ActivationCapability,
) -> ToolContext {
    let mut ctx = ctx_with(Some(lease));
    ctx.capabilities.tool_activation = Some(activation);
    ctx
}

async fn wait_until(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

// ── tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn zero_spawn_during_load_search_and_exact_activation() {
    let fx = fixture("zero-spawn", "ok", advertised_tools());
    let (mgr, registry) = deferred_manager(&fx).await;
    let runtime = mgr.extension_runtime();

    let reg = registry.read().await;
    // Searchable passively.
    let index = DiscoveryIndex::build(reg.catalog()).unwrap();
    let hits = index.search(
        &DiscoveryQuery::parse("deferred search").unwrap(),
        &SearchLimits::new(16, 8 * 1024).unwrap(),
    );
    assert!(hits
        .hits()
        .iter()
        .any(|h| h.id() == &ToolId::extension(PLUGIN, "search")));

    // Exact activation + gate authorization with the runtime PRESENT:
    // still no spawn.
    let mut set = SessionToolSet::progressive_core_for_catalog(sid(), reg.catalog());
    activate_exact_for_user(
        &mut set,
        reg.catalog(),
        &ToolId::extension(PLUGIN, "search"),
    )
    .unwrap();
    ExecutionGate::authorize_wire_call(&reg, &set, "fixture-plugin:search").unwrap();

    assert!(fx.events().is_empty(), "no process before leased execution");
    assert_eq!(runtime.lease_count(), 0);
    fx.cleanup();
}

#[tokio::test]
async fn exact_execute_starts_once_reuses_and_calls_only_exact_tool() {
    let fx = fixture("exact-once", "ok", advertised_tools());
    let (mgr, _registry) = deferred_manager(&fx).await;
    let runtime = mgr.extension_runtime();
    let cap = ExtensionLeaseCapability::new(sid(), Arc::clone(&runtime));

    let tools = dormant_extension_tools(PLUGIN, &fx.manifest);
    let search = tools
        .iter()
        .find(|t| t.name() == "fixture-plugin:search")
        .unwrap();

    let out1 = search
        .execute(json!({"q":"a"}), ctx_with(Some(cap.clone())))
        .await
        .unwrap();
    let out2 = search
        .execute(json!({"q":"b"}), ctx_with(Some(cap.clone())))
        .await
        .unwrap();
    assert_eq!(out1, "called:search");
    assert_eq!(out2, "called:search");

    assert_eq!(fx.count("spawn"), 1, "one child for two calls");
    assert_eq!(fx.count("request:initialize"), 1, "initialize once");
    assert_eq!(fx.count("call:search"), 2);
    assert_eq!(fx.count("call:sibling"), 0);
    assert_eq!(runtime.lease_count(), 1);
    runtime.terminate_all();
    fx.cleanup();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_first_calls_are_single_flight() {
    let fx = fixture("single-flight", "ok", advertised_tools());
    let (mgr, _registry) = deferred_manager(&fx).await;
    let runtime = mgr.extension_runtime();
    let digest = SchemaDigest::of_schema(&search_schema());

    let session = sid();
    let a = runtime.call_exact(&session, PLUGIN, "search", &digest, json!({"q":"a"}));
    let b = runtime.call_exact(&session, PLUGIN, "search", &digest, json!({"q":"b"}));
    // Outer timeout guards against a lost-wakeup hang regression.
    let (ra, rb) = tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(a, b) })
        .await
        .expect("single-flight follower must never hang");
    assert_eq!(ra.unwrap()["content"], json!("called:search"));
    assert_eq!(rb.unwrap()["content"], json!("called:search"));
    assert_eq!(fx.count("spawn"), 1, "concurrent first calls: ONE child");
    assert_eq!(fx.count("request:initialize"), 1);
    runtime.terminate_all();
    fx.cleanup();
}

#[tokio::test]
async fn sibling_stays_gate_denied_and_is_never_called() {
    let fx = fixture("sibling", "ok", advertised_tools());
    let (mgr, registry) = deferred_manager(&fx).await;
    let runtime = mgr.extension_runtime();
    let cap = ExtensionLeaseCapability::new(sid(), Arc::clone(&runtime));

    let reg = registry.read().await;
    let mut set = SessionToolSet::progressive_core_for_catalog(sid(), reg.catalog());
    activate_exact_for_user(
        &mut set,
        reg.catalog(),
        &ToolId::extension(PLUGIN, "search"),
    )
    .unwrap();

    // Exact tool authorized and leased; the sibling on the SAME child
    // stays gate-denied and the extension never sees a sibling call.
    let tools = dormant_extension_tools(PLUGIN, &fx.manifest);
    let search = tools
        .iter()
        .find(|t| t.name() == "fixture-plugin:search")
        .unwrap();
    ExecutionGate::authorize_wire_call(&reg, &set, "fixture-plugin:search").unwrap();
    search
        .execute(json!({"q":"x"}), ctx_with(Some(cap.clone())))
        .await
        .unwrap();
    assert!(
        ExecutionGate::authorize_wire_call(&reg, &set, "fixture-plugin:sibling").is_err(),
        "sibling must stay ungranted"
    );
    assert_eq!(fx.count("call:search"), 1);
    assert_eq!(fx.count("call:sibling"), 0);
    runtime.terminate_all();
    fx.cleanup();
}

#[tokio::test]
async fn runtime_declaration_mismatch_terminates_without_call_and_revokes_exact_grant() {
    // Runtime advertises a DIFFERENT schema for `search` than the manifest
    // declared: initialize-time validation must shut the child down before
    // any call, and the exact grant must fall.
    let drifted = json!([
        {"name": "search", "description": "deferred search", "input_schema": {"type":"object","properties":{"evil":{"type":"string"}}}},
        {"name": "sibling", "description": "sibling stays dormant", "input_schema": sibling_schema()},
    ]);
    let fx = fixture("mismatch", "ok", drifted);
    let (mgr, registry) = deferred_manager(&fx).await;
    let runtime = mgr.extension_runtime();
    let cap = ExtensionLeaseCapability::new(sid(), Arc::clone(&runtime));

    let reg = registry.read().await;
    let catalog_generation = reg.catalog().generation();
    let shared = shared_set_with_activations(&reg);
    let tools = dormant_extension_tools(PLUGIN, &fx.manifest);
    let search = tools
        .iter()
        .find(|t| t.name() == "fixture-plugin:search")
        .unwrap();

    let err = search
        .execute(
            json!({"q":"x"}),
            ctx_full(cap.clone(), activation_cap_for(&reg, &shared)),
        )
        .await
        .expect_err("declaration mismatch must deny");
    assert!(err.to_string().contains("declarations"), "{err}");
    assert_eq!(fx.count("call:search"), 0, "no call after mismatch");
    assert_eq!(runtime.lease_count(), 0, "poisoned lease terminated");
    assert!(
        wait_until(|| fx.count("shutdown") == 1).await,
        "child shut down"
    );

    let set = shared.read().unwrap();
    assert!(
        set.activation(&ToolId::extension(PLUGIN, "search"))
            .is_none(),
        "exact grant revoked"
    );
    assert!(
        set.activation(&ToolId::extension(PLUGIN, "sibling"))
            .is_some(),
        "sibling grant untouched"
    );
    drop(set);
    assert_eq!(
        reg.catalog().generation(),
        catalog_generation,
        "no catalog mutation"
    );
    fx.cleanup();
}

#[tokio::test]
async fn permission_drift_fails_closed_before_spawn_and_revokes_grant() {
    let fx = fixture("perm-drift", "ok", advertised_tools());
    let (mgr, registry) = deferred_manager(&fx).await;
    let runtime = mgr.extension_runtime();
    let cap = ExtensionLeaseCapability::new(sid(), Arc::clone(&runtime));

    // Tamper the retained record's permissions so re-validation at
    // acquisition fails (deferred tools now lack tools.register).
    assert!(
        mgr.tamper_deferred_manifest_permissions_for_tests(PLUGIN, vec!["memory.read".to_string()])
    );

    let reg = registry.read().await;
    let shared = shared_set_with_activations(&reg);
    let tools = dormant_extension_tools(PLUGIN, &fx.manifest);
    let search = tools
        .iter()
        .find(|t| t.name() == "fixture-plugin:search")
        .unwrap();
    let err = search
        .execute(
            json!({"q":"x"}),
            ctx_full(cap.clone(), activation_cap_for(&reg, &shared)),
        )
        .await
        .expect_err("invalid permissions must deny");
    assert!(err.to_string().contains("re-validation"), "{err}");
    assert_eq!(fx.count("spawn"), 0, "failed BEFORE any spawn");
    assert_eq!(runtime.lease_count(), 0);
    assert!(
        shared
            .read()
            .unwrap()
            .activation(&ToolId::extension(PLUGIN, "search"))
            .is_none(),
        "exact grant revoked"
    );
    fx.cleanup();
}

#[tokio::test]
async fn config_drift_invalidates_live_lease_and_revokes_grant() {
    let fx = fixture("config-drift", "ok", advertised_tools());
    let (mgr, registry) = deferred_manager(&fx).await;
    let runtime = mgr.extension_runtime();
    let cap = ExtensionLeaseCapability::new(sid(), Arc::clone(&runtime));

    let reg = registry.read().await;
    let shared = shared_set_with_activations(&reg);
    let tools = dormant_extension_tools(PLUGIN, &fx.manifest);
    let search = tools
        .iter()
        .find(|t| t.name() == "fixture-plugin:search")
        .unwrap();

    // Healthy call first (lease live, grant intact).
    search
        .execute(
            json!({"q":"a"}),
            ctx_full(cap.clone(), activation_cap_for(&reg, &shared)),
        )
        .await
        .unwrap();
    let weak = runtime
        .lease_liveness_for_tests(&sid(), PLUGIN)
        .expect("lease live");

    // Drift the retained RESOLVED config: the live lease's pinned launch
    // fingerprint no longer matches — next call denies, terminates, and
    // revokes the exact grant. The child is never called again.
    assert!(mgr.tamper_deferred_config_for_tests(PLUGIN, json!({"api_key": "rotated"})));
    let err = search
        .execute(
            json!({"q":"b"}),
            ctx_full(cap.clone(), activation_cap_for(&reg, &shared)),
        )
        .await
        .expect_err("launch drift must deny");
    assert!(err.to_string().contains("changed"), "{err}");
    assert_eq!(runtime.lease_count(), 0, "lease gone");
    assert_eq!(fx.count("call:search"), 1, "no call after drift");
    assert!(
        wait_until(|| weak.upgrade().is_none()).await,
        "ownership released"
    );
    assert!(
        shared
            .read()
            .unwrap()
            .activation(&ToolId::extension(PLUGIN, "search"))
            .is_none(),
        "exact grant revoked"
    );
    assert!(
        shared
            .read()
            .unwrap()
            .activation(&ToolId::extension(PLUGIN, "sibling"))
            .is_some(),
        "sibling grant untouched"
    );
    fx.cleanup();
}

#[tokio::test]
async fn extension_reported_errors_are_withheld_and_do_not_revoke() {
    let fx = fixture("hostile-err", "hostile-error", advertised_tools());
    let (mgr, registry) = deferred_manager(&fx).await;
    let runtime = mgr.extension_runtime();
    let cap = ExtensionLeaseCapability::new(sid(), Arc::clone(&runtime));

    let reg = registry.read().await;
    let shared = shared_set_with_activations(&reg);
    let tools = dormant_extension_tools(PLUGIN, &fx.manifest);
    let search = tools
        .iter()
        .find(|t| t.name() == "fixture-plugin:search")
        .unwrap();
    let err = search
        .execute(
            json!({"q":"x"}),
            ctx_full(cap.clone(), activation_cap_for(&reg, &shared)),
        )
        .await
        .expect_err("extension-reported error must fail the call");
    let msg = err.to_string();
    assert!(!msg.contains("HOSTILE_EXTENSION_MARKER"), "{msg}");
    assert!(!msg.contains("s3cr3t"), "{msg}");
    assert!(msg.contains("withheld"), "{msg}");
    assert!(msg.len() < 300, "error stays bounded: {} bytes", msg.len());
    assert!(
        shared
            .read()
            .unwrap()
            .activation(&ToolId::extension(PLUGIN, "search"))
            .is_some(),
        "transient tool error must NOT revoke the grant"
    );
    runtime.terminate_all();
    fx.cleanup();
}

#[tokio::test]
async fn bounded_stderr_flood_does_not_break_leased_calls() {
    let fx = fixture("stderr-flood", "huge-stderr", advertised_tools());
    let (mgr, _registry) = deferred_manager(&fx).await;
    let runtime = mgr.extension_runtime();
    let cap = ExtensionLeaseCapability::new(sid(), Arc::clone(&runtime));

    let tools = dormant_extension_tools(PLUGIN, &fx.manifest);
    let search = tools
        .iter()
        .find(|t| t.name() == "fixture-plugin:search")
        .unwrap();
    // 1 MiB of newline-free stderr is drained bounded; the call succeeds.
    let out = search
        .execute(json!({"q":"x"}), ctx_with(Some(cap)))
        .await
        .unwrap();
    assert_eq!(out, "called:search");
    runtime.terminate_all();
    fx.cleanup();
}

#[tokio::test]
async fn session_end_guard_revocation_and_idle_reap_kill_children() {
    let fx = fixture("teardown", "ok", advertised_tools());
    let registry = Arc::new(tokio::sync::RwLock::new(ToolRegistry::new()));
    let mut mgr = ExtensionManager::new_with_tools(Arc::new(HookBus::new()), Arc::clone(&registry));
    mgr.set_progressive_deferral(true);
    mgr.load(PLUGIN, &fx.manifest).await.unwrap();
    // idle_max ZERO: any pre-existing lease is idle by the next acquisition.
    let runtime = mgr.extension_runtime_with_idle(Duration::ZERO);
    let digest = SchemaDigest::of_schema(&search_schema());

    // 1. RAII session-end guard kills the child on drop.
    runtime
        .call_exact(&sid(), PLUGIN, "search", &digest, json!({"q":"a"}))
        .await
        .unwrap();
    let weak = runtime.lease_liveness_for_tests(&sid(), PLUGIN).unwrap();
    let guard = ExtensionSessionEndGuard::new(sid(), Arc::clone(&runtime));
    drop(guard);
    assert_eq!(runtime.lease_count(), 0);
    assert!(
        wait_until(|| fx.count("shutdown") == 1).await,
        "guard killed child"
    );
    assert!(wait_until(|| weak.upgrade().is_none()).await);

    // 2. Re-acquire (idle reap already proved by ZERO idle_max: the
    // manager reaps on acquisition), then exact revocation kills again.
    runtime
        .call_exact(&sid(), PLUGIN, "search", &digest, json!({"q":"b"}))
        .await
        .unwrap();
    assert_eq!(fx.count("spawn"), 2, "second lease respawned");
    runtime.revoke_exact_tool(&sid(), PLUGIN, "search");
    assert_eq!(runtime.lease_count(), 0);
    assert!(
        wait_until(|| fx.count("shutdown") == 2).await,
        "revoke killed child"
    );

    // 3. No leaked child on runtime-manager drop.
    runtime
        .call_exact(&sid(), PLUGIN, "search", &digest, json!({"q":"c"}))
        .await
        .unwrap();
    assert_eq!(fx.count("spawn"), 3);
    let weak = runtime.lease_liveness_for_tests(&sid(), PLUGIN).unwrap();
    drop(runtime);
    drop(mgr);
    assert!(
        wait_until(|| weak.upgrade().is_none()).await,
        "manager drop released the last lease ownership"
    );
    fx.cleanup();
}

#[tokio::test]
async fn unload_removes_record_dormant_batch_and_live_lease() {
    let fx = fixture("unload", "ok", advertised_tools());
    let (mut mgr, registry) = deferred_manager(&fx).await;
    let runtime = mgr.extension_runtime();
    let cap = ExtensionLeaseCapability::new(sid(), Arc::clone(&runtime));
    let digest = SchemaDigest::of_schema(&search_schema());

    runtime
        .call_exact(&sid(), PLUGIN, "search", &digest, json!({"q":"a"}))
        .await
        .unwrap();
    let weak = runtime.lease_liveness_for_tests(&sid(), PLUGIN).unwrap();
    let generation_before = registry.read().await.catalog().generation();

    mgr.unload(PLUGIN).await.unwrap();

    assert!(!mgr.is_deferred_tool_only(PLUGIN), "record removed");
    assert!(
        registry
            .read()
            .await
            .catalog()
            .get(&ToolId::extension(PLUGIN, "search"))
            .is_none(),
        "dormant batch deregistered"
    );
    assert!(
        registry.read().await.catalog().generation() > generation_before,
        "catalog generation advanced so stale grants cannot survive"
    );
    assert_eq!(runtime.lease_count(), 0, "live lease terminated");
    assert!(
        wait_until(|| fx.count("shutdown") == 1).await,
        "unload killed child"
    );
    assert!(wait_until(|| weak.upgrade().is_none()).await);

    // A retained dormant tool object now fails typed without spawning.
    let tools = dormant_extension_tools(PLUGIN, &fx.manifest);
    let search = tools
        .iter()
        .find(|t| t.name() == "fixture-plugin:search")
        .unwrap();
    let err = search
        .execute(json!({"q":"x"}), ctx_with(Some(cap)))
        .await
        .expect_err("unloaded plugin must not execute");
    assert!(err.to_string().contains("no retained deferred"), "{err}");
    assert_eq!(fx.count("spawn"), 1, "no respawn after unload");
    fx.cleanup();
}

#[tokio::test]
async fn flag_off_eager_load_of_the_same_manifest_is_unchanged() {
    let fx = fixture("eager", "ok", advertised_tools());
    let registry = Arc::new(tokio::sync::RwLock::new(ToolRegistry::new()));
    let mut mgr = ExtensionManager::new_with_tools(Arc::new(HookBus::new()), Arc::clone(&registry));
    mgr.set_progressive_deferral(false);

    // The SAME manifest (deferred block present) takes the legacy eager
    // path end-to-end against the REAL fixture: spawn at load, live
    // handler, live registered tools.
    mgr.load(PLUGIN, &fx.manifest).await.unwrap();
    assert_eq!(fx.count("spawn"), 1, "eager load spawns at load time");
    assert_eq!(fx.count("request:initialize"), 1);
    assert_eq!(mgr.count(), 1, "live handler registered");
    assert!(!mgr.is_deferred_tool_only(PLUGIN));
    assert!(registry.read().await.get("fixture-plugin:search").is_some());
    mgr.shutdown_all().await;
    fx.cleanup();
}

#[tokio::test]
async fn lease_lifecycle_never_mutates_the_catalog() {
    let fx = fixture("no-catalog-drift", "ok", advertised_tools());
    let (mgr, registry) = deferred_manager(&fx).await;
    let runtime = mgr.extension_runtime();
    let cap = ExtensionLeaseCapability::new(sid(), Arc::clone(&runtime));
    let generation = registry.read().await.catalog().generation();

    let tools = dormant_extension_tools(PLUGIN, &fx.manifest);
    let search = tools
        .iter()
        .find(|t| t.name() == "fixture-plugin:search")
        .unwrap();
    search
        .execute(json!({"q":"x"}), ctx_with(Some(cap)))
        .await
        .unwrap();
    runtime.terminate_session(&sid());

    assert_eq!(
        registry.read().await.catalog().generation(),
        generation,
        "lease acquisition/termination must never mutate the catalog"
    );
    fx.cleanup();
}

#[tokio::test]
async fn durable_shared_scope_survives_turns_and_only_last_owner_terminates() {
    let fx = fixture("durable-scope", "ok", advertised_tools());
    let (mgr, _registry) = deferred_manager(&fx).await;
    let runtime = mgr.extension_runtime();
    let digest = SchemaDigest::of_schema(&search_schema());
    let session = sid();

    // Durable shared scope: what the Runtime owns and every clone/stream holds.
    let scope = Arc::new(ExtensionSessionEndGuard::new(
        session.clone(),
        Arc::clone(&runtime),
    ));

    runtime
        .call_exact(&session, PLUGIN, "search", &digest, json!({"q":"turn1"}))
        .await
        .unwrap();

    // Turn 1 ends: its HOLD on the shared scope drops — the lease survives.
    let turn1_hold = Arc::clone(&scope);
    drop(turn1_hold);
    assert_eq!(runtime.lease_count(), 1, "lease survives a turn ending");
    runtime
        .call_exact(&session, PLUGIN, "search", &digest, json!({"q":"turn2"}))
        .await
        .unwrap();
    assert_eq!(fx.count("spawn"), 1, "turn 2 reuses the retained lease");
    assert_eq!(fx.count("request:initialize"), 1, "no reinitialize");

    // A concurrent clone stream holds the SAME scope.
    let concurrent_hold = Arc::clone(&scope);
    drop(scope);
    assert_eq!(runtime.lease_count(), 1, "sibling owner keeps leases alive");

    // LAST owner drops: now — and only now — the session terminates.
    drop(concurrent_hold);
    assert_eq!(
        runtime.lease_count(),
        0,
        "last owner terminates the session"
    );
    assert!(
        wait_until(|| fx.count("shutdown") == 1).await,
        "child exits at true session end"
    );
    fx.cleanup();
}

// ── Commit C: provider / hook / sidecar / mixed / legacy classes ────────────

/// Flexible fixture: custom manifest JSON with fixture argv wired in.
fn fixture_with(
    tag: &str,
    mode: &str,
    tools: Value,
    providers: Option<Value>,
    mut manifest_json: Value,
) -> Fixture {
    let dir = tmp_dir(tag);
    let spy = dir.join("spy.log");
    let tools_json = dir.join("tools.json");
    std::fs::write(&tools_json, serde_json::to_vec(&tools).unwrap()).unwrap();
    let mut args = vec![
        fixture_script().display().to_string(),
        spy.display().to_string(),
        tools_json.display().to_string(),
        mode.to_string(),
    ];
    if let Some(providers) = providers {
        let providers_json = dir.join("providers.json");
        std::fs::write(&providers_json, serde_json::to_vec(&providers).unwrap()).unwrap();
        args.push(providers_json.display().to_string());
    }
    manifest_json["runtime"] = json!("process");
    manifest_json["command"] = json!("python3");
    manifest_json["args"] = json!(args);
    let manifest: ExtensionManifest = serde_json::from_value(manifest_json).unwrap();
    Fixture { dir, spy, manifest }
}

fn declared_provider_json() -> Value {
    json!({
        "id": "prov",
        "display_name": "Provider",
        "description": "declared provider",
        "models": [{
            "id": "model-1",
            "capabilities": {"tool_use": false},
            "context_window": 8192
        }]
    })
}

/// Runtime provider registration matching the declaration exactly.
fn registered_provider_json() -> Value {
    declared_provider_json()
}

fn provider_params() -> agent_engine::extensions::runtime::process::ProviderCompleteParams {
    agent_engine::extensions::runtime::process::ProviderCompleteParams {
        provider_id: "prov".to_string(),
        model_id: "model-1".to_string(),
        model: format!("{PLUGIN}:prov:model-1"),
        messages: vec![],
        system_prompt: None,
        tools: vec![],
        temperature: None,
        max_tokens: None,
        thinking_budget: 0,
    }
}

#[tokio::test]
async fn provider_class_registers_metadata_and_first_selection_starts_once() {
    let fx = fixture_with(
        "provider-class",
        "ok",
        json!([]),
        Some(json!([registered_provider_json()])),
        json!({
            "permissions": ["providers.register"],
            "deferred": {"providers": [declared_provider_json()]}
        }),
    );
    let (mgr, _registry) = deferred_manager(&fx).await;
    let runtime = mgr.extension_runtime();

    // Metadata registered at load with ZERO spawn.
    assert!(fx.events().is_empty(), "provider metadata must not spawn");
    assert!(mgr.is_deferred(PLUGIN));
    let provider = mgr
        .provider(&format!("{PLUGIN}:prov"))
        .expect("declared provider metadata registered");
    assert_eq!(provider.spec.display_name, "Provider");
    let handler = provider.handler.clone().expect("lazy handler attached");

    // First SELECTED provider completion starts once and validates.
    let result = handler.provider_complete(provider_params()).await.unwrap();
    assert_eq!(result.content[0]["text"], json!("provider-reply"));
    assert_eq!(fx.count("spawn"), 1);
    assert_eq!(fx.count("request:initialize"), 1);
    assert_eq!(fx.count("provider:model-1"), 1);

    // Second completion reuses the same child.
    handler.provider_complete(provider_params()).await.unwrap();
    assert_eq!(fx.count("spawn"), 1, "second selection reuses the lease");
    assert_eq!(fx.count("provider:model-1"), 2);
    assert_eq!(runtime.lease_count(), 1);
    runtime.terminate_all();
    fx.cleanup();
}

#[tokio::test]
async fn provider_runtime_mismatch_fails_before_route_and_terminates() {
    // Runtime registers a DIFFERENT display_name than declared: strict
    // declaration matching terminates the child before any provider call.
    let mut drifted = registered_provider_json();
    drifted["display_name"] = json!("Evil Provider");
    let fx = fixture_with(
        "provider-mismatch",
        "ok",
        json!([]),
        Some(json!([drifted])),
        json!({
            "permissions": ["providers.register"],
            "deferred": {"providers": [declared_provider_json()]}
        }),
    );
    let (mgr, _registry) = deferred_manager(&fx).await;
    let runtime = mgr.extension_runtime();
    let handler = mgr
        .provider(&format!("{PLUGIN}:prov"))
        .unwrap()
        .handler
        .clone()
        .unwrap();

    let err = handler
        .provider_complete(provider_params())
        .await
        .expect_err("declaration mismatch must deny the route");
    assert!(err.contains("declarations"), "{err}");
    assert_eq!(fx.count("provider:model-1"), 0, "no provider call routed");
    assert_eq!(runtime.lease_count(), 0, "poisoned lease terminated");
    assert!(
        wait_until(|| fx.count("shutdown") == 1).await,
        "mismatching child shut down"
    );
    fx.cleanup();
}

#[tokio::test]
async fn hook_class_first_authorized_matching_event_starts_once() {
    use agent_engine::extensions::hooks::events::{HookEvent, HookResult};

    let fx = fixture_with(
        "hook-class",
        "ok",
        json!([]),
        None,
        json!({
            "permissions": ["tools.intercept"],
            "hooks": [{"hook": "before_tool_call"}],
            "deferred": {}
        }),
    );
    let (mgr, _registry) = deferred_manager(&fx).await;
    let runtime = mgr.extension_runtime();
    assert!(
        fx.events().is_empty(),
        "lazy hook subscription must not spawn"
    );

    // A NON-matching event kind reaches no subscription: still no spawn.
    let unmatched = HookEvent::after_tool_call("bash", json!({}), "out".to_string());
    let _ = mgr.hook_bus().emit(&unmatched).await;
    assert!(fx.events().is_empty(), "unmatched event must not spawn");

    // First AUTHORIZED matching event starts the child once.
    let event = HookEvent::before_tool_call("bash", json!({"cmd": "ls"}));
    let result = mgr.hook_bus().emit(&event).await;
    assert!(matches!(result, HookResult::Continue));
    assert_eq!(fx.count("spawn"), 1);
    assert_eq!(fx.count("request:initialize"), 1);
    let hook_events = |fx: &Fixture| {
        fx.events()
            .iter()
            .filter(|e| e.starts_with("hook:"))
            .count()
    };
    assert_eq!(hook_events(&fx), 1);

    // Second matching event reuses the same child.
    let _ = mgr.hook_bus().emit(&event).await;
    assert_eq!(fx.count("spawn"), 1, "second event reuses the lease");
    assert_eq!(hook_events(&fx), 2);
    assert_eq!(runtime.lease_count(), 1);
    runtime.terminate_all();
    fx.cleanup();
}

#[tokio::test]
async fn sidecar_class_stays_user_triggered() {
    let fx = fixture_with(
        "sidecar-class",
        "ok",
        json!([]),
        None,
        json!({
            "permissions": ["config.write"],
            "deferred": {"lifecycle": "user"}
        }),
    );
    let (mgr, _registry) = deferred_manager(&fx).await;
    let runtime = mgr.extension_runtime();

    // Discovery/load leave the sidecar fully dormant.
    assert!(fx.events().is_empty(), "user-lifecycle load must not spawn");
    assert!(mgr.is_deferred(PLUGIN));

    // The explicit USER sidecar API is the legitimate trigger.
    let args = mgr.sidecar_spawn_args(PLUGIN).await.unwrap();
    assert_eq!(args.args, vec!["--fixture-sidecar".to_string()]);
    assert_eq!(fx.count("spawn"), 1);
    assert_eq!(fx.count("sidecar"), 1);
    runtime.terminate_all();
    fx.cleanup();
}

#[tokio::test]
async fn mixed_class_shares_one_process_and_tool_search_never_triggers() {
    let fx = fixture_with(
        "mixed-class",
        "ok",
        advertised_tools(),
        Some(json!([registered_provider_json()])),
        json!({
            "permissions": ["tools.register", "providers.register"],
            "deferred": {
                "tools": [
                    {"name": "search", "description": "deferred search", "input_schema": search_schema()},
                    {"name": "sibling", "description": "sibling stays dormant", "input_schema": sibling_schema()},
                ],
                "providers": [declared_provider_json()]
            }
        }),
    );
    let (mgr, registry) = deferred_manager(&fx).await;
    let runtime = mgr.extension_runtime();
    // Bind the handler host scope to the test session BEFORE first use so
    // tool leases and handler leases share ONE key (as the engine does).
    runtime.bind_host_scope(sid());

    // Tool SEARCH alone never triggers a start.
    let reg = registry.read().await;
    let index = DiscoveryIndex::build(reg.catalog()).unwrap();
    let hits = index.search(
        &DiscoveryQuery::parse("deferred search").unwrap(),
        &SearchLimits::new(16, 8 * 1024).unwrap(),
    );
    assert!(!hits.hits().is_empty());
    drop(reg);
    assert!(fx.events().is_empty(), "search must never spawn");

    // Earliest legitimate capability (provider selection) starts the ONE
    // shared child…
    let handler = mgr
        .provider(&format!("{PLUGIN}:prov"))
        .unwrap()
        .handler
        .clone()
        .unwrap();
    handler.provider_complete(provider_params()).await.unwrap();
    assert_eq!(fx.count("spawn"), 1);

    // …and the exact tool call REUSES the same process.
    let cap = ExtensionLeaseCapability::new(sid(), Arc::clone(&runtime));
    let tools = dormant_extension_tools(PLUGIN, &fx.manifest);
    let search = tools
        .iter()
        .find(|t| t.name() == "fixture-plugin:search")
        .unwrap();
    let out = search
        .execute(json!({"q":"x"}), ctx_with(Some(cap)))
        .await
        .unwrap();
    assert_eq!(out, "called:search");
    assert_eq!(fx.count("spawn"), 1, "mixed class shares ONE child");
    assert_eq!(fx.count("request:initialize"), 1, "initialize once");
    assert_eq!(fx.count("call:search"), 1);
    assert_eq!(runtime.lease_count(), 1, "one shared lease");
    runtime.terminate_all();
    fx.cleanup();
}

#[tokio::test]
async fn legacy_manifest_without_deferred_stays_eager_under_progressive_flag() {
    let fx = fixture_with(
        "legacy-eager",
        "ok",
        json!([]),
        None,
        json!({
            "permissions": ["tools.intercept"],
            "hooks": [{"hook": "before_tool_call"}]
        }),
    );
    let registry = Arc::new(tokio::sync::RwLock::new(ToolRegistry::new()));
    let mut mgr = ExtensionManager::new_with_tools(Arc::new(HookBus::new()), Arc::clone(&registry));
    mgr.set_progressive_deferral(true);

    // No `deferred` block => documented legacy EAGER lifecycle even with
    // the progressive flag ON.
    mgr.load(PLUGIN, &fx.manifest).await.unwrap();
    assert_eq!(fx.count("spawn"), 1, "legacy manifest spawns at load");
    assert_eq!(fx.count("request:initialize"), 1);
    assert_eq!(mgr.count(), 1, "live handler registered");
    assert!(!mgr.is_deferred(PLUGIN));
    mgr.shutdown_all().await;
    fx.cleanup();
}
