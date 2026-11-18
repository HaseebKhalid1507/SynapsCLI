//! The owner claim over a real UDS. The daemon runs in-process, so a frame
//! this test writes comes from the test binary; a claim that must come from
//! the owner app is written by a child (a copy of `/bin/sh`, the app image
//! here) on a dup of the connection's fd, as the per-frame writer
//! (`SCM_CREDENTIALS`) is what the daemon checks.
#![cfg(target_os = "linux")]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::owner_claim::{APP_EXE_ENV, DUPLICATE_CLAIM, REFUSED_ENV, SETTLE_ENV};
use agent_engine::daemon::{self, Daemon, DaemonOpts};
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts};
use phase2::HomeGuard;
use serial_test::serial;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const K1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const K2: &str = "2222222222222222222222222222222222222222222222222222222222222222";

/// The turn-origin label the marked commands here name.
const LABEL: &str = "test-app";

/// A daemon whose `daemon.owner_app_exe` is `config_exe`. The host is
/// installed once per test binary, so the config file under this test's
/// home is written and re-read into the host's snapshot before the daemon
/// starts (the daemon reads the value once, at start).
async fn start(guard: &HomeGuard, config_exe: Option<&Path>) -> Daemon {
    let config = match config_exe {
        Some(p) => format!("daemon.owner_app_exe = {}\n", p.display()),
        None => String::new(),
    };
    std::fs::write(guard.base_dir().join("config"), config).unwrap();
    let host: Arc<EngineHost> =
        EngineHost::boot_and_install(HostOpts { profile: None, no_extensions: true, mcp_discovery: agent_engine::McpDiscovery::Inline }).await.expect("host boot");
    host.reload_config();
    assert_eq!(host.config().daemon.owner_app_exe, config_exe.map(|p| p.display().to_string()));
    let runtime_dir = guard.base_dir().join("run");
    Daemon::start(host, DaemonOpts { runtime_dir: Some(runtime_dir), factory: Some(daemon::echo_factory()), ..Default::default() })
        .await
        .expect("daemon start")
}

/// The owner-claim env for one test; cleared on drop.
struct EnvGuard;

impl EnvGuard {
    fn set(app: Option<&Path>, settle: &str) -> Self {
        match app {
            Some(app) => std::env::set_var(APP_EXE_ENV, app),
            None => std::env::remove_var(APP_EXE_ENV),
        }
        std::env::set_var(SETTLE_ENV, settle);
        Self
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        std::env::remove_var(APP_EXE_ENV);
        std::env::remove_var(SETTLE_ENV);
    }
}

struct Raw {
    r: BufReader<tokio::net::unix::OwnedReadHalf>,
    w: tokio::net::unix::OwnedWriteHalf,
    fd: OwnedFd,
}

impl Raw {
    async fn connect(p: &Path) -> Self {
        let s = tokio::net::UnixStream::connect(p).await.unwrap();
        let fd = s.as_fd().try_clone_to_owned().unwrap();
        let (r, w) = s.into_split();
        Self { r: BufReader::new(r), w, fd }
    }

    /// Connected and welcomed, with `hello`'s meta.
    async fn hello(p: &Path, hello: Hello) -> (Self, Welcome) {
        let mut c = Self::connect(p).await;
        c.send(&ClientFrame::Hello(hello)).await;
        match c.recv().await {
            Some(DaemonFrame::Welcome(w)) => (c, w),
            other => panic!("{other:?}"),
        }
    }

    async fn welcomed(p: &Path) -> Self {
        Self::hello(p, Hello::new(ClientKind::Test)).await.0
    }

    async fn send_line(&mut self, s: &str) {
        self.w.write_all(s.as_bytes()).await.unwrap();
        self.w.write_all(b"\n").await.unwrap();
    }

    async fn send(&mut self, f: &ClientFrame) {
        self.w.write_all(encode_line(f).unwrap().as_bytes()).await.unwrap();
    }

    async fn claim(&mut self, key: &str) {
        self.send_line(&claim_frame(key)).await;
    }

