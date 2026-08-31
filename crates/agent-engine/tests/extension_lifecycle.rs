//! Task 20 Commit A — typed extension manifest inventory/classification,
//! bounded passive deferred declarations, and dormant zero-spawn
//! descriptor tools for tool-only extensions.

use agent_engine::extensions::lifecycle::{
    classify, dormant_extension_tools, earliest_trigger, validate_runtime_tool_declarations,
    ActivationTrigger, DeclaredExtensionProvider, DeclaredExtensionProviderModel,
    DeclaredExtensionTool, DeferredDeclarations, DeferredLifecycle, ExtensionClass,
};
use agent_engine::extensions::manifest::{ExtensionManifest, ExtensionRuntime};
use agent_engine::extensions::runtime::process::RegisteredExtensionToolSpec;
use agent_engine::tools::activation::{SessionId, SessionToolSet};
use agent_engine::tools::catalog::{
    DiscoveryIndex, DiscoveryQuery, SchemaDigest, SearchLimits, ToolId,
};
use agent_engine::tools::{ToolCapabilities, ToolChannels, ToolContext, ToolLimits, ToolRegistry};
use serde_json::{json, Value};

fn schema() -> Value {
    json!({"type":"object","properties":{"q":{"type":"string"}},"required":["q"]})
}

fn declared(name: &str) -> DeclaredExtensionTool {
    DeclaredExtensionTool {
        name: name.to_string(),
        description: format!("declared {name}"),
        input_schema: schema(),
    }
}

fn base_manifest() -> ExtensionManifest {
    serde_json::from_value(json!({
        "runtime": "process",
        "command": "/bin/false",
        "permissions": ["tools.register"]
    }))
    .unwrap()
}

fn with_deferred(
    tools: Vec<DeclaredExtensionTool>,
    lifecycle: Option<DeferredLifecycle>,
) -> ExtensionManifest {
    let mut m = base_manifest();
    m.deferred = Some(DeferredDeclarations {
        tools,
        providers: Vec::new(),
        context_providers: vec![],
        lifecycle,
    });
    m
}

