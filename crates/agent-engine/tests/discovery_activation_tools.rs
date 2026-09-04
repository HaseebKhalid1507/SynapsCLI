//! Task 17 — model-facing discovery/activation tools and deterministic bulk
//! updates (spec §4.2, §7.2, §7.3, §7.7).
//!
//! Covers:
//! - `search_tools`: bounded, deterministic, descriptor-only output (stable
//!   `ToolId`s, no full schemas, no factories, no source paths), typed
//!   failures for empty/malformed/oversized input;
//! - `activate_tools`: model-initiated activation requires host-supplied
//!   typed confirmation authority (never a model-authored JSON parameter),
//!   activates the exact requested identity only (siblings stay denied),
//!   and rejects source-wide/provider-wide strings and unknown ids;
//! - explicit user-requested host API activation without a redundant prompt;
//! - `activate_many`: atomic (any bad id leaves the set byte-stable), stable
//!   ToolId apply order, exactly one session schema-generation advance per
//!   nonempty batch, zero-batch no-op semantics;
//! - catalog generation drift invalidates activation and projection through
//!   the public surface;
//! - the retained shared session set: `activate_tools` mutates the SAME set
//!   the `ExecutionGate` and the extension-provider route consume.

use std::sync::{Arc, RwLock};

use agent_engine::tools::activation::{
    activate_exact_for_user, issue_exact_grant, route_session_set, ActivationAuthority,
    ActivationError, BulkActivationError, ExecutionGate, GrantIssuanceError, HostActivationError,
    SessionId, SessionToolSet, SharedSessionToolSet, ToolAuthorizationError,
};
use agent_engine::tools::catalog::{SessionActivationGrant, ToolId};
use agent_engine::tools::discovery::{ActivateToolsTool, ActivationCapability, SearchToolsTool};
use agent_engine::tools::{
    Tool, ToolCapabilities, ToolChannels, ToolContext, ToolLimits, ToolOrigin, ToolRegistry,
};
use agent_engine::{Result, Value};
use async_trait::async_trait;
use serde_json::json;

// ── Fixtures ────────────────────────────────────────────────────────────────

/// Builtin-origin fixture tool with a schema carrying a secret marker so
/// tests can prove discovery output never embeds full schemas.
struct FixtureTool {
    name: String,
    origin: ToolOrigin,
}

impl FixtureTool {
    fn builtin(name: &str) -> Self {
        Self {
            name: name.to_string(),
            origin: ToolOrigin::Builtin,
        }
    }

    fn unknown(name: &str) -> Self {
        Self {
            name: name.to_string(),
            origin: ToolOrigin::Unknown,
        }
    }
}

const SCHEMA_MARKER: &str = "very_secret_full_schema_marker_property";

#[async_trait]
impl Tool for FixtureTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "task 17 discovery fixture tool"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { SCHEMA_MARKER: {"type": "string"} }
        })
    }
    fn origin(&self) -> ToolOrigin {
        self.origin.clone()
    }
    async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
        Ok("fixture executed".to_string())
    }
}

fn session_id() -> SessionId {
    SessionId::parse("task17-session").expect("valid session id")
}

/// Registry with three builtin fixture tools: `alpha_tool` (core),
/// `beta_tool` and `gamma_tool` (deferred).
fn fixture_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::empty();
    registry.register(Arc::new(FixtureTool::builtin("alpha_tool")));
    registry.register(Arc::new(FixtureTool::builtin("beta_tool")));
    registry.register(Arc::new(FixtureTool::builtin("gamma_tool")));
    registry
}

fn minimal_set(registry: &ToolRegistry) -> SessionToolSet {
    SessionToolSet::new(
        session_id(),
        [ToolId::builtin("alpha_tool")],
        registry.catalog(),
    )
    .expect("core id resolves")
}

fn shared(set: SessionToolSet) -> SharedSessionToolSet {
    Arc::new(RwLock::new(set))
}

fn ctx(cap: Option<ActivationCapability>) -> ToolContext {
    ctx_with_prompt(cap, None)
}

