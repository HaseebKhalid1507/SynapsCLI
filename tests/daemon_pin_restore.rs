//! Pinned sessions come back after the daemon restarts (the pin book,
//! `daemon::pins`). Real host, real UDS, loopback Anthropic stub; a restart
//! is `stop` + a fresh `Daemon::start` in the same config dir.

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::pins::{PinBook, PinEntry, MAX_ATTEMPTS};
use agent_engine::daemon::{Daemon, DaemonOpts};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts};
use phase2::{spawn_stub, HomeGuard, Script, ANTHROPIC_SSE};
use serial_test::serial;

async fn start(guard: &HomeGuard) -> Daemon {
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

async fn boot(guard: &HomeGuard) -> (Daemon, Arc<std::sync::atomic::AtomicUsize>) {
    std::env::set_var("SYNAPS_DAEMON_PARK_GRACE_SECS", "never");
    let (url, hits, _bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    (start(guard).await, hits)
}

async fn until(t: &mut SocketTransport, pred: impl Fn(&SessionEventWire) -> bool) {
    loop {
        let e = tokio::time::timeout(Duration::from_secs(10), t.next_event()).await.expect("timely").expect("open");
        if pred(&e.event) {
            return;
        }
    }
}

/// A persisted session that ran one turn (so it has a journal).
async fn session_with_turn(sock: &Path, cwd: &Path) -> (SocketTransport, SessionId) {
    let conn = SocketTransport::connect(sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (mut t, _snap) = SocketTransport::attach(
        conn,
        Attach::Create {
            config: SessionConfig {
                cwd: Some(cwd.to_path_buf()),
                model_override: Some("claude-sonnet-4-5".into()),
                ..Default::default()
            },
            mode: AttachMode::Mirror,
        },
    )
    .await
    .expect("attach create");
    t.send_from_self(SessionCommand::Submit { text: "hello".into(), attachments: vec![] }).await.unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    let sid = t.session_id().clone();
    (t, sid)
}

async fn listed(sock: &Path, sid: &str) -> Option<SessionMeta> {
    SocketTransport::sessions(sock)
        .await
        .unwrap_or_default()
        .into_iter()
        .find(|m| m.id.as_str() == sid || m.journal_id == sid)
}

/// Wait until `sid` is listed (restore runs after ready, in the background).
async fn wait_listed(sock: &Path, sid: &str, budget: Duration) -> Option<SessionMeta> {
    let t0 = std::time::Instant::now();
    loop {
        if let Some(m) = listed(sock, sid).await {
            return Some(m);
        }
        if t0.elapsed() > budget {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait for the cold-start restore to finish.
async fn restored(d: &Daemon) {
    tokio::time::sleep(Duration::from_millis(100)).await;
    let t0 = std::time::Instant::now();
    while d.state.restoring.load(Ordering::SeqCst) && t0.elapsed() < Duration::from_secs(20) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn stop(d: Daemon, sock: &Path) {
    SocketTransport::shutdown(sock, false).await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), d.wait()).await.expect("daemon wait");
}

fn book() -> PinBook {
    PinBook::for_active_profile()
}

fn entry(book: &PinBook, id: &str) -> Option<PinEntry> {
    book.load().into_iter().find(|e| e.journal_id == id)
}

fn seed(book: &PinBook, e: PinEntry) {
    book.update(|v| v.push(e)).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_pinned_session_comes_back_pinned_with_its_conversation_after_a_stop() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard).await;
    let sock = d.paths.sock.clone();
    let (t, sid) = session_with_turn(&sock, guard.home.path()).await;
    assert!(entry(&book(), sid.as_str()).is_none(), "unpinned: not in the book");
    assert!(SocketTransport::pin(&sock, &sid, true).await.unwrap());
    let e = entry(&book(), sid.as_str()).expect("pinning writes the book");
    assert_eq!(e.cwd.as_deref(), Some(guard.home.path()));
    let before = listed(&sock, sid.as_str()).await.unwrap().messages;
    assert!(before >= 2, "a turn ran: {before}");
    t.detach().await;
    stop(d, &sock).await;
    assert!(entry(&book(), sid.as_str()).is_some(), "a stop keeps the pin");

    let d = start(&guard).await;
    let m = wait_listed(&sock, sid.as_str(), Duration::from_secs(20)).await.expect("restored on start");
    assert!(m.pinned, "and pinned");
    // The listing's message count is published on the first attach.
    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (w, _snap) =
        SocketTransport::attach(conn, Attach::Existing { session_id: m.id.clone(), mode: AttachMode::Observe })
            .await
            .expect("attach the restored session");
    assert_eq!(listed(&sock, sid.as_str()).await.unwrap().messages, before, "conversation intact");
    w.detach().await;
    restored(&d).await;
    let e = entry(&book(), sid.as_str()).expect("still in the book");
    assert_eq!((e.state, e.attempts), (None, 0), "restored: no state, marker cleared");

    // Unpin: the entry goes; the next start brings nothing back.
    assert!(!SocketTransport::pin(&sock, &sid, false).await.unwrap());
    assert!(entry(&book(), sid.as_str()).is_none(), "unpinning drops the entry");
    stop(d, &sock).await;
    let d = start(&guard).await;
    restored(&d).await;
    assert!(listed(&sock, sid.as_str()).await.is_none(), "unpinned: does not come back");
    stop(d, &sock).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn closing_a_pinned_session_on_purpose_drops_its_pin() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard).await;
    let sock = d.paths.sock.clone();
    let (t, sid) = session_with_turn(&sock, guard.home.path()).await;
    assert!(SocketTransport::pin(&sock, &sid, true).await.unwrap());
    t.detach().await;
    SocketTransport::close(&sock, &sid, false, false).await.expect("closed");
    assert!(entry(&book(), sid.as_str()).is_none(), "daemon close drops the pin");

    // `/close` from the session itself, too.
    let (mut t, sid2) = session_with_turn(&sock, guard.home.path()).await;
    assert!(SocketTransport::pin(&sock, &sid2, true).await.unwrap());
    t.send_from_self(SessionCommand::EngineCommand { id: 9, name: "close".into(), arg: String::new() }).await.unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Ended { .. })).await;
    assert!(entry(&book(), sid2.as_str()).is_none(), "/close drops the pin");

    stop(d, &sock).await;
    let d = start(&guard).await;
    restored(&d).await;
    assert!(listed(&sock, sid.as_str()).await.is_none() && listed(&sock, sid2.as_str()).await.is_none());
    stop(d, &sock).await;
}

/// A journal another process holds is never stolen: the entry waits, named
/// with its holder, and stays clearable with `pin --off`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_held_journal_waits_with_its_holder_and_pin_off_clears_it() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard).await;
    let sock = d.paths.sock.clone();
    let (t, sid) = session_with_turn(&sock, guard.home.path()).await;
    assert!(SocketTransport::pin(&sock, &sid, true).await.unwrap());
    t.detach().await;
    stop(d, &sock).await;

    let dir = agent_core::session_lock::sessions_dir();
    let holder = agent_core::session_lock::LockHolder { pid: 4242, kind: "tui".into() };
    let lock = agent_core::session_lock::SessionLock::try_acquire(&dir, sid.as_str(), holder).expect("hold it");
    let d = start(&guard).await;
    restored(&d).await;
    assert!(listed(&sock, sid.as_str()).await.is_none(), "not stolen");
    let e = entry(&book(), sid.as_str()).expect("kept");
    assert_eq!(e.state_text(), "waiting: held by pid 4242, tui");

    assert!(!SocketTransport::pin(&sock, &sid, false).await.expect("off on a pin that is not live"));
    assert!(entry(&book(), sid.as_str()).is_none(), "cleared");
    drop(lock);
    stop(d, &sock).await;
}

