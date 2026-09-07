//! Task 19 Commit B — session-scoped exact MCP runtime leases.
//!
//! Uses the checked-in Python stdio JSON-RPC fixture (json.loads/json.dumps
//! per line; no sockets, no network). Every invariant here is observed via
//! the fixture spy log, weak liveness tokens, and typed errors.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_engine::mcp::descriptors::{
    dormant_tools_for_config, server_config_fingerprint, CachedServerDescriptors,
    CachedToolDescriptor, McpDescriptorCache,
};
use agent_engine::mcp::lease::{ConfigSource, McpLeaseError};
use agent_engine::mcp::{
    McpConfig, McpLeaseCapability, McpRuntimeManager, McpServerConfig, McpSessionEndGuard,
};
use agent_engine::tools::activation::{
    activate_exact_for_user, ExecutionGate, SessionId, SessionToolSet,
};
use agent_engine::tools::catalog::{SchemaDigest, ToolId};
use agent_engine::tools::{ToolCapabilities, ToolChannels, ToolContext, ToolLimits, ToolRegistry};
use serde_json::{json, Value};

// ── fixture plumbing ────────────────────────────────────────────────────────

fn fixture_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp_fixture_server.py")
}

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("synaps-mcp-lease-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn echo_schema() -> Value {
    json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]})
}

fn sibling_schema() -> Value {
    json!({"type":"object","properties":{}})
}

/// Fixture advertisement matching the cached descriptors exactly.
fn advertised_tools() -> Value {
    json!([
        {"name": "echo_tool", "description": "echoes text back", "inputSchema": echo_schema()},
        {"name": "sibling_tool", "description": "sibling stays dormant", "inputSchema": sibling_schema()},
    ])
}

struct Fixture {
    dir: PathBuf,
    spy: PathBuf,
    config: McpServerConfig,
}

