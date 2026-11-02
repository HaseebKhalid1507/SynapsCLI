//! 790d's owed wire pins (pria-apps `.local/reviews/parity-wire-ruling-790d.md`),
//! driven over the real daemon socket with the Pria app's own hello lines
//! and checked against the frame and tag sets the app's vendored decoder
//! (pria-apps `crates/synaps-client`) knows:
//!
//! - R3: `Closed`/`Pinned`/`Renamed`/`RenameFailed`/`ExtensionReloaded`/
//!   `ExtensionReloadFailed` go only to the requester; an app drive and an
//!   app observer decode every frame while ANOTHER client closes, pins,
//!   renames and reloads an extension.
//! - R4: `ClientJoined` reaches the app as a top-level `ev` (its
//!   `#[serde(other)] Unknown`); `ToolQueued`/`ToolStarted` never reach a
//!   hello that did not declare `capabilities.tool_queue`, even one that
//!   accepted every `accepted_events` name. Both opt-ins stay separate.
//! - R5: the recorded app hello gets today's Welcome keys, `features` and
//!   history mode (digest because it asks; full when `history` is absent).
//! - R6: an app-created session is Exclusive by default; after a TUI
//!   takeover the app sees the owner change and its submit is refused in
//!   the form the app parses as `not_input_owner`.
//! - R7: rename/unname change only the name, never `id` or `journal_id`.
//!
//! Written at merge time (upstream d90ffd5fc × ours d79540b5b) and not
//! compiled then. Depends on two lines owned elsewhere in the merge:
//! agent-core `core/config.rs:2156` (R6: effective default
//! `ClientInput::Exclusive`) and `session/actor.rs:1839` (the refusal reason
//! must start `input owned by client #<id>`). Reconnect after `Reloading`
//! is 790d row (b), not here.

mod session_actor_common;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::control_queries::daemon_features;
use agent_engine::daemon::{Daemon, DaemonOpts};
use agent_engine::ipc::{ClientReadHalf, ClientWriteHalf};
use agent_engine::session::socket_transport::{Renamed, SocketTransport};
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, LlmEvent, StreamEvent};
use serde_json::{json, Value};
use serial_test::serial;
use session_actor_common::{host, stub, stub_seq, Home, MODEL, SSE_HI};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};

// ── what the app's decoder knows (pria-apps crates/synaps-client) ─────────────

/// The app's `DaemonFrame` (no catch-all: anything else is a decode error).
const APP_FRAME_TYPES: &[&str] = &[
    "welcome",
    "refused",
    "pong",
    "session_list",
    "attached",
    "event",
    "error",
    "bye",
    "child_list",
    "sandbox_status_result",
    "model_favorites_result",
];

/// R3: the control replies; only the requester may see them.
const REPLY_FRAME_TYPES: &[&str] =
    &["closed", "pinned", "renamed", "rename_failed", "extension_reloaded", "extension_reload_failed"];

/// Nested tag sets under `ev: stream` / `loader_progress` (no catch-all).
const APP_LLM: &[&str] = &[
    "thinking",
    "text",
    "tool_use_start",
    "tool_use_delta",
    "tool_use",
    "tool_result",
    "tool_result_delta",
    "response_start",
    "response_reset",
];
const APP_SESSION_STREAM: &[&str] = &["message_history", "usage", "done", "error", "notice"];
const APP_AGENT: &[&str] = &["subagent_start", "subagent_update", "subagent_done", "steering_delivered"];
const APP_LOADER: &[&str] = &["started", "loaded", "failed", "finished"];
const APP_OWNER_REASONS: &[&str] = &["attach", "takeover", "owner_detached", "reload"];
const APP_END_REASONS: &[&str] = &["client_quit", "host_shutdown", "evicted", "error", "idle"];

/// R5: the Welcome keys a recorded app hello gets today.
const WELCOME_REQUIRED: &[&str] = &[
    "type",
    "protocol_version",
    "daemon_version",
    "pid",
    "profile",
    "sessions",
    "progressive_tool_disclosure",
    "generation",
    "daemon_build",
    "features",
];
/// Present only when set (`accepted_events` only when the hello asked).
const WELCOME_OPTIONAL: &[&str] = &["config_path", "memory_backend"];

