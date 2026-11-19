//! While the sandbox is on under the daemon's base dir (its own config or any
//! profile's says `sandbox.confine = enforce`), a client inside a Synaps
//! sandbox can't drive the daemon, even after it has left its parent's
//! session and process tree (setsid, then a second fork: the grandchild is
//! reparented). Each of its connections is answered `Refused` after Hello
//! and closed: no session is created, a `Cmd` to a live session and
//! `Shutdown` are not served, and the daemon keeps serving everyone else.
//! The same detached client, unsandboxed, is served (the control). With the
//! sandbox off everywhere under the base the uid check alone admits, so a
//! client with no_new_privs (a systemd unit with `NoNewPrivileges=`) is
//! served. Linux only: elsewhere no Synaps sandbox exists.
#![cfg(target_os = "linux")]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::{self, Daemon, DaemonOpts};
use agent_engine::sandbox::landlock;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts};
use phase2::HomeGuard;
use serial_test::serial;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// `true` when this kernel has usable Landlock (without it no process can be
/// in a Synaps sandbox, and the daemon refuses nobody); says so when not.
fn landlock_ok(test: &str) -> bool {
    match landlock::usable_abi() {
        Ok(abi) => {
            eprintln!("{test}: landlock abi {abi}");
            true
        }
        Err(why) => {
            if std::env::var_os("SYNAPS_REQUIRE_LANDLOCK").is_some_and(|v| v == "1") {
                panic!("{test}: SYNAPS_REQUIRE_LANDLOCK=1, but this host has no usable Landlock ({why:?})");
            }
            eprintln!("SKIPPED {test}: no usable Landlock on this host ({why:?})");
            false
        }
    }
}

async fn start(runtime_dir: &Path) -> Daemon {
    let host: Arc<EngineHost> = EngineHost::boot_and_install(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .expect("host boot");
    Daemon::start(
        host,
        DaemonOpts { runtime_dir: Some(runtime_dir.to_path_buf()), factory: Some(daemon::echo_factory()), ..Default::default() },
    )
    .await
    .expect("daemon start")
}

struct Raw {
    r: BufReader<agent_engine::ipc::ClientReadHalf>,
    w: agent_engine::ipc::ClientWriteHalf,
}

impl Raw {
    async fn connect(p: &Path) -> Self {
        let (r, w) = agent_engine::ipc::split_client(agent_engine::ipc::connect(p).await.unwrap());
        Self { r: BufReader::new(r), w }
    }
    async fn send(&mut self, f: &ClientFrame) {
        self.w.write_all(encode_line(f).unwrap().as_bytes()).await.unwrap();
    }
    async fn recv(&mut self) -> Option<DaemonFrame> {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(5), self.r.read_line(&mut line)).await.expect("timely").unwrap();
        if n == 0 {
            return None;
        }
        Some(decode_line(line.trim_end()).unwrap())
    }
}

fn line(f: &ClientFrame) -> String {
    encode_line(f).unwrap().trim_end().to_string()
}

/// How the detached client is started.
#[derive(Clone, Copy, Debug)]
enum How {
    /// The real confined-spawn path: a Landlock domain (granting only the
    /// client's own folder) applied before exec, which sets no_new_privs.
    Landlock,
    /// Only no_new_privs, no Landlock domain: the flag is the marker.
    NoNewPrivsOnly,
    /// Neither: the control.
    Unsandboxed,
}

/// Run the detached client against `sock` with one connection per frame,
/// and return its record (blocking: call from a blocking thread).
fn run_client(sock: &Path, dir: &Path, frames: &[String], how: How) -> serde_json::Value {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/detached_daemon_client.py");
    let out = dir.join("record.json");
    let mut cmd = Command::new("python3");
    cmd.arg(&script);
    if matches!(how, How::NoNewPrivsOnly) {
        cmd.arg("--no-new-privs");
    }
    cmd.arg(sock).arg(&out).arg(line(&ClientFrame::Hello(Hello::new(ClientKind::Test)))).args(frames);
    if matches!(how, How::Landlock) {
        let mut ruleset = landlock::Ruleset::new().expect("ruleset");
        assert!(ruleset.allow(dir).expect("allow the client's folder"));
        landlock::confine_command(&mut cmd, &ruleset).expect("confine");
    }
    let status = cmd.status().expect("spawn python3");
    assert!(status.success(), "{how:?}: the client's first process failed: {status}");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !out.exists() {
        assert!(std::time::Instant::now() < deadline, "{how:?}: the detached client wrote no record");
        std::thread::sleep(Duration::from_millis(20));
    }
    serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap()
}

