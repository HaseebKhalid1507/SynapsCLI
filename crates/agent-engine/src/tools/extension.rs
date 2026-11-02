use std::sync::Arc;

use serde_json::Value;

use crate::extensions::runtime::process::RegisteredExtensionToolSpec;
use crate::extensions::runtime::ExtensionHandler;
use crate::tools::ToolOutput;
use crate::{Result, RuntimeError, ToolContext};

/// Render an extension `tool.call` result value: a plain string, the
/// `content` string field, or the serialized value. A result object carrying
/// `_meta["synaps/memory_only"] == true` is a memory-only result
/// (`runtime::memory_only`). Shared by the eager and the deferred tool.
pub(crate) fn extension_tool_output(value: &Value) -> ToolOutput {
    let text = if let Some(text) = value.as_str() {
        text.to_string()
    } else if let Some(text) = value.get("content").and_then(Value::as_str) {
        text.to_string()
    } else {
        value.to_string()
    };
    if crate::runtime::memory_only::meta_marked(value) {
        ToolOutput::MemoryOnly(text)
    } else {
        ToolOutput::Text(text)
    }
}

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
        self.execute_rich(params, ctx).await.map(ToolOutput::into_summary)
    }

    async fn execute_rich(&self, params: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let binding = ctx
            .capabilities
            .memory_backend
            .clone()
            .unwrap_or_else(crate::memory_backend::MemoryBinding::configured_current);
        if binding.exclusive() && crate::memory_backend::competing_memory_tool(self) {
            return Err(RuntimeError::Tool("independent extension memory is disabled by the selected host backend; use short memory tools".into()));
        }
        let value = self
            .handler
            .call_tool_in(&self.tool_name, params, &ctx.capabilities)
            .await
            .map_err(RuntimeError::Tool)?;
        Ok(extension_tool_output(&value))
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

    /// An extension marks one result memory-only with `_meta`; the text the
    /// model gets is unchanged, the variant tells the stream to keep it in RAM.
    #[tokio::test]
    async fn memory_only_meta_on_an_extension_result_marks_it() {
        struct Marked;
        #[async_trait::async_trait]
        impl ExtensionHandler for Marked {
            fn id(&self) -> &str {
                "memfix"
            }
            async fn handle(&self, _event: &HookEvent) -> HookResult {
                HookResult::Continue
            }
            async fn shutdown(&self) {}
            async fn call_tool(&self, name: &str, _input: Value) -> std::result::Result<Value, String> {
                Ok(match name {
                    "vault_fetch" => json!({"content": "VAULT-TEXT", "_meta": {"synaps/memory_only": true}}),
                    _ => json!({"content": "PLAIN-TEXT"}),
                })
            }
        }
        let handler: Arc<dyn ExtensionHandler> = Arc::new(Marked);
        let spec = |name: &str| RegisteredExtensionToolSpec {
            name: name.into(),
            description: name.into(),
            input_schema: json!({"type": "object"}),
        };
        let dir = tempfile::tempdir().unwrap();
        let ctx = || context_with_memory(memory_binding(MemoryBackendKind::Legacy, dir.path()));
        let marked = ExtensionTool::new("memfix", spec("vault_fetch"), handler.clone());
        match marked.execute_rich(json!({}), ctx()).await.unwrap() {
            ToolOutput::MemoryOnly(text) => assert_eq!(text, "VAULT-TEXT"),
            other => panic!("expected a memory-only output, got {other:?}"),
        }
        assert_eq!(marked.execute(json!({}), ctx()).await.unwrap(), "VAULT-TEXT");
        let plain = ExtensionTool::new("memfix", spec("other"), handler);
        assert!(matches!(
            plain.execute_rich(json!({}), ctx()).await.unwrap(),
            ToolOutput::Text(text) if text == "PLAIN-TEXT"
        ));
    }

    /// The two name forms a memory-only glob matches for an extension tool:
    /// the runtime name `<plugin>:<tool>` and the wire name the model sees,
    /// `<plugin>_<tool>` (characters outside `[A-Za-z0-9_-]` become `_`).
    #[test]
    fn memory_only_globs_match_an_extension_tool_by_runtime_or_wire_name() {
        let handler = Arc::new(Counting::default());
        let registry = ToolRegistry::from_tools_for_tests(vec![Arc::new(tool(
            "pria-vault",
            "fetch",
            &handler,
        ))]);
        let wire = registry
            .tools_schema()
            .iter()
            .find_map(|t| t["name"].as_str().filter(|n| n.contains("fetch")).map(String::from))
            .expect("extension tool exposed");
        assert_eq!(wire, "pria-vault_fetch");
        assert_eq!(registry.runtime_name_for_api(&wire), "pria-vault:fetch");
        let ext = registry.get(&wire).expect("registered");
        for glob in ["pria-vault:*", "pria-vault_*", "pria-vault:fetch", "pria-vault_fetch"] {
            let mut policy = crate::runtime::memory_only::Policy::default();
            policy.set_session(vec![glob.into()]);
            assert!(policy.marks(&ext.origin(), ext.name(), &wire), "{glob}");
        }
        let mut other = crate::runtime::memory_only::Policy::default();
        other.set_session(vec!["vault:*".into()]);
        assert!(!other.marks(&ext.origin(), ext.name(), &wire));
    }

    /// A glob also matches the tool's OWN name, the one the extension
    /// registered (`pria_vault_get`), whatever the extension id: here the
    /// runtime name is `pria-desktop:pria_vault_get` and the wire name
    /// `pria-desktop_pria_vault_get`, which `pria_vault_*` matches neither of.
    #[test]
    fn memory_only_globs_match_an_extension_tool_by_its_own_name() {
        let handler = Arc::new(Counting::default());
        let registry = ToolRegistry::from_tools_for_tests(vec![Arc::new(tool(
            "pria-desktop",
            "pria_vault_get",
            &handler,
        ))]);
        let wire = registry
            .tools_schema()
            .iter()
            .find_map(|t| t["name"].as_str().filter(|n| n.contains("pria_vault_get")).map(String::from))
            .expect("extension tool exposed");
        assert_eq!(wire, "pria-desktop_pria_vault_get");
        assert_eq!(registry.runtime_name_for_api(&wire), "pria-desktop:pria_vault_get");
        let ext = registry.get(&wire).expect("registered");
        let globs = vec!["pria_vault_*".to_string(), "pria_memory_*".to_string()];
        let mut policy = crate::runtime::memory_only::Policy::default();
        policy.set_session(globs.clone());
        assert!(policy.marks(&ext.origin(), ext.name(), &wire), "the IK1 globs mark pria_vault_get");
        let mut config = crate::runtime::memory_only::Policy::default();
        config.set_config(globs);
        assert!(config.marks(&ext.origin(), ext.name(), &wire), "the same from the config key");
        let mut pinned = crate::runtime::memory_only::Policy::default();
        pinned.set_session(vec!["other:pria_vault_*".into()]);
        assert!(!pinned.marks(&ext.origin(), ext.name(), &wire), "a runtime-form glob pins the extension");
        // Never a built-in, even one with a matching name.
        let mut builtin = crate::runtime::memory_only::Policy::default();
        builtin.set_session(vec!["pria_vault_*".into()]);
        assert!(!builtin.marks(&crate::tools::ToolOrigin::Builtin, "pria_vault_get", "pria_vault_get"));
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