fn ctx_with_prompt(
    cap: Option<ActivationCapability>,
    secret_prompt: Option<agent_engine::tools::SecretPromptHandle>,
) -> ToolContext {
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
            secret_prompt,
            orchestration: None,
            tool_activation: cap,
            mcp_leases: None,
            extension_leases: None,
            memory_context: None,
            cwd: None,
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

fn capability(
    registry: &ToolRegistry,
    set: &SharedSessionToolSet,
    authority: ActivationAuthority,
) -> ActivationCapability {
    ActivationCapability::new(registry.catalog().clone(), Arc::clone(set), authority)
}

/// Snapshot of externally observable session-set state, for byte-stable
/// no-partial-mutation assertions.
fn set_fingerprint(set: &SessionToolSet) -> (u64, Vec<String>, Vec<String>) {
    (
        set.schema_generation(),
        set.core_ids().map(|id| id.as_str().to_string()).collect(),
        set.activated()
            .map(|a| a.grant().tool_id().as_str().to_string())
            .collect(),
    )
}

// ── search_tools ────────────────────────────────────────────────────────────

#[tokio::test]
async fn search_tools_is_bounded_deterministic_and_descriptor_only() {
    let registry = fixture_registry();
    let set = shared(minimal_set(&registry));

    let first = SearchToolsTool
        .execute(
            json!({"query": "tool"}),
            ctx(Some(capability(
                &registry,
                &set,
                ActivationAuthority::Unauthorized,
            ))),
        )
        .await
        .expect("search succeeds");
    let second = SearchToolsTool
        .execute(
            json!({"query": "tool"}),
            ctx(Some(capability(
                &registry,
                &set,
                ActivationAuthority::Unauthorized,
            ))),
        )
        .await
        .expect("search succeeds");

    // Deterministic byte-for-byte across identical runs.
    assert_eq!(first, second);
    // Stable ToolIds present, in deterministic ToolId order.
    let alpha = first.find("builtin:alpha_tool").expect("alpha id present");
    let beta = first.find("builtin:beta_tool").expect("beta id present");
    let gamma = first.find("builtin:gamma_tool").expect("gamma id present");
    assert!(alpha < beta && beta < gamma, "ids in ToolId order");
    // Descriptor-only: no full schema bytes leak into discovery output.
    assert!(
        !first.contains(SCHEMA_MARKER),
        "full schema must not appear in search output: {first}"
    );
    assert!(!first.contains("input_schema"));
    // Bounded: descriptor budget plus a small fixed envelope.
    assert!(
        first.len() <= 10_000,
        "search output must stay bounded, got {} bytes",
        first.len()
    );
}

#[tokio::test]
async fn search_tools_fails_typed_on_empty_malformed_oversized_and_missing_context() {
    let registry = fixture_registry();
    let set = shared(minimal_set(&registry));
    let cap = || {
        Some(capability(
            &registry,
            &set,
            ActivationAuthority::Unauthorized,
        ))
    };

    let empty = SearchToolsTool
        .execute(json!({"query": "   "}), ctx(cap()))
        .await
        .expect_err("empty query fails");
    assert!(empty.to_string().contains("empty"), "{empty}");

    let oversized = SearchToolsTool
        .execute(json!({"query": "q".repeat(4096)}), ctx(cap()))
        .await
        .expect_err("oversized query fails");
    assert!(oversized.to_string().contains("oversized"), "{oversized}");
    // The hostile oversized query must not be echoed back verbatim.
    assert!(oversized.to_string().len() < 512);

    let control = SearchToolsTool
        .execute(json!({"query": "a\u{1b}[31m"}), ctx(cap()))
        .await
        .expect_err("control characters fail");
    assert!(control.to_string().contains("control"), "{control}");

    let missing_param = SearchToolsTool
        .execute(json!({}), ctx(cap()))
        .await
        .expect_err("missing query fails");
    assert!(
        missing_param.to_string().contains("query"),
        "{missing_param}"
    );

    let no_context = SearchToolsTool
        .execute(json!({"query": "tool"}), ctx(None))
        .await
        .expect_err("missing capability context fails");
    assert!(
        no_context.to_string().contains("not available"),
        "{no_context}"
    );
}

// ── activate_tools (model-initiated) ────────────────────────────────────────

#[tokio::test]
async fn activate_tools_denied_without_host_confirmation_authority() {
    let registry = fixture_registry();
    let set = shared(minimal_set(&registry));
    let before = set_fingerprint(&set.read().unwrap());

    // Unauthorized context: denial happens before any grant/set mutation,
    // and a model-authored "confirmed" JSON parameter must not self-approve.
    let err = ActivateToolsTool
        .execute(
            json!({"tools": ["builtin:beta_tool"], "confirmed": true}),
            ctx(Some(capability(
                &registry,
                &set,
                ActivationAuthority::Unauthorized,
            ))),
        )
        .await
        .expect_err("unconfirmed model activation must fail");
    assert!(err.to_string().contains("confirmation"), "{err}");

    let after = set_fingerprint(&set.read().unwrap());
    assert_eq!(before, after, "denied activation must not mutate the set");

    // The gate still denies the sibling: nothing was activated.
    let denial = ExecutionGate::authorize_wire_call(&registry, &set.read().unwrap(), "beta_tool")
        .expect_err("beta_tool stays unactivated");
    assert!(matches!(
        denial,
        ToolAuthorizationError::NotActivated(ref id) if id.as_str() == "builtin:beta_tool"
    ));
}

#[tokio::test]
async fn activate_tools_confirmed_exact_activation_leaves_siblings_denied() {
    let registry = fixture_registry();
    let set = shared(minimal_set(&registry));

    let out = ActivateToolsTool
        .execute(
            json!({"tools": ["builtin:beta_tool"]}),
            ctx(Some(capability(
                &registry,
                &set,
                ActivationAuthority::ModelConfirmed,
            ))),
        )
        .await
        .expect("confirmed exact activation succeeds");
    assert!(out.contains("builtin:beta_tool"), "{out}");

    {
        let guard = set.read().unwrap();
        assert_eq!(guard.schema_generation(), 1, "exactly one schema update");
        // The retained set was mutated: the gate authorizes the exact tool…
        ExecutionGate::authorize_wire_call(&registry, &guard, "beta_tool")
            .expect("activated tool authorizes");
        // …and ONLY that tool: the sibling stays denied.
        let denial = ExecutionGate::authorize_wire_call(&registry, &guard, "gamma_tool")
            .expect_err("sibling stays denied");
        assert!(matches!(denial, ToolAuthorizationError::NotActivated(_)));
    }

    // The session projection now contains the activated schema; the sibling
    // remains absent (also covered in session_schema_projection.rs).
    let projection = registry.session_tools_schema(&set.read().unwrap()).schema;
    let names: Vec<&str> = projection
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(names.contains(&"beta_tool"));
    assert!(!names.contains(&"gamma_tool"));
}

#[tokio::test]
async fn activate_tools_rejects_source_wide_unknown_and_malformed_ids() {
    let registry = fixture_registry();
    let set = shared(minimal_set(&registry));

    for hostile in [
        "builtin",           // whole-namespace / source-wide request
        "builtin:*",         // wildcard sibling broadening
        "ext.some-plugin:*", // provider/source-wide extension request
        "mcp.server:",       // empty name segment
        "builtin:nope_tool", // unknown exact id
        "",                  // empty
    ] {
        let before = set_fingerprint(&set.read().unwrap());
        let err = ActivateToolsTool
            .execute(
                json!({"tools": [hostile]}),
                ctx(Some(capability(
                    &registry,
                    &set,
                    ActivationAuthority::ModelConfirmed,
                ))),
            )
            .await
            .expect_err("hostile id must be rejected");
        let _ = err;
        let after = set_fingerprint(&set.read().unwrap());
        assert_eq!(before, after, "rejected id {hostile:?} must not mutate");
    }

    // Empty batch and oversized batch fail typed too.
    let empty = ActivateToolsTool
        .execute(
            json!({"tools": []}),
            ctx(Some(capability(
                &registry,
                &set,
                ActivationAuthority::ModelConfirmed,
            ))),
        )
        .await
        .expect_err("empty batch fails typed");
    assert!(empty.to_string().contains("no tool"), "{empty}");
}

// ── activate_tools (interactive host confirmation) ──────────────────────────

/// Fake interactive host: answers every secret-prompt request with `answer`
/// and records the prompt text it was shown.
fn fake_prompt(
    answer: Option<&'static str>,
) -> (
    agent_engine::tools::SecretPromptHandle,
    Arc<std::sync::Mutex<Vec<String>>>,
) {
    let (tx, mut rx) =
        tokio::sync::mpsc::unbounded_channel::<agent_engine::tools::SecretPromptRequest>();
    let handle = agent_engine::tools::SecretPromptHandle::new(tx);
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_writer = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            seen_writer
                .lock()
                .unwrap()
                .push(format!("{}\n{}", req.title, req.prompt));
            let _ = req.response_tx.send(answer.map(str::to_string));
        }
    });
    (handle, seen)
}

