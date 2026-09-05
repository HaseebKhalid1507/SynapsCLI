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
    // Compare the `messages` array only: the system prompt legitimately contains
    // English like "from a file".
    let msgs = |b: &str| serde_json::from_str::<serde_json::Value>(b).unwrap()["messages"].to_string();
    let (m0, m1) = (msgs(&b0), msgs(&b1));
    assert!(m0.contains("from a") && !m0.contains("from b"), "{m0}");
    assert!(m1.contains("from b") && !m1.contains("from a"), "{m1}");

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

/// Bug: `--continue X` of a journal already live in the daemon built a
/// second actor on the same journal (two per-session UDS / registry entries
/// fighting over one file). The daemon's `Attach::Create{continue}` must
/// land on the running actor (mirror) with a notice — never a second actor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn continue_of_live_journal_attaches_instead_of_second_actor() {
    let guard = HomeGuard::new();
    let (url, _hits, _bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host: Arc<EngineHost> =
        EngineHost::boot_and_install(HostOpts { profile: None, no_extensions: true }).await.expect("host boot");
    let d = Daemon::start(host.clone(), DaemonOpts { runtime_dir: Some(guard.base_dir().join("run")), ..Default::default() })
        .await
        .expect("daemon start");
    let sock = d.paths.sock.clone();
    let cwd = guard.home.path().join("a");
    std::fs::create_dir_all(&cwd).unwrap();

    // A: created fresh, named at create, NO turn yet (nothing but the name on disk).
    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (mut a, snap_a) = SocketTransport::attach(
        conn,
        Attach::Create {
            config: SessionConfig {
                cwd: Some(cwd.clone()),
                model_override: Some("claude-sonnet-4-5".into()),
                name: Some("ambient".into()),
                ..Default::default()
            },
            mode: AttachMode::Mirror,
        },
    )
    .await
    .expect("attach create");
    assert_eq!(snap_a.meta.name.as_deref(), Some("ambient"));
    let id_a = a.session_id().clone();
    assert!(matches!(next(&mut a).await.event, SessionEventWire::ClientJoined { .. }));

    // Second client: `--continue ambient` (by name) and `--continue <id>` — same actor both times.
    let mut mirrors = Vec::new();
    for query in ["ambient", id_a.as_str()] {
        let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
        let (mut b, snap_b) = SocketTransport::attach(
            conn,
            Attach::Create {
                config: SessionConfig {
                    continue_session: Some(Some(query.to_string())),
                    cwd: Some(cwd.clone()),
                    ..Default::default()
                },
                mode: AttachMode::Mirror,
            },
        )
        .await
        .expect("attach continue");
        assert_eq!(b.session_id(), &id_a, "continue {query:?} → the live session");
        assert_eq!(snap_b.meta.name.as_deref(), Some("ambient"));
        assert_eq!(host.sessions().len(), 1, "one actor after continue {query:?}");
        assert_eq!(d.state.live_sessions().len(), 1);
        // The client is told it was attached, not given a fresh session.
        let notice = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let e = next(&mut b).await;
                if let SessionEventWire::SystemNotice(t) = e.event {
                    if t.contains("already running") {
                        return t;
                    }
                }
            }
        })
        .await
        .expect("attach-to-live notice");
        assert!(notice.contains(id_a.as_str()), "{notice}");
        // A sees the join (a Conversation/Lifecycle publish may precede it).
        let mut seen = Vec::new();
        loop {
            let e = next(&mut a).await;
            let joined = matches!(e.event, SessionEventWire::ClientJoined { .. });
            seen.push(format!("{:?}", e.event).chars().take(60).collect::<String>());
            if joined {
                break;
            }
            assert!(seen.len() < 8, "A never saw ClientJoined: {seen:?}");
        }
        mirrors.push(b);
    }
    // One registry entry, resolvable by name — the first actor's socket, still bound.
    let regs = agent_engine::events::registry::list_active_sessions();
    assert_eq!(regs.len(), 1, "{regs:?}");
    assert_eq!(regs[0].name.as_deref(), Some("ambient"));
    assert!(tokio::net::UnixStream::connect(&regs[0].socket_path).await.is_ok());

    // Same actor: a turn submitted via A streams to the mirrors.
    a.send(SessionCommand::Submit { text: "from a".into(), attachments: vec![] }).await.unwrap();
    let seen_a = turn(&mut a).await;
    assert_eq!(text_of(&seen_a), "hi");
    for b in mirrors.iter_mut() {
        let seen_b = turn(b).await;
        assert_eq!(text_of(&seen_b), "hi", "mirror saw A's turn");
    }

    SocketTransport::shutdown(&sock, false).await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), d.wait()).await.expect("daemon wait");
}
