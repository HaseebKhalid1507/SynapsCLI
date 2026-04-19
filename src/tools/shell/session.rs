//! Session manager — owns and manages active shell sessions.
//!
//! `SessionManager` is the core lifecycle engine for interactive PTY sessions.
//! It provides thread-safe creation, I/O, and cleanup of `ShellSession`s, each
//! backed by a `PtyHandle` and a `ReadinessDetector`.
//!
//! Design invariants:
//! - The `Mutex` is held **only** for HashMap insert/remove — never during I/O.
//! - Sessions are removed from the map before I/O and reinserted after, so the
//!   lock is never contended by blocking reads/writes.
//! - ANSI escape sequences are stripped from all output before returning.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::tools::strip_ansi;
use crate::{Result, RuntimeError};

use super::config::ShellConfig;
use super::pty::PtyHandle;
use super::readiness::{ReadinessDetector, ReadinessResult, ReadinessStrategy};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Thread-safe manager for all active shell sessions.
pub struct SessionManager {
    sessions: Mutex<HashMap<String, ShellSession>>,
    config: ShellConfig,
    next_id: AtomicU32,
}

/// A single shell session — PTY handle, readiness detector, and metadata.
struct ShellSession {
    pty: PtyHandle,
    detector: ReadinessDetector,
    created_at: Instant,
    last_active: Instant,
    idle_timeout: Duration,
    status: SessionStatus,
}

/// Lifecycle status of a session.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionStatus {
    Active,
    Exited(Option<i32>),
    Closed,
}

/// Options for creating a new session.
pub struct SessionOpts {
    pub command: Option<String>,
    pub working_directory: Option<String>,
    pub env: HashMap<String, String>,
    pub rows: Option<u16>,
    pub cols: Option<u16>,
    pub readiness_timeout_ms: Option<u64>,
    pub idle_timeout: Option<u64>,
}

/// Result of sending input to a session.
#[derive(Debug)]
pub struct SendResult {
    pub output: String,
    /// One of: `"active"`, `"exited"`, `"exited(N)"`, `"timeout"`
    pub status: String,
}

/// Snapshot of session metadata (no mutable borrows needed).
pub struct ShellSessionInfo {
    pub id: String,
    pub status: SessionStatus,
    pub created_at: Instant,
    pub last_active: Instant,
}

// ---------------------------------------------------------------------------
// Core readiness polling loop
// ---------------------------------------------------------------------------

/// Normalize PTY output: strip ANSI escapes and convert \r\n → \n.
fn normalize_output(raw: &str) -> String {
    strip_ansi(raw).replace("\r\n", "\n").replace('\r', "")
}

/// Process escape sequences in input strings from the model.
///
/// The model sometimes sends literal two-character sequences like `\n` instead
/// of actual control characters. This function converts common literal escapes
/// to their real byte values as a defense-in-depth measure.
fn process_input_escapes(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.peek() {
                Some('n') => { chars.next(); result.push('\n'); }
                Some('r') => { chars.next(); result.push('\r'); }
                Some('t') => { chars.next(); result.push('\t'); }
                Some('\\') => { chars.next(); result.push('\\'); }
                Some('a') => { chars.next(); result.push('\x07'); }  // bell
                Some('b') => { chars.next(); result.push('\x08'); }  // backspace
                Some('0') => { chars.next(); result.push('\0'); }    // null
                Some('e') => {
                    chars.next();
                    tracing::warn!("blocked \\e escape sequence (raw ESC) in shell input");
                }
                Some('x') => {
                    chars.next(); // consume 'x'
                    let mut hex = String::new();
                    for _ in 0..2 {
                        if let Some(&c) = chars.peek() {
                            if c.is_ascii_hexdigit() {
                                hex.push(c);
                                chars.next();
                            } else {
                                break;
                            }
                        }
                    }
                    if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                        if byte == 0x1b {
                            // Block ESC (ANSI escape initiator)
                            tracing::warn!("blocked \\x1b escape sequence (raw ESC) in shell input");
                        } else if byte >= 0x80 {
                            // Block high bytes
                            tracing::warn!("blocked \\x{hex:} high byte (>= 0x80) in shell input");
                        } else {
                            // Allow: 0x00-0x1a, 0x1c-0x1f (control chars except ESC), 0x20-0x7f
                            result.push(byte as char);
                        }
                    } else {
                        // Failed to parse — emit the original characters
                        result.push('\\');
                        result.push('x');
                        result.push_str(&hex);
                    }
                }
                _ => {
                    // Unknown escape — pass through literally
                    result.push(ch);
                }
            }
        } else {
            result.push(ch);
        }
    }
    result
}