/// Missing journal → failed with the reason, kept and visible; a crash-loop
/// marker → failed, not retried; `pin` on it tries again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_missing_journal_fails_visibly_and_a_crash_loop_is_not_retried() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard).await;
    let sock = d.paths.sock.clone();
    let (t, sid) = session_with_turn(&sock, guard.home.path()).await;
    t.detach().await;
    SocketTransport::close(&sock, &sid, false, false).await.expect("closed");
    stop(d, &sock).await;

    let b = book();
    seed(&b, PinEntry { journal_id: "20990101-000000-gone".into(), ..Default::default() });
    seed(&b, PinEntry { journal_id: sid.as_str().into(), attempts: MAX_ATTEMPTS, ..Default::default() });
    let d = start(&guard).await;
    restored(&d).await;
    let gone = entry(&b, "20990101-000000-gone").expect("kept");
    assert!(gone.state_text().starts_with("failed: journal 20990101-000000-gone is missing"), "{}", gone.state_text());
    let looped = entry(&b, sid.as_str()).expect("kept");
    assert!(looped.state_text().contains("died 2 times while restoring"), "{}", looped.state_text());
    assert!(listed(&sock, sid.as_str()).await.is_none(), "a crash loop is not retried");

    // `daemon pin <id>` retries it: the marker is reset.
    assert!(SocketTransport::pin(&sock, &sid, true).await.expect("retry restores"));
    assert!(listed(&sock, sid.as_str()).await.is_some_and(|m| m.pinned));
    assert_eq!(entry(&b, sid.as_str()).map(|e| (e.state, e.attempts)), Some((None, 0)));
    stop(d, &sock).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn restore_pins_off_starts_without_restoring_and_keeps_the_book() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard).await;
    let sock = d.paths.sock.clone();
    let (t, sid) = session_with_turn(&sock, guard.home.path()).await;
    assert!(SocketTransport::pin(&sock, &sid, true).await.unwrap());
    t.detach().await;
    stop(d, &sock).await;

    std::env::set_var(agent_engine::daemon::pins::RESTORE_ENV, "0");
    let d = start(&guard).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::env::remove_var(agent_engine::daemon::pins::RESTORE_ENV);
    assert!(listed(&sock, sid.as_str()).await.is_none(), "not restored");
    assert!(entry(&book(), sid.as_str()).is_some(), "the pin is kept for the next start");
    stop(d, &sock).await;
}

