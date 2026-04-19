//! PTY abstraction — spawn processes on a pseudo-terminal, async read/write.
//!
//! Wraps `portable-pty` to provide an async-friendly handle with:
//! - Spawning commands on a PTY (with cwd, env, size)
//! - Non-blocking reads via a background reader thread + mpsc channel
//! - Synchronous writes to the PTY master
//! - Resize support
//! - Alive-check and graceful cleanup on Drop

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system, Child, ChildKiller};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::{Result, RuntimeError};

/// Async-friendly wrapper around a PTY master/child pair.
///
/// The reader runs on a blocking Tokio thread and pushes raw byte chunks
/// into an unbounded mpsc channel. Consumers drain the channel via
/// `try_read_output()`.
pub struct PtyHandle {
    /// Master PTY — retained for resize operations.
    master: Box<dyn MasterPty + Send>,
    /// Writer end of the PTY master (bytes written here reach the child's stdin).
    writer: Box<dyn Write + Send>,
    /// Handle to the blocking reader task (for cleanup tracking).
    _reader_task: JoinHandle<()>,
    /// Receiving end of the output channel fed by the reader task.
    output_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    /// Child process handle — used for try_wait / kill.
    child: Box<dyn Child + Send + Sync>,
    /// Cached alive flag — once the child exits, this stays false.
    alive: Arc<AtomicBool>,
    /// Separate killer handle so Drop can kill even if child is borrowed.
    killer: Box<dyn ChildKiller + Send + Sync>,
}

impl PtyHandle {
    /// Spawn a command on a new PTY.
    ///
    /// # Arguments
    /// * `command` — the program (and optional arguments) to run, e.g. `"bash"` or `"ssh user@host"`.
    /// * `working_dir` — optional working directory for the child process.
    /// * `env` — additional environment variables (merged on top of inherited env).
    /// * `rows` / `cols` — initial terminal dimensions.
    pub fn spawn(
        command: &str,
        working_dir: Option<&str>,
        env: HashMap<String, String>,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        // 1. Open a PTY pair with the requested size.
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| RuntimeError::Tool(format!("Failed to open PTY: {e}")))?;

        // 2. Build the command.
        //    We split on whitespace for simple cases ("bash -l", "ssh user@host").
        let parts: Vec<&str> = command.split_whitespace().collect();
        let program = parts
            .first()
            .ok_or_else(|| RuntimeError::Tool("Empty command string".to_string()))?;
        let mut cmd = CommandBuilder::new(program);
        for arg in parts.iter().skip(1) {
            cmd.arg(arg);
        }

        // Set working directory if provided.
        if let Some(dir) = working_dir {
            cmd.cwd(dir);
        }

        // Inject environment variables; always set TERM.
        cmd.env("TERM", "xterm-256color");
        for (k, v) in &env {
            cmd.env(k, v);
        }

