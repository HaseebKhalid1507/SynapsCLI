//! Task 14 fix 1 — source-aware `ToolId` construction, structural
//! `SchemaDigest`, and checked `CatalogGeneration` advancement.
//!
//! Actual runtime identities (extension runtime names `<plugin_id>:<tool>`,
//! uppercase/Unicode plugin ids, MCP server/tool pairs) must be representable
//! exactly through deterministic alias-safe canonical encoding, while the
//! external `ToolId::parse` boundary stays strict and fail-closed.

use agent_core::orchestration::capability::{
    CatalogGeneration, SchemaDigest, ToolId, TOOL_ID_MAX_BYTES, TOOL_ID_NAMESPACE_MAX_BYTES,
    TOOL_ID_NAME_MAX_BYTES,
};

// ── Source-aware constructors ───────────────────────────────────────────────

#[test]
fn source_constructors_produce_parseable_canonical_ids() {
    let ids = [
        ToolId::builtin("bash"),
        ToolId::extension("my-plugin", "do_thing"),
        ToolId::mcp("server-1", "list_issues"),
        ToolId::plugin("planner", "outline"),
        ToolId::unclassified("mystery"),
    ];
    for id in &ids {
        let reparsed = ToolId::parse(id.as_str())
            .unwrap_or_else(|e| panic!("constructed id {id} must round-trip parse: {e}"));
        assert_eq!(&reparsed, id);
    }
    assert_eq!(ids[0].as_str(), "builtin:bash");
    assert_eq!(ids[1].namespace(), "ext.my-plugin");
    assert_eq!(ids[2].namespace(), "mcp.server-1");
    assert_eq!(ids[3].namespace(), "plugin.planner");
    assert_eq!(ids[4].as_str(), "unknown:mystery");
}

#[test]
fn source_constructor_namespaces_are_disjoint_for_identical_raw_names() {
    let same = "alpha";
    let ids = [
        ToolId::builtin(same),
        ToolId::extension(same, same),
        ToolId::mcp(same, same),
        ToolId::plugin(same, same),
        ToolId::unclassified(same),
    ];
    for (i, a) in ids.iter().enumerate() {
        for b in ids.iter().skip(i + 1) {
            assert_ne!(a, b, "sources must never collide: {a} vs {b}");
        }
    }
}

#[test]
fn uppercase_and_unicode_identities_are_encoded_not_rejected_and_alias_safe() {
    // Actual extension ids permit uppercase and non-ASCII; they must be
    // representable deterministically without sanitization collapse.
    let upper = ToolId::extension("MyPlugin", "Do Thing");
    let lower = ToolId::extension("myplugin", "do thing");
    let unicode = ToolId::extension("caf\u{e9}", "outil");
    assert_ne!(upper, lower, "case must not collapse into one identity");
    assert_ne!(upper, unicode);
    for id in [&upper, &lower, &unicode] {
        assert!(
            ToolId::parse(id.as_str()).is_ok(),
            "encoded id {id} must stay canonical"
        );
    }
    // Deterministic: same raw identity always encodes identically.
    assert_eq!(upper, ToolId::extension("MyPlugin", "Do Thing"));
}

#[test]
fn sanitization_style_alias_pairs_stay_distinct() {
    // "Bad Name!" and "Bad_Name_" collapse under API sanitization; the
    // catalog identity must keep them distinct.
    let a = ToolId::unclassified("Bad Name!");
    let b = ToolId::unclassified("Bad_Name_");
    assert_ne!(a, b);
    // Reserved-prefix forgery: a raw name that spells the encoded form of
    // another name must not collide with it.
    let c = ToolId::unclassified("weird");
    let forged = ToolId::unclassified(c.name());
    if c.name() != "weird" {
        assert_ne!(c, forged);
    }
}

#[test]
fn oversized_identities_stay_bounded_and_deterministic() {
    let huge_ns = "N".repeat(4096);
    let huge_name = "\u{03b1}".repeat(4096);
    let id = ToolId::extension(&huge_ns, &huge_name);
    assert!(id.as_str().len() <= TOOL_ID_MAX_BYTES);
    assert!(id.namespace().len() <= TOOL_ID_NAMESPACE_MAX_BYTES);
    assert!(id.name().len() <= TOOL_ID_NAME_MAX_BYTES);
    assert!(ToolId::parse(id.as_str()).is_ok());
    assert_eq!(id, ToolId::extension(&huge_ns, &huge_name));
    // A different oversized identity must not collapse into the same id.
    let other = ToolId::extension(&huge_ns, &"\u{03b2}".repeat(4096));
    assert_ne!(id, other);
}

#[test]
fn external_parse_boundary_remains_strict() {
    assert!(ToolId::parse("builtin:Bash").is_err());
    assert!(ToolId::parse("").is_err());
    let oversized = format!("builtin:{}", "a".repeat(TOOL_ID_MAX_BYTES));
    assert!(ToolId::parse(&oversized).is_err());
}

// ── Structural schema digest ────────────────────────────────────────────────

#[test]
fn schema_digest_uses_type_and_length_framing() {
    use serde_json::json;
    // Length framing: moving a byte across a boundary must change the digest.
    assert_ne!(
        SchemaDigest::of_schema(&json!({"a": "bc"})),
        SchemaDigest::of_schema(&json!({"ab": "c"}))
    );
    // Type framing: string "1" vs number 1.
    assert_ne!(
        SchemaDigest::of_schema(&json!({"a": "1"})),
        SchemaDigest::of_schema(&json!({"a": 1}))
    );
    // Structure framing: ["ab"] vs ["a","b"].
    assert_ne!(
        SchemaDigest::of_schema(&json!(["ab"])),
        SchemaDigest::of_schema(&json!(["a", "b"]))
    );
    // Determinism for identical values.
    assert_eq!(
        SchemaDigest::of_schema(&json!({"b": 1, "a": [true, null]})),
        SchemaDigest::of_schema(&json!({"b": 1, "a": [true, null]}))
    );
}

#[test]
fn schema_digest_is_safe_on_deeply_nested_values() {
    // Programmatically built values can nest far deeper than serde_json's
    // parser allows; digesting must not overflow the stack.
    let mut value = serde_json::Value::Null;
    for _ in 0..200_000 {
        value = serde_json::Value::Array(vec![value]);
    }
    let a = SchemaDigest::of_schema(&value);
    let b = SchemaDigest::of_schema(&value);
    assert_eq!(a, b);
    // Tear the fixture down iteratively: serde_json's recursive `Drop` would
    // otherwise overflow the stack in the test itself.
    let mut current = value;
    while let serde_json::Value::Array(mut items) = current {
        current = items.pop().unwrap_or(serde_json::Value::Null);
    }
}

// ── Checked generation advancement ──────────────────────────────────────────

#[test]
fn generation_checked_next_fails_typed_at_the_boundary() {
    let max = CatalogGeneration::new(u64::MAX);
    assert!(
        max.checked_next().is_err(),
        "u64::MAX must not wrap or stick"
    );
    let almost = CatalogGeneration::new(u64::MAX - 1);
    let next = almost
        .checked_next()
        .expect("u64::MAX - 1 advances exactly once more");
    assert_eq!(next.value(), u64::MAX);
}
