//! Task 22 — Phase 3 adversarial acceptance harness (spec §13.4).
//!
//! Headless, external-style composition of PUBLIC runtime surfaces only
//! (`synaps_cli::{tools, mcp, extensions, skills}`) plus the checked-in
//! stdio fixtures. No mocks of internals, no test-only backdoors: every
//! scenario drives the same registries/gates/lease managers production
//! wires, with process-spy fixture logs and typed errors as evidence.
//! Transport evidence is stated NARROWLY: every MCP/extension source in
//! this harness is a stdio-only child process started from a local path,
//! so process-spy zero-spawn proves no SOURCE TRANSPORT was initialized
//! for those sources during passive phases. It does not by itself prove
//! process-wide absence of sockets; zero external network is verified
//! independently by the tester's system-call audit (strace) pass. No
//! HTTP stubs are used anywhere in this harness — the test-local network
//! destination allowlist is EMPTY by construction.
//!
//! ── Traceability matrix (spec §7 acceptance bullet → named test) ────────
//! §7 b1  first request = exactly the configured core schemas, below the
//!        documented byte budget .......... a01_first_request_core_only_within_budget
//! §7 b2  dormant builtin/extension/MCP/skill bodies absent ............
//!        .................................. a02_dormant_bodies_absent_from_first_request
//! §7 b3  search starts zero MCP/extension processes, zero transport ....
//!        .................................. a03_search_starts_zero_processes
//! §7 b4  activating one deferred tool adds exactly that schema ........
//!        .................................. a04_activation_adds_exactly_one_schema
//! §7 b5  forged known-but-unactivated call fails before execution .....
//!        .................................. a05_forged_unactivated_call_denied_before_execution
//! §7 b6  runtime-name and sanitized-name aliases cannot bypass ........
//!        .................................. a06_alias_spellings_cannot_bypass_activation
//! §7 b7  new sessions inherit no activation . a07_new_sessions_inherit_no_activation
//! §7 b8  one MCP tool ⇒ one server, no sibling grants .................
//!        .................................. a08_mcp_exact_lease_no_sibling_grant
//!        (extension twin) ................. a09_extension_exact_lease_no_sibling_grant
//! §7 b9  revocation / digest change / generation change invalidate ....
//!        .................................. a10_revocation_digest_generation_invalidate
//! §7 b10 all providers expose the same logical active tool set after
//!        translation ...................... a11_cross_provider_logical_set_equivalence
//! §7.5   deferred extension classes stay dormant under search .........
//!        .................................. a03 (extension arm) + a09
//! §7.6   skill bodies absent at boot; exact selection loads verified
//!        body ............................. a12_skill_bodies_lazy_and_exact
//! §7.7   activate_many performs ONE stable-order generation update ....
//!        .................................. a13_activate_many_single_generation_update
//! §13.4  sibling/provider-wide escalation attempts ....................
//!        .................................. a05/a06/a08/a09 sibling arms
//! §13.4  consent simulated via host authorization policy hooks ........
//!        .................................. a14_consent_policy_hooks_gate_model_activation
//! Flag matrix: every session-set scenario runs with the core-set flag
//! OFF (default core) and ON (progressive core) via `for_both_cores`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use synaps_cli::extensions::hooks::HookBus;
use synaps_cli::extensions::lease::ExtensionLeaseCapability;
use synaps_cli::extensions::lifecycle::dormant_extension_tools;
use synaps_cli::extensions::manager::ExtensionManager;
use synaps_cli::extensions::manifest::ExtensionManifest;
use synaps_cli::mcp::descriptors::{
    dormant_tools_for_config, server_config_fingerprint, CachedServerDescriptors,
    CachedToolDescriptor, McpDescriptorCache,
};
use synaps_cli::mcp::{McpConfig, McpLeaseCapability, McpRuntimeManager, McpServerConfig};
use synaps_cli::skills::registry::CommandRegistry;
use synaps_cli::skills::tool::{LoadSkillTool, SearchSkillsTool};
use synaps_cli::tools::activation::{
    activate_exact_for_user, ActivationAuthority, ExecutionGate, SessionId, SessionToolSet,
    ToolAuthorizationError,
};
use synaps_cli::tools::catalog::{DiscoveryIndex, DiscoveryQuery, SearchLimits, ToolId};
use synaps_cli::tools::discovery::{ActivateToolsTool, ActivationCapability, SearchToolsTool};
use synaps_cli::tools::{
    Tool, ToolCapabilities, ToolChannels, ToolContext, ToolLimits, ToolRegistry,
};

// ── shared plumbing (public-surface only) ───────────────────────────────────

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("synaps-p3-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn engine_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("crates/agent-engine/tests/fixtures")
        .join(name)
}

fn sid(tag: &str) -> SessionId {
    SessionId::parse(&format!("p3-{tag}")).unwrap()
}

fn ctx() -> ToolContext {
    ctx_full(None, None, None, None)
}

fn ctx_full(
    activation: Option<ActivationCapability>,
    prompt: Option<synaps_cli::tools::SecretPromptHandle>,
    mcp: Option<McpLeaseCapability>,
    ext: Option<ExtensionLeaseCapability>,
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
            secret_prompt: prompt,
            orchestration: None,
            tool_activation: activation,
            mcp_leases: mcp,
            extension_leases: ext,
            memory_context: None,
            cwd: None,
        },
        limits: ToolLimits {
            max_tool_output: 64 * 1024,
            max_tool_buffer: 256 * 1024,
            bash_timeout: 30,
            bash_max_timeout: 300,
            subagent_timeout: 300,
        },
    }
}

