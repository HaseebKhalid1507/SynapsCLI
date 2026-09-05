//! Phase 4 risk #10 — `daemon reload` with a `HistoryMode::Digest` client
//! attached (the thin `--attach` client). The reconnect must come back
//! DisplayTail-only: `Attached` carries a `display_tail`, no
//! `conversation.api_messages`, no `MessageHistory` on the wire, and the
//! transcript (the tail) is intact and grows on the next turn.
//!
//! Unix only (execv + flock).

#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
#[allow(dead_code)]
mod phase2;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use agent_engine::daemon::registry;
use agent_engine::session::display::DisplayItem;
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{SessionEvent, StreamEvent};
use phase2::*;
use serial_test::serial;

struct DaemonProc {
    child: Child,
    paths: registry::DaemonPaths,
}

impl Drop for DaemonProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn spawn_daemon(guard: &HomeGuard, url: &str) -> DaemonProc {
    let run = guard.base_dir().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let paths = registry::daemon_paths_in(&run, None);
    let child = Command::new(env!("CARGO_BIN_EXE_synaps"))
        .args(["daemon", "--foreground"])
        .env("HOME", guard.home.path())
        .env("SYNAPS_BASE_DIR", guard.base_dir())
        .env("SYNAPS_RUNTIME_DIR", &run)
        .env("SYNAPS_DAEMON", "1")
        .env("SYNAPS_ANTHROPIC_BASE_URL", url)
        .env("SYNAPS_NO_BOOT_FX", "1")
        .env_remove("SYNAPS_DAEMON_RELOAD_STATE")
        .env_remove("SYNAPS_DAEMON_LOCK_FD")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn daemon");
    let d = DaemonProc { child, paths };
    let t0 = std::time::Instant::now();
    loop {
        if SocketTransport::ping(&d.paths.sock).await.is_ok() {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "daemon never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    d
}

/// One turn on a Digest client: collect every event up to `Idle`; return
/// the `Conversation` digest's `messages_len` and whether `MessageHistory`
/// ever showed up.
async fn one_turn(t: &mut SocketTransport, text: &str) -> (usize, bool) {
    t.send_from_self(SessionCommand::Submit { text: text.into(), attachments: vec![] }).await.unwrap();
    let mut len = None;
    let mut saw_history = false;
    loop {
        let env = tokio::time::timeout(Duration::from_secs(20), t.next_event()).await.expect("turn hung").expect("alive");
        match env.event {
            SessionEventWire::Conversation(c) => len = Some(c.messages_len),
            SessionEventWire::Stream(StreamEvent::Session(SessionEvent::MessageHistory(_))) => saw_history = true,
            SessionEventWire::Idle => break,
            SessionEventWire::Refused { reason, .. } => panic!("refused: {reason}"),
            _ => {}
        }
    }
    (len.expect("conversation digest after the turn"), saw_history)
}

fn texts(items: &[DisplayItem]) -> Vec<String> {
    items
        .iter()
        .map(|i| match i {
            DisplayItem::User { text } => format!("U:{text}"),
            DisplayItem::Text { text } => format!("A:{text}"),
            DisplayItem::Thinking { text } => format!("T:{text}"),
            DisplayItem::ToolUse { tool_name, .. } => format!("tool:{tool_name}"),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn reload_reconnects_digest_client_with_display_tail_only() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let d = spawn_daemon(&guard, &url).await;
    let pid_before = d.child.id();

    let hello = Hello::new(ClientKind::Test).with_history(HistoryMode::Digest).with_tail_items(120);
    let conn = SocketTransport::connect(&d.paths.sock, hello).await.unwrap();
    let (mut t, snap) = SocketTransport::attach(
        conn,
        Attach::Create {
            config: SessionConfig {
                cwd: Some(Path::new(guard.home.path()).to_path_buf()),
                model_override: Some("claude-sonnet-4-5".into()),
                ..Default::default()
            },
            mode: AttachMode::Mirror,
        },
    )
    .await
    .unwrap();
    assert_eq!(t.history_mode(), HistoryMode::Digest);
    assert!(snap.display_tail.is_some(), "Digest attach carries a display_tail");
    assert!(snap.conversation.api_messages.is_empty());
    let sid = t.session_id().clone();

    let (len, saw_history) = one_turn(&mut t, "hello before reload").await;
    assert_eq!(len, 2, "user + assistant");
    assert!(!saw_history, "Digest client saw MessageHistory before reload");
    assert!(t.messages().is_empty(), "Digest client keeps no mirror");

    let gen = SocketTransport::reload(&d.paths.sock, true, None, None).await.expect("reload accepted");
    assert_eq!(gen, 2);
    let mut saw_reloading = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(10), t.next_event()).await.expect("announce") {
            Some(env) => {
                if let SessionEventWire::Reloading { generation, .. } = env.event {
                    assert_eq!(generation, 2);
                    saw_reloading = true;
                }
            }
            None => break,
        }
    }
    assert!(saw_reloading);

    let snap = tokio::time::timeout(Duration::from_secs(15), t.reconnect(AttachMode::Mirror))
        .await
        .expect("reconnect budget")
        .expect("reconnected");
    assert_eq!(t.welcome.generation, 2);
    assert_eq!(t.welcome.pid, pid_before, "same pid after execv");
    assert_eq!(t.session_id(), &sid);
    assert_eq!(t.history_mode(), HistoryMode::Digest, "history mode survives the reconnect Hello");

    // DisplayTail-only: no api_messages, no mirror, the tail is the transcript.
    assert!(snap.conversation.api_messages.is_empty(), "reconnect must not ship api_messages to a Digest client");
    assert!(t.messages().is_empty(), "no mirror after reconnect");
    let tail = snap.display_tail.as_ref().expect("reconnect Attached carries display_tail");
    assert_eq!(tail.omitted, 0);
    let got = texts(&tail.items);
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0], "U:hello before reload");
    assert!(got[1].starts_with("A:"), "{got:?}");

    // The next turn streams normally and still never carries MessageHistory.
    let (len, saw_history) = one_turn(&mut t, "hello after reload").await;
    assert_eq!(len, 4);
    assert!(!saw_history, "Digest client saw MessageHistory after reload");

    t.detach().await;
    SocketTransport::shutdown(&d.paths.sock, false).await.unwrap();
    let t0 = std::time::Instant::now();
    while registry::is_alive(&d.paths) && t0.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!registry::is_alive(&d.paths));
}
