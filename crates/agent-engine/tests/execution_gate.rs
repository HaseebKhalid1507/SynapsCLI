//! Task 16 — `ExecutionGate` (spec §7.1 step list, §4.2 state separation).
//!
//! The gate is the load-bearing authorization boundary immediately before
//! tool implementation lookup/execution: resolve the wire name to the exact
//! live `ToolId`, verify catalog/session snapshot generation and pinned
//! schema digest, require core status or an exact session activation grant,
//! re-check source permission/trust conservatively, and only then acquire
//! the implementation. Every denial is typed, static, and metadata-only;
//! aliases and collision suffixes can never select or authorize a different
//! identity; a failed authorization leaves session state untouched and
//! never invokes an implementation factory.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agent_engine::tools::activation::{
    ExecutionGate, ResolvedToolCall, SessionId, SessionToolSet, ToolAuthorizationError,
};
use agent_engine::tools::catalog::{
    CapabilityRecord, CapabilitySource, SchemaLocator, SessionActivationGrant, ToolCatalog, ToolId,
    TrustProvenance,
};
use agent_engine::tools::{Tool, ToolContext, ToolOrigin, ToolRegistry};
use agent_engine::{Result, Value};
use async_trait::async_trait;

// ── Fixtures ────────────────────────────────────────────────────────────────

/// A registrable tool with a configurable name and origin whose execution
/// must never be reached by gate tests.
struct FixtureTool {
    name: String,
    origin: ToolOrigin,
}

impl FixtureTool {
    fn extension(name: &str, extension_id: &str) -> Self {
        Self {
            name: name.to_string(),
            origin: ToolOrigin::Extension {
                extension_id: extension_id.to_string(),
            },
        }
    }

    fn unknown(name: &str) -> Self {
        Self {
            name: name.to_string(),
            origin: ToolOrigin::Unknown,
        }
    }

    fn builtin(name: &str) -> Self {
        Self {
            name: name.to_string(),
            origin: ToolOrigin::Builtin,
        }
    }
}

#[async_trait]
impl Tool for FixtureTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "gate fixture tool"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn extension_id(&self) -> Option<&str> {
        match &self.origin {
            ToolOrigin::Extension { extension_id } => Some(extension_id),
            _ => None,
        }
    }
    fn origin(&self) -> ToolOrigin {
        self.origin.clone()
    }
    async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
        panic!("gate fixture tool must never execute in authorization tests");
    }
}

/// A deferred capability whose factory counts invocations: the gate must
/// invoke it exactly once on success and never on denial.
fn spy_record(
    id: ToolId,
    source: CapabilitySource,
    provenance: TrustProvenance,
    spy: Arc<AtomicUsize>,
) -> CapabilityRecord {
    let factory = move || -> Arc<dyn Tool> {
        spy.fetch_add(1, Ordering::SeqCst);
        Arc::new(FixtureTool::unknown("spy-implementation"))
    };
    CapabilityRecord::new(
        id,
        source,
        "gate fixture capability",
        Vec::new(),
        SchemaLocator::Inline(serde_json::json!({"type": "object"})),
        Arc::new(factory),
        provenance,
    )
}

fn mcp_spy_record(id: &str, spy: Arc<AtomicUsize>) -> CapabilityRecord {
    spy_record(
        ToolId::parse(id).expect("fixture id is canonical"),
        CapabilitySource::Mcp {
            server_id: "server-1".to_string(),
            server_tool_name: id.rsplit(':').next().unwrap_or(id).to_string(),
        },
        TrustProvenance::McpConfig {
            server_id: "server-1".to_string(),
        },
        spy,
    )
}

fn session(raw: &str) -> SessionId {
    SessionId::parse(raw).expect("fixture session id is valid")
}

fn grant_for(session: &SessionId, catalog: &ToolCatalog, id: &ToolId) -> SessionActivationGrant {
    let record = catalog.get(id).expect("tool present in catalog");
    SessionActivationGrant::new(
        session.as_str(),
        id.clone(),
        catalog.generation(),
        record.schema_digest().clone(),
    )
    .expect("fixture grant is valid")
}

fn resolved(catalog_id: &ToolId) -> ResolvedToolCall {
    ResolvedToolCall::new("wire-name", "runtime-name", catalog_id.clone())
}

// ── Core success path ───────────────────────────────────────────────────────

#[test]
fn default_core_builtin_authorizes_with_exact_identity() {
    let registry = ToolRegistry::new();
    let set = SessionToolSet::default_core_for_catalog(session("s-core"), registry.catalog());

    let authorized = ExecutionGate::authorize_wire_call(&registry, &set, "bash")
        .expect("core builtin must authorize");
    assert_eq!(authorized.wire_name(), "bash");
    assert_eq!(authorized.runtime_name(), "bash");
    assert_eq!(authorized.tool_id(), &ToolId::builtin("bash"));
    assert_eq!(authorized.implementation().name(), "bash");
}

#[test]
fn default_session_set_core_is_exactly_verified_registered_tools() {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(FixtureTool::unknown("mystery-tool")));
    let set = SessionToolSet::default_core_for_catalog(session("s-default"), registry.catalog());

    // Every verified-source registered tool is core (behavior preservation).
    for record in registry.catalog().iter() {
        if record.provenance() == &TrustProvenance::Unverified {
            assert!(
                !set.is_core(record.id()),
                "unverified capability must not be core: {}",
                record.id()
            );
        } else {
            assert!(
                set.is_core(record.id()),
                "verified registered tool must be core by default: {}",
                record.id()
            );
        }
    }
    assert_eq!(set.activated().count(), 0, "no default activations");
}

// ── Deferred (non-core) denial and exact activation ─────────────────────────

/// Retained-set semantics the stream loop relies on (Task 16 review fix):
/// ONE set snapshot judges every sibling call of a model response at ONE
/// generation; a catalog mutation makes that retained set stale for ALL
/// subsequent calls (typed denial, never a silent per-call refresh); only
/// an explicit deterministic rebuild — the stream's round-top step —
/// recovers, and it exposes newly registered verified tools as default
/// core with zero inherited activations.
#[test]
fn retained_set_judges_siblings_at_one_generation_and_denies_after_mutation_until_rebuild() {
    let mut registry = ToolRegistry::new();
    let set = SessionToolSet::default_core_for_catalog(session("s-retained"), registry.catalog());
    let built_at = set.catalog_generation();

    // Sibling calls of one response: same retained set, same generation.
    ExecutionGate::authorize_wire_call(&registry, &set, "bash").expect("sibling 1 authorizes");
    ExecutionGate::authorize_wire_call(&registry, &set, "ls").expect("sibling 2 authorizes");
    assert_eq!(
        set.catalog_generation(),
        built_at,
        "authorization must never advance or refresh the set snapshot"
    );

    // Dynamic registration advances the catalog generation.
    registry.register(Arc::new(FixtureTool::builtin("late-registered")));

    // The retained set is now stale for EVERY call — including tools that
    // authorized moments ago — until the explicit rebuild.
    for wire in ["bash", "ls", "late-registered"] {
        let err = ExecutionGate::authorize_wire_call(&registry, &set, wire)
            .expect_err("stale retained set must deny, not silently refresh");
        assert_eq!(
            err,
            ToolAuthorizationError::StaleSessionSet {
                set: built_at,
                catalog: registry.catalog().generation(),
            },
            "denial must carry the exact stale/current generation pair"
        );
    }

    // Explicit deterministic rebuild (the stream's round-top step): fresh
    // default core from currently verified tools, zero activations.
    let rebuilt =
        SessionToolSet::default_core_for_catalog(session("s-retained"), registry.catalog());
    assert_eq!(rebuilt.activated().count(), 0, "no inherited activations");
    ExecutionGate::authorize_wire_call(&registry, &rebuilt, "bash").expect("rebuild recovers");
    ExecutionGate::authorize_wire_call(&registry, &rebuilt, "late-registered")
        .expect("newly registered verified tool becomes default core after rebuild");
}