/// The record shows a grandchild in a session of its own, reparented away
/// from the process that started it.
fn assert_detached(record: &serde_json::Value, how: How) {
    let n = |k: &str| record[k].as_i64().unwrap_or_else(|| panic!("{how:?}: {k} missing in {record}"));
    assert_eq!(n("sid"), n("middle"), "{how:?}: the grandchild is in the session its parent's setsid made");
    assert_ne!(n("sid"), n("top_sid"), "{how:?}: setsid left the test's session");
    assert_ne!(n("ppid"), n("middle"), "{how:?}: the grandchild was reparented");
    assert_ne!(n("pid"), n("top"));
}

fn replies(record: &serde_json::Value) -> Vec<(String, String)> {
    record["replies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["first"].as_str().unwrap().to_string(), r["after"].as_str().unwrap().to_string()))
        .collect()
}

/// Writes an owner-set config file (making its folder).
fn write_config(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Runs the detached client `how` with `frames` (one connection each).
async fn detached(sock: &Path, scratch: &Path, frames: &[String], how: How) -> serde_json::Value {
    let dir: PathBuf = tempfile::tempdir_in(scratch).unwrap().keep();
    let (s, fr) = (sock.to_path_buf(), frames.to_vec());
    let record = tokio::task::spawn_blocking(move || run_client(&s, &dir, &fr, how)).await.unwrap();
    assert_detached(&record, how);
    record
}

/// Every connection of `record` got `Refused` (the sandbox text) after
/// Hello and was closed.
fn assert_refused(record: &serde_json::Value, frames: &[String], how: How) {
    let got = replies(record);
    assert_eq!(got.len(), frames.len());
    for ((first, after), sent) in got.iter().zip(frames) {
        match decode_line(first.trim_end()) {
            Ok(DaemonFrame::Refused { reason: RefuseReason::Protocol, message }) => {
                assert!(message.contains("inside a Synaps sandbox"), "{how:?}: {message}");
            }
            other => panic!("{how:?}: {sent} was answered {other:?} ({first:?})"),
        }
        assert_eq!(after, "", "{how:?}: the connection stayed open after the refusal of {sent}");
    }
}

/// The one connection of `record` (a `Ping`) got `Welcome`, then `Pong`.
fn assert_served(record: &serde_json::Value, how: How) {
    let got = replies(record);
    assert!(matches!(decode_line(got[0].0.trim_end()), Ok(DaemonFrame::Welcome(_))), "{how:?}: {got:?}");
    assert!(matches!(decode_line(got[0].1.trim_end()), Ok(DaemonFrame::Pong { .. })), "{how:?}: {got:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_sandboxed_client_is_refused_even_after_setsid_and_a_double_fork() {
    if !landlock_ok("a_sandboxed_client_is_refused_even_after_setsid_and_a_double_fork") {
        return;
    }
    let guard = HomeGuard::new();
    let d = start(&guard.base_dir().join("run")).await;
    let sock = d.paths.sock.clone();
    let scratch = tempfile::tempdir_in("/var/tmp").unwrap();

    // The test's own connection makes a session and stays attached.
    let mut own = Raw::connect(&sock).await;
    own.send(&ClientFrame::Hello(Hello::new(ClientKind::Test))).await;
    assert!(matches!(own.recv().await, Some(DaemonFrame::Welcome(_))));
    own.send(&ClientFrame::Attach(Attach::create(
        SessionConfig { cwd: Some(scratch.path().to_path_buf()), ..Default::default() },
        AttachMode::Mirror,
    )))
    .await;
    assert!(matches!(own.recv().await, Some(DaemonFrame::Attached(_))));
    let live = d.state.live_sessions();
    assert_eq!(live.len(), 1);
    let session_id = live[0].id.clone();

    // The owner turns the sandbox on (after the test's own session started:
    // with no provider, a new session would be refused). The daemon reads it
    // at the next accept.
    write_config(&guard.base_dir().join("config"), "sandbox.confine = enforce\n");

    let driving = vec![
        line(&ClientFrame::Attach(Attach::create(
            SessionConfig { cwd: Some(scratch.path().to_path_buf()), ..Default::default() },
            AttachMode::Takeover,
        ))),
        line(&ClientFrame::Cmd {
            session_id: session_id.clone(),
            cmd: SessionCommand::Submit { text: "from-the-sandbox".into(), attachments: vec![], origin: None },
        }),
        line(&ClientFrame::Shutdown { force: true }),
        line(&ClientFrame::Sessions),
    ];
    for how in [How::Landlock, How::NoNewPrivsOnly] {
        let record = detached(&sock, scratch.path(), &driving, how).await;
        assert_eq!(record["no_new_privs"], "1", "{how:?}: {record}");
        assert_refused(&record, &driving, how);
        // Nothing was created, the session is still there, the daemon runs.
        assert_eq!(d.state.live_sessions().len(), 1, "{how:?}: a sandboxed client made a session");
        assert!(!d.shutdown_token().is_cancelled(), "{how:?}: a sandboxed client shut the daemon down");
        let mut again = Raw::connect(&sock).await;
        again.send(&ClientFrame::Hello(Hello::new(ClientKind::Test))).await;
        assert!(matches!(again.recv().await, Some(DaemonFrame::Welcome(_))), "{how:?}: the daemon stopped serving");
    }

    // The control: the same detached client, unsandboxed, is served.
    let record = detached(&sock, scratch.path(), &[line(&ClientFrame::Ping)], How::Unsandboxed).await;
    assert_eq!(record["no_new_privs"], "0", "the test runner itself runs with no_new_privs: {record}");
    assert_served(&record, How::Unsandboxed);

    drop(own);
    d.shutdown_token().cancel();
    d.wait().await;
}

/// With the sandbox off everywhere under the base dir the daemon admits by
/// uid alone, as before the sandbox: a confined client and a client that
/// only has no_new_privs (as a systemd unit with `NoNewPrivileges=` has) are
/// both served.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn with_the_sandbox_off_a_no_new_privs_client_is_served() {
    if !landlock_ok("with_the_sandbox_off_a_no_new_privs_client_is_served") {
        return;
    }
    let guard = HomeGuard::new();
    let d = start(&guard.base_dir().join("run")).await;
    let sock = d.paths.sock.clone();
    let scratch = tempfile::tempdir_in("/var/tmp").unwrap();
    write_config(&guard.base_dir().join("config"), "sandbox.confine = off\n");
    write_config(&guard.base_dir().join("work/config"), "model = m\n");
    for how in [How::Landlock, How::NoNewPrivsOnly] {
        let record = detached(&sock, scratch.path(), &[line(&ClientFrame::Ping)], how).await;
        assert_eq!(record["no_new_privs"], "1", "{how:?}: {record}");
        assert_served(&record, how);
    }
    d.shutdown_token().cancel();
    d.wait().await;
}

/// The daemon's own config is off, but another profile under the same base
/// dir has `enforce`: a session of that profile may be sandboxed, so a
/// client with no_new_privs is refused. Once that profile turns it off
/// again the next connection is served (the configs are read at each
/// accept).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_sibling_profiles_enforce_makes_the_daemon_refuse_a_no_new_privs_client() {
    if !landlock_ok("a_sibling_profiles_enforce_makes_the_daemon_refuse_a_no_new_privs_client") {
        return;
    }
    let guard = HomeGuard::new();
    let d = start(&guard.base_dir().join("run")).await;
    let sock = d.paths.sock.clone();
    let scratch = tempfile::tempdir_in("/var/tmp").unwrap();
    write_config(&guard.base_dir().join("config"), "sandbox.confine = off\n");
    let sibling = guard.base_dir().join("other/config");
    write_config(&sibling, "sandbox.confine = enforce\n");
    let frames = vec![line(&ClientFrame::Ping), line(&ClientFrame::Shutdown { force: true })];
    for how in [How::Landlock, How::NoNewPrivsOnly] {
        let record = detached(&sock, scratch.path(), &frames, how).await;
        assert_eq!(record["no_new_privs"], "1", "{how:?}: {record}");
        assert_refused(&record, &frames, how);
        assert!(!d.shutdown_token().is_cancelled(), "{how:?}: a sandboxed client shut the daemon down");
    }
    write_config(&sibling, "sandbox.confine = off\n");
    let record = detached(&sock, scratch.path(), &[line(&ClientFrame::Ping)], How::NoNewPrivsOnly).await;
    assert_served(&record, How::NoNewPrivsOnly);
    d.shutdown_token().cancel();
    d.wait().await;
}
