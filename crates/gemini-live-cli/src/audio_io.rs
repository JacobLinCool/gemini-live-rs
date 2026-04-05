//! Audio I/O via `cpal` with WebRTC echo cancellation.
//!
//! [`Mic`] captures from the default input device, runs captured audio through
//! WebRTC's AEC to remove speaker echo, and sends clean mono i16-LE PCM
//! through a channel.
//!
//! [`Speaker`] plays model audio (24 kHz) through the default output device
//! and feeds the same audio into the AEC as the render reference signal.
//!
//! Both share an [`AecProcessor`] so the echo canceller knows what the speaker
//! is playing and can subtract it from the mic signal.

#[cfg(any(feature = "mic", feature = "speak"))]
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};

use webrtc_audio_processing::Processor as AecProcessor;
#[cfg(any(feature = "mic", feature = "speak"))]
use webrtc_audio_processing::config::{Config as AecConfig, EchoCanceller};

// ── Shared AEC processor ─────────────────────────────────────────────────────

/// Processing sample rate for AEC. Both mic and speaker audio are resampled
/// to this rate before passing through the processor.
#[cfg(any(feature = "mic", feature = "speak"))]
const AEC_SAMPLE_RATE: u32 = 48_000;

/// 10ms frame size at AEC_SAMPLE_RATE (required by WebRTC AP).
#[cfg(any(feature = "mic", feature = "speak"))]
const AEC_FRAME_SIZE: usize = (AEC_SAMPLE_RATE / 100) as usize;

/// Create a shared AEC processor. Call once and pass the `Arc` to both
/// [`Mic`] and [`Speaker`].
#[cfg(any(feature = "mic", feature = "speak"))]
pub fn create_aec() -> Arc<AecProcessor> {
    let ap = AecProcessor::new(AEC_SAMPLE_RATE).expect("failed to create AEC processor");
    ap.set_config(AecConfig {
        echo_canceller: Some(EchoCanceller::Full {
            stream_delay_ms: None,
        }),
        ..Default::default()
    });
    Arc::new(ap)
}

// ── Microphone ───────────────────────────────────────────────────────────────

#[cfg(feature = "mic")]
use tokio::sync::mpsc;

#[cfg(feature = "mic")]
/// Captures audio from the default input device, processes it through AEC,
/// and sends clean mono i16-LE PCM chunks through a channel.
pub struct Mic {
    _stream: cpal::Stream,
    pub sample_rate: u32,
}

#[cfg(feature = "mic")]
impl Mic {
    pub fn start(tx: mpsc::Sender<Vec<u8>>, aec: Arc<AecProcessor>) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host.default_input_device().ok_or("no input device")?;
        let sup = device.default_input_config().map_err(|e| e.to_string())?;
        let sample_rate = sup.sample_rate().0;
        let ch = sup.channels() as usize;
        let cfg: StreamConfig = sup.config();

        let stream = match sup.sample_format() {
            SampleFormat::F32 => device.build_input_stream(
                &cfg,
                move |data: &[f32], _| {
                    process_mic_f32(data, ch, sample_rate, &aec, &tx);
                },
                |e| tracing::warn!("mic: {e}"),
                None,
            ),
            SampleFormat::I16 => {
                let aec2 = aec;
                device.build_input_stream(
                    &cfg,
                    move |data: &[i16], _| {
                        // Convert i16 → f32 then process
                        let f32_data: Vec<f32> = data.iter().map(|&s| s as f32 / 32768.0).collect();
                        process_mic_f32(&f32_data, ch, sample_rate, &aec2, &tx);
                    },
                    |e| tracing::warn!("mic: {e}"),
                    None,
                )
            }
            f => return Err(format!("unsupported input format: {f:?}")),
        }
        .map_err(|e| e.to_string())?;

        stream.play().map_err(|e| e.to_string())?;
        Ok(Self {
            _stream: stream,
            sample_rate,
        })
    }
}

