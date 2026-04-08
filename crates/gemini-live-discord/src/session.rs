//! Gemini Live runtime bootstrap for the Discord host.
//!
//! The Discord product wants one shared Live session that can serve:
//!
//! - text messages posted in the configured voice channel chat
//! - live audio turns when the owner is present in voice
//!
//! This module therefore builds an audio-first Live setup:
//!
//! - `responseModalities = ["AUDIO"]`
//! - a built-in system instruction describing the Discord host context
//! - `inputAudioTranscription = {}`
//! - `outputAudioTranscription = {}`
//! - `sessionResumption = {}`
//! - `contextWindowCompression = { slidingWindow: {} }`
//!
//! Text chat still uses the same session via `send_text`, while text replies
//! can be projected from the model's output transcription stream. Initial
//! history mode is enabled only on fresh rehydrate wakes, not on every session.

use gemini_live::session::{ReconnectPolicy, SessionConfig};
use gemini_live::transport::{Auth, TransportConfig};
use gemini_live::types::{
    AudioTranscriptionConfig, Content, ContextWindowCompressionConfig, GenerationConfig, Modality,
    Part, SessionResumptionConfig, SetupConfig, SlidingWindow,
};
use gemini_live_runtime::{
    GeminiSessionDriver, IdlePolicy, InMemoryConversationMemory, ManagedRuntime, NoopToolAdapter,
    RuntimeConfig, RuntimeEventReceiver, SessionManager,
};

use crate::config::DiscordBotConfig;

pub type DiscordManagedRuntime = ManagedRuntime<GeminiSessionDriver, NoopToolAdapter>;
pub type DiscordConversationMemory = InMemoryConversationMemory;
pub type DiscordSessionManager =
    SessionManager<GeminiSessionDriver, NoopToolAdapter, DiscordConversationMemory>;

const DISCORD_SYSTEM_INSTRUCTION: &str = concat!(
    "You are an assistant talking with users on Discord. ",
    "The conversation happens inside a Discord voice channel and its linked text chat. ",
    "Some user input arrives as Discord chat messages and some arrives as live voice audio. ",
    "Reply naturally for a Discord conversation, keeping responses clear and conversational. ",
);

pub fn build_live_setup(model: impl Into<String>) -> SetupConfig {
    SetupConfig {
        model: model.into(),
        generation_config: Some(GenerationConfig {
            response_modalities: Some(vec![Modality::Audio]),
            ..Default::default()
        }),
        system_instruction: Some(discord_system_instruction()),
        input_audio_transcription: Some(AudioTranscriptionConfig {}),
        output_audio_transcription: Some(AudioTranscriptionConfig {}),
        session_resumption: Some(SessionResumptionConfig::default()),
        context_window_compression: Some(ContextWindowCompressionConfig {
            sliding_window: Some(SlidingWindow::default()),
            trigger_tokens: None,
        }),
        ..Default::default()
    }
}

fn discord_system_instruction() -> Content {
    Content {
        role: None,
        parts: vec![Part {
            text: Some(DISCORD_SYSTEM_INSTRUCTION.to_string()),
            inline_data: None,
        }],
    }
}

pub fn build_runtime_config(config: &DiscordBotConfig) -> RuntimeConfig {
    RuntimeConfig {
        session: SessionConfig {
            transport: TransportConfig {
                auth: Auth::ApiKey(config.gemini_api_key.clone()),
                ..Default::default()
            },
            setup: build_live_setup(config.model.clone()),
            reconnect: ReconnectPolicy::default(),
        },
    }
}

pub fn new_managed_runtime(
    config: &DiscordBotConfig,
) -> (DiscordManagedRuntime, RuntimeEventReceiver) {
    ManagedRuntime::new(
        build_runtime_config(config),
        GeminiSessionDriver,
        NoopToolAdapter,
    )
}

pub fn new_session_manager(
    config: &DiscordBotConfig,
) -> (DiscordSessionManager, RuntimeEventReceiver) {
    let (runtime, runtime_events) = new_managed_runtime(config);
    (
        SessionManager::new(
            runtime,
            InMemoryConversationMemory::new(),
            IdlePolicy {
                idle_timeout: config.idle_timeout,
                max_recent_turns: config.max_recent_turns,
                ..Default::default()
            },
        ),
        runtime_events,
    )
}

#[cfg(test)]
mod tests {
    use gemini_live::transport::Auth;
    use serenity::all::{GuildId, UserId};

    use super::*;

    fn config() -> DiscordBotConfig {
        DiscordBotConfig {
            discord_bot_token: "discord-token".into(),
            gemini_api_key: "gemini-key".into(),
            guild_id: GuildId::new(123),
            owner_user_id: UserId::new(456),
            voice_channel_name: "gemini-live".into(),
            model: "models/custom-live".into(),
            idle_timeout: std::time::Duration::from_secs(90),
            max_recent_turns: 24,
        }
    }

    #[test]
    fn builds_audio_first_live_setup() {
        let setup = build_live_setup("models/custom-live");

        assert_eq!(setup.model, "models/custom-live");
        assert_eq!(
            setup
                .generation_config
                .as_ref()
                .and_then(|config| config.response_modalities.as_ref())
                .expect("response modalities"),
            &vec![Modality::Audio]
        );
        assert_eq!(
            setup
                .system_instruction
                .as_ref()
                .and_then(|content| content.parts.first().and_then(|part| part.text.as_deref())),
            Some(DISCORD_SYSTEM_INSTRUCTION)
        );
        assert!(setup.input_audio_transcription.is_some());
        assert!(setup.output_audio_transcription.is_some());
        assert!(setup.session_resumption.is_some());
        assert_eq!(
            setup.context_window_compression,
            Some(ContextWindowCompressionConfig {
                sliding_window: Some(SlidingWindow::default()),
                trigger_tokens: None,
            })
        );
        assert!(setup.history_config.is_none());
    }

    #[test]
    fn runtime_config_uses_gemini_api_key_auth() {
        let runtime = build_runtime_config(&config());

        match runtime.session.transport.auth {
            Auth::ApiKey(ref api_key) => assert_eq!(api_key, "gemini-key"),
            ref other => panic!("expected ApiKey auth, got {other:?}"),
        }
    }

    #[test]
    fn session_manager_uses_configured_idle_policy() {
        let (manager, _events) = new_session_manager(&config());

        assert_eq!(
            manager.idle_policy().idle_timeout,
            std::time::Duration::from_secs(90)
        );
        assert_eq!(manager.idle_policy().max_recent_turns, 24);
    }
}
