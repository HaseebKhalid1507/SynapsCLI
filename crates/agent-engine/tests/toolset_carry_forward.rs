//! `SessionToolSet::rebuilt_for_catalog` — the round-top rebuild carries
//! forward exact activations whose CURRENT catalog record still matches the
//! pinned (schema digest, trust provenance); drifted/removed ones drop.
//!
//! The `ExecutionGate` is untouched: the last test proves a carried
//! activation is still denied the moment its record drifts after the rebuild.

use std::sync::Arc;

use agent_engine::tools::activation::{
    DropReason, ExecutionGate, ResolvedToolCall, SessionId, SessionToolSet,
    ToolAuthorizationError,
};
use agent_engine::tools::catalog::{
    CapabilityRecord, CapabilitySource, SchemaLocator, SessionActivationGrant, ToolCatalog, ToolId,
    TrustProvenance,
};
use agent_engine::tools::{Tool, ToolContext, ToolOrigin};
use agent_engine::{Result, Value};
use async_trait::async_trait;

struct FixtureTool;

#[async_trait]
impl Tool for FixtureTool {
    fn name(&self) -> &str {
        "fixture"
    }
    fn description(&self) -> &str {
        "carry-forward fixture"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Unknown
    }
    async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
        panic!("fixture must never execute");
    }
}

fn record(id: &str, schema: Value, server: &str) -> CapabilityRecord {
    CapabilityRecord::new(
        ToolId::parse(id).expect("canonical id"),
        CapabilitySource::Mcp {
            server_id: server.to_string(),
            server_tool_name: id.rsplit(':').next().unwrap_or(id).to_string(),
        },
        "carry-forward fixture capability",
        Vec::new(),
        SchemaLocator::Inline(schema),
        Arc::new(|| -> Arc<dyn Tool> { Arc::new(FixtureTool) }),
        TrustProvenance::McpConfig {
            server_id: server.to_string(),
        },
    )
}

fn mcp(id: &str) -> CapabilityRecord {
    record(id, serde_json::json!({"type": "object"}), "server-1")
}

fn session(raw: &str) -> SessionId {
    SessionId::parse(raw).expect("valid session id")
}

fn grant_for(session: &SessionId, catalog: &ToolCatalog, id: &ToolId) -> SessionActivationGrant {
    let rec = catalog.get(id).expect("present");
    SessionActivationGrant::new(
        session.as_str(),
        id.clone(),
        catalog.generation(),
        rec.schema_digest().clone(),
    )
    .expect("valid grant")
}

fn resolved(id: &ToolId) -> ResolvedToolCall {
    ResolvedToolCall::new("wire", "runtime", id.clone())
}

/// Catalog with one core record and two activatable ones; the set has both
/// activatable tools activated. Rebuilds use the progressive core (only
/// essential builtins, none of which exist here) so activations stay
/// activations; the promotion test uses the default core instead.
fn fixture() -> (ToolCatalog, SessionToolSet, ToolId, ToolId, ToolId) {
    let mut catalog = ToolCatalog::empty();
    let core = mcp("mcp.server-1:core");
    let a = mcp("mcp.server-1:alpha");
    let b = mcp("mcp.server-1:beta");
    let (core_id, a_id, b_id) = (core.id().clone(), a.id().clone(), b.id().clone());
    catalog.insert(core).unwrap();
    catalog.insert(a).unwrap();
    catalog.insert(b).unwrap();

    let sid = session("s-carry");
    let mut set = SessionToolSet::new(sid.clone(), vec![core_id.clone()], &catalog).unwrap();
    set.activate(grant_for(&sid, &catalog, &a_id), &catalog).unwrap();
    set.activate(grant_for(&sid, &catalog, &b_id), &catalog).unwrap();
    assert_eq!(set.schema_generation(), 2);
    (catalog, set, core_id, a_id, b_id)
}

#[test]
fn unrelated_generation_bump_carries_every_activation() {
    let (mut catalog, set, _core, a, b) = fixture();
    catalog.insert(mcp("mcp.server-1:unrelated")).unwrap();
    assert!(set.is_stale(&catalog));

    let (next, dropped) = set.rebuilt_for_catalog(&catalog, true);
    assert!(dropped.is_empty());
    assert!(!next.is_stale(&catalog));
    assert!(next.activation(&a).is_some());
    assert!(next.activation(&b).is_some());
    // Re-issued at the new generation, same digest.
    assert_eq!(
        next.activation(&a).unwrap().catalog_generation(),
        catalog.generation()
    );
    assert_eq!(
        next.activation(&a).unwrap().schema_digest(),
        catalog.get(&a).unwrap().schema_digest()
    );
    // Carrying is not an activation batch.
    assert_eq!(next.schema_generation(), set.schema_generation());

    // Gate accepts the carried grant exactly as a fresh one.
    ExecutionGate::authorize(&catalog, &next, resolved(&a)).expect("carried activation authorizes");
}

#[test]
fn digest_drift_drops_only_that_activation() {
    let (mut catalog, set, _core, a, b) = fixture();
    let changed = record(
        "mcp.server-1:alpha",
        serde_json::json!({"type": "object", "properties": {"x": {"type": "string"}}}),
        "server-1",
    );
    catalog.upsert(Some(&a), changed).unwrap();

    let (next, dropped) = set.rebuilt_for_catalog(&catalog, true);
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped[0].id, a);
    assert_eq!(dropped[0].reason, DropReason::Drifted);
    assert!(next.activation(&a).is_none());
    assert!(next.activation(&b).is_some());
    assert_eq!(next.schema_generation(), set.schema_generation() + 1);

    let err = ExecutionGate::authorize(&catalog, &next, resolved(&a)).expect_err("drifted denied");
    assert_eq!(err, ToolAuthorizationError::NotActivated(a));
}

