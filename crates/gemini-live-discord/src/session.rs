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
//! - `generationConfig.thinkingConfig = { thinkingLevel = "<resolved config value>" }`
//! - a profile-scoped system instruction describing the Discord host context
//! - optional `generationConfig.speechConfig.voiceConfig.prebuiltVoiceConfig`
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
    Part, PrebuiltVoiceConfig, SessionResumptionConfig, SetupConfig, SlidingWindow, SpeechConfig,
    ThinkingConfig, Tool, VoiceConfig,
};
use gemini_live_harness::{Harness, HarnessController, HarnessRuntimeBridge, ToolProvider};
use gemini_live_runtime::{
    GeminiSessionDriver, IdlePolicy, InMemoryConversationMemory, ManagedRuntime, RuntimeConfig,
    RuntimeEventReceiver, SessionManager,
};
use gemini_live_tools::timer::{TimerToolAdapter, TimerToolSelection};

use crate::config::{DiscordBotConfig, harness_profile_name};
use crate::error::DiscordServiceError;

pub type DiscordManagedRuntime = ManagedRuntime<GeminiSessionDriver>;
pub type DiscordHarnessController = HarnessController<TimerToolAdapter>;
pub type DiscordHarnessRuntimeBridge = HarnessRuntimeBridge<TimerToolAdapter>;
pub type DiscordConversationMemory = InMemoryConversationMemory;
pub type DiscordSessionManager = SessionManager<GeminiSessionDriver, DiscordConversationMemory>;

pub fn build_live_setup(config: &DiscordBotConfig) -> SetupConfig {
    build_live_setup_with_tools(config, discord_host_tools().advertised_tools())
}

