use super::unix_programs::{run_bounded, truncation_footer, BoundedRun};
use super::util::path_arg_never_option;
use super::{resolve_path_in, Tool, ToolContext};
use crate::{Result, RuntimeError};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::process::Command;

/// How long one search may run.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Directories find never descends into (each is still listed itself when
/// it matches, as before).
const EXCLUDED_DIRS: [&str; 3] = [".git", "node_modules", "target"];

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
        "Find files by name using glob patterns. Searches recursively from the given path. Excludes .git directories."
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
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let cmd = build_command(&params, &ctx)?;
        let cap = ctx.limits.max_tool_buffer;
        let run = run_bounded(cmd, "find", cap, TIMEOUT).await?;
        render(run, cap, TIMEOUT)
    }
}

/// The tool result for a finished (or stopped) find run. find exits 0 when
/// it could read everything and non-zero after any error (a missing path, an
/// unreadable directory), still printing what it found.
fn render(run: BoundedRun, cap: usize, timeout: Duration) -> Result<String> {
    let (mut out, cut) = run.text(cap);
    let secs = timeout.as_secs();
    let notes = run.stderr.notes();

    if out.trim().is_empty() {
        if run.timed_out {
            return Err(RuntimeError::Tool(format!(
                "Find timed out after {secs}s before finding a match; partial results may be \
                 missing. Narrow the path."
            )));
        }
        let code = run.code;
        if code != Some(0) {
            return Err(RuntimeError::Tool(run.stderr.failure(code)));
        }
        out = "No files found.".to_string();
    } else {
        if cut {
            out.push_str("\n\n");
            out.push_str(&truncation_footer(cap));
        }
        if run.timed_out {
            out.push_str(&format!(
                "\n\n... (find timed out after {secs}s; results are partial, more files may be missing)"
            ));
        }
    }
    for note in notes {
        out.push_str("\n\n");
        out.push_str(&note);
    }
    Ok(out)
}

