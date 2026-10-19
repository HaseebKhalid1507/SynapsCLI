//! Run child processes in their own systemd scope (Linux).
//!
//! systemd-oomd, under memory pressure, kills a whole cgroup at once, choosing
//! the one using the most swap. Without this, everything Synaps starts lives
//! in the cgroup of whatever started Synaps — usually a terminal's scope. A
//! single runaway tool command (S348: a 270-file test run from the bash tool)
//! then makes that whole scope the biggest, and systemd-oomd kills it: the
//! command, the daemon, every session it hosts, and the terminal.
//!
//! A scope gives each child a separate cgroup, not a memory cap. An ancestor
//! cgroup OOM or global kernel OOM can still kill the daemon and other scopes.
//! Windows isolation is not implemented.
//!
//! `systemd-run --user --scope` registers a transient scope for its own pid
//! and then execs the command in place: same pid, process group, session,
//! environment, inherited fds and stdio. Group cleanup covers descendants that
//! remain in that group. Each launch first performs a bounded fresh probe.
//!
//! Used only when it is available (Linux, a reachable systemd user manager)
//! and not turned off (`process_scopes = off` in config, or
//! `SYNAPS_PROCESS_SCOPES=off`). Otherwise commands start exactly as before.
//!
//! Slices: by default systemd puts each scope where the user manager puts
//! transient units (usually `app.slice`, next to the desktop's apps). The
//! config keys `tool_scope_slice` (bash tool commands and PTY shells) and
//! `daemon_scope_slice` (the daemon's own scope) pass `--slice=<name>`, so
//! an operator can cap agent tool work on its own slice. Synaps sets no
//! limits; the operator sets them on the slice. The probe uses the same
//! `--slice`: a slice the manager refuses fails the probe, the command
//! starts as it does without scopes, and one WARN names the slice.

use std::ffi::OsString;
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;

/// Which of Synaps's scopes a launch makes. Each kind has its own slice key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    /// A tool command: a bash tool command or a PTY shell (`tool_scope_slice`).
    Tool,
    /// The daemon itself (`daemon_scope_slice`).
    Daemon,
}

impl ScopeKind {
    /// The config key naming this kind's slice.
    pub fn slice_key(self) -> &'static str {
        match self {
            ScopeKind::Tool => "tool_scope_slice",
            ScopeKind::Daemon => "daemon_scope_slice",
        }
    }
}

/// The configured slice for `kind`, if any. The config parser has already
/// refused invalid names (they leave the key unset).
fn slice_for(kind: ScopeKind, config: &crate::config::SynapsConfig) -> Option<String> {
    match kind {
        ScopeKind::Tool => config.tool_scope_slice.clone(),
        ScopeKind::Daemon => config.daemon_scope_slice.clone(),
    }
}

/// Turns `program args…` into `systemd-run --user --scope … -- program args…`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeLauncher {
    systemd_run: PathBuf,
    /// `--slice=<name>`, from the kind's config key; `None` adds nothing.
    slice: Option<String>,
}

/// systemd-run options shared by real launches and the probe, so the probe
/// also proves the exact option set (the slice included) is accepted.
fn scope_options(description: &str, slice: Option<&str>) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec![
        "--user".into(),
        "--scope".into(),
        "--quiet".into(),
        // Garbage-collect the unit even if the command fails, so failed
        // commands do not accumulate as failed units.
        "--collect".into(),
    ];
    if let Some(slice) = slice {
        argv.push(format!("--slice={slice}").into());
    }
    argv.push(format!("--description={description}").into());
    argv
}

impl ScopeLauncher {
    /// The program to spawn and its full argument list. `description` names
    /// the scope in `systemctl --user list-units --type=scope`.
    pub fn wrap(
        &self,
        description: &str,
        program: impl Into<OsString>,
        args: impl IntoIterator<Item = OsString>,
    ) -> (OsString, Vec<OsString>) {
        let mut argv = scope_options(description, self.slice.as_deref());
        argv.push("--".into());
        argv.push(program.into());
        argv.extend(args);
        (self.systemd_run.clone().into_os_string(), argv)
    }
}