fn ctx() -> ToolContext {
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
            secret_prompt: None,
            orchestration: None,
            tool_activation: None,
            mcp_leases: None,
            extension_leases: None,
            memory_context: None,
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

// ── serde back-compat ───────────────────────────────────────────────────────

#[test]
fn absent_deferred_block_stays_fully_backward_compatible() {
    // Pre-Task-20 manifest JSON: no `deferred` key anywhere.
    let m = base_manifest();
    assert!(m.deferred.is_none());
    assert_eq!(classify(&m), ExtensionClass::LegacyEager);
    // Round-trip does not invent the field for legacy manifests.
    let round: Value = serde_json::to_value(&m).unwrap();
    assert!(round.get("deferred").map(|d| d.is_null()).unwrap_or(true));
    // Validation continues to succeed exactly as before.
    m.validate("legacy").unwrap();
}

// ── classification ──────────────────────────────────────────────────────────

#[test]
fn classification_matrix_is_typed_and_exact() {
    // Legacy: deferred present but empty declares nothing => eager.
    assert_eq!(
        classify(&with_deferred(vec![], None)),
        ExtensionClass::LegacyEager
    );
    // Tool-only.
    assert_eq!(
        classify(&with_deferred(vec![declared("search")], None)),
        ExtensionClass::ToolOnly
    );
    // Provider-only.
    let mut provider_only = base_manifest();
    provider_only.deferred = Some(DeferredDeclarations {
        tools: vec![],
        providers: vec![declared_provider("prov")],
        context_providers: vec![],
        lifecycle: None,
    });
    assert_eq!(classify(&provider_only), ExtensionClass::Provider);
    assert_eq!(
        earliest_trigger(&provider_only),
        ActivationTrigger::ProviderSelection
    );
    // Hook lifecycle via subscriptions.
    let mut hooky: ExtensionManifest = serde_json::from_value(json!({
        "runtime": "process",
        "command": "/bin/false",
        "permissions": ["tools.register"],
        "hooks": [{"hook": "before_tool_call"}]
    }))
    .unwrap();
    hooky.deferred = Some(DeferredDeclarations {
        tools: vec![],
        providers: vec![],
        context_providers: vec![],
        lifecycle: None,
    });
    assert_eq!(classify(&hooky), ExtensionClass::HookLifecycle);
    // UI/sidecar via explicit lifecycle hint.
    assert_eq!(
        classify(&with_deferred(vec![], Some(DeferredLifecycle::User))),
        ExtensionClass::UiSidecar
    );
    // Mixed: tools + hooks.
    let mut mixed: ExtensionManifest = serde_json::from_value(json!({
        "runtime": "process",
        "command": "/bin/false",
        "permissions": ["tools.register"],
        "hooks": [{"hook": "before_tool_call"}]
    }))
    .unwrap();
    mixed.deferred = Some(DeferredDeclarations {
        tools: vec![declared("t")],
        providers: vec![],
        context_providers: vec![],
        lifecycle: None,
    });
    assert_eq!(classify(&mixed), ExtensionClass::Mixed);
}

// ── bounded validation of declarations ──────────────────────────────────────

#[test]
fn hostile_or_unbounded_deferred_declarations_fail_manifest_validation_closed() {
    // Control-char name.
    let m = with_deferred(vec![declared("bad\u{7}name")], None);
    assert!(m.validate("p").is_err());
    // Oversized name.
    let m = with_deferred(vec![declared(&"n".repeat(200))], None);
    assert!(m.validate("p").is_err());
    // Empty name.
    let m = with_deferred(vec![declared("")], None);
    assert!(m.validate("p").is_err());
    // Duplicate names.
    let m = with_deferred(vec![declared("dup"), declared("dup")], None);
    assert!(m.validate("p").is_err());
    // Non-object schema.
    let mut bad = declared("t");
    bad.input_schema = json!("not an object");
    assert!(with_deferred(vec![bad], None).validate("p").is_err());
    // Sane declarations pass.
    with_deferred(vec![declared("ok")], None)
        .validate("p")
        .unwrap();
}

// ── dormant descriptor tools: zero spawn, searchable, gated ─────────────────

#[test]
fn dormant_extension_tools_are_searchable_gated_and_never_spawn() {
    let dir = std::env::temp_dir().join(format!("synaps-ext-lc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("spawn.marker");

    let mut m = with_deferred(vec![declared("search"), declared("sibling")], None);
    // If anything in this test constructed a ProcessExtension, the marker
    // would exist afterwards.
    m.command = "/bin/sh".to_string();
    m.args = vec!["-c".to_string(), format!("echo x >> {}", marker.display())];
    m.validate("fixture-plugin").unwrap();

    let tools = dormant_extension_tools("fixture-plugin", &m);
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(
        names,
        vec!["fixture-plugin:search", "fixture-plugin:sibling"]
    );

    let mut registry = ToolRegistry::new();
    registry.try_register_batch(tools).unwrap();

    // Truthful catalog identity + digest parity with the declaration.
    let id = ToolId::extension("fixture-plugin", "search");
    let record = registry.catalog().get(&id).expect("cataloged");
    assert_eq!(record.schema_digest(), &SchemaDigest::of_schema(&schema()));

    // Searchable passively; excluded from the progressive core.
    let index = DiscoveryIndex::build(registry.catalog()).unwrap();
    let hits = index.search(
        &DiscoveryQuery::parse("declared search").unwrap(),
        &SearchLimits::new(16, 8 * 1024).unwrap(),
    );
    assert!(hits.hits().iter().any(|h| h.id() == &id));
    let set = SessionToolSet::progressive_core_for_catalog(
        SessionId::parse("task20").unwrap(),
        registry.catalog(),
    );
    let projected: Vec<String> = registry
        .session_tools_schema(&set)
        .schema
        .iter()
        .filter_map(|s| s["name"].as_str().map(String::from))
        .collect();
    assert!(!projected.iter().any(|n| n.starts_with("fixture-plugin:")));

    assert!(!marker.exists(), "discovery/search must never spawn");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn deferred_execute_without_runtime_lease_fails_typed_and_spawns_nothing() {
    let dir = std::env::temp_dir().join(format!("synaps-ext-exec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("spawn.marker");
    let mut m = with_deferred(vec![declared("search")], None);
    m.command = "/bin/sh".to_string();
    m.args = vec!["-c".to_string(), format!("echo x >> {}", marker.display())];

    let tools = dormant_extension_tools("fixture-plugin", &m);
    let err = tools[0]
        .execute(json!({"q":"x"}), ctx())
        .await
        .expect_err("deferred extension tool must not run without a lease");
    let msg = err.to_string();
    assert!(msg.contains("deferred") || msg.contains("lease"), "{msg}");
    assert!(!marker.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

// ── strict runtime declaration validation ───────────────────────────────────

#[test]
fn runtime_initialize_declarations_must_match_manifest_exactly() {
    let declared = vec![declared("search")];
    let ok = vec![RegisteredExtensionToolSpec {
        name: "search".to_string(),
        description: "declared search".to_string(),
        input_schema: schema(),
    }];
    validate_runtime_tool_declarations(&declared, &ok).unwrap();

    // Missing declared tool.
    assert!(validate_runtime_tool_declarations(&declared, &[]).is_err());

    // Undeclared extra tool.
    let extra = vec![
        ok[0].clone(),
        RegisteredExtensionToolSpec {
            name: "surprise".to_string(),
            description: String::new(),
            input_schema: schema(),
        },
    ];
    assert!(validate_runtime_tool_declarations(&declared, &extra).is_err());

    // Schema digest mismatch.
    let drifted = vec![RegisteredExtensionToolSpec {
        name: "search".to_string(),
        description: String::new(),
        input_schema: json!({"type":"object","properties":{"evil":{"type":"string"}}}),
    }];
    assert!(validate_runtime_tool_declarations(&declared, &drifted).is_err());
}

fn declared_provider(id: &str) -> DeclaredExtensionProvider {
    DeclaredExtensionProvider {
        id: id.to_string(),
        display_name: "Provider".to_string(),
        description: "declared provider".to_string(),
        models: vec![DeclaredExtensionProviderModel {
            id: "model-1".to_string(),
            display_name: None,
            capabilities: json!({"tool_use": false}),
            context_window: Some(8192),
        }],
        config_schema: None,
    }
}

#[test]
fn earliest_trigger_matrix_never_uses_tool_search_alone() {
    let mut mixed: ExtensionManifest = serde_json::from_value(json!({
        "runtime": "process",
        "command": "/bin/false",
        "permissions": ["tools.register"],
        "hooks": [{"hook": "before_tool_call"}]
    }))
    .unwrap();
    mixed.deferred = Some(DeferredDeclarations {
        tools: vec![declared("t")],
        providers: vec![],
        context_providers: vec![],
        lifecycle: None,
    });
    assert_eq!(classify(&mixed), ExtensionClass::Mixed);
    assert_eq!(
        earliest_trigger(&mixed),
        ActivationTrigger::FirstAuthorizedHookEvent
    );
    let mut tp = base_manifest();
    tp.deferred = Some(DeferredDeclarations {
        tools: vec![declared("t")],
        providers: vec![declared_provider("p")],
        context_providers: vec![],
        lifecycle: None,
    });
    assert_eq!(classify(&tp), ExtensionClass::Mixed);
    assert_eq!(earliest_trigger(&tp), ActivationTrigger::ProviderSelection);
    assert_eq!(
        earliest_trigger(&with_deferred(vec![declared("t")], None)),
        ActivationTrigger::ExactToolActivation
    );
    assert_eq!(earliest_trigger(&base_manifest()), ActivationTrigger::Eager);
}

#[test]
fn provider_declarations_are_deeply_bounded_and_typed() {
    let ok = |p: DeclaredExtensionProvider| {
        let mut m = base_manifest();
        // Review fix A1: passive provider declarations now REQUIRE the
        // exact `providers.register` permission during validation.
        m.permissions.push("providers.register".to_string());
        m.deferred = Some(DeferredDeclarations {
            tools: vec![],
            providers: vec![p],
            context_providers: vec![],
            lifecycle: None,
        });
        m.validate("plug")
    };
    ok(declared_provider("prov")).unwrap();

    let mut m = base_manifest();
    m.permissions.push("providers.register".to_string());
    m.deferred = Some(DeferredDeclarations {
        tools: vec![],
        providers: vec![declared_provider("dup"), declared_provider("dup")],
        context_providers: vec![],
        lifecycle: None,
    });
    assert!(m.validate("plug").is_err());

    let mut p = declared_provider("has:colon");
    assert!(ok(p.clone()).is_err());
    p = declared_provider("prov");
    p.models.clear();
    assert!(ok(p.clone()).is_err());
    p = declared_provider("prov");
    p.models.push(p.models[0].clone());
    assert!(ok(p.clone()).is_err());
    p = declared_provider("prov");
    p.models[0].capabilities = json!({"tool_use": "yes"});
    assert!(ok(p.clone()).is_err());
    p = declared_provider("prov");
    p.config_schema = Some(json!("not an object"));
    assert!(ok(p).is_err());
}

#[test]
fn user_lifecycle_conflicts_with_active_capabilities_fail_closed() {
    let m = with_deferred(vec![declared("t")], Some(DeferredLifecycle::User));
    assert!(m.validate("plug").is_err());
    let mut m = base_manifest();
    m.deferred = Some(DeferredDeclarations {
        tools: vec![],
        providers: vec![declared_provider("p")],
        context_providers: vec![],
        lifecycle: Some(DeferredLifecycle::User),
    });
    assert!(m.validate("plug").is_err());
    with_deferred(vec![], Some(DeferredLifecycle::User))
        .validate("plug")
        .unwrap();
}

#[test]
fn colon_tool_names_and_digest_metadata() {
    assert!(with_deferred(vec![declared("evil:name")], None)
        .validate("plug")
        .is_err());
    let d = declared("search");
    assert_eq!(d.schema_digest(), SchemaDigest::of_schema(&schema()));
    let m = with_deferred(vec![declared("search")], None);
    assert!(dormant_extension_tools("bad:plugin", &m).is_empty());
}

#[test]
fn runtime_description_mismatch_and_unbounded_registered_specs_fail() {
    let decl = vec![declared("search")];
    let renamed = vec![RegisteredExtensionToolSpec {
        name: "search".to_string(),
        description: "different description".to_string(),
        input_schema: schema(),
    }];
    assert_eq!(
        validate_runtime_tool_declarations(&decl, &renamed),
        Err("registered_tool_description_mismatch")
    );
    let hostile = vec![RegisteredExtensionToolSpec {
        name: "bad\u{7}".to_string(),
        description: "declared search".to_string(),
        input_schema: schema(),
    }];
    assert!(validate_runtime_tool_declarations(&decl, &hostile).is_err());
}

// keep unused-import lints honest across cfg branches
#[allow(dead_code)]
fn _assert_runtime_enum(m: &ExtensionManifest) -> &ExtensionRuntime {
    &m.runtime
}

// ── review fix A1/A2: permission + hook coupling fail closed pre-spawn ──────

/// A1: deferred tools are a future `tools.register` surface, so a manifest
/// without that permission must fail validation (and the manager load)
/// BEFORE any spawn or catalog registration.
#[tokio::test]
async fn deferred_tools_without_tools_register_permission_fail_before_spawn() {
    use agent_engine::extensions::hooks::HookBus;
    use agent_engine::extensions::manager::ExtensionManager;

    let dir = std::env::temp_dir().join(format!("synaps-ext-a1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("spawn.marker");

    // `memory.read` satisfies the legacy hook-or-capability gate, so the
    // failure below is specifically the new deferred permission coupling.
    let mut m: ExtensionManifest = serde_json::from_value(json!({
        "runtime": "process",
        "command": "/bin/sh",
        "permissions": ["memory.read"]
    }))
    .unwrap();
    m.args = vec!["-c".to_string(), format!("echo x >> {}", marker.display())];
    m.deferred = Some(DeferredDeclarations {
        tools: vec![declared("search")],
        providers: vec![],
        context_providers: vec![],
        lifecycle: None,
    });

    let err = m.validate("plug").expect_err("must fail closed");
    assert!(err.contains("tools_register"), "{err}");

    // No dormant descriptors are ever minted for the unauthorized manifest.
    assert!(dormant_extension_tools("plug", &m).is_empty());

    // Manager load fails BEFORE spawn and BEFORE catalog registration,
    // with and without progressive deferral.
    for progressive in [true, false] {
        let registry = std::sync::Arc::new(tokio::sync::RwLock::new(ToolRegistry::new()));
        let mut mgr = ExtensionManager::new_with_tools(
            std::sync::Arc::new(HookBus::new()),
            std::sync::Arc::clone(&registry),
        );
        mgr.set_progressive_deferral(progressive);
        assert!(mgr.load("plug", &m).await.is_err());
        assert!(!marker.exists(), "validation must reject before any spawn");
        assert!(!mgr.is_deferred_tool_only("plug"));
        assert!(registry
            .read()
            .await
            .catalog()
            .get(&ToolId::extension("plug", "search"))
            .is_none());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A1 (providers): deferred provider metadata requires `providers.register`.
#[test]
fn deferred_providers_without_providers_register_permission_fail_closed() {
    let mut m = base_manifest(); // permissions: ["tools.register"] only
    m.deferred = Some(DeferredDeclarations {
        tools: vec![],
        providers: vec![declared_provider("prov")],
        context_providers: vec![],
        lifecycle: None,
    });
    let err = m.validate("plug").expect_err("must fail closed");
    assert!(err.contains("providers_register"), "{err}");
    // Granting the exact permission fixes exactly this failure.
    m.permissions.push("providers.register".to_string());
    m.validate("plug").unwrap();
}

/// A2: `lifecycle = "hook"` with no manifest hook subscription has no
/// authorized trigger — it must fail closed, not linger untriggerable.
#[test]
fn hook_lifecycle_without_hook_subscriptions_fails_closed() {
    let mut m: ExtensionManifest = serde_json::from_value(json!({
        "runtime": "process",
        "command": "/bin/false",
        "permissions": ["memory.read"]
    }))
    .unwrap();
    m.deferred = Some(DeferredDeclarations {
        tools: vec![],
        providers: vec![],
        context_providers: vec![],
        lifecycle: Some(DeferredLifecycle::Hook),
    });
    let err = m.validate("plug").expect_err("must fail closed");
    assert!(err.contains("hook"), "{err}");

    // A REAL authorized subscription makes the same lifecycle valid.
    let mut hooked: ExtensionManifest = serde_json::from_value(json!({
        "runtime": "process",
        "command": "/bin/false",
        "permissions": ["tools.intercept"],
        "hooks": [{"hook": "before_tool_call"}]
    }))
    .unwrap();
    hooked.deferred = Some(DeferredDeclarations {
        tools: vec![],
        providers: vec![],
        context_providers: vec![],
        lifecycle: Some(DeferredLifecycle::Hook),
    });
    hooked.validate("plug").unwrap();
    assert_eq!(classify(&hooked), ExtensionClass::HookLifecycle);
    assert_eq!(
        earliest_trigger(&hooked),
        ActivationTrigger::FirstAuthorizedHookEvent
    );
}

// ── manager boot gating: zero spawn under progressive deferral ──────────────

#[tokio::test]
async fn manager_defers_tool_only_extension_without_spawn_and_eager_flag_off_spawns() {
    use agent_engine::extensions::hooks::HookBus;
    use agent_engine::extensions::manager::ExtensionManager;

    let dir = std::env::temp_dir().join(format!("synaps-ext-mgr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("spawn.marker");

    let mut m = with_deferred(vec![declared("search")], None);
    m.command = "/bin/sh".to_string();
    m.args = vec!["-c".to_string(), format!("echo x >> {}", marker.display())];

    // Progressive: deferred — zero spawn, dormant descriptor cataloged,
    // launch record retained, no live handler.
    let registry = std::sync::Arc::new(tokio::sync::RwLock::new(ToolRegistry::new()));
    let mut mgr = ExtensionManager::new_with_tools(
        std::sync::Arc::new(HookBus::new()),
        std::sync::Arc::clone(&registry),
    );
    mgr.set_progressive_deferral(true);
    mgr.load("fixture-plugin", &m).await.unwrap();
    assert!(
        !marker.exists(),
        "progressive tool-only load must not spawn"
    );
    assert_eq!(mgr.count(), 0, "no live handler for a deferred extension");
    assert!(mgr.is_deferred_tool_only("fixture-plugin"));
    assert_eq!(mgr.deferred_tool_only_count(), 1);
    assert!(registry
        .read()
        .await
        .catalog()
        .get(&ToolId::extension("fixture-plugin", "search"))
        .is_some());

    // Flag-off: the SAME manifest takes the legacy eager path — the spawn
    // is attempted (marker appears) even though the spy script is not a
    // real extension and the load ultimately fails initialize.
    let registry2 = std::sync::Arc::new(tokio::sync::RwLock::new(ToolRegistry::new()));
    let mut eager = ExtensionManager::new_with_tools(
        std::sync::Arc::new(HookBus::new()),
        std::sync::Arc::clone(&registry2),
    );
    eager.set_progressive_deferral(false);
    let result = eager.load("fixture-plugin", &m).await;
    assert!(
        marker.exists(),
        "flag-off must remain eager (spawn attempted)"
    );
    assert!(result.is_err(), "spy script is not a real extension");
    let _ = std::fs::remove_dir_all(&dir);
}
