//! `cwd` on hook events and extension tool calls is the chat's folder, not
//! the host process's. One host (as in the daemon), one recording extension,
//! sessions created in folders that are not the host's: everything the
//! extension receives for a session names that session's folder. Covered:
//! every way a session's `on_session_start` is sent (at create, queued,
//! after an unpark), compaction, a chat woken from park, and `--continue`
//! from another folder.
//!
//! Drives the production actor → runtime → Anthropic transport against a
//! loopback stub, like the other `session_actor_*` tests.

mod session_actor_common;
use session_actor_common::*;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_engine::extensions::hooks::events::{HookEvent, HookKind, HookResult};
use agent_engine::extensions::runtime::process::RegisteredExtensionToolSpec;
use agent_engine::extensions::runtime::ExtensionHandler;
use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionCommand, SessionConfig,
    SessionEventWire, SessionHandle, SessionLifecycle,
};
use agent_engine::EngineHost;
use axum::response::IntoResponse;
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
    fn all(&self) -> Vec<Seen> {
        self.0.lock().unwrap().clone()
    }
    fn seen_for(&self, session_id: &str) -> Vec<Seen> {
        let mut seen = self.all();
        seen.retain(|s| s.session_id.as_deref() == Some(session_id));
        seen
    }
    /// Forget what came before (a session's earlier life).
    fn clear(&self) {
        self.0.lock().unwrap().clear();
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

/// The provider: every user message gets one `prompt_fixture` tool round,
/// then the answer; the non-streaming compaction call gets a summary.
async fn stub_tool_turns() -> String {
    let app = axum::Router::new().fallback(|body: String| async move {
        let request: Value = serde_json::from_str(&body).unwrap_or_default();
        if request["stream"] != true {
            return axum::Json(json!({
                "id": "msg_c", "type": "message", "role": "assistant",
                "model": MODEL, "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "SUMMARY: the user asked where they are."}],
                "usage": {"input_tokens": 50, "output_tokens": 10}
            }))
            .into_response();
        }
        let tool_ran = request["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .and_then(|last| last["content"].as_array())
            .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"));
        (
            axum::http::StatusCode::OK,
            [("content-type", "text/event-stream")],
            if tool_ran { SSE_HI } else { SSE_PROMPT_TOOL_USE }.to_string(),
        )
            .into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// A temp HOME, the stub provider, and a recording host.
async fn setup() -> (Home, Arc<EngineHost>, Arc<Recorder>) {
    let home = Home::new();
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", stub_tool_turns().await);
    let (host, recorder) = recording_host().await;
    (home, host, recorder)
}

/// A persisted session in `dir`.
fn cfg_in(dir: &Path) -> SessionConfig {
    SessionConfig {
        cwd: Some(dir.to_path_buf()),
        persist: true,
        ..cfg()
    }
}

async fn attach(handle: &SessionHandle) -> LocalTransport {
    LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap()
        .0
}

async fn session(host: &Arc<EngineHost>, config: SessionConfig) -> (SessionHandle, LocalTransport) {
    let handle = host.create_session(config).await.unwrap();
    let transport = attach(&handle).await;
    (handle, transport)
}

/// One turn: a tool call, then the final answer.
async fn turn(t: &mut LocalTransport) {
    t.send(submit("where am I?")).await.unwrap();
    until(t, |e| matches!(e, SessionEventWire::Idle)).await;
}

/// Hooks sent from a background task land a moment after what caused them.
async fn wait_for(recorder: &Recorder, session_id: &str, kind: HookKind) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !recorder
        .seen_for(session_id)
        .iter()
        .any(|s| s.kind == Some(kind))
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no {kind:?} for {session_id}: {:#?}",
            recorder.all()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// End the session and wait until its actor is gone (journal lock free).
async fn end_and_close(
    recorder: &Recorder,
    handle: &SessionHandle,
    mut transport: LocalTransport,
) {
    end(&mut transport).await;
    wait_for(recorder, handle.id.as_str(), HookKind::OnSessionEnd).await;
    tokio::time::timeout(Duration::from_secs(10), handle.closed())
        .await
        .expect("the ended session's actor never went away");
}

async fn detach(mut t: LocalTransport) {
    t.send(SessionCommand::Detach {
        client: t.client_id(),
    })
    .await
    .unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::ClientLeft { .. })).await;
}

async fn wait_parked(handle: &SessionHandle) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while handle.lifecycle() != SessionLifecycle::Parked {
        assert!(tokio::time::Instant::now() < deadline, "never parked");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// What a turn sends an extension: the hooks around the message and its
/// tool call, and the `tool.call` itself (`None`).
const TURN: [Option<HookKind>; 5] = [
    Some(HookKind::BeforeMessage),
    Some(HookKind::BeforeToolCall),
    Some(HookKind::AfterToolCall),
    Some(HookKind::OnMessageComplete),
    None,
];

/// The extension got each of `kinds` for the session, and everything it got
/// for the session names `dir`: a folder, and not the host's.
fn assert_names(recorder: &Recorder, session_id: &str, dir: &Path, kinds: &[Option<HookKind>]) {
    let host_dir: PathBuf = std::env::current_dir().unwrap();
    assert_ne!(dir, host_dir);
    let seen = recorder.seen_for(session_id);
    for kind in kinds {
        assert!(
            seen.iter().any(|s| s.kind == *kind),
            "session {session_id}: nothing recorded for {kind:?} (None = tool.call): {seen:#?}"
        );
    }
    for s in &seen {
        assert_eq!(
            s.cwd.as_deref(),
            dir.to_str(),
            "session {session_id}, {:?}: host folder is {host_dir:?}",
            s.kind
        );
    }
}

/// Nothing reached the extension unattributed: every event and call it
/// recorded is one of `ids`', so each was checked against a folder by
/// `assert_names` (an event with no session, or another session's, fails
/// here).
fn assert_only_sessions(recorder: &Recorder, ids: &[&str]) {
    let all = recorder.all();
    assert!(
        all.iter()
            .all(|s| s.session_id.as_deref().is_some_and(|id| ids.contains(&id))),
        "{all:#?}"
    );
}

/// Two live sessions on one host: every hook kind a session fires, and its
/// extension tool call, name its own folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn each_sessions_hooks_and_tool_calls_name_its_own_folder_not_the_hosts() {
    let (_h, host, recorder) = setup().await;
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    assert_ne!(dir_a.path(), dir_b.path());

    // Both sessions live on the one host before either runs a turn.
    let (a, mut ta) = session(&host, cfg_in(dir_a.path())).await;
    let (b, mut tb) = session(&host, cfg_in(dir_b.path())).await;
    for t in [&mut ta, &mut tb] {
        turn(t).await;
        turn(t).await;
    }
    for t in [&mut ta, &mut tb] {
        t.send(SessionCommand::Compact { instructions: None }).await.unwrap();
        until(t, |e| matches!(e, SessionEventWire::CompactionApplied { .. })).await;
    }
    end_and_close(&recorder, &a, ta).await;
    end_and_close(&recorder, &b, tb).await;

    let mut kinds = TURN.to_vec();
    kinds.extend([
        Some(HookKind::OnSessionStart),
        Some(HookKind::OnCompaction),
        Some(HookKind::OnSessionEnd),
    ]);
    assert_names(&recorder, a.id.as_str(), dir_a.path(), &kinds);
    assert_names(&recorder, b.id.as_str(), dir_b.path(), &kinds);
    assert_only_sessions(&recorder, &[a.id.as_str(), b.id.as_str()]);
}

/// `await_extensions = false` (the TUI's quick start, every session a daemon
/// reload restores live, a pinned session restored at startup): the hook is
/// sent from a queued background task, not at create.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_queued_session_start_names_the_sessions_folder() {
    let (_h, host, recorder) = setup().await;
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let queued = |dir: &Path| SessionConfig {
        await_extensions: false,
        ..cfg_in(dir)
    };

    let (a, mut ta) = session(&host, queued(dir_a.path())).await;
    let (b, mut tb) = session(&host, queued(dir_b.path())).await;
    // The first turn waits for the hook.
    turn(&mut ta).await;
    turn(&mut tb).await;

    let mut kinds = TURN.to_vec();
    kinds.push(Some(HookKind::OnSessionStart));
    assert_names(&recorder, a.id.as_str(), dir_a.path(), &kinds);
    assert_names(&recorder, b.id.as_str(), dir_b.path(), &kinds);
    end_and_close(&recorder, &a, ta).await;
    end_and_close(&recorder, &b, tb).await;
    assert_only_sessions(&recorder, &[a.id.as_str(), b.id.as_str()]);
}

/// A session a daemon reload restores parked: its `on_session_start` waits
/// for the unpark and is queued from there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_session_restored_parked_starts_in_its_folder_when_woken() {
    let (_h, host, recorder) = setup().await;
    let dir = tempfile::tempdir().unwrap();
    // A chat with history on disk.
    let (first, mut t) = session(&host, cfg_in(dir.path())).await;
    turn(&mut t).await;
    end_and_close(&recorder, &first, t).await;
    let id = first.id.as_str();
    recorder.clear();

    // As the reload does it: created, then parked before anyone attaches.
    let restored = host
        .create_session(SessionConfig {
            continue_session: Some(Some(id.to_string())),
            await_extensions: false,
            restore_parked: true,
            ..cfg_in(dir.path())
        })
        .await
        .unwrap();
    restored.send(SessionCommand::Park).await.unwrap();
    wait_parked(&restored).await;
    assert!(
        recorder.all().is_empty(),
        "nothing is sent for a parked session: {:#?}",
        recorder.all()
    );

    // Woken by a client: the hook runs now, then the turn.
    let mut t = attach(&restored).await;
    turn(&mut t).await;
    let mut kinds = TURN.to_vec();
    kinds.push(Some(HookKind::OnSessionStart));
    assert_names(&recorder, id, dir.path(), &kinds);
    end_and_close(&recorder, &restored, t).await;
    assert_only_sessions(&recorder, &[id]);
}

/// Parking drops the runtime; waking builds a new one. Its hooks and tool
/// calls still name the chat's folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_chat_woken_from_park_still_names_its_folder() {
    let (_h, host, recorder) = setup().await;
    let dir = tempfile::tempdir().unwrap();
    let (handle, mut t) = session(&host, cfg_in(dir.path())).await;
    let id = handle.id.as_str();
    turn(&mut t).await;
    detach(t).await;
    handle.send(SessionCommand::Park).await.unwrap();
    wait_parked(&handle).await;
    recorder.clear();

    let mut t = attach(&handle).await;
    assert_eq!(handle.lifecycle(), SessionLifecycle::Live);
    turn(&mut t).await;
    end_and_close(&recorder, &handle, t).await;
    let mut kinds = TURN.to_vec();
    kinds.push(Some(HookKind::OnSessionEnd));
    assert_names(&recorder, id, dir.path(), &kinds);
    assert_only_sessions(&recorder, &[id]);
}

/// A chat whose folder was deleted while it was parked wakes without it, as
/// a reload restores it: in the host's folder, never naming the dead path
/// (which has no project, so memory and notes would have nowhere to go).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_chat_whose_folder_was_deleted_while_parked_wakes_without_it() {
    let (_h, host, recorder) = setup().await;
    let parent = tempfile::tempdir().unwrap();
    let folder = parent.path().join("chat");
    std::fs::create_dir(&folder).unwrap();
    let (handle, mut t) = session(&host, cfg_in(&folder)).await;
    let id = handle.id.as_str();
    turn(&mut t).await;
    detach(t).await;
    handle.send(SessionCommand::Park).await.unwrap();
    wait_parked(&handle).await;
    std::fs::remove_dir(&folder).unwrap();
    recorder.clear();

    let mut t = attach(&handle).await;
    assert_eq!(handle.lifecycle(), SessionLifecycle::Live);
    turn(&mut t).await;
    end_and_close(&recorder, &handle, t).await;
    let host_dir = std::env::current_dir().unwrap();
    let seen = recorder.all();
    assert!(seen.iter().any(|s| s.kind.is_none()), "no tool.call: {seen:#?}");
    for s in &seen {
        assert_eq!(s.session_id.as_deref(), Some(id), "{seen:#?}");
        assert_eq!(s.cwd.as_deref(), host_dir.to_str(), "{:?} names {:?}", s.kind, s.cwd);
    }
}

/// `--continue` of a chat that is not running takes the folder of the client
/// that resumed it: that is where its tools now run, so that is what its
/// hooks and tool calls name. Not the folder it was created in, and not the
/// host's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_continued_chat_names_the_folder_it_was_resumed_in() {
    let (_h, host, recorder) = setup().await;
    let (created_in, resumed_in) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    assert_ne!(created_in.path(), resumed_in.path());
    let (first, mut t) = session(&host, cfg_in(created_in.path())).await;
    turn(&mut t).await;
    end_and_close(&recorder, &first, t).await;
    let id = first.id.as_str();
    recorder.clear();

    let (resumed, mut t) = session(
        &host,
        SessionConfig {
            continue_session: Some(Some(id.to_string())),
            ..cfg_in(resumed_in.path())
        },
    )
    .await;
    assert_eq!(resumed.id.as_str(), id, "the same chat");
    turn(&mut t).await;
    end_and_close(&recorder, &resumed, t).await;
    let mut kinds = TURN.to_vec();
    kinds.extend([Some(HookKind::OnSessionStart), Some(HookKind::OnSessionEnd)]);
    assert_names(&recorder, id, resumed_in.path(), &kinds);
    assert_only_sessions(&recorder, &[id]);
}
