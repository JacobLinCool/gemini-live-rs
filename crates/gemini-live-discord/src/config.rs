//! Environment-backed deployment config plus profile-scoped Discord session
//! settings.
//!
//! Deployment secrets and Discord identities stay in process environment:
//!
//! - `DISCORD_BOT_TOKEN`
//! - `GEMINI_API_KEY`
//! - `DISCORD_GUILD_ID`
//! - `DISCORD_OWNER_USER_ID`
//!
//! Session-facing settings are persisted inside the harness profile for that
//! guild:
//!
//! - `~/.gemini-live/harness/profiles/discord-guild-<guild_id>/config/discord.json`
//! - model / system instruction / thinking level / voice / idle policy live in
//!   that profile and may be seeded from environment on first run
//!
//! Startup precedence for those persisted session settings is:
//!
//! 1. environment override
//! 2. stored profile value
//! 3. built-in default

use std::path::PathBuf;
use std::time::Duration;

use gemini_live::types::ThinkingLevel;
use gemini_live_harness::HarnessProfileStore;
use serde::{Deserialize, Serialize};
use serenity::all::{GuildId, UserId};

use crate::error::ConfigError;

pub const DEFAULT_GEMINI_MODEL: &str = "models/gemini-3.1-flash-live-preview";
pub const DEFAULT_THINKING_LEVEL: ThinkingLevel = ThinkingLevel::High;
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 10 * 60;
pub const DEFAULT_MAX_RECENT_TURNS: usize = 16;
pub const DEFAULT_DISCORD_SYSTEM_INSTRUCTION: &str = concat!(
    "You are an assistant talking with users on Discord. ",
    "The conversation happens inside a Discord voice channel and its linked text chat. ",
    "Some user input arrives as Discord chat messages and some arrives as live voice audio. ",
    "Reply naturally for a Discord conversation, keeping responses clear and conversational. ",
);

/// Persisted Discord session preferences scoped to one harness profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiscordProfileConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice_channel_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_recent_turns: Option<usize>,
}

pub fn harness_profile_name(guild_id: GuildId) -> String {
    format!("discord-guild-{}", guild_id.get())
}

/// Fully resolved startup configuration for the single-guild Discord bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordBotConfig {
    pub discord_bot_token: String,
    pub gemini_api_key: String,
    pub guild_id: GuildId,
    pub owner_user_id: UserId,
    pub voice_channel_name: String,
    pub model: String,
    pub thinking_level: ThinkingLevel,
    pub system_instruction: String,
    pub voice_name: Option<String>,
    pub idle_timeout: Duration,
    pub max_recent_turns: usize,
}