#[tokio::test]
async fn activate_tools_host_prompt_yes_activates_exact_tool_only() {
    let registry = fixture_registry();
    let set = shared(minimal_set(&registry));
    let (prompt, seen) = fake_prompt(Some("yes"));

    let out = ActivateToolsTool
        .execute(
            json!({"tools": ["builtin:beta_tool"]}),
            ctx_with_prompt(
                Some(capability(
                    &registry,
                    &set,
                    ActivationAuthority::Unauthorized,
                )),
                Some(prompt),
            ),
        )
        .await
        .expect("host-confirmed activation succeeds");

    // The host was shown the bounded EXACT id list before authorization.
    let prompts = seen.lock().unwrap().clone();
    assert_eq!(prompts.len(), 1, "exactly one confirmation prompt");
    assert!(prompts[0].contains("builtin:beta_tool"), "{}", prompts[0]);

    // Exact tool activated; sibling stays denied.
    let guard = set.read().unwrap();
    assert_eq!(guard.schema_generation(), 1);
    ExecutionGate::authorize_wire_call(&registry, &guard, "beta_tool")
        .expect("activated tool authorizes");
    let denial = ExecutionGate::authorize_wire_call(&registry, &guard, "gamma_tool")
        .expect_err("sibling stays denied");
    assert!(matches!(denial, ToolAuthorizationError::NotActivated(_)));

    // The reported schema_generation is the completed batch's generation.
    let body: serde_json::Value = serde_json::from_str(&out).expect("json result");
    assert_eq!(body["schema_generation"], json!(guard.schema_generation()));
}

#[tokio::test]
async fn activate_tools_host_prompt_no_or_absent_denies_without_mutation() {
    let registry = fixture_registry();
    let set = shared(minimal_set(&registry));
    let before = set_fingerprint(&set.read().unwrap());

    // Host answers "no": denied, zero mutation.
    let (deny_prompt, _) = fake_prompt(Some("no"));
    let err = ActivateToolsTool
        .execute(
            json!({"tools": ["builtin:beta_tool"]}),
            ctx_with_prompt(
                Some(capability(
                    &registry,
                    &set,
                    ActivationAuthority::Unauthorized,
                )),
                Some(deny_prompt),
            ),
        )
        .await
        .expect_err("host denial fails closed");
    assert!(err.to_string().contains("confirmation"), "{err}");
    assert_eq!(set_fingerprint(&set.read().unwrap()), before);

    // Host cancels (None response): denied, zero mutation.
    let (cancel_prompt, _) = fake_prompt(None);
    let err = ActivateToolsTool
        .execute(
            json!({"tools": ["builtin:beta_tool"]}),
            ctx_with_prompt(
                Some(capability(
                    &registry,
                    &set,
                    ActivationAuthority::Unauthorized,
                )),
                Some(cancel_prompt),
            ),
        )
        .await
        .expect_err("host cancel fails closed");
    assert!(err.to_string().contains("confirmation"), "{err}");
    assert_eq!(set_fingerprint(&set.read().unwrap()), before);

    // Model-authored `confirmed` flag with NO host prompt available: the
    // model JSON can never substitute for host authority.
    let err = ActivateToolsTool
        .execute(
            json!({"tools": ["builtin:beta_tool"], "confirmed": true}),
            ctx_with_prompt(
                Some(capability(
                    &registry,
                    &set,
                    ActivationAuthority::Unauthorized,
                )),
                None,
            ),
        )
        .await
        .expect_err("model-authored flag cannot bypass absent prompt");
    assert!(err.to_string().contains("confirmation"), "{err}");
    assert_eq!(set_fingerprint(&set.read().unwrap()), before);
}

#[tokio::test]
async fn activate_tools_reports_generation_of_the_completed_batch() {
    let registry = fixture_registry();
    let set = shared(minimal_set(&registry));

    // First batch (nonconcurrent): reported generation == set generation.
    let first = ActivateToolsTool
        .execute(
            json!({"tools": ["builtin:beta_tool"]}),
            ctx(Some(capability(
                &registry,
                &set,
                ActivationAuthority::ModelConfirmed,
            ))),
        )
        .await
        .expect("first batch succeeds");
    let first: serde_json::Value = serde_json::from_str(&first).expect("json");
    assert_eq!(first["schema_generation"], json!(1));
    assert_eq!(set.read().unwrap().schema_generation(), 1);

    // Second batch advances by exactly one and reports its own generation.
    let second = ActivateToolsTool
        .execute(
            json!({"tools": ["builtin:gamma_tool"]}),
            ctx(Some(capability(
                &registry,
                &set,
                ActivationAuthority::ModelConfirmed,
            ))),
        )
        .await
        .expect("second batch succeeds");
    let second: serde_json::Value = serde_json::from_str(&second).expect("json");
    assert_eq!(second["schema_generation"], json!(2));
    assert_eq!(set.read().unwrap().schema_generation(), 2);
}

// ── Host APIs ───────────────────────────────────────────────────────────────

#[test]
fn user_requested_host_api_activates_exact_identity_without_prompt() {
    let registry = fixture_registry();
    let mut set = minimal_set(&registry);

    // Explicit user request for a known exact ToolId: host authorizes that
    // exact identity with no confirmation authority involved (PR #63
    // ergonomics). This is a host Rust API, not a model-reachable boolean.
    activate_exact_for_user(&mut set, registry.catalog(), &ToolId::builtin("beta_tool"))
        .expect("user-requested exact activation succeeds");

    ExecutionGate::authorize_wire_call(&registry, &set, "beta_tool").expect("authorized");
    let denial = ExecutionGate::authorize_wire_call(&registry, &set, "gamma_tool")
        .expect_err("sibling stays denied");
    assert!(matches!(denial, ToolAuthorizationError::NotActivated(_)));
    assert_eq!(set.schema_generation(), 1);
}

#[test]
fn issue_exact_grant_rejects_unknown_and_untrusted() {
    let mut registry = fixture_registry();
    registry.register(Arc::new(FixtureTool::unknown("shady_tool")));
    let session = session_id();

    let unknown = issue_exact_grant(
        registry.catalog(),
        &session,
        &ToolId::builtin("not_cataloged"),
    )
    .expect_err("unknown id fails");
    assert!(matches!(unknown, GrantIssuanceError::UnknownTool(_)));

    let untrusted = issue_exact_grant(
        registry.catalog(),
        &session,
        &ToolId::unclassified("shady_tool"),
    )
    .expect_err("unverified provenance fails");
    assert!(matches!(untrusted, GrantIssuanceError::UntrustedSource(_)));
}

// ── activate_many ───────────────────────────────────────────────────────────

fn grant_for(registry: &ToolRegistry, name: &str) -> SessionActivationGrant {
    issue_exact_grant(registry.catalog(), &session_id(), &ToolId::builtin(name))
        .expect("grant issues for known trusted tool")
}

#[test]
fn activate_many_applies_in_stable_order_with_one_generation_advance() {
    let registry = fixture_registry();
    let mut set = minimal_set(&registry);

    // Request out of order; apply order and projection are ToolId-stable.
    let activated = set
        .activate_many(
            vec![
                grant_for(&registry, "gamma_tool"),
                grant_for(&registry, "beta_tool"),
            ],
            registry.catalog(),
        )
        .expect("bulk activation succeeds");
    assert_eq!(activated, 2);
    assert_eq!(
        set.schema_generation(),
        1,
        "exactly ONE schema-generation advance for a nonempty batch"
    );
    let ids: Vec<&str> = set
        .activated()
        .map(|a| a.grant().tool_id().as_str())
        .collect();
    assert_eq!(ids, vec!["builtin:beta_tool", "builtin:gamma_tool"]);

    // Empty batch: no-op, no generation advance.
    let mut fresh = minimal_set(&registry);
    assert_eq!(
        fresh
            .activate_many(Vec::new(), registry.catalog())
            .expect("empty batch is a no-op"),
        0
    );
    assert_eq!(fresh.schema_generation(), 0);
}

#[test]
fn activate_many_is_atomic_on_any_invalid_entry() {
    let registry = fixture_registry();
    let catalog = registry.catalog();

    // Duplicate id in one batch.
    let mut set = minimal_set(&registry);
    let before = set_fingerprint(&set);
    let err = set
        .activate_many(
            vec![
                grant_for(&registry, "beta_tool"),
                grant_for(&registry, "beta_tool"),
            ],
            catalog,
        )
        .expect_err("duplicate request fails");
    assert!(matches!(err, BulkActivationError::DuplicateRequest(_)));
    assert_eq!(set_fingerprint(&set), before, "no partial mutation");

    // Unknown tool id (grant forged for an uncataloged identity).
    let mut set = minimal_set(&registry);
    let sample = grant_for(&registry, "beta_tool");
    let unknown_grant = SessionActivationGrant::new(
        session_id().as_str(),
        ToolId::builtin("not_cataloged"),
        catalog.generation(),
        sample.schema_digest().clone(),
    )
    .expect("grant constructs");
    let err = set
        .activate_many(
            vec![grant_for(&registry, "gamma_tool"), unknown_grant],
            catalog,
        )
        .expect_err("unknown id fails whole batch");
    assert!(matches!(
        err,
        BulkActivationError::Grant(ActivationError::UnknownTool(_))
    ));
    assert_eq!(set_fingerprint(&set), before);

    // Core tool re-activation.
    let mut set = minimal_set(&registry);
    let err = set
        .activate_many(
            vec![
                grant_for(&registry, "alpha_tool"),
                grant_for(&registry, "beta_tool"),
            ],
            catalog,
        )
        .expect_err("core id fails whole batch");
    assert!(matches!(
        err,
        BulkActivationError::Grant(ActivationError::AlreadyCore(_))
    ));
    assert_eq!(set_fingerprint(&set), before);

    // Stale generation grant.
    let mut set = minimal_set(&registry);
    let stale = SessionActivationGrant::new(
        session_id().as_str(),
        ToolId::builtin("beta_tool"),
        agent_engine::tools::catalog::CatalogGeneration::initial(),
        sample.schema_digest().clone(),
    )
    .expect("grant constructs");
    let err = set
        .activate_many(vec![stale, grant_for(&registry, "gamma_tool")], catalog)
        .expect_err("stale generation fails whole batch");
    assert!(matches!(
        err,
        BulkActivationError::Grant(ActivationError::StaleGeneration { .. })
    ));
    assert_eq!(set_fingerprint(&set), before);

    // Digest mismatch grant.
    let mut set = minimal_set(&registry);
    let mismatched = SessionActivationGrant::new(
        session_id().as_str(),
        ToolId::builtin("beta_tool"),
        catalog.generation(),
        agent_engine::tools::catalog::SchemaDigest::of_schema(&json!({"other": true})),
    )
    .expect("grant constructs");
    let err = set
        .activate_many(
            vec![mismatched, grant_for(&registry, "gamma_tool")],
            catalog,
        )
        .expect_err("digest mismatch fails whole batch");
    assert!(matches!(
        err,
        BulkActivationError::Grant(ActivationError::DigestMismatch(_))
    ));
    assert_eq!(set_fingerprint(&set), before);
}

