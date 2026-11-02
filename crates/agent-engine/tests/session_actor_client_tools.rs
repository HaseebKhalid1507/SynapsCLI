//! Client tools v1 (docs/client-tools.md, IK1 seam B'): tools the session's
//! input owner registers over the wire and answers itself. The overlay is
//! RAM only, per session, and offered and run only on the session's own
//! root stream: a worker never sees it, and a call from a worker or from a
//! stream without a conversation id is refused before any frame is sent.
//!
//! The actor tests run a real session with two attached clients and a
//! loopback provider. In-process every subscriber sees every envelope, so
//! they assert the `client` each addressed event names; the daemon's own
//! routing (to that client only) is `daemon::conn`'s unit test.
mod session_actor_common;
use session_actor_common::*;

use agent_engine::runtime::client_tools::{
    Outbound, Overlay, Registration, ToolDef, MAX_ARGS_BYTES, TEST_TAKE_DELAY_ENV, TEST_TIMEOUT_ENV,
};
use agent_engine::session::client_tools::{
    daemon_generation, set_daemon_generation, ClientToolsAck, ToolInvokeFrame, ToolReturnAckFrame,
};
use agent_engine::session::{
    AttachMode, CheckpointReason, ClientKind, ClientMeta, ClientTransport, Envelope, LocalTransport,
    SessionCommand, SessionConfig, SessionEventWire, SessionHandle, SessionQuery, SessionSetting,
};
use agent_engine::{EngineHost, Runtime};
use futures::StreamExt;
use serde_json::{json, Map, Value};
use serial_test::serial;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The registered tool: `pria-desktop` + `pria_vault_get`.
const WIRE: &str = "pria-desktop_pria_vault_get";

/// The §7 gate text a refused call gets.
const GATE_TEXT: &str = "Tool execution failed: client tool pria-desktop_pria_vault_get runs only in the session's own conversation, not in a worker or without a conversation id.";

/// The registered description: a definition must never reach a disk.
const DESCRIPTION: &str = "Get a vault record CT_DEF_SENTINEL_77c1";

/// What the client returns: memory-only, so it reaches the live request
/// and nothing else.
const SENTINEL: &str = "CT_RESULT_SENTINEL_3a9f3a9f";

/// The marker the oversized arguments repeat.
const ARGS_MARKER: &str = "CT_ARGS_SENTINEL_5e1f";

/// What a memory-only client call's arguments quote: no hook may see it.
const HOOK_ARGS: &str = "CT_HOOK_ARGS_SENTINEL_4d2c";

/// An ordinary tool's arguments: hooks see these.
const READ_ARGS: &str = "CT_HOOK_READ_ARGS_7b2e";

/// One call of the client tool by its wire name.
fn sse_call(call_id: &str) -> String {
    format!(
        concat!(
            "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_c1\",\"type\":\"message\",",
            "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
            "\"stop_sequence\":null,\"usage\":{{\"input_tokens\":10,\"output_tokens\":0,",
            "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}}}\n\n",
            "data: {{\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"{wire}\"}}}}\n\n",
            "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
            "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\",",
            "\"stop_sequence\":null}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":5,",
            "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}\n\n",
            "data: {{\"type\":\"message_stop\"}}\n\n",
        ),
        id = call_id,
        wire = WIRE,
    )
}

/// One call of the client tool with `args` (compact JSON) as its input.
fn sse_call_with(call_id: &str, args: &str) -> String {
    sse_tool(call_id, WIRE, args)
}

/// One call of tool `name` with `args` (compact JSON) as its input.
fn sse_tool(call_id: &str, name: &str, args: &str) -> String {
    let usage = json!({"input_tokens": 10, "output_tokens": 5, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0});
    [
        json!({"type": "message_start", "message": {"id": "msg_c2", "type": "message", "role": "assistant",
            "content": [], "model": MODEL, "stop_reason": null, "stop_sequence": null, "usage": usage}}),
        json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": call_id, "name": name, "input": {}}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": args}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": null}, "usage": usage}),
        json!({"type": "message_stop"}),
    ]
    .iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect()
}

/// Arguments over [`MAX_ARGS_BYTES`] as compact JSON.
fn big_args() -> String {
    json!({"blob": ARGS_MARKER.repeat(MAX_ARGS_BYTES / ARGS_MARKER.len() + 1)}).to_string()
}

/// The stub's answer, chosen by the request's LAST message: a prompt with
/// `call-it` gets one call of the client tool, `call-big` one with
/// oversized arguments, `call-hooked-N` one quoting [`HOOK_ARGS`] (call
/// `toolu_hkN`), `call-read` a `read` of [`READ_ARGS`], anything else (a
/// tool result included) a plain text answer.
fn script(body: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let last = parsed["messages"]
        .as_array()
        .and_then(|m| m.last())
        .map(|m| m["content"].to_string())
        .unwrap_or_default();
    if last.contains("tool_result") {
        SSE_HI.to_string()
    } else if last.contains("call-it") {
        sse_call("toolu_ct1")
    } else if last.contains("call-big") {
        sse_call_with("toolu_big", &big_args())
    } else if let Some(at) = last.find("call-hooked-") {
        let n = &last[at + "call-hooked-".len()..][..1];
        sse_call_with(&format!("toolu_hk{n}"), &json!({"record": HOOK_ARGS}).to_string())
    } else if last.contains("call-read") {
        sse_tool("toolu_rd", "read", &json!({"path": READ_ARGS}).to_string())
    } else {
        SSE_HI.to_string()
    }
}

/// A loopback provider that records every request body (no body limit:
/// the history of the edge tests runs past axum's default).
async fn provider() -> Arc<Mutex<Vec<String>>> {
    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured = bodies.clone();
    let app = axum::Router::new()
        .fallback(move |body: String| {
            let captured = captured.clone();
            async move {
                use axum::response::IntoResponse;
                let sse = script(&body);
                captured.lock().unwrap().push(body);
                ([("content-type", "text/event-stream")], sse).into_response()
            }
        })
        .layer(axum::extract::DefaultBodyLimit::disable());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", format!("http://{addr}"));
    bodies
}

/// Streaming request bodies sent since `from`.
fn streamed_since(bodies: &Arc<Mutex<Vec<String>>>, from: usize) -> Vec<String> {
    bodies.lock().unwrap()[from..]
        .iter()
        .filter(|b| b.contains("\"stream\":true"))
        .cloned()
        .collect()
}

