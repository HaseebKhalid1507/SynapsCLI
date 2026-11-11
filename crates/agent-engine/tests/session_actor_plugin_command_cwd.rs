//! A tool-backed `/plugin:command` is the chat's own call. One host (as in
//! the daemon), two live sessions created in different folders: the
//! `tool.call` each one's command makes names that session and its folder,
//! not the host's.
//!
//! Its own test binary: `/plugin:command` resolves against the installed
//! process host, and a process installs one host for good.

mod session_actor_common;
use session_actor_common::*;

use std::path::Path;
use std::sync::{Arc, Mutex};

use agent_engine::extensions::hooks::events::{hook_cwd, HookEvent, HookResult};
use agent_engine::extensions::runtime::process::RegisteredExtensionToolSpec;
use agent_engine::extensions::runtime::ExtensionHandler;
use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionCommand, SessionConfig,
    SessionEventWire,
};
use agent_engine::EngineHost;
use serde_json::{json, Value};
use serial_test::serial;

/// `(session_id, cwd)` of each `tool.call`, as `ProcessExtension` puts them
/// on the wire.
#[derive(Default)]
struct Calls(Mutex<Vec<(Option<String>, Option<String>)>>);

#[async_trait::async_trait]
impl ExtensionHandler for Calls {
    fn id(&self) -> &str {
        "where"
    }
    async fn handle(&self, _event: &HookEvent) -> HookResult {
        HookResult::Continue
    }
    async fn call_tool_for(
        &self,
        _name: &str,
        _input: Value,
        session_id: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<Value, String> {
        self.0
            .lock()
            .unwrap()
            .push((session_id.map(str::to_string), hook_cwd(cwd)));
        Ok(json!("here"))
    }
    async fn shutdown(&self) {}
}

/// A host with plugin `where`: extension tool `where:ami`, and the slash
/// command `/where:ami` that runs it.
async fn host_with_plugin_command() -> (Arc<EngineHost>, Arc<Calls>, tempfile::TempDir) {
    let host = host().await;
    assert!(
        EngineHost::install(Arc::clone(&host)).is_ok(),
        "this binary installs one host"
    );
    let calls = Arc::new(Calls::default());
    let tool = agent_engine::tools::ExtensionTool::new(
        "where",
        RegisteredExtensionToolSpec {
            name: "ami".into(),
            description: "records the caller".into(),
            input_schema: json!({"type": "object"}),
        },
        calls.clone(),
    );
    host.parts().tools.write().await.register(Arc::new(tool));
    let plugin_root = tempfile::tempdir().unwrap();
    host.command_registry().rebuild_with_plugins(
        vec![],
        vec![agent_engine::skills::Plugin {
            name: "where".into(),
            root: plugin_root.path().to_path_buf(),
            marketplace: None,
            version: None,
            description: None,
            extension: None,
            manifest: Some(
                serde_json::from_value(json!({
                    "name": "where",
                    "commands": [{"name": "ami", "tool": "ami", "input": {}}],
                }))
                .unwrap(),
            ),
        }],
    );
    (host, calls, plugin_root)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_plugin_commands_tool_call_names_the_chat_and_its_folder_not_the_hosts() {
    let _h = Home::new();
    // No turn runs; the stub only keeps the host off the network.
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let (host, calls, _plugin_root) = host_with_plugin_command().await;

    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let host_dir = std::env::current_dir().unwrap();
    assert!(dir_a.path() != host_dir && dir_b.path() != host_dir && dir_a.path() != dir_b.path());

    // Both sessions live on the one host before either runs its command.
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
        sessions.push((handle, transport, dir));
    }

    let mut expected = Vec::new();
    for (query, (handle, transport, dir)) in sessions.iter_mut().enumerate() {
        let query = query as u64 + 1;
        transport
            .send(SessionCommand::PluginCommand {
                id: query,
                plugin: "where".into(),
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
        let Some(SessionEventWire::QueryResult { value, .. }) = seen.last().map(|e| &e.event)
        else {
            unreachable!()
        };
        assert_eq!(value["kind"], "plugin_output", "{value}");
        assert_eq!(value["stdout"], "here", "{value}");
        expected.push((
            Some(handle.id.as_str().to_string()),
            dir.to_str().map(str::to_string),
        ));
    }

    // Each call names its own session and that session's folder: not the
    // other's, and not the folder of the process hosting both.
    let calls = calls.0.lock().unwrap().clone();
    assert_eq!(calls, expected, "host folder is {host_dir:?}");

    for (_, transport, _) in sessions.iter_mut() {
        end(transport).await;
    }
}
