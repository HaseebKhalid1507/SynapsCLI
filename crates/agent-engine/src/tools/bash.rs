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

impl BashIntermediarySnapshot {
    /// Every byte ever produced has been consumed or dropped: nothing is in
    /// flight anywhere in the process. A leaked byte keeps this false forever.
    pub fn is_settled(&self) -> bool {
        self.produced_bytes == self.consumed_bytes + self.dropped_bytes
    }
}

/// Counters are read downstream-first. Each one only grows, and upstream
/// counts are bumped before the bytes move on, so a snapshot torn by a
/// concurrent relay can overstate in-flight bytes but never understate them:
/// `is_settled` is never true unless the process really was settled.
pub fn bash_intermediary_snapshot() -> BashIntermediarySnapshot {
    use std::sync::atomic::Ordering::SeqCst;
    let consumed = BASH_INTERMEDIARY_CONSUMED_BYTES.load(SeqCst);
    let dropped = BASH_INTERMEDIARY_DROPPED_BYTES.load(SeqCst);
    let accepted = BASH_INTERMEDIARY_ACCEPTED_BYTES.load(SeqCst);
    let produced = BASH_INTERMEDIARY_PRODUCED_BYTES.load(SeqCst);
    BashIntermediarySnapshot {
        produced_bytes: produced,
        accepted_bytes: accepted,
        consumed_bytes: consumed,
        dropped_bytes: dropped,
        retained_bytes: produced.saturating_sub(consumed).saturating_sub(dropped),
    }
}

/// Test oracle for the handoff counters: wait until the whole process is
/// settled. Concurrent relays only delay it; a leaked byte times it out.
#[cfg(test)]
async fn wait_handoff_settled() -> BashIntermediarySnapshot {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let now = bash_intermediary_snapshot();
        if now.is_settled() {
            return now;
        }
        assert!(std::time::Instant::now() < deadline, "handoff bytes never settled: {now:?}");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

pub struct BashTool;

/// One sanitized read in flight between a stream reader and the executor.
/// Its bytes are counted out exactly once: `consume` when the executor takes
/// it, or `Drop` when it never arrives (the channel closed before the send,
/// or it was still queued when the receiver went away on timeout or cancel).
/// Tokio drops queued values with the channel, so no path can leak a count.
struct HandoffChunk(Option<String>);

impl HandoffChunk {
    fn new(text: String) -> Self {
        BASH_INTERMEDIARY_PRODUCED_BYTES
            .fetch_add(text.len() as u64, std::sync::atomic::Ordering::SeqCst);
        Self(Some(text))
    }

    fn len(&self) -> usize {
        self.0.as_ref().map_or(0, String::len)
    }

    fn consume(mut self) -> String {
        let text = self.0.take().unwrap_or_default();
        BASH_INTERMEDIARY_CONSUMED_BYTES
            .fetch_add(text.len() as u64, std::sync::atomic::Ordering::SeqCst);
        text
    }
}

impl Drop for HandoffChunk {
    fn drop(&mut self) {
        if let Some(text) = self.0.take() {
            BASH_INTERMEDIARY_DROPPED_BYTES
                .fetch_add(text.len() as u64, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

const READ_CHUNK_SIZE: usize = 1024;
const MAX_STREAMED_DELTA_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptKind {
    Sudo,
    Password,
}

fn sanitize_output(input: &str) -> String {
    let stripped = strip_ansi(input);
    stripped
        .chars()
        .filter(|ch| {
            *ch == '\n' || *ch == '\r' || *ch == '\t' || (!ch.is_control() && *ch != '\u{7f}')
        })
        .collect()
}

/// Read one child stream in `READ_CHUNK_SIZE` chunks and forward the
/// sanitized text to the bash executor, tagged stdout/stderr. A character
/// split between two reads is held back and decoded whole (#286); only an
/// incomplete sequence (at most 3 bytes) waits, so prompts without a
/// trailing newline are still forwarded as soon as they are read.
async fn forward_output<R>(
    mut reader: R,
    is_stderr: bool,
    tx: tokio::sync::mpsc::Sender<(bool, HandoffChunk)>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; READ_CHUNK_SIZE];
    let mut decoder = super::utf8_stream::Utf8StreamDecoder::new();
    loop {
        let (text, eof) = match reader.read(&mut buf).await {
            Ok(0) | Err(_) => (decoder.finish(), true),
            Ok(n) => (decoder.decode(&buf[..n]), false),
        };
        let msg = sanitize_output(&text);
        if !msg.is_empty() {
            let chunk = HandoffChunk::new(msg);
            // Count acceptance before the send makes the chunk visible, so
            // accepted >= consumed holds at every instant. A closed channel
            // drops the chunk here, and its Drop counts it.
            let Ok(permit) = tx.reserve().await else {
                break;
            };
            BASH_INTERMEDIARY_ACCEPTED_BYTES
                .fetch_add(chunk.len() as u64, std::sync::atomic::Ordering::SeqCst);
            permit.send((is_stderr, chunk));
        }
        if eof {
            break;
        }
    }
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

/// Kills the command's whole process group unless the command finished on
/// its own. `child.kill()` stops only the shell: everything the command
/// started (a test runner's workers, `cmd &` jobs) used to keep running, and
/// keep using memory, after a timeout or cancel (S348). The command leads its
/// own group: `setsid` in `pre_exec`, and `systemd-run --scope` execs in
/// place, so the pid is the group id. Dropped before the `Child`, so the
/// leader is not reaped by Child drop before the kill. Explicit kill paths
/// signal/disarm first; normal wait disarms synchronously on completion.
struct KillGroupUnlessFinished {
    pgid: Option<u32>,
}

impl KillGroupUnlessFinished {
    fn new(pid: Option<u32>) -> Self {
        Self { pgid: pid }
    }

    /// The command exited by itself: nothing to kill.
    fn finished(&mut self) {
        self.pgid = None;
    }

    /// Signal and disarm BEFORE any explicit kill/wait can reap the leader.
    fn kill_now(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid.take() {
            // SAFETY: killpg(2) on our own child's process group.
            unsafe {
                libc::killpg(pgid as libc::pid_t, libc::SIGKILL);
            }
        }
        #[cfg(not(unix))]
        {
            self.pgid = None;
        }
    }
}

impl Drop for KillGroupUnlessFinished {
    fn drop(&mut self) {
        self.kill_now();
    }
}

/// Which shell backend the shared executor drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellSpec {
    Bash,
    PowerShell,
}

impl ShellSpec {
    /// Names the command's systemd scope (`systemctl --user list-units --type=scope`).
    fn scope_description(self) -> &'static str {
        match self {
            ShellSpec::Bash => "synaps tool: bash",
            ShellSpec::PowerShell => "synaps tool: powershell",
        }
    }
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
        let effective_env = ctx.capabilities.env.as_ref()
            .map(|env| env.iter().map(|(k, v)| (k.into(), v.into())).collect())
            .unwrap_or_else(crate::process_scope::inherited_env);
        let probe_env = effective_env.clone();
        let probe_cwd = ctx.capabilities.cwd.clone();
        // This worker can only launch a bounded throwaway probe. Abandoning
        // this future can never launch the real command from the worker.
        if ctx.capabilities.launch_cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
            return Err(RuntimeError::Tool("Command canceled before launch".into()));
        }
        let probe_task = tokio::task::spawn_blocking(move || {
            crate::process_scope::launcher_for_effective_env(
                &probe_env, probe_cwd.as_deref())
        });
        let launcher = if let Some(cancel) = &ctx.capabilities.launch_cancel {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(RuntimeError::Tool("Command canceled before launch".into())),
                result = probe_task => result,
            }
        } else {
            probe_task.await
        }.map_err(|e| RuntimeError::Tool(e.to_string()))?;
        let (program, args) = match launcher {
            Some(launcher) => launcher.wrap(spec.scope_description(),
                crate::process_scope::resolve_program(program, &effective_env,
                    ctx.capabilities.cwd.as_deref()).map_err(|e| RuntimeError::Tool(e.to_string()))?, args),
            None => (program, args),
        };
        let mut cmd = tokio::process::Command::new(program);
        if let Some(cwd) = ctx.capabilities.cwd.as_deref() {
            cmd.current_dir(cwd);
        }
        cmd.env_clear().envs(effective_env);
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

        if ctx.capabilities.launch_cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
            return Err(RuntimeError::Tool("Command canceled before launch".into()));
        }
        let mut child = cmd.spawn().map_err(|e| RuntimeError::Tool(e.to_string()))?;
        // Armed until the command finishes on its own: a timeout, a cancel,
        // the output cap or an abandoned password prompt kills the whole
        // process group, not just the shell.
        let mut group = KillGroupUnlessFinished::new(child.id());

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
            tokio::sync::mpsc::channel::<(bool, HandoffChunk)>(BASH_INTERMEDIARY_CHANNEL_CAPACITY);

        tokio::spawn(forward_output(stdout, false, tx_inter.clone()));
        tokio::spawn(forward_output(stderr, true, tx_inter.clone()));

        drop(tx_inter);

        let result = tokio::time::timeout(tokio::time::Duration::from_secs(timeout_secs), async {
            use tokio::io::AsyncWriteExt;

            let mut stdin = stdin;
            let mut full_output = String::new();
            let mut stderr_tail = String::new();
            let mut truncated = false;
            let mut streamed_bytes = 0usize;
            let mut redactions: Vec<String> = Vec::new();

            while let Some((is_stderr, chunk)) = rx_inter.recv().await {
                let mut msg = chunk.consume();
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
                                group.kill_now();
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
                    group.kill_now();
                    let _ = child.kill().await;
                }
            }
            let status = child.wait().await;
            // wait errors may mean the leader was already reaped externally;
            // never signal a potentially recycled process-group ID afterwards.
            group.finished();
            let status = status.map_err(|e| RuntimeError::Tool(e.to_string()))?;
            // Finished on its own: leave anything it deliberately started in
            // the background (`server &`) running, as before.
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
                    let mut notice = String::new();
                    // T5: when a command fails and references a stripped secret,
                    // append a notice (once per name per session).
                    if !ctx.capabilities.env_stripped.is_empty() {
                        let mut warned = ctx.capabilities.env_warned.lock().unwrap_or_else(|p| p.into_inner());
                        for name in &ctx.capabilities.env_stripped {
                            if warned.contains(name) {
                                continue;
                            }
                            // Word-boundary match: covers $NAME, ${NAME},
                            // ${NAME:?}, ${NAME:-x}, $env:NAME and NAME=…
                            if references_env_name(script, name) {
                                notice.push_str(&format!(
                                    "\nnote: ${name} was stripped from the session env as a secret; \
                                     reattach from a shell that has it \
                                     (synaps --system … from that shell) \
                                     — the daemon never receives credentials."
                                ));
                                warned.insert(name.clone());
                            }
                        }
                    }
                    Err(RuntimeError::Tool(format!(
                        "Command failed (exit {}):\n{}{notice}",
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

/// Does `script` mention `name` as a whole identifier (not as a substring of
/// a longer one)? Used by the T5 stripped-secret notice; a false positive
/// costs one advisory line on an already-failing command, a false negative
/// hides the reason the command failed — so err towards matching.
fn references_env_name(script: &str, name: &str) -> bool {
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut from = 0;
    while let Some(pos) = script[from..].find(name) {
        let start = from + pos;
        let end = start + name.len();
        let before_ok = !script[..start].chars().next_back().is_some_and(is_ident);
        let after_ok = !script[end..].chars().next().is_some_and(is_ident);
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_child_kills_signal_and_disarm_group_before_reaping() {
        let source = include_str!("bash.rs");
        let executor = source.split("pub(crate) async fn run_shell_command").nth(1).unwrap()
            .split("#[cfg(test)]").next().unwrap();
        let kills: Vec<_> = executor.match_indices("let _ = child.kill().await;").collect();
        assert_eq!(kills.len(), 2, "audit every new explicit kill path");
        for (offset, _) in kills {
            assert!(executor[..offset].trim_end().ends_with("group.kill_now();"));
        }
        assert!(executor.contains("let status = child.wait().await;"));
        let wait = executor.split("let status = child.wait().await;").nth(1).unwrap();
        assert!(wait.find("group.finished();").unwrap() < wait.find("status.map_err").unwrap());
        assert!(executor.find("let mut child = cmd.spawn()").unwrap()
            < executor.find("let mut group = KillGroupUnlessFinished::new").unwrap());
    }

    #[tokio::test]
    async fn canceled_launch_never_runs_payload() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("must-not-exist");
        let token = crate::CancellationToken::new();
        token.cancel();
        let mut ctx = create_tool_context();
        ctx.capabilities.launch_cancel = Some(token);
        let error = BashTool.execute(json!({"command": format!("touch '{}'", marker.display())}), ctx)
            .await.unwrap_err().to_string();
        assert!(error.contains("canceled before launch"), "{error}");
        assert!(!marker.exists());
    }

    fn own_cgroup() -> String {
        std::fs::read_to_string("/proc/self/cgroup")
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    /// The command runs in its own systemd scope when the machine supports
    /// it, so an out-of-memory kill aimed at it cannot take the daemon's
    /// cgroup down with it (S348). Without a reachable user manager (CI
    /// containers), the command must still run, in this process's cgroup.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bash_command_gets_its_own_scope_when_available() {
        if std::env::var("SYNAPS_TEST_REAL_SCOPES").as_deref() != Ok("1") { return; }
        let out = BashTool
            .execute(
                json!({ "command": "cat /proc/self/cgroup; echo pid=$$", "timeout": 30 }),
                create_tool_context(),
            )
            .await
            .unwrap();
        let child_cgroup = out.lines().next().unwrap_or_default().trim().to_string();
        assert!(child_cgroup.starts_with("0::/"), "cgroup v2 line: {out}");
        if crate::process_scope::launcher_for_env(None).is_some() {
            assert_ne!(
                child_cgroup,
                own_cgroup(),
                "the command must leave our cgroup"
            );
            assert!(child_cgroup.ends_with(".scope"), "{child_cgroup}");
            assert!(child_cgroup.contains("/app.slice/"), "{child_cgroup}");
        } else {
            assert_eq!(child_cgroup, own_cgroup(), "unwrapped fallback");
        }
    }

    /// A session environment rebuilt without the user-bus variables cannot
    /// reach systemd: the command starts unwrapped instead of failing.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bash_command_with_a_rebuilt_env_lacking_the_user_bus_runs_unwrapped() {
        let mut ctx = create_tool_context();
        ctx.capabilities.env = Some(vec![(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        )]);
        let out = BashTool
            .execute(
                json!({ "command": "cat /proc/self/cgroup", "timeout": 30 }),
                ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.lines().next().unwrap_or_default().trim(), own_cgroup());
    }

    fn processes_named(name: &str) -> Vec<String> {
        let out = std::process::Command::new("pgrep")
            .args(["-f", name])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect()
    }

    /// A timed-out command takes everything it started with it, not just the
    /// shell (S348: a timed-out test run would leave all its workers running).
    /// Each child is renamed to the marker (`exec -a`), so a survivor is seen.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bash_timeout_kills_the_whole_process_group() {
        let marker = format!("synaps-timeout-child-{}", std::process::id());
        let out = BashTool
            .execute(
                json!({
                    "command": format!("(exec -a {marker} sleep 300) & (exec -a {marker} sleep 300); wait"),
                    "timeout": 1
                }),
                create_tool_context(),
            )
            .await;
        let text = match out {
            Ok(s) => s,
            Err(e) => e.to_string(),
        };
        assert!(text.contains("timed out"), "{text}");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !processes_named(&marker).is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let left = processes_named(&marker);
        if !left.is_empty() {
            let _ = std::process::Command::new("pkill")
                .args(["-f", &marker])
                .status();
        }
        assert!(left.is_empty(), "children survived the timeout: {left:?}");
    }

    /// A command that exits by itself keeps what it deliberately started in
    /// the background, as before: only abnormal endings kill the group.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bash_normal_exit_leaves_a_background_job_running() {
        let marker = format!("synaps-background-job-{}", std::process::id());
        let out = BashTool
            .execute(
                json!({
                    "command": format!("(exec -a {marker} sleep 30) >/dev/null 2>&1 & echo started"),
                    "timeout": 10
                }),
                create_tool_context(),
            )
            .await
            .unwrap();
        assert!(out.contains("started"), "{out}");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let alive = processes_named(&marker);
        let _ = std::process::Command::new("pkill")
            .args(["-f", &marker])
            .status();
        assert!(
            !alive.is_empty(),
            "a deliberate background job must survive a normal exit"
        );
    }

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

        // The counters are process-global and sibling tests run bash
        // concurrently, so a before/after delta is skewed by whatever a
        // sibling had mid-channel when `before` was read, and that skew
        // never rebalances. Assert the invariant that does hold: once
        // in-flight relays drain, every byte ever produced in this process
        // was consumed or dropped. Siblings only delay this; a leaked byte
        // keeps it false forever.
        let after = wait_handoff_settled().await;
        assert_eq!(after.retained_bytes, 0);
        assert!(after.accepted_bytes >= after.consumed_bytes);
        assert!(after.produced_bytes - before.produced_bytes >= 64 * 1024);
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
        let cleaned = sanitize_output("ok\x1b[2J\x00done");
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

    #[test]
    fn references_env_name_is_word_bounded() {
        assert!(references_env_name("echo ${GH_TOKEN:?}", "GH_TOKEN"));
        assert!(references_env_name("echo $GH_TOKEN", "GH_TOKEN"));
        assert!(references_env_name("GH_TOKEN=x gh auth", "GH_TOKEN"));
        assert!(references_env_name("$env:GH_TOKEN", "GH_TOKEN"));
        assert!(!references_env_name("echo $GH_TOKEN_FILE", "GH_TOKEN"));
        assert!(!references_env_name("echo $MY_GH_TOKEN", "GH_TOKEN"));
        assert!(!references_env_name("gh auth status", "GH_TOKEN"));
    }

    /// T5 notice: non-zero exit + script references stripped var → notice.
    #[cfg(unix)]
    #[tokio::test]
    async fn bash_notice_on_stripped_secret_reference() {
        let warned = std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashSet::<String>::new(),
        ));
        // --- case 1: non-zero exit + reference → notice
        let mut ctx = create_tool_context();
        ctx.capabilities.env_stripped = vec!["GH_TOKEN".into()];
        ctx.capabilities.env_warned = warned.clone();
        let err = BashTool
            .execute(json!({"command": "echo ${GH_TOKEN:?} 2>&1; exit 1"}), ctx)
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("note: $GH_TOKEN was stripped"),
            "notice must name the var: {msg}"
        );

        // --- case 2: zero exit → no notice
        let mut ctx2 = create_tool_context();
        ctx2.capabilities.env_stripped = vec!["GH_TOKEN".into()];
        ctx2.capabilities.env_warned = warned.clone();
        let out = BashTool
            .execute(json!({"command": "echo $GH_TOKEN; exit 0"}), ctx2)
            .await
            .unwrap();
        assert!(
            !out.contains("note:"),
            "zero exit must not produce a notice: {out}"
        );

        // --- case 3: second failure → no duplicate (same Arc)
        let mut ctx3 = create_tool_context();
        ctx3.capabilities.env_stripped = vec!["GH_TOKEN".into()];
        ctx3.capabilities.env_warned = warned.clone();
        let err2 = BashTool
            .execute(json!({"command": "echo ${GH_TOKEN}; exit 1"}), ctx3)
            .await
            .unwrap_err();
        let msg2 = format!("{err2}");
        assert!(
            !msg2.contains("note: $GH_TOKEN was stripped"),
            "dedup must suppress the second notice: {msg2}"
        );
    }
}