#[test]
fn trusted_non_core_without_grant_is_not_activated_and_factory_untouched() {
    let spy = Arc::new(AtomicUsize::new(0));
    let mut catalog = ToolCatalog::empty();
    let record = mcp_spy_record("mcp.server-1:deferred", Arc::clone(&spy));
    let id = record.id().clone();
    catalog.insert(record).expect("insert fixture record");

    let set = SessionToolSet::new(session("s-deferred"), Vec::new(), &catalog).expect("empty core");
    let err = ExecutionGate::authorize(&catalog, &set, resolved(&id))
        .expect_err("non-core tool without grant must be denied");
    assert_eq!(err, ToolAuthorizationError::NotActivated(id));
    assert_eq!(
        spy.load(Ordering::SeqCst),
        0,
        "denial must never invoke the implementation factory"
    );
    assert_eq!(set.activated().count(), 0, "failure leaves set unchanged");
}

#[test]
fn exact_activation_grant_authorizes_and_acquires_once() {
    let spy = Arc::new(AtomicUsize::new(0));
    let mut catalog = ToolCatalog::empty();
    let record = mcp_spy_record("mcp.server-1:deferred", Arc::clone(&spy));
    let id = record.id().clone();
    catalog.insert(record).expect("insert fixture record");

    let sid = session("s-activated");
    let mut set = SessionToolSet::new(sid.clone(), Vec::new(), &catalog).expect("empty core");
    set.activate(grant_for(&sid, &catalog, &id), &catalog)
        .expect("exact grant activates");

    let authorized = ExecutionGate::authorize(&catalog, &set, resolved(&id))
        .expect("exact activation must authorize");
    assert_eq!(authorized.tool_id(), &id);
    assert_eq!(
        spy.load(Ordering::SeqCst),
        1,
        "implementation acquired exactly once, only after authorization"
    );
}

#[test]
fn foreign_session_set_has_no_grant_and_is_denied() {
    let spy = Arc::new(AtomicUsize::new(0));
    let mut catalog = ToolCatalog::empty();
    let record = mcp_spy_record("mcp.server-1:deferred", Arc::clone(&spy));
    let id = record.id().clone();
    catalog.insert(record).expect("insert fixture record");

    let sid_a = session("s-a");
    let mut set_a = SessionToolSet::new(sid_a.clone(), Vec::new(), &catalog).expect("empty core");
    set_a
        .activate(grant_for(&sid_a, &catalog, &id), &catalog)
        .expect("exact grant activates in session A");

    // Session B inherits nothing from session A.
    let set_b = SessionToolSet::new(session("s-b"), Vec::new(), &catalog).expect("empty core");
    let err = ExecutionGate::authorize(&catalog, &set_b, resolved(&id))
        .expect_err("foreign session must not borrow session A's activation");
    assert_eq!(err, ToolAuthorizationError::NotActivated(id));
    assert_eq!(spy.load(Ordering::SeqCst), 0);
}

// ── Staleness and digest pinning ────────────────────────────────────────────

#[test]
fn catalog_mutation_stales_session_set_and_denies() {
    let spy = Arc::new(AtomicUsize::new(0));
    let mut catalog = ToolCatalog::empty();
    let record = mcp_spy_record("mcp.server-1:deferred", Arc::clone(&spy));
    let id = record.id().clone();
    catalog.insert(record).expect("insert fixture record");

    let set = SessionToolSet::new(session("s-stale"), vec![id.clone()], &catalog)
        .expect("core set builds");

    // Registry/catalog mutation after the snapshot: generation advances.
    catalog
        .insert(mcp_spy_record("mcp.server-1:later", Arc::clone(&spy)))
        .expect("mutation advances generation");

    let err = ExecutionGate::authorize(&catalog, &set, resolved(&id))
        .expect_err("stale session snapshot must deny, not silently bless");
    assert!(
        matches!(err, ToolAuthorizationError::StaleSessionSet { .. }),
        "expected StaleSessionSet, got: {err:?}"
    );
    assert_eq!(spy.load(Ordering::SeqCst), 0);

    // Explicit deterministic rebuild against the current catalog recovers.
    let rebuilt = SessionToolSet::new(session("s-stale"), vec![id.clone()], &catalog)
        .expect("rebuild against current catalog");
    ExecutionGate::authorize(&catalog, &rebuilt, resolved(&id))
        .expect("rebuilt set authorizes again");
}

