//! The kill guard on tool children (Linux with glibc; docs/daemon-mode.md,
//! "Kill guard on tool children").
//!
//! A command a tool starts (the bash tool, a shell session) gets a tiny
//! `LD_PRELOAD` shim, built from `shim/killguard.c`, that fails
//! `kill(-1, …)` and `killpg(1, …)` with EPERM and passes every other call
//! through. One stray `kill(-1, SIGKILL)` from a tool child otherwise ends
//! every process of the user. It sets no no_new_privs and installs no
//! seccomp filter, and it is no sandbox: the docs list what it can't see.
//!
//! Fail-open: when the shim can't be installed, or ld.so doesn't load it
//! into `true` cleanly (the self-check), children start without it and the
//! process logs one WARN ([`UNAVAILABLE`]). A tool never fails because of
//! the guard. Built for Linux with glibc only (build.rs builds the shim for
//! no other target); everywhere else, musl included, [`apply_env`] does
//! nothing.

use std::ffi::OsString;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
use std::os::unix::ffi::OsStrExt;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
use std::path::{Path, PathBuf};
#[cfg(all(target_os = "linux", target_env = "gnu"))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(all(target_os = "linux", target_env = "gnu"))]
use std::time::Duration;

/// The one WARN a process logs when its tool children run without the guard.
pub const UNAVAILABLE: &str = "kill guard shim unavailable; tool children run without it";

/// What a guarded child gets in `ASAN_OPTIONS`: ASan aborts when another
/// library is preloaded ahead of its runtime.
pub const ASAN_LINK_ORDER_OFF: &str = "verify_asan_link_order=0";

/// The shim, built by build.rs from `shim/killguard.c`.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(crate) const SHIM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/synaps-killguard.so"));

/// The `true` the self-check runs: the first of these that exists.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const TRUE_PROGRAMS: &[&str] = &["/usr/bin/true", "/bin/true", "/run/current-system/sw/bin/true"];

/// How long the self-check waits for `true`.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const SELF_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// How much of `true`'s stderr a failed self-check's WARN carries.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const STDERR_EXCERPT_CHARS: usize = 120;

/// Adds the guard to a tool child's environment (`LD_PRELOAD`,
/// `ASAN_OPTIONS`). The first call in a process installs the shim under
/// `<base dir>/lib` and self-checks it, so call it off the async runtime.
/// Leaves `env` as it was when the shim is unavailable, and on every target
/// but Linux with glibc.
pub fn apply_env(env: &mut Vec<(OsString, OsString)>) {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    global().apply_env(env);
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    let _ = env;
}

/// The process's guard, installed (and self-checked) on first use.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn global() -> &'static KillGuard {
    static GUARD: std::sync::OnceLock<KillGuard> = std::sync::OnceLock::new();
    GUARD.get_or_init(|| KillGuard::install_in(&install_dir()))
}

/// `<base dir>/lib`, by the daemon's own base-dir resolution
/// (`SYNAPS_BASE_DIR`, else the person's home).
#[cfg(all(target_os = "linux", target_env = "gnu", not(test)))]
fn install_dir() -> PathBuf {
    crate::config::base_dir().join("lib")
}

/// Lib tests install into their own build dir: they point
/// `SYNAPS_BASE_DIR` at temp dirs that are deleted, and must not write into
/// the real base dir.
#[cfg(all(target_os = "linux", target_env = "gnu", test))]
fn install_dir() -> PathBuf {
    PathBuf::from(concat!(env!("OUT_DIR"), "/killguard-test-lib"))
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).into()
}

/// A shim's file name: `synaps-killguard-<first 12 hex of its sha256>.so`.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn file_name(shim: &[u8]) -> String {
    let hex: String = sha256(shim).iter().map(|b| format!("{b:02x}")).collect();
    format!("synaps-killguard-{}.so", &hex[..12])
}

/// The bytes `LD_PRELOAD` splits its list on.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn preload_separator(b: &u8) -> bool {
    matches!(b, b' ' | b':')
}

/// The bytes ASan splits its options on.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn asan_separator(b: &u8) -> bool {
    matches!(b, b' ' | b',' | b':' | b'\t' | b'\n' | b'\r')
}

/// The guard of one install directory: the shim's path, or why there is
/// none.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub struct KillGuard {
    shim: Result<PathBuf, String>,
    warned: AtomicBool,
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
impl KillGuard {
    /// Installs the shim in `dir` (created 0700) as [`file_name`] (0644): an
    /// existing file whose sha256 matches is reused; otherwise a temp file in
    /// `dir` is written, synced and renamed over it. Other versions beside it
    /// are never deleted. Nothing is written for a path `LD_PRELOAD` can't
    /// carry (relative, or with a space or ':'). Then the self-check runs
    /// once ([`self_check`]); a shim that fails it is not used.
    pub fn install_in(dir: &Path) -> Self {
        Self::install_shim_in(dir, SHIM)
    }

    /// [`Self::install_in`] with other bytes as the shim (tests).
    pub(crate) fn install_shim_in(dir: &Path, shim: &[u8]) -> Self {
        let shim = install(dir, shim).and_then(|path| self_check(&path).map(|()| path));
        Self { shim, warned: AtomicBool::new(false) }
    }

    /// The installed shim, if there is one.
    pub fn shim_path(&self) -> Option<&Path> {
        self.shim.as_deref().ok()
    }

    /// Adds the shim to `LD_PRELOAD` (after what is there, once) and
    /// [`ASAN_LINK_ORDER_OFF`] to `ASAN_OPTIONS`. When the shim is
    /// unavailable or its file is gone, leaves `env` unchanged and logs
    /// [`UNAVAILABLE`] once for this guard.
    pub fn apply_env(&self, env: &mut Vec<(OsString, OsString)>) {
        match &self.shim {
            Ok(path) if path.is_file() => add(env, path),
            Ok(path) => self.warn_once(&format!("shim file {} is missing", path.display())),
            Err(reason) => self.warn_once(reason),
        }
    }

