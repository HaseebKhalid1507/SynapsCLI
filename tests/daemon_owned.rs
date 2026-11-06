//! Owned sessions (`Attach::Create{owned}`): a session bound to the
//! connection that created it. The owner going away for any reason (drop,
//! detach, its process SIGKILLed) cancels a running turn as `Cancel` does
//! and ends the session (`EndReason::OwnerGone`); observers stay observers.
//! A daemon reload gives the owner a grace to re-bind with its token.
//! `forward_secrets` hands the session's tools the secret env vars the
//! daemon otherwise strips, in memory only. Real daemons (in-process host
//! or the `synaps` binary), real UDS, loopback Anthropic stub.
#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::{registry, Daemon, DaemonOpts};
use agent_engine::engine::interrupt::InterruptReason;
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts, LlmEvent, StreamEvent};
use phase2::{spawn_stub, HomeGuard, Script, ANTHROPIC_SSE, ANTHROPIC_SSE_PREFIX};
use serial_test::serial;

// ── harness ─────────────────────────────────────────────────────────────────

async fn start_in_process(guard: &HomeGuard, script: Script, idle_exit: Option<Duration>) -> Daemon {
    start_counted(guard, script, idle_exit).await.0
}

/// [`start_in_process`], plus the stub's request count.
async fn start_counted(
    guard: &HomeGuard,
    script: Script,
    idle_exit: Option<Duration>,
) -> (Daemon, Arc<std::sync::atomic::AtomicUsize>) {
    let (url, hits, _bodies) = spawn_stub(script).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host: Arc<EngineHost> = EngineHost::boot_and_install(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .expect("host boot");
    let d = Daemon::start(host, DaemonOpts { runtime_dir: Some(guard.base_dir().join("run")), idle_exit, ..Default::default() })
        .await
        .expect("daemon start");
    (d, hits)
}

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

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_synaps")
}

/// The env a `synaps` child needs to find this guard's daemon.
fn synaps_cmd(guard: &HomeGuard, url: &str) -> Command {
    let mut c = Command::new(bin());
    c.env("HOME", guard.home.path())
        .env("SYNAPS_BASE_DIR", guard.base_dir())
        .env("SYNAPS_RUNTIME_DIR", guard.base_dir().join("run"))
        .env("SYNAPS_DAEMON", "1")
        .env("SYNAPS_ANTHROPIC_BASE_URL", url)
        .env("SYNAPS_NO_BOOT_FX", "1")
        .env_remove("SYNAPS_DAEMON_RELOAD_STATE")
        .env_remove("SYNAPS_DAEMON_LOCK_FD")
        // The daemon never has the secret itself: what a tool sees came
        // from the client.
        .env_remove("GH_TOKEN");
    c
}

/// `synaps daemon --foreground` under the guard's HOME; its stderr goes to
/// a file under HOME (as an auto-spawned daemon's does), so a grep of HOME
/// covers its logs too.
async fn spawn_daemon(guard: &HomeGuard, url: &str, extra: &[(&str, &str)]) -> DaemonProc {
    let run = guard.base_dir().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let paths = registry::daemon_paths_in(&run, None);
    let stderr = std::fs::File::create(guard.base_dir().join("daemon-stderr.log")).unwrap();
    let child = synaps_cmd(guard, url)
        .args(["daemon", "--foreground"])
        .envs(extra.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn daemon");
    let d = DaemonProc { child, paths };
    let t0 = std::time::Instant::now();
    while SocketTransport::ping(&d.paths.sock).await.is_err() {
        assert!(t0.elapsed() < Duration::from_secs(20), "daemon never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    d
}

fn config(cwd: &Path) -> SessionConfig {
    SessionConfig {
        cwd: Some(cwd.to_path_buf()),
        model_override: Some("claude-sonnet-4-5".into()),
        persist: true,
        ..Default::default()
    }
}

async fn create_owned(sock: &Path, cwd: &Path, forward_secrets: bool) -> (SocketTransport, SessionId, AttachSnapshot) {
    let (t, snap) = SocketTransport::create_owned(sock, Hello::new(ClientKind::Test), config(cwd), AttachMode::Mirror, forward_secrets)
        .await
        .expect("create owned");
    assert!(t.is_session_owner());
    let sid = t.session_id().clone();
    (t, sid, snap)
}

async fn observe(sock: &Path, sid: &SessionId) -> SocketTransport {
    let conn = SocketTransport::connect(sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (t, _) = SocketTransport::attach(conn, Attach::Existing { session_id: sid.clone(), mode: AttachMode::Observe })
        .await
        .expect("observe");
    assert!(!t.is_session_owner(), "an observer never owns");
    t
}

async fn until(t: &mut SocketTransport, pred: impl Fn(&SessionEventWire) -> bool) -> Vec<Envelope> {
    let mut seen = Vec::new();
    loop {
        let e = tokio::time::timeout(Duration::from_secs(20), t.next_event())
            .await
            .expect("timely")
            .unwrap_or_else(|| panic!("stream closed; saw {:?}", seen.iter().map(|e: &Envelope| &e.event).collect::<Vec<_>>()));
        let done = pred(&e.event);
        seen.push(e);
        if done {
            return seen;
        }
    }
}

fn is_text(e: &SessionEventWire) -> bool {
    matches!(e, SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::Text(_))))
}

async fn expect_owner_gone(obs: &mut SocketTransport) {
    let seen = until(obs, |e| matches!(e, SessionEventWire::Ended { .. })).await;
    assert!(
        matches!(seen.last().unwrap().event, SessionEventWire::Ended { reason: EndReason::OwnerGone }),
        "{:?}",
        seen.last().unwrap().event
    );
}

async fn listed(sock: &Path, sid: &SessionId) -> bool {
    SocketTransport::sessions(sock).await.unwrap_or_default().iter().any(|m| &m.id == sid)
}

async fn wait_gone(sock: &Path, sid: &SessionId, budget: Duration) {
    let t0 = std::time::Instant::now();
    while listed(sock, sid).await {
        assert!(t0.elapsed() < budget, "{sid} still listed after {budget:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn journal(guard: &HomeGuard, journal_id: &str) -> String {
    let p = guard.base_dir().join("sessions").join(format!("{journal_id}.json"));
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// The journal of an owner-gone turn: the partial reply, then `Cancel`'s
/// marker.
fn assert_cancelled_journal(guard: &HomeGuard, journal_id: &str) {
    let j = journal(guard, journal_id);
    assert!(j.contains("partial"), "partial work kept: {j}");
    assert!(j.contains(InterruptReason::User.marker()), "interruption marker: {j}");
}

async fn stop_in_process(d: Daemon) {
    SocketTransport::shutdown(&d.paths.sock, false).await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), d.wait()).await.expect("daemon wait");
}

// ── lifetime ────────────────────────────────────────────────────────────────

/// The owner's stream dropped mid-turn (no Detach, no Bye: plain EOF): the
/// turn is cancelled like `Cancel`, the observer sees `Ended{OwnerGone}`,
/// the session leaves the map and its journal keeps the partial turn and
/// the marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn owner_dropped_mid_turn_cancels_the_turn_and_ends_the_session() {
    let guard = HomeGuard::new();
    let d = start_in_process(&guard, Script::Endless(ANTHROPIC_SSE_PREFIX), None).await;
    let sock = d.paths.sock.clone();
    let (owner, sid, snap) = create_owned(&sock, guard.home.path(), false).await;
    let mut obs = observe(&sock, &sid).await;
    owner.send_from_self(SessionCommand::Submit { text: "never finishes".into(), attachments: vec![] }).await.unwrap();
    until(&mut obs, is_text).await;

    drop(owner);
    expect_owner_gone(&mut obs).await;
    assert!(obs.next_event().await.is_none(), "the observer's feed ends with the session");
    wait_gone(&sock, &sid, Duration::from_secs(5)).await;
    assert_cancelled_journal(&guard, &snap.meta.journal_id);
    stop_in_process(d).await;
}

/// A clean Detach/Bye by the owner ends the session too; an observer
/// leaving does not. The journal is a normal one afterwards: `--continue`
/// loads it as an ordinary (unowned, pinnable) session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn owner_detach_ends_the_session_and_the_journal_continues_unowned() {
    let guard = HomeGuard::new();
    let d = start_in_process(&guard, Script::Sse(ANTHROPIC_SSE), None).await;
    let sock = d.paths.sock.clone();
    let (mut owner, sid, snap) = create_owned(&sock, guard.home.path(), false).await;
    owner.send_from_self(SessionCommand::Submit { text: "hello".into(), attachments: vec![] }).await.unwrap();
    until(&mut owner, |e| matches!(e, SessionEventWire::Idle)).await;

    let leaver = observe(&sock, &sid).await;
    let mut obs = observe(&sock, &sid).await;
    leaver.detach().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(listed(&sock, &sid).await, "an observer leaving never ends an owned session");

    owner.detach().await;
    expect_owner_gone(&mut obs).await;
    wait_gone(&sock, &sid, Duration::from_secs(5)).await;

    // `--continue` it: a normal session now, pinnable, outliving its client.
    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    let cfg = SessionConfig { continue_session: Some(Some(snap.meta.journal_id.clone())), ..config(guard.home.path()) };
    let (t, snap2) = SocketTransport::attach(conn, Attach::create(cfg, AttachMode::Mirror))
        .await
        .expect("continue the owned session's journal");
    assert!(!t.is_session_owner());
    assert_eq!(snap2.conversation.api_messages.len(), 2, "user + assistant");
    let sid2 = t.session_id().clone();
    assert!(SocketTransport::pin(&sock, &sid2, true).await.expect("pin a continued session"));
    t.detach().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(listed(&sock, &sid2).await, "an unowned session outlives its client");
    stop_in_process(d).await;
}

/// The owner process SIGKILLed mid-turn (`synaps attach --owned`, the line
/// client): same as a drop — turn cancelled, `Ended{OwnerGone}`, marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn owner_process_killed_mid_turn_ends_the_session() {
    let guard = HomeGuard::new();
    let (url, _hits, _bodies) = spawn_stub(Script::Endless(ANTHROPIC_SSE_PREFIX)).await;
    let d = spawn_daemon(&guard, &url, &[]).await;
    let sock = d.paths.sock.clone();

    let mut client = synaps_cmd(&guard, &url)
        .args(["attach", "--owned"])
        .current_dir(guard.home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn synaps attach --owned");
    let stdout = client.stdout.take().unwrap();
    let ready = tokio::task::spawn_blocking(move || {
        let mut r = std::io::BufReader::new(stdout);
        let mut line = String::new();
        while r.read_line(&mut line).unwrap_or(0) > 0 {
            if line.contains("○ ready") {
                return (true, r);
            }
            line.clear();
        }
        (false, r)
    });
    let (ok, _stdout) = tokio::time::timeout(Duration::from_secs(20), ready).await.expect("ready in time").unwrap();
    assert!(ok, "`synaps attach --owned` never printed ready");

    let metas = SocketTransport::sessions(&sock).await.unwrap();
    assert_eq!(metas.len(), 1);
    let sid = metas[0].id.clone();
    let journal_id = metas[0].journal_id.clone();
    let mut obs = observe(&sock, &sid).await;
    let mut stdin = client.stdin.take().unwrap();
    writeln!(stdin, "never finishes").unwrap();
    stdin.flush().unwrap();
    until(&mut obs, is_text).await;

    client.kill().expect("SIGKILL the owner");
    let _ = client.wait();
    expect_owner_gone(&mut obs).await;
    wait_gone(&sock, &sid, Duration::from_secs(5)).await;
    assert_cancelled_journal(&guard, &journal_id);
    drop(stdin);
    SocketTransport::shutdown(&sock, true).await.unwrap();
}

/// Pinning an owned session is refused, from the control connection
/// (`synaps daemon pin`) and from inside (`/pin`), with the reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn pinning_an_owned_session_is_refused() {
    let guard = HomeGuard::new();
    let d = start_in_process(&guard, Script::Sse(ANTHROPIC_SSE), None).await;
    let sock = d.paths.sock.clone();
    let (mut owner, sid, _) = create_owned(&sock, guard.home.path(), false).await;

    let err = SocketTransport::pin(&sock, &sid, true).await.expect_err("pin refused");
    assert!(matches!(err, TransportError::Refused(ref m) if m.contains("cannot be pinned")), "{err}");
    owner.send_from_self(SessionCommand::KeepWarm { on: true }).await.unwrap();
    let seen = until(&mut owner, |e| matches!(e, SessionEventWire::Refused { .. })).await;
    assert!(
        matches!(&seen.last().unwrap().event, SessionEventWire::Refused { reason, .. } if reason.contains("cannot be pinned")),
        "{:?}",
        seen.last().unwrap().event
    );
    let metas = SocketTransport::sessions(&sock).await.unwrap();
    assert!(!metas.iter().find(|m| m.id == sid).unwrap().pinned);
    // Nor created pinned.
    let cfg = SessionConfig { keep_warm: true, ..config(guard.home.path()) };
    let err = SocketTransport::create_owned(&sock, Hello::new(ClientKind::Test), cfg, AttachMode::Mirror, false)
        .await
        .err()
        .expect("owned + keep_warm refused");
    assert!(matches!(err, TransportError::Refused(ref m) if m.contains("cannot be pinned")), "{err}");
    assert_eq!(SocketTransport::sessions(&sock).await.unwrap().len(), 1, "nothing was created");
    owner.detach().await;
    wait_gone(&sock, &sid, Duration::from_secs(5)).await;
    stop_in_process(d).await;
}

/// The auto-spawned daemon's idle exit: a live owner holds it like any
/// client; once the owner leaves, the owned session is gone with it and the
/// daemon exits (an unowned session would have stayed, parked or idle).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn idle_exit_fires_once_the_owned_session_ends() {
    let guard = HomeGuard::new();
    let d = start_in_process(&guard, Script::Sse(ANTHROPIC_SSE), Some(Duration::from_millis(300))).await;
    let sock = d.paths.sock.clone();
    let (owner, sid, _) = create_owned(&sock, guard.home.path(), false).await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(!d.state.shutdown.is_cancelled(), "a live owner blocks idle-exit");
    owner.detach().await;
    tokio::time::timeout(Duration::from_secs(10), d.wait()).await.expect("idle-exit after the owned session ended");
    assert!(!sock.exists());
    let _ = sid;
}

// ── the owner's lease ───────────────────────────────────────────────────────

/// An owned session built the way the daemon's create builds one, its owner
/// claim handed straight to the actor with `lease` as the connection's
/// liveness: what a connection task does, minus the `Detach` on its way out.
async fn owned_via_host(d: &Daemon, cwd: &Path, restored: bool) -> (SessionHandle, OwnerToken) {
    let token = OwnerToken::mint();
    let cfg = SessionConfig { owned: Some(OwnedConfig { token: token.clone(), restored }), ..config(cwd) };
    (d.state.create(cfg).await.expect("create"), token)
}

async fn bind_via_host(h: &SessionHandle, token: &OwnerToken, lease: &tokio_util::sync::CancellationToken) {
    let claim = OwnerClaim { token: token.clone(), forward_secrets: None };
    h.send(SessionCommand::Attach {
        client: Hello::new(ClientKind::Test).client,
        mode: AttachMode::Mirror,
        owner: Some(OwnerBind { claim, alive: lease.clone() }),
    })
    .await
    .unwrap();
}

/// The owner's `Detach` never arrives (a full command queue, a connection
/// task that panicked): its connection's lease closing alone ends the
/// session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_owner_lease_closing_ends_the_session_without_a_detach() {
    let guard = HomeGuard::new();
    let d = start_in_process(&guard, Script::Sse(ANTHROPIC_SSE), None).await;
    let sock = d.paths.sock.clone();
    let (h, token) = owned_via_host(&d, guard.home.path(), false).await;
    let mut obs = observe(&sock, &h.id).await;
    let lease = tokio_util::sync::CancellationToken::new();
    bind_via_host(&h, &token, &lease).await;
    let t0 = std::time::Instant::now();
    while h.presence().owning_client.is_none() {
        assert!(t0.elapsed() < Duration::from_secs(5), "the owner never bound");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    lease.cancel();
    expect_owner_gone(&mut obs).await;
    wait_gone(&sock, &h.id, Duration::from_secs(5)).await;
    stop_in_process(d).await;
}

/// The creating connection gave up on the attach (`ATTACH_TIMEOUT`) and
/// went away before the actor even read the claim. Nobody else ever had the
/// token: the claim never binds, nobody is attached for it, and the session
/// ends on the spot rather than after the grace (30 s here).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_claim_whose_connection_is_already_gone_ends_the_session() {
    let guard = HomeGuard::new();
    let d = start_in_process(&guard, Script::Sse(ANTHROPIC_SSE), None).await;
    let sock = d.paths.sock.clone();
    let (h, token) = owned_via_host(&d, guard.home.path(), false).await;
    let mut obs = observe(&sock, &h.id).await;
    let lease = tokio_util::sync::CancellationToken::new();
    lease.cancel();
    let t0 = std::time::Instant::now();
    bind_via_host(&h, &token, &lease).await;
    expect_owner_gone(&mut obs).await;
    assert!(t0.elapsed() < Duration::from_secs(5), "ended at once, not after the grace: {:?}", t0.elapsed());
    assert!(h.presence().owning_client.is_none(), "the dead claim never bound");
    wait_gone(&sock, &h.id, Duration::from_secs(5)).await;
    stop_in_process(d).await;
}

// ── restored sessions ───────────────────────────────────────────────────────

struct EnvVar(&'static str);
impl EnvVar {
    fn set(name: &'static str, value: &str) -> Self {
        std::env::set_var(name, value);
        Self(name)
    }
}
impl Drop for EnvVar {
    fn drop(&mut self) {
        std::env::remove_var(self.0);
    }
}

/// Held for its `Drop` (unsets the var).
struct GraceVar(#[allow(dead_code)] EnvVar);
impl GraceVar {
    fn set(secs: &str) -> Self {
        Self(EnvVar::set("SYNAPS_DAEMON_OWNER_REBIND_SECS", secs))
    }
}

/// Queue an event for `h` on its session socket (an autonomous turn).
async fn queue_event(h: &SessionHandle, id: &str) {
    let mut ev = agent_engine::events::types::Event::simple("owned-test", "wake up", None);
    ev.id = id.into();
    let path = agent_engine::events::registry::find_session_registration(h.id.as_str())
        .expect("published session endpoint")
        .socket_path;
    agent_engine::events::socket::send_to_session_socket(&path, serde_json::to_string(&ev).unwrap().as_bytes())
        .await
        .expect("session socket");
}

/// The owner re-binding over the socket with `token` (as `reconnect` does).
async fn rebind(sock: &Path, sid: &SessionId, token: &OwnerToken) -> SocketTransport {
    let mut hello = Hello::new(ClientKind::Test);
    hello.reconnect_of = Some(ClientReconnect {
        previous_client: ClientId(1),
        session_id: sid.clone(),
        was_owner: true,
        generation: 1,
        owner: Some(OwnerClaim { token: token.clone(), forward_secrets: None }),
    });
    let conn = SocketTransport::connect(sock, hello).await.unwrap();
    let (owner, _) = SocketTransport::attach(conn, Attach::Existing { session_id: sid.clone(), mode: AttachMode::Mirror })
        .await
        .expect("the owner re-binds");
    assert!(owner.is_session_owner());
    owner
}

fn hits_now(hits: &std::sync::atomic::AtomicUsize) -> usize {
    hits.load(std::sync::atomic::Ordering::SeqCst)
}

/// A session a reload restores waits for `ArmOwnerRebind` (sent once the
/// new image accepts connections) before its grace starts; then it ends if
/// the owner never comes back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_restored_owned_session_starts_its_grace_only_when_armed() {
    let guard = HomeGuard::new();
    let _grace = GraceVar::set("1");
    let d = start_in_process(&guard, Script::Sse(ANTHROPIC_SSE), None).await;
    let sock = d.paths.sock.clone();
    let (h, _token) = owned_via_host(&d, guard.home.path(), true).await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(listed(&sock, &h.id).await, "no grace runs before the daemon accepts connections");

    h.send(SessionCommand::ArmOwnerRebind).await.unwrap();
    wait_gone(&sock, &h.id, Duration::from_secs(6)).await;
    stop_in_process(d).await;
}

/// A restored owned session starts no turn (here: an event queued for it)
/// before its owner re-binds; once it does, the held event runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_restored_owned_session_holds_its_work_until_the_owner_rebinds() {
    let guard = HomeGuard::new();
    let (d, hits) = start_counted(&guard, Script::Sse(ANTHROPIC_SSE), None).await;
    let sock = d.paths.sock.clone();
    let (h, token) = owned_via_host(&d, guard.home.path(), true).await;
    queue_event(&h, "evt-owned-hold").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(hits_now(&hits), 0, "no turn without the owner");

    let mut owner = rebind(&sock, &h.id, &token).await;
    until(&mut owner, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    until(&mut owner, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(hits.load(std::sync::atomic::Ordering::SeqCst) > 0, "the held event ran");
    owner.detach().await;
    wait_gone(&sock, &h.id, Duration::from_secs(5)).await;
    stop_in_process(d).await;
}

/// The daemon's `ArmOwnerRebind` never reaches a restored owned session (a
/// full queue, a lost send): its fallback deadline, the restore cap plus
/// the grace, still ends it. Nothing leaks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_restored_owned_session_never_armed_ends_at_its_fallback_deadline() {
    let guard = HomeGuard::new();
    let _grace = GraceVar::set("1");
    let _cap = EnvVar::set("SYNAPS_DAEMON_OWNER_RESTORE_CAP_SECS", "2");
    let d = start_in_process(&guard, Script::Sse(ANTHROPIC_SSE), None).await;
    let sock = d.paths.sock.clone();
    let t0 = std::time::Instant::now();
    let (h, _token) = owned_via_host(&d, guard.home.path(), true).await;
    let mut obs = observe(&sock, &h.id).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(listed(&sock, &h.id).await, "the grace alone (1 s) does not end an unarmed session");
    expect_owner_gone(&mut obs).await;
    assert!(t0.elapsed() >= Duration::from_millis(2900), "not before cap + grace: {:?}", t0.elapsed());
    wait_gone(&sock, &h.id, Duration::from_secs(5)).await;
    stop_in_process(d).await;
}

/// A re-bind whose connection already gave up (attempt A timed out while
/// the actor was busy; retry B is on its way): the claim never binds,
/// releases nothing the session holds, and the grace keeps running. B then
/// re-binds and the held work runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_rebind_whose_connection_is_gone_is_ignored_and_the_grace_runs_on() {
    let guard = HomeGuard::new();
    let (d, hits) = start_counted(&guard, Script::Sse(ANTHROPIC_SSE), None).await;
    let sock = d.paths.sock.clone();
    let (h, token) = owned_via_host(&d, guard.home.path(), true).await;
    h.send(SessionCommand::ArmOwnerRebind).await.unwrap();
    queue_event(&h, "evt-owned-dead-rebind").await;

    let dead = tokio_util::sync::CancellationToken::new();
    dead.cancel();
    bind_via_host(&h, &token, &dead).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(listed(&sock, &h.id).await, "a stale attempt does not end the session");
    let presence = h.presence();
    assert!(presence.owning_client.is_none(), "it never bound");
    assert_eq!(presence.clients, 0, "nobody was attached for it");
    assert_eq!(hits_now(&hits), 0, "nothing held was released");

    let mut owner = rebind(&sock, &h.id, &token).await;
    until(&mut owner, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    until(&mut owner, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(hits_now(&hits) > 0, "the held event ran once the owner was back");
    owner.detach().await;
    wait_gone(&sock, &h.id, Duration::from_secs(5)).await;
    stop_in_process(d).await;
}

/// The owner leaves while the session is fenced for a reload, and the exec
/// fails (`ReloadAborted`): the fence lifts, but what queued meanwhile waits
/// for the owner's re-bind, as on a restored session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_failed_reload_holds_the_work_until_the_owner_rebinds() {
    let guard = HomeGuard::new();
    let (d, hits) = start_counted(&guard, Script::Sse(ANTHROPIC_SSE), None).await;
    let sock = d.paths.sock.clone();
    let (h, token) = owned_via_host(&d, guard.home.path(), false).await;
    let owner = rebind(&sock, &h.id, &token).await;
    let mut obs = observe(&sock, &h.id).await;
    let mut rx = h.subscribe();
    h.send(SessionCommand::Checkpoint { reason: CheckpointReason::Reload }).await.unwrap();
    loop {
        let env = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("checkpointed").unwrap();
        if matches!(env.event, SessionEventWire::QueryResult { .. }) {
            break;
        }
    }
    owner.detach().await;
    queue_event(&h, "evt-owned-failed-exec").await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    h.send(SessionCommand::ReloadAborted).await.unwrap();
    until(&mut obs, |e| matches!(e, SessionEventWire::SystemNotice(n) if n.starts_with("daemon reload failed"))).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(listed(&sock, &h.id).await, "the owner's grace is running");
    assert_eq!(hits_now(&hits), 0, "no turn while the owner is away");

    let mut owner = rebind(&sock, &h.id, &token).await;
    until(&mut owner, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    until(&mut owner, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(hits_now(&hits) > 0, "the held event ran once the owner was back");
    owner.detach().await;
    expect_owner_gone(&mut obs).await;
    stop_in_process(d).await;
}

// ── only the owner drives ───────────────────────────────────────────────────

/// Anyone else attached to an owned session watches: input, `/close`,
/// `/takeover` and a takeover attach are refused with the reason; the
/// owner's input still works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn only_the_owner_drives_an_owned_session() {
    let guard = HomeGuard::new();
    let d = start_in_process(&guard, Script::Sse(ANTHROPIC_SSE), None).await;
    let sock = d.paths.sock.clone();
    let (mut owner, sid, _) = create_owned(&sock, guard.home.path(), false).await;

    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    let err = SocketTransport::attach(conn, Attach::Existing { session_id: sid.clone(), mode: AttachMode::Takeover })
        .await
        .err()
        .expect("takeover refused");
    assert!(matches!(err, TransportError::Refused(ref m) if m.contains("cannot be taken over")), "{err}");

    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (mut mirror, snap) = SocketTransport::attach(conn, Attach::Existing { session_id: sid.clone(), mode: AttachMode::Mirror })
        .await
        .expect("mirror");
    assert_ne!(snap.input_owner, Some(mirror.client_id()), "a mirror never gets input");
    for cmd in [
        SessionCommand::Submit { text: "not yours".into(), attachments: vec![] },
        SessionCommand::EngineCommand { id: 7, name: "close".into(), arg: String::new() },
        SessionCommand::TakeInput,
        SessionCommand::Cancel,
    ] {
        mirror.send_from_self(cmd).await.unwrap();
        let seen = until(&mut mirror, |e| matches!(e, SessionEventWire::Refused { .. })).await;
        assert!(
            matches!(&seen.last().unwrap().event, SessionEventWire::Refused { reason, .. } if reason.contains("only that client can drive it")),
            "{:?}",
            seen.last().unwrap().event
        );
    }
    assert!(listed(&sock, &sid).await, "/close from a watcher ended nothing");

    owner.send_from_self(SessionCommand::Submit { text: "hello".into(), attachments: vec![] }).await.unwrap();
    until(&mut owner, |e| matches!(e, SessionEventWire::Idle)).await;
    // Not even the owner turns on cache keep-warm: spend it never sees.
    owner
        .send_from_self(SessionCommand::EngineCommand { id: 8, name: "keepwarm".into(), arg: "auto".into() })
        .await
        .unwrap();
    let seen = until(&mut owner, |e| matches!(e, SessionEventWire::QueryResult { id: 8, .. })).await;
    let SessionEventWire::QueryResult { value, .. } = &seen.last().unwrap().event else { unreachable!() };
    assert_eq!(value["kind"], "error", "{value}");
    assert!(value["text"].as_str().unwrap().contains("never keeps the cache warm"), "{value}");
    mirror.detach().await;
    owner.detach().await;
    wait_gone(&sock, &sid, Duration::from_secs(5)).await;
    stop_in_process(d).await;
}

// ── continue ────────────────────────────────────────────────────────────────

async fn create_plain_session(sock: &Path, cwd: &Path) -> SocketTransport {
    let conn = SocketTransport::connect(sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (t, _) = SocketTransport::attach(conn, Attach::create(config(cwd), AttachMode::Mirror)).await.expect("create plain");
    t
}

/// An owned create continuing a session that is live here with nobody
/// attached ends it (saved) and adopts its journal as owned; with a client
/// attached it is refused, saying how many.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_owned_continue_adopts_a_clientless_live_session_and_refuses_an_attached_one() {
    let guard = HomeGuard::new();
    let d = start_in_process(&guard, Script::Sse(ANTHROPIC_SSE), None).await;
    let sock = d.paths.sock.clone();
    let mut plain = create_plain_session(&sock, guard.home.path()).await;
    let sid = plain.session_id().clone();
    plain.send_from_self(SessionCommand::Submit { text: "hello".into(), attachments: vec![] }).await.unwrap();
    until(&mut plain, |e| matches!(e, SessionEventWire::Idle)).await;

    let cont = || SessionConfig { continue_session: Some(Some(sid.as_str().to_string())), ..config(guard.home.path()) };
    let err = SocketTransport::create_owned(&sock, Hello::new(ClientKind::Test), cont(), AttachMode::Mirror, false)
        .await
        .err()
        .expect("attached: refused");
    assert!(matches!(err, TransportError::Refused(ref m) if m.contains("attached by 1 client(s)")), "{err}");
    assert!(listed(&sock, &sid).await, "the refused create left it alone");

    plain.detach().await;
    let (mut owner, snap) = SocketTransport::create_owned(&sock, Hello::new(ClientKind::Test), cont(), AttachMode::Mirror, false)
        .await
        .expect("client-less: adopted");
    assert!(owner.is_session_owner());
    assert_eq!(snap.meta.journal_id, sid.as_str(), "the same journal");
    assert_eq!(snap.conversation.api_messages.len(), 2, "its conversation came along");
    let metas = SocketTransport::sessions(&sock).await.unwrap();
    assert_eq!(metas.len(), 1, "one session: the plain one ended {metas:?}");
    owner.send_from_self(SessionCommand::Submit { text: "again".into(), attachments: vec![] }).await.unwrap();
    let seen = until(&mut owner, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(
        !seen.iter().any(|e| matches!(&e.event, SessionEventWire::SystemNotice(n) if n.contains("already running"))),
        "an adopted journal is a new session, not the live one attached to"
    );
    let owned_id = owner.session_id().clone();
    owner.detach().await;
    wait_gone(&sock, &owned_id, Duration::from_secs(5)).await;
    stop_in_process(d).await;
}

/// A client attaches to the continue target while the owned create is
/// adopting it: the attach reached the actor first (a parked target is
/// still waking up for it, so the daemon's presence still says nobody is
/// attached). The actor refuses the close; the create is refused and the
/// session stays with its client.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_owned_continue_never_closes_a_session_a_client_is_attaching_to() {
    let guard = HomeGuard::new();
    let d = start_in_process(&guard, Script::Sse(ANTHROPIC_SSE), None).await;
    let sock = d.paths.sock.clone();
    let mut plain = create_plain_session(&sock, guard.home.path()).await;
    let sid = plain.session_id().clone();
    plain.send_from_self(SessionCommand::Submit { text: "hello".into(), attachments: vec![] }).await.unwrap();
    until(&mut plain, |e| matches!(e, SessionEventWire::Idle)).await;
    plain.detach().await;
    let h = d.state.attach(&sid).expect("live");
    let t0 = std::time::Instant::now();
    while h.presence().clients > 0 {
        assert!(t0.elapsed() < Duration::from_secs(10), "never detached");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    h.send(SessionCommand::Park).await.unwrap();
    while h.lifecycle() != SessionLifecycle::Parked {
        assert!(t0.elapsed() < Duration::from_secs(10), "never parked");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut rx = h.subscribe();
    h.send(SessionCommand::Attach { client: Hello::new(ClientKind::Test).client, mode: AttachMode::Mirror, owner: None })
        .await
        .unwrap();
    let cont = SessionConfig { continue_session: Some(Some(sid.as_str().to_string())), ..config(guard.home.path()) };
    let err = SocketTransport::create_owned(&sock, Hello::new(ClientKind::Test), cont, AttachMode::Mirror, false)
        .await
        .err()
        .expect("a client is attached: refused");
    assert!(matches!(err, TransportError::Refused(ref m) if m.contains("attached by 1 client(s)")), "{err}");
    let client = loop {
        match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("attached").unwrap().event {
            SessionEventWire::Attached { client, .. } => break client,
            _ => continue,
        }
    };
    assert!(listed(&sock, &sid).await, "not closed under its client");
    assert_eq!(h.presence().clients, 1);
    h.send(SessionCommand::Detach { client }).await.unwrap();
    stop_in_process(d).await;
}

// ── forward_secrets ─────────────────────────────────────────────────────────

/// Needs the `bash` tool (`command-exec`).
#[cfg(feature = "command-exec")]
mod forward_secrets {
    use super::*;
    use std::path::PathBuf;

    async fn create_plain(sock: &Path, cwd: &Path) -> SocketTransport {
        let conn = SocketTransport::connect(sock, Hello::new(ClientKind::Test)).await.unwrap();
        let (t, _) = SocketTransport::attach(conn, Attach::create(config(cwd), AttachMode::Mirror))
            .await
            .expect("create plain");
        assert!(!t.is_session_owner());
        t
    }

    const DUMMY_SECRET: &str = "sdo3-dummy-secret-7f2c91ab5e";

    /// Sets `GH_TOKEN` to the dummy secret; restores the previous value on
    /// drop, even if the test fails.
    struct SecretVar(Option<String>);
    impl SecretVar {
        fn set() -> Self {
            let prev = std::env::var("GH_TOKEN").ok();
            std::env::set_var("GH_TOKEN", DUMMY_SECRET);
            Self(prev)
        }
    }
    impl Drop for SecretVar {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var("GH_TOKEN", v),
                None => std::env::remove_var("GH_TOKEN"),
            }
        }
    }

    fn hex(s: &str) -> String {
        s.bytes().map(|b| format!("{b:02x}")).collect()
    }

    fn sse_frames(blocks: &str, stop: &str) -> String {
        format!(
            concat!(
                "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_ow\",\"type\":\"message\",",
                "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
                "\"stop_sequence\":null,\"usage\":{{\"input_tokens\":10,\"output_tokens\":0,",
                "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}}}\n\n",
                "{blocks}",
                "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop}\",",
                "\"stop_sequence\":null}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":5,",
                "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}\n\n",
                "data: {{\"type\":\"message_stop\"}}\n\n",
            ),
            blocks = blocks,
            stop = stop,
        )
    }

    /// `bash` printing `$GH_TOKEN` hex-encoded: the tool proves it saw the
    /// value without the value itself ever entering history.
    fn sse_bash_secret_hex() -> String {
        let input = serde_json::json!({"command": "printf '%s' \"$GH_TOKEN\" | od -An -tx1 | tr -d ' \\n'; echo"});
        let start = serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_sec","name":"bash","input":{}}});
        let delta = serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input.to_string()}});
        let stop = serde_json::json!({"type":"content_block_stop","index":0});
        sse_frames(&format!("data: {start}\n\ndata: {delta}\n\ndata: {stop}\n\n"), "tool_use")
    }

    fn sse_done() -> String {
        let start = serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}});
        let delta = serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}});
        let stop = serde_json::json!({"type":"content_block_stop","index":0});
        sse_frames(&format!("data: {start}\n\ndata: {delta}\n\ndata: {stop}\n\n"), "end_turn")
    }

    /// One turn running the hex tool; returns the tool's output.
    async fn secret_turn(t: &mut SocketTransport) -> String {
        t.send_from_self(SessionCommand::Submit { text: "show the token".into(), attachments: vec![] }).await.unwrap();
        let mut result = None;
        for e in until(t, |e| matches!(e, SessionEventWire::Idle)).await {
            if let SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::ToolResult { result: r, .. })) = e.event {
                result = Some(r);
            }
        }
        result.expect("the bash tool ran")
    }

    /// Every file under `dir` whose bytes contain `needle`.
    fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
        let mut hits = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for entry in rd.flatten() {
                let p = entry.path();
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_dir() {
                    stack.push(p);
                } else if ft.is_file() {
                    if let Ok(bytes) = std::fs::read(&p) {
                        if bytes.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                            hits.push(p);
                        }
                    }
                }
            }
        }
        hits
    }

    /// `forward_secrets` on an owned session: the tool sees the client's
    /// `GH_TOKEN` (the daemon process has none). An unowned session from the
    /// same client still gets it stripped. Nothing under HOME — journals,
    /// session index, reload/run files, the daemon's debug log and stderr —
    /// ever holds the value.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn forwarded_secrets_reach_the_tools_and_never_the_disk() {
        let guard = HomeGuard::new();
        let bodies: &'static [&'static str] = Box::leak(
            vec![sse_bash_secret_hex(), sse_done(), sse_bash_secret_hex(), sse_done()]
                .into_iter()
                .map(|s| &*Box::leak(s.into_boxed_str()))
                .collect::<Vec<&'static str>>()
                .into_boxed_slice(),
        );
        let (url, _hits, _bodies) = spawn_stub(Script::SeqSse(bodies)).await;
        // Temp files (a tool's, the daemon's) land under HOME too, so the
        // grep below covers them.
        let tmp = guard.home.path().join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let d = spawn_daemon(&guard, &url, &[("RUST_LOG", "trace"), ("TMPDIR", tmp.to_str().unwrap())]).await;
        let sock = d.paths.sock.clone();

        let _secret = SecretVar::set();
        assert!(Hello::new(ClientKind::Test).env.unwrap().iter().all(|(k, _)| k != "GH_TOKEN"), "Hello strips it as ever");

        let (mut owner, sid, snap) = create_owned(&sock, guard.home.path(), true).await;
        let seen = secret_turn(&mut owner).await;
        assert_eq!(seen.trim(), hex(DUMMY_SECRET), "the owned session's tool saw the forwarded secret");

        let mut plain = create_plain(&sock, guard.home.path()).await;
        let seen = secret_turn(&mut plain).await;
        assert_eq!(seen.trim(), "", "an unowned session's tool never sees it");
        plain.detach().await;

        owner.detach().await;
        wait_gone(&sock, &sid, Duration::from_secs(5)).await;
        SocketTransport::shutdown(&sock, false).await.unwrap();
        let t0 = std::time::Instant::now();
        while registry::is_alive(&d.paths) && t0.elapsed() < Duration::from_secs(20) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // The journal is where we look: it holds the tool's (hex) output…
        assert!(journal(&guard, &snap.meta.journal_id).contains(&hex(DUMMY_SECRET)));
        // …and nothing anywhere holds the secret.
        let logs: Vec<PathBuf> = std::fs::read_dir(guard.base_dir())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("synaps.log")))
            .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.len() > 0))
            .collect();
        assert!(!logs.is_empty(), "the daemon's trace log is under HOME, so the grep covers it");
        assert!(tmp.starts_with(guard.home.path()), "and so is its TMPDIR");
        let hits = files_containing(guard.home.path(), DUMMY_SECRET);
        assert!(hits.is_empty(), "the forwarded secret reached disk: {hits:?}");
    }
}

// ── reload ──────────────────────────────────────────────────────────────────

/// `synaps daemon reload`: the owner re-binds with its token after the exec
/// and still owns the session (its departure ends it); the conversation
/// survived.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn reload_lets_the_owner_rebind_within_the_grace() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let d = spawn_daemon(&guard, &url, &[]).await;
    let sock = d.paths.sock.clone();
    let (mut owner, sid, _) = create_owned(&sock, guard.home.path(), false).await;
    owner.send_from_self(SessionCommand::Submit { text: "hello".into(), attachments: vec![] }).await.unwrap();
    until(&mut owner, |e| matches!(e, SessionEventWire::Idle)).await;

    assert_eq!(SocketTransport::reload(&sock, true, None, None).await.expect("reload"), 2);
    while tokio::time::timeout(Duration::from_secs(10), owner.next_event()).await.expect("announce").is_some() {}
    assert!(matches!(owner.last_error(), Some(TransportError::Reloading { generation: 2 })));
    let snap = tokio::time::timeout(Duration::from_secs(15), owner.reconnect(AttachMode::Mirror))
        .await
        .expect("reconnect budget")
        .expect("re-bound");
    assert!(owner.is_session_owner(), "re-bound as the owner");
    assert_eq!(owner.session_id(), &sid);
    assert_eq!(snap.conversation.api_messages.len(), 2, "conversation survived");

    // Still owned: the owner leaving ends it.
    let mut obs = observe(&sock, &sid).await;
    owner.detach().await;
    expect_owner_gone(&mut obs).await;
    wait_gone(&sock, &sid, Duration::from_secs(5)).await;
    SocketTransport::shutdown(&sock, false).await.unwrap();
}

/// No re-bind within the grace (`SYNAPS_DAEMON_OWNER_REBIND_SECS`): the
/// restored session ends instead of leaking. A stranger's token never binds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn reload_without_a_rebind_ends_the_session_after_the_grace() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let d = spawn_daemon(&guard, &url, &[("SYNAPS_DAEMON_OWNER_REBIND_SECS", "6")]).await;
    let sock = d.paths.sock.clone();
    let (mut owner, sid, snap) = create_owned(&sock, guard.home.path(), false).await;
    owner.send_from_self(SessionCommand::Submit { text: "hello".into(), attachments: vec![] }).await.unwrap();
    until(&mut owner, |e| matches!(e, SessionEventWire::Idle)).await;

    assert_eq!(SocketTransport::reload(&sock, true, None, None).await.expect("reload"), 2);
    drop(owner);
    let t0 = std::time::Instant::now();
    while SocketTransport::ping(&sock).await.map(|_| ()).is_err() || !listed(&sock, &sid).await {
        assert!(t0.elapsed() < Duration::from_secs(20), "the owned session should come back for its grace");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(registry::read_daemon_json(&d.paths).unwrap().generation, 2);

    // A forged claim is refused loudly and binds nothing.
    let mut hello = Hello::new(ClientKind::Test);
    hello.reconnect_of = Some(ClientReconnect {
        previous_client: ClientId(1),
        session_id: sid.clone(),
        was_owner: true,
        generation: 2,
        owner: Some(OwnerClaim { token: OwnerToken("forged".into()), forward_secrets: None }),
    });
    let conn = SocketTransport::connect(&sock, hello).await.unwrap();
    let err = SocketTransport::attach(conn, Attach::Existing { session_id: sid.clone(), mode: AttachMode::Mirror })
        .await
        .err()
        .expect("forged token refused");
    assert!(matches!(err, TransportError::Refused(ref m) if m.contains("owner token rejected")), "{err}");

    wait_gone(&sock, &sid, Duration::from_secs(15)).await;
    assert!(journal(&guard, &snap.meta.journal_id).contains("hello"), "the journal stays");
    SocketTransport::shutdown(&sock, false).await.unwrap();
}