/// Today's `Welcome.features`, in order.
fn today_features() -> Vec<String> {
    let mut f = Vec::new();
    if agent_engine::session::children::supported() {
        f.push("children");
    }
    f.extend([
        "thinking_options",
        "model_favorites",
        "sandbox_status",
        "ephemeral_layer_v1",
        "memory_only_results_v1",
        "client_tools_v1",
    ]);
    f.into_iter().map(String::from).collect()
}

/// The app's `refusal()` (pria-apps drive/track.rs:1341-1356):
/// `Some(owner)` = the app reads it as `RejectReason::NotInputOwner`;
/// `None` = a plain `Refused` (no typed rejection).
fn app_not_input_owner(reason: &str) -> Option<Option<u64>> {
    if let Some(rest) = reason.strip_prefix("input owned by client #") {
        return Some(rest.split(|c: char| !c.is_ascii_digit()).next().and_then(|n| n.parse().ok()));
    }
    if reason.starts_with("input has no owner") {
        return Some(None);
    }
    None
}

// ── the app's hello lines ─────────────────────────────────────────────────────

fn instance(n: u32) -> String {
    format!("5f0c1d2e-0000-4000-8000-{n:012}")
}

/// The app's hello, byte for byte as it sends it today.
fn app_hello(n: u32) -> String {
    format!(
        concat!(
            "{{\"type\":\"hello\",\"protocol_version\":3,\"client\":{{\"kind\":\"rpc\",\"terminal\":null,",
            "\"instance\":\"{}\",\"history\":\"digest\",\"tail_items\":120}},\"cwd\":\"/\",",
            "\"client_version\":\"pria-synaps-client/0.1.0\",\"env\":null,\"reconnect_of\":null}}"
        ),
        instance(n)
    )
}

/// The app's hello without `history`/`tail_items` (an absent field means
/// today's default: full history).
fn app_hello_no_history(n: u32) -> String {
    let mut v: Value = serde_json::from_str(&app_hello(n)).unwrap();
    let client = v["client"].as_object_mut().unwrap();
    client.remove("history");
    client.remove("tail_items");
    v.to_string()
}

/// The app's hello plus every `accepted_events` name, still without
/// `capabilities` (R4: neither opt-in implies the other).
fn app_hello_all_events(n: u32) -> String {
    let mut v: Value = serde_json::from_str(&app_hello(n)).unwrap();
    v["accepted_events"] = json!(OPTIONAL_EVENTS);
    v.to_string()
}

fn create_line(cwd: &Path) -> Value {
    json!({
        "type": "attach",
        "attach": "create",
        "mode": "mirror",
        "config": { "cwd": cwd, "model_override": MODEL, "persist": true },
    })
}

fn existing_line(sid: &SessionId, mode: &str) -> Value {
    json!({ "type": "attach", "attach": "existing", "session_id": sid.as_str(), "mode": mode })
}

fn submit_line(sid: &SessionId, text: &str) -> Value {
    json!({ "type": "cmd", "session_id": sid.as_str(), "cmd": { "cmd": "submit", "text": text } })
}

// ── the app's decode check ────────────────────────────────────────────────────

