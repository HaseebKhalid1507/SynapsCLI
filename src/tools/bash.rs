use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext, strip_ansi};

pub struct BashTool;

#[async_trait::async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str { "bash" }

    fn description(&self) -> &str {
        "Execute a bash command and return its output. Use for running programs, installing packages, git operations, and any shell commands. Commands time out after 30 seconds."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 30, max: 300)"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let command = params["command"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing command parameter".to_string()))?;

        let timeout_secs = params["timeout"].as_u64().unwrap_or(ctx.bash_timeout).min(ctx.bash_max_timeout);
        let max_output = ctx.max_tool_output;

        let mut child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(command)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| RuntimeError::Tool(e.to_string()))?;

        let stdout = child.stdout.take()
            .ok_or_else(|| RuntimeError::Tool("Failed to capture stdout".to_string()))?;
        let stderr = child.stderr.take()
            .ok_or_else(|| RuntimeError::Tool("Failed to capture stderr".to_string()))?;

        let (tx_inter, mut rx_inter) = tokio::sync::mpsc::unbounded_channel::<String>();

        let tx_o = tx_inter.clone();
        let txd1 = ctx.tx_delta.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut reader = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let msg = format!("{}\n", strip_ansi(&line));
                let _ = tx_o.send(msg.clone());
                if let Some(ref t) = txd1 { let _ = t.send(msg); }
            }
        });

        let tx_e = tx_inter.clone();
        let txd2 = ctx.tx_delta.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut reader = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let msg = format!("{}\n", strip_ansi(&line));
                let _ = tx_e.send(msg.clone());
                if let Some(ref t) = txd2 { let _ = t.send(msg); }
            }
        });

        drop(tx_inter);

        let result = tokio::time::timeout(tokio::time::Duration::from_secs(timeout_secs), async {
            let mut full_output = String::new();
            while let Some(line) = rx_inter.recv().await {
                full_output.push_str(&line);
                if full_output.len() > max_output {
                    full_output.truncate(max_output);
                    full_output.push_str(&format!("\n\n[output truncated at {}]", max_output));
                    break;
                }
            }
            let status = child.wait().await.map_err(|e| RuntimeError::Tool(e.to_string()))?;
            Ok::<_, RuntimeError>((status, full_output))
        }).await;

        match result {
            Ok(Ok((status, output))) => {
                if status.success() {
                    Ok(output)
                } else {
                    Err(RuntimeError::Tool(format!(
                        "Command failed (exit {}):\n{}",
                        status.code().unwrap_or(-1), output
                    )))
                }
            }
            Ok(Err(e)) => Err(RuntimeError::Tool(format!("Failed to execute command: {}", e))),
            Err(_) => Err(RuntimeError::Tool(format!("Command timed out after {}s", timeout_secs))),
        }
    }
}