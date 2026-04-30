use std::path::{Path, PathBuf};
#[cfg(feature = "voice-stt-whisper")]
use std::sync::OnceLock;
#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
use std::thread::{self, JoinHandle};
#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
use std::time::Duration;

#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
use cpal::Sample;
#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::{Result, RuntimeError, SpeechToTextProvider, VoiceEventSender};
#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
use crate::voice::audio::{convert_interleaved_to_whisper_pcm, AudioFormat, WHISPER_SAMPLE_RATE_HZ};
#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
use crate::voice::vad::{VadConfig, VoiceActivityDetector};
#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
use crate::VoiceEvent;

#[cfg(feature = "voice-stt-whisper")]
static WHISPER_LOG_HOOKS: OnceLock<()> = OnceLock::new();

#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
const MIC_AUDIO_BUFFER_CAPACITY: usize = 32;
#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
const MIC_WORKER_POLL_MS: u64 = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhisperSttConfig {
    pub model_path: PathBuf,
    pub language: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptionOptions {
    pub language: Option<String>,
    pub translate: bool,
}

#[derive(Debug)]
pub struct WhisperSttProvider {
    config: WhisperSttConfig,
    running: bool,
    #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
    vad_config: Option<VadConfig>,
    #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
    stop_signal: Option<Arc<AtomicBool>>,
    #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
    worker: Option<JoinHandle<()>>,
}

impl WhisperSttProvider {
    pub fn new(model_path: impl AsRef<Path>, language: Option<String>) -> Result<Self> {
        let model_path = model_path.as_ref().to_path_buf();
        validate_model_path(&model_path)?;
        Ok(Self {
            config: WhisperSttConfig { model_path, language },
            running: false,
            #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
            vad_config: None,
            #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
            stop_signal: None,
            #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
            worker: None,
        })
    }

    pub fn from_config(config: &crate::VoiceConfig) -> Result<Self> {
        let model_path = expand_whisper_model_path(&config.stt_model_path);
        Self::from_config_fields(model_path, language_option(&config.stt_language), config)
    }

    #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
    fn from_config_fields(
        model_path: PathBuf,
        language: Option<String>,
        config: &crate::VoiceConfig,
    ) -> Result<Self> {
        let mut provider = Self::new(model_path, language)?;
        provider.vad_config = Some(VadConfig {
            sample_rate_hz: WHISPER_SAMPLE_RATE_HZ,
            rms_threshold: config.stt_vad_rms_threshold,
            silence_submit_ms: config.stt_silence_submit_ms,
            min_speech_ms: config.stt_min_speech_ms,
            preroll_ms: config.stt_preroll_ms,
            max_utterance_ms: config.stt_max_utterance_ms,
        });
        if let Some(vad_config) = provider.vad_config {
            VoiceActivityDetector::new(vad_config)?;
        }
        Ok(provider)
    }

    #[cfg(not(all(feature = "voice-stt-whisper", feature = "voice-mic")))]
    fn from_config_fields(
        model_path: PathBuf,
        language: Option<String>,
        _config: &crate::VoiceConfig,
    ) -> Result<Self> {
        Self::new(model_path, language)
    }

    pub fn model_path(&self) -> &Path {
        &self.config.model_path
    }

    pub fn language(&self) -> Option<&str> {
        self.config.language.as_deref()
    }

    pub fn transcription_options(&self) -> TranscriptionOptions {
        TranscriptionOptions {
            language: self.config.language.clone(),
            translate: false,
        }
    }

}

impl SpeechToTextProvider for WhisperSttProvider {
    #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
    fn start(&mut self, events: VoiceEventSender) -> Result<()> {
        if self.running {
            return Ok(());
        }

        let stop_signal = Arc::new(AtomicBool::new(false));
        let worker_stop_signal = stop_signal.clone();
        let model_path = self.config.model_path.clone();
        let language = self.config.language.clone();
        let vad_config = self.vad_config.unwrap_or_default();

        let worker = thread::Builder::new()
            .name("synaps-voice-mic".to_string())
            .spawn(move || {
                run_mic_worker(model_path, language, vad_config, events, worker_stop_signal);
            })
            .map_err(|err| RuntimeError::Tool(format!("failed to start voice microphone worker: {err}")))?;

        self.stop_signal = Some(stop_signal);
        self.worker = Some(worker);
        self.running = true;
        Ok(())
    }

    #[cfg(not(all(feature = "voice-stt-whisper", feature = "voice-mic")))]
    fn start(&mut self, _events: VoiceEventSender) -> Result<()> {
        self.running = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
        {
            if let Some(stop_signal) = &self.stop_signal {
                stop_signal.store(true, Ordering::SeqCst);
            }
            if let Some(worker) = self.worker.take() {
                worker
                    .join()
                    .map_err(|_| RuntimeError::Tool("voice microphone worker panicked".to_string()))?;
            }
            self.stop_signal = None;
        }
        self.running = false;
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.running
    }
}

impl Drop for WhisperSttProvider {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

pub fn validate_model_path(path: &Path) -> Result<()> {
    if path.to_string_lossy().contains('\0') {
        return Err(RuntimeError::Tool(
            "whisper model path contains an invalid null byte".to_string(),
        ));
    }
    if !path.exists() {
        return Err(RuntimeError::Tool(format!(
            "whisper model path does not exist: {}",
            path.display()
        )));
    }
    if !path.is_file() {
        return Err(RuntimeError::Tool(format!(
            "whisper model path is not a file: {}",
            path.display()
        )));
    }
    Ok(())
}

pub fn expand_whisper_model_path(path: &Path) -> PathBuf {
    let path_string = path.to_string_lossy();
    let expanded = if path_string == "~" {
        if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home)
        } else {
            path.to_path_buf()
        }
    } else if let Some(rest) = path_string.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join(rest)
        } else {
            path.to_path_buf()
        }
    } else {
        path.to_path_buf()
    };

    if expanded.exists() {
        return expanded;
    }

    // The historical default points at ggml-base.en.bin, but local installs may
    // only have another Whisper ggml model downloaded (e.g. tiny for demos).
    // If the user did not explicitly choose a different filename, discover an
    // available local model in the same directory instead of failing on the
    // absent default path.
    if expanded.file_name().and_then(|name| name.to_str()) == Some("ggml-base.en.bin") {
        if let Some(parent) = expanded.parent() {
            if let Ok(entries) = std::fs::read_dir(parent) {
                let mut candidates = entries
                    .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                    .filter(|path| path.is_file())
                    .filter(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with("ggml-") && name.ends_with(".bin"))
                    })
                    .collect::<Vec<_>>();
                candidates.sort();
                if let Some(candidate) = candidates.into_iter().next() {
                    return candidate;
                }
            }
        }
    }

    expanded
}

