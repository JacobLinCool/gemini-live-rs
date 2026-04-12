use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};

use crate::error::AudioIoError;

use super::aec::{AEC_FRAME_SIZE, AEC_SAMPLE_RATE, AecHandle};
use super::resample::linear_resample_into;

/// Sample rate used by model audio returned from the Live API.
pub const MODEL_AUDIO_SAMPLE_RATE: u32 = 24_000;

/// Active output stream for model audio playback.
pub struct SpeakerPlayback {
    _stream: cpal::Stream,
    buffer: Arc<Mutex<VecDeque<f32>>>,
    push_state: Mutex<SpeakerPushState>,
    /// Native sample rate reported by the output device.
    pub device_sample_rate: u32,
}

#[derive(Default)]
struct SpeakerCallbackState {
    output_f32: Vec<f32>,
    mono: Vec<f32>,
    resampled: Vec<f32>,
    aec_frame: Vec<f32>,
}

#[derive(Default)]
struct SpeakerPushState {
    decoded: Vec<f32>,
    resampled: Vec<f32>,
}

struct SpeakerCallbackContext<'a> {
    channels: usize,
    device_sample_rate: u32,
    buffer: &'a Mutex<VecDeque<f32>>,
    aec: &'a AecHandle,
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
            SampleFormat::F32 => {
                let mut callback_state = SpeakerCallbackState::default();
                device.build_output_stream(
                    &config,
                    move |data: &mut [f32], _| {
                        let SpeakerCallbackState {
                            output_f32: _,
                            mono,
                            resampled,
                            aec_frame,
                        } = &mut callback_state;
                        fill_and_feed_aec_f32(
                            data,
                            SpeakerCallbackContext {
                                channels,
                                device_sample_rate,
                                buffer: &shared_buffer,
                                aec: &shared_aec,
                            },
                            mono,
                            resampled,
                            aec_frame,
                        );
                    },
                    |e| tracing::warn!("speaker: {e}"),
                    None,
                )
            }
            SampleFormat::I16 => {
                let mut callback_state = SpeakerCallbackState::default();
                device.build_output_stream(
                    &config,
                    move |data: &mut [i16], _| {
                        let SpeakerCallbackState {
                            output_f32,
                            mono,
                            resampled,
                            aec_frame,
                        } = &mut callback_state;
                        output_f32.resize(data.len(), 0.0);
                        fill_and_feed_aec_f32(
                            output_f32.as_mut_slice(),
                            SpeakerCallbackContext {
                                channels,
                                device_sample_rate,
                                buffer: &shared_buffer,
                                aec: &shared_aec,
                            },
                            mono,
                            resampled,
                            aec_frame,
                        );
                        for (output, sample) in data.iter_mut().zip(output_f32.iter().copied()) {
                            *output = (sample * 32767.0) as i16;
                        }
                    },
                    |e| tracing::warn!("speaker: {e}"),
                    None,
                )
            }
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
            push_state: Mutex::new(SpeakerPushState::default()),
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
        let mut push_state = self.push_state.lock().expect("speaker push state lock");
        let SpeakerPushState { decoded, resampled } = &mut *push_state;
        decode_pcm_i16le_to_f32_into(decoded, pcm_i16_le);

        let output = if self.device_sample_rate == MODEL_AUDIO_SAMPLE_RATE {
            decoded.as_slice()
        } else {
            linear_resample_into(
                resampled,
                decoded,
                MODEL_AUDIO_SAMPLE_RATE,
                self.device_sample_rate,
            );
            resampled.as_slice()
        };

        if let Ok(mut buffer) = self.buffer.lock() {
            buffer.extend(output.iter().copied());
        }
    }
}

fn fill_and_feed_aec_f32(
    data: &mut [f32],
    ctx: SpeakerCallbackContext<'_>,
    mono: &mut Vec<f32>,
    resampled: &mut Vec<f32>,
    aec_frame: &mut Vec<f32>,
) {
    let SpeakerCallbackContext {
        channels,
        device_sample_rate,
        buffer,
        aec,
    } = ctx;

    if let Ok(mut shared_buffer) = buffer.try_lock() {
        data.fill(0.0);
        for frame in data.chunks_mut(channels) {
            let sample = shared_buffer.pop_front().unwrap_or(0.0);
            frame.fill(sample);
        }
    } else {
        data.fill(0.0);
        return;
    }

    mono.resize(data.len() / channels, 0.0);
    for (slot, frame) in mono.iter_mut().zip(data.chunks_exact(channels)) {
        *slot = frame[0];
    }

    let aec_input = if device_sample_rate == AEC_SAMPLE_RATE {
        mono.as_slice()
    } else {
        linear_resample_into(resampled, mono, device_sample_rate, AEC_SAMPLE_RATE);
        resampled.as_slice()
    };

    aec_frame.resize(AEC_FRAME_SIZE, 0.0);
    for chunk in aec_input.chunks(AEC_FRAME_SIZE) {
        if chunk.len() < AEC_FRAME_SIZE {
            break;
        }

        aec_frame.copy_from_slice(chunk);
        aec.processor()
            .process_render_frame(&mut [&mut aec_frame[..]])
            .ok();
    }
}

fn decode_pcm_i16le_to_f32_into(output: &mut Vec<f32>, pcm_i16_le: &[u8]) {
    output.resize(pcm_i16_le.len() / std::mem::size_of::<i16>(), 0.0);
    for (slot, chunk) in output.iter_mut().zip(pcm_i16_le.chunks_exact(2)) {
        *slot = i16::from_le_bytes([chunk[0], chunk[1]]) as f32 / 32768.0;
    }
}
