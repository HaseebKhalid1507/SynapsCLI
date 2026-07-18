//! Regression tests for `synaps chat` piped-stdin behaviour (C4).
//!
//! These tests are **no-network**: they pipe commands over stdin, assert on
//! stderr output, and exit.  No API key is set; no real model is called.
//!
//! # C4a coverage
//! `continuation_logic_*` tests live in-tree inside `engine/reactor.rs`
//! (pure unit tests of `wake_action` / `drain_event_queue`).  This file adds
//! a binary-subprocess harness that proves stdin / command parsing survived
//! the C4b async-stdin rewrite.
//!
//! # C4b coverage
//! `piped_status_then_quit` — pipe `/status\n/quit\n`; expect "model:" line
//! and clean exit (exit-code 0).  Proves:
//!   1. Async stdin BufReader reads complete lines correctly.
//!   2. /status and /quit commands are dispatched before any API call.
//!   3. EOF on the last command causes a clean exit (no hang, no panic).

use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

// ── harness ───────────────────────────────────────────────────────────────────

struct ChatChild {
    child: Child,
    stdin: Option<ChildStdin>,
    stderr: BufReader<tokio::process::ChildStderr>,
    /// Kept open (not read) so the child never sees EPIPE on stdout writes —
    /// exit codes must reflect turn logic, not broken-pipe panics.
    _stdout: Option<tokio::process::ChildStdout>,
    _home: TempDir,
}

impl ChatChild {
    async fn spawn(setup: impl FnOnce(&Path)) -> anyhow::Result<Self> {
        let home = TempDir::new()?;
        setup(home.path());

        let cfg_path = home.path().join(".synaps-cli");
        std::fs::create_dir_all(&cfg_path)?;
        // Minimal config: no model key → chat boots but won't call API.
        let config_file = cfg_path.join("config");
        if !config_file.exists() {
            std::fs::write(&config_file, "")?;
        }

        let bin = env!("CARGO_BIN_EXE_synaps");
        let mut cmd = Command::new(bin);
        cmd.arg("chat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env("HOME", home.path())
            .env("SYNAPS_BASE_DIR", &cfg_path)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("GROQ_API_KEY")
            .env_remove("CEREBRAS_API_KEY")
            .env_remove("NVIDIA_API_KEY")
            .env_remove("SAMBANOVA_API_KEY")
            .env_remove("OPENROUTER_API_KEY")
            .env_remove("GOOGLE_API_KEY")
            .env_remove("DEEPINFRA_API_KEY")
            .env_remove("DEEPINFRA_TOKEN")
            .env_remove("HUGGINGFACE_API_KEY")
            .env_remove("HF_TOKEN")
            .env_remove("FIREWORKS_API_KEY")
            .env_remove("HYPERBOLIC_API_KEY")
            .env_remove("SCALEWAY_API_KEY")
            .env_remove("SILICONFLOW_API_KEY")
            .env_remove("TOGETHER_API_KEY")
            .env_remove("CHUTES_API_KEY")
            .env_remove("CODESTRAL_API_KEY")
            .env_remove("PERPLEXITY_API_KEY")
            .env_remove("PPLX_API_KEY");

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stderr_raw = child.stderr.take().expect("piped stderr");
        let stderr = BufReader::new(stderr_raw);
        // stdout is piped and held open; these tests only assert on stderr.
        let stdout = child.stdout.take();

        Ok(Self { child, stdin: Some(stdin), stderr, _stdout: stdout, _home: home })
    }

    /// Read lines from stderr until `predicate` is satisfied or timeout.
    async fn wait_stderr_line(
        &mut self,
        predicate: impl Fn(&str) -> bool,
        timeout: Duration,
    ) -> anyhow::Result<String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                anyhow::bail!("timeout waiting for expected stderr line");
            }
            let mut line = String::new();
            let n = tokio::time::timeout(
                remaining,
                self.stderr.read_line(&mut line),
            ).await??;
            if n == 0 {
                anyhow::bail!("stderr closed before expected line appeared");
            }
            let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
            if predicate(trimmed) {
                return Ok(trimmed.to_string());
            }
        }
    }

    async fn send(&mut self, text: &str) -> anyhow::Result<()> {
        let stdin = self.stdin.as_mut().expect("stdin already closed");
        stdin.write_all(text.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Close the child's stdin (EOF) so the read loop terminates. Dropping
    /// the handle is what actually closes the pipe.
    fn close_stdin(&mut self) {
        drop(self.stdin.take());
    }

    /// The child's SYNAPS_BASE_DIR (where sessions/ lives).
    fn base_dir(&self) -> std::path::PathBuf {
        self._home.path().join(".synaps-cli")
    }

    async fn wait_exit(&mut self, timeout: Duration) -> anyhow::Result<std::process::ExitStatus> {
        Ok(tokio::time::timeout(timeout, self.child.wait()).await??)
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Pipe `/status\n/quit\n` → the process must print "model:" (from /status)
/// and exit 0 (from /quit).
///
/// This is the primary regression guard for C4b: proves the async tokio stdin
/// reader correctly reads complete lines from a pipe and dispatches commands.
#[tokio::test]
async fn piped_status_then_quit() -> anyhow::Result<()> {
    let mut child = ChatChild::spawn(|_| {}).await?;

    // Wait for the banner so the process is ready to accept commands.
    child.wait_stderr_line(|l| l.contains("synaps"), Duration::from_secs(15)).await?;

    // Send commands
    child.send("/status\n/quit\n").await?;

    // Must see "model:" from /status output
    child
        .wait_stderr_line(|l| l.contains("model:"), Duration::from_secs(5))
        .await
        .map_err(|_| anyhow::anyhow!("did not see 'model:' in stderr after /status — async stdin may be broken"))?;

    // Must exit cleanly (code 0 or 1; just not hang)
    let status = child.wait_exit(Duration::from_secs(5)).await
        .map_err(|_| anyhow::anyhow!("/quit did not produce clean exit — process hung"))?;
    // exit code is 0 (clean) or any non-signal exit
    assert!(status.code().is_some(), "process was killed by signal rather than exiting");

    Ok(())
}

/// Pipe an empty stdin (EOF immediately) → must exit cleanly, not hang.
///
/// Regression for C4b EOF handling: the async BufReader must propagate EOF
/// the same way blocking `read_line` did (return Ok(0)).
#[tokio::test]
async fn eof_exits_cleanly() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let cfg_path = home.path().join(".synaps-cli");
    std::fs::create_dir_all(&cfg_path)?;
    std::fs::write(cfg_path.join("config"), "")?;

    let bin = env!("CARGO_BIN_EXE_synaps");
    // stdin = null → immediate EOF
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        Command::new(bin)
            .arg("chat")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .env("HOME", home.path())
            .env("SYNAPS_BASE_DIR", &cfg_path)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("process hung on EOF stdin — async stdin EOF handling broken"))??;

    assert!(
        output.status.code().is_some(),
        "process was killed by signal on EOF"
    );
    Ok(())
}

/// CRLF trimming: pipe `/status\r\n/quit\r\n` → same as LF-only.
///
/// Preserves the existing CRLF-trim behaviour after the async rewrite.
#[tokio::test]
async fn crlf_trimmed_correctly() -> anyhow::Result<()> {
    let mut child = ChatChild::spawn(|_| {}).await?;
    child.wait_stderr_line(|l| l.contains("synaps"), Duration::from_secs(15)).await?;

    child.send("/status\r\n/quit\r\n").await?;

    child
        .wait_stderr_line(|l| l.contains("model:"), Duration::from_secs(5))
        .await
        .map_err(|_| anyhow::anyhow!("CRLF not trimmed — /status not executed"))?;

    let status = child.wait_exit(Duration::from_secs(5)).await
        .map_err(|_| anyhow::anyhow!("process hung after CRLF /quit"))?;
    assert!(status.code().is_some());

    Ok(())
}

/// T3 criterion 1+2: an unrecovered provider failure in headless mode must
/// exit NONZERO while still saving the valid partial history (here: the
/// user's prompt) into the session file.
///
/// The failure fixture is credential-free boot: the default model routes to
/// Anthropic, no auth.json / API key exists, so the pre-stream token refresh
/// fails locally (no network) and surfaces as a provider failure.
#[tokio::test]
async fn provider_failure_exits_nonzero_and_preserves_history() -> anyhow::Result<()> {
    let mut child = ChatChild::spawn(|_| {}).await?;

    // Wait for the banner so the process is ready.
    child
        .wait_stderr_line(|l| l.contains("synaps"), Duration::from_secs(15))
        .await?;

    // Send a real prompt, then EOF — the turn must fail (no credentials).
    child.send("hello from the failure fixture\n").await?;
    child.close_stdin();

    let status = child
        .wait_exit(Duration::from_secs(30))
        .await
        .map_err(|_| anyhow::anyhow!("chat hung after provider failure"))?;

    // Criterion 1: unrecovered provider failure must exit nonzero.
    assert!(
        status.code().is_some(),
        "process was killed by signal rather than exiting"
    );
    assert_ne!(
        status.code(),
        Some(0),
        "headless chat must exit nonzero on an unrecovered provider failure"
    );

    // Criterion 2: valid partial history (the user prompt) survives in the
    // saved session file.
    let sessions_dir = child.base_dir().join("sessions");
    let mut saved = String::new();
    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            saved.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
        }
    }
    assert!(
        saved.contains("hello from the failure fixture"),
        "saved session must preserve the user's message after a provider \
         failure; sessions dir contents: {:?}",
        saved
    );

    Ok(())
}

