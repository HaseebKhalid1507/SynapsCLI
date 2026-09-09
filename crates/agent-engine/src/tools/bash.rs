use super::{strip_ansi, Tool, ToolContext};
use crate::{Result, RuntimeError};
use serde_json::{json, Value};
use zeroize::Zeroize;

const BASH_INTERMEDIARY_CHANNEL_CAPACITY: usize = 64;
static BASH_INTERMEDIARY_PRODUCED_BYTES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static BASH_INTERMEDIARY_ACCEPTED_BYTES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static BASH_INTERMEDIARY_CONSUMED_BYTES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static BASH_INTERMEDIARY_DROPPED_BYTES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
pub struct BashIntermediarySnapshot {
    pub produced_bytes: u64,
    pub accepted_bytes: u64,
    pub consumed_bytes: u64,
    pub dropped_bytes: u64,
    pub retained_bytes: u64,
}

pub fn bash_intermediary_snapshot() -> BashIntermediarySnapshot {
    use std::sync::atomic::Ordering;
    let produced = BASH_INTERMEDIARY_PRODUCED_BYTES.load(Ordering::Relaxed);
    let accepted = BASH_INTERMEDIARY_ACCEPTED_BYTES.load(Ordering::Relaxed);
    let consumed = BASH_INTERMEDIARY_CONSUMED_BYTES.load(Ordering::Relaxed);
    let dropped = BASH_INTERMEDIARY_DROPPED_BYTES.load(Ordering::Relaxed);
    BashIntermediarySnapshot {
        produced_bytes: produced,
        accepted_bytes: accepted,
        consumed_bytes: consumed,
        dropped_bytes: dropped,
        retained_bytes: produced.saturating_sub(consumed).saturating_sub(dropped),
    }
}

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
            *ch == '\n' || *ch == '\r' || *ch == '\t' || (!ch.is_control() && *ch != '\u{7f}')
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

/// Resolve the bash executable for the bash tool.
///
/// Unix: `bash` from PATH, as always. Windows: bash is usually NOT on PATH —
/// prefer an explicit PATH hit, then Git Bash's known install locations, then
/// WSL's `bash.exe` (runs inside the default distro) as a last resort.
pub(crate) fn bash_program() -> std::ffi::OsString {
    #[cfg(unix)]
    {
        "bash".into()
    }
    #[cfg(windows)]
    {
        let on_path = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d.join("bash.exe").is_file()))
            .unwrap_or(false);
        if on_path {
            return "bash".into();
        }
        for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
            if let Some(pf) = std::env::var_os(var) {
                let candidate = std::path::Path::new(&pf).join(r"Git\bin\bash.exe");
                if candidate.is_file() {
                    return candidate.into_os_string();
                }
            }
        }
        let wsl = std::path::Path::new(r"C:\Windows\System32\bash.exe");
        if wsl.is_file() {
            return wsl.as_os_str().to_os_string();
        }
        // Nothing found — let spawn fail with a readable NotFound error.
        "bash".into()
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

/// Resolve the PowerShell executable. Prefers pwsh (PowerShell 7+, the
/// cross-platform build with sane defaults) and falls back to the
/// Windows-bundled powershell.exe (5.1).
#[cfg(windows)]
pub(crate) fn powershell_program() -> std::ffi::OsString {
    let on_path = |exe: &str| {
        std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d.join(exe).is_file()))
            .unwrap_or(false)
    };
    if on_path("pwsh.exe") {
        return "pwsh".into();
    }
    for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Some(pf) = std::env::var_os(var) {
            let candidate = std::path::Path::new(&pf).join(r"PowerShell\7\pwsh.exe");
            if candidate.is_file() {
                return candidate.into_os_string();
            }
        }
    }
    // powershell.exe is always present on Windows (System32).
    "powershell".into()
}

