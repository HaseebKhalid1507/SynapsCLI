//! `daemon.umask` against a REAL `synaps daemon --foreground` process that
//! is started under umask 0002 (as a systemd user service runs it).
//!
//! The configured mask is set once per daemon start, before the daemon
//! starts any process, so everything it starts sees it: a `bash` tool
//! command, a PTY shell from `shell_start`, and an extension process
//! (`/proc/<pid>/status` `Umask`). A reload exec runs the same start path:
//! a changed value takes effect there, and a removed line returns to the
//! mask the daemon was first started with. `inherit` (the default) and a
//! malformed value keep the started-with mask; a malformed value warns.
//! A trace test checks the order and the number of `umask` calls from the
//! kernel's side.
//!
//! Linux only (`/proc`).

#![cfg(target_os = "linux")]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use agent_engine::daemon::registry;
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use phase2::*;
use serial_test::serial;

/// One tool round: `umask` from a bash tool command and from a PTY shell,
/// each written to a file in the session's working directory. The bash
/// command also writes the names of its environment variables.
const SSE_PROBE: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_umask1\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_umask_bash\",\"name\":\"bash\",\"input\":{}}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",",
    "\"partial_json\":\"{\\\"command\\\":\\\"umask > bash-umask.out; env | cut -d= -f1 | sort > bash-env.out\\\"}\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_umask_pty\",\"name\":\"shell_start\",\"input\":{}}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",",
    "\"partial_json\":\"{\\\"command\\\":\\\"sh pty-umask.sh\\\"}\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

const SCRIPT: Script = Script::ToolRound { tool_use: SSE_PROBE, done: ANTHROPIC_SSE };

/// The PTY shell's script (the `shell_start` command line is whitespace-split,
/// so the redirection lives in a file).
const PTY_SCRIPT: &str = "umask > pty-umask.out\n";

const EXTENSION_SCRIPT: &str = "slow_session_start_extension.py";

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

/// `synaps daemon --foreground` started by a shell that set umask 0002 and
/// then exec'd it (same pid). Waits for Ping (≤ 20 s).
async fn spawn_daemon_under_0002(guard: &HomeGuard, url: &str) -> DaemonProc {
    spawn_daemon(guard, url, None).await
}

/// [`spawn_daemon_under_0002`], optionally under `strace -f` writing the
/// daemon's (and its threads' and children's) umask, execve and
/// process/thread-creation calls to `trace`.
async fn spawn_daemon(guard: &HomeGuard, url: &str, trace: Option<&Path>) -> DaemonProc {
    let run = guard.base_dir().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let paths = registry::daemon_paths_in(&run, None);
    let start = ["/bin/sh", "-c", "umask 0002 && exec \"$0\" daemon --foreground", bin()];
    let mut cmd = match trace {
        Some(trace) => {
            let mut cmd = Command::new("strace");
            cmd.args(["-f", "-qq", "--kill-on-exit", "-e", "signal=none"])
                .args(["-e", "trace=umask,execve,clone,clone3,fork,vfork", "-o"])
                .arg(trace)
                .args(start);
            cmd
        }
        None => {
            let mut cmd = Command::new(start[0]);
            cmd.args(&start[1..]);
            cmd
        }
    };
    let child = cmd
        .env("HOME", guard.home.path())
        .env("SYNAPS_BASE_DIR", guard.base_dir())
        .env("SYNAPS_RUNTIME_DIR", &run)
        .env("SYNAPS_DAEMON", "1")
        .env("SYNAPS_ANTHROPIC_BASE_URL", url)
        .env("SYNAPS_NO_BOOT_FX", "1")
        .env_remove("SYNAPS_DAEMON_RELOAD_STATE")
        .env_remove("SYNAPS_DAEMON_LOCK_FD")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn daemon");
    let d = DaemonProc { child, paths };
    let t0 = std::time::Instant::now();
    loop {
        if SocketTransport::ping(&d.paths.sock).await.is_ok() {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "daemon never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    d
}

fn write_config(guard: &HomeGuard, content: &str) {
    std::fs::write(guard.base_dir().join("config"), content).unwrap();
}

/// A long-lived process extension (the slow-start fixture, answering at once).
fn install_extension(guard: &HomeGuard) {
    let dir = guard.base_dir().join("plugins/umask-probe");
    std::fs::create_dir_all(dir.join(".synaps-plugin")).unwrap();
    std::fs::create_dir_all(dir.join("extensions")).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(EXTENSION_SCRIPT),
        dir.join("extensions").join(EXTENSION_SCRIPT),
    )
    .unwrap();
    std::fs::write(
        dir.join(".synaps-plugin/plugin.json"),
        r#"{
  "name": "umask-probe",
  "version": "0.1.0",
  "extension": {
    "protocol_version": 1,
    "runtime": "process",
    "command": "python3",
    "args": ["extensions/slow_session_start_extension.py"],
    "permissions": ["session.lifecycle"],
    "hooks": [{"hook": "on_session_start"}]
  }
}
"#,
    )
    .unwrap();
}

fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ppid …`: comm may hold spaces, so split after ')'.
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

/// The `Umask` line of `/proc/<pid>/status`, e.g. "0022".
fn proc_umask(pid: u32) -> Option<String> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status.lines().find_map(|l| l.strip_prefix("Umask:")).map(|v| v.trim().to_string())
}

/// Pids of the daemon's live extension processes (children running the fixture).
fn extension_pids(daemon: u32) -> Vec<u32> {
    let mut pids = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if parent_pid(pid) != Some(daemon) {
            continue;
        }
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        if String::from_utf8_lossy(&cmdline).contains(EXTENSION_SCRIPT) {
            pids.push(pid);
        }
    }
    pids
}

/// Wait (≤ 15 s) for exactly one extension process that is not in `old`,
/// and return its pid and umask.
async fn extension_umask(daemon: u32, old: &[u32]) -> (u32, String) {
    let t0 = std::time::Instant::now();
    loop {
        let fresh: Vec<u32> =
            extension_pids(daemon).into_iter().filter(|p| !old.contains(p)).collect();
        if let [pid] = fresh[..] {
            if let Some(mask) = proc_umask(pid) {
                return (pid, mask);
            }
        }
        assert!(t0.elapsed() < Duration::from_secs(15), "no extension process (fresh: {fresh:?})");
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

/// One turn in a new session whose tools write `umask` to files in `cwd`;
/// returns (bash tool, PTY shell).
async fn tool_umasks(paths: &registry::DaemonPaths, cwd: &Path) -> (String, String) {
    let bash_out = cwd.join("bash-umask.out");
    let pty_out = cwd.join("pty-umask.out");
    let _ = std::fs::remove_file(&bash_out);
    let _ = std::fs::remove_file(&pty_out);
    std::fs::write(cwd.join("pty-umask.sh"), PTY_SCRIPT).unwrap();
    let mut t = attach_create(paths, cwd).await;
    t.send_from_self(SessionCommand::Submit { text: "show the umask".into(), attachments: vec![] })
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
    let read = |p: &PathBuf| {
        std::fs::read_to_string(p)
            .unwrap_or_else(|e| panic!("{} not written: {e}", p.display()))
            .trim()
            .to_string()
    };
    (read(&bash_out), read(&pty_out))
}

/// The names of the bash tool's environment variables, from the last
/// [`tool_umasks`] turn in `cwd`.
fn tool_env_names(cwd: &Path) -> Vec<String> {
    let path = cwd.join("bash-env.out");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} not written: {e}", path.display()))
        .lines()
        .map(str::to_string)
        .collect()
}

/// The daemon's pid (the `/bin/sh` that exec'd it, or under strace the
/// traced process), from its registration.
fn daemon_pid(d: &DaemonProc) -> u32 {
    registry::read_daemon_json(&d.paths).expect("daemon.json").pid
}

/// The daemon log's umask lines so far (the file appender is asynchronous,
/// so wait ≤ 10 s for at least `want` of them).
async fn umask_log_lines(guard: &HomeGuard, want: usize) -> Vec<String> {
    let t0 = std::time::Instant::now();
    loop {
        let mut lines = Vec::new();
        for entry in std::fs::read_dir(guard.base_dir()).into_iter().flatten().flatten() {
            if !entry.file_name().to_string_lossy().starts_with("synaps.log") {
                continue;
            }
            let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
            lines.extend(text.lines().filter(|l| l.contains("daemon: umask")).map(str::to_string));
        }
        if lines.len() >= want || t0.elapsed() > Duration::from_secs(10) {
            return lines;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn reload_to_generation(d: &DaemonProc, generation: u64) {
    let got = SocketTransport::reload(&d.paths.sock, true, None, None).await.expect("reload accepted");
    assert_eq!(got, generation);
    let t0 = std::time::Instant::now();
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
    let t0 = std::time::Instant::now();
    while registry::is_alive(&d.paths) && t0.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `daemon.umask = "022"` under a 0002 start: tools, PTY shells and
/// extensions get 0022. A reload with the value changed to 027 applies 0027;
/// a reload with the line removed returns to 0002, the started-with mask
/// (so "remove the line and reload" is a complete rollback).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_configured_umask_reaches_tools_shells_and_extensions_and_follows_reloads() {
    let guard = HomeGuard::new();
    install_extension(&guard);
    write_config(&guard, "daemon.umask = \"022\"\n");
    let (url, _hits, _) = spawn_stub(SCRIPT).await;
    let d = spawn_daemon_under_0002(&guard, &url).await;
    let pid = d.child.id();
    let cwd = guard.home.path();

    let (ext1, mask) = extension_umask(pid, &[]).await;
    assert_eq!(mask, "0022", "extension process");
    assert_eq!(tool_umasks(&d.paths, cwd).await, ("0022".into(), "0022".into()), "(bash tool, PTY shell)");
    // Nothing about the mask reaches what the daemon starts.
    let env1 = tool_env_names(cwd);
    assert!(env1.iter().any(|n| n == "HOME"), "{env1:?}");
    assert!(!env1.iter().any(|n| n.contains("UMASK")), "{env1:?}");
    let log = umask_log_lines(&guard, 1).await;
    assert_eq!(log.len(), 1, "{log:#?}");
    assert!(log[0].contains("umask=0022") && log[0].contains("source=config"), "{}", log[0]);

    // A changed value takes effect at the reload exec.
    write_config(&guard, "daemon.umask = 027\n");
    reload_to_generation(&d, 2).await;
    let (ext2, mask) = extension_umask(pid, &[ext1]).await;
    assert_eq!(mask, "0027", "extension process after the reload");
    assert_eq!(tool_umasks(&d.paths, cwd).await, ("0027".into(), "0027".into()), "after the reload");
    assert_eq!(tool_env_names(cwd), env1, "a tool's environment after the reload");

    // The line removed: back to the mask the daemon was started with.
    write_config(&guard, "");
    reload_to_generation(&d, 3).await;
    let (_, mask) = extension_umask(pid, &[ext1, ext2]).await;
    assert_eq!(mask, "0002", "extension process after the rollback reload");
    assert_eq!(tool_umasks(&d.paths, cwd).await, ("0002".into(), "0002".into()), "after the rollback reload");
    assert_eq!(tool_env_names(cwd), env1, "a tool's environment after the rollback reload");
    let log = umask_log_lines(&guard, 3).await;
    assert_eq!(log.len(), 3, "one umask line per start: {log:#?}");
    assert!(log[1].contains("umask=0027") && log[1].contains("source=config"), "{}", log[1]);
    assert!(log[2].contains("umask=0002") && log[2].contains("source=inherit"), "{}", log[2]);

    shutdown(&d).await;
}

/// No `daemon.umask` (= inherit): the started-with 0002 stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn inherit_keeps_the_umask_the_daemon_was_started_with() {
    let guard = HomeGuard::new();
    install_extension(&guard);
    let (url, _hits, _) = spawn_stub(SCRIPT).await;
    let d = spawn_daemon_under_0002(&guard, &url).await;

    let (_, mask) = extension_umask(d.child.id(), &[]).await;
    assert_eq!(mask, "0002", "extension process");
    assert_eq!(tool_umasks(&d.paths, guard.home.path()).await, ("0002".into(), "0002".into()));
    let log = umask_log_lines(&guard, 1).await;
    assert_eq!(log.len(), 1, "{log:#?}");
    assert!(log[0].contains("umask=0002") && log[0].contains("source=inherit"), "{}", log[0]);
    assert!(!log[0].contains("WARN"), "{}", log[0]);

    shutdown(&d).await;
}

/// A malformed value warns (one line, naming the key) and inherits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_malformed_umask_warns_and_inherits() {
    let guard = HomeGuard::new();
    install_extension(&guard);
    write_config(&guard, "daemon.umask = 0778\n");
    let (url, _hits, _) = spawn_stub(SCRIPT).await;
    let d = spawn_daemon_under_0002(&guard, &url).await;

    let (_, mask) = extension_umask(d.child.id(), &[]).await;
    assert_eq!(mask, "0002", "extension process");
    assert_eq!(tool_umasks(&d.paths, guard.home.path()).await, ("0002".into(), "0002".into()));
    let log = umask_log_lines(&guard, 1).await;
    assert_eq!(log.len(), 1, "{log:#?}");
    assert!(
        log[0].contains("WARN") && log[0].contains("daemon.umask") && log[0].contains("umask=0002")
            && log[0].contains("source=inherit"),
        "{}",
        log[0]
    );

    shutdown(&d).await;
}

/// `strace` is optional on a dev box: the trace test says so and returns
/// when it is missing or cannot trace.
fn strace_available() -> bool {
    Command::new("strace")
        .args(["-f", "-qq", "-o", "/dev/null", "/bin/true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// One traced call of interest, in trace order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Traced {
    /// `umask(<octal>)` by this thread.
    Umask(u32),
    /// An `execve` of this path, started by this thread.
    Exec(String),
    /// A new process (not a thread) created by this thread.
    Spawn,
}

/// `strace -f -o` lines -> (line index, thread id, call), plus each thread's
/// process (tgid). Handles `<unfinished ...>` / `<... resumed>` splits: a
/// call is placed at its start line; a clone's new id comes from its
/// result, which may be logged after the new thread's own first calls.
/// [`parse_trace`]'s result: the calls in trace order (line index, thread
/// id, call), and each thread's process (tgid).
type ParsedTrace = (Vec<(usize, u32, Traced)>, std::collections::HashMap<u32, u32>);

fn parse_trace(text: &str) -> ParsedTrace {
    use std::collections::HashMap;
    let mut events = Vec::new();
    // tid -> (creator tid, is a thread)
    let mut created: HashMap<u32, (u32, bool)> = HashMap::new();
    // tid -> pending clone (is a thread) awaiting its resumed result
    let mut pending: HashMap<u32, bool> = HashMap::new();
    let new_id = |line: &str| -> Option<u32> { line.rsplit_once(") = ")?.1.split_whitespace().next()?.parse().ok() };
    for (i, line) in text.lines().enumerate() {
        let Some((tid, call)) = line.split_once(' ') else { continue };
        let Ok(tid) = tid.parse::<u32>() else { continue };
        let call = call.trim_start();
        if let Some(rest) = call.strip_prefix("<... ") {
            if rest.starts_with("clone") || rest.starts_with("fork") || rest.starts_with("vfork") {
                if let (Some(is_thread), Some(child)) = (pending.remove(&tid), new_id(call)) {
                    created.insert(child, (tid, is_thread));
                }
            }
            continue;
        }
        let name = call.split('(').next().unwrap_or("");
        match name {
            "umask" => {
                let arg = call["umask(".len()..].split(')').next().unwrap_or("");
                if let Ok(mode) = u32::from_str_radix(arg.trim(), 8) {
                    events.push((i, tid, Traced::Umask(mode)));
                }
            }
            "execve" => {
                let path = call["execve(".len()..].split('"').nth(1).unwrap_or("").to_string();
                events.push((i, tid, Traced::Exec(path)));
            }
            "clone" | "clone3" | "fork" | "vfork" => {
                let is_thread = call.contains("CLONE_THREAD");
                if !is_thread {
                    events.push((i, tid, Traced::Spawn));
                }
                match new_id(call) {
                    Some(child) if !call.contains("<unfinished") => {
                        created.insert(child, (tid, is_thread));
                    }
                    _ => {
                        pending.insert(tid, is_thread);
                    }
                }
            }
            _ => {}
        }
    }
    // Resolve each thread to its process: a thread belongs to its creator's
    // process, a new process is its own.
    let mut tgid = HashMap::new();
    let ids: Vec<u32> = created.keys().copied().collect();
    for id in ids {
        let mut cur = id;
        let mut hops = 0;
        let leader = loop {
            match created.get(&cur) {
                Some(&(creator, true)) if hops < 10_000 => {
                    cur = creator;
                    hops += 1;
                }
                _ => break cur,
            }
        };
        tgid.insert(id, leader);
    }
    (events, tgid)
}

/// The daemon calls `umask` at most once per start, and only when the mask
/// changes: never for no key (or `inherit`) when the started-with mask is
/// already in place, once to set a configured value, once to return to the
/// started-with mask after the line is removed. Each call comes before the
/// daemon creates any process (extensions, tool commands, PTY shells), in
/// the first image and after each reload exec. Checked from the kernel's
/// side with `strace -f`, including threads that the allocator or runtime
/// started.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_daemon_sets_the_mask_with_one_call_before_it_starts_any_process() {
    if !strace_available() {
        eprintln!("skipped: strace is not available or cannot trace here");
        return;
    }
    let guard = HomeGuard::new();
    install_extension(&guard);
    let (url, _hits, _) = spawn_stub(SCRIPT).await;
    let trace = guard.base_dir().join("daemon.strace");
    let mut d = spawn_daemon(&guard, &url, Some(&trace)).await;
    let pid = daemon_pid(&d);
    let cwd = guard.home.path();

    // Start 1: no key. Start 2: "022" (and a tool turn). Start 3: the line
    // removed. Start 4: still no key.
    let (ext1, mask) = extension_umask(pid, &[]).await;
    assert_eq!(mask, "0002");
    write_config(&guard, "daemon.umask = \"022\"\n");
    reload_to_generation(&d, 2).await;
    let (ext2, mask) = extension_umask(pid, &[ext1]).await;
    assert_eq!(mask, "0022");
    assert_eq!(tool_umasks(&d.paths, cwd).await, ("0022".into(), "0022".into()));
    write_config(&guard, "");
    reload_to_generation(&d, 3).await;
    let (ext3, mask) = extension_umask(pid, &[ext1, ext2]).await;
    assert_eq!(mask, "0002");
    reload_to_generation(&d, 4).await;
    let (_, mask) = extension_umask(pid, &[ext1, ext2, ext3]).await;
    assert_eq!(mask, "0002");
    shutdown(&d).await;
    let t0 = std::time::Instant::now();
    while d.child.try_wait().unwrap().is_none() {
        assert!(t0.elapsed() < Duration::from_secs(15), "strace did not end with the daemon");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let text = std::fs::read_to_string(&trace).unwrap();
    let (events, tgid) = parse_trace(&text);
    let process = |tid: u32| *tgid.get(&tid).unwrap_or(&tid);
    // The daemon's own calls (any of its threads); its execs of the synaps
    // binary split the trace into images (strace's own exec of /bin/sh and
    // the shell's `umask 0002` come first).
    let mine: Vec<&(usize, u32, Traced)> = events.iter().filter(|(_, t, _)| process(*t) == pid).collect();
    let starts: Vec<usize> = mine
        .iter()
        .filter(|(_, _, c)| matches!(c, Traced::Exec(path) if path != "/bin/sh"))
        .map(|(i, _, _)| *i)
        .collect();
    assert_eq!(starts.len(), 4, "four daemon images (start + three reload execs): {starts:?}");
    let want: [&[u32]; 4] = [&[], &[0o022], &[0o002], &[]];
    for (k, &begin) in starts.iter().enumerate() {
        let end = starts.get(k + 1).copied().unwrap_or(usize::MAX);
        let inside: Vec<&(usize, u32, Traced)> =
            mine.iter().copied().filter(|(i, _, _)| *i > begin && *i < end).collect();
        let umasks: Vec<(usize, u32)> = inside
            .iter()
            .filter_map(|(i, _, c)| match c {
                Traced::Umask(m) => Some((*i, *m)),
                _ => None,
            })
            .collect();
        let octal = |modes: &[u32]| modes.iter().map(|m| format!("{m:04o}")).collect::<Vec<_>>();
        let modes: Vec<u32> = umasks.iter().map(|(_, m)| *m).collect();
        assert_eq!(octal(&modes), octal(want[k]), "image {}: the daemon's umask calls", k + 1);
        let spawns: Vec<usize> =
            inside.iter().filter(|(_, _, c)| *c == Traced::Spawn).map(|(i, _, _)| *i).collect();
        assert!(!spawns.is_empty(), "image {}: the daemon started no process (the extension)", k + 1);
        if let Some(&(at, _)) = umasks.first() {
            assert!(
                spawns.iter().all(|s| *s > at),
                "image {}: a process was started before the umask call (line {at}): {spawns:?}",
                k + 1
            );
        }
    }
}