        // 3. Spawn the child on the slave side.
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| RuntimeError::Tool(format!("Failed to spawn command: {e}")))?;

        // Drop the slave — the child process owns its end now.
        drop(pair.slave);

        // 4. Obtain writer and reader from the master.
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| RuntimeError::Tool(format!("Failed to take PTY writer: {e}")))?;

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| RuntimeError::Tool(format!("Failed to clone PTY reader: {e}")))?;

        // 5. Spawn a blocking reader task that pushes chunks into the channel.
        let (output_tx, output_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let alive = Arc::new(AtomicBool::new(true));
        let reader_alive = alive.clone();

        let reader_task = tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        // EOF — child closed its side.
                        break;
                    }
                    Ok(n) => {
                        if output_tx.send(buf[..n].to_vec()).is_err() {
                            // Receiver dropped — no one is listening anymore.
                            break;
                        }
                    }
                    Err(_) => {
                        // Read error (child exited, fd closed, etc.) — exit cleanly.
                        break;
                    }
                }
            }
            reader_alive.store(false, Ordering::SeqCst);
        });

        // 6. Clone a killer for Drop usage.
        let killer = child.clone_killer();

        Ok(PtyHandle {
            master: pair.master,
            writer,
            _reader_task: reader_task,
            output_rx,
            child,
            alive,
            killer,
        })
    }

    /// Write raw bytes to the PTY (reaches the child's stdin).
    pub fn write(&mut self, input: &[u8]) -> Result<()> {
        self.writer
            .write_all(input)
            .map_err(|e| RuntimeError::Tool(format!("PTY write failed: {e}")))?;
        self.writer
            .flush()
            .map_err(|e| RuntimeError::Tool(format!("PTY flush failed: {e}")))?;
        Ok(())
    }

    /// Read all available output from the PTY, waiting up to `timeout` for data.
    ///
    /// Behavior:
    /// 1. Drain everything currently in the channel (non-blocking).
    /// 2. If nothing was found, wait up to `timeout` for the first chunk.
    /// 3. After getting something (or timing out), drain any remaining buffered data.
    ///
    /// Returns an empty `Vec` if no data arrived within the timeout.
    pub async fn try_read_output(&mut self, timeout: Duration) -> Vec<u8> {
        let mut collected = Vec::new();

        // Phase 1: non-blocking drain of everything already queued.
        while let Ok(chunk) = self.output_rx.try_recv() {
            collected.extend_from_slice(&chunk);
        }

        // Phase 2: if we got nothing, wait up to `timeout` for at least one chunk.
        if collected.is_empty() {
            match tokio::time::timeout(timeout, self.output_rx.recv()).await {
                Ok(Some(chunk)) => {
                    collected.extend_from_slice(&chunk);
                }
                Ok(None) | Err(_) => {
                    // Channel closed or timeout — return whatever we have (empty).
                    return collected;
                }
            }

            // Phase 3: drain any additional chunks that arrived while we waited.
            while let Ok(chunk) = self.output_rx.try_recv() {
                collected.extend_from_slice(&chunk);
            }
        }

        collected
    }

    /// Resize the PTY to new dimensions.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| RuntimeError::Tool(format!("PTY resize failed: {e}")))
    }

    /// Check whether the child process is still running.
    ///
    /// Once the child exits, subsequent calls return `false` without syscalls.
    pub fn is_alive(&mut self) -> bool {
        if !self.alive.load(Ordering::SeqCst) {
            return false;
        }
        match self.child.try_wait() {
            Ok(Some(_status)) => {
                // Child exited.
                self.alive.store(false, Ordering::SeqCst);
                false
            }
            Ok(None) => true,
            Err(_) => {
                // If we can't query, assume dead.
                self.alive.store(false, Ordering::SeqCst);
                false
            }
        }
    }
}

impl Drop for PtyHandle {
    fn drop(&mut self) {
        if self.alive.load(Ordering::SeqCst) {
            let _ = self.killer.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_spawn_echo_hello() {
        let mut handle = PtyHandle::spawn(
            "echo hello",
            None,
            HashMap::new(),
            24,
            80,
        )
        .expect("failed to spawn echo");

        // Give the process time to produce output and exit.
        let output = handle
            .try_read_output(Duration::from_secs(3))
            .await;

        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("hello"),
            "expected 'hello' in output, got: {text:?}"
        );
    }

    #[tokio::test]
    async fn test_cat_echo_back() {
        let mut handle = PtyHandle::spawn(
            "cat",
            None,
            HashMap::new(),
            24,
            80,
        )
        .expect("failed to spawn cat");

        // Write input — cat will echo it back via the PTY.
        handle.write(b"test\n").expect("write failed");

        let output = handle
            .try_read_output(Duration::from_secs(3))
            .await;

        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("test"),
            "expected 'test' in output, got: {text:?}"
        );
    }

    #[tokio::test]
    async fn test_exit_code_detection() {
        let mut handle = PtyHandle::spawn(
            "bash -c exit 42",
            None,
            HashMap::new(),
            24,
            80,
        )
        .expect("failed to spawn bash exit");

        // Wait for the process to finish — read until EOF / timeout.
        let _ = handle
            .try_read_output(Duration::from_secs(3))
            .await;

        // Small additional delay to let try_wait catch up.
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(
            !handle.is_alive(),
            "expected process to have exited"
        );
    }
}