/// The tool names a request body offers.
fn offered(body: &str) -> Vec<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
    parsed["tools"]
        .as_array()
        .map(|tools| tools.iter().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// Pria Desktop's IK1 registration: one tool.
fn registration() -> Registration {
    Registration {
        registration_id: agent_engine::runtime::client_tools::mint_registration_id(),
        extension_id: "pria-desktop".into(),
        require_memory_only: false,
        tools: vec![ToolDef {
            name: "pria_vault_get".into(),
            description: "Get a vault record".into(),
            input_schema: serde_json::json!({"type": "object"}),
            result_family: "pria.vault".into(),
            outcomes: vec!["ok".into(), "not_found".into()],
        }],
    }
}

/// One whole turn on `runtime`; the request bodies it sent.
async fn turn(runtime: &Runtime, bodies: &Arc<Mutex<Vec<String>>>, prompt: &str) -> Vec<String> {
    let from = bodies.lock().unwrap().len();
    let mut stream = runtime.run_stream(prompt.to_owned(), tokio_util::sync::CancellationToken::new()).await;
    tokio::time::timeout(Duration::from_secs(30), async { while stream.next().await.is_some() {} })
        .await
        .expect("the turn ends");
    streamed_since(bodies, from)
}

// ── The actor tests' helpers ─────────────────────────────────────────────

fn tui() -> ClientMeta {
    ClientMeta::new(ClientKind::Tui)
}

/// A session created with `env` set (the overlay reads its test knobs at
/// creation), then cleared.
async fn session(host: &Arc<EngineHost>, config: SessionConfig, env: &[(&str, &str)]) -> SessionHandle {
    for (key, value) in env {
        std::env::set_var(key, value);
    }
    let handle = host.create_session(config).await.unwrap();
    for (key, _) in env {
        std::env::remove_var(key);
    }
    handle
}

async fn attach(handle: &SessionHandle, mode: AttachMode) -> LocalTransport {
    LocalTransport::attach_with(handle.clone(), tui(), mode).await.unwrap().0
}

fn tool(name: &str, description: &str) -> Value {
    json!({"name": name, "description": description, "input_schema": {"type": "object"},
        "result_family": "pria.vault", "outcomes": ["ok", "not_found"]})
}

fn fields(extension_id: &str, require_memory_only: bool, tools: Vec<Value>) -> Map<String, Value> {
    let Value::Object(fields) = json!({"contract": 1, "daemon_generation": daemon_generation(),
        "extension_id": extension_id, "require_memory_only": require_memory_only, "tools": tools})
    else {
        unreachable!()
    };
    fields
}

/// Pria Desktop's IK1 register: `pria_vault_get`.
fn ik1(require_memory_only: bool) -> Map<String, Value> {
    fields("pria-desktop", require_memory_only, vec![tool("pria_vault_get", DESCRIPTION)])
}

/// `register_client_tools` from `t`'s client; the reply addressed to it.
async fn register(t: &mut LocalTransport, id: u64, fields: Map<String, Value>) -> ClientToolsAck {
    let me = t.client_id();
    t.send_from_self(SessionCommand::RegisterClientTools { id, fields }).await.unwrap();
    let events = until(t, |e| {
        matches!(e, SessionEventWire::ClientTools { client, ack } if *client == Some(me) && ack.id == Some(id))
    })
    .await;
    match &events.last().unwrap().event {
        SessionEventWire::ClientTools { ack, .. } => (**ack).clone(),
        _ => unreachable!(),
    }
}

async fn refused(t: &mut LocalTransport, id: u64, fields: Map<String, Value>, code: &str) -> ClientToolsAck {
    let ack = register(t, id, fields).await;
    assert_eq!(ack.outcome, "refused", "register {id}");
    assert_eq!(ack.code.as_deref(), Some(code), "register {id}: {:?}", ack.message);
    ack
}

fn return_fields(invocation_id: &str, outcome: &str, content: &str, memory_only: bool) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("invocation_id".into(), json!(invocation_id));
    fields.insert("outcome".into(), json!(outcome));
    fields.insert("content".into(), json!(content));
    if memory_only {
        fields.insert("_meta".into(), json!({"synaps/memory_only": true}));
    }
    fields
}

/// `tool_return` from `t`'s client; the ack addressed to it.
async fn tool_return(t: &mut LocalTransport, id: u64, fields: Map<String, Value>) -> ToolReturnAckFrame {
    let me = t.client_id();
    t.send_from_self(SessionCommand::ToolReturn { id, fields }).await.unwrap();
    let events = until(t, |e| {
        matches!(e, SessionEventWire::ToolReturnAck { client, ack } if *client == Some(me) && ack.id == id)
    })
    .await;
    match &events.last().unwrap().event {
        SessionEventWire::ToolReturnAck { ack, .. } => ack.clone(),
        _ => unreachable!(),
    }
}

async fn accepted(t: &mut LocalTransport, id: u64, fields: Map<String, Value>) {
    let ack = tool_return(t, id, fields).await;
    assert_eq!((ack.outcome.as_str(), ack.code.as_deref()), ("accepted", None), "return {id}");
}

async fn return_refused(t: &mut LocalTransport, id: u64, fields: Map<String, Value>, code: &str) {
    let ack = tool_return(t, id, fields).await;
    assert_eq!((ack.outcome.as_str(), ack.code.as_deref()), ("refused", Some(code)), "return {id}");
}

/// The next `tool_invoke`; it must name `t`'s client (the installer).
async fn invoked(t: &mut LocalTransport) -> ToolInvokeFrame {
    let me = t.client_id();
    let events = until(t, |e| matches!(e, SessionEventWire::ToolInvoke { .. })).await;
    assert!(no_cancel(&events), "a tool_cancel came before the invoke");
    match &events.last().unwrap().event {
        SessionEventWire::ToolInvoke { client, invoke } => {
            assert_eq!(*client, Some(me), "the invoke goes to the installer");
            (**invoke).clone()
        }
        _ => unreachable!(),
    }
}