/// The `/keepwarm` override comes back with the pin, and the restore sends
/// no ping by itself (the cache may be cold; keep-warm waits for a turn).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_restored_pin_carries_its_keepwarm_override_and_pings_nothing() {
    let guard = HomeGuard::new();
    let (d, hits) = boot(&guard).await;
    let sock = d.paths.sock.clone();
    let (t, sid) = session_with_turn(&sock, guard.home.path()).await;
    assert!(SocketTransport::pin(&sock, &sid, true).await.unwrap());
    t.send_from_self(SessionCommand::EngineCommand { id: 5, name: "keepwarm".into(), arg: "6h".into() }).await.unwrap();
    let t0 = std::time::Instant::now();
    while entry(&book(), sid.as_str()).and_then(|e| e.cache_keepwarm).is_none() && t0.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(entry(&book(), sid.as_str()).and_then(|e| e.cache_keepwarm).as_deref(), Some("6h"), "kept current");
    t.detach().await;
    stop(d, &sock).await;

    let calls = hits.load(Ordering::SeqCst);
    let d = start(&guard).await;
    wait_listed(&sock, sid.as_str(), Duration::from_secs(20)).await.expect("restored");
    restored(&d).await;
    let h = d.state.attach(&sid).expect("live");
    let t0 = std::time::Instant::now();
    while h.cache_keepwarm_override().is_none() && t0.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(h.cache_keepwarm_override().as_deref(), Some("6h"));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(hits.load(Ordering::SeqCst), calls, "no provider call from the restore");
    stop(d, &sock).await;
}

/// `/clear` on a pinned session starts a new journal: the entry follows it,
/// so the next start continues the new conversation, not the old one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_entry_follows_a_pinned_sessions_new_journal() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard).await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = session_with_turn(&sock, guard.home.path()).await;
    assert!(SocketTransport::pin(&sock, &sid, true).await.unwrap());
    t.send_from_self(SessionCommand::NewSession).await.unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Cleared { .. })).await;
    let journal = d.state.attach(&sid).expect("live").journal_id();
    assert_ne!(journal, sid.as_str());
    let ids: Vec<String> = book().load().into_iter().map(|e| e.journal_id).collect();
    assert_eq!(ids, [journal], "one entry, on the new journal");
    t.detach().await;
    stop(d, &sock).await;
}
