//! `synaps daemon rename` (`ClientFrame::Rename`): name or unname one session
//! from a control connection, with host authority and without attaching.
//! `/saveas`'s rules under a process-wide name lock, a compare-and-set
//! `expect`, a durable save that keeps file mtimes; a parked session is
//! renamed on disk without being woken. Real host, real UDS, loopback stub.

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::{Daemon, DaemonOpts};
use agent_engine::session::socket_transport::{Connected, RenameFailure, Renamed, SocketTransport};
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts};
use phase2::{spawn_stub, HomeGuard, Script, ANTHROPIC_SSE, ANTHROPIC_SSE_PREFIX};
use serial_test::serial;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

type RenameResult = Result<Result<Renamed, RenameFailure>, TransportError>;

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

/// Stub provider at `url` + daemon; `park_grace` is `SYNAPS_DAEMON_PARK_GRACE_SECS`.
async fn boot_at(guard: &HomeGuard, url: &str, park_grace: &str) -> Daemon {
    std::env::set_var("SYNAPS_DAEMON_PARK_GRACE_SECS", park_grace);
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", url);
    start(guard).await
}

async fn boot(guard: &HomeGuard, park_grace: &str) -> (Daemon, Arc<AtomicUsize>) {
    let (url, hits, _bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    (boot_at(guard, &url, park_grace).await, hits)
}

async fn next(t: &mut SocketTransport) -> Envelope {
    tokio::time::timeout(Duration::from_secs(10), t.next_event()).await.expect("timely").expect("open")
}

async fn until(t: &mut SocketTransport, pred: impl Fn(&SessionEventWire) -> bool) -> Vec<Envelope> {
    let mut seen = Vec::new();
    loop {
        let e = next(t).await;
        let done = pred(&e.event);
        seen.push(e);
        if done {
            return seen;
        }
    }
}

async fn create_with(sock: &Path, config: SessionConfig) -> (SocketTransport, SessionId) {
    let conn = SocketTransport::connect(sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (t, _snap) = SocketTransport::attach(conn, Attach::Create { config, mode: AttachMode::Mirror })
        .await
        .expect("attach create");
    let sid = t.session_id().clone();
    (t, sid)
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

/// Create a persisted session over the socket; this client owns input.
async fn create(sock: &Path, cwd: &Path) -> (SocketTransport, SessionId) {
    create_with(sock, config(cwd)).await
}

async fn observe(sock: &Path, sid: &SessionId) -> SocketTransport {
    let conn = SocketTransport::connect(sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (t, _snap) =
        SocketTransport::attach(conn, Attach::Existing { session_id: sid.clone(), mode: AttachMode::Observe })
            .await
            .expect("attach observe");
    t
}

async fn attach_mirror(sock: &Path, sid: &SessionId) -> SocketTransport {
    let conn = SocketTransport::connect(sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (t, _snap) =
        SocketTransport::attach(conn, Attach::Existing { session_id: sid.clone(), mode: AttachMode::Mirror })
            .await
            .expect("attach mirror");
    t
}

/// `/saveas <name>` from this (owning) client; the reply's `value`.
async fn saveas(t: &mut SocketTransport, id: u64, name: &str) -> serde_json::Value {
    t.send_from_self(SessionCommand::EngineCommand { id, name: "saveas".into(), arg: name.into() }).await.unwrap();
    loop {
        if let SessionEventWire::QueryResult { id: got, value } = next(t).await.event {
            if got == id {
                return value;
            }
        }
    }
}

async fn run_turn(t: &mut SocketTransport) {
    t.send_from_self(SessionCommand::Submit { text: "hello".into(), attachments: vec![] }).await.unwrap();
    until(t, |e| matches!(e, SessionEventWire::Idle)).await;
}

async fn listed(sock: &Path, sid: &SessionId) -> Option<SessionMeta> {
    SocketTransport::sessions(sock).await.unwrap_or_default().into_iter().find(|m| &m.id == sid)
}

async fn wait_lifecycle(sock: &Path, sid: &SessionId, want: SessionLifecycle, budget: Duration) -> bool {
    let t0 = std::time::Instant::now();
    loop {
        if listed(sock, sid).await.is_some_and(|m| m.lifecycle == want) {
            return true;
        }
        if t0.elapsed() > budget {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn stop(d: Daemon, sock: &Path) {
    SocketTransport::shutdown(sock, false).await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), d.wait()).await.expect("daemon wait");
    std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
}

fn session_file(guard: &HomeGuard, journal_id: &str) -> PathBuf {
    guard.base_dir().join("sessions").join(format!("{journal_id}.json"))
}

/// The saved session file's `name` (`sessions/<journal id>.json`).
fn saved_name(guard: &HomeGuard, journal_id: &str) -> Option<String> {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(session_file(guard, journal_id)).unwrap()).unwrap();
    v["name"].as_str().map(str::to_string)
}

fn mtime(path: &Path) -> std::time::SystemTime {
    std::fs::metadata(path).unwrap().modified().unwrap()
}

/// The `name` of every session card under `run/` (`run/<sid>.json`).
fn card_names(guard: &HomeGuard) -> Vec<Option<String>> {
    let Ok(dir) = std::fs::read_dir(guard.base_dir().join("run")) else { return Vec::new() };
    dir.flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(e.path()).ok()?).ok())
        .filter(|v| v.get("session_id").is_some())
        .map(|v| v["name"].as_str().map(str::to_string))
        .collect()
}

fn renamed(name: &str) -> Result<Renamed, RenameFailure> {
    Ok(Renamed { name: Some(name.into()), in_memory_only: false })
}

fn failure(r: RenameResult) -> RenameFailure {
    match r {
        Ok(Err(f)) => f,
        other => panic!("expected a rename failure, got {other:?}"),
    }
}

/// `expect: Some(None)`: the default precondition (only if unnamed).
const UNNAMED: Option<Option<String>> = Some(None);

fn expect_named(n: &str) -> Option<Option<String>> {
    Some(Some(n.to_string()))
}

/// The subconscious' case: an observer is attached (it does not own input
/// and must not have to), the session is renamed from a control
/// connection, and the observer sees the name at once. Presence is
/// unchanged. The rename keeps the file mtime, so `--continue` and the
/// `/sessions` order are untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn rename_names_a_live_session_without_attaching_or_touching_its_mtime() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "never").await;
    let sock = d.paths.sock.clone();
    let (mut owner, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut owner).await;
    // A newer session: the one `--continue` must keep picking.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (mut newer, _newer_sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut newer).await;
    let latest_before = agent_engine::latest_session().unwrap().id;
    let order_before: Vec<String> =
        agent_engine::list_recent_sessions(10).unwrap().into_iter().map(|s| s.id).collect();
    let journal = listed(&sock, &sid).await.unwrap().journal_id;
    let mtime_before = mtime(&session_file(&guard, &journal));

    let mut obs = observe(&sock, &sid).await;
    let before = d.state.attach(&sid).unwrap().presence();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let got = SocketTransport::rename(&sock, &sid, Some("ambient-notes"), UNNAMED).await.expect("reply");
    assert_eq!(got, renamed("ambient-notes"));

    until(&mut obs, |e| {
        matches!(e, SessionEventWire::Conversation(c) if c.header.name.as_deref() == Some("ambient-notes"))
    })
    .await;
    let m = listed(&sock, &sid).await.expect("listed");
    assert_eq!(m.name.as_deref(), Some("ambient-notes"), "the listing shows it");
    assert_eq!(saved_name(&guard, &journal).as_deref(), Some("ambient-notes"), "saved to the session file");
    assert!(card_names(&guard).contains(&Some("ambient-notes".into())), "run/<sid>.json carries it");
    let after = d.state.attach(&sid).unwrap().presence();
    assert_eq!((after.clients, after.input_owner), (before.clients, before.input_owner), "presence unchanged");

    // S1: a rename is not activity.
    assert_eq!(mtime(&session_file(&guard, &journal)), mtime_before, "snapshot mtime kept");
    assert_eq!(agent_engine::latest_session().unwrap().id, latest_before, "--continue still picks the newer session");
    let order_after: Vec<String> =
        agent_engine::list_recent_sessions(10).unwrap().into_iter().map(|s| s.id).collect();
    assert_eq!(order_after, order_before, "/sessions order unchanged");

    // Renaming to the current name again is a no-op success.
    let again = SocketTransport::rename(&sock, &sid, Some("ambient-notes"), expect_named("ambient-notes")).await;
    assert_eq!(again.unwrap(), renamed("ambient-notes"));

    // Clear, with the matching precondition.
    let cleared = SocketTransport::rename(&sock, &sid, None, expect_named("ambient-notes")).await.unwrap();
    assert_eq!(cleared, Ok(Renamed { name: None, in_memory_only: false }));
    until(&mut obs, |e| matches!(e, SessionEventWire::Conversation(c) if c.header.name.is_none())).await;
    assert_eq!(listed(&sock, &sid).await.unwrap().name, None);
    assert_eq!(saved_name(&guard, &journal), None);

    stop(d, &sock).await;
}

/// S3: rename never silently replaces a name. `expect` is checked in the
/// actor, so a name set after the caller's listing still wins.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_expect_precondition_keeps_a_name_the_user_set() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "never").await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut t).await;
    // The user names it (as /saveas would) after the auto-namer listed it.
    assert_eq!(SocketTransport::rename(&sock, &sid, Some("mine"), None).await.unwrap(), renamed("mine"));

    let f = failure(SocketTransport::rename(&sock, &sid, Some("auto-name"), UNNAMED).await);
    assert_eq!(f.reason, RenameRefusal::Named);
    assert_eq!(f.current_name.as_deref(), Some("mine"));
    let f = failure(SocketTransport::rename(&sock, &sid, None, expect_named("other")).await);
    assert_eq!(f.reason, RenameRefusal::Named, "a clear is guarded too");
    assert_eq!(listed(&sock, &sid).await.unwrap().name.as_deref(), Some("mine"), "unchanged");

    assert_eq!(
        SocketTransport::rename(&sock, &sid, Some("theirs"), expect_named("mine")).await.unwrap(),
        renamed("theirs"),
        "the matching precondition (--force) replaces it"
    );
    stop(d, &sock).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn rename_refuses_an_invalid_a_taken_and_a_chain_name_with_the_precise_reason() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "never").await;
    let sock = d.paths.sock.clone();
    let (mut a, sid_a) = create(&sock, guard.home.path()).await;
    run_turn(&mut a).await;
    let (mut b, sid_b) = create(&sock, guard.home.path()).await;
    run_turn(&mut b).await;
    assert_eq!(SocketTransport::rename(&sock, &sid_a, Some("axon"), UNNAMED).await.unwrap(), renamed("axon"));

    let f = failure(SocketTransport::rename(&sock, &sid_b, Some("Bad Name"), UNNAMED).await);
    assert_eq!(f.reason, RenameRefusal::Invalid);
    assert!(f.message.contains("invalid name 'Bad Name'"), "{}", f.message);

    let f = failure(SocketTransport::rename(&sock, &sid_b, Some("axon"), UNNAMED).await);
    let journal_a = listed(&sock, &sid_a).await.unwrap().journal_id;
    assert_eq!(f.reason, RenameRefusal::Taken);
    assert_eq!(f.taken_by.as_deref(), Some(journal_a.as_str()), "names the holder");

    agent_engine::core::chain::save_chain("my-chain", &journal_a).expect("chain");
    let f = failure(SocketTransport::rename(&sock, &sid_b, Some("my-chain"), UNNAMED).await);
    assert_eq!(f.reason, RenameRefusal::ChainConflict, "{}", f.message);

    assert_eq!(listed(&sock, &sid_b).await.unwrap().name, None, "nothing changed");
    stop(d, &sock).await;
}

