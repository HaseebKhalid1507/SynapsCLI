//! The children an old image leaves at a reload exec are reaped by the new
//! one. `daemon reload` execs in place (same pid), so whatever children the
//! old image had at the exec become the new image's, with no `Child` handle
//! anywhere; without a reaper each one that exits stays a zombie for the
//! daemon's lifetime. Against a REAL daemon process.
//!
//! Linux only (the checks read `/proc`).

#![cfg(target_os = "linux")]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agent_engine::daemon::registry;
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use phase2::*;
use serial_test::serial;

/// The old image's seam (`testing` feature): two children left unwaited
/// right before the exec, their pids written to this file.
const LEAVE_CHILDREN_ENV: &str = "SYNAPS_TEST_LEAVE_CHILDREN";

/// One tool round that is still running when the reload comes: a PTY shell
/// (`sh`, stays open) and a bash tool command `sleep 30`.
const SSE_OPEN: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_reap1\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_reap_pty\",\"name\":\"shell_start\",\"input\":{}}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",",
    "\"partial_json\":\"{\\\"command\\\":\\\"sh\\\"}\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_reap_bash\",\"name\":\"bash\",\"input\":{}}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",",
    "\"partial_json\":\"{\\\"command\\\":\\\"sleep 30\\\"}\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// One bash tool command that exits 3 (after the reload: the new image's
/// own children keep their exit status).
const SSE_EXIT3: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_reap2\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_reap_exit\",\"name\":\"bash\",\"input\":{}}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",",
    "\"partial_json\":\"{\\\"command\\\":\\\"exit 3\\\"}\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

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

