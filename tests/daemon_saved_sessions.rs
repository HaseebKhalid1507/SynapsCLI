//! `ClientFrame::SavedSessions`: a thin client lists the saved sessions on
//! disk through the daemon (named ones included, held or not), one row per
//! conversation, and resumes one by name with `Attach::Create{continue}`.
//! Real host, real UDS (a named pipe on Windows), a seeded throwaway HOME.

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::Path;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use std::sync::Arc;
use std::time::Duration;

use agent_core::session::Session;
use agent_engine::daemon::{Daemon, DaemonOpts};
use agent_engine::session::socket_transport::{SavedSessions, SocketTransport};
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts};
use chrono::Utc;
use phase2::{spawn_stub, HomeGuard, Script, ANTHROPIC_SSE};
use serial_test::serial;

const ALPHA: &str = "20261101-100000-aaaa";
const BRAVO: &str = "20261101-090000-bbbb";
const GAMMA_OLD: &str = "20261101-060000-cc01";
const GAMMA: &str = "20261101-080000-cc02";
const DELTA: &str = "20261101-070000-dddd";
const SHORT: &str = "20261101-050000-eeee";

/// Write `sessions/<id>.json` the way a saved session looks on disk.
fn seed(dir: &Path, id: &str, name: Option<&str>, title: &str, hours_ago: i64, msgs: usize, into: Option<&str>) {
    let mut s = Session::new("claude-sonnet-4-5", "medium", None);
    s.id = id.into();
    s.name = name.map(str::to_string);
    s.title = title.into();
    s.updated_at = Utc::now() - chrono::Duration::hours(hours_ago);
    s.created_at = s.updated_at - chrono::Duration::minutes(30);
    s.api_messages = (0..msgs)
        .map(|i| {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            Arc::new(serde_json::json!({"role": role, "content": format!("m{i}")}))
        })
        .collect();
    s.message_count = msgs;
    s.compacted_into = into.map(str::to_string);
    std::fs::write(dir.join(format!("{id}.json")), serde_json::to_vec(&s).unwrap()).unwrap();
}

fn seed_home(guard: &HomeGuard) {
    let dir = guard.base_dir().join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    seed(&dir, ALPHA, Some("alpha"), "Alpha work", 1, 6, None);
    seed(&dir, BRAVO, None, "Bravo, unnamed", 2, 6, None);
    // A LinkedSuccessor chain: the name moved to the head, which holds only
    // the summary (2 messages) and is still listed.
    seed(&dir, GAMMA_OLD, None, "Gamma", 6, 40, Some(GAMMA));
    seed(&dir, GAMMA, Some("gamma"), "↳ Gamma", 3, 2, None);
    seed(&dir, DELTA, Some("delta"), "Delta", 4, 6, None);
    // Noise the Past tab hides: under 3 messages, and a junk id.
    seed(&dir, SHORT, None, "short", 5, 1, None);
    seed(&dir, "scratch", Some("junk"), "junk", 0, 9, None);
    let cwd = guard.home.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let line = serde_json::json!({"session_id": ALPHA, "event": "start", "cwd": cwd});
    std::fs::write(dir.join("index.jsonl"), format!("{line}\n")).unwrap();
}

async fn start(guard: &HomeGuard) -> Daemon {
    let (url, _hits, _bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", url);
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

async fn list(sock: &Path, named_only: bool, limit: Option<u32>) -> SavedSessions {
    tokio::time::timeout(Duration::from_secs(20), SocketTransport::saved_sessions(sock, named_only, limit))
        .await
        .expect("timely")
        .expect("saved sessions")
}

fn ids(l: &SavedSessions) -> Vec<&str> {
    l.sessions.iter().map(|s| s.id.as_str()).collect()
}

/// Ids in list order, without `skip` (the held session, whose
/// `updated_at` a resume may move).
fn order_without<'a>(l: &'a SavedSessions, skip: &str) -> Vec<&'a str> {
    ids(l).into_iter().filter(|id| *id != skip).collect()
}