/// S2: the uniqueness check and the save happen under one process-wide
/// lock, so two sessions renamed to the same name at once cannot both win.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn concurrent_renames_of_two_sessions_to_one_name_have_exactly_one_winner() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "never").await;
    let sock = d.paths.sock.clone();
    let mut sids = Vec::new();
    let mut keep = Vec::new();
    for _ in 0..4 {
        let (mut t, sid) = create(&sock, guard.home.path()).await;
        run_turn(&mut t).await;
        sids.push(sid);
        keep.push(t);
    }
    for round in 0..5 {
        let name = format!("dup-{round}");
        let results = futures::future::join_all(
            sids.iter().map(|sid| SocketTransport::rename(&sock, sid, Some(name.as_str()), None)),
        )
        .await;
        let wins = results.iter().filter(|r| matches!(r, Ok(Ok(_)))).count();
        let taken = results.iter().filter(|r| matches!(r, Ok(Err(f)) if f.reason == RenameRefusal::Taken)).count();
        assert_eq!((wins, taken), (1, sids.len() - 1), "round {round}: {results:?}");
        let mut holders = 0;
        for sid in &sids {
            if listed(&sock, sid).await.unwrap().name.as_deref() == Some(name.as_str()) {
                holders += 1;
            }
        }
        assert_eq!(holders, 1, "exactly one session carries {name}");
    }
    stop(d, &sock).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn rename_of_an_unknown_session_is_refused_and_the_connection_closes() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "never").await;
    let sock = d.paths.sock.clone();
    let mut c = Connected::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    let err = c.rename(&SessionId::from("no-such-session"), Some("x"), None).await.unwrap_err();
    assert!(matches!(err, TransportError::UnknownSession(ref m) if m.contains("no-such-session")), "{err}");
    assert!(c.ping().await.is_err(), "a refusal is terminal, as for pin and close");
    stop(d, &sock).await;
}

