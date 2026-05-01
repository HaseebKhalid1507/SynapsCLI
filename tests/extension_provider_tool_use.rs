use serde_json::json;

use synaps_cli::extensions::runtime::process::{
    extract_provider_tool_uses,
    ProviderToolUse,
};

#[test]
fn extracts_anthropic_tool_use_blocks_from_provider_content() {
    let content = vec![
        json!({"type": "text", "text": "checking"}),
        json!({
            "type": "tool_use",
            "id": "call-1",
            "name": "read",
            "input": {"path": "Cargo.toml"}
        }),
    ];

    let tool_uses = extract_provider_tool_uses(&content).expect("valid tool use blocks");

    assert_eq!(tool_uses, vec![ProviderToolUse {
        id: "call-1".to_string(),
        name: "read".to_string(),
        input: json!({"path": "Cargo.toml"}),
    }]);
}

#[test]
fn rejects_provider_tool_use_without_required_fields() {
    let content = vec![json!({
        "type": "tool_use",
        "id": "call-1",
        "input": {"path": "Cargo.toml"}
    })];

    let err = extract_provider_tool_uses(&content).unwrap_err();

    assert!(err.contains("missing name"));
}