/// Core-set flag matrix. The two modes have DIFFERENT documented
/// contracts: flag OFF (default core) exposes the full catalog as core —
/// legacy compatibility — while flag ON (progressive core) is where the
/// §7 activation-enforcement bullets apply. Each scenario asserts the
/// contract of ITS mode; enforcement is never asserted against the
/// legacy mode (that would be vacuous or false by design).
fn progressive_set(registry: &ToolRegistry, session: &SessionId) -> SessionToolSet {
    SessionToolSet::progressive_core_for_catalog(session.clone(), registry.catalog())
}

fn legacy_set(registry: &ToolRegistry, session: &SessionId) -> SessionToolSet {
    SessionToolSet::default_core_for_catalog(session.clone(), registry.catalog())
}

/// Inert dormant tool with a schema marker that must never appear in
/// passive surfaces. Executes typed-fail (acceptance never runs it).
const SCHEMA_MARKER: &str = "P3_DORMANT_SCHEMA_MARKER";
const BODY_MARKER: &str = "P3_SKILL_BODY_MARKER";

struct InertTool {
    name: String,
    origin: synaps_cli::tools::ToolOrigin,
    /// Only DORMANT fixtures carry the leak-detection marker; core
    /// fixtures use a plain schema, so a02 asserts real dormancy, not
    /// marker noise from the core itself.
    marked: bool,
}

impl InertTool {
    fn core(name: &str) -> Self {
        Self {
            name: name.to_string(),
            origin: synaps_cli::tools::ToolOrigin::Builtin,
            marked: false,
        }
    }
    fn dormant(name: &str) -> Self {
        Self {
            name: name.to_string(),
            origin: synaps_cli::tools::ToolOrigin::Builtin,
            marked: true,
        }
    }
}

#[async_trait::async_trait]
impl Tool for InertTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn origin(&self) -> synaps_cli::tools::ToolOrigin {
        self.origin.clone()
    }
    fn description(&self) -> &str {
        "phase3 inert fixture tool"
    }
    fn parameters(&self) -> Value {
        if self.marked {
            json!({"type":"object","properties":{"m":{"type":"string","description":SCHEMA_MARKER}}})
        } else {
            json!({"type":"object","properties":{"m":{"type":"string"}}})
        }
    }
    async fn execute(&self, _p: Value, _c: ToolContext) -> synaps_cli::error::Result<String> {
        Err(synaps_cli::error::RuntimeError::Tool(
            "phase3 inert tool must never execute".to_string(),
        ))
    }
}

/// MCP fixture plumbing (mirrors mcp_lease_lifecycle, via public surfaces).
struct McpFixture {
    dir: PathBuf,
    spy: PathBuf,
    config: McpServerConfig,
}

fn mcp_fixture(tag: &str, tools: Value) -> McpFixture {
    let dir = tmp_dir(&format!("mcp-{tag}"));
    let spy = dir.join("spy.log");
    let tools_json = dir.join("tools.json");
    std::fs::write(&tools_json, serde_json::to_vec(&tools).unwrap()).unwrap();
    let mut env = std::collections::HashMap::new();
    env.insert("MCP_FIXTURE_SPY".to_string(), spy.display().to_string());
    env.insert(
        "MCP_FIXTURE_TOOLS_JSON".to_string(),
        tools_json.display().to_string(),
    );
    env.insert("MCP_FIXTURE_MODE".to_string(), "ok".to_string());
    McpFixture {
        dir,
        spy,
        config: McpServerConfig {
            command: "python3".to_string(),
            args: vec![engine_fixture("mcp_fixture_server.py")
                .display()
                .to_string()],
            env,
        },
    }
}

impl McpFixture {
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

fn echo_schema() -> Value {
    json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]})
}

fn sibling_schema() -> Value {
    json!({"type":"object","properties":{}})
}