impl DiscordBotConfig {
    /// Resolve deployment secrets from environment, then merge session settings
    /// with the guild-scoped harness profile and persist the resolved result.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_with_profile_base_root(|key| std::env::var(key).ok(), None)
    }

    /// Resolve configuration from a caller-provided environment reader.
    ///
    /// Tests use this with a temporary harness base root so profile state does
    /// not leak into the caller's real home directory.
    pub fn from_env_with(
        read_env: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        Self::from_env_with_profile_base_root(read_env, None)
    }

    fn from_env_with_profile_base_root(
        mut read_env: impl FnMut(&str) -> Option<String>,
        base_root: Option<PathBuf>,
    ) -> Result<Self, ConfigError> {
        let discord_bot_token = required_nonempty(&mut read_env, "DISCORD_BOT_TOKEN")?;
        let gemini_api_key = required_nonempty(&mut read_env, "GEMINI_API_KEY")?;
        let guild_id = parse_discord_id(
            required_nonempty(&mut read_env, "DISCORD_GUILD_ID")?,
            "DISCORD_GUILD_ID",
            GuildId::new,
        )?;
        let owner_user_id = parse_discord_id(
            required_nonempty(&mut read_env, "DISCORD_OWNER_USER_ID")?,
            "DISCORD_OWNER_USER_ID",
            UserId::new,
        )?;

        let profile_name = harness_profile_name(guild_id);
        let mut profile_store = load_profile_store(&profile_name, base_root)?;
        let stored_profile = profile_store.active_profile().clone();

        let voice_channel_name = optional_nonempty(&mut read_env, "DISCORD_VOICE_CHANNEL_NAME")?
            .or(stored_profile.voice_channel_name.clone())
            .ok_or(ConfigError::MissingEnv {
                key: "DISCORD_VOICE_CHANNEL_NAME",
            })?;
        let model = optional_nonempty(&mut read_env, "GEMINI_MODEL")?
            .or(stored_profile.model.clone())
            .unwrap_or_else(|| DEFAULT_GEMINI_MODEL.to_string());
        let thinking_level = optional_thinking_level(&mut read_env, "GEMINI_THINKING_LEVEL")?
            .or(stored_profile.thinking_level)
            .unwrap_or(DEFAULT_THINKING_LEVEL);
        let system_instruction = optional_nonempty(&mut read_env, "GEMINI_SYSTEM_INSTRUCTION")?
            .or(stored_profile.system_instruction.clone())
            .unwrap_or_else(|| DEFAULT_DISCORD_SYSTEM_INSTRUCTION.to_string());
        let voice_name = optional_nonempty(&mut read_env, "GEMINI_VOICE_NAME")?
            .or(stored_profile.voice_name.clone());
        let idle_timeout_secs =
            optional_positive_u64(&mut read_env, "DISCORD_SESSION_IDLE_TIMEOUT_SECS")?
                .or(stored_profile.idle_timeout_secs)
                .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);
        let max_recent_turns =
            optional_positive_usize(&mut read_env, "DISCORD_SESSION_MAX_RECENT_TURNS")?
                .or(stored_profile.max_recent_turns)
                .unwrap_or(DEFAULT_MAX_RECENT_TURNS);

        profile_store
            .persist_profile(DiscordProfileConfig {
                voice_channel_name: Some(voice_channel_name.clone()),
                model: Some(model.clone()),
                thinking_level: Some(thinking_level),
                system_instruction: Some(system_instruction.clone()),
                voice_name: voice_name.clone(),
                idle_timeout_secs: Some(idle_timeout_secs),
                max_recent_turns: Some(max_recent_turns),
            })
            .map_err(profile_store_error)?;

        Ok(Self {
            discord_bot_token,
            gemini_api_key,
            guild_id,
            owner_user_id,
            voice_channel_name,
            model,
            thinking_level,
            system_instruction,
            voice_name,
            idle_timeout: Duration::from_secs(idle_timeout_secs),
            max_recent_turns,
        })
    }
}

fn required_nonempty(
    read_env: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
) -> Result<String, ConfigError> {
    match read_env(key) {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                Err(ConfigError::EmptyEnv { key })
            } else {
                Ok(trimmed.to_owned())
            }
        }
        None => Err(ConfigError::MissingEnv { key }),
    }
}

fn optional_nonempty(
    read_env: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
) -> Result<Option<String>, ConfigError> {
    match read_env(key) {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                Err(ConfigError::EmptyEnv { key })
            } else {
                Ok(Some(trimmed.to_owned()))
            }
        }
        None => Ok(None),
    }
}

fn optional_thinking_level(
    read_env: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
) -> Result<Option<ThinkingLevel>, ConfigError> {
    match read_env(key) {
        Some(raw) => {
            if raw.trim().is_empty() {
                return Err(ConfigError::EmptyEnv { key });
            }
            raw.parse::<ThinkingLevel>()
                .map(Some)
                .map_err(|_| ConfigError::InvalidThinkingLevel { key, value: raw })
        }
        None => Ok(None),
    }
}

fn parse_discord_id<T>(
    raw: String,
    key: &'static str,
    ctor: impl FnOnce(u64) -> T,
) -> Result<T, ConfigError> {
    let parsed = raw
        .parse::<u64>()
        .map_err(|_| ConfigError::InvalidDiscordId {
            key,
            value: raw.clone(),
        })?;
    if parsed == 0 {
        return Err(ConfigError::InvalidDiscordId { key, value: raw });
    }
    Ok(ctor(parsed))
}

fn optional_positive_u64(
    read_env: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
) -> Result<Option<u64>, ConfigError> {
    match read_env(key) {
        Some(raw) => parse_positive_u64(raw, key).map(Some),
        None => Ok(None),
    }
}

fn optional_positive_usize(
    read_env: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
) -> Result<Option<usize>, ConfigError> {
    match read_env(key) {
        Some(raw) => parse_positive_usize(raw, key).map(Some),
        None => Ok(None),
    }
}

fn parse_positive_u64(raw: String, key: &'static str) -> Result<u64, ConfigError> {
    let parsed = raw
        .trim()
        .parse::<u64>()
        .map_err(|_| ConfigError::InvalidPositiveInt {
            key,
            value: raw.clone(),
        })?;
    if parsed == 0 {
        return Err(ConfigError::InvalidPositiveInt { key, value: raw });
    }
    Ok(parsed)
}

