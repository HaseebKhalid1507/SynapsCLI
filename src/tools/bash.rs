use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext, strip_ansi};

pub struct BashTool;

const READ_CHUNK_SIZE: usize = 1024;
const MAX_STREAMED_DELTA_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptKind {
    Sudo,
    Password,
}

fn sanitize_output(input: &[u8]) -> String {
    let lossy = String::from_utf8_lossy(input);
    let stripped = strip_ansi(&lossy);
    stripped
        .chars()
        .filter(|ch| {
            *ch == '\n'
                || *ch == '\r'
                || *ch == '\t'
                || (!ch.is_control() && *ch != '\u{7f}')
        })
        .collect()
}

fn detect_password_prompt(text: &str) -> Option<PromptKind> {
    let lower = text.to_ascii_lowercase();
    let has_password = lower.contains("password");
    if !has_password {
        return None;
    }
    if lower.contains("[sudo]") || lower.contains("sudo") {
        Some(PromptKind::Sudo)
    } else if lower.trim_end().ends_with(':') || lower.contains("password:") {
        Some(PromptKind::Password)
    } else {
        None
    }
}

fn append_bounded(output: &mut String, text: &str, max_output: usize) -> bool {
    if output.len() >= max_output {
        return false;
    }
    let remaining = max_output - output.len();
    if text.len() <= remaining {
        output.push_str(text);
        true
    } else {
        let mut end = remaining;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&text[..end]);
        false
    }
}

pub(crate) fn bash_script_with_secure_sudo(command: &str) -> String {
    // sudo normally opens /dev/tty for password input, bypassing our piped
    // stdin/stderr and corrupting the TUI. In the non-interactive bash tool,
    // shadow simple `sudo ...` invocations with a shell function that forces
    // sudo to read from stdin and write the prompt to stderr, where the secure
    // prompt detector can intercept it before it reaches chat/model output.
    format!(
        r#"sudo() {{
    command sudo -S -p '[sudo] password required: ' "$@"
}}
{command}"#
    )
}

