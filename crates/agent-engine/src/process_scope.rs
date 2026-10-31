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
//! Command arguments are passed verbatim: where systemd-run supports it
//! (systemd >= 254, from a cached `--version`), `--expand-environment=no`
//! stops it from substituting `${NAME}` in tool scripts before exec.
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
//! `--slice`: when it fails (no reachable user manager, or a slice it
//! refuses) the command starts as it does without scopes, with one WARN per
//! slice name. A slice that doesn't exist is not refused: systemd creates it
//! on the fly, without limits. So, once per slice name and on a background
//! thread, Synaps asks the manager for the slice's limits and WARNs if none
//! is set (a typo, or a unit file not yet `daemon-reload`ed).

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
    /// Pass `--expand-environment=no`. Only set when the binary is known to
    /// accept it (systemd >= 254); an unknown option would fail every launch.
    no_expand: bool,
    /// `--slice=<name>`, from the kind's config key; `None` adds nothing.
    slice: Option<String>,
}

/// First systemd release whose systemd-run knows `--expand-environment=`.
///
/// systemd NEWS: v254 added the switch and, for `--scope` only, left
/// expansion off by default (with a warning); v258 turned it on for scopes
/// too, so systemd-run itself rewrites `${NAME}`/`$NAME` in the command
/// arguments before exec. Releases before 254 never expanded scope commands
/// (a scope has no ExecStart for the manager to expand), so omitting the flag
/// there keeps arguments verbatim.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // read by the Linux probe; tested everywhere
const EXPAND_ENVIRONMENT_SINCE: u32 = 254;

/// systemd-run options shared by real launches and the probe, so the probe
/// also proves the exact option set (the slice included) is accepted.
fn scope_options(description: &str, no_expand: bool, slice: Option<&str>) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec![
        "--user".into(),
        "--scope".into(),
        "--quiet".into(),
        // Garbage-collect the unit even if the command fails, so failed
        // commands do not accumulate as failed units.
        "--collect".into(),
    ];
    if no_expand {
        // Arguments are data: tool scripts contain literal `${...}` (shell
        // parameter expansion, JS template literals) that systemd-run would
        // otherwise substitute from its own environment.
        argv.push("--expand-environment=no".into());
    }
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
        let mut argv = scope_options(description, self.no_expand, self.slice.as_deref());
        argv.push("--".into());
        argv.push(program.into());
        argv.extend(args);
        (self.systemd_run.clone().into_os_string(), argv)
    }
}

/// Major version from `systemd-run --version` ("systemd 259 (259.5-1)").
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // read by the Linux probe; tested everywhere
fn parse_systemd_version(output: &str) -> Option<u32> {
    let rest = output.lines().next()?.trim().strip_prefix("systemd ")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Whether to pass `--expand-environment=no`; `None` if the version could not
/// be determined, in which case scopes are not used at all: the flag may be
/// required (>= 258) yet cannot be known to be accepted.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // read by the Linux probe; tested everywhere
fn no_expand_for_version(version: Option<u32>) -> Option<bool> {
    version.map(|v| v >= EXPAND_ENVIRONMENT_SINCE)
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

/// Start only a throwaway scope, with the exact child environment and cwd and
/// the same options real launches use (`slice` included). Bounded to three
/// seconds.
#[cfg(target_os = "linux")]
fn probe(
    systemd_run: &std::path::Path,
    no_expand: bool,
    env: &[(OsString, OsString)],
    cwd: Option<&std::path::Path>,
    slice: Option<&str>,
) -> bool {
    if !has_manager_env(env) {
        return false;
    }
    let mut cmd = std::process::Command::new(systemd_run);
    cmd.env_clear().envs(env.iter().cloned());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    cmd.args(scope_options("synaps scope probe", no_expand, slice))
        .args(["--", "/bin/true"])
        .stdout(std::process::Stdio::null());
    run_bounded(cmd).is_some_and(|(status, _)| status.success())
}

/// `systemd-run --version`, bounded like the probe. Touches no manager.
#[cfg(target_os = "linux")]
fn systemd_run_version(systemd_run: &std::path::Path) -> Option<u32> {
    let mut cmd = std::process::Command::new(systemd_run);
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("SYSTEMD_PAGER", "")
        .arg("--version")
        .stdout(std::process::Stdio::piped());
    let (status, stdout) = run_bounded(cmd)?;
    status.success().then_some(())?;
    parse_systemd_version(&String::from_utf8_lossy(&stdout))
}

/// Run a short helper command in its own session with null stdin/stderr.
/// Returns its status and (if piped) stdout, or `None` on spawn failure or
/// after three seconds, when the whole group is killed and reaped off-path.
#[cfg(target_os = "linux")]
fn run_bounded(mut cmd: std::process::Command) -> Option<(std::process::ExitStatus, Vec<u8>)> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};
    // Helper descendants must not survive a timeout. No real payload runs here.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Output is tiny (a version line), well under a pipe buffer.
                let mut out = Vec::new();
                if let Some(mut stdout) = child.stdout.take() {
                    let _ = stdout.by_ref().take(64 * 1024).read_to_end(&mut out);
                }
                return Some((status, out));
            }
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
                return None;
            }
        }
    }
}

/// Test seam, unit tests only: the `systemd-run` named by
/// `SYNAPS_TEST_FAKE_SYSTEMD_RUN` in the launch's own environment. Tests
/// that set it run only their own synthetic script; a test without it in
/// its env is unaffected, so parallel tests don't see each other's fake.
///
/// There is no non-test version: outside `cfg(test)` this function and the
/// variable's name don't exist, and the call site is `cfg(test)` too, so a
/// release build cannot consult the seam at all.
#[cfg(test)]
fn test_fake_systemd_run(env: &[(OsString, OsString)]) -> Option<PathBuf> {
    env.iter()
        .find(|(k, _)| k == "SYNAPS_TEST_FAKE_SYSTEMD_RUN")
        .map(|(_, v)| PathBuf::from(v))
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
    #[cfg(test)]
    let fake = test_fake_systemd_run(env);
    #[cfg(not(test))]
    let fake: Option<PathBuf> = None;
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
            // The fake answers no `--version`: it launches without
            // `--expand-environment=no`, as a pre-254 systemd-run would.
            return launcher_with_binary(&fake, false, env, cwd, kind, slice);
        }
        static BINARY: OnceLock<Option<PathBuf>> = OnceLock::new();
        let binary = BINARY.get_or_init(find_systemd_run).as_ref()?;
        // A property of the binary, not of manager availability: cache only a
        // determined answer. Unknown means no scope rather than a guess.
        static NO_EXPAND: OnceLock<bool> = OnceLock::new();
        let no_expand = match NO_EXPAND.get() {
            Some(v) => *v,
            None => {
                let v = no_expand_for_version(systemd_run_version(binary))?;
                *NO_EXPAND.get_or_init(|| v)
            }
        };
        launcher_with_binary(binary, no_expand, env, cwd, kind, slice)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (env, cwd, fake, slice);
        None
    }
}

#[cfg(target_os = "linux")]
fn launcher_with_binary(
    binary: &std::path::Path,
    no_expand: bool,
    env: &[(OsString, OsString)],
    cwd: Option<&std::path::Path>,
    kind: ScopeKind,
    slice: Option<String>,
) -> Option<ScopeLauncher> {
    if probe(binary, no_expand, env, cwd, slice.as_deref()) {
        if let Some(slice) = slice.as_deref() {
            if first_limits_check(slice) {
                spawn_slice_limits_check(binary, env, kind, slice);
            }
        }
        return Some(ScopeLauncher {
            systemd_run: binary.to_owned(),
            no_expand,
            slice,
        });
    }
    // Without a user manager in reach a slice changes nothing: no WARN then.
    if let Some(slice) = slice.filter(|_| has_manager_env(env)) {
        if first_refusal(&slice) {
            tracing::warn!(key = kind.slice_key(), slice = %slice, "{}", probe_failed_warning(&slice));
        }
    }
    None
}

/// The WARN for a failed probe with a slice set. The probe also fails when
/// the user manager can't be reached (the bus env is set but stale, the
/// manager is gone or slow), so it must not blame the slice alone.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // logged by the Linux launcher; tested everywhere
fn probe_failed_warning(slice: &str) -> String {
    format!(
        "process scope: scope probe failed (user manager unreachable, or slice {slice} refused); these launches start without a scope"
    )
}

