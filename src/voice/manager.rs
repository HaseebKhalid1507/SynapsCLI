//! Voice sidecar lifecycle and IO.
//!
//! `VoiceManager` spawns a sidecar process, writes line-JSON
//! [`SidecarCommand`] values to its stdin, and surfaces the
//! deserialized [`SidecarEvent`] stream as higher-level
//! [`VoiceManagerEvent`] values on an mpsc channel.
//!
//! The actual mic/STT runtime lives in the sidecar (see
//! `synaps-skills/local-voice-plugin`); this module is intentionally
//! small and dependency-free beyond `tokio` + `serde_json`.

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
pub enum VoiceManagerEvent {
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
pub enum VoiceManagerError {
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
/// Construct via [`VoiceManager::spawn`]; drive with [`press`],
/// [`release`], [`shutdown`]. Receive events with [`next_event`].
pub struct VoiceManager {
    child: Option<Child>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    rx: mpsc::Receiver<VoiceManagerEvent>,
    reader_handle: Option<tokio::task::JoinHandle<()>>,
    stderr_handle: Option<tokio::task::JoinHandle<()>>,
}

impl VoiceManager {
    /// Spawn `bin` with `args`, send the [`Init`] handshake, and start
    /// the background reader task.
    ///
    /// [`Init`]: SidecarCommand::Init
    pub async fn spawn(
        bin: &Path,
        args: &[String],
        config: SidecarConfig,
    ) -> Result<Self, VoiceManagerError> {
        let mut command = Command::new(bin);
        command
            .args(args.iter().map(OsStr::new))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|source| VoiceManagerError::Spawn {
            bin: bin.display().to_string(),
            source,
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or(VoiceManagerError::PipesUnavailable)?;
        let stdout = child
            .stdout
            .take()
            .ok_or(VoiceManagerError::PipesUnavailable)?;
        let stderr = child.stderr.take();

        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let stdin = Arc::new(Mutex::new(Some(stdin)));

        // Reader task: parse line-JSON events and forward as VoiceManagerEvent.
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
                            .send(VoiceManagerEvent::Error(format!(
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
                    } => Some(VoiceManagerEvent::Ready {
                        protocol_version,
                        extension,
                        capabilities,
                    }),
                    SidecarEvent::Status { state, .. } => {
                        Some(VoiceManagerEvent::StateChanged(state))
                    }
                    SidecarEvent::ListeningStarted => Some(VoiceManagerEvent::ListeningStarted),
                    SidecarEvent::ListeningStopped => Some(VoiceManagerEvent::ListeningStopped),
                    SidecarEvent::TranscribingStarted => {
                        Some(VoiceManagerEvent::TranscribingStarted)
                    }
                    SidecarEvent::PartialTranscript { text } => {
                        Some(VoiceManagerEvent::PartialTranscript(text))
                    }
                    SidecarEvent::FinalTranscript { text } => {
                        Some(VoiceManagerEvent::FinalTranscript(text))
                    }
                    SidecarEvent::Error { message } => Some(VoiceManagerEvent::Error(message)),
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
            let _ = event_tx.send(VoiceManagerEvent::Exited).await;
        });

        // Stderr task: forward sidecar stderr to tracing for diagnostics.
        let stderr_handle = stderr.map(|stderr| {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "voice::sidecar", "{line}");
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

    /// Send a `voice_control_pressed` toggle-on command.
    pub async fn press(&mut self) -> Result<(), VoiceManagerError> {
        self.send(SidecarCommand::VoiceControlPressed).await
    }

    /// Send a `voice_control_released` toggle-off command.
    pub async fn release(&mut self) -> Result<(), VoiceManagerError> {
        self.send(SidecarCommand::VoiceControlReleased).await
    }

    /// Send a graceful `shutdown` command and reap the child process.
    pub async fn shutdown(&mut self) -> Result<(), VoiceManagerError> {
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
    pub async fn next_event(&mut self) -> Option<VoiceManagerEvent> {
        self.rx.recv().await
    }

    async fn send(&self, cmd: SidecarCommand) -> Result<(), VoiceManagerError> {
        let mut buf = serde_json::to_vec(&cmd)?;
        buf.push(b'\n');
        let mut guard = self.stdin.lock().await;
        let stdin = guard.as_mut().ok_or(VoiceManagerError::AlreadyShutDown)?;
        stdin.write_all(&buf).await?;
        stdin.flush().await?;
        Ok(())
    }
}

impl Drop for VoiceManager {
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