fn assert_newest_first(l: &SavedSessions) {
    assert!(l.sessions.windows(2).all(|w| w[0].updated_at >= w[1].updated_at), "{:?}", ids(l));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn saved_sessions_lists_conversations_and_a_name_resumes_through_the_daemon() {
    let guard = HomeGuard::new();
    seed_home(&guard);
    let d = start(&guard).await;
    let sock = d.paths.sock.clone();

    // The feature is advertised, so a client may ask.
    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    assert!(conn.welcome.supports(FEATURE_SAVED_SESSIONS), "{:?}", conn.welcome.features);
    assert!(conn.welcome.features.iter().any(|f| f == "saved_sessions"));
    conn.bye().await;

    // Nothing held yet: one row per conversation, noise and junk out.
    let all = list(&sock, false, None).await;
    assert!(!all.truncated);
    assert_eq!(ids(&all), [ALPHA, BRAVO, GAMMA, DELTA]);
    assert_newest_first(&all);
    assert!(all.sessions.iter().all(|s| !s.live));
    let gamma = all.sessions.iter().find(|s| s.id == GAMMA).unwrap();
    assert_eq!(gamma.name.as_deref(), Some("gamma"));
    assert_eq!(gamma.message_count, Some(2));
    assert!(gamma.created_at < gamma.updated_at - chrono::Duration::hours(2), "the chain's start, not the head's");
    let alpha = &all.sessions[0];
    assert_eq!(alpha.title, "Alpha work");
    assert_eq!(alpha.model, "claude-sonnet-4-5");
    assert_eq!(alpha.cwd.as_deref(), Some(guard.home.path().join("proj").as_path()));

    // Resume by NAME over the daemon path (`resolve_session`: chain →
    // name → id): the held session is that journal.
    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    let config = SessionConfig {
        cwd: Some(guard.home.path().to_path_buf()),
        env: None,
        persist: true,
        continue_session: Some(Some("delta".into())),
        ..Default::default()
    };
    let (t, snap) = SocketTransport::attach(conn, Attach::create(config, AttachMode::Mirror)).await.expect("continue by name");
    assert_eq!(snap.meta.journal_id, DELTA, "`delta` resolved to its saved session");
    assert_eq!(snap.meta.name.as_deref(), Some("delta"));
    assert!(snap.conversation.messages_len >= 6, "history restored: {}", snap.conversation.messages_len);

    // It is now held: `live`, matched by journal id.
    let all = list(&sock, false, None).await;
    assert_eq!(all.sessions.len(), 4, "{:?}", ids(&all));
    assert_newest_first(&all);
    assert_eq!(order_without(&all, DELTA), [ALPHA, BRAVO, GAMMA]);
    for s in &all.sessions {
        assert_eq!(s.live, s.id == DELTA, "{}", s.id);
    }

    // named_only: unnamed BRAVO goes; the chain counts by its head's name.
    let named = list(&sock, true, None).await;
    assert_eq!(order_without(&named, DELTA), [ALPHA, GAMMA]);
    assert!(named.sessions.iter().any(|s| s.id == DELTA && s.live));
    assert!(named.sessions.iter().all(|s| s.name.is_some()));

    // limit / truncated.
    let two = list(&sock, false, Some(2)).await;
    assert_eq!(two.sessions.len(), 2);
    assert!(two.truncated);
    // DELTA, held, may have been saved (moved up) since `all`.
    assert!(order_without(&all, DELTA).starts_with(&order_without(&two, DELTA)), "{:?}", ids(&two));
    let exact = list(&sock, false, Some(4)).await;
    assert!(!exact.truncated, "exactly `limit` matched");
    let named_two = list(&sock, true, Some(2)).await;
    assert!(named_two.truncated && named_two.sessions.len() == 2);

    drop(t);
    SocketTransport::shutdown(&sock, false).await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), d.wait()).await.expect("daemon wait");
}

/// A raw client, to drive the frames by hand.
struct Raw {
    r: BufReader<agent_engine::ipc::ClientReadHalf>,
    w: agent_engine::ipc::ClientWriteHalf,
}

impl Raw {
    async fn hello(p: &Path) -> Self {
        let (r, w) = agent_engine::ipc::split_client(agent_engine::ipc::connect(p).await.unwrap());
        let mut c = Self { r: BufReader::new(r), w };
        c.send(&ClientFrame::Hello(Hello::new(ClientKind::Test))).await;
        assert!(matches!(c.recv().await, Some(DaemonFrame::Welcome(_))));
        c
    }
    async fn send(&mut self, f: &ClientFrame) {
        self.w.write_all(encode_line(f).unwrap().as_bytes()).await.unwrap();
    }
    /// `None` = EOF.
    async fn recv(&mut self) -> Option<DaemonFrame> {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(10), self.r.read_line(&mut line)).await.expect("timely").unwrap();
        (n > 0).then(|| decode_line(line.trim_end()).unwrap())
    }
    /// The next frame that is not a session event.
    async fn recv_reply(&mut self) -> Option<DaemonFrame> {
        loop {
            match self.recv().await {
                Some(DaemonFrame::Event(_)) => continue,
                other => return other,
            }
        }
    }
}

