//! The kill guard's macOS stand-in: a Seatbelt profile on each tool child
//! (docs/daemon-mode.md, "Kill guard on tool children").
//!
//! The `LD_PRELOAD` shim of [`crate::killguard`] can't work on macOS: SIP
//! strips `DYLD_*` variables at /bin/bash and every /usr/bin program. So on
//! macOS the children the shim guards on Linux (bash tool commands and PTY
//! shells, no others) start as `/usr/bin/sandbox-exec -p <PROFILE> <program>
//! <args…>`. sandbox-exec applies [`PROFILE`] and execs the program in place:
//! same pid, so process-group kills work as before. The profile allows
//! everything except a signal to a process outside the child's own sandbox.
//! The kernel enforces it, so it also covers what the shim can't see: Go,
//! static and raw-syscall programs calling `kill(-1, …)`.
//!
//! Fail-open, like the shim: the first use in a process runs
//! `sandbox-exec -p <PROFILE> /usr/bin/true` once ([`probe`], 2 s at most).
//! When that fails (no sandbox-exec, or Synaps itself runs inside a sandbox,
//! which Seatbelt can't nest), children start unwrapped and the process logs
//! one WARN ([`UNAVAILABLE`]). A tool never fails because of the guard.
//! sandbox-exec never prompts. No off switch, as on Linux.
//!
//! Everything here builds and is tested on every target. Only [`prepare`]
//! and [`tool_child`] act, and only on macOS; elsewhere they do nothing and
//! every child starts exactly as before.

use std::ffi::OsString;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The program that applies a profile and execs the command in place.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The Seatbelt profile of every guarded tool child: anything goes, except
/// a signal to a process outside this sandbox. One rule per line; a later
/// rule set (a Seatbelt write sandbox) extends this text.
/// `scripts/seatbelt-proof.sh` carries the same bytes (a test checks).
pub const PROFILE: &str =
    "(version 1)\n(allow default)\n(deny signal)\n(allow signal (target same-sandbox))\n";

/// The one WARN a process logs when its tool children run without the
/// Seatbelt guard (macOS; Linux's is [`crate::killguard::UNAVAILABLE`]).
pub const UNAVAILABLE: &str = "kill guard seatbelt unavailable; tool children run without it";

/// What the probe runs under the profile.
pub const PROBE_PROGRAM: &str = "/usr/bin/true";

/// How long the probe waits for sandbox-exec.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How much of the probe's stderr a failed probe's WARN carries.
const STDERR_EXCERPT_CHARS: usize = 120;

/// `program args…` as `/usr/bin/sandbox-exec -p <PROFILE> program args…`:
/// the program to spawn and its full argument list. Arguments pass
/// verbatim.
pub fn seatbelt_wrap(
    program: impl Into<OsString>,
    args: impl IntoIterator<Item = OsString>,
) -> (OsString, Vec<OsString>) {
    wrap_with(Path::new(SANDBOX_EXEC), program, args)
}

/// [`seatbelt_wrap`] with another sandbox-exec (the probe's tests).
fn wrap_with(
    sandbox_exec: &Path,
    program: impl Into<OsString>,
    args: impl IntoIterator<Item = OsString>,
) -> (OsString, Vec<OsString>) {
    let mut argv: Vec<OsString> = vec!["-p".into(), PROFILE.into(), program.into()];
    argv.extend(args);
    (sandbox_exec.as_os_str().to_owned(), argv)
}

/// How one tool child starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Launch {
    /// Under sandbox-exec with [`PROFILE`] ([`seatbelt_wrap`]).
    Seatbelt,
    /// As it is.
    Direct,
}

impl Launch {
    /// The program to spawn and its arguments. `Direct` returns both
    /// unchanged. `Seatbelt` first resolves `program` as the child would
    /// (the `PATH` in `env`, from `cwd`), as a process scope does, so
    /// sandbox-exec's own search plays no part; a program that can't be
    /// found is the spawn's error.
    pub fn apply(
        self,
        program: OsString,
        args: Vec<OsString>,
        env: &[(OsString, OsString)],
        cwd: Option<&Path>,
    ) -> std::io::Result<(OsString, Vec<OsString>)> {
        match self {
            Launch::Direct => Ok((program, args)),
            Launch::Seatbelt => {
                let program = crate::process_scope::resolve_program(program, env, cwd)?;
                Ok(seatbelt_wrap(program, args))
            }
        }
    }
}

