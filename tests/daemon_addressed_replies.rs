//! Contract v2 §1 (PR 0a) over the socket: a reply reaches only the
//! connection that asked. Every client numbers its own requests, so two
//! clients' request 5 share an id; the daemon used to broadcast every reply
//! and each took the other's. Shared replies (`SettingChanged`, `Resumed`)
//! still reach everyone, view-only for those who did not ask; a host's
//! reply reaches no client; `SocketTransport`'s digest resync, which old
//! clients send too, is its asker's alone and still repairs the mirror.
//! The raw-frame client plays an older reader: the wire is unchanged.

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::{Daemon, DaemonOpts};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts};
use phase2::{spawn_stub, HomeGuard, Script, ANTHROPIC_SSE};
use serial_test::serial;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

const QUIET: Duration = Duration::from_millis(500);

async fn boot(guard: &HomeGuard) -> Daemon {
    let (url, _hits, _bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host: Arc<EngineHost> = EngineHost::boot_and_install(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .expect("host boot");
    Daemon::start(host, DaemonOpts { runtime_dir: Some(guard.base_dir().join("run")), ..Default::default() })
        .await
        .expect("daemon start")
}

async fn stop(d: Daemon, sock: &Path) {
    SocketTransport::shutdown(sock, true).await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), d.wait()).await.expect("daemon wait");
}

fn config(guard: &HomeGuard, persist: bool) -> SessionConfig {
    SessionConfig {
        cwd: Some(guard.home.path().to_path_buf()),
        model_override: Some("claude-sonnet-4-5".into()),
        persist,
        client_input: Some(ClientInput::Shared),
        ..Default::default()
    }
}

async fn attach(sock: &Path, a: Attach) -> SocketTransport {
    let conn = SocketTransport::connect(sock, Hello::new(ClientKind::Test)).await.unwrap();
    SocketTransport::attach(conn, a).await.unwrap().0
}

async fn next(t: &mut SocketTransport) -> Envelope {
    tokio::time::timeout(Duration::from_secs(10), t.next_event()).await.expect("timely").expect("open")
}

async fn until(t: &mut SocketTransport, pred: impl Fn(&SessionEventWire) -> bool) -> Vec<SessionEventWire> {
    let mut seen = Vec::new();
    loop {
        let e = next(t).await.event;
        let done = pred(&e);
        seen.push(e);
        if done {
            return seen;
        }
    }
}

async fn drain(t: &mut SocketTransport) -> Vec<SessionEventWire> {
    let mut seen = Vec::new();
    while let Ok(Some(e)) = tokio::time::timeout(QUIET, t.next_event()).await {
        seen.push(e.event);
    }
    seen
}

fn results(seen: &[SessionEventWire], want: u64) -> Vec<serde_json::Value> {
    seen.iter()
        .filter_map(|e| match e {
            SessionEventWire::QueryResult { id, value } if *id == want => Some(value.clone()),
            _ => None,
        })
        .collect()
}

fn settings(seen: &[SessionEventWire]) -> Vec<(u64, String)> {
    seen.iter()
        .filter_map(|e| match e {
            SessionEventWire::SettingChanged(a) => Some((a.id, a.setting.clone())),
            _ => None,
        })
        .collect()
}

// ── raw-frame client (an older reader: it sees exactly what is on the wire) ──

type Lines = tokio::io::Lines<tokio::io::BufReader<agent_engine::ipc::ClientReadHalf>>;

struct Raw {
    lines: Lines,
    w: agent_engine::ipc::ClientWriteHalf,
    sid: SessionId,
}

impl Raw {
    async fn attach(sock: &Path, sid: &SessionId) -> Raw {
        let (r, w) = agent_engine::ipc::split_client(agent_engine::ipc::connect(sock).await.unwrap());
        let mut raw = Raw { lines: tokio::io::BufReader::new(r).lines(), w, sid: sid.clone() };
        raw.write(&ClientFrame::Hello(Hello::new(ClientKind::Attach))).await;
        raw.until(|f| matches!(f, DaemonFrame::Welcome(_))).await;
        raw.write(&ClientFrame::Attach(Attach::Existing { session_id: sid.clone(), mode: AttachMode::Mirror })).await;
        raw.until(|f| matches!(f, DaemonFrame::Attached(_))).await;
        raw
    }

    async fn write(&mut self, f: &ClientFrame) {
        self.w.write_all(encode_line(f).unwrap().as_bytes()).await.unwrap();
    }

