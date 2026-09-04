//! Task 17 — deterministic `ToolRegistry` schema projection for a
//! `SessionToolSet` (spec §7.7, §4.2).
//!
//! The projection selects, from the EXISTING cached exposed schema, exactly
//! the entries whose capability identity is core or exactly-activated for
//! the session — preserving API-safe names, collision suffixes, and the
//! cached byte ordering. It performs no catalog insertion, no schema
//! rebuild, no factory invocation. Default runtime behavior (full cached
//! schema sent to providers) is untouched until Task 18 opts in.

use std::sync::Arc;

use agent_engine::tools::activation::{activate_exact_for_user, SessionId, SessionToolSet};
use agent_engine::tools::catalog::{
    CapabilityRecord, CapabilitySource, SchemaLocator, ToolId, TrustProvenance,
};
use agent_engine::tools::DroppedSessionMember;
use agent_engine::tools::{Tool, ToolContext, ToolOrigin, ToolRegistry};
use agent_engine::{Result, Value};
use async_trait::async_trait;
use serde_json::json;

struct NamedTool(&'static str);

#[async_trait]
impl Tool for NamedTool {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "projection fixture tool"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"arg": {"type": "string"}}})
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
        Ok("ok".to_string())
    }
}

fn session_id() -> SessionId {
    SessionId::parse("task17-projection").expect("valid session id")
}

/// Registry with colliding sanitized names: `a:b` → `a_b`, `a.b` → `a_b_2`,
/// plus a plain deferred/core pair.
fn collision_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::empty();
    registry.register(Arc::new(NamedTool("a:b")));
    registry.register(Arc::new(NamedTool("a.b")));
    registry.register(Arc::new(NamedTool("core_tool")));
    registry.register(Arc::new(NamedTool("deferred_tool")));
    registry
}

fn projection_bytes(registry: &ToolRegistry, set: &SessionToolSet) -> Vec<u8> {
    serde_json::to_vec(&registry.session_tools_schema(set).schema).expect("serializes")
}

#[test]
fn projection_selects_core_entries_from_cached_schema_in_stable_order() {
    let registry = collision_registry();
    let set = SessionToolSet::new(
        session_id(),
        [ToolId::builtin("core_tool")],
        registry.catalog(),
    )
    .expect("core resolves");

    let projection = registry.session_tools_schema(&set).schema;
    let names: Vec<&str> = projection
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert_eq!(names, vec!["core_tool"], "core-only projection");

    // The projected entry is the EXACT cached schema value (no rebuild).
    let cached = registry.tools_schema();
    let cached_core = cached
        .iter()
        .find(|s| s["name"] == "core_tool")
        .expect("cached entry exists");
    assert_eq!(&projection[0], cached_core);

    // Byte-stable across repeated calls.
    assert_eq!(
        projection_bytes(&registry, &set),
        projection_bytes(&registry, &set)
    );
}

#[test]
fn activation_adds_exactly_one_cached_schema_and_siblings_stay_absent() {
    let registry = collision_registry();
    let mut set = SessionToolSet::new(
        session_id(),
        [ToolId::builtin("core_tool")],
        registry.catalog(),
    )
    .expect("core resolves");

    let before = registry.session_tools_schema(&set).schema;

    activate_exact_for_user(
        &mut set,
        registry.catalog(),
        &ToolId::builtin("deferred_tool"),
    )
    .expect("activation succeeds");

    let after = registry.session_tools_schema(&set).schema;
    assert_eq!(after.len(), before.len() + 1, "exactly one schema added");

    let names: Vec<&str> = after.iter().filter_map(|s| s["name"].as_str()).collect();
    assert_eq!(names, vec!["core_tool", "deferred_tool"]);
    // Siblings (`a:b`, `a.b`) remain absent.
    assert!(!names.contains(&"a_b"));
    assert!(!names.contains(&"a_b_2"));

    // The added entry is byte-identical to the cached full-schema entry.
    let cached = registry.tools_schema();
    let cached_deferred = cached
        .iter()
        .find(|s| s["name"] == "deferred_tool")
        .expect("cached entry exists");
    assert_eq!(
        after.iter().find(|s| s["name"] == "deferred_tool").unwrap(),
        cached_deferred
    );
}

#[test]
fn projection_preserves_api_alias_and_collision_names() {
    let registry = collision_registry();
    let mut set = SessionToolSet::new(session_id(), Vec::<ToolId>::new(), registry.catalog())
        .expect("empty core resolves");

    // Activate the collision pair; their sanitized API names must match the
    // cached exposed schema exactly (`a_b`, `a_b_2` — deterministic suffix
    // assignment preserved, ToolId derived from the live tools).
    let a_colon = ToolId::builtin("a:b");
    let a_dot = ToolId::builtin("a.b");
    activate_exact_for_user(&mut set, registry.catalog(), &a_colon).expect("activates");
    activate_exact_for_user(&mut set, registry.catalog(), &a_dot).expect("activates");

    let projection = registry.session_tools_schema(&set).schema;
    let names: Vec<&str> = projection
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert_eq!(names, vec!["a_b", "a_b_2"], "collision mapping preserved");
}