/// The next `tool_cancel`: its client, invocation and reason.
async fn cancelled(t: &mut LocalTransport) -> (Option<agent_engine::session::ClientId>, String, String) {
    let events = until(t, |e| matches!(e, SessionEventWire::ToolCancel { .. })).await;
    match &events.last().unwrap().event {
        SessionEventWire::ToolCancel { client, cancel } => {
            (*client, cancel.invocation_id.clone(), cancel.reason.clone())
        }
        _ => unreachable!(),
    }
}

fn no_cancel(events: &[Envelope]) -> bool {
    !events.iter().any(|e| matches!(e.event, SessionEventWire::ToolCancel { .. }))
}

fn client_tool_event(event: &SessionEventWire) -> bool {
    matches!(
        event,
        SessionEventWire::ClientTools { .. }
            | SessionEventWire::ToolInvoke { .. }
            | SessionEventWire::ToolCancel { .. }
            | SessionEventWire::ToolReturnAck { .. }
    )
}

async fn idle(t: &mut LocalTransport) -> Vec<Envelope> {
    until(t, |e| matches!(e, SessionEventWire::Idle)).await
}

async fn query(t: &mut LocalTransport, id: u64, q: SessionQuery) -> Value {
    t.send(SessionCommand::Query { id, query: q }).await.unwrap();
    let events = until(t, |e| matches!(e, SessionEventWire::QueryResult { id: got, .. } if *got == id)).await;
    match &events.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => value.clone(),
        _ => unreachable!(),
    }
}

/// The answer to `q` as a client receives it (the wire event's JSON).
async fn query_wire(t: &mut LocalTransport, id: u64, q: SessionQuery) -> Value {
    t.send(SessionCommand::Query { id, query: q }).await.unwrap();
    let events = until(t, |e| matches!(e, SessionEventWire::QueryResult { id: got, .. } if *got == id)).await;
    let envelope = agent_engine::session::wire::WireEnvelope::for_client(events.last().unwrap().clone()).unwrap();
    serde_json::to_value(envelope).unwrap()["event"].clone()
}

/// The first object in `value` (depth first) that `matches`.
fn find(value: &Value, matches: &dyn Fn(&Map<String, Value>) -> bool) -> Option<Value> {
    match value {
        Value::Object(o) if matches(o) => Some(value.clone()),
        Value::Object(o) => o.values().find_map(|v| find(v, matches)),
        Value::Array(a) => a.iter().find_map(|v| find(v, matches)),
        _ => None,
    }
}

/// IK1's largest result: 196,608 bytes of plain ASCII holding [`SENTINEL`].
fn ik1_edge() -> String {
    let head = format!("record: {SENTINEL} ");
    let content = format!("{head}{}", "x".repeat(196_608 - head.len()));
    assert_eq!(content.len(), 196_608);
    content
}

/// Some file under `dir` holds `needle`.
fn holds(dir: &std::path::Path, needle: &str) -> bool {
    std::fs::read_dir(dir).unwrap().any(|entry| {
        let path = entry.unwrap().path();
        if path.is_dir() {
            holds(&path, needle)
        } else {
            path.is_file() && std::fs::read(&path).unwrap().windows(needle.len()).any(|w| w == needle.as_bytes())
        }
    })
}

/// No file under `dir` holds any of `needles`.
fn scan(dir: &std::path::Path, needles: &[&str]) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            scan(&path, needles);
        } else if path.is_file() {
            let bytes = std::fs::read(&path).unwrap();
            for needle in needles {
                assert!(
                    !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                    "{needle} reached {}",
                    path.display()
                );
            }
        }
    }
}

/// A builtin-origin tool whose name is the wire name `acme_lookup` would
/// have: an `acme` registration of `lookup` collides with it.
struct AcmeLookup;

#[async_trait::async_trait]
impl agent_engine::Tool for AcmeLookup {
    fn name(&self) -> &str {
        "acme_lookup"
    }
    fn description(&self) -> &str {
        "an existing tool"
    }
    fn parameters(&self) -> agent_engine::Value {
        json!({"type": "object"})
    }
    async fn execute(&self, _params: agent_engine::Value, _ctx: agent_engine::ToolContext) -> agent_engine::Result<String> {
        Ok("acme".into())
    }
}

// ── 1 + 7 ───────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn register_echoes_and_is_add_only() {
    let h = Home::new();
    let _bodies = provider().await;
    let host = host().await;
    host.parts().tools.write().await.register(Arc::new(AcmeLookup));
    let mut config = cfg();
    config.memory_only_tools = vec!["pria_vault_*".into()];
    let handle = session(&host, config, &[]).await;
    let mut a = attach(&handle, AttachMode::Mirror).await;
    let mut b = attach(&handle, AttachMode::Mirror).await;
    assert_eq!(a.input_owner(), Some(a.client_id()), "precondition: the first client owns input");

    // Applied: the reply echoes the registration in force.
    let ack = register(&mut a, 1, ik1(true)).await;
    assert_eq!((ack.outcome.as_str(), ack.code.as_deref()), ("applied", None), "{:?}", ack.message);
    let registration_id = ack.registration_id.clone().expect("a registration id");
    assert!(registration_id.starts_with("cr_") && registration_id.len() == 35, "{registration_id}");
    assert_eq!(ack.client, Some(a.client_id()));
    assert_eq!(ack.contract, 1);
    assert_eq!(ack.daemon_generation, daemon_generation());
    assert!(ack.conversation_id.is_some(), "the conversation id is echoed");
    assert_eq!(ack.forked_from, None);
    assert_eq!(ack.extension_id.as_deref(), Some("pria-desktop"));
    assert!(ack.require_memory_only);
    assert_eq!(ack.tools.len(), 1);
    let echoed = &ack.tools[0];
    assert_eq!(
        (echoed.name.as_str(), echoed.runtime_name.as_str(), echoed.wire_name.as_str()),
        ("pria_vault_get", "pria-desktop:pria_vault_get", WIRE)
    );
    assert_eq!(echoed.description, DESCRIPTION);
    assert!(echoed.memory_only_marked, "the session's glob marks the tool");
    assert_eq!(ack.pending, 0);
    assert_eq!(
        (ack.limits.max_tools, ack.limits.max_pending, ack.limits.timeout_s, ack.limits.max_content_bytes),
        (32, 8, 120, 262_144)
    );

    // Identical: unchanged, same id.
    let ack = register(&mut a, 2, ik1(true)).await;
    assert_eq!((ack.outcome.as_str(), ack.registration_id.as_deref()), ("unchanged", Some(registration_id.as_str())));

    // 7 (R1): require_memory_only flipped true -> false is def_changed;
    // the registration and its marking are unchanged.
    let ack = refused(&mut a, 3, ik1(false), "def_changed").await;
    assert_eq!(ack.registration_id.as_deref(), Some(registration_id.as_str()), "a refusal echoes the registration in force");
    assert!(ack.require_memory_only, "still require_memory_only");
    assert!(ack.tools[0].memory_only_marked, "still marked");
    // A changed definition: def_changed. Dropping a tool: omitted.
    refused(&mut a, 4, fields("pria-desktop", true, vec![tool("pria_vault_get", "another description")]), "def_changed").await;
    refused(&mut a, 5, fields("pria-desktop", true, vec![tool("pria_vault_list", "List records")]), "omitted").await;

    // A superset is applied under the same id.
    let both = || fields("pria-desktop", true, vec![tool("pria_vault_get", DESCRIPTION), tool("pria_vault_list", "List records")]);
    let ack = register(&mut a, 6, both()).await;
    assert_eq!((ack.outcome.as_str(), ack.registration_id.as_deref()), ("applied", Some(registration_id.as_str())));
    assert_eq!(ack.tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["pria_vault_get", "pria_vault_list"]);

    // A client that is not the input owner: not_owner, echoes nothing.
    let ack = refused(&mut b, 7, both(), "not_owner").await;
    assert_eq!(ack.client, Some(b.client_id()), "the reply names its sender");
    assert_eq!((ack.registration_id.as_ref(), ack.tools.len()), (None, 0), "not_owner echoes no registration");

    // A name equal to a tool the session already has: name_collision.
    refused(&mut a, 8, fields("acme", false, vec![tool("lookup", "Look up")]), "name_collision").await;
    // Another extension_id, no collision: extension_mismatch.
    refused(&mut a, 9, fields("acme", false, vec![tool("find", "Find")]), "extension_mismatch").await;
    // The contract and the generation are checked.
    let mut wrong = both();
    wrong.insert("contract".into(), json!(2));
    refused(&mut a, 10, wrong, "contract_unsupported").await;
    let mut wrong = both();
    wrong.insert("daemon_generation".into(), json!(daemon_generation() + 1));
    refused(&mut a, 11, wrong, "wrong_generation").await;
    // A tool the session does not mark, under require_memory_only.
    refused(
        &mut a,
        12,
        fields("pria-desktop", true, vec![tool("pria_vault_get", DESCRIPTION), tool("pria_vault_list", "List records"), tool("other", "Other")]),
        "not_marked",
    )
    .await;
    // client_tools = off in the config: feature_off.
    let config_path = h.dir.path().join(".synaps-cli/config");
    std::fs::write(&config_path, "client_tools = off\n").unwrap();
    refused(&mut a, 13, both(), "feature_off").await;
    std::fs::write(&config_path, "").unwrap();

    // Nothing above changed the registration.
    let ack = register(&mut a, 14, both()).await;
    assert_eq!((ack.outcome.as_str(), ack.registration_id.as_deref()), ("unchanged", Some(registration_id.as_str())));
    assert!(ack.tools.iter().all(|t| t.memory_only_marked));
    end(&mut a).await;
}

// ── 2 ───────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn invoke_then_return_is_accepted_once() {
    let h = Home::new();
    let bodies = provider().await;
    let host = host().await;
    let mut config = cfg();
    config.persist = true;
    config.memory_only_tools = vec!["pria_vault_*".into()];
    let handle = session(&host, config, &[]).await;
    let mut a = attach(&handle, AttachMode::Mirror).await;
    let mut b = attach(&handle, AttachMode::Mirror).await;
    let ack = register(&mut a, 1, ik1(true)).await;
    assert_eq!(ack.outcome, "applied");
    let registration_id = ack.registration_id.clone().unwrap();
    // 10 (R4): a client tool result is never cut or withheld by
    // max_tool_output, not even at 1 byte.
    a.send(SessionCommand::Set { id: 90, setting: SessionSetting::MaxToolOutput { bytes: 1 } }).await.unwrap();
    let events = until(&mut a, |e| matches!(e, SessionEventWire::SettingChanged(s) if s.id == 90)).await;
    assert!(matches!(&events.last().unwrap().event, SessionEventWire::SettingChanged(s) if s.ok), "max_tool_output = 1");

    let from = bodies.lock().unwrap().len();
    a.send(submit("call-it")).await.unwrap();
    let invoke = invoked(&mut a).await;
    assert!(invoke.invocation_id.starts_with("ci_") && invoke.invocation_id.len() == 35);
    assert_eq!(invoke.registration_id, registration_id);
    assert_eq!(Some(invoke.conversation_id.clone()), ack.conversation_id, "the invoke names the conversation");
    assert_eq!((invoke.tool.as_str(), invoke.wire_name.as_str(), invoke.tool_use_id.as_str()), ("pria_vault_get", WIRE, "toolu_ct1"));
    assert_eq!(invoke.daemon_generation, daemon_generation());
    assert!(!invoke.turn_correlation_id.is_empty(), "the invoke names its turn");
    assert_eq!(invoke.deadline_ms, 120_000);
    // The other client sees no invoke addressed to it.
    let seen = drain_for(&mut b, Duration::from_millis(200)).await;
    assert!(
        !seen.iter().any(|e| matches!(&e.event, SessionEventWire::ToolInvoke { client, .. } if *client == Some(b.client_id()))),
        "only the installer is sent the invoke"
    );
    // A client attaching now replays none of the client tool frames.
    let (_late, snapshot) = LocalTransport::attach_with(handle.clone(), tui(), AttachMode::Observe).await.unwrap();
    assert!(!snapshot.replay.iter().any(|e| client_tool_event(&e.event)), "a client tool frame entered the replay ring");

    // The installer answers: accepted, once. 10 (R4): IK1's 196,608-byte
    // edge under `max_tool_output = 1` (set above) is neither cut nor
    // withheld.
    let content = ik1_edge();
    accepted(&mut a, 2, return_fields(&invoke.invocation_id, "ok", &content, true)).await;
    let events = idle(&mut a).await;
    return_refused(&mut a, 3, return_fields(&invoke.invocation_id, "ok", &content, true), "not_pending").await;

    // The raw result reaches the live request only, whole.
    let sent = streamed_since(&bodies, from);
    assert_eq!(sent.len(), 2, "the prompt, then after the call");
    assert_eq!(offered(&sent[0]).last().map(String::as_str), Some(WIRE), "the tool is offered last");
    assert!(sent[1].contains(&content), "the raw result reaches the live request whole");
    let wire: Vec<Value> = events
        .iter()
        .filter_map(|e| agent_engine::session::wire::WireEnvelope::for_client(e.clone()))
        .map(|e| serde_json::to_value(&e).unwrap())
        .collect();
    assert!(!Value::Array(wire.clone()).to_string().contains(SENTINEL), "an event carried the memory-only result");
    // The tool card: the whole placeholder, the typed flag and the stamp.
    let card = wire
        .iter()
        .find_map(|e| find(e, &|o| o.get("llm") == Some(&json!("tool_result")) && o.get("tool_id") == Some(&json!("toolu_ct1"))))
        .expect("the call's tool result event");
    let placeholder = agent_engine::runtime::memory_only::placeholder(WIRE, "toolu_ct1");
    assert_eq!(card["result"], json!(placeholder), "the whole placeholder, not cut or withheld: {card}");
    assert_eq!(card["memory_only"], json!(true), "{card}");
    let stamp = json!({"registration_id": registration_id, "invocation_id": invoke.invocation_id,
        "extension_id": "pria-desktop", "tool": "pria_vault_get", "family": "pria.vault", "outcome": "ok",
        "memory_only": true});
    assert_eq!(card["client_tool"], stamp, "the stamp: {card}");
    // The history keeps the whole placeholder; the Messages answer and a
    // new client's Attached carry the stamp by tool_use_id.
    let answer = query_wire(&mut a, 4, SessionQuery::Messages).await;
    let messages = answer["value"].to_string();
    assert!(!messages.contains(SENTINEL), "the durable history holds the memory-only result");
    assert!(messages.contains(&serde_json::to_string(&placeholder).unwrap()), "the whole placeholder is kept: {messages}");
    assert_eq!(answer["client_tools"], json!({"toolu_ct1": stamp}), "the Messages answer");
    let (late, snapshot) = LocalTransport::attach_with(handle.clone(), tui(), AttachMode::Observe).await.unwrap();
    let attached = serde_json::to_value(agent_engine::session::wire::AttachedWire::new(late.client_id(), snapshot)).unwrap();
    assert_eq!(attached["client_tools"], json!({"toolu_ct1": stamp}), "Attached");
    drop(late);
    end(&mut a).await;
    tokio::time::timeout(Duration::from_secs(30), handle.closed()).await.expect("the actor exits");
    // The stamp is saved next to the messages (it names the registration
    // and the invocation); the result and the definition never are.
    assert!(holds(h.dir.path(), &invoke.invocation_id), "the stamp is saved with the session");
    scan(h.dir.path(), &[SENTINEL, DESCRIPTION]);
}