async fn wait_requests(d: &Daemon, n: usize) {
    let t0 = std::time::Instant::now();
    while d.state.saved.requests() < n {
        assert!(t0.elapsed() < Duration::from_secs(10), "{} of {n} requests reached the daemon", d.state.saved.requests());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Many clients listing at once cost the daemon ONE catalog load (one
/// blocking thread, no pool); each gets the full answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn concurrent_listings_share_one_catalog_load() {
    let guard = HomeGuard::new();
    seed_home(&guard);
    let d = start(&guard).await;
    let sock = d.paths.sock.clone();
    let held = d.state.saved.hold().await;
    let n = 12;
    let jobs: Vec<_> = (0..n)
        .map(|i| {
            let sock = sock.clone();
            tokio::spawn(async move { list(&sock, i % 2 == 0, None).await })
        })
        .collect();
    wait_requests(&d, n).await;
    assert_eq!(d.state.saved.loads(), 0);
    drop(held);
    for (i, j) in jobs.into_iter().enumerate() {
        let l = j.await.unwrap();
        let want: &[&str] = if i % 2 == 0 { &[ALPHA, GAMMA, DELTA] } else { &[ALPHA, BRAVO, GAMMA, DELTA] };
        assert_eq!(ids(&l), want, "request {i}");
    }
    assert_eq!(d.state.saved.loads(), 1, "{n} concurrent requests, one load");
    d.shutdown_token().cancel();
    tokio::time::timeout(Duration::from_secs(20), d.wait()).await.expect("daemon wait");
}

/// On an attached connection the listing is answered, and a slow one
/// (the loader held) blocks nothing: the client's frames are still read
/// (`Ping` → `Pong`) and a reload still says `Bye{Reloading}` at once,
/// with no listing after it. A control connection waiting on a listing
/// gets its `Bye{Reloading}` too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_slow_listing_never_holds_up_commands_or_a_reload() {
    let guard = HomeGuard::new();
    seed_home(&guard);
    let d = start(&guard).await;
    let sock = d.paths.sock.clone();

    let mut a = Raw::hello(&sock).await;
    let config = SessionConfig { cwd: Some(guard.home.path().to_path_buf()), env: None, persist: false, ..Default::default() };
    a.send(&ClientFrame::Attach(Attach::create(config, AttachMode::Mirror))).await;
    loop {
        match a.recv().await.expect("attached") {
            DaemonFrame::Attached(_) => break,
            DaemonFrame::Event(_) => {}
            other => panic!("{other:?}"),
        }
    }
    a.send(&ClientFrame::SavedSessions { named_only: true, limit: Some(2) }).await;
    match a.recv_reply().await {
        Some(DaemonFrame::SavedSessionList { sessions, truncated }) => {
            assert_eq!(sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), [ALPHA, GAMMA]);
            assert!(truncated);
        }
        other => panic!("{other:?}"),
    }

    let held = d.state.saved.hold().await;
    let before = d.state.saved.requests();
    a.send(&ClientFrame::SavedSessions { named_only: false, limit: None }).await;
    let mut c = Raw::hello(&sock).await;
    c.send(&ClientFrame::SavedSessions { named_only: false, limit: None }).await;
    wait_requests(&d, before + 2).await;
    a.send(&ClientFrame::Ping).await;
    assert!(matches!(a.recv_reply().await, Some(DaemonFrame::Pong { .. })), "frames are read while the catalog loads");

    let t0 = std::time::Instant::now();
    d.state.fire_announce();
    for (who, conn) in [("attached", &mut a), ("control", &mut c)] {
        match conn.recv_reply().await {
            Some(DaemonFrame::Bye { reason: Some(ByeReason::Reloading { .. }) }) => {}
            other => panic!("{who}: {other:?}"),
        }
        let rest = conn.recv().await;
        assert!(rest.is_none(), "{who}: nothing after the Bye: {rest:?}");
    }
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    drop(held);
    d.state.reset_announce();
    d.shutdown_token().cancel();
    tokio::time::timeout(Duration::from_secs(20), d.wait()).await.expect("daemon wait");
}
