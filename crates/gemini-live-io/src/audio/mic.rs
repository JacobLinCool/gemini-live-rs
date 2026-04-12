use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use tokio::sync::mpsc;

use crate::error::AudioIoError;

use super::aec::{AEC_FRAME_SIZE, AEC_SAMPLE_RATE, AecHandle};
use super::resample::linear_resample_into;

/// Echo-cancelled mono PCM chunk captured from the default microphone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedAudio {
    /// i16 little-endian PCM payload suitable for `send_audio_at_rate`.
    pub pcm_i16_le: Vec<u8>,
    /// Sample rate of `pcm_i16_le`.
    pub sample_rate: u32,
}

/// Active capture stream for the default input device.
pub struct MicCapture {
    _stream: cpal::Stream,
    /// Native sample rate reported by the capture device.
    pub input_sample_rate: u32,
    /// Output sample rate after AEC processing.
    pub output_sample_rate: u32,
}

impl std::fmt::Debug for MicCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicCapture")
            .field("input_sample_rate", &self.input_sample_rate)
            .field("output_sample_rate", &self.output_sample_rate)
            .finish()
    }
}

#[derive(Default)]
struct MicCallbackState {
    input_f32: Vec<f32>,
    mono: Vec<f32>,
    resampled: Vec<f32>,
    aec_frame: Vec<f32>,
}

impl MicCallbackState {
    fn process_f32(
        &mut self,
        data: &[f32],
        channels: usize,
        input_sample_rate: u32,
        aec: &AecHandle,
        tx: &mpsc::Sender<CapturedAudio>,
    ) {
        let Self {
            input_f32: _,
            mono,
            resampled,
            aec_frame,
        } = self;
        process_mic_samples(
            data,
            channels,
            input_sample_rate,
            aec,
            tx,
            mono,
            resampled,
            aec_frame,
        );
    }

    fn process_i16(
        &mut self,
        data: &[i16],
        channels: usize,
        input_sample_rate: u32,
        aec: &AecHandle,
        tx: &mpsc::Sender<CapturedAudio>,
    ) {
        let Self {
            input_f32,
            mono,
            resampled,
            aec_frame,
        } = self;
        decode_i16_to_f32_into(input_f32, data);
        process_mic_samples(
            input_f32,
            channels,
            input_sample_rate,
            aec,
            tx,
            mono,
            resampled,
            aec_frame,
        );
    }
}

impl MicCapture {
    /// Start capturing from the default input device and forward cleaned PCM
    /// through `tx`.
    pub fn start(tx: mpsc::Sender<CapturedAudio>, aec: AecHandle) -> Result<Self, AudioIoError> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or(AudioIoError::NoInputDevice)?;
        let supported = device
            .default_input_config()
            .map_err(|e| AudioIoError::DefaultInputConfig(e.to_string()))?;
        let input_sample_rate = supported.sample_rate().0;
        let channels = supported.channels() as usize;
        let config: StreamConfig = supported.config();

        let stream = match supported.sample_format() {
            SampleFormat::F32 => {
                let aec = aec.clone();
                let tx = tx.clone();
                let mut callback_state = MicCallbackState::default();
                device.build_input_stream(
                    &config,
                    move |data: &[f32], _| {
                        callback_state.process_f32(data, channels, input_sample_rate, &aec, &tx);
                    },
                    |e| tracing::warn!("mic: {e}"),
                    None,
                )
            }
            SampleFormat::I16 => {
                let aec = aec.clone();
                let tx = tx.clone();
                let mut callback_state = MicCallbackState::default();
                device.build_input_stream(
                    &config,
                    move |data: &[i16], _| {
                        callback_state.process_i16(data, channels, input_sample_rate, &aec, &tx);
                    },
                    |e| tracing::warn!("mic: {e}"),
                    None,
                )
            }
            format => {
                return Err(AudioIoError::UnsupportedInputFormat(format!("{format:?}")));
            }
        }
        .map_err(|e| AudioIoError::BuildInputStream(e.to_string()))?;

        stream
            .play()
            .map_err(|e| AudioIoError::StartStream(e.to_string()))?;

        Ok(Self {
            _stream: stream,
            input_sample_rate,
            output_sample_rate: AEC_SAMPLE_RATE,
        })
    }
}

fn process_mic_samples(
    data: &[f32],
    channels: usize,
    input_sample_rate: u32,
    aec: &AecHandle,
    tx: &mpsc::Sender<CapturedAudio>,
    mono: &mut Vec<f32>,
    resampled: &mut Vec<f32>,
    aec_frame: &mut Vec<f32>,
) {
    mono.resize(data.len() / channels, 0.0);
    for (slot, frame) in mono.iter_mut().zip(data.chunks_exact(channels)) {
        let mut sum = 0.0;
        for &sample in frame {
            sum += sample;
        }
        *slot = sum / channels as f32;
    }

    let aec_input = if input_sample_rate == AEC_SAMPLE_RATE {
        mono.as_slice()
    } else {
        linear_resample_into(resampled, mono, input_sample_rate, AEC_SAMPLE_RATE);
        resampled.as_slice()
    };

    aec_frame.resize(AEC_FRAME_SIZE, 0.0);
    for chunk in aec_input.chunks(AEC_FRAME_SIZE) {
        if chunk.len() < AEC_FRAME_SIZE {
            break;
        }

        aec_frame.copy_from_slice(chunk);
        if aec
            .processor()
            .process_capture_frame(&mut [&mut aec_frame[..]])
            .is_err()
        {
            continue;
        }

        let pcm_i16_le = encode_f32_to_pcm_i16le(aec_frame);

        tx.try_send(CapturedAudio {
            pcm_i16_le,
            sample_rate: AEC_SAMPLE_RATE,
        })
        .ok();
    }
}

fn decode_i16_to_f32_into(output: &mut Vec<f32>, data: &[i16]) {
    output.resize(data.len(), 0.0);
    for (slot, &sample) in output.iter_mut().zip(data) {
        *slot = sample as f32 / 32768.0;
    }
}

fn encode_f32_to_pcm_i16le(samples: &[f32]) -> Vec<u8> {
    let mut pcm_i16_le = Vec::with_capacity(samples.len() * std::mem::size_of::<i16>());
    for &sample in samples {
        let normalized = (sample * 32767.0).clamp(-32768.0, 32767.0) as i16;
        pcm_i16_le.extend_from_slice(&normalized.to_le_bytes());
    }
    pcm_i16_le
}