/// A process's guard: the probe's answer, and whether its WARN was logged.
pub struct Guard {
    probe: Result<(), String>,
    warned: AtomicBool,
}

impl Guard {
    /// The guard a probe result makes (`Err` carries the reason).
    pub fn from_probe(probe: Result<(), String>) -> Self {
        Self {
            probe,
            warned: AtomicBool::new(false),
        }
    }

    /// `Seatbelt` when the probe passed. Otherwise `Direct`, and the first
    /// such answer of this guard logs [`UNAVAILABLE`] with the reason.
    pub fn launch(&self) -> Launch {
        match &self.probe {
            Ok(()) => Launch::Seatbelt,
            Err(reason) => {
                if !self.warned.swap(true, Ordering::Relaxed) {
                    tracing::warn!(reason = %reason, "{UNAVAILABLE}");
                }
                Launch::Direct
            }
        }
    }
}

/// sandbox-exec must apply the profile and exec a plain program cleanly:
/// `<sandbox_exec> -p <PROFILE> /usr/bin/true` runs once with this
/// process's env, stdin and stdout null, and must exit 0 with an empty
/// stderr within `timeout` (output on every run would taint every tool
/// child's). `Err` carries the exit code and at most 120 chars of stderr.
/// The wait is on a thread of its own: a run that hangs costs the timeout
/// once and is reaped whenever it exits; no signal is sent.
pub fn probe(sandbox_exec: &Path, timeout: Duration) -> Result<(), String> {
    use std::process::{Command, Stdio};

    let (program, args) = wrap_with(sandbox_exec, PROBE_PROGRAM, []);
    let failed = |what: String| {
        format!(
            "probe: {} -p <profile> {PROBE_PROGRAM}: {what}",
            sandbox_exec.display()
        )
    };
    let child = Command::new(&program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| failed(format!("cannot run it: {e}")))?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("seatbelt-probe".into())
        .spawn(move || {
            let _ = tx.send(child.wait_with_output());
        })
        .map_err(|e| failed(format!("cannot wait for it: {e}")))?;
    let out = match rx.recv_timeout(timeout) {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(failed(format!("cannot wait for it: {e}"))),
        Err(_) => return Err(failed(format!("no exit within {timeout:?}"))),
    };
    if out.status.success() && out.stderr.is_empty() {
        return Ok(());
    }
    let code = out
        .status
        .code()
        .map_or_else(|| format!("none ({})", out.status), |c| c.to_string());
    Err(failed(format!(
        "exit code {code}, stderr {:?}",
        excerpt(&out.stderr)
    )))
}

/// The start of a stderr: at most [`STDERR_EXCERPT_CHARS`] chars, `…` when
/// cut.
fn excerpt(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    let mut cut: String = text.chars().take(STDERR_EXCERPT_CHARS).collect();
    if cut.len() < text.len() {
        cut.push('…');
    }
    cut
}

/// The process's guard, probed on first use (macOS; tests everywhere).
#[cfg(any(target_os = "macos", test))]
fn global() -> &'static Guard {
    static GUARD: std::sync::OnceLock<Guard> = std::sync::OnceLock::new();
    GUARD.get_or_init(|| Guard::from_probe(probe(Path::new(SANDBOX_EXEC), PROBE_TIMEOUT)))
}

/// Runs the probe now if this process hasn't yet (macOS, up to
/// [`PROBE_TIMEOUT`]), so a later [`tool_child`] finds the answer without
/// waiting: call it off the async runtime. Elsewhere it does nothing.
pub fn prepare() {
    #[cfg(target_os = "macos")]
    let _ = global();
}

/// How the next tool child starts. On macOS: `Seatbelt` when the probe
/// passed, else `Direct` (one WARN per process); the first call (or
/// [`prepare`]) probes, so make it off the async runtime. Elsewhere: always
/// `Direct`, and nothing runs.
pub fn tool_child() -> Launch {
    #[cfg(target_os = "macos")]
    {
        global().launch()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Launch::Direct
    }
}

#[cfg(test)]
mod tests;
