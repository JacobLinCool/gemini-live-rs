//! Configuration types used within the `setup` message.
//!
//! These control model behaviour, audio/video input handling, VAD, session
//! resumption, context compression, and more.  All structs derive [`Default`]
//! so callers can use the `..Default::default()` pattern for partial init.

use serde::{Deserialize, Serialize};

// ── Generation config ────────────────────────────────────────────────────────

/// Controls how the model generates responses.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationConfig {
    /// Which modalities the model should produce (`AUDIO`, `TEXT`, or both).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_modalities: Option<Vec<Modality>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speech_config: Option<SpeechConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_config: Option<ThinkingConfig>,
    /// Image resolution hint sent to the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_resolution: Option<MediaResolution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_count: Option<u32>,
}

/// Output modality requested from the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Modality {
    Audio,
    Text,
}

// ── Speech / Voice ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeechConfig {
    pub voice_config: VoiceConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceConfig {
    pub prebuilt_voice_config: PrebuiltVoiceConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrebuiltVoiceConfig {
    /// Voice name, e.g. `"Kore"`, `"Puck"`, `"Charon"`, etc.
    pub voice_name: String,
}

// ── Thinking ─────────────────────────────────────────────────────────────────

/// Thinking / reasoning configuration.
///
/// Gemini 3.1 uses `thinking_level` (enum), while Gemini 2.5 uses
/// `thinking_budget` (token count).  Both may be set; the model ignores
/// the field it doesn't understand.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingConfig {
    /// Gemini 3.1: discrete level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
    /// Gemini 2.5: token budget.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_thoughts: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
}

// ── Media resolution ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MediaResolution {
    MediaResolutionLow,
    MediaResolutionHigh,
}

// ── Realtime input config ────────────────────────────────────────────────────

/// Controls how the server interprets real-time audio/video input.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RealtimeInputConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automatic_activity_detection: Option<AutomaticActivityDetection>,
    /// What happens when user activity is detected while the model is speaking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity_handling: Option<ActivityHandling>,
    /// What audio is included in the user's turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_coverage: Option<TurnCoverage>,
}

/// Server-side Voice Activity Detection parameters.
///
/// When `disabled` is `true`, the client must send `activityStart` /
/// `activityEnd` signals manually.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomaticActivityDetection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_of_speech_sensitivity: Option<StartSensitivity>,
    /// Milliseconds of audio to retain before the detected speech onset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix_padding_ms: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_of_speech_sensitivity: Option<EndSensitivity>,
    /// Milliseconds of silence required to mark speech as ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub silence_duration_ms: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StartSensitivity {
    StartSensitivityHigh,
    StartSensitivityLow,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EndSensitivity {
    EndSensitivityHigh,
    EndSensitivityLow,
}

/// What happens when user activity (speech) is detected while the model is
/// generating a response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ActivityHandling {
    /// User speech interrupts the model (default).
    StartOfActivityInterrupts,
    /// Model continues uninterrupted.
    NoInterruption,
}

/// Which portions of the audio stream are included in the user's turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TurnCoverage {
    /// Only detected speech activity (default on Gemini 2.5).
    TurnIncludesOnlyActivity,
    /// All audio including silence.
    TurnIncludesAllInput,
    /// Speech activity + all video frames (default on Gemini 3.1).
    TurnIncludesAudioActivityAndAllVideo,
}

// ── Session resumption ───────────────────────────────────────────────────────

/// Enables session resumption.  Include an empty struct to opt in; pass a
/// previous `handle` to resume a disconnected session.
///
/// Handles are valid for **2 hours** after disconnect; sessions can be
/// resumed within **24 hours**.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionResumptionConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
}

// ── Context window compression ───────────────────────────────────────────────

/// Server-side context compression.  When the context grows past
/// `trigger_tokens`, the server compresses it down to
/// `sliding_window.target_tokens`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextWindowCompressionConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sliding_window: Option<SlidingWindow>,
    /// Token count that triggers compression (default ≈ 80% of context limit).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlidingWindow {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_tokens: Option<u64>,
}

// ── Transcription ────────────────────────────────────────────────────────────

/// Presence-activated config — include an empty `{}` to enable transcription
/// for the corresponding direction (input or output).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioTranscriptionConfig {}

// ── Proactivity (v1alpha, Gemini 2.5) ────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProactivityConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proactive_audio: Option<bool>,
}

// ── History (Gemini 3.1) ─────────────────────────────────────────────────────

/// Controls how conversation history is bootstrapped.
///
/// On Gemini 3.1, `clientContent` can only be sent as initial history
/// (before the first `realtimeInput`).  Set `initial_history_in_client_content`
/// to `true` to enable this.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_history_in_client_content: Option<bool>,
}

// ── Tool definitions ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub function_declarations: Vec<FunctionDeclaration>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionDeclaration {
    pub name: String,
    pub description: String,
    /// JSON Schema object describing the function's parameters.
    pub parameters: serde_json::Value,
    /// Gemini 2.5: when to trigger this function relative to model output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduling: Option<FunctionScheduling>,
    /// Gemini 2.5: whether the function blocks model generation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behavior: Option<FunctionBehavior>,
}

/// When the function call is dispatched relative to model output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FunctionScheduling {
    /// Immediately interrupt model output (default).
    Interrupt,
    /// Wait until the model is idle.
    WhenIdle,
    /// Run silently without interrupting.
    Silent,
}

/// Whether the function response blocks continued model generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FunctionBehavior {
    /// Model continues generating while awaiting the response.
    NonBlocking,
}