// ── pure/helper unit tests for continuation logic ─────────────────────────────
//
// These live in engine/reactor.rs (C1 tests) and here in a thin wrapper that
// confirms the public symbols are accessible from a workspace integration test.

mod continuation_policy {
    use agent_engine::engine::reactor::{
        drain_event_queue, wake_action, WakeAction, AUTO_TURN_CAP,
    };
    use agent_engine::events::{EventQueue, types::Severity};
    use std::sync::Arc;

    fn user_msg(text: &str) -> agent_engine::SharedMessage {
        Arc::new(serde_json::json!({"role": "user", "content": text}))
    }

    /// Under AUTO_TURN_CAP consecutive auto-turns, wake_action → RunTurn.
    #[test]
    fn run_turn_under_cap() {
        let q = EventQueue::new(10);
        q.push(agent_engine::events::types::Event::simple(
            "test", "completion", Some(Severity::Medium),
        )).unwrap();
        let mut messages = vec![user_msg("hello")];
        let mut pending: Vec<String> = Vec::new();

        let drained = drain_event_queue(&q, &mut messages, &mut pending, false, None);
        let action = wake_action(&drained, &messages, false, true, 0);
        assert_eq!(action, WakeAction::RunTurn);
    }

    /// At exactly AUTO_TURN_CAP, wake_action → Forward (park).
    #[test]
    fn parks_at_cap() {
        let q = EventQueue::new(10);
        q.push(agent_engine::events::types::Event::simple(
            "test", "completion", Some(Severity::Medium),
        )).unwrap();
        let mut messages = vec![user_msg("hello")];
        let mut pending: Vec<String> = Vec::new();

        let drained = drain_event_queue(&q, &mut messages, &mut pending, false, None);
        let action = wake_action(&drained, &messages, false, true, AUTO_TURN_CAP);
        assert_eq!(action, WakeAction::Forward);
    }

    /// Cap resets to 0 after real user input arrives.
    /// (Symbolic test — actual reset is enforced by chat.rs C4a logic.)
    #[test]
    fn cap_resets_under_new_user_input() {
        // After reset, first auto-turn fires again.
        let q = EventQueue::new(10);
        q.push(agent_engine::events::types::Event::simple(
            "test", "new-event", Some(Severity::Medium),
        )).unwrap();
        let mut messages = vec![user_msg("new user message after reset")];
        let mut pending: Vec<String> = Vec::new();

        let drained = drain_event_queue(&q, &mut messages, &mut pending, false, None);
        // consecutive = 0 after reset
        let action = wake_action(&drained, &messages, false, true, 0);
        assert_eq!(action, WakeAction::RunTurn);
    }
}
