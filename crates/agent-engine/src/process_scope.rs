//! Run child processes in their own systemd scope (Linux).
//!
//! systemd-oomd, under memory pressure, kills a whole cgroup at once, choosing
//! the one using the most swap. Without this, everything Synaps starts lives
//! in the cgroup of whatever started Synaps — usually a terminal's scope. A
//! single runaway tool command (S348: a 270-file test run from the bash tool)
//! then makes that whole scope the biggest, and systemd-oomd kills it: the
//! command, the daemon, every session it hosts, and the terminal.
//!
//! With a scope per child, the runaway command is its own cgroup and the only
//! thing killed. The daemon gets its own scope too, so closing or killing the
//! terminal that started it no longer takes it down.
//!
//! `systemd-run --user --scope` registers a transient scope for its own pid
//! and then execs the command in place: same pid, process group, session,
//! environment, inherited fds and stdio. Killing the process group still kills
//! everything. Cost: one D-Bus round trip, ~4 ms per spawn.
//!
//! Used only when it is available (Linux, a reachable systemd user manager)
//! and not turned off (`process_scopes = off` in config, or
//! `SYNAPS_PROCESS_SCOPES=off`). Otherwise commands start exactly as before.

use std::ffi::OsString;
use std::path::PathBuf;
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

/// Whether a child with this environment can reach the systemd user manager.
/// A tool whose environment was cleared and rebuilt (sandboxed sessions) may
/// lack the variables `systemd-run --user` needs; such a child starts
/// unwrapped instead of failing.
fn env_reaches_user_manager(env: Option<&[(String, String)]>) -> bool {
    match env {
        None => true,
        Some(env) => env.iter().any(|(k, v)| {
            (k == "XDG_RUNTIME_DIR" || k == "DBUS_SESSION_BUS_ADDRESS") && !v.is_empty()
        }),
    }
}

#[cfg(target_os = "linux")]
fn find_systemd_run() -> Option<PathBuf> {
    let from_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join("systemd-run"));
    from_path
        .chain(["/usr/bin/systemd-run", "/bin/systemd-run"].map(PathBuf::from))
        .find(|p| p.is_file())
}

/// Start a throwaway scope and check it worked (bounded, 3 s).
#[cfg(target_os = "linux")]
fn probe(systemd_run: &std::path::Path) -> bool {
    use std::time::{Duration, Instant};
    if std::env::var_os("XDG_RUNTIME_DIR").is_none()
        && std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none()
    {
        return false;
    }
    let Ok(mut child) = std::process::Command::new(systemd_run)
        .args([
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            "--description=synaps scope probe",
            "--",
            "true",
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
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn detect() -> Option<ScopeLauncher> {
    let config_on = crate::config::load_config().process_scopes;
    if !enabled_from(
        std::env::var("SYNAPS_PROCESS_SCOPES").ok().as_deref(),
        config_on,
    ) {
        tracing::info!("process scopes: off (process_scopes / SYNAPS_PROCESS_SCOPES)");
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        let Some(systemd_run) = find_systemd_run() else {
            tracing::info!("process scopes: unavailable (no systemd-run)");
            return None;
        };
        if probe(&systemd_run) {
            tracing::info!(systemd_run = %systemd_run.display(), "process scopes: on");
            Some(ScopeLauncher { systemd_run })
        } else {
            tracing::info!("process scopes: unavailable (no reachable systemd user manager)");
            None
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// The launcher for a child started with exactly `env` (`None` = the child
/// inherits this process's environment), or `None` to start it directly.
/// Detection runs once per process: it is a property of the machine, not of
/// a session.
pub fn launcher_for_env(env: Option<&[(String, String)]>) -> Option<&'static ScopeLauncher> {
    static LAUNCHER: OnceLock<Option<ScopeLauncher>> = OnceLock::new();
    let launcher = LAUNCHER.get_or_init(detect).as_ref()?;
    env_reaches_user_manager(env).then_some(launcher)
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

    #[test]
    fn a_rebuilt_environment_without_the_user_bus_is_not_wrapped() {
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert!(env_reaches_user_manager(None), "inherited environment");
        assert!(!env_reaches_user_manager(Some(&[pair("PATH", "/usr/bin")])));
        assert!(
            !env_reaches_user_manager(Some(&[pair("XDG_RUNTIME_DIR", "")])),
            "empty value"
        );
        assert!(env_reaches_user_manager(Some(&[
            pair("PATH", "/usr/bin"),
            pair("XDG_RUNTIME_DIR", "/run/user/1000")
        ])));
        assert!(env_reaches_user_manager(Some(&[pair(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/run/user/1000/bus"
        )])));
    }
}
