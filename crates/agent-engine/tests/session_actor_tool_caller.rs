//! `caller` on an extension tool call made from a chat's turn is the
//! author of that chat's own memory binding (`binding.author()`).
//! One host, one
//! recording extension, sessions run through the production actor →
//! runtime → stream loop against a loopback stub, like
//! `session_actor_hook_cwd.rs`.

mod session_actor_common;
use session_actor_common::*;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_core::memory::author::Author;
use agent_engine::extensions::hooks::events::{HookEvent, HookResult};
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

/// One extension tool call: whose it is, the author of the binding its tool
/// context came with, and the `caller` the handler was given for the wire.
#[derive(Debug, Clone, PartialEq)]
struct Call {
    session_id: Option<String>,
    bound: Option<Author>,
    caller: Option<Author>,
}

#[derive(Default)]
struct Recorder {
    /// What the tool context held, set by [`Admitted`] just before the call.
    bound: Mutex<Option<Author>>,
    calls: Mutex<Vec<Call>>,
}

impl Recorder {
    fn calls_of(&self, session_id: &str) -> Vec<Call> {
        let mut calls = self.calls.lock().unwrap().clone();
        calls.retain(|c| c.session_id.as_deref() == Some(session_id));
        calls
    }
}

#[async_trait::async_trait]
impl ExtensionHandler for Recorder {
    fn id(&self) -> &str {
        "caller-recorder"
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

/// The real `ExtensionTool` under the name the stub's turn calls, with a
/// built-in origin so the default session tool set admits it. It notes the
/// author of the context's binding, and the
/// call itself is `ExtensionTool::execute`. Turns here run one at a time.
struct Admitted(agent_engine::tools::ExtensionTool, Arc<Recorder>);

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
        *self.1.bound.lock().unwrap() = ctx
            .capabilities
            .memory_backend
            .as_ref()
            .map(|binding| binding.author().clone());
        self.0.execute(params, ctx).await
    }
}

/// The provider: every user message gets one `prompt_fixture` tool round,
/// then the answer.
async fn stub_tool_turns() -> String {
    let app = axum::Router::new().fallback(|body: String| async move {
        let request: Value = serde_json::from_str(&body).unwrap_or_default();
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

async fn setup() -> (Home, Arc<EngineHost>, Arc<Recorder>) {
    let home = Home::new();
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", stub_tool_turns().await);
    let host = host().await;
    let recorder = Arc::new(Recorder::default());
    let tool = agent_engine::tools::ExtensionTool::new(
        "caller-recorder",
        RegisteredExtensionToolSpec {
            name: "whoami".into(),
            description: "records the caller".into(),
            input_schema: json!({"type": "object"}),
        },
        recorder.clone(),
    );
    host.parts()
        .tools
        .write()
        .await
        .register(Arc::new(Admitted(tool, recorder.clone())));
    (home, host, recorder)
}

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

/// One turn: a tool call, then the final answer.
async fn turn(t: &mut LocalTransport) {
    t.send(submit("who am I?")).await.unwrap();
    until(t, |e| matches!(e, SessionEventWire::Idle)).await;
}

/// The session made `count` calls, each with a valid `caller` that is the
/// author of the binding its context held. Returns that one author.
fn the_caller(recorder: &Recorder, session_id: &str, count: usize) -> Author {
    let calls = recorder.calls_of(session_id);
    assert_eq!(calls.len(), count, "{calls:#?}");
    let caller = calls[0].caller.clone().expect("a chat's call names its caller");
    caller.validate().unwrap();
    assert!(caller.parent.is_none(), "a chat has no parent: {caller:?}");
    for call in &calls {
        assert!(call.bound.is_some(), "a turn's tool context has a binding");
        assert_eq!(call.caller, call.bound, "the binding's author, not another");
        assert_eq!(call.caller.as_ref(), Some(&caller), "one run, one caller");
    }
    caller
}

/// Two live chats on one host: each call names its chat's own author, the
/// same on every turn of the chat, and the two chats share nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn each_chats_tool_calls_name_its_own_bindings_author() {
    let (_h, host, recorder) = setup().await;
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = host.create_session(cfg_in(dir_a.path())).await.unwrap();
    let b = host.create_session(cfg_in(dir_b.path())).await.unwrap();
    let (mut ta, mut tb) = (attach(&a).await, attach(&b).await);
    for _ in 0..2 {
        turn(&mut ta).await;
        turn(&mut tb).await;
    }
    let of_a = the_caller(&recorder, a.id.as_str(), 2);
    let of_b = the_caller(&recorder, b.id.as_str(), 2);
    assert_ne!(of_a.actor, of_b.actor);
    assert_ne!(of_a.group, of_b.group);
    assert_eq!(recorder.calls.lock().unwrap().len(), 4, "every call is one chat's");
    end(&mut ta).await;
    end(&mut tb).await;
}

/// Parking drops the runtime and waking builds a new one. Whatever author
/// the woken chat's binding has, its calls name that one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_chat_woken_from_park_names_the_author_of_the_binding_it_woke_with() {
    let (_h, host, recorder) = setup().await;
    let dir = tempfile::tempdir().unwrap();
    let handle = host.create_session(cfg_in(dir.path())).await.unwrap();
    let mut t = attach(&handle).await;
    turn(&mut t).await;
    the_caller(&recorder, handle.id.as_str(), 1);

    t.send(SessionCommand::Detach {
        client: t.client_id(),
    })
    .await
    .unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::ClientLeft { .. })).await;
    handle.send(SessionCommand::Park).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while handle.lifecycle() != SessionLifecycle::Parked {
        assert!(tokio::time::Instant::now() < deadline, "never parked");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    recorder.calls.lock().unwrap().clear();

    let mut t = attach(&handle).await;
    assert_eq!(handle.lifecycle(), SessionLifecycle::Live);
    turn(&mut t).await;
    the_caller(&recorder, handle.id.as_str(), 1);
    end(&mut t).await;
}