fn fixture(tag: &str, mode: &str, tools: Value) -> Fixture {
    let dir = tmp_dir(tag);
    let spy = dir.join("spy.log");
    let tools_json = dir.join("tools.json");
    std::fs::write(&tools_json, serde_json::to_vec(&tools).unwrap()).unwrap();
    let mut env = HashMap::new();
    env.insert("MCP_FIXTURE_SPY".to_string(), spy.display().to_string());
    env.insert(
        "MCP_FIXTURE_TOOLS_JSON".to_string(),
        tools_json.display().to_string(),
    );
    env.insert("MCP_FIXTURE_MODE".to_string(), mode.to_string());
    let config = McpServerConfig {
        command: "python3".to_string(),
        args: vec![fixture_script().display().to_string()],
        env,
    };
    Fixture { dir, spy, config }
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

fn seeded_cache(fp: &str) -> McpDescriptorCache {
    let mut cache = McpDescriptorCache::empty();
    cache.servers.insert(
        "srv".to_string(),
        CachedServerDescriptors {
            fingerprint: fp.to_string(),
            tools: vec![
                CachedToolDescriptor {
                    name: "echo_tool".to_string(),
                    description: "echoes text back".to_string(),
                    input_schema: echo_schema(),
                },
                CachedToolDescriptor {
                    name: "sibling_tool".to_string(),
                    description: "sibling stays dormant".to_string(),
                    input_schema: sibling_schema(),
                },
            ],
        },
    );
    cache
}

fn config_with(cfg: McpServerConfig) -> McpConfig {
    let mut servers = HashMap::new();
    servers.insert("srv".to_string(), cfg);
    McpConfig {
        mcp_servers: servers,
    }
}

/// Mutable injected config source (tests mutate to simulate drift).
fn shared_source(
    cfg: McpServerConfig,
) -> (
    ConfigSource,
    Arc<std::sync::RwLock<HashMap<String, McpServerConfig>>>,
) {
    let map = Arc::new(std::sync::RwLock::new(HashMap::from([(
        "srv".to_string(),
        cfg,
    )])));
    let map2 = Arc::clone(&map);
    let source: ConfigSource =
        Arc::new(move |server: &str| map2.read().unwrap().get(server).cloned());
    (source, map)
}

fn sid() -> SessionId {
    SessionId::parse("task19-lease").unwrap()
}

fn ctx_with(cap: Option<McpLeaseCapability>) -> ToolContext {
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
            mcp_leases: cap,
            extension_leases: None,
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
async fn zero_spawn_before_leased_execution() {
    let fx = fixture("zero-spawn", "ok", advertised_tools());
    let fp = server_config_fingerprint(&fx.config);
    let (source, _) = shared_source(fx.config.clone());
    let manager = Arc::new(McpRuntimeManager::new(source, Duration::from_secs(300)));

    let mut registry = ToolRegistry::new();
    registry
        .try_register_batch(dormant_tools_for_config(
            &config_with(fx.config.clone()),
            &seeded_cache(&fp),
        ))
        .unwrap();

    // Search + exact activation with the manager PRESENT: still no spawn.
    let mut set = SessionToolSet::progressive_core_for_catalog(sid(), registry.catalog());
    activate_exact_for_user(
        &mut set,
        registry.catalog(),
        &ToolId::mcp("srv", "echo_tool"),
    )
    .unwrap();
    ExecutionGate::authorize_wire_call(&registry, &set, "ext__srv__echo_tool").unwrap();

    assert!(fx.events().is_empty(), "no process before leased execution");
    assert_eq!(manager.lease_count(), 0);
    fx.cleanup();
}

#[tokio::test]
async fn exact_execute_starts_once_reuses_and_calls_only_exact_tool() {
    let fx = fixture("exact-once", "ok", advertised_tools());
    let fp = server_config_fingerprint(&fx.config);
    let (source, _) = shared_source(fx.config.clone());
    let manager = Arc::new(McpRuntimeManager::new(source, Duration::from_secs(300)));
    let cap = McpLeaseCapability::new(sid(), Arc::clone(&manager));

    let tools = dormant_tools_for_config(&config_with(fx.config.clone()), &seeded_cache(&fp));
    let echo = tools
        .iter()
        .find(|t| t.name() == "ext__srv__echo_tool")
        .unwrap();

    let out1 = echo
        .execute(json!({"text":"a"}), ctx_with(Some(cap.clone())))
        .await
        .unwrap();
    let out2 = echo
        .execute(json!({"text":"b"}), ctx_with(Some(cap.clone())))
        .await
        .unwrap();
    assert_eq!(out1, "called:echo_tool");
    assert_eq!(out2, "called:echo_tool");

    assert_eq!(fx.count("spawn"), 1, "one child for two calls");
    assert_eq!(fx.count("request:tools/list"), 1, "initialize/list once");
    assert_eq!(fx.count("request:tools/call:echo_tool"), 2);
    assert_eq!(fx.count("request:tools/call:sibling_tool"), 0);
    assert_eq!(manager.lease_count(), 1);
    manager.terminate_all();
    fx.cleanup();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_first_calls_are_single_flight() {
    let fx = fixture("single-flight", "ok", advertised_tools());
    let fp = server_config_fingerprint(&fx.config);
    let (source, _) = shared_source(fx.config.clone());
    let manager = Arc::new(McpRuntimeManager::new(source, Duration::from_secs(300)));
    let digest = SchemaDigest::of_schema(&echo_schema());

    let session = sid();
    let a = manager.call_exact(
        &session,
        "srv",
        &fp,
        "echo_tool",
        &digest,
        json!({"text":"a"}),
    );
    let b = manager.call_exact(
        &session,
        "srv",
        &fp,
        "echo_tool",
        &digest,
        json!({"text":"b"}),
    );
    // Outer timeout guards against a lost-wakeup hang regression.
    let (ra, rb) = tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(a, b) })
        .await
        .expect("single-flight follower must never hang");
    assert_eq!(ra.unwrap(), "called:echo_tool");
    assert_eq!(rb.unwrap(), "called:echo_tool");
    assert_eq!(fx.count("spawn"), 1, "concurrent first calls: ONE child");
    assert_eq!(fx.count("request:tools/list"), 1);
    manager.terminate_all();
    fx.cleanup();
}

#[tokio::test]
async fn sibling_stays_gate_denied_and_is_never_called() {
    let fx = fixture("sibling", "ok", advertised_tools());
    let fp = server_config_fingerprint(&fx.config);
    let (source, _) = shared_source(fx.config.clone());
    let manager = Arc::new(McpRuntimeManager::new(source, Duration::from_secs(300)));
    let cap = McpLeaseCapability::new(sid(), Arc::clone(&manager));

    let mut registry = ToolRegistry::new();
    let tools = dormant_tools_for_config(&config_with(fx.config.clone()), &seeded_cache(&fp));
    registry.try_register_batch(tools.clone()).unwrap();

    let mut set = SessionToolSet::progressive_core_for_catalog(sid(), registry.catalog());
    activate_exact_for_user(
        &mut set,
        registry.catalog(),
        &ToolId::mcp("srv", "echo_tool"),
    )
    .unwrap();

    // Exact tool authorized and leased; sibling on the SAME connection
    // stays gate-denied and the server never sees a sibling call.
    let echo = tools
        .iter()
        .find(|t| t.name() == "ext__srv__echo_tool")
        .unwrap();
    ExecutionGate::authorize_wire_call(&registry, &set, "ext__srv__echo_tool").unwrap();
    echo.execute(json!({"text":"x"}), ctx_with(Some(cap.clone())))
        .await
        .unwrap();
    assert!(
        ExecutionGate::authorize_wire_call(&registry, &set, "ext__srv__sibling_tool").is_err(),
        "sibling must stay ungranted"
    );
    assert_eq!(fx.count("request:tools/call:echo_tool"), 1);
    assert_eq!(fx.count("request:tools/call:sibling_tool"), 0);
    manager.terminate_all();
    fx.cleanup();
}

#[tokio::test]
async fn fingerprint_drift_invalidates_lease_and_gracefully_kills_child() {
    let fx = fixture("drift", "ok", advertised_tools());
    let fp = server_config_fingerprint(&fx.config);
    let (source, map) = shared_source(fx.config.clone());
    let manager = Arc::new(McpRuntimeManager::new(source, Duration::from_secs(300)));
    let cap = McpLeaseCapability::new(sid(), Arc::clone(&manager));

    let tools = dormant_tools_for_config(&config_with(fx.config.clone()), &seeded_cache(&fp));
    let echo = tools
        .iter()
        .find(|t| t.name() == "ext__srv__echo_tool")
        .unwrap();
    echo.execute(json!({"text":"x"}), ctx_with(Some(cap.clone())))
        .await
        .unwrap();
    let weak = manager
        .lease_liveness_for_tests(&sid(), "srv")
        .expect("lease live");

    // Drift the CURRENT config: next call must fail typed, terminate the
    // lease, and never call the server again.
    map.write()
        .unwrap()
        .get_mut("srv")
        .unwrap()
        .args
        .push("--drifted".to_string());
    let err = echo
        .execute(json!({"text":"y"}), ctx_with(Some(cap.clone())))
        .await
        .expect_err("fingerprint drift must deny");
    assert!(err.to_string().contains("fingerprint"), "{err}");
    assert_eq!(manager.lease_count(), 0);
    assert_eq!(
        fx.count("request:tools/call:echo_tool"),
        1,
        "no call after drift"
    );
    // Graceful termination: child sees stdin EOF and records it, then the
    // bounded cleanup releases the last ownership of the lease.
    assert!(
        wait_until(|| fx.count("eof") == 1).await,
        "child exited on EOF"
    );
    assert!(
        wait_until(|| weak.upgrade().is_none()).await,
        "ownership released"
    );
    fx.cleanup();
}

#[tokio::test]
async fn name_and_schema_mismatch_deny_before_any_call() {
    // Live server advertises a DIFFERENT schema for echo_tool and no
    // missing_tool at all; cache still pins the expected descriptors.
    let hostile = json!([
        {"name": "echo_tool", "description": "changed", "inputSchema": {"type":"object","properties":{"evil":{"type":"string"}}}},
    ]);
    let fx = fixture("mismatch", "ok", hostile);
    let fp = server_config_fingerprint(&fx.config);
    let (source, _) = shared_source(fx.config.clone());
    let manager = Arc::new(McpRuntimeManager::new(source, Duration::from_secs(300)));

    let digest = SchemaDigest::of_schema(&echo_schema());
    let err = manager
        .call_exact(
            &sid(),
            "srv",
            &fp,
            "echo_tool",
            &digest,
            json!({"text":"x"}),
        )
        .await
        .expect_err("schema mismatch must deny");
    assert!(matches!(err, McpLeaseError::SchemaMismatch(_, _)));

    let err = manager
        .call_exact(&sid(), "srv", &fp, "missing_tool", &digest, json!({}))
        .await
        .expect_err("unlisted name must deny");
    assert!(matches!(err, McpLeaseError::NameNotListed(_, _)));

    let calls = fx
        .events()
        .iter()
        .filter(|e| e.starts_with("request:tools/call"))
        .count();
    assert_eq!(calls, 0, "poisoned lease must never reach tools/call");
    assert_eq!(manager.lease_count(), 0, "poisoned lease terminated");
    fx.cleanup();
}

#[tokio::test]
async fn session_end_guard_revocation_and_idle_reap_kill_children() {
    let fx = fixture("teardown", "ok", advertised_tools());
    let fp = server_config_fingerprint(&fx.config);
    let (source, _) = shared_source(fx.config.clone());
    // idle_max ZERO: any pre-existing lease is idle by the next acquisition.
    let manager = Arc::new(McpRuntimeManager::new(source, Duration::ZERO));
    let cap = McpLeaseCapability::new(sid(), Arc::clone(&manager));
    let digest = SchemaDigest::of_schema(&echo_schema());

    // 1. RAII session-end guard kills the child on drop.
    manager
        .call_exact(
            &sid(),
            "srv",
            &fp,
            "echo_tool",
            &digest,
            json!({"text":"a"}),
        )
        .await
        .unwrap();
    let weak = manager.lease_liveness_for_tests(&sid(), "srv").unwrap();
    let guard = McpSessionEndGuard::new(sid(), Arc::clone(&manager));
    drop(guard);
    assert_eq!(manager.lease_count(), 0);
    assert!(
        wait_until(|| fx.count("eof") == 1).await,
        "guard killed child"
    );
    assert!(wait_until(|| weak.upgrade().is_none()).await);

    // 2. Re-acquire (idle reap already proved by ZERO idle_max: the manager
    // reaps on acquisition), then exact revocation kills again.
    manager
        .call_exact(
            &sid(),
            "srv",
            &fp,
            "echo_tool",
            &digest,
            json!({"text":"b"}),
        )
        .await
        .unwrap();
    assert_eq!(fx.count("spawn"), 2, "second lease respawned");
    manager.revoke_exact_tool(&sid(), "srv", "echo_tool");
    assert_eq!(manager.lease_count(), 0);
    assert!(
        wait_until(|| fx.count("eof") == 2).await,
        "revoke killed child"
    );

    // 3. No leaked child on manager drop.
    manager
        .call_exact(
            &sid(),
            "srv",
            &fp,
            "echo_tool",
            &digest,
            json!({"text":"c"}),
        )
        .await
        .unwrap();
    assert_eq!(fx.count("spawn"), 3);
    drop(cap);
    drop(manager);
    assert!(
        wait_until(|| fx.count("eof") == 3).await,
        "manager drop killed child"
    );
    fx.cleanup();
}

#[tokio::test]
async fn hostile_and_oversized_provider_data_is_bounded_and_withheld() {
    // Oversized initialize line: typed bounded failure, content absent.
    let fx = fixture("huge", "huge", advertised_tools());
    let fp = server_config_fingerprint(&fx.config);
    let (source, _) = shared_source(fx.config.clone());
    let manager = Arc::new(McpRuntimeManager::new(source, Duration::from_secs(300)));
    let digest = SchemaDigest::of_schema(&echo_schema());
    let err = manager
        .call_exact(
            &sid(),
            "srv",
            &fp,
            "echo_tool",
            &digest,
            json!({"text":"x"}),
        )
        .await
        .expect_err("oversized line must fail typed");
    let msg = err.to_string();
    assert!(msg.contains("byte bound"), "typed bound failure: {msg}");
    assert!(!msg.contains("XXXX"), "oversized content must be withheld");
    assert!(msg.len() < 300, "error stays bounded: {} bytes", msg.len());
    fx.cleanup();

    // Hostile provider error message: code + length only, content withheld.
    let fx = fixture("hostile-err", "error", advertised_tools());
    let fp = server_config_fingerprint(&fx.config);
    let (source, _) = shared_source(fx.config.clone());
    let manager = Arc::new(McpRuntimeManager::new(source, Duration::from_secs(300)));
    let err = manager
        .call_exact(
            &sid(),
            "srv",
            &fp,
            "echo_tool",
            &digest,
            json!({"text":"x"}),
        )
        .await
        .expect_err("provider error must fail typed");
    let msg = err.to_string();
    assert!(!msg.contains("HOSTILE_PROVIDER_MARKER"), "{msg}");
    assert!(!msg.contains("s3cr3t"), "{msg}");
    assert!(msg.contains("withheld"), "{msg}");
    fx.cleanup();
}

#[tokio::test]
async fn lease_lifecycle_never_mutates_the_catalog() {
    let fx = fixture("no-drift", "ok", advertised_tools());
    let fp = server_config_fingerprint(&fx.config);
    let (source, _) = shared_source(fx.config.clone());
    let manager = Arc::new(McpRuntimeManager::new(source, Duration::from_secs(300)));
    let cap = McpLeaseCapability::new(sid(), Arc::clone(&manager));

    let mut registry = ToolRegistry::new();
    let tools = dormant_tools_for_config(&config_with(fx.config.clone()), &seeded_cache(&fp));
    registry.try_register_batch(tools.clone()).unwrap();
    let generation = registry.catalog().generation();

    let echo = tools
        .iter()
        .find(|t| t.name() == "ext__srv__echo_tool")
        .unwrap();
    echo.execute(json!({"text":"x"}), ctx_with(Some(cap.clone())))
        .await
        .unwrap();
    manager.terminate_session(&sid());

    assert_eq!(
        registry.catalog().generation(),
        generation,
        "lease acquisition/termination must never mutate the catalog"
    );
    fx.cleanup();
}

#[tokio::test]
async fn durable_shared_scope_survives_turns_and_only_last_owner_terminates() {
    let fx = fixture("durable-scope", "ok", advertised_tools());
    let fp = server_config_fingerprint(&fx.config);
    let (source, _) = shared_source(fx.config.clone());
    let manager = Arc::new(McpRuntimeManager::new(source, Duration::from_secs(300)));
    let digest = SchemaDigest::of_schema(&echo_schema());
    let session = sid();

    // Durable shared scope: what the Runtime owns and every clone/stream holds.
    let scope = Arc::new(McpSessionEndGuard::new(
        session.clone(),
        Arc::clone(&manager),
    ));

    manager
        .call_exact(
            &session,
            "srv",
            &fp,
            "echo_tool",
            &digest,
            json!({"text":"turn1"}),
        )
        .await
        .unwrap();

    // Turn 1 ends: its HOLD on the shared scope drops — the lease must
    // survive (no per-turn termination, no respawn next turn).
    let turn1_hold = Arc::clone(&scope);
    drop(turn1_hold);
    assert_eq!(manager.lease_count(), 1, "lease survives a turn ending");
    assert_eq!(fx.count("eof"), 0, "child not killed by turn end");
    manager
        .call_exact(
            &session,
            "srv",
            &fp,
            "echo_tool",
            &digest,
            json!({"text":"turn2"}),
        )
        .await
        .unwrap();
    assert_eq!(fx.count("spawn"), 1, "turn 2 reuses the retained lease");
    assert_eq!(
        fx.count("request:tools/list"),
        1,
        "no reinitialize across turns"
    );

    // A concurrent runtime-clone stream holds the SAME scope: dropping the
    // primary owner cannot kill the sibling's leases.
    let concurrent_clone_hold = Arc::clone(&scope);
    drop(scope);
    assert_eq!(manager.lease_count(), 1, "sibling owner keeps leases alive");
    assert_eq!(fx.count("eof"), 0);

    // LAST owner drops: now — and only now — the session terminates.
    drop(concurrent_clone_hold);
    assert_eq!(
        manager.lease_count(),
        0,
        "last owner terminates the session"
    );
    assert!(
        wait_until(|| fx.count("eof") == 1).await,
        "child exits at true session end"
    );
    fx.cleanup();
}

// ── grant invalidation (Task 19 final acceptance fix) ───────────────────────

fn shared_set_with_activations(
    registry: &ToolRegistry,
) -> agent_engine::tools::activation::SharedSessionToolSet {
    let mut set = SessionToolSet::progressive_core_for_catalog(sid(), registry.catalog());
    activate_exact_for_user(
        &mut set,
        registry.catalog(),
        &ToolId::mcp("srv", "echo_tool"),
    )
    .unwrap();
    activate_exact_for_user(
        &mut set,
        registry.catalog(),
        &ToolId::mcp("srv", "sibling_tool"),
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
    lease: McpLeaseCapability,
    activation: agent_engine::tools::discovery::ActivationCapability,
) -> ToolContext {
    let mut ctx = ctx_with(Some(lease));
    ctx.capabilities.tool_activation = Some(activation);
    ctx
}

#[tokio::test]
async fn fingerprint_drift_revokes_exact_grant_but_not_siblings_or_core() {
    let fx = fixture("grant-drift", "ok", advertised_tools());
    let fp = server_config_fingerprint(&fx.config);
    let (source, map) = shared_source(fx.config.clone());
    let manager = Arc::new(McpRuntimeManager::new(source, Duration::from_secs(300)));
    let cap = McpLeaseCapability::new(sid(), Arc::clone(&manager));

    let mut registry = ToolRegistry::new();
    let tools = dormant_tools_for_config(&config_with(fx.config.clone()), &seeded_cache(&fp));
    registry.try_register_batch(tools.clone()).unwrap();
    let catalog_generation = registry.catalog().generation();
    let shared = shared_set_with_activations(&registry);
    let echo = tools
        .iter()
        .find(|t| t.name() == "ext__srv__echo_tool")
        .unwrap();

    // Healthy call first (lease live, grant intact).
    echo.execute(
        json!({"text":"a"}),
        ctx_full(cap.clone(), activation_cap_for(&registry, &shared)),
    )
    .await
    .unwrap();
    let schema_generation_before = shared.read().unwrap().schema_generation();

    // Drift the config: lease AND the exact grant must fall together.
    map.write()
        .unwrap()
        .get_mut("srv")
        .unwrap()
        .args
        .push("--drift".into());
    let err = echo
        .execute(
            json!({"text":"b"}),
            ctx_full(cap.clone(), activation_cap_for(&registry, &shared)),
        )
        .await
        .expect_err("drift denies");
    assert!(err.to_string().contains("fingerprint"), "{err}");

    assert_eq!(manager.lease_count(), 0, "lease gone");
    let set = shared.read().unwrap();
    assert!(
        set.activation(&ToolId::mcp("srv", "echo_tool")).is_none(),
        "exact grant revoked"
    );
    assert!(
        set.activation(&ToolId::mcp("srv", "sibling_tool"))
            .is_some(),
        "sibling grant untouched"
    );
    assert!(
        set.is_core(&agent_engine::tools::catalog::ToolId::builtin("bash")),
        "core untouched"
    );
    assert_eq!(
        set.schema_generation(),
        schema_generation_before + 1,
        "schema generation advances exactly once on revocation"
    );
    drop(set);
    // Next projection excludes exactly the revoked schema.
    let names: Vec<String> = registry
        .session_tools_schema(&shared.read().unwrap())
        .schema
        .iter()
        .filter_map(|s| s["name"].as_str().map(String::from))
        .collect();
    assert!(!names.contains(&"ext__srv__echo_tool".to_string()));
    assert!(names.contains(&"ext__srv__sibling_tool".to_string()));
    assert_eq!(
        registry.catalog().generation(),
        catalog_generation,
        "no catalog mutation"
    );
    fx.cleanup();
}

#[tokio::test]
async fn schema_mismatch_revokes_grant_but_transport_errors_do_not() {
    // Live schema mismatch: grant falls.
    let hostile = json!([
        {"name": "echo_tool", "description": "changed", "inputSchema": {"type":"object","properties":{"evil":{"type":"string"}}}},
    ]);
    let fx = fixture("grant-mismatch", "ok", hostile);
    let fp = server_config_fingerprint(&fx.config);
    let (source, _) = shared_source(fx.config.clone());
    let manager = Arc::new(McpRuntimeManager::new(source, Duration::from_secs(300)));
    let cap = McpLeaseCapability::new(sid(), Arc::clone(&manager));

    let mut registry = ToolRegistry::new();
    let tools = dormant_tools_for_config(&config_with(fx.config.clone()), &seeded_cache(&fp));
    registry.try_register_batch(tools.clone()).unwrap();
    let shared = shared_set_with_activations(&registry);
    let echo = tools
        .iter()
        .find(|t| t.name() == "ext__srv__echo_tool")
        .unwrap();

    echo.execute(
        json!({"text":"x"}),
        ctx_full(cap.clone(), activation_cap_for(&registry, &shared)),
    )
    .await
    .expect_err("mismatch denies");
    assert!(
        shared
            .read()
            .unwrap()
            .activation(&ToolId::mcp("srv", "echo_tool"))
            .is_none(),
        "schema mismatch revokes the exact grant"
    );
    fx.cleanup();

    // Transient transport failure: grant survives.
    let fx = fixture("grant-transport", "huge", advertised_tools());
    let fp = server_config_fingerprint(&fx.config);
    let (source, _) = shared_source(fx.config.clone());
    let manager = Arc::new(McpRuntimeManager::new(source, Duration::from_secs(300)));
    let cap = McpLeaseCapability::new(sid(), Arc::clone(&manager));
    let mut registry = ToolRegistry::new();
    let tools = dormant_tools_for_config(&config_with(fx.config.clone()), &seeded_cache(&fp));
    registry.try_register_batch(tools.clone()).unwrap();
    let shared = shared_set_with_activations(&registry);
    let echo = tools
        .iter()
        .find(|t| t.name() == "ext__srv__echo_tool")
        .unwrap();

    echo.execute(
        json!({"text":"x"}),
        ctx_full(cap.clone(), activation_cap_for(&registry, &shared)),
    )
    .await
    .expect_err("transport failure denies this call");
    assert!(
        shared
            .read()
            .unwrap()
            .activation(&ToolId::mcp("srv", "echo_tool"))
            .is_some(),
        "transient transport failure must NOT revoke the grant"
    );
    fx.cleanup();
}

#[test]
fn revoke_exact_is_typed_and_reap_scan_covers_full_cap() {
    use agent_engine::mcp::lease::{MAX_LIVE_LEASES, REAP_SCAN_MAX};
    use agent_engine::tools::activation::ExactRevocationError;

    // Reap scan must cover the whole capped map (no prefix starvation).
    assert_eq!(REAP_SCAN_MAX, MAX_LIVE_LEASES);

    let fx = fixture("revoke-typed", "ok", advertised_tools());
    let fp = server_config_fingerprint(&fx.config);
    let mut registry = ToolRegistry::new();
    registry
        .try_register_batch(dormant_tools_for_config(
            &config_with(fx.config.clone()),
            &seeded_cache(&fp),
        ))
        .unwrap();
    let mut set = SessionToolSet::progressive_core_for_catalog(sid(), registry.catalog());
    let echo_id = ToolId::mcp("srv", "echo_tool");
    activate_exact_for_user(&mut set, registry.catalog(), &echo_id).unwrap();
    let generation = set.schema_generation();

    // Core revocation refused typed, zero mutation.
    let bash = agent_engine::tools::catalog::ToolId::builtin("bash");
    assert_eq!(
        set.revoke_exact(&bash),
        Err(ExactRevocationError::CoreTool(bash.clone()))
    );
    // Unknown/never-activated refused typed, zero mutation.
    let ghost = ToolId::mcp("srv", "ghost");
    assert_eq!(
        set.revoke_exact(&ghost),
        Err(ExactRevocationError::NotActivated(ghost.clone()))
    );
    assert_eq!(set.schema_generation(), generation);

    // Exact revocation removes once, advances once, then fails typed.
    assert!(set.revoke_exact(&echo_id).is_ok());
    assert_eq!(set.schema_generation(), generation + 1);
    assert_eq!(
        set.revoke_exact(&echo_id),
        Err(ExactRevocationError::NotActivated(echo_id.clone()))
    );
    assert_eq!(set.schema_generation(), generation + 1);
    fx.cleanup();
}