/// Unix stub — the powershell tool is only registered on Windows, so this is
/// never called there; it exists so cross-compilation `cargo check` works.
#[cfg(not(windows))]
pub(crate) fn powershell_program() -> std::ffi::OsString {
    "pwsh".into()
}

#[async_trait::async_trait]
impl Tool for BashTool {
    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }

    /// Explicitly NonIdempotent (Task 24): arbitrary shell side effects —
    /// serialized execution, no concurrency key.
    fn effect(&self) -> crate::tools::catalog::ToolEffect {
        crate::tools::catalog::ToolEffect::NonIdempotent
    }

    fn name(&self) -> &str {
        "bash"
    }

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
        let command = params["command"]
            .as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing command parameter".to_string()))?;

        let script = bash_script_with_secure_sudo(command);
        run_shell_command(&script, ShellSpec::Bash, params["timeout"].as_u64(), ctx).await
    }
}

/// Which shell backend the shared executor drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellSpec {
    Bash,
    PowerShell,
}

/// Shared execution core for bash + powershell: piped stdin/stdout/stderr,
/// chunked streaming with byte-conservation accounting, secret-prompt
/// interception, truncation, timeout. The only per-shell differences are
/// which program is spawned and which args it takes.
pub(crate) async fn run_shell_command(
    script: &str,
    spec: ShellSpec,
    requested_timeout: Option<u64>,
    ctx: ToolContext,
) -> Result<String> {
    {
        let requested_timeout = requested_timeout.unwrap_or(ctx.limits.bash_timeout);
        // H5: enforce bash_max_timeout cap — prevent DoS via prompt injection
        // requesting unbounded timeouts (e.g. timeout:2592000 + infinite loop).
        let timeout_secs = if ctx.limits.bash_max_timeout > 0 {
            requested_timeout.min(ctx.limits.bash_max_timeout)
        } else {
            requested_timeout
        };
        let max_output = ctx.limits.max_tool_buffer;

        let (program, args): (std::ffi::OsString, Vec<std::ffi::OsString>) = match spec {
            ShellSpec::Bash => (bash_program(), vec!["-c".into(), script.into()]),
            ShellSpec::PowerShell => (
                powershell_program(),
                vec!["-NoProfile".into(), "-Command".into(), script.into()],
            ),
        };
        let mut cmd = tokio::process::Command::new(program);
        if let Some(cwd) = ctx.capabilities.cwd.as_deref() {
            cmd.current_dir(cwd);
        }
        cmd.args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        // Start the child in a new session (setsid) so it has no controlling
        // terminal. Programs that open /dev/tty directly (SSH fingerprint
        // prompts, gpg pinentry, git credential helpers, pagers) will get
        // ENXIO and fail with a readable error on stderr instead of hanging
        // invisibly until timeout. Sudo is unaffected — we already force
        // `-S` (stdin) via bash_script_with_secure_sudo().
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }

        let mut child = cmd.spawn().map_err(|e| RuntimeError::Tool(e.to_string()))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RuntimeError::Tool("Failed to capture stdout".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| RuntimeError::Tool("Failed to capture stderr".to_string()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| RuntimeError::Tool("Failed to capture stdin".to_string()))?;

        let (tx_inter, mut rx_inter) =
            tokio::sync::mpsc::channel::<(bool, String)>(BASH_INTERMEDIARY_CHANNEL_CAPACITY);

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
                            use std::sync::atomic::Ordering;
                            BASH_INTERMEDIARY_PRODUCED_BYTES
                                .fetch_add(msg.len() as u64, Ordering::Relaxed);
                            let len = msg.len();
                            if tx_o.send((false, msg)).await.is_ok() {
                                BASH_INTERMEDIARY_ACCEPTED_BYTES
                                    .fetch_add(len as u64, Ordering::Relaxed);
                            } else {
                                BASH_INTERMEDIARY_DROPPED_BYTES
                                    .fetch_add(len as u64, Ordering::Relaxed);
                                break;
                            }
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
                            use std::sync::atomic::Ordering;
                            BASH_INTERMEDIARY_PRODUCED_BYTES
                                .fetch_add(msg.len() as u64, Ordering::Relaxed);
                            let len = msg.len();
                            if tx_e.send((true, msg)).await.is_ok() {
                                BASH_INTERMEDIARY_ACCEPTED_BYTES
                                    .fetch_add(len as u64, Ordering::Relaxed);
                            } else {
                                BASH_INTERMEDIARY_DROPPED_BYTES
                                    .fetch_add(len as u64, Ordering::Relaxed);
                                break;
                            }
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
                BASH_INTERMEDIARY_CONSUMED_BYTES
                    .fetch_add(msg.len() as u64, std::sync::atomic::Ordering::Relaxed);
                if is_stderr {
                    stderr_tail.push_str(&msg);
                    if stderr_tail.len() > 512 {
                        let keep_from = stderr_tail.len() - 512;
                        if let Some((idx, _)) =
                            stderr_tail.char_indices().find(|(i, _)| *i >= keep_from)
                        {
                            stderr_tail.drain(..idx);
                        }
                    }
                    if let Some(kind) = detect_password_prompt(&stderr_tail) {
                        let prompt_text = stderr_tail.trim().to_string();
                        let secret = match &ctx.capabilities.secret_prompt {
                            Some(prompt) => {
                                prompt
                                    .prompt(
                                        match kind {
                                            PromptKind::Sudo => {
                                                "sudo password required".to_string()
                                            }
                                            PromptKind::Password => "password required".to_string(),
                                        },
                                        prompt_text.clone(),
                                    )
                                    .await
                            }
                            None => None,
                        };
                        match secret {
                            Some(mut value) => {
                                let secret_value = value.clone();
                                if !secret_value.is_empty() {
                                    redactions.push(secret_value);
                                }
                                value.push('\n');
                                let write_result = stdin.write_all(value.as_bytes()).await;
                                let flush_result = stdin.flush().await;
                                // Zeroize the password from memory immediately after use
                                value.zeroize();
                                write_result.map_err(|e| RuntimeError::Tool(e.to_string()))?;
                                flush_result.map_err(|e| RuntimeError::Tool(e.to_string()))?;
                            }
                            None => {
                                let _ = child.kill().await;
                                return Err(RuntimeError::Tool(
                                    "Command canceled while waiting for password".to_string(),
                                ));
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
                            crate::truncate_str(&msg, remaining).to_string()
                        };
                        streamed_bytes += delta.len();
                        if !delta.is_empty() {
                            txd.send(delta);
                        }
                    }
                }

                if !added_all {
                    full_output.push_str(&format!("\n\n[output truncated at {}]", max_output));
                    if let Some(ref txd) = ctx.channels.tx_delta {
                        txd.send(format!("\n\n[output truncated at {}]", max_output));
                    }
                    truncated = true;
                    let _ = child.kill().await;
                }
            }
            let status = child
                .wait()
                .await
                .map_err(|e| RuntimeError::Tool(e.to_string()))?;
            // Zeroize redactions (passwords) from memory now that command is done
            for secret in &mut redactions {
                secret.zeroize();
            }
            Ok::<_, RuntimeError>((status, full_output, truncated))
        })
        .await;

        match result {
            Ok(Ok((status, output, was_truncated))) => {
                if status.success() || was_truncated {
                    Ok(output)
                } else {
                    Err(RuntimeError::Tool(format!(
                        "Command failed (exit {}):\n{}",
                        status.code().unwrap_or(-1),
                        output
                    )))
                }
            }
            Ok(Err(e)) => Err(RuntimeError::Tool(format!(
                "Failed to execute command: {}",
                e
            ))),
            Err(_) => Err(RuntimeError::Tool(format!(
                "Command timed out after {}s",
                timeout_secs
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bash_intermediary_handoff_conserves_bytes_under_large_output() {
        let before = bash_intermediary_snapshot();
        let tool = BashTool;
        let mut ctx = create_tool_context();
        ctx.limits.max_tool_buffer = 64 * 1024;
        let result = tool
            .execute(
                json!({
                    "command": "python3 -c \"import sys; sys.stdout.write('x' * 1048576)\"",
                    "timeout": 30
                }),
                ctx,
            )
            .await
            .unwrap();
        assert!(result.contains("output truncated"));

        // The counters are PROCESS-GLOBAL: sibling tests running bash
        // concurrently (--test-threads > 1) contribute mid-flight bytes to
        // any instantaneous snapshot. Conservation is therefore asserted at
        // quiescence: every completed relay balances produced == consumed +
        // dropped and returns retained to baseline, so the delta window
        // rebalances once in-flight relays finish. A REAL leak never
        // rebalances — the bounded poll keeps the oracle strict while
        // removing scheduling sensitivity (observed flake under the full
        // workspace run at 8 threads).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let (mut produced, mut consumed, mut accepted, mut dropped);
        loop {
            let after = bash_intermediary_snapshot();
            produced = after.produced_bytes - before.produced_bytes;
            consumed = after.consumed_bytes - before.consumed_bytes;
            accepted = after.accepted_bytes - before.accepted_bytes;
            dropped = after.dropped_bytes - before.dropped_bytes;
            let balanced =
                produced == consumed + dropped && after.retained_bytes == before.retained_bytes;
            if balanced || std::time::Instant::now() >= deadline {
                assert_eq!(
                    produced,
                    consumed + dropped,
                    "handoff bytes must be conserved at quiescence"
                );
                assert_eq!(after.retained_bytes, before.retained_bytes);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(accepted >= consumed);
        assert!(produced >= 64 * 1024);
    }

    #[test]
    fn detects_sudo_password_prompt_without_newline() {
        assert_eq!(
            detect_password_prompt("[sudo] password for me: "),
            Some(PromptKind::Sudo)
        );
    }

    #[test]
    fn sanitizes_terminal_control_sequences_and_nuls() {
        let cleaned = sanitize_output(b"ok\x1b[2J\x00done");
        assert_eq!(cleaned, "okdone");
    }

    use super::super::test_helpers::create_tool_context;
    use crate::tools::Tool;
    use serde_json::json;

    #[test]
    fn test_bash_tool_schema() {
        let tool = BashTool;
        assert_eq!(tool.name(), "bash");
        assert!(!tool.description().is_empty());

        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert!(params["required"].is_array());
    }

    #[tokio::test]
    async fn test_bash_tool_execution() {
        let tool = BashTool;

        // Test simple echo command
        let ctx = create_tool_context();
        let params = json!({
            "command": "echo hello"
        });

        let result = tool.execute(params, ctx).await.unwrap();
        assert!(result.contains("hello"));

        // Test timeout parameter with quick command
        let ctx = create_tool_context();
        let params = json!({
            "command": "sleep 1",
            "timeout": 2
        });

        let result = tool.execute(params, ctx).await;
        // Should succeed (1 second sleep with 2 second timeout)
        assert!(result.is_ok());

        // Test timeout with longer command
        let ctx = create_tool_context();
        let params = json!({
            "command": "sleep 3",
            "timeout": 1
        });

        let result = tool.execute(params, ctx).await;
        // Should timeout
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn test_bash_tool_requested_timeout_is_clamped_by_max_timeout() {
        let tool = BashTool;
        let mut ctx = create_tool_context();
        ctx.limits.bash_max_timeout = 1;

        let params = json!({
            "command": "sleep 5; echo done",
            "timeout": 10
        });

        let result = tool.execute(params, ctx).await;
        assert!(
            result.unwrap_err().to_string().contains("timed out"),
            "requested timeout MUST be clamped by bash_max_timeout"
        );
    }

    #[tokio::test]
    async fn test_bash_fake_sudo_prompt_uses_secret_prompt_and_redacts_password() {
        let tool = BashTool;
        let (prompt_tx, mut prompt_rx) = tokio::sync::mpsc::unbounded_channel();
        let prompt_handle = crate::tools::SecretPromptHandle::new(prompt_tx);
        let channel = crate::tools::output::delta_channel();
        let (delta_tx, mut delta_rx) = (channel.sender, channel.receiver);

        let responder = tokio::spawn(async move {
            let req = prompt_rx
                .recv()
                .await
                .expect("bash should request a secret prompt");
            assert!(
                req.prompt.to_ascii_lowercase().contains("password"),
                "prompt was {:?}",
                req.prompt
            );
            req.response_tx.send(Some("swordfish".to_string())).unwrap();
        });

        let mut ctx = create_tool_context();
        ctx.capabilities.secret_prompt = Some(prompt_handle);
        ctx.channels.tx_delta = Some(delta_tx);
        let params = json!({
            "command": "printf '[sudo] password for testuser: ' >&2; read -r pw; if [ \"$pw\" = swordfish ]; then echo AUTH_OK; else echo AUTH_FAIL; fi",
            "timeout": 30
        });

        let result = tool.execute(params, ctx).await.unwrap();
        responder.await.unwrap();
        let mut streamed = String::new();
        while let Some(delta) = delta_rx.try_drain() {
            streamed.push_str(&delta);
        }

        assert!(result.contains("AUTH_OK"));
        assert!(!result.contains("swordfish"));
        assert!(!result.contains("[sudo] password"));
        assert!(!streamed.contains("[sudo] password"));
    }

    #[test]
    fn test_bash_wraps_sudo_to_force_stdin_prompt() {
        let script = super::bash_script_with_secure_sudo("sudo id");

        assert!(script.contains("sudo()"));
        assert!(script.contains("command sudo -S -p '[sudo] password required: '"));
        assert!(script.ends_with("sudo id"));
    }

    /// Writes an executable stub `sudo` into `dir` that emulates the flags the
    /// secure wrapper passes (`-S -p PROMPT`): `-k` exits silently like a real
    /// timestamp reset, anything else writes PROMPT to stderr, consumes one
    /// stdin line, and fails. Lets the wrapper be tested without the system
    /// sudo, whose prompting behaviour is host-dependent.
    #[cfg(unix)]
    fn write_fake_sudo(dir: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let sudo_path = dir.join("sudo");
        std::fs::write(
            &sudo_path,
            r#"#!/bin/sh
prompt=""
while [ $# -gt 0 ]; do
  case "$1" in
    -S) shift ;;
    -p) prompt="$2"; shift 2 ;;
    -k) exit 0 ;;
    *) break ;;
  esac
done
printf '%s' "$prompt" >&2
read -r _pw
exit 1
"#,
        )
        .expect("write fake sudo");
        let mut perms = std::fs::metadata(&sudo_path)
            .expect("stat fake sudo")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&sudo_path, perms).expect("chmod fake sudo");
    }

    /// The `sudo()` wrapper's prompt is intercepted by the secret-prompt path
    /// and never reaches the delta stream.
    ///
    /// Hermetic by construction: it drives a STUB sudo on PATH, not the host's.
    /// `command sudo` bypasses the shell function but still honours PATH, so
    /// the real wrapper path is exercised. PATH is exported inside the script
    /// rather than through `std::env::set_var` because these tests are not
    /// `#[serial]` and mutating process-global env would race them.
    ///
    /// This previously drove the system sudo and asserted that it prompts —
    /// false on any host with passwordless sudo, which includes every GitHub
    /// runner, so it could never pass in CI.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_bash_sudo_function_prompt_is_intercepted_before_streaming() {
        let fake_bin = tempfile::tempdir().expect("tempdir");
        write_fake_sudo(fake_bin.path());
        let tool = BashTool;
        let (prompt_tx, mut prompt_rx) = tokio::sync::mpsc::unbounded_channel();
        let prompt_handle = crate::tools::SecretPromptHandle::new(prompt_tx);
        let channel = crate::tools::output::delta_channel();
        let (delta_tx, mut delta_rx) = (channel.sender, channel.receiver);

        let responder = tokio::spawn(async move {
            let req = prompt_rx
                .recv()
                .await
                .expect("bash should request a secret prompt");
            assert!(
                req.prompt.contains("[sudo] password required"),
                "prompt was {:?}",
                req.prompt
            );
            req.response_tx
                .send(Some("wrong-password-for-test".to_string()))
                .unwrap();
        });

        let mut ctx = create_tool_context();
        ctx.capabilities.secret_prompt = Some(prompt_handle);
        ctx.channels.tx_delta = Some(delta_tx);
        let params = json!({
            "command": format!(
                "export PATH=\"{}:$PATH\"; sudo -k; sudo -v",
                fake_bin.path().display()
            ),
            "timeout": 30
        });

        let _ = tool.execute(params, ctx).await;
        responder.await.unwrap();
        let mut streamed = String::new();
        while let Some(delta) = delta_rx.try_drain() {
            streamed.push_str(&delta);
        }

        assert!(
            !streamed.contains("[sudo] password required"),
            "sudo password prompt leaked into deltas: {streamed:?}"
        );
    }

    #[tokio::test]
    async fn test_bash_control_char_output_is_sanitized_and_bounded_in_deltas() {
        let tool = BashTool;
        let channel = crate::tools::output::delta_channel();
        let (delta_tx, mut delta_rx) = (channel.sender, channel.receiver);
        let mut ctx = create_tool_context();
        ctx.channels.tx_delta = Some(delta_tx);
        ctx.limits.max_tool_buffer = 256;

        let params = json!({
            "command": "python3 -c \"import sys; sys.stdout.buffer.write(b'clean\\x1b[2J\\x00' + b'A' * 2000); sys.stdout.flush()\"",
            "timeout": 30
        });

        let result = tool.execute(params, ctx).await.unwrap();
        let mut streamed = String::new();
        while let Some(delta) = delta_rx.try_drain() {
            streamed.push_str(&delta);
        }

        assert!(result.contains("[output truncated at 256]"));
        assert!(!result.contains('\u{1b}'));
        assert!(!result.contains('\0'));
        assert!(!streamed.contains('\u{1b}'));
        assert!(!streamed.contains('\0'));
        assert!(
            streamed.len() <= 2048,
            "streamed deltas must be bounded, got {} bytes",
            streamed.len()
        );
    }

    #[tokio::test]
    async fn test_bash_echoed_secret_is_redacted_from_output() {
        let tool = BashTool;
        let (prompt_tx, mut prompt_rx) = tokio::sync::mpsc::unbounded_channel();
        let prompt_handle = crate::tools::SecretPromptHandle::new(prompt_tx);

        let responder = tokio::spawn(async move {
            let req = prompt_rx
                .recv()
                .await
                .expect("bash should request a secret prompt");
            req.response_tx.send(Some("swordfish".to_string())).unwrap();
        });

        let mut ctx = create_tool_context();
        ctx.capabilities.secret_prompt = Some(prompt_handle);
        let params = json!({
            "command": "printf 'Password: ' >&2; read -r pw; echo seen:$pw",
            "timeout": 30
        });

        let result = tool.execute(params, ctx).await.unwrap();
        responder.await.unwrap();

        assert!(result.contains("seen:[redacted]"));
        assert!(!result.contains("swordfish"));
    }

    #[tokio::test]
    async fn test_bash_sequential_password_prompts_are_each_handled() {
        let tool = BashTool;
        let (prompt_tx, mut prompt_rx) = tokio::sync::mpsc::unbounded_channel();
        let prompt_handle = crate::tools::SecretPromptHandle::new(prompt_tx);

        let responder = tokio::spawn(async move {
            for value in ["first", "second"] {
                let req = prompt_rx
                    .recv()
                    .await
                    .expect("bash should request each secret prompt");
                assert!(req.prompt.to_ascii_lowercase().contains("password"));
                req.response_tx.send(Some(value.to_string())).unwrap();
            }
        });

        let mut ctx = create_tool_context();
        ctx.capabilities.secret_prompt = Some(prompt_handle);
        let params = json!({
            "command": "printf 'Password: ' >&2; read -r one; printf 'Password: ' >&2; read -r two; echo done:$one:$two",
            "timeout": 30
        });

        let result = tool.execute(params, ctx).await.unwrap();
        responder.await.unwrap();

        assert!(result.contains("done:[redacted]:[redacted]"));
        assert!(!result.contains("first"));
        assert!(!result.contains("second"));
    }

    #[tokio::test]
    async fn test_bash_password_prompt_cancel_kills_command_without_leaking_partial_secret() {
        let tool = BashTool;
        let (prompt_tx, mut prompt_rx) = tokio::sync::mpsc::unbounded_channel();
        let prompt_handle = crate::tools::SecretPromptHandle::new(prompt_tx);

        let responder = tokio::spawn(async move {
            let req = prompt_rx
                .recv()
                .await
                .expect("bash should request a secret prompt");
            req.response_tx.send(None).unwrap();
        });

        let mut ctx = create_tool_context();
        ctx.capabilities.secret_prompt = Some(prompt_handle);
        let params = json!({
            "command": "printf 'Password: ' >&2; read -r pw; echo should-not-run:$pw",
            "timeout": 30
        });

        let err = tool.execute(params, ctx).await.unwrap_err().to_string();
        responder.await.unwrap();

        assert!(err.contains("waiting for password"));
        assert!(!err.contains("should-not-run"));
    }

    #[tokio::test]
    async fn test_bash_binary_output_is_sanitized() {
        let tool = BashTool;
        let ctx = create_tool_context();
        let params = json!({
            "command": "python3 -c \"import sys; sys.stdout.buffer.write(bytes(range(32)) + b'visible')\"",
            "timeout": 30
        });

        let result = tool.execute(params, ctx).await.unwrap();

        assert!(result.contains("visible"));
        assert!(!result.contains('\0'));
        assert!(!result.contains('\u{1b}'));
    }

    #[tokio::test]
    async fn test_bash_tool_timeout() {
        let tool = BashTool;
        let ctx = create_tool_context();

        let params = json!({
            "command": "sleep 10",
            "timeout": 1
        });

        let result = tool.execute(params, ctx).await;

        // Should timeout and return error
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("timed out"));
    }

    #[tokio::test]
    async fn test_bash_tool_failure() {
        let tool = BashTool;
        let ctx = create_tool_context();

        let params = json!({
            "command": "exit 1"
        });

        let result = tool.execute(params, ctx).await;

        // Should fail and return error
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("failed") || error.contains("exit"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_honours_capability_cwd_when_set() {
        let tmp = tempfile::TempDir::new().unwrap();
        let expected = tmp.path().canonicalize().unwrap();
        let mut ctx = create_tool_context();
        ctx.capabilities.cwd = Some(tmp.path().to_path_buf());
        let out = BashTool
            .execute(json!({"command": "pwd -P"}), ctx)
            .await
            .unwrap();
        assert_eq!(out.trim(), expected.to_string_lossy());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_inherits_process_cwd_when_capability_cwd_is_none() {
        let expected = std::env::current_dir().unwrap().canonicalize().unwrap();
        let ctx = create_tool_context();
        assert!(ctx.capabilities.cwd.is_none());
        let out = BashTool
            .execute(json!({"command": "pwd -P"}), ctx)
            .await
            .unwrap();
        assert_eq!(out.trim(), expected.to_string_lossy());
    }
}