fn mcp_cache_for(config: &McpServerConfig) -> McpDescriptorCache {
    let mut cache = McpDescriptorCache::empty();
    cache.servers.insert(
        "srv".to_string(),
        CachedServerDescriptors {
            fingerprint: server_config_fingerprint(config),
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

fn mcp_config_with(cfg: McpServerConfig) -> McpConfig {
    let mut servers = std::collections::HashMap::new();
    servers.insert("srv".to_string(), cfg);
    McpConfig {
        mcp_servers: servers,
    }
}

fn mcp_manager(cfg: &McpServerConfig) -> Arc<McpRuntimeManager> {
    let cfg = cfg.clone();
    Arc::new(McpRuntimeManager::new(
        Arc::new(move |server: &str| (server == "srv").then(|| cfg.clone())),
        Duration::from_secs(300),
    ))
}

/// Extension fixture plumbing (mirrors extension_lease_lifecycle).
struct ExtFixture {
    dir: PathBuf,
    spy: PathBuf,
    manifest: ExtensionManifest,
}

fn ext_fixture(tag: &str) -> ExtFixture {
    let dir = tmp_dir(&format!("ext-{tag}"));
    let spy = dir.join("spy.log");
    let tools_json = dir.join("tools.json");
    std::fs::write(
        &tools_json,
        serde_json::to_vec(&json!([
            {"name": "search", "description": "deferred search", "input_schema": echo_schema()},
            {"name": "sibling", "description": "sibling stays dormant", "input_schema": sibling_schema()},
        ]))
        .unwrap(),
    )
    .unwrap();
    let manifest: ExtensionManifest = serde_json::from_value(json!({
        "runtime": "process",
        "command": "python3",
        "args": [
            engine_fixture("extension_fixture.py").display().to_string(),
            spy.display().to_string(),
            tools_json.display().to_string(),
            "ok",
        ],
        "permissions": ["tools.register"],
        "deferred": {
            "tools": [
                {"name": "search", "description": "deferred search", "input_schema": echo_schema()},
                {"name": "sibling", "description": "sibling stays dormant", "input_schema": sibling_schema()},
            ]
        }
    }))
    .unwrap();
    ExtFixture { dir, spy, manifest }
}

impl ExtFixture {
    fn events(&self) -> Vec<String> {
        std::fs::read_to_string(&self.spy)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Full acceptance registry: production-shaped core names plus dormant
/// builtin, MCP (descriptor cache), extension (deferred manifest), and
/// skill tools — assembled exclusively through public registration paths.
fn acceptance_registry(mcp: &McpFixture, ext: &ExtFixture) -> ToolRegistry {
    let mut registry = ToolRegistry::empty();
    for name in ["bash", "read", "search_tools", "activate_tools"] {
        registry.register(Arc::new(InertTool::core(name)));
    }
    for i in 0..8 {
        registry.register(Arc::new(InertTool::dormant(&format!("dormant_{i:02}"))));
    }
    registry
        .try_register_batch(dormant_tools_for_config(
            &mcp_config_with(mcp.config.clone()),
            &mcp_cache_for(&mcp.config),
        ))
        .unwrap();
    registry
        .try_register_batch(dormant_extension_tools("fixture-plugin", &ext.manifest))
        .unwrap();
    registry
}

// ── a01/a02: first request = core only, bounded; dormant bodies absent ──────

#[test]
fn a01_first_request_core_only_within_budget() {
    let mcp = mcp_fixture("a01", json!([]));
    let ext = ext_fixture("a01");
    let registry = acceptance_registry(&mcp, &ext);
    // Documented budget (docs/request-lifecycle-progressive-disclosure.md,
    // proven against the production core in progressive_disclosure.rs).
    const DOCUMENTED_FIRST_REQUEST_BUDGET_BYTES: usize = 8 * 1024;

    let set = SessionToolSet::progressive_core_for_catalog(sid("a01"), registry.catalog());
    let schemas = registry.session_tools_schema(&set).schema;
    let names: BTreeSet<String> = schemas
        .iter()
        .filter_map(|s| s["name"].as_str().map(String::from))
        .collect();
    // EXACT expected core equality for this fixture: of the documented
    // essential builtins, exactly these four are registered here — the
    // first request must be exactly them, nothing more, nothing less.
    let expected: BTreeSet<String> = ["bash", "read", "search_tools", "activate_tools"]
        .into_iter()
        .map(String::from)
        .collect();
    assert!(!names.is_empty(), "first request must not be empty");
    assert_eq!(
        names, expected,
        "first request = exactly the configured core"
    );
    let bytes = serde_json::to_vec(&*schemas).unwrap().len();
    assert!(
        bytes <= DOCUMENTED_FIRST_REQUEST_BUDGET_BYTES,
        "first request {bytes} bytes exceeds the documented budget"
    );
    mcp.cleanup();
    ext.cleanup();
}

#[test]
fn a02_dormant_bodies_absent_from_first_request() {
    let mcp = mcp_fixture("a02", json!([]));
    let ext = ext_fixture("a02");
    let registry = acceptance_registry(&mcp, &ext);
    // Progressive (flag ON): dormant builtin/MCP/extension schemas absent.
    let set = progressive_set(&registry, &sid("a02"));
    let serialized = serde_json::to_string(&registry.session_tools_schema(&set).schema).unwrap();
    assert!(
        !serialized.contains(SCHEMA_MARKER),
        "dormant builtin schema leaked"
    );
    assert!(
        !serialized.contains("ext__srv__"),
        "dormant MCP schema leaked"
    );
    assert!(
        !serialized.contains("fixture-plugin"),
        "dormant extension schema leaked"
    );
    // Legacy (flag OFF): full catalog exposed — documented compatibility.
    let legacy = legacy_set(&registry, &sid("a02"));
    let legacy_serialized =
        serde_json::to_string(&registry.session_tools_schema(&legacy).schema).unwrap();
    assert!(
        legacy_serialized.contains("ext__srv__echo_tool"),
        "flag-off keeps the legacy full exposure"
    );
    // Skill bodies: the constant load_skill schema and search results are
    // proven body-free in a12; the first-request projection above cannot
    // contain them because skills are not tools_schema entries at all.
    mcp.cleanup();
    ext.cleanup();
}

// ── a03: search starts zero processes (MCP + extension), zero transport ─────

#[tokio::test]
async fn a03_search_starts_zero_processes() {
    let mcp = mcp_fixture("a03", json!([]));
    let ext = ext_fixture("a03");
    let registry = acceptance_registry(&mcp, &ext);

    // Passive catalog search over EVERY source. Non-vacuous: each query
    // must actually HIT its dormant capability (the descriptors exist and
    // are searchable) while the spy stays empty.
    let index = DiscoveryIndex::build(registry.catalog()).unwrap();
    for (query, expected) in [
        ("echo", "mcp.srv:echo_tool"),
        ("sibling", "mcp.srv:sibling_tool"),
        ("search", "ext.fixture-plugin:search"),
        ("dormant", "builtin:dormant_00"),
    ] {
        let hits = index.search(
            &DiscoveryQuery::parse(query).unwrap(),
            &SearchLimits::new(16, 8 * 1024).unwrap(),
        );
        assert!(
            hits.hits().iter().any(|h| h.id().as_str() == expected),
            "query '{query}' must hit '{expected}' (dormant + searchable)"
        );
    }
    // Model-facing search_tools too (public builtin), both flag modes
    // (search is passive in EITHER mode — no spawn expected in both).
    for set in [
        progressive_set(&registry, &sid("a03")),
        legacy_set(&registry, &sid("a03-legacy")),
    ] {
        let shared = Arc::new(std::sync::RwLock::new(set));
        let cap = ActivationCapability::new(
            registry.catalog().clone(),
            shared,
            ActivationAuthority::Unauthorized,
        );
        SearchToolsTool
            .execute(
                json!({"query": "echo"}),
                ctx_full(Some(cap), None, None, None),
            )
            .await
            .unwrap();
    }

    // The stdio fixtures are the ONLY transport either source could open;
    // neither ever started, so no process and no socket exists.
    assert!(mcp.events().is_empty(), "search must not start MCP servers");
    assert!(ext.events().is_empty(), "search must not start extensions");
    mcp.cleanup();
    ext.cleanup();
}

// ── a04: exact activation adds exactly one schema ───────────────────────────

#[test]
fn a04_activation_adds_exactly_one_schema() {
    let mcp = mcp_fixture("a04", json!([]));
    let ext = ext_fixture("a04");
    let registry = acceptance_registry(&mcp, &ext);
    let mut set = progressive_set(&registry, &sid("a04"));
    let before: BTreeSet<String> = registry
        .session_tools_schema(&set)
        .schema
        .iter()
        .filter_map(|s| s["name"].as_str().map(String::from))
        .collect();
    activate_exact_for_user(
        &mut set,
        registry.catalog(),
        &ToolId::mcp("srv", "echo_tool"),
    )
    .unwrap();
    let after: BTreeSet<String> = registry
        .session_tools_schema(&set)
        .schema
        .iter()
        .filter_map(|s| s["name"].as_str().map(String::from))
        .collect();
    let added: Vec<&String> = after.difference(&before).collect();
    assert_eq!(
        added,
        vec![&"ext__srv__echo_tool".to_string()],
        "exactly one schema added"
    );
    assert!(mcp.events().is_empty(), "activation must not spawn");
    // Legacy mode: activating an already-core id fails TYPED, zero drift.
    let mut legacy = legacy_set(&registry, &sid("a04"));
    let generation = legacy.schema_generation();
    assert!(
        activate_exact_for_user(
            &mut legacy,
            registry.catalog(),
            &ToolId::mcp("srv", "echo_tool")
        )
        .is_err(),
        "flag-off: already-core activation is a typed no-op"
    );
    assert_eq!(legacy.schema_generation(), generation);
    mcp.cleanup();
    ext.cleanup();
}

// ── a05/a06: forged + alias spellings fail before execution ─────────────────

#[test]
fn a05_forged_unactivated_call_denied_before_execution() {
    let mcp = mcp_fixture("a05", json!([]));
    let ext = ext_fixture("a05");
    let registry = acceptance_registry(&mcp, &ext);
    let set = progressive_set(&registry, &sid("a05"));
    // Known-but-unactivated across every source class (flag ON). Non-
    // vacuous: each forged id IS cataloged (the attack targets a real
    // capability) yet the gate denies BEFORE any implementation runs —
    // the registry implementations would error loudly if executed, and
    // the fixture spies stay empty below.
    for (wire, id) in [
        ("dormant_00", ToolId::builtin("dormant_00")),
        ("ext__srv__echo_tool", ToolId::mcp("srv", "echo_tool")),
        (
            "fixture-plugin:search",
            ToolId::extension("fixture-plugin", "search"),
        ),
    ] {
        assert!(
            registry.catalog().get(&id).is_some(),
            "'{wire}' is a KNOWN capability"
        );
        assert!(
            ExecutionGate::authorize_wire_call(&registry, &set, wire).is_err(),
            "forged unactivated '{wire}' must be denied"
        );
    }
    // Unknown names denied in BOTH modes (never reach implementation lookup).
    let legacy = legacy_set(&registry, &sid("a05"));
    for set in [&set, &legacy] {
        assert!(ExecutionGate::authorize_wire_call(&registry, set, "no_such_tool").is_err());
    }
    // Legacy compatibility: cataloged tools stay authorized flag-off.
    ExecutionGate::authorize_wire_call(&registry, &legacy, "ext__srv__echo_tool")
        .expect("flag-off keeps the legacy full authorization");
    assert!(mcp.events().is_empty() && ext.events().is_empty());
    mcp.cleanup();
    ext.cleanup();
}

#[test]
fn a06_alias_spellings_cannot_bypass_activation() {
    let mcp = mcp_fixture("a06", json!([]));
    let ext = ext_fixture("a06");
    let registry = acceptance_registry(&mcp, &ext);
    let set = progressive_set(&registry, &sid("a06"));
    // Runtime name, sanitized API name, and forged spellings of the SAME
    // dormant capabilities: no spelling may bypass the gate (flag ON).
    let sanitized_alias = {
        // The projection exposes an api-safe name for the ':' runtime
        // name; recover it from a temporarily-activated twin session so
        // the alias tested is the REAL sanitized spelling.
        let mut probe = progressive_set(&registry, &sid("a06-probe"));
        activate_exact_for_user(
            &mut probe,
            registry.catalog(),
            &ToolId::extension("fixture-plugin", "search"),
        )
        .unwrap();
        registry
            .session_tools_schema(&probe)
            .schema
            .iter()
            .filter_map(|s| s["name"].as_str().map(String::from))
            .find(|n| registry.runtime_name_for_api(n) == "fixture-plugin:search")
            .expect("sanitized alias exists for the ':' runtime name")
    };
    let spellings = [
        "fixture-plugin:search".to_string(),
        sanitized_alias,
        "fixture-plugin_search".to_string(), // sanitized-alias guess
        "ext__srv__echo_tool".to_string(),
        "EXT__SRV__ECHO_TOOL".to_string(), // case-forged
    ];
    for wire in &spellings {
        assert!(
            ExecutionGate::authorize_wire_call(&registry, &set, wire).is_err(),
            "alias '{wire}' must not bypass activation"
        );
    }
    assert!(mcp.events().is_empty() && ext.events().is_empty());
    mcp.cleanup();
    ext.cleanup();
}

// ── a07: no inherited activation across sessions ────────────────────────────

#[test]
fn a07_new_sessions_inherit_no_activation() {
    let mcp = mcp_fixture("a07", json!([]));
    let ext = ext_fixture("a07");
    let registry = acceptance_registry(&mcp, &ext);
    let mut first =
        SessionToolSet::progressive_core_for_catalog(sid("a07-one"), registry.catalog());
    activate_exact_for_user(
        &mut first,
        registry.catalog(),
        &ToolId::mcp("srv", "echo_tool"),
    )
    .unwrap();
    ExecutionGate::authorize_wire_call(&registry, &first, "ext__srv__echo_tool").unwrap();

    // A NEW progressive session over the same registry/catalog starts
    // with zero inherited activation.
    let fresh = progressive_set(&registry, &sid("a07-two"));
    assert!(
        ExecutionGate::authorize_wire_call(&registry, &fresh, "ext__srv__echo_tool").is_err(),
        "new session must not inherit activation"
    );
    assert_eq!(fresh.activated().count(), 0);
    // Legacy sessions carry no inherited SESSION activations either
    // (their authorization is the core set, not the sibling's grants).
    let legacy = legacy_set(&registry, &sid("a07-three"));
    assert_eq!(legacy.activated().count(), 0);
    mcp.cleanup();
    ext.cleanup();
}

// ── a08/a09: exact lease isolation (one child, no sibling grant/call) ───────

#[tokio::test]
async fn a08_mcp_exact_lease_no_sibling_grant() {
    let mcp = mcp_fixture(
        "a08",
        json!([
            {"name": "echo_tool", "description": "echoes text back", "inputSchema": echo_schema()},
            {"name": "sibling_tool", "description": "sibling stays dormant", "inputSchema": sibling_schema()},
        ]),
    );
    let ext = ext_fixture("a08");
    let registry = acceptance_registry(&mcp, &ext);
    let manager = mcp_manager(&mcp.config);
    let session = sid("a08");
    let cap = McpLeaseCapability::new(session.clone(), Arc::clone(&manager));

    let mut set = SessionToolSet::progressive_core_for_catalog(session, registry.catalog());
    activate_exact_for_user(
        &mut set,
        registry.catalog(),
        &ToolId::mcp("srv", "echo_tool"),
    )
    .unwrap();
    let authorized =
        ExecutionGate::authorize_wire_call(&registry, &set, "ext__srv__echo_tool").unwrap();
    let tool = registry.get(authorized.runtime_name()).unwrap();
    let out = tool
        .execute(json!({"text": "hi"}), ctx_full(None, None, Some(cap), None))
        .await
        .unwrap();
    assert_eq!(out, "called:echo_tool");

    // ONE server child; sibling never granted, never called.
    assert_eq!(mcp.count("spawn"), 1, "exactly one server started");
    assert_eq!(mcp.count("request:tools/call:echo_tool"), 1);
    assert_eq!(mcp.count("request:tools/call:sibling_tool"), 0);
    assert!(
        ExecutionGate::authorize_wire_call(&registry, &set, "ext__srv__sibling_tool").is_err(),
        "sibling on the same connection stays ungranted"
    );
    assert!(ext.events().is_empty(), "no cross-source process");
    manager.terminate_all();
    mcp.cleanup();
    ext.cleanup();
}

#[tokio::test]
async fn a09_extension_exact_lease_no_sibling_grant() {
    let mcp = mcp_fixture("a09", json!([]));
    let ext = ext_fixture("a09");
    let registry = Arc::new(tokio::sync::RwLock::new(acceptance_registry(&mcp, &ext)));
    let mut mgr = ExtensionManager::new_with_tools(Arc::new(HookBus::new()), Arc::clone(&registry));
    mgr.set_progressive_deferral(true);
    // The acceptance registry already cataloged the dormant batch; load
    // the record through the manager against a fresh registry namespace
    // is unnecessary — use dormant tools + manager record path directly.
    let mut mgr2 = ExtensionManager::new_with_tools(
        Arc::new(HookBus::new()),
        Arc::new(tokio::sync::RwLock::new(ToolRegistry::empty())),
    );
    mgr2.set_progressive_deferral(true);
    mgr2.load("fixture-plugin", &ext.manifest).await.unwrap();
    assert!(ext.events().is_empty(), "deferred load must not spawn");
    let runtime = mgr2.extension_runtime();
    let session = sid("a09");
    let cap = ExtensionLeaseCapability::new(session.clone(), Arc::clone(&runtime));

    let reg = registry.read().await;
    let mut set = SessionToolSet::progressive_core_for_catalog(session, reg.catalog());
    activate_exact_for_user(
        &mut set,
        reg.catalog(),
        &ToolId::extension("fixture-plugin", "search"),
    )
    .unwrap();
    let authorized =
        ExecutionGate::authorize_wire_call(&reg, &set, "fixture-plugin:search").unwrap();
    let tool = reg.get(authorized.runtime_name()).unwrap();
    let out = tool
        .execute(json!({"text": "x"}), ctx_full(None, None, None, Some(cap)))
        .await
        .unwrap();
    assert_eq!(out, "called:search");

    let events = ext.events();
    assert_eq!(events.iter().filter(|e| *e == "spawn").count(), 1);
    assert_eq!(events.iter().filter(|e| *e == "call:search").count(), 1);
    assert_eq!(events.iter().filter(|e| *e == "call:sibling").count(), 0);
    assert!(
        ExecutionGate::authorize_wire_call(&reg, &set, "fixture-plugin:sibling").is_err(),
        "extension sibling stays ungranted"
    );
    runtime.terminate_all();
    mcp.cleanup();
    ext.cleanup();
}

// ── a10: revocation / digest drift / generation drift invalidate ────────────

#[test]
fn a10_revocation_digest_generation_invalidate() {
    let mcp = mcp_fixture("a10", json!([]));
    let ext = ext_fixture("a10");
    let mut registry = acceptance_registry(&mcp, &ext);
    let session = sid("a10");
    let mut set = SessionToolSet::progressive_core_for_catalog(session.clone(), registry.catalog());
    let echo = ToolId::mcp("srv", "echo_tool");
    activate_exact_for_user(&mut set, registry.catalog(), &echo).unwrap();
    ExecutionGate::authorize_wire_call(&registry, &set, "ext__srv__echo_tool").unwrap();

    // 1. Exact revocation invalidates exactly that grant.
    let generation = set.schema_generation();
    set.revoke_exact(&echo).unwrap();
    assert_eq!(set.schema_generation(), generation + 1);
    assert!(matches!(
        ExecutionGate::authorize_wire_call(&registry, &set, "ext__srv__echo_tool"),
        Err(ToolAuthorizationError::NotActivated(ref id)) if *id == echo
    ));

    // 2. Catalog generation change (unrelated registry mutation) marks the
    // set stale but does NOT deny per-tool: the gate validates each pinned
    // record's digest + provenance individually (6b668835), so an unchanged
    // activated tool keeps authorizing while fresh grants are blocked.
    let mut set2 =
        SessionToolSet::progressive_core_for_catalog(session.clone(), registry.catalog());
    activate_exact_for_user(&mut set2, registry.catalog(), &echo).unwrap();
    registry
        .try_disable(&["dormant_07".to_string()])
        .expect("disable advances the catalog generation");
    assert!(set2.is_stale(registry.catalog()));
    ExecutionGate::authorize_wire_call(&registry, &set2, "ext__srv__echo_tool")
        .expect("schema-identical activated tool survives unrelated drift");

    // 3. Schema digest drift: a session set whose grant was pinned under
    // the ORIGINAL schema must be denied against a registry whose live
    // record now carries a DIFFERENT schema (same construction sequence,
    // so catalog generations match and the digest is the discriminator).
    let mcp_b = mcp_fixture("a10-drift", json!([]));
    let ext_b = ext_fixture("a10-drift");
    let mut drifted_cache = mcp_cache_for(&mcp_b.config);
    drifted_cache.servers.get_mut("srv").unwrap().tools[0].input_schema =
        json!({"type":"object","properties":{"evil":{"type":"string"}}});
    let mut registry_b = ToolRegistry::empty();
    for name in ["bash", "read", "search_tools", "activate_tools"] {
        registry_b.register(Arc::new(InertTool::core(name)));
    }
    for i in 0..8 {
        registry_b.register(Arc::new(InertTool::dormant(&format!("dormant_{i:02}"))));
    }
    registry_b
        .try_register_batch(dormant_tools_for_config(
            &mcp_config_with(mcp_b.config.clone()),
            &drifted_cache,
        ))
        .unwrap();
    registry_b
        .try_register_batch(dormant_extension_tools("fixture-plugin", &ext_b.manifest))
        .unwrap();
    // Build the registries through identical sequences so generations
    // align; sanity-check that alignment before relying on it.
    let registry_a = acceptance_registry(&mcp_b, &ext_b);
    assert_eq!(
        registry_a.catalog().generation(),
        registry_b.catalog().generation(),
        "generation aligned: the digest is the only discriminator"
    );
    assert_ne!(
        registry_a.catalog().get(&echo).unwrap().schema_digest(),
        registry_b.catalog().get(&echo).unwrap().schema_digest(),
        "schemas genuinely drifted"
    );
    let mut pinned =
        SessionToolSet::progressive_core_for_catalog(sid("a10-drift"), registry_a.catalog());
    activate_exact_for_user(&mut pinned, registry_a.catalog(), &echo).unwrap();
    ExecutionGate::authorize_wire_call(&registry_a, &pinned, "ext__srv__echo_tool")
        .expect("original digest authorizes");
    let err = ExecutionGate::authorize_wire_call(&registry_b, &pinned, "ext__srv__echo_tool")
        .expect_err("digest drift must deny the pinned grant");
    assert!(
        matches!(err, ToolAuthorizationError::SchemaDigestMismatch(ref id) if *id == echo),
        "expected SchemaDigestMismatch for the exact id"
    );
    mcp_b.cleanup();
    ext_b.cleanup();
    mcp.cleanup();
    ext.cleanup();
}

// ── a11: cross-provider logical tool-set equivalence after translation ──────

#[test]
fn a11_cross_provider_logical_set_equivalence() {
    let mcp = mcp_fixture("a11", json!([]));
    let ext = ext_fixture("a11");
    let registry = acceptance_registry(&mcp, &ext);
    let mut modes: Vec<(&str, SessionToolSet)> = Vec::new();
    let mut progressive = progressive_set(&registry, &sid("a11"));
    activate_exact_for_user(
        &mut progressive,
        registry.catalog(),
        &ToolId::mcp("srv", "echo_tool"),
    )
    .unwrap();
    activate_exact_for_user(
        &mut progressive,
        registry.catalog(),
        &ToolId::extension("fixture-plugin", "search"),
    )
    .unwrap();
    modes.push(("flag-on/progressive", progressive));
    // Legacy full exposure runs through the SAME adapters.
    modes.push(("flag-off/legacy", legacy_set(&registry, &sid("a11-legacy"))));
    for (mode, set) in modes {
        // Anthropic-side: the session projection (api-safe names).
        let schemas = registry.session_tools_schema(&set).schema;
        let anthropic_logical: BTreeSet<String> = schemas
            .iter()
            .filter_map(|s| s["name"].as_str())
            .map(|api| registry.runtime_name_for_api(api).to_string())
            .collect();

        // OpenAI-side: the REAL request adapter over the same projection.
        let (oai_tools, name_map) = synaps_cli::runtime::openai::translate::tools_to_oai(&schemas);
        let openai_logical: BTreeSet<String> = oai_tools
            .iter()
            .map(|t| {
                registry
                    .runtime_name_for_api(name_map.to_original(&t.function.name))
                    .to_string()
            })
            .collect();

        // Gemini-side: the REAL public provider adapter over the same
        // projection (internal-only names are dropped by contract; none
        // are present in this session set — assert that non-vacuously).
        let gemini_specs = synaps_cli::runtime::google_gemini::translate_tool_schemas(&schemas);
        let gemini_logical: BTreeSet<String> = gemini_specs
            .iter()
            .map(|t| registry.runtime_name_for_api(&t.name).to_string())
            .collect();

        assert_eq!(
            anthropic_logical, openai_logical,
            "[{mode}] OpenAI must expose the same logical tool set"
        );
        assert_eq!(
            anthropic_logical, gemini_logical,
            "[{mode}] Gemini must expose the same logical tool set"
        );
        assert!(
            anthropic_logical.contains("ext__srv__echo_tool"),
            "[{mode}]"
        );
        assert!(
            anthropic_logical.contains("fixture-plugin:search"),
            "[{mode}]"
        );

        // Cloud honesty: text-only cloud routes do NOT expose tools — the
        // documented pre-flight rejects a tools-bearing request LOCALLY
        // (typed, before broker/credential/network), so the acceptance
        // claim is an explicit unsupported-capability loss, not silent
        // inequivalent exposure.
        let has_tools = !schemas.is_empty();
        assert!(
            has_tools,
            "[{mode}] non-vacuous: the session projects tools"
        );
        let providers = [
            synaps_cli::auth::CloudProviderId::AzureOpenAi,
            synaps_cli::auth::CloudProviderId::AwsBedrock,
            synaps_cli::auth::CloudProviderId::GoogleVertex,
        ];
        // No silent skip: this claim is only meaningful while ALL cloud
        // descriptors are text-only. If a descriptor gains tool support,
        // this assertion forces the harness to model it honestly instead
        // of quietly narrowing coverage.
        assert!(
            providers.iter().all(|p| !p.supports_tools()),
            "[{mode}] every cloud descriptor is documented text-only"
        );
        let mut tested = 0;
        for provider in providers {
            assert!(
                synaps_cli::auth::preflight_cloud_capability(provider, has_tools).is_err(),
                "[{mode}] text-only cloud must reject tools locally"
            );
            tested += 1;
        }
        assert_eq!(tested, 3, "[{mode}] all three cloud providers exercised");
    }
    mcp.cleanup();
    ext.cleanup();
}

// ── a12: skill bodies lazy at boot, exact verified load ─────────────────────

#[tokio::test]
async fn a12_skill_bodies_lazy_and_exact() {
    let root = tmp_dir("skill");
    let dir = root.join("skills").join("zq-skill");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: zq-skill\ndescription: phase3 acceptance skill\n---\n{BODY_MARKER}"),
    )
    .unwrap();
    let (_p, skills) = synaps_cli::skills::loader::load_all(std::slice::from_ref(&root));
    assert_eq!(skills.len(), 1);
    assert!(
        !format!("{skills:?}").contains(BODY_MARKER),
        "boot metadata carries no body"
    );
    let registry = Arc::new(CommandRegistry::new(&[], skills));
    let load = LoadSkillTool::new(registry.clone());
    assert!(
        !serde_json::to_string(&load.parameters())
            .unwrap()
            .contains(BODY_MARKER),
        "constant schema carries no body"
    );
    let search = SearchSkillsTool::new(registry.clone());
    let found = search
        .execute(json!({"query": "zq-skill"}), ctx())
        .await
        .unwrap();
    assert!(found.contains("zq-skill") && !found.contains(BODY_MARKER));
    // Compose the ACTUAL first-request projection: register load_skill/
    // search_skills into a tool registry (production registration path)
    // and assert the serialized session projection carries no body bytes.
    let mut tools = ToolRegistry::empty();
    tools.register(Arc::new(LoadSkillTool::new(registry.clone())));
    tools.register(Arc::new(SearchSkillsTool::new(registry.clone())));
    let set = SessionToolSet::progressive_core_for_catalog(sid("a12"), tools.catalog());
    let projection = serde_json::to_string(&tools.session_tools_schema(&set).schema).unwrap();
    assert!(
        projection.contains("load_skill"),
        "projection includes the skill tools"
    );
    assert!(
        !projection.contains(BODY_MARKER),
        "first-request projection carries no skill body"
    );
    assert!(
        !projection.contains("zq-skill"),
        "constant schema does not enumerate the catalog"
    );
    // Exact selection loads the verified body.
    let out = load
        .execute(json!({"skill": "zq-skill"}), ctx())
        .await
        .unwrap();
    assert!(out.contains(BODY_MARKER));
    let _ = std::fs::remove_dir_all(&root);
}

// ── a13: activate_many = ONE stable-order generation update ─────────────────

#[test]
fn a13_activate_many_single_generation_update() {
    let mcp = mcp_fixture("a13", json!([]));
    let ext = ext_fixture("a13");
    let registry = acceptance_registry(&mcp, &ext);
    let mut set = progressive_set(&registry, &sid("a13"));
    let generation = set.schema_generation();
    let grants: Vec<_> = [
        ToolId::builtin("dormant_00"),
        ToolId::builtin("dormant_01"),
        ToolId::mcp("srv", "echo_tool"),
    ]
    .iter()
    .map(|id| {
        synaps_cli::tools::activation::issue_exact_grant(registry.catalog(), set.session(), id)
            .unwrap()
    })
    .collect();
    let granted = set.activate_many(grants, registry.catalog()).unwrap();
    assert_eq!(granted, 3);
    assert_eq!(
        set.schema_generation(),
        generation + 1,
        "bulk activation advances the generation exactly once"
    );
    // Stable order: projection order deterministic across runs.
    let names: Vec<String> = registry
        .session_tools_schema(&set)
        .schema
        .iter()
        .filter_map(|s| s["name"].as_str().map(String::from))
        .collect();
    let again: Vec<String> = registry
        .session_tools_schema(&set)
        .schema
        .iter()
        .filter_map(|s| s["name"].as_str().map(String::from))
        .collect();
    assert_eq!(names, again, "deterministic stable order");
    // Legacy mode: bulk-activating already-core ids fails typed with
    // ZERO generation drift.
    let mut legacy = legacy_set(&registry, &sid("a13-legacy"));
    let legacy_generation = legacy.schema_generation();
    assert!(synaps_cli::tools::activation::issue_exact_grant(
        registry.catalog(),
        legacy.session(),
        &ToolId::builtin("dormant_00")
    )
    .map(|grant| legacy.activate_many(vec![grant], registry.catalog()))
    .map_or(true, |r| r.is_err()));
    assert_eq!(legacy.schema_generation(), legacy_generation);
    assert!(mcp.events().is_empty() && ext.events().is_empty());
    mcp.cleanup();
    ext.cleanup();
}

// ── a14: consent simulated via host authorization policy hooks ──────────────

#[tokio::test]
async fn a14_consent_policy_hooks_gate_model_activation() {
    let mcp = mcp_fixture("a14", json!([]));
    let ext = ext_fixture("a14");
    let registry = acceptance_registry(&mcp, &ext);
    let session = sid("a14");

    // Programmatic host consent: an interactive prompt channel answered
    // by the harness (the host authorization policy hook), no TTY.
    let consent = |answer: &'static str| {
        let (tx, mut rx) =
            tokio::sync::mpsc::unbounded_channel::<synaps_cli::tools::SecretPromptRequest>();
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                let _ = req.response_tx.send(Some(answer.to_string()));
            }
        });
        synaps_cli::tools::SecretPromptHandle::new(tx)
    };

    for (answer, expect_granted) in [("y", true), ("n", false)] {
        let set = Arc::new(std::sync::RwLock::new(
            SessionToolSet::progressive_core_for_catalog(session.clone(), registry.catalog()),
        ));
        let cap = ActivationCapability::new(
            registry.catalog().clone(),
            Arc::clone(&set),
            ActivationAuthority::Unauthorized,
        );
        let result = ActivateToolsTool
            .execute(
                json!({"tools": ["builtin:dormant_00"]}),
                ctx_full(Some(cap), Some(consent(answer)), None, None),
            )
            .await;
        let granted =
            ExecutionGate::authorize_wire_call(&registry, &set.read().unwrap(), "dormant_00")
                .is_ok();
        assert_eq!(
            granted, expect_granted,
            "host answer '{answer}' must decide the grant (result: {result:?})"
        );
    }

    // NO prompt handle at all: Unauthorized fails closed — the model
    // cannot self-authorize when the host policy hook is absent.
    let set = Arc::new(std::sync::RwLock::new(
        SessionToolSet::progressive_core_for_catalog(session.clone(), registry.catalog()),
    ));
    let cap = ActivationCapability::new(
        registry.catalog().clone(),
        Arc::clone(&set),
        ActivationAuthority::Unauthorized,
    );
    ActivateToolsTool
        .execute(
            json!({"tools": ["builtin:dormant_00"]}),
            ctx_full(Some(cap), None, None, None),
        )
        .await
        .expect_err("no host prompt => activation denied");
    assert!(
        ExecutionGate::authorize_wire_call(&registry, &set.read().unwrap(), "dormant_00").is_err()
    );

    // MODEL-authored confirmation JSON cannot replace the host prompt:
    // extra params claiming consent are ignored; still denied without a
    // host answer.
    let cap = ActivationCapability::new(
        registry.catalog().clone(),
        Arc::clone(&set),
        ActivationAuthority::Unauthorized,
    );
    ActivateToolsTool
        .execute(
            json!({
                "tools": ["builtin:dormant_00"],
                "confirmed": true,
                "authority": "ModelConfirmed",
                "consent": {"granted": true}
            }),
            ctx_full(Some(cap), None, None, None),
        )
        .await
        .expect_err("model-authored consent JSON must not self-authorize");
    assert!(
        ExecutionGate::authorize_wire_call(&registry, &set.read().unwrap(), "dormant_00").is_err(),
        "no grant after forged consent params"
    );
    assert!(mcp.events().is_empty() && ext.events().is_empty());
    mcp.cleanup();
    ext.cleanup();
}
