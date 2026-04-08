//! Discord host crate for a single-guild Gemini Live agent.
//!
//! This crate is intentionally narrower than a generic "bot utils" package:
//!
//! - `gemini-live` owns the Live API wire contract
//! - `gemini-live-runtime` owns session orchestration
//! - `gemini-live-discord` owns Discord-specific policy, setup, and gateway
//!   integration
//!
//! The initial pass establishes compileable host boundaries and the concrete
//! product contract for:
//!
//! - one configured guild
//! - one configured owner
//! - one configured voice-channel name
//! - one shared Gemini Live session across text and voice
//! - one concrete Serenity + Songbird host implementation

pub mod config;
pub mod error;
pub mod gateway;
pub mod policy;
pub mod service;
pub mod session;
pub mod setup;
pub mod voice;

pub use config::{DEFAULT_GEMINI_MODEL, DiscordBotConfig};
pub use error::{ConfigError, DiscordServiceError};
pub use gateway::gateway_intents;
pub use policy::BotConversationScope;
pub use service::{DiscordAgentService, DiscordServiceState, PreparedDiscordService};
pub use session::{
    DiscordConversationMemory, DiscordManagedRuntime, DiscordSessionManager, build_live_setup,
    build_runtime_config, new_managed_runtime, new_session_manager,
};
pub use setup::{
    SetupAction, TargetChannelSummary, ensure_target_voice_channel, plan_voice_channel,
};
pub use voice::{
    ActiveVoiceBridge, DISCORD_CAPTURE_SAMPLE_RATE, DiscordVoiceManager, MODEL_AUDIO_SAMPLE_RATE,
    VoiceBridge, VoiceSessionPlan,
};