/// Format a `SessionStatus` as a status string.
fn status_string(status: &SessionStatus) -> String {
    match status {
        SessionStatus::Active => "active".into(),
        SessionStatus::Exited(Some(code)) => format!("exited({code})"),
        SessionStatus::Exited(None) => "exited".into(),
        SessionStatus::Closed => "closed".into(),
    }
}

/// Wait for output from the PTY until the readiness detector signals completion.
///
/// Returns `(normalized_output, status_string)` where status is one of
/// `"active"`, `"exited"`, `"exited(N)"`, or `"timeout"`.
async fn wait_for_output(
    pty: &mut PtyHandle,
    detector: &ReadinessDetector,
    timeout_override: Option<u64>,
    tx_delta: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    max_output: usize,
) -> (String, String) {
    // If a timeout override is provided, build a temporary detector with that
    // silence timeout instead of using the session's detector.
    let override_detector;
    let effective_detector = if let Some(ms) = timeout_override {
        override_detector = ReadinessDetector::new(
            ReadinessStrategy::Hybrid,
            &[], // no prompt patterns — falls back to Timeout strategy
            ms,
            ms.saturating_mul(10).max(10_000), // reasonable max timeout
        );
        &override_detector
    } else {
        detector
    };

    let mut output = String::new();
    let start = Instant::now();
    let mut last_output_time = Instant::now();
    let poll_interval = Duration::from_millis(50);

    loop {
        // Try reading from PTY (async — waits up to poll_interval for data).
        let bytes = pty.try_read_output(poll_interval).await;

        if !bytes.is_empty() {
            let text = String::from_utf8_lossy(&bytes);
            output.push_str(&text);
            last_output_time = Instant::now();
            
            // Stream to TUI if requested (normalized)
            if let Some(tx) = tx_delta {
                let _ = tx.send(normalize_output(&text));
            }
        }

        // Check if output exceeds the max size — truncate and return early.
        if output.len() > max_output {
            output.truncate(max_output);
            return (normalize_output(&output), "active".into());
        }

        // Check if process exited.
        if !pty.is_alive() {
            // Drain any remaining buffered output.
            tokio::time::sleep(Duration::from_millis(50)).await;
            let remaining = pty.try_read_output(Duration::from_millis(100)).await;
            if !remaining.is_empty() {
                let remaining_text = String::from_utf8_lossy(&remaining);
                output.push_str(&remaining_text);
                
                // Stream remaining output to TUI
                if let Some(tx) = tx_delta {
                    let _ = tx.send(normalize_output(&remaining_text));
                }
            }
            // PtyHandle doesn't expose exit codes currently — use None
            return (normalize_output(&output), status_string(&SessionStatus::Exited(None)));
        }

        // Evaluate readiness.
        let silence_elapsed = last_output_time.elapsed();
        let total_elapsed = start.elapsed();

        match effective_detector.check(&output, silence_elapsed, total_elapsed) {
            ReadinessResult::Ready => return (normalize_output(&output), "active".into()),
            ReadinessResult::SilenceTimeout => return (normalize_output(&output), "active".into()),
            ReadinessResult::MaxTimeout => return (normalize_output(&output), "timeout".into()),
            ReadinessResult::Waiting => continue,
        }
    }
}

// ---------------------------------------------------------------------------
// SessionManager implementation
// ---------------------------------------------------------------------------

