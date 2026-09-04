//! §5.1 — byte-identical proof for `LocalTransport`: the same scripted
//! session runs through the FROZEN reference reactor (`support/reference_reactor.rs`,
//! today's inline engine halves) and through `EngineHost::create_session` +
//! `LocalTransport`; the `StreamEvent` sequences, the final `api_messages`
//! and the auto-turn counters must be identical.
//!
//! Phase 3 (C1): `support/reference_reactor_ext.rs` (also frozen, own sha)
//! adds Abort, the secret-prompt handle, save counting and usage — read
//! its header for the list of what the oracle STILL cannot assert. The
//! scenario table:
//!
//! | scenario                          | asserted                                                                 | not asserted                                   |
//! |-----------------------------------|--------------------------------------------------------------------------|------------------------------------------------|
//! | plain_turn (frozen)               | StreamEvent seq, api_messages, auto-turn counters                        | saves                                          |
//! | error_repairs_history (frozen)    | same + repair                                                            | saves                                          |
//! | event_injection_idle_… (frozen)   | same + cap notices, real UDS                                             | saves                                          |
//! | tool_loop                         | seq incl. ToolUse/ToolResult/MessageHistory, api_messages, cost/tokens,  |                                                |
//! |                                   | save count (oracle logical == sampled(oracle) == sampled(actor)), file   |                                                |
//! |                                   | fields (api_messages/abort_context/tokens/cost)                          |                                                |
//! | steer_mid_stream                  | Steer lands mid-stream (paced SSE) → SteeringDelivered position, 2nd     | delivered=false / auto-send (unreachable       |
//! |                                   | round, api_messages, saves                                               | deterministically — ext header #2)             |
//! | event_injection_busy_steered      | inject via real UDS mid-stream → Steered disposition, api_messages, saves| Buffered disposition (ext header #3)           |
//! | cancel_captures_abort_context     | abort_context text equal, save at abort, Dequeued of a queued steer,     | partial ToolResultDelta fold (ext header #1)   |
//! |                                   | next Submit's request body (fold) equal, file abort_context equal        |                                                |
//! | secret_prompt_roundtrip           | Some(handle) on both sides; tool result equal; actor replay to a 2nd     |                                                |
//! |                                   | client carries no Answer/value; saves                                    |                                                |
//!
//! `queue_while_busy_then_autosend` from PLAN §5.2 is NOT here: see ext
//! header #2 — the `delivered == false` branch cannot be reached
//! deterministically on either side.

#[path = "support/phase2/mod.rs"]
mod support;
#[path = "support/reference_reactor.rs"]
mod reference_reactor;
#[path = "support/reference_reactor_ext.rs"]
mod reference_reactor_ext;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, EndReason, LocalTransport, SessionCommand,
    SessionConfig, SessionEventWire, SessionHandle,
};
use agent_engine::{EngineHost, HostOpts};
use reference_reactor::ReferenceReactor;
use reference_reactor_ext::ReferenceReactorExt;
use serial_test::serial;
use support::*;
use synaps_cli::{LlmEvent, SessionEvent, StreamEvent};

const MODEL: &str = "claude-sonnet-4-5";

/// `// FROZEN` (C1): the extension oracle is pinned like the base file.
#[test]
fn reference_reactor_ext_is_frozen() {
    use sha2::{Digest, Sha256};
    let src = include_str!("support/reference_reactor_ext.rs");
    let hex = format!("{:x}", Sha256::digest(src.as_bytes()));
    assert_eq!(
        hex, "99543baeaf5c22e576647c26d6fc3a76b48825335c3c41547a68c8e0d7014a2d",
        "tests/support/reference_reactor_ext.rs is a frozen oracle — do not edit"
    );
}

/// `// FROZEN`: the oracle never changes after A5.
#[test]
fn reference_reactor_is_frozen() {
    use sha2::{Digest, Sha256};
    let src = include_str!("support/reference_reactor.rs");
    let hex = format!("{:x}", Sha256::digest(src.as_bytes()));
    assert_eq!(
        hex, "080bbdd63a5b141cb0cc2ab8d360aec8276c75956ef5cd364548d017ce162c87",
        "tests/support/reference_reactor.rs is a frozen oracle — do not edit"
    );
}

