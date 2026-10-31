//! Which Git program to execute; shared by host and independently built Axel
//! service (included by path), so both always pick the same binary.
//!
//! On macOS `/usr/bin/git` is Apple's `xcrun` shim. Without the Command Line
//! Tools, executing it opens the "install developer tools" dialog, and
//! `GIT_TERMINAL_PROMPT=0` does not prevent that. So on macOS Synaps never runs
//! the shim and never searches `PATH`: it runs a developer Git by absolute path,
//! or treats Git as unavailable. Every other platform runs `git` from `PATH`.
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

/// Apple's `xcrun` shim. Never returned, wherever a candidate points.
const SHIM: &str = "/usr/bin/git";
/// Default Command Line Tools directory, the last candidate.
const COMMAND_LINE_TOOLS: &str = "/Library/Developer/CommandLineTools";
/// Symlink to the developer directory chosen with `xcode-select --switch`.
#[cfg(target_os = "macos")]
const SELECT_LINK: &str = "/var/db/xcode_select_link";

/// macOS has no developer Git (Apple's Command Line Tools are not installed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitUnavailable;

impl std::fmt::Display for GitUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no developer git (Apple's Command Line Tools are not installed)")
    }
}

impl std::error::Error for GitUnavailable {}

/// The program to pass to `Command::new` for Git: `<dir>/usr/bin/git` by
/// absolute path, from [`resolve_developer_git`] with this process's
/// `DEVELOPER_DIR` and the `xcode-select` link, or [`GitUnavailable`].
/// Never `/usr/bin/git`, never a `PATH` lookup.
#[cfg(target_os = "macos")]
pub fn git_program() -> Result<PathBuf, GitUnavailable> {
    let developer_dir = std::env::var_os("DEVELOPER_DIR");
    let select_link = std::fs::read_link(SELECT_LINK).ok();
    resolve_developer_git(developer_dir.as_deref(), select_link.as_deref(), |git| {
        // A regular file, following symlinks, but never one to the shim.
        std::fs::canonicalize(git).is_ok_and(|real| real.is_file() && real != Path::new(SHIM))
    })
    .ok_or(GitUnavailable)
}

/// The program to pass to `Command::new` for Git: plain `git`, found on
/// `PATH`, exactly as before (only macOS resolves a developer Git).
#[cfg(not(target_os = "macos"))]
pub fn git_program() -> Result<PathBuf, GitUnavailable> {
    Ok(PathBuf::from("git"))
}

/// The macOS developer Git. Pure (the caller supplies `DEVELOPER_DIR`, the
/// target of `/var/db/xcode_select_link` and the file test), so it builds and
/// is tested on every platform. Candidate directories, in order:
///
/// 1. `developer_dir` (`DEVELOPER_DIR`), if set and absolute;
/// 2. `select_link_target`, if absolute;
/// 3. `/Library/Developer/CommandLineTools`.
///
/// The first `<dir>/usr/bin/git` that `is_file` accepts wins. A candidate that
/// is set but has no Git falls through to the next one. Relative candidates and
/// ones with `..` are skipped, and the result is never `/usr/bin/git`, even if
/// a candidate is `/`. `None` means Git is unavailable; there is no `PATH`
/// fallback.
pub fn resolve_developer_git(
    developer_dir: Option<&OsStr>,
    select_link_target: Option<&Path>,
    is_file: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    [
        developer_dir.map(Path::new),
        select_link_target,
        Some(Path::new(COMMAND_LINE_TOOLS)),
    ]
    .into_iter()
    .flatten()
    // `has_root`, not `is_absolute`: macOS path rules on every test host.
    .filter(|dir| dir.has_root() && !dir.components().any(|c| c == Component::ParentDir))
    .map(|dir| dir.join("usr/bin/git"))
    .find(|git| git != Path::new(SHIM) && is_file(git))
}

#[cfg(test)]
mod tests {
    use super::*;

    const XCODE: &str = "/Applications/Xcode.app/Contents/Developer";
    const BETA: &str = "/Applications/Xcode-beta.app/Contents/Developer";

    fn git(dir: &str) -> PathBuf {
        Path::new(dir).join("usr/bin/git")
    }

    /// Resolve with `present` as the only existing regular files.
    fn resolve(dev: Option<&str>, link: Option<&str>, present: &[PathBuf]) -> Option<PathBuf> {
        resolve_developer_git(dev.map(OsStr::new), link.map(Path::new), |p| {
            present.iter().any(|g| g == p)
        })
    }

    #[test]
    fn developer_dir_then_select_link_then_command_line_tools() {
        let all = [git(BETA), git(XCODE), git(COMMAND_LINE_TOOLS)];
        assert_eq!(resolve(Some(BETA), Some(XCODE), &all), Some(git(BETA)));
        assert_eq!(resolve(None, Some(XCODE), &all), Some(git(XCODE)));
        assert_eq!(resolve(None, None, &all), Some(git(COMMAND_LINE_TOOLS)));
        let tools = PathBuf::from("/Library/Developer/CommandLineTools/usr/bin/git");
        assert_eq!(resolve(None, None, &all[2..]), Some(tools));
    }

    #[test]
    fn a_candidate_without_git_falls_through() {
        let rest = [git(XCODE), git(COMMAND_LINE_TOOLS)];
        assert_eq!(resolve(Some(BETA), Some(XCODE), &rest), Some(git(XCODE)));
        assert_eq!(
            resolve(Some(BETA), Some(XCODE), &rest[1..]),
            Some(git(COMMAND_LINE_TOOLS))
        );
    }

    #[test]
    fn relative_or_parent_candidates_are_ignored() {
        let skipped = ["", "Developer", "Xcode.app/Contents/Developer", "/opt/../x"];
        let mut present = skipped.map(git).to_vec();
        let relative = Some("Developer");
        assert_eq!(resolve(relative, relative, &present), None);
        present.push(git(COMMAND_LINE_TOOLS));
        for dir in skipped {
            let found = resolve(Some(dir), Some(dir), &present);
            assert_eq!(found, Some(git(COMMAND_LINE_TOOLS)), "{dir:?}");
        }
    }

    #[test]
    fn no_developer_git_is_unavailable() {
        assert_eq!(resolve(None, None, &[]), None);
        assert_eq!(resolve(Some(BETA), Some(XCODE), &[]), None);
    }

    #[test]
    fn never_the_usr_bin_git_shim() {
        // `is_file` claims every path exists, the shim included.
        let everything = [git("/"), git(COMMAND_LINE_TOOLS)];
        assert_eq!(everything[0], Path::new(SHIM));
        for dir in ["/", "/.", "/usr/..", "/usr/bin/.."] {
            let found =
                resolve_developer_git(Some(OsStr::new(dir)), Some(Path::new(dir)), |_| true);
            assert_eq!(found, Some(git(COMMAND_LINE_TOOLS)), "{dir:?}");
            assert_eq!(resolve(Some(dir), Some(dir), &everything[..1]), None);
        }
        assert_eq!(resolve(None, None, &everything[..1]), None);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn other_platforms_run_git_from_path() {
        assert_eq!(git_program(), Ok(PathBuf::from("git")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_runs_an_absolute_developer_git_or_none() {
        if let Ok(git) = git_program() {
            assert!(git.is_absolute() && git.ends_with("usr/bin/git") && git != Path::new(SHIM));
        }
    }
}
