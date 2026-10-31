//! Which Git program to execute. Compiled into the host and, by path, into the
//! independently built Axel memory service.
//!
//! On macOS `/usr/bin/git` is Apple's `xcrun` shim. Without the Command Line
//! Tools, executing it opens the "install developer tools" dialog, and
//! `GIT_TERMINAL_PROMPT=0` does not prevent that. So on macOS Synaps never runs
//! the shim: it runs a Git by absolute path, or treats Git as unavailable.
//! Every other platform runs plain `git` from `PATH`, as before.
//!
//! On macOS the host and the service deliberately follow different rules.
//! Do not unify them:
//!
//! - The host ([`git_program`]) runs the Git the user's shell runs: the first
//!   `PATH` directory with a `git`, so Homebrew, Nix and MacPorts Git keep
//!   working. Where `PATH` reaches the shim it runs the developer Git the shim
//!   would have run (so a `PATH` with `/usr/bin` first still gets Apple's Git),
//!   or skips the shim when there is no developer Git. With no Git on `PATH`
//!   at all it falls back to the developer Git.
//! - The Axel service ([`developer_git`]) never consults `PATH`. The host
//!   spawns it with an empty environment (`env_clear`: no `PATH`, no
//!   `DEVELOPER_DIR`), so nothing the host inherits picks the program the
//!   verifier runs. It runs the developer Git the shim would choose in an
//!   empty environment, as `xcrun` did for it before: the `xcode-select`
//!   choice, then Xcode, then the Command Line Tools.
//!
//! So the two can run different Gits (say Homebrew's and Apple's). Both only
//! run read-only `rev-parse` (Git 2.31 or newer), whose paths do not depend on
//! the Git build. A service Git that cannot read the repository (too old for
//! its format, say) makes the service fail closed with no scope, never yield a
//! different identity.
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

/// Apple's `xcrun` shim. Never returned, wherever a candidate points.
const SHIM: &str = "/usr/bin/git";
/// Default Xcode developer directory, `xcode-select`'s first default.
const XCODE: &str = "/Applications/Xcode.app/Contents/Developer";
/// Default Command Line Tools directory, the last candidate.
const COMMAND_LINE_TOOLS: &str = "/Library/Developer/CommandLineTools";
/// Symlink to the developer directory chosen with `xcode-select --switch`.
#[cfg(target_os = "macos")]
const SELECT_LINK: &str = "/var/db/xcode_select_link";

/// macOS has no Git: none on `PATH` and no developer Git (Apple's Command
/// Line Tools are not installed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitUnavailable;

impl std::fmt::Display for GitUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "git unavailable: run `xcode-select --install` \
             (no git on PATH and Apple's Command Line Tools are not installed)",
        )
    }
}

impl std::error::Error for GitUnavailable {}

/// The host's Git, for `Command::new`: an absolute path from
/// [`resolve_path_git`] with this process's `PATH`, falling back to
/// [`developer_git`], or [`GitUnavailable`]. Never `/usr/bin/git`.
#[cfg(target_os = "macos")]
pub fn git_program() -> Result<PathBuf, GitUnavailable> {
    let path = std::env::var_os("PATH");
    resolve_path_git(path.as_deref(), is_real_git, is_shim, || {
        developer_git().ok()
    })
    .ok_or(GitUnavailable)
}

/// The Axel service's Git (and the host's fallback), for `Command::new`:
/// `<dir>/usr/bin/git` by absolute path from [`resolve_developer_git`] with
/// this process's `DEVELOPER_DIR` (never set in the service, whose environment
/// is empty) and the `xcode-select` link, or [`GitUnavailable`]. Never
/// `/usr/bin/git`, never a `PATH` lookup.
#[cfg(target_os = "macos")]
pub fn developer_git() -> Result<PathBuf, GitUnavailable> {
    let developer_dir = std::env::var_os("DEVELOPER_DIR");
    let select_link = std::fs::read_link(SELECT_LINK).ok();
    resolve_developer_git(
        developer_dir.as_deref(),
        select_link.as_deref(),
        is_real_git,
    )
    .ok_or(GitUnavailable)
}