impl SessionManager {
    /// Create a new session manager backed by the given configuration.
    pub fn new(config: ShellConfig) -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            config,
            next_id: AtomicU32::new(0),
        })
    }

    /// Create a new interactive shell session.
    ///
    /// Returns `(session_id, initial_output, status)` on success.
    pub async fn create_session(
        &self, 
        opts: SessionOpts,
        tx_delta: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<(String, String, String)> {
        // --- Check session limit ---
        {
            let sessions = self.sessions.lock().map_err(|e| {
                RuntimeError::Tool(format!("session lock poisoned: {e}"))
            })?;
            if sessions.len() >= self.config.max_sessions {
                return Err(RuntimeError::Tool(format!(
                    "maximum session limit reached ({})",
                    self.config.max_sessions
                )));
            }
        }

        // --- Generate ID (shell_01, shell_02, …) ---
        let seq = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let id = format!("shell_{:02}", seq);

        // --- Resolve parameters with config defaults ---
        let command = opts.command.unwrap_or_else(|| {
            std::env::var("SHELL").unwrap_or_else(|_| "bash".into())
        });
        let rows = opts.rows.unwrap_or(self.config.default_rows);
        let cols = opts.cols.unwrap_or(self.config.default_cols);

        let idle_timeout = opts
            .idle_timeout
            .map(Duration::from_secs)
            .unwrap_or(self.config.idle_timeout);

        // --- Spawn PTY ---
        let mut pty = PtyHandle::spawn(
            &command,
            opts.working_directory.as_deref(),
            opts.env,
            rows,
            cols,
        )?;

        // --- Build readiness detector (with per-session overrides) ---
        let silence_ms = opts
            .readiness_timeout_ms
            .unwrap_or(self.config.readiness_timeout_ms);
        let detector = ReadinessDetector::new(
            super::readiness::ReadinessStrategy::Hybrid,
            &self.config.prompt_patterns,
            silence_ms,
            self.config.max_readiness_timeout_ms,
        );

        // --- Wait for initial output ---
        // Give the process a moment to start producing output before polling.
        // Without this, the silence timeout can fire before the process has
        // had time to print anything (e.g. Python startup, shell rc files).
        tokio::time::sleep(Duration::from_millis(200)).await;
        let (initial_output, status_str) =
            wait_for_output(&mut pty, &detector, opts.readiness_timeout_ms, tx_delta, 30000).await;

        let now = Instant::now();
        let status = if status_str.starts_with("exited") {
            SessionStatus::Exited(None)
        } else {
            SessionStatus::Active
        };

        let session = ShellSession {
            pty,
            detector,
            created_at: now,
            last_active: now,
            idle_timeout,
            status,
        };

        // --- Insert into map ---
        {
            let mut sessions = self.sessions.lock().map_err(|e| {
                RuntimeError::Tool(format!("session lock poisoned: {e}"))
            })?;
            sessions.insert(id.clone(), session);
        }

        Ok((id, initial_output, status_str))
    }

    /// Send input to an active session and return the output produced.
    pub async fn send_input(
        &self,
        id: &str,
        input: &str,
        timeout_ms: Option<u64>,
        tx_delta: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<SendResult> {
        // --- Remove session from map (release lock before I/O) ---
        let mut session = {
            let mut sessions = self.sessions.lock().map_err(|e| {
                RuntimeError::Tool(format!("session lock poisoned: {e}"))
            })?;
            sessions.remove(id).ok_or_else(|| {
                RuntimeError::Tool(format!(
                    "session {id} not found — it may have been closed, reaped, or is currently in use by another call"
                ))
            })?
        };

        // --- Reject if not active ---
        if session.status != SessionStatus::Active {
            // Reinsert so it can still be closed/inspected.
            let s_str = status_string(&session.status);
            let mut sessions = self.sessions.lock().map_err(|e| {
                RuntimeError::Tool(format!("session lock poisoned: {e}"))
            })?;
            sessions.insert(id.to_string(), session);
            return Err(RuntimeError::Tool(format!(
                "session {id} is not active (status: {s_str})"
            )));
        }

        // --- Write input (with escape sequence processing) ---
        let processed = process_input_escapes(input);
        session.pty.write(processed.as_bytes())?;

        // --- Wait for output ---
        let (output, status_str) =
            wait_for_output(&mut session.pty, &session.detector, timeout_ms, tx_delta, 30000).await;

        // --- Update metadata ---
        session.last_active = Instant::now();
        if !session.pty.is_alive() {
            session.status = SessionStatus::Exited(None);
        }

        let result = SendResult {
            output,
            status: status_str,
        };

        // --- Reinsert into map ---
        {
            let mut sessions = self.sessions.lock().map_err(|e| {
                RuntimeError::Tool(format!("session lock poisoned: {e}"))
            })?;
            sessions.insert(id.to_string(), session);
        }

        Ok(result)
    }

    /// Close a session, returning any final output.
    ///
    /// Idempotent — closing a non-existent session returns `Ok("")`.
    pub async fn close_session(&self, id: &str) -> Result<String> {
        let mut session = {
            let mut sessions = self.sessions.lock().map_err(|e| {
                RuntimeError::Tool(format!("session lock poisoned: {e}"))
            })?;
            match sessions.remove(id) {
                Some(s) => s,
                None => return Ok(String::new()),
            }
        };

        // Read remaining output with a short timeout.
        let remaining = session
            .pty
            .try_read_output(Duration::from_millis(100))
            .await;
        let final_output = if remaining.is_empty() {
            String::new()
        } else {
            strip_ansi(&String::from_utf8_lossy(&remaining))
        };

        // PtyHandle::drop will kill the child process.
        drop(session);

        Ok(final_output)
    }

    /// Reap sessions that have been idle beyond their timeout.
    ///
    /// Returns the IDs of sessions that were removed.
    pub fn reap_idle(&self) -> Vec<String> {
        let mut sessions = match self.sessions.lock() {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("session lock poisoned: {e}");
                return Vec::new();
            }
        };

        let grace_period = Duration::from_secs(5);

        let ids_to_reap: Vec<String> = sessions
            .iter()
            .filter(|(_, s)| {
                let elapsed = s.last_active.elapsed();
                elapsed > s.idle_timeout && elapsed > grace_period
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in &ids_to_reap {
            sessions.remove(id);
            // Dropped sessions clean up via PtyHandle::drop.
        }

        ids_to_reap
    }

    /// Shutdown all sessions immediately.
    pub fn shutdown_all(&self) {
        match self.sessions.lock() {
            Ok(mut sessions) => {
                sessions.drain();
                // All PtyHandles dropped — children killed.
            }
            Err(e) => {
                tracing::error!("session lock poisoned: {e}");
            }
        }
    }

    /// Number of sessions currently in the map.
    pub fn active_count(&self) -> usize {
        match self.sessions.lock() {
            Ok(s) => s.len(),
            Err(e) => {
                tracing::error!("session lock poisoned: {e}");
                0
            }
        }
    }

    /// Snapshot of all sessions.
    pub fn list_sessions(&self) -> Vec<ShellSessionInfo> {
        match self.sessions.lock() {
            Ok(sessions) => {
                sessions
                    .iter()
                    .map(|(id, s)| ShellSessionInfo {
                        id: id.clone(),
                        status: s.status.clone(),
                        created_at: s.created_at,
                        last_active: s.last_active,
                    })
                    .collect()
            }
            Err(e) => {
                tracing::error!("session lock poisoned: {e}");
                Vec::new()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Drop implementation — cleanup on drop
// ---------------------------------------------------------------------------

impl Drop for SessionManager {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

// ---------------------------------------------------------------------------
// Background reaper
// ---------------------------------------------------------------------------

/// Spawn a background task that periodically reaps idle sessions.
///
/// The reaper runs every 30 seconds, checking for sessions whose last activity
/// exceeds their configured idle timeout. Returns immediately — the task runs
/// until the `CancellationToken` is cancelled (or the process exits).
pub fn start_reaper(
    manager: Arc<SessionManager>,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let interval = Duration::from_secs(30);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(interval) => {
                    let reaped = manager.reap_idle();
                    for id in &reaped {
                        tracing::info!(session_id = %id, "reaped idle shell session");
                    }
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn default_manager() -> Arc<SessionManager> {
        SessionManager::new(ShellConfig::default())
    }

    fn opts_for(command: &str) -> SessionOpts {
        SessionOpts {
            command: Some(command.to_string()),
            working_directory: None,
            env: HashMap::new(),
            rows: None,
            cols: None,
            readiness_timeout_ms: None,
            idle_timeout: None,
        }
    }

    // 1. Create session with `echo hello` → output contains "hello"
    #[tokio::test]
    async fn test_create_session_echo_hello() {
        let mgr = default_manager();
        let (id, output, _status) = mgr
            .create_session(opts_for("echo hello"), None)
            .await
            .expect("failed to create session");

        assert!(id.starts_with("shell_"));
        assert!(
            output.contains("hello"),
            "expected 'hello' in output, got: {output:?}"
        );
    }

    // 2. Create bash session, send `echo test\n` → output contains "test"
    #[tokio::test]
    async fn test_send_input_echo() {
        let mgr = default_manager();
        let (id, _initial, _status) = mgr
            .create_session(opts_for("bash"), None)
            .await
            .expect("failed to create session");

        let result = mgr
            .send_input(&id, "echo test\n", None, None)
            .await
            .expect("failed to send input");

        assert!(
            result.output.contains("test"),
            "expected 'test' in output, got: {:?}",
            result.output
        );

        // Clean up
        let _ = mgr.close_session(&id).await;
    }

    // 3. Close session → idempotent (close twice is fine)
    #[tokio::test]
    async fn test_close_session_idempotent() {
        let mgr = default_manager();
        let (id, _, _status) = mgr
            .create_session(opts_for("bash"), None)
            .await
            .expect("failed to create session");

        let result1 = mgr.close_session(&id).await;
        assert!(result1.is_ok(), "first close should succeed");

        let result2 = mgr.close_session(&id).await;
        assert!(result2.is_ok(), "second close should also succeed (idempotent)");
        assert_eq!(result2.unwrap(), "", "second close returns empty string");
    }

    // 4. Max sessions limit → error on exceeding
    #[tokio::test]
    async fn test_max_sessions_limit() {
        let mut config = ShellConfig::default();
        config.max_sessions = 2;
        let mgr = SessionManager::new(config);

        let (id1, _, _s) = mgr
            .create_session(opts_for("bash"), None)
            .await
            .expect("session 1");
        let (id2, _, _s) = mgr
            .create_session(opts_for("bash"), None)
            .await
            .expect("session 2");

        let result = mgr.create_session(opts_for("bash"), None).await;
        assert!(result.is_err(), "third session should fail");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("maximum session limit"),
            "error should mention limit, got: {err_msg}"
        );

        // Clean up
        let _ = mgr.close_session(&id1).await;
        let _ = mgr.close_session(&id2).await;
    }

    // 5. Session not found → error
    #[tokio::test]
    async fn test_session_not_found() {
        let mgr = default_manager();
        let result = mgr.send_input("shell_99", "hello\n", None, None).await;
        assert!(result.is_err(), "send to non-existent session should fail");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("not found"),
            "error should mention 'not found', got: {err_msg}"
        );
    }

    // ── normalize_output tests ──

    #[test]
    fn test_normalize_output_crlf() {
        assert_eq!(normalize_output("hello\r\nworld\r\n"), "hello\nworld\n");
    }

    #[test]
    fn test_normalize_output_lone_cr() {
        assert_eq!(normalize_output("abc\rdef"), "abcdef");
    }

    // ── process_input_escapes tests ──

    #[test]
    fn test_escape_newline() {
        assert_eq!(process_input_escapes(r"hello\n"), "hello\n");
    }

    #[test]
    fn test_escape_tab() {
        assert_eq!(process_input_escapes(r"a\tb"), "a\tb");
    }

    #[test]
    fn test_escape_ctrl_c() {
        assert_eq!(process_input_escapes(r"\x03"), "\x03");
    }

    #[test]
    fn test_escape_ctrl_d() {
        assert_eq!(process_input_escapes(r"\x04"), "\x04");
    }

    #[test]
    fn test_escape_literal_backslash() {
        assert_eq!(process_input_escapes(r"a\\b"), "a\\b");
    }

    #[test]
    fn test_escape_real_newline_passthrough() {
        // If the model sends an actual newline (JSON parsed correctly), it passes through
        assert_eq!(process_input_escapes("hello\n"), "hello\n");
    }

    #[test]
    fn test_escape_mixed() {
        assert_eq!(process_input_escapes(r"ls -la\n"), "ls -la\n");
        assert_eq!(process_input_escapes(r"124\n"), "124\n");
    }

    #[test]
    fn test_escape_unknown_sequence() {
        // Unknown escapes pass through literally
        assert_eq!(process_input_escapes(r"\q"), "\\q");
    }

    #[test]
    fn test_escape_hex_partial() {
        // Incomplete hex — pass through
        assert_eq!(process_input_escapes(r"\xZZ"), "\\xZZ");
    }

    #[test]
    fn test_escape_bell() {
        assert_eq!(process_input_escapes(r"\a"), "\x07");
    }

    #[test]
    fn test_escape_backspace() {
        assert_eq!(process_input_escapes(r"\b"), "\x08");
    }

    #[test]
    fn test_escape_null() {
        assert_eq!(process_input_escapes(r"\0"), "\0");
    }

    #[test]
    fn test_escape_esc_blocked() {
        // \e should be blocked (produces empty string for that escape)
        assert_eq!(process_input_escapes(r"\e"), "");
    }

    #[test]
    fn test_escape_hex_1b_blocked() {
        // \x1b should be blocked
        assert_eq!(process_input_escapes(r"\x1b"), "");
    }

    #[test]
    fn test_escape_hex_high_byte_blocked() {
        // \x80 and above should be blocked
        assert_eq!(process_input_escapes(r"\x80"), "");
        assert_eq!(process_input_escapes(r"\xff"), "");
    }

    #[test]
    fn test_escape_hex_del_allowed() {
        // \x7f (DEL) should be allowed
        assert_eq!(process_input_escapes(r"\x7f"), "\x7f");
    }
}
