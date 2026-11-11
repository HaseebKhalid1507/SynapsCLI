//! A host prompt over the daemon endpoint (a UDS on Unix, the named pipe on
//! Windows) against the REAL actor: hello → welcome (protocol 3), create,
//! submit; the tool prompts; the client detaches; a new client re-attaches,
//! finds the prompt pending in its snapshot and answers it; the tool sees
//! the value and the turn ends idle. `session_actor.rs` covers the same
//! prompt over `LocalTransport` only; the daemon suites use `echo_factory`,
//! which never prompts.

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::{Daemon, DaemonOpts};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts};
use phase2::*;
use serial_test::serial;
use synaps_cli::{LlmEvent, StreamEvent};

/// Prompts through the stream's `SecretPromptHandle`; reports the length
/// only (copy of `session_actor_differential.rs::PromptFixtureTool`).
struct PromptFixtureTool;

#[async_trait::async_trait]
impl agent_engine::Tool for PromptFixtureTool {
    fn name(&self) -> &str {
        "prompt_fixture"
    }
    fn description(&self) -> &str {
        "prompts"
    }
    fn parameters(&self) -> agent_engine::Value {
        serde_json::json!({"type": "object"})
    }
    fn origin(&self) -> agent_engine::tools::ToolOrigin {
        agent_engine::tools::ToolOrigin::Builtin
    }
    async fn execute(&self, _params: agent_engine::Value, ctx: agent_engine::ToolContext) -> agent_engine::Result<String> {
        let handle = ctx.capabilities.secret_prompt.expect("the actor passes its SecretPromptHandle to the stream");
        Ok(match handle.prompt("Secret".into(), "enter secret".into()).await {
            Some(v) => format!("answered:{}", v.len()),
            None => "cancelled".to_string(),
        })
    }
}

/// One Anthropic SSE reply that calls tool `name` (copy of
/// `session_actor_differential.rs::sse_tool_call`).
fn sse_tool_call(name: &str, id: &str) -> &'static str {
    Box::leak(
        format!(
            concat!(
                "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_d1\",\"type\":\"message\",",
                "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
                "\"stop_sequence\":null,\"usage\":{{\"input_tokens\":10,\"output_tokens\":0,",
                "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}}}\n\n",
                "data: {{\"type\":\"content_block_start\",\"index\":0,",
                "\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"{name}\"}}}}\n\n",
                "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
                "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\",",
                "\"stop_sequence\":null}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":5,",
                "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}\n\n",
                "data: {{\"type\":\"message_stop\"}}\n\n",
            ),
            id = id,
            name = name,
        )
        .into_boxed_str(),
    )
}

async fn until(t: &mut SocketTransport, pred: impl Fn(&SessionEventWire) -> bool) -> SessionEventWire {
    loop {
        let env = tokio::time::timeout(Duration::from_secs(10), t.next_event()).await.expect("timely").expect("open");
        if pred(&env.event) {
            return env.event;
        }
    }
}

fn is_tool_result(e: &SessionEventWire) -> bool {
    matches!(e, SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::ToolResult { .. })))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_pending_prompt_survives_reattach_and_is_answered_over_the_endpoint() {
    let guard = HomeGuard::new();
    let script: &'static [&'static str] =
        Box::leak(vec![sse_tool_call("prompt_fixture", "toolu_fixture"), ANTHROPIC_SSE].into_boxed_slice());
    let (url, _hits, _) = spawn_stub(Script::SeqSse(script)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let host: Arc<EngineHost> = EngineHost::boot_and_install(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .expect("host boot");
    host.parts().tools.write().await.register(Arc::new(PromptFixtureTool));
    let run = guard.base_dir().join("run");
    let d = Daemon::start(host, DaemonOpts { runtime_dir: Some(run), ..Default::default() }).await.expect("daemon start");
    let paths = d.paths.clone();

    // Hello → Welcome over the endpoint.
    let conn = SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Tui)).await.unwrap();
    assert_eq!(conn.welcome.protocol_version, 3);
    let (mut a, _) = SocketTransport::attach(
        conn,
        Attach::create(
            SessionConfig {
                cwd: Some(guard.home.path().to_path_buf()),
                model_override: Some("claude-sonnet-4-5".into()),
                persist: false,
                client_input: Some(ClientInput::Exclusive),
                ..Default::default()
            },
            AttachMode::Mirror,
        ),
    )
    .await
    .unwrap();
    let sid = a.session_id().clone();

    a.send_from_self(SessionCommand::Submit { text: "ask me".into(), attachments: vec![] }).await.unwrap();
    let SessionEventWire::Prompt(req) = until(&mut a, |e| matches!(e, SessionEventWire::Prompt(_))).await else {
        unreachable!()
    };
    assert_eq!(req.title, "Secret");

    // The only client leaves; the prompt stays pending (abandon window: 1 h by default).
    a.send_from_self(SessionCommand::Detach { client: a.client_id() }).await.unwrap();
    drop(a);

    let conn = SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Attach)).await.unwrap();
    let (mut b, snap) =
        SocketTransport::attach(conn, Attach::Existing { session_id: sid, mode: AttachMode::Mirror }).await.unwrap();
    assert_eq!(snap.pending_prompts.len(), 1, "the prompt survives the detach");
    assert_eq!(snap.pending_prompts[0].id, req.id);
    assert_eq!(snap.pending_prompts[0].title, "Secret");

    b.send_from_self(SessionCommand::Answer { prompt_id: req.id, value: Some("s3cret".into()) }).await.unwrap();
    let SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::ToolResult { result, .. })) =
        until(&mut b, is_tool_result).await
    else {
        unreachable!()
    };
    assert_eq!(result, "answered:6", "the tool got the answer over the endpoint");
    until(&mut b, |e| matches!(e, SessionEventWire::Idle)).await;

    b.send_from_self(SessionCommand::End { reason: EndReason::ClientQuit }).await.unwrap();
    d.shutdown_token().cancel();
    d.wait().await;
}
