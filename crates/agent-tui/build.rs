//! Build script: capture the git commit the binary was built from, so the TUI
//! can display it next to the version. Resolved at compile time and baked in
//! via `GIT_HASH` — no runtime git shelling.
//!
//! IMPORTANT: only trust git when its repo root IS our own workspace root.
//! Source-tarball builds (AUR, Homebrew, crates.io) are often extracted inside
//! an UNRELATED git repo — e.g. the AUR package's own checkout — and a naive
//! `git rev-parse` there walks up and returns that repo's hash plus a spurious
//! `-dirty` (the extracted files look untracked). For any packaged build we
//! fall back to `release` instead of baking a misleading hash.

use std::path::Path;
use std::process::Command;

fn main() {
    let git_hash = synaps_repo_hash().unwrap_or_else(|| "release".to_string());
    println!("cargo:rustc-env=GIT_HASH={git_hash}");

    // Rebuild when HEAD moves or the index changes so the hash stays accurate
    // in-repo. Harmless (ignored) when these paths don't exist in a tarball.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}

/// Short commit (`+ "-dirty"`) ONLY when building inside the real synaps repo.
/// Returns `None` for any other situation (no git, or git resolves to a
/// different/unrelated repository).
fn synaps_repo_hash() -> Option<String> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").ok()?; // .../crates/agent-tui
                                                              // Our workspace root is two levels up from this crate dir.
    let ws_root = Path::new(&manifest)
        .parent()?
        .parent()?
        .canonicalize()
        .ok()?;

    // Where does git think the repo root is, starting from here?
    let toplevel = Command::new("git")
        .args(["-C", &manifest, "rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| Path::new(s.trim()).canonicalize().ok())?;

    // If git's repo root isn't OUR workspace root, we're inside an unrelated
    // repo (tarball extracted in some package's git dir) — don't trust it.
    if toplevel != ws_root {
        return None;
    }

    let hash = Command::new("git")
        .args(["-C", &manifest, "rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;

    let dirty = Command::new("git")
        .args([
            "-C",
            &manifest,
            "status",
            "--porcelain",
            "--untracked-files=no",
        ])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    Some(if dirty { format!("{hash}-dirty") } else { hash })
}
