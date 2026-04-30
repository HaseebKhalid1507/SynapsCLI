use crate::{Result, RuntimeError};

pub const WHISPER_SAMPLE_RATE_HZ: u32 = 16_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate_hz: u32,
    pub channels: u16,
}

impl AudioFormat {
    pub fn new(sample_rate_hz: u32, channels: u16) -> Self {
        Self {
            sample_rate_hz,
            channels: channels.max(1),
        }
    }
}

pub fn interleaved_to_mono_f32(input: &[f32], channels: u16) -> Vec<f32> {
    let channel_count = usize::from(channels.max(1));
    if channel_count == 1 {
        return input.to_vec();
    }

    input
        .chunks(channel_count)
        .map(|frame| frame.iter().copied().sum::<f32>() / frame.len() as f32)
        .collect()
}

pub fn resample_linear_mono(input: &[f32], from_rate_hz: u32, to_rate_hz: u32) -> Result<Vec<f32>> {
    if from_rate_hz == 0 || to_rate_hz == 0 {
        return Err(RuntimeError::Tool(
            "audio sample rate must be greater than zero".to_string(),
        ));
    }
    if input.is_empty() || from_rate_hz == to_rate_hz {
        return Ok(input.to_vec());
    }

    let output_len = ((input.len() as u64 * to_rate_hz as u64) / from_rate_hz as u64).max(1) as usize;
    let ratio = from_rate_hz as f64 / to_rate_hz as f64;
    let mut output = Vec::with_capacity(output_len);

    for out_index in 0..output_len {
        let src_pos = out_index as f64 * ratio;
        let left = src_pos.floor() as usize;
        let right = (left + 1).min(input.len() - 1);
        let frac = (src_pos - left as f64) as f32;
        let sample = input[left] * (1.0 - frac) + input[right] * frac;
        output.push(sample);
    }

    Ok(output)
}

pub fn convert_interleaved_to_whisper_pcm(input: &[f32], format: AudioFormat) -> Result<Vec<f32>> {
    let mono = interleaved_to_mono_f32(input, format.channels);
    resample_linear_mono(&mono, format.sample_rate_hz, WHISPER_SAMPLE_RATE_HZ)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_interleaved_audio_is_averaged_to_mono() {
        let mono = interleaved_to_mono_f32(&[1.0, 0.0, 0.25, 0.75], 2);

        assert_eq!(mono, vec![0.5, 0.5]);
    }

    #[test]
    fn mono_audio_is_left_unchanged() {
        let mono = interleaved_to_mono_f32(&[0.1, -0.2, 0.3], 1);

        assert_eq!(mono, vec![0.1, -0.2, 0.3]);
    }

    #[test]
    fn resampling_from_48khz_to_16khz_keeps_expected_length() {
        let input = vec![0.5; 48_000];

        let output = resample_linear_mono(&input, 48_000, 16_000).unwrap();

        assert_eq!(output.len(), 16_000);
        assert!(output.iter().all(|sample| (*sample - 0.5).abs() < f32::EPSILON));
    }

    #[test]
    fn invalid_sample_rate_returns_error_without_panicking() {
        let err = resample_linear_mono(&[0.0], 0, 16_000)
            .unwrap_err()
            .to_string();

        assert!(err.contains("sample rate"));
    }
}
