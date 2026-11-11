//! `cwd` on hook events and extension tool calls is the chat's folder, not
//! the host process's. One host (as in the daemon), one recording extension,
//! two live sessions created in different folders: everything the extension
//! receives for a session names that session's folder.
//!
//! Drives the production actor → runtime → Anthropic transport against a
//! loopback SSE stub, like the other `session_actor_*` tests.

mod session_actor_common;
use session_actor_common::*;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_engine::extensions::hooks::events::{HookEvent, HookKind, HookResult};
use agent_engine::extensions::runtime::process::RegisteredExtensionToolSpec;
use agent_engine::extensions::runtime::ExtensionHandler;
use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionConfig, SessionEventWire,
    SessionHandle,
};
use agent_engine::EngineHost;
use serde_json::{json, Value};
use serial_test::serial;

/// What the extension was told about who a hook event or tool call is for.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    /// `None` for a `tool.call`.
    kind: Option<HookKind>,
    session_id: Option<String>,
    cwd: Option<String>,
}

/// An in-process extension: subscribed to every hook, and the handler behind
/// one tool.
#[derive(Default)]
struct Recorder(Mutex<Vec<Seen>>);

impl Recorder {
    fn seen_for(&self, session_id: &str) -> Vec<Seen> {
        let seen = self.0.lock().unwrap();
        seen.iter()
            .filter(|s| s.session_id.as_deref() == Some(session_id))
            .cloned()
            .collect()
    }
}

#[async_trait::async_trait]
impl ExtensionHandler for Recorder {
    fn id(&self) -> &str {
        "cwd-recorder"
    }
    async fn handle(&self, event: &HookEvent) -> HookResult {
        self.0.lock().unwrap().push(Seen {
            kind: Some(event.kind),
            session_id: event.session_id.clone(),
            cwd: event.cwd.clone(),
        });
        HookResult::Continue
    }
    /// What `ProcessExtension` puts on the wire as `tool.call` params.
    async fn call_tool_for(
        &self,
        _name: &str,
        _input: Value,
        session_id: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<Value, String> {
        self.0.lock().unwrap().push(Seen {
            kind: None,
            session_id: session_id.map(str::to_string),
            cwd: agent_engine::extensions::hooks::events::hook_cwd(cwd),
        });
        Ok(json!("here"))
    }
    async fn shutdown(&self) {}
}

/// The real `ExtensionTool` under the name the stub's turn calls, with a
/// built-in origin so the default session tool set admits it without an
/// activation grant. The call itself is `ExtensionTool::execute`.
struct Admitted(agent_engine::tools::ExtensionTool);

#[async_trait::async_trait]
impl agent_engine::Tool for Admitted {
    fn name(&self) -> &str {
        "prompt_fixture"
    }
    fn description(&self) -> &str {
        "asks the recording extension who is calling"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
    fn origin(&self) -> agent_engine::tools::ToolOrigin {
        agent_engine::tools::ToolOrigin::Builtin
    }
    async fn execute(
        &self,
        params: Value,
        ctx: agent_engine::ToolContext,
    ) -> agent_engine::Result<String> {
        self.0.execute(params, ctx).await
    }
}

async fn recording_host() -> (Arc<EngineHost>, Arc<Recorder>) {
    let host = host().await;
    let recorder = Arc::new(Recorder::default());
    for &kind in HookKind::ALL {
        let mut perms = agent_engine::extensions::permissions::PermissionSet::new();
        perms.grant(kind.required_permission());
        host.parts()
            .hook_bus
            .subscribe(kind, recorder.clone(), None, None, perms)
            .await
            .unwrap();
    }
    let tool = agent_engine::tools::ExtensionTool::new(
        "cwd-recorder",
        RegisteredExtensionToolSpec {
            name: "whereami".into(),
            description: "records the caller".into(),
            input_schema: json!({"type": "object"}),
        },
        recorder.clone(),
    );
    host.parts().tools.write().await.register(Arc::new(Admitted(tool)));
    (host, recorder)
}

async fn session_in(host: &Arc<EngineHost>, dir: &Path) -> (SessionHandle, LocalTransport) {
    let handle = host
        .create_session(SessionConfig {
            cwd: Some(dir.to_path_buf()),
            ..cfg()
        })
        .await
        .unwrap();
    let (transport, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    (handle, transport)
}

/// One turn: a tool call, then the final answer.
async fn turn(t: &mut LocalTransport) {
    t.send(submit("where am I?")).await.unwrap();
    until(t, |e| matches!(e, SessionEventWire::Idle)).await;
}

/// `on_session_end` runs as the session ends; give it a moment to land.
async fn wait_for_end(recorder: &Recorder, session_id: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !recorder
        .seen_for(session_id)
        .iter()
        .any(|s| s.kind == Some(HookKind::OnSessionEnd))
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no on_session_end for {session_id}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn each_sessions_hooks_and_tool_calls_name_its_own_folder_not_the_hosts() {
    let _h = Home::new();
    // Session A's turn, then session B's: tool round, answer, twice.
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI, SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let (host, recorder) = recording_host().await;

    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let host_dir: PathBuf = std::env::current_dir().unwrap();
    assert!(dir_a.path() != host_dir && dir_b.path() != host_dir && dir_a.path() != dir_b.path());

    // Both sessions live on the one host before either runs a turn.
    let (a, mut ta) = session_in(&host, dir_a.path()).await;
    let (b, mut tb) = session_in(&host, dir_b.path()).await;
    turn(&mut ta).await;
    turn(&mut tb).await;
    end(&mut ta).await;
    end(&mut tb).await;

    for (handle, dir) in [(&a, dir_a.path()), (&b, dir_b.path())] {
        let id = handle.id.as_str();
        wait_for_end(&recorder, id).await;
        let seen = recorder.seen_for(id);
        // Every hook the session fires, and its extension tool call.
        for kind in [
            Some(HookKind::OnSessionStart),
            Some(HookKind::BeforeMessage),
            Some(HookKind::BeforeToolCall),
            Some(HookKind::AfterToolCall),
            Some(HookKind::OnMessageComplete),
            Some(HookKind::OnSessionEnd),
            None,
        ] {
            assert!(
                seen.iter().any(|s| s.kind == kind),
                "session {id}: nothing recorded for {kind:?} (None = tool.call): {seen:#?}"
            );
        }
        // All of it names this session's folder: not the other session's,
        // and not the folder of the process hosting both.
        for s in &seen {
            assert_eq!(
                s.cwd.as_deref(),
                dir.to_str(),
                "session {id}, {:?}: host folder is {host_dir:?}",
                s.kind
            );
        }
    }
    // Nothing reached the extension unattributed, in the host's folder.
    let all = recorder.0.lock().unwrap().clone();
    assert!(
        all.iter().all(|s| s.cwd.as_deref() != host_dir.to_str()),
        "{all:#?}"
    );
}
