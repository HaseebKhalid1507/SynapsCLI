//! The `grep` tool: `grep -rnE` over a file or directory.
//!
//! Patterns are POSIX extended regexes (ERE, `grep -E`), so `a|b`, `+`, `?`,
//! `{n}` and `( )` work as written. `\|` is not alternation in ERE: GNU grep
//! reads it as a literal `|` (BSD grep may differ). `\d` and `(?i)` are not
//! ERE either; grep's warning about them is passed on as a note.
//!
//! The exit status decides the result: 0 is matches, 1 is "No matches
//! found.", 2 is an error. An error with no matches is a tool error (stderr's
//! first line, with a hint when the pattern is invalid); matches that come
//! with errors (an unreadable file) keep the matches and add a note.

use super::unix_programs::{run_bounded, truncation_footer, BoundedRun, StderrSummary};
use super::{resolve_path_in, Tool, ToolContext};
use crate::{Result, RuntimeError};
use serde_json::{json, Value};
use std::time::Duration;

/// How long one search may run.
const TIMEOUT: Duration = Duration::from_secs(15);

pub struct GrepTool;

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn effect(&self) -> crate::tools::catalog::ToolEffect {
        crate::tools::catalog::ToolEffect::ReadOnly
    }

    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }

    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents using regex patterns. Returns matching lines with file paths and line numbers. Supports file type filtering and context lines."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search in (default: current directory)"
                },
                "include": {
                    "type": "string",
                    "description": "Glob pattern to filter files (e.g. \"*.rs\", \"*.py\")"
                },
                "context": {
                    "type": "integer",
                    "description": "Number of context lines to show before and after each match"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        search(&params, &ctx, TIMEOUT).await
    }
}

async fn search(params: &Value, ctx: &ToolContext, timeout: Duration) -> Result<String> {
    let pattern = params["pattern"]
        .as_str()
        .ok_or_else(|| RuntimeError::Tool("Missing pattern parameter".to_string()))?;
    let path = resolve_path_in(
        params["path"].as_str().unwrap_or("."),
        ctx.capabilities.cwd.as_deref(),
    );
    let include = params["include"].as_str();
    let context = params["context"].as_u64();

    let mut cmd = super::unix_programs::unix_tool_command("grep", ctx)?;
    cmd.arg("-rnE");
    cmd.arg("--color=never");

    if let Some(glob) = include {
        cmd.arg("--include").arg(glob);
    }

    if let Some(lines) = context {
        cmd.arg(format!("-C{lines}"));
    }

    cmd.arg("--exclude-dir=.git");
    cmd.arg("--exclude-dir=node_modules");
    cmd.arg("--exclude-dir=target");

    cmd.arg("--").arg(pattern).arg(&path);

    let cap = ctx.limits.max_tool_buffer;
    let run = run_bounded(cmd, "grep", cap, timeout).await?;
    render(run, cap, timeout)
}

/// The tool result for a finished (or stopped) grep run.
fn render(mut run: BoundedRun, cap: usize, timeout: Duration) -> Result<String> {
    let (text, cut) = run.text(cap);
    // BSD grep, and GNU grep before 3.5, report binary matches on stdout.
    let mut lines = Vec::new();
    for line in text.split('\n') {
        match line
            .strip_prefix("Binary file ")
            .and_then(|l| l.strip_suffix(" matches"))
        {
            Some(file) => run.stderr.add_binary(file),
            None => lines.push(line),
        }
    }
    let body = lines.join("\n");
    let secs = timeout.as_secs();

    if body.is_empty() && run.stderr.binary_count == 0 {
        if run.timed_out {
            return Err(RuntimeError::Tool(format!(
                "Grep timed out after {secs}s before finding a match; partial results may be \
                 missing. Narrow the path or add an include filter."
            )));
        }
        let code = run.code;
        return match code {
            Some(0 | 1) => Ok(with_notes("No matches found.".to_string(), &run.stderr)),
            _ => Err(RuntimeError::Tool(failure(&run.stderr, code))),
        };
    }

    let mut out = body;
    if cut {
        out.push_str("\n\n");
        out.push_str(&truncation_footer(cap));
    }
    if run.timed_out {
        out.push_str(&format!(
            "\n\n... (grep timed out after {secs}s; results are partial, more matches may be missing)"
        ));
    }
    Ok(with_notes(out, &run.stderr))
}

