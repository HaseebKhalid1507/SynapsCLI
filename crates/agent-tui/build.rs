//! Build script: capture the git commit the binary was built from, so the TUI
//! can display it next to the version. Resolved at compile time and baked in
//! via `GIT_HASH` — no runtime git shelling. Falls back to "unknown" when git
//! is unavailable (e.g. building from a source tarball).

use std::process::Command;

fn main() {
    let short_hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // Mark builds from a modified working tree so a dev binary is honest about
    // not matching the committed hash.
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let git_hash = match short_hash {
        Some(h) if dirty => format!("{h}-dirty"),
        Some(h) => h,
        None => "unknown".to_string(),
    };
    println!("cargo:rustc-env=GIT_HASH={git_hash}");

    // Rebuild when HEAD moves (new commit/checkout) or the index changes
    // (staging affects the dirty flag) so the displayed hash stays accurate.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}
