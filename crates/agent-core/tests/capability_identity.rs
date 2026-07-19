//! Task 14 — typed capability identities (spec §4.1, §12).
//!
//! Boundary parsing of `ToolId` must fail closed with typed errors for
//! malformed and oversized input; `CatalogGeneration` and `SchemaDigest`
//! must behave deterministically; `SessionActivationGrant` binds an exact
//! session/tool/generation/digest tuple.

use agent_core::orchestration::capability::{
    ActivationGrantError, CatalogGeneration, SchemaDigest, SessionActivationGrant, ToolId,
    ToolIdError, TOOL_ID_MAX_BYTES,
};

// ── ToolId boundary parsing ─────────────────────────────────────────────────

#[test]
fn tool_id_parses_canonical_namespaced_forms() {
    for raw in [
        "builtin:bash",
        "builtin:subagent_model_authorize",
        "extension.my_ext:send_channel",
        "mcp.server-1:list_issues",
        "plugin.x:tool.v2",
    ] {
        let id = ToolId::parse(raw).unwrap_or_else(|e| panic!("{raw} should parse: {e}"));
        assert_eq!(id.as_str(), raw);
    }
    let id = ToolId::parse("builtin:bash").unwrap();
    assert_eq!(id.namespace(), "builtin");
    assert_eq!(id.name(), "bash");
}

#[test]
fn tool_id_rejects_empty_and_missing_namespace() {
    assert_eq!(ToolId::parse(""), Err(ToolIdError::Empty));
    assert_eq!(ToolId::parse("bash"), Err(ToolIdError::MissingNamespace));
}

#[test]
fn tool_id_rejects_oversized_input_with_typed_error() {
    let oversized = format!("builtin:{}", "a".repeat(TOOL_ID_MAX_BYTES));
    match ToolId::parse(&oversized) {
        Err(ToolIdError::Oversized { actual, limit }) => {
            assert_eq!(actual, oversized.len());
            assert_eq!(limit, TOOL_ID_MAX_BYTES);
        }
        other => panic!("expected Oversized, got {other:?}"),
    }
}

#[test]
fn tool_id_rejects_alias_prone_and_malformed_segments() {
    // No sanitization at the boundary: anything non-canonical fails closed,
    // so two distinct raw spellings can never collapse into one identity.
    for raw in [
        "builtin:", // empty name
        ":bash",    // empty namespace
        "builtin:Bash",
        "BUILTIN:bash",
        "builtin:ba sh",
        "builtin:ba/sh",
        "builtin:bash\u{0000}",
        "builtin:bäsh",
        "builtin:bash:extra",
        "builtin::bash",
        "builtin:-leading-dash",
        ".dot:bash",
    ] {
        assert!(
            ToolId::parse(raw).is_err(),
            "{raw:?} must be rejected at the boundary"
        );
    }
}

// ── CatalogGeneration ───────────────────────────────────────────────────────

#[test]
fn catalog_generation_increments_monotonically() {
    let g0 = CatalogGeneration::initial();
    let g1 = g0.checked_next().expect("no overflow at 0");
    let g2 = g1.checked_next().expect("no overflow at 1");
    assert_eq!(g0.value(), 0);
    assert_eq!(g1.value(), 1);
    assert_eq!(g2.value(), 2);
    assert!(g0 < g1 && g1 < g2);
}

// ── SchemaDigest determinism ────────────────────────────────────────────────

#[test]
fn schema_digest_is_deterministic_and_content_sensitive() {
    let schema_a = serde_json::json!({
        "type": "object",
        "properties": { "path": {"type": "string"}, "limit": {"type": "integer"} }
    });
    let schema_a_again = serde_json::json!({
        "type": "object",
        "properties": { "limit": {"type": "integer"}, "path": {"type": "string"} }
    });
    let schema_b = serde_json::json!({ "type": "object" });

    let d1 = SchemaDigest::of_schema(&schema_a);
    let d2 = SchemaDigest::of_schema(&schema_a_again);
    let d3 = SchemaDigest::of_schema(&schema_b);
    assert_eq!(d1, d2, "equal schema content must digest identically");
    assert_ne!(d1, d3, "different schema content must digest differently");
    assert_eq!(d1.as_hex().len(), 64);
    assert!(d1.as_hex().chars().all(|c| c.is_ascii_hexdigit()));
}

// ── SessionActivationGrant ──────────────────────────────────────────────────

#[test]
fn activation_grant_binds_exact_session_tool_generation_and_digest() {
    let tool_id = ToolId::parse("builtin:bash").unwrap();
    let generation = CatalogGeneration::initial().checked_next().unwrap();
    let digest = SchemaDigest::of_schema(&serde_json::json!({"type": "object"}));
    let grant =
        SessionActivationGrant::new("session-1", tool_id.clone(), generation, digest.clone())
            .unwrap();

    assert!(grant.covers("session-1", &tool_id, generation, &digest));

    let other_tool = ToolId::parse("builtin:read").unwrap();
    let other_digest = SchemaDigest::of_schema(&serde_json::json!({"type": "string"}));
    assert!(!grant.covers("session-2", &tool_id, generation, &digest));
    assert!(!grant.covers("session-1", &other_tool, generation, &digest));
    assert!(!grant.covers(
        "session-1",
        &tool_id,
        generation.checked_next().unwrap(),
        &digest
    ));
    assert!(!grant.covers("session-1", &tool_id, generation, &other_digest));
}

#[test]
fn activation_grant_rejects_invalid_session_ids() {
    let tool_id = ToolId::parse("builtin:bash").unwrap();
    let generation = CatalogGeneration::initial();
    let digest = SchemaDigest::of_schema(&serde_json::json!({"type": "object"}));
    assert_eq!(
        SessionActivationGrant::new("", tool_id.clone(), generation, digest.clone())
            .expect_err("empty session id must fail closed"),
        ActivationGrantError::EmptySessionId,
    );
    let oversized = "s".repeat(4096);
    assert!(matches!(
        SessionActivationGrant::new(&oversized, tool_id, generation, digest),
        Err(ActivationGrantError::OversizedSessionId { .. })
    ));
}
