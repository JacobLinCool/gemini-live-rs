use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};

use crate::error::AudioIoError;

use super::aec::{AEC_FRAME_SIZE, AEC_SAMPLE_RATE, AecHandle};
use super::resample::linear_resample;

/// Sample rate used by model audio returned from the Live API.
pub const MODEL_AUDIO_SAMPLE_RATE: u32 = 24_000;

/// Active output stream for model audio playback.
pub struct SpeakerPlayback {
    _stream: cpal::Stream,
    buffer: Arc<Mutex<VecDeque<f32>>>,
    /// Native sample rate reported by the output device.
    pub device_sample_rate: u32,
}

impl std::fmt::Debug for SpeakerPlayback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpeakerPlayback")
            .field("device_sample_rate", &self.device_sample_rate)
            .finish()
    }
}

impl SpeakerPlayback {
    /// Start playback on the default output device and feed render audio into
    /// the shared AEC processor.
    pub fn start(aec: AecHandle) -> Result<Self, AudioIoError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(AudioIoError::NoOutputDevice)?;
        let supported = device
            .default_output_config()
            .map_err(|e| AudioIoError::DefaultOutputConfig(e.to_string()))?;
        let device_sample_rate = supported.sample_rate().0;
        let channels = supported.channels() as usize;
        let config: StreamConfig = supported.config();

        let buffer = Arc::new(Mutex::new(VecDeque::<f32>::with_capacity(
            device_sample_rate as usize * 2,
        )));
        let shared_buffer = Arc::clone(&buffer);
        let shared_aec = aec.clone();

        let stream = match supported.sample_format() {
            SampleFormat::F32 => device.build_output_stream(
                &config,
                move |data: &mut [f32], _| {
                    fill_and_feed_aec_f32(
                        data,
                        channels,
                        device_sample_rate,
                        &shared_buffer,
                        &shared_aec,
                    );
                },
                |e| tracing::warn!("speaker: {e}"),
                None,
            ),
            SampleFormat::I16 => device.build_output_stream(
                &config,
                move |data: &mut [i16], _| {
                    let mut f32_buffer = vec![0.0f32; data.len()];
                    fill_and_feed_aec_f32(
                        &mut f32_buffer,
                        channels,
                        device_sample_rate,
                        &shared_buffer,
                        &shared_aec,
                    );
                    for (output, sample) in data.iter_mut().zip(f32_buffer) {
                        *output = (sample * 32767.0) as i16;
                    }
                },
                |e| tracing::warn!("speaker: {e}"),
                None,
            ),
            format => {
                return Err(AudioIoError::UnsupportedOutputFormat(format!("{format:?}")));
            }
        }
        .map_err(|e| AudioIoError::BuildOutputStream(e.to_string()))?;

        stream
            .play()
            .map_err(|e| AudioIoError::StartStream(e.to_string()))?;

        Ok(Self {
            _stream: stream,
            buffer,
            device_sample_rate,
        })
    }

    /// Discard buffered audio immediately.
    pub fn clear(&self) {
        if let Ok(mut buffer) = self.buffer.lock() {
            buffer.clear();
        }
    }

    /// Queue model PCM audio (`24 kHz`, `i16`, little-endian) for playback.
    pub fn push_model_pcm24k_i16(&self, pcm_i16_le: &[u8]) {
        let samples: Vec<f32> = pcm_i16_le
            .chunks_exact(2)
            .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]) as f32 / 32768.0)
            .collect();

        let output = if self.device_sample_rate == MODEL_AUDIO_SAMPLE_RATE {
            samples
        } else {
            linear_resample(&samples, MODEL_AUDIO_SAMPLE_RATE, self.device_sample_rate)
        };

        if let Ok(mut buffer) = self.buffer.lock() {
            buffer.extend(output);
        }
    }
}

fn fill_and_feed_aec_f32(
    data: &mut [f32],
    channels: usize,
    device_sample_rate: u32,
    buffer: &Mutex<VecDeque<f32>>,
    aec: &AecHandle,
) {
    if let Ok(mut shared_buffer) = buffer.try_lock() {
        for frame in data.chunks_mut(channels) {
            let sample = shared_buffer.pop_front().unwrap_or(0.0);
            frame.fill(sample);
        }
    } else {
        data.fill(0.0);
        return;
    }

    let mono: Vec<f32> = data.chunks(channels).map(|frame| frame[0]).collect();
    let aec_input = if device_sample_rate == AEC_SAMPLE_RATE {
        mono
    } else {
        linear_resample(&mono, device_sample_rate, AEC_SAMPLE_RATE)
    };

    for chunk in aec_input.chunks(AEC_FRAME_SIZE) {
        if chunk.len() < AEC_FRAME_SIZE {
            break;
        }

        let mut frame = chunk.to_vec();
        aec.processor().process_render_frame(&mut [&mut frame]).ok();
    }
}