#[test]
fn changed_schema_digest_invalidates_pinned_core_snapshot() {
    let spy = Arc::new(AtomicUsize::new(0));
    let mut catalog = ToolCatalog::empty();
    let record = mcp_spy_record("mcp.server-1:deferred", Arc::clone(&spy));
    let id = record.id().clone();
    catalog.insert(record).expect("insert fixture record");

    let set = SessionToolSet::new(session("s-digest"), vec![id.clone()], &catalog)
        .expect("core set builds");
    let pinned_generation = set.catalog_generation();

    // The capability's schema changes in place (same id, new digest).
    let changed = CapabilityRecord::new(
        id.clone(),
        CapabilitySource::Mcp {
            server_id: "server-1".to_string(),
            server_tool_name: "deferred".to_string(),
        },
        "gate fixture capability",
        Vec::new(),
        SchemaLocator::Inline(serde_json::json!({
            "type": "object",
            "properties": {"changed": {"type": "string"}}
        })),
        {
            let spy = Arc::clone(&spy);
            Arc::new(move || -> Arc<dyn Tool> {
                spy.fetch_add(1, Ordering::SeqCst);
                Arc::new(FixtureTool::unknown("spy-implementation"))
            })
        },
        TrustProvenance::McpConfig {
            server_id: "server-1".to_string(),
        },
    );
    catalog
        .upsert(Some(&id), changed)
        .expect("replace in place");
    // Force the generations back into agreement so the digest check itself
    // is exercised (not masked by the staleness check).
    catalog.set_generation_for_tests(pinned_generation);

    let err = ExecutionGate::authorize(&catalog, &set, resolved(&id))
        .expect_err("changed schema digest must never be silently blessed");
    assert_eq!(err, ToolAuthorizationError::SchemaDigestMismatch(id));
    assert_eq!(spy.load(Ordering::SeqCst), 0);
}

// ── Source/trust re-check ───────────────────────────────────────────────────

#[test]
fn unverified_provenance_is_denied_even_when_core() {
    let spy = Arc::new(AtomicUsize::new(0));
    let mut catalog = ToolCatalog::empty();
    let record = spy_record(
        ToolId::unclassified("mystery"),
        CapabilitySource::Unknown {
            runtime_name: "mystery".to_string(),
        },
        TrustProvenance::Unverified,
        Arc::clone(&spy),
    );
    let id = record.id().clone();
    catalog.insert(record).expect("insert fixture record");

    // Deliberately (mis)configure the unverified tool as core: the trust
    // re-check must still deny by default.
    let set = SessionToolSet::new(session("s-unverified"), vec![id.clone()], &catalog)
        .expect("core set builds");
    let err = ExecutionGate::authorize(&catalog, &set, resolved(&id))
        .expect_err("unverified provenance must be denied even if core");
    assert_eq!(err, ToolAuthorizationError::UntrustedSource(id));
    assert_eq!(spy.load(Ordering::SeqCst), 0);
}

#[test]
fn source_provenance_mismatch_is_denied() {
    let spy = Arc::new(AtomicUsize::new(0));
    // Extension-shaped source but MCP-shaped provenance: the catalog record
    // is internally inconsistent and must never authorize.
    let record = spy_record(
        ToolId::extension("acme", "tool"),
        CapabilitySource::Extension {
            extension_id: "acme".to_string(),
            tool_name: "tool".to_string(),
        },
        TrustProvenance::McpConfig {
            server_id: "server-1".to_string(),
        },
        Arc::clone(&spy),
    );
    let id = record.id().clone();
    let mut catalog = ToolCatalog::empty();
    catalog.insert(record).expect("insert fixture record");

    let set = SessionToolSet::new(session("s-mismatch"), vec![id.clone()], &catalog)
        .expect("core set builds");
    let err = ExecutionGate::authorize(&catalog, &set, resolved(&id))
        .expect_err("source/provenance mismatch must be denied");
    assert_eq!(err, ToolAuthorizationError::SourceProvenanceMismatch(id));
    assert_eq!(spy.load(Ordering::SeqCst), 0);
}

