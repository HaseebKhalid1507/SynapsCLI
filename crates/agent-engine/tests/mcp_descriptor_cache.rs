//! Task 19 Commit A — safe local MCP descriptor cache and dormant
//! deferred-tool catalog integration (spec §7.4, plan Task 19).
//!
//! Everything in this file must hold WITHOUT any process spawn or network
//! activity: descriptor knowledge comes only from bounded local state.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_engine::mcp::descriptors::{
    dormant_tools_for_config, load_cache_from, server_config_fingerprint, store_cache_at,
    CachedServerDescriptors, CachedToolDescriptor, DescriptorCacheError, McpDescriptorCache,
    CACHE_FORMAT_VERSION, CACHE_MAX_BYTES, TOOL_DESCRIPTION_MAX_BYTES,
};
use agent_engine::mcp::{McpConfig, McpServerConfig};
use agent_engine::tools::activation::{activate_exact_for_user, SessionId, SessionToolSet};
use agent_engine::tools::catalog::{
    DiscoveryIndex, DiscoveryQuery, SchemaDigest, SearchLimits, ToolId,
};
use agent_engine::tools::{ToolCapabilities, ToolChannels, ToolContext, ToolLimits, ToolRegistry};
use serde_json::{json, Value};

// ── fixtures ────────────────────────────────────────────────────────────────

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("synaps-mcp-cache-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn spy_config(marker: &Path) -> McpServerConfig {
    // If ANY code path under test spawned this server, the marker file
    // would exist afterwards. No test in this file may create it.
    McpServerConfig {
        command: "/bin/sh".to_string(),
        args: vec![
            "-c".to_string(),
            format!("echo spawned >> {}", marker.display()),
        ],
        env: HashMap::new(),
    }
}

fn sample_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"text": {"type": "string"}},
        "required": ["text"]
    })
}

fn sibling_schema() -> Value {
    json!({"type": "object", "properties": {}})
}

fn seeded_cache(fingerprint: &str) -> McpDescriptorCache {
    let mut cache = McpDescriptorCache::empty();
    cache.servers.insert(
        "srv".to_string(),
        CachedServerDescriptors {
            fingerprint: fingerprint.to_string(),
            tools: vec![
                CachedToolDescriptor {
                    name: "echo_tool".to_string(),
                    description: "echoes text back".to_string(),
                    input_schema: sample_schema(),
                },
                CachedToolDescriptor {
                    name: "sibling_tool".to_string(),
                    description: "sibling that must stay dormant".to_string(),
                    input_schema: sibling_schema(),
                },
            ],
        },
    );
    cache
}

fn config_with(server: &str, server_config: McpServerConfig) -> McpConfig {
    let mut servers = HashMap::new();
    servers.insert(server.to_string(), server_config);
    McpConfig {
        mcp_servers: servers,
    }
}