fn language_option(language: &str) -> Option<String> {
    let trimmed = language.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
fn run_mic_worker(
    model_path: PathBuf,
    language: Option<String>,
    vad_config: VadConfig,
    events: VoiceEventSender,
    stop_signal: Arc<AtomicBool>,
) {
    if let Err(err) = capture_and_transcribe_mic(model_path, language, vad_config, events.clone(), stop_signal) {
        emit_voice_event(&events, VoiceEvent::Error(err.to_string()));
    }
    emit_voice_event(&events, VoiceEvent::ListeningStopped);
}

#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
fn capture_and_transcribe_mic(
    model_path: PathBuf,
    language: Option<String>,
    vad_config: VadConfig,
    events: VoiceEventSender,
    stop_signal: Arc<AtomicBool>,
) -> Result<()> {
    let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(MIC_AUDIO_BUFFER_CAPACITY);

    let stream = MicInputStream::open(events.clone(), audio_tx)?;
    let mut vad = VoiceActivityDetector::new(vad_config)?;

    stream.play()?;
    emit_voice_event(&events, VoiceEvent::ListeningStarted);

    while !stop_signal.load(Ordering::SeqCst) {
        match audio_rx.recv_timeout(Duration::from_millis(MIC_WORKER_POLL_MS)) {
            Ok(chunk) => {
                let converted = stream.convert_chunk(&chunk)?;
                for utterance in vad.process_chunk(&converted) {
                    transcribe_and_emit(&model_path, &language, &utterance, &events)?;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    if let Some(utterance) = vad.finish() {
        transcribe_and_emit(&model_path, &language, &utterance, &events)?;
    }
    Ok(())
}

#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
fn transcribe_and_emit(
    model_path: &Path,
    language: &Option<String>,
    pcm: &[f32],
    events: &VoiceEventSender,
) -> Result<()> {
    let transcript = transcribe_pcm_16khz(model_path, pcm, language.as_deref())?;
    if !transcript.is_empty() {
        emit_voice_event(events, VoiceEvent::FinalTranscript(transcript));
    }
    Ok(())
}

#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
struct MicInputStream {
    stream: cpal::Stream,
    format: AudioFormat,
}

#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
impl MicInputStream {
    fn open(events: VoiceEventSender, audio_tx: std::sync::mpsc::SyncSender<Vec<f32>>) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| RuntimeError::Tool("no default microphone input device is available".to_string()))?;
        let config = device
            .default_input_config()
            .map_err(|err| RuntimeError::Tool(format!("failed to read default microphone input config: {err}")))?;
        let sample_rate_hz = config.sample_rate().0;
        let channels = config.channels();
        let stream_config: cpal::StreamConfig = config.clone().into();
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => build_input_stream::<f32>(&device, &stream_config, audio_tx.clone(), events),
            cpal::SampleFormat::I16 => build_input_stream::<i16>(&device, &stream_config, audio_tx.clone(), events),
            cpal::SampleFormat::U16 => build_input_stream::<u16>(&device, &stream_config, audio_tx.clone(), events),
            sample_format => Err(RuntimeError::Tool(format!(
                "unsupported microphone sample format: {sample_format:?}"
            ))),
        }?;
        Ok(Self {
            stream,
            format: AudioFormat::new(sample_rate_hz, channels),
        })
    }

    fn play(&self) -> Result<()> {
        self.stream
            .play()
            .map_err(|err| RuntimeError::Tool(format!("failed to start microphone input stream: {err}")))
    }

    fn convert_chunk(&self, chunk: &[f32]) -> Result<Vec<f32>> {
        convert_interleaved_to_whisper_pcm(chunk, self.format)
    }
}

#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
fn input_samples_to_f32<T>(data: &[T]) -> Vec<f32>
where
    T: cpal::Sample + cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    data.iter().copied().map(f32::from_sample).collect()
}

#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
fn build_input_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    audio_tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    events: VoiceEventSender,
) -> Result<cpal::Stream>
where
    T: cpal::Sample + cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                let chunk = input_samples_to_f32(data);
                let _ = audio_tx.try_send(chunk);
            },
            move |err| {
                emit_voice_event(&events, VoiceEvent::Error(format!("microphone input stream error: {err}")));
            },
            None,
        )
        .map_err(|err| RuntimeError::Tool(format!("failed to build microphone input stream: {err}")))
}

#[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
fn emit_voice_event(events: &VoiceEventSender, event: VoiceEvent) {
    let _ = events.try_send(event);
}

#[cfg(feature = "voice-stt-whisper")]
pub fn transcribe_pcm_16khz(model_path: &Path, pcm: &[f32], language: Option<&str>) -> Result<String> {
    if pcm.is_empty() {
        return Err(RuntimeError::Tool(
            "whisper transcription requires non-empty 16kHz mono PCM".to_string(),
        ));
    }
    validate_model_path(model_path)?;

    WHISPER_LOG_HOOKS.get_or_init(|| {
        whisper_rs::install_logging_hooks();
    });
    let context = whisper_rs::WhisperContext::new_with_params(
        model_path,
        whisper_rs::WhisperContextParameters::default(),
    )
    .map_err(|err| RuntimeError::Tool(format!("failed to load whisper model: {err}")))?;
    let mut state = context
        .create_state()
        .map_err(|err| RuntimeError::Tool(format!("failed to create whisper state: {err}")))?;
    let mut params = whisper_rs::FullParams::new(whisper_rs::SamplingStrategy::Greedy { best_of: 1 });
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_print_special(false);
    params.set_no_timestamps(true);
    params.set_language(language);

    state
        .full(params, pcm)
        .map_err(|err| RuntimeError::Tool(format!("whisper transcription failed: {err}")))?;

    let mut transcript = String::new();
    for segment in state.as_iter() {
        let text = segment
            .to_str()
            .map_err(|err| RuntimeError::Tool(format!("failed to read whisper transcript segment: {err}")))?;
        transcript.push_str(text);
    }
    Ok(transcript.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_model_path_returns_actionable_error() {
        let path = PathBuf::from("/tmp/synaps-cli-missing-whisper-model.bin");

        let err = WhisperSttProvider::new(&path, Some("en".to_string()))
            .unwrap_err()
            .to_string();

        assert!(err.contains("whisper model path does not exist"));
        assert!(err.contains(path.to_string_lossy().as_ref()));
    }

    #[test]
    fn directory_model_path_returns_actionable_error() {
        let dir = tempfile::tempdir().unwrap();

        let err = WhisperSttProvider::new(dir.path(), None)
            .unwrap_err()
            .to_string();

        assert!(err.contains("whisper model path is not a file"));
    }

    #[test]
    fn null_byte_model_path_returns_actionable_error() {
        let path = PathBuf::from("/tmp/synaps\0whisper.bin");

        let err = validate_model_path(&path).unwrap_err().to_string();

        assert!(err.contains("invalid null byte"));
    }

    #[test]
    fn provider_construction_accepts_existing_model_file_path() {
        let file = tempfile::NamedTempFile::new().unwrap();

        let provider = WhisperSttProvider::new(file.path(), Some("en".to_string())).unwrap();

        assert_eq!(provider.model_path(), file.path());
        assert_eq!(provider.language(), Some("en"));
    }

    #[test]
    fn provider_start_stop_tracks_running_state_without_opening_microphone() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut provider = WhisperSttProvider::new(file.path(), None).unwrap();
        let (events, _rx) = tokio::sync::mpsc::channel(1);

        assert!(!provider.is_running());
        provider.start(events).unwrap();
        assert!(provider.is_running());
        provider.stop().unwrap();
        assert!(!provider.is_running());
    }

    #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
    #[test]
    fn input_samples_are_converted_to_f32_without_opening_microphone() {
        assert_eq!(input_samples_to_f32(&[0.0_f32, -0.5, 0.5]), vec![0.0, -0.5, 0.5]);

        let i16_audio = input_samples_to_f32(&[0_i16, i16::MAX]);
        assert_eq!(i16_audio[0], 0.0);
        assert!((i16_audio[1] - 1.0).abs() < 0.0001);

        let u16_audio = input_samples_to_f32(&[0_u16, u16::MAX]);
        assert!((u16_audio[0] + 1.0).abs() < 0.0001);
        assert!((u16_audio[1] - 1.0).abs() < 0.0001);
    }

    #[cfg(all(feature = "voice-stt-whisper", feature = "voice-mic"))]
    #[test]
    fn mic_event_sender_drops_when_channel_is_full_without_blocking() {
        let (events, _rx) = tokio::sync::mpsc::channel(1);

        emit_voice_event(&events, VoiceEvent::ListeningStarted);
        emit_voice_event(&events, VoiceEvent::ListeningStopped);
    }

    #[test]
    fn model_path_expands_home_prefix_without_validating_until_provider_construction() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());

        let expanded = expand_whisper_model_path(Path::new("~/models/whisper.bin"));

        assert_eq!(expanded, home.path().join("models/whisper.bin"));
    }

    #[test]
    fn auto_language_maps_to_none_for_whisper_auto_detection() {
        assert_eq!(language_option("auto"), None);
        assert_eq!(language_option(""), None);
        assert_eq!(language_option(" en "), Some("en".to_string()));
    }

    #[cfg(feature = "voice-stt-whisper")]
    #[test]
    fn empty_pcm_returns_clear_error_before_loading_model() {
        let path = PathBuf::from("/tmp/synaps-cli-missing-whisper-model.bin");

        let err = transcribe_pcm_16khz(&path, &[], Some("en"))
            .unwrap_err()
            .to_string();

        assert!(err.contains("non-empty 16kHz mono PCM"));
    }
}