/// `true` the first time `name` is seen in `seen`.
#[cfg(target_os = "linux")]
fn first_in(seen: &std::sync::Mutex<Vec<String>>, name: &str) -> bool {
    let mut seen = seen.lock().unwrap_or_else(|e| e.into_inner());
    if seen.iter().any(|s| s == name) {
        return false;
    }
    seen.push(name.to_owned());
    true
}

/// `true` the first time `slice` fails its probe in this process, so a
/// refused slice logs one WARN, not one per command.
#[cfg(target_os = "linux")]
fn first_refusal(slice: &str) -> bool {
    static WARNED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    first_in(&WARNED, slice)
}

/// `true` the first time a probe with `slice` succeeds in this process: its
/// limits are looked up once per name, never per command.
#[cfg(target_os = "linux")]
fn first_limits_check(slice: &str) -> bool {
    static CHECKED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    first_in(&CHECKED, slice)
}

/// Compatibility convenience for inherited or fully rebuilt environments,
/// for a tool launch.
pub fn launcher_for_env(env: Option<&[(String, String)]>) -> Option<ScopeLauncher> {
    let env = env
        .map(|e| e.iter().map(|(k, v)| (k.into(), v.into())).collect())
        .unwrap_or_else(inherited_env);
    launcher_for_effective_env(&env, None, ScopeKind::Tool)
}

/// The slice properties that cap what runs in it. If none is set, the slice
/// caps nothing: most likely a typo (systemd makes a missing slice on the
/// fly) or a unit file the manager hasn't loaded yet.
const SLICE_LIMIT_PROPERTIES: &str = "MemoryHigh,MemoryMax,MemorySwapMax,CPUWeight,CPUQuotaPerSecUSec,TasksMax";

/// From `systemctl --user show -p <SLICE_LIMIT_PROPERTIES> -p FragmentPath`:
/// `Some(has_unit_file)` when the output lists the limit properties and every
/// one is at its "no limit" value, `None` when one is set or the output is
/// not what was asked for (then nothing is reported). systemd prints an
/// unset value as `infinity`, `[not set]`, empty, or (older releases)
/// `u64::MAX`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // read by the Linux check; tested everywhere
fn slice_without_limits(show: &str) -> Option<bool> {
    let mut limits_seen = 0;
    let mut unit_file = false;
    for line in show.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key == "FragmentPath" {
            unit_file = !value.is_empty();
        } else if SLICE_LIMIT_PROPERTIES.split(',').any(|p| p == key) {
            if !matches!(value, "" | "infinity" | "[not set]" | "18446744073709551615") {
                return None;
            }
            limits_seen += 1;
        }
    }
    (limits_seen == SLICE_LIMIT_PROPERTIES.split(',').count()).then_some(unit_file)
}

/// The WARN for a slice with no limits.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // logged by the Linux check; tested everywhere
fn no_limits_warning(slice: &str, unit_file: bool) -> String {
    let why = if unit_file {
        "its unit sets none"
    } else {
        "no unit file defines it: a typo, or a new unit not yet loaded with `systemctl --user daemon-reload`?"
    };
    format!(
        "process scope: slice {slice} has no MemoryHigh, MemoryMax, MemorySwapMax, CPUWeight, CPUQuota or TasksMax set ({why}); scopes in it run uncapped"
    )
}

/// Ask the user manager `env` reaches whether `slice` sets any limit, with
/// the `systemctl` next to the `systemd-run` in use (never a PATH lookup).
/// `Some(has_unit_file)` if it sets none; `None` if it sets one or the
/// question fails. Bounded like the probe.
#[cfg(target_os = "linux")]
fn query_slice_without_limits(
    systemctl: &std::path::Path,
    env: &[(OsString, OsString)],
    slice: &str,
) -> Option<bool> {
    let mut cmd = std::process::Command::new(systemctl);
    cmd.env_clear()
        .envs(env.iter().cloned())
        .env("LC_ALL", "C")
        .env("SYSTEMD_PAGER", "")
        .args(["--user", "show", "-p", SLICE_LIMIT_PROPERTIES, "-p", "FragmentPath", "--", slice])
        .stdout(std::process::Stdio::piped());
    let (status, stdout) = run_bounded(cmd)?;
    status.success().then_some(())?;
    slice_without_limits(&String::from_utf8_lossy(&stdout))
}