#[test]
fn activate_many_rejects_untrusted_source() {
    let mut registry = fixture_registry();
    registry.register(Arc::new(FixtureTool::unknown("shady_tool")));
    let catalog = registry.catalog();
    let mut set = SessionToolSet::new(session_id(), [ToolId::builtin("alpha_tool")], catalog)
        .expect("core resolves");
    let before = set_fingerprint(&set);

    let shady_id = ToolId::unclassified("shady_tool");
    let record_digest = catalog
        .get(&shady_id)
        .expect("shady tool cataloged")
        .schema_digest()
        .clone();
    let shady_grant = SessionActivationGrant::new(
        session_id().as_str(),
        shady_id,
        catalog.generation(),
        record_digest,
    )
    .expect("grant constructs");

    let err = set
        .activate_many(
            vec![
                issue_exact_grant(catalog, &session_id(), &ToolId::builtin("beta_tool"))
                    .expect("trusted grant issues"),
                shady_grant,
            ],
            catalog,
        )
        .expect_err("untrusted source fails whole batch");
    assert!(matches!(err, BulkActivationError::UntrustedSource(_)));
    assert_eq!(set_fingerprint(&set), before);
}

// ── Drift invalidation through the public surface ───────────────────────────

#[test]
fn catalog_generation_drift_keeps_unchanged_tools_but_blocks_new_grants() {
    let mut registry = fixture_registry();
    let mut set = minimal_set(&registry);
    activate_exact_for_user(&mut set, registry.catalog(), &ToolId::builtin("beta_tool"))
        .expect("activation succeeds");
    ExecutionGate::authorize_wire_call(&registry, &set, "beta_tool").expect("authorized");

    // Catalog mutation (dynamic registration) advances the generation.
    registry.register(Arc::new(FixtureTool::builtin("late_tool")));

    // Per-tool digest validation: the activated tool's current record is
    // schema-identical, so execution and projection SURVIVE the unrelated
    // drift (wholesale generation denial would kill the in-flight round).
    ExecutionGate::authorize_wire_call(&registry, &set, "beta_tool")
        .expect("unchanged activated tool survives unrelated drift");
    let report = registry.session_tools_schema(&set);
    assert!(
        report.dropped.is_empty(),
        "projection survives unrelated drift, dropping nothing"
    );

    // Grant issuance/activation stays generation-STRICT: fresh grants
    // against the old snapshot cannot extend a drifted set.
    let err = activate_exact_for_user(&mut set, registry.catalog(), &ToolId::builtin("gamma_tool"))
        .expect_err("stale snapshot denies further activation");
    assert!(matches!(
        err,
        HostActivationError::Bulk(BulkActivationError::Grant(
            ActivationError::StaleSnapshot { .. }
        ))
    ));
}

// ── Extension-provider route threading ──────────────────────────────────────

#[test]
fn route_session_set_consumes_retained_set_and_survives_drift() {
    let mut registry = fixture_registry();
    let set = shared(minimal_set(&registry));
    activate_exact_for_user(
        &mut set.write().unwrap(),
        registry.catalog(),
        &ToolId::builtin("beta_tool"),
    )
    .expect("activation succeeds");

    // Retained + fresh: the route receives the SAME set state/generation,
    // including the exact activation — not a fresh default-core mint.
    let routed = route_session_set(Some(&set), registry.catalog(), session_id);
    assert_eq!(
        routed.catalog_generation(),
        set.read().unwrap().catalog_generation()
    );
    assert!(routed.activation(&ToolId::builtin("beta_tool")).is_some());
    ExecutionGate::authorize_wire_call(&registry, &routed, "beta_tool").expect("authorized");

    // Retained + drifted: still served (never a fresh mid-round mint, never
    // a wholesale kill) — per-call gate authorization is what protects
    // execution, and it still passes for the schema-identical tool.
    registry.register(Arc::new(FixtureTool::builtin("late_tool")));
    let routed = route_session_set(Some(&set), registry.catalog(), session_id);
    assert!(routed.is_stale(registry.catalog()));
    assert!(routed.activation(&ToolId::builtin("beta_tool")).is_some());
    ExecutionGate::authorize_wire_call(&registry, &routed, "beta_tool")
        .expect("unchanged tool authorizes across drift");

    // No retained handle (internal callers): fail closed to a fresh
    // default-core set with zero activations, as before.
    let fallback = route_session_set(None, registry.catalog(), session_id);
    assert_eq!(fallback.activated().count(), 0);
    assert!(!fallback.is_stale(registry.catalog()));
}