#[test]
fn projection_never_mutates_default_full_schema_exposure() {
    let registry = collision_registry();
    let set = SessionToolSet::default_core_for_catalog(session_id(), registry.catalog());

    let full_before = serde_json::to_vec(registry.tools_schema().as_ref()).expect("serializes");
    let _ = registry.session_tools_schema(&set).schema;
    let full_after = serde_json::to_vec(registry.tools_schema().as_ref()).expect("serializes");

    // Default behavior stays byte-identical: the full cached schema is
    // untouched by projection reads (Task 18 flips exposure, not Task 17).
    assert_eq!(full_before, full_after);

    // A default-core session projects the entire cached schema, byte-equal.
    let projection = registry.session_tools_schema(&set).schema;
    assert_eq!(
        serde_json::to_vec(&projection).expect("serializes"),
        full_before
    );
}

#[test]
fn drifted_session_set_projects_only_digest_matching_pins() {
    let mut registry = collision_registry();
    let set = SessionToolSet::default_core_for_catalog(session_id(), registry.catalog());
    let before = registry.session_tools_schema(&set);
    assert!(before.dropped.is_empty(), "fresh set drops nothing");
    let before = before.schema;

    // Unrelated registration drifts the generation; the session's pinned
    // members are untouched, so the projection survives and serves exactly
    // the digest-matching pins — the late tool is absent (never pinned).
    registry.register(Arc::new(NamedTool("late_tool")));
    let report = registry.session_tools_schema(&set);
    assert!(
        report.dropped.is_empty(),
        "unrelated drift must not drop any pinned member"
    );
    let after = report.schema;
    assert_eq!(
        serde_json::to_vec(&after).expect("serializes"),
        serde_json::to_vec(&before).expect("serializes"),
        "projection across unrelated drift is byte-identical to the fresh one"
    );
    assert!(
        !after
            .iter()
            .any(|s| s["name"].as_str() == Some("late_tool")),
        "unpinned late registration must not appear in the session projection"
    );
}

/// Concern-2 report shape: a pinned member whose record drifted (digest) is
/// EXCLUDED from the served schema and REPORTED typed — never served, never
/// silently swallowed into a log line.
#[test]
fn projection_reports_digest_drifted_member_as_dropped() {
    let mut registry = collision_registry();
    let set = SessionToolSet::default_core_for_catalog(session_id(), registry.catalog());

    // Same runtime name + origin → same ToolId, different parameters →
    // different schema digest for the current record.
    struct ChangedTool;
    #[async_trait]
    impl Tool for ChangedTool {
        fn name(&self) -> &str {
            "core_tool"
        }
        fn description(&self) -> &str {
            "changed fixture tool"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {"other": {"type": "number"}}})
        }
        fn origin(&self) -> ToolOrigin {
            ToolOrigin::Builtin
        }
        async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
            Ok("ok".to_string())
        }
    }
    registry.register(Arc::new(ChangedTool));

    let report = registry.session_tools_schema(&set);
    assert!(
        !report
            .schema
            .iter()
            .any(|s| s["name"].as_str() == Some("core_tool")),
        "a drifted member's schema must never be served"
    );
    assert_eq!(
        report.dropped,
        vec![(
            ToolId::builtin("core_tool"),
            DroppedSessionMember::SchemaDigestMismatch
        )]
    );
}

/// BLOCKER coverage at the projection boundary: a coherent imposter (same
/// `ToolId`, identical schema/digest, different but internally consistent
/// source+provenance) must be excluded from the served schema and reported
/// as a provenance mismatch — pinned provenance governs inclusion.
#[test]
fn projection_reports_coherent_provenance_imposter_as_dropped() {
    let mut registry = collision_registry();
    let set = SessionToolSet::default_core_for_catalog(session_id(), registry.catalog());
    let id = ToolId::builtin("core_tool");
    let pinned_digest = registry
        .catalog()
        .get(&id)
        .expect("record present")
        .schema_digest()
        .clone();

    // Replace the catalog record in place: same ToolId, byte-identical
    // schema (equal digest), but a coherent Plugin source+provenance pair.
    let imposter = CapabilityRecord::new(
        id.clone(),
        CapabilitySource::Plugin {
            plugin_id: "imposter-plugin".to_string(),
            tool_name: "core_tool".to_string(),
        },
        "projection fixture tool",
        Vec::new(),
        SchemaLocator::Inline(json!({"type": "object", "properties": {"arg": {"type": "string"}}})),
        std::sync::Arc::new(|| -> Arc<dyn Tool> {
            panic!("projection must never invoke the implementation factory")
        }),
        TrustProvenance::PluginConfig {
            plugin_id: "imposter-plugin".to_string(),
        },
    );
    assert_eq!(
        imposter.schema_digest(),
        &pinned_digest,
        "imposter must be schema-identical for the test to bite"
    );
    registry.upsert_catalog_record_for_tests(Some(&id), imposter);

    let report = registry.session_tools_schema(&set);
    assert!(
        !report
            .schema
            .iter()
            .any(|s| s["name"].as_str() == Some("core_tool")),
        "a provenance-drifted member's schema must never be served"
    );
    assert_eq!(
        report.dropped,
        vec![(id, DroppedSessionMember::ProvenanceMismatch)]
    );
}
