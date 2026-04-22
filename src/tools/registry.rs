//! Tool registry — maintains name→tool map and cached JSON schema for the API.
use std::sync::Arc;
use serde_json::Value;
use std::collections::HashMap;
use crate::tools::Tool;

/// Registry of available tools. Maintains a name→tool map and a cached JSON schema
/// array that gets sent to the API. Thread-safe via `Arc<RwLock<ToolRegistry>>`.
#[derive(Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    /// Cached schema — rebuilt on register(), shared via Arc for zero-copy reads.
    cached_schema: Arc<Vec<Value>>,
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
            Arc::new(crate::tools::tmux_split::TmuxSplitTool),
            Arc::new(crate::tools::tmux_send::TmuxSendTool),
            Arc::new(crate::tools::tmux_capture::TmuxCaptureTool),
            Arc::new(crate::tools::tmux_layout::TmuxLayoutTool),
            Arc::new(crate::tools::tmux_window::TmuxWindowTool),
            Arc::new(crate::tools::tmux_resize::TmuxResizeTool),
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
        let mut tools = HashMap::new();
        for tool in &tool_list {
            tools.insert(tool.name().to_string(), Arc::clone(tool));
        }

        let cached_schema = tool_list.iter().map(|tool| {
            serde_json::json!({
                "name": tool.name(),
                "description": tool.description(),
                "input_schema": tool.parameters()
            })
        }).collect();

        ToolRegistry { tools, cached_schema: Arc::new(cached_schema) }
    }

    /// Register an additional tool at runtime (e.g. MCP tools, custom tools).
    /// If a tool with the same name exists, it is replaced.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();

        // Get mutable access to schema (Arc::make_mut clones only if shared)
        let schema = Arc::make_mut(&mut self.cached_schema);

        // Remove existing schema entry if replacing
        if self.tools.contains_key(&name) {
            schema.retain(|s| s["name"].as_str() != Some(&name));
        }

        schema.push(serde_json::json!({
            "name": tool.name(),
            "description": tool.description(),
            "input_schema": tool.parameters()
        }));
        self.tools.insert(name, tool);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn tools_schema(&self) -> Arc<Vec<Value>> {
        Arc::clone(&self.cached_schema)
    }
}