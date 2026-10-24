//! A session's exact tool activations survive a daemon reload (and a park).
//!
//! The regression (request trace, 2026-10-24 05:37): with deferred loading,
//! an activated tool whose `tool_reference` left the history (context
//! rollover, compaction) is part of the NON-deferred tools array — the start
//! of the cached prompt prefix. The reload record did not carry the
//! session's activations, so the rehydrated session's first request sent
//! 14 tools instead of 19 and rewrote ~70k cached tokens; the model also
//! lost tools it was using mid-session.
//!
//! These tests model the hand-off without a daemon: image A's set →
//! `carried_activations()` → JSON (the reload-state file) → image B's fresh
//! set for a NEW tool session → `restore_carried()`.

use std::collections::HashSet;
use std::sync::Arc;

use agent_engine::tools::activation::{
    activate_exact_for_user, CarriedActivation, CarriedProvenance, ExecutionGate, ResolvedToolCall,
    RestoreDropReason, SessionId, SessionToolSet, ToolAuthorizationError,
};
use agent_engine::tools::catalog::{
    CapabilityRecord, CapabilitySource, SchemaLocator, SessionActivationGrant, ToolCatalog, ToolId,
    TrustProvenance,
};
use agent_engine::tools::{Tool, ToolContext, ToolOrigin, ToolRegistry};
use agent_engine::{Result, Value};
use async_trait::async_trait;

/// The tools the 05:37 reload dropped.
const DROPPED_AT_0537: &[&str] = &[
    "subagent_collect",
    "subagent_models",
    "subagent_start",
    "subagent_status",
    "subagent_steer",
];

fn sid(raw: &str) -> SessionId {
    SessionId::parse(raw).expect("valid session id")
}

