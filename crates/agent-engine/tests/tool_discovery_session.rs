//! Task 15 — `DiscoveryIndex` and `SessionToolSet` (spec §7.1, §4.2).
//!
//! Discovery is a bounded, local, pure projection of catalog compact
//! descriptors: strict result-count and serialized-byte budgets, never full
//! schemas, never implementation acquisition, deterministic ordering.
//! `SessionToolSet` holds the configured core set plus exact activated
//! deferred tools for one session; new sessions start with zero activations
//! and grants validate exactly (session, tool, generation, digest).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agent_engine::tools::activation::{
    ActivationError, RuntimeLease, SessionId, SessionIdError, SessionToolSet, SessionToolSetError,
};
use agent_engine::tools::catalog::{
    CapabilityRecord, CapabilitySource, DiscoveryIndex, DiscoveryQuery, DiscoveryQueryError,
    SchemaLocator, SearchLimits, SearchLimitsError, SessionActivationGrant, ToolCatalog, ToolId,
    TrustProvenance, QUERY_MAX_BYTES, SEARCH_MAX_RESULTS_CAP, SEARCH_MAX_RESULT_BYTES_CAP,
};
use agent_engine::tools::{Tool, ToolContext};
use agent_engine::{Result, Value};
use async_trait::async_trait;

// ── Fixtures ────────────────────────────────────────────────────────────────

/// Marker that must never leak out of the full schema into discovery output.
const SCHEMA_SECRET: &str = "FULL_SCHEMA_SECRET_MARKER_7f3a";

struct FixtureTool;

#[async_trait]
impl Tool for FixtureTool {
    fn name(&self) -> &str {
        "fixture"
    }
    fn description(&self) -> &str {
        "fixture tool"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
        panic!("fixture tool must never execute during discovery/session tests");
    }
}

/// A capability whose factory counts constructions — discovery and session
/// bookkeeping must never invoke it.
fn spy_record(
    id: &str,
    summary: &str,
    tags: Vec<String>,
    spy: Arc<AtomicUsize>,
) -> CapabilityRecord {
    let factory = move || -> Arc<dyn Tool> {
        spy.fetch_add(1, Ordering::SeqCst);
        Arc::new(FixtureTool)
    };
    CapabilityRecord::new(
        ToolId::parse(id).expect("fixture id is canonical"),
        CapabilitySource::Mcp {
            server_id: "server-1".to_string(),
            server_tool_name: id.rsplit(':').next().unwrap_or(id).to_string(),
        },
        summary,
        tags,
        SchemaLocator::Inline(serde_json::json!({
            "type": "object",
            "properties": {"secret": {"description": SCHEMA_SECRET}}
        })),
        Arc::new(factory),
        TrustProvenance::McpConfig {
            server_id: "server-1".to_string(),
        },
    )
}

/// Adversarial catalog: many entries with maximal-length multibyte summaries
/// and tags so descriptor text alone dwarfs modest byte budgets.
fn adversarial_catalog(spy: Arc<AtomicUsize>) -> ToolCatalog {
    let mut catalog = ToolCatalog::empty();
    // 'é' is 2 bytes: 400 chars = 800 bytes, beyond the 256-byte summary cap,
    // exercising the bounded-descriptor path too.
    let long_summary = format!("searchword {}", "é".repeat(400));
    for i in 0..20 {
        let record = spy_record(
            &format!("mcp.server-1:adversarial-{i:02}"),
            &long_summary,
            vec!["searchword-tag".repeat(20); 12],
            Arc::clone(&spy),
        );
        catalog.insert(record).expect("insert fixture record");
    }
    catalog
}

fn limits(max_results: usize, max_bytes: usize) -> SearchLimits {
    SearchLimits::new(max_results, max_bytes).expect("fixture limits are valid")
}