// ── 3 + 8 ───────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn foreign_and_late_returns_are_refused() {
    let h = Home::new();
    // PARITY (upstream dfa68045c): the merged context footprint counts the
    // over-1 MiB `toolu_big` call in history (~260k tokens) rather than the
    // stub's reported usage, so automatic context management would refuse
    // the round that carries the refusal text (no rollover can shrink the
    // current round in a 200k window). This test is about the client-tool
    // refusal, not context management: it runs with it off (the key is
    // `context_management.mode`; a bare `context_management` is no key).
    std::fs::write(
        h.dir.path().join(".synaps-cli/config"),
        "context_management.mode = off\n",
    )
    .unwrap();
    let bodies = provider().await;
    let host = host().await;
    let handle = session(&host, cfg(), &[(TEST_TIMEOUT_ENV, "6000")]).await;
    let mut a = attach(&handle, AttachMode::Mirror).await;
    let mut b = attach(&handle, AttachMode::Mirror).await;
    assert_eq!(register(&mut a, 1, ik1(false)).await.outcome, "applied");

    // 8: arguments over 1 MiB end the call with no frame.
    let from = bodies.lock().unwrap().len();
    a.send(submit("call-big")).await.unwrap();
    let events = idle(&mut a).await;
    assert!(!events.iter().any(|e| client_tool_event(&e.event)), "no frame for oversized arguments");
    let sent = streamed_since(&bodies, from);
    assert_eq!(sent.len(), 2);
    assert!(
        sent[1].contains(&format!("Tool execution failed: client tool {WIRE} refused: its arguments are over 1048576 bytes (call toolu_big). Send smaller arguments.")),
        "the model gets synaps_args_too_large's text"
    );

    // One call: a foreign return, a malformed one, an unknown outcome and an
    // over-limit content are refused and leave it pending.
    a.send(submit("call-it")).await.unwrap();
    let invoke = invoked(&mut a).await;
    let ok = |content: &str| return_fields(&invoke.invocation_id, "ok", content, false);
    return_refused(&mut b, 2, ok("from b"), "not_installer").await;
    let ack = tool_return(&mut a, 3, return_fields("ci_1", "ok", "x", false)).await;
    assert_eq!((ack.code.as_deref(), ack.invocation_id.as_ref()), (Some("invalid"), None), "an invalid return echoes no id");
    return_refused(&mut a, 4, return_fields(&invoke.invocation_id, "nope", "x", false), "unknown_outcome").await;
    return_refused(&mut a, 5, ok(&format!("{}x", "é".repeat(131_072))), "too_large").await;
    // Heavily escaped JSON at the IK1 edge (196,608 B decoded): accepted.
    let escaped = serde_json::to_string(&json!({"v": "\"\\\n\t".repeat(24_575)})).unwrap();
    assert_eq!(escaped.len(), 196_608);
    accepted(&mut a, 6, ok(&escaped)).await;
    idle(&mut a).await;

    // 262,144 decoded bytes: accepted whole.
    a.send(submit("call-it")).await.unwrap();
    let invoke = invoked(&mut a).await;
    accepted(&mut a, 7, return_fields(&invoke.invocation_id, "ok", &"é".repeat(131_072), false)).await;
    idle(&mut a).await;

    // No return within the timeout: tool_cancel{timeout}, the timeout text,
    // and a late return is not_pending.
    let from = bodies.lock().unwrap().len();
    a.send(submit("call-it")).await.unwrap();
    let invoke = invoked(&mut a).await;
    let (client, invocation_id, reason) = cancelled(&mut a).await;
    assert_eq!((client, invocation_id.as_str(), reason.as_str()), (Some(a.client_id()), invoke.invocation_id.as_str(), "timeout"));
    idle(&mut a).await;
    let sent = streamed_since(&bodies, from);
    assert!(sent[1].contains(&format!("client tool {WIRE} timed out after")), "the model gets the timeout text");
    return_refused(&mut a, 8, return_fields(&invoke.invocation_id, "ok", "late", false), "not_pending").await;
    end(&mut a).await;
    scan(h.dir.path(), &[ARGS_MARKER]);
}

