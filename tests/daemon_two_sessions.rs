//! Two REAL sessions (SessionActor via `EngineHost::create_session`, A1) in
//! one daemon, each answered by a loopback Anthropic stub (PLAN-phase2 §5.3).

#[path = "support/phase2/mod.rs"]
#[allow(dead_code)]
mod phase2;

use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::{Daemon, DaemonOpts};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts, LlmEvent, SessionEvent, StreamEvent};
use phase2::{spawn_stub, HomeGuard, Script, ANTHROPIC_SSE};
use serial_test::serial;

async fn next(t: &mut SocketTransport) -> Envelope {
    tokio::time::timeout(Duration::from_secs(10), t.next_event()).await.expect("timely").expect("open")
}

/// Everything up to and including `Idle` (stream ended, no auto-turn followed).
async fn turn(t: &mut SocketTransport) -> Vec<Envelope> {
    let mut seen = Vec::new();
    loop {
        let e = next(t).await;
        let done = matches!(e.event, SessionEventWire::Idle);
        seen.push(e);
        if done {
            return seen;
        }
    }
}

fn text_of(seen: &[Envelope]) -> String {
    seen.iter()
        .filter_map(|e| match &e.event {
            SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::Text(t))) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn two_sessions_one_daemon_isolated() {
    let guard = HomeGuard::new();
    let (url, hits, bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host: Arc<EngineHost> =
        EngineHost::boot_and_install(HostOpts { profile: None, no_extensions: true }).await.expect("host boot");
    let d = Daemon::start(host.clone(), DaemonOpts { runtime_dir: Some(guard.base_dir().join("run")), ..Default::default() })
        .await
        .expect("daemon start");
    let sock = d.paths.sock.clone();

    let cwd_a = guard.home.path().join("a");
    let cwd_b = guard.home.path().join("b");
    std::fs::create_dir_all(&cwd_a).unwrap();
    std::fs::create_dir_all(&cwd_b).unwrap();

    let mut clients = Vec::new();
    for cwd in [&cwd_a, &cwd_b] {
        let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
        let (t, snap) = SocketTransport::attach(
            conn,
            Attach::Create {
                config: SessionConfig {
                    cwd: Some(cwd.clone()),
                    model_override: Some("claude-sonnet-4-5".into()),
                    persist: false,
                    ..Default::default()
                },
                mode: AttachMode::Mirror,
            },
        )
        .await
        .expect("attach create");
        assert_eq!(snap.meta.cwd.as_deref(), Some(cwd.as_path()));
        assert_eq!(snap.meta.host_pid, std::process::id());
        assert_eq!(snap.view.model, "claude-sonnet-4-5");
        clients.push(t);
    }
    let [mut a, mut b]: [SocketTransport; 2] = clients.try_into().ok().unwrap();
    assert_ne!(a.session_id(), b.session_id());
    assert_eq!(host.sessions().len(), 2, "both live on the host");
    assert_eq!(d.state.live_sessions().len(), 2);

    // ClientJoined for each
    assert!(matches!(next(&mut a).await.event, SessionEventWire::ClientJoined { .. }));
    assert!(matches!(next(&mut b).await.event, SessionEventWire::ClientJoined { .. }));

    a.send(SessionCommand::Submit { text: "from a".into(), attachments: vec![] }).await.unwrap();
    let sa = turn(&mut a).await;
    b.send(SessionCommand::Submit { text: "from b".into(), attachments: vec![] }).await.unwrap();
    let sb = turn(&mut b).await;

    for (s, who) in [(&sa, "a"), (&sb, "b")] {
        assert!(matches!(s[0].event, SessionEventWire::TurnStarted { .. }), "{who}: {:?}", s[0].event);
        assert_eq!(text_of(s), "hi", "{who}");
        assert!(s.iter().any(|e| matches!(e.event, SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Done)))), "{who}");
        let seqs: Vec<u64> = s.iter().map(|e| e.seq).collect();
        assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{who} gapless {seqs:?}");
    }
    // each envelope is stamped with its own session id
    assert!(sa.iter().all(|e| &e.session_id == a.session_id()));
    assert!(sb.iter().all(|e| &e.session_id == b.session_id()));
    // the Conversation snapshot is per session: one user turn each
    let conv = |s: &[Envelope]| {
        s.iter()
            .rev()
            .find_map(|e| match &e.event {
                SessionEventWire::Conversation(c) => Some(c.api_messages.clone()),
                _ => None,
            })
            .expect("Conversation snapshot")
    };
    let ma = conv(&sa);
    let mb = conv(&sb);
    assert_eq!(ma.len(), 2);
    assert_eq!(mb.len(), 2);
    assert_eq!(ma[0]["content"], "from a");
    assert_eq!(mb[0]["content"], "from b");

    // provider saw exactly two requests, each carrying only its own session's prompt
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    let bodies = bodies.lock().unwrap();
    let b0 = String::from_utf8_lossy(&bodies[0]);
    let b1 = String::from_utf8_lossy(&bodies[1]);
    assert!(b0.contains("from a") && !b0.contains("from b"));
    assert!(b1.contains("from b") && !b1.contains("from a"));

    // detach a mid-life: session stays; b unaffected
    a.detach().await;
    assert!(a.next_event().await.is_none());
    assert_eq!(host.sessions().len(), 2);

    // daemon stop ends both sessions
    SocketTransport::shutdown(&sock, false).await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), d.wait()).await.expect("daemon wait");
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match b.next_event().await {
                Some(e) if matches!(e.event, SessionEventWire::Ended { .. }) => return true,
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await
    .unwrap();
    assert!(ended);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(host.sessions().is_empty(), "host map drained after End");
}