/// `text` (which may be empty: only binary files matched) and the notes,
/// a blank line apart.
fn with_notes(text: String, stderr: &StderrSummary) -> String {
    let mut parts = vec![text];
    parts.extend(stderr.notes());
    parts.retain(|p| !p.is_empty());
    parts.join("\n\n")
}

/// The tool error for a run that failed without matches.
fn failure(stderr: &StderrSummary, code: Option<i32>) -> String {
    match &stderr.first_error {
        Some(first) if is_regex_error(first) => format!(
            "invalid regex: {first} — the pattern is a POSIX extended regex (grep -E): escape \
             regex metacharacters such as ( [ {{ with a backslash to match them literally"
        ),
        _ => stderr.failure(code),
    }
}

/// grep's complaint about the pattern rather than a file. GNU says e.g.
/// "Unmatched ( or \(", "Invalid range end", "Trailing backslash"; BSD
/// "parentheses not balanced", "repetition-operator operand invalid". File
/// errors read `path: reason`.
fn is_regex_error(msg: &str) -> bool {
    const KEYS: [&str; 10] = [
        "unmatched",
        "invalid",
        "not balanced",
        "trailing backslash",
        "regular expression",
        "repetition",
        "empty (sub)expression",
        "character class",
        "back reference",
        "backreference",
    ];
    let lower = msg.to_ascii_lowercase();
    !msg.contains(": ") && KEYS.iter().any(|k| lower.contains(k))
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::create_tool_context;
    use super::*;
    use crate::tools::Tool;
    use serde_json::json;

    #[test]
    fn test_grep_tool_schema() {
        let tool = GrepTool;
        assert_eq!(tool.name(), "grep");
        assert!(!tool.description().is_empty());

        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert!(params["required"].is_array());
    }

    #[tokio::test]
    async fn test_grep_tool_execution() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("test_grep_tool_execution.txt");

        // Write test content
        let content = "hello world\nfoo bar\nhello again";
        std::fs::write(&test_file, content).unwrap();

        let tool = GrepTool;
        let ctx = create_tool_context();

        let params = json!({
            "pattern": "hello",
            "path": test_file.to_string_lossy()
        });

        let result = tool.execute(params, ctx).await.unwrap();

        // Should contain both matching lines with line numbers
        assert!(result.contains("hello world"));
        assert!(result.contains("hello again"));
        assert!(result.contains("1:") || result.contains("hello world"));
        assert!(result.contains("3:") || result.contains("hello again"));

        // Cleanup
        let _ = std::fs::remove_file(&test_file);
    }

    // ── Rendering (no grep binary needed) ───────────────────────────────

    /// A tool error's own message (Display adds "Tool execution failed: ").
    fn tool_msg(e: RuntimeError) -> String {
        match e {
            RuntimeError::Tool(msg) => msg,
            other => other.to_string(),
        }
    }

    fn run(stdout: &str, code: Option<i32>, stderr: &str) -> BoundedRun {
        let mut summary = StderrSummary::new("grep");
        summary.push(stderr.as_bytes());
        summary.finish();
        BoundedRun {
            stdout: stdout.as_bytes().to_vec(),
            capped: false,
            timed_out: false,
            code,
            stderr: summary,
        }
    }

    const CAP: usize = 256 * 1024;

    /// Exit 1 with nothing on stderr is the one "No matches found.".
    #[test]
    fn exit_1_is_no_matches() {
        assert_eq!(
            render(run("", Some(1), ""), CAP, TIMEOUT).unwrap(),
            "No matches found."
        );
    }

    /// Exit 2 with no output is a tool error carrying grep's message, never
    /// "No matches found.".
    #[test]
    fn exit_2_without_output_is_an_error() {
        let err = render(
            run("", Some(2), "grep: /p/nope: No such file or directory\n"),
            CAP,
            TIMEOUT,
        )
        .map_err(tool_msg)
        .unwrap_err();
        assert!(
            err.contains("grep: /p/nope: No such file or directory"),
            "{err}"
        );
        assert!(!err.contains("No matches"), "{err}");
        assert!(!err.contains("invalid regex"), "{err}");
    }

    /// GNU and BSD wordings of a bad pattern get the escaping hint; file
    /// errors don't.
    #[test]
    fn regex_errors_are_recognised() {
        for msg in [
            "Unmatched ( or \\(",
            "Unmatched [, [^, [:, [., or [=",
            "Invalid range end",
            "Trailing backslash",
            "parentheses not balanced",
            "repetition-operator operand invalid",
            "empty (sub)expression",
        ] {
            assert!(is_regex_error(msg), "{msg}");
            let err = render(run("", Some(2), &format!("grep: {msg}\n")), CAP, TIMEOUT)
                .map_err(tool_msg)
                .unwrap_err();
            assert!(err.starts_with(&format!("invalid regex: {msg}")), "{err}");
            assert!(err.contains("escape regex metacharacters"), "{err}");
        }
        assert!(!is_regex_error("/p/x: Invalid argument"));
        assert!(!is_regex_error("/p/x: Permission denied"));
    }

    /// Matches that come with errors keep the matches and add one note with
    /// the error count and the first error.
    #[test]
    fn matches_with_errors_keep_the_matches_and_add_a_note() {
        let stderr = "grep: /p/a: Permission denied\ngrep: /p/b: Permission denied\n";
        let out = render(run("/p/x:1:alpha\n", Some(2), stderr), CAP, TIMEOUT).unwrap();
        assert!(out.starts_with("/p/x:1:alpha\n\n"), "{out}");
        assert!(
            out.contains("[2 errors while searching, results may be incomplete. First: grep: /p/a: Permission denied]"),
            "{out}"
        );
    }

    /// Both binary-match wordings: GNU 3.5+ on stderr, BSD and older GNU on
    /// stdout. Binary-only matches are a result, not "No matches found.".
    #[test]
    fn binary_matches_from_stdout_or_stderr_are_a_note() {
        let out = render(
            run("", Some(0), "grep: /p/a.bin: binary file matches\n"),
            CAP,
            TIMEOUT,
        )
        .unwrap();
        assert_eq!(out, "[binary file matches, lines not shown: /p/a.bin]");

        let stdout = "/p/x:1:alpha\nBinary file /p/b.bin matches\n";
        let out = render(
            run(stdout, Some(0), "grep: /p/a.bin: binary file matches\n"),
            CAP,
            TIMEOUT,
        )
        .unwrap();
        assert_eq!(
            out,
            "/p/x:1:alpha\n\n[2 binary files match, lines not shown: /p/a.bin, /p/b.bin]"
        );
    }

    /// A warning (e.g. GNU's "stray \ before d" for `\d`) is passed on, also
    /// next to "No matches found.".
    #[test]
    fn warnings_are_passed_on() {
        let out = render(
            run("", Some(1), "grep: warning: ? at start of expression\n"),
            CAP,
            TIMEOUT,
        )
        .unwrap();
        assert_eq!(
            out,
            "No matches found.\n\n[grep warning: ? at start of expression]"
        );
    }

    #[test]
    fn a_timeout_keeps_partial_output_with_a_note() {
        // grep was killed mid-line: the partial line is dropped.
        let mut partial = run("/p/x:1:alpha\n/p/x:2:alp", None, "");
        partial.timed_out = true;
        let out = render(partial, CAP, TIMEOUT).unwrap();
        assert_eq!(
            out,
            "/p/x:1:alpha\n\n... (grep timed out after 15s; results are partial, more matches may be missing)"
        );

        let mut nothing = run("", None, "");
        nothing.timed_out = true;
        let err = render(nothing, CAP, TIMEOUT).map_err(tool_msg).unwrap_err();
        assert!(
            err.contains("Grep timed out after 15s")
                && err.contains("partial results may be missing"),
            "{err}"
        );
    }

    #[test]
    fn capped_output_ends_on_a_whole_line_with_the_footer() {
        let mut capped = run("/p/x:1:aaaa\n/p/x:2:bbbb\n/p/x:3:cc", None, "");
        capped.capped = true;
        let out = render(capped, 26, TIMEOUT).unwrap();
        assert_eq!(
            out,
            "/p/x:1:aaaa\n/p/x:2:bbbb\n\n... (output truncated, more than 26 total bytes)"
        );
    }

    // ── Real grep (scope §8 rows 1–8, 13, 17) ───────────────────────────

    #[cfg(unix)]
    fn dir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            std::fs::write(dir.path().join(name), body).unwrap();
        }
        dir
    }

    /// Search `dir` (holding just `t.txt`), so every line reads
    /// `path:N:text` with GNU and BSD grep alike.
    #[cfg(unix)]
    async fn grep_in(dir: &std::path::Path, pattern: &str) -> Result<String> {
        let ctx = create_tool_context();
        search(&json!({"pattern": pattern, "path": dir}), &ctx, TIMEOUT).await
    }

    #[cfg(unix)]
    const TEXT: &str = "alpha\nbeta\ngamma\nalpha|beta\nfoo(bar\nfoo{x\ncolor\ncolour\naaa\nxx\n123\naa\nword here\nfn main() {\n";

    /// The matched text of each `path:N:text` line.
    #[cfg(unix)]
    fn lines_of(out: &str) -> Vec<&str> {
        out.lines()
            .filter_map(|l| l.splitn(3, ':').nth(2))
            .collect()
    }

    /// Row 1, the reproduction: `alpha|beta` was BRE, so it matched nothing.
    #[cfg(unix)]
    #[tokio::test]
    async fn alternation_matches_both_lines() {
        let dir = dir_with(&[("t.txt", TEXT)]);
        let out = grep_in(dir.path(), "alpha|beta").await.unwrap();
        assert_eq!(lines_of(&out), ["alpha", "beta", "alpha|beta"], "{out}");
    }

    /// `+`, `?`, `{2}`, groups and POSIX classes (row 6's ERE half).
    #[cfg(unix)]
    #[tokio::test]
    async fn ere_quantifiers_and_groups_work() {
        let dir = dir_with(&[("t.txt", TEXT)]);
        for (pattern, want) in [
            ("^a+$", &["aaa", "aa"][..]),
            ("colou?r", &["color", "colour"][..]),
            ("^x{2}$", &["xx"][..]),
            ("^(al|be)", &["alpha", "beta", "alpha|beta"][..]),
            ("^[[:digit:]]+$", &["123"][..]),
        ] {
            let out = grep_in(dir.path(), pattern).await.unwrap();
            assert_eq!(lines_of(&out), want, "{pattern}: {out}");
        }
    }

    /// Row 3: `\(` is a literal paren (it was an invalid BRE, reported as
    /// "No matches found.").
    #[cfg(unix)]
    #[tokio::test]
    async fn escaped_paren_is_literal() {
        let dir = dir_with(&[("t.txt", TEXT)]);
        let out = grep_in(dir.path(), "foo\\(").await.unwrap();
        assert_eq!(lines_of(&out), ["foo(bar"], "{out}");
    }

    /// An invalid pattern is a tool error with grep's message and a hint.
    #[cfg(unix)]
    #[tokio::test]
    async fn invalid_pattern_is_an_error_not_no_matches() {
        let dir = dir_with(&[("t.txt", TEXT)]);
        let err = grep_in(dir.path(), "foo(")
            .await
            .map_err(tool_msg)
            .unwrap_err();
        assert!(err.starts_with("invalid regex: "), "{err}");
        assert!(err.contains("escape regex metacharacters"), "{err}");
        assert!(!err.contains("No matches"), "{err}");
        if cfg!(not(target_os = "macos")) {
            assert!(err.contains("Unmatched ( or \\("), "{err}");
        }
    }

    /// A genuine miss is exactly "No matches found.".
    #[cfg(unix)]
    #[tokio::test]
    async fn genuine_no_match_says_no_matches_found() {
        let dir = dir_with(&[("t.txt", TEXT)]);
        assert_eq!(
            grep_in(dir.path(), "zebra").await.unwrap(),
            "No matches found."
        );
    }

    /// Row 17, second half: a missing path is an error.
    #[cfg(unix)]
    #[tokio::test]
    async fn missing_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = grep_in(&dir.path().join("nope"), "alpha")
            .await
            .map_err(tool_msg)
            .unwrap_err();
        assert!(err.contains("No such file or directory"), "{err}");
    }

    /// Rows 2, 4, 5, 6, 7 and 8 as GNU grep -E has them (Linux, and Git for
    /// Windows' grep). BSD grep on macOS rejects some of these (`()`,
    /// leading `*`) and may read `\|` differently.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[tokio::test]
    async fn gnu_ere_edge_cases_are_pinned() {
        let dir = dir_with(&[("t.txt", TEXT)]);
        let grep = |p: &'static str| {
            let dir = dir.path().to_path_buf();
            async move { grep_in(&dir, p).await.unwrap() }
        };
        // Row 2: `\|` is a literal pipe in ERE, not alternation.
        assert_eq!(lines_of(&grep("alpha\\|beta").await), ["alpha|beta"]);
        // Row 4: `()` is an empty group, so `fn main() {` misses its own line.
        assert_eq!(grep("fn main() {").await, "No matches found.");
        // Row 5: a leading `*` and an unclosed `{` are not errors.
        assert!(lines_of(&grep("*foo").await).contains(&"foo(bar"));
        assert_eq!(lines_of(&grep("foo{").await), ["foo{x"]);
        // Row 6: `\d` is not a digit class and `(?i)` is not a flag.
        assert!(!lines_of(&grep("\\d+").await).contains(&"123"));
        assert!(grep("(?i)ALPHA").await.starts_with("No matches found."));
        // Row 7: word boundaries and `\w`/`\s`.
        assert_eq!(lines_of(&grep("\\bword\\b").await), ["word here"]);
        assert!(lines_of(&grep("\\w+\\s").await).contains(&"word here"));
        // Row 8: back-references work; lookaround matches nothing.
        assert_eq!(lines_of(&grep("^(a)\\1$").await), ["aa"]);
        assert!(grep("(?=x)").await.starts_with("No matches found."));
    }

    /// Row 13: binary matches are surfaced, alone or next to text matches.
    #[cfg(unix)]
    #[tokio::test]
    async fn binary_file_matches_are_surfaced() {
        let only = dir_with(&[("blob.bin", "abc\0alpha\n")]);
        let ctx = create_tool_context();
        let out = search(
            &json!({"pattern": "alpha", "path": only.path()}),
            &ctx,
            TIMEOUT,
        )
        .await
        .unwrap();
        assert!(
            out.starts_with("[binary file matches, lines not shown: "),
            "{out}"
        );
        assert!(out.contains("blob.bin"), "{out}");

        let mixed = dir_with(&[("blob.bin", "abc\0alpha\n"), ("t.txt", "alpha\n")]);
        let out = search(
            &json!({"pattern": "alpha", "path": mixed.path()}),
            &ctx,
            TIMEOUT,
        )
        .await
        .unwrap();
        assert!(out.contains("t.txt:1:alpha"), "{out}");
        assert!(
            out.contains("[binary file matches, lines not shown: ") && out.contains("blob.bin"),
            "{out}"
        );
        assert!(
            !out.contains("Binary file"),
            "BSD-style line left in the output: {out}"
        );
    }

    /// Row 17: an unreadable file next to matches gives the matches plus a
    /// note. Skipped when permissions don't apply (root).
    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_file_beside_matches_gives_matches_and_a_note() {
        use std::os::unix::fs::PermissionsExt;
        let dir = dir_with(&[("t.txt", "alpha\n"), ("locked.txt", "alpha\n")]);
        let locked = dir.path().join("locked.txt");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&locked).is_ok() {
            eprintln!("skipped: permissions are not enforced for this user");
            return;
        }
        let ctx = create_tool_context();
        let out = search(
            &json!({"pattern": "alpha", "path": dir.path()}),
            &ctx,
            TIMEOUT,
        )
        .await
        .unwrap();
        assert!(out.contains("t.txt:1:alpha"), "{out}");
        assert!(
            out.contains("[1 error while searching, results may be incomplete. First: grep: "),
            "{out}"
        );
        assert!(out.contains("locked.txt: Permission denied]"), "{out}");
    }

    #[cfg(unix)]
    fn mkfifo(path: &std::path::Path) {
        let status = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// Feed `fifo` from a thread: `body` once, then (if `endless`) forever.
    /// The thread reports the write error that ended it, which is a broken
    /// pipe once grep is gone.
    #[cfg(unix)]
    fn feed(
        fifo: &std::path::Path,
        body: String,
        endless: bool,
    ) -> std::sync::mpsc::Receiver<std::io::ErrorKind> {
        use std::io::Write;
        let (tx, rx) = std::sync::mpsc::channel();
        let fifo = fifo.to_path_buf();
        std::thread::spawn(move || {
            let mut f = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();
            let mut result = f.write_all(body.as_bytes());
            while result.is_ok() {
                result = if endless {
                    f.write_all(body.as_bytes())
                } else {
                    // A line that doesn't match, so a dead reader shows.
                    std::thread::sleep(Duration::from_millis(100));
                    f.write_all(b"z\n")
                };
            }
            let _ = tx.send(result.unwrap_err().kind());
        });
        rx
    }

    /// Output past the cap stops grep there: the input never ends, so only
    /// the cap can end the search before the deadline.
    #[cfg(unix)]
    #[tokio::test]
    async fn output_past_the_cap_stops_grep_with_the_footer() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("endless.txt");
        mkfifo(&fifo);
        let writer = feed(&fifo, "alpha\n".repeat(1024), true);
        let mut ctx = create_tool_context();
        ctx.limits.max_tool_buffer = 4096;
        let out = search(
            &json!({"pattern": "alpha", "path": fifo}),
            &ctx,
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        let (body, footer) = out.split_once("\n\n").unwrap();
        assert!(body.len() <= 4096 && body.ends_with(":alpha"), "{body}");
        assert!(
            body.len() > 3000,
            "stopped well short of the cap: {}",
            body.len()
        );
        assert_eq!(footer, "... (output truncated, more than 4096 total bytes)");
        assert_eq!(
            writer.recv_timeout(Duration::from_secs(10)).unwrap(),
            std::io::ErrorKind::BrokenPipe,
            "grep still running after the cap"
        );
    }

    /// At the deadline the matches found so far are kept and grep is killed.
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_keeps_partial_output_and_kills_grep() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("slow.txt");
        mkfifo(&fifo);
        // Enough to get past grep's own output buffer; the rest trickles.
        let writer = feed(&fifo, "alpha\n".repeat(4096), false);
        let ctx = create_tool_context();
        let out = search(
            &json!({"pattern": "alpha", "path": fifo}),
            &ctx,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let first = out.lines().next().unwrap();
        assert!(first == "1:alpha" || first.ends_with(":1:alpha"), "{first}");
        assert!(out.lines().filter(|l| l.ends_with(":alpha")).count() > 100);
        assert!(
            out.ends_with(
                "... (grep timed out after 2s; results are partial, more matches may be missing)"
            ),
            "{}",
            &out[out.len().saturating_sub(200)..]
        );
        assert_eq!(
            writer.recv_timeout(Duration::from_secs(10)).unwrap(),
            std::io::ErrorKind::BrokenPipe,
            "grep still running after the timeout"
        );

        // Nothing found before the deadline: an error that says so.
        let silent = dir.path().join("silent.txt");
        mkfifo(&silent);
        let err = search(
            &json!({"pattern": "alpha", "path": silent}),
            &ctx,
            Duration::from_secs(1),
        )
        .await
        .map_err(tool_msg)
        .unwrap_err();
        assert!(
            err.starts_with("Grep timed out after 1s before finding a match"),
            "{err}"
        );
    }
}
