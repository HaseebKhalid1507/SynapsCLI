//! Task 14 fix 1 — the capability catalog is integrated into the live
//! `ToolRegistry` (construction, dynamic registration, replacement, disable,
//! extension merge) with truthful runtime-origin provenance and proven
//! passivity against production-shaped extension/MCP metadata.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agent_engine::extensions::hooks::events::{HookEvent, HookResult};
use agent_engine::extensions::runtime::process::RegisteredExtensionToolSpec;
use agent_engine::extensions::runtime::ExtensionHandler;
use agent_engine::mcp::McpConnectTool;
use agent_engine::tools::catalog::{
    CapabilitySource, CatalogError, CatalogGeneration, ToolCatalog, ToolId, TrustProvenance,
};
use agent_engine::tools::{ExtensionTool, Tool, ToolContext, ToolOrigin, ToolRegistry};
use agent_engine::{Result, Value};
use async_trait::async_trait;

// ── Fixtures ────────────────────────────────────────────────────────────────

/// Unknown-origin dynamic tool: no origin metadata, execution panics so any
/// accidental invocation during cataloging is loud.
struct UnknownTool {
    name: &'static str,
}

#[async_trait]
impl Tool for UnknownTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "an unclassified dynamic tool"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
        panic!("cataloging must never execute a tool");
    }
}

/// Production-shaped MCP tool metadata: origin declares exact server/tool
/// identity; execute panics to prove registration never runs it.
struct McpShapedTool {
    runtime_name: &'static str,
    server_id: &'static str,
    server_tool_name: &'static str,
}

#[async_trait]
impl Tool for McpShapedTool {
    fn name(&self) -> &str {
        self.runtime_name
    }
    fn description(&self) -> &str {
        "an MCP-backed tool"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
        panic!("cataloging must never call MCP connection methods");
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Mcp {
            server_id: self.server_id.to_string(),
            server_tool_name: self.server_tool_name.to_string(),
        }
    }
}

/// Spy extension handler: every runtime entry point counts; catalog work must
/// leave all counters at zero.
#[derive(Default)]
struct SpyHandler {
    hook_calls: AtomicUsize,
    tool_calls: AtomicUsize,
    shutdowns: AtomicUsize,
}

#[async_trait]
impl ExtensionHandler for SpyHandler {
    fn id(&self) -> &str {
        "spy"
    }
    async fn handle(&self, _event: &HookEvent) -> HookResult {
        self.hook_calls.fetch_add(1, Ordering::SeqCst);
        HookResult::Continue
    }
    async fn call_tool(&self, _name: &str, _input: Value) -> std::result::Result<Value, String> {
        self.tool_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Value::Null)
    }
    async fn shutdown(&self) {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
    }
}

fn extension_tool(handler: Arc<SpyHandler>, plugin_id: &str, name: &str) -> Arc<dyn Tool> {
    Arc::new(ExtensionTool::new(
        plugin_id,
        RegisteredExtensionToolSpec {
            name: name.to_string(),
            description: "extension-provided tool".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        },
        handler,
    ))
}

// ── Construction paths carry a truthful catalog ─────────────────────────────

#[test]
fn new_registry_catalogs_every_builtin_with_builtin_provenance() {
    let registry = ToolRegistry::new();
    let schema_before = registry.tools_schema();

    let catalog = registry.catalog();
    let tools = registry.iter_tools_sorted();
    assert_eq!(catalog.len(), tools.len());
    assert!(
        catalog.generation().value() > 0,
        "construction is a mutation"
    );
    for tool in &tools {
        let id = ToolId::builtin(tool.name());
        let record = catalog
            .get(&id)
            .unwrap_or_else(|| panic!("missing catalog record for {}", tool.name()));
        assert_eq!(record.source(), &CapabilitySource::Builtin);
        assert_eq!(record.provenance(), &TrustProvenance::BuiltinRuntime);
    }

    // Reading the catalog leaves the exposed schema byte-identical.
    assert_eq!(*schema_before, *registry.tools_schema());
}

#[test]
fn without_subagent_catalog_excludes_recursive_subagent_capabilities() {
    let registry = ToolRegistry::without_subagent();
    let catalog = registry.catalog();
    assert_eq!(catalog.len(), registry.iter_tools_sorted().len());
    for record in catalog.iter() {
        assert!(
            !record.id().name().starts_with("subagent"),
            "subagent capability leaked into subagent catalog: {}",
            record.id()
        );
    }
}

// ── Dynamic mutation keeps one truthful catalog ─────────────────────────────