fn parse_positive_usize(raw: String, key: &'static str) -> Result<usize, ConfigError> {
    let parsed = raw
        .trim()
        .parse::<usize>()
        .map_err(|_| ConfigError::InvalidPositiveInt {
            key,
            value: raw.clone(),
        })?;
    if parsed == 0 {
        return Err(ConfigError::InvalidPositiveInt { key, value: raw });
    }
    Ok(parsed)
}

fn load_profile_store(
    profile_name: &str,
    base_root: Option<PathBuf>,
) -> Result<HarnessProfileStore<DiscordProfileConfig>, ConfigError> {
    match base_root {
        Some(base_root) => {
            HarnessProfileStore::load_at_base_root(base_root, "discord", Some(profile_name))
        }
        None => HarnessProfileStore::load("discord", Some(profile_name)),
    }
    .map_err(profile_store_error)
}

fn profile_store_error(error: gemini_live_harness::HarnessError) -> ConfigError {
    ConfigError::ProfileStore {
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_base_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "gemini-live-discord-config-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time before unix epoch")
                .as_nanos()
        ))
    }

    fn resolve(
        values: &[(&str, &str)],
        base_root: PathBuf,
    ) -> Result<DiscordBotConfig, ConfigError> {
        let env = values
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect::<HashMap<_, _>>();
        DiscordBotConfig::from_env_with_profile_base_root(
            |key| env.get(key).cloned(),
            Some(base_root),
        )
    }

    #[test]
    fn resolves_required_environment_and_persists_profile_settings() {
        let base_root = temp_base_root();
        let config = resolve(
            &[
                ("DISCORD_BOT_TOKEN", "discord-token"),
                ("GEMINI_API_KEY", "gemini-key"),
                ("DISCORD_GUILD_ID", "123"),
                ("DISCORD_OWNER_USER_ID", "456"),
                ("DISCORD_VOICE_CHANNEL_NAME", "gemini-live"),
            ],
            base_root.clone(),
        )
        .expect("config should resolve");

        assert_eq!(config.discord_bot_token, "discord-token");
        assert_eq!(config.gemini_api_key, "gemini-key");
        assert_eq!(config.guild_id, GuildId::new(123));
        assert_eq!(config.owner_user_id, UserId::new(456));
        assert_eq!(config.voice_channel_name, "gemini-live");
        assert_eq!(config.model, DEFAULT_GEMINI_MODEL);
        assert_eq!(config.thinking_level, DEFAULT_THINKING_LEVEL);
        assert_eq!(
            config.system_instruction,
            DEFAULT_DISCORD_SYSTEM_INSTRUCTION
        );
        assert_eq!(config.voice_name, None);
        assert_eq!(
            config.idle_timeout,
            Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS)
        );
        assert_eq!(config.max_recent_turns, DEFAULT_MAX_RECENT_TURNS);

        let persisted = std::fs::read_to_string(
            base_root
                .join("profiles")
                .join("discord-guild-123")
                .join("config")
                .join("discord.json"),
        )
        .expect("read persisted profile");
        let persisted: DiscordProfileConfig =
            serde_json::from_str(&persisted).expect("parse persisted profile");
        assert_eq!(persisted.voice_channel_name.as_deref(), Some("gemini-live"));
        assert_eq!(persisted.model.as_deref(), Some(DEFAULT_GEMINI_MODEL));
        assert_eq!(persisted.thinking_level, Some(DEFAULT_THINKING_LEVEL));
    }

    #[test]
    fn rejects_missing_required_values() {
        let base_root = temp_base_root();
        let error = resolve(
            &[
                ("GEMINI_API_KEY", "gemini-key"),
                ("DISCORD_GUILD_ID", "123"),
                ("DISCORD_OWNER_USER_ID", "456"),
                ("DISCORD_VOICE_CHANNEL_NAME", "gemini-live"),
            ],
            base_root,
        )
        .expect_err("missing bot token should fail");

        assert_eq!(
            error,
            ConfigError::MissingEnv {
                key: "DISCORD_BOT_TOKEN"
            }
        );
    }

    #[test]
    fn rejects_invalid_discord_ids() {
        let base_root = temp_base_root();
        let error = resolve(
            &[
                ("DISCORD_BOT_TOKEN", "discord-token"),
                ("GEMINI_API_KEY", "gemini-key"),
                ("DISCORD_GUILD_ID", "not-a-number"),
                ("DISCORD_OWNER_USER_ID", "456"),
                ("DISCORD_VOICE_CHANNEL_NAME", "gemini-live"),
            ],
            base_root,
        )
        .expect_err("invalid guild id should fail");

        assert_eq!(
            error,
            ConfigError::InvalidDiscordId {
                key: "DISCORD_GUILD_ID",
                value: "not-a-number".into(),
            }
        );
    }

    #[test]
    fn environment_overrides_seed_profile_and_later_runs_reuse_it() {
        let base_root = temp_base_root();
        let config = resolve(
            &[
                ("DISCORD_BOT_TOKEN", "discord-token"),
                ("GEMINI_API_KEY", "gemini-key"),
                ("DISCORD_GUILD_ID", "123"),
                ("DISCORD_OWNER_USER_ID", "456"),
                ("DISCORD_VOICE_CHANNEL_NAME", "gemini-live"),
                ("GEMINI_MODEL", "  models/custom-live  "),
                ("GEMINI_THINKING_LEVEL", "medium"),
                ("GEMINI_SYSTEM_INSTRUCTION", "  Be concise on Discord.  "),
                ("GEMINI_VOICE_NAME", "  Kore  "),
                ("DISCORD_SESSION_IDLE_TIMEOUT_SECS", "90"),
                ("DISCORD_SESSION_MAX_RECENT_TURNS", "24"),
            ],
            base_root.clone(),
        )
        .expect("config should resolve");

        assert_eq!(config.model, "models/custom-live");
        assert_eq!(config.thinking_level, ThinkingLevel::Medium);
        assert_eq!(config.system_instruction, "Be concise on Discord.");
        assert_eq!(config.voice_name.as_deref(), Some("Kore"));
        assert_eq!(config.idle_timeout, Duration::from_secs(90));
        assert_eq!(config.max_recent_turns, 24);

        let reused = resolve(
            &[
                ("DISCORD_BOT_TOKEN", "discord-token"),
                ("GEMINI_API_KEY", "gemini-key"),
                ("DISCORD_GUILD_ID", "123"),
                ("DISCORD_OWNER_USER_ID", "456"),
            ],
            base_root,
        )
        .expect("config should resolve from stored profile");
        assert_eq!(reused.voice_channel_name, "gemini-live");
        assert_eq!(reused.model, "models/custom-live");
        assert_eq!(reused.thinking_level, ThinkingLevel::Medium);
        assert_eq!(reused.system_instruction, "Be concise on Discord.");
        assert_eq!(reused.voice_name.as_deref(), Some("Kore"));
        assert_eq!(reused.idle_timeout, Duration::from_secs(90));
        assert_eq!(reused.max_recent_turns, 24);
    }

    #[test]
    fn rejects_invalid_thinking_level() {
        let base_root = temp_base_root();
        let error = resolve(
            &[
                ("DISCORD_BOT_TOKEN", "discord-token"),
                ("GEMINI_API_KEY", "gemini-key"),
                ("DISCORD_GUILD_ID", "123"),
                ("DISCORD_OWNER_USER_ID", "456"),
                ("DISCORD_VOICE_CHANNEL_NAME", "gemini-live"),
                ("GEMINI_THINKING_LEVEL", "extreme"),
            ],
            base_root,
        )
        .expect_err("invalid thinking level should fail");

        assert_eq!(
            error,
            ConfigError::InvalidThinkingLevel {
                key: "GEMINI_THINKING_LEVEL",
                value: "extreme".into(),
            }
        );
    }

    #[test]
    fn rejects_empty_thinking_level() {
        let base_root = temp_base_root();
        let error = resolve(
            &[
                ("DISCORD_BOT_TOKEN", "discord-token"),
                ("GEMINI_API_KEY", "gemini-key"),
                ("DISCORD_GUILD_ID", "123"),
                ("DISCORD_OWNER_USER_ID", "456"),
                ("DISCORD_VOICE_CHANNEL_NAME", "gemini-live"),
                ("GEMINI_THINKING_LEVEL", "   "),
            ],
            base_root,
        )
        .expect_err("empty thinking level should fail");

        assert_eq!(
            error,
            ConfigError::EmptyEnv {
                key: "GEMINI_THINKING_LEVEL"
            }
        );
    }

    #[test]
    fn missing_voice_channel_requires_env_or_persisted_profile() {
        let base_root = temp_base_root();
        let error = resolve(
            &[
                ("DISCORD_BOT_TOKEN", "discord-token"),
                ("GEMINI_API_KEY", "gemini-key"),
                ("DISCORD_GUILD_ID", "123"),
                ("DISCORD_OWNER_USER_ID", "456"),
            ],
            base_root,
        )
        .expect_err("voice channel should still be required on first run");
        assert_eq!(
            error,
            ConfigError::MissingEnv {
                key: "DISCORD_VOICE_CHANNEL_NAME"
            }
        );
    }
}
