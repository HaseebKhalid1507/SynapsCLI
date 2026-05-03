//! Sidecar lifecycle and IO.
//!
//! [`SidecarManager`] spawns a sidecar process, writes line-JSON
//! [`SidecarCommand`] values to its stdin, and surfaces the
//! deserialized [`SidecarEvent`] stream as higher-level
//! [`SidecarLifecycleEvent`] values on an mpsc channel.
//!
//! Modality-agnostic. The actual per-modality runtime (mic/STT, OCR,
//! agent, etc.) lives in the plugin process; this module is
//! intentionally small and dependency-free beyond `tokio` +
//! `serde_json`.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, Mutex};

use super::protocol::{
    SidecarCapability, SidecarCommand, SidecarConfig, SidecarEvent, SidecarProviderState,
};

const EVENT_CHANNEL_CAPACITY: usize = 64;

/// High-level events emitted by the manager. This is a curated subset
/// of [`SidecarEvent`] tailored for the chatui consumer; raw protocol
/// events that aren't actionable yet (e.g. `BargeIn`) are dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarLifecycleEvent {
    /// Sidecar handshake complete; STT is available.
    Ready {
        protocol_version: u16,
        extension: String,
        capabilities: Vec<SidecarCapability>,
    },
    /// Sidecar reports a state transition.
    StateChanged(SidecarProviderState),
    ListeningStarted,
    ListeningStopped,
    TranscribingStarted,
    PartialTranscript(String),
    /// Final transcript ready to insert into the input buffer.
    FinalTranscript(String),
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
    #[error("sidecar process has already shut down")]
    AlreadyShutDown,
    #[error("failed to encode sidecar command: {0}")]
    Encode(#[from] serde_json::Error),
}

/// Supervises one sidecar process and its line-JSON streams.
///
/// Construct via [`SidecarManager::spawn`]; drive with [`press`],
/// [`release`], [`shutdown`]. Receive events with [`next_event`].
pub struct SidecarManager {
    child: Option<Child>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    rx: mpsc::Receiver<SidecarLifecycleEvent>,
    reader_handle: Option<tokio::task::JoinHandle<()>>,
    stderr_handle: Option<tokio::task::JoinHandle<()>>,
}

impl SidecarManager {
    /// Spawn `bin` with `args`, send the [`Init`] handshake, and start
    /// the background reader task.
    ///
    /// [`Init`]: SidecarCommand::Init
    pub async fn spawn(
        bin: &Path,
        args: &[String],
        config: SidecarConfig,
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

        let stdin = child
            .stdin
            .take()
            .ok_or(SidecarError::PipesUnavailable)?;
        let stdout = child
            .stdout
            .take()
            .ok_or(SidecarError::PipesUnavailable)?;
        let stderr = child.stderr.take();

        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let stdin = Arc::new(Mutex::new(Some(stdin)));

        // Reader task: parse line-JSON events and forward as SidecarLifecycleEvent.
        let event_tx = tx.clone();
        let reader_handle = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let event = match serde_json::from_str::<SidecarEvent>(&line) {
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
                    SidecarEvent::Hello {
                        protocol_version,
                        extension,
                        capabilities,
                    } => Some(SidecarLifecycleEvent::Ready {
                        protocol_version,
                        extension,
                        capabilities,
                    }),
                    SidecarEvent::Status { state, .. } => {
                        Some(SidecarLifecycleEvent::StateChanged(state))
                    }
                    SidecarEvent::ListeningStarted => Some(SidecarLifecycleEvent::ListeningStarted),
                    SidecarEvent::ListeningStopped => Some(SidecarLifecycleEvent::ListeningStopped),
                    SidecarEvent::TranscribingStarted => {
                        Some(SidecarLifecycleEvent::TranscribingStarted)
                    }
                    SidecarEvent::PartialTranscript { text } => {
                        Some(SidecarLifecycleEvent::PartialTranscript(text))
                    }
                    SidecarEvent::FinalTranscript { text } => {
                        Some(SidecarLifecycleEvent::FinalTranscript(text))
                    }
                    SidecarEvent::Error { message } => Some(SidecarLifecycleEvent::Error(message)),
                    // Reserved for future capabilities; drop silently.
                    SidecarEvent::VoiceCommand { .. } | SidecarEvent::BargeIn => None,
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
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "sidecar::manager", "{line}");
                }
            })
        });

        let manager = Self {
            child: Some(child),
            stdin,
            rx,
            reader_handle: Some(reader_handle),
            stderr_handle,
        };

        manager.send(SidecarCommand::Init { config }).await?;
        Ok(manager)
    }

    /// Send a trigger-pressed command.
    pub async fn press(&mut self) -> Result<(), SidecarError> {
        self.send(SidecarCommand::TriggerPressed).await
    }

    /// Send a trigger-released command.
    pub async fn release(&mut self) -> Result<(), SidecarError> {
        self.send(SidecarCommand::TriggerReleased).await
    }

    /// Send a graceful `shutdown` command and reap the child process.
    pub async fn shutdown(&mut self) -> Result<(), SidecarError> {
        let _ = self.send(SidecarCommand::Shutdown).await;
        // Drop the stdin so the sidecar sees EOF if it ignored shutdown.
        if let Some(mut stdin) = self.stdin.lock().await.take() {
            let _ = stdin.shutdown().await;
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.wait().await;
        }
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.stderr_handle.take() {
            handle.abort();
        }
        Ok(())
    }

    /// Receive the next high-level event, or `None` if the channel
    /// closed (sidecar exited and reader task drained).
    pub async fn next_event(&mut self) -> Option<SidecarLifecycleEvent> {
        self.rx.recv().await
    }

    async fn send(&self, cmd: SidecarCommand) -> Result<(), SidecarError> {
        let mut buf = serde_json::to_vec(&cmd)?;
        buf.push(b'\n');
        let mut guard = self.stdin.lock().await;
        let stdin = guard.as_mut().ok_or(SidecarError::AlreadyShutDown)?;
        stdin.write_all(&buf).await?;
        stdin.flush().await?;
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