/// Build the `find` argv. Split out so the argv can be checked (and run in a temp
/// dir) by tests.
fn build_command(params: &Value, ctx: &ToolContext) -> Result<Command> {
    let pattern = params["pattern"]
        .as_str()
        .ok_or_else(|| RuntimeError::Tool("Missing pattern parameter".to_string()))?;
    let path = path_arg_never_option(resolve_path_in(
        params["path"].as_str().unwrap_or("."),
        ctx.capabilities.cwd.as_deref(),
    ));
    let file_type = params["type"].as_str();
    if let Some(t) = file_type {
        if !matches!(t, "f" | "d" | "l") {
            return Err(RuntimeError::Tool(format!(
                "Invalid type {t:?}: use \"f\", \"d\" or \"l\""
            )));
        }
    }

    let mut cmd = super::unix_programs::unix_tool_command("find", ctx)?;
    cmd.arg(&path);

    // POSIX only (GNU, BSD and Git for Windows' find):
    //   ( -type d ( -name .git -o -name node_modules -o -name target ) -prune MATCH )
    //   -o MATCH
    // with MATCH = [-type T] -name PATTERN -print. An excluded directory is
    // pruned (never walked) and still listed itself when it matches, as
    // before; everything else is listed when it matches.
    let mut matcher: Vec<&str> = Vec::new();
    if let Some(t) = file_type {
        matcher.extend(["-type", t]);
    }
    matcher.extend(["-name", pattern, "-print"]);

    cmd.args(["(", "-type", "d", "("]);
    for (i, dir) in EXCLUDED_DIRS.into_iter().enumerate() {
        if i > 0 {
            cmd.arg("-o");
        }
        cmd.args(["-name", dir]);
    }
    cmd.args([")", "-prune"]);
    cmd.args(&matcher);
    cmd.args([")", "-o"]);
    cmd.args(matcher);

    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::create_tool_context;
    use super::*;
    use crate::tools::Tool;
    use serde_json::json;

    #[test]
    fn test_find_tool_schema() {
        let tool = FindTool;
        assert_eq!(tool.name(), "find");
        assert!(!tool.description().is_empty());

        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert!(params["required"].is_array());
    }

    #[tokio::test]
    async fn test_find_tool_execution() {
        let temp_dir = std::env::temp_dir().join("test_find_tool_execution");
        std::fs::create_dir_all(&temp_dir).unwrap();

        let test_file = temp_dir.join("test_find_me.txt");
        std::fs::write(&test_file, "test content").unwrap();

        let tool = FindTool;
        let ctx = create_tool_context();

        let params = json!({
            "pattern": "test_find_me*",
            "path": temp_dir.to_string_lossy()
        });

        let result = tool.execute(params, ctx).await.unwrap();

        // Should contain the filename
        assert!(result.contains("test_find_me.txt"));

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    fn first_arg(cmd: &Command) -> String {
        cmd.as_std()
            .get_args()
            .next()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    /// 01 I3: with no session cwd, a `path` starting with `-` must never
    /// become find's first argv (GNU find would parse it as an expression).
    #[test]
    fn leading_dash_path_is_never_an_expression() {
        let ctx = create_tool_context();
        assert!(ctx.capabilities.cwd.is_none());
        for p in ["-delete", "-exec", "--help", "-fprint"] {
            let cmd = build_command(&json!({"pattern": "*", "path": p}), &ctx).unwrap();
            let first = first_arg(&cmd);
            assert!(!first.starts_with('-'), "{p} became argv[1] {first:?}");
            assert_eq!(first, format!(".{}{p}", std::path::MAIN_SEPARATOR));
        }
        // Ordinary paths are untouched.
        let cmd = build_command(&json!({"pattern": "*", "path": "src"}), &ctx).unwrap();
        assert_eq!(first_arg(&cmd), "src");
    }

    /// GNU find also ends the start points at `!` and `(` (and `)`, `,`):
    /// `path:"!"` would negate the `.git` exclusion. They must reach find as
    /// paths.
    #[test]
    fn start_point_terminators_are_never_expressions() {
        let ctx = create_tool_context();
        for p in ["!", "(", ")", ",", "!foo", "(x"] {
            let cmd = build_command(&json!({"pattern": "*", "path": p}), &ctx).unwrap();
            assert_eq!(
                first_arg(&cmd),
                format!(".{}{p}", std::path::MAIN_SEPARATOR),
                "path {p:?}"
            );
        }
    }

    /// End to end in a temp dir: `path:"!"` must not list `.git` contents.
    #[tokio::test]
    async fn path_bang_does_not_expose_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/secret_obj"), "").unwrap();
        let ctx = create_tool_context();
        let mut cmd = build_command(&json!({"pattern": "*", "path": "!"}), &ctx).unwrap();
        cmd.current_dir(dir.path());
        let out = cmd.output().await.unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(!stdout.contains("secret_obj"), "{stdout}");
    }

    /// End to end, confined to a temp dir: `path:"-delete"` must not delete
    /// anything. Pre-fix this ran `find -delete …` and emptied the cwd.
    #[tokio::test]
    async fn path_dash_delete_deletes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, "keep me").unwrap();
        let ctx = create_tool_context();
        let mut cmd = build_command(&json!({"pattern": "*", "path": "-delete"}), &ctx).unwrap();
        cmd.current_dir(dir.path());
        let _ = cmd.output().await.unwrap();
        assert!(victim.exists(), "find path:-delete removed files");
    }

    /// Run from a folder whose own names match the patterns. Git for
    /// Windows' find is an MSYS program, which expands wildcards in its argv
    /// against its cwd unless MSYS has `noglob`: `-name *.txt` became
    /// `-name a.txt b.txt` and `*/.git/*` became file names, so find failed.
    #[tokio::test]
    async fn patterns_reach_find_unexpanded_and_git_stays_excluded() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub/.git")).unwrap();
        for f in ["a.txt", "b.txt", "sub/c.txt", "sub/.git/d.txt"] {
            std::fs::write(dir.path().join(f), "").unwrap();
        }
        let ctx = create_tool_context();
        let mut cmd = build_command(&json!({"pattern": "*.txt", "path": "."}), &ctx).unwrap();
        cmd.current_dir(dir.path());
        let out = cmd.output().await.unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).replace('\\', "/");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        for f in ["a.txt", "b.txt", "sub/c.txt"] {
            assert!(stdout.contains(f), "{f} missing from {stdout}");
        }
        assert!(!stdout.contains("d.txt"), ".git leaked: {stdout}");
    }

    #[test]
    fn type_is_restricted_to_f_d_l() {
        let ctx = create_tool_context();
        for t in ["f", "d", "l"] {
            assert!(build_command(&json!({"pattern": "*", "type": t}), &ctx).is_ok());
        }
        for t in ["-delete", "x", "fd", ""] {
            assert!(
                build_command(&json!({"pattern": "*", "type": t}), &ctx).is_err(),
                "type {t:?} must be rejected"
            );
        }
    }

    /// The excluded directories are pruned, not filtered after the walk.
    #[test]
    fn excluded_dirs_are_pruned() {
        let ctx = create_tool_context();
        let cmd = build_command(
            &json!({"pattern": "*.rs", "type": "f", "path": "src"}),
            &ctx,
        )
        .unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let m = ["-type", "f", "-name", "*.rs", "-print"];
        let mut want = vec![
            "src", "(", "-type", "d", "(", "-name", ".git", "-o", "-name",
        ];
        want.extend(["node_modules", "-o", "-name", "target", ")", "-prune"]);
        want.extend(m);
        want.extend([")", "-o"]);
        want.extend(m);
        assert_eq!(args, want);
    }

    #[cfg(unix)]
    fn tree(paths: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for p in paths {
            let p = dir.path().join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "").unwrap();
        }
        dir
    }

    /// Make `dir` a directory nobody can read. False when permissions don't
    /// apply to this user (root), so the test can't tell anything.
    #[cfg(unix)]
    fn lock(dir: &std::path::Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(dir).is_ok() {
            eprintln!("skipped: permissions are not enforced for this user");
            unlock(dir);
            return false;
        }
        true
    }

    #[cfg(unix)]
    fn unlock(dir: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
    }

    #[cfg(unix)]
    async fn find_in(dir: &std::path::Path, params: Value) -> Result<String> {
        let mut ctx = create_tool_context();
        ctx.capabilities.cwd = Some(dir.to_path_buf());
        FindTool.execute(params, ctx).await
    }

    /// Row 25: find never enters node_modules (or .git, target). An
    /// unreadable directory deep inside one would be an error if it did.
    /// The excluded directories themselves are still listed, as before.
    #[cfg(unix)]
    #[tokio::test]
    async fn find_does_not_descend_into_excluded_dirs() {
        let dir = tree(&[
            "src/a.rs",
            "node_modules/pkg/index.rs",
            ".git/objects/x.rs",
            "target/debug/build.rs",
        ]);
        let deep = dir.path().join("node_modules/pkg/deep/er");
        if !lock(&deep) {
            return;
        }
        let out = find_in(dir.path(), json!({"pattern": "*"})).await;
        unlock(&deep);
        // Relative to the temp dir, whose own path could hold any word.
        let out = out.unwrap().replace(&dir.path().display().to_string(), "");
        assert!(out.contains("src/a.rs"), "{out}");
        for listed in ["/node_modules", "/.git", "/target"] {
            assert!(
                out.lines().any(|l| l.ends_with(listed)),
                "{listed} missing: {out}"
            );
        }
        for hidden in ["index.rs", "x.rs", "build.rs", "pkg", "objects", "debug"] {
            assert!(!out.contains(hidden), "{hidden} listed: {out}");
        }
        assert!(!out.contains("error"), "find walked node_modules: {out}");
    }

    /// Only directories inside the search are pruned: a search rooted under
    /// a `target` directory finds its files (the old `-not -path */target/*`
    /// excluded everything there).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_search_under_a_target_dir_finds_files() {
        let dir = tree(&["target/app/src/a.rs", "target/app/target/debug/b.rs"]);
        let out = find_in(&dir.path().join("target/app"), json!({"pattern": "*.rs"}))
            .await
            .unwrap();
        assert!(out.ends_with("/src/a.rs"), "{out}");
        assert!(!out.contains("b.rs"), "{out}");
    }

    /// An unreadable directory outside the excludes is reported beside the
    /// results.
    #[cfg(unix)]
    #[tokio::test]
    async fn find_errors_are_surfaced_with_the_results() {
        let dir = tree(&["src/a.rs"]);
        let locked = dir.path().join("src/locked");
        if !lock(&locked) {
            return;
        }
        let out = find_in(dir.path(), json!({"pattern": "*.rs"})).await;
        unlock(&locked);
        let out = out.unwrap();
        let a_rs = dir.path().join("./src/a.rs").display().to_string();
        assert!(out.starts_with(&a_rs), "{out}");
        assert!(
            out.contains("[1 error while searching, results may be incomplete. First: find: "),
            "{out}"
        );
        assert!(
            out.contains("locked") && out.contains("Permission denied]"),
            "{out}"
        );
    }

    /// A missing path is an error, not "No files found.".
    #[cfg(unix)]
    #[tokio::test]
    async fn find_missing_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let msg = match find_in(dir.path(), json!({"pattern": "*", "path": "nope"})).await {
            Err(RuntimeError::Tool(msg)) => msg,
            other => panic!("expected a tool error, got {other:?}"),
        };
        assert!(
            msg.starts_with("find: ") && msg.contains("No such file or directory"),
            "{msg}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn find_no_match_says_no_files_found() {
        let dir = tree(&["src/a.rs"]);
        let out = find_in(dir.path(), json!({"pattern": "*.py"}))
            .await
            .unwrap();
        assert_eq!(out, "No files found.");
    }

    /// Output past `max_tool_buffer` stops with grep's truncation footer.
    #[cfg(unix)]
    #[tokio::test]
    async fn find_output_is_capped() {
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
        assert_eq!(footer, "... (output truncated, more than 2048 total bytes)");
    }
}
