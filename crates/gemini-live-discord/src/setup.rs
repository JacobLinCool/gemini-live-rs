//! Planning and execution helpers for target voice-channel discovery.
//!
//! The Discord host always converges toward one configured voice channel name in
//! one configured guild:
//!
//! - reuse an existing voice channel with that exact name when possible
//! - otherwise create a new voice channel with that name
//!
//! This module is the natural home for both the pure planning step and the
//! Discord HTTP operation that executes it.

use std::sync::Arc;

use serenity::all::{ChannelId, ChannelType, CreateChannel, GuildChannel, GuildId};
use serenity::http::Http;

/// Minimal channel snapshot used for setup planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetChannelSummary {
    pub id: ChannelId,
    pub name: String,
    pub kind: ChannelType,
}

/// Next setup step for the configured target voice channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupAction {
    UseExisting(ChannelId),
    CreateNamedVoiceChannel(String),
}

pub async fn ensure_target_voice_channel(
    http: &Arc<Http>,
    guild_id: GuildId,
    target_name: &str,
) -> Result<GuildChannel, serenity::Error> {
    let channels = guild_id.channels(http).await?;
    let summaries = channels
        .values()
        .map(|channel| TargetChannelSummary {
            id: channel.id,
            name: channel.name.clone(),
            kind: channel.kind,
        })
        .collect::<Vec<_>>();

    match plan_voice_channel(target_name, &summaries) {
        SetupAction::UseExisting(channel_id) => Ok(channels
            .get(&channel_id)
            .expect("planned existing channel must be in the fetched channel map")
            .clone()),
        SetupAction::CreateNamedVoiceChannel(name) => {
            guild_id
                .create_channel(http, CreateChannel::new(name).kind(ChannelType::Voice))
                .await
        }
    }
}

pub fn plan_voice_channel(target_name: &str, channels: &[TargetChannelSummary]) -> SetupAction {
    channels
        .iter()
        .find(|channel| channel.kind == ChannelType::Voice && channel.name == target_name)
        .map(|channel| SetupAction::UseExisting(channel.id))
        .unwrap_or_else(|| SetupAction::CreateNamedVoiceChannel(target_name.to_owned()))
}

#[cfg(test)]
mod tests {
    use serenity::all::{ChannelId, ChannelType};

    use super::*;

    #[test]
    fn reuses_existing_voice_channel_when_names_match() {
        let channels = vec![
            TargetChannelSummary {
                id: ChannelId::new(1),
                name: "general".into(),
                kind: ChannelType::Voice,
            },
            TargetChannelSummary {
                id: ChannelId::new(2),
                name: "gemini-live".into(),
                kind: ChannelType::Voice,
            },
        ];

        assert_eq!(
            plan_voice_channel("gemini-live", &channels),
            SetupAction::UseExisting(ChannelId::new(2))
        );
    }

    #[test]
    fn ignores_non_voice_channels_with_same_name() {
        let channels = vec![TargetChannelSummary {
            id: ChannelId::new(1),
            name: "gemini-live".into(),
            kind: ChannelType::Text,
        }];

        assert_eq!(
            plan_voice_channel("gemini-live", &channels),
            SetupAction::CreateNamedVoiceChannel("gemini-live".into())
        );
    }
}
