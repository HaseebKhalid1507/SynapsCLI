//! `ClientFrame::CatalogPage`: a thin client pages through the saved
//! sessions (the conversations on disk this daemon does not hold), one row
//! per conversation, and resumes one with `Attach::Create{continue}`.
//! Held and saved never overlap and together cover every listable
//! conversation, through `/clear`, a resume, an end, a cold restart and a
//! reload. Real host, real UDS (a named pipe on Windows), a seeded
//! throwaway HOME; the restart and reload tests run the real binary.

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use agent_core::session::Session;
use agent_engine::daemon::{registry, Daemon, DaemonOpts};
use agent_engine::session::socket_transport::{SavedPage, SocketTransport};
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
#[allow(clippy::too_many_arguments)]
fn seed(dir: &Path, id: &str, name: Option<&str>, title: &str, hours_ago: i64, msgs: usize, into: Option<&str>, parent: Option<&str>) {
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
    s.parent_session = parent.map(str::to_string);
    std::fs::write(dir.join(format!("{id}.json")), serde_json::to_vec(&s).unwrap()).unwrap();
}

fn seed_home(guard: &HomeGuard) {
    let dir = guard.base_dir().join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    seed(&dir, ALPHA, Some("alpha"), "Alpha work", 1, 6, None, None);
    seed(&dir, BRAVO, None, "Bravo, unnamed", 2, 6, None, None);
    // A LinkedSuccessor chain: the name moved to the head, which holds ONE
    // message (the summary) and is still listed: it is chained.
    seed(&dir, GAMMA_OLD, None, "Gamma", 6, 40, Some(GAMMA), None);
    seed(&dir, GAMMA, Some("gamma"), "↳ Gamma", 3, 1, None, Some(GAMMA_OLD));
    seed(&dir, DELTA, Some("delta"), "Delta", 4, 6, None, None);
    // Noise: under 3 messages and not chained, and a junk id.
    seed(&dir, SHORT, None, "short", 5, 1, None, None);
    seed(&dir, "scratch", Some("junk"), "junk", 0, 9, None, None);
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

async fn stop(d: Daemon) {
    d.shutdown_token().cancel();
    tokio::time::timeout(Duration::from_secs(20), d.wait()).await.expect("daemon wait");
}

async fn page(sock: &Path, before: Option<CatalogCursor>, limit: Option<u32>, named_only: bool) -> SavedPage {
    tokio::time::timeout(Duration::from_secs(20), SocketTransport::catalog_page(sock, before, limit, named_only))
        .await
        .expect("timely")
        .expect("catalog page")
}

/// Every saved row, `limit` at a time.
async fn all_pages(sock: &Path, limit: u32, named_only: bool) -> Vec<SessionMeta> {
    let (mut rows, mut before) = (Vec::new(), None);
    loop {
        let p = page(sock, before, Some(limit), named_only).await;
        assert!(p.sessions.len() <= limit as usize);
        rows.extend(p.sessions);
        match p.next {
            Some(c) => before = Some(c),
            None => return rows,
        }
    }
}

fn ids(rows: &[SessionMeta]) -> Vec<&str> {
    rows.iter().map(|s| s.journal_id.as_str()).collect()
}

/// Wait until page 1 (limit 200) satisfies `ok`.
async fn page_until(sock: &Path, what: &str, ok: impl Fn(&[SessionMeta]) -> bool) -> Vec<SessionMeta> {
    let t0 = Instant::now();
    loop {
        let rows = page(sock, None, Some(200), false).await.sessions;
        if ok(&rows) {
            return rows;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "{what}: {:?}", ids(&rows));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn config(cwd: &Path) -> SessionConfig {
    SessionConfig {
        cwd: Some(cwd.to_path_buf()),
        env: None,
        model_override: Some("claude-sonnet-4-5".into()),
        persist: true,
        ..Default::default()
    }
}

async fn create(sock: &Path, config: SessionConfig) -> (SocketTransport, AttachSnapshot) {
    let conn = SocketTransport::connect(sock, Hello::new(ClientKind::Test)).await.unwrap();
    SocketTransport::attach(conn, Attach::create(config, AttachMode::Mirror)).await.expect("attach create")
}

async fn next_event(t: &mut SocketTransport) -> SessionEventWire {
    tokio::time::timeout(Duration::from_secs(20), t.next_event()).await.expect("timely").expect("open").event
}

async fn run_turn(t: &mut SocketTransport, text: &str) {
    t.send_from_self(SessionCommand::Submit { text: text.into(), attachments: vec![] }).await.unwrap();
    loop {
        match next_event(t).await {
            SessionEventWire::Idle => return,
            SessionEventWire::Refused { reason, .. } => panic!("refused: {reason}"),
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn pages_list_saved_conversations_and_a_resumed_one_leaves_them() {
    let guard = HomeGuard::new();
    seed_home(&guard);
    let d = start(&guard).await;
    let sock = d.paths.sock.clone();

    // The feature is advertised, so a client may ask.
    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    assert!(conn.welcome.supports(FEATURE_SESSION_PAGE), "{:?}", conn.welcome.features);
    conn.bye().await;

    // Nothing held yet: one row per conversation, noise and junk out; the
    // compacted head with one message is listed (chained).
    let first = page(&sock, None, None, false).await;
    assert_eq!(first.next, None);
    assert_eq!(ids(&first.sessions), [ALPHA, BRAVO, GAMMA, DELTA]);
    assert!(first.sessions.windows(2).all(|w| w[0].updated_at > w[1].updated_at));
    for s in &first.sessions {
        assert_eq!(s.lifecycle, SessionLifecycle::Saved);
        assert_eq!(s.id.as_str(), s.journal_id, "a saved row's id is its journal id");
        assert_eq!(s.host_pid, 0);
    }
    let gamma = &first.sessions[2];
    assert_eq!(gamma.name.as_deref(), Some("gamma"));
    assert_eq!(gamma.messages, 1);
    assert!(gamma.created_at < gamma.updated_at.unwrap() - chrono::Duration::hours(2), "the chain's start");
    let alpha = &first.sessions[0];
    assert_eq!(alpha.title.as_deref(), Some("Alpha work"));
    assert_eq!(alpha.model, "claude-sonnet-4-5");
    assert_eq!(alpha.cwd.as_deref(), Some(guard.home.path().join("proj").as_path()));

    // Paged two at a time: the same rows, once each.
    assert_eq!(ids(&all_pages(&sock, 2, false).await), [ALPHA, BRAVO, GAMMA, DELTA]);
    assert_eq!(ids(&all_pages(&sock, 1, true).await), [ALPHA, GAMMA, DELTA]);

    // Resume by NAME (`resolve_session`: chain → name → id): the held
    // session is that journal, and it leaves the pages.
    let config = SessionConfig { continue_session: Some(Some("delta".into())), ..config(guard.home.path()) };
    let (t, snap) = create(&sock, config).await;
    assert_eq!(snap.meta.journal_id, DELTA, "`delta` resolved to its saved session");
    assert_eq!(snap.meta.name.as_deref(), Some("delta"));
    assert!(snap.conversation.messages_len >= 6, "history restored: {}", snap.conversation.messages_len);
    assert_eq!(ids(&page(&sock, None, None, false).await.sessions), [ALPHA, BRAVO, GAMMA]);
    assert_eq!(ids(&all_pages(&sock, 1, false).await), [ALPHA, BRAVO, GAMMA]);
    let held = SocketTransport::sessions(&sock).await.unwrap();
    assert_eq!(held.iter().map(|m| m.journal_id.as_str()).collect::<Vec<_>>(), [DELTA]);
    assert!(held.iter().all(|m| m.lifecycle != SessionLifecycle::Saved && m.updated_at.is_none()));

    // Resume a saved row by its id, as a client does from a page.
    let config = SessionConfig { continue_session: Some(Some(BRAVO.into())), ..self::config(guard.home.path()) };
    let (t2, snap) = create(&sock, config).await;
    assert_eq!(snap.meta.journal_id, BRAVO);
    assert_eq!(ids(&page(&sock, None, None, false).await.sessions), [ALPHA, GAMMA]);

    drop((t, t2));
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
    async fn page(&mut self, req: u64) {
        self.send(&ClientFrame::CatalogPage { req, before: None, limit: None, named_only: false }).await;
    }
    /// `None` = EOF.
    async fn recv(&mut self) -> Option<DaemonFrame> {
        self.recv_within(Duration::from_secs(10)).await.expect("timely")
    }
    /// `Err` = nothing within `budget`.
    async fn recv_within(&mut self, budget: Duration) -> Result<Option<DaemonFrame>, ()> {
        let mut line = String::new();
        match tokio::time::timeout(budget, self.r.read_line(&mut line)).await {
            Ok(n) => Ok((n.unwrap() > 0).then(|| decode_line(line.trim_end()).unwrap())),
            Err(_) => Err(()),
        }
    }
}

async fn wait_requests(d: &Daemon, n: usize) {
    let t0 = Instant::now();
    while d.state.saved.requests() < n {
        assert!(t0.elapsed() < Duration::from_secs(10), "{} of {n} requests reached the daemon", d.state.saved.requests());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Many clients asking at once cost the daemon ONE catalog load (one
/// blocking thread, no pool); each gets the full answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn concurrent_pages_share_one_catalog_load() {
    let guard = HomeGuard::new();
    seed_home(&guard);
    let d = start(&guard).await;
    let sock = d.paths.sock.clone();
    let held = d.state.saved.hold().await;
    let n = 12;
    let jobs: Vec<_> = (0..n)
        .map(|i| {
            let sock = sock.clone();
            tokio::spawn(async move { page(&sock, None, None, i % 2 == 0).await })
        })
        .collect();
    wait_requests(&d, n).await;
    assert_eq!(d.state.saved.loads(), 0);
    drop(held);
    for (i, j) in jobs.into_iter().enumerate() {
        let p = j.await.unwrap();
        let want: &[&str] = if i % 2 == 0 { &[ALPHA, GAMMA, DELTA] } else { &[ALPHA, BRAVO, GAMMA, DELTA] };
        assert_eq!(ids(&p.sessions), want, "request {i}");
    }
    assert_eq!(d.state.saved.loads(), 1, "{n} concurrent requests, one load");
    stop(d).await;
}

/// A page whose load is held open for 2 s blocks nothing on its own
/// connection: `Sessions` and `Ping` are answered at once, and so is a
/// `Reload` (refused here: a bad exe). An announced reload says
/// `Bye{Reloading}` at once, with no page after it. The reply carries its
/// `req` whenever it comes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_held_page_never_holds_up_sessions_ping_or_a_reload() {
    let guard = HomeGuard::new();
    seed_home(&guard);
    let d = start(&guard).await;
    let sock = d.paths.sock.clone();
    let quick = Duration::from_millis(1500);

    let held = d.state.saved.hold().await;
    let t0 = Instant::now();
    let before = d.state.saved.requests();
    let mut a = Raw::hello(&sock).await;
    a.page(41).await;
    let mut b = Raw::hello(&sock).await;
    b.page(42).await;
    let mut c = Raw::hello(&sock).await;
    c.page(43).await;
    wait_requests(&d, before + 3).await;

    a.send(&ClientFrame::Sessions).await;
    assert!(matches!(a.recv_within(quick).await, Ok(Some(DaemonFrame::SessionList { .. }))), "Sessions while a page loads");
    a.send(&ClientFrame::Ping).await;
    assert!(matches!(a.recv_within(quick).await, Ok(Some(DaemonFrame::Pong { .. }))), "Ping while a page loads");
    b.send(&ClientFrame::Reload { now: true, drain_secs: None, exe: Some(guard.home.path().join("no-such-synaps")) }).await;
    match b.recv_within(quick).await {
        Ok(Some(DaemonFrame::Refused { reason: RefuseReason::ReloadRefused { .. }, .. })) => {}
        other => panic!("Reload while a page loads: {other:?}"),
    }

    // Hold the load for 2 s in all, then let it go: the page arrives.
    tokio::time::sleep(Duration::from_secs(2).saturating_sub(t0.elapsed())).await;
    a.send(&ClientFrame::Ping).await;
    assert!(matches!(a.recv_within(quick).await, Ok(Some(DaemonFrame::Pong { .. }))), "still answering at 2 s");
    drop(held);
    match a.recv().await {
        Some(DaemonFrame::CatalogPage { req: 41, sessions, next: None, error: None }) => {
            assert_eq!(ids(&sessions), [ALPHA, BRAVO, GAMMA, DELTA]);
        }
        other => panic!("{other:?}"),
    }

    // An announced reload with a page in flight: `Bye` at once, nothing after.
    let held = d.state.saved.hold().await;
    c.page(44).await;
    let t1 = Instant::now();
    d.state.fire_announce();
    // `c`'s first page (43) may have been served in the window above.
    loop {
        match c.recv().await {
            Some(DaemonFrame::CatalogPage { req: 43, .. }) => continue,
            Some(DaemonFrame::Bye { reason: Some(ByeReason::Reloading { .. }) }) => break,
            other => panic!("{other:?}"),
        }
    }
    let rest = c.recv().await;
    assert!(rest.is_none(), "nothing after the Bye: {rest:?}");
    assert!(t1.elapsed() < Duration::from_secs(5), "{:?}", t1.elapsed());
    drop(held);
    d.state.reset_announce();
    stop(d).await;
}

/// `CATALOG_PAGES_IN_FLIGHT` per connection: a fifth page is answered at
/// once with `error`; the four in flight complete, and the connection
/// takes pages again after.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_fifth_page_in_flight_gets_an_error() {
    let guard = HomeGuard::new();
    seed_home(&guard);
    let d = start(&guard).await;
    let sock = d.paths.sock.clone();
    let held = d.state.saved.hold().await;
    let before = d.state.saved.requests();
    let mut a = Raw::hello(&sock).await;
    for req in 1..=4 {
        a.page(req).await;
    }
    wait_requests(&d, before + 4).await;
    a.page(5).await;
    match a.recv_within(Duration::from_secs(2)).await {
        Ok(Some(DaemonFrame::CatalogPage { req: 5, sessions, next: None, error: Some(e) })) => {
            assert!(sessions.is_empty());
            assert!(e.contains("too many catalog pages in flight"), "{e}");
        }
        other => panic!("{other:?}"),
    }
    // Another connection is not limited by this one.
    let mut b = Raw::hello(&sock).await;
    b.page(9).await;
    drop(held);
    let mut got = Vec::new();
    for _ in 1..=4 {
        match a.recv().await {
            Some(DaemonFrame::CatalogPage { req, error: None, sessions, .. }) => {
                assert_eq!(sessions.len(), 4);
                got.push(req);
            }
            other => panic!("{other:?}"),
        }
    }
    got.sort();
    assert_eq!(got, [1, 2, 3, 4]);
    assert!(matches!(b.recv().await, Some(DaemonFrame::CatalogPage { req: 9, error: None, .. })));
    a.page(6).await;
    assert!(matches!(a.recv().await, Some(DaemonFrame::CatalogPage { req: 6, error: None, .. })));
    stop(d).await;
}

/// A page still loading when the connection attaches is dropped: the
/// attach stream never carries a `CatalogPage`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_page_in_flight_never_reaches_an_attach_stream() {
    let guard = HomeGuard::new();
    seed_home(&guard);
    let d = start(&guard).await;
    let sock = d.paths.sock.clone();
    let held = d.state.saved.hold().await;
    let before = d.state.saved.requests();
    let mut a = Raw::hello(&sock).await;
    a.page(1).await;
    wait_requests(&d, before + 1).await;
    a.send(&ClientFrame::Attach(Attach::create(config(guard.home.path()), AttachMode::Mirror))).await;
    loop {
        match a.recv().await.expect("attached") {
            DaemonFrame::Attached(_) => break,
            DaemonFrame::Event(_) => {}
            other => panic!("{other:?}"),
        }
    }
    drop(held);
    // The load runs now (a fresh request shares nothing with the dropped one).
    page(&sock, None, None, false).await;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(500) {
        match a.recv_within(Duration::from_millis(100)).await {
            Ok(Some(DaemonFrame::Event(_))) | Err(()) => {}
            other => panic!("on the attach stream: {other:?}"),
        }
    }
    a.send(&ClientFrame::Bye).await;
    stop(d).await;
}

/// `/clear` leaves actor `A` on a new conversation `N`: `Sessions` lists
/// the actor with journal `N`, and the old conversation `A` is saved,
/// listed once. A page never lists `N` (held), nor anything twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn after_clear_the_old_conversation_is_listed_once_and_sessions_has_the_new() {
    let guard = HomeGuard::new();
    let d = start(&guard).await;
    let sock = d.paths.sock.clone();
    let (mut t, _snap) = create(&sock, config(guard.home.path())).await;
    let a = t.session_id().clone();
    run_turn(&mut t, "conversation A, first").await;
    run_turn(&mut t, "conversation A, second").await;
    assert!(page(&sock, None, None, false).await.sessions.is_empty(), "held, so not saved");

    t.send_from_self(SessionCommand::NewSession).await.unwrap();
    let n = loop {
        if let SessionEventWire::Cleared { session_id } = next_event(&mut t).await {
            break session_id;
        }
    };
    assert_ne!(n, a.as_str());
    let rows = page_until(&sock, "A is saved", |r| r.iter().any(|m| m.journal_id == a.as_str())).await;
    assert_eq!(ids(&rows), [a.as_str()], "listed once, N not at all");
    assert_eq!(rows[0].messages, 4);
    let held = SocketTransport::sessions(&sock).await.unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].id, a, "the actor keeps its id");
    assert_eq!(held[0].journal_id, n, "and holds N");

    // A turn on N puts it on disk; it is still held, still not listed.
    run_turn(&mut t, "conversation N").await;
    assert_eq!(ids(&page(&sock, None, None, false).await.sessions), [a.as_str()]);
    drop(t);
    stop(d).await;
}

/// A session that never took a turn has nothing on disk: never listed,
/// neither while held nor after it ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_zero_turn_session_is_never_listed() {
    let guard = HomeGuard::new();
    seed_home(&guard);
    let d = start(&guard).await;
    let sock = d.paths.sock.clone();
    let (t, snap) = create(&sock, config(guard.home.path())).await;
    let id = t.session_id().clone();
    let journal = snap.meta.journal_id.clone();
    let want = [ALPHA, BRAVO, GAMMA, DELTA];
    assert_eq!(ids(&page(&sock, None, None, false).await.sessions), want);
    drop(t);
    SocketTransport::close(&sock, &id, true, true).await.expect("closed");
    let t0 = Instant::now();
    while SocketTransport::sessions(&sock).await.unwrap().iter().any(|m| m.id == id) {
        assert!(t0.elapsed() < Duration::from_secs(10), "the session never left");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let rows = page(&sock, None, None, false).await.sessions;
    assert_eq!(ids(&rows), want, "{journal} is not listed");
    stop(d).await;
}

// ── the real binary: cold restart and reload ─────────────────────────────────

struct DaemonProc {
    child: Child,
    paths: registry::DaemonPaths,
}

impl Drop for DaemonProc {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(info) = registry::read_daemon_json(&self.paths) {
            if info.pid != self.child.id() && registry::is_alive(&self.paths) {
                let _ = Command::new("taskkill").args(["/PID", &info.pid.to_string(), "/T", "/F"]).output();
            }
        }
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
        .env("SYNAPS_DAEMON_PARK_GRACE_SECS", "0")
        .env_remove("SYNAPS_DAEMON_RELOAD_STATE")
        .env_remove("SYNAPS_DAEMON_LOCK_FD")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn daemon");
    let d = DaemonProc { child, paths };
    let t0 = Instant::now();
    while SocketTransport::ping(&d.paths.sock).await.is_err() {
        assert!(t0.elapsed() < Duration::from_secs(20), "daemon never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    d
}

/// Three held sessions, each listable once saved: one named with a single
/// turn, two unnamed with two turns. Returns their journal ids.
async fn three_sessions(sock: &Path, cwd: &Path) -> Vec<String> {
    let mut journals = Vec::new();
    for (i, name) in [Some("kept"), None, None].into_iter().enumerate() {
        let (mut t, snap) = create(sock, SessionConfig { name: name.map(Into::into), ..config(cwd) }).await;
        run_turn(&mut t, &format!("session {i}")).await;
        if name.is_none() {
            run_turn(&mut t, &format!("session {i}, again")).await;
        }
        journals.push(snap.meta.journal_id.clone());
        t.detach().await;
    }
    journals
}

/// A cold start holds nothing (no pins): every session the last image held
/// is saved now, and the first page lists each, once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn after_a_cold_restart_page_one_lists_every_formerly_held_session() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let mut d = spawn_daemon(&guard, &url).await;
    let journals = three_sessions(&d.paths.sock, guard.home.path()).await;
    let held: Vec<String> = SocketTransport::sessions(&d.paths.sock).await.unwrap().into_iter().map(|m| m.journal_id).collect();
    assert_eq!(held.len(), 3, "{held:?}");
    assert!(page(&d.paths.sock, None, None, false).await.sessions.is_empty(), "all held");

    SocketTransport::shutdown(&d.paths.sock, false).await.unwrap();
    let t0 = Instant::now();
    while d.child.try_wait().unwrap().is_none() {
        assert!(t0.elapsed() < Duration::from_secs(20), "daemon never stopped");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    drop(d);

    let d = spawn_daemon(&guard, &url).await;
    assert!(SocketTransport::sessions(&d.paths.sock).await.unwrap().is_empty(), "a cold start holds nothing");
    let mut listed: Vec<String> = page(&d.paths.sock, None, None, false).await.sessions.into_iter().map(|m| m.journal_id).collect();
    listed.sort();
    let mut want = journals;
    want.sort();
    assert_eq!(listed, want);
    SocketTransport::shutdown(&d.paths.sock, false).await.unwrap();
}

/// A session the new image cannot restore (its snapshot's body is broken,
/// its header still readable) is not held after the reload, so page 1
/// lists it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn after_a_reload_a_session_that_failed_to_restore_is_listed() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let d = spawn_daemon(&guard, &url).await;
    let sock = d.paths.sock.clone();
    let journals = three_sessions(&sock, guard.home.path()).await;
    let broken = journals[1].clone();
    // Parked: the conversation is on disk and out of memory, so the
    // reload's checkpoint does not write it again.
    let t0 = Instant::now();
    while !SocketTransport::sessions(&sock).await.unwrap().iter().all(|m| m.lifecycle == SessionLifecycle::Parked) {
        assert!(t0.elapsed() < Duration::from_secs(20), "sessions never parked");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let dir = guard.base_dir().join("sessions");
    let path = dir.join(format!("{broken}.json"));
    let text = std::fs::read_to_string(&path).unwrap();
    let cut = text.find("\"api_messages\"").expect("api_messages");
    std::fs::write(&path, format!("{}\"api_messages\":[{{\"role\":", &text[..cut])).unwrap();

    assert_eq!(SocketTransport::reload(&sock, true, None, None).await.expect("reload accepted"), 2);
    let t0 = Instant::now();
    while !(registry::read_daemon_json(&d.paths).is_some_and(|i| i.generation == 2) && SocketTransport::ping(&sock).await.is_ok()) {
        assert!(t0.elapsed() < Duration::from_secs(20), "new image never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let rows = page_until(&sock, "the broken session is saved", |r| r.iter().any(|m| m.journal_id == broken)).await;
    assert_eq!(ids(&rows), [broken.as_str()], "the restored ones are held");
    let held = SocketTransport::sessions(&sock).await.unwrap();
    assert!(held.iter().all(|m| m.journal_id != broken), "{held:?}");
    for j in [&journals[0], &journals[2]] {
        assert!(held.iter().any(|m| &m.journal_id == j), "{j} restored");
    }
    SocketTransport::shutdown(&sock, false).await.unwrap();
}
