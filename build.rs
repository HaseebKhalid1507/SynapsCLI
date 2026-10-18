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
//! repo's hash (as in `agent-tui`'s build script). Otherwise empty: unknown,
//! and clients compare versions only.
//!
//! `-dirty` is computed whenever the script runs, which is when HEAD or its
//! ref moves (commit, checkout, reset). It deliberately does NOT watch
//! `.git/index`, so it reflects the tree at the last HEAD move, not edits
//! made since. Two builds of one dirty commit look the same either way.

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
    // Rerun when the commit moves: HEAD (branch switch, detached moves), the
    // branch's ref file (a commit), and packed-refs (a ref packed by gc). In a
    // worktree `.git` is a file, so ask git for the directories. Only paths
    // that exist: cargo treats a missing one as always changed.
    let git_dir = abs_git_path(&ws_root, &["rev-parse", "--git-dir"]);
    let common_dir = abs_git_path(&ws_root, &["rev-parse", "--git-common-dir"]);
    let head_ref = git(&ws_root, &["rev-parse", "--symbolic-full-name", "HEAD"]);
    let mut watched: Vec<PathBuf> = Vec::new();
    if let Some(d) = &git_dir {
        watched.push(d.join("HEAD"));
    }
    if let Some(d) = &common_dir {
        watched.push(d.join("packed-refs"));
        if let Some(r) = head_ref.as_deref().filter(|r| r.starts_with("refs/")) {
            watched.push(d.join(r));
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
