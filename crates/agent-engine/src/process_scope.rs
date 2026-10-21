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

use std::ffi::OsString;
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;

/// Turns `program args…` into `systemd-run --user --scope … -- program args…`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeLauncher {
    systemd_run: PathBuf,
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
        let mut argv: Vec<OsString> = vec![
            "--user".into(),
            "--scope".into(),
            "--quiet".into(),
            // Garbage-collect the unit even if the command fails, so failed
            // commands do not accumulate as failed units.
            "--collect".into(),
            format!("--description={description}").into(),
            "--".into(),
            program.into(),
        ];
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

/// Start only a throwaway scope, with the exact child environment and cwd.
/// Bounded to three seconds; on timeout kill and reap the probe child.
#[cfg(target_os = "linux")]
fn probe(
    systemd_run: &std::path::Path,
    env: &[(OsString, OsString)],
    cwd: Option<&std::path::Path>,
) -> bool {
    use std::time::{Duration, Instant};
    if !env
        .iter()
        .any(|(k, v)| (k == "XDG_RUNTIME_DIR" || k == "DBUS_SESSION_BUS_ADDRESS") && !v.is_empty())
    {
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
    let Ok(mut child) = cmd
        .args([
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            "--description=synaps scope probe",
            "--",
            "/bin/true",
        ])
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

/// Fresh, bounded availability check. Synchronous callers (PTY and detached
/// daemon spawn) may wait up to three seconds. Async callers must offload it.
/// Probe failure selects direct launch. After success, wrapper failure is an
/// error, NEVER a reason to replay the command: availability can race, and the
/// wrapped command may already have executed. This is not a launch handshake.
pub fn launcher_for_effective_env(
    env: &[(OsString, OsString)],
    cwd: Option<&std::path::Path>,
) -> Option<ScopeLauncher> {
    // Test-support builds must not reach a developer's user manager merely
    // because ordinary bash/PTY tests inherit a desktop environment. Synthetic
    // launcher tests call launcher_with_binary directly; real smoke is opt-in.
    #[cfg(any(test, feature = "testing"))]
    if std::env::var("SYNAPS_TEST_REAL_SCOPES").as_deref() != Ok("1") {
        return None;
    }
    // Operator policy belongs to the host, not the tool's rebuilt env. A
    // child cannot enable scopes behind the host's explicit opt-out.
    if !enabled_from(
        std::env::var("SYNAPS_PROCESS_SCOPES").ok().as_deref(),
        crate::config::load_config().process_scopes,
    ) {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        static BINARY: OnceLock<Option<PathBuf>> = OnceLock::new();
        let binary = BINARY.get_or_init(find_systemd_run).as_ref()?;
        launcher_with_binary(binary, env, cwd)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (env, cwd);
        None
    }
}

#[cfg(target_os = "linux")]
fn launcher_with_binary(
    binary: &std::path::Path,
    env: &[(OsString, OsString)],
    cwd: Option<&std::path::Path>,
) -> Option<ScopeLauncher> {
    probe(binary, env, cwd).then(|| ScopeLauncher {
        systemd_run: binary.to_owned(),
    })
}

/// Compatibility convenience for inherited or fully rebuilt environments.
pub fn launcher_for_env(env: Option<&[(String, String)]>) -> Option<ScopeLauncher> {
    let env = env
        .map(|e| e.iter().map(|(k, v)| (k.into(), v.into())).collect())
        .unwrap_or_else(inherited_env);
    launcher_for_effective_env(&env, None)
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
    #[cfg(not(unix))]
    let _ = (env, cwd);
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
        assert!(launcher_for_effective_env(&env, None).is_none());
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
        assert!(launcher_with_binary(&binary, &env, Some(dir.path())).is_some());
        set_env(&mut env, "DBUS_SESSION_BUS_ADDRESS", "fake:stale");
        assert!(launcher_with_binary(&binary, &env, Some(dir.path())).is_none());
        set_env(&mut env, "DBUS_SESSION_BUS_ADDRESS", "fake:good");
        assert!(launcher_with_binary(&binary, &env, Some(dir.path())).is_some());
        // Same discovered path, changed availability: no cached positive.
        fake_script(dir.path(), "systemd-run", "exit 1");
        assert!(launcher_with_binary(&binary, &env, Some(dir.path())).is_none());
        env.retain(|(k, _)| k != "DBUS_SESSION_BUS_ADDRESS");
        assert!(!probe(&binary, &env, Some(dir.path())));
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
            let launcher = launcher_with_binary(&binary, &env, Some(dir.path())).unwrap();
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
        assert!(
            spawn.find("launcher_for_effective_env").unwrap() < spawn.find("libc::pipe(").unwrap()
        );
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
}
