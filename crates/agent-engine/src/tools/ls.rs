use super::util::path_arg_never_option;
use super::{resolve_path_in, Tool, ToolContext};
use crate::{Result, RuntimeError};
use serde_json::{json, Value};
use tokio::process::Command;

pub struct LsTool;

#[async_trait::async_trait]
impl Tool for LsTool {
    fn effect(&self) -> crate::tools::catalog::ToolEffect {
        crate::tools::catalog::ToolEffect::ReadOnly
    }

    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }

    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        "List directory contents with details (permissions, size, modification date). Defaults to current directory."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path to list (default: current directory)"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let output = build_command(&params, &ctx)
            .output()
            .await
            .map_err(|e| RuntimeError::Tool(format!("Failed to execute ls: {}", e)))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            Err(RuntimeError::Tool(format!("ls failed: {}", stderr)))
        } else if stdout.is_empty() {
            Ok("Directory is empty.".to_string())
        } else {
            Ok(stdout.to_string())
        }
    }
}

/// Build the `ls` argv (split out for tests).
fn build_command(params: &Value, ctx: &ToolContext) -> Command {
    let path = path_arg_never_option(resolve_path_in(
        params["path"].as_str().unwrap_or("."),
        ctx.capabilities.cwd.as_deref(),
    ));
    let mut cmd = Command::new("ls");
    if let Some(env) = &ctx.capabilities.env {
        cmd.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
    }
    cmd.arg("-lah").arg(&path);
    cmd
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::create_tool_context;
    use super::*;
    use crate::tools::Tool;
    use serde_json::json;

    #[test]
    fn test_ls_tool_schema() {
        let tool = LsTool;
        assert_eq!(tool.name(), "ls");
        assert!(!tool.description().is_empty());

        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert!(params["required"].is_array());
    }

    #[tokio::test]
    async fn test_ls_tool_execution() {
        let tool = LsTool;
        let ctx = create_tool_context();

        // Use a dedicated temp dir to avoid races with other tests
        let dir = std::env::temp_dir().join("synaps_test_ls");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hello.txt"), "hi").unwrap();

        let params = json!({
            "path": dir.to_str().unwrap()
        });

        let result = tool.execute(params, ctx).await.unwrap();
        assert!(result.contains("hello.txt"));

        // Cleanup
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 01 I3 audit: an option-like `path` must reach `ls` as a path.
    #[test]
    fn leading_dash_path_is_never_an_option() {
        let ctx = create_tool_context();
        let cmd = build_command(&json!({"path": "-R"}), &ctx);
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["-lah", "./-R"]);
    }

    #[tokio::test]
    async fn lists_a_file_whose_name_starts_with_dash() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("-x"), "").unwrap();
        std::fs::write(dir.path().join("other.txt"), "").unwrap();
        let ctx = create_tool_context();
        let mut cmd = build_command(&json!({"path": "-x"}), &ctx);
        cmd.current_dir(dir.path());
        let out = cmd.output().await.unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // `-x` is listed as a file, not taken as `ls -x` over the whole dir.
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("-x"), "{stdout}");
        assert!(!stdout.contains("other.txt"), "{stdout}");
    }
}
