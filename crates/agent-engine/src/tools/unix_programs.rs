//! The programs behind the `find` and `ls` tools. (`grep` no longer runs a
//! program: it searches in-process, see `tools::search`.)
//!
//! Unix: `find` and `ls` from PATH, as always.
//!
//! Windows has neither on a stock install, and the `find` that PATH (and
//! the system search order) does give is `System32\find.exe`, an unrelated
//! string search that answers "no files found" instead of failing. So the
//! tools run a GNU build: from a PATH directory outside the Windows directory
//! (e.g. Git's `usr\bin` when the user put it there), else from Git for
//! Windows' own `usr\bin`, found next to `git.exe` on PATH or in the standard
//! install folders. With neither, the call fails with a message pointing at
//! Git for Windows and the `powershell` tool.
//!
//! Git's GNU tools are MSYS programs. When a non-MSYS process starts one, the
//! MSYS runtime expands wildcards in its arguments against its working
//! directory, which turns `-name "*.rs"` into file names. The child
//! therefore gets `noglob` added to `MSYS`.
//!
//! [`run_bounded`] runs `find`: stdout is read as it arrives and the child
//! is killed once it passes the byte cap or the deadline, stderr is
//! summarised (errors, warnings, binary-file matches) and the exit status is
//! kept, so a failure is never mistaken for an empty result.

use std::ffi::OsString;
#[cfg(any(windows, test))]
use std::ffi::OsStr;
#[cfg(any(windows, test))]
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr, ChildStdout, Command};

use super::ToolContext;
use crate::{Result, RuntimeError};

/// A `Command` for `name` (`find` or `ls`) with the session's env
/// snapshot applied, ready for the caller's arguments.
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

/// What one bounded run of `find` produced.
pub(crate) struct BoundedRun {
    /// stdout as read: at most the cap plus one read.
    pub stdout: Vec<u8>,
    /// stdout passed the cap, so the child was killed.
    pub capped: bool,
    /// The deadline passed, so the child was killed.
    pub timed_out: bool,
    /// The exit code, when the child exited on its own (not when it was
    /// killed, by us or by a signal).
    pub code: Option<i32>,
    pub stderr: StderrSummary,
}

impl BoundedRun {
    /// stdout as text, at most `cap` bytes, and whether it was cut there.
    /// A run stopped at the cap or the deadline can end mid-line; that
    /// partial line is dropped.
    pub fn text(&self, cap: usize) -> (String, bool) {
        let text = String::from_utf8_lossy(&self.stdout);
        let bounded = agent_core::BoundedText::new(&text, cap);
        let cut = self.capped || bounded.truncated;
        let mut text = bounded.text;
        if (cut || self.timed_out) && !text.ends_with('\n') {
            if let Some(nl) = text.rfind('\n') {
                text.truncate(nl);
            }
        }
        (text.trim_end_matches(['\n', '\r']).to_string(), cut)
    }
}

/// stderr of a run, line by line: binary-file matches (GNU grep 3.5+ reports
/// them here), warnings, and everything else counted as an error.
#[derive(Debug, Default)]
pub(crate) struct StderrSummary {
    program: &'static str,
    partial: Vec<u8>,
    pub errors: usize,
    /// First error line, without the program-name prefix.
    pub first_error: Option<String>,
    /// Distinct warnings, at most [`MAX_LISTED`].
    pub warnings: Vec<String>,
    /// Files that matched as binary, at most [`MAX_LISTED`] kept.
    pub binary: Vec<String>,
    pub binary_count: usize,
}

/// Paths or warnings kept for a note.
pub(crate) const MAX_LISTED: usize = 5;
/// Bytes kept from one stderr line; the rest of a longer line is dropped.
const STDERR_LINE_MAX: usize = 2048;

impl StderrSummary {
    pub fn new(program: &'static str) -> Self {
        Self {
            program,
            ..Self::default()
        }
    }

    pub fn push(&mut self, mut bytes: &[u8]) {
        while let Some(nl) = bytes.iter().position(|&b| b == b'\n') {
            self.keep(&bytes[..nl]);
            let line = std::mem::take(&mut self.partial);
            self.line(&line);
            bytes = &bytes[nl + 1..];
        }
        self.keep(bytes);
    }

