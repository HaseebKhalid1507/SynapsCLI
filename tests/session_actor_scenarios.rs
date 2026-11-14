//! `SessionActor` turn scenarios through `EngineHost::create_session` +
//! `LocalTransport`, against the scripted provider stub: a provider error
//! leaving only the prompt, idle event injection auto-turning to the cap, a steer
//! landing mid-stream, and an event injected mid-stream joining the running
//! turn; how often the actor writes its journal in a tool loop and a
//! cancelled turn, and what the journal holds after a tool loop. Plain
//! assertions on the actor's own output.
//!
//! These scenarios used to be checked against a frozen copy of the
//! pre-actor inline reactor (`session_actor_differential.rs`, removed in
//! 0.12.0). The other scenarios of that file have direct actor tests:
//! plain turn (`crates/agent-engine/tests/session_actor.rs`
//! `attach_detach_counts_and_seq_is_gapless`), tool loop
//! (`session_actor_checkpoint.rs`), cancel (`session_actor_interrupt.rs`),
//! secret prompt (`session_actor.rs` `pending_prompt_*`).

#[path = "support/phase2/mod.rs"]
mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, EndReason, LocalTransport, SessionCommand,
    SessionConfig, SessionEventWire, SessionHandle,
};
use agent_engine::{EngineHost, HostOpts};
use serial_test::serial;
use support::*;
use synaps_cli::{SessionEvent, StreamEvent};

const MODEL: &str = "claude-sonnet-4-5";

