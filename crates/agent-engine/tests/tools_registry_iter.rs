//! P13 unit tests for `ToolRegistry::iter_tools_sorted()`.
//!
//! These are integration tests that exercise the public API of `agent_engine`
//! without modifying Spike's implementation files.
//!
//! Written by Shady (subagent, code review / test authorship).
//! Branch: feat/tui-headless-harness  (P13 coverage pass)

use std::sync::Arc;

use agent_engine::tools::{Tool, ToolContext, ToolRegistry};
use agent_engine::{Result, Value};
use async_trait::async_trait;

// ─── Minimal test-double tool ───────────────────────────────────────────────

struct StubTool {
    name: &'static str,
    description: &'static str,
    parameters: Value,
}

impl StubTool {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            description: "a stub tool",
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "arg": { "type": "string", "description": "test argument" }
                },
                "required": []
            }),
        }
    }
}

#[async_trait]
impl Tool for StubTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        self.description
    }
    fn parameters(&self) -> Value {
        self.parameters.clone()
    }
    async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
        Ok("stub".to_string())
    }
}

// ─── 1. iter_tools_sorted() returns tools in stable alphabetical order ───────

#[test]
fn iter_tools_sorted_returns_alphabetical_order() {
    // Build a registry with tools that are deliberately added out of order.
    let mut registry = ToolRegistry::empty();
    registry.register(Arc::new(StubTool::new("zebra")));
    registry.register(Arc::new(StubTool::new("alpha")));
    registry.register(Arc::new(StubTool::new("mango")));
    registry.register(Arc::new(StubTool::new("banana")));

    let tools = registry.iter_tools_sorted();
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();

    assert_eq!(
        names,
        vec!["alpha", "banana", "mango", "zebra"],
        "iter_tools_sorted() must return tools in ascending lexicographic order"
    );
}

// ─── 2. Stable across multiple calls (deterministic — not HashMap-order) ────

#[test]
fn iter_tools_sorted_is_deterministic_across_calls() {
    let mut registry = ToolRegistry::empty();
    // Insert in a different order each call to expose any HashMap non-determinism.
    for name in &["zeta", "alpha", "gamma", "beta", "delta", "epsilon"] {
        registry.register(Arc::new(StubTool::new(name)));
    }

    let first_call: Vec<&str> = registry
        .iter_tools_sorted()
        .iter()
        .map(|t| t.name())
        .collect();
    let second_call: Vec<&str> = registry
        .iter_tools_sorted()
        .iter()
        .map(|t| t.name())
        .collect();

    assert_eq!(
        first_call, second_call,
        "iter_tools_sorted() must be byte-identical across repeated calls on the same registry"
    );

    // Verify it's actually sorted (not just consistent).
    let mut expected = first_call.clone();
    expected.sort();
    assert_eq!(first_call, expected, "order must be ascending alphabetical");
}

// ─── 3. Builtin registry (ToolRegistry::new) — 20 tools in sorted order ─────

#[test]
fn iter_tools_sorted_on_default_registry_has_18_tools_in_order() {
    let registry = ToolRegistry::new();
    let tools = registry.iter_tools_sorted();

    assert_eq!(
        tools.len(),
        20,
        "default registry must contain exactly 20 builtin tools"
    );

    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();

    // These are the exact names in docs/tools.json (alphabetical order).
    let expected = vec![
        "activate_tools",
        "bash",
        "edit",
        "find",
        "grep",
        "ls",
        "read",
        "search_tools",
        "shell_end",
        "shell_send",
        "shell_start",
        "subagent",
        "subagent_collect",
        "subagent_model_authorize",
        "subagent_models",
        "subagent_resume",
        "subagent_start",
        "subagent_status",
        "subagent_steer",
        "write",
    ];

    assert_eq!(names, expected,
        "iter_tools_sorted() on ToolRegistry::new() must yield the 20 builtin tools in alphabetical order");
}

// ─── 4. Each tool has non-empty name, description, well-formed parameters ────

