//! Task 14 — `ToolCatalog` behavior (spec §7.1).
//!
//! The catalog is an additive inventory of locally known capabilities.
//! Insertion must be inert: no implementation initialization, no process
//! start, no network, no schema exposure, no execution grant. The registry
//! remains the active behavior projection.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agent_engine::tools::catalog::{
    CapabilityRecord, CapabilitySource, CatalogError, SchemaDigest, SchemaLocator, ToolCatalog,
    ToolId, TrustProvenance,
};
use agent_engine::tools::{Tool, ToolContext, ToolRegistry};
use agent_engine::{Result, Value};
use async_trait::async_trait;

// ── Inert fixture tool + counting factory ───────────────────────────────────

struct FixtureTool {
    name: &'static str,
}

#[async_trait]
impl Tool for FixtureTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "fixture tool"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
        Ok("fixture".to_string())
    }
}

fn spy_record(id: &str, constructions: Arc<AtomicUsize>) -> CapabilityRecord {
    let factory = move || -> Arc<dyn Tool> {
        constructions.fetch_add(1, Ordering::SeqCst);
        Arc::new(FixtureTool { name: "spy" })
    };
    CapabilityRecord::new(
        ToolId::parse(id).expect("fixture id is canonical"),
        CapabilitySource::Mcp {
            server_id: "server-1".to_string(),
            server_tool_name: id.rsplit(':').next().unwrap_or(id).to_string(),
        },
        "a spy capability",
        vec!["spy".to_string()],
        SchemaLocator::Inline(serde_json::json!({"type": "object"})),
        Arc::new(factory),
        TrustProvenance::McpConfig {
            server_id: "server-1".to_string(),
        },
    )
}

// ── Insertion is inert ──────────────────────────────────────────────────────

#[test]
fn insertion_never_invokes_the_implementation_factory() {
    let constructions = Arc::new(AtomicUsize::new(0));
    let mut catalog = ToolCatalog::empty();
    catalog
        .insert(spy_record(
            "mcp.server-1:list_issues",
            constructions.clone(),
        ))
        .expect("insert succeeds");
    catalog
        .insert(spy_record(
            "mcp.server-1:create_issue",
            constructions.clone(),
        ))
        .expect("insert succeeds");

    // Knowledge ≠ activation ≠ execution: cataloging both capabilities must
    // not construct either implementation (spec §4.2).
    assert_eq!(constructions.load(Ordering::SeqCst), 0);

    // Only an explicit later acquisition constructs the implementation.
    let id = ToolId::parse("mcp.server-1:list_issues").unwrap();
    let record = catalog.get(&id).expect("record present");
    let tool = record.implementation();
    assert_eq!(tool.name(), "spy");
    assert_eq!(constructions.load(Ordering::SeqCst), 1);
}

// ── Population from the registry construction path ──────────────────────────

#[test]
fn from_registry_catalogs_every_builtin_without_changing_registry_exposure() {
    let registry = ToolRegistry::new();
    let schema_before = registry.tools_schema();

    let catalog = ToolCatalog::from_registry(&registry).expect("builtins are canonical");

    // Every registry tool is known to the catalog under `builtin:<name>`.
    let registry_tools = registry.iter_tools_sorted();
    assert_eq!(catalog.len(), registry_tools.len());
    for tool in &registry_tools {
        let id = ToolId::parse(&format!("builtin:{}", tool.name()))
            .unwrap_or_else(|e| panic!("builtin name {} must be canonical: {e}", tool.name()));
        let record = catalog
            .get(&id)
            .unwrap_or_else(|| panic!("missing catalog record for {}", tool.name()));
        assert_eq!(record.source(), &CapabilitySource::Builtin);
        assert_eq!(record.provenance(), &TrustProvenance::BuiltinRuntime);
        // Task 24: the catalog records each builtin's declared effect class
        // verbatim (the trait default keeps unclassified tools NonIdempotent).
        assert_eq!(record.effect(), tool.effect());
        assert!(!record.summary().is_empty(), "compact summary retained");
    }

    // The registry projection (what the model sees) is byte-identical.
    assert_eq!(*schema_before, *registry.tools_schema());
}

#[test]
fn from_registry_digests_are_deterministic_across_rebuilds() {
    let a = ToolCatalog::from_registry(&ToolRegistry::new()).unwrap();
    let b = ToolCatalog::from_registry(&ToolRegistry::new()).unwrap();
    assert_eq!(a.len(), b.len());
    assert_eq!(a.generation(), b.generation());
    for (ra, rb) in a.iter().zip(b.iter()) {
        assert_eq!(ra.id(), rb.id());
        assert_eq!(ra.schema_digest(), rb.schema_digest());
    }
}

