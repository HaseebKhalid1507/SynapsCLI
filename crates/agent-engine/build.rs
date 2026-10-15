//! Build script: the build id the daemon reports in its handshake
//! (`Welcome::daemon_build`), so an attached client can tell it is not the
//! same build as its daemon even when the version number is the same (a
//! `daemon reload` onto a newer build of the same release re-execs only the
//! daemon). Baked in as `SYNAPS_BUILD_ID`; no runtime git shelling.
//!
//! `SYNAPS_BUILD_ID` from the build environment wins (packagers). Otherwise
//! the short commit (`-dirty` with uncommitted tracked changes), but only when
//! git's repo root IS our workspace root: a source tarball extracted inside an
//! unrelated repo must not borrow that repo's hash (as in `agent-tui`'s
//! build script). Otherwise empty: unknown, and clients compare versions only.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=SYNAPS_BUILD_ID");
    let id = std::env::var("SYNAPS_BUILD_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(repo_build_id)
        .unwrap_or_default();
    println!("cargo:rustc-env=SYNAPS_BUILD_ID={}", id.trim());
}

fn git(dir: &str, args: &[&str]) -> Option<String> {
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

fn repo_build_id() -> Option<String> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").ok()?; // .../crates/agent-engine
    let ws_root = Path::new(&manifest).parent()?.parent()?.canonicalize().ok()?;
    let toplevel = PathBuf::from(git(&manifest, &["rev-parse", "--show-toplevel"])?)
        .canonicalize()
        .ok()?;
    if toplevel != ws_root {
        return None;
    }
    // Rerun when the commit moves. Absolute paths from git itself (in a
    // worktree `.git` is a file, not the directory), and only paths that
    // exist: cargo treats a missing one as always changed.
    let git_dir = git(&manifest, &["rev-parse", "--path-format=absolute", "--git-dir"]);
    let common_dir = git(&manifest, &["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    let head_ref = git(&manifest, &["rev-parse", "--symbolic-full-name", "HEAD"]);
    let mut watched: Vec<PathBuf> = Vec::new();
    if let Some(d) = &git_dir {
        watched.extend([Path::new(d).join("HEAD"), Path::new(d).join("index")]);
    }
    if let Some(d) = &common_dir {
        watched.push(Path::new(d).join("packed-refs"));
        if let Some(r) = head_ref.as_deref().filter(|r| r.starts_with("refs/")) {
            watched.push(Path::new(d).join(r));
        }
    }
    for p in watched.iter().filter(|p| p.exists()) {
        println!("cargo:rerun-if-changed={}", p.display());
    }
    let hash = git(&manifest, &["rev-parse", "--short", "HEAD"]).filter(|s| !s.is_empty())?;
    // `--no-optional-locks`: never rewrite the index we watch.
    let dirty = git(&manifest, &["--no-optional-locks", "status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|s| !s.is_empty());
    Some(if dirty { format!("{hash}-dirty") } else { hash })
}