/// `synaps daemon --foreground` in the guard's scratch home; waits for Ping
/// (≤ 20 s).
async fn spawn_daemon(guard: &HomeGuard, url: &str, extra: &[(&str, &OsStr)]) -> DaemonProc {
    let run = guard.base_dir().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let paths = registry::daemon_paths_in(&run, None);
    let mut cmd = Command::new(bin());
    cmd.args(["daemon", "--foreground"])
        .env("HOME", guard.home.path())
        .env("SYNAPS_BASE_DIR", guard.base_dir())
        .env("SYNAPS_RUNTIME_DIR", &run)
        .env("SYNAPS_DAEMON", "1")
        .env("SYNAPS_ANTHROPIC_BASE_URL", url)
        .env("SYNAPS_NO_BOOT_FX", "1")
        .env_remove("SYNAPS_DAEMON_RELOAD_STATE")
        .env_remove("SYNAPS_DAEMON_LOCK_FD")
        .env_remove(LEAVE_CHILDREN_ENV);
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn daemon");
    let d = DaemonProc { child, paths };
    let t0 = Instant::now();
    loop {
        if SocketTransport::ping(&d.paths.sock).await.is_ok() {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "daemon never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    d
}

async fn reload_to_generation(d: &DaemonProc, generation: u64) {
    let got = SocketTransport::reload(&d.paths.sock, true, None, None).await.expect("reload accepted");
    assert_eq!(got, generation);
    let t0 = Instant::now();
    loop {
        if registry::read_daemon_json(&d.paths).is_some_and(|i| i.generation == generation)
            && tokio::time::timeout(Duration::from_millis(500), SocketTransport::ping(&d.paths.sock))
                .await
                .is_ok_and(|r| r.is_ok())
        {
            return;
        }
        assert!(t0.elapsed() < Duration::from_secs(30), "generation {generation} never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn shutdown(d: &DaemonProc) {
    SocketTransport::shutdown(&d.paths.sock, true).await.unwrap();
    let t0 = Instant::now();
    while registry::is_alive(&d.paths) && t0.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn attach_create(paths: &registry::DaemonPaths, cwd: &Path) -> SocketTransport {
    let conn = SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
    SocketTransport::attach(
        conn,
        Attach::Create {
            config: SessionConfig {
                cwd: Some(cwd.to_path_buf()),
                env: None,
                model_override: Some("claude-sonnet-4-5".into()),
                ..Default::default()
            },
            mode: AttachMode::Mirror,
        },
    )
    .await
    .unwrap()
    .0
}

/// `pid`'s direct children (every thread's `children` file).
fn children(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for task in std::fs::read_dir(format!("/proc/{pid}/task")).into_iter().flatten().flatten() {
        let text = std::fs::read_to_string(task.path().join("children")).unwrap_or_default();
        out.extend(text.split_whitespace().filter_map(|p| p.parse::<u32>().ok()));
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// (state, comm) from `/proc/<pid>/stat`; `None` once the pid is gone.
fn stat(pid: u32) -> Option<(char, String)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let state = text[close + 1..].split_whitespace().next()?.chars().next()?;
    Some((state, text[open + 1..close].to_string()))
}

fn parent(pid: u32) -> Option<u32> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    text[text.rfind(')')? + 1..].split_whitespace().nth(1)?.parse().ok()
}

fn cmdline(pid: u32) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| String::from_utf8_lossy(&b).replace('\0', " ").trim().to_string())
        .unwrap_or_default()
}

/// `pid`'s zombie children, as "pid comm".
fn zombie_children(pid: u32) -> Vec<String> {
    children(pid)
        .into_iter()
        .filter_map(|c| match stat(c) {
            Some(('Z', comm)) => Some(format!("{c} {comm}")),
            _ => None,
        })
        .collect()
}

/// Whether some process with this exact command line descends from `root`.
fn descendant_running(root: u32, line: &str) -> bool {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| cmdline(pid) == line)
        .any(|pid| {
            let mut p = pid;
            for _ in 0..16 {
                match parent(p) {
                    Some(pp) if pp == root => return true,
                    Some(pp) if pp > 1 => p = pp,
                    _ => return false,
                }
            }
            false
        })
}

/// The daemon log's lines containing `needle` (the appender is asynchronous:
/// wait ≤ 10 s for at least `want`).
async fn log_lines(guard: &HomeGuard, needle: &str, want: usize) -> Vec<String> {
    let t0 = Instant::now();
    loop {
        let mut lines = Vec::new();
        for entry in std::fs::read_dir(guard.base_dir()).into_iter().flatten().flatten() {
            if !entry.file_name().to_string_lossy().starts_with("synaps.log") {
                continue;
            }
            let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
            lines.extend(text.lines().filter(|l| l.contains(needle)).map(str::to_string));
        }
        if lines.len() >= want || t0.elapsed() > Duration::from_secs(10) {
            return lines;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The old image leaves `true` (exited: a zombie at the exec) and `sleep 5`
/// (still running) unwaited. The new image reaps the zombie at start and the
/// sleeper when it exits (SIGCHLD, well inside the 60 s fallback tick), and
/// logs one INFO line. A fresh start, which inherits nothing, logs none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn children_left_at_the_reload_exec_are_reaped_by_the_new_image() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let pids_file = guard.base_dir().join("left-children.pids");
    let d = spawn_daemon(&guard, &url, &[(LEAVE_CHILDREN_ENV, pids_file.as_os_str())]).await;
    let daemon = d.child.id();

    reload_to_generation(&d, 2).await;
    let text = std::fs::read_to_string(&pids_file).expect("the seam wrote the pids");
    let pids: Vec<u32> = text.split_whitespace().map(|p| p.parse().unwrap()).collect();
    let (exited, running) = (pids[0], pids[1]);

    let t0 = Instant::now();
    while stat(exited).is_some() || stat(running).is_some() {
        assert!(
            t0.elapsed() < Duration::from_secs(15),
            "not reaped 15 s after the reload: exited {exited} {:?}, running {running} {:?}; zombie children of the daemon: {:?}",
            stat(exited),
            stat(running),
            zombie_children(daemon)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(zombie_children(daemon), Vec::<String>::new());

    let lines = log_lines(&guard, "daemon: inherited", 1).await;
    assert_eq!(lines.len(), 1, "one line, at the reload's start only: {lines:#?}");
    assert!(lines[0].contains("inherited 2 children, 1 already exited and reaped"), "{}", lines[0]);

    shutdown(&d).await;
}

/// A turn whose PTY shell is open and whose bash tool command (`sleep 30`)
/// is still running when `reload --now` comes: after the reload the daemon
/// has no zombie child left from either. A bash command run by the new
/// image afterwards (in a new session, or in the cut turn the reload
/// resumes) still reports its exit status (the reaper never waits on the
/// new image's own children).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_tool_command_and_a_pty_shell_open_at_the_reload_leave_no_zombie() {
    let guard = HomeGuard::new();
    let (url, _hits, bodies) = spawn_stub(Script::SeqSse(&[SSE_OPEN, SSE_EXIT3, ANTHROPIC_SSE])).await;
    let d = spawn_daemon(&guard, &url, &[]).await;
    let daemon = d.child.id();
    let cwd = tempfile::tempdir().unwrap();

    let t = attach_create(&d.paths, cwd.path()).await;
    t.send_from_self(SessionCommand::Submit { text: "start them".into(), attachments: vec![] })
        .await
        .unwrap();
    let t0 = Instant::now();
    while !(descendant_running(daemon, "sleep 30") && descendant_running(daemon, "sh")) {
        assert!(t0.elapsed() < Duration::from_secs(30), "the PTY shell and `sleep 30` never both ran");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    t.detach().await;

    reload_to_generation(&d, 2).await;
    let t0 = Instant::now();
    loop {
        let zombies = zombie_children(daemon);
        if zombies.is_empty() {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "zombie children of the daemon 10 s after the reload: {zombies:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The new image's own child keeps its exit status.
    let mut t = attach_create(&d.paths, cwd.path()).await;
    t.send_from_self(SessionCommand::Submit { text: "exit 3".into(), attachments: vec![] })
        .await
        .unwrap();
    loop {
        let env = tokio::time::timeout(Duration::from_secs(30), t.next_event())
            .await
            .expect("turn hung")
            .expect("alive");
        match env.event {
            SessionEventWire::Idle => break,
            SessionEventWire::Refused { reason, .. } => panic!("refused: {reason}"),
            _ => {}
        }
    }
    t.detach().await;
    // The reload resumes the cut turn (0c27e88c5) in the new image too, and
    // the stub answers by arrival order: whichever of that resume and this
    // session asks first gets SSE_EXIT3, so the continuation carrying the
    // exit status may come from the resumed session after this one is idle.
    // Either way the `exit 3` ran as a child of the new image.
    let t0 = Instant::now();
    let continuation = loop {
        let found = bodies
            .lock()
            .unwrap()
            .iter()
            .map(|b| String::from_utf8_lossy(b).to_string())
            .find(|b| b.contains("\"tool_use_id\":\"toolu_reap_exit\""));
        if let Some(body) = found {
            break body;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(30),
            "a continuation request with the tool result"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(continuation.contains("Command failed (exit 3)"), "the exit status reached the tool result");
    assert_eq!(zombie_children(daemon), Vec::<String>::new());

    shutdown(&d).await;
}
