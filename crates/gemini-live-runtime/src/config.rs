//! Runtime-owned configuration wrappers layered above `gemini_live`.
//!
//! These types let host applications stage selected `setup` changes without
//! rebuilding an entire [`gemini_live::types::SetupConfig`] by hand.

use gemini_live::session::SessionConfig;
use gemini_live::types::{
    AudioTranscriptionConfig, Content, ContextWindowCompressionConfig, GenerationConfig,
    HistoryConfig, ProactivityConfig, RealtimeInputConfig, SetupConfig, Tool,
};

/// Runtime bootstrap configuration.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Baseline session configuration used for the next connect or apply.
    pub session: SessionConfig,
}

/// A tri-state patch used for staged setup edits.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Patch<T> {
    /// Leave the target field unchanged.
    #[default]
    Unchanged,
    /// Replace the target field with a new value.
    Set(T),
    /// Remove the target field entirely.
    Clear,
}

impl<T: Clone> Patch<T> {
    pub fn is_unchanged(&self) -> bool {
        matches!(self, Self::Unchanged)
    }

    pub fn apply_to(&self, slot: &mut Option<T>) {
        match self {
            Self::Unchanged => {}
            Self::Set(value) => *slot = Some(value.clone()),
            Self::Clear => *slot = None,
        }
    }
}

/// A staged update for the mutable portions of `setup`.
///
/// Hosts can keep a baseline [`SetupConfig`], stage one or more patch values,
/// and then apply the result through [`crate::LiveRuntime`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SetupPatch {
    pub system_instruction: Patch<Content>,
    pub tools: Patch<Vec<Tool>>,
    pub generation_config: Patch<GenerationConfig>,
    pub realtime_input_config: Patch<RealtimeInputConfig>,
    pub context_window_compression: Patch<ContextWindowCompressionConfig>,
    pub input_audio_transcription: Patch<AudioTranscriptionConfig>,
    pub output_audio_transcription: Patch<AudioTranscriptionConfig>,
    pub proactivity: Patch<ProactivityConfig>,
    pub history_config: Patch<HistoryConfig>,
}

impl SetupPatch {
    pub fn is_empty(&self) -> bool {
        self.system_instruction.is_unchanged()
            && self.tools.is_unchanged()
            && self.generation_config.is_unchanged()
            && self.realtime_input_config.is_unchanged()
            && self.context_window_compression.is_unchanged()
            && self.input_audio_transcription.is_unchanged()
            && self.output_audio_transcription.is_unchanged()
            && self.proactivity.is_unchanged()
            && self.history_config.is_unchanged()
    }

    /// Apply the staged changes onto an existing `setup` value.
    pub fn apply_to(&self, setup: &mut SetupConfig) {
        self.system_instruction
            .apply_to(&mut setup.system_instruction);
        self.tools.apply_to(&mut setup.tools);
        self.generation_config
            .apply_to(&mut setup.generation_config);
        self.realtime_input_config
            .apply_to(&mut setup.realtime_input_config);
        self.context_window_compression
            .apply_to(&mut setup.context_window_compression);
        self.input_audio_transcription
            .apply_to(&mut setup.input_audio_transcription);
        self.output_audio_transcription
            .apply_to(&mut setup.output_audio_transcription);
        self.proactivity.apply_to(&mut setup.proactivity);
        self.history_config.apply_to(&mut setup.history_config);
    }
}
