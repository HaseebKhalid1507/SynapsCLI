//! The `find` tool: the in-process walk grep uses (`tools::search::files`),
//! listing paths whose file name matches a glob, one per line, as `find PATH
//! -name GLOB` printed them. Nothing is spawned.

use super::search::{self, files, pattern};
use super::{resolve_path_in, Tool, ToolContext};
use crate::{Result, RuntimeError};
use serde_json::{json, Value};
use std::time::Duration;

/// How long one search may run before it returns what it has.
const TIMEOUT: Duration = Duration::from_secs(10);

pub struct FindTool;

#[async_trait::async_trait]
impl Tool for FindTool {
    fn effect(&self) -> crate::tools::catalog::ToolEffect {
        crate::tools::catalog::ToolEffect::ReadOnly
    }

    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }

    fn name(&self) -> &str {
        "find"
    }

    fn description(&self) -> &str {
        "Find files by name: a glob matched against file names, searched recursively from the given path. Respects .gitignore (use no_ignore to include ignored files, node_modules and target); hidden files are listed, VCS dirs (.git) never."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match file names (e.g. \"*.rs\", \"Cargo.*\")"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (default: current directory)"
                },
                "type": {
                    "type": "string",
                    "description": "Filter by type: \"f\" for files, \"d\" for directories, \"l\" for symlinks"
                },
                "no_ignore": {
                    "type": "boolean",
                    "description": "Also list files skipped by .gitignore/.ignore, node_modules and target (default false)"
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
fn request(params: &Value, ctx: &ToolContext) -> Result<files::FilesRequest> {
    let glob = params["pattern"]
        .as_str()
        .ok_or_else(|| RuntimeError::Tool("Missing pattern parameter".to_string()))?;
    let root = resolve_path_in(
        params["path"].as_str().unwrap_or("."),
        ctx.capabilities.cwd.as_deref(),
    );
    let entry_type = params["type"]
        .as_str()
        .map(files::EntryType::parse)
        .transpose()
        .map_err(RuntimeError::Tool)?;
    let name = pattern::name_glob(glob).map_err(RuntimeError::Tool)?;
    Ok(files::FilesRequest {
        root,
        name,
        entry_type,
        no_ignore: params["no_ignore"].as_bool().unwrap_or(false),
        cwd: ctx.capabilities.cwd.clone(),
        cap: ctx.limits.max_tool_buffer,
    })
}

/// Runs the walk on the blocking pool and renders the result. `turn`
/// cancelled (or this future dropped) stops it with partial results.
async fn run_search(
    request: files::FilesRequest,
    turn: Option<crate::CancellationToken>,
    timeout: Duration,
) -> Result<String> {
    let cap = request.cap;
    let outcome = search::run_blocking(timeout, turn, move |control| {
        files::search(&request, control)
    })
    .await?
    .map_err(RuntimeError::Tool)?;
    files::render(outcome, cap, timeout)
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::create_tool_context;
    use super::*;
    use crate::tools::Tool;
    use serde_json::json;
    use std::path::{Path, PathBuf};

    /// `root` joined with a `/`-separated relative path (native separators).
    fn native(root: &Path, rel: &str) -> PathBuf {
        rel.split('/').fold(root.to_path_buf(), |p, c| p.join(c))
    }

    fn tree(paths: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for p in paths {
            let p = native(dir.path(), p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "").unwrap();
        }
        dir
    }

    /// Runs find with the session cwd at `dir`.
    async fn find_in(dir: &Path, params: Value) -> Result<String> {
        let mut ctx = create_tool_context();
        ctx.capabilities.cwd = Some(dir.to_path_buf());
        FindTool.execute(params, ctx).await
    }

    /// A tool error's own message (Display adds "Tool execution failed: ").
    fn tool_msg(e: RuntimeError) -> String {
        match e {
            RuntimeError::Tool(msg) => msg,
            other => other.to_string(),
        }
    }

    /// The result's paths relative to `dir`, `/`-separated (`<cwd>/./x` is
    /// `x`); the search path itself is `.`.
    fn rel(out: &str, dir: &Path) -> Vec<String> {
        let dir = dir.display().to_string();
        out.lines()
            .map(|line| {
                let rest = line.strip_prefix(dir.as_str()).unwrap_or(line);
                let rest = rest.replace('\\', "/");
                let rest = rest.trim_start_matches('/');
                let rest = rest.strip_prefix("./").unwrap_or(rest);
                if rest.is_empty() { "." } else { rest }.to_string()
            })
            .collect()
    }

    #[test]
    fn test_find_tool_schema() {
        let tool = FindTool;
        assert_eq!(tool.name(), "find");
        assert!(tool.description().contains(".gitignore"));
        assert!(tool.description().contains("no_ignore"));

        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert_eq!(params["required"], json!(["pattern"]));
        for (name, ty) in [
            ("pattern", "string"),
            ("path", "string"),
            ("type", "string"),
            ("no_ignore", "boolean"),
        ] {
            assert_eq!(params["properties"][name]["type"], ty, "{name}");
        }
    }

    #[tokio::test]
    async fn test_find_tool_execution() {
        let dir = tree(&["test_find_me.txt", "other.txt"]);
        let params = json!({
            "pattern": "test_find_me*",
            "path": dir.path().to_string_lossy()
        });
        let result = FindTool
            .execute(params, create_tool_context())
            .await
            .unwrap();
        assert_eq!(
            result,
            dir.path().join("test_find_me.txt").display().to_string()
        );
    }

    /// The default path is the session cwd joined with `.`, and paths keep
    /// that form (`<cwd>/./src/a.rs`), as `find <cwd>/.` printed them; no
    /// trailing newline.
    #[tokio::test]
    async fn default_path_is_the_session_cwd() {
        let dir = tree(&["src/a.rs", "b.rs"]);
        let out = find_in(dir.path(), json!({"pattern": "*.rs"}))
            .await
            .unwrap();
        let dot = dir.path().join(".");
        assert_eq!(
            out,
            format!(
                "{}\n{}",
                dot.join("b.rs").display(),
                native(&dot, "src/a.rs").display()
            )
        );
        // `.` as the path is the same.
        let same = find_in(dir.path(), json!({"pattern": "*.rs", "path": "."}))
            .await
            .unwrap();
        assert_eq!(same, out);
    }

    /// A file as the path lists that file when its name matches, as the
    /// path was given (`src/a.rs` joined onto the cwd).
    #[tokio::test]
    async fn a_file_path_lists_the_file() {
        let dir = tree(&["src/a.rs"]);
        let out = find_in(dir.path(), json!({"pattern": "*.rs", "path": "src/a.rs"}))
            .await
            .unwrap();
        assert_eq!(out, dir.path().join("src/a.rs").display().to_string());
    }

    #[tokio::test]
    async fn no_match_says_no_files_found() {
        let dir = tree(&["src/a.rs"]);
        let out = find_in(dir.path(), json!({"pattern": "*.py"}))
            .await
            .unwrap();
        assert_eq!(out, format!("No files found.\n\n{}", search::IGNORED_NOTE));
        let out = find_in(dir.path(), json!({"pattern": "*.py", "no_ignore": true}))
            .await
            .unwrap();
        assert_eq!(out, "No files found.");
    }

    /// A file the ignore rules exclude (a `.env`, typically) isn't listed, the
    /// miss says how to include it, and `no_ignore` lists it.
    #[tokio::test]
    async fn an_ignored_file_is_missed_with_a_note_and_no_ignore_lists_it() {
        let dir = tree(&[".env"]);
        std::fs::write(dir.path().join(".ignore"), ".env\n").unwrap();
        let missed = find_in(dir.path(), json!({"pattern": ".env"}))
            .await
            .unwrap();
        assert!(missed.starts_with("No files found."), "{missed}");
        assert!(missed.contains("no_ignore: true"), "{missed}");
        let found = find_in(dir.path(), json!({"pattern": ".env", "no_ignore": true}))
            .await
            .unwrap();
        assert_eq!(
            found,
            dir.path().join(".").join(".env").display().to_string()
        );
    }

    /// A missing path is an error, not "No files found.".
    #[tokio::test]
    async fn missing_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = find_in(dir.path(), json!({"pattern": "*", "path": "nope"}))
            .await
            .map_err(tool_msg)
            .unwrap_err();
        assert_eq!(
            err,
            format!("Path not found: {}", dir.path().join("nope").display())
        );
    }

    /// A pattern with a separator can't match a name: an error pointing at
    /// `path`. An invalid glob is an error with the parser's message.
    #[tokio::test]
    async fn bad_patterns_are_errors() {
        let dir = tree(&["src/a.rs"]);
        let err = find_in(dir.path(), json!({"pattern": "src/*.rs"}))
            .await
            .map_err(tool_msg)
            .unwrap_err();
        assert!(err.starts_with("invalid pattern \"src/*.rs\""), "{err}");
        assert!(err.contains("`path`"), "{err}");
        let err = find_in(dir.path(), json!({"pattern": "[a"}))
            .await
            .map_err(tool_msg)
            .unwrap_err();
        assert!(err.starts_with("invalid pattern: "), "{err}");
        assert!(err.contains("unclosed character class"), "{err}");
        let err = find_in(dir.path(), json!({}))
            .await
            .map_err(tool_msg)
            .unwrap_err();
        assert_eq!(err, "Missing pattern parameter");
    }

    #[tokio::test]
    async fn type_and_no_ignore_reach_the_engine() {
        let dir = tree(&[".git/HEAD", "node_modules/m/x.rs", "src/a.rs", ".hidden.rs"]);
        std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(dir.path().join("x.log"), "").unwrap();
        let out = find_in(dir.path(), json!({"pattern": "*", "type": "d"}))
            .await
            .unwrap();
        assert_eq!(rel(&out, dir.path()), [".", "src"]);
        let out = find_in(dir.path(), json!({"pattern": "*", "type": "f"}))
            .await
            .unwrap();
        assert_eq!(
            rel(&out, dir.path()),
            [".gitignore", ".hidden.rs", "src/a.rs"]
        );
        let out = find_in(
            dir.path(),
            json!({"pattern": "*", "type": "f", "no_ignore": true}),
        )
        .await
        .unwrap();
        assert_eq!(
            rel(&out, dir.path()),
            [
                ".gitignore",
                ".hidden.rs",
                "node_modules/m/x.rs",
                "src/a.rs",
                "x.log"
            ]
        );
    }

    #[tokio::test]
    async fn type_is_restricted_to_f_d_l() {
        let dir = tree(&["a.rs"]);
        for t in ["f", "d", "l"] {
            assert!(
                find_in(dir.path(), json!({"pattern": "*", "type": t}))
                    .await
                    .is_ok(),
                "type {t:?}"
            );
        }
        for t in ["-delete", "x", "fd", ""] {
            let err = find_in(dir.path(), json!({"pattern": "*", "type": t}))
                .await
                .map_err(tool_msg)
                .unwrap_err();
            assert!(err.starts_with("Invalid type"), "type {t:?}: {err}");
        }
    }

    /// `path:"!"` (once a GNU find expression that negated the `.git`
    /// exclusion) is just a path: missing here, so an error; `.git` contents
    /// are never listed, whatever the path.
    #[tokio::test]
    async fn path_bang_does_not_expose_git_dir() {
        let dir = tree(&[".git/secret_obj", "a.txt"]);
        let err = find_in(dir.path(), json!({"pattern": "*", "path": "!"}))
            .await
            .map_err(tool_msg)
            .unwrap_err();
        assert!(err.starts_with("Path not found: "), "{err}");
        assert!(!err.contains("secret_obj"), "{err}");
        for path in [".", "./"] {
            let out = find_in(dir.path(), json!({"pattern": "*", "path": path}))
                .await
                .unwrap();
            assert!(out.contains("a.txt"), "{out}");
            assert!(
                !out.contains("secret_obj") && !out.contains(".git"),
                "{out}"
            );
        }
    }

    /// `path:"-delete"` (once a GNU find expression that emptied the cwd) is
    /// just a path: nothing is deleted, and a directory of that name is
    /// listed like any other.
    #[tokio::test]
    async fn path_dash_delete_deletes_nothing() {
        let dir = tree(&["victim.txt"]);
        let victim = dir.path().join("victim.txt");
        let err = find_in(dir.path(), json!({"pattern": "*", "path": "-delete"}))
            .await
            .map_err(tool_msg)
            .unwrap_err();
        assert!(err.starts_with("Path not found: "), "{err}");
        assert!(victim.exists(), "find path:-delete removed files");
        std::fs::create_dir(dir.path().join("-delete")).unwrap();
        std::fs::write(dir.path().join("-delete").join("in.txt"), "").unwrap();
        let out = find_in(dir.path(), json!({"pattern": "*.txt", "path": "-delete"}))
            .await
            .unwrap();
        assert_eq!(rel(&out, &dir.path().join("-delete")), ["in.txt"]);
        assert!(victim.exists(), "find path:-delete removed files");
    }

    /// Run from a folder whose own names match the pattern: the pattern is
    /// matched in-process, never expanded by a shell or MSYS (Git for
    /// Windows' find turned `-name *.txt` into file names), and `.git` stays
    /// out, even in a nested repo.
    #[tokio::test]
    async fn patterns_reach_find_unexpanded_and_git_stays_excluded() {
        let dir = tree(&["a.txt", "b.txt", "sub/c.txt", "sub/.git/d.txt"]);
        let out = find_in(dir.path(), json!({"pattern": "*.txt", "path": "."}))
            .await
            .unwrap();
        assert_eq!(rel(&out, dir.path()), ["a.txt", "b.txt", "sub/c.txt"]);
    }

    /// Row 25: `.git`, `node_modules` and `target` are neither listed nor
    /// walked; the worktree form of `.git` (a file) is never listed either.
    /// `no_ignore` lists node_modules and target, never `.git`.
    #[tokio::test]
    async fn excluded_dirs_are_absent_and_no_ignore_lists_all_but_git() {
        let dir = tree(&[
            "src/a.rs",
            "node_modules/pkg/index.rs",
            ".git/objects/x.rs",
            "target/debug/build.rs",
            "wt/.git",
            "wt/b.rs",
        ]);
        let out = find_in(dir.path(), json!({"pattern": "*"})).await.unwrap();
        assert_eq!(
            rel(&out, dir.path()),
            [".", "src", "src/a.rs", "wt", "wt/b.rs"]
        );
        let out = find_in(dir.path(), json!({"pattern": "*", "no_ignore": true}))
            .await
            .unwrap();
        let listed = rel(&out, dir.path());
        for want in [
            "node_modules/pkg/index.rs",
            "target",
            "target/debug/build.rs",
        ] {
            assert!(listed.iter().any(|l| l == want), "{want} missing: {out}");
        }
        assert!(!out.contains(".git"), "{out}");
    }

    /// A search rooted under a `target` directory finds its files; a
    /// `target` below the path is still pruned.
    #[tokio::test]
    async fn a_search_under_a_target_dir_finds_files() {
        let dir = tree(&["target/app/src/a.rs", "target/app/target/debug/b.rs"]);
        let out = find_in(dir.path(), json!({"pattern": "*.rs", "path": "target/app"}))
            .await
            .unwrap();
        assert_eq!(rel(&out, &dir.path().join("target/app")), ["src/a.rs"]);
    }

    /// An unreadable directory is reported beside the results.
    #[cfg(unix)]
    #[tokio::test]
    async fn walk_errors_are_surfaced_with_the_results() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tree(&["src/a.rs", "src/locked/b.rs"]);
        let locked = native(dir.path(), "src/locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&locked).is_ok() {
            eprintln!("skipped: permissions are not enforced for this user");
            return;
        }
        let out = find_in(dir.path(), json!({"pattern": "*.rs"})).await;
        let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755));
        let out = out.unwrap();
        let a_rs = native(&dir.path().join("."), "src/a.rs")
            .display()
            .to_string();
        let note = format!(
            "{a_rs}\n\n[1 error while searching, results may be incomplete. First: {}: ",
            native(&dir.path().join("."), "src/locked").display()
        );
        assert!(out.starts_with(&note), "{out}");
        assert!(out.ends_with("Permission denied (os error 13)]"), "{out}");
    }

    /// Output past `max_tool_buffer` stops with grep's truncation footer, on
    /// a whole path.
    #[tokio::test]
    async fn output_is_capped() {
        let names: Vec<String> = (0..400).map(|i| format!("f{i:03}.txt")).collect();
        let dir = tree(&names.iter().map(String::as_str).collect::<Vec<_>>());
        let mut ctx = create_tool_context();
        ctx.capabilities.cwd = Some(dir.path().to_path_buf());
        ctx.limits.max_tool_buffer = 2048;
        let out = FindTool
            .execute(json!({"pattern": "*.txt"}), ctx)
            .await
            .unwrap();
        let (body, footer) = out.split_once("\n\n").unwrap();
        assert!(body.len() <= 2048 && body.ends_with(".txt"), "{body}");
        assert!(body.lines().all(|l| l.ends_with(".txt")), "{body}");
        assert_eq!(footer, "... (output truncated, more than 2048 total bytes)");
    }

    /// A cancelled turn stops the walk: nothing found yet is an error.
    #[tokio::test]
    async fn turn_cancellation_reaches_the_walk() {
        let dir = tree(&["a.rs"]);
        let turn = crate::CancellationToken::new();
        turn.cancel();
        let mut ctx = create_tool_context();
        ctx.capabilities.cwd = Some(dir.path().to_path_buf());
        ctx.capabilities.launch_cancel = Some(turn);
        let err = FindTool
            .execute(json!({"pattern": "*.rs"}), ctx)
            .await
            .map_err(tool_msg)
            .unwrap_err();
        assert_eq!(err, "Find was cancelled before finding a match.");
    }

    /// The deadline reaches the blocking walk: with no time at all the call
    /// is a timeout error, not "No files found.".
    #[tokio::test]
    async fn deadline_reaches_the_walk() {
        let dir = tree(&["a.rs"]);
        let mut ctx = create_tool_context();
        ctx.capabilities.cwd = Some(dir.path().to_path_buf());
        let request = request(&json!({"pattern": "*.rs"}), &ctx).unwrap();
        let err = run_search(request, None, Duration::ZERO)
            .await
            .map_err(tool_msg)
            .unwrap_err();
        assert!(
            err.starts_with("Find timed out after 0s before finding a match"),
            "{err}"
        );
    }
}