/// The reload-state file in between: plain JSON, read back untrusted.
fn through_reload_state(carried: Vec<CarriedActivation>) -> Vec<CarriedActivation> {
    let bytes = serde_json::to_vec(&carried).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// The wire split as the Anthropic body serializes it.
fn wire(
    registry: &ToolRegistry,
    set: &SessionToolSet,
    referenced: &HashSet<String>,
) -> (Vec<u8>, Vec<u8>) {
    let split = registry.deferred_tool_split(set, referenced);
    (
        serde_json::to_vec(&split.loaded).unwrap(),
        serde_json::to_vec(&split.deferred).unwrap(),
    )
}

fn loaded_names(registry: &ToolRegistry, set: &SessionToolSet) -> Vec<String> {
    registry
        .deferred_tool_split(set, &HashSet::new())
        .loaded
        .iter()
        .filter_map(|t| t["name"].as_str().map(String::from))
        .collect()
}

/// Image A: the session activated the subagent lifecycle tools, then a
/// rollover dropped their `tool_reference`s from history.
fn image_a_session() -> (ToolRegistry, SessionToolSet) {
    let registry = ToolRegistry::new();
    let mut set =
        SessionToolSet::progressive_core_for_catalog(sid("rt-image-a"), registry.catalog());
    for name in DROPPED_AT_0537 {
        if registry.catalog().get(&ToolId::builtin(name)).is_some() {
            activate_exact_for_user(&mut set, registry.catalog(), &ToolId::builtin(name))
                .unwrap_or_else(|e| panic!("activate {name}: {e}"));
        }
    }
    assert!(
        set.activated().count() >= 4,
        "fixture activates the subagent tools"
    );
    (registry, set)
}

#[test]
fn reload_keeps_activations_and_the_wire_tool_split_is_byte_identical() {
    let none = HashSet::new();
    let (image_a, set_a) = image_a_session();
    let before = wire(&image_a, &set_a, &none);
    for name in DROPPED_AT_0537 {
        if image_a.catalog().get(&ToolId::builtin(name)).is_some() {
            assert!(
                loaded_names(&image_a, &set_a).iter().any(|n| n == name),
                "reference lost: {name} rides in the non-deferred (cached) tools"
            );
        }
    }

    let carried = through_reload_state(set_a.carried_activations());
    assert_eq!(carried.len(), set_a.activated().count());
    assert!(carried
        .iter()
        .all(|c| c.provenance == CarriedProvenance::BuiltinRuntime));

    // Image B: a new process, a new registry, a new runtime tool session.
    let image_b = ToolRegistry::new();
    let mut set_b =
        SessionToolSet::progressive_core_for_catalog(sid("rt-image-b"), image_b.catalog());
    let report = set_b.restore_carried(&carried, image_b.catalog());
    assert!(report.dropped.is_empty(), "{:?}", report.dropped);
    assert_eq!(report.restored.len(), carried.len());

    let after = wire(&image_b, &set_b, &none);
    assert_eq!(
        String::from_utf8(after.0).unwrap(),
        String::from_utf8(before.0).unwrap(),
        "non-deferred tools (cached prefix) must be byte-identical across the reload"
    );
    assert_eq!(
        String::from_utf8(after.1).unwrap(),
        String::from_utf8(before.1).unwrap(),
        "deferred tools must be byte-identical across the reload"
    );

    // The restored grants are this session's: the gate authorizes them.
    for c in &carried {
        let id = ToolId::parse(&c.tool_id).unwrap();
        ExecutionGate::authorize(
            image_b.catalog(),
            &set_b,
            ResolvedToolCall::new(id.as_str(), id.as_str(), id.clone()),
        )
        .unwrap_or_else(|e| panic!("restored {id} must authorize: {e}"));
    }
}

/// The bug, pinned: without the hand-off the rehydrated session's
/// non-deferred tools shrink (19 → 14 in the 05:37 trace).
#[test]
fn without_the_hand_off_the_cached_tools_array_shrinks() {
    let (image_a, set_a) = image_a_session();
    let image_b = ToolRegistry::new();
    let set_b = SessionToolSet::progressive_core_for_catalog(sid("rt-image-b"), image_b.catalog());
    assert!(
        loaded_names(&image_b, &set_b).len() < loaded_names(&image_a, &set_a).len(),
        "a fresh set has only the core"
    );
}

/// Restore is idempotent and core-aware: an id already activated or now in
/// the core is neither duplicated nor reported dropped.
#[test]
fn restore_skips_ids_already_held() {
    let (image_a, set_a) = image_a_session();
    let carried = set_a.carried_activations();
    let mut again =
        SessionToolSet::progressive_core_for_catalog(sid("rt-again"), image_a.catalog());
    assert_eq!(
        again
            .restore_carried(&carried, image_a.catalog())
            .restored
            .len(),
        carried.len()
    );
    let generation = again.schema_generation();
    let second = again.restore_carried(&carried, image_a.catalog());
    assert!(
        second.restored.is_empty() && second.dropped.is_empty(),
        "{second:?}"
    );
    assert_eq!(
        again.schema_generation(),
        generation,
        "nothing restored, no batch"
    );

    let mut default_core =
        SessionToolSet::default_core_for_catalog(sid("rt-default"), image_a.catalog());
    let report = default_core.restore_carried(&carried, image_a.catalog());
    assert!(
        report.restored.is_empty() && report.dropped.is_empty(),
        "{report:?}"
    );
    assert_eq!(default_core.activated().count(), 0);
}

// ── drift between images: dropped fail-closed, reported, never re-granted ──

struct FixtureTool;

#[async_trait]
impl Tool for FixtureTool {
    fn name(&self) -> &str {
        "fixture"
    }
    fn description(&self) -> &str {
        "reload fixture"
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
        "reload fixture capability",
        Vec::new(),
        SchemaLocator::Inline(schema),
        Arc::new(|| -> Arc<dyn Tool> { Arc::new(FixtureTool) }),
        TrustProvenance::McpConfig {
            server_id: server.to_string(),
        },
    )
}

fn plain(id: &str) -> CapabilityRecord {
    record(id, serde_json::json!({"type": "object"}), "server-1")
}

const ALPHA: &str = "mcp.server-1:alpha";
const BETA: &str = "mcp.server-1:beta";

/// Image A: `alpha` and `beta` activated; their pins carried as JSON.
fn carried_from_image_a() -> Vec<CarriedActivation> {
    let mut catalog = ToolCatalog::empty();
    catalog.insert(plain(ALPHA)).unwrap();
    catalog.insert(plain(BETA)).unwrap();
    let session = sid("rt-a");
    let mut set = SessionToolSet::new(session.clone(), Vec::new(), &catalog).unwrap();
    for id in [ALPHA, BETA] {
        let id = ToolId::parse(id).unwrap();
        let grant = SessionActivationGrant::new(
            session.as_str(),
            id.clone(),
            catalog.generation(),
            catalog.get(&id).unwrap().schema_digest().clone(),
        )
        .unwrap();
        set.activate(grant, &catalog).unwrap();
    }
    through_reload_state(set.carried_activations())
}

/// Image B's catalog with `alpha` replaced by `alpha_record` (or absent).
fn image_b(alpha_record: Option<CapabilityRecord>) -> (ToolCatalog, SessionToolSet) {
    let mut catalog = ToolCatalog::empty();
    if let Some(alpha) = alpha_record {
        catalog.insert(alpha).unwrap();
    }
    catalog.insert(plain(BETA)).unwrap();
    let set = SessionToolSet::new(sid("rt-b"), Vec::new(), &catalog).unwrap();
    (catalog, set)
}

fn assert_only_alpha_dropped(
    catalog: &ToolCatalog,
    set: &SessionToolSet,
    report: &agent_engine::tools::activation::RestoreReport,
    reason: RestoreDropReason,
) {
    let alpha = ToolId::parse(ALPHA).unwrap();
    let beta = ToolId::parse(BETA).unwrap();
    assert_eq!(report.dropped, vec![(ALPHA.to_string(), reason)]);
    assert_eq!(report.restored, vec![beta.clone()]);
    assert!(
        set.activation(&alpha).is_none(),
        "never silently re-granted"
    );
    assert!(set.activation(&beta).is_some());
    if catalog.get(&alpha).is_some() {
        let err = ExecutionGate::authorize(
            catalog,
            set,
            ResolvedToolCall::new("alpha", "alpha", alpha.clone()),
        )
        .expect_err("a dropped tool is denied");
        assert_eq!(err, ToolAuthorizationError::NotActivated(alpha));
    }
}

#[test]
fn schema_digest_drift_between_images_is_dropped_and_reported() {
    let carried = carried_from_image_a();
    let changed = record(
        ALPHA,
        serde_json::json!({"type": "object", "properties": {"x": {"type": "string"}}}),
        "server-1",
    );
    let (catalog, mut set) = image_b(Some(changed));
    let report = set.restore_carried(&carried, &catalog);
    assert_only_alpha_dropped(&catalog, &set, &report, RestoreDropReason::Drifted);
}

#[test]
fn provenance_drift_between_images_is_dropped_and_reported() {
    let carried = carried_from_image_a();
    // Same id, same schema, a different (internally coherent) provenance.
    let imposter = record(ALPHA, serde_json::json!({"type": "object"}), "server-2");
    let (catalog, mut set) = image_b(Some(imposter));
    let report = set.restore_carried(&carried, &catalog);
    assert_only_alpha_dropped(&catalog, &set, &report, RestoreDropReason::Drifted);
}

#[test]
fn source_provenance_mismatch_between_images_is_dropped() {
    let carried = carried_from_image_a();
    let mismatched = CapabilityRecord::new(
        ToolId::parse(ALPHA).unwrap(),
        CapabilitySource::Mcp {
            server_id: "server-2".to_string(),
            server_tool_name: "alpha".to_string(),
        },
        "reload fixture capability",
        Vec::new(),
        SchemaLocator::Inline(serde_json::json!({"type": "object"})),
        Arc::new(|| -> Arc<dyn Tool> { Arc::new(FixtureTool) }),
        TrustProvenance::McpConfig {
            server_id: "server-1".to_string(),
        },
    );
    let (catalog, mut set) = image_b(Some(mismatched));
    let report = set.restore_carried(&carried, &catalog);
    assert_only_alpha_dropped(&catalog, &set, &report, RestoreDropReason::Drifted);
}

#[test]
fn a_tool_gone_from_the_new_image_is_reported_removed() {
    let carried = carried_from_image_a();
    let (catalog, mut set) = image_b(None);
    let report = set.restore_carried(&carried, &catalog);
    assert_only_alpha_dropped(&catalog, &set, &report, RestoreDropReason::Removed);
}

/// The reload-state file is untrusted input: a tampered pin or a garbage id
/// grants nothing.
#[test]
fn tampered_reload_state_grants_nothing() {
    let mut carried = carried_from_image_a();
    carried[0].schema_digest = "0".repeat(64);
    carried[1].provenance = CarriedProvenance::McpConfig {
        server_id: "elsewhere".into(),
    };
    carried.push(CarriedActivation {
        tool_id: "NOT A TOOL ID".into(),
        schema_digest: String::new(),
        provenance: CarriedProvenance::BuiltinRuntime,
    });
    let (catalog, mut set) = image_b(Some(plain(ALPHA)));
    let report = set.restore_carried(&carried, &catalog);
    assert!(report.restored.is_empty(), "{report:?}");
    assert_eq!(report.dropped.len(), 3);
    assert_eq!(set.activated().count(), 0);
    assert_eq!(set.schema_generation(), 0);
}

/// A set that is not built against the catalog it is restored against
/// takes nothing (same rule as `activate`).
#[test]
fn a_stale_set_takes_no_restore() {
    let carried = carried_from_image_a();
    let (mut catalog, mut set) = image_b(Some(plain(ALPHA)));
    catalog.insert(plain("mcp.server-1:unrelated")).unwrap();
    let report = set.restore_carried(&carried, &catalog);
    assert!(report.restored.is_empty());
    assert!(report
        .dropped
        .iter()
        .all(|(_, reason)| *reason == RestoreDropReason::StaleSet));
    assert_eq!(set.activated().count(), 0);
}
