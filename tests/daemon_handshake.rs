//! Handshake + refusal paths over a real UDS (PLAN-phase2 §5.3), driven with
//! a raw client so the daemon's behaviour is tested, not the transport's.

#[path = "support/phase2/mod.rs"]
#[allow(dead_code)]
mod phase2;

use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::{self, Daemon, DaemonOpts};
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts};
use phase2::HomeGuard;
use serial_test::serial;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

async fn start(runtime_dir: &std::path::Path) -> Daemon {
    let host: Arc<EngineHost> =
        EngineHost::boot_and_install(HostOpts { profile: None, no_extensions: true }).await.expect("host boot");
    Daemon::start(host, DaemonOpts { runtime_dir: Some(runtime_dir.to_path_buf()), factory: Some(daemon::echo_factory()), ..Default::default() })
        .await
        .expect("daemon start")
}

struct Raw {
    r: BufReader<tokio::net::unix::OwnedReadHalf>,
    w: tokio::net::unix::OwnedWriteHalf,
}

impl Raw {
    async fn connect(p: &std::path::Path) -> Self {
        let (r, w) = UnixStream::connect(p).await.unwrap().into_split();
        Self { r: BufReader::new(r), w }
    }
    async fn send_line(&mut self, s: &str) {
        self.w.write_all(s.as_bytes()).await.unwrap();
        self.w.write_all(b"\n").await.unwrap();
    }
    /// For oversize probes: the daemon may close before the tail is written.
    async fn send_line_lossy(&mut self, s: &str) {
        let _ = self.w.write_all(s.as_bytes()).await;
        let _ = self.w.write_all(b"\n").await;
    }
    async fn send(&mut self, f: &ClientFrame) {
        self.w.write_all(encode_line(f).unwrap().as_bytes()).await.unwrap();
    }
    /// `None` = EOF.
    async fn recv(&mut self) -> Option<DaemonFrame> {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(3), self.r.read_line(&mut line)).await.expect("timely").unwrap();
        if n == 0 {
            return None;
        }
        Some(decode_line(line.trim_end()).unwrap())
    }
}

#[tokio::test]
#[serial]
async fn handshake_refuses_version_mismatch_and_protocol_violations() {
    let guard = HomeGuard::new();
    let d = start(&guard.base_dir().join("run")).await;
    let sock = d.paths.sock.clone();

    // version mismatch → Refused{Version} then EOF
    let mut c = Raw::connect(&sock).await;
    let mut hello = Hello::new(ClientKind::Test);
    hello.protocol_version = 99;
    c.send(&ClientFrame::Hello(hello)).await;
    match c.recv().await {
        Some(DaemonFrame::Refused { reason: RefuseReason::Version { daemon_version, min, max }, message }) => {
            assert_eq!((daemon_version, min, max), (PROTOCOL_VERSION, PROTOCOL_MIN, PROTOCOL_MAX));
            assert!(message.contains("99"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert!(c.recv().await.is_none(), "daemon closes after refusal");

    // Hello not first → Refused{Protocol} then EOF
    let mut c = Raw::connect(&sock).await;
    c.send(&ClientFrame::Ping).await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Refused { reason: RefuseReason::Protocol, .. })));
    assert!(c.recv().await.is_none());

    // malformed first frame → Refused{Protocol}
    let mut c = Raw::connect(&sock).await;
    c.send_line("{not json").await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Refused { reason: RefuseReason::Protocol, .. })));

    // > rpc's 1 MiB but under DAEMON_MAX_FRAME_BYTES → accepted (Pong)
    let mut c = Raw::connect(&sock).await;
    c.send(&ClientFrame::Hello(Hello::new(ClientKind::Test))).await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Welcome(_))));
    let mid = format!("{{\"type\":\"ping\",\"pad\":\"{}\"}}", "x".repeat(RPC_MAX_FRAME_BYTES * 2));
    c.send_line(&mid).await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Pong { .. })));

    // oversize frame (> DAEMON_MAX_FRAME_BYTES = 64 MiB) → Error + close
    let big = format!("{{\"type\":\"ping\",\"pad\":\"{}\"}}", "x".repeat(DAEMON_MAX_FRAME_BYTES + 10));
    c.send_line_lossy(&big).await;
    match c.recv().await {
        Some(DaemonFrame::Error { message, .. }) => assert!(message.contains("64 MiB"), "{message}"),
        other => panic!("{other:?}"),
    }
    assert!(c.recv().await.is_none());

    // different binary version, same protocol → allowed (Welcome carries daemon_version)
    let mut c = Raw::connect(&sock).await;
    let mut hello = Hello::new(ClientKind::Test);
    hello.client_version = "0.0.0-other".into();
    c.send(&ClientFrame::Hello(hello)).await;
    match c.recv().await {
        Some(DaemonFrame::Welcome(w)) => {
            assert_eq!(w.protocol_version, PROTOCOL_VERSION);
            assert_eq!(w.daemon_version, binary_version());
            assert_eq!(w.pid, std::process::id());
        }
        other => panic!("{other:?}"),
    }
    // Cmd before Attach → Error, connection stays open; Sessions still answered
    c.send(&ClientFrame::Cmd { session_id: "x".into(), cmd: SessionCommand::Save }).await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Error { .. })));
    c.send(&ClientFrame::Sessions).await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::SessionList { .. })));
    c.send(&ClientFrame::Bye).await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Bye { .. })));

    d.shutdown_token().cancel();
    d.wait().await;
}

#[tokio::test]
#[serial]
async fn control_fast_path_allocates_no_session() {
    let guard = HomeGuard::new();
    let d = start(&guard.base_dir().join("run")).await;
    let sock = d.paths.sock.clone();
    let mut c = Raw::connect(&sock).await;
    c.send(&ClientFrame::Hello(Hello::new(ClientKind::Test))).await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Welcome(_))));
    for _ in 0..3 {
        c.send(&ClientFrame::Ping).await;
        match c.recv().await {
            Some(DaemonFrame::Pong { sessions, pid, .. }) => {
                assert_eq!(sessions, 0);
                assert_eq!(pid, std::process::id());
            }
            other => panic!("{other:?}"),
        }
        c.send(&ClientFrame::Sessions).await;
        assert!(matches!(c.recv().await, Some(DaemonFrame::SessionList { sessions }) if sessions.is_empty()));
    }
    assert!(d.state.live_sessions().is_empty());
    assert!(d.state.host.sessions().is_empty());
    d.shutdown_token().cancel();
    d.wait().await;
}

#[tokio::test]
#[serial]
async fn create_refuses_relative_or_missing_cwd() {
    let guard = HomeGuard::new();
    let d = start(&guard.base_dir().join("run")).await;
    let sock = d.paths.sock.clone();
    let mut c = Raw::connect(&sock).await;
    c.send(&ClientFrame::Hello(Hello::new(ClientKind::Test))).await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Welcome(_))));
    c.send(&ClientFrame::Attach(Attach::Create {
        config: SessionConfig { cwd: Some("/definitely/not/here".into()), ..Default::default() },
        mode: AttachMode::Mirror,
    }))
    .await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Error { message, .. }) if message.contains("cwd")));
    assert!(d.state.live_sessions().is_empty());
    d.shutdown_token().cancel();
    d.wait().await;
}
