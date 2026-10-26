use std::sync::Arc;

use serde_json::Value;

use crate::extensions::runtime::process::RegisteredExtensionToolSpec;
use crate::extensions::runtime::ExtensionHandler;
use crate::{Result, RuntimeError, ToolContext};

pub struct ExtensionTool {
    runtime_name: String,
    description: String,
    input_schema: Value,
    handler: Arc<dyn ExtensionHandler>,
    tool_name: String,
    plugin_id: String,
}

impl ExtensionTool {
    pub fn new(
        plugin_id: &str,
        spec: RegisteredExtensionToolSpec,
        handler: Arc<dyn ExtensionHandler>,
    ) -> Self {
        Self {
            runtime_name: format!("{}:{}", plugin_id, spec.name),
            description: spec.description,
            input_schema: spec.input_schema,
            handler,
            tool_name: spec.name,
            plugin_id: plugin_id.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl crate::tools::Tool for ExtensionTool {
    fn name(&self) -> &str {
        &self.runtime_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.input_schema.clone()
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let binding = ctx
            .capabilities
            .memory_backend
            .unwrap_or_else(crate::memory_backend::MemoryBinding::configured_current);
        if binding.exclusive() && crate::memory_backend::competing_memory_tool(self) {
            return Err(RuntimeError::Tool("independent extension memory is disabled by the selected host backend; use short memory tools".into()));
        }
        let value = self
            .handler
            .call_tool_for(
                &self.tool_name,
                params,
                ctx.capabilities.session_id.as_deref(),
            )
            .await
            .map_err(RuntimeError::Tool)?;
        if let Some(text) = value.as_str() {
            Ok(text.to_string())
        } else if let Some(text) = value.get("content").and_then(Value::as_str) {
            Ok(text.to_string())
        } else {
            Ok(value.to_string())
        }
    }

    fn extension_id(&self) -> Option<&str> {
        Some(&self.plugin_id)
    }

    fn source_operation(&self) -> Option<&str> {
        Some(&self.tool_name)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use agent_core::config::MemoryBackendKind;
    use serde_json::json;

    use super::*;
    use crate::extensions::hooks::events::{HookEvent, HookResult};
    use crate::tools::test_helpers::{context_with_memory, memory_binding};
    use crate::tools::{Tool, ToolRegistry};

    const REFUSAL: &str = "independent extension memory is disabled by the selected host backend; use short memory tools";

    /// An extension process stand-in that counts the tool calls it receives.
    #[derive(Default)]
    struct Counting(AtomicUsize);

    #[async_trait::async_trait]
    impl ExtensionHandler for Counting {
        fn id(&self) -> &str {
            "fixture"
        }
        async fn handle(&self, _event: &HookEvent) -> HookResult {
            HookResult::Continue
        }
        async fn shutdown(&self) {}
        async fn call_tool(&self, _name: &str, _input: Value) -> std::result::Result<Value, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(json!("extension result"))
        }
    }

    fn tool(plugin: &str, operation: &str, handler: &Arc<Counting>) -> ExtensionTool {
        ExtensionTool::new(
            plugin,
            RegisteredExtensionToolSpec {
                name: operation.into(),
                description: format!("{plugin} {operation}"),
                input_schema: json!({"type": "object"}),
            },
            handler.clone(),
        )
    }

    async fn call(tool: &dyn Tool, kind: MemoryBackendKind) -> Result<String> {
        let dir = tempfile::tempdir().unwrap();
        let binding = memory_binding(kind, dir.path());
        tool.execute(json!({}), context_with_memory(binding)).await
    }

    fn refusal(result: Result<String>) -> String {
        match result {
            Err(RuntimeError::Tool(message)) => message,
            other => panic!("expected a tool refusal, got {other:?}"),
        }
    }

    /// Regression: an extension `memory` registering `search` is offered as
    /// `memory_search` (`memory_search_2` beside the host's own tool), yet
    /// only its operation `search` was checked.
    #[tokio::test]
    async fn an_exclusive_binding_refuses_extension_memory_aliases() {
        let handler = Arc::new(Counting::default());
        for (plugin, operation) in [
            ("memory", "search"),
            ("memory", "fetch"),
            ("memory", "store"),
            ("memory", "forget"),
            ("plug", "memory.search"),
            ("Memory", "Search"),
        ] {
            let tool = tool(plugin, operation, &handler);
            assert_eq!(
                refusal(call(&tool, MemoryBackendKind::Axel).await),
                REFUSAL,
                "{plugin}:{operation}"
            );
        }
        assert_eq!(handler.0.load(Ordering::SeqCst), 0, "no call reached the extension");
    }

    #[tokio::test]
    async fn an_exclusive_binding_still_refuses_exact_memory_tool_names() {
        let handler = Arc::new(Counting::default());
        for operation in ["memory_search", "memory_fetch", "memory_store", "memory_forget"] {
            let tool = tool("axel-memory-manager", operation, &handler);
            assert_eq!(refusal(call(&tool, MemoryBackendKind::Axel).await), REFUSAL);
        }
        assert_eq!(handler.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_exclusive_binding_runs_other_extension_tools() {
        let handler = Arc::new(Counting::default());
        for (plugin, operation) in [("weather", "search"), ("memory", "summarize")] {
            let tool = tool(plugin, operation, &handler);
            assert_eq!(
                call(&tool, MemoryBackendKind::Axel).await.unwrap(),
                "extension result"
            );
        }
        assert_eq!(handler.0.load(Ordering::SeqCst), 2);
    }

    /// The suffixed wire name the model sees beside the host's own tool
    /// reaches the extension tool, which refuses.
    #[tokio::test]
    async fn the_suffixed_wire_name_of_an_alias_is_refused() {
        let handler = Arc::new(Counting::default());
        let registry = ToolRegistry::from_tools_for_tests(vec![
            Arc::new(crate::tools::memory::MemorySearchTool),
            Arc::new(tool("memory", "search", &handler)),
        ]);
        assert_eq!(registry.runtime_name_for_api("memory_search"), "memory_search");
        assert_eq!(registry.runtime_name_for_api("memory_search_2"), "memory:search");
        let alias = registry.get("memory:search").expect("alias registered");
        assert_eq!(
            refusal(call(alias.as_ref(), MemoryBackendKind::Axel).await),
            REFUSAL
        );
        assert_eq!(handler.0.load(Ordering::SeqCst), 0);
    }

    /// No behaviour change without an exclusive binding.
    #[tokio::test]
    async fn a_legacy_binding_runs_extension_memory_tools_unchanged() {
        let handler = Arc::new(Counting::default());
        for (plugin, operation) in [("memory", "search"), ("axel-memory-manager", "memory_search")] {
            let tool = tool(plugin, operation, &handler);
            assert_eq!(
                call(&tool, MemoryBackendKind::Legacy).await.unwrap(),
                "extension result"
            );
        }
        assert_eq!(handler.0.load(Ordering::SeqCst), 2);
    }
}
