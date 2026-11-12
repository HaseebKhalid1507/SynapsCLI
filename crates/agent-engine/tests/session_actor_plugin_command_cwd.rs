//! A `/plugin:command` is the chat's own. One host (as in the daemon), two
//! live sessions created in different folders with different environments:
//! what each one's command is told and given is that session's, not the
//! host's. The `tool.call` of a tool-backed command names the session and
//! its folder; that tool's context holds the session's memory binding; and
//! a shell-backed command is spawned with the session's environment.
//!
//! Its own test binary, and one test: `/plugin:command` resolves against
//! the installed process host, and a process installs one host for good.

mod session_actor_common;
use session_actor_common::*;

use std::path::Path;
use std::sync::{Arc, Mutex};

use agent_engine::extensions::hooks::events::{hook_cwd, HookEvent, HookResult};
use agent_engine::extensions::runtime::process::RegisteredExtensionToolSpec;
use agent_engine::extensions::runtime::ExtensionHandler;
use agent_engine::memory_backend::MemoryBinding;
use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionCommand, SessionConfig,
    SessionEventWire,
};
use agent_engine::{EngineHost, Tool, ToolContext};
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

/// The tool behind `/where:project`: records the project its context's
/// memory binding is for (`None`: the context has no binding).
#[derive(Default)]
struct Project(Mutex<Vec<Option<String>>>);

#[async_trait::async_trait]
impl Tool for Project {
    fn name(&self) -> &str {
        "where:project"
    }
    fn description(&self) -> &str {
        "records the project of the memory binding it is given"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
    async fn execute(&self, _params: Value, ctx: ToolContext) -> agent_engine::Result<String> {
        let project = ctx
            .capabilities
            .memory_backend
            .as_ref()
            .and_then(|binding| binding.scope().ok())
            .map(|scope| scope.key().to_string());
        self.0.lock().unwrap().push(project);
        Ok("recorded".to_string())
    }
}

/// The variable `/where:env` prints, and one only the host process has.
const WHOSE: &str = "SYNAPS_TEST_WHOSE_COMMAND";
const HOST_ONLY: &str = "SYNAPS_TEST_HOST_ONLY";

/// A command that prints both variables, for the platform's own shell.
fn print_both() -> Value {
    if cfg!(windows) {
        json!({"name": "env", "command": "cmd", "args": ["/C", format!("echo %{WHOSE}% %{HOST_ONLY}%")]})
    } else {
        let script = format!("printf '%s %s' \"${WHOSE}\" \"${HOST_ONLY}\"");
        json!({"name": "env", "command": "sh", "args": ["-c", script]})
    }
}

/// A session environment: `whose` for [`WHOSE`], nothing for [`HOST_ONLY`],
/// and what the platform's shell needs to be found and to start.
fn session_env(whose: &str) -> Vec<(String, String)> {
    let mut env = vec![(WHOSE.to_string(), whose.to_string())];
    for key in ["PATH", "SystemRoot", "ComSpec", "PATHEXT"] {
        if let Ok(value) = std::env::var(key) {
            env.push((key.to_string(), value));
        }
    }
    env
}

/// A host with plugin `where` and its three slash commands: `/where:ami`
/// (extension tool `where:ami`), `/where:project` (tool `where:project`) and
/// `/where:env` (a shell command).
async fn host_with_plugin_commands(
) -> (Arc<EngineHost>, Arc<Calls>, Arc<Project>, tempfile::TempDir) {
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
    let project = Arc::new(Project::default());
    {
        let mut tools = host.parts().tools.write().await;
        tools.register(Arc::new(tool));
        tools.register(project.clone());
    }
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
                    "commands": [
                        {"name": "ami", "tool": "ami", "input": {}},
                        {"name": "project", "tool": "project", "input": {}},
                        print_both(),
                    ],
                }))
                .unwrap(),
            ),
        }],
    );
    (host, calls, project, plugin_root)
}

/// Run `/where:<name>` in the session behind `transport`; its stdout.
async fn run(transport: &mut LocalTransport, query: u64, name: &str) -> String {
    transport
        .send(SessionCommand::PluginCommand {
            id: query,
            plugin: "where".into(),
            name: name.into(),
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
    assert_eq!(value["kind"], "plugin_output", "/where:{name}: {value}");
    assert_eq!(value["status"], 0, "/where:{name}: {value}");
    value["stdout"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_chats_plugin_commands_get_that_chats_id_folder_binding_and_environment_not_the_hosts() {
    let _h = Home::new();
    // No turn runs; the stub only keeps the host off the network.
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    // The host process's own environment: what a daemon was started with.
    std::env::set_var(WHOSE, "the-host");
    std::env::set_var(HOST_ONLY, "host-only-value");
    let (host, calls, project, _plugin_root) = host_with_plugin_commands().await;

    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let host_dir = std::env::current_dir().unwrap();
    assert!(dir_a.path() != host_dir && dir_b.path() != host_dir && dir_a.path() != dir_b.path());

    // Both sessions live on the one host before either runs a command.
    let mut sessions = Vec::new();
    for (dir, whose) in [(dir_a.path(), "chat-A"), (dir_b.path(), "chat-B")] {
        let handle = host
            .create_session(SessionConfig {
                cwd: Some(dir.to_path_buf()),
                env: Some(session_env(whose)),
                env_stripped: vec![HOST_ONLY.to_string()],
                ..cfg()
            })
            .await
            .unwrap();
        let (transport, _) =
            LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
                .await
                .unwrap();
        sessions.push((handle, transport, dir, whose));
    }

    let project_of = |dir: &Path| {
        MemoryBinding::for_cwd(Some(dir.to_path_buf()))
            .scope()
            .unwrap()
            .key()
            .to_string()
    };
    let hosts_project = project_of(&host_dir);

    let (mut callers, mut projects) = (Vec::new(), Vec::new());
    for (n, (handle, transport, dir, whose)) in sessions.iter_mut().enumerate() {
        let query = n as u64 * 10;
        assert_eq!(run(transport, query + 1, "ami").await, "here");
        callers.push((
            Some(handle.id.as_str().to_string()),
            dir.to_str().map(str::to_string),
        ));

        assert_eq!(run(transport, query + 2, "project").await, "recorded");
        assert_ne!(project_of(dir), hosts_project);
        projects.push(Some(project_of(dir)));

        // The session's value, not the host's; and a variable only the host
        // process has does not reach the command.
        let printed = run(transport, query + 3, "env").await;
        assert!(printed.contains(*whose), "{whose}: {printed:?}");
        assert!(!printed.contains("the-host"), "{whose}: {printed:?}");
        assert!(!printed.contains("host-only-value"), "{whose}: {printed:?}");
    }
    std::env::remove_var(WHOSE);
    std::env::remove_var(HOST_ONLY);

    // Each `tool.call` names its own session and that session's folder: not
    // the other's, and not the folder of the process hosting both.
    let calls = calls.0.lock().unwrap().clone();
    assert_eq!(calls, callers, "host folder is {host_dir:?}");
    // Each tool was given its own session's memory binding: the project of
    // that session's folder, not the other's, not the host's, and not none.
    assert_ne!(projects[0], projects[1]);
    let given = project.0.lock().unwrap().clone();
    assert_eq!(given, projects, "the host's project is {hosts_project}");

    for (_, transport, _, _) in sessions.iter_mut() {
        end(transport).await;
    }
}
