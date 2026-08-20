//! `powershell` tool — Windows-native shell execution.
//!
//! Registered only on Windows. Wraps the shared `run_shell_command` core with
//! the PowerShell invocation (`-NoProfile -Command`). This gives the model a
//! native shell on Windows hosts: no quoting translation through bash, no
//! WSL/Git-Bash path impedance, native cmdlets (`Get-Process`, `Get-ChildItem`),
//! and direct access to Windows paths.

use super::{bash::ShellSpec, Tool, ToolContext};
use crate::{Result, RuntimeError};
use serde_json::{json, Value};

pub struct PowerShellTool;

#[async_trait::async_trait]
impl Tool for PowerShellTool {
    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }

    /// Same effect class as bash: arbitrary shell side effects.
    fn effect(&self) -> crate::tools::catalog::ToolEffect {
        crate::tools::catalog::ToolEffect::NonIdempotent
    }

    fn name(&self) -> &str {
        "powershell"
    }

    fn description(&self) -> &str {
        "Execute a PowerShell command on the Windows host. Use this for Windows-native tasks: \
         file system operations on C:\\ paths, registry, services, processes, Windows APIs. \
         PowerShell quoting rules apply — single quotes are literal, double quotes interpolate \
         ($var, `n). Native cmdlets like Get-Process, Get-ChildItem, Test-Path are available. \
         Commands time out after 30 seconds by default; pass a larger timeout when needed."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The PowerShell command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 30). Use a larger value for long-running commands."
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let command = params["command"]
            .as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing command parameter".to_string()))?;

        super::bash::run_shell_command(
            command,
            ShellSpec::PowerShell,
            params["timeout"].as_u64(),
            ctx,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_tool_schema() {
        let tool = PowerShellTool;
        assert_eq!(tool.name(), "powershell");
        let params = tool.parameters();
        assert!(params["properties"]["command"].is_object());
        assert!(params["required"]
            .as_array()
            .unwrap()
            .contains(&json!("command")));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn powershell_executes_simple_command() {
        let tool = PowerShellTool;
        let ctx = super::super::test_helpers::create_tool_context();
        let result = tool
            .execute(json!({ "command": "Write-Output hello-ps" }), ctx)
            .await
            .unwrap();
        assert!(result.contains("hello-ps"), "got: {result}");
    }
}
