//! The `grep` tool: ripgrep's engine in-process (`tools::search`), with the
//! output `grep -rn` gave: `path:line:text` for matches, `path-line-text` for
//! context lines, `--` between groups when `context` is set, and no path
//! prefix when `path` is a file.

use super::search::{self, content, pattern};
use super::{resolve_path_in, Tool, ToolContext};
use crate::{Result, RuntimeError};
use serde_json::{json, Value};
use std::time::Duration;

/// How long one search may run before it returns what it has.
const TIMEOUT: Duration = Duration::from_secs(15);

/// Most context lines either side of a match.
const MAX_CONTEXT: usize = 1000;

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
        "Search file contents with a regex (Rust regex syntax, as in ripgrep). Returns matching lines with file paths and line numbers. Respects .gitignore (use no_ignore to include ignored files); hidden files are searched, binary files skipped."
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
                },
                "no_ignore": {
                    "type": "boolean",
                    "description": "Also search files skipped by .gitignore/.ignore, node_modules and target (default false)"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let request = request(&params, &ctx)?;
        run_search(request, ctx.capabilities.launch_cancel.clone(), TIMEOUT).await
    }
}

/// The call's parameters, resolved against the session.
fn request(params: &Value, ctx: &ToolContext) -> Result<content::ContentRequest> {
    let regex = params["pattern"]
        .as_str()
        .ok_or_else(|| RuntimeError::Tool("Missing pattern parameter".to_string()))?;
    let root = resolve_path_in(
        params["path"].as_str().unwrap_or("."),
        ctx.capabilities.cwd.as_deref(),
    );
    let matcher = pattern::content_matcher(regex).map_err(RuntimeError::Tool)?;
    let include = match params["include"].as_str().map(str::trim) {
        Some(glob) if !glob.is_empty() => {
            Some(pattern::include_filter(glob).map_err(RuntimeError::Tool)?)
        }
        _ => None,
    };
    let context = params["context"]
        .as_u64()
        .map(|n| usize::try_from(n).unwrap_or(usize::MAX).min(MAX_CONTEXT));
    Ok(content::ContentRequest {
        root,
        matcher,
        include,
        context,
        no_ignore: params["no_ignore"].as_bool().unwrap_or(false),
        cwd: ctx.capabilities.cwd.clone(),
        cap: ctx.limits.max_tool_buffer,
    })
}