/// Whether the app's vendored decoder takes `line`: it must be one of our
/// frames, of a type the app knows, with every nested tag in its sets.
fn app_decodes(line: &str) -> Result<(), String> {
    decode_line::<DaemonFrame>(line).map_err(|e| format!("not a DaemonFrame: {e}"))?;
    let v: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
    let ty = v["type"].as_str().ok_or_else(|| "frame without type".to_string())?;
    if !APP_FRAME_TYPES.contains(&ty) {
        return Err(format!("frame type {ty:?} is unknown to the app's DaemonFrame (790d R3)"));
    }
    match ty {
        "event" => app_event_decodes(&v["event"]),
        "attached" => {
            for env in v["replay"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
                app_event_decodes(&env["event"])?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn known(set: &[&str], tag: &Value, what: &str, e: &Value) -> Result<(), String> {
    match tag.as_str() {
        Some(t) if set.contains(&t) => Ok(()),
        _ => Err(format!("{what} {tag} is not in the app's set {set:?} (no catch-all there): {e}")),
    }
}

/// `WireSessionEvent` has `#[serde(other)] Unknown` on the app's side, so
/// only the nested enums and the typed fields it reads can fail.
fn app_event_decodes(e: &Value) -> Result<(), String> {
    let ev = e["ev"].as_str().ok_or_else(|| format!("event without ev: {e}"))?;
    match ev {
        "stream" => {
            let s = &e["event"];
            match s["kind"].as_str() {
                Some("llm") => known(APP_LLM, &s["llm"], "llm event", e),
                Some("session") => known(APP_SESSION_STREAM, &s["session"], "session stream event", e),
                Some("agent") => known(APP_AGENT, &s["agent"], "agent event", e),
                _ => Err(format!("stream kind {} is unknown to the app: {e}", s["kind"])),
            }
        }
        "loader_progress" => known(APP_LOADER, &e["event"]["loader"], "loader event", e),
        "input_owner_changed" => known(APP_OWNER_REASONS, &e["reason"], "owner-change reason", e),
        "ended" => known(APP_END_REASONS, &e["reason"], "end reason", e),
        "query_result" => match e["id"].as_u64() {
            Some(_) => Ok(()),
            None => Err(format!("query_result id is not a u64: {e}")),
        },
        "client_joined" => {
            if e["client"].as_u64().is_some() && e["kind"].is_string() {
                Ok(())
            } else {
                Err(format!("client_joined without a numeric client and a kind: {e}"))
            }
        }
        _ => Ok(()),
    }
}

fn is_ev(v: &Value, ev: &str) -> bool {
    v["type"] == "event" && v["event"]["ev"] == ev
}

/// Every session event a connection saw: live ones and attach replays.
fn session_events(seen: &[Value]) -> Vec<&Value> {
    let mut out = Vec::new();
    for v in seen {
        if v["type"] == "event" {
            out.push(&v["event"]);
        } else if v["type"] == "attached" {
            for env in v["replay"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
                out.push(&env["event"]);
            }
        }
    }
    out
}

fn llm_tag(e: &Value) -> Option<&str> {
    if e["ev"] == "stream" && e["event"]["kind"] == "llm" {
        e["event"]["llm"].as_str()
    } else {
        None
    }
}

fn sid_of(v: &Value) -> SessionId {
    SessionId(v.as_str().expect("session id").to_string())
}

// ── one raw app connection ────────────────────────────────────────────────────

struct AppConn {
    label: &'static str,
    w: ClientWriteHalf,
    lines: Lines<BufReader<ClientReadHalf>>,
    seen: Vec<Value>,
}

impl AppConn {
    /// Connect, send `hello`, read the Welcome.
    async fn open(sock: &Path, label: &'static str, hello: &str) -> (Self, Value) {
        let stream = agent_engine::ipc::connect(sock).await.expect("connect");
        let (r, w) = agent_engine::ipc::split_client(stream);
        let mut c = Self { label, w, lines: BufReader::new(r).lines(), seen: Vec::new() };
        c.send(hello).await;
        let welcome = c.until("welcome", |v| v["type"] == "welcome").await;
        (c, welcome)
    }

    async fn send(&mut self, line: &str) {
        self.w.write_all(line.as_bytes()).await.expect("write");
        self.w.write_all(b"\n").await.expect("write");
        self.w.flush().await.expect("flush");
    }

    async fn send_json(&mut self, v: Value) {
        self.send(&v.to_string()).await;
    }

    fn push(&mut self, line: &str) -> Value {
        if let Err(why) = app_decodes(line) {
            panic!("{}: the app cannot decode {line}\n  {why}", self.label);
        }
        let v: Value = serde_json::from_str(line).unwrap();
        self.seen.push(v.clone());
        v
    }

    fn types(&self) -> Vec<String> {
        self.seen
            .iter()
            .map(|v| match v["event"]["ev"].as_str() {
                Some(ev) if v["type"] == "event" => format!("event/{ev}"),
                _ => v["type"].as_str().unwrap_or("?").to_string(),
            })
            .collect()
    }

    /// Read until `pred` (30 s budget); every frame is decode-checked.
    async fn until(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            match tokio::time::timeout_at(deadline, self.lines.next_line()).await {
                Ok(Ok(Some(line))) => {
                    let v = self.push(&line);
                    if pred(&v) {
                        return v;
                    }
                }
                Ok(_) => panic!("{}: connection closed waiting for {what}; saw {:?}", self.label, self.types()),
                Err(_) => panic!("{}: no {what} in 30 s; saw {:?}", self.label, self.types()),
            }
        }
    }

    /// Read for `d`; true = the daemon closed the connection.
    async fn drain_for(&mut self, d: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + d;
        loop {
            match tokio::time::timeout_at(deadline, self.lines.next_line()).await {
                Ok(Ok(Some(line))) => {
                    self.push(&line);
                }
                Ok(_) => return true,
                Err(_) => return false,
            }
        }
    }
}

// ── daemon ────────────────────────────────────────────────────────────────────

async fn start(home: &Home, host: Arc<EngineHost>) -> Daemon {
    std::env::set_var("SYNAPS_DAEMON_PARK_GRACE_SECS", "never");
    Daemon::start(host, DaemonOpts { runtime_dir: Some(home.dir.path().join("run")), ..Default::default() })
        .await
        .expect("daemon start")
}

async fn stop(d: Daemon) {
    d.shutdown_token().cancel();
    tokio::time::timeout(Duration::from_secs(20), d.wait()).await.expect("daemon stops");
}

async fn listed(sock: &Path, sid: &SessionId) -> SessionMeta {
    SocketTransport::sessions(sock)
        .await
        .expect("sessions")
        .into_iter()
        .find(|m| &m.id == sid)
        .unwrap_or_else(|| panic!("{sid} not listed"))
}

/// The app creates a persisted session and runs one turn to idle.
async fn app_creates(sock: &Path, cwd: &Path) -> (AppConn, SessionId, u64) {
    let (mut app, _) = AppConn::open(sock, "app-drive", &app_hello(1)).await;
    app.send_json(create_line(cwd)).await;
    let attached = app.until("attached", |v| v["type"] == "attached").await;
    let sid = sid_of(&attached["meta"]["id"]);
    let app_id = attached["client"].as_u64().expect("attached.client");
    (app, sid, app_id)
}

async fn app_turn(app: &mut AppConn, sid: &SessionId, text: &str) {
    app.send_json(submit_line(sid, text)).await;
    app.until("idle", |v| is_ev(v, "idle")).await;
}

const UNNAMED: Option<Option<String>> = Some(None);

// ── R3 ────────────────────────────────────────────────────────────────────────

/// 790d R3: while another client pins, renames, reloads an extension and
/// closes the session, the app's drive and observer see no control reply
/// and decode every frame; the close reaches them as `ended{client_quit}`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn r3_control_replies_go_only_to_the_requester() {
    let home = Home::new();
    let (url, _hits) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let d = start(&home, host().await).await;
    let sock = d.paths.sock.clone();
    let cwd = home.dir.path().to_path_buf();

    let (mut app, sid, _) = app_creates(&sock, &cwd).await;
    app_turn(&mut app, &sid, "hello").await;
    let (mut obs, _) = AppConn::open(&sock, "app-observer", &app_hello(2)).await;
    obs.send_json(existing_line(&sid, "observe")).await;
    obs.until("attached", |v| v["type"] == "attached").await;

    assert!(SocketTransport::pin(&sock, &sid, true).await.expect("pin reply"), "pinned");
    let renamed = SocketTransport::rename(&sock, &sid, Some("parity-r3"), UNNAMED).await.expect("rename reply");
    assert!(renamed.is_ok(), "rename refused: {renamed:?}");
    let mut control = SocketTransport::connect(&sock, Hello::new(ClientKind::Attach)).await.expect("connect");
    let reload = control.reload_extension("parity-no-such-extension").await;
    control.bye().await;
    assert!(reload.is_ok(), "reload_extension got no typed reply: {:?}", reload.as_ref().err());
    assert!(!SocketTransport::pin(&sock, &sid, false).await.expect("unpin reply"), "unpinned");
    // Whatever the four requests caused on the session, before the close.
    app.drain_for(Duration::from_millis(300)).await;
    obs.drain_for(Duration::from_millis(300)).await;

    let text = SocketTransport::close(&sock, &sid, false, true).await.expect("close reply");
    assert!(text.contains(sid.as_str()), "{text}");

    for c in [&mut app, &mut obs] {
        let ended = c.until("ended", |v| is_ev(v, "ended")).await;
        assert_eq!(ended["event"]["reason"], "client_quit", "{}: 790d R7: ended, not crashed", c.label);
        c.drain_for(Duration::from_secs(2)).await;
    }
    for c in [&app, &obs] {
        for t in REPLY_FRAME_TYPES {
            assert!(!c.seen.iter().any(|v| v["type"] == *t), "{}: got another client's {t}: {:?}", c.label, c.types());
        }
        for e in session_events(&c.seen) {
            if e["ev"] == "query_result" {
                let id = e["id"].as_u64().unwrap();
                // The app ignores reserved ids (drive/track.rs:832); any
                // other id would be read as an answer to its own query.
                assert!(id >= RESERVED_QUERY_ID_BASE, "{}: a non-reserved query_result it never asked for: {e}", c.label);
            }
        }
    }
    stop(d).await;
}

// ── R4 ────────────────────────────────────────────────────────────────────────

/// One response asking for two calls of the same serial tool: the second
/// waits behind the first (`ToolQueued`, then `ToolStarted`).
const SSE_TWO_TOOLS: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_q1\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_q1\",\"name\":\"queue_fixture\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_q2\",\"name\":\"queue_fixture\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

static TWO_TOOLS_THEN_HI: [&str; 2] = [SSE_TWO_TOOLS, SSE_HI];

/// Default effect (`NonIdempotent`): the serial lane.
struct QueueFixtureTool;

#[async_trait::async_trait]
impl agent_engine::Tool for QueueFixtureTool {
    fn name(&self) -> &str {
        "queue_fixture"
    }
    fn description(&self) -> &str {
        "takes a moment"
    }
    fn parameters(&self) -> agent_engine::Value {
        json!({"type": "object"})
    }
    fn origin(&self) -> agent_engine::tools::ToolOrigin {
        agent_engine::tools::ToolOrigin::Builtin
    }
    async fn execute(&self, _params: agent_engine::Value, _ctx: agent_engine::ToolContext) -> agent_engine::Result<String> {
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok("done".to_string())
    }
}

/// 790d R4 end to end: a turn whose second tool is queued sends
/// `ToolQueued`/`ToolStarted` to the TUI (it declares `tool_queue`), never
/// to the app (it does not), even when the app's hello accepted every
/// optional event; the app sees `client_joined` as a top-level `ev`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn r4_tool_queue_events_only_with_the_capability() {
    let home = Home::new();
    let (url, _hits) = stub_seq(&TWO_TOOLS_THEN_HI).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    host.parts().tools.write().await.register(Arc::new(QueueFixtureTool));
    let d = start(&home, host).await;
    let sock = d.paths.sock.clone();
    let cwd = home.dir.path().to_path_buf();

    let (mut app, sid, _) = app_creates(&sock, &cwd).await;
    let (mut all, all_welcome) = AppConn::open(&sock, "app-all-events", &app_hello_all_events(2)).await;
    all.send_json(existing_line(&sid, "observe")).await;
    all.until("attached", |v| v["type"] == "attached").await;
    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Tui)).await.expect("tui connect");
    let (mut tui, _) = SocketTransport::attach(conn, Attach::Existing { session_id: sid.clone(), mode: AttachMode::Observe })
        .await
        .expect("tui attach");
    let tui_id = tui.client_id();

    let joined = app.until("client_joined", |v| is_ev(v, "client_joined") && v["event"]["client"] == tui_id.0).await;
    assert_eq!(joined["event"]["kind"], "tui", "{joined}");

    app.send_json(submit_line(&sid, "run both")).await;
    app.until("idle", |v| is_ev(v, "idle")).await;
    all.until("idle", |v| is_ev(v, "idle")).await;
    let mut tui_seen = Vec::new();
    loop {
        let env = tokio::time::timeout(Duration::from_secs(30), tui.next_event()).await.expect("timely").expect("open");
        let idle = matches!(env.event, SessionEventWire::Idle);
        tui_seen.push(env.event);
        if idle {
            break;
        }
    }

    let queued = |e: &SessionEventWire| {
        matches!(e, SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::ToolQueued { tool_id })) if tool_id == "toolu_q2")
    };
    let started = |e: &SessionEventWire| {
        matches!(e, SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::ToolStarted { tool_id })) if tool_id == "toolu_q2")
    };
    assert!(tui_seen.iter().any(queued), "the TUI declares tool_queue: {tui_seen:#?}");
    assert!(tui_seen.iter().any(started), "the TUI declares tool_queue: {tui_seen:#?}");
    for c in [&app, &all] {
        let events = session_events(&c.seen);
        let tags: Vec<&str> = events.iter().filter_map(|e| llm_tag(e)).collect();
        assert!(!tags.iter().any(|t| *t == "tool_queued" || *t == "tool_started"), "{}: {tags:?}", c.label);
        for id in ["toolu_q1", "toolu_q2"] {
            assert!(
                events.iter().any(|e| llm_tag(e) == Some("tool_result") && e["event"]["tool_id"] == id),
                "{}: no tool_result for {id}: {:?}",
                c.label,
                c.types()
            );
        }
    }
    assert!(all_welcome.get("accepted_events").is_some(), "the opt-in hello gets the echo: {all_welcome}");
    let (_plain, plain_welcome) = AppConn::open(&sock, "app-plain", &app_hello(3)).await;
    assert!(plain_welcome.get("accepted_events").is_none(), "{plain_welcome}");
    tui.detach().await;
    stop(d).await;
}