#[test]
fn provenance_drift_drops_the_activation() {
    let (mut catalog, set, _core, a, b) = fixture();
    // Same id, same schema, different source/provenance (coherent imposter).
    let imposter = record("mcp.server-1:alpha", serde_json::json!({"type": "object"}), "server-2");
    catalog.upsert(Some(&a), imposter).unwrap();

    let (next, dropped) = set.rebuilt_for_catalog(&catalog, true);
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped[0].reason, DropReason::Drifted);
    assert!(next.activation(&a).is_none());
    assert!(next.activation(&b).is_some());
}

/// Source drifts while the pinned provenance stays: a re-activation would
/// fail `check_source_trust`, so carry-forward drops it too (was: carried
/// into the schema, then denied at the gate — same security, wrong report).
#[test]
fn source_provenance_mismatch_drops_the_activation() {
    let (mut catalog, set, _core, a, b) = fixture();
    let mismatched = CapabilityRecord::new(
        a.clone(),
        CapabilitySource::Mcp {
            server_id: "server-2".to_string(),
            server_tool_name: "alpha".to_string(),
        },
        "carry-forward fixture capability",
        Vec::new(),
        SchemaLocator::Inline(serde_json::json!({"type": "object"})),
        Arc::new(|| -> Arc<dyn Tool> { Arc::new(FixtureTool) }),
        // Provenance unchanged from the pinned record.
        TrustProvenance::McpConfig {
            server_id: "server-1".to_string(),
        },
    );
    catalog.upsert(Some(&a), mismatched).unwrap();

    let (next, dropped) = set.rebuilt_for_catalog(&catalog, true);
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped[0].id, a);
    assert_eq!(dropped[0].reason, DropReason::Drifted);
    assert!(next.activation(&a).is_none(), "not exposed in the schema");
    assert!(next.activation(&b).is_some());
}

#[test]
fn removed_tool_is_reported_removed() {
    let (catalog, set, _core, a, b) = fixture();
    // Rebuild the catalog without `alpha`, strictly past the old generation.
    let mut without = ToolCatalog::empty();
    without.insert(mcp("mcp.server-1:core")).unwrap();
    without.insert(mcp("mcp.server-1:beta")).unwrap();
    without.rebase_past(catalog.generation()).unwrap();
    assert!(set.is_stale(&without));

    let (next, dropped) = set.rebuilt_for_catalog(&without, true);
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped[0].id, a);
    assert_eq!(dropped[0].reason, DropReason::Removed);
    assert!(next.activation(&b).is_some());
    assert_eq!(next.schema_generation(), set.schema_generation() + 1);
}

#[test]
fn activation_promoted_to_core_is_not_duplicated() {
    // Default-core rebuild: every verified record becomes core, so the
    // prior activations must land in core, not in activated.
    let (mut catalog, set, _core, a, b) = fixture();
    catalog.insert(mcp("mcp.server-1:unrelated")).unwrap();
    let (next, dropped) = set.rebuilt_for_catalog(&catalog, false);
    assert!(dropped.is_empty());
    assert!(next.is_core(&a));
    assert!(next.is_core(&b));
    assert_eq!(next.activated().count(), 0);
    assert_eq!(next.schema_generation(), set.schema_generation());
    ExecutionGate::authorize(&catalog, &next, resolved(&a)).expect("core authorizes");
}

#[test]
fn carried_activation_still_denied_when_record_drifts_after_rebuild() {
    // Proves the gate is untouched: a carried grant is judged against the
    // CURRENT record on every call, exactly like a fresh activation.
    let (mut catalog, set, _core, a, _b) = fixture();
    catalog.insert(mcp("mcp.server-1:unrelated")).unwrap();
    let (next, _) = set.rebuilt_for_catalog(&catalog, true);
    ExecutionGate::authorize(&catalog, &next, resolved(&a)).expect("carried authorizes");

    let changed = record(
        "mcp.server-1:alpha",
        serde_json::json!({"type": "object", "properties": {"y": {"type": "number"}}}),
        "server-1",
    );
    catalog.upsert(Some(&a), changed).unwrap();
    let err = ExecutionGate::authorize(&catalog, &next, resolved(&a))
        .expect_err("digest drift after rebuild must still deny");
    assert!(
        !matches!(err, ToolAuthorizationError::NotActivated(_)),
        "denied for drift, not for missing activation: {err:?}"
    );
}

#[test]
fn kill_switch_restores_zero_inherit_rebuild() {
    // The env-var read is what the stream consults; `default_core_for_catalog`
    // is the old two-arm rebuild and carries nothing.
    std::env::set_var("SYNAPS_TOOLSET_CARRY_FORWARD", "0");
    assert!(!agent_engine::tools::activation::carry_forward_enabled());
    std::env::remove_var("SYNAPS_TOOLSET_CARRY_FORWARD");
    assert!(agent_engine::tools::activation::carry_forward_enabled());

    let (mut catalog, set, _core, a, _b) = fixture();
    catalog.insert(mcp("mcp.server-1:unrelated")).unwrap();
    let old = SessionToolSet::new(set.session().clone(), Vec::new(), &catalog).unwrap();
    assert_eq!(old.activated().count(), 0);
    let err = ExecutionGate::authorize(&catalog, &old, resolved(&a)).expect_err("zero-inherit");
    assert_eq!(err, ToolAuthorizationError::NotActivated(a));
}