/// Runs the search on the blocking pool and renders the result. `turn`
/// cancelled (or this future dropped) stops it with partial results.
async fn run_search(
    request: content::ContentRequest,
    turn: Option<crate::CancellationToken>,
    timeout: Duration,
) -> Result<String> {
    let cap = request.cap;
    let outcome = search::run_blocking(timeout, turn, move |control| {
        content::search(&request, control, search::open_regular)
    })
    .await?
    .map_err(RuntimeError::Tool)?;
    content::render(outcome, cap, timeout)
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::create_tool_context;
    use super::*;
    use crate::tools::Tool;
    use serde_json::json;
    use std::path::Path;

    fn write(root: &Path, rel: &str, body: &str) {
        let path = rel.split('/').fold(root.to_path_buf(), |p, c| p.join(c));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn test_grep_tool_schema() {
        let tool = GrepTool;
        assert_eq!(tool.name(), "grep");
        assert!(tool.description().contains("no_ignore"));
        assert!(tool.description().contains(".gitignore"));

        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert_eq!(params["required"], json!(["pattern"]));
        for (name, ty) in [
            ("pattern", "string"),
            ("path", "string"),
            ("include", "string"),
            ("context", "integer"),
            ("no_ignore", "boolean"),
        ] {
            assert_eq!(params["properties"][name]["type"], ty, "{name}");
        }
    }

    /// A file as the path: `N:text` lines, exactly as `grep -rn` printed.
    #[tokio::test]
    async fn test_grep_tool_execution() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello world\nfoo bar\nhello again").unwrap();

        let params = json!({ "pattern": "hello", "path": file.to_string_lossy() });
        let result = GrepTool
            .execute(params, create_tool_context())
            .await
            .unwrap();
        assert_eq!(result, "1:hello world\n3:hello again\n");
    }

    /// The default path is the session cwd joined with `.`, and paths keep
    /// that form (`<cwd>/./src/a.txt`), as `grep -rn` printed them.
    #[tokio::test]
    async fn default_path_is_the_session_cwd() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/a.txt", "x\nneedle\n");
        let mut ctx = create_tool_context();
        ctx.capabilities.cwd = Some(dir.path().to_path_buf());

        let result = GrepTool
            .execute(json!({ "pattern": "needle" }), ctx)
            .await
            .unwrap();
        let shown = dir.path().join(".").join("src").join("a.txt");
        assert_eq!(result, format!("{}:2:needle\n", shown.display()));
    }

    #[tokio::test]
    async fn invalid_regex_is_a_tool_error() {
        let dir = tempfile::tempdir().unwrap();
        let params = json!({ "pattern": "fn main() {", "path": dir.path().to_string_lossy() });
        let err = GrepTool
            .execute(params, create_tool_context())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid regex"), "{err}");
        assert!(err.contains("Hint:"), "{err}");
    }

    /// A missing path is an error, never "No matches found.".
    #[tokio::test]
    async fn missing_path_is_a_tool_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let params = json!({ "pattern": "x", "path": missing.to_string_lossy() });
        let err = GrepTool
            .execute(params, create_tool_context())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Path not found"), "{err}");
        assert!(!err.contains("No matches"), "{err}");
    }

    #[tokio::test]
    async fn no_ignore_and_include_reach_the_engine() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "node_modules/m.rs", "hit\n");
        write(root, "src/a.rs", "hit\n");
        write(root, "src/b.txt", "hit\n");
        let path = root.to_string_lossy();

        let params = json!({ "pattern": "hit", "path": path, "include": "*.rs" });
        let result = GrepTool
            .execute(params, create_tool_context())
            .await
            .unwrap();
        let a = root.join("src").join("a.rs").display().to_string();
        assert_eq!(result, format!("{a}:1:hit\n"));

        let params =
            json!({ "pattern": "hit", "path": path, "include": "*.rs", "no_ignore": true });
        let result = GrepTool
            .execute(params, create_tool_context())
            .await
            .unwrap();
        let m = root.join("node_modules").join("m.rs").display().to_string();
        assert_eq!(result, format!("{m}:1:hit\n{a}:1:hit\n"));
    }

    #[tokio::test]
    async fn output_is_capped_at_max_tool_buffer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hit\n".repeat(1000)).unwrap();
        let mut ctx = create_tool_context();
        ctx.limits.max_tool_buffer = 100;
        let params =
            json!({ "pattern": "hit", "path": dir.path().join("a.txt").to_string_lossy() });
        let result = GrepTool.execute(params, ctx).await.unwrap();
        let (body, footer) = result.split_once("\n\n").unwrap();
        assert!(body.len() < 100 && body.ends_with(":hit"), "{body}");
        assert!(
            body.len() > 80,
            "stopped well short of the cap: {}",
            body.len()
        );
        assert_eq!(footer, "... (output truncated, more than 100 total bytes)");
    }

    /// The cut falls on a whole line, then the footer.
    #[tokio::test]
    async fn capped_output_ends_on_a_whole_line_with_the_footer() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "aaaa\nbbbb\ncccc\n").unwrap();
        let mut ctx = create_tool_context();
        ctx.limits.max_tool_buffer = 16;
        let params = json!({ "pattern": "[a-c]", "path": file.to_string_lossy() });
        let result = GrepTool.execute(params, ctx).await.unwrap();
        assert_eq!(
            result,
            "1:aaaa\n2:bbbb\n\n... (output truncated, more than 16 total bytes)"
        );
    }

    /// A cancelled turn stops the search: nothing found yet is an error.
    #[tokio::test]
    async fn turn_cancellation_reaches_the_search() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.txt", "hit\n");
        let turn = crate::CancellationToken::new();
        turn.cancel();
        let mut ctx = create_tool_context();
        ctx.capabilities.launch_cancel = Some(turn);
        let params = json!({ "pattern": "hit", "path": dir.path().to_string_lossy() });
        let err = GrepTool.execute(params, ctx).await.unwrap_err().to_string();
        assert!(err.contains("Grep was cancelled"), "{err}");
    }

    /// The deadline reaches the blocking search: with no time at all the
    /// call is a timeout error, not "No matches found.".
    #[tokio::test]
    async fn deadline_reaches_the_search() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.txt", "hit\n");
        let params = json!({ "pattern": "hit", "path": dir.path().to_string_lossy() });
        let request = request(&params, &create_tool_context()).unwrap();
        let err = run_search(request, None, Duration::ZERO)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out"), "{err}");
    }

    // ── Rendering (scope §8: what a result says) ─────────────────────────

    /// A tool error's own message (Display adds "Tool execution failed: ").
    fn tool_msg(e: RuntimeError) -> String {
        match e {
            RuntimeError::Tool(msg) => msg,
            other => other.to_string(),
        }
    }

    const CAP: usize = 256 * 1024;

    /// What one search found: `text`, with every file read.
    fn outcome(text: &str) -> content::ContentOutcome {
        content::ContentOutcome {
            text: text.to_string(),
            searched: 1,
            ..Default::default()
        }
    }

    /// Nothing found and nothing failed is exactly "No matches found.".
    #[test]
    fn nothing_found_is_no_matches() {
        assert_eq!(
            content::render(outcome(""), CAP, TIMEOUT).unwrap(),
            "No matches found."
        );
    }

    /// Errors with nothing read are a tool error carrying the first error,
    /// never "No matches found.".
    #[test]
    fn errors_without_output_are_an_error() {
        let mut failed = outcome("");
        failed.searched = 0;
        failed.errors.add("/p/nope: No such file or directory");
        let err = content::render(failed, CAP, TIMEOUT)
            .map_err(tool_msg)
            .unwrap_err();
        assert_eq!(err, "/p/nope: No such file or directory");
        assert!(!err.contains("No matches"), "{err}");
        assert!(!err.contains("invalid regex"), "{err}");
    }

    /// Matches that come with errors keep the matches and add one note with
    /// the error count and the first error.
    #[test]
    fn matches_with_errors_keep_the_matches_and_add_a_note() {
        let mut out = outcome("/p/x:1:alpha\n");
        out.errors.add("/p/a: Permission denied");
        out.errors.add("/p/b: Permission denied");
        assert_eq!(
            content::render(out, CAP, TIMEOUT).unwrap(),
            "/p/x:1:alpha\n\n[2 errors while searching, results may be incomplete. First: /p/a: Permission denied]"
        );
    }

    #[test]
    fn a_timeout_keeps_partial_output_with_a_note() {
        let mut partial = outcome("/p/x:1:alpha\n");
        partial.stop = Some(search::Stop::TimedOut);
        assert_eq!(
            content::render(partial, CAP, TIMEOUT).unwrap(),
            "/p/x:1:alpha\n\n... (grep timed out after 15s; results are partial, more may be missing)"
        );

        let mut nothing = outcome("");
        nothing.stop = Some(search::Stop::TimedOut);
        let err = content::render(nothing, CAP, TIMEOUT)
            .map_err(tool_msg)
            .unwrap_err();
        assert!(
            err.starts_with("Grep timed out after 15s before finding a match"),
            "{err}"
        );
    }

    // ── The dialect end to end (scope §8 rows 1–8, 13, 17) ──────────────

    const TEXT: &str = "alpha\nbeta\ngamma\nalpha|beta\nfoo(bar\nfoo{x\ncolor\ncolour\naaa\nxx\n123\naa\nword here\nfn main() {\n";

    /// A dir holding `t.txt` with [`TEXT`].
    fn text_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.txt", TEXT);
        dir
    }

    async fn grep_in(path: &Path, pattern: &str) -> Result<String> {
        let params = json!({ "pattern": pattern, "path": path });
        GrepTool.execute(params, create_tool_context()).await
    }

    /// The matched text of each `N:text` line (a file root has no path).
    fn lines_of(out: &str) -> Vec<&str> {
        out.lines()
            .filter_map(|l| l.split_once(':').map(|(_, text)| text))
            .collect()
    }

    /// Row 1, the reproduction: `alpha|beta` was BRE, so it matched nothing.
    #[tokio::test]
    async fn alternation_matches_both_lines() {
        let dir = text_dir();
        let out = grep_in(&dir.path().join("t.txt"), "alpha|beta")
            .await
            .unwrap();
        assert_eq!(lines_of(&out), ["alpha", "beta", "alpha|beta"], "{out}");
    }

    /// `+`, `?`, `{2}`, groups and POSIX classes.
    #[tokio::test]
    async fn quantifiers_groups_and_classes_work() {
        let dir = text_dir();
        let file = dir.path().join("t.txt");
        for (pattern, want) in [
            ("^a+$", &["aaa", "aa"][..]),
            ("colou?r", &["color", "colour"][..]),
            ("^x{2}$", &["xx"][..]),
            ("^(al|be)", &["alpha", "beta", "alpha|beta"][..]),
            ("^[[:digit:]]+$", &["123"][..]),
        ] {
            let out = grep_in(&file, pattern).await.unwrap();
            assert_eq!(lines_of(&out), want, "{pattern}: {out}");
        }
    }

    /// Row 3: `\(` is a literal paren (it was an invalid BRE, reported as
    /// "No matches found.").
    #[tokio::test]
    async fn escaped_paren_is_literal() {
        let dir = text_dir();
        let out = grep_in(&dir.path().join("t.txt"), "foo\\(").await.unwrap();
        assert_eq!(lines_of(&out), ["foo(bar"], "{out}");
    }

    /// An invalid pattern is a tool error with the parser's message and a
    /// hint.
    #[tokio::test]
    async fn invalid_pattern_is_an_error_not_no_matches() {
        let dir = text_dir();
        let err = grep_in(&dir.path().join("t.txt"), "foo(")
            .await
            .map_err(tool_msg)
            .unwrap_err();
        assert!(err.starts_with("invalid regex: "), "{err}");
        assert!(err.contains("unclosed group"), "{err}");
        assert!(err.contains("Hint: the pattern is a Rust regex"), "{err}");
        assert!(!err.contains("No matches"), "{err}");
    }

    /// A genuine miss under a directory is "No matches found." plus the note
    /// that ignored files weren't searched.
    #[tokio::test]
    async fn genuine_no_match_says_no_matches_found() {
        let dir = text_dir();
        assert_eq!(
            grep_in(dir.path(), "zebra").await.unwrap(),
            format!("No matches found.\n\n{}", search::IGNORED_NOTE)
        );
    }

    /// A file the ignore rules exclude (a `.env`, typically) is missed, the
    /// miss says how to include it, and `no_ignore` finds it.
    #[tokio::test]
    async fn an_ignored_file_is_missed_with_a_note_and_no_ignore_finds_it() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".ignore", ".env\n");
        write(dir.path(), ".env", "API_TOKEN=x\n");
        let missed = grep_in(dir.path(), "API_TOKEN").await.unwrap();
        assert!(missed.starts_with("No matches found."), "{missed}");
        assert!(missed.contains("no_ignore: true"), "{missed}");
        let params = json!({ "pattern": "API_TOKEN", "path": dir.path(), "no_ignore": true });
        let found = GrepTool
            .execute(params, create_tool_context())
            .await
            .unwrap();
        assert!(found.contains("API_TOKEN=x"), "{found}");
    }

    /// No note when nothing was ignored: `no_ignore` set, or one file searched.
    #[tokio::test]
    async fn no_ignored_note_with_no_ignore_or_a_file_root() {
        let dir = text_dir();
        let params = json!({ "pattern": "zebra", "path": dir.path(), "no_ignore": true });
        assert_eq!(
            GrepTool
                .execute(params, create_tool_context())
                .await
                .unwrap(),
            "No matches found."
        );
        assert_eq!(
            grep_in(&dir.path().join("t.txt"), "zebra").await.unwrap(),
            "No matches found."
        );
    }

    /// Rows 2 and 4–8 as Rust regex has them (Phase 1 pinned GNU `grep -E`
    /// here, where most of these differ).
    #[tokio::test]
    async fn rust_regex_edge_cases_are_pinned() {
        let dir = text_dir();
        let file = dir.path().join("t.txt");
        // Row 2: `\|` is a literal pipe, not alternation.
        let out = grep_in(&file, "alpha\\|beta").await.unwrap();
        assert_eq!(lines_of(&out), ["alpha|beta"], "{out}");
        // Row 6: `\d` is a digit class and `(?i)` a flag.
        let out = grep_in(&file, "\\d+").await.unwrap();
        assert_eq!(lines_of(&out), ["123"], "{out}");
        let out = grep_in(&file, "(?i)ALPHA").await.unwrap();
        assert_eq!(lines_of(&out), ["alpha", "alpha|beta"], "{out}");
        // Row 7: word boundaries and `\w`/`\s`.
        let out = grep_in(&file, "\\bword\\b").await.unwrap();
        assert_eq!(lines_of(&out), ["word here"], "{out}");
        let out = grep_in(&file, "\\w+\\s").await.unwrap();
        assert!(lines_of(&out).contains(&"word here"), "{out}");
        // Rows 4, 5 and 8: an unescaped `{`, a leading `*`, back-references
        // and lookaround don't parse: a tool error, never a silent miss.
        for pattern in ["fn main() {", "foo{", "*foo", "^(a)\\1$", "(?=x)"] {
            let err = grep_in(&file, pattern).await.map_err(tool_msg).unwrap_err();
            assert!(err.starts_with("invalid regex: "), "{pattern}: {err}");
        }
    }

    /// Row 13: binary matches are surfaced, alone or next to text matches.
    #[tokio::test]
    async fn binary_file_matches_are_surfaced() {
        let only = tempfile::tempdir().unwrap();
        write(only.path(), "blob.bin", "abc\0alpha\n");
        let out = grep_in(only.path(), "alpha").await.unwrap();
        assert!(
            out.starts_with("[binary file matches, lines not shown: "),
            "{out}"
        );
        assert!(out.contains("blob.bin"), "{out}");

        let mixed = tempfile::tempdir().unwrap();
        write(mixed.path(), "blob.bin", "abc\0alpha\n");
        write(mixed.path(), "t.txt", "alpha\n");
        let out = grep_in(mixed.path(), "alpha").await.unwrap();
        assert!(out.contains("t.txt:1:alpha"), "{out}");
        assert!(
            out.contains("[binary file matches, lines not shown: ") && out.contains("blob.bin"),
            "{out}"
        );
        assert!(!out.contains("Binary file"), "{out}");
    }

    /// Row 17: an unreadable file next to matches gives the matches plus a
    /// note. Skipped when permissions don't apply (root).
    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_file_beside_matches_gives_matches_and_a_note() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.txt", "alpha\n");
        write(dir.path(), "locked.txt", "alpha\n");
        let locked = dir.path().join("locked.txt");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&locked).is_ok() {
            eprintln!("skipped: permissions are not enforced for this user");
            return;
        }
        let out = grep_in(dir.path(), "alpha").await.unwrap();
        assert!(out.contains("t.txt:1:alpha"), "{out}");
        let note = format!(
            "[1 error while searching, results may be incomplete. First: {}: Permission denied",
            locked.display()
        );
        assert!(out.contains(&note), "{out}");
    }
}