    async fn send(&mut self, cmd: SessionCommand) {
        let session_id = self.sid.clone();
        self.write(&ClientFrame::Cmd { session_id, cmd }).await;
    }

    async fn frame(&mut self, budget: Duration) -> Option<DaemonFrame> {
        let line = tokio::time::timeout(budget, self.lines.next_line()).await.ok()?.unwrap()?;
        Some(decode_line::<DaemonFrame>(&line).unwrap())
    }

    async fn until(&mut self, pred: impl Fn(&DaemonFrame) -> bool) -> Vec<DaemonFrame> {
        let mut seen = Vec::new();
        loop {
            let f = self.frame(Duration::from_secs(10)).await.expect("a frame in time");
            let done = pred(&f);
            seen.push(f);
            if done {
                return seen;
            }
        }
    }

    /// Every event until [`QUIET`] passes with none.
    async fn drain(&mut self) -> Vec<WireSessionEvent> {
        let mut seen = Vec::new();
        while let Some(f) = self.frame(QUIET).await {
            if let DaemonFrame::Event(env) = f {
                seen.push(env.event);
            }
        }
        seen
    }
}

fn wire_events(frames: Vec<DaemonFrame>) -> Vec<WireSessionEvent> {
    frames
        .into_iter()
        .filter_map(|f| match f {
            DaemonFrame::Event(env) => Some(env.event),
            _ => None,
        })
        .collect()
}

/// Two clients send the same request ids at once: each gets only its own
/// `Query`, `Set`, `Resume` (error) and `EngineCommand` reply; each sees
/// the other's `Set` as a view change.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn two_clients_with_the_same_request_ids_get_only_their_own_replies() {
    let guard = HomeGuard::new();
    let d = boot(&guard).await;
    let sock = d.paths.sock.clone();
    let mut a = attach(&sock, Attach::create(config(&guard, false), AttachMode::Mirror)).await;
    let sid = a.session_id().clone();
    let mut b = attach(&sock, Attach::Existing { session_id: sid, mode: AttachMode::Mirror }).await;

    let send_all = |query: SessionQuery, setting: SessionSetting, resume: &str, barrier: u64| {
        vec![
            SessionCommand::Query { id: 5, query },
            SessionCommand::Set { id: 6, setting },
            SessionCommand::Resume { id: 7, query: resume.to_string() },
            SessionCommand::EngineCommand { id: 8, name: "model".into(), arg: String::new() },
            SessionCommand::Query { id: barrier, query: SessionQuery::View },
        ]
    };
    for cmd in send_all(SessionQuery::Status, SessionSetting::ApiRetries { n: 2 }, "no-such-session-aaaa", 100) {
        a.send(cmd).await.unwrap();
    }
    for cmd in send_all(SessionQuery::View, SessionSetting::MaxToolOutput { bytes: 9000 }, "no-such-session-bbbb", 200) {
        b.send(cmd).await.unwrap();
    }
    let mut sa = until(&mut a, |e| matches!(e, SessionEventWire::QueryResult { id: 100, .. })).await;
    let mut sb = until(&mut b, |e| matches!(e, SessionEventWire::QueryResult { id: 200, .. })).await;
    sa.extend(drain(&mut a).await);
    sb.extend(drain(&mut b).await);

    for (who, seen) in [("a", &sa), ("b", &sb)] {
        for id in [5, 7, 8] {
            assert_eq!(results(seen, id).len(), 1, "{who}: one reply to its request {id}: {seen:#?}");
        }
        assert_eq!(results(seen, 7)[0]["kind"], "error", "{who}");
    }
    assert!(results(&sa, 5)[0].get("streaming").is_some(), "a's Status");
    assert!(results(&sb, 5)[0].get("streaming").is_none(), "b's View");
    assert!(results(&sa, 200).is_empty() && results(&sb, 100).is_empty(), "never the other's");
    let (set_a, set_b) = (settings(&sa), settings(&sb));
    let own = |s: &[(u64, String)]| s.iter().filter(|(id, _)| *id == 6).map(|(_, n)| n.clone()).collect::<Vec<_>>();
    let (own_a, own_b) = (own(&set_a), own(&set_b));
    assert_eq!((own_a.len(), own_b.len()), (1, 1), "{set_a:?} / {set_b:?}");
    assert_ne!(own_a, own_b);
    assert!(set_a.contains(&(VIEW_CHANGED_SETTING_ID, own_b[0].clone())), "a sees b's, view-only: {set_a:?}");
    assert!(set_b.contains(&(VIEW_CHANGED_SETTING_ID, own_a[0].clone())), "b sees a's, view-only: {set_b:?}");

    drop((a, b));
    stop(d, &sock).await;
}