#[test]
fn from_registry_encodes_non_canonical_tool_names_without_alias_collapse() {
    // Fix 1 (Task 14): actual runtime names may be non-canonical
    // (uppercase, spaces, punctuation). They are represented exactly via
    // deterministic alias-safe encoding under conservative unknown
    // provenance — not rejected and not collapsed into one identity.
    let mut registry = ToolRegistry::empty();
    registry.register(Arc::new(FixtureTool { name: "Bad Name!" }));
    registry.register(Arc::new(FixtureTool { name: "Bad_Name_" }));
    let catalog = ToolCatalog::from_registry(&registry).expect("names are representable");
    assert_eq!(catalog.len(), 2, "alias-prone spellings stay distinct");
    for record in catalog.iter() {
        assert_eq!(record.provenance(), &TrustProvenance::Unverified);
    }
}

// ── Generation and collision behavior ───────────────────────────────────────

#[test]
fn generation_increments_on_each_mutation() {
    let constructions = Arc::new(AtomicUsize::new(0));
    let mut catalog = ToolCatalog::empty();
    assert_eq!(catalog.generation().value(), 0);
    catalog
        .insert(spy_record(
            "mcp.server-1:list_issues",
            constructions.clone(),
        ))
        .unwrap();
    assert_eq!(catalog.generation().value(), 1);
    catalog
        .insert(spy_record(
            "mcp.server-1:create_issue",
            constructions.clone(),
        ))
        .unwrap();
    assert_eq!(catalog.generation().value(), 2);
}

#[test]
fn duplicate_ids_are_rejected_and_do_not_advance_generation() {
    let constructions = Arc::new(AtomicUsize::new(0));
    let mut catalog = ToolCatalog::empty();
    catalog
        .insert(spy_record(
            "mcp.server-1:list_issues",
            constructions.clone(),
        ))
        .unwrap();
    let generation = catalog.generation();
    let err = catalog
        .insert(spy_record(
            "mcp.server-1:list_issues",
            constructions.clone(),
        ))
        .expect_err("duplicate id must fail closed");
    assert!(matches!(err, CatalogError::DuplicateToolId(_)));
    assert_eq!(catalog.generation(), generation);
    assert_eq!(catalog.len(), 1);
}

// ── Bounded compact descriptors ─────────────────────────────────────────────

#[test]
fn summaries_and_tags_are_byte_bounded_against_adversarial_input() {
    let constructions = Arc::new(AtomicUsize::new(0));
    let huge = "α".repeat(10_000);
    let factory_count = constructions.clone();
    let factory = move || -> Arc<dyn Tool> {
        factory_count.fetch_add(1, Ordering::SeqCst);
        Arc::new(FixtureTool { name: "huge" })
    };
    let record = CapabilityRecord::new(
        ToolId::parse("mcp.server-1:huge").unwrap(),
        CapabilitySource::Mcp {
            server_id: "server-1".to_string(),
            server_tool_name: "huge".to_string(),
        },
        &huge,
        vec![huge.clone(); 64],
        SchemaLocator::Inline(serde_json::json!({"type": "object"})),
        Arc::new(factory),
        TrustProvenance::McpConfig {
            server_id: "server-1".to_string(),
        },
    );
    assert!(record.summary().len() <= CapabilityRecord::SUMMARY_MAX_BYTES);
    assert!(record.tags().len() <= CapabilityRecord::MAX_TAGS);
    for tag in record.tags() {
        assert!(tag.len() <= CapabilityRecord::TAG_MAX_BYTES);
    }
    assert_eq!(constructions.load(Ordering::SeqCst), 0);
}

// ── Digest agreement with shared identity type ──────────────────────────────

#[test]
fn record_digest_matches_schema_digest_of_locator_schema() {
    let registry = ToolRegistry::new();
    let catalog = ToolCatalog::from_registry(&registry).unwrap();
    for tool in registry.iter_tools_sorted() {
        let id = ToolId::parse(&format!("builtin:{}", tool.name())).unwrap();
        let record = catalog.get(&id).unwrap();
        assert_eq!(
            record.schema_digest(),
            &SchemaDigest::of_schema(&tool.parameters()),
            "digest for {} must be recomputable from the source schema",
            tool.name()
        );
    }
}
