//! Sidecar lifecycle and IO.
//!
//! [`SidecarManager`] spawns a sidecar process, writes line-JSON
//! [`SidecarCommand`] values to its stdin, and surfaces the
//! deserialized [`SidecarFrame`] stream as higher-level
//! [`SidecarLifecycleEvent`] values on an mpsc channel.
//!
//! Modality-agnostic. Plugin-specific work lives in the plugin process;
//! this module is intentionally small and dependency-free beyond `tokio`
//! and `serde_json`.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

/// Maximum bytes per sidecar stdout/stderr line. A hostile or buggy sidecar
/// sending a single unbounded line could OOM the host without this cap.
const MAX_SIDECAR_LINE_BYTES: u64 = 1024 * 1024; // 1 MiB, matches MCP

use super::protocol::{InsertTextMode, SidecarCommand, SidecarFrame, SIDECAR_PROTOCOL_VERSION};

const EVENT_CHANNEL_CAPACITY: usize = 64;
/// Bound every command frame, including Init, when a sidecar stops reading.
const COMMAND_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// High-level events emitted by the manager. This is a curated subset
/// of [`SidecarFrame`] tailored for chatui consumers; plugin-specific
/// frames that are not actionable by core are dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarLifecycleEvent {
    /// Sidecar handshake complete.
    Ready {
        protocol_version: u16,
        extension: String,
        capabilities: Vec<String>,
    },
    /// Sidecar reports a plugin-defined state transition.
    StateChanged {
        state: String,
        label: Option<String>,
    },
    /// Sidecar wants text applied to the input buffer.
    InsertText { text: String, mode: InsertTextMode },
    /// Sidecar reported an error message.
    Error(String),
    /// Sidecar process exited (clean or otherwise).
    Exited,
}

/// Errors surfaced by the manager.
#[derive(Debug, thiserror::Error)]
pub enum SidecarError {
    #[error("failed to spawn sidecar {bin}: {source}")]
    Spawn {
        bin: String,
        #[source]
        source: std::io::Error,
    },
    #[error("sidecar stdin/stdout was not captured")]
    PipesUnavailable,
    #[error("sidecar IO error: {0}")]
    Io(#[from] std::io::Error),
    /// No more commands can be sent (shutdown, failed write, or cancelled write).
    #[error("sidecar command input is closed")]
    AlreadyShutDown,
    #[error("failed to encode sidecar command: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("sidecar protocol error: {0}")]
    Protocol(String),
}

/// Supervises one sidecar process and its line-JSON streams.
///
/// Construct via [`SidecarManager::spawn`]; drive with [`press`],
/// [`release`], [`shutdown`]. Receive events with [`next_event`].
pub struct SidecarManager {
    hello_capabilities: Vec<String>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    rx: mpsc::Receiver<SidecarLifecycleEvent>,
    reader_handle: Option<tokio::task::JoinHandle<()>>,
    stderr_handle: Option<tokio::task::JoinHandle<()>>,
}

impl SidecarManager {
    /// Spawn `bin` with `args`, wait for Hello, send [`Init`], and start
    /// the background reader task. Hello is readiness; no post-Init status
    /// is required. Hello has a 10s deadline and command writes have a 2s
    /// deadline. Cancelling startup drops the child and reader tasks.
    ///
    /// [`Init`]: SidecarCommand::Init
    pub async fn spawn(
        bin: &Path,
        args: &[String],
        config: serde_json::Value,
    ) -> Result<Self, SidecarError> {
        let mut command = Command::new(bin);
        command
            .args(args.iter().map(OsStr::new))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|source| SidecarError::Spawn {
            bin: bin.display().to_string(),
            source,
        })?;

        let stdin = child.stdin.take().ok_or(SidecarError::PipesUnavailable)?;
        let stdout = child.stdout.take().ok_or(SidecarError::PipesUnavailable)?;
        let stderr = child.stderr.take();

        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);