#[test]
fn dynamic_register_advances_generation_and_records_exact_mcp_identity() {
    let mut registry = ToolRegistry::empty();
    assert_eq!(registry.catalog().len(), 0);
    let g0 = registry.catalog().generation();

    registry
        .try_register(Arc::new(McpShapedTool {
            runtime_name: "ext__server-1__list_issues",
            server_id: "server-1",
            server_tool_name: "list_issues",
        }))
        .expect("registration succeeds");

    let catalog = registry.catalog();
    assert!(
        catalog.generation() > g0,
        "mutation must advance generation"
    );
    let id = ToolId::mcp("server-1", "list_issues");
    let record = catalog.get(&id).expect("MCP capability cataloged");
    assert_eq!(
        record.source(),
        &CapabilitySource::Mcp {
            server_id: "server-1".to_string(),
            server_tool_name: "list_issues".to_string(),
        }
    );
    assert_eq!(
        record.provenance(),
        &TrustProvenance::McpConfig {
            server_id: "server-1".to_string(),
        }
    );
}

#[test]
fn replacement_advances_generation_and_drops_the_stale_identity() {
    let mut registry = ToolRegistry::empty();
    registry
        .try_register(Arc::new(UnknownTool {
            name: "shared_name",
        }))
        .expect("first registration succeeds");
    let g1 = registry.catalog().generation();
    assert!(registry
        .catalog()
        .get(&ToolId::unclassified("shared_name"))
        .is_some());

    // Same runtime name, different origin: the old identity must not linger.
    registry
        .try_register(Arc::new(McpShapedTool {
            runtime_name: "shared_name",
            server_id: "srv",
            server_tool_name: "shared_name",
        }))
        .expect("replacement succeeds");

    let catalog = registry.catalog();
    assert!(catalog.generation() > g1, "replacement is a real mutation");
    assert_eq!(catalog.len(), 1, "stale identity must be removed");
    assert!(catalog.get(&ToolId::unclassified("shared_name")).is_none());
    assert!(catalog.get(&ToolId::mcp("srv", "shared_name")).is_some());
}

#[test]
fn disable_advances_generation_and_noop_disable_does_not() {
    let mut registry = ToolRegistry::new();
    let g0 = registry.catalog().generation();

    registry.disable(&["bash".to_string()]);
    let g1 = registry.catalog().generation();
    assert!(
        g1 > g0,
        "disable must strictly advance past the prior generation"
    );
    assert!(registry.catalog().get(&ToolId::builtin("bash")).is_none());

    registry.disable(&["no_such_tool".to_string()]);
    assert_eq!(
        registry.catalog().generation(),
        g1,
        "no-op disable must not advance the generation"
    );
}

// ── Unknown/dynamic tools are never invented as trusted builtins ────────────

#[test]
fn unknown_dynamic_tools_catalog_with_conservative_provenance() {
    let mut registry = ToolRegistry::empty();
    registry
        .try_register(Arc::new(UnknownTool { name: "mystery" }))
        .expect("registration succeeds");
    let record = registry
        .catalog()
        .get(&ToolId::unclassified("mystery"))
        .expect("unknown tool cataloged under the unknown namespace");
    assert_eq!(
        record.source(),
        &CapabilitySource::Unknown {
            runtime_name: "mystery".to_string(),
        }
    );
    assert_eq!(record.provenance(), &TrustProvenance::Unverified);
    assert!(
        registry
            .catalog()
            .get(&ToolId::builtin("mystery"))
            .is_none(),
        "dynamic tools must not be classified as builtin"
    );
}

#[test]
fn non_canonical_names_are_encoded_distinctly_not_dropped_or_collapsed() {
    let mut registry = ToolRegistry::empty();
    registry
        .try_register(Arc::new(UnknownTool { name: "Bad Name!" }))
        .expect("non-canonical names are representable");
    registry
        .try_register(Arc::new(UnknownTool { name: "Bad_Name_" }))
        .expect("sanitization twin is representable");
    assert_eq!(
        registry.catalog().len(),
        2,
        "alias-prone spellings must remain distinct identities"
    );
}

// ── Extension merge: truthful provenance, proven inert ──────────────────────

#[test]
fn extension_merge_catalogs_extension_provenance_without_invoking_the_handler() {
    let handler = Arc::new(SpyHandler::default());
    let mut shared = ToolRegistry::empty();
    shared
        .try_register(extension_tool(handler.clone(), "My.Plugin", "do_thing"))
        .expect("extension tool registration succeeds");

    let combined = ToolRegistry::without_subagent_with_extensions(&shared);
    let id = ToolId::extension("My.Plugin", "do_thing");
    let record = combined
        .catalog()
        .get(&id)
        .expect("merged extension tool cataloged");
    assert_eq!(
        record.source(),
        &CapabilitySource::Extension {
            extension_id: "My.Plugin".to_string(),
            tool_name: "do_thing".to_string(),
        }
    );
    assert_eq!(
        record.provenance(),
        &TrustProvenance::ExtensionManifest {
            extension_id: "My.Plugin".to_string(),
        }
    );

    // Cataloging in both registries must never touch the extension runtime.
    assert_eq!(handler.hook_calls.load(Ordering::SeqCst), 0);
    assert_eq!(handler.tool_calls.load(Ordering::SeqCst), 0);
    assert_eq!(handler.shutdowns.load(Ordering::SeqCst), 0);
}

// ── MCP gateway is explicitly builtin; no connection at catalog time ────────

#[test]
fn mcp_gateway_tool_is_explicitly_builtin_and_inert_to_catalog() {
    let mut registry = ToolRegistry::empty();
    registry
        .try_register(Arc::new(McpConnectTool::new(HashMap::new())))
        .expect("gateway registration succeeds");
    let record = registry
        .catalog()
        .get(&ToolId::builtin("connect_mcp_server"))
        .expect("gateway cataloged as builtin");
    assert_eq!(record.source(), &CapabilitySource::Builtin);
    assert_eq!(record.provenance(), &TrustProvenance::BuiltinRuntime);
}

// ── Duplicate capability identity across runtime names fails typed ──────────

/// Owned-string MCP-shaped tool for adversarial identity fixtures.
struct McpOwnedTool {
    runtime_name: String,
    server_id: String,
    server_tool_name: String,
}

#[async_trait]
impl Tool for McpOwnedTool {
    fn name(&self) -> &str {
        &self.runtime_name
    }
    fn description(&self) -> &str {
        "an MCP-backed tool"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
        panic!("cataloging must never call MCP connection methods");
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Mcp {
            server_id: self.server_id.clone(),
            server_tool_name: self.server_tool_name.clone(),
        }
    }
}

fn mcp_owned(runtime_name: &str, server_id: &str, server_tool_name: &str) -> Arc<dyn Tool> {
    Arc::new(McpOwnedTool {
        runtime_name: runtime_name.to_string(),
        server_id: server_id.to_string(),
        server_tool_name: server_tool_name.to_string(),
    })
}

#[test]
fn duplicate_identity_across_distinct_runtime_names_fails_typed_without_mutation() {
    let mut registry = ToolRegistry::empty();
    registry
        .try_register(mcp_owned("alias_a", "srv", "shared_tool"))
        .expect("first identity registration succeeds");
    let schema_before = registry.tools_schema();
    let generation_before = registry.catalog().generation();
    let len_before = registry.catalog().len();

    // A second, distinct runtime name declaring the SAME capability identity
    // must fail typed — not silently overwrite the catalog entry while both
    // live tools keep executing.
    let err = registry
        .try_register(mcp_owned("alias_b", "srv", "shared_tool"))
        .expect_err("duplicate identity under a new runtime name must fail closed");
    assert!(matches!(err, CatalogError::DuplicateToolId(_)));

    // Nothing mutated: tools map, exposed schema, catalog entries, generation.
    assert!(registry.get("alias_a").is_some());
    assert!(registry.get("alias_b").is_none());
    assert_eq!(*schema_before, *registry.tools_schema());
    assert_eq!(registry.catalog().len(), len_before);
    assert_eq!(registry.catalog().generation(), generation_before);
}

#[test]
fn replacement_stealing_identity_owned_by_another_runtime_name_fails_typed() {
    let mut registry = ToolRegistry::empty();
    registry
        .try_register(mcp_owned("owner", "srv", "stolen"))
        .expect("identity owner registers");
    registry
        .try_register(Arc::new(UnknownTool { name: "thief" }))
        .expect("unknown tool registers");
    let schema_before = registry.tools_schema();
    let generation_before = registry.catalog().generation();

    // Same-runtime-name replacement of `thief` that switches to an identity
    // already owned by `owner` must fail typed, not drop/overwrite entries.
    let err = registry
        .try_register(mcp_owned("thief", "srv", "stolen"))
        .expect_err("identity theft via replacement must fail closed");
    assert!(matches!(err, CatalogError::DuplicateToolId(_)));

    assert_eq!(*schema_before, *registry.tools_schema());
    assert_eq!(registry.catalog().generation(), generation_before);
    assert_eq!(registry.catalog().len(), 2);
    assert!(registry
        .catalog()
        .get(&ToolId::unclassified("thief"))
        .is_some());
    assert!(registry
        .catalog()
        .get(&ToolId::mcp("srv", "stolen"))
        .is_some());
}