/// Off the launch path: a detached thread looks the slice up and WARNs if it
/// caps nothing. Best effort: a short-lived process may exit first, and any
/// failure stays silent (the probe already decided the launch).
#[cfg(target_os = "linux")]
fn spawn_slice_limits_check(systemd_run: &std::path::Path, env: &[(OsString, OsString)], kind: ScopeKind, slice: &str) {
    let systemctl = systemd_run.with_file_name("systemctl");
    let env = env.to_vec();
    let slice = slice.to_owned();
    let _ = std::thread::Builder::new()
        .name("synaps-slice-limits".into())
        .spawn(move || {
            if let Some(unit_file) = query_slice_without_limits(&systemctl, &env, &slice) {
                tracing::warn!(key = kind.slice_key(), slice = %slice, "{}", no_limits_warning(&slice, unit_file));
            }
        });
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

    fn wrapped(no_expand: bool, script: &str) -> (OsString, Vec<String>) {
        let l = ScopeLauncher {
            systemd_run: PathBuf::from("/usr/bin/systemd-run"),
            no_expand,
            slice: None,
        };
        let (program, args) = l.wrap("synaps tool: bash", "bash", ["-c".into(), script.into()]);
        let args = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        (program, args)
    }

    #[test]
    fn wrap_disables_expansion_when_systemd_run_supports_it() {
        let (program, args) = wrapped(true, "echo 'D:${x}' $HOME");
        assert_eq!(program, OsString::from("/usr/bin/systemd-run"));
        assert_eq!(
            args,
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--expand-environment=no",
                "--description=synaps tool: bash",
                "--",
                "bash",
                "-c",
                "echo 'D:${x}' $HOME",
            ]
        );
    }

    #[test]
    fn wrap_never_passes_the_flag_to_a_systemd_run_without_it() {
        let (program, args) = wrapped(false, "echo hi");
        assert_eq!(program, OsString::from("/usr/bin/systemd-run"));
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
        assert!(!args.iter().any(|a| a.starts_with("--expand-environment")));
    }

    #[test]
    fn version_decides_the_flag_and_unknown_versions_decide_nothing() {
        assert_eq!(
            parse_systemd_version("systemd 259 (259.5-0ubuntu3.4)\n+PAM +AUDIT"),
            Some(259)
        );
        assert_eq!(parse_systemd_version("systemd 254 (254.5-1)"), Some(254));
        assert_eq!(parse_systemd_version("systemd 249\n"), Some(249));
        assert_eq!(parse_systemd_version("systemd 219"), Some(219));
        assert_eq!(parse_systemd_version(""), None);
        assert_eq!(parse_systemd_version("something else 300"), None);
        assert_eq!(parse_systemd_version("systemd x"), None);
        assert_eq!(no_expand_for_version(Some(259)), Some(true));
        assert_eq!(no_expand_for_version(Some(254)), Some(true));
        assert_eq!(no_expand_for_version(Some(253)), Some(false));
        assert_eq!(no_expand_for_version(Some(219)), Some(false));
        assert_eq!(no_expand_for_version(None), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn version_query_and_probe_use_exactly_the_launch_options() {
        let dir = tempfile::tempdir().unwrap();
        // An old systemd-run: rejects the unknown option like the real one.
        let binary = fake_script(
            dir.path(),
            "systemd-run",
            r#"
[ "$1" = --version ] && { echo 'systemd 249 (249.11-0ubuntu3)'; exit 0; }
for a in "$@"; do case "$a" in --expand-environment=*) exit 1;; --) break;; esac; done
exit 0
"#,
        );
        assert_eq!(systemd_run_version(&binary), Some(249));
        let env = vec![("DBUS_SESSION_BUS_ADDRESS".into(), "fake:good".into())];
        assert!(launcher_with_binary(&binary, false, &env, None, ScopeKind::Tool, None).is_some());
        // Had support been misjudged, the probe fails: direct launch, never a
        // wrapped launch with an option the binary rejects.
        assert!(launcher_with_binary(&binary, true, &env, None, ScopeKind::Tool, None).is_none());
        fake_script(dir.path(), "systemd-run", "exit 1");
        assert_eq!(systemd_run_version(&binary), None);
        fake_script(dir.path(), "systemd-run", "echo 'systemd 258 (258)'");
        assert_eq!(systemd_run_version(&binary), Some(258));
    }

    /// Real systemd-run and user manager; skipped where either is missing.
    /// A literal `${x}` must reach the program unchanged even though `x` is
    /// set in the child environment (systemd >= 258 expands it by default).
    #[cfg(target_os = "linux")]
    #[test]
    fn real_scope_passes_dollar_braces_through_verbatim() {
        if std::env::var("SYNAPS_PROCESS_SCOPES").is_ok_and(|v| !enabled_from(Some(&v), true)) {
            eprintln!("skip: SYNAPS_PROCESS_SCOPES is off");
            return;
        }
        let Some(binary) = find_systemd_run() else {
            eprintln!("skip: no systemd-run");
            return;
        };
        let Some(no_expand) = no_expand_for_version(systemd_run_version(&binary)) else {
            eprintln!("skip: systemd-run --version unusable");
            return;
        };
        let mut env = inherited_env();
        set_env(&mut env, "x", "EXPANDED");
        let Some(launcher) = launcher_with_binary(&binary, no_expand, &env, None, ScopeKind::Tool, None) else {
            eprintln!("skip: no reachable systemd user manager");
            return;
        };
        let payload = ["D:${x}", "$x", "'${x:-dflt}'", "`${a}${b}`", "$$", "${}"];
        let mut args: Vec<OsString> =
            vec!["-c".into(), "printf '%s\\n' \"$@\"".into(), "sh".into()];
        args.extend(payload.iter().map(OsString::from));
        let (program, argv) = launcher.wrap("synaps test: no expand", "/bin/sh", args);
        let out = std::process::Command::new(program)
            .args(argv)
            .env_clear()
            .envs(env)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert_eq!(stdout.lines().collect::<Vec<_>>(), payload);
        assert!(
            !String::from_utf8_lossy(&out.stderr).contains("environment variable"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
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
        assert!(launcher_with_binary(&binary, true, &env, Some(dir.path()), ScopeKind::Tool, None).is_some());
        set_env(&mut env, "DBUS_SESSION_BUS_ADDRESS", "fake:stale");
        assert!(launcher_with_binary(&binary, true, &env, Some(dir.path()), ScopeKind::Tool, None).is_none());
        set_env(&mut env, "DBUS_SESSION_BUS_ADDRESS", "fake:good");
        assert!(launcher_with_binary(&binary, true, &env, Some(dir.path()), ScopeKind::Tool, None).is_some());
        // Same discovered path, changed availability: no cached positive.
        fake_script(dir.path(), "systemd-run", "exit 1");
        assert!(launcher_with_binary(&binary, true, &env, Some(dir.path()), ScopeKind::Tool, None).is_none());
        env.retain(|(k, _)| k != "DBUS_SESSION_BUS_ADDRESS");
        assert!(!probe(&binary, true, &env, Some(dir.path()), None));
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
            let launcher = launcher_with_binary(&binary, true, &env, Some(dir.path()), ScopeKind::Tool, None).unwrap();
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
            no_expand: false,
            slice: Some("synaps_tools.slice".into()),
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
                "--slice=synaps_tools.slice",
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
        c.tool_scope_slice = Some("t_tools.slice".into());
        assert_eq!(slice_for(ScopeKind::Tool, &c).as_deref(), Some("t_tools.slice"));
        assert_eq!(slice_for(ScopeKind::Daemon, &c), None);
        c.daemon_scope_slice = Some("d_daemon.slice".into());
        assert_eq!(slice_for(ScopeKind::Daemon, &c).as_deref(), Some("d_daemon.slice"));
        assert_eq!(slice_for(ScopeKind::Tool, &c).as_deref(), Some("t_tools.slice"));
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
        assert!(launcher_with_binary(&fake, false, &env, Some(dir.path()), ScopeKind::Tool, None).is_some());
        assert_eq!(
            test_support::recorded(dir.path(), "probe.args"),
            ["--user", "--scope", "--quiet", "--collect", "--description=synaps scope probe", "--", "/bin/true"]
        );
        // Set: the probe carries the launch's --slice.
        let slice = Some("synaps_tools.slice".to_string());
        let l = launcher_with_binary(&fake, false, &env, Some(dir.path()), ScopeKind::Tool, slice).unwrap();
        assert_eq!(
            test_support::recorded(dir.path(), "probe.args"),
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--slice=synaps_tools.slice",
                "--description=synaps scope probe",
                "--",
                "/bin/true",
            ]
        );
        assert!(args_of(&l.wrap("t", "/bin/echo", []).1).contains(&"--slice=synaps_tools.slice".to_string()));
        // A manager that refuses the slice fails the probe: no launcher, so
        // the command starts directly, as with scopes unavailable.
        let refusing = fake_script(
            dir.path(),
            "refusing-systemd-run",
            r#"for a in "$@"; do case "$a" in --slice=*) exit 1;; esac; done
exit 0"#,
        );
        let refused = Some("refused-probe-test.slice".to_string());
        assert!(launcher_with_binary(&refusing, false, &env, Some(dir.path()), ScopeKind::Tool, refused).is_none());
        assert!(launcher_with_binary(&refusing, false, &env, Some(dir.path()), ScopeKind::Tool, None).is_some());
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
        std::fs::write(&config, "tool_scope_slice = t_tools.slice\ndaemon_scope_slice = d_daemon.slice\n").unwrap();
        assert_eq!(slices(ScopeKind::Tool), Some(vec!["--slice=t_tools.slice".to_string()]));
        assert_eq!(slices(ScopeKind::Daemon), Some(vec!["--slice=d_daemon.slice".to_string()]));
        std::fs::write(&config, "process_scopes = off\ntool_scope_slice = t_tools.slice\n").unwrap();
        assert_eq!(slices(ScopeKind::Tool), None);
    }

    /// The test seam is compiled into unit tests only: its one definition and
    /// its one call are each gated on `cfg(test)`, so a non-test build has
    /// neither (a stray ungated definition would also be dead code there,
    /// which clippy `-D warnings` rejects). Whitespace and line breaks don't
    /// matter to this check.
    #[test]
    fn the_fake_systemd_run_seam_is_test_only() {
        let compact: String = include_str!("process_scope.rs")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        // Built from pieces so this test's own text doesn't match.
        let name = ["test_fake", "_systemd_run"].concat();
        let gate = "#[cfg(test)]";
        let definitions: Vec<_> = compact.match_indices(&format!("fn{name}(")).collect();
        assert_eq!(definitions.len(), 1, "one definition of the seam");
        assert!(compact[..definitions[0].0].ends_with(gate), "the definition is cfg(test)");
        let calls: Vec<_> = compact.match_indices(&format!("={name}(")).collect();
        assert_eq!(calls.len(), 1, "one call of the seam");
        assert!(
            compact[..calls[0].0].ends_with(&format!("{gate}letfake")),
            "the call is cfg(test)"
        );
        // Outside the cfg(test) modules (`tests`, `test_support`) the
        // variable is named in the seam alone.
        let production = compact.split("#[cfg(test)]modtests{").next().unwrap();
        let env_name = ["SYNAPS_TEST_FAKE", "_SYSTEMD_RUN"].concat();
        assert_eq!(production.matches(&format!("\"{env_name}\"")).count(), 1, "named only in the seam");
    }

    #[test]
    fn a_failed_probe_warning_does_not_blame_the_slice_alone() {
        let w = probe_failed_warning("synaps_tools.slice");
        assert!(w.contains("user manager unreachable"), "{w}");
        assert!(w.contains("or slice synaps_tools.slice refused"), "{w}");
        assert!(w.contains("start without a scope"), "{w}");
    }

    #[test]
    fn a_slice_with_no_limit_set_is_reported_and_one_limit_is_enough() {
        // systemd 262 on a slice nobody defined (made on the fly).
        let missing = "LoadState=loaded\nFragmentPath=\nCPUWeight=[not set]\nCPUQuotaPerSecUSec=infinity\n\
                       MemoryHigh=infinity\nMemoryMax=infinity\nMemorySwapMax=infinity\nTasksMax=infinity\n";
        assert_eq!(slice_without_limits(missing), Some(false));
        let with_unit = missing.replace("FragmentPath=\n", "FragmentPath=/home/u/.config/systemd/user/synaps_tools.slice\n");
        assert_eq!(slice_without_limits(&with_unit), Some(true));
        // Older releases print an unset value as u64::MAX, or nothing.
        let old = missing.replace("[not set]", "18446744073709551615").replace("TasksMax=infinity", "TasksMax=");
        assert_eq!(slice_without_limits(&old), Some(false));
        // Any one limit set: nothing to report.
        for (from, to) in [
            ("CPUWeight=[not set]", "CPUWeight=20"),
            ("CPUQuotaPerSecUSec=infinity", "CPUQuotaPerSecUSec=2s"),
            ("MemoryHigh=infinity", "MemoryHigh=8589934592"),
            ("MemoryMax=infinity", "MemoryMax=12884901888"),
            ("MemorySwapMax=infinity", "MemorySwapMax=0"),
            ("TasksMax=infinity", "TasksMax=512"),
        ] {
            assert_eq!(slice_without_limits(&missing.replace(from, to)), None, "{to}");
        }
        // Output that doesn't answer the question (an error, a property
        // missing): nothing to report rather than a false alarm.
        assert_eq!(slice_without_limits(""), None);
        assert_eq!(slice_without_limits(&missing.replace("TasksMax=infinity\n", "")), None);
        let w = no_limits_warning("synaps_tools.slice", false);
        assert!(w.contains("synaps_tools.slice") && w.contains("typo") && w.contains("daemon-reload"), "{w}");
        assert!(no_limits_warning("synaps_tools.slice", true).contains("its unit sets none"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn slice_limits_are_looked_up_once_per_name_off_the_launch_path() {
        assert!(first_limits_check("limits-once-test-a.slice"));
        assert!(!first_limits_check("limits-once-test-a.slice"));
        assert!(first_limits_check("limits-once-test-b.slice"));
        // The gates are separate: a name checked for limits can still warn
        // once for a failed probe, and the other way round.
        assert!(first_refusal("limits-once-test-a.slice"));
    }

    /// End to end with a fake `systemd-run` and a fake `systemctl` beside it:
    /// a successful probe with a slice asks the same manager (same env) for
    /// that slice's limits, once per name; without a slice nothing is asked.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_launch_with_a_slice_asks_the_manager_for_its_limits_once() {
        let dir = tempfile::tempdir().unwrap();
        let fake = test_support::fake_systemd_run(dir.path());
        let log = dir.path().join("systemctl.log");
        fake_script(
            dir.path(),
            "systemctl",
            &format!(
                "printf '%s %s\\n' \"$DBUS_SESSION_BUS_ADDRESS\" \"$*\" >> '{}'\n\
                 printf 'FragmentPath=\\nCPUWeight=[not set]\\nCPUQuotaPerSecUSec=infinity\\n'\n\
                 printf 'MemoryHigh=infinity\\nMemoryMax=infinity\\nMemorySwapMax=infinity\\nTasksMax=infinity\\n'",
                log.display()
            ),
        );
        let env = vec![("DBUS_SESSION_BUS_ADDRESS".into(), "fake:limits".into())];
        let read_log = || std::fs::read_to_string(&log).unwrap_or_default();
        // Unset: no lookup.
        assert!(launcher_with_binary(&fake, false, &env, None, ScopeKind::Tool, None).is_some());
        let slice = "limits-e2e-test.slice".to_string();
        for _ in 0..3 {
            assert!(launcher_with_binary(&fake, false, &env, None, ScopeKind::Tool, Some(slice.clone())).is_some());
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while read_log().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Give a (wrong) second lookup time to show up before counting.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(
            read_log().lines().collect::<Vec<_>>(),
            [format!("fake:limits --user show -p {SLICE_LIMIT_PROPERTIES} -p FragmentPath -- {slice}")]
        );
        // The query itself, synchronously: defaults and no unit file.
        let systemctl = dir.path().join("systemctl");
        assert_eq!(query_slice_without_limits(&systemctl, &env, &slice), Some(false));
        fake_script(dir.path(), "systemctl", "exit 1");
        assert_eq!(query_slice_without_limits(&systemctl, &env, &slice), None);
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
