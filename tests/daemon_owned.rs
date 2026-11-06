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
    let (url, _hits, _bodies) = spawn_stub(script).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host: Arc<EngineHost> = EngineHost::boot_and_install(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .expect("host boot");
    Daemon::start(host, DaemonOpts { runtime_dir: Some(guard.base_dir().join("run")), idle_exit, ..Default::default() })
        .await
        .expect("daemon start")
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
        let d = spawn_daemon(&guard, &url, &[("RUST_LOG", "trace")]).await;
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