/// Plain `git`, found on `PATH`, exactly as before (only macOS resolves).
#[cfg(not(target_os = "macos"))]
pub fn git_program() -> Result<PathBuf, GitUnavailable> {
    Ok(PathBuf::from("git"))
}

/// Plain `git`, found on `PATH`, exactly as before (only macOS resolves).
#[cfg(not(target_os = "macos"))]
pub fn developer_git() -> Result<PathBuf, GitUnavailable> {
    Ok(PathBuf::from("git"))
}

/// A Git that may be run: following symlinks, an executable regular file that
/// is not the `/usr/bin/git` shim.
#[cfg(unix)]
pub fn is_real_git(git: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::canonicalize(git).is_ok_and(|real| {
        real != Path::new(SHIM)
            && std::fs::metadata(&real)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
}

/// `git` is, or links to, the `/usr/bin/git` shim.
#[cfg(unix)]
pub fn is_shim(git: &Path) -> bool {
    std::fs::canonicalize(git).is_ok_and(|real| real == Path::new(SHIM))
}

/// The macOS host Git. Pure (the caller supplies `PATH`, the file tests and
/// the developer Git), so it builds and is tested on every platform.
///
/// Walks the absolute directories of `path` in order. The first `<dir>/git`
/// that `is_git` accepts wins. One that `is_shim` recognises stands for
/// `developer_git()` (asked at most once): what the shim would run, if there
/// is one; otherwise the walk goes on. After the walk, `developer_git()`.
/// Relative and empty entries (the current directory) are skipped, and the
/// result is never `/usr/bin/git`.
pub fn resolve_path_git(
    path: Option<&OsStr>,
    is_git: impl Fn(&Path) -> bool,
    is_shim: impl Fn(&Path) -> bool,
    developer_git: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    // `None` once asked: if it had found one, the walk would have returned it.
    let mut developer_git = Some(developer_git);
    let mut developer = || developer_git.take().and_then(|find| find());
    // `has_root`, not `is_absolute`: macOS path rules on every test host.
    for dir in std::env::split_paths(path.unwrap_or_default()).filter(|dir| dir.has_root()) {
        let git = dir.join("git");
        if git != Path::new(SHIM) && is_git(&git) {
            return Some(git);
        }
        if is_shim(&git) {
            if let Some(git) = developer() {
                return Some(git);
            }
        }
    }
    developer()
}

/// The macOS developer Git. Pure (the caller supplies `DEVELOPER_DIR`, the
/// target of `/var/db/xcode_select_link` and the file test), so it builds and
/// is tested on every platform. Candidate directories, in order:
///
/// 1. `developer_dir` (`DEVELOPER_DIR`), if set and absolute;
/// 2. `select_link_target`, if absolute;
/// 3. `/Applications/Xcode.app/Contents/Developer`;
/// 4. `/Library/Developer/CommandLineTools`.
///
/// For each, `<dir>/usr/bin/git`, then `<dir>/Contents/Developer/usr/bin/git`
/// (a directory given as the `.app` bundle, as `xcrun` accepts). The first
/// that `is_file` accepts wins. A candidate that is set but has no Git falls
/// through to the next one. Relative candidates and ones with `..` are
/// skipped, and the result is never `/usr/bin/git`, even if a candidate is
/// `/`. `None` means there is no developer Git; there is no `PATH` lookup.
pub fn resolve_developer_git(
    developer_dir: Option<&OsStr>,
    select_link_target: Option<&Path>,
    is_file: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    [
        developer_dir.map(Path::new),
        select_link_target,
        Some(Path::new(XCODE)),
        Some(Path::new(COMMAND_LINE_TOOLS)),
    ]
    .into_iter()
    .flatten()
    // `has_root`, not `is_absolute`: macOS path rules on every test host.
    .filter(|dir| dir.has_root() && !dir.components().any(|c| c == Component::ParentDir))
    .flat_map(|dir| {
        [
            dir.join("usr/bin/git"),
            dir.join("Contents/Developer/usr/bin/git"),
        ]
    })
    .find(|git| git != Path::new(SHIM) && is_file(git))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BETA_APP: &str = "/Applications/Xcode-beta.app";
    const BETA: &str = "/Applications/Xcode-beta.app/Contents/Developer";
    const SELECTED: &str = "/Applications/Xcode-15.app/Contents/Developer";

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
    fn developer_dir_then_select_link_then_xcode_then_command_line_tools() {
        let all = [
            git(BETA),
            git(SELECTED),
            git(XCODE),
            git(COMMAND_LINE_TOOLS),
        ];
        assert_eq!(resolve(Some(BETA), Some(SELECTED), &all), Some(git(BETA)));
        assert_eq!(resolve(None, Some(SELECTED), &all), Some(git(SELECTED)));
        assert_eq!(resolve(None, None, &all), Some(git(XCODE)));
        let tools = PathBuf::from("/Library/Developer/CommandLineTools/usr/bin/git");
        assert_eq!(resolve(None, None, &all[3..]), Some(tools));
    }

    #[test]
    fn a_candidate_without_git_falls_through() {
        let rest = [git(SELECTED), git(XCODE), git(COMMAND_LINE_TOOLS)];
        assert_eq!(
            resolve(Some(BETA), Some(SELECTED), &rest),
            Some(git(SELECTED))
        );
        assert_eq!(
            resolve(Some(BETA), Some(SELECTED), &rest[1..]),
            Some(git(XCODE))
        );
        assert_eq!(
            resolve(Some(BETA), Some(SELECTED), &rest[2..]),
            Some(git(COMMAND_LINE_TOOLS))
        );
    }

    #[test]
    fn an_app_bundle_candidate_uses_its_contents_developer() {
        // DEVELOPER_DIR=/Applications/Xcode-beta.app, as xcrun accepts.
        let present = [git(BETA), git(COMMAND_LINE_TOOLS)];
        assert_eq!(resolve(Some(BETA_APP), None, &present), Some(git(BETA)));
        assert_eq!(resolve(None, Some(BETA_APP), &present), Some(git(BETA)));
        // `/Applications/Xcode.app` given as the bundle too.
        assert_eq!(
            resolve(Some("/Applications/Xcode.app"), None, &[git(XCODE)]),
            Some(git(XCODE))
        );
    }

    #[test]
    fn relative_or_parent_candidates_are_ignored() {
        let skipped = ["", "Developer", "Xcode.app/Contents/Developer", "/opt/../x"];
        let mut present = skipped.map(git).to_vec();
        present.extend(skipped.map(|dir| git(&format!("{dir}/Contents/Developer"))));
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
        assert_eq!(resolve(Some(BETA), Some(SELECTED), &[]), None);
    }

    #[test]
    fn never_the_usr_bin_git_shim() {
        // `is_file` claims the shim exists.
        let present = [git("/"), git(COMMAND_LINE_TOOLS)];
        assert_eq!(present[0], Path::new(SHIM));
        for dir in ["/", "/.", "/usr/..", "/usr/bin/.."] {
            assert_eq!(
                resolve(Some(dir), Some(dir), &present),
                Some(git(COMMAND_LINE_TOOLS))
            );
            assert_eq!(
                resolve(Some(dir), Some(dir), &present[..1]),
                None,
                "{dir:?}"
            );
        }
        assert_eq!(resolve(None, None, &present[..1]), None);
        // Nor from PATH, when `is_git` wrongly accepts it.
        let path = OsStr::new("/usr/bin");
        assert_eq!(
            resolve_path_git(Some(path), |_| true, |_| false, || None),
            None
        );
    }

    /// Host resolution over `path` with `present` as the only Gits, `shims` as
    /// the paths that are the shim and `developer` as the developer Git.
    fn host(
        path: &[&str],
        present: &[&str],
        shims: &[&str],
        developer: Option<&str>,
    ) -> Option<PathBuf> {
        let path = std::env::join_paths(path).unwrap();
        resolve_path_git(
            Some(&path),
            |git| present.iter().any(|p| git == Path::new(p)),
            |git| shims.iter().any(|p| git == Path::new(p)),
            || developer.map(PathBuf::from),
        )
    }

    const BREW: &str = "/opt/homebrew/bin/git";
    const NIX: &str = "/run/current-system/sw/bin/git";
    const TOOLS: &str = "/Library/Developer/CommandLineTools/usr/bin/git";

    #[test]
    fn host_runs_the_first_git_on_path() {
        let path = [
            "/opt/homebrew/bin",
            "/run/current-system/sw/bin",
            "/usr/bin",
        ];
        assert_eq!(
            host(&path, &[BREW, NIX], &[SHIM], Some(TOOLS)),
            Some(BREW.into())
        );
        assert_eq!(host(&path, &[NIX], &[SHIM], Some(TOOLS)), Some(NIX.into()));
        assert_eq!(
            host(&path[1..], &[BREW, NIX], &[SHIM], Some(TOOLS)),
            Some(NIX.into())
        );
    }

    #[test]
    fn host_shim_on_path_stands_for_the_developer_git() {
        // `/usr/bin` before Homebrew: Apple's Git, as `git` in a shell runs.
        let path = ["/usr/bin", "/opt/homebrew/bin"];
        assert_eq!(
            host(&path, &[BREW], &[SHIM], Some(TOOLS)),
            Some(TOOLS.into())
        );
        // No developer Git: skip the shim (no dialog), keep walking.
        assert_eq!(host(&path, &[BREW], &[SHIM], None), Some(BREW.into()));
        // A link to the shim elsewhere counts as the shim.
        let link = "/usr/local/bin/git";
        let path = ["/usr/local/bin", "/opt/homebrew/bin"];
        assert_eq!(
            host(&path, &[BREW], &[link], Some(TOOLS)),
            Some(TOOLS.into())
        );
        assert_eq!(host(&path, &[BREW], &[link], None), Some(BREW.into()));
    }

    #[test]
    fn host_falls_back_to_the_developer_git() {
        assert_eq!(
            host(&["/opt/homebrew/bin"], &[], &[], Some(TOOLS)),
            Some(TOOLS.into())
        );
        assert_eq!(host(&[], &[], &[], Some(TOOLS)), Some(TOOLS.into()));
        assert_eq!(
            resolve_path_git(None, |_| true, |_| true, || Some(TOOLS.into())),
            Some(TOOLS.into())
        );
        assert_eq!(host(&["/usr/bin"], &[], &[SHIM], None), None);
        assert_eq!(host(&[], &[], &[], None), None);
    }

    #[test]
    fn host_asks_for_the_developer_git_at_most_once() {
        let asked = std::cell::Cell::new(0);
        let path = std::env::join_paths(["/usr/bin", "/usr/bin", "/opt/homebrew/bin"]).unwrap();
        let found = resolve_path_git(
            Some(&path),
            |_| false,
            |git| git == Path::new(SHIM),
            || {
                asked.set(asked.get() + 1);
                None
            },
        );
        assert_eq!((found, asked.get()), (None, 1));
    }

    #[test]
    fn host_skips_relative_and_empty_path_entries() {
        let path = ["", ".", "bin", "/opt/homebrew/bin"];
        let present = ["git", "./git", "bin/git", BREW];
        assert_eq!(host(&path, &present, &[], None), Some(BREW.into()));
        assert_eq!(host(&path[..3], &present, &[], None), None);
    }

    /// A unique temporary directory, removed on drop (no dev-dependency: this
    /// module is also built into the Axel service).
    #[cfg(unix)]
    struct TempDir(PathBuf);

    #[cfg(unix)]
    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let name = format!(
                "synaps-git-program-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let dir = std::env::temp_dir().join(name);
            std::fs::create_dir(&dir).unwrap();
            Self(dir.canonicalize().unwrap())
        }

        fn file(&self, name: &str, mode: u32) -> PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let path = self.0.join(name);
            std::fs::write(&path, b"#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path
        }
    }

    #[cfg(unix)]
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn real_git_is_an_executable_regular_file() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new();
        let real = tmp.file("git", 0o755);
        assert!(is_real_git(&real));
        assert!(is_real_git(&tmp.file("owner-only", 0o700)));
        let link = tmp.0.join("link");
        symlink(&real, &link).unwrap();
        assert!(is_real_git(&link), "a link to a real Git, like Homebrew's");

        assert!(!is_real_git(&tmp.0), "a directory");
        let dir = tmp.0.join("dir");
        std::fs::create_dir(&dir).unwrap();
        assert!(!is_real_git(&dir), "a directory named for Git");
        let dangling = tmp.0.join("dangling");
        symlink(tmp.0.join("missing"), &dangling).unwrap();
        assert!(!is_real_git(&dangling), "a dangling link");
        assert!(!is_real_git(&tmp.0.join("missing")), "nothing");
        assert!(!is_real_git(&tmp.file("plain", 0o644)), "not executable");
        assert!(!is_shim(&real) && !is_shim(&dangling));
    }

    #[cfg(unix)]
    #[test]
    fn real_git_is_never_the_shim() {
        use std::os::unix::fs::symlink;
        let shim = Path::new(SHIM);
        if std::fs::canonicalize(shim).ok().as_deref() != Some(shim) {
            eprintln!("skipped: no /usr/bin/git on this host");
            return;
        }
        let tmp = TempDir::new();
        let link = tmp.0.join("git");
        symlink(shim, &link).unwrap();
        assert!(!is_real_git(shim) && !is_real_git(&link));
        assert!(is_shim(shim) && is_shim(&link));
    }

    #[cfg(unix)]
    #[test]
    fn host_resolution_on_the_real_file_system() {
        let (brew, empty) = (TempDir::new(), TempDir::new());
        let git = brew.file("git", 0o755);
        std::fs::create_dir(empty.0.join("git")).unwrap(); // a directory, skipped
        let tools = PathBuf::from(TOOLS);
        let resolve = |path: &[&Path], developer: Option<&PathBuf>| {
            let path = std::env::join_paths(path).unwrap();
            resolve_path_git(Some(&path), is_real_git, is_shim, || developer.cloned())
        };
        assert_eq!(
            resolve(&[&empty.0, &brew.0], Some(&tools)),
            Some(git.clone())
        );
        assert_eq!(resolve(&[&empty.0], Some(&tools)), Some(tools.clone()));
        if is_shim(Path::new(SHIM)) {
            let usr_bin = Path::new("/usr/bin");
            assert_eq!(resolve(&[usr_bin, &brew.0], Some(&tools)), Some(tools));
            assert_eq!(resolve(&[usr_bin, &brew.0], None), Some(git));
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn other_platforms_run_git_from_path() {
        assert_eq!(git_program(), Ok(PathBuf::from("git")));
        assert_eq!(developer_git(), Ok(PathBuf::from("git")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_runs_an_absolute_git_never_the_shim() {
        let host = git_program();
        let developer = developer_git();
        // CI's macOS runners ship Xcode: there the answer must be a Git.
        if std::env::var_os("CI").is_some() {
            assert!(developer.is_ok(), "no developer git on a macOS CI runner");
            assert!(host.is_ok(), "no git on a macOS CI runner");
        }
        for git in [host, developer].into_iter().flatten() {
            assert!(git.is_absolute() && git != Path::new(SHIM), "{git:?}");
            assert!(is_real_git(&git), "{git:?}");
        }
        if let Ok(git) = developer_git() {
            assert!(git.ends_with("usr/bin/git"), "{git:?}");
        }
        // The host Git is `PATH`'s, when `PATH` has a real Git before the shim.
        let path = std::env::var_os("PATH").unwrap_or_default();
        let first = std::env::split_paths(&path)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join("git"))
            .find(|git| is_real_git(git) || is_shim(git));
        if let Some(first) = first.filter(|git| is_real_git(git)) {
            assert_eq!(git_program(), Ok(first));
        }
    }
}