#[cfg(feature = "mic")]
fn process_mic_f32(
    data: &[f32],
    channels: usize,
    device_rate: u32,
    aec: &AecProcessor,
    tx: &mpsc::Sender<Vec<u8>>,
) {
    // Mix to mono
    let mono: Vec<f32> = data
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect();

    // Resample to AEC rate if needed
    let aec_input = if device_rate == AEC_SAMPLE_RATE {
        mono
    } else {
        linear_resample(&mono, device_rate, AEC_SAMPLE_RATE)
    };

    // Process in 10ms frames through AEC
    for chunk in aec_input.chunks(AEC_FRAME_SIZE) {
        if chunk.len() < AEC_FRAME_SIZE {
            break; // skip incomplete final frame
        }
        let mut frame = chunk.to_vec();
        // process_capture_frame removes echo from the mic signal
        if aec.process_capture_frame(&mut [&mut frame]).is_ok() {
            // Convert f32 → i16 LE bytes
            let pcm: Vec<u8> = frame
                .iter()
                .flat_map(|&s| {
                    let i = (s * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    i.to_le_bytes()
                })
                .collect();
            tx.try_send(pcm).ok();
        }
    }
}

// ── Speaker ──────────────────────────────────────────────────────────────────

#[cfg(feature = "speak")]
use std::collections::VecDeque;
#[cfg(feature = "speak")]
use std::sync::Mutex;

/// Plays audio through the default output device and feeds it into the AEC
/// as a render reference signal.
#[cfg(feature = "speak")]
pub struct Speaker {
    _stream: cpal::Stream,
    buffer: Arc<Mutex<VecDeque<f32>>>,
    pub device_rate: u32,
}

#[cfg(feature = "speak")]
impl Speaker {
    pub fn start(aec: Arc<AecProcessor>) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("no output device")?;
        let sup = device.default_output_config().map_err(|e| e.to_string())?;
        let device_rate = sup.sample_rate().0;
        let ch = sup.channels() as usize;
        let cfg: StreamConfig = sup.config();

        let buffer = Arc::new(Mutex::new(VecDeque::<f32>::with_capacity(
            device_rate as usize * 2,
        )));
        let buf = buffer.clone();
        let aec_ref = aec.clone();

        let stream = match sup.sample_format() {
            SampleFormat::F32 => device.build_output_stream(
                &cfg,
                move |data: &mut [f32], _| {
                    fill_and_feed_aec_f32(data, ch, device_rate, &buf, &aec_ref);
                },
                |e| tracing::warn!("speaker: {e}"),
                None,
            ),
            SampleFormat::I16 => {
                device.build_output_stream(
                    &cfg,
                    move |data: &mut [i16], _| {
                        // Fill f32 buffer, convert to i16 for output
                        let mut f32_buf = vec![0.0f32; data.len()];
                        fill_and_feed_aec_f32(&mut f32_buf, ch, device_rate, &buf, &aec_ref);
                        for (out, &sample) in data.iter_mut().zip(f32_buf.iter()) {
                            *out = (sample * 32767.0) as i16;
                        }
                    },
                    |e| tracing::warn!("speaker: {e}"),
                    None,
                )
            }
            f => return Err(format!("unsupported output format: {f:?}")),
        }
        .map_err(|e| e.to_string())?;

        stream.play().map_err(|e| e.to_string())?;
        Ok(Self {
            _stream: stream,
            buffer,
            device_rate,
        })
    }

    /// Discard all buffered audio. Called on interruption to stop the model
    /// from talking over the user.
    pub fn clear(&self) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.clear();
        }
    }

    /// Push model audio (24 kHz i16-LE PCM) into the playback buffer.
    pub fn push(&self, pcm_i16_le: &[u8]) {
        const MODEL_RATE: u32 = 24_000;

        let samples: Vec<f32> = pcm_i16_le
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
            .collect();

        let out = if self.device_rate == MODEL_RATE {
            samples
        } else {
            linear_resample(&samples, MODEL_RATE, self.device_rate)
        };

        if let Ok(mut buf) = self.buffer.lock() {
            buf.extend(out);
        }
    }
}

/// Fill the output buffer and feed the mono mix to AEC as render reference.
#[cfg(feature = "speak")]
fn fill_and_feed_aec_f32(
    data: &mut [f32],
    channels: usize,
    device_rate: u32,
    buffer: &Mutex<VecDeque<f32>>,
    aec: &AecProcessor,
) {
    // Fill output from buffer (mono → all channels)
    if let Ok(mut buf) = buffer.try_lock() {
        for frame in data.chunks_mut(channels) {
            let s = buf.pop_front().unwrap_or(0.0);
            frame.fill(s);
        }
    } else {
        data.fill(0.0);
        return;
    }

    // Extract mono signal for AEC render reference
    let mono: Vec<f32> = data.chunks(channels).map(|frame| frame[0]).collect();

    // Resample to AEC rate if needed
    let aec_input = if device_rate == AEC_SAMPLE_RATE {
        mono
    } else {
        linear_resample(&mono, device_rate, AEC_SAMPLE_RATE)
    };

    // Feed render frames to AEC (10ms chunks)
    for chunk in aec_input.chunks(AEC_FRAME_SIZE) {
        if chunk.len() < AEC_FRAME_SIZE {
            break;
        }
        let mut frame = chunk.to_vec();
        // Tell AEC what the speaker is playing
        aec.process_render_frame(&mut [&mut frame]).ok();
    }
}

// ── Shared utilities ─────────────────────────────────────────────────────────

#[cfg(any(feature = "mic", feature = "speak"))]
fn linear_resample(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if samples.is_empty() || from_rate == to_rate {
        return samples.to_vec();
    }
    let ratio = to_rate as f64 / from_rate as f64;
    let out_len = (samples.len() as f64 * ratio) as usize;
    (0..out_len)
        .map(|i| {
            let src = i as f64 / ratio;
            let idx = src as usize;
            let frac = (src - idx as f64) as f32;
            let s0 = samples[idx.min(samples.len() - 1)];
            let s1 = samples[(idx + 1).min(samples.len() - 1)];
            s0 + (s1 - s0) * frac
        })
        .collect()
}
