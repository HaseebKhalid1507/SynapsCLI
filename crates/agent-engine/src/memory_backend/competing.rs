//! Which tools an exclusive memory backend refuses as independent memory.
use crate::tools::{Tool, ToolOrigin, ToolRegistry};

/// Whether `name`, exactly or in the wire form the tool registry offers it
/// under (`memory:search` and `memory.search` are offered as
/// `memory_search`), ignoring ASCII case, is one of the memory tool names.
fn memory_tool_name(name: &str) -> bool {
    super::competing_note_tool(&name.to_ascii_lowercase())
        || super::competing_note_tool(&ToolRegistry::unclaimed_wire_name(name).to_ascii_lowercase())
}

/// Whether a non-builtin tool is an independent memory capability, which an
/// exclusive (host-selected) memory backend refuses beside the host's own
/// memory tools: its source registered a memory operation (an extension's
/// [`Tool::source_operation`], an MCP server's or a plugin's tool name in its
/// [`ToolOrigin`]), or its runtime name, or that name's last `:` segment, is
/// or is offered as a memory tool's name. An extension `memory` registering
/// `search` is offered as `memory_search` (`memory_search_2` beside the
/// host's own tool) and is therefore competing. Builtins are never
/// classified. A custom tool with any other name cannot be recognized
/// generically; this guards against names a model would take for a memory
/// tool, it is not a sandbox.
pub fn competing_memory_tool(tool: &dyn Tool) -> bool {
    let typed = match tool.origin() {
        ToolOrigin::Builtin => return false,
        ToolOrigin::Mcp {
            server_tool_name, ..
        } => memory_tool_name(&server_tool_name),
        ToolOrigin::Plugin { tool_name, .. } => memory_tool_name(&tool_name),
        ToolOrigin::Extension { .. } | ToolOrigin::Unknown => false,
    };
    let name = tool.name();
    let last = name.rsplit(':').next().unwrap_or(name);
    typed
        || tool.source_operation().is_some_and(memory_tool_name)
        || memory_tool_name(name)
        || memory_tool_name(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolContext;
    use serde_json::Value;

    /// A tool with a runtime name, a registered operation and an origin.
    struct Named {
        name: &'static str,
        operation: Option<&'static str>,
        origin: ToolOrigin,
    }

    #[async_trait::async_trait]
    impl Tool for Named {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "fixture"
        }
        fn parameters(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _params: Value, _ctx: ToolContext) -> crate::Result<String> {
            Ok(String::new())
        }
        fn source_operation(&self) -> Option<&str> {
            self.operation
        }
        fn origin(&self) -> ToolOrigin {
            self.origin.clone()
        }
    }

    fn extension(plugin: &str, operation: &'static str) -> Named {
        Named {
            name: Box::leak(format!("{plugin}:{operation}").into_boxed_str()),
            operation: Some(operation),
            origin: ToolOrigin::Extension {
                extension_id: plugin.into(),
            },
        }
    }

    fn mcp(server: &str, tool: &str) -> Named {
        Named {
            name: Box::leak(format!("ext__{server}__{tool}").into_boxed_str()),
            operation: None,
            origin: ToolOrigin::Mcp {
                server_id: server.into(),
                server_tool_name: tool.into(),
            },
        }
    }

    #[test]
    fn a_registered_memory_operation_is_competing_under_any_plugin_name() {
        for operation in [
            "memory_store",
            "memory_search",
            "memory_fetch",
            "memory_forget",
            "memory_capture",
            "memory_recall",
        ] {
            assert!(
                competing_memory_tool(&extension("axel-memory-manager", operation)),
                "{operation}"
            );
            assert!(
                competing_memory_tool(&extension("plug", operation)),
                "{operation}"
            );
        }
    }

    /// `memory:search` is offered to the model as `memory_search` (or with a
    /// collision suffix next to the host's own tool), although its
    /// registered operation is only `search`.
    #[test]
    fn a_runtime_name_offered_as_a_memory_tool_name_is_competing() {
        for (plugin, operation) in [
            ("memory", "search"),
            ("memory", "fetch"),
            ("memory", "store"),
            ("memory", "forget"),
            ("Memory", "Search"),
            ("plug", "memory.search"),
            ("plug", "memory:fetch"),
            ("plug", "MEMORY_STORE"),
        ] {
            assert!(
                competing_memory_tool(&extension(plugin, operation)),
                "{plugin}:{operation}"
            );
        }
    }

    #[test]
    fn other_names_are_not_competing() {
        for (plugin, operation) in [
            ("weather", "search"),
            ("memory", "summarize"),
            ("memory", "search_web"),
            ("notes", "memory_searcher"),
            ("memory_search", "status"),
            ("memory-search", "run"),
        ] {
            assert!(
                !competing_memory_tool(&extension(plugin, operation)),
                "{plugin}:{operation}"
            );
        }
    }

    #[test]
    fn builtins_are_never_classified() {
        let builtin = Named {
            name: "memory_search",
            operation: Some("memory_search"),
            origin: ToolOrigin::Builtin,
        };
        assert!(!competing_memory_tool(&builtin));
    }

    #[test]
    fn mcp_tools_are_classified_by_their_server_tool_name_and_wire_form() {
        assert!(competing_memory_tool(&mcp("notes", "memory_fetch")));
        assert!(competing_memory_tool(&mcp("notes", "memory.search")));
        assert!(competing_memory_tool(&mcp("notes", "Memory_Store")));
        // `ext__memory__search` is not offered under a memory tool's name.
        assert!(!competing_memory_tool(&mcp("memory", "search")));
        assert!(!competing_memory_tool(&mcp("notes", "lookup")));
    }

    /// A tool that declares neither an operation nor an owner is still
    /// classified by its runtime name.
    #[test]
    fn an_undeclared_tool_is_classified_by_its_runtime_name() {
        let undeclared = |name: &'static str| Named {
            name,
            operation: None,
            origin: ToolOrigin::Unknown,
        };
        assert!(competing_memory_tool(&undeclared("memory:recall")));
        assert!(competing_memory_tool(&undeclared("x:memory_capture")));
        assert!(!competing_memory_tool(&undeclared("x:recall")));
    }
}
