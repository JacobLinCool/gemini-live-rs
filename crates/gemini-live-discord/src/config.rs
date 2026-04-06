//! Environment-backed configuration for the Discord host.
//!
//! This module is the canonical home for the bot's startup contract:
//!
//! - which environment variables are required
//! - which values are optional
//! - how Discord ids are validated
//! - what default Gemini model is used when the caller does not override it

use serenity::all::{GuildId, UserId};

use crate::error::ConfigError;

pub const DEFAULT_GEMINI_MODEL: &str = "models/gemini-3.1-flash-live-preview";

/// Startup configuration for the single-guild Discord bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordBotConfig {
    pub discord_bot_token: String,
    pub gemini_api_key: String,
    pub guild_id: GuildId,
    pub owner_user_id: UserId,
    pub voice_channel_name: String,
    pub model: String,
}

impl DiscordBotConfig {
    /// Resolve configuration from process environment variables.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_with(|key| std::env::var(key).ok())
    }

    /// Resolve configuration from a caller-provided environment reader.
    ///
    /// This exists so tests can validate the startup contract without mutating
    /// process-global environment state.
    pub fn from_env_with(
        mut read_env: impl FnMut(&str) -> Option<String>,
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
        let voice_channel_name = required_nonempty(&mut read_env, "DISCORD_VOICE_CHANNEL_NAME")?;
        let model = read_env("GEMINI_MODEL")
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_GEMINI_MODEL.to_string());

        Ok(Self {
            discord_bot_token,
            gemini_api_key,
            guild_id,
            owner_user_id,
            voice_channel_name,
            model,
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn resolve(values: &[(&str, &str)]) -> Result<DiscordBotConfig, ConfigError> {
        let env = values
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect::<HashMap<_, _>>();
        DiscordBotConfig::from_env_with(|key| env.get(key).cloned())
    }

    #[test]
    fn resolves_required_environment() {
        let config = resolve(&[
            ("DISCORD_BOT_TOKEN", "discord-token"),
            ("GEMINI_API_KEY", "gemini-key"),
            ("DISCORD_GUILD_ID", "123"),
            ("DISCORD_OWNER_USER_ID", "456"),
            ("DISCORD_VOICE_CHANNEL_NAME", "gemini-live"),
        ])
        .expect("config should resolve");

        assert_eq!(config.discord_bot_token, "discord-token");
        assert_eq!(config.gemini_api_key, "gemini-key");
        assert_eq!(config.guild_id, GuildId::new(123));
        assert_eq!(config.owner_user_id, UserId::new(456));
        assert_eq!(config.voice_channel_name, "gemini-live");
        assert_eq!(config.model, DEFAULT_GEMINI_MODEL);
    }

    #[test]
    fn rejects_missing_required_values() {
        let error = resolve(&[
            ("GEMINI_API_KEY", "gemini-key"),
            ("DISCORD_GUILD_ID", "123"),
            ("DISCORD_OWNER_USER_ID", "456"),
            ("DISCORD_VOICE_CHANNEL_NAME", "gemini-live"),
        ])
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
        let error = resolve(&[
            ("DISCORD_BOT_TOKEN", "discord-token"),
            ("GEMINI_API_KEY", "gemini-key"),
            ("DISCORD_GUILD_ID", "not-a-number"),
            ("DISCORD_OWNER_USER_ID", "456"),
            ("DISCORD_VOICE_CHANNEL_NAME", "gemini-live"),
        ])
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
    fn trims_optional_model_override() {
        let config = resolve(&[
            ("DISCORD_BOT_TOKEN", "discord-token"),
            ("GEMINI_API_KEY", "gemini-key"),
            ("DISCORD_GUILD_ID", "123"),
            ("DISCORD_OWNER_USER_ID", "456"),
            ("DISCORD_VOICE_CHANNEL_NAME", "gemini-live"),
            ("GEMINI_MODEL", "  models/custom-live  "),
        ])
        .expect("config should resolve");

        assert_eq!(config.model, "models/custom-live");
    }
}