fn manual_ctx() -> ToolContext {
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
            secret_prompt: None,
            orchestration: None,
            tool_activation: None,
            mcp_leases: None,
            extension_leases: None,
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

fn sid() -> SessionId {
    SessionId::parse("task19-descriptors").unwrap()
}

// ── fingerprint ─────────────────────────────────────────────────────────────

#[test]
fn fingerprint_is_deterministic_and_config_sensitive() {
    let dir = tmp_dir("fp");
    let marker = dir.join("spawn.marker");
    let base = spy_config(&marker);

    let fp1 = server_config_fingerprint(&base);
    let fp2 = server_config_fingerprint(&base.clone());
    assert_eq!(fp1, fp2, "same config must fingerprint identically");

    let mut changed_args = base.clone();
    changed_args.args.push("--extra".to_string());
    assert_ne!(fp1, server_config_fingerprint(&changed_args));

    let mut changed_env = base.clone();
    changed_env.env.insert("MCP_X".to_string(), "1".to_string());
    assert_ne!(fp1, server_config_fingerprint(&changed_env));

    let mut changed_cmd = base.clone();
    changed_cmd.command = "/bin/echo".to_string();
    assert_ne!(fp1, server_config_fingerprint(&changed_cmd));

    assert!(!marker.exists(), "fingerprinting must never spawn");
    let _ = std::fs::remove_dir_all(&dir);
}

// ── cache store/load ────────────────────────────────────────────────────────

#[test]
fn store_then_load_roundtrip_is_private_and_atomic() {
    let dir = tmp_dir("roundtrip");
    let path = dir.join("mcp-descriptors.json");
    let cache = seeded_cache("fp-test");

    store_cache_at(&path, &cache).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "descriptor cache must be private");
    }
    assert!(
        !path.with_file_name("mcp-descriptors.json.tmp").exists(),
        "atomic write must not leave the temp file behind"
    );

    let loaded = load_cache_from(&path).unwrap();
    assert_eq!(loaded.version, CACHE_FORMAT_VERSION);
    let srv = loaded.servers.get("srv").unwrap();
    assert_eq!(srv.fingerprint, "fp-test");
    assert_eq!(srv.tools.len(), 2);
    assert_eq!(srv.tools[0].name, "echo_tool");
    assert_eq!(srv.tools[0].input_schema, sample_schema());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_rejects_missing_nonregular_symlink_oversize_malformed_and_versions() {
    let dir = tmp_dir("reject");

    // Missing file.
    assert!(matches!(
        load_cache_from(&dir.join("absent.json")),
        Err(DescriptorCacheError::NotFound)
    ));

    // Directory (non-regular).
    assert!(matches!(
        load_cache_from(&dir),
        Err(DescriptorCacheError::NotRegularFile)
    ));

    // Symlink, even to a valid cache.
    #[cfg(unix)]
    {
        let real = dir.join("real.json");
        store_cache_at(&real, &seeded_cache("fp")).unwrap();
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(
            load_cache_from(&link),
            Err(DescriptorCacheError::NotRegularFile)
        ));
    }

    // Oversize.
    let big = dir.join("big.json");
    std::fs::write(&big, vec![b' '; (CACHE_MAX_BYTES + 1) as usize]).unwrap();
    assert!(matches!(
        load_cache_from(&big),
        Err(DescriptorCacheError::Oversize { .. })
    ));

    // Malformed JSON.
    let bad = dir.join("bad.json");
    std::fs::write(&bad, "{not json").unwrap();
    assert!(matches!(
        load_cache_from(&bad),
        Err(DescriptorCacheError::Parse(_))
    ));

    // Unsupported version.
    let versioned = dir.join("versioned.json");
    std::fs::write(
        &versioned,
        serde_json::to_vec(&json!({"version": 99, "servers": {}})).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        load_cache_from(&versioned),
        Err(DescriptorCacheError::Version(99))
    ));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hostile_descriptor_entries_are_dropped_and_bounded() {
    let dir = tmp_dir("hostile");
    let path = dir.join("mcp-descriptors.json");
    let long_name = "n".repeat(200);
    let huge_description = "d".repeat(5000);
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "version": CACHE_FORMAT_VERSION,
            "servers": {
                "srv": {
                    "fingerprint": "fp",
                    "tools": [
                        {"name": "ok_tool", "description": huge_description, "input_schema": {"type": "object"}},
                        {"name": "evil\u{0007}name", "description": "control chars", "input_schema": {"type": "object"}},
                        {"name": long_name, "description": "too long", "input_schema": {"type": "object"}},
                        {"name": "", "description": "empty name", "input_schema": {"type": "object"}},
                        {"name": "not_object_schema", "description": "x", "input_schema": "just a string"},
                        {"name": "ok_tool", "description": "duplicate — keep first", "input_schema": {"type": "object"}}
                    ]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let loaded = load_cache_from(&path).unwrap();
    let srv = loaded.servers.get("srv").unwrap();
    assert_eq!(
        srv.tools
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        vec!["ok_tool"],
        "invalid, hostile, and duplicate descriptors must be dropped"
    );
    assert!(
        srv.tools[0].description.len() <= TOOL_DESCRIPTION_MAX_BYTES,
        "provider-shaped description must be bounded on load"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── dormant tool construction ───────────────────────────────────────────────

#[test]
fn dormant_tools_come_only_from_fingerprint_matching_cache_entries() {
    let dir = tmp_dir("dormant-src");
    let marker = dir.join("spawn.marker");
    let cfg = spy_config(&marker);
    let fp = server_config_fingerprint(&cfg);

    // Config-only server (no cache entry): no invented descriptors.
    let no_cache = dormant_tools_for_config(
        &config_with("srv", cfg.clone()),
        &McpDescriptorCache::empty(),
    );
    assert!(
        no_cache.is_empty(),
        "descriptors must never come from config alone"
    );

    // Cache entry with a stale fingerprint: ignored.
    let stale =
        dormant_tools_for_config(&config_with("srv", cfg.clone()), &seeded_cache("other-fp"));
    assert!(
        stale.is_empty(),
        "fingerprint mismatch must invalidate cached descriptors"
    );

    // Cache entry for a server that is no longer configured: ignored.
    let ghost =
        dormant_tools_for_config(&config_with("different", cfg.clone()), &seeded_cache(&fp));
    assert!(
        ghost.is_empty(),
        "cache entries without a configured server must be ignored"
    );

    // Matching fingerprint: exactly the cached descriptors, prefixed names.
    let tools = dormant_tools_for_config(&config_with("srv", cfg), &seeded_cache(&fp));
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(names, vec!["ext__srv__echo_tool", "ext__srv__sibling_tool"]);

    assert!(!marker.exists(), "building dormant tools must never spawn");
    let _ = std::fs::remove_dir_all(&dir);
}

// ── registry/catalog integration ────────────────────────────────────────────

#[test]
fn dormant_registration_is_atomic_searchable_and_activation_gated_without_spawn() {
    let dir = tmp_dir("dormant-reg");
    let marker = dir.join("spawn.marker");
    let cfg = spy_config(&marker);
    let fp = server_config_fingerprint(&cfg);
    let config = config_with("srv", cfg);
    let dormant = dormant_tools_for_config(&config, &seeded_cache(&fp));

    let mut registry = ToolRegistry::new();
    registry.try_register_batch(dormant).unwrap();

    // Truthful catalog identity: stable MCP ToolId with the digest of the
    // cached schema, so a later live listing can be validated against it.
    let echo_id = ToolId::mcp("srv", "echo_tool");
    let record = registry
        .catalog()
        .get(&echo_id)
        .expect("dormant record cataloged");
    assert_eq!(
        record.schema_digest(),
        &SchemaDigest::of_schema(&sample_schema())
    );

    // Searchable via the passive discovery index — zero process activity.
    let index = DiscoveryIndex::build(registry.catalog()).unwrap();
    let query = DiscoveryQuery::parse("echoes text").unwrap();
    let limits = SearchLimits::new(16, 8 * 1024).unwrap();
    let results = index.search(&query, &limits);
    assert!(
        results.hits().iter().any(|hit| hit.id() == &echo_id),
        "dormant MCP descriptor must be discoverable by search"
    );

    // Progressive core excludes dormant MCP entries until exact activation.
    let mut set = SessionToolSet::progressive_core_for_catalog(sid(), registry.catalog());
    let before: Vec<String> = registry
        .session_tools_schema(&set)
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(String::from))
        .collect();
    assert!(!before.iter().any(|n| n.starts_with("ext__srv__")));

    // Exact activation exposes exactly the requested tool — no siblings —
    // and mutates NO catalog state (no generation drift on activation).
    let generation_before = registry.catalog().generation();
    activate_exact_for_user(&mut set, registry.catalog(), &echo_id).unwrap();
    assert_eq!(registry.catalog().generation(), generation_before);
    let after: Vec<String> = registry
        .session_tools_schema(&set)
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(String::from))
        .collect();
    assert!(after.contains(&"ext__srv__echo_tool".to_string()));
    assert!(!after.contains(&"ext__srv__sibling_tool".to_string()));

    assert!(
        !marker.exists(),
        "registration/search/activation must never spawn"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn try_register_batch_is_atomic_on_duplicate_identity() {
    use agent_engine::tools::{Tool, ToolOrigin};

    // A tool whose runtime name differs but whose capability identity
    // collides with the already-registered dormant echo tool.
    struct CollidingTool;
    #[async_trait::async_trait]
    impl Tool for CollidingTool {
        fn name(&self) -> &str {
            "different_runtime_name"
        }
        fn description(&self) -> &str {
            "collides on mcp:srv:echo_tool identity"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        fn origin(&self) -> ToolOrigin {
            ToolOrigin::Mcp {
                server_id: "srv".to_string(),
                server_tool_name: "echo_tool".to_string(),
            }
        }
        async fn execute(&self, _p: Value, _c: ToolContext) -> agent_engine::Result<String> {
            Ok(String::new())
        }
    }
    struct FreshTool;
    #[async_trait::async_trait]
    impl Tool for FreshTool {
        fn name(&self) -> &str {
            "brand_new_tool"
        }
        fn description(&self) -> &str {
            "valid batch member that must not survive a failed batch"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        fn origin(&self) -> ToolOrigin {
            ToolOrigin::Builtin
        }
        async fn execute(&self, _p: Value, _c: ToolContext) -> agent_engine::Result<String> {
            Ok(String::new())
        }
    }

    let dir = tmp_dir("atomic");
    let marker = dir.join("spawn.marker");
    let cfg = spy_config(&marker);
    let fp = server_config_fingerprint(&cfg);
    let config = config_with("srv", cfg);

    let mut registry = ToolRegistry::new();
    registry
        .try_register_batch(dormant_tools_for_config(&config, &seeded_cache(&fp)))
        .unwrap();

    let generation = registry.catalog().generation();
    let len = registry.catalog().len();
    let schema_before = serde_json::to_vec(registry.tools_schema().as_ref()).unwrap();

    // Batch = one valid new tool + one identity collision: the WHOLE batch
    // must be rejected with the registry left byte-identical (no partial
    // registration — the valid member must not survive).
    let retry: Vec<Arc<dyn Tool>> = vec![Arc::new(FreshTool), Arc::new(CollidingTool)];
    assert!(registry.try_register_batch(retry).is_err());
    assert!(registry.get("brand_new_tool").is_none(), "no partial batch");
    assert_eq!(registry.catalog().generation(), generation);
    assert_eq!(registry.catalog().len(), len);
    assert_eq!(
        serde_json::to_vec(registry.tools_schema().as_ref()).unwrap(),
        schema_before,
        "failed batch must not change the exposed schema"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── deferred execution stays inert without a lease manager ──────────────────

#[tokio::test]
async fn deferred_execute_without_lease_manager_fails_typed_and_spawns_nothing() {
    let dir = tmp_dir("deferred-exec");
    let marker = dir.join("spawn.marker");
    let cfg = spy_config(&marker);
    let fp = server_config_fingerprint(&cfg);
    let tools = dormant_tools_for_config(&config_with("srv", cfg), &seeded_cache(&fp));
    let echo: &Arc<dyn agent_engine::tools::Tool> = tools
        .iter()
        .find(|t| t.name() == "ext__srv__echo_tool")
        .unwrap();

    let err = echo
        .execute(json!({"text": "hi"}), manual_ctx())
        .await
        .expect_err("deferred MCP tool must not run without a lease manager");
    let msg = err.to_string();
    assert!(
        msg.contains("lease") || msg.contains("deferred"),
        "typed error should explain the missing lease capability: {msg}"
    );
    assert!(
        !marker.exists(),
        "deferred execute without manager must not spawn"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── strictly-additive batch preflight (Commit A review fix) ─────────────────

/// Minimal batch member with a chosen runtime name and MCP identity.
struct NamedMcpTool {
    runtime_name: String,
    server: String,
    tool: String,
}

#[async_trait::async_trait]
impl agent_engine::tools::Tool for NamedMcpTool {
    fn name(&self) -> &str {
        &self.runtime_name
    }
    fn description(&self) -> &str {
        "batch preflight fixture"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
    fn origin(&self) -> agent_engine::tools::ToolOrigin {
        agent_engine::tools::ToolOrigin::Mcp {
            server_id: self.server.clone(),
            server_tool_name: self.tool.clone(),
        }
    }
    async fn execute(&self, _p: Value, _c: ToolContext) -> agent_engine::Result<String> {
        Ok(String::new())
    }
}

fn named(runtime: &str, server: &str, tool: &str) -> Arc<dyn agent_engine::tools::Tool> {
    Arc::new(NamedMcpTool {
        runtime_name: runtime.to_string(),
        server: server.to_string(),
        tool: tool.to_string(),
    })
}

/// Snapshot (generation, catalog len, exposed schema bytes) for unchanged
/// assertions after every rejected batch.
fn registry_snapshot(registry: &ToolRegistry) -> (u64, usize, Vec<u8>) {
    (
        registry.catalog().generation().value(),
        registry.catalog().len(),
        serde_json::to_vec(registry.tools_schema().as_ref()).unwrap(),
    )
}

#[test]
fn batch_rejects_same_runtime_name_and_identity_duplicate() {
    let mut registry = ToolRegistry::new();
    registry
        .try_register_batch(vec![named("ext__a__t", "a", "t")])
        .unwrap();
    let before = registry_snapshot(&registry);

    // Re-registering the SAME runtime name + identity must be rejected —
    // batches are strictly additive, never a replacement path.
    let err = registry
        .try_register_batch(vec![named("ext__a__t", "a", "t")])
        .expect_err("duplicate must be rejected");
    assert!(matches!(
        err,
        agent_engine::tools::catalog::CatalogError::DuplicateRuntimeName(_)
    ));
    assert_eq!(registry_snapshot(&registry), before);
}

#[test]
fn batch_rejects_distinct_identities_colliding_on_runtime_name() {
    let mut registry = ToolRegistry::new();
    let before = registry_snapshot(&registry);

    // server "a", tool "b__c" and server "a__b", tool "c" are DIFFERENT
    // capability identities whose ext__{server}__{tool} runtime strings
    // collide via the separator scheme. Silently dropping one identity
    // (count 2, one catalog record) must be impossible.
    let batch = vec![
        named("ext__a__b__c", "a", "b__c"),
        named("ext__a__b__c", "a__b", "c"),
    ];
    let err = registry
        .try_register_batch(batch)
        .expect_err("separator collision must be rejected");
    assert!(matches!(
        err,
        agent_engine::tools::catalog::CatalogError::DuplicateRuntimeName(_)
    ));
    assert_eq!(registry_snapshot(&registry), before);
}

#[test]
fn batch_rejects_collision_with_existing_registry_tool() {
    let mut registry = ToolRegistry::new();
    let before = registry_snapshot(&registry);

    // "bash" is a live builtin: a batch must never replace it, whatever
    // origin the incoming tool claims.
    let err = registry
        .try_register_batch(vec![named("bash", "srv", "bash")])
        .expect_err("existing runtime tool must not be replaceable via batch");
    assert!(matches!(
        err,
        agent_engine::tools::catalog::CatalogError::DuplicateRuntimeName(_)
    ));
    assert_eq!(registry_snapshot(&registry), before);
    // The live builtin is untouched.
    assert!(registry.get("bash").is_some());
}