// ── 4 + 9 ───────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn detach_and_turn_cancel_end_pending_calls() {
    let _home = Home::new();
    let bodies = provider().await;
    let host = host().await;
    let handle = session(&host, cfg(), &[]).await;
    let mut a = attach(&handle, AttachMode::Mirror).await;
    let mut b = attach(&handle, AttachMode::Mirror).await;
    assert_eq!(register(&mut a, 1, ik1(false)).await.outcome, "applied");

    // The installer detaches: the call ends at once with the detach text,
    // and no frame is sent.
    let from = bodies.lock().unwrap().len();
    a.send(submit("call-it")).await.unwrap();
    let invoke = invoked(&mut a).await;
    let detached_at = std::time::Instant::now();
    b.send(SessionCommand::Detach { client: a.client_id() }).await.unwrap();
    let events = idle(&mut b).await;
    assert!(detached_at.elapsed() < Duration::from_secs(20), "the detach ended the call, not its timeout");
    assert!(no_cancel(&events), "a detach sends no tool_cancel");
    let sent = streamed_since(&bodies, from);
    assert!(
        sent[1].contains(&format!("client tool {WIRE} ended: its client disconnected (call toolu_ct1).")),
        "the model gets the detach text"
    );
    return_refused(&mut a, 2, return_fields(&invoke.invocation_id, "ok", "late", false), "not_installer").await;

    // The new owner registers; Cancel ends its call: tool_cancel{turn_cancel}
    // and the turn's cancel phrase; a later return is not_pending.
    assert_eq!(b.input_owner(), Some(b.client_id()), "input passed to the remaining client");
    assert_eq!(register(&mut b, 3, ik1(false)).await.outcome, "applied");
    b.send(submit("call-it")).await.unwrap();
    let invoke = invoked(&mut b).await;
    b.send(SessionCommand::Cancel).await.unwrap();
    let (client, invocation_id, reason) = cancelled(&mut b).await;
    assert_eq!((client, invocation_id.as_str(), reason.as_str()), (Some(b.client_id()), invoke.invocation_id.as_str(), "turn_cancel"));
    idle(&mut b).await;
    let messages = query(&mut b, 4, SessionQuery::Messages).await.to_string();
    assert!(messages.contains("Canceled by user"), "the call's result is the cancel phrase");
    return_refused(&mut b, 5, return_fields(&invoke.invocation_id, "ok", "late", false), "not_pending").await;
    end(&mut b).await;

    // 9 (R3): an accepted result its authority lost before it was taken
    // never reaches the model. The take delay holds the woken call between
    // the accept and its take.
    let handle = session(&host, cfg(), &[(TEST_TAKE_DELAY_ENV, "1500")]).await;
    let mut p = attach(&handle, AttachMode::Mirror).await;
    let first = register(&mut p, 1, ik1(false)).await;
    assert_eq!(first.outcome, "applied");
    let from = bodies.lock().unwrap().len();
    p.send(submit("call-it")).await.unwrap();
    let invoke = invoked(&mut p).await;
    let stale = format!("stale {SENTINEL}");
    accepted(&mut p, 2, return_fields(&invoke.invocation_id, "ok", &stale, false)).await;
    let mut q = attach(&handle, AttachMode::Takeover).await;
    let (client, invocation_id, reason) = cancelled(&mut p).await;
    assert_eq!((client, invocation_id.as_str(), reason.as_str()), (Some(p.client_id()), invoke.invocation_id.as_str(), "owner_change"));
    let events = until(&mut p, |e| matches!(e, SessionEventWire::ClientTools { .. })).await;
    match &events.last().unwrap().event {
        SessionEventWire::ClientTools { client, ack } => {
            assert_eq!(*client, Some(p.client_id()));
            assert_eq!((ack.id, ack.outcome.as_str(), ack.code.as_deref()), (None, "dropped", Some("owner_change")));
            assert_eq!((ack.registration_id.as_ref(), ack.tools.len()), (None, 0), "the notice echoes nothing");
        }
        _ => unreachable!(),
    }
    idle(&mut q).await;
    let sent = streamed_since(&bodies, from);
    assert_eq!(sent.len(), 2);
    assert!(
        sent[1].contains(&format!("client tool {WIRE} ended: the session's input owner changed (call toolu_ct1).")),
        "the model gets the owner-change text"
    );
    assert!(!sent[1].contains(SENTINEL), "the accepted result was not taken");
    // The new owner's own registration: a later return of the old call is
    // not_pending; the old installer is not_installer.
    let second = register(&mut q, 3, ik1(false)).await;
    assert_eq!(second.outcome, "applied");
    assert_ne!(second.registration_id, first.registration_id, "a new registration");
    return_refused(&mut q, 4, return_fields(&invoke.invocation_id, "ok", "late", false), "not_pending").await;
    return_refused(&mut p, 5, return_fields(&invoke.invocation_id, "ok", "late", false), "not_installer").await;
    end(&mut q).await;
}