/// 790d R4/R5 without a daemon: the two opt-ins are separate, and the
/// recorded app hello declares neither.
#[test]
fn r4_r5_the_two_opt_ins_stay_apart() {
    let queued = SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::ToolQueued { tool_id: "t".into() }));
    assert!(!client_decodes(ClientCapabilities::default(), &queued));
    assert!(client_decodes(ClientCapabilities::current(), &queued));
    assert_eq!(optional_event_name(&queued), None, "not an accepted_events kind");
    let all: Vec<String> = OPTIONAL_EVENTS.iter().map(|s| s.to_string()).collect();
    assert!(AcceptedEvents::negotiate(&all).allows(&queued), "accepted_events does not gate it ...");
    assert!(!client_decodes(ClientCapabilities::default(), &queued), "... and does not imply the capability");
    let lagged = SessionEventWire::ChildSpendLagged { dropped: 0 };
    assert!(!AcceptedEvents::negotiate(&[]).allows(&lagged), "accepted_events gates it ...");
    assert!(client_decodes(ClientCapabilities::current(), &lagged), "... and the capability does not open it");
    assert!(!OPTIONAL_EVENTS.iter().any(|n| n.starts_with("tool_queue") || n.starts_with("tool_started")));
    assert!(Hello::new(ClientKind::Rpc).accepted_events.is_empty());

    assert_eq!(PROTOCOL_VERSION, 3, "the app sends protocol_version 3");
    let line = app_hello(1);
    let Ok(ClientFrame::Hello(h)) = decode_line::<ClientFrame>(&line) else {
        panic!("the recorded app hello no longer decodes as Hello: {line}");
    };
    assert_eq!(h.protocol_version, 3);
    assert_eq!(h.client.kind, ClientKind::Rpc);
    assert_eq!(h.client.history, HistoryMode::Digest);
    assert_eq!(h.client.tail_items, 120);
    assert_eq!(h.client.capabilities, ClientCapabilities::default());
    assert_eq!(h.client.name, None);
    assert!(h.accepted_events.is_empty());
    assert!(h.env.is_none() && h.reconnect_of.is_none());
}