/// B1: a parked session is renamed on disk (under its journal lock) and is
/// NOT woken; listing, card and file carry the name, the mtime is kept,
/// and when it unparks the name comes from disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_parked_session_is_renamed_on_disk_without_waking_and_keeps_the_name_after_unpark() {
    let guard = HomeGuard::new();
    let (d, hits) = boot(&guard, "0").await;
    let sock = d.paths.sock.clone();
    let (mut other, other_sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut other).await;
    let (mut t, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut t).await;
    t.detach().await;
    assert!(wait_lifecycle(&sock, &sid, SessionLifecycle::Parked, Duration::from_secs(10)).await, "parks");
    let calls = hits.load(Ordering::SeqCst);
    let handle = d.state.attach(&sid).expect("listed");
    let mut rx = handle.subscribe();
    let journal = listed(&sock, &sid).await.unwrap().journal_id;
    let mtime_before = mtime(&session_file(&guard, &journal));

    let got = SocketTransport::rename(&sock, &sid, Some("parked-notes"), UNNAMED).await.unwrap();
    assert_eq!(got, renamed("parked-notes"));
    // A taken name is refused for a parked session too.
    assert_eq!(SocketTransport::rename(&sock, &other_sid, Some("live-one"), UNNAMED).await.unwrap(), renamed("live-one"));
    let f = failure(SocketTransport::rename(&sock, &sid, Some("live-one"), None).await);
    assert_eq!(f.reason, RenameRefusal::Taken);
    // And the precondition applies.
    let f = failure(SocketTransport::rename(&sock, &sid, Some("x"), UNNAMED).await);
    assert_eq!((f.reason, f.current_name.as_deref()), (RenameRefusal::Named, Some("parked-notes")));

    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut events = Vec::new();
    while let Ok(env) = rx.try_recv() {
        events.push(env.event);
    }
    assert!(
        !events.iter().any(|e| matches!(e, SessionEventWire::Lifecycle(SessionLifecycle::Live))),
        "never woken: {events:#?}"
    );
    assert_eq!(handle.lifecycle(), SessionLifecycle::Parked);
    assert_eq!(hits.load(Ordering::SeqCst), calls, "no provider call");
    assert_eq!(listed(&sock, &sid).await.unwrap().name.as_deref(), Some("parked-notes"), "listing");
    assert_eq!(saved_name(&guard, &journal).as_deref(), Some("parked-notes"), "file");
    assert!(card_names(&guard).contains(&Some("parked-notes".into())), "card");
    assert_eq!(mtime(&session_file(&guard, &journal)), mtime_before, "mtime kept");

    // Unpark and run a turn: the conversation was rebuilt from the file, so
    // its turn-end save keeps the name (a stale in-memory session would
    // write it back without one).
    let mut back = attach_mirror(&sock, &sid).await;
    assert!(wait_lifecycle(&sock, &sid, SessionLifecycle::Live, Duration::from_secs(10)).await, "unparked");
    run_turn(&mut back).await;
    tokio::time::sleep(Duration::from_millis(300)).await; // the queued turn-end save
    assert_eq!(saved_name(&guard, &journal).as_deref(), Some("parked-notes"), "file after a turn");
    assert_eq!(listed(&sock, &sid).await.unwrap().name.as_deref(), Some("parked-notes"));
    assert_eq!(d.state.attach(&sid).unwrap().name().as_deref(), Some("parked-notes"));
    stop(d, &sock).await;
}

/// S6: renaming and clearing a pinned session's name updates the pin book
/// verbatim; the old name stops resolving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn renaming_and_clearing_a_pinned_session_updates_the_pin_book() {
    use agent_engine::daemon::pins::{resolve_entry, PinBook};
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "never").await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut t).await;
    assert!(SocketTransport::pin(&sock, &sid, true).await.expect("pinned"));

    assert_eq!(SocketTransport::rename(&sock, &sid, Some("pinny"), UNNAMED).await.unwrap(), renamed("pinny"));
    let book = PinBook::for_active_profile();
    assert!(resolve_entry(&book.load(), "pinny").is_some(), "{:?}", book.load());

    let cleared = SocketTransport::rename(&sock, &sid, None, expect_named("pinny")).await.unwrap();
    assert_eq!(cleared, Ok(Renamed { name: None, in_memory_only: false }));
    let entries = book.load();
    assert_eq!(entries.len(), 1, "still pinned");
    assert_eq!(entries[0].name, None, "the book's name is cleared, not kept");
    assert!(resolve_entry(&entries, "pinny").is_none(), "the old name no longer resolves");
    stop(d, &sock).await;
}

/// S4: a session that is not persisted gets the name in memory only (and
/// the reply says so); nothing is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_unpersisted_session_is_renamed_in_memory_only() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "never").await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create_with(&sock, SessionConfig { persist: false, ..config(guard.home.path()) }).await;
    run_turn(&mut t).await;
    let journal = listed(&sock, &sid).await.unwrap().journal_id;
    let got = SocketTransport::rename(&sock, &sid, Some("ephemeral"), UNNAMED).await.unwrap();
    assert_eq!(got, Ok(Renamed { name: Some("ephemeral".into()), in_memory_only: true }));
    assert_eq!(listed(&sock, &sid).await.unwrap().name.as_deref(), Some("ephemeral"));
    assert!(!session_file(&guard, &journal).exists(), "nothing written for a persist:false session");
    stop(d, &sock).await;
}

/// A stub whose first request is a whole turn and every later one streams
/// a first frame and then hangs (a turn in flight until cancelled).
async fn stub_then_hang() -> String {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new().fallback(move || {
        let hits = Arc::clone(&hits);
        async move {
            use axum::response::IntoResponse;
            if hits.fetch_add(1, Ordering::SeqCst) == 0 {
                return (axum::http::StatusCode::OK, [("content-type", "text/event-stream")], ANTHROPIC_SSE.to_string())
                    .into_response();
            }
            use futures::StreamExt;
            let body = futures::stream::once(async { Ok::<_, std::convert::Infallible>(ANTHROPIC_SSE_PREFIX.to_string()) })
                .chain(futures::stream::pending());
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from_stream(body))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// A rename while a turn streams is answered at once, and the turn-end
/// save (after a cancel) keeps the new name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_rename_during_a_streaming_turn_survives_the_turn_end_save() {
    let guard = HomeGuard::new();
    let url = stub_then_hang().await;
    let d = boot_at(&guard, &url, "never").await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut t).await;
    t.send_from_self(SessionCommand::Submit { text: "second".into(), attachments: vec![] }).await.unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;

    let got = tokio::time::timeout(
        Duration::from_secs(10),
        SocketTransport::rename(&sock, &sid, Some("mid-turn"), UNNAMED),
    )
    .await
    .expect("answered while streaming");
    assert_eq!(got.unwrap(), renamed("mid-turn"));

    t.send_from_self(SessionCommand::Cancel).await.unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    tokio::time::sleep(Duration::from_millis(300)).await; // the queued turn-end save
    let journal = listed(&sock, &sid).await.unwrap().journal_id;
    assert_eq!(saved_name(&guard, &journal).as_deref(), Some("mid-turn"), "not clobbered by the turn-end save");
    assert_eq!(listed(&sock, &sid).await.unwrap().name.as_deref(), Some("mid-turn"));
    stop(d, &sock).await;
}

/// Serves streaming turns and the (non-streaming) compaction summary.
async fn stub_compact() -> String {
    let app = axum::Router::new().fallback(move |body: String| async move {
        use axum::response::IntoResponse;
        if body.contains("\"stream\":true") {
            return (axum::http::StatusCode::OK, [("content-type", "text/event-stream")], ANTHROPIC_SSE.to_string())
                .into_response();
        }
        axum::Json(serde_json::json!({
            "id": "msg_c", "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "SUMMARY: the user said hello."}],
            "usage": {"input_tokens": 50, "output_tokens": 10}
        }))
        .into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// After a linked-successor compaction the journal id changes: a rename
/// lands on the successor, and the predecessor's name does not block it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn rename_after_a_compaction_lands_on_the_successor() {
    let guard = HomeGuard::new();
    let url = stub_compact().await;
    let d = boot_at(&guard, &url, "never").await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create_with(
        &sock,
        SessionConfig {
            await_extensions: true,
            compaction_policy: CompactionPolicyWire::LinkedSuccessor,
            ..config(guard.home.path())
        },
    )
    .await;
    run_turn(&mut t).await;
    run_turn(&mut t).await;
    assert_eq!(SocketTransport::rename(&sock, &sid, Some("before"), UNNAMED).await.unwrap(), renamed("before"));
    let old_journal = listed(&sock, &sid).await.unwrap().journal_id;

    t.send(SessionCommand::Compact { instructions: None }).await.unwrap();
    let mut new_journal = String::new();
    for e in until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await {
        if let SessionEventWire::CompactionApplied { session_id, .. } = e.event {
            new_journal = session_id;
        }
    }
    assert!(!new_journal.is_empty() && new_journal != old_journal, "compacted into a successor");

    let current = listed(&sock, &sid).await.unwrap().name;
    let got = SocketTransport::rename(&sock, &sid, Some("after"), Some(current)).await.unwrap();
    assert_eq!(got, renamed("after"));
    assert_eq!(saved_name(&guard, &new_journal).as_deref(), Some("after"), "the successor carries it");
    assert_ne!(saved_name(&guard, &old_journal).as_deref(), Some("after"));
    // The predecessor no longer holds "before": the successor can take it back.
    let back = SocketTransport::rename(&sock, &sid, Some("before"), expect_named("after")).await.unwrap();
    assert_eq!(back, renamed("before"), "the predecessor does not report it taken");
    stop(d, &sock).await;
}

// ── raw-socket client ─────────────────────────────────────────────────────────

async fn write_frame(w: &mut tokio::net::unix::OwnedWriteHalf, f: &ClientFrame) {
    w.write_all(encode_line(f).unwrap().as_bytes()).await.unwrap();
}

async fn read_until<T>(
    lines: &mut tokio::io::Lines<tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>>,
    mut pick: impl FnMut(DaemonFrame) -> Option<T>,
) -> T {
    loop {
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .expect("timely")
            .unwrap()
            .expect("open");
        if let Some(t) = pick(decode_line::<DaemonFrame>(&line).unwrap()) {
            return t;
        }
    }
}

/// An attached client has `/saveas`: neither the control frame on its
/// connection nor the host-only `rename` engine command from it renames,
/// and a host `rename` with plain-text args (an in-process `/rename foo`)
/// is refused too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn rename_from_an_attached_connection_or_as_plain_text_is_refused() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "never").await;
    let sock = d.paths.sock.clone();

    let (r, mut w) = tokio::net::UnixStream::connect(&sock).await.unwrap().into_split();
    let mut lines = tokio::io::BufReader::new(r).lines();
    write_frame(&mut w, &ClientFrame::Hello(Hello::new(ClientKind::Test))).await;
    read_until(&mut lines, |f| matches!(f, DaemonFrame::Welcome(_)).then_some(())).await;
    let config = SessionConfig { cwd: Some(guard.home.path().to_path_buf()), ..Default::default() };
    write_frame(&mut w, &ClientFrame::Attach(Attach::Create { config, mode: AttachMode::Mirror })).await;
    let sid = read_until(&mut lines, |f| match f {
        DaemonFrame::Attached(a) => Some(a.meta.id),
        _ => None,
    })
    .await;

    write_frame(&mut w, &ClientFrame::Rename { session_id: sid.clone(), name: Some("nope".into()), expect: None }).await;
    let message = read_until(&mut lines, |f| match f {
        DaemonFrame::Error { message, .. } => Some(message),
        _ => None,
    })
    .await;
    assert!(message.contains("control frames are not accepted"), "{message}");

    let cmd = SessionCommand::EngineCommand { id: 9, name: "rename".into(), arg: r#"{"name":"nope"}"#.into() };
    write_frame(&mut w, &ClientFrame::Cmd { session_id: sid.clone(), cmd }).await;
    let reason = read_until(&mut lines, |f| match f {
        DaemonFrame::Event(env) => match SessionEventWire::from(env.event) {
            SessionEventWire::Refused { reason, .. } => Some(reason),
            _ => None,
        },
        _ => None,
    })
    .await;
    assert!(reason.contains("host-only"), "{reason}");

    // Host authority but plain text: what an in-process `/rename foo` sends.
    let handle = d.state.attach(&sid).unwrap();
    let mut rx = handle.subscribe();
    handle.send(SessionCommand::EngineCommand { id: 11, name: "rename".into(), arg: "nope".into() }).await.unwrap();
    let value = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let SessionEventWire::QueryResult { id: 11, value } = rx.recv().await.unwrap().event {
                return value;
            }
        }
    })
    .await
    .unwrap();
    assert!(value["text"].as_str().unwrap().contains("use /saveas"), "{value}");
    assert_eq!(listed(&sock, &sid).await.expect("listed").name, None);

    write_frame(&mut w, &ClientFrame::Bye).await;
    stop(d, &sock).await;
}

/// `synaps daemon rename <args> --json` as a separate process: (exit code,
/// stdout, stderr).
async fn cli_raw(guard: &HomeGuard, run: &Path, args: &[&str]) -> (i32, String, String) {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_synaps"));
    cmd.arg("daemon")
        .arg("rename")
        .args(args)
        .arg("--json")
        .env("HOME", guard.home.path())
        .env("SYNAPS_BASE_DIR", guard.base_dir())
        .env("SYNAPS_RUNTIME_DIR", run)
        .env("SYNAPS_DAEMON", "1")
        .env("SYNAPS_NO_BOOT_FX", "1")
        .env_remove("SYNAPS_DAEMON_RELOAD_STATE")
        .env_remove("SYNAPS_DAEMON_LOCK_FD")
        .stdin(std::process::Stdio::null());
    let out = tokio::task::spawn_blocking(move || cmd.output().expect("run synaps daemon rename")).await.unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

async fn cli_rename(guard: &HomeGuard, run: &Path, args: &[&str]) -> (i32, serde_json::Value) {
    let (code, stdout, stderr) = cli_raw(guard, run, args).await;
    let json = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout} / {stderr}"));
    (code, json)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_cli_renames_refuses_to_replace_without_force_and_reports_each_exit_code() {
    let guard = HomeGuard::new();
    let run: PathBuf = guard.base_dir().join("run");

    let (code, json) = cli_rename(&guard, &run, &["anything", "x"]).await;
    assert_eq!((code, json["outcome"].as_str()), (1, Some("no_daemon")), "{json}");

    let (d, _hits) = boot(&guard, "0").await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut t).await;

    let (code, json) = cli_rename(&guard, &run, &["no-such-session", "x"]).await;
    assert_eq!((code, json["outcome"].as_str()), (1, Some("not_found")), "{json}");

    // S7: a blank positional is a usage error, never a clear.
    let (code, _out, err) = cli_raw(&guard, &run, &[sid.as_str(), ""]).await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("--clear"), "{err}");

    let prefix = &sid.as_str()[..sid.as_str().len() - 1];
    let (code, json) = cli_rename(&guard, &run, &[prefix, "daily-notes"]).await;
    assert_eq!(code, 0, "{json}");
    assert_eq!((json["outcome"].as_str(), json["name"].as_str()), (Some("renamed"), Some("daily-notes")));
    assert_eq!(json["session_id"], sid.as_str());

    // S3: replacing or clearing a name needs --force (exit 3 otherwise).
    let (code, json) = cli_rename(&guard, &run, &["daily-notes", "other"]).await;
    assert_eq!((code, json["outcome"].as_str()), (3, Some("named")), "{json}");
    assert_eq!(json["name"], "daily-notes", "the current name");
    let (code, json) = cli_rename(&guard, &run, &["daily-notes", "--clear"]).await;
    assert_eq!((code, json["outcome"].as_str()), (3, Some("named")), "{json}");
    let (code, json) = cli_rename(&guard, &run, &["daily-notes", "UPPER", "--force"]).await;
    assert_eq!((code, json["outcome"].as_str()), (1, Some("invalid")), "{json}");
    let (code, json) = cli_rename(&guard, &run, &["daily-notes", "weekly", "--force"]).await;
    assert_eq!((code, json["outcome"].as_str()), (0, Some("renamed")), "{json}");

    // Parked: renamed on disk, exit 0.
    t.detach().await;
    assert!(wait_lifecycle(&sock, &sid, SessionLifecycle::Parked, Duration::from_secs(10)).await);
    let (code, json) = cli_rename(&guard, &run, &["weekly", "--clear", "--force"]).await;
    assert_eq!((code, json["outcome"].as_str()), (0, Some("cleared")), "{json}");
    assert!(json["name"].is_null());
    let m = listed(&sock, &sid).await.unwrap();
    assert_eq!((m.name, m.lifecycle), (None, SessionLifecycle::Parked));

    stop(d, &sock).await;
}

/// L1: `/saveas` takes the same process-wide name lock as `daemon rename`,
/// so a human naming session B and the auto-namer naming session A the
/// same name at once can't both win.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_saveas_racing_a_daemon_rename_of_another_session_has_one_winner() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "never").await;
    let sock = d.paths.sock.clone();
    let (mut a, sid_a) = create(&sock, guard.home.path()).await;
    run_turn(&mut a).await;
    let (mut b, sid_b) = create(&sock, guard.home.path()).await;
    run_turn(&mut b).await;
    for round in 0..8u64 {
        let name = format!("race-{round}");
        let (rename, saved) = tokio::join!(
            SocketTransport::rename(&sock, &sid_a, Some(name.as_str()), None),
            saveas(&mut b, 100 + round, &name),
        );
        let rename_won = matches!(rename, Ok(Ok(_)));
        let saveas_won = saved["kind"] == "output";
        assert!(rename_won ^ saveas_won, "round {round}: exactly one wins: {rename:?} / {saved}");
        let a_name = listed(&sock, &sid_a).await.unwrap().name;
        let b_name = listed(&sock, &sid_b).await.unwrap().name;
        let holders = [a_name, b_name].iter().filter(|n| n.as_deref() == Some(name.as_str())).count();
        assert_eq!(holders, 1, "round {round}: one session named {name}");
    }
    stop(d, &sock).await;
}

