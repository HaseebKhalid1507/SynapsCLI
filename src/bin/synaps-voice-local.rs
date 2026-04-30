use std::io::{self, Write};
#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
use std::path::PathBuf;

use synaps_cli::voice::sidecar_protocol::{
    SidecarCapability, SidecarCommand, SidecarEvent, SidecarProviderState,
    VOICE_SIDECAR_PROTOCOL_VERSION,
};
use synaps_cli::voice::sidecar_provider::voice_event_to_sidecar_event;
#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
use synaps_cli::{SpeechToTextProvider, VoiceEvent, WhisperSttProvider};
#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
use synaps_cli::voice::stt_whisper::expand_whisper_model_path;
#[cfg(not(all(feature = "voice-stt-whisper", feature = "voice-mic")))]
use synaps_cli::VoiceEvent;

fn emit(event: &SidecarEvent) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, event)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

fn emit_ready() -> io::Result<()> {
    emit(&SidecarEvent::Hello {
        protocol_version: VOICE_SIDECAR_PROTOCOL_VERSION,
        extension: "synaps-voice-local".to_string(),
        capabilities: vec![SidecarCapability::Stt],
    })?;
    emit(&SidecarEvent::Status {
        state: SidecarProviderState::Ready,
        capabilities: vec![SidecarCapability::Stt],
    })
}

fn emit_error(message: impl Into<String>) -> io::Result<()> {
    emit(&SidecarEvent::Error {
        message: message.into(),
    })
}

fn arg_value(flag: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == flag {
            return args.next();
        }
    }
    None
}

struct LocalVoiceSidecar {
    mock_transcript: Option<String>,
    #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
    model_path: Option<PathBuf>,
    #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
    language: Option<String>,
    #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
    voice_tx: tokio::sync::mpsc::Sender<VoiceEvent>,
    #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
    stt: Option<WhisperSttProvider>,
}

impl LocalVoiceSidecar {
    fn new(_voice_tx: tokio::sync::mpsc::Sender<VoiceEvent>) -> Self {
        Self {
            mock_transcript: arg_value("--mock-transcript"),
            #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
            model_path: arg_value("--model-path").map(PathBuf::from),
            #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
            language: arg_value("--language"),
            #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
            voice_tx: _voice_tx,
            #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
            stt: None,
        }
    }

    fn handle_init(&mut self) -> io::Result<()> {
        emit_ready()
    }

    fn handle_press(&mut self) -> io::Result<()> {
        eprintln!("synaps-voice-local: voice_control_pressed");
        if self.mock_transcript.is_some() {
            return emit(&SidecarEvent::ListeningStarted);
        }
        self.start_real_stt()
    }

    fn handle_release(&mut self) -> io::Result<()> {
        eprintln!("synaps-voice-local: voice_control_released");
        if let Some(text) = &self.mock_transcript {
            emit(&SidecarEvent::ListeningStopped)?;
            emit(&SidecarEvent::TranscribingStarted)?;
            return emit(&SidecarEvent::FinalTranscript { text: text.clone() });
        }
        emit(&SidecarEvent::TranscribingStarted)?;
        self.stop_real_stt()
    }

    #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
    fn start_real_stt(&mut self) -> io::Result<()> {
        if self.stt.as_ref().is_some_and(|provider| provider.is_running()) {
            return Ok(());
        }
        let Some(model_path) = self.model_path.clone().map(|path| expand_whisper_model_path(&path)) else {
            return emit_error("local voice sidecar requires --model-path unless --mock-transcript is used");
        };
        let mut provider = match WhisperSttProvider::new(model_path, self.language.clone()) {
            Ok(provider) => provider,
            Err(err) => return emit_error(err.to_string()),
        };
        if let Err(err) = provider.start(self.voice_tx.clone()) {
            return emit_error(err.to_string());
        }
        eprintln!("synaps-voice-local: real STT started");
        self.stt = Some(provider);
        Ok(())
    }

    #[cfg(not(all(feature = "voice-stt-whisper", feature = "voice-mic")))]
    fn start_real_stt(&mut self) -> io::Result<()> {
        emit_error("local voice sidecar was built without voice-stt-whisper and voice-mic features; use --mock-transcript or rebuild with local voice features")
    }

    #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
    fn stop_real_stt(&mut self) -> io::Result<()> {
        if let Some(provider) = self.stt.as_mut() {
            if let Err(err) = provider.stop() {
                return emit_error(err.to_string());
            }
        }
        Ok(())
    }

    #[cfg(not(all(feature = "voice-stt-whisper", feature = "voice-mic")))]
    fn stop_real_stt(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn shutdown(&mut self) {
        #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
        if let Some(provider) = self.stt.as_mut() {
            let _ = provider.stop();
        }
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let (voice_tx, mut voice_rx) = tokio::sync::mpsc::channel::<VoiceEvent>(128);
    let mut sidecar = LocalVoiceSidecar::new(voice_tx);
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut lines = tokio::io::AsyncBufReadExt::lines(stdin);

    loop {
        tokio::select! {
            maybe_line = lines.next_line() => {
                let Some(line) = maybe_line? else { break; };
                if line.trim().is_empty() {
                    continue;
                }
                let command: SidecarCommand = match serde_json::from_str(&line) {
                    Ok(command) => command,
                    Err(err) => {
                        emit_error(format!("invalid sidecar command: {err}"))?;
                        continue;
                    }
                };

                match command {
                    SidecarCommand::Init { .. } => sidecar.handle_init()?,
                    SidecarCommand::VoiceControlPressed => sidecar.handle_press()?,
                    SidecarCommand::VoiceControlReleased => sidecar.handle_release()?,
                    SidecarCommand::Shutdown => break,
                }
            }
            maybe_voice = voice_rx.recv(), if !voice_rx.is_closed() => {
            if let Some(event) = maybe_voice.and_then(voice_event_to_sidecar_event) {
                eprintln!("synaps-voice-local: emitting {:?}", event);
                emit(&event)?;
            }
            }
        }
    }

    sidecar.shutdown();
    Ok(())
}