/// Issue #286: a multi-byte character split across two reads must come back
/// whole, not as U+FFFD.
#[cfg(test)]
mod utf8_chunk_tests {
    use super::*;
    use crate::tools::Tool;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Yields exactly the given chunks, one per `read`, then EOF.
    struct ChunkedReader(VecDeque<Vec<u8>>);

    impl tokio::io::AsyncRead for ChunkedReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if let Some(chunk) = self.0.pop_front() {
                assert!(chunk.len() <= buf.remaining(), "test chunk larger than read buffer");
                buf.put_slice(&chunk);
            }
            Poll::Ready(Ok(()))
        }
    }

    async fn forward_chunks(chunks: Vec<Vec<u8>>, is_stderr: bool) -> String {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        forward_output(ChunkedReader(chunks.into()), is_stderr, tx).await;
        let mut out = String::new();
        while let Some((stderr, chunk)) = rx.recv().await {
            let msg = chunk.consume();
            assert_eq!(stderr, is_stderr);
            out.push_str(&msg);
        }
        out
    }

    #[tokio::test]
    async fn characters_split_at_every_offset_survive_both_streams() {
        for ch in ['é', '▀', '😀'] {
            let text = format!("a{ch}b{ch}{ch}c");
            let bytes = text.as_bytes();
            for split in 1..bytes.len() {
                for is_stderr in [false, true] {
                    let out = forward_chunks(
                        vec![bytes[..split].to_vec(), bytes[split..].to_vec()],
                        is_stderr,
                    )
                    .await;
                    assert!(!out.contains('\u{FFFD}'), "{ch:?} split at {split}: {out:?}");
                    assert_eq!(out.as_bytes(), bytes, "{ch:?} split at {split}");
                }
            }
        }
    }

    #[tokio::test]
    async fn one_byte_reads_reassemble_every_character() {
        let text = "é▀😀 mixed ascii ✓ 日本語";
        let chunks = text.bytes().map(|b| vec![b]).collect();
        assert_eq!(forward_chunks(chunks, false).await, text);
    }

    #[tokio::test]
    async fn genuinely_invalid_bytes_still_become_replacement_characters() {
        assert_eq!(forward_chunks(vec![b"ok\xffok".to_vec()], false).await, "ok\u{FFFD}ok");
        // A lead byte followed by a non-continuation byte in the next read.
        assert_eq!(
            forward_chunks(vec![b"a\xe2".to_vec(), b"Xb".to_vec()], false).await,
            "a\u{FFFD}Xb"
        );
        // A stray continuation byte at the start of a read.
        assert_eq!(
            forward_chunks(vec![b"a".to_vec(), b"\x80b".to_vec()], false).await,
            "a\u{FFFD}b"
        );
    }

    #[tokio::test]
    async fn incomplete_sequence_at_eof_is_flushed_not_dropped() {
        assert_eq!(forward_chunks(vec![b"ab\xe2\x96".to_vec()], false).await, "ab\u{FFFD}");
        assert_eq!(
            forward_chunks(vec![b"ab".to_vec(), b"\xf0\x9f".to_vec()], true).await,
            "ab\u{FFFD}"
        );
    }

    /// A consumer that gives up (timeout, cancelled password prompt) drops
    /// the receiver with chunks still queued. Those bytes must be counted as
    /// dropped, or the process-wide accounting never settles again.
    #[tokio::test]
    async fn chunks_still_queued_when_the_receiver_goes_away_count_as_dropped() {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let chunks: Vec<Vec<u8>> = (0..8).map(|_| b"queued".to_vec()).collect();
        forward_output(ChunkedReader(chunks.into()), false, tx).await;
        let before = bash_intermediary_snapshot();
        drop(rx);
        let after = wait_handoff_settled().await;
        assert!(after.dropped_bytes - before.dropped_bytes >= 8 * 6);
    }

    /// A password prompt has no trailing newline; it must be forwarded as soon
    /// as it is read, not held back waiting for more output or EOF.
    #[tokio::test]
    async fn prompt_without_newline_is_forwarded_before_eof() {
        let (mut writer, reader) = tokio::io::duplex(4096);
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let task = tokio::spawn(forward_output(reader, true, tx));
        use tokio::io::AsyncWriteExt;
        writer.write_all("[sudo] password for x: ".as_bytes()).await.unwrap();
        let (is_stderr, chunk) =
            tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("prompt must arrive without EOF")
                .unwrap();
        let msg = chunk.consume();
        assert!(is_stderr);
        assert_eq!(detect_password_prompt(&msg), Some(PromptKind::Sudo), "{msg:?}");
        drop(writer);
        task.await.unwrap();
    }

    /// Through the real tool: more than 1 KiB of 3-byte characters on both
    /// streams. 1024 is not a multiple of 3, so the reads split characters.
    #[tokio::test]
    async fn bash_tool_output_over_1kib_keeps_multibyte_characters_whole() {
        let channel = crate::tools::output::delta_channel();
        let (delta_tx, mut delta_rx) = (channel.sender, channel.receiver);
        let mut ctx = super::super::test_helpers::create_tool_context();
        ctx.channels.tx_delta = Some(delta_tx);
        let result = BashTool
            .execute(json!({"command": "printf '▀%.0s' {1..2000}", "timeout": 30}), ctx)
            .await
            .unwrap();
        let mut streamed = String::new();
        while let Some(delta) = delta_rx.try_drain() {
            streamed.push_str(&delta);
        }
        let expected = "▀".repeat(2000);
        assert!(!result.contains('\u{FFFD}'), "result has U+FFFD");
        assert_eq!(result, expected);
        assert!(!streamed.contains('\u{FFFD}'), "deltas have U+FFFD");
        assert!(expected.starts_with(&streamed) && !streamed.is_empty());

        let ctx = super::super::test_helpers::create_tool_context();
        let err = BashTool
            .execute(
                json!({"command": "python3 -c \"import sys; sys.stderr.write(('▀'*119+'\\n')*45)\"; exit 3", "timeout": 30}),
                ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(!err.contains('\u{FFFD}'), "stderr has U+FFFD: {err}");
        assert!(err.contains(&("▀".repeat(119) + "\n").repeat(45)), "{err}");
    }
}
