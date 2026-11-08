//! The program behind the `ls` tool. (`grep` and `find` no longer run a
//! program: they walk in-process, see `tools::search`.)
//!
//! Unix: `ls` from PATH, as always.
//!
//! Windows has no `ls` on a stock install, so the tool runs a GNU build: from
//! a PATH directory outside the Windows directory (e.g. Git's `usr\bin` when
//! the user put it there), else from Git for Windows' own `usr\bin`, found
//! next to `git.exe` on PATH or in the standard install folders. With
//! neither, the call fails with a message pointing at Git for Windows and the
//! `powershell` tool.
//!
//! Git's GNU tools are MSYS programs. When a non-MSYS process starts one, the
//! MSYS runtime expands wildcards in its arguments against its working
//! directory. The child therefore gets `noglob` added to `MSYS`, so a path
//! with `*` or `?` in it reaches `ls` as written.

use std::ffi::OsString;
#[cfg(any(windows, test))]
use std::ffi::OsStr;
#[cfg(any(windows, test))]
use std::path::{Path, PathBuf};

use tokio::process::Command;

use super::ToolContext;
use crate::Result;
#[cfg(windows)]
use crate::RuntimeError;

/// A `Command` for `name` (`ls`) with the session's env snapshot applied,
/// ready for the caller's arguments.
pub(crate) fn unix_tool_command(name: &str, ctx: &ToolContext) -> Result<Command> {
    let env = ctx.capabilities.env.as_ref();
    #[cfg(windows)]
    let program = {
        let path_var = match env {
            Some(env) => env_lookup(env, "PATH").map(OsString::from),
            None => std::env::var_os("PATH"),
        };
        let system_root = std::env::var_os("SystemRoot").map(PathBuf::from);
        resolve_windows(name, path_var.as_deref(), system_root.as_deref(), &git_roots(path_var.as_deref()))
            .ok_or_else(|| RuntimeError::Tool(missing_message(name)))?
            .into_os_string()
    };
    #[cfg(not(windows))]
    let program = OsString::from(name);

    let mut cmd = Command::new(program);
    if let Some(env) = env {
        cmd.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
    }
    #[cfg(windows)]
    {
        let current = match env {
            Some(env) => env_lookup(env, "MSYS").map(OsString::from),
            None => std::env::var_os("MSYS"),
        };
        cmd.env("MSYS", msys_with_noglob(current.as_deref()));
    }
    Ok(cmd)
}

/// The error for a Windows machine without a GNU `name`.
#[cfg(any(windows, test))]
fn missing_message(name: &str) -> String {
    format!(
        "The {name} tool needs a GNU {name}.exe, and none was found. On Windows it runs Git for \
         Windows' usr\\bin\\{name}.exe: install Git for Windows, or use the powershell tool instead."
    )
}

/// Case-insensitive env lookup (Windows env names are case-insensitive, and
/// a snapshot can carry `Path` rather than `PATH`).
#[cfg(any(windows, test))]
fn env_lookup<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

/// `MSYS` with `noglob` added (kept as is when already there).
#[cfg(any(windows, test))]
fn msys_with_noglob(current: Option<&OsStr>) -> OsString {
    match current.map(|c| c.to_string_lossy().trim().to_string()) {
        Some(c) if c.split_whitespace().any(|o| o == "noglob") => OsString::from(c),
        Some(c) if !c.is_empty() => OsString::from(format!("{c} noglob")),
        _ => OsString::from("noglob"),
    }
}

/// Where Git for Windows may be installed: derived from `git.exe` on PATH
/// (`<root>\cmd`, `<root>\bin` or `<root>\mingw64\bin`), then the standard
/// machine-wide and per-user install folders.
#[cfg(windows)]
fn git_roots(path_var: Option<&OsStr>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(path_var) = path_var {
        for dir in std::env::split_paths(path_var) {
            if dir.join("git.exe").is_file() {
                roots.extend(git_root_from_git_dir(&dir));
            }
        }
    }
    for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(pf) = std::env::var_os(var) {
            roots.push(Path::new(&pf).join("Git"));
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        roots.push(Path::new(&local).join(r"Programs\Git"));
    }
    roots
}

/// `<root>` for a directory holding Git's `git.exe`.
#[cfg(any(windows, test))]
fn git_root_from_git_dir(dir: &Path) -> Option<PathBuf> {
    let leaf = dir.file_name()?.to_string_lossy().to_ascii_lowercase();
    let parent = dir.parent()?;
    match leaf.as_str() {
        "cmd" => Some(parent.to_path_buf()),
        "bin" => {
            let up = parent.file_name()?.to_string_lossy().to_ascii_lowercase();
            if up == "mingw64" || up == "mingw32" || up == "clangarm64" {
                parent.parent().map(Path::to_path_buf)
            } else {
                Some(parent.to_path_buf())
            }
        }
        _ => None,
    }
}