    fn keep(&mut self, bytes: &[u8]) {
        let room = STDERR_LINE_MAX.saturating_sub(self.partial.len());
        self.partial
            .extend_from_slice(&bytes[..bytes.len().min(room)]);
    }

    pub fn finish(&mut self) {
        let line = std::mem::take(&mut self.partial);
        self.line(&line);
    }

    fn line(&mut self, raw: &[u8]) {
        let line = String::from_utf8_lossy(raw);
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() {
            return;
        }
        let msg = strip_program(line, self.program);
        if let Some(path) = msg.strip_suffix(": binary file matches") {
            self.add_binary(path);
        } else if let Some(warning) = msg.strip_prefix("warning: ") {
            if self.warnings.len() < MAX_LISTED && !self.warnings.iter().any(|w| w == warning) {
                self.warnings.push(warning.to_string());
            }
        } else {
            self.errors += 1;
            self.first_error.get_or_insert_with(|| msg.to_string());
        }
    }

    pub fn add_binary(&mut self, path: &str) {
        self.binary_count += 1;
        if self.binary.len() < MAX_LISTED {
            self.binary.push(path.to_string());
        }
    }

    /// One-line notes for the result: binary matches, warnings, errors.
    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.binary_count > 0 {
            let more = self.binary_count - self.binary.len();
            let list = self.binary.join(", ");
            let list = if more > 0 {
                format!("{list} and {more} more")
            } else {
                list
            };
            notes.push(if self.binary_count == 1 {
                format!("[binary file matches, lines not shown: {list}]")
            } else {
                format!(
                    "[{} binary files match, lines not shown: {list}]",
                    self.binary_count
                )
            });
        }
        for warning in &self.warnings {
            notes.push(format!("[{} warning: {warning}]", self.program));
        }
        if let Some(first) = &self.first_error {
            let s = if self.errors == 1 { "" } else { "s" };
            notes.push(format!(
                "[{} error{s} while searching, results may be incomplete. First: {}: {first}]",
                self.errors, self.program
            ));
        }
        notes
    }

    /// The tool error for a failed run with no results: the first stderr line
    /// and how many more there were.
    pub fn failure(&self, code: Option<i32>) -> String {
        match &self.first_error {
            Some(first) if self.errors > 1 => {
                let more = self.errors - 1;
                let s = if more == 1 { "" } else { "s" };
                format!("{}: {first} (and {more} more error{s})", self.program)
            }
            Some(first) => format!("{}: {first}", self.program),
            None => match code {
                Some(code) => format!("{} failed with exit status {code}", self.program),
                None => format!("{} was terminated by a signal", self.program),
            },
        }
    }
}

/// `line` without the `prog: ` prefix GNU and BSD tools put on messages. The
/// prefix is argv[0], which on Windows can be a full path to `prog.exe`.
pub(crate) fn strip_program<'a>(line: &'a str, program: &str) -> &'a str {
    if let Some((prefix, rest)) = line.split_once(": ") {
        let leaf = prefix.rsplit(['/', '\\']).next().unwrap_or(prefix);
        let leaf = leaf.strip_suffix(".exe").unwrap_or(leaf);
        if leaf.eq_ignore_ascii_case(program) {
            return rest;
        }
    }
    line
}

/// Run `cmd` (from [`unix_tool_command`]) to completion, the stdout cap or
/// the deadline, whichever comes first. The child is killed at the cap or the
/// deadline, and when the caller's future is dropped (`kill_on_drop`), so a
/// timed-out or cancelled search never keeps running.
pub(crate) async fn run_bounded(
    mut cmd: Command,
    program: &'static str,
    cap: usize,
    timeout: Duration,
) -> Result<BoundedRun> {
    // English messages, so binary-file notices and warnings are recognised
    // (an LC_ALL in the session env still wins; such lines then count as
    // errors, which are still reported).
    cmd.env("LC_MESSAGES", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| RuntimeError::Tool(format!("Failed to execute {program}: {e}")))?;
    let mut pipes = Pipes::new(&mut child);
    let mut run = BoundedRun {
        stdout: Vec::new(),
        capped: false,
        timed_out: false,
        code: None,
        stderr: StderrSummary::new(program),
    };

    // Ok(None): stopped at the cap.
    let finished = tokio::time::timeout(timeout, async {
        if pipes.pump(cap, &mut run).await {
            Ok(None)
        } else {
            child.wait().await.map(Some)
        }
    })
    .await;
    match finished {
        Ok(Ok(Some(status))) => run.code = status.code(),
        Ok(Ok(None)) => {
            run.capped = true;
            let _ = child.kill().await;
        }
        Ok(Err(e)) => {
            return Err(RuntimeError::Tool(format!("Failed to wait for {program}: {e}")));
        }
        Err(_) => {
            run.timed_out = true;
            let _ = child.kill().await;
            // Keep what was already in the pipes when the child died.
            let drain = pipes.pump(cap, &mut run);
            let _ = tokio::time::timeout(Duration::from_millis(500), drain).await;
            run.capped = run.stdout.len() > cap;
        }
    }
    run.stderr.finish();
    Ok(run)
}

struct Pipes {
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
}

impl Pipes {
    fn new(child: &mut Child) -> Self {
        Self {
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
        }
    }

    /// Read both pipes until both are closed (false) or stdout passes `cap`
    /// (true). Cancel-safe: a dropped call loses no bytes already read.
    async fn pump(&mut self, cap: usize, run: &mut BoundedRun) -> bool {
        let mut out = vec![0u8; 16 * 1024];
        let mut err = [0u8; 4096];
        while self.stdout.is_some() || self.stderr.is_some() {
            let out_open = self.stdout.is_some();
            let err_open = self.stderr.is_some();
            tokio::select! {
                r = read_some(self.stdout.as_mut(), &mut out), if out_open => match r {
                    Some(n) => {
                        run.stdout.extend_from_slice(&out[..n]);
                        if run.stdout.len() > cap {
                            return true;
                        }
                    }
                    None => self.stdout = None,
                },
                r = read_some(self.stderr.as_mut(), &mut err), if err_open => match r {
                    Some(n) => run.stderr.push(&err[..n]),
                    None => self.stderr = None,
                },
            }
        }
        false
    }
}

/// One read: `Some(n)` with n > 0, or `None` at end of file or an error.
async fn read_some<R>(pipe: Option<&mut R>, buf: &mut [u8]) -> Option<usize>
where
    R: tokio::io::AsyncRead + Unpin,
{
    match pipe?.read(buf).await {
        Ok(0) | Err(_) => None,
        Ok(n) => Some(n),
    }
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

    #[test]
    fn the_windows_find_is_skipped_for_git_usr_bin() {
        let tmp = tempfile::tempdir().unwrap();
        let windows = tmp.path().join("Windows");
        let system32 = windows.join("System32");
        touch(&system32.join("find.exe"));
        let git = tmp.path().join("Git");
        touch(&git.join(r"usr").join("bin").join("find.exe"));

        let path = path_of(&[&system32]);
        let found = resolve_windows("find", Some(&path), Some(&windows), std::slice::from_ref(&git));
        assert_eq!(found, Some(git.join("usr").join("bin").join("find.exe")));
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
        touch(&system32.join("find.exe"));
        let path = path_of(&[&system32]);
        assert_eq!(resolve_windows("find", Some(&path), Some(&windows), &[]), None);
        assert_eq!(resolve_windows("ls", None, None, &[tmp.path().join("NoGit")]), None);

        let msg = missing_message("find");
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

    #[test]
    fn stderr_lines_are_sorted_into_binary_warnings_and_errors() {
        let mut s = StderrSummary::new("grep");
        // Split mid-line, CRLF, and a Windows argv[0] prefix.
        s.push(b"grep: /p/a.bin: binary file matches\ngrep: warn");
        s.push(b"ing: stray \\ before d\r\n");
        s.push(b"C:\\Git\\usr\\bin\\grep.exe: C:\\p/x: Permission denied\n");
        s.push(b"grep: warning: stray \\ before d\n/usr/bin/grep: /p/y: Permission denied");
        s.finish();
        assert_eq!(s.binary, ["/p/a.bin"]);
        assert_eq!(s.binary_count, 1);
        assert_eq!(s.warnings, ["stray \\ before d"]);
        assert_eq!(s.errors, 2);
        assert_eq!(s.first_error.as_deref(), Some("C:\\p/x: Permission denied"));
        assert_eq!(
            s.failure(Some(2)),
            "grep: C:\\p/x: Permission denied (and 1 more error)"
        );
    }

    #[test]
    fn a_long_stderr_line_is_bounded() {
        let mut s = StderrSummary::new("find");
        s.push(&[b'x'; STDERR_LINE_MAX * 3]);
        s.push(b"\nfind: 'b': Permission denied\n");
        s.finish();
        assert_eq!(s.errors, 2);
        assert_eq!(
            s.first_error.as_ref().map(String::len),
            Some(STDERR_LINE_MAX)
        );
    }

    #[test]
    fn many_binary_matches_are_listed_up_to_a_limit() {
        let mut s = StderrSummary::new("grep");
        for i in 0..8 {
            s.add_binary(&format!("/p/{i}.bin"));
        }
        assert_eq!(
            s.notes(),
            ["[8 binary files match, lines not shown: /p/0.bin, /p/1.bin, /p/2.bin, /p/3.bin, /p/4.bin and 3 more]"]
        );
    }

    #[test]
    fn a_failure_without_stderr_names_the_exit() {
        let s = StderrSummary::new("find");
        assert_eq!(s.failure(Some(1)), "find failed with exit status 1");
        assert_eq!(s.failure(None), "find was terminated by a signal");
    }

    #[cfg(unix)]
    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        cmd
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_bounded_keeps_the_exit_code_and_stderr() {
        let run = run_bounded(
            sh("echo out; echo 'grep: /p/x: Permission denied' >&2; exit 2"),
            "grep",
            1024,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(run.code, Some(2));
        assert_eq!(run.stdout, b"out\n");
        assert!(!run.capped && !run.timed_out);
        assert_eq!(
            run.stderr.first_error.as_deref(),
            Some("/p/x: Permission denied")
        );
    }

    /// The child is killed at the cap: `yes` never ends on its own.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_bounded_stops_the_child_at_the_cap() {
        let run = run_bounded(sh("exec yes alpha"), "grep", 1000, Duration::from_secs(20))
            .await
            .unwrap();
        assert!(
            run.capped && !run.timed_out,
            "stopped by the deadline, not the cap"
        );
        assert!(run.stdout.len() > 1000 && run.stdout.len() <= 1000 + 16 * 1024);
        assert_eq!(run.code, None);
        let (text, cut) = run.text(1000);
        assert!(
            cut && text.len() <= 1000 && text.ends_with("alpha"),
            "{text}"
        );
    }

    /// At the deadline the child is killed and what it wrote is kept.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_bounded_keeps_output_written_before_the_deadline() {
        let started = std::time::Instant::now();
        let run = run_bounded(
            sh("echo partial; echo 'grep: /p/x: Permission denied' >&2; exec sleep 30"),
            "grep",
            1024,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "child not killed"
        );
        assert!(run.timed_out && !run.capped);
        assert_eq!(run.code, None);
        assert_eq!(run.stdout, b"partial\n");
        assert_eq!(run.stderr.errors, 1);
    }

    /// Dropping the caller's future (a cancelled tool call) kills the child.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_dropped_run_kills_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let script = format!("echo $$ > '{}'; exec sleep 30", pid_file.display());
        let run = run_bounded(sh(&script), "grep", 1024, Duration::from_secs(30));
        let _ = tokio::time::timeout(Duration::from_secs(2), run).await;
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // Reaped or a zombie: either way no longer running.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let state = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            let state = String::from_utf8_lossy(&state.stdout).trim().to_string();
            if state.is_empty() || state.starts_with('Z') {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child {pid} still running: {state}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_command_is_the_plain_program_name() {
        let ctx = super::super::test_helpers::create_tool_context();
        let cmd = unix_tool_command("find", &ctx).unwrap();
        assert_eq!(cmd.as_std().get_program(), OsStr::new("find"));
    }
}