// ── 5 ───────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn worker_and_no_session_id_are_refused() {
    let _home = Home::new();
    let bodies = provider().await;
    let (overlay, mut frames) = Overlay::new();
    overlay.install(registration());

    // The session's root runtime: its own conversation, the overlay handed
    // to it explicitly. It offers the tool, after every other tool.
    let mut root = Runtime::new().await.unwrap();
    root.set_model(MODEL.into());
    root.set_session_id(Some("conv-client-tools".into()));
    root.share_client_tools(&overlay);
    assert_eq!(root.client_tool_wire_names(), [WIRE]);
    let sent = turn(&root, &bodies, "hello").await;
    assert_eq!(sent.len(), 1);
    let tools = offered(&sent[0]);
    assert_eq!(tools.last().map(String::as_str), Some(WIRE), "the root stream offers the client tool last: {tools:?}");
    // Never in the shared registry.
    let shared = root.tools_shared();
    assert!(shared.read().await.get("pria-desktop:pria_vault_get").is_none());
    assert!(shared.read().await.get(WIRE).is_none());

    // A worker's runtime is a clone: it starts without the overlay.
    let mut worker = root.clone();
    assert!(worker.client_tool_wire_names().is_empty(), "a clone inherits no client tools");
    worker.set_delegation_parent(Some("parent-session".into()));
    // Even handed the overlay, a stream with a delegation parent (a worker)
    // and one without a conversation id neither offer the tool nor run it.
    worker.share_client_tools(&overlay);
    let mut orphan = root.clone();
    orphan.set_session_id(None);
    orphan.share_client_tools(&overlay);
    for (label, runtime) in [("worker", &worker), ("no session id", &orphan)] {
        let sent = turn(runtime, &bodies, "call-it").await;
        assert_eq!(sent.len(), 2, "{label}: the prompt, then after the call");
        assert!(!offered(&sent[0]).iter().any(|t| t == WIRE), "{label}: the tool is not offered");
        assert!(sent[1].contains(GATE_TEXT), "{label}: the call gets the gate text");
        assert!(frames.try_recv().is_err(), "{label}: no frame was sent");
        assert_eq!(overlay.pending(), 0, "{label}: nothing is pending");
    }
    assert!(overlay.registration().is_some(), "a refusal leaves the registration in force");
}

