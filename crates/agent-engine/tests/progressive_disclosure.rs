//! Task 18 — opt-in minimal first-request tool schemas.

use std::sync::Arc;

use agent_engine::tools::activation::{activate_exact_for_user, SessionId, SessionToolSet};
use agent_engine::tools::catalog::ToolId;
use agent_engine::tools::{Tool, ToolContext, ToolOrigin, ToolRegistry};
use agent_engine::{Result, Value};
use async_trait::async_trait;
use serde_json::json;

struct InertTool {
    name: String,
    origin: ToolOrigin,
    marker: String,
}

impl InertTool {
    fn builtin(name: &str) -> Self {
        Self {
            name: name.to_string(),
            origin: ToolOrigin::Builtin,
            marker: format!("schema-marker-{name}"),
        }
    }

    fn extension(name: &str) -> Self {
        Self {
            name: name.to_string(),
            origin: ToolOrigin::Extension {
                extension_id: "fixture-extension".to_string(),
            },
            marker: format!("extension-marker-{name}"),
        }
    }

    fn mcp(name: &str) -> Self {
        Self {
            name: name.to_string(),
            origin: ToolOrigin::Mcp {
                server_id: "fixture-server".to_string(),
                server_tool_name: name.to_string(),
            },
            marker: format!("mcp-marker-{name}"),
        }
    }
}

#[async_trait]
impl Tool for InertTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "inert progressive-disclosure fixture"
    }

    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{&self.marker:{"type":"string"}}})
    }

    fn origin(&self) -> ToolOrigin {
        self.origin.clone()
    }

    async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
        Ok("not executed".to_string())
    }
}

fn sid() -> SessionId {
    SessionId::parse("task18-progressive").unwrap()
}

fn registry_with_dormant(count: usize) -> ToolRegistry {
    let mut tools: Vec<Arc<dyn Tool>> = [
        "bash",
        "read",
        "write",
        "edit",
        "grep",
        "find",
        "ls",
        "search_tools",
        "activate_tools",
        "search_skills",
        "load_skill",
        "subagent_start",
        "shell_start",
    ]
    .into_iter()
    .map(|name| Arc::new(InertTool::builtin(name)) as Arc<dyn Tool>)
    .collect();
    tools.push(Arc::new(InertTool::extension("fixture-extension:remote")));
    tools.push(Arc::new(InertTool::mcp("ext__fixture__remote")));
    for i in 0..count {
        tools.push(Arc::new(InertTool::builtin(&format!("dormant_{i:04}"))));
    }
    ToolRegistry::from_tools_for_tests(tools)
}

fn bytes(value: &[Value]) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}

#[test]
fn progressive_core_is_exact_and_excludes_specialized_sources() {
    let registry = registry_with_dormant(10);
    let set = SessionToolSet::progressive_core_for_catalog(sid(), registry.catalog());
    let projected = registry.session_tools_schema(&set).unwrap();
    let names: Vec<_> = projected
        .iter()
        .filter_map(|schema| schema["name"].as_str())
        .collect();

    assert_eq!(
        names,
        vec![
            "activate_tools",
            "bash",
            "edit",
            "find",
            "grep",
            "load_skill",
            "ls",
            "read",
            "search_skills",
            "search_tools",
            "write",
        ]
    );
    for absent in [
        "subagent_start",
        "shell_start",
        "fixture-extension:remote",
        "ext__fixture__remote",
        "dormant_0000",
    ] {
        assert!(!names.contains(&absent), "dormant schema leaked: {absent}");
    }
}

#[test]
fn progressive_first_request_bytes_are_invariant_at_catalog_scale() {
    const BUDGET: usize = 4 * 1024;
    let mut baseline = None;
    for dormant in [10, 100, 500, 1_000, 2_000] {
        let registry = registry_with_dormant(dormant);
        let set = SessionToolSet::progressive_core_for_catalog(sid(), registry.catalog());
        let projected = registry.session_tools_schema(&set).unwrap();
        let encoded = bytes(&projected);
        eprintln!(
            "dormant={dormant} first_request_tool_schema_bytes={}",
            encoded.len()
        );
        assert!(
            encoded.len() <= BUDGET,
            "{dormant} dormant tools produced {} bytes > {BUDGET}",
            encoded.len()
        );
        match &baseline {
            None => baseline = Some(encoded),
            Some(expected) => assert_eq!(
                &encoded, expected,
                "first-request schema changed at {dormant} dormant tools"
            ),
        }
    }
}

#[test]
fn activation_adds_one_exact_schema_to_progressive_projection() {
    let registry = registry_with_dormant(10);
    let mut set = SessionToolSet::progressive_core_for_catalog(sid(), registry.catalog());
    let before = registry.session_tools_schema(&set).unwrap();

    activate_exact_for_user(
        &mut set,
        registry.catalog(),
        &ToolId::builtin("dormant_0003"),
    )
    .unwrap();

    let after = registry.session_tools_schema(&set).unwrap();
    assert_eq!(after.len(), before.len() + 1);
    let names: Vec<_> = after
        .iter()
        .filter_map(|schema| schema["name"].as_str())
        .collect();
    assert!(names.contains(&"dormant_0003"));
    assert!(!names.contains(&"dormant_0002"));
    assert!(!names.contains(&"dormant_0004"));
}

#[test]
fn flag_off_full_schema_path_remains_byte_identical() {
    let registry = registry_with_dormant(100);
    let before = bytes(registry.tools_schema().as_ref());
    let full_set = SessionToolSet::default_core_for_catalog(sid(), registry.catalog());
    let projected_full = registry.session_tools_schema(&full_set).unwrap();
    let after = bytes(registry.tools_schema().as_ref());

    assert_eq!(before, after, "projection mutated the legacy cached schema");
    assert_eq!(before, bytes(&projected_full));
}
