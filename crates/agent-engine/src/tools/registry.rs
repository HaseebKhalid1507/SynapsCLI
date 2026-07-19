//! Tool registry — maintains name→tool map and cached JSON schema for the API.
use crate::tools::catalog::{tool_id_for, CapabilityRecord, CatalogError, ToolCatalog};
use crate::tools::Tool;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

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
    /// Passive capability inventory kept truthful across every construction
    /// and mutation path. Reads never change the exposed schema; mutations
    /// advance the catalog generation fail-closed (Task 14).
    catalog: ToolCatalog,
}

#[derive(Clone, Debug, Default)]
struct SchemaNameMap {
    api_to_runtime: HashMap<String, String>,
    children: HashMap<String, SchemaNameMap>,
    /// Name map for array `items` schemas (objects inside arrays).
    items: Option<Box<SchemaNameMap>>,
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
            Arc::new(crate::tools::subagent::authorize_model::SubagentModelAuthorizeTool),
            Arc::new(crate::tools::subagent::models::SubagentModelsTool),
            Arc::new(crate::tools::subagent::start::SubagentStartTool),
            Arc::new(crate::tools::subagent::status::SubagentStatusTool),
            Arc::new(crate::tools::subagent::steer::SubagentSteerTool),
            Arc::new(crate::tools::subagent::collect::SubagentCollectTool),
            Arc::new(crate::tools::subagent::resume::SubagentResumeTool),
            Arc::new(crate::tools::shell::ShellStartTool),
            Arc::new(crate::tools::shell::ShellSendTool),
            Arc::new(crate::tools::shell::ShellEndTool),
        ];
        Self::from_tools(tools)
    }

    /// Empty registry for tests and narrow embedded runtimes that want to opt in
    /// to specific tools explicitly.
    pub fn empty() -> Self {
        Self::from_tools(Vec::new())
    }

    /// Remove built-in tools by runtime name (config `disabled_tools`).
    ///
    /// Compatibility wrapper over [`Self::try_disable`]; the only failure
    /// mode is catalog generation exhaustion, on which this fails closed by
    /// panicking with the registry left fully unchanged.
    pub fn disable(&mut self, names: &[String]) {
        self.try_disable(names).expect(CATALOG_REJECTED_MSG);
    }

    /// Fallible disable: remove built-in tools by runtime name, then rebuild
    /// the cached schema, name maps, and capability catalog. Unknown names
    /// are ignored; a call that removes nothing is a no-op and does not
    /// advance the catalog generation. A call that removes tools advances
    /// the catalog generation strictly past its prior value, so grants
    /// issued against removed capabilities cannot stay valid.
    ///
    /// The rebuilt candidate is fully constructed and generation-rebased
    /// BEFORE it is committed to `self`: on any failure the live registry
    /// (tools, schema, catalog entries, generation) is left untouched, so a
    /// generation-exhausted disable can never leave a generation-rewound,
    /// partially changed registry behind a non-poisoning lock.
    pub fn try_disable(&mut self, names: &[String]) -> Result<(), CatalogError> {
        if names.is_empty() {
            return Ok(());
        }
        let remaining: Vec<Arc<dyn Tool>> = self
            .tools
            .values()
            .filter(|t| !names.iter().any(|n| n == t.name()))
            .cloned()
            .collect();
        if remaining.len() == self.tools.len() {
            return Ok(());
        }
        let prior = self.catalog.generation();
        let mut candidate = Self::try_from_tools(remaining)?;
        candidate.catalog.rebase_past(prior)?;
        *self = candidate;
        Ok(())
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

    /// Built-in subagent registry plus extension tools merged in from a shared
    /// registry. Used by subagents that need to invoke extension-provided
    /// tools while still excluding the recursive `subagent_*` tools.
    ///
    /// Only tools whose `Tool::extension_id()` returns `Some(_)` are merged;
    /// built-ins inside `extension_tools` are ignored to avoid duplicating or
    /// shadowing the canonical built-in instances.
    pub fn without_subagent_with_extensions(extension_tools: &ToolRegistry) -> Self {
        let mut combined = Self::without_subagent();
        for tool in extension_tools.tools.values() {
            if tool.extension_id().is_some() && !is_recursive_subagent_tool_name(tool.name()) {
                combined
                    .insert_tool_with_catalog(tool.clone())
                    .expect(CATALOG_REJECTED_MSG);
            }
        }
        combined.rebuild_schema();
        combined
    }

    /// Infallible construction wrapper over [`Self::try_from_tools`] for
    /// compiled-in tool lists. A typed failure here (duplicate capability
    /// identity or generation exhaustion) means the constructed set itself
    /// is inconsistent, so this fails closed by panicking rather than
    /// exposing live tools the catalog could not truthfully record.
    fn from_tools(tool_list: Vec<Arc<dyn Tool>>) -> Self {
        Self::try_from_tools(tool_list).expect(CATALOG_REJECTED_MSG)
    }

    fn try_from_tools(tool_list: Vec<Arc<dyn Tool>>) -> Result<Self, CatalogError> {
        let mut registry = ToolRegistry {
            tools: HashMap::new(),
            cached_schema: Arc::new(Vec::new()),
            api_to_runtime_names: HashMap::new(),
            input_name_maps: HashMap::new(),
            catalog: ToolCatalog::empty(),
        };
        // Insert all tools first, then rebuild schema once.
        // Calling register() in a loop would rebuild_schema() on every
        // iteration, making initialization O(n²) with MCP tool counts.
        for tool in tool_list {
            let name = tool.name().to_string();
            registry.tools.insert(name, tool);
        }
        // Catalog the constructed set in deterministic name order. Purely
        // passive: reads in-process metadata only, invokes no factory,
        // starts no process, exposes no schema, issues no grant. Distinct
        // runtime names collapsing onto one capability identity fail typed —
        // construction must never hide an uncataloged live tool.
        let sorted: Vec<Arc<dyn Tool>> =
            registry.iter_tools_sorted().into_iter().cloned().collect();
        for tool in sorted {
            let record = CapabilityRecord::for_registered_tool(&tool);
            registry.catalog.upsert(None, record)?;
        }
        registry.rebuild_schema();
        Ok(registry)
    }

    fn api_safe_name(name: &str, used: &HashSet<String>) -> String {
        Self::api_safe_identifier(name, used, 128, false)
    }

    fn api_safe_property_name(name: &str, used: &HashSet<String>) -> String {
        Self::api_safe_identifier(name, used, 64, true)
    }

    fn api_safe_identifier(
        name: &str,
        used: &HashSet<String>,
        max_len: usize,
        allow_dot: bool,
    ) -> String {
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

        // Recurse into array item schemas; store the child map so
        // translate_input_names can reverse-map property names inside array elements.
        if let Some(items) = obj.get_mut("items") {
            let (sanitized_items, items_map) = Self::sanitize_schema(std::mem::take(items));
            if !items_map.api_to_runtime.is_empty()
                || !items_map.children.is_empty()
                || items_map.items.is_some()
            {
                map.items = Some(Box::new(items_map));
            }
            *items = sanitized_items;
        }

        (schema, map)
    }

    fn translate_input_names(input: Value, map: &SchemaNameMap) -> Value {
        match input {
            Value::Object(obj) => {
                let mut out = serde_json::Map::new();
                for (api_name, value) in obj {
                    let runtime_name = map
                        .api_to_runtime
                        .get(&api_name)
                        .cloned()
                        .unwrap_or_else(|| api_name.clone());
                    let value = if let Some(child) = map.children.get(&api_name) {
                        Self::translate_input_names(value, child)
                    } else {
                        value
                    };
                    out.insert(runtime_name, value);
                }
                Value::Object(out)
            }
            Value::Array(arr) => {
                // If the schema had an items map, apply it to each array element.
                if let Some(items_map) = &map.items {
                    Value::Array(
                        arr.into_iter()
                            .map(|v| Self::translate_input_names(v, items_map))
                            .collect(),
                    )
                } else {
                    Value::Array(arr)
                }
            }
            other => other,
        }
    }

    fn rebuild_schema(&mut self) {
        let mut used = HashSet::new();
        let mut api_to_runtime_names = HashMap::new();
        let mut input_name_maps = HashMap::new();
        let mut schema = Vec::with_capacity(self.tools.len());

        // Sort by runtime name for deterministic API name assignment.
        // HashMap iteration is random, so without sorting, collision suffixes
        // (_2, _3) could change between rebuilds, breaking in-flight conversations.
        let mut sorted_tools: Vec<_> = self.tools.values().collect();
        sorted_tools.sort_by_key(|t| t.name().to_string());

        for tool in sorted_tools {
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
    ///
    /// Compatibility wrapper over [`Self::try_register`]. The only failure
    /// mode is catalog generation exhaustion (a `u64` mutation counter),
    /// which is unreachable in practice; if it ever occurs this wrapper
    /// fails closed by panicking rather than exposing a tool the catalog
    /// could not record. Callers that can surface errors should prefer
    /// [`Self::try_register`].
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.try_register(tool).expect(CATALOG_REJECTED_MSG);
    }

    /// Fallible registration: catalog the tool first (fail-closed — on error
    /// nothing is mutated and no schema is exposed), then insert it into the
    /// live map and rebuild the exposed schema. Replacing a tool whose
    /// capability identity changes drops the stale catalog entry in the same
    /// generation advance.
    pub fn try_register(&mut self, tool: Arc<dyn Tool>) -> Result<(), CatalogError> {
        self.insert_tool_with_catalog(tool)?;
        self.rebuild_schema();
        Ok(())
    }

    /// Shared fail-closed mutation core: catalog mutation happens before the
    /// tools-map mutation, so a catalog failure leaves the registry (and the
    /// exposed schema) untouched. A capability identity already owned by a
    /// DIFFERENT runtime name fails typed (`CatalogError::DuplicateToolId`)
    /// instead of silently diverging live tools from the catalog; only the
    /// same-runtime-name tool being replaced may keep its identity. Never
    /// invokes the tool, its factory, any extension handler, or any MCP
    /// connection method.
    fn insert_tool_with_catalog(&mut self, tool: Arc<dyn Tool>) -> Result<(), CatalogError> {
        let record = CapabilityRecord::for_registered_tool(&tool);
        let replaced = self
            .tools
            .get(tool.name())
            .map(|old| tool_id_for(old.as_ref()));
        self.catalog.upsert(replaced.as_ref(), record)?;
        self.tools.insert(tool.name().to_string(), tool);
        Ok(())
    }

    /// Read-only view of the capability catalog. Reading never mutates
    /// registry state; the exposed `tools_schema()` stays byte-identical.
    pub fn catalog(&self) -> &ToolCatalog {
        &self.catalog
    }

    /// Boundary-test support: resume the catalog generation counter at an
    /// explicit value. Grants nothing, exposes nothing, and changes no
    /// tools, schema, or catalog entries; only the counter consulted by
    /// fail-closed mutation checks is overwritten.
    #[doc(hidden)]
    pub fn resume_catalog_generation_for_tests(
        &mut self,
        generation: crate::tools::catalog::CatalogGeneration,
    ) {
        self.catalog.set_generation_for_tests(generation);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        let runtime_name = self
            .api_to_runtime_names
            .get(name)
            .map(String::as_str)
            .unwrap_or(name);
        self.tools.get(runtime_name)
    }

    pub fn runtime_name_for_api<'a>(&'a self, name: &'a str) -> &'a str {
        self.api_to_runtime_names
            .get(name)
            .map(String::as_str)
            .unwrap_or(name)
    }

    /// Atomic wire-name resolution for the execution gate (Task 16): map
    /// the incoming API/wire name through the deterministic sanitized-name
    /// reverse mapping to the exact live runtime tool, then derive its
    /// catalog [`crate::tools::catalog::ToolId`] from the resolved tool
    /// instance itself — never from the alias string. Because this runs on
    /// one `&self` borrow (i.e. under the caller's registry read lock), the
    /// resolution, the catalog reachable via [`Self::catalog`], and any
    /// subsequent implementation acquisition observe one consistent
    /// registry snapshot.
    pub fn resolve_wire_call(
        &self,
        wire_name: &str,
    ) -> Option<crate::tools::activation::ResolvedToolCall> {
        let runtime_name = self
            .api_to_runtime_names
            .get(wire_name)
            .map(String::as_str)
            .unwrap_or(wire_name);
        let tool = self.tools.get(runtime_name)?;
        Some(crate::tools::activation::ResolvedToolCall::new(
            wire_name,
            runtime_name,
            tool_id_for(tool.as_ref()),
        ))
    }

    pub fn translate_input_for_api_tool(&self, tool_name: &str, input: Value) -> Value {
        if let Some(map) = self.input_name_maps.get(tool_name) {
            Self::translate_input_names(input, map)
        } else {
            input
        }
    }

    pub fn tools_schema(&self) -> Arc<Vec<Value>> {
        Arc::clone(&self.cached_schema)
    }

    /// Return runtime names of tools owned by the given extension id, sorted ascending.
    /// Built-in tools (which return `None` from `Tool::extension_id`) are excluded.
    pub fn tool_names_for_extension(&self, extension_id: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .tools
            .values()
            .filter(|t| t.extension_id() == Some(extension_id))
            .map(|t| t.name().to_string())
            .collect();
        names.sort();
        names
    }

    /// Iterate all registered tools, sorted by name for deterministic output.
    /// Used by `synaps tools export` to build the static schema manifest.
    pub fn iter_tools_sorted(&self) -> Vec<&Arc<dyn Tool>> {
        let mut tools: Vec<_> = self.tools.values().collect();
        tools.sort_by_key(|t| t.name());
        tools
    }
}

/// Fail-closed message for infallible compatibility wrappers over typed
/// catalog mutations. Failure means either generation exhaustion (a `u64`
/// mutation counter, unreachable in practice) or a duplicate capability
/// identity across distinct runtime names; both panic here rather than
/// exposing a live tool the catalog could not truthfully record.
const CATALOG_REJECTED_MSG: &str =
    "tool catalog rejected mutation: refusing to expose a tool the catalog could not record";

fn is_recursive_subagent_tool_name(name: &str) -> bool {
    let api_name = ToolRegistry::api_safe_name(name, &HashSet::new());
    matches!(
        api_name.as_str(),
        "subagent"
            | "subagent_start"
            | "subagent_status"
            | "subagent_steer"
            | "subagent_collect"
            | "subagent_resume"
            | "subagent_model_authorize"
            | "subagent_models"
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Result, ToolContext};
    use serde_json::json;

    struct NamedTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "test tool"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
            Ok("ok".to_string())
        }
    }

    struct SchemaTool;

    #[async_trait::async_trait]
    impl Tool for SchemaTool {
        fn name(&self) -> &str {
            "schema_tool"
        }
        fn description(&self) -> &str {
            "schema tool"
        }
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
        assert_eq!(
            registry.runtime_name_for_api("plugin_skill_tool"),
            "plugin:skill.tool"
        );
    }

    #[test]
    fn tool_schema_disambiguates_sanitized_name_collisions() {
        let registry =
            ToolRegistry::from_tools(vec![Arc::new(NamedTool("a:b")), Arc::new(NamedTool("a.b"))]);
        let names: HashSet<String> = registry
            .tools_schema()
            .iter()
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
        assert!(name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
        assert!(registry.get(name).is_some());
    }

    #[test]
    fn tool_schema_sanitizes_input_property_names_and_translates_inputs_back() {
        let registry = ToolRegistry::from_tools(vec![Arc::new(SchemaTool)]);
        let schema = registry.tools_schema();
        let input_schema = &schema[0]["input_schema"];
        let props = input_schema["properties"].as_object().unwrap();

        assert!(props.keys().all(|k| k.len() <= 64
            && k.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')));
        assert_eq!(
            input_schema["required"].as_array().unwrap()[0]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        assert_eq!(input_schema["required"][1], "nested_obj");
        assert!(props["nested_obj"]["properties"]
            .as_object()
            .unwrap()
            .contains_key("inner_key"));
        assert_eq!(props["nested_obj"]["required"][0], "inner_key");

        let first_required = input_schema["required"][0].as_str().unwrap();
        let translated = registry.translate_input_for_api_tool(
            "schema_tool",
            json!({
                first_required: "value",
                "nested_obj": {"inner_key": "nested"}
            }),
        );

        assert_eq!(
            translated["bad:key/that/is/far/too/long/for/anthropic/property/names/and/keeps/going"],
            "value"
        );
        assert_eq!(translated["nested:obj"]["inner/key"], "nested");
    }

    #[test]
    fn test_tool_registry_new() {
        let registry = ToolRegistry::new();

        // Includes read-only model discovery plus session authorization.
        assert_eq!(registry.tools_schema().len(), 18);

        // Should find bash tool
        assert!(registry.get("bash").is_some());

        // Should not find nonexistent tool
        assert!(registry.get("nonexistent").is_none());

        // Verify all expected tools are present
        assert!(registry.get("bash").is_some());
        assert!(registry.get("read").is_some());
        assert!(registry.get("write").is_some());
        assert!(registry.get("edit").is_some());
        assert!(registry.get("grep").is_some());
        assert!(registry.get("find").is_some());
        assert!(registry.get("ls").is_some());
        assert!(registry.get("subagent").is_some());
        assert!(registry.get("subagent_model_authorize").is_some());
        assert!(registry.get("subagent_models").is_some());
    }

    #[test]
    fn test_disable_removes_tools_from_registry_and_schema() {
        let mut registry = ToolRegistry::new();
        let before = registry.tools_schema().len();

        registry.disable(&["bash".to_string(), "ls".to_string()]);

        // bash + ls gone from lookup
        assert!(registry.get("bash").is_none());
        assert!(registry.get("ls").is_none());
        // others untouched
        assert!(registry.get("read").is_some());
        assert!(registry.get("subagent").is_some());
        // schema rebuilt with exactly two fewer tools
        assert_eq!(registry.tools_schema().len(), before - 2);
    }

    #[test]
    fn test_disable_empty_and_unknown_are_noops() {
        let mut registry = ToolRegistry::new();
        let n = registry.tools_schema().len();
        registry.disable(&[]); // empty → no change
        assert_eq!(registry.tools_schema().len(), n);
        registry.disable(&["not_a_real_tool".to_string()]); // unknown → no change
        assert_eq!(registry.tools_schema().len(), n);
        assert!(registry.get("bash").is_some());
    }

    #[test]
    fn test_tool_registry_without_subagent() {
        let registry = ToolRegistry::without_subagent();

        // Should have 10 tools without subagent (7 base + 3 shell)
        assert_eq!(registry.tools_schema().len(), 10);
        // Should not have subagent tool
        assert!(registry.get("subagent").is_none());

        // Should still have bash tool
        assert!(registry.get("bash").is_some());

        // Verify all expected tools are present except subagent
        assert!(registry.get("bash").is_some());
        assert!(registry.get("read").is_some());
        assert!(registry.get("write").is_some());
        assert!(registry.get("edit").is_some());
        assert!(registry.get("grep").is_some());
        assert!(registry.get("find").is_some());
        assert!(registry.get("ls").is_some());
    }

    #[test]
    fn test_tool_registry_register() {
        let mut registry = ToolRegistry::without_subagent();
        let initial_count = registry.tools_schema().len();

        // Register a new tool (using BashTool with different name for simplicity)
        struct TestTool;
        #[async_trait::async_trait]
        impl Tool for TestTool {
            fn name(&self) -> &str {
                "test_tool"
            }
            fn description(&self) -> &str {
                "A test tool"
            }
            fn parameters(&self) -> Value {
                json!({"type": "object"})
            }
            async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
                Ok("test result".to_string())
            }
        }

        registry.register(Arc::new(TestTool));

        // Should have one more tool now
        assert_eq!(registry.tools_schema().len(), initial_count + 1);

        // Should find the new tool
        assert!(registry.get("test_tool").is_some());
    }

    #[test]
    fn tool_names_for_extension_filters_by_owner_and_sorts() {
        struct OwnedTool(&'static str, Option<&'static str>);
        #[async_trait::async_trait]
        impl Tool for OwnedTool {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                "owned"
            }
            fn parameters(&self) -> Value {
                json!({"type": "object"})
            }
            async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
                Ok("ok".to_string())
            }
            fn extension_id(&self) -> Option<&str> {
                self.1
            }
        }

        let mut registry = ToolRegistry::without_subagent();
        registry.register(Arc::new(OwnedTool("alpha:zed", Some("alpha"))));
        registry.register(Arc::new(OwnedTool("alpha:bar", Some("alpha"))));
        registry.register(Arc::new(OwnedTool("beta:thing", Some("beta"))));

        assert_eq!(
            registry.tool_names_for_extension("alpha"),
            vec!["alpha:bar".to_string(), "alpha:zed".to_string()]
        );
        assert_eq!(
            registry.tool_names_for_extension("beta"),
            vec!["beta:thing".to_string()]
        );
        assert!(registry.tool_names_for_extension("ghost").is_empty());
        // Built-in tools (no owner) must not leak.
        assert!(registry.tool_names_for_extension("bash").is_empty());
    }

    struct OwnedTool(&'static str, Option<&'static str>);
    #[async_trait::async_trait]
    impl Tool for OwnedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "owned"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
            Ok("ok".to_string())
        }
        fn extension_id(&self) -> Option<&str> {
            self.1
        }
    }

    #[test]
    fn without_subagent_excludes_subagent_tools() {
        let registry = ToolRegistry::without_subagent();
        assert!(registry.get("subagent").is_none());
        assert!(registry.get("subagent_start").is_none());
        assert!(registry.get("subagent_status").is_none());
        assert!(registry.get("subagent_steer").is_none());
        assert!(registry.get("subagent_collect").is_none());
        assert!(registry.get("subagent_resume").is_none());
        // Built-ins remain.
        assert!(registry.get("bash").is_some());
        assert!(registry.get("read").is_some());
    }

    #[test]
    fn without_subagent_with_extensions_includes_extension_tools() {
        let mut other = ToolRegistry::empty();
        other.register(Arc::new(OwnedTool("alpha:do_thing", Some("alpha"))));

        let merged = ToolRegistry::without_subagent_with_extensions(&other);

        // Extension tool present.
        assert!(merged.get("alpha:do_thing").is_some());
        // Built-ins still present.
        assert!(merged.get("bash").is_some());
        assert!(merged.get("read").is_some());
        // Subagent tools still absent.
        assert!(merged.get("subagent_start").is_none());
    }

    #[test]
    fn without_subagent_with_extensions_excludes_built_ins_from_other_registry() {
        // `other` simulates a shared registry that already holds built-ins
        // (e.g. the extension manager's tools registry). Only tools with an
        // extension owner must be merged — built-ins must NOT be re-added or
        // overwritten with a foreign instance.
        let other = ToolRegistry::new();

        let merged = ToolRegistry::without_subagent_with_extensions(&other);

        // Only one instance of `bash`, and it's the built-in (no extension_id).
        let bash = merged.get("bash").expect("bash present");
        assert!(bash.extension_id().is_none());
        // No subagent tools leaked from `other`.
        assert!(merged.get("subagent_start").is_none());
        assert!(merged.get("subagent").is_none());
    }

    #[test]
    fn without_subagent_with_extensions_rejects_recursive_tool_name_collisions() {
        let mut other = ToolRegistry::empty();
        for name in [
            "subagent",
            "subagent_start",
            "subagent_status",
            "subagent_steer",
            "subagent_collect",
            "subagent_resume",
            "subagent_model_authorize",
            "subagent_models",
        ] {
            other.register(Arc::new(OwnedTool(name, Some("malicious-extension"))));
        }

        let merged = ToolRegistry::without_subagent_with_extensions(&other);
        for name in [
            "subagent",
            "subagent_start",
            "subagent_status",
            "subagent_steer",
            "subagent_collect",
            "subagent_resume",
            "subagent_model_authorize",
            "subagent_models",
        ] {
            assert!(merged.get(name).is_none(), "{name} must stay unavailable");
        }
    }

    #[test]
    fn without_subagent_with_extensions_rejects_api_sanitized_recursive_collisions() {
        let mut other = ToolRegistry::empty();
        for runtime_name in [
            "subagent:start",
            "subagent:status",
            "subagent:steer",
            "subagent:collect",
            "subagent:resume",
            "subagent:model:authorize",
            "subagent:models",
        ] {
            other.register(Arc::new(OwnedTool(runtime_name, Some("subagent"))));
        }

        let merged = ToolRegistry::without_subagent_with_extensions(&other);
        for api_name in [
            "subagent_start",
            "subagent_status",
            "subagent_steer",
            "subagent_collect",
            "subagent_resume",
            "subagent_model_authorize",
            "subagent_models",
        ] {
            assert!(
                merged.get(api_name).is_none(),
                "API-safe recursive name {api_name} must stay unavailable"
            );
        }
    }

    #[test]
    fn without_subagent_with_extensions_does_not_overwrite_existing_builtin() {
        // If `other` somehow contained a tool named like a built-in but with
        // an extension_id, our merge currently allows it to overwrite. We
        // skip non-extension tools, but we DO allow extension-owned tools to
        // shadow names — document this by asserting that built-ins without
        // matching names in `other` are preserved unchanged.
        let mut other = ToolRegistry::empty();
        other.register(Arc::new(OwnedTool("ext:custom", Some("ext"))));

        let merged = ToolRegistry::without_subagent_with_extensions(&other);
        assert!(merged.get("ext:custom").is_some());
        assert!(merged.get("bash").is_some());
        assert!(merged.get("bash").unwrap().extension_id().is_none());
    }

    /// Two distinct runtime names declaring the same capability identity.
    struct McpTwin(&'static str);

    #[async_trait::async_trait]
    impl Tool for McpTwin {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "mcp twin"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
            Ok("ok".to_string())
        }
        fn origin(&self) -> crate::tools::ToolOrigin {
            crate::tools::ToolOrigin::Mcp {
                server_id: "srv".to_string(),
                server_tool_name: "shared".to_string(),
            }
        }
    }

    #[test]
    #[should_panic(expected = "tool catalog rejected mutation")]
    fn from_tools_with_duplicate_capability_identity_fails_closed() {
        // Initial construction must not silently hide an uncataloged tool:
        // two distinct runtime names collapsing to one capability identity
        // must abort construction, never diverge live tools from the catalog.
        let _ = ToolRegistry::from_tools(vec![
            Arc::new(McpTwin("twin_one")),
            Arc::new(McpTwin("twin_two")),
        ]);
    }
}
