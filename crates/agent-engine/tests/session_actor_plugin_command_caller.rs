//! `caller` on a tool-backed `/plugin:command`. The command's tool runs in
//! a context made for the command (`skills/commands.rs`), not in a turn's,
//! and the `tool.call` it makes names the author of the chat's own memory
//! binding: the one a turn's call of the same chat would name. One host,
//! two live sessions, as in `session_actor_plugin_command_cwd.rs`.
//!
//! Its own test binary, and one test: `/plugin:command` resolves against
//! the installed process host, and a process installs one host for good.

mod session_actor_common;
use session_actor_common::*;

use std::path::Path;
use std::sync::{Arc, Mutex};

use agent_core::memory::author::Author;
use agent_engine::extensions::hooks::events::{HookEvent, HookResult};
use agent_engine::extensions::runtime::process::RegisteredExtensionToolSpec;
use agent_engine::extensions::runtime::ExtensionHandler;
use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionCommand, SessionConfig,
    SessionEventWire,
};
use agent_engine::{EngineHost, Tool, ToolContext};
use serde_json::{json, Value};
use serial_test::serial;

/// One `tool.call` of `/who:ami`: whose it is, the author of the binding
/// the command's tool context held, and the `caller` the handler was given
/// for the wire.
#[derive(Debug, Clone, PartialEq)]
struct Call {
    session_id: Option<String>,
    bound: Option<Author>,
    caller: Option<Author>,
}

#[derive(Default)]
struct Recorder {
    /// What the tool context held, set by [`Noting`] just before the call.
    bound: Mutex<Option<Author>>,
    calls: Mutex<Vec<Call>>,
}

#[async_trait::async_trait]
impl ExtensionHandler for Recorder {
    fn id(&self) -> &str {
        "who"
    }
    async fn handle(&self, _event: &HookEvent) -> HookResult {
        HookResult::Continue
    }
    /// What `ProcessExtension` puts on the wire as `tool.call` params.
    async fn call_tool_as(
        &self,
        _name: &str,
        _input: Value,
        session_id: Option<&str>,
        _cwd: Option<&Path>,
        caller: Option<&Author>,
    ) -> Result<Value, String> {
        self.calls.lock().unwrap().push(Call {
            session_id: session_id.map(str::to_string),
            bound: self.bound.lock().unwrap().take(),
            caller: caller.cloned(),
        });
        Ok(json!("here"))
    }
    async fn shutdown(&self) {}
}

/// The real `ExtensionTool` behind `/who:ami`, under its own name. It notes
/// the author of the context's binding, and
/// the call itself is `ExtensionTool::execute`. Commands here run one at a
/// time.
struct Noting(agent_engine::tools::ExtensionTool, Arc<Recorder>);

#[async_trait::async_trait]
impl Tool for Noting {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn description(&self) -> &str {
        self.0.description()
    }
    fn parameters(&self) -> Value {
        self.0.parameters()
    }
    async fn execute(&self, params: Value, ctx: ToolContext) -> agent_engine::Result<String> {
        *self.1.bound.lock().unwrap() = ctx
            .capabilities
            .memory_backend
            .as_ref()
            .map(|binding| binding.forum_author().clone());
        self.0.execute(params, ctx).await
    }
}

/// A host with plugin `who` and its slash command `/who:ami`, backed by the
/// extension tool `who:ami`.
async fn host_with_the_command() -> (Arc<EngineHost>, Arc<Recorder>, tempfile::TempDir) {
    let host = host().await;
    assert!(
        EngineHost::install(Arc::clone(&host)).is_ok(),
        "this binary installs one host"
    );
    let recorder = Arc::new(Recorder::default());
    let tool = agent_engine::tools::ExtensionTool::new(
        "who",
        RegisteredExtensionToolSpec {
            name: "ami".into(),
            description: "records the caller".into(),
            input_schema: json!({"type": "object"}),
        },
        recorder.clone(),
    );
    assert_eq!(tool.name(), "who:ami");
    host.parts()
        .tools
        .write()
        .await
        .register(Arc::new(Noting(tool, recorder.clone())));
    let plugin_root = tempfile::tempdir().unwrap();
    host.command_registry().rebuild_with_plugins(
        vec![],
        vec![agent_engine::skills::Plugin {
            name: "who".into(),
            root: plugin_root.path().to_path_buf(),
            marketplace: None,
            version: None,
            description: None,
            extension: None,
            manifest: Some(
                serde_json::from_value(json!({
                    "name": "who",
                    "commands": [{"name": "ami", "tool": "ami", "input": {}}],
                }))
                .unwrap(),
            ),
        }],
    );
    (host, recorder, plugin_root)
}

/// Run `/who:ami` in the session behind `transport`.
async fn who_am_i(transport: &mut LocalTransport, query: u64) {
    transport
        .send(SessionCommand::PluginCommand {
            id: query,
            plugin: "who".into(),
            name: "ami".into(),
            arg: String::new(),
        })
        .await
        .unwrap();
    let seen = until(
        transport,
        |e| matches!(e, SessionEventWire::QueryResult { id, .. } if *id == query),
    )
    .await;
    let Some(SessionEventWire::QueryResult { value, .. }) = seen.last().map(|e| &e.event) else {
        unreachable!()
    };
    assert_eq!(value["kind"], "plugin_output", "/who:ami: {value}");
    assert_eq!(value["status"], 0, "/who:ami: {value}");
    assert_eq!(value["stdout"], "here", "/who:ami: {value}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_chats_plugin_command_names_that_chats_own_author_as_caller() {
    let _h = Home::new();
    // No turn runs; the stub only keeps the host off the network.
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let (host, recorder, _plugin_root) = host_with_the_command().await;

    // Both sessions live on the one host before either runs the command.
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut sessions = Vec::new();
    for dir in [dir_a.path(), dir_b.path()] {
        let handle = host
            .create_session(SessionConfig {
                cwd: Some(dir.to_path_buf()),
                ..cfg()
            })
            .await
            .unwrap();
        let (transport, _) =
            LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
                .await
                .unwrap();
        sessions.push((handle, transport));
    }
    // A, B, then A again: each its own, and A's the same the second time.
    for (query, chat) in [0usize, 1, 0].into_iter().enumerate() {
        who_am_i(&mut sessions[chat].1, query as u64 + 1).await;
    }

    let calls = recorder.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 3, "{calls:#?}");
    let mut callers = Vec::new();
    for (call, chat) in calls.iter().zip([0usize, 1, 0]) {
        assert_eq!(
            call.session_id.as_deref(),
            Some(sessions[chat].0.id.as_str()),
            "{call:?}"
        );
        let caller = call.caller.clone().expect("a chat's command names its caller");
        caller.validate().unwrap();
        assert!(caller.parent.is_none(), "a chat has no parent: {caller:?}");
        assert!(call.bound.is_some(), "the command's tool context has a binding");
        assert_eq!(call.caller, call.bound, "the binding's author, not another");
        callers.push(caller);
    }
    assert_eq!(callers[0], callers[2], "one chat, one caller");
    assert_ne!(callers[0].actor, callers[1].actor);
    assert_ne!(callers[0].group, callers[1].group);

    for (_, transport) in sessions.iter_mut() {
        end(transport).await;
    }
}