fn build_live_setup_with_tools(config: &DiscordBotConfig, tools: Option<Vec<Tool>>) -> SetupConfig {
    SetupConfig {
        model: config.model.clone(),
        generation_config: Some(GenerationConfig {
            response_modalities: Some(vec![Modality::Audio]),
            thinking_config: Some(ThinkingConfig {
                thinking_level: Some(config.thinking_level),
                ..Default::default()
            }),
            speech_config: config.voice_name.as_ref().map(|voice_name| SpeechConfig {
                voice_config: VoiceConfig {
                    prebuilt_voice_config: PrebuiltVoiceConfig {
                        voice_name: voice_name.clone(),
                    },
                },
            }),
            ..Default::default()
        }),
        system_instruction: Some(discord_system_instruction(&config.system_instruction)),
        tools,
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

fn discord_system_instruction(text: &str) -> Content {
    Content {
        role: None,
        parts: vec![Part {
            text: Some(text.to_string()),
            inline_data: None,
        }],
    }
}

pub fn build_runtime_config(config: &DiscordBotConfig) -> RuntimeConfig {
    build_runtime_config_with_tools(config, discord_host_tools().advertised_tools())
}

fn build_runtime_config_with_tools(
    config: &DiscordBotConfig,
    tools: Option<Vec<Tool>>,
) -> RuntimeConfig {
    RuntimeConfig {
        session: SessionConfig {
            transport: TransportConfig {
                auth: Auth::ApiKey(config.gemini_api_key.clone()),
                ..Default::default()
            },
            setup: build_live_setup_with_tools(config, tools),
            reconnect: ReconnectPolicy::default(),
        },
    }
}

pub fn new_managed_runtime(
    config: &DiscordBotConfig,
) -> Result<
    (
        DiscordManagedRuntime,
        DiscordHarnessController,
        RuntimeEventReceiver,
    ),
    DiscordServiceError,
> {
    let harness = Harness::open_profile(&harness_profile_name(config.guild_id))?;
    new_managed_runtime_with_harness(config, harness)
}

pub fn new_session_manager(
    config: &DiscordBotConfig,
) -> Result<
    (
        DiscordSessionManager,
        DiscordHarnessController,
        RuntimeEventReceiver,
    ),
    DiscordServiceError,
> {
    let harness = Harness::open_profile(&harness_profile_name(config.guild_id))?;
    new_session_manager_with_harness(config, harness)
}

pub(crate) fn new_managed_runtime_with_harness(
    config: &DiscordBotConfig,
    harness: Harness,
) -> Result<
    (
        DiscordManagedRuntime,
        DiscordHarnessController,
        RuntimeEventReceiver,
    ),
    DiscordServiceError,
> {
    let harness_controller = HarnessController::with_host_tools(harness, discord_host_tools())?;
    let setup_tools = harness_controller.advertised_tools();
    let (runtime, runtime_events) = ManagedRuntime::new(
        build_runtime_config_with_tools(config, setup_tools),
        GeminiSessionDriver,
    );
    Ok((runtime, harness_controller, runtime_events))
}

pub(crate) fn new_session_manager_with_harness(
    config: &DiscordBotConfig,
    harness: Harness,
) -> Result<
    (
        DiscordSessionManager,
        DiscordHarnessController,
        RuntimeEventReceiver,
    ),
    DiscordServiceError,
> {
    let (runtime, harness_controller, runtime_events) =
        new_managed_runtime_with_harness(config, harness)?;
    Ok((
        SessionManager::new(
            runtime,
            InMemoryConversationMemory::new(),
            IdlePolicy {
                idle_timeout: config.idle_timeout,
                max_recent_turns: config.max_recent_turns,
                ..Default::default()
            },
        ),
        harness_controller,
        runtime_events,
    ))
}

fn discord_host_tools() -> TimerToolAdapter {
    TimerToolAdapter::new(TimerToolSelection::default())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use gemini_live::transport::Auth;
    use gemini_live::types::ThinkingLevel;
    use gemini_live_harness::Harness;
    use serenity::all::{GuildId, UserId};

    use super::*;
    use crate::config::DEFAULT_DISCORD_SYSTEM_INSTRUCTION;

    fn config() -> DiscordBotConfig {
        DiscordBotConfig {
            discord_bot_token: "discord-token".into(),
            gemini_api_key: "gemini-key".into(),
            guild_id: GuildId::new(123),
            owner_user_id: UserId::new(456),
            voice_channel_name: "gemini-live".into(),
            model: "models/custom-live".into(),
            thinking_level: ThinkingLevel::High,
            system_instruction: DEFAULT_DISCORD_SYSTEM_INSTRUCTION.into(),
            voice_name: None,
            idle_timeout: std::time::Duration::from_secs(90),
            max_recent_turns: 24,
        }
    }

    fn temp_harness() -> Harness {
        let path = std::env::temp_dir().join(format!(
            "gemini-live-discord-harness-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time before unix epoch")
                .as_nanos()
        ));
        Harness::open(path).expect("open test harness")
    }

    #[test]
    fn builds_audio_first_live_setup() {
        let setup = build_live_setup(&config());

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
                .generation_config
                .as_ref()
                .and_then(|config| config.thinking_config.as_ref())
                .and_then(|config| config.thinking_level.as_ref()),
            Some(&ThinkingLevel::High)
        );
        assert_eq!(
            setup
                .system_instruction
                .as_ref()
                .and_then(|content| content.parts.first().and_then(|part| part.text.as_deref())),
            Some(DEFAULT_DISCORD_SYSTEM_INSTRUCTION)
        );
        assert!(setup.input_audio_transcription.is_some());
        assert!(setup.output_audio_transcription.is_some());
        assert!(setup.session_resumption.is_some());
        let tools = setup.tools.as_ref().expect("setup tools");
        assert_eq!(tools.len(), 1);
        let Tool::FunctionDeclarations(functions) = &tools[0] else {
            panic!("expected function declarations");
        };
        assert_eq!(functions[0].name, "set_timer");
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
    fn build_live_setup_includes_optional_voice_and_transcription_settings() {
        let mut config = config();
        config.voice_name = Some("Kore".into());
        config.thinking_level = ThinkingLevel::Low;
        let setup = build_live_setup(&config);

        let generation_config = setup.generation_config.as_ref().expect("generation config");
        assert_eq!(
            generation_config
                .thinking_config
                .as_ref()
                .and_then(|config| config.thinking_level.as_ref()),
            Some(&ThinkingLevel::Low)
        );
        assert_eq!(
            generation_config.speech_config.as_ref().map(|speech| {
                speech
                    .voice_config
                    .prebuilt_voice_config
                    .voice_name
                    .as_str()
            }),
            Some("Kore")
        );
        assert!(setup.input_audio_transcription.is_some());
        assert!(setup.output_audio_transcription.is_some());
    }

    #[test]
    fn managed_runtime_advertises_timer_tool() {
        let (runtime, _harness_controller, _events) =
            new_managed_runtime_with_harness(&config(), temp_harness()).expect("runtime");

        let tools = runtime.desired_setup().tools.as_ref().expect("setup tools");
        let Tool::FunctionDeclarations(functions) = &tools[0] else {
            panic!("expected function declarations");
        };
        assert_eq!(functions[0].name, "set_timer");
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
        let (manager, _harness_controller, _events) =
            new_session_manager_with_harness(&config(), temp_harness()).expect("session manager");

        assert_eq!(
            manager.idle_policy().idle_timeout,
            std::time::Duration::from_secs(90)
        );
        assert_eq!(manager.idle_policy().max_recent_turns, 24);
    }
}
