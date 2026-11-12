//! `SessionActor` turn scenarios through `EngineHost::create_session` +
//! `LocalTransport`, against the scripted provider stub: a provider error
//! repairing history, idle event injection auto-turning to the cap, a steer
//! landing mid-stream, and an event injected mid-stream joining the running
//! turn. Plain assertions on the actor's own output.
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

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, EndReason, LocalTransport, SessionCommand,
    SessionConfig, SessionEventWire,
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
    seen: Vec<String>,
    api_messages: Vec<synaps_cli::SharedMessage>,
    consecutive_auto_turns: u32,
    conversations_after_terminal: usize,
    cap_notices: u32,
    turns_started: usize,
    steered: Vec<bool>,
}

impl Run {
    async fn new(host: &Arc<EngineHost>) -> Self {
        let handle = host
            .create_session(SessionConfig {
                model_override: Some(MODEL.into()),
                persist: false,
                ..SessionConfig::default()
            })
            .await
            .expect("create_session");
        let (t, _snap) = LocalTransport::attach(handle, ClientMeta::new(ClientKind::Test))
            .await
            .unwrap();
        Self {
            t,
            seen: Vec::new(),
            api_messages: Vec::new(),
            consecutive_auto_turns: 0,
            conversations_after_terminal: 0,
            cap_notices: 0,
            turns_started: 0,
            steered: Vec::new(),
        }
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
                }
                SessionEventWire::TurnStarted { .. } => self.turns_started += 1,
                SessionEventWire::AutoTurnCapReached { .. } => self.cap_notices += 1,
                SessionEventWire::Steered { delivered, .. } => self.steered.push(*delivered),
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

/// A provider that fails every request: the turn ends in `Error`, and the
/// prompt that started it is all that is left of the turn in history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn provider_error_repairs_history_to_the_prompt() {
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
/// `Steered { delivered: true }`, a `SteeringDelivered` event, a second
/// provider round in the same turn, and the steer in history.
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
    assert_eq!(hits.load(Ordering::SeqCst), 2, "the steer forces a second round");
    assert!(a.seen.iter().any(|s| s.contains("SteeringDelivered")));
    let contents = a.contents();
    assert_eq!(contents.first().map(String::as_str), Some("start"));
    assert!(contents.iter().any(|c| c == "redirect"), "{contents:?}");
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
