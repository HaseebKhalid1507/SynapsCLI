use crate::{Result, RuntimeError, VoiceEvent};
use std::sync::{Arc, Mutex};

use super::sidecar_host::VoiceSidecarHost;
use super::sidecar_protocol::{SidecarCommand, SidecarConfig, SidecarEvent, VoiceSidecarMode};
use super::types::{SpeechToTextProvider, VoiceEventSender};

pub fn sidecar_event_to_voice_event(event: SidecarEvent) -> Option<VoiceEvent> {
    match event {
        SidecarEvent::ListeningStarted => Some(VoiceEvent::ListeningStarted),
        SidecarEvent::ListeningStopped => Some(VoiceEvent::ListeningStopped),
        SidecarEvent::PartialTranscript { text } => Some(VoiceEvent::PartialTranscript(text)),
        SidecarEvent::FinalTranscript { text } => Some(VoiceEvent::FinalTranscript(text)),
        SidecarEvent::Error { message } => Some(VoiceEvent::Error(message)),
        SidecarEvent::VoiceCommand { command } => Some(VoiceEvent::FinalTranscript(command)),
        SidecarEvent::BargeIn => Some(VoiceEvent::ListeningStarted),
        SidecarEvent::Hello { .. }
        | SidecarEvent::Status { .. }
        | SidecarEvent::TranscribingStarted => None,
    }
}

pub fn voice_event_to_sidecar_event(event: VoiceEvent) -> Option<SidecarEvent> {
    match event {
        VoiceEvent::ListeningStarted => Some(SidecarEvent::ListeningStarted),
        VoiceEvent::ListeningStopped => Some(SidecarEvent::ListeningStopped),
        VoiceEvent::PartialTranscript(text) => Some(SidecarEvent::PartialTranscript { text }),
        VoiceEvent::FinalTranscript(text) => Some(SidecarEvent::FinalTranscript { text }),
        VoiceEvent::Error(message) => Some(SidecarEvent::Error { message }),
    }
}

pub fn sidecar_args_from_config(config: &crate::VoiceConfig) -> Vec<String> {
    if !config.sidecar_args.is_empty() {
        return config.sidecar_args.clone();
    }

    let mut args = Vec::new();
    args.push("--model-path".to_string());
    args.push(config.stt_model_path.to_string_lossy().to_string());

    let language = config.stt_language.trim();
    if !language.is_empty() && !language.eq_ignore_ascii_case("auto") {
        args.push("--language".to_string());
        args.push(language.to_string());
    }

    args
}

pub struct SidecarSttProvider {
    command: String,
    args: Vec<String>,
    language: String,
    protocol_version: u16,
    control_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<SidecarCommand>>>>,
    worker: Option<tokio::task::JoinHandle<()>>,
    running: bool,
}

impl SidecarSttProvider {
    pub fn new(command: impl Into<String>, args: Vec<String>, language: impl Into<String>, protocol_version: u16) -> Self {
        Self {
            command: command.into(),
            args,
            language: language.into(),
            protocol_version,
            control_tx: Arc::new(Mutex::new(None)),
            worker: None,
            running: false,
        }
    }

    pub fn from_config(config: &crate::VoiceConfig) -> Self {
        Self::new(
            config.sidecar_command.clone(),
            sidecar_args_from_config(config),
            config.stt_language.clone(),
            config.sidecar_protocol_version,
        )
    }

    pub fn sidecar_command_sender(&self) -> Option<tokio::sync::mpsc::UnboundedSender<SidecarCommand>> {
        self.control_tx.lock().ok().and_then(|guard| guard.clone())
    }

    pub fn sidecar_command_sender_handle(&self) -> Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<SidecarCommand>>>> {
        self.control_tx.clone()
    }

    fn abort_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
        if let Ok(mut control_tx) = self.control_tx.lock() {
            *control_tx = None;
        }
    }
}

impl SpeechToTextProvider for SidecarSttProvider {
    fn start(&mut self, events: VoiceEventSender) -> Result<()> {
        if self.running {
            return Ok(());
        }

        self.abort_worker();
        let command = self.command.clone();
        let args = self.args.clone();
        let language = self.language.clone();
        let protocol_version = self.protocol_version;
        let (control_tx, mut control_rx) = tokio::sync::mpsc::unbounded_channel::<SidecarCommand>();

        let worker = tokio::spawn(async move {
            tracing::debug!(command = %command, args = ?args, "starting voice sidecar process");
            let mut host = match VoiceSidecarHost::spawn(&command, &args).await {
                Ok(host) => host,
                Err(err) => {
                    let _ = events.try_send(VoiceEvent::Error(err.to_string()));
                    return;
                }
            };

            let init = SidecarCommand::Init {
                config: SidecarConfig {
                    mode: VoiceSidecarMode::Dictation,
                    language: Some(language),
                    protocol_version,
                },
            };
            if let Err(err) = host.send(init).await {
                let _ = events.try_send(VoiceEvent::Error(err.to_string()));
                let _ = host.shutdown().await;
                return;
            }
            if let Err(err) = host.send(SidecarCommand::VoiceControlPressed).await {
                let _ = events.try_send(VoiceEvent::Error(err.to_string()));
                let _ = host.shutdown().await;
                return;
            }

            loop {
                tokio::select! {
                    maybe_command = control_rx.recv() => {
                        match maybe_command {
                            Some(SidecarCommand::Shutdown) | None => break,
                            Some(command) => {
                                if let Err(err) = host.send(command).await {
                                    let _ = events.try_send(VoiceEvent::Error(err.to_string()));
                                    break;
                                }
                            }
                        }
                    }
                    event = host.recv() => {
                        match event {
                            Ok(sidecar_event) => {
                                tracing::debug!(event = ?sidecar_event, "voice sidecar event received");
                                if let Some(voice_event) = sidecar_event_to_voice_event(sidecar_event) {
                                    let _ = events.try_send(voice_event);
                                }
                            }
                            Err(err) => {
                                let _ = events.try_send(VoiceEvent::Error(err.to_string()));
                                break;
                            }
                        }
                    }
                }
            }
            let _ = host.shutdown().await;
        });

        if let Ok(mut stored_control_tx) = self.control_tx.lock() {
            *stored_control_tx = Some(control_tx.clone());
        }
        self.worker = Some(worker);
        self.running = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        if self.running {
            if let Some(tx) = self.sidecar_command_sender() {
                tx.send(SidecarCommand::VoiceControlReleased).map_err(|err| {
                    RuntimeError::Tool(format!("failed to stop voice sidecar listening: {err}"))
                })?;
            }
            self.running = false;
        }
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.running
    }
}

impl Drop for SidecarSttProvider {
    fn drop(&mut self) {
        if let Some(tx) = self.sidecar_command_sender() {
            let _ = tx.send(SidecarCommand::Shutdown);
        }
        self.abort_worker();
    }
}
