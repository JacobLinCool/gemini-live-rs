use std::sync::Arc;

use webrtc_audio_processing::Processor;
use webrtc_audio_processing::config::{Config as AecConfig, EchoCanceller};

use crate::error::AudioIoError;

/// Processing sample rate used by the shared WebRTC AEC pipeline.
pub const AEC_SAMPLE_RATE: u32 = 48_000;

/// Required 10 ms frame size at [`AEC_SAMPLE_RATE`].
pub const AEC_FRAME_SIZE: usize = (AEC_SAMPLE_RATE / 100) as usize;

/// Shared echo-cancellation processor for desktop mic + speaker adapters.
#[derive(Clone)]
pub struct AecHandle {
    processor: Arc<Processor>,
}

impl std::fmt::Debug for AecHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AecHandle")
            .field("sample_rate", &AEC_SAMPLE_RATE)
            .finish()
    }
}

impl AecHandle {
    /// Create a WebRTC AEC processor configured for full echo cancellation.
    pub fn new() -> Result<Self, AudioIoError> {
        let processor =
            Processor::new(AEC_SAMPLE_RATE).map_err(|e| AudioIoError::AecInit(e.to_string()))?;
        processor.set_config(AecConfig {
            echo_canceller: Some(EchoCanceller::Full {
                stream_delay_ms: None,
            }),
            ..Default::default()
        });
        Ok(Self {
            processor: Arc::new(processor),
        })
    }

    pub(crate) fn processor(&self) -> &Processor {
        self.processor.as_ref()
    }
}