    fn warn_once(&self, reason: &str) {
        if !self.warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(reason = %reason, "{UNAVAILABLE}");
        }
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn install(dir: &Path, shim: &[u8]) -> Result<PathBuf, String> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let path = dir.join(file_name(shim));
    if !path.is_absolute() {
        return Err(format!("install dir {} is not absolute", dir.display()));
    }
    if path.as_os_str().as_bytes().iter().any(preload_separator) {
        return Err(format!("shim path {} has a space or ':', which LD_PRELOAD can't carry", path.display()));
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    if std::fs::read(&path).is_ok_and(|bytes| sha256(&bytes) == sha256(shim)) {
        return Ok(path);
    }
    let cannot_write = |e: std::io::Error| format!("cannot write {}: {e}", path.display());
    let mut tmp = tempfile::Builder::new()
        .prefix(".synaps-killguard-")
        .suffix(".tmp")
        .tempfile_in(dir)
        .map_err(cannot_write)?;
    tmp.write_all(shim).map_err(cannot_write)?;
    tmp.as_file().set_permissions(std::fs::Permissions::from_mode(0o644)).map_err(cannot_write)?;
    tmp.as_file().sync_all().map_err(cannot_write)?;
    tmp.persist(&path).map_err(|e| cannot_write(e.error))?;
    // Best effort: the rename itself is durable once the dir is synced.
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(path)
}

/// ld.so must load the shim into a plain program without a word: `true`
/// (the first of [`TRUE_PROGRAMS`]) runs once with this process's env plus
/// `LD_PRELOAD` set to the shim alone, stdin and stdout null, and must exit
/// 0 with an empty stderr within [`SELF_CHECK_TIMEOUT`]. It catches what the
/// build can't foresee (a libc the shim can't link against, an object ld.so
/// rejects), which would fail or taint every tool child. `Err` carries the
/// exit code and at most [`STDERR_EXCERPT_CHARS`] of stderr. The wait is on
/// a thread of its own: a `true` that hangs costs the timeout once and is
/// reaped whenever it exits; no signal is sent.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn self_check(shim: &Path) -> Result<(), String> {
    use std::process::{Command, Stdio};

    let program = TRUE_PROGRAMS
        .iter()
        .map(Path::new)
        .find(|p| p.is_file())
        .ok_or_else(|| format!("self_check: none of {TRUE_PROGRAMS:?} exists"))?;
    let failed = |what: String| format!("self_check: {} with the shim preloaded: {what}", program.display());
    let child = Command::new(program)
        .env("LD_PRELOAD", shim)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| failed(format!("cannot run it: {e}")))?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("killguard-self-check".into())
        .spawn(move || {
            let _ = tx.send(child.wait_with_output());
        })
        .map_err(|e| failed(format!("cannot wait for it: {e}")))?;
    let out = match rx.recv_timeout(SELF_CHECK_TIMEOUT) {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(failed(format!("cannot wait for it: {e}"))),
        Err(_) => return Err(failed(format!("no exit within {}s", SELF_CHECK_TIMEOUT.as_secs()))),
    };
    if out.status.success() && out.stderr.is_empty() {
        return Ok(());
    }
    let code = out.status.code().map_or_else(|| format!("none ({})", out.status), |c| c.to_string());
    Err(failed(format!("exit code {code}, stderr {:?}", excerpt(&out.stderr))))
}

/// The start of a stderr: at most [`STDERR_EXCERPT_CHARS`] chars, `…` when
/// cut.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn excerpt(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    let mut cut: String = text.chars().take(STDERR_EXCERPT_CHARS).collect();
    if cut.len() < text.len() {
        cut.push('…');
    }
    cut
}

/// The value of `key`, the last one when it repeats.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn get<'a>(env: &'a [(OsString, OsString)], key: &str) -> Option<&'a OsString> {
    env.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn add(env: &mut Vec<(OsString, OsString)>, shim: &Path) {
    let ours = shim.as_os_str();
    let preload = match get(env, "LD_PRELOAD") {
        Some(v) if v.as_bytes().split(preload_separator).any(|e| e == ours.as_bytes()) => None,
        Some(v) if !v.as_bytes().iter().all(preload_separator) => {
            let mut joined = v.clone();
            if !v.as_bytes().last().is_some_and(preload_separator) {
                joined.push(":");
            }
            joined.push(ours);
            Some(joined)
        }
        _ => Some(ours.to_owned()),
    };
    if let Some(value) = preload {
        crate::process_scope::set_env(env, "LD_PRELOAD", value);
    }

    // ASan takes the last value of a flag: unchanged when the last
    // verify_asan_link_order is already ours, else ours goes last.
    let key = b"verify_asan_link_order=";
    let asan = match get(env, "ASAN_OPTIONS") {
        Some(v) if v.as_bytes().split(asan_separator).rfind(|e| e.starts_with(key))
            == Some(ASAN_LINK_ORDER_OFF.as_bytes()) => None,
        Some(v) if !v.as_bytes().iter().all(asan_separator) => {
            let mut joined = v.clone();
            if !v.as_bytes().last().is_some_and(asan_separator) {
                joined.push(":");
            }
            joined.push(ASAN_LINK_ORDER_OFF);
            Some(joined)
        }
        _ => Some(ASAN_LINK_ORDER_OFF.into()),
    };
    if let Some(value) = asan {
        crate::process_scope::set_env(env, "ASAN_OPTIONS", value);
    }
}

#[cfg(all(test, target_os = "linux", target_env = "gnu"))]
mod tests;