/// `true` unless turned off. The environment variable wins over config so it
/// can be flipped for one process without editing the config file.
fn enabled_from(env_value: Option<&str>, config_value: bool) -> bool {
    match env_value.map(|v| v.trim().to_ascii_lowercase()) {
        Some(v) if matches!(v.as_str(), "0" | "off" | "false" | "no") => false,
        Some(v) if matches!(v.as_str(), "1" | "on" | "true" | "yes" | "auto") => true,
        _ => config_value,
    }
}

/// Snapshot inherited environment before applying child-specific overrides.
pub fn inherited_env() -> Vec<(OsString, OsString)> {
    std::env::vars_os().collect()
}

pub fn set_env(env: &mut Vec<(OsString, OsString)>, key: &str, value: impl Into<OsString>) {
    env.retain(|(k, _)| k != key);
    env.push((key.into(), value.into()));
}

#[cfg(target_os = "linux")]
fn find_systemd_run() -> Option<PathBuf> {
    // Discovery alone is cached. Never cache manager availability or consult
    // a tool-controlled PATH to choose the isolation launcher.
    ["/usr/bin/systemd-run", "/bin/systemd-run"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

/// Whether `env` can reach a user manager at all (`XDG_RUNTIME_DIR` or
/// `DBUS_SESSION_BUS_ADDRESS` set and non-empty).
#[cfg(target_os = "linux")]
fn has_manager_env(env: &[(OsString, OsString)]) -> bool {
    env.iter()
        .any(|(k, v)| (k == "XDG_RUNTIME_DIR" || k == "DBUS_SESSION_BUS_ADDRESS") && !v.is_empty())
}

/// Start only a throwaway scope, with the exact child environment and cwd
/// and the real launches' options (`slice` included).
/// Bounded to three seconds; on timeout kill and reap the probe child.
#[cfg(target_os = "linux")]
fn probe(
    systemd_run: &std::path::Path,
    env: &[(OsString, OsString)],
    cwd: Option<&std::path::Path>,
    slice: Option<&str>,
) -> bool {
    use std::time::{Duration, Instant};
    if !has_manager_env(env) {
        return false;
    }
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(systemd_run);
    // Probe descendants must not survive a timeout. No real payload runs here.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.env_clear().envs(env.iter().cloned());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let mut probe_args = scope_options("synaps scope probe", slice);
    probe_args.extend(["--".into(), "/bin/true".into()]);
    let Ok(mut child) = cmd
        .args(probe_args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                // Signal before any reap; the unreaped leader pins this ID.
                unsafe {
                    libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
                }
                let _ = child.kill();
                let cleanup_deadline = Instant::now() + Duration::from_millis(100);
                while matches!(child.try_wait(), Ok(None)) && Instant::now() < cleanup_deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                // SIGKILL cannot bound kernel D-state. Reap eventually off the
                // launch path rather than blocking the caller indefinitely.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return false;
            }
        }
    }
}

/// Test seam, unit tests only: the `systemd-run` named by
/// `SYNAPS_TEST_FAKE_SYSTEMD_RUN` in the launch's own environment. Tests
/// that set it run only their own synthetic script; a test without it in
/// its env is unaffected, so parallel tests don't see each other's fake.
#[cfg(test)]
fn test_fake_systemd_run(env: &[(OsString, OsString)]) -> Option<PathBuf> {
    env.iter()
        .find(|(k, _)| k == "SYNAPS_TEST_FAKE_SYSTEMD_RUN")
        .map(|(_, v)| PathBuf::from(v))
}

/// Outside unit tests the seam is always empty.
#[cfg(not(test))]
fn test_fake_systemd_run(_env: &[(OsString, OsString)]) -> Option<PathBuf> {
    None
}

