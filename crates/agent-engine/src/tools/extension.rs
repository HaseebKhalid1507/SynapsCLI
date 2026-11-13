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

/// Who an extension tool call is made by, for `tool.call` `params.caller`:
/// the author of the memory binding the call's context came with (an
/// extension's forum records it as the run's author). `None` when
/// the context has no binding: a binding made on the spot would carry an
/// author no run has, so none is sent.
fn caller_of(ctx: &ToolContext) -> Option<agent_core::memory::author::Author> {
    ctx.capabilities
        .memory_backend
        .as_ref()
        .map(|binding| binding.author().clone())
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
        let caller = caller_of(&ctx);
        let value = self
            .handler
            .call_tool_as(
                &self.tool_name,
                params,
                ctx.capabilities.session_id.as_deref(),
                ctx.capabilities.cwd.as_deref(),
                caller.as_ref(),
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

    use serde_json::json;

    use super::*;
    use crate::extensions::hooks::events::{HookEvent, HookResult};
    use crate::tools::test_helpers::{context_with_memory, create_tool_context, memory_binding};
    use crate::tools::{Tool, ToolRegistry};

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
        async fn call_tool_for(
            &self,
            _name: &str,
            _input: Value,
            _session_id: Option<&str>,
            _cwd: Option<&std::path::Path>,
        ) -> std::result::Result<Value, String> {
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

    /// One call with a chat's binding and one with no binding at all.
    async fn call(tool: &dyn Tool) -> [Result<String>; 2] {
        let dir = tempfile::tempdir().unwrap();
        let binding = memory_binding(dir.path());
        [
            tool.execute(json!({}), context_with_memory(binding)).await,
            tool.execute(json!({}), create_tool_context()).await,
        ]
    }

    /// T-B7. The guard that refused an extension's memory tools beside an
    /// exclusive backend went with the backend: an extension tool runs
    /// whatever it is named, `memory_search` and its aliases included.
    #[tokio::test]
    async fn extension_tools_named_like_memory_tools_run() {
        let handler = Arc::new(Counting::default());
        let named = [
            ("memory", "search"),
            ("memory", "fetch"),
            ("memory", "store"),
            ("memory", "forget"),
            ("plug", "memory.search"),
            ("Memory", "Search"),
            ("axel", "memory_search"),
            ("axel-memory-manager", "memory_search"),
            ("axel-memory-manager", "memory_fetch"),
            ("axel-memory-manager", "memory_store"),
            ("axel-memory-manager", "memory_forget"),
            ("weather", "search"),
            ("memory", "summarize"),
        ];
        for (plugin, operation) in named {
            let tool = tool(plugin, operation, &handler);
            for result in call(&tool).await {
                assert_eq!(result.unwrap(), "extension result", "{plugin}:{operation}");
            }
        }
        assert_eq!(handler.0.load(Ordering::SeqCst), named.len() * 2, "every call reached the extension");
    }

    /// The suffixed wire name the model sees beside the host's own tool
    /// reaches the extension tool, which runs.
    #[tokio::test]
    async fn the_suffixed_wire_name_of_an_alias_runs() {
        let handler = Arc::new(Counting::default());
        let registry = ToolRegistry::from_tools_for_tests(vec![
            Arc::new(crate::tools::memory::MemorySearchTool),
            Arc::new(tool("memory", "search", &handler)),
        ]);
        assert_eq!(registry.runtime_name_for_api("memory_search"), "memory_search");
        assert_eq!(registry.runtime_name_for_api("memory_search_2"), "memory:search");
        let alias = registry.get("memory:search").expect("alias registered");
        for result in call(alias.as_ref()).await {
            assert_eq!(result.unwrap(), "extension result");
        }
        assert_eq!(handler.0.load(Ordering::SeqCst), 2);
    }
}

/// `tool.call` `params.caller` over the real wire: a child process that
/// keeps every `tool.call` it is sent, called through `ExtensionTool` with
/// the contexts a chat and its workers have.
#[cfg(all(test, unix))]
mod caller_tests {
    use std::path::Path;
    use std::time::Duration;

    use agent_core::memory::author::Author;
    use serde_json::json;

    use super::*;
    use crate::extensions::manifest::CURRENT_EXTENSION_PROTOCOL_VERSION;
    use crate::extensions::runtime::process::ProcessExtension;
    use crate::memory_backend::MemoryBinding;
    use crate::tools::test_helpers::{context_with_memory, create_tool_context};
    use crate::tools::Tool;

    /// An extension process that appends the body of each `tool.call` to
    /// `log` (one JSON-RPC request a line) and answers it `"ok"`. It reads
    /// no param of the call: an extension that ignores `caller`.
    async fn wire_tool(log: &Path) -> ExtensionTool {
        let init = json!({
            "protocol_version": CURRENT_EXTENSION_PROTOCOL_VERSION,
            "capabilities": {}
        });
        let script = format!(
            r#"while IFS= read -r header; do
    length=${{header#Content-Length: }}
    length=${{length%?}}
    IFS= read -r blank || exit 1
    body=$(dd bs=1 count="$length" 2>/dev/null) || exit 1
    id=${{body##*\"id\":}}
    id=${{id%%[!0-9]*}}
    case "$body" in
        *'"method":"initialize"'*) result='{init}' ;;
        *'"method":"tool.call"'*) printf '%s\n' "$body" >> '{log}'; result='"ok"' ;;
        *) result=null ;;
    esac
    reply='{{"jsonrpc":"2.0","id":'"$id"',"result":'"$result"'}}'
    printf 'Content-Length: %s\r\n\r\n%s' "${{#reply}}" "$reply"
done"#,
            log = log.display(),
        );
        let process = ProcessExtension::spawn("caller-fixture", "/bin/sh", &["-c".into(), script])
            .await
            .expect("spawn the fixture");
        tokio::time::timeout(Duration::from_secs(5), process.initialize_for_test(None))
            .await
            .expect("bounded initialize")
            .expect("initialize the fixture");
        ExtensionTool::new(
            "fixture",
            RegisteredExtensionToolSpec {
                name: "whoami".into(),
                description: "keeps the call".into(),
                input_schema: json!({"type": "object"}),
            },
            Arc::new(process),
        )
    }

    /// One call with `ctx`; the `params` the process was sent for it.
    async fn params_sent(tool: &ExtensionTool, log: &Path, ctx: ToolContext) -> Value {
        let before = sent(log).len();
        let reply = tokio::time::timeout(Duration::from_secs(5), tool.execute(json!({}), ctx))
            .await
            .expect("bounded call")
            .expect("the fixture answers");
        assert_eq!(reply, "ok");
        let all = sent(log);
        assert_eq!(all.len(), before + 1, "one tool.call a call");
        let params = all[before]["params"].clone();
        assert_eq!(params["name"], json!("whoami"));
        params
    }

    fn sent(log: &Path) -> Vec<Value> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("a tool.call body"))
            .collect()
    }

    /// A chat's binding, for a chat that runs in `dir`.
    fn chat_binding(dir: &Path) -> MemoryBinding {
        MemoryBinding::for_cwd(Some(dir.to_path_buf()))
    }

    /// The author `binding` holds, as it serialises.
    fn author_of(binding: &MemoryBinding) -> Value {
        serde_json::to_value(binding.author()).unwrap()
    }

    /// What `tools/subagent` gives a worker runtime of a chat with `chat`:
    /// oneshot, start and resume all go through this one function.
    fn worker_binding(chat: &MemoryBinding) -> MemoryBinding {
        let mut worker = crate::Runtime::new_headless();
        crate::tools::subagent::apply_subagent_runtime_policy(
            &mut worker,
            &Default::default(),
            Some(chat),
            None,
            None,
            &Default::default(),
        );
        worker.memory_backend_for_test()
    }

    fn as_author(caller: &Value) -> Author {
        let author: Author = serde_json::from_value(caller.clone()).expect("an Author");
        author.validate().expect("a valid Author");
        author
    }

    #[tokio::test]
    async fn a_chats_tool_call_names_the_author_its_binding_holds() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let log = dir.join("calls.jsonl");
        let tool = wire_tool(&log).await;
        let chat = chat_binding(&dir);

        let params = params_sent(&tool, &log, context_with_memory(chat.clone())).await;
        assert_eq!(params["caller"], author_of(&chat), "the run's own author");

        // As the type serialises: a chat has no `parent` key; it reads back
        // as an `Author` and validates.
        let caller = as_author(&params["caller"]);
        assert_eq!(&caller, chat.author());
        assert!(caller.parent.is_none());
        let mut keys: Vec<&str> =
            params["caller"].as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["actor", "group"]);
        // The same run later: the same caller.
        let again = params_sent(&tool, &log, context_with_memory(chat.clone())).await;
        assert_eq!(again["caller"], params["caller"]);
        // Another chat is another actor in another group.
        let other = params_sent(&tool, &log, context_with_memory(chat_binding(&dir))).await;
        let other = as_author(&other["caller"]);
        assert_ne!(other.actor, caller.actor);
        assert_ne!(other.group, caller.group);
    }

    #[tokio::test]
    async fn a_workers_tool_call_names_its_own_actor_in_the_chats_group() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let log = dir.join("calls.jsonl");
        let tool = wire_tool(&log).await;
        let chat = chat_binding(&dir);
        let (first, second) = (worker_binding(&chat), worker_binding(&chat));

        let of_chat = params_sent(&tool, &log, context_with_memory(chat.clone())).await;
        let of_chat = as_author(&of_chat["caller"]);
        let mut actors = vec![of_chat.actor.clone()];
        for worker in [&first, &second] {
            let params = params_sent(&tool, &log, context_with_memory(worker.clone())).await;
            assert_eq!(
                params["caller"],
                author_of(worker),
                "a worker's call names the worker's own author"
            );
            let caller = as_author(&params["caller"]);
            assert_eq!(caller.group, of_chat.group);
            assert_eq!(caller.parent.as_deref(), Some(of_chat.actor.as_str()));
            assert!(!actors.contains(&caller.actor), "each run its own actor");
            actors.push(caller.actor);
        }
        // The chat's own author is what it was before it had workers.
        let after = params_sent(&tool, &log, context_with_memory(chat.clone())).await;
        assert_eq!(as_author(&after["caller"]), of_chat);
    }

    /// No binding in the context: no `caller` key at all, and the call is
    /// otherwise the call it was.
    #[tokio::test]
    async fn a_call_with_no_binding_has_no_caller_key() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let log = dir.join("calls.jsonl");
        let tool = wire_tool(&log).await;
        let ctx = create_tool_context();
        assert!(ctx.capabilities.memory_backend.is_none());
        let params = params_sent(&tool, &log, ctx).await;
        let mut keys: Vec<&str> = params.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["cwd", "input", "name", "session_id"]);
    }
}
