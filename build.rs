//! Build script: the build id the daemon reports in its handshake
//! (`Welcome::daemon_build`), so an attached client can tell it is not the
//! same build as its daemon even when the version number is the same (a
//! `daemon reload` onto a newer build of the same release re-execs only the
//! daemon). Baked in as `SYNAPS_BUILD_ID` and handed to the engine at startup
//! (`agent_engine::session::wire::set_build_id`, first thing in `main`); no
//! runtime git shelling.
//!
//! It lives in the root crate, not in `synaps-engine`: an `env!` baked into
//! the engine recompiled ~178k lines (plus tui and the binary, LTO in
//! release) on every commit, and the old `.git/index` watch did it on every
//! `git add`. Here a new commit rebuilds only this package.
//!
//! `SYNAPS_BUILD_ID` from the build environment wins (packagers). Otherwise
//! the short commit, but only when git's repo root IS our workspace root: a
//! source tarball extracted inside an unrelated repo must not borrow that
//! repo's hash. Otherwise empty: unknown, and clients compare versions only.
//! This is the only build id: the TUI header reads it through the engine
//! (`BinaryId::this()`), and `agent-tui`'s build script computes none.
//!
//! `-dirty` is computed whenever the script runs, which is when HEAD moves
//! (commit, checkout, reset; see the watch list in `repo_build_id`). It
//! deliberately does NOT watch `.git/index`, so it reflects the tree at the
//! last HEAD move, not edits made since. Two builds of one dirty commit look
//! the same either way.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SYNAPS_BUILD_ID");
    let id = std::env::var("SYNAPS_BUILD_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(repo_build_id)
        .unwrap_or_default();
    println!("cargo:rustc-env=SYNAPS_BUILD_ID={}", id.trim());

    // Windows reserves 1 MiB for the main thread's stack (Linux: 8 MiB). The
    // CLI's top-level future is large in unoptimised builds and overflows it
    // before clap has even parsed `--version`; reserve what Linux gives.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            println!("cargo:rustc-link-arg-bins=/STACK:8388608");
        } else {
            println!("cargo:rustc-link-arg-bins=-Wl,--stack,8388608");
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
}

/// `git rev-parse --git-dir` style output as an absolute path. Older git
/// (< 2.31) has no `--path-format=absolute` and prints paths relative to
/// `-C`; join those ourselves.
fn abs_git_path(root: &Path, args: &[&str]) -> Option<PathBuf> {
    let p = PathBuf::from(git(root, args)?);
    Some(if p.is_absolute() { p } else { root.join(p) })
}

fn repo_build_id() -> Option<String> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").ok()?); // workspace root
    let ws_root = manifest.canonicalize().ok()?;
    let toplevel = PathBuf::from(git(&ws_root, &["rev-parse", "--show-toplevel"])?)
        .canonicalize()
        .ok()?;
    if toplevel != ws_root {
        return None;
    }
    // Rerun when the commit moves. In a worktree `.git` is a file, so ask git
    // for the directories. Only paths that exist: cargo treats a missing one
    // as always changed.
    //
    // - `<git-dir>/HEAD`: branch switch, detached moves.
    // - `<git-dir>/logs/HEAD`, HEAD's reflog: git appends to it on every HEAD
    //   move (commit, amend, reset, checkout), including a commit to a
    //   branch that lives only in packed-refs after `git gc`. That commit
    //   creates a loose ref file that didn't exist when this script ran, so
    //   watching the ref file alone misses it. Per worktree, so another
    //   worktree's commits don't trigger it.
    // - The branch's ref file and packed-refs: for repos without a reflog
    //   (`core.logAllRefUpdates = false`). There, with no loose ref yet, the
    //   ref file's parent directory stands in (cargo scans a directory for
    //   changes), so the first loose write is still seen.
    // - `<common>/reftable`: the reftable backend keeps refs and reflogs
    //   there, with no ref files at all. Any ref update reruns the script.
    //   In a linked worktree the worktree's own refs (its HEAD and HEAD
    //   reflog) live in `<git-dir>/reftable` instead, so watch that too when
    //   it is a different directory.
    let git_dir = abs_git_path(&ws_root, &["rev-parse", "--git-dir"]);
    let common_dir = abs_git_path(&ws_root, &["rev-parse", "--git-common-dir"]);
    let head_ref = git(&ws_root, &["rev-parse", "--symbolic-full-name", "HEAD"]);
    let mut watched: Vec<PathBuf> = Vec::new();
    let mut has_reflog = false;
    if let Some(d) = &git_dir {
        watched.push(d.join("HEAD"));
        let reflog = d.join("logs").join("HEAD");
        has_reflog = reflog.exists();
        watched.push(reflog);
    }
    if let (Some(g), Some(c)) = (&git_dir, &common_dir) {
        if g != c {
            watched.push(g.join("reftable"));
        }
    }
    if let Some(d) = &common_dir {
        watched.push(d.join("reftable"));
        watched.push(d.join("packed-refs"));
        if let Some(r) = head_ref.as_deref().filter(|r| r.starts_with("refs/")) {
            let loose = d.join(r);
            if !has_reflog && !loose.exists() {
                if let Some(parent) = loose.parent() {
                    watched.push(parent.to_path_buf());
                }
            }
            watched.push(loose);
        }
    }
    for p in watched.iter().filter(|p| p.exists()) {
        println!("cargo:rerun-if-changed={}", p.display());
    }
    let hash = git(&ws_root, &["rev-parse", "--short", "HEAD"]).filter(|s| !s.is_empty())?;
    // `--no-optional-locks`: never rewrite the index as a side effect.
    let dirty = git(&ws_root, &["--no-optional-locks", "status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|s| !s.is_empty());
    Some(if dirty { format!("{hash}-dirty") } else { hash })
}