/// `/resume` switches the session for everyone: the asker gets `Resumed`
/// with its id, an older reader gets it view-only. The asker's mirror no
/// longer matches the digest that follows; its transport's resync (the
/// transport-internal id) repairs it and reaches no one else. An older
/// client sends that same reserved id itself and still gets its answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_resume_is_view_only_for_others_and_the_digest_resync_is_its_askers() {
    let guard = HomeGuard::new();
    let d = boot(&guard).await;
    let sock = d.paths.sock.clone();

    // A conversation on disk to resume: one turn, then closed.
    let mut x = attach(&sock, Attach::create(config(&guard, true), AttachMode::Mirror)).await;
    let x_sid = x.session_id().clone();
    x.send(SessionCommand::Submit { text: "conversation x".into(), attachments: vec![] }).await.unwrap();
    until(&mut x, |e| matches!(e, SessionEventWire::Idle)).await;
    let journal_x = x.meta().journal_id.clone();
    drop(x);
    SocketTransport::close(&sock, &x_sid, false, true).await.expect("x closed");

    let mut a = attach(&sock, Attach::create(config(&guard, true), AttachMode::Mirror)).await;
    let sid = a.session_id().clone();
    let mut old = Raw::attach(&sock, &sid).await;
    a.send(SessionCommand::Resume { id: 4, query: journal_x.clone() }).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Conversation(c) if c.messages_len > 0)).await;
    assert!(
        seen.iter().any(|e| matches!(e, SessionEventWire::Resumed { id: 4, new_id, .. } if *new_id == journal_x)),
        "{seen:#?}"
    );
    let Some(SessionEventWire::Conversation(c)) = seen.last() else { unreachable!() };
    assert_eq!(c.api_messages.len(), c.messages_len, "the mirror was repaired");
    assert!(serde_json::to_string(&c.api_messages).unwrap().contains("conversation x"), "{:?}", c.api_messages);

    let theirs = old.drain().await;
    let resumed: Vec<u64> = theirs
        .iter()
        .filter_map(|e| match e {
            WireSessionEvent::Resumed { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(resumed, vec![VIEW_CHANGED_SETTING_ID], "the other reader's copy is view-only: {theirs:#?}");
    assert!(
        !theirs.iter().any(|e| matches!(e, WireSessionEvent::QueryResult { .. })),
        "neither the asker's resync nor any other reply: {theirs:#?}"
    );

    // The older client's own resync, under the same reserved id.
    old.send(SessionCommand::Query { id: DIGEST_RESYNC_QUERY_ID, query: SessionQuery::Messages }).await;
    let frames = old.until(|f| matches!(f, DaemonFrame::Event(env) if matches!(env.event, WireSessionEvent::QueryResult { .. }))).await;
    match wire_events(frames).pop() {
        Some(WireSessionEvent::QueryResult { id, value }) => {
            assert_eq!(id, DIGEST_RESYNC_QUERY_ID);
            assert_eq!(value.as_array().map(Vec::len), Some(c.messages_len), "{value}");
        }
        other => panic!("{other:?}"),
    }
    drop(a);
    stop(d, &sock).await;
}

/// `daemon close` (a host caller) reads its own reply; an attached reader
/// sees the session end, never the host's `QueryResult`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_host_reply_never_reaches_a_client() {
    let guard = HomeGuard::new();
    let d = boot(&guard).await;
    let sock = d.paths.sock.clone();
    let a = attach(&sock, Attach::create(config(&guard, false), AttachMode::Mirror)).await;
    let sid = a.session_id().clone();
    let mut old = Raw::attach(&sock, &sid).await;
    let text = SocketTransport::close(&sock, &sid, false, true).await.expect("the host got its reply");
    assert!(text.contains("closing session"), "{text}");
    let seen = wire_events(old.until(|f| matches!(f, DaemonFrame::Event(env) if matches!(env.event, WireSessionEvent::Ended { .. }))).await);
    assert!(!seen.iter().any(|e| matches!(e, WireSessionEvent::QueryResult { .. })), "{seen:#?}");
    drop(a);
    stop(d, &sock).await;
}