/// L2: waiting for the name lock is bounded; a session whose rename can't
/// get it answers `busy` (and so does `/saveas`) instead of freezing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_rename_that_cannot_get_the_name_lock_answers_busy() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "never").await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut t).await;

    let held = agent_engine::session::actor_cmds::hold_name_lock_for_tests().await;
    let t0 = std::time::Instant::now();
    let f = failure(SocketTransport::rename(&sock, &sid, Some("later"), UNNAMED).await);
    assert_eq!(f.reason, RenameRefusal::Busy, "{}", f.message);
    assert!(t0.elapsed() < Duration::from_secs(10), "bounded: {:?}", t0.elapsed());
    let v = saveas(&mut t, 7, "later").await;
    assert_eq!(v["kind"], "error", "{v}");
    assert!(v["text"].as_str().unwrap().contains("retry"), "{v}");
    drop(held);

    assert_eq!(SocketTransport::rename(&sock, &sid, Some("later"), UNNAMED).await.unwrap(), renamed("later"));
    stop(d, &sock).await;
}

/// `locked`: a parked session whose journal another process holds (an
/// in-process `--continue`) is not renamed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_parked_session_held_by_another_process_is_locked() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "0").await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut t).await;
    t.detach().await;
    assert!(wait_lifecycle(&sock, &sid, SessionLifecycle::Parked, Duration::from_secs(10)).await);
    let journal = listed(&sock, &sid).await.unwrap().journal_id;
    let lock = agent_core::session_lock::SessionLock::try_acquire(
        &agent_core::session_lock::sessions_dir(),
        &journal,
        agent_core::session_lock::LockHolder { pid: 4242, kind: "tui".into() },
    )
    .expect("the parked session released its lock");

    let f = failure(SocketTransport::rename(&sock, &sid, Some("held"), UNNAMED).await);
    assert_eq!(f.reason, RenameRefusal::Locked, "{}", f.message);
    assert_eq!(saved_name(&guard, &journal), None);
    drop(lock);
    assert_eq!(SocketTransport::rename(&sock, &sid, Some("held"), UNNAMED).await.unwrap(), renamed("held"));
    stop(d, &sock).await;
}

/// A parked session's `expect` is checked against the FILE: another
/// process renamed it while parked, so an "only if unnamed" rename loses,
/// and the listing picks up the name on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_parked_rename_checks_expect_against_the_file() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "0").await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut t).await;
    t.detach().await;
    assert!(wait_lifecycle(&sock, &sid, SessionLifecycle::Parked, Duration::from_secs(10)).await);
    let journal = listed(&sock, &sid).await.unwrap().journal_id;
    // The session picker in another process (the journal lock is free).
    agent_engine::core::session_catalog::rename_past_session(
        &agent_core::session_lock::sessions_dir(),
        &journal,
        Some("external"),
    )
    .unwrap();
    assert_eq!(listed(&sock, &sid).await.unwrap().name, None, "the daemon has not seen it yet");

    let f = failure(SocketTransport::rename(&sock, &sid, Some("auto"), UNNAMED).await);
    assert_eq!((f.reason, f.current_name.as_deref()), (RenameRefusal::Named, Some("external")));
    assert_eq!(saved_name(&guard, &journal).as_deref(), Some("external"), "not clobbered");
    assert_eq!(listed(&sock, &sid).await.unwrap().name.as_deref(), Some("external"), "listing refreshed");
    stop(d, &sock).await;
}

/// S5: a save that fails is reported (`failed`) and rolled back: memory,
/// listing and file keep the old name. The save is made to fail by putting
/// a directory where the snapshot goes (the atomic rename onto it fails).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_rename_whose_save_fails_is_rolled_back() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "never").await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut t).await;
    let journal = listed(&sock, &sid).await.unwrap().journal_id;
    let file = session_file(&guard, &journal);
    let aside = file.with_extension("json.aside");
    std::fs::rename(&file, &aside).unwrap();
    std::fs::create_dir(&file).unwrap();

    let r = SocketTransport::rename(&sock, &sid, Some("unsaved"), UNNAMED).await;
    std::fs::remove_dir(&file).unwrap();
    std::fs::rename(&aside, &file).unwrap();
    let f = failure(r);
    assert_eq!(f.reason, RenameRefusal::Failed);
    assert!(f.message.contains("rolled back"), "{}", f.message);
    assert_eq!(listed(&sock, &sid).await.unwrap().name, None, "listing unchanged");
    assert_eq!(saved_name(&guard, &journal), None, "file unchanged");
    // Memory too: the precondition "unnamed" still holds.
    assert_eq!(SocketTransport::rename(&sock, &sid, Some("saved"), UNNAMED).await.unwrap(), renamed("saved"));
    stop(d, &sock).await;
}