fn query(raw: &str) -> DiscoveryQuery {
    DiscoveryQuery::parse(raw).expect("fixture query is valid")
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

// ── DiscoveryIndex: budgets under adversarial descriptors ───────────────────

#[test]
fn search_respects_count_budget_under_adversarial_descriptors() {
    let spy = Arc::new(AtomicUsize::new(0));
    let catalog = adversarial_catalog(Arc::clone(&spy));
    let index = DiscoveryIndex::build(&catalog).expect("build index");

    let results = index.search(
        &query("searchword"),
        &limits(3, SEARCH_MAX_RESULT_BYTES_CAP),
    );
    assert_eq!(results.hits().len(), 3, "count budget must cap results");
    assert!(
        results.truncated(),
        "excess matches must be reported as truncated"
    );
    assert_eq!(
        spy.load(Ordering::SeqCst),
        0,
        "search must not construct implementations"
    );
}

#[test]
fn search_respects_serialized_byte_budget_under_adversarial_descriptors() {
    let spy = Arc::new(AtomicUsize::new(0));
    let catalog = adversarial_catalog(Arc::clone(&spy));
    let index = DiscoveryIndex::build(&catalog).expect("build index");

    let byte_budget = 2048usize;
    let results = index.search(
        &query("searchword"),
        &limits(SEARCH_MAX_RESULTS_CAP, byte_budget),
    );
    assert!(
        !results.hits().is_empty(),
        "budget fits at least one compact entry"
    );
    // The budget covers full serialized entries (metadata + JSON overhead),
    // not merely descriptor text.
    let total: usize = results
        .hits()
        .iter()
        .map(|hit| serde_json::to_vec(hit).expect("entry serializes").len())
        .sum();
    assert!(
        total <= byte_budget,
        "serialized results ({total} bytes) exceed byte budget ({byte_budget})"
    );
    assert!(
        results.truncated(),
        "matches beyond the byte budget must be reported"
    );
    // Returned strings are owned Rust `String`s — valid UTF-8 by type; verify
    // the bounded summaries survived multibyte truncation intact.
    for hit in results.hits() {
        assert!(hit.summary().is_char_boundary(hit.summary().len()));
    }
    assert_eq!(spy.load(Ordering::SeqCst), 0);
}

#[test]
fn search_single_entry_larger_than_budget_returns_bounded_empty_result() {
    let spy = Arc::new(AtomicUsize::new(0));
    let catalog = adversarial_catalog(Arc::clone(&spy));
    let index = DiscoveryIndex::build(&catalog).expect("build index");

    let results = index.search(&query("searchword"), &limits(SEARCH_MAX_RESULTS_CAP, 1));
    assert!(results.hits().is_empty(), "no entry fits one byte");
    assert!(results.truncated(), "dropped matches must be reported");
}

// ── DiscoveryIndex: never exposes schemas or implementations ────────────────

#[test]
fn search_results_never_contain_full_schema_material() {
    let spy = Arc::new(AtomicUsize::new(0));
    let catalog = adversarial_catalog(Arc::clone(&spy));
    let index = DiscoveryIndex::build(&catalog).expect("build index");

    let results = index.search(
        &query("searchword"),
        &limits(SEARCH_MAX_RESULTS_CAP, SEARCH_MAX_RESULT_BYTES_CAP),
    );
    assert!(!results.hits().is_empty());
    for hit in results.hits() {
        let serialized = serde_json::to_string(hit).expect("entry serializes");
        assert!(
            !serialized.contains(SCHEMA_SECRET),
            "discovery entry leaked full schema content: {serialized}"
        );
        assert!(
            !serialized.contains("properties"),
            "discovery entry leaked schema structure: {serialized}"
        );
        // Digest metadata is allowed; it is not the schema.
        assert!(!hit.schema_digest().as_hex().is_empty());
    }
    assert_eq!(
        spy.load(Ordering::SeqCst),
        0,
        "no implementation acquisition during search"
    );
}

// ── DiscoveryIndex: determinism and staleness ───────────────────────────────

#[test]
fn search_is_deterministic_and_ordered_by_tool_id() {
    let spy = Arc::new(AtomicUsize::new(0));
    let catalog = adversarial_catalog(Arc::clone(&spy));
    let index = DiscoveryIndex::build(&catalog).expect("build index");
    let lim = limits(10, SEARCH_MAX_RESULT_BYTES_CAP);

    let first: Vec<String> = index
        .search(&query("searchword"), &lim)
        .hits()
        .iter()
        .map(|hit| hit.id().to_string())
        .collect();
    let second: Vec<String> = index
        .search(&query("searchword"), &lim)
        .hits()
        .iter()
        .map(|hit| hit.id().to_string())
        .collect();
    assert_eq!(
        first, second,
        "identical query must yield identical results"
    );
    let mut sorted = first.clone();
    sorted.sort();
    assert_eq!(
        first, sorted,
        "results must be in deterministic ToolId order"
    );
}

#[test]
fn discovery_index_snapshot_detects_catalog_mutation() {
    let spy = Arc::new(AtomicUsize::new(0));
    let mut catalog = adversarial_catalog(Arc::clone(&spy));
    let index = DiscoveryIndex::build(&catalog).expect("build index");
    assert!(!index.is_stale(&catalog));
    assert_eq!(index.generation(), catalog.generation());

    catalog
        .insert(spy_record(
            "mcp.server-1:late-arrival",
            "late",
            vec![],
            Arc::clone(&spy),
        ))
        .expect("insert");
    assert!(
        index.is_stale(&catalog),
        "generation change must be detectable"
    );
}

// ── DiscoveryIndex: boundary parsing of query and limits ────────────────────

#[test]
fn discovery_query_rejects_empty_and_oversized_input_typed() {
    assert_eq!(
        DiscoveryQuery::parse("").unwrap_err(),
        DiscoveryQueryError::Empty
    );
    assert_eq!(
        DiscoveryQuery::parse("   \t").unwrap_err(),
        DiscoveryQueryError::Empty
    );
    let oversized = "q".repeat(QUERY_MAX_BYTES + 1);
    assert_eq!(
        DiscoveryQuery::parse(&oversized).unwrap_err(),
        DiscoveryQueryError::Oversized {
            actual: QUERY_MAX_BYTES + 1,
            limit: QUERY_MAX_BYTES
        }
    );
}

#[test]
fn search_limits_reject_zero_and_over_cap_values_typed() {
    assert_eq!(
        SearchLimits::new(0, 1024).unwrap_err(),
        SearchLimitsError::ZeroResults
    );
    assert_eq!(
        SearchLimits::new(4, 0).unwrap_err(),
        SearchLimitsError::ZeroBytes
    );
    assert_eq!(
        SearchLimits::new(SEARCH_MAX_RESULTS_CAP + 1, 1024).unwrap_err(),
        SearchLimitsError::ResultsOverCap {
            actual: SEARCH_MAX_RESULTS_CAP + 1,
            cap: SEARCH_MAX_RESULTS_CAP
        }
    );
    assert_eq!(
        SearchLimits::new(4, SEARCH_MAX_RESULT_BYTES_CAP + 1).unwrap_err(),
        SearchLimitsError::BytesOverCap {
            actual: SEARCH_MAX_RESULT_BYTES_CAP + 1,
            cap: SEARCH_MAX_RESULT_BYTES_CAP
        }
    );
}

// ── SessionId boundary parsing ──────────────────────────────────────────────

#[test]
fn session_id_rejects_empty_and_oversized_typed() {
    assert_eq!(SessionId::parse("").unwrap_err(), SessionIdError::Empty);
    let oversized = "s".repeat(257);
    assert!(matches!(
        SessionId::parse(&oversized).unwrap_err(),
        SessionIdError::Oversized { actual: 257, .. }
    ));
    let ok = SessionId::parse("session-a").expect("valid session id");
    assert_eq!(ok.as_str(), "session-a");
}

// ── SessionToolSet: zero inheritance and exact grants ───────────────────────

fn session(raw: &str) -> SessionId {
    SessionId::parse(raw).expect("fixture session id is valid")
}

#[test]
fn new_sessions_start_with_zero_activations_and_do_not_inherit() {
    let spy = Arc::new(AtomicUsize::new(0));
    let catalog = adversarial_catalog(Arc::clone(&spy));
    let core = vec![ToolId::parse("mcp.server-1:adversarial-00").expect("core id")];
    let tool = ToolId::parse("mcp.server-1:adversarial-05").expect("tool id");

    let session_a = session("session-a");
    let mut set_a =
        SessionToolSet::new(session_a.clone(), core.clone(), &catalog).expect("build set a");
    assert_eq!(set_a.activated().count(), 0, "new session must start empty");

    let grant = grant_for(&session_a, &catalog, &tool);
    set_a
        .activate(grant, &catalog)
        .expect("exact grant activates");
    assert_eq!(set_a.activated().count(), 1);
    assert!(set_a.activation(&tool).is_some());

    // A fresh session for the same catalog sees nothing from session A.
    let set_b = SessionToolSet::new(session("session-b"), core, &catalog).expect("build set b");
    assert_eq!(
        set_b.activated().count(),
        0,
        "session B must not inherit from session A"
    );
    assert!(set_b.activation(&tool).is_none());
    assert_eq!(
        spy.load(Ordering::SeqCst),
        0,
        "activation bookkeeping must not construct tools"
    );
}

#[test]
fn core_and_activated_projections_are_deterministic_and_exact() {
    let spy = Arc::new(AtomicUsize::new(0));
    let catalog = adversarial_catalog(Arc::clone(&spy));
    // Core ids intentionally unsorted at configuration time.
    let core = vec![
        ToolId::parse("mcp.server-1:adversarial-03").expect("id"),
        ToolId::parse("mcp.server-1:adversarial-01").expect("id"),
        ToolId::parse("mcp.server-1:adversarial-02").expect("id"),
    ];
    let sid = session("session-proj");
    let mut set = SessionToolSet::new(sid.clone(), core, &catalog).expect("build set");

    let core_ids: Vec<&str> = set.core_ids().map(ToolId::as_str).collect();
    assert_eq!(
        core_ids,
        vec![
            "mcp.server-1:adversarial-01",
            "mcp.server-1:adversarial-02",
            "mcp.server-1:adversarial-03",
        ],
        "core projection must be deterministic"
    );

    for raw in ["mcp.server-1:adversarial-09", "mcp.server-1:adversarial-07"] {
        let id = ToolId::parse(raw).expect("id");
        set.activate(grant_for(&sid, &catalog, &id), &catalog)
            .expect("activate");
    }
    let activated: Vec<&str> = set
        .activated()
        .map(|a| a.grant().tool_id().as_str())
        .collect();
    assert_eq!(
        activated,
        vec!["mcp.server-1:adversarial-07", "mcp.server-1:adversarial-09"],
        "activated projection must be deterministic and exact"
    );
    for entry in set.activated() {
        assert_eq!(
            entry.lease(),
            RuntimeLease::NotAcquired,
            "lease is placeholder metadata"
        );
    }
}

#[test]
fn session_tool_set_rejects_unknown_core_tool_typed() {
    let spy = Arc::new(AtomicUsize::new(0));
    let catalog = adversarial_catalog(spy);
    let missing = ToolId::parse("mcp.server-1:not-in-catalog").expect("id");
    let err = SessionToolSet::new(session("session-x"), vec![missing.clone()], &catalog)
        .expect_err("unknown core tool must fail typed");
    assert_eq!(err, SessionToolSetError::UnknownCoreTool(missing));
}

#[test]
fn activation_rejects_mismatched_grants_without_partial_mutation() {
    let spy = Arc::new(AtomicUsize::new(0));
    let mut catalog = adversarial_catalog(Arc::clone(&spy));
    let sid = session("session-a");
    let tool = ToolId::parse("mcp.server-1:adversarial-04").expect("id");
    let mut set = SessionToolSet::new(sid.clone(), vec![], &catalog).expect("build set");

    // 1. Grant issued for another session.
    let foreign = grant_for(&session("session-other"), &catalog, &tool);
    assert!(matches!(
        set.activate(foreign, &catalog).unwrap_err(),
        ActivationError::SessionMismatch { .. }
    ));
    assert_eq!(
        set.activated().count(),
        0,
        "failed activation must not mutate the set"
    );

    // 2. Grant for a tool the catalog does not know.
    let ghost_id = ToolId::parse("mcp.server-1:ghost").expect("id");
    let record = catalog.get(&tool).expect("record");
    let ghost = SessionActivationGrant::new(
        sid.as_str(),
        ghost_id.clone(),
        catalog.generation(),
        record.schema_digest().clone(),
    )
    .expect("grant");
    assert!(matches!(
        set.activate(ghost, &catalog).unwrap_err(),
        ActivationError::UnknownTool(id) if id == ghost_id
    ));
    assert_eq!(set.activated().count(), 0);

    // 3. Wrong schema digest for the right tool.
    let wrong_digest = agent_engine::tools::catalog::SchemaDigest::of_schema(
        &serde_json::json!({"type": "object", "properties": {"other": {}}}),
    );
    let mismatched = SessionActivationGrant::new(
        sid.as_str(),
        tool.clone(),
        catalog.generation(),
        wrong_digest,
    )
    .expect("grant");
    assert!(matches!(
        set.activate(mismatched, &catalog).unwrap_err(),
        ActivationError::DigestMismatch(id) if id == tool
    ));
    assert_eq!(set.activated().count(), 0);

    // 4. Stale generation: grant minted, then the catalog mutates.
    let stale = grant_for(&sid, &catalog, &tool);
    catalog
        .insert(spy_record(
            "mcp.server-1:mutation",
            "mutation",
            vec![],
            Arc::clone(&spy),
        ))
        .expect("insert");
    assert!(matches!(
        set.activate(stale, &catalog).unwrap_err(),
        ActivationError::StaleGeneration { .. }
    ));
    assert_eq!(set.activated().count(), 0);

    // The set itself is now a stale snapshot and reports it.
    assert!(
        set.is_stale(&catalog),
        "set must expose catalog drift for rebuild"
    );
    assert_eq!(
        spy.load(Ordering::SeqCst),
        0,
        "rejections must not construct implementations"
    );
}

#[test]
fn activation_rejects_duplicate_exact_activation_typed() {
    let spy = Arc::new(AtomicUsize::new(0));
    let catalog = adversarial_catalog(spy);
    let sid = session("session-a");
    let tool = ToolId::parse("mcp.server-1:adversarial-06").expect("id");
    let mut set = SessionToolSet::new(sid.clone(), vec![], &catalog).expect("build set");

    set.activate(grant_for(&sid, &catalog, &tool), &catalog)
        .expect("first activation");
    let err = set
        .activate(grant_for(&sid, &catalog, &tool), &catalog)
        .expect_err("duplicate activation must be typed, not silent");
    assert!(matches!(err, ActivationError::AlreadyActivated(id) if id == tool));
    assert_eq!(
        set.activated().count(),
        1,
        "duplicate rejection must leave the set unchanged"
    );
}
