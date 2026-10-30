//! Hermetic config for integration-test binaries that don't otherwise pin
//! one (06 I3).
//!
//! Integration tests link the engine without `cfg(test)`, so several of its
//! constructors still fall back to the ambient config: `ExtensionManager::new`,
//! `ProcessExtension::spawn*` and its inbox, the extension tools' memory
//! check (`MemoryBinding::configured_current()` → `load_config()`). Unpinned,
//! a test binary reads the developer's real `~/.synaps-cli/config`, so with
//! `memory.backend = axel` every manager ran with `exclusive_memory = true`
//! and latched that config's identity and provider keys. CI's clean HOME
//! never saw it.
//!
//! Including this module points `SYNAPS_BASE_DIR` at one private dir for
//! the whole process, holding an explicit legacy memory config. On Linux and
//! macOS that happens in a load-time initializer, before `main` and so
//! before libtest starts any thread: the `set_var` can't race a `getenv`.
//! Elsewhere the first [`pin`] call does it. Every test still calls [`pin`]
//! first (it returns the dir, and is the fallback). Other writers of
//! `SYNAPS_BASE_DIR` in the same binary must restore it (a `HomeGuard`
//! does), so every test outside such a guard falls back to this dir, never
//! to the real HOME.
//!
//! The dir lives under cargo's `CARGO_TARGET_TMPDIR` (removed by `cargo
//! clean`), named after this binary and its pid. Each run also removes this
//! binary's dirs left by processes that no longer exist (Linux: `/proc`).
//!
//! `SYNAPS_RUNTIME_DIR` is pinned too, to a short private dir under `/tmp`.
//! Session sockets otherwise live in `<base>/run`, and under the target dir
//! that path is past the 108-byte `sun_path` limit (about 128 bytes on CI).
//! `create_session` then failed with "path must be shorter than SUN_LEN"
//! unless a `HomeGuard` test happened to hold its short temp HOME at that
//! moment. `HomeGuard` leaves `SYNAPS_RUNTIME_DIR` alone, so the pin holds
//! for every test.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static BASE: OnceLock<PathBuf> = OnceLock::new();

pub fn pin() -> &'static Path {
    BASE.get_or_init(create)
}

#[used]
#[cfg_attr(any(target_os = "linux", target_os = "android", target_os = "freebsd"), link_section = ".init_array")]
#[cfg_attr(target_vendor = "apple", link_section = "__DATA,__mod_init_func")]
static INIT_BEFORE_MAIN: extern "C" fn() = init_before_main;

/// Set only by the load-time initializer, so a test can tell it really ran
/// before `main` rather than a later [`pin`] call creating the dir.
static RAN_BEFORE_MAIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn init_before_main() {
    BASE.get_or_init(create);
    RAN_BEFORE_MAIN.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Where the initializer exists, it ran before any test thread did.
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
#[test]
fn hermetic_base_dir_is_set_before_main() {
    assert!(RAN_BEFORE_MAIN.load(std::sync::atomic::Ordering::SeqCst), "load-time initializer never ran");
    // The dir, not the live env var: a `HomeGuard` test may hold that right now.
    let base = BASE.get().expect("created by the initializer");
    assert!(base.starts_with(env!("CARGO_TARGET_TMPDIR")), "{base:?}");
}

/// A session socket (`s-<32 hex>.sock`) fits `sun_path` (104 bytes on macOS,
/// 108 on Linux) wherever the target dir is.
#[cfg(unix)]
#[test]
fn hermetic_runtime_dir_fits_a_session_socket() {
    pin();
    let run = PathBuf::from(std::env::var_os("SYNAPS_RUNTIME_DIR").expect("pinned"));
    let run = std::fs::canonicalize(&run).unwrap_or(run);
    let sock = run.join(format!("s-{}.sock", "0".repeat(32)));
    assert!(sock.as_os_str().len() < 104, "{sock:?}");
}

fn create() -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let prefix = format!("hermetic-{}-", env!("CARGO_CRATE_NAME"));
    sweep_dead(root, &prefix);
    let dir = root.join(format!("{prefix}{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir); // a recycled pid's leftovers
    std::fs::create_dir_all(&dir).expect("hermetic base dir");
    std::fs::write(dir.join("config"), "memory.backend = legacy\n").expect("hermetic config");
    std::env::set_var("SYNAPS_BASE_DIR", &dir);
    #[cfg(unix)]
    std::env::set_var("SYNAPS_RUNTIME_DIR", runtime_dir());
    dir
}

/// `/tmp/synaps-rt-<pid>-<random>`, mode 0700, created fresh (no reuse of an
/// existing path). Left behind on exit and swept by the next run, like the
/// base dir.
#[cfg(unix)]
fn runtime_dir() -> PathBuf {
    let root = Path::new("/tmp");
    let prefix = format!("synaps-rt-{}-", std::process::id());
    sweep_dead_runtime(root);
    tempfile::Builder::new()
        .prefix(&prefix)
        .tempdir_in(root)
        .expect("hermetic runtime dir")
        .keep()
}

/// Remove `synaps-rt-<pid>-*` dirs whose process is gone (Linux: `/proc`).
#[cfg(unix)]
fn sweep_dead_runtime(root: &Path) {
    if !Path::new("/proc/self").exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix("synaps-rt-"))
            .and_then(|rest| rest.split('-').next())
            .and_then(|p| p.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Remove `<prefix><pid>` dirs whose process is gone. Only where liveness is
/// cheap to check (`/proc`); elsewhere they stay until `cargo clean`.
fn sweep_dead(root: &Path, prefix: &str) {
    if !Path::new("/proc/self").exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.strip_prefix(prefix)).and_then(|p| p.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}
