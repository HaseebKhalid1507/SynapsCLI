pub mod types;
pub mod sidecar_protocol;
pub mod sidecar_host;
pub mod sidecar_provider;
pub mod audio;
pub mod vad;
pub mod transcript;
pub mod commands;
pub mod conversation;

#[cfg(feature = "voice-stt-whisper")]
pub mod stt_whisper;

#[cfg(feature = "voice-stt-whisper")]
pub use stt_whisper::WhisperSttProvider;

pub use audio::{convert_interleaved_to_whisper_pcm, interleaved_to_mono_f32, resample_linear_mono, AudioFormat};
pub use transcript::{sanitize_voice_transcript, DEFAULT_MAX_VOICE_TRANSCRIPT_CHARS};
pub use commands::{map_spoken_phrase, VoiceCommandAction, VoiceCommandConfig};
pub use conversation::transcript_to_conversation_action;
pub use sidecar_provider::{sidecar_event_to_voice_event, SidecarSttProvider};
pub use vad::{VadConfig, VoiceActivityDetector};
pub use types::{
    SpeechToTextProvider, VoiceEvent, VoiceEventReceiver, VoiceEventSender,
    VoiceProviderState, VoiceRuntime, VoiceRuntimeHandle,
};
