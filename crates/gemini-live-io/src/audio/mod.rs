//! Desktop audio adapters backed by `cpal` and WebRTC AEC.
//!
//! The microphone and speaker adapters share an [`AecHandle`] so the echo
//! canceller can subtract speaker output from microphone capture before hosts
//! forward audio to the Live API.

#[cfg(feature = "aec")]
mod aec;
#[cfg(feature = "mic")]
mod mic;
mod resample;
#[cfg(feature = "speaker")]
mod speaker;

#[cfg(feature = "aec")]
pub use aec::{AEC_FRAME_SIZE, AEC_SAMPLE_RATE, AecHandle};
#[cfg(feature = "mic")]
pub use mic::{CapturedAudio, MicCapture};
#[cfg(feature = "speaker")]
pub use speaker::{MODEL_AUDIO_SAMPLE_RATE, SpeakerPlayback};