    /// The next raw line; `None` = EOF.
    async fn line(&mut self) -> Option<String> {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(10), self.r.read_line(&mut line)).await.expect("timely").unwrap();
        (n > 0).then(|| line.trim_end().to_string())
    }

    async fn recv(&mut self) -> Option<DaemonFrame> {
        self.line().await.map(|l| decode_line(&l).unwrap())
    }

    /// The next line whose `type` is `ty` (other frames skipped).
    async fn until(&mut self, ty: &str) -> String {
        let tag = format!(r#""type":"{ty}""#);
        for _ in 0..64 {
            let line = self.line().await.unwrap_or_else(|| panic!("EOF before {ty}"));
            if line.contains(&tag) {
                return line;
            }
        }
        panic!("no {ty} frame")
    }

    /// The echo actor's note of what the daemon told of an origin-carrying
    /// frame's writer: `true` = the owner's.
    async fn peer_owner(&mut self) -> bool {
        for _ in 0..64 {
            let line = self.line().await.expect("EOF before the echo's peer note");
            if line.contains("echo: frame peer owner=true") {
                return true;
            }
            if line.contains("echo: frame peer owner=false") {
                return false;
            }
        }
        panic!("no peer note")
    }

    /// Ping → the very next frame is Pong (nothing was queued before it).
    async fn nothing_queued(&mut self) {
        self.send(&ClientFrame::Ping).await;
        let line = self.line().await.expect("pong");
        assert!(line.contains(r#""type":"pong""#), "a frame was queued: {line}");
    }

    /// Ping → no `owner_claim` frame before the Pong (an attached link's
    /// session events may be queued).
    async fn no_owner_claim(&mut self) {
        self.send(&ClientFrame::Ping).await;
        for _ in 0..256 {
            let line = self.line().await.expect("pong");
            assert!(!line.contains(r#""type":"owner_claim""#), "an owner_claim frame was queued: {line}");
            if line.contains(r#""type":"pong""#) {
                return;
            }
        }
        panic!("no pong")
    }
}

fn verdict(line: &str) -> (bool, Option<OwnerClaimReason>) {
    match decode_line::<DaemonFrame>(line).unwrap() {
        DaemonFrame::OwnerClaim { owner_bound, reason } => (owner_bound, reason),
        other => panic!("not owner_claim: {other:?}"),
    }
}

fn refused(r: OwnerClaimReason) -> (bool, Option<OwnerClaimReason>) {
    (false, Some(r))
}

/// A copy of `/bin/sh` standing in for the owner app.
fn app_image(dir: &Path) -> PathBuf {
    let path = dir.join("example-app");
    std::fs::copy("/bin/sh", &path).expect("copy /bin/sh");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// An app process that writes `frame` on `fd` (a dup of a link's socket),
/// then each line given to [`AppProc::say`]; killed on drop.
struct AppProc {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl AppProc {
    /// The app writes `line` on its link, from its own pid.
    fn say(&mut self, line: &str) {
        use std::io::Write;
        let stdin = self.stdin.as_mut().expect("stdin");
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }
}

impl Drop for AppProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn app_writes(image: &Path, fd: &OwnedFd, frame: &str) -> AppProc {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut cmd = Command::new(image);
        cmd.args(["-c", r#"printf '%s\n' "$APP_FRAME"; while read -r x; do printf '%s\n' "$x"; done"#])
            .env("APP_FRAME", frame)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(fd.try_clone().unwrap()))
            .stderr(Stdio::null());
        for name in REFUSED_ENV {
            cmd.env_remove(name);
        }
        match cmd.spawn() {
            Ok(mut child) => {
                let stdin = child.stdin.take();
                return AppProc { child, stdin };
            }
            // ETXTBSY: the copy's write fd may linger in a parallel fork.
            Err(e) if e.raw_os_error() == Some(26) && attempt < 100 => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => panic!("spawn: {e}"),
        }
    }
}

fn claim_frame(key: &str) -> String {
    format!(r#"{{"type":"claim_owner","key":"{key}"}}"#)
}

/// A marked Submit into `session_id`, as one line without its newline.
fn marked(session_id: SessionId) -> String {
    let frame = ClientFrame::Cmd {
        session_id,
        cmd: SessionCommand::Submit { text: "hi".into(), attachments: vec![], origin: Some(LABEL.into()) },
    };
    encode_line(&frame).unwrap().trim_end().to_string()
}

/// The exact frames: Welcome lists `turn_origin_owner` with no app path
/// set; a claim is `unsupported`; a second claim on the link, or a key that
/// is not 64 lower-case hex, is a non-terminal `Error` that never echoes
/// the key; a claim before Welcome is refused; a link that never claimed
/// hears nothing, one that declared a turn-origin label too.
#[tokio::test]
#[serial]
async fn an_unconfigured_daemon_lists_the_feature_and_answers_unsupported() {
    let guard = HomeGuard::new();
    let _env = EnvGuard::set(None, "15");
    let d = start(&guard, None).await;
    let sock = d.paths.sock.clone();

    let (mut a, welcome) = Raw::hello(&sock, Hello::new(ClientKind::Test)).await;
    assert!(welcome.features.iter().any(|f| f == "turn_origin_owner"), "{:?}", welcome.features);
    let mut quiet = Raw::welcomed(&sock).await;

    a.claim(K1).await;
    let line = a.until("owner_claim").await;
    assert_eq!(line, r#"{"type":"owner_claim","owner_bound":false,"reason":"unsupported"}"#);
    a.claim(K1).await;
    match a.recv().await {
        Some(DaemonFrame::Error { message, .. }) => assert_eq!(message, DUPLICATE_CLAIM),
        other => panic!("{other:?}"),
    }
    a.nothing_queued().await;

    let mut b = Raw::welcomed(&sock).await;
    for bad in ["ab".repeat(32).to_ascii_uppercase(), "ABCDEF".repeat(10) + "abcd", K1[..63].to_string(), format!("{K1}1"), "zz".repeat(32)] {
        b.claim(&bad).await;
        match b.recv().await {
            Some(DaemonFrame::Error { message, .. }) => {
                assert!(message.contains("claim_owner key must be 64 lower-case hex digits"), "{message}");
                assert!(!message.contains(&bad), "the refusal never echoes the key: {message}");
            }
            other => panic!("{other:?}"),
        }
        b.nothing_queued().await;
    }
    b.claim(K2).await;
    assert_eq!(verdict(&b.until("owner_claim").await), refused(OwnerClaimReason::Unsupported), "a refused frame left the link free to claim");

    quiet.nothing_queued().await;

    let (mut c, _) = Raw::hello(&sock, Hello::new(ClientKind::Chat).with_turn_origin(Some(LABEL.into()))).await;
    c.nothing_queued().await;

    // A claim before Welcome is a terminal Protocol refusal.
    let mut early = Raw::connect(&sock).await;
    early.claim(K1).await;
    assert!(matches!(early.recv().await, Some(DaemonFrame::Refused { reason: RefuseReason::Protocol, .. })));
    assert!(early.recv().await.is_none());

    d.shutdown_token().cancel();
    d.wait().await;
}

/// The owner app binds through its own frame; env wins over the config
/// value (which names this test binary, and is ignored); the right key from
/// another app process is `not_the_registrant` with the owner still bound;
/// `Attached.owner_bound` only on a claimed link; on the bound link a marked
/// Submit is the owner's only from the registrant's own frame; a second key
/// is contested and `owner reset` denies, each pushed only to links that
/// claimed.
#[tokio::test]
#[serial]
async fn the_app_binds_by_its_own_frame_and_only_claimed_links_hear_of_it() {
    let guard = HomeGuard::new();
    let me = std::env::current_exe().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let image = app_image(dir.path());
    let _env = EnvGuard::set(Some(&image), "0");
    let d = start(&guard, Some(&me)).await;
    assert!(std::env::var_os(APP_EXE_ENV).is_none(), "the daemon took the app path out of its env at start");
    let sock = d.paths.sock.clone();

    let mut p = Raw::welcomed(&sock).await;
    p.claim(K1).await;
    assert_eq!(verdict(&p.until("owner_claim").await), refused(OwnerClaimReason::NotTheApp), "the config's exe is ignored: env wins");

    let mut a = Raw::welcomed(&sock).await;
    let mut app = app_writes(&image, &a.fd, &claim_frame(K1));
    let line = a.until("owner_claim").await;
    assert_eq!(line, r#"{"type":"owner_claim","owner_bound":true}"#);

    let mut b = Raw::welcomed(&sock).await;
    let _other = app_writes(&image, &b.fd, &claim_frame(K1));
    assert_eq!(verdict(&b.until("owner_claim").await), refused(OwnerClaimReason::NotTheRegistrant));
    a.nothing_queued().await;

    let mut q = Raw::welcomed(&sock).await;
    q.send(&ClientFrame::Attach(Attach::create(SessionConfig::default(), AttachMode::Mirror))).await;
    let attached = q.until("attached").await;
    assert!(!attached.contains("owner_bound"), "an unclaimed link's Attached has no owner_bound");
    a.send(&ClientFrame::Attach(Attach::create(SessionConfig::default(), AttachMode::Mirror))).await;
    let attached = a.until("attached").await;
    assert!(attached.contains(r#""owner_bound":true"#), "{attached}");

    // On the bound link a marked Submit is the owner's only from the
    // registrant's own frame; the same frame written by this test process
    // on that link is not.
    let DaemonFrame::Attached(w) = decode_line::<DaemonFrame>(&attached).unwrap() else { unreachable!() };
    let marked = marked(w.meta.id);
    a.send_line(&marked).await;
    assert!(!a.peer_owner().await, "this process's frame on the bound link is not the owner's");
    app.say(&marked);
    assert!(a.peer_owner().await, "the registrant's own frame is the owner's");

    let mut r = Raw::welcomed(&sock).await;
    let mut c = Raw::welcomed(&sock).await;
    let _third = app_writes(&image, &c.fd, &claim_frame(K2));
    assert_eq!(verdict(&c.until("owner_claim").await), refused(OwnerClaimReason::Contested));
    assert_eq!(verdict(&a.until("owner_claim").await), refused(OwnerClaimReason::Contested));
    assert_eq!(verdict(&b.until("owner_claim").await), refused(OwnerClaimReason::Contested));
    r.nothing_queued().await;
    q.no_owner_claim().await;

    SocketTransport::owner_reset(&sock).await.expect("owner reset");
    assert_eq!(verdict(&a.until("owner_claim").await), refused(OwnerClaimReason::Reset));
    assert_eq!(verdict(&c.until("owner_claim").await), refused(OwnerClaimReason::Reset));
    r.nothing_queued().await;
    q.no_owner_claim().await;

    d.shutdown_token().cancel();
    d.wait().await;
}

/// A marked Submit written behind the claim, in the same write and so
/// before the daemon's image check (the shape of an `exec` into the app
/// with a frame already queued), is not the owner's, though its writer is
/// the registrant; the registrant's next marked Submit, written after the
/// check, is.
#[tokio::test]
#[serial]
async fn a_frame_queued_behind_the_claim_before_its_check_is_not_the_owners() {
    let guard = HomeGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let image = app_image(dir.path());
    let _env = EnvGuard::set(Some(&image), "0");
    let d = start(&guard, None).await;
    let sock = d.paths.sock.clone();

    let mut a = Raw::welcomed(&sock).await;
    a.send(&ClientFrame::Attach(Attach::create(SessionConfig::default(), AttachMode::Mirror))).await;
    let attached = a.until("attached").await;
    let DaemonFrame::Attached(w) = decode_line::<DaemonFrame>(&attached).unwrap() else { unreachable!() };
    let marked = marked(w.meta.id);
    let both = format!("{}\n{}", claim_frame(K1), marked);
    let mut app = app_writes(&image, &a.fd, &both);
    assert_eq!(a.until("owner_claim").await, r#"{"type":"owner_claim","owner_bound":true}"#);
    assert!(!a.peer_owner().await, "written before the check: nobody's");
    app.say(&marked);
    assert!(a.peer_owner().await, "written after the check: the owner's");

    d.shutdown_token().cancel();
    d.wait().await;
}

/// `main` moves the env app path out of the environment before it builds
/// the tokio runtime, while it has one thread (on the daemon path nothing
/// before it starts one), so the `remove_var` races no `getenv`.
#[test]
fn main_takes_the_app_path_out_of_the_env_before_its_runtime() {
    let src = include_str!("../src/main.rs");
    let body = &src[src.find("\nfn main() -> anyhow::Result<()> {").expect("main")..];
    let stash = body.find("agent_engine::daemon::owner_claim::stash_app_exe_env();").expect("main stashes the app path");
    let runtime = body.find("tokio::runtime::Builder::").expect("the runtime build");
    assert!(stash < runtime, "stashed before the runtime is built");
}
