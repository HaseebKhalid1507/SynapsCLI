use std::io::{self, BufRead, Write};

use synaps_cli::voice::sidecar_protocol::{
    SidecarCapability, SidecarCommand, SidecarEvent, SidecarProviderState,
    VOICE_SIDECAR_PROTOCOL_VERSION,
};

fn emit(event: &SidecarEvent) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, event)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

fn emit_ready() -> io::Result<()> {
    emit(&SidecarEvent::Hello {
        protocol_version: VOICE_SIDECAR_PROTOCOL_VERSION,
        extension: "synaps-voice-mock".to_string(),
        capabilities: vec![SidecarCapability::Stt],
    })?;
    emit(&SidecarEvent::Status {
        state: SidecarProviderState::Ready,
        capabilities: vec![SidecarCapability::Stt],
    })
}

fn main() -> io::Result<()> {
    let transcript = std::env::args()
        .skip_while(|arg| arg != "--transcript")
        .nth(1)
        .unwrap_or_else(|| "mock transcript".to_string());

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let command: SidecarCommand = match serde_json::from_str(&line) {
            Ok(command) => command,
            Err(err) => {
                emit(&SidecarEvent::Error {
                    message: format!("invalid sidecar command: {err}"),
                })?;
                continue;
            }
        };

        match command {
            SidecarCommand::Init { .. } => emit_ready()?,
            SidecarCommand::VoiceControlPressed => emit(&SidecarEvent::ListeningStarted)?,
            SidecarCommand::VoiceControlReleased => {
                emit(&SidecarEvent::ListeningStopped)?;
                emit(&SidecarEvent::TranscribingStarted)?;
                emit(&SidecarEvent::FinalTranscript {
                    text: transcript.clone(),
                })?;
            }
            SidecarCommand::Shutdown => break,
        }
    }

    Ok(())
}