        // Reader task: parse line-JSON events and forward as SidecarLifecycleEvent.
        let event_tx = tx.clone();
        let reader_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                let mut limited = (&mut reader).take(MAX_SIDECAR_LINE_BYTES + 1);
                match tokio::io::AsyncBufReadExt::read_line(&mut limited, &mut line).await {
                    Ok(0) => break, // EOF
                    Ok(_) => {}
                    Err(_) => break,
                }
                if line.len() as u64 > MAX_SIDECAR_LINE_BYTES {
                    let _ = event_tx
                        .send(SidecarLifecycleEvent::Error(format!(
                            "sidecar stdout line exceeded {MAX_SIDECAR_LINE_BYTES} bytes, dropped"
                        )))
                        .await;
                    continue;
                }
                let line = line.trim_end();
                if line.trim().is_empty() {
                    continue;
                }
                let event = match serde_json::from_str::<SidecarFrame>(line) {
                    Ok(ev) => ev,
                    Err(err) => {
                        let _ = event_tx
                            .send(SidecarLifecycleEvent::Error(format!(
                                "failed to parse sidecar line: {err}: {line}"
                            )))
                            .await;
                        continue;
                    }
                };
                let mapped = match event {
                    SidecarFrame::Hello {
                        protocol_version,
                        extension,
                        capabilities,
                    } => {
                        if protocol_version < SIDECAR_PROTOCOL_VERSION {
                            Some(SidecarLifecycleEvent::Error(format!(
                                "sidecar protocol v{protocol_version} is too old; host requires v{SIDECAR_PROTOCOL_VERSION}. Update the plugin via /plugins."
                            )))
                        } else {
                            Some(SidecarLifecycleEvent::Ready {
                                protocol_version,
                                extension,
                                capabilities,
                            })
                        }
                    }
                    SidecarFrame::Status { state, label, .. } => {
                        Some(SidecarLifecycleEvent::StateChanged { state, label })
                    }
                    SidecarFrame::InsertText { text, mode } => {
                        Some(SidecarLifecycleEvent::InsertText { text, mode })
                    }
                    SidecarFrame::Error { message } => Some(SidecarLifecycleEvent::Error(message)),
                    SidecarFrame::Custom => None,
                };
                if let Some(event) = mapped {
                    if event_tx.send(event).await.is_err() {
                        // Receiver dropped — give up.
                        break;
                    }
                }
            }
            let _ = event_tx.send(SidecarLifecycleEvent::Exited).await;
        });

        // Stderr task: forward sidecar stderr to tracing for diagnostics.
        let stderr_handle = stderr.map(|stderr| {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    let mut limited = (&mut reader).take(MAX_SIDECAR_LINE_BYTES + 1);
                    match tokio::io::AsyncBufReadExt::read_line(&mut limited, &mut line).await {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                    if line.len() as u64 > MAX_SIDECAR_LINE_BYTES {
                        tracing::warn!(target: "sidecar::manager", "stderr line exceeded bound, dropped");
                        continue;
                    }
                    tracing::debug!(target: "sidecar::manager", "{}", line.trim_end());
                }
            })
        });

        let mut manager = Self {
            hello_capabilities: Vec::new(),
            child: Some(child),
            stdin: Some(stdin),
            rx,
            reader_handle: Some(reader_handle),
            stderr_handle,
        };

        // Wait for the sidecar's Hello frame before sending Init.
        // The sidecar must announce its protocol version first so we can
        // reject incompatible versions before committing to the handshake.
        // Timeout: 10s — if the sidecar can't say Hello in 10s, it's broken.
        let hello_timeout =
            tokio::time::timeout(std::time::Duration::from_secs(10), manager.rx.recv())
                .await
                .map_err(|_| {
                    SidecarError::Protocol("sidecar did not send Hello within 10s".to_string())
                })?;

        match hello_timeout {
            Some(SidecarLifecycleEvent::Ready { capabilities, .. }) => {
                manager.hello_capabilities = capabilities;
            }
            Some(SidecarLifecycleEvent::Error(e)) => {
                return Err(SidecarError::Protocol(format!("sidecar Hello failed: {e}")));
            }
            Some(SidecarLifecycleEvent::Exited) | None => {
                return Err(SidecarError::Protocol(
                    "sidecar exited before sending Hello".to_string(),
                ));
            }
            Some(other) => {
                return Err(SidecarError::Protocol(format!(
                    "expected Hello from sidecar, got: {:?}",
                    other
                )));
            }
        }

        manager.send(SidecarCommand::Init { config }).await?;
        Ok(manager)
    }

    /// Optional initialization contract. Without it Hello+Init is the legacy
    /// protocol-ready boundary, not proof that model/device loading finished.
    pub fn ready_after_init(&self) -> bool {
        self.hello_capabilities
            .iter()
            .any(|c| c == "ready_after_init")
    }

    /// Nonblocking drain for hosts about to publish readiness or activate.
    pub fn try_next_event(&mut self) -> Option<SidecarLifecycleEvent> {
        match self.rx.try_recv() {
            Ok(event) => Some(event),
            Err(mpsc::error::TryRecvError::Disconnected) => Some(SidecarLifecycleEvent::Exited),
            Err(mpsc::error::TryRecvError::Empty) => None,
        }
    }

    /// Send a trigger press command.
    pub async fn press(&mut self) -> Result<(), SidecarError> {
        self.send(SidecarCommand::Trigger {
            name: "press".into(),
            payload: None,
        })
        .await
    }

    /// Send a trigger release command.
    pub async fn release(&mut self) -> Result<(), SidecarError> {
        self.send(SidecarCommand::Trigger {
            name: "release".into(),
            payload: None,
        })
        .await
    }

    /// Send a graceful `shutdown` command and reap the child process.
    /// A write error is returned only after cleanup; an already closed input
    /// still permits cleanup and repeated shutdown calls.
    pub async fn shutdown(&mut self) -> Result<(), SidecarError> {
        let send_result = match self.send(SidecarCommand::Shutdown).await {
            Err(SidecarError::AlreadyShutDown) => Ok(()),
            result => result,
        };
        // Closing the pipe is synchronous; no unbounded flush/shutdown await.
        // The sidecar sees EOF even if it ignored the shutdown command.
        drop(self.stdin.take());
        if let Some(mut child) = self.child.take() {
            // Grace period: wait up to 2s, then kill. Matches MCP/extension patterns.
            match tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await {
                Ok(_) => {} // exited gracefully
                Err(_) => {
                    let _ = child.kill().await;
                }
            }
        }
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.stderr_handle.take() {
            handle.abort();
        }
        send_result
    }

    /// Receive the next high-level event, or `None` if the channel
    /// closed (sidecar exited and reader task drained).
    pub async fn next_event(&mut self) -> Option<SidecarLifecycleEvent> {
        self.rx.recv().await
    }

    async fn send(&mut self, cmd: SidecarCommand) -> Result<(), SidecarError> {
        let mut buf = serde_json::to_vec(&cmd)?;
        buf.push(b'\n');
        // All callers hold &mut self. Take ownership across the await so an IO
        // error, timeout, or cancelled future closes stdin and leaves it None.
        // A partially written JSON frame must never be followed by a retry.
        let mut stdin = self.stdin.take().ok_or(SidecarError::AlreadyShutDown)?;
        tokio::time::timeout(COMMAND_WRITE_TIMEOUT, async {
            stdin.write_all(&buf).await?;
            stdin.flush().await
        })
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "sidecar command write did not complete within 2s; command input closed",
            )
        })??;
        self.stdin = Some(stdin);
        Ok(())
    }
}

impl Drop for SidecarManager {
    fn drop(&mut self) {
        // Best-effort: kill the child if shutdown wasn't called.
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.stderr_handle.take() {
            handle.abort();
        }
    }
}
