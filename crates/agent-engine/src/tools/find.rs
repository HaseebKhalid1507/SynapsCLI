use super::util::path_arg_never_option;
use super::{resolve_path_in, Tool, ToolContext};
use crate::{Result, RuntimeError};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::process::Command;

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
        let mut cmd = build_command(&params, &ctx)?;
        let output = tokio::time::timeout(Duration::from_secs(10), cmd.output())
            .await
            .map_err(|_| RuntimeError::Tool("Find timed out after 10s".to_string()))?
            .map_err(|e| RuntimeError::Tool(format!("Failed to execute find: {}", e)))?;

        let stdout = String::from_utf8_lossy(&output.stdout);

        if stdout.is_empty() {
            Ok("No files found.".to_string())
        } else {
            Ok(stdout.trim().to_string())
        }
    }
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

    let mut cmd = Command::new("find");
    if let Some(env) = &ctx.capabilities.env {
        cmd.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
    }
    cmd.arg(&path);

    cmd.args(["-not", "-path", "*/.git/*"]);
    cmd.args(["-not", "-path", "*/node_modules/*"]);
    cmd.args(["-not", "-path", "*/target/*"]);

    if let Some(t) = file_type {
        cmd.arg("-type").arg(t);
    }

    cmd.arg("-name").arg(pattern);

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
            assert_eq!(first, format!("./{p}"));
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
            assert_eq!(first_arg(&cmd), format!("./{p}"), "path {p:?}");
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
}