// ── R5 ────────────────────────────────────────────────────────────────────────

/// 790d R5: the recorded app hello gets today's Welcome keys and features;
/// it asks for digest history and gets it; without `history` a hello gets
/// full history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn r5_recorded_app_hello_gets_todays_welcome_and_history() {
    let home = Home::new();
    let (url, _hits) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let d = start(&home, host().await).await;
    let sock = d.paths.sock.clone();
    let cwd = home.dir.path().to_path_buf();

    let (mut app, welcome) = AppConn::open(&sock, "app-drive", &app_hello(1)).await;
    let keys: BTreeSet<&str> = welcome.as_object().expect("welcome object").keys().map(String::as_str).collect();
    for k in WELCOME_REQUIRED {
        assert!(keys.contains(k), "Welcome lacks {k}: {welcome}");
    }
    for k in &keys {
        assert!(WELCOME_REQUIRED.contains(k) || WELCOME_OPTIONAL.contains(k), "Welcome key {k} is new to the recorded app hello: {welcome}");
    }
    assert!(!keys.contains("accepted_events"), "it asked for none");
    assert_eq!(welcome["protocol_version"], PROTOCOL_VERSION);
    assert!(welcome["generation"].as_u64().is_some_and(|g| g >= 1), "{welcome}");
    let features: Vec<String> = serde_json::from_value(welcome["features"].clone()).expect("features");
    assert_eq!(features, daemon_features());
    assert_eq!(features, today_features());

    app.send_json(create_line(&cwd)).await;
    let attached = app.until("attached", |v| v["type"] == "attached").await;
    let sid = sid_of(&attached["meta"]["id"]);
    app_turn(&mut app, &sid, "hello").await;
    assert!(
        !session_events(&app.seen).iter().any(|e| e["ev"] == "stream" && e["event"]["session"] == "message_history"),
        "a digest client never gets message_history: {:?}",
        app.types()
    );

    let (mut digest, _) = AppConn::open(&sock, "app-observer", &app_hello(2)).await;
    digest.send_json(existing_line(&sid, "observe")).await;
    let snap = digest.until("attached", |v| v["type"] == "attached").await;
    assert_eq!(snap["conversation"]["api_messages"], json!([]), "digest: no api_messages");
    assert!(snap["conversation"]["messages_len"].as_u64().is_some_and(|n| n > 0), "{}", snap["conversation"]);
    assert!(!snap["display_tail"].is_null(), "digest: a display tail");

    let (mut full, _) = AppConn::open(&sock, "app-no-history", &app_hello_no_history(3)).await;
    full.send_json(existing_line(&sid, "observe")).await;
    let snap = full.until("attached", |v| v["type"] == "attached").await;
    assert!(snap["conversation"]["api_messages"].as_array().is_some_and(|m| !m.is_empty()), "absent history = full");
    assert!(snap["display_tail"].is_null(), "full history: no display tail");
    stop(d).await;
}