// ── 6 ───────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn reload_drops_the_registration() {
    let h = Home::new();
    let bodies = provider().await;
    let host = host().await;
    let mut config = cfg();
    config.persist = true;
    let handle = session(&host, config, &[]).await;
    let mut a = attach(&handle, AttachMode::Mirror).await;
    let ack = register(&mut a, 1, ik1(false)).await;
    assert_eq!(ack.outcome, "applied");
    let old_id = ack.registration_id.clone().unwrap();
    let from = bodies.lock().unwrap().len();
    a.send(submit("hello")).await.unwrap();
    idle(&mut a).await;
    assert_eq!(offered(&streamed_since(&bodies, from)[0]).last().map(String::as_str), Some(WIRE));

    // A reload with a call pending: tool_cancel{reload} before the record.
    let journal = query(&mut a, 2, SessionQuery::Status).await["session"].as_str().unwrap().to_owned();
    a.send(submit("call-it")).await.unwrap();
    let invoke = invoked(&mut a).await;
    a.send(SessionCommand::Checkpoint { reason: CheckpointReason::Reload, deadline: None, skip_quiesce_grace: true })
        .await
        .unwrap();
    let events = until(&mut a, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == agent_engine::session::wire::CHECKPOINT_QUERY_ID)
    })
    .await;
    let cancels: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.event {
            SessionEventWire::ToolCancel { client, cancel } => Some((*client, cancel.invocation_id.clone(), cancel.reason.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(cancels, [(Some(a.client_id()), invoke.invocation_id.clone(), "reload".to_string())]);
    // Then the dropped notice: if the reload aborts or is refused, the app
    // knows to register again.
    let notices: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.event {
            SessionEventWire::ClientTools { client, ack } if ack.outcome == "dropped" => Some((*client, ack.code.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(notices, [(Some(a.client_id()), Some("reload".to_string()))]);
    let value = match &events.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => value.clone(),
        _ => unreachable!(),
    };
    let record: agent_engine::session::SessionReloadRecord = serde_json::from_value(value["record"].clone()).unwrap();
    let text = serde_json::to_string(&record).unwrap();
    for needle in [old_id.as_str(), DESCRIPTION, "pria.vault", "client_tools", "invocation"] {
        assert!(!text.contains(needle), "the reload record holds {needle}");
    }
    end(&mut a).await;
    tokio::time::timeout(Duration::from_secs(30), handle.closed()).await.expect("the actor exits");

    // The next image (generation 2) restores the session: no client tools.
    set_daemon_generation(2);
    let mut restored = record.config;
    restored.name = None;
    restored.await_extensions = false;
    restored.continue_session = Some(Some(journal));
    restored.continue_lookup = agent_engine::session::ContinueLookup::ExactJournalId;
    let handle = session(&host, restored, &[]).await;
    let mut a = attach(&handle, AttachMode::Mirror).await;
    let from = bodies.lock().unwrap().len();
    a.send(submit("hello")).await.unwrap();
    idle(&mut a).await;
    assert!(!offered(&streamed_since(&bodies, from)[0]).iter().any(|t| t == WIRE), "the tools are gone after the reload");
    // The old generation is refused; a fresh register gets a new id.
    let mut old = ik1(false);
    old.insert("daemon_generation".into(), json!(1));
    refused(&mut a, 3, old, "wrong_generation").await;
    let ack = register(&mut a, 4, ik1(false)).await;
    set_daemon_generation(1);
    assert_eq!(ack.outcome, "applied");
    assert_ne!(ack.registration_id.as_deref(), Some(old_id.as_str()), "a new registration id");
    let from = bodies.lock().unwrap().len();
    a.send(submit("hello")).await.unwrap();
    idle(&mut a).await;
    assert_eq!(offered(&streamed_since(&bodies, from)[0]).last().map(String::as_str), Some(WIRE));
    end(&mut a).await;
    scan(h.dir.path(), &[DESCRIPTION, &old_id]);
}

// ── 11: hooks see a memory-only call's name only ────────────────────────

/// A tool-call hook that records every event it is shown, as the JSON an
/// extension process is sent, and answers a `before_tool_call` with
/// `before` (Continue when unset) and an `after_tool_call` with Continue.
#[derive(Default)]
struct HookSpy {
    seen: Mutex<Vec<Value>>,
    before: Mutex<Option<agent_engine::extensions::hooks::events::HookResult>>,
}

impl HookSpy {
    /// The events seen for tool `name`, in order; they are cleared.
    fn take(&self, name: &str) -> Vec<Value> {
        let mut seen = self.seen.lock().unwrap();
        let (mine, rest): (Vec<Value>, Vec<Value>) = seen.drain(..).partition(|e| e["tool_name"] == name);
        *seen = rest;
        mine
    }
}

#[async_trait::async_trait]
impl agent_engine::extensions::runtime::ExtensionHandler for HookSpy {
    fn id(&self) -> &str {
        "hook-spy"
    }
    async fn handle(
        &self,
        event: &agent_engine::extensions::hooks::events::HookEvent,
    ) -> agent_engine::extensions::hooks::events::HookResult {
        use agent_engine::extensions::hooks::events::{HookKind, HookResult};
        self.seen.lock().unwrap().push(serde_json::to_value(event).unwrap());
        match event.kind {
            HookKind::BeforeToolCall => self.before.lock().unwrap().clone().unwrap_or(HookResult::Continue),
            _ => HookResult::Continue,
        }
    }
    async fn shutdown(&self) {}
}

/// One turn of `prompt` on `root` while the installer answers the client
/// call it makes with the memory-only [`SENTINEL`]: the request bodies, and
/// the args the installer was sent.
async fn hooked_turn(
    root: &Runtime,
    bodies: &Arc<Mutex<Vec<String>>>,
    overlay: &Overlay,
    frames: &mut tokio::sync::mpsc::UnboundedReceiver<Outbound>,
    prompt: &str,
) -> (Vec<String>, Value) {
    let answer = async {
        match frames.recv().await {
            Some(Outbound::Invoke(invoke)) => {
                overlay.resolve(&invoke.invocation_id, "ok", SENTINEL.into(), 0, true).expect("accepted");
                invoke.args
            }
            other => panic!("expected an invoke, got {other:?}"),
        }
    };
    tokio::join!(turn(root, bodies, prompt), answer)
}

/// Decision 18(a): for a call of a memory-only client tool, a hook (before
/// and after, registered by the tool's name) is shown the name and the ids,
/// never the args or the result; its rewrite of the args is ignored; its
/// block still refuses the call. The same hook on an ordinary tool still
/// sees the args.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn hooks_see_only_the_name_of_a_memory_only_call() {
    use agent_engine::extensions::hooks::events::{HookKind, HookResult};
    let _home = Home::new();
    let bodies = provider().await;
    let (overlay, mut frames) = Overlay::new();
    overlay.install(Registration { require_memory_only: true, ..registration() });
    let mut root = Runtime::new().await.unwrap();
    root.set_model(MODEL.into());
    root.set_session_id(Some("conv-client-tools".into()));
    root.share_client_tools(&overlay);
    let spy = Arc::new(HookSpy::default());
    for kind in [HookKind::BeforeToolCall, HookKind::AfterToolCall] {
        for name in [WIRE, "read"] {
            let mut perms = agent_engine::extensions::permissions::PermissionSet::new();
            perms.grant(kind.required_permission());
            root.hook_bus().subscribe(kind, spy.clone(), Some(name.into()), None, perms).await.unwrap();
        }
    }

    // a: the name and the ids, no args and no result; the call completes.
    let (sent, args) = hooked_turn(&root, &bodies, &overlay, &mut frames, "call-hooked-1").await;
    assert_eq!(args, json!({"record": HOOK_ARGS}), "the installer is sent the model's args");
    assert_eq!(sent.len(), 2, "the prompt, then after the call");
    assert!(sent[1].contains(SENTINEL), "the call completed: its result reaches the live request");
    let seen = spy.take(WIRE);
    let kinds: Vec<Value> = seen.iter().map(|e| e["kind"].clone()).collect();
    assert_eq!(kinds, [json!("before_tool_call"), json!("after_tool_call")], "{seen:?}");
    for event in &seen {
        assert_eq!(event["tool_call_id"], json!("toolu_hk1"), "{event}");
        assert_eq!(event["session_id"], json!("conv-client-tools"), "{event}");
        assert!(event["tool_input"].is_null(), "no args: {event}");
        assert_eq!(event["input_withheld"], json!(true), "{event}");
    }
    assert!(seen[1]["tool_output"].is_null(), "no result: {}", seen[1]);
    assert_eq!(seen[1]["output_withheld"], json!(true), "{}", seen[1]);
    let shown = Value::Array(seen).to_string();
    assert!(!shown.contains(HOOK_ARGS) && !shown.contains(SENTINEL), "a hook was shown the args or the result: {shown}");

    // The control: the same hook on an ordinary tool sees its args.
    let sent = turn(&root, &bodies, "call-read").await;
    assert!(offered(&sent[0]).iter().any(|t| t == "read"), "precondition: read is offered");
    let seen = spy.take("read");
    assert_eq!(seen.len(), 2, "before and after: {seen:?}");
    for event in &seen {
        assert_eq!(event["tool_input"], json!({"path": READ_ARGS}), "{event}");
        assert!(event.get("input_withheld").is_none() && event.get("output_withheld").is_none(), "{event}");
    }
    assert!(seen[1]["tool_output"].is_string(), "{}", seen[1]);
    assert!(frames.try_recv().is_err(), "read sent no frame");

    // b: a hook's rewrite of the args is ignored.
    *spy.before.lock().unwrap() = Some(HookResult::Modify { input: json!({"record": "CT_HOOK_REWRITE_9e0a"}) });
    let (sent, args) = hooked_turn(&root, &bodies, &overlay, &mut frames, "call-hooked-2").await;
    assert_eq!(args, json!({"record": HOOK_ARGS}), "the hook's rewrite never reaches the installer");
    assert!(sent[1].contains(SENTINEL), "the call completed");
    assert_eq!(spy.take(WIRE).len(), 2);

    // c: a hook's block still refuses the call, before any frame.
    *spy.before.lock().unwrap() = Some(HookResult::Block { reason: "veto".into() });
    let sent = turn(&root, &bodies, "call-hooked-3").await;
    assert_eq!(sent.len(), 2, "the prompt, then after the call");
    assert!(sent[1].contains("Tool call blocked by extension: veto"), "the call is refused");
    assert!(frames.try_recv().is_err(), "no frame was sent");
    assert_eq!(overlay.pending(), 0);
    let seen = spy.take(WIRE);
    assert_eq!(seen.len(), 1, "the before hook only: {seen:?}");
    assert!(seen[0]["tool_input"].is_null(), "{}", seen[0]);
}