#[async_trait::async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str { "bash" }

    fn description(&self) -> &str {
        "Execute a bash command and return its output. Use for running programs, installing packages, git operations, and any shell commands. Commands time out after 30 seconds by default; pass a larger timeout when needed. If sudo asks for a password, the user is prompted securely in the TUI and the password is never shown to the model."
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
                    "description": "Timeout in seconds (default: 30). Use a larger value for long-running commands."
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let command = params["command"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing command parameter".to_string()))?;

        let timeout_secs = params["timeout"].as_u64().unwrap_or(ctx.limits.bash_timeout);
        let max_output = ctx.limits.max_tool_output;

        let script = bash_script_with_secure_sudo(command);
        let mut child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(&script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| RuntimeError::Tool(e.to_string()))?;

        let stdout = child.stdout.take()
            .ok_or_else(|| RuntimeError::Tool("Failed to capture stdout".to_string()))?;
        let stderr = child.stderr.take()
            .ok_or_else(|| RuntimeError::Tool("Failed to capture stderr".to_string()))?;
        let stdin = child.stdin.take()
            .ok_or_else(|| RuntimeError::Tool("Failed to capture stdin".to_string()))?;

        let (tx_inter, mut rx_inter) = tokio::sync::mpsc::unbounded_channel::<(bool, String)>();

        let tx_o = tx_inter.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut reader = stdout;
            let mut buf = vec![0u8; READ_CHUNK_SIZE];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let msg = sanitize_output(&buf[..n]);
                        if !msg.is_empty() {
                            let _ = tx_o.send((false, msg));
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let tx_e = tx_inter.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut reader = stderr;
            let mut buf = vec![0u8; READ_CHUNK_SIZE];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let msg = sanitize_output(&buf[..n]);
                        if !msg.is_empty() {
                            let _ = tx_e.send((true, msg));
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        drop(tx_inter);

        let result = tokio::time::timeout(tokio::time::Duration::from_secs(timeout_secs), async {
            use tokio::io::AsyncWriteExt;

            let mut stdin = stdin;
            let mut full_output = String::new();
            let mut stderr_tail = String::new();
            let mut truncated = false;
            let mut streamed_bytes = 0usize;
            let mut redactions: Vec<String> = Vec::new();

            while let Some((is_stderr, mut msg)) = rx_inter.recv().await {
                if is_stderr {
                    stderr_tail.push_str(&msg);
                    if stderr_tail.len() > 512 {
                        let keep_from = stderr_tail.len() - 512;
                        if let Some((idx, _)) = stderr_tail.char_indices().find(|(i, _)| *i >= keep_from) {
                            stderr_tail.drain(..idx);
                        }
                    }
                    if let Some(kind) = detect_password_prompt(&stderr_tail) {
                        let prompt_text = stderr_tail.trim().to_string();
                        let secret = match &ctx.capabilities.secret_prompt {
                            Some(prompt) => prompt.prompt(
                                match kind {
                                    PromptKind::Sudo => "sudo password required".to_string(),
                                    PromptKind::Password => "password required".to_string(),
                                },
                                prompt_text.clone(),
                            ).await,
                            None => None,
                        };
                        match secret {
                            Some(mut value) => {
                                let secret_value = value.clone();
                                if !secret_value.is_empty() {
                                    redactions.push(secret_value);
                                }
                                value.push('\n');
                                stdin.write_all(value.as_bytes()).await
                                    .map_err(|e| RuntimeError::Tool(e.to_string()))?;
                                stdin.flush().await
                                    .map_err(|e| RuntimeError::Tool(e.to_string()))?;
                            }
                            None => {
                                let _ = child.kill().await;
                                return Err(RuntimeError::Tool("Command canceled while waiting for password".to_string()));
                            }
                        }
                        let prompt_len = prompt_text.len();
                        if prompt_len <= msg.len() {
                            let keep_len = msg.len() - prompt_len;
                            msg.truncate(keep_len);
                        } else {
                            msg.clear();
                        }
                        stderr_tail.clear();
                    }
                }

                for secret in &redactions {
                    if !secret.is_empty() {
                        msg = msg.replace(secret, "[redacted]");
                    }
                }

                if truncated {
                    continue;
                }

                let added_all = append_bounded(&mut full_output, &msg, max_output);
                if let Some(ref txd) = ctx.channels.tx_delta {
                    if streamed_bytes < MAX_STREAMED_DELTA_BYTES {
                        let remaining = MAX_STREAMED_DELTA_BYTES - streamed_bytes;
                        let delta = if msg.len() <= remaining {
                            msg.clone()
                        } else {
                            let mut end = remaining;
                            while end > 0 && !msg.is_char_boundary(end) {
                                end -= 1;
                            }
                            msg[..end].to_string()
                        };
                        streamed_bytes += delta.len();
                        if !delta.is_empty() {
                            let _ = txd.send(delta);
                        }
                    }
                }

                if !added_all {
                    full_output.push_str(&format!("\n\n[output truncated at {}]", max_output));
                    if let Some(ref txd) = ctx.channels.tx_delta {
                        let _ = txd.send(format!("\n\n[output truncated at {}]", max_output));
                    }
                    truncated = true;
                    let _ = child.kill().await;
                }
            }
            let status = child.wait().await.map_err(|e| RuntimeError::Tool(e.to_string()))?;
            Ok::<_, RuntimeError>((status, full_output))
        }).await;

        match result {
            Ok(Ok((status, output))) => {
                if status.success() || output.contains("[output truncated at") {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_sudo_password_prompt_without_newline() {
        assert_eq!(detect_password_prompt("[sudo] password for me: "), Some(PromptKind::Sudo));
    }

    #[test]
    fn sanitizes_terminal_control_sequences_and_nuls() {
        let cleaned = sanitize_output(b"ok\x1b[2J\x00done");
        assert_eq!(cleaned, "okdone");
    }
}
