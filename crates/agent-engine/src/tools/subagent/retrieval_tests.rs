//! An owned worker's registry holds no memory capability besides its owned
//! history (§4 "Retrieval"; slice 4 review). Extension tools are real
//! [`ExtensionTool`]s merged the way every worker registry is built.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use super::{competing_memory_capability, install_owned_history, owned_worker_registry};
use crate::extensions::hooks::events::{HookEvent, HookResult};
use crate::extensions::runtime::process::RegisteredExtensionToolSpec;
use crate::extensions::runtime::ExtensionHandler;
use crate::tools::subagent::lineage::OwnedHistoryGrant;
use crate::tools::subagent::test_fixtures as fx;
use crate::tools::test_helpers::create_tool_context;
use crate::tools::{ExtensionTool, Tool, ToolContext, ToolOrigin, ToolRegistry};

/// An extension process stand-in that counts every tool call it receives.
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
    async fn call_tool(&self, _name: &str, _input: Value) -> Result<Value, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(json!("project-wide extension memory"))
    }
}

fn extension(plugin: &str, operation: &str, handler: &Arc<Counting>) -> Arc<dyn Tool> {
    Arc::new(ExtensionTool::new(
        plugin,
        RegisteredExtensionToolSpec {
            name: operation.into(),
            description: format!("{plugin} {operation}"),
            input_schema: json!({"type": "object"}),
        },
        handler.clone(),
    ))
}

/// A tool with a declared origin and no extension owner (MCP, plugin or
/// undeclared), so the worker merge would not import it; built directly.
struct Declared(&'static str, ToolOrigin);

#[async_trait::async_trait]
impl Tool for Declared {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "declared"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
    async fn execute(&self, _params: Value, _ctx: ToolContext) -> crate::Result<String> {
        Ok("declared".into())
    }
    fn origin(&self) -> ToolOrigin {
        self.1.clone()
    }
}

/// A grant for one sealed window; the store must outlive the grant's use.
async fn grant() -> (fx::Store, Arc<OwnedHistoryGrant>) {
    let store = fx::legacy();
    let r = fx::run(21);
    let w1 = fx::seal_run(&store.binding, &r, "NEEDLE own window").await;
    let rec = fx::record(&r, fx::current(&store.binding), vec![], &[w1], None);
    let grant = fx::grant(&rec, &store.binding).await;
    (store, grant)
}

/// The worker registry every worker gets from a shared registry holding
/// these extension tools.
fn worker_with(handler: &Arc<Counting>) -> ToolRegistry {
    let mut shared = ToolRegistry::without_subagent();
    for (plugin, operation) in [
        ("plugin", "memory_search"),
        ("plugin", "memory_fetch"),
        ("plugin", "memory_store"),
        // Runtime name memory:search, wire name memory_search.
        ("memory", "search"),
        ("plugin", "weather"),
    ] {
        shared.register(extension(plugin, operation, handler));
    }
    ToolRegistry::without_subagent_with_extensions(&shared)
}

const COMPETING: [&str; 4] = [
    "plugin:memory_search",
    "plugin:memory_fetch",
    "plugin:memory_store",
    "memory:search",
];

#[tokio::test]
async fn o1_an_owned_worker_registry_holds_no_extension_memory_tool() {
    let handler = Arc::new(Counting::default());
    let worker = worker_with(&handler);
    // Ordinary (full-history) workers keep them: the merge is unchanged.
    for name in COMPETING {
        assert!(worker.get(name).is_some(), "{name} is merged into workers");
    }
    assert_eq!(
        worker.runtime_name_for_api("memory_search"),
        "memory:search",
        "without a builtin, the alias owns the builtin's wire name"
    );

    let (_store, grant) = grant().await;
    let owned = owned_worker_registry(&worker, grant).unwrap();
    for name in COMPETING {
        assert!(owned.get(name).is_none(), "{name} is not in an owned worker");
    }
    assert!(owned.get("plugin:weather").is_some(), "other tools stay");
    assert!(owned.get("bash").is_some());
    // The base registry is not changed by building the owned one.
    for name in COMPETING {
        assert!(worker.get(name).is_some());
    }

    // Every tool the model can call by wire name: memory_* only resolve to
    // the owned tools, and nothing else is a memory capability.
    for entry in owned.tools_schema().iter() {
        let wire = entry["name"].as_str().unwrap();
        let tool = owned.get(wire).unwrap();
        if wire == "memory_search" || wire == "memory_fetch" {
            assert_eq!(tool.origin(), ToolOrigin::Builtin, "{wire}");
            assert!(tool.description().contains("YOUR OWN"), "{wire}");
        } else {
            assert!(!competing_memory_capability(tool.as_ref()), "{wire}");
            assert!(!wire.starts_with("memory_"), "{wire}");
        }
    }
    for wire in ["memory_search_2", "memory_fetch_2", "plugin_memory_search"] {
        assert!(owned.get(wire).is_none(), "{wire}");
    }

    // Calls through the owned registry never reach the extension.
    let search = Arc::clone(owned.get("memory_search").unwrap());
    let out = search
        .execute(json!({"source": "history", "query": "NEEDLE"}), create_tool_context())
        .await
        .unwrap();
    assert!(out.contains("ctx-"), "{out}");
    let notes = search
        .execute(json!({"query": "NEEDLE"}), create_tool_context())
        .await
        .unwrap_err()
        .to_string();
    assert!(notes.contains("source=history is required"), "{notes}");
    assert_eq!(handler.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn o2_a_failed_owned_build_or_install_changes_nothing() {
    let handler = Arc::new(Counting::default());
    let (_store, grant) = grant().await;
    // A registry that already has the owned tools, and extension memory.
    let mut tools = worker_with(&handler);
    install_owned_history(&mut tools, Arc::clone(&grant)).unwrap();
    let mut tools_with_extension = tools.clone();
    tools_with_extension.register(extension("other", "memory_recall", &handler));
    let before: Vec<String> = tools_with_extension
        .tools_schema()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_owned())
        .collect();
    let generation = tools_with_extension.catalog().generation();

    // Builtin tools of either name are never replaced: the build fails...
    let Err(err) = owned_worker_registry(&tools_with_extension, Arc::clone(&grant)) else {
        panic!("a builtin memory tool must fail the owned build");
    };
    assert!(err.contains("could not be installed"), "{err}");
    // ...and an install leaves everything as it was, extension tools included.
    let err = install_owned_history(&mut tools_with_extension, grant).unwrap_err();
    assert!(err.contains("could not be installed"), "{err}");
    let after: Vec<String> = tools_with_extension
        .tools_schema()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(before, after);
    assert_eq!(tools_with_extension.catalog().generation(), generation);
    assert!(tools_with_extension.get("other:memory_recall").is_some());
}

#[test]
fn o3_memory_capabilities_are_classified_by_origin_operation_and_wire_name() {
    let handler = Arc::new(Counting::default());
    let mcp = |server_tool_name: &str| ToolOrigin::Mcp {
        server_id: "srv".into(),
        server_tool_name: server_tool_name.into(),
    };
    let plugin = |tool_name: &str| ToolOrigin::Plugin {
        plugin_id: "plug".into(),
        tool_name: tool_name.into(),
    };
    let cases: Vec<(Arc<dyn Tool>, bool)> = vec![
        // The operation an extension registered, whatever its runtime name.
        (extension("plugin", "memory_capture", &handler), true),
        (extension("plugin", "memory_forget", &handler), true),
        // A runtime name that sanitizes to a memory tool's wire name.
        (extension("memory", "fetch", &handler), true),
        (extension("memory", "recall", &handler), true),
        (extension("plugin", "weather", &handler), false),
        (extension("plugin", "recall", &handler), false),
        // MCP and plugin origins name the original operation.
        (Arc::new(Declared("ext__srv__lookup", mcp("memory_search"))), true),
        (Arc::new(Declared("ext__srv__lookup", mcp("lookup"))), false),
        (Arc::new(Declared("plug_find", plugin("memory_fetch"))), true),
        // No declared origin: the wire name alone.
        (Arc::new(Declared("memory_forget", ToolOrigin::Unknown)), true),
        (Arc::new(Declared("x:memory_store", ToolOrigin::Unknown)), true),
        (Arc::new(Declared("notes", ToolOrigin::Unknown)), false),
        // Builtins are never stripped: an install refuses them instead.
        (Arc::new(Declared("memory_search", ToolOrigin::Builtin)), false),
    ];
    for (tool, competing) in cases {
        assert_eq!(
            competing_memory_capability(tool.as_ref()),
            competing,
            "{} ({:?})",
            tool.name(),
            tool.origin()
        );
    }
}
