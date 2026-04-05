//! Audio I/O via `cpal` — microphone capture and speaker playback.
//!
//! [`Mic`] opens the default input device and sends mono i16-LE PCM chunks
//! through a tokio channel.  [`Speaker`] opens the default output device and
//! plays model audio (24 kHz) resampled to the device's native rate.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use tokio::sync::mpsc;

// ── Microphone ───────────────────────────────────────────────────────────────

/// Captures audio from the default input device and sends PCM chunks
/// (mono i16-LE bytes) through a channel.
pub struct Mic {
    _stream: cpal::Stream,
    pub sample_rate: u32,
}

impl Mic {
    pub fn start(tx: mpsc::Sender<Vec<u8>>) -> Result<Self, String> {
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
                    tx.try_send(mono_i16_le_from_f32(data, ch)).ok();
                },
                |e| tracing::warn!("mic: {e}"),
                None,
            ),
            SampleFormat::I16 => device.build_input_stream(
                &cfg,
                move |data: &[i16], _| {
                    tx.try_send(mono_i16_le_from_i16(data, ch)).ok();
                },
                |e| tracing::warn!("mic: {e}"),
                None,
            ),
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

fn mono_i16_le_from_f32(data: &[f32], channels: usize) -> Vec<u8> {
    data.chunks(channels)
        .flat_map(|frame| {
            let avg = frame.iter().sum::<f32>() / channels as f32;
            let s = (avg * 32767.0).clamp(-32768.0, 32767.0) as i16;
            s.to_le_bytes()
        })
        .collect()
}

fn mono_i16_le_from_i16(data: &[i16], channels: usize) -> Vec<u8> {
    data.chunks(channels)
        .flat_map(|frame| {
            let avg = frame.iter().map(|&s| s as i32).sum::<i32>() / channels as i32;
            (avg as i16).to_le_bytes()
        })
        .collect()
}

// ── Speaker ──────────────────────────────────────────────────────────────────

/// Plays audio through the default output device.
/// Model audio (24 kHz i16-LE PCM) is pushed via [`Speaker::push`] and
/// resampled to the device's native rate.
pub struct Speaker {
    _stream: cpal::Stream,
    buffer: Arc<Mutex<VecDeque<f32>>>,
    pub device_rate: u32,
}

impl Speaker {
    pub fn start() -> Result<Self, String> {
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

        let stream = match sup.sample_format() {
            SampleFormat::F32 => device.build_output_stream(
                &cfg,
                move |data: &mut [f32], _| fill_f32(data, ch, &buf),
                |e| tracing::warn!("speaker: {e}"),
                None,
            ),
            SampleFormat::I16 => device.build_output_stream(
                &cfg,
                move |data: &mut [i16], _| fill_i16(data, ch, &buf),
                |e| tracing::warn!("speaker: {e}"),
                None,
            ),
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

fn fill_f32(data: &mut [f32], channels: usize, buffer: &Mutex<VecDeque<f32>>) {
    if let Ok(mut buf) = buffer.try_lock() {
        for frame in data.chunks_mut(channels) {
            let s = buf.pop_front().unwrap_or(0.0);
            frame.fill(s);
        }
    } else {
        data.fill(0.0);
    }
}

fn fill_i16(data: &mut [i16], channels: usize, buffer: &Mutex<VecDeque<f32>>) {
    if let Ok(mut buf) = buffer.try_lock() {
        for frame in data.chunks_mut(channels) {
            let s = buf.pop_front().unwrap_or(0.0);
            frame.fill((s * 32767.0) as i16);
        }
    } else {
        data.fill(0);
    }
}

fn linear_resample(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
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