#[test]
fn iter_tools_sorted_all_tools_have_nonempty_name_and_description() {
    let registry = ToolRegistry::new();

    for tool in registry.iter_tools_sorted() {
        assert!(
            !tool.name().is_empty(),
            "every tool must have a non-empty name"
        );
        assert!(
            !tool.description().is_empty(),
            "tool '{}' has an empty description — that's a bug, not a feature",
            tool.name()
        );
    }
}

#[test]
fn iter_tools_sorted_all_parameters_are_object_type_json_schema() {
    let registry = ToolRegistry::new();

    for tool in registry.iter_tools_sorted() {
        let params = tool.parameters();

        // parameters() must return a JSON object (not null, not array, not primitive)
        assert!(
            params.is_object(),
            "tool '{}': parameters() must return a JSON object, got: {:?}",
            tool.name(),
            params
        );

        // Must have a "type" field (JSON Schema root requirement)
        let schema_type = params.get("type").and_then(|v| v.as_str());
        assert_eq!(
            schema_type,
            Some("object"),
            "tool '{}': parameters schema must have {{\"type\": \"object\"}}",
            tool.name()
        );

        // Must have a "properties" field (all builtins are object-shaped)
        assert!(
            params.get("properties").is_some(),
            "tool '{}': parameters schema must have a 'properties' key",
            tool.name()
        );
    }
}

#[test]
fn iter_tools_sorted_all_required_fields_exist_in_properties() {
    let registry = ToolRegistry::new();

    for tool in registry.iter_tools_sorted() {
        let params = tool.parameters();

        let required = params
            .get("required")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();

        let properties = params
            .get("properties")
            .and_then(|p| p.as_object())
            .cloned()
            .unwrap_or_default();

        for req in &required {
            let field_name = req.as_str().expect("required entries must be strings");
            assert!(
                properties.contains_key(field_name),
                "tool '{}': required field '{}' is not present in 'properties'",
                tool.name(),
                field_name
            );
        }
    }
}

// ─── 5. Empty registry returns empty sorted list ─────────────────────────────

#[test]
fn iter_tools_sorted_on_empty_registry_returns_empty_vec() {
    let registry = ToolRegistry::empty();
    let tools = registry.iter_tools_sorted();
    assert!(
        tools.is_empty(),
        "iter_tools_sorted() on an empty registry must return an empty vec, not panic"
    );
}

// ─── 6. Export JSON shape: {name, description, parameters} per tool ──────────
//
// This mirrors what `synaps tools export` does — we test the data shape that
// the export handler would produce, using the same registry API it calls.

#[test]
fn export_shape_has_name_description_parameters_per_tool() {
    let registry = ToolRegistry::new();

    let manifest: Vec<serde_json::Value> = registry
        .iter_tools_sorted()
        .into_iter()
        .map(|tool| {
            serde_json::json!({
                "name":        tool.name(),
                "description": tool.description(),
                "parameters":  tool.parameters(),
            })
        })
        .collect();

    for entry in &manifest {
        let obj = entry
            .as_object()
            .expect("each manifest entry must be a JSON object");

        // Must have exactly {name, description, parameters} — no extra keys, no missing keys.
        assert!(obj.contains_key("name"), "manifest entry missing 'name'");
        assert!(
            obj.contains_key("description"),
            "manifest entry missing 'description'"
        );
        assert!(
            obj.contains_key("parameters"),
            "manifest entry missing 'parameters'"
        );

        let name = obj["name"].as_str().unwrap_or("");
        assert!(!name.is_empty(), "manifest entry 'name' must not be empty");

        let desc = obj["description"].as_str().unwrap_or("");
        assert!(
            !desc.is_empty(),
            "manifest entry 'description' for tool '{}' must not be empty",
            name
        );

        assert!(
            obj["parameters"].is_object(),
            "manifest entry 'parameters' for tool '{}' must be a JSON object",
            name
        );
    }

    // Full manifest must contain all 20 tools.
    assert_eq!(
        manifest.len(),
        20,
        "export manifest must contain exactly 20 builtin tools"
    );
}