/// Fresh, bounded availability check. Synchronous callers (PTY and detached
/// daemon spawn) may wait up to three seconds. Async callers must offload it.
/// Probe failure selects direct launch. After success, wrapper failure is an
/// error, NEVER a reason to replay the command: availability can race, and the
/// wrapped command may already have executed. This is not a launch handshake.
/// `kind` picks the slice key (`tool_scope_slice` / `daemon_scope_slice`),
/// read from the config this call loads.
pub fn launcher_for_effective_env(
    env: &[(OsString, OsString)],
    cwd: Option<&std::path::Path>,
    kind: ScopeKind,
) -> Option<ScopeLauncher> {
    // Unit tests only (always `None` otherwise): a synthetic systemd-run
    // named in this launch's own env, so the callers' argv can be tested.
    let fake = test_fake_systemd_run(env);
    // Test-support builds must not reach a developer's user manager merely
    // because ordinary bash/PTY tests inherit a desktop environment. Synthetic
    // launcher tests use a fake systemd-run; real smoke is opt-in.
    #[cfg(any(test, feature = "testing"))]
    if fake.is_none() && std::env::var("SYNAPS_TEST_REAL_SCOPES").as_deref() != Ok("1") {
        return None;
    }
    // Operator policy belongs to the host, not the tool's rebuilt env. A
    // child cannot enable scopes behind the host's explicit opt-out.
    let config = crate::config::load_config();
    if !enabled_from(
        std::env::var("SYNAPS_PROCESS_SCOPES").ok().as_deref(),
        config.process_scopes,
    ) {
        return None;
    }
    let slice = slice_for(kind, &config);
    #[cfg(target_os = "linux")]
    {
        if let Some(fake) = fake {
            return launcher_with_binary(&fake, env, cwd, kind, slice);
        }
        static BINARY: OnceLock<Option<PathBuf>> = OnceLock::new();
        let binary = BINARY.get_or_init(find_systemd_run).as_ref()?;
        launcher_with_binary(binary, env, cwd, kind, slice)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (cwd, fake, slice);
        None
    }
}

#[cfg(target_os = "linux")]
fn launcher_with_binary(
    binary: &std::path::Path,
    env: &[(OsString, OsString)],
    cwd: Option<&std::path::Path>,
    kind: ScopeKind,
    slice: Option<String>,
) -> Option<ScopeLauncher> {
    if probe(binary, env, cwd, slice.as_deref()) {
        return Some(ScopeLauncher {
            systemd_run: binary.to_owned(),
            slice,
        });
    }
    // Without a user manager in reach a slice changes nothing: no WARN then.
    if let Some(slice) = slice.filter(|_| has_manager_env(env)) {
        if first_refusal(&slice) {
            tracing::warn!(
                key = kind.slice_key(),
                slice = %slice,
                "process scope: the scope probe with --slice={slice} failed (does the user manager accept that slice?); these launches start without a scope"
            );
        }
    }
    None
}

/// `true` the first time `slice` fails its probe in this process, so a
/// refused slice logs one WARN, not one per command.
#[cfg(target_os = "linux")]
fn first_refusal(slice: &str) -> bool {
    static WARNED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
    if warned.iter().any(|s| s == slice) {
        return false;
    }
    warned.push(slice.to_owned());
    true
}

/// Compatibility convenience for inherited or fully rebuilt environments,
/// for a tool launch.
pub fn launcher_for_env(env: Option<&[(String, String)]>) -> Option<ScopeLauncher> {
    let env = env
        .map(|e| e.iter().map(|(k, v)| (k.into(), v.into())).collect())
        .unwrap_or_else(inherited_env);
    launcher_for_effective_env(&env, None, ScopeKind::Tool)
}

