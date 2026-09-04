//! §5.1 — byte-identical proof for `LocalTransport`: the same scripted
//! session runs through the FROZEN reference reactor (`support/reference_reactor.rs`,
//! today's inline engine halves) and through `EngineHost::create_session` +
//! `LocalTransport`; the `StreamEvent` sequences, the final `api_messages`
//! and the auto-turn counters must be identical.

#[path = "support/phase2/mod.rs"]
mod support;
#[path = "support/reference_reactor.rs"]
mod reference_reactor;

use std::sync::Arc;

use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, EndReason, LocalTransport, SessionCommand,
    SessionConfig, SessionEventWire,
};
use agent_engine::{EngineHost, HostOpts};
use reference_reactor::ReferenceReactor;
use serial_test::serial;
use support::*;
use synaps_cli::{SessionEvent, StreamEvent};

const MODEL: &str = "claude-sonnet-4-5";

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
    seen: Vec<String>,
    api_messages: Vec<synaps_cli::SharedMessage>,
    consecutive_auto_turns: u32,
    conversations_after_terminal: usize,
    cap_notices: u32,
}

async fn actor(host: &Arc<EngineHost>) -> ActorRun {
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
    ActorRun {
        t,
        seen: Vec::new(),
        api_messages: Vec::new(),
        consecutive_auto_turns: 0,
        conversations_after_terminal: 0,
        cap_notices: 0,
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
                    self.api_messages = c.api_messages;
                    self.consecutive_auto_turns = c.consecutive_auto_turns;
                }
                SessionEventWire::AutoTurnCapReached { .. } => self.cap_notices += 1,
                SessionEventWire::Idle => break,
                SessionEventWire::Ended { .. } => panic!("ended early"),
                _ => {}
            }
        }
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
