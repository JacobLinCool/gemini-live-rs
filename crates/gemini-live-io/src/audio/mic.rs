use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use tokio::sync::mpsc;

use crate::error::AudioIoError;

use super::aec::{AEC_FRAME_SIZE, AEC_SAMPLE_RATE, AecHandle};
use super::resample::linear_resample;

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
                device.build_input_stream(
                    &config,
                    move |data: &[f32], _| {
                        process_mic_f32(data, channels, input_sample_rate, &aec, &tx);
                    },
                    |e| tracing::warn!("mic: {e}"),
                    None,
                )
            }
            SampleFormat::I16 => {
                let aec = aec.clone();
                let tx = tx.clone();
                device.build_input_stream(
                    &config,
                    move |data: &[i16], _| {
                        let f32_data: Vec<f32> =
                            data.iter().map(|&sample| sample as f32 / 32768.0).collect();
                        process_mic_f32(&f32_data, channels, input_sample_rate, &aec, &tx);
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

fn process_mic_f32(
    data: &[f32],
    channels: usize,
    input_sample_rate: u32,
    aec: &AecHandle,
    tx: &mpsc::Sender<CapturedAudio>,
) {
    let mono: Vec<f32> = data
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect();

    let aec_input = if input_sample_rate == AEC_SAMPLE_RATE {
        mono
    } else {
        linear_resample(&mono, input_sample_rate, AEC_SAMPLE_RATE)
    };

    for chunk in aec_input.chunks(AEC_FRAME_SIZE) {
        if chunk.len() < AEC_FRAME_SIZE {
            break;
        }

        let mut frame = chunk.to_vec();
        if aec
            .processor()
            .process_capture_frame(&mut [&mut frame])
            .is_err()
        {
            continue;
        }

        let pcm_i16_le: Vec<u8> = frame
            .iter()
            .flat_map(|&sample| {
                let normalized = (sample * 32767.0).clamp(-32768.0, 32767.0) as i16;
                normalized.to_le_bytes()
            })
            .collect();

        tx.try_send(CapturedAudio {
            pcm_i16_le,
            sample_rate: AEC_SAMPLE_RATE,
        })
        .ok();
    }
}