// ── R6 ────────────────────────────────────────────────────────────────────────

/// 790d R6: a session the app creates (no `client_input`) is Exclusive;
/// a TUI takes it over; the app sees the owner change, and its next submit
/// is refused in the form the app reads as `not_input_owner` and never
/// runs. The new owner's submit runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn r6_exclusive_default_and_takeover_refuses_the_app() {
    let home = Home::new();
    let (url, hits) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let d = start(&home, host().await).await;
    let sock = d.paths.sock.clone();
    let cwd = home.dir.path().to_path_buf();

    let (mut app, _) = AppConn::open(&sock, "app-drive", &app_hello(1)).await;
    app.send_json(create_line(&cwd)).await;
    let attached = app.until("attached", |v| v["type"] == "attached").await;
    let sid = sid_of(&attached["meta"]["id"]);
    let app_id = attached["client"].as_u64().expect("attached.client");
    assert_eq!(
        attached["client_input"], "exclusive",
        "790d R6: the effective default is Exclusive (agent-core config.rs:2156)"
    );
    assert_eq!(attached["input_owner"], app_id, "the creator owns input");

    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Tui)).await.expect("tui connect");
    let (mut tui, snap) = SocketTransport::attach(conn, Attach::Existing { session_id: sid.clone(), mode: AttachMode::Takeover })
        .await
        .expect("takeover");
    let tui_id = tui.client_id();
    assert_eq!(snap.input_owner, Some(tui_id));
    assert_eq!(snap.client_input, ClientInput::Exclusive);

    let changed = app.until("input_owner_changed", |v| is_ev(v, "input_owner_changed")).await;
    let e = &changed["event"];
    assert_eq!(e["from"], app_id, "{changed}");
    assert_eq!(e["to"], tui_id.0, "{changed}");
    assert_eq!(e["reason"], "takeover", "{changed}");

    let m = listed(&sock, &sid).await;
    assert_eq!(m.input_owner, Some(tui_id));
    assert_eq!(m.input_owner_kind, Some(ClientKind::Tui));
    assert!(matches!(m.client_input, None | Some(ClientInput::Exclusive)), "{:?}", m.client_input);

    let before = hits.load(Ordering::SeqCst);
    app.send_json(submit_line(&sid, "from the app")).await;
    let refused = app.until("refused", |v| is_ev(v, "refused")).await;
    let e = &refused["event"];
    assert_eq!(e["client"], app_id, "{refused}");
    assert_eq!(e["command"], "submit", "{refused}");
    let reason = e["reason"].as_str().expect("reason");
    assert_eq!(
        app_not_input_owner(reason),
        Some(Some(tui_id.0)),
        "790d R6: the app reads only `input owned by client #<id>` / `input has no owner` as not_input_owner \
         (pria-apps drive/track.rs:1341-1356); see the REQUEST for session/actor.rs:1839. Got {reason:?}"
    );
    app.drain_for(Duration::from_millis(500)).await;
    assert!(!session_events(&app.seen).iter().any(|e| e["ev"] == "turn_started"), "the refused submit never ran");
    assert_eq!(hits.load(Ordering::SeqCst), before, "no provider request for the refused submit");

    tui.send_from_self(SessionCommand::Submit { text: "from the tui".into(), attachments: vec![] }).await.expect("send");
    loop {
        let env = tokio::time::timeout(Duration::from_secs(30), tui.next_event()).await.expect("timely").expect("open");
        if matches!(env.event, SessionEventWire::Idle) {
            break;
        }
    }
    assert!(hits.load(Ordering::SeqCst) > before, "the owner's submit ran");
    tui.detach().await;
    stop(d).await;
}

