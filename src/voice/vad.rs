use std::collections::VecDeque;

use crate::{Result, RuntimeError};

use super::audio::WHISPER_SAMPLE_RATE_HZ;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VadConfig {
    pub sample_rate_hz: u32,
    pub rms_threshold: f32,
    pub silence_submit_ms: u64,
    pub min_speech_ms: u64,
    pub preroll_ms: u64,
    pub max_utterance_ms: u64,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            sample_rate_hz: WHISPER_SAMPLE_RATE_HZ,
            rms_threshold: 0.01,
            silence_submit_ms: 1_000,
            min_speech_ms: 250,
            preroll_ms: 300,
            max_utterance_ms: 30_000,
        }
    }
}

#[derive(Debug)]
pub struct VoiceActivityDetector {
    config: VadConfig,
    preroll: VecDeque<f32>,
    utterance: Vec<f32>,
    speaking: bool,
    speech_samples: usize,
    trailing_silence_samples: usize,
}

impl VoiceActivityDetector {
    pub fn new(config: VadConfig) -> Result<Self> {
        validate_vad_config(config)?;
        Ok(Self {
            config,
            preroll: VecDeque::with_capacity(samples_for_ms(config.sample_rate_hz, config.preroll_ms)),
            utterance: Vec::new(),
            speaking: false,
            speech_samples: 0,
            trailing_silence_samples: 0,
        })
    }

    pub fn process_chunk(&mut self, chunk: &[f32]) -> Vec<Vec<f32>> {
        let mut utterances = Vec::new();
        if chunk.is_empty() {
            return utterances;
        }

        let is_speech = rms(chunk) >= self.config.rms_threshold;
        if is_speech {
            if !self.speaking {
                self.start_utterance_with_preroll();
            }
            self.utterance.extend_from_slice(chunk);
            self.speech_samples = self.speech_samples.saturating_add(chunk.len());
            self.trailing_silence_samples = 0;
        } else if self.speaking {
            self.utterance.extend_from_slice(chunk);
            self.trailing_silence_samples = self.trailing_silence_samples.saturating_add(chunk.len());
        } else {
            self.push_preroll(chunk);
        }

        let max_samples = samples_for_ms(self.config.sample_rate_hz, self.config.max_utterance_ms);
        if self.speaking && self.utterance.len() >= max_samples {
            if let Some(utterance) = self.take_valid_utterance() {
                utterances.push(utterance);
            }
        } else {
            let silence_samples = samples_for_ms(self.config.sample_rate_hz, self.config.silence_submit_ms);
            if self.speaking && self.trailing_silence_samples >= silence_samples {
                if let Some(utterance) = self.take_valid_utterance() {
                    utterances.push(utterance);
                }
            }
        }

        utterances
    }

    pub fn finish(&mut self) -> Option<Vec<f32>> {
        if self.speaking {
            self.take_valid_utterance()
        } else {
            None
        }
    }

    fn start_utterance_with_preroll(&mut self) {
        self.speaking = true;
        self.utterance.extend(self.preroll.drain(..));
    }

    fn push_preroll(&mut self, chunk: &[f32]) {
        let max_preroll_samples = samples_for_ms(self.config.sample_rate_hz, self.config.preroll_ms);
        for sample in chunk {
            self.preroll.push_back(*sample);
            while self.preroll.len() > max_preroll_samples {
                self.preroll.pop_front();
            }
        }
    }

    fn take_valid_utterance(&mut self) -> Option<Vec<f32>> {
        let min_samples = samples_for_ms(self.config.sample_rate_hz, self.config.min_speech_ms);
        let max_samples = samples_for_ms(self.config.sample_rate_hz, self.config.max_utterance_ms);
        let speech_samples = self.speech_samples;
        let mut utterance = std::mem::take(&mut self.utterance);
        self.speaking = false;
        self.speech_samples = 0;
        self.trailing_silence_samples = 0;
        self.preroll.clear();

        if utterance.len() > max_samples {
            utterance.truncate(max_samples);
        }

        if speech_samples >= min_samples {
            Some(utterance)
        } else {
            None
        }
    }
}

fn validate_vad_config(config: VadConfig) -> Result<()> {
    if config.sample_rate_hz == 0 {
        return Err(RuntimeError::Tool("VAD sample rate must be greater than zero".to_string()));
    }
    if !config.rms_threshold.is_finite() || config.rms_threshold < 0.0 {
        return Err(RuntimeError::Tool("VAD RMS threshold must be a finite non-negative value".to_string()));
    }
    if config.max_utterance_ms == 0 {
        return Err(RuntimeError::Tool("VAD max utterance duration must be greater than zero".to_string()));
    }
    Ok(())
}

fn samples_for_ms(sample_rate_hz: u32, ms: u64) -> usize {
    ((sample_rate_hz as u64).saturating_mul(ms) / 1_000) as usize
}

fn rms(chunk: &[f32]) -> f32 {
    if chunk.is_empty() {
        return 0.0;
    }
    let mean_square = chunk.iter().map(|sample| sample * sample).sum::<f32>() / chunk.len() as f32;
    mean_square.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> VadConfig {
        VadConfig {
            sample_rate_hz: 1_000,
            rms_threshold: 0.1,
            silence_submit_ms: 100,
            min_speech_ms: 50,
            preroll_ms: 20,
            max_utterance_ms: 500,
        }
    }

    #[test]
    fn silence_does_not_emit_utterances() {
        let mut vad = VoiceActivityDetector::new(test_config()).unwrap();

        for _ in 0..20 {
            assert!(vad.process_chunk(&vec![0.0; 10]).is_empty());
        }
        assert!(vad.finish().is_none());
    }

    #[test]
    fn speech_followed_by_configured_silence_emits_final_utterance() {
        let mut vad = VoiceActivityDetector::new(test_config()).unwrap();

        assert!(vad.process_chunk(&vec![0.2; 60]).is_empty());
        assert!(vad.process_chunk(&vec![0.0; 90]).is_empty());
        let utterances = vad.process_chunk(&vec![0.0; 10]);

        assert_eq!(utterances.len(), 1);
        assert!(utterances[0].len() >= 60);
    }

    #[test]
    fn short_noise_burst_below_min_speech_is_discarded() {
        let mut vad = VoiceActivityDetector::new(test_config()).unwrap();

        assert!(vad.process_chunk(&vec![0.2; 30]).is_empty());
        assert!(vad.process_chunk(&vec![0.0; 100]).is_empty());
        assert!(vad.finish().is_none());
    }

    #[test]
    fn preroll_is_preserved_when_speech_starts() {
        let mut vad = VoiceActivityDetector::new(test_config()).unwrap();

        assert!(vad.process_chunk(&vec![0.01; 20]).is_empty());
        assert!(vad.process_chunk(&vec![0.2; 60]).is_empty());
        let utterance = vad.process_chunk(&vec![0.0; 100]).remove(0);

        assert!(utterance.starts_with(&vec![0.01; 20]));
    }

    #[test]
    fn max_utterance_duration_forces_emit_without_waiting_for_silence() {
        let mut vad = VoiceActivityDetector::new(test_config()).unwrap();

        let utterances = vad.process_chunk(&vec![0.2; 500]);

        assert_eq!(utterances.len(), 1);
        assert_eq!(utterances[0].len(), 500);
    }

    #[test]
    fn oversized_input_chunk_is_capped_at_max_utterance_duration() {
        let mut vad = VoiceActivityDetector::new(test_config()).unwrap();

        let utterances = vad.process_chunk(&vec![0.2; 800]);

        assert_eq!(utterances.len(), 1);
        assert_eq!(utterances[0].len(), 500);
    }

    #[test]
    fn invalid_config_returns_error_without_panicking() {
        let err = VoiceActivityDetector::new(VadConfig { sample_rate_hz: 0, ..test_config() })
            .unwrap_err()
            .to_string();

        assert!(err.contains("sample rate"));
    }
}