#[test]
fn same_runtime_name_replacement_with_unchanged_identity_remains_valid() {
    let mut registry = ToolRegistry::empty();
    registry
        .try_register(mcp_owned("stable", "srv", "same"))
        .expect("first registration succeeds");
    let g1 = registry.catalog().generation();

    registry
        .try_register(mcp_owned("stable", "srv", "same"))
        .expect("same-name same-identity replacement stays valid");
    assert!(registry.catalog().generation() > g1);
    assert_eq!(registry.catalog().len(), 1);
    assert!(registry
        .catalog()
        .get(&ToolId::mcp("srv", "same"))
        .is_some());
}

// ── Failed disable leaves the registry fully unchanged ──────────────────────

#[test]
fn failed_disable_at_generation_boundary_leaves_registry_unchanged() {
    let mut registry = ToolRegistry::new();
    registry.resume_catalog_generation_for_tests(CatalogGeneration::new(u64::MAX));
    let schema_before = registry.tools_schema();
    let names_before: Vec<String> = registry
        .iter_tools_sorted()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    let catalog_ids_before: Vec<ToolId> =
        registry.catalog().iter().map(|r| r.id().clone()).collect();

    let err = registry
        .try_disable(&["bash".to_string()])
        .expect_err("disable without a new generation must fail closed");
    assert!(matches!(err, CatalogError::GenerationExhausted(_)));

    // Byte-for-byte / structurally unchanged: tools, schema, catalog, generation.
    assert!(registry.get("bash").is_some(), "tool must not be removed");
    assert_eq!(*schema_before, *registry.tools_schema());
    let names_after: Vec<String> = registry
        .iter_tools_sorted()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    assert_eq!(names_before, names_after);
    let catalog_ids_after: Vec<ToolId> =
        registry.catalog().iter().map(|r| r.id().clone()).collect();
    assert_eq!(catalog_ids_before, catalog_ids_after);
    assert_eq!(
        registry.catalog().generation(),
        CatalogGeneration::new(u64::MAX),
        "failed disable must not rewind or advance the generation"
    );
}

// ── Oversized source identities stay visibly distinct ───────────────────────

#[test]
fn oversized_source_identities_remain_distinct_in_catalog_metadata() {
    let long_a = "a".repeat(300);
    let long_b = format!("{}b", "a".repeat(299));
    let mut registry = ToolRegistry::empty();
    registry
        .try_register(mcp_owned("tool_a", &long_a, "t"))
        .expect("first long identity registers");
    registry
        .try_register(mcp_owned("tool_b", &long_b, "t"))
        .expect("second long identity registers");

    let record_a = registry
        .catalog()
        .get(&ToolId::mcp(&long_a, "t"))
        .expect("first record cataloged");
    let record_b = registry
        .catalog()
        .get(&ToolId::mcp(&long_b, "t"))
        .expect("second record cataloged");

    // Two distinct trust-relevant identities must never look identical in
    // displayed/source metadata after byte-bounding.
    assert_ne!(
        record_a.source(),
        record_b.source(),
        "distinct oversized source identities collapsed after truncation"
    );
    assert_ne!(record_a.provenance(), record_b.provenance());
}

// ── Generation overflow boundary fails closed ───────────────────────────────

#[test]
fn catalog_mutation_at_generation_boundary_fails_closed() {
    let mut registry = ToolRegistry::empty();
    registry
        .try_register(Arc::new(UnknownTool { name: "seed" }))
        .expect("seed registration succeeds");
    let record = registry
        .catalog()
        .get(&ToolId::unclassified("seed"))
        .expect("seed cataloged")
        .clone();

    let mut catalog = ToolCatalog::resume_at_generation_for_tests(CatalogGeneration::new(u64::MAX));
    let err = catalog
        .insert(record)
        .expect_err("mutation without a new generation must fail");
    assert!(matches!(err, CatalogError::GenerationExhausted(_)));
    assert_eq!(catalog.len(), 0, "failed mutation must not partially apply");
    assert_eq!(catalog.generation(), CatalogGeneration::new(u64::MAX));
}
