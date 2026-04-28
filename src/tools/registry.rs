//! Tool registry — maintains name→tool map and cached JSON schema for the API.
use std::sync::Arc;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use crate::tools::Tool;

/// Registry of available tools. Maintains a name→tool map and a cached JSON schema
/// array that gets sent to the API. Thread-safe via `Arc<RwLock<ToolRegistry>>`.
#[derive(Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    /// Cached Anthropic-compatible schema — rebuilt on register(), shared via Arc for zero-copy reads.
    cached_schema: Arc<Vec<Value>>,
    /// Mapping from API-safe tool names back to their runtime names.
    api_to_runtime_names: HashMap<String, String>,
    /// Per-tool mapping from API-safe input property names back to runtime names.
    input_name_maps: HashMap<String, SchemaNameMap>,
}

#[derive(Clone, Debug, Default)]
struct SchemaNameMap {
    api_to_runtime: HashMap<String, String>,
    children: HashMap<String, SchemaNameMap>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(crate::tools::bash::BashTool),
            Arc::new(crate::tools::read::ReadTool),
            Arc::new(crate::tools::write::WriteTool),
            Arc::new(crate::tools::edit::EditTool),
            Arc::new(crate::tools::grep::GrepTool),
            Arc::new(crate::tools::find::FindTool),
            Arc::new(crate::tools::ls::LsTool),
            Arc::new(crate::tools::subagent::SubagentTool),
            Arc::new(crate::tools::subagent_start::SubagentStartTool),
            Arc::new(crate::tools::subagent_status::SubagentStatusTool),
            Arc::new(crate::tools::subagent_steer::SubagentSteerTool),
            Arc::new(crate::tools::subagent_collect::SubagentCollectTool),
            Arc::new(crate::tools::subagent_resume::SubagentResumeTool),
            Arc::new(crate::tools::shell::ShellStartTool),
            Arc::new(crate::tools::shell::ShellSendTool),
            Arc::new(crate::tools::shell::ShellEndTool),
        ];
        Self::from_tools(tools)
    }

    /// Registry without subagent tool — used for subagent runtimes to prevent recursion.
    pub fn without_subagent() -> Self {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(crate::tools::bash::BashTool),
            Arc::new(crate::tools::read::ReadTool),
            Arc::new(crate::tools::write::WriteTool),
            Arc::new(crate::tools::edit::EditTool),
            Arc::new(crate::tools::grep::GrepTool),
            Arc::new(crate::tools::find::FindTool),
            Arc::new(crate::tools::ls::LsTool),
            Arc::new(crate::tools::shell::ShellStartTool),
            Arc::new(crate::tools::shell::ShellSendTool),
            Arc::new(crate::tools::shell::ShellEndTool),
        ];
        Self::from_tools(tools)
    }

    fn from_tools(tool_list: Vec<Arc<dyn Tool>>) -> Self {
        let mut registry = ToolRegistry {
            tools: HashMap::new(),
            cached_schema: Arc::new(Vec::new()),
            api_to_runtime_names: HashMap::new(),
            input_name_maps: HashMap::new(),
        };
        for tool in tool_list {
            registry.register(tool);
        }
        registry
    }

    fn api_safe_name(name: &str, used: &HashSet<String>) -> String {
        Self::api_safe_identifier(name, used, 128, false)
    }

    fn api_safe_property_name(name: &str, used: &HashSet<String>) -> String {
        Self::api_safe_identifier(name, used, 64, true)
    }

    fn api_safe_identifier(name: &str, used: &HashSet<String>, max_len: usize, allow_dot: bool) -> String {
        let mut sanitized = String::with_capacity(name.len());
        for ch in name.chars() {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || (allow_dot && ch == '.') {
                sanitized.push(ch);
            } else {
                sanitized.push('_');
            }
        }
        if sanitized.is_empty() {
            sanitized.push_str("field");
        }
        if sanitized.len() > max_len {
            sanitized.truncate(max_len);
        }

        let base = sanitized.clone();
        let mut suffix = 2;
        while used.contains(&sanitized) {
            let suffix_str = format!("_{suffix}");
            let keep = max_len.saturating_sub(suffix_str.len());
            sanitized = format!("{}{}", &base[..base.len().min(keep)], suffix_str);
            suffix += 1;
        }
        sanitized
    }

    fn sanitize_schema(mut schema: Value) -> (Value, SchemaNameMap) {
        let mut map = SchemaNameMap::default();
        let Some(obj) = schema.as_object_mut() else {
            return (schema, map);
        };

        let mut required_name_map = HashMap::new();
        if let Some(props_value) = obj.get_mut("properties") {
            if let Some(props) = props_value.as_object_mut() {
                let original = std::mem::take(props);
                let mut used = HashSet::new();
                for (runtime_name, child_schema) in original {
                    let api_name = Self::api_safe_property_name(&runtime_name, &used);
                    used.insert(api_name.clone());
                    required_name_map.insert(runtime_name.clone(), api_name.clone());
                    map.api_to_runtime.insert(api_name.clone(), runtime_name);

                    let (sanitized_child, child_map) = Self::sanitize_schema(child_schema);
                    if !child_map.api_to_runtime.is_empty() || !child_map.children.is_empty() {
                        map.children.insert(api_name.clone(), child_map);
                    }
                    props.insert(api_name, sanitized_child);
                }
            }
        }

        if let Some(required) = obj.get_mut("required").and_then(Value::as_array_mut) {
            for item in required.iter_mut() {
                if let Some(name) = item.as_str() {
                    if let Some(api_name) = required_name_map.get(name) {
                        *item = Value::String(api_name.clone());
                    }
                }
            }
        }

        // Recurse into array item schemas too; no direct key mapping is needed at this level.
        if let Some(items) = obj.get_mut("items") {
            let (sanitized_items, _) = Self::sanitize_schema(std::mem::take(items));
            *items = sanitized_items;
        }

        (schema, map)
    }

    fn translate_input_names(input: Value, map: &SchemaNameMap) -> Value {
        match input {
            Value::Object(obj) => {
                let mut out = serde_json::Map::new();
                for (api_name, value) in obj {
                    let runtime_name = map.api_to_runtime.get(&api_name).cloned().unwrap_or_else(|| api_name.clone());
                    let value = map.children.get(&api_name)
                        .map(|child| Self::translate_input_names(value.clone(), child))
                        .unwrap_or(value);
                    out.insert(runtime_name, value);
                }
                Value::Object(out)
            }
            Value::Array(arr) => Value::Array(arr.into_iter().map(|v| Self::translate_input_names(v, map)).collect()),
            other => other,
        }
    }

    fn rebuild_schema(&mut self) {
        let mut used = HashSet::new();
        let mut api_to_runtime_names = HashMap::new();
        let mut input_name_maps = HashMap::new();
        let mut schema = Vec::with_capacity(self.tools.len());

        for tool in self.tools.values() {
            let runtime_name = tool.name();
            let api_name = Self::api_safe_name(runtime_name, &used);
            used.insert(api_name.clone());
            api_to_runtime_names.insert(api_name.clone(), runtime_name.to_string());
            let (input_schema, input_map) = Self::sanitize_schema(tool.parameters());
            input_name_maps.insert(api_name.clone(), input_map);
            schema.push(serde_json::json!({
                "name": api_name,
                "description": tool.description(),
                "input_schema": input_schema
            }));
        }

        self.api_to_runtime_names = api_to_runtime_names;
        self.input_name_maps = input_name_maps;
        self.cached_schema = Arc::new(schema);
    }

    /// Register an additional tool at runtime (e.g. MCP tools, custom tools).
    /// If a tool with the same name exists, it is replaced.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.insert(name, tool);
        self.rebuild_schema();
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        let runtime_name = self.api_to_runtime_names.get(name).map(String::as_str).unwrap_or(name);
        self.tools.get(runtime_name)
    }

    pub fn runtime_name_for_api<'a>(&'a self, name: &'a str) -> &'a str {
        self.api_to_runtime_names.get(name).map(String::as_str).unwrap_or(name)
    }

    pub fn translate_input_for_api_tool(&self, tool_name: &str, input: Value) -> Value {
        self.input_name_maps.get(tool_name)
            .map(|map| Self::translate_input_names(input.clone(), map))
            .unwrap_or(input)
    }

    pub fn tools_schema(&self) -> Arc<Vec<Value>> {
        Arc::clone(&self.cached_schema)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Result, ToolContext};
    use serde_json::json;

    struct NamedTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str { self.0 }
        fn description(&self) -> &str { "test tool" }
        fn parameters(&self) -> Value { json!({"type": "object"}) }
        async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
            Ok("ok".to_string())
        }
    }



    struct SchemaTool;

    #[async_trait::async_trait]
    impl Tool for SchemaTool {
        fn name(&self) -> &str { "schema_tool" }
        fn description(&self) -> &str { "schema tool" }
        fn parameters(&self) -> Value {
            json!({
                "type": "object",
                "properties": {
                    "bad:key/that/is/far/too/long/for/anthropic/property/names/and/keeps/going": {"type": "string"},
                    "nested:obj": {
                        "type": "object",
                        "properties": {"inner/key": {"type": "string"}},
                        "required": ["inner/key"]
                    }
                },
                "required": [
                    "bad:key/that/is/far/too/long/for/anthropic/property/names/and/keeps/going",
                    "nested:obj"
                ]
            })
        }
        async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
            Ok("ok".to_string())
        }
    }

    #[test]
    fn tool_schema_uses_api_safe_names_and_maps_back() {
        let registry = ToolRegistry::from_tools(vec![Arc::new(NamedTool("plugin:skill.tool"))]);

        assert_eq!(registry.tools_schema()[0]["name"], "plugin_skill_tool");
        assert!(registry.get("plugin:skill.tool").is_some());
        assert!(registry.get("plugin_skill_tool").is_some());
        assert_eq!(registry.runtime_name_for_api("plugin_skill_tool"), "plugin:skill.tool");
    }

    #[test]
    fn tool_schema_disambiguates_sanitized_name_collisions() {
        let registry = ToolRegistry::from_tools(vec![
            Arc::new(NamedTool("a:b")),
            Arc::new(NamedTool("a.b")),
        ]);
        let names: HashSet<String> = registry.tools_schema().iter()
            .filter_map(|s| s["name"].as_str().map(str::to_string))
            .collect();

        assert_eq!(names.len(), 2);
        assert!(names.contains("a_b"));
        assert!(names.contains("a_b_2"));
        assert!(registry.get("a_b").is_some());
        assert!(registry.get("a_b_2").is_some());
    }

    #[test]
    fn tool_schema_truncates_long_names_to_anthropic_limit() {
        let long = "x".repeat(140);
        let leaked: &'static str = Box::leak(long.into_boxed_str());
        let registry = ToolRegistry::from_tools(vec![Arc::new(NamedTool(leaked))]);
        let schema = registry.tools_schema();
        let name = schema[0]["name"].as_str().unwrap();

        assert_eq!(name.len(), 128);
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
        assert!(registry.get(name).is_some());
    }

    #[test]
    fn tool_schema_sanitizes_input_property_names_and_translates_inputs_back() {
        let registry = ToolRegistry::from_tools(vec![Arc::new(SchemaTool)]);
        let schema = registry.tools_schema();
        let input_schema = &schema[0]["input_schema"];
        let props = input_schema["properties"].as_object().unwrap();

        assert!(props.keys().all(|k| k.len() <= 64 && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')));
        assert_eq!(input_schema["required"].as_array().unwrap()[0].as_str().unwrap().len(), 64);
        assert_eq!(input_schema["required"][1], "nested_obj");
        assert!(props["nested_obj"]["properties"].as_object().unwrap().contains_key("inner_key"));
        assert_eq!(props["nested_obj"]["required"][0], "inner_key");

        let first_required = input_schema["required"][0].as_str().unwrap();
        let translated = registry.translate_input_for_api_tool("schema_tool", json!({
            first_required: "value",
            "nested_obj": {"inner_key": "nested"}
        }));

        assert_eq!(translated["bad:key/that/is/far/too/long/for/anthropic/property/names/and/keeps/going"], "value");
        assert_eq!(translated["nested:obj"]["inner/key"], "nested");
    }

}