/// The GNU `name.exe` to run: a PATH directory outside `system_root` first,
/// then `<git root>\usr\bin`.
#[cfg(any(windows, test))]
fn resolve_windows(
    name: &str,
    path_var: Option<&OsStr>,
    system_root: Option<&Path>,
    git_roots: &[PathBuf],
) -> Option<PathBuf> {
    let exe = format!("{name}.exe");
    let in_system_root = |dir: &Path| {
        system_root.is_some_and(|root| {
            let dir = dir.to_string_lossy().to_ascii_lowercase();
            let root = root.to_string_lossy().to_ascii_lowercase();
            dir == root || dir.starts_with(&format!("{}\\", root.trim_end_matches('\\')))
                || dir.starts_with(&format!("{}/", root.trim_end_matches('/')))
        })
    };
    if let Some(path_var) = path_var {
        for dir in std::env::split_paths(path_var) {
            if dir.as_os_str().is_empty() || in_system_root(&dir) {
                continue;
            }
            let candidate = dir.join(&exe);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    git_roots
        .iter()
        .map(|root| root.join("usr").join("bin").join(&exe))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"").unwrap();
    }

    fn path_of(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    /// A program of that name in the Windows directory (as `find.exe`, an
    /// unrelated string search, was for the old GNU find) is skipped.
    #[test]
    fn the_windows_dir_is_skipped_for_git_usr_bin() {
        let tmp = tempfile::tempdir().unwrap();
        let windows = tmp.path().join("Windows");
        let system32 = windows.join("System32");
        touch(&system32.join("ls.exe"));
        let git = tmp.path().join("Git");
        touch(&git.join(r"usr").join("bin").join("ls.exe"));

        let path = path_of(&[&system32]);
        let found = resolve_windows("ls", Some(&path), Some(&windows), std::slice::from_ref(&git));
        assert_eq!(found, Some(git.join("usr").join("bin").join("ls.exe")));
    }

    #[test]
    fn a_gnu_tool_on_path_outside_the_windows_dir_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let tools = tmp.path().join("gnu");
        touch(&tools.join("ls.exe"));
        let git = tmp.path().join("Git");
        touch(&git.join("usr").join("bin").join("ls.exe"));

        let path = path_of(&[&tools]);
        let found = resolve_windows("ls", Some(&path), None, &[git]);
        assert_eq!(found, Some(tools.join("ls.exe")));
    }

    #[test]
    fn nothing_found_is_none_and_the_message_points_at_git_and_powershell() {
        let tmp = tempfile::tempdir().unwrap();
        let windows = tmp.path().join("Windows");
        let system32 = windows.join("System32");
        touch(&system32.join("ls.exe"));
        let path = path_of(&[&system32]);
        assert_eq!(resolve_windows("ls", Some(&path), Some(&windows), &[]), None);
        assert_eq!(resolve_windows("ls", None, None, &[tmp.path().join("NoGit")]), None);

        let msg = missing_message("ls");
        assert!(msg.contains("Git for Windows") && msg.contains("powershell"), "{msg}");
    }

    #[test]
    fn git_root_is_derived_from_where_git_exe_lives() {
        let root = Path::new("Git");
        assert_eq!(git_root_from_git_dir(&root.join("cmd")), Some(root.to_path_buf()));
        assert_eq!(git_root_from_git_dir(&root.join("bin")), Some(root.to_path_buf()));
        assert_eq!(
            git_root_from_git_dir(&root.join("mingw64").join("bin")),
            Some(root.to_path_buf())
        );
        assert_eq!(git_root_from_git_dir(&root.join("libexec")), None);
    }

    #[test]
    fn msys_gets_noglob_once_and_keeps_other_options() {
        assert_eq!(msys_with_noglob(None), OsString::from("noglob"));
        assert_eq!(msys_with_noglob(Some(OsStr::new(""))), OsString::from("noglob"));
        assert_eq!(
            msys_with_noglob(Some(OsStr::new("winsymlinks:nativestrict"))),
            OsString::from("winsymlinks:nativestrict noglob")
        );
        assert_eq!(
            msys_with_noglob(Some(OsStr::new("noglob winsymlinks:native"))),
            OsString::from("noglob winsymlinks:native")
        );
    }

    #[test]
    fn env_lookup_ignores_case() {
        let env = vec![("Path".to_string(), "a".to_string()), ("msys".to_string(), "b".to_string())];
        assert_eq!(env_lookup(&env, "PATH"), Some("a"));
        assert_eq!(env_lookup(&env, "MSYS"), Some("b"));
        assert_eq!(env_lookup(&env, "HOME"), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_command_is_the_plain_program_name() {
        let ctx = super::super::test_helpers::create_tool_context();
        let cmd = unix_tool_command("ls", &ctx).unwrap();
        assert_eq!(cmd.as_std().get_program(), OsStr::new("ls"));
    }
}