async fn host() -> Arc<EngineHost> {
    EngineHost::boot(HostOpts {
        profile: None,
        no_extensions: true,
    })
    .await
    .expect("host boot")
}

async fn oracle(host: &Arc<EngineHost>) -> ReferenceReactor {
    let mut rt = host.foreground_runtime().await.unwrap();
    rt.set_model(MODEL.to_string());
    ReferenceReactor::new(rt)
}

struct ActorRun {
    t: LocalTransport,
    handle: SessionHandle,
    seen: Vec<String>,
    api_messages: Vec<synaps_cli::SharedMessage>,
    consecutive_auto_turns: u32,
    conversations_after_terminal: usize,
    cap_notices: u32,
    abort_context: Option<String>,
    cost: f64,
    tokens: (u64, u64),
    steered: Vec<bool>,
    dequeued: Vec<String>,
}

async fn actor(host: &Arc<EngineHost>) -> ActorRun {
    actor_with(host, false).await
}

async fn actor_with(host: &Arc<EngineHost>, persist: bool) -> ActorRun {
    let handle = host
        .create_session(SessionConfig {
            model_override: Some(MODEL.into()),
            persist,
            ..SessionConfig::default()
        })
        .await
        .expect("create_session");
    let (t, _snap) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    ActorRun {
        t,
        handle,
        seen: Vec::new(),
        api_messages: Vec::new(),
        consecutive_auto_turns: 0,
        conversations_after_terminal: 0,
        cap_notices: 0,
        abort_context: None,
        cost: 0.0,
        tokens: (0, 0),
        steered: Vec::new(),
        dequeued: Vec::new(),
    }
}

impl ActorRun {
    /// Pump until `Idle` (the actor's own "turn machine parked" signal).
    async fn drive_to_idle(&mut self) {
        let mut expect_conv = false;
        loop {
            let env = tokio::time::timeout(std::time::Duration::from_secs(30), self.t.next_event())
                .await
                .expect("actor hung")
                .expect("actor alive");
            match env.event {
                SessionEventWire::Stream(ev) => {
                    self.seen.push(format!("{:?}", ev));
                    if matches!(
                        ev,
                        StreamEvent::Session(SessionEvent::Done)
                            | StreamEvent::Session(SessionEvent::Error(_))
                            | StreamEvent::Session(SessionEvent::MessageHistory(_))
                    ) {
                        expect_conv = true;
                    }
                }
                SessionEventWire::Conversation(c) => {
                    if expect_conv {
                        self.conversations_after_terminal += 1;
                        expect_conv = false;
                    }
                    self.absorb(c);
                }
                SessionEventWire::AutoTurnCapReached { .. } => self.cap_notices += 1,
                SessionEventWire::Steered { delivered, .. } => self.steered.push(delivered),
                SessionEventWire::Dequeued { text } => self.dequeued.push(text),
                SessionEventWire::Idle => break,
                SessionEventWire::Ended { .. } => panic!("ended early"),
                _ => {}
            }
        }
    }

    fn absorb(&mut self, c: agent_engine::session::ConversationSnapshot) {
        self.api_messages = c.api_messages;
        self.consecutive_auto_turns = c.consecutive_auto_turns;
        self.abort_context = c.abort_context;
        self.cost = c.cost;
        self.tokens = (c.tokens.input, c.tokens.output);
    }

    /// Pump (recording like `drive_to_idle`) until `pred` matches an
    /// envelope; returns the matching event.
    async fn until(
        &mut self,
        pred: impl Fn(&SessionEventWire) -> bool,
    ) -> SessionEventWire {
        loop {
            let env = tokio::time::timeout(Duration::from_secs(30), self.t.next_event())
                .await
                .expect("actor hung")
                .expect("actor alive");
            let hit = pred(&env.event);
            match &env.event {
                SessionEventWire::Stream(ev) => self.seen.push(format!("{:?}", ev)),
                SessionEventWire::Conversation(c) => self.absorb(c.clone()),
                SessionEventWire::Steered { delivered, .. } => self.steered.push(*delivered),
                SessionEventWire::Dequeued { text } => self.dequeued.push(text.clone()),
                SessionEventWire::AutoTurnCapReached { .. } => self.cap_notices += 1,
                SessionEventWire::Ended { .. } => panic!("ended early"),
                _ => {}
            }
            if hit {
                return env.event;
            }
        }
    }