/// Resolve before wrapping: systemd-run must not substitute its own executable
/// search rules for the child's PATH/cwd. Missing programs remain spawn errors.
pub fn resolve_program(
    program: OsString,
    env: &[(OsString, OsString)],
    cwd: Option<&std::path::Path>,
) -> std::io::Result<OsString> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let base = cwd
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let base = if base.is_absolute() {
            base
        } else {
            std::env::current_dir().unwrap_or_default().join(base)
        };
        if std::path::Path::new(&program).components().count() > 1 {
            return Ok(base.join(program).into_os_string());
        }
        let path = env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "/bin:/usr/bin".into());
        for dir in std::env::split_paths(&path) {
            let candidate = base.join(dir).join(&program);
            if candidate
                .metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            {
                return Ok(candidate.into_os_string());
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("program not found in child PATH: {program:?}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_puts_the_command_after_systemd_run_options() {
        let l = ScopeLauncher {
            systemd_run: PathBuf::from("/usr/bin/systemd-run"),
            slice: None,
        };
        let (program, args) = l.wrap("synaps tool: bash", "bash", ["-c".into(), "echo hi".into()]);
        assert_eq!(program, OsString::from("/usr/bin/systemd-run"));
        let args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--description=synaps tool: bash",
                "--",
                "bash",
                "-c",
                "echo hi",
            ]
        );
    }

    #[test]
    fn ordinary_test_build_does_not_probe_host_manager() {
        if std::env::var("SYNAPS_TEST_REAL_SCOPES").as_deref() == Ok("1") {
            return;
        }
        let env = vec![(
            "DBUS_SESSION_BUS_ADDRESS".into(),
            "fake:must-not-probe".into(),
        )];
        assert!(launcher_for_effective_env(&env, None, ScopeKind::Tool).is_none());
    }

    #[test]
    fn env_variable_overrides_config_and_unknown_values_fall_back_to_config() {
        assert!(enabled_from(None, true));
        assert!(!enabled_from(None, false));
        for off in ["0", "off", "OFF", " false ", "no"] {
            assert!(!enabled_from(Some(off), true), "{off:?}");
        }
        for on in ["1", "on", "true", "yes", "auto"] {
            assert!(enabled_from(Some(on), false), "{on:?}");
        }
        assert!(enabled_from(Some("maybe"), true));
        assert!(!enabled_from(Some("maybe"), false));
    }

    // These tests execute only synthetic scripts in private tempdirs, never a
    // manager or a host systemd-run. No production marker-file protocol.
    #[cfg(target_os = "linux")]
    fn fake_script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fresh_probe_uses_exact_env_cwd_and_observes_availability_changes() {
        let dir = tempfile::tempdir().unwrap();
        let binary = fake_script(
            dir.path(),
            "systemd-run",
            r#"
[ "$DBUS_SESSION_BUS_ADDRESS" = fake:good ] || exit 1
[ "$TERM" = user-term ] || exit 2
[ "$SYNAPS_DAEMON" = 1 ] || exit 3
[ "$SYNAPS_DAEMON_READY_FD" = 987 ] || exit 4
[ "${HOME+x}" != x ] || exit 5
[ "$PWD" = "$EXPECTED_CWD" ] || exit 6
while [ "$1" != -- ]; do shift; done
shift
[ "$1" = /bin/true ] || exit 7
exit 0
"#,
        );
        let mut env = vec![];
        for (k, v) in [
            ("DBUS_SESSION_BUS_ADDRESS", "fake:good"),
            ("TERM", "user-term"),
            ("SYNAPS_DAEMON", "1"),
            ("SYNAPS_DAEMON_READY_FD", "987"),
            ("PATH", "/nonexistent"),
        ] {
            set_env(&mut env, k, v);
        }
        set_env(&mut env, "EXPECTED_CWD", dir.path().as_os_str());
        assert!(launcher_with_binary(&binary, &env, Some(dir.path()), ScopeKind::Tool, None).is_some());
        set_env(&mut env, "DBUS_SESSION_BUS_ADDRESS", "fake:stale");
        assert!(launcher_with_binary(&binary, &env, Some(dir.path()), ScopeKind::Tool, None).is_none());
        set_env(&mut env, "DBUS_SESSION_BUS_ADDRESS", "fake:good");
        assert!(launcher_with_binary(&binary, &env, Some(dir.path()), ScopeKind::Tool, None).is_some());
        // Same discovered path, changed availability: no cached positive.
        fake_script(dir.path(), "systemd-run", "exit 1");
        assert!(launcher_with_binary(&binary, &env, Some(dir.path()), ScopeKind::Tool, None).is_none());
        env.retain(|(k, _)| k != "DBUS_SESSION_BUS_ADDRESS");
        assert!(!probe(&binary, &env, Some(dir.path()), None));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn post_probe_failure_never_replays_and_exec_preserves_pid_and_stdio() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let dir = tempfile::tempdir().unwrap();
        let command = fake_script(
            dir.path(),
            "command",
            "printf 'command=%s\\n' \"$$\"; read line; printf '%s\\n' \"$line\"; exit 23",
        );
        let env = vec![("DBUS_SESSION_BUS_ADDRESS".into(), "fake:good".into())];
        for started in [false, true] {
            let binary = fake_script(
                dir.path(),
                "systemd-run",
                &format!(
                    r#"
while [ "$1" != -- ]; do shift; done
shift
[ "$1" = /bin/true ] && exit 0
printf 'wrapper=%s\n' "$$"
{}
"#,
                    if started { "exec \"$@\"" } else { "exit 42" }
                ),
            );
            let launcher = launcher_with_binary(&binary, &env, Some(dir.path()), ScopeKind::Tool, None).unwrap();
            let (program, args) = launcher.wrap("test", command.as_os_str(), []);
            let mut child = Command::new(program)
                .args(args)
                .env_clear()
                .envs(env.clone())
                .current_dir(dir.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let pid = child.id();
            if started {
                child.stdin.take().unwrap().write_all(b"input\n").unwrap();
            }
            let output = child.wait_with_output().unwrap();
            let text = String::from_utf8(output.stdout).unwrap();
            assert_eq!(
                text,
                if started {
                    format!("wrapper={pid}\ncommand={pid}\ninput\n")
                } else {
                    format!("wrapper={pid}\n")
                }
            );
            assert_eq!(output.status.code(), Some(if started { 23 } else { 42 }));
        }
    }

    #[cfg(unix)]
    #[test]
    fn missing_child_path_program_is_an_error_not_launcher_path_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let env = vec![("PATH".into(), dir.path().as_os_str().to_owned())];
        assert!(resolve_program("bash".into(), &env, Some(dir.path())).is_err());
    }

    #[test]
    fn host_policy_is_not_selected_from_the_child_environment() {
        let source = include_str!("process_scope.rs");
        let selection = source
            .split("pub fn launcher_for_effective_env")
            .nth(1)
            .unwrap()
            .split("/// Compatibility convenience")
            .next()
            .unwrap();
        assert!(selection.contains("std::env::var(\"SYNAPS_PROCESS_SCOPES\")"));
        assert!(!selection.contains("env.iter()"));
        assert!(!enabled_from(Some("off"), true));
    }

    #[test]
    fn production_callers_snapshot_overrides_and_do_not_retry_wrapped_launches() {
        let pty = include_str!("tools/shell/pty.rs");
        let spawn = pty
            .split("pub(crate) fn prepare_scope")
            .nth(1)
            .unwrap()
            .split("// 4.")
            .next()
            .unwrap();
        assert!(
            spawn.find("set_env(&mut effective_env, \"TERM\"").unwrap()
                < spawn.find("for (k, v) in env").unwrap()
        );
        assert!(
            spawn.find("for (k, v) in env").unwrap()
                < spawn.find("launcher_for_effective_env").unwrap()
        );
        assert!(spawn.contains("cmd.env_clear();"));
        assert_eq!(spawn.matches(".spawn_command(cmd)").count(), 1);
        let daemon = include_str!("daemon/mod.rs");
        let spawn = daemon.split("pub fn spawn_detached(").nth(1).unwrap();
        assert!(spawn.find("daemon_launch(").unwrap() < spawn.find("libc::pipe(").unwrap());
        assert!(
            spawn.find("libc::pipe(").unwrap()
                < spawn
                    .find("set_env(&mut effective_env, READY_FD_ENV")
                    .unwrap()
        );
        assert!(spawn.find("child.kill()").unwrap() < spawn.find("take(64 * 1024)").unwrap());
        assert!(spawn.contains("cmd.env_clear().envs(effective_env)"));
        let bash = include_str!("tools/bash.rs");
        let spawn = bash
            .split("pub(crate) async fn run_shell_command")
            .nth(1)
            .unwrap()
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert_eq!(spawn.matches("cmd.spawn()").count(), 1);
        assert!(spawn.contains("_ = cancel.cancelled()"));
    }

    fn args_of(args: &[OsString]) -> Vec<String> {
        args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn wrap_passes_the_slice_before_the_description_when_set() {
        let l = ScopeLauncher {
            systemd_run: PathBuf::from("/usr/bin/systemd-run"),
            slice: Some("worker-agent.slice".into()),
        };
        let (program, args) = l.wrap("synaps tool: bash", "bash", ["-c".into(), "echo hi".into()]);
        assert_eq!(program, OsString::from("/usr/bin/systemd-run"));
        assert_eq!(
            args_of(&args),
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--slice=worker-agent.slice",
                "--description=synaps tool: bash",
                "--",
                "bash",
                "-c",
                "echo hi",
            ]
        );
    }

    #[test]
    fn each_kind_reads_its_own_slice_key() {
        let mut c = crate::config::SynapsConfig::default();
        assert_eq!(slice_for(ScopeKind::Tool, &c), None);
        assert_eq!(slice_for(ScopeKind::Daemon, &c), None);
        c.tool_scope_slice = Some("t-agent.slice".into());
        assert_eq!(slice_for(ScopeKind::Tool, &c).as_deref(), Some("t-agent.slice"));
        assert_eq!(slice_for(ScopeKind::Daemon, &c), None);
        c.daemon_scope_slice = Some("d-agent.slice".into());
        assert_eq!(slice_for(ScopeKind::Daemon, &c).as_deref(), Some("d-agent.slice"));
        assert_eq!(slice_for(ScopeKind::Tool, &c).as_deref(), Some("t-agent.slice"));
        assert_eq!(ScopeKind::Tool.slice_key(), "tool_scope_slice");
        assert_eq!(ScopeKind::Daemon.slice_key(), "daemon_scope_slice");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_probe_runs_with_the_launch_slice_and_a_refused_slice_launches_direct() {
        let dir = tempfile::tempdir().unwrap();
        let fake = test_support::fake_systemd_run(dir.path());
        let env = vec![("DBUS_SESSION_BUS_ADDRESS".into(), "fake:good".into())];
        // Unset: the probe's options are today's, byte for byte.
        assert!(launcher_with_binary(&fake, &env, Some(dir.path()), ScopeKind::Tool, None).is_some());
        assert_eq!(
            test_support::recorded(dir.path(), "probe.args"),
            ["--user", "--scope", "--quiet", "--collect", "--description=synaps scope probe", "--", "/bin/true"]
        );
        // Set: the probe carries the launch's --slice.
        let slice = Some("worker-agent.slice".to_string());
        let l = launcher_with_binary(&fake, &env, Some(dir.path()), ScopeKind::Tool, slice).unwrap();
        assert_eq!(
            test_support::recorded(dir.path(), "probe.args"),
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--slice=worker-agent.slice",
                "--description=synaps scope probe",
                "--",
                "/bin/true",
            ]
        );
        assert!(args_of(&l.wrap("t", "/bin/echo", []).1).contains(&"--slice=worker-agent.slice".to_string()));
        // A manager that refuses the slice fails the probe: no launcher, so
        // the command starts directly, as with scopes unavailable.
        let refusing = fake_script(
            dir.path(),
            "refusing-systemd-run",
            r#"for a in "$@"; do case "$a" in --slice=*) exit 1;; esac; done
exit 0"#,
        );
        let refused = Some("refused-probe-test.slice".to_string());
        assert!(launcher_with_binary(&refusing, &env, Some(dir.path()), ScopeKind::Tool, refused).is_none());
        assert!(launcher_with_binary(&refusing, &env, Some(dir.path()), ScopeKind::Tool, None).is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_refused_slice_warns_once_per_name() {
        assert!(first_refusal("warn-once-test-a.slice"));
        assert!(!first_refusal("warn-once-test-a.slice"));
        assert!(first_refusal("warn-once-test-b.slice"));
    }

    /// End to end through the selection: each kind's scope goes in its own
    /// configured slice; neither key set, no `--slice`; `process_scopes =
    /// off` still means no scope.
    #[cfg(target_os = "linux")]
    #[test]
    #[serial_test::serial(synaps_base_dir)]
    fn each_kind_launches_in_its_configured_slice() {
        let base = crate::test_env::BaseDirGuard::new();
        let _scopes = crate::test_env::EnvVarGuard::remove("SYNAPS_PROCESS_SCOPES");
        let dir = tempfile::tempdir().unwrap();
        let fake = test_support::fake_systemd_run(dir.path());
        let env: Vec<(OsString, OsString)> = test_support::fake_env(&fake)
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        let slices = |kind| {
            launcher_for_effective_env(&env, Some(dir.path()), kind).map(|l| {
                args_of(&l.wrap("t", "/bin/echo", []).1)
                    .into_iter()
                    .filter(|a| a.starts_with("--slice"))
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(slices(ScopeKind::Tool), Some(vec![]));
        assert_eq!(slices(ScopeKind::Daemon), Some(vec![]));
        let config = base.path().join("config");
        std::fs::write(&config, "tool_scope_slice = t-agent.slice\ndaemon_scope_slice = d-agent.slice\n").unwrap();
        assert_eq!(slices(ScopeKind::Tool), Some(vec!["--slice=t-agent.slice".to_string()]));
        assert_eq!(slices(ScopeKind::Daemon), Some(vec!["--slice=d-agent.slice".to_string()]));
        std::fs::write(&config, "process_scopes = off\ntool_scope_slice = t-agent.slice\n").unwrap();
        assert_eq!(slices(ScopeKind::Tool), None);
    }

    /// The test seam is compiled into unit tests only.
    #[test]
    fn the_fake_systemd_run_seam_is_test_only() {
        let source = include_str!("process_scope.rs");
        assert!(source.contains("#[cfg(test)]\nfn test_fake_systemd_run("));
        assert!(source.contains(
            "#[cfg(not(test))]\nfn test_fake_systemd_run(_env: &[(OsString, OsString)]) -> Option<PathBuf> {\n    None\n}"
        ));
    }
}

/// Shared by the callers' tests (bash, PTY, daemon): a synthetic
/// systemd-run, never a real manager.
#[cfg(all(test, target_os = "linux"))]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    /// A fake `systemd-run` in `dir`. The probe (`… -- /bin/true`) writes its
    /// arguments to `dir/probe.args` and succeeds; a launch writes them to
    /// `dir/launch.args` and execs the command after `--`.
    pub(crate) fn fake_systemd_run(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("systemd-run");
        let body = format!(
            "#!/bin/sh\nfor a in \"$@\"; do last=$a; done\n\
             if [ \"$last\" = /bin/true ]; then printf '%s\\n' \"$@\" > '{probe}'; exit 0; fi\n\
             printf '%s\\n' \"$@\" > '{launch}'\n\
             while [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"\n",
            probe = dir.join("probe.args").display(),
            launch = dir.join("launch.args").display(),
        );
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    /// The arguments the fake recorded (`probe.args` / `launch.args`).
    pub(crate) fn recorded(dir: &Path, which: &str) -> Vec<String> {
        std::fs::read_to_string(dir.join(which))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// What a launch through the fake needs in its env: a user-bus address
    /// (the probe's precondition) and the seam variable naming the fake.
    pub(crate) fn fake_env(fake: &Path) -> Vec<(String, String)> {
        vec![
            ("DBUS_SESSION_BUS_ADDRESS".into(), "fake:slice-test".into()),
            ("SYNAPS_TEST_FAKE_SYSTEMD_RUN".into(), fake.display().to_string()),
        ]
    }
}