// ── R7 ────────────────────────────────────────────────────────────────────────

/// 790d R7: rename and unname change only the name; `id` and `journal_id`
/// stay, in the listing and on every envelope, across a later turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn r7_rename_keeps_id_and_journal_id() {
    let home = Home::new();
    let (url, _hits) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let d = start(&home, host().await).await;
    let sock = d.paths.sock.clone();
    let cwd = home.dir.path().to_path_buf();

    let (mut app, sid, _) = app_creates(&sock, &cwd).await;
    app_turn(&mut app, &sid, "hello").await;
    let (mut obs, _) = AppConn::open(&sock, "app-observer", &app_hello(2)).await;
    obs.send_json(existing_line(&sid, "observe")).await;
    obs.until("attached", |v| v["type"] == "attached").await;

    let before = listed(&sock, &sid).await;
    let journal = before.journal_id.clone();
    assert!(!journal.is_empty(), "a persisted session has a journal id");

    let r = SocketTransport::rename(&sock, &sid, Some("parity-r7"), UNNAMED).await.expect("rename reply");
    assert_eq!(r, Ok(Renamed { name: Some("parity-r7".into()), in_memory_only: false }));
    let conv = obs
        .until("conversation with the new name", |v| is_ev(v, "conversation") && v["event"]["digest"]["header"]["name"] == "parity-r7")
        .await;
    assert_eq!(conv["session_id"], sid.as_str(), "{conv}");
    let m = listed(&sock, &sid).await;
    assert_eq!((m.id.clone(), m.journal_id.clone(), m.name.as_deref()), (sid.clone(), journal.clone(), Some("parity-r7")));

    app_turn(&mut app, &sid, "again").await;
    let m = listed(&sock, &sid).await;
    assert_eq!((m.id.clone(), m.journal_id.clone()), (sid.clone(), journal.clone()), "a turn after the rename");

    let r = SocketTransport::rename(&sock, &sid, None, Some(Some("parity-r7".into()))).await.expect("unname reply");
    assert_eq!(r, Ok(Renamed { name: None, in_memory_only: false }));
    let m = listed(&sock, &sid).await;
    assert_eq!((m.id.clone(), m.journal_id.clone(), m.name.clone()), (sid.clone(), journal.clone(), None));

    obs.drain_for(Duration::from_millis(300)).await;
    for c in [&app, &obs] {
        for v in c.seen.iter().filter(|v| v["type"] == "event") {
            assert_eq!(v["session_id"], sid.as_str(), "{}: an envelope under another id: {v}", c.label);
        }
    }
    stop(d).await;
}