// ── Alias / collision-suffix identity safety ────────────────────────────────

#[test]
fn api_safe_alias_resolves_to_exact_intended_identity() {
    let mut registry = ToolRegistry::empty();
    registry.register(Arc::new(FixtureTool::extension(
        "acme:spaced name!",
        "acme",
    )));

    // The exposed schema carries the sanitized alias, not the runtime name.
    let schema = registry.tools_schema();
    let api_name = schema[0]["name"].as_str().expect("api name").to_string();
    assert_ne!(api_name, "acme:spaced name!");

    let set = SessionToolSet::default_core_for_catalog(session("s-alias"), registry.catalog());
    let authorized = ExecutionGate::authorize_wire_call(&registry, &set, &api_name)
        .expect("alias resolves and authorizes the exact tool");
    assert_eq!(authorized.runtime_name(), "acme:spaced name!");
    assert_eq!(
        authorized.tool_id(),
        &ToolId::extension("acme", "spaced name!")
    );
}

#[test]
fn collision_suffix_cannot_borrow_sibling_core_status() {
    let mut registry = ToolRegistry::empty();
    // "trusted_tool" (verified extension) and "trusted tool!" (Unknown
    // origin) sanitize onto colliding api-safe names; deterministic suffixing
    // keeps them distinct. The unknown sibling must not inherit the trusted
    // sibling's core status through either alias.
    registry.register(Arc::new(FixtureTool::extension("trusted_tool", "acme")));
    registry.register(Arc::new(FixtureTool::unknown("trusted tool!")));

    let schema = registry.tools_schema();
    let api_names: Vec<String> = schema
        .iter()
        .map(|t| t["name"].as_str().expect("api name").to_string())
        .collect();
    assert_eq!(api_names.len(), 2);
    assert_ne!(api_names[0], api_names[1], "collision must be suffixed");

    let set = SessionToolSet::default_core_for_catalog(session("s-collide"), registry.catalog());
    let mut authorized_trusted = 0;
    let mut denied_unknown = 0;
    for api_name in &api_names {
        match ExecutionGate::authorize_wire_call(&registry, &set, api_name) {
            Ok(authorized) => {
                assert_eq!(authorized.runtime_name(), "trusted_tool");
                assert_eq!(
                    authorized.tool_id(),
                    &ToolId::extension("acme", "trusted_tool")
                );
                authorized_trusted += 1;
            }
            Err(err) => {
                assert_eq!(
                    err,
                    ToolAuthorizationError::NotActivated(ToolId::unclassified("trusted tool!")),
                    "unknown sibling must be denied under its OWN exact identity"
                );
                denied_unknown += 1;
            }
        }
    }
    assert_eq!(
        authorized_trusted, 1,
        "exactly one alias is the trusted tool"
    );
    assert_eq!(denied_unknown, 1, "exactly one alias is the denied unknown");
}

// ── Unknown wire names ──────────────────────────────────────────────────────

#[test]
fn unknown_wire_name_yields_bounded_typed_denial() {
    let registry = ToolRegistry::new();
    let set = SessionToolSet::default_core_for_catalog(session("s-unknown"), registry.catalog());

    let hostile = "x".repeat(64 * 1024);
    let err = ExecutionGate::authorize_wire_call(&registry, &set, &hostile)
        .expect_err("unknown wire name must be denied");
    let ToolAuthorizationError::UnknownTool { ref wire_name } = err else {
        panic!("expected UnknownTool, got: {err:?}");
    };
    assert!(
        wire_name.len() <= 256,
        "denial must bound hostile wire names, got {} bytes",
        wire_name.len()
    );
    assert!(
        err.to_string().len() < 512,
        "denial text must be static/bounded metadata"
    );
}