    async fn submit(&mut self, text: &str) {
        self.t
            .send(SessionCommand::Submit {
                text: text.into(),
                attachments: vec![],
            })
            .await
            .unwrap();
    }

    fn journal_path(&self, guard: &HomeGuard) -> std::path::PathBuf {
        guard
            .base_dir()
            .join("sessions")
            .join(format!("{}.json", self.handle.journal_id()))
    }

    async fn end(mut self) {
        self.t
            .send(SessionCommand::End {
                reason: EndReason::ClientQuit,
            })
            .await
            .unwrap();
        while let Some(env) = self.t.next_event().await {
            if matches!(env.event, SessionEventWire::Ended { .. }) {
                break;
            }
        }
    }
}

fn msgs_json(m: &[synaps_cli::SharedMessage]) -> String {
    serde_json::to_string(m).unwrap()
}

/// `TurnError.correlation_id` comes from a process-global counter
/// (`next_turn_correlation_id`) — the only legitimately run-dependent bytes.
fn normalise(seen: &[String]) -> Vec<String> {
    let re = regex::Regex::new(r#"correlation_id: "turn-[0-9]+-[0-9]+""#).unwrap();
    seen.iter()
        .map(|s| re.replace_all(s, r#"correlation_id: "turn-N-N""#).into_owned())
        .collect()
}

fn assert_same(o: &ReferenceReactor, a: &ActorRun) {
    assert_eq!(
        normalise(&o.seen),
        normalise(&a.seen),
        "StreamEvent sequence differs"
    );
    assert_eq!(
        msgs_json(&o.api_messages),
        msgs_json(&a.api_messages),
        "final api_messages differ"
    );
    assert_eq!(o.consecutive_auto_turns, a.consecutive_auto_turns);
    assert_eq!(o.cap_notices, a.cap_notices);
}

/// Deterministic event: id + timestamp fixed so the formatted injection is
/// identical through both paths.
fn fixed_event(i: usize) -> synaps_cli::events::types::Event {
    let mut ev =
        synaps_cli::events::types::Event::simple("differential", &format!("event {i}"), None);
    ev.id = format!("evt-{i}");
    ev.timestamp = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    ev
}

/// Inject an event through the session's real UDS (the `synaps send` path).
async fn inject(session_id: &str, ev: &synaps_cli::events::types::Event) {
    use tokio::io::AsyncWriteExt;
    let path = synaps_cli::events::registry::socket_path_for_session(session_id);
    let mut s = tokio::net::UnixStream::connect(&path).await.expect("session socket");
    s.write_all(serde_json::to_string(ev).unwrap().as_bytes()).await.unwrap();
    s.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn plain_turn() {
    let _guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;

    let mut o = oracle(&host).await;
    o.submit("hello".into()).await;
    o.drive_to_idle().await;

    let mut a = actor(&host).await;
    a.t.send(SessionCommand::Submit {
        text: "hello".into(),
        attachments: vec![],
    })
    .await
    .unwrap();
    a.drive_to_idle().await;

    assert!(o.seen.iter().any(|s| s.contains("Done")));
    assert_same(&o, &a);
    assert!(a.conversations_after_terminal >= 1, "mirror invariant");
    a.end().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn error_repairs_history() {
    let _guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::AlwaysFailEcho(500)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;

    let mut o = oracle(&host).await;
    o.submit("fail me".into()).await;
    o.drive_to_idle().await;

    let mut a = actor(&host).await;
    a.t.send(SessionCommand::Submit {
        text: "fail me".into(),
        attachments: vec![],
    })
    .await
    .unwrap();
    a.drive_to_idle().await;

    assert!(o.seen.iter().any(|s| s.contains("Error")));
    assert_same(&o, &a);
    // The prompt that started the turn survives repair in both.
    assert_eq!(a.api_messages.len(), 1);
    a.end().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn event_injection_idle_autoturn_until_cap() {
    let _guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;

    // Oracle: push into the queue directly, wake, drive — 6×; the 6th parks.
    let mut o = oracle(&host).await;
    for i in 0..6 {
        o.runtime.event_queue().push(fixed_event(i)).unwrap();
        o.on_queue_wake().await;
        o.drive_to_idle().await;
    }

    // Actor: same events via its real per-session socket.
    let mut a = actor(&host).await;
    let sid = a.t.session_id().as_str().to_string();
    for i in 0..6 {
        inject(&sid, &fixed_event(i)).await;
        // Wait for the External card, then either a turn or the cap notice.
        let mut saw_turn = false;
        loop {
            let env = tokio::time::timeout(std::time::Duration::from_secs(30), a.t.next_event())
                .await
                .expect("actor hung")
                .expect("alive");
            match env.event {
                SessionEventWire::TurnStarted { .. } => {
                    saw_turn = true;
                    break;
                }
                SessionEventWire::AutoTurnCapReached { .. } => {
                    a.cap_notices += 1;
                    break;
                }
                SessionEventWire::Conversation(c) => {
                    a.api_messages = c.api_messages;
                    a.consecutive_auto_turns = c.consecutive_auto_turns;
                }
                _ => {}
            }
        }
        if saw_turn {
            a.drive_to_idle().await;
        }
    }

    assert_eq!(o.consecutive_auto_turns, 5);
    assert_eq!(o.cap_notices, 1);
    assert_same(&o, &a);
    a.end().await;
}

// ═══ C1: ext-oracle scenarios ════════════════════════════════════════════════

async fn oracle_ext(host: &Arc<EngineHost>) -> ReferenceReactorExt {
    let mut rt = host.foreground_runtime().await.unwrap();
    rt.set_model(MODEL.to_string());
    let session = synaps_cli::Session::new(rt.model(), rt.thinking_level(), rt.system_prompt());
    ReferenceReactorExt::new(ReferenceReactor::new(rt), session)
}

fn oracle_journal_path(o: &ReferenceReactorExt, guard: &HomeGuard) -> std::path::PathBuf {
    guard
        .base_dir()
        .join("sessions")
        .join(format!("{}.json", o.session.id))
}

/// Save counter from OUTSIDE the process under test: `Session::save` goes
/// through `write_atomic_private` (tmp + rename), so every save gives
/// `sessions/<id>.json` a fresh inode. A spinning thread samples the inode
/// far faster than a save can complete (JSON serialise + spawn_blocking
/// round-trip); the oracle's logical counter is compared against this
/// sampler on the oracle's own file in every scenario, so a sampler miss
/// shows up as an oracle self-mismatch, not as a false actor pass.
struct SaveSampler {
    stop: Arc<AtomicBool>,
    inodes: Arc<Mutex<Vec<u64>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SaveSampler {
    fn watch(path: std::path::PathBuf) -> Self {
        use std::os::unix::fs::MetadataExt;
        let stop = Arc::new(AtomicBool::new(false));
        let inodes: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let (s, i) = (Arc::clone(&stop), Arc::clone(&inodes));
        let thread = std::thread::spawn(move || {
            let mut last = 0u64;
            while !s.load(Ordering::Relaxed) {
                if let Ok(m) = std::fs::metadata(&path) {
                    let ino = m.ino();
                    if ino != last {
                        last = ino;
                        i.lock().unwrap().push(ino);
                    }
                }
                std::thread::yield_now();
            }
        });
        Self {
            stop,
            inodes,
            thread: Some(thread),
        }
    }

    fn count(&self) -> usize {
        self.inodes.lock().unwrap().len()
    }
}

impl Drop for SaveSampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The journal fields both sides write (id/title/timestamps/model differ by
/// construction — the actor's `Session` is created before `model_override`
/// is applied, the oracle's after — and are excluded).
fn journal_fields(path: &std::path::Path) -> serde_json::Value {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("journal exists")).unwrap();
    serde_json::json!({
        "api_messages": v["api_messages"],
        "abort_context": v["abort_context"],
        "total_input_tokens": v["total_input_tokens"],
        "total_output_tokens": v["total_output_tokens"],
        "session_cost": v["session_cost"],
        "message_count": v["message_count"],
    })
}

fn assert_same_ext(o: &ReferenceReactorExt, a: &ActorRun) {
    assert_same(&o.r, a);
    assert_eq!(o.r.abort_context, a.abort_context, "abort_context differs");
    assert_eq!(o.session_cost, a.cost, "session cost differs");
    assert_eq!(
        (o.total_input_tokens, o.total_output_tokens),
        a.tokens,
        "token totals differ"
    );
}

fn assert_saves(
    o: &ReferenceReactorExt,
    o_s: &SaveSampler,
    a_s: &SaveSampler,
    o_path: &std::path::Path,
    a_path: &std::path::Path,
) {
    // Let the last rename land before reading.
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        o.saves,
        o_s.count(),
        "sampler self-check: oracle logical saves != sampled saves"
    );
    assert_eq!(o_s.count(), a_s.count(), "save count differs (oracle vs actor)");
    if o.saves > 0 {
        assert_eq!(
            journal_fields(o_path),
            journal_fields(a_path),
            "journal fields differ"
        );
    }
}

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

/// Deterministic builtin tool: constant result, no environment reads.
struct EchoFixtureTool;

#[async_trait::async_trait]
impl agent_engine::Tool for EchoFixtureTool {
    fn name(&self) -> &str {
        "echo_fixture"
    }
    fn description(&self) -> &str {
        "constant"
    }
    fn parameters(&self) -> agent_engine::Value {
        serde_json::json!({"type": "object"})
    }
    fn origin(&self) -> agent_engine::tools::ToolOrigin {
        agent_engine::tools::ToolOrigin::Builtin
    }
    async fn execute(
        &self,
        _params: agent_engine::Value,
        _ctx: agent_engine::ToolContext,
    ) -> agent_engine::Result<String> {
        Ok("fixture-ok".to_string())
    }
}

/// Prompts through the stream's `SecretPromptHandle`; reports the length
/// only (mirrors `session_actor.rs::PromptFixtureTool`).
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
    async fn execute(
        &self,
        _params: agent_engine::Value,
        ctx: agent_engine::ToolContext,
    ) -> agent_engine::Result<String> {
        let handle = ctx
            .capabilities
            .secret_prompt
            .expect("both sides pass Some(handle) to the stream");
        Ok(match handle.prompt("Secret".into(), "enter secret".into()).await {
            Some(v) => format!("answered:{}", v.len()),
            None => "cancelled".to_string(),
        })
    }
}

async fn host_with_tools() -> Arc<EngineHost> {
    let host = host().await;
    {
        let mut tools = host.parts().tools.write().await;
        tools.register(Arc::new(EchoFixtureTool));
        tools.register(Arc::new(PromptFixtureTool));
    }
    host
}

/// Oracle-side `until(is_text)`: drive (with full bookkeeping) until the
/// first `Text` delta.
async fn pump_until_text(o: &mut ReferenceReactorExt) {
    o.drive_until(None, |ev| matches!(ev, StreamEvent::Llm(LlmEvent::Text(_))))
        .await;
    assert!(o.r.streaming, "stream still live after the first Text");
}

fn is_text(ev: &SessionEventWire) -> bool {
    matches!(ev, SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::Text(_))))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn tool_loop() {
    let guard = HomeGuard::new();
    let tool = sse_tool_call("echo_fixture", "toolu_diff1");
    let bodies: &'static [&'static str] =
        Box::leak(Box::new([tool, ANTHROPIC_SSE, tool, ANTHROPIC_SSE]));
    let (url, _hits, _) = spawn_stub(Script::SeqSse(bodies)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host_with_tools().await;

    let mut o = oracle_ext(&host).await;
    let o_path = oracle_journal_path(&o, &guard);
    let o_s = SaveSampler::watch(o_path.clone());
    o.submit("call the tool".into()).await;
    o.drive_to_idle(None).await;

    let mut a = actor_with(&host, true).await;
    let a_path = a.journal_path(&guard);
    let a_s = SaveSampler::watch(a_path.clone());
    a.submit("call the tool").await;
    a.drive_to_idle().await;

    let histories = o.r.seen.iter().filter(|s| s.contains("MessageHistory")).count();
    assert!(histories >= 1, "tool loop must publish MessageHistory");
    assert!(o.r.seen.iter().any(|s| s.contains("ToolResult")));
    assert_same_ext(&o, &a);
    assert!(
        a.api_messages.iter().any(|m| m.to_string().contains("fixture-ok")),
        "tool result reached the history"
    );
    assert_saves(&o, &o_s, &a_s, &o_path, &a_path);
    assert_eq!(o.saves, histories, "one save per MessageHistory (stream_handler.rs:77)");
    a.end().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn steer_mid_stream() {
    let guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::Paced {
        body: ANTHROPIC_SSE,
        frame_delay: Duration::from_millis(100),
    })
    .await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host_with_tools().await;

    // Oracle: submit, pump until the first Text, steer, drive to idle.
    let mut o = oracle_ext(&host).await;
    let o_path = oracle_journal_path(&o, &guard);
    let o_s = SaveSampler::watch(o_path.clone());
    o.submit("start".into()).await;
    pump_until_text(&mut o).await;
    assert!(o.steer("redirect".into()), "steer delivered while streaming");
    o.drive_to_idle(None).await;
    let oracle_hits = hits.load(Ordering::SeqCst);
    assert_eq!(oracle_hits, 2, "steer forces a second provider round");

    let mut a = actor_with(&host, true).await;
    let a_path = a.journal_path(&guard);
    let a_s = SaveSampler::watch(a_path.clone());
    a.submit("start").await;
    a.until(is_text).await;
    a.t.send(SessionCommand::Steer {
        text: "redirect".into(),
    })
    .await
    .unwrap();
    a.drive_to_idle().await;
    assert_eq!(hits.load(Ordering::SeqCst), 4);
    assert_eq!(a.steered, vec![true]);

    let delivered_at = |seen: &[String]| seen.iter().position(|s| s.contains("SteeringDelivered"));
    assert!(delivered_at(&o.r.seen).is_some());
    assert_eq!(delivered_at(&o.r.seen), delivered_at(&a.seen));
    assert_same_ext(&o, &a);
    assert!(a.api_messages.iter().any(|m| m["content"] == "redirect"));
    assert_saves(&o, &o_s, &a_s, &o_path, &a_path);
    a.end().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn event_injection_busy_steered() {
    let guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::Paced {
        body: ANTHROPIC_SSE,
        frame_delay: Duration::from_millis(100),
    })
    .await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host_with_tools().await;

    let mut o = oracle_ext(&host).await;
    let o_path = oracle_journal_path(&o, &guard);
    let o_s = SaveSampler::watch(o_path.clone());
    o.submit("start".into()).await;
    pump_until_text(&mut o).await;
    o.r.runtime.event_queue().push(fixed_event(0)).unwrap();
    o.on_queue_wake().await;
    assert!(o.r.pending_events.is_empty(), "live steer channel ⇒ Steered, not Buffered");
    o.drive_to_idle(None).await;
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    let mut a = actor_with(&host, true).await;
    let a_path = a.journal_path(&guard);
    let a_s = SaveSampler::watch(a_path.clone());
    let sid = a.t.session_id().as_str().to_string();
    a.submit("start").await;
    a.until(is_text).await;
    inject(&sid, &fixed_event(0)).await;
    a.until(|e| matches!(e, SessionEventWire::External(_))).await;
    a.drive_to_idle().await;
    assert_eq!(hits.load(Ordering::SeqCst), 4);

    assert_same_ext(&o, &a);
    assert!(
        a.api_messages
            .iter()
            .any(|m| m["content"].as_str().is_some_and(|c| c.contains("event 0")))
    );
    assert_saves(&o, &o_s, &a_s, &o_path, &a_path);
    a.end().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn cancel_captures_abort_context() {
    let guard = HomeGuard::new();
    let (url, _hits, bodies) = spawn_stub(Script::Endless(ANTHROPIC_SSE_PREFIX)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host_with_tools().await;

    // Oracle: submit, see "partial", queue a steer, abort, then submit again
    // (fold) and abort that too (the stub never finishes a turn).
    let mut o = oracle_ext(&host).await;
    let o_path = oracle_journal_path(&o, &guard);
    let o_s = SaveSampler::watch(o_path.clone());
    o.submit("first".into()).await;
    pump_until_text(&mut o).await;
    o.steer("queued-then-dropped".into());
    let dequeued = o.abort().await;
    assert_eq!(dequeued.as_deref(), Some("queued-then-dropped"));
    let o_ctx = o.r.abort_context.clone().expect("oracle captured context");
    assert!(o_ctx.contains("[response]: partial"), "{o_ctx}");
    o.submit("second".into()).await;
    pump_until_text(&mut o).await;
    o.abort().await;

    let mut a = actor_with(&host, true).await;
    let a_path = a.journal_path(&guard);
    let a_s = SaveSampler::watch(a_path.clone());
    a.submit("first").await;
    a.until(is_text).await;
    a.t.send(SessionCommand::Steer {
        text: "queued-then-dropped".into(),
    })
    .await
    .unwrap();
    a.t.send(SessionCommand::Cancel).await.unwrap();
    a.until(|e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(a.dequeued, vec!["queued-then-dropped".to_string()]);
    assert_eq!(a.abort_context.as_deref(), Some(o_ctx.as_str()));
    a.submit("second").await;
    a.until(is_text).await;
    a.t.send(SessionCommand::Cancel).await.unwrap();
    a.until(|e| matches!(e, SessionEventWire::Idle)).await;

    // Both saw exactly the prefix events per turn (Endless never terminates).
    assert_eq!(normalise(&o.r.seen), normalise(&a.seen));
    assert_eq!(o.r.abort_context, a.abort_context, "second abort context");
    // The fold: the second request's messages (captured at the stub) are
    // identical — oracle = hit 1, actor = hit 3.
    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 4);
    let msgs = |i: usize| -> serde_json::Value {
        serde_json::from_slice::<serde_json::Value>(&bodies[i]).unwrap()["messages"].clone()
    };
    assert_eq!(msgs(1), msgs(3), "abort-context fold differs");
    assert!(msgs(1)[0].to_string().contains("[ABORT CONTEXT"), "{}", msgs(1)[0]);
    assert_eq!(o.r.api_messages.len(), a.api_messages.len());
    assert_eq!(msgs_json(&o.r.api_messages), msgs_json(&a.api_messages));
    assert_saves(&o, &o_s, &a_s, &o_path, &a_path);
    assert_eq!(o.saves, 2, "one save per abort (dispatch.rs:191)");
    a.end().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn secret_prompt_roundtrip() {
    let guard = HomeGuard::new();
    let tool = sse_tool_call("prompt_fixture", "toolu_diffp");
    let bodies: &'static [&'static str] =
        Box::leak(Box::new([tool, ANTHROPIC_SSE, tool, ANTHROPIC_SSE]));
    let (url, _hits, _) = spawn_stub(Script::SeqSse(bodies)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host_with_tools().await;

    let mut o = oracle_ext(&host).await;
    let o_path = oracle_journal_path(&o, &guard);
    let o_s = SaveSampler::watch(o_path.clone());
    o.submit("go".into()).await;
    o.drive_to_idle(Some("s3cret".into())).await;

    let mut a = actor_with(&host, true).await;
    let a_path = a.journal_path(&guard);
    let a_s = SaveSampler::watch(a_path.clone());
    a.submit("go").await;
    let prompt = a.until(|e| matches!(e, SessionEventWire::Prompt(_))).await;
    let SessionEventWire::Prompt(p) = prompt else {
        unreachable!()
    };
    // A second client attaching mid-prompt sees the prompt in
    // `pending_prompts`, never in the replay, and never the answer.
    let (mut b, snap) = LocalTransport::attach(a.handle.clone(), ClientMeta::new(ClientKind::Attach))
        .await
        .unwrap();
    assert_eq!(snap.pending_prompts.len(), 1);
    assert!(!snap.replay.iter().any(|e| matches!(e.event, SessionEventWire::Prompt(_))));
    a.t.send(SessionCommand::Answer {
        prompt_id: p.id,
        value: Some("s3cret".into()),
    })
    .await
    .unwrap();
    a.drive_to_idle().await;
    let mut b_seen = Vec::new();
    loop {
        let env = tokio::time::timeout(Duration::from_secs(5), b.next_event())
            .await
            .expect("b hung")
            .expect("alive");
        let s = format!("{:?}", env.event);
        let idle = matches!(env.event, SessionEventWire::Idle);
        b_seen.push(s);
        if idle {
            break;
        }
    }
    assert!(
        !b_seen.iter().any(|s| s.contains("s3cret") || s.contains("Answer")),
        "second client never sees the answer: {b_seen:?}"
    );
    assert!(a.seen.iter().any(|s| s.contains("answered:6")));
    assert_same_ext(&o, &a);
    assert_saves(&o, &o_s, &a_s, &o_path, &a_path);
    a.end().await;
}