async fn host() -> Arc<EngineHost> {
    EngineHost::boot(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .expect("host boot")
}

/// What one client saw of a session.
struct Run {
    t: LocalTransport,
    handle: SessionHandle,
    seen: Vec<String>,
    api_messages: Vec<synaps_cli::SharedMessage>,
    consecutive_auto_turns: u32,
    conversations_after_terminal: usize,
    cap_notices: u32,
    turns_started: usize,
    steered: Vec<bool>,
    dequeued: Vec<String>,
    abort_context: Option<String>,
    cost: f64,
    tokens: (u64, u64),
}

impl Run {
    async fn new(host: &Arc<EngineHost>) -> Self {
        Self::with(host, false).await
    }

    async fn with(host: &Arc<EngineHost>, persist: bool) -> Self {
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
        Self {
            t,
            handle,
            seen: Vec::new(),
            api_messages: Vec::new(),
            consecutive_auto_turns: 0,
            conversations_after_terminal: 0,
            cap_notices: 0,
            turns_started: 0,
            steered: Vec::new(),
            dequeued: Vec::new(),
            abort_context: None,
            cost: 0.0,
            tokens: (0, 0),
        }
    }

    fn journal_path(&self, guard: &HomeGuard) -> std::path::PathBuf {
        guard
            .base_dir()
            .join("sessions")
            .join(format!("{}.json", self.handle.journal_id()))
    }

    /// Barrier: every save the actor has queued is on disk. Commands run in
    /// order and `Save` waits for the persister, so the `Status` reply that
    /// follows it proves the flush.
    async fn flushed(&mut self) {
        const ID: u64 = 0x5a7e;
        self.t.send(SessionCommand::Save).await.unwrap();
        self.t
            .send(SessionCommand::Query {
                id: ID,
                query: agent_engine::session::SessionQuery::Status,
            })
            .await
            .unwrap();
        self.until(|e| matches!(e, SessionEventWire::QueryResult { id: ID, .. }))
            .await;
    }

    /// Pump, recording, until `pred` matches an envelope; returns it.
    async fn until(&mut self, pred: impl Fn(&SessionEventWire) -> bool) -> SessionEventWire {
        let mut expect_conv = false;
        loop {
            let env = tokio::time::timeout(Duration::from_secs(30), self.t.next_event())
                .await
                .expect("actor hung")
                .expect("actor alive");
            let hit = pred(&env.event);
            match &env.event {
                SessionEventWire::Stream(ev) => {
                    self.seen.push(format!("{ev:?}"));
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
                    self.api_messages = c.api_messages.clone();
                    self.consecutive_auto_turns = c.consecutive_auto_turns;
                    self.abort_context = c.abort_context.clone();
                    self.cost = c.cost;
                    self.tokens = (c.tokens.input, c.tokens.output);
                }
                SessionEventWire::TurnStarted { .. } => self.turns_started += 1,
                SessionEventWire::AutoTurnCapReached { .. } => self.cap_notices += 1,
                SessionEventWire::Steered { delivered, .. } => self.steered.push(*delivered),
                SessionEventWire::Dequeued { text } => self.dequeued.push(text.clone()),
                SessionEventWire::Ended { .. } => panic!("ended early"),
                _ => {}
            }
            if hit {
                return env.event;
            }
        }
    }

    async fn drive_to_idle(&mut self) {
        self.until(|e| matches!(e, SessionEventWire::Idle)).await;
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

    fn contents(&self) -> Vec<String> {
        self.api_messages
            .iter()
            .map(|m| match &m["content"] {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect()
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

fn is_text(ev: &SessionEventWire) -> bool {
    matches!(
        ev,
        SessionEventWire::Stream(StreamEvent::Llm(synaps_cli::LlmEvent::Text(_)))
    )
}

fn fixed_event(i: usize) -> synaps_cli::events::types::Event {
    let mut ev = synaps_cli::events::types::Event::simple("scenario", &format!("event {i}"), None);
    ev.id = format!("evt-{i}");
    ev
}

/// Inject an event through the session's real socket (the `synaps send` path).
async fn inject(session_id: &str, ev: &synaps_cli::events::types::Event) {
    let path = synaps_cli::events::registry::find_session_registration(session_id)
        .expect("published session endpoint")
        .socket_path;
    synaps_cli::events::socket::send_to_session_socket(
        &path,
        serde_json::to_string(ev).unwrap().as_bytes(),
    )
    .await
    .expect("session socket");
}

/// Save counter from OUTSIDE the actor: a journal save is a tmp + rename
/// (`write_atomic_private`), so every save gives `sessions/<id>.json` a
/// fresh inode, which a spinning thread samples.
///
/// A LOWER bound, never exact: a thread not scheduled between two renames
/// misses one, and a freed inode number can come straight back. So it is
/// only ever checked against upper bounds: a miss can't fail a test, and a
/// count above the bound is a real extra write.
struct SaveSampler {
    stop: Arc<AtomicBool>,
    inodes: Arc<Mutex<Vec<u64>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SaveSampler {
    fn watch(path: std::path::PathBuf) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let inodes: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        #[cfg(unix)]
        let thread = {
            use std::os::unix::fs::MetadataExt;
            let (s, i) = (Arc::clone(&stop), Arc::clone(&inodes));
            Some(std::thread::spawn(move || {
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
            }))
        };
        // No inode numbers off Unix, so nothing is sampled and `count` is
        // `None`, which skips the save-count checks there.
        #[cfg(not(unix))]
        let thread = {
            let _ = path;
            None
        };
        Self { stop, inodes, thread }
    }

    fn count(&self) -> Option<usize> {
        cfg!(unix).then(|| self.inodes.lock().unwrap().len())
    }

    /// The save-count checks are upper bounds, which a sampler that counts
    /// nothing passes trivially. So prove it is live: once the journal
    /// exists the sampler must have seen at least that inode. Polls to a
    /// short deadline; the thread may not have run since the rename.
    fn assert_live(&self, path: &std::path::Path) {
        assert!(path.exists(), "the actor never saved");
        if self.count().is_none() {
            return;
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while self.count() == Some(0) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            self.count() >= Some(1),
            "save sampler counted no save although the journal exists: \
             the save-count checks would prove nothing"
        );
    }

    fn assert_at_most(&self, path: &std::path::Path, bound: usize, what: &str) {
        self.assert_live(path);
        if let Some(n) = self.count() {
            assert!(n <= bound, "{what}: {n} saves > {bound}");
        }
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

/// Without the host-only message times (`_synaps_at`).
fn msgs_json(m: &serde_json::Value) -> String {
    let re = regex::Regex::new(r#""_synaps_at":"[^"]*",?"#).unwrap();
    re.replace_all(&serde_json::to_string(m).unwrap(), "").into_owned()
}

fn read_journal(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("journal exists")).unwrap()
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

async fn host_with_echo_tool() -> Arc<EngineHost> {
    let host = host().await;
    host.parts()
        .tools
        .write()
        .await
        .register(Arc::new(EchoFixtureTool));
    host
}

/// A provider that fails every request: the turn ends in `Error`, and the
/// prompt that started it is all that is left of the turn in history.
///
/// Not a test of `repair_history_after_failure` dropping anything: the
/// engine never publishes a partial assistant message on a provider error
/// (it appends a reply only once the round completes), so there is nothing
/// for the actor's repair to drop on this path. Its trimming is unit-tested
/// in `engine::stream`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn provider_error_keeps_only_the_prompt_in_history() {
    let _guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::AlwaysFailEcho(500)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;

    let mut a = Run::new(&host).await;
    a.submit("fail me").await;
    a.drive_to_idle().await;

    assert!(hits.load(Ordering::SeqCst) >= 1);
    assert!(
        a.seen.iter().any(|s| s.starts_with("Session(Error(")),
        "{:?}",
        a.seen
    );
    assert!(!a.seen.iter().any(|s| s == "Session(Done)"));
    assert_eq!(a.contents(), ["fail me"]);
    assert_eq!(a.api_messages[0]["role"], "user");
    assert!(a.conversations_after_terminal >= 1, "history published after the error");
    a.end().await;
}

/// Events arriving while idle each start an auto-turn until the default cap
/// (5); the sixth parks with one cap notice and no provider request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn idle_events_auto_turn_until_the_cap() {
    let _guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;

    let mut a = Run::new(&host).await;
    let sid = a.t.session_id().as_str().to_string();
    for i in 0..6 {
        inject(&sid, &fixed_event(i)).await;
        let ev = a
            .until(|e| {
                matches!(
                    e,
                    SessionEventWire::TurnStarted { .. } | SessionEventWire::AutoTurnCapReached { .. }
                )
            })
            .await;
        if matches!(ev, SessionEventWire::TurnStarted { .. }) {
            assert!(i < 5, "event {i} must not start a turn past the cap");
            a.drive_to_idle().await;
        } else {
            assert_eq!(i, 5, "the cap trips on the sixth event, not event {i}");
        }
    }

    assert_eq!(a.turns_started, 5);
    assert_eq!(a.cap_notices, 1);
    assert_eq!(a.consecutive_auto_turns, 5);
    assert_eq!(hits.load(Ordering::SeqCst), 5, "the parked event made no request");
    let contents = a.contents();
    for i in 0..5 {
        assert!(
            contents.iter().any(|c| c.contains(&format!("event {i}"))),
            "event {i} reached history: {contents:?}"
        );
    }
    a.end().await;
}

/// A steer sent while the reply streams is delivered into the running turn:
/// `Steered { delivered: true }`, and `SteeringDelivered` after the first
/// round's reply and before the second's. History has the first reply, then
/// the steer, then the redirected reply; token and cost totals are both
/// rounds' (twice a plain one-round turn's on the same stub).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn steer_mid_stream_is_delivered_into_the_running_turn() {
    let _guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::Paced {
        body: ANTHROPIC_SSE,
        frame_delay: Duration::from_millis(100),
    })
    .await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;

    let mut plain = Run::new(&host).await;
    plain.submit("start").await;
    plain.drive_to_idle().await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let (one_tokens, one_cost) = (plain.tokens, plain.cost);
    assert!(one_tokens.0 > 0 && one_tokens.1 > 0 && one_cost > 0.0);
    plain.end().await;

    let mut a = Run::new(&host).await;
    a.submit("start").await;
    a.until(is_text).await;
    a.t.send(SessionCommand::Steer {
        text: "redirect".into(),
    })
    .await
    .unwrap();
    a.drive_to_idle().await;

    assert_eq!(a.steered, vec![true]);
    assert_eq!(a.turns_started, 1, "one turn");
    assert_eq!(hits.load(Ordering::SeqCst), 3, "the steer forces a second round");

    let texts: Vec<usize> = (0..a.seen.len())
        .filter(|&i| a.seen[i].starts_with("Llm(Text("))
        .collect();
    let delivered = a
        .seen
        .iter()
        .position(|s| s.contains("SteeringDelivered"))
        .expect("SteeringDelivered");
    assert!(
        texts.first().is_some_and(|&t| t < delivered),
        "SteeringDelivered after the first reply: {:?}",
        a.seen
    );
    assert!(
        texts.last().is_some_and(|&t| t > delivered),
        "the redirected reply streams after SteeringDelivered: {:?}",
        a.seen
    );

    let roles: Vec<&str> = a
        .api_messages
        .iter()
        .map(|m| m["role"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant"], "{:?}", a.contents());
    let contents = a.contents();
    assert_eq!(contents[0], "start");
    assert!(contents[1].contains("hi"), "first reply kept: {contents:?}");
    assert!(contents[2].contains("redirect"), "steer after the first reply: {contents:?}");
    assert!(contents[3].contains("hi"), "redirected reply last: {contents:?}");

    assert_eq!(a.tokens, (one_tokens.0 * 2, one_tokens.1 * 2), "two rounds' tokens");
    assert!(
        (a.cost - one_cost * 2.0).abs() < 1e-12,
        "two rounds' cost: {} vs 2 × {}",
        a.cost,
        one_cost
    );
    a.end().await;
}

/// An event arriving while a turn streams is steered into that turn (not
/// buffered for a later auto-turn): one turn, two provider rounds, the
/// event in history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn event_mid_stream_joins_the_running_turn() {
    let _guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::Paced {
        body: ANTHROPIC_SSE,
        frame_delay: Duration::from_millis(100),
    })
    .await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;

    let mut a = Run::new(&host).await;
    let sid = a.t.session_id().as_str().to_string();
    a.submit("start").await;
    a.until(is_text).await;
    inject(&sid, &fixed_event(0)).await;
    a.until(|e| matches!(e, SessionEventWire::External(_))).await;
    a.drive_to_idle().await;

    assert_eq!(a.turns_started, 1, "no separate auto-turn for the event");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(a.cap_notices, 0);
    let contents = a.contents();
    assert!(
        contents.iter().any(|c| c.contains("event 0")),
        "{contents:?}"
    );
    a.end().await;
}

/// A tool loop writes the journal at most once per `MessageHistory` the
/// engine publishes, and the journal it leaves matches the final snapshot:
/// history (with the tool result), token totals, cost, no abort context.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn tool_loop_saves_at_most_once_per_history_and_journals_the_turn() {
    let guard = HomeGuard::new();
    let tool = sse_tool_call("echo_fixture", "toolu_scn1");
    let bodies: &'static [&'static str] = Box::leak(Box::new([tool, ANTHROPIC_SSE]));
    let (url, hits, _) = spawn_stub(Script::SeqSse(bodies)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host_with_echo_tool().await;

    let mut a = Run::with(&host, true).await;
    let path = a.journal_path(&guard);
    let sampler = SaveSampler::watch(path.clone());
    a.submit("call the tool").await;
    a.drive_to_idle().await;
    a.flushed().await;

    assert_eq!(hits.load(Ordering::SeqCst), 2, "tool round + final round");
    let histories = a.seen.iter().filter(|s| s.contains("MessageHistory")).count();
    assert!(histories >= 1, "a tool loop publishes MessageHistory");
    assert!(a.seen.iter().any(|s| s.contains("ToolResult")));
    sampler.assert_at_most(&path, histories, "tool loop");

    let j = read_journal(&path);
    assert_eq!(
        msgs_json(&j["api_messages"]),
        msgs_json(&serde_json::to_value(&a.api_messages).unwrap()),
        "journal history is the final snapshot's"
    );
    assert!(
        j["api_messages"].to_string().contains("fixture-ok"),
        "the tool result is in the journal"
    );
    assert_eq!(a.api_messages.len(), 4, "prompt, tool_use, tool_result, reply");
    assert_eq!(a.tokens, (20, 6), "both rounds' usage counted");
    assert_eq!(j["total_input_tokens"], a.tokens.0);
    assert_eq!(j["total_output_tokens"], a.tokens.1);
    assert!(a.cost > 0.0);
    assert_eq!(j["session_cost"].as_f64(), Some(a.cost));
    assert!(j["abort_context"].is_null());
    assert!(a.abort_context.is_none());
    a.end().await;
}

/// A cancelled turn writes the journal at most twice: the prompt
/// checkpoint before the first request and one at the abort. Two cancelled
/// turns, checked after each.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_cancelled_turn_saves_at_most_the_prompt_checkpoint_and_the_abort() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Endless(ANTHROPIC_SSE_PREFIX)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;

    let mut a = Run::with(&host, true).await;
    let path = a.journal_path(&guard);
    let sampler = SaveSampler::watch(path.clone());

    a.submit("first").await;
    a.until(is_text).await;
    a.t.send(SessionCommand::Steer {
        text: "queued-then-dropped".into(),
    })
    .await
    .unwrap();
    a.t.send(SessionCommand::Cancel).await.unwrap();
    a.drive_to_idle().await;
    assert_eq!(a.dequeued, ["queued-then-dropped"]);
    sampler.assert_at_most(&path, 2, "first cancelled turn");

    a.submit("second").await;
    a.until(is_text).await;
    a.t.send(SessionCommand::Cancel).await.unwrap();
    a.drive_to_idle().await;
    a.flushed().await;
    sampler.assert_at_most(&path, 4, "two cancelled turns");

    let marker = synaps_cli::engine::interrupt::InterruptReason::User.marker();
    let j = read_journal(&path);
    assert_eq!(
        msgs_json(&j["api_messages"]),
        msgs_json(&serde_json::to_value(&a.api_messages).unwrap())
    );
    assert_eq!(a.api_messages.len(), 6, "{:?}", a.contents());
    assert!(a.contents()[2].contains(marker));
    assert!(j["abort_context"].is_null(), "no recap folded into history");
    a.end().await;
}
