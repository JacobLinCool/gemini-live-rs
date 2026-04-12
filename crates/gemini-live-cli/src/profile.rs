//! Persistent named profile storage for the CLI.
//!
//! The harness owns the filesystem-level profile mechanism. Each CLI profile
//! now lives under one harness profile root, and selecting a profile changes
//! both the persisted CLI config path and the durable harness state root used
//! for tasks, notifications, and memory.

use std::collections::BTreeMap;
use std::io;

use gemini_live::types::ThinkingLevel;
use gemini_live_harness::{Harness, HarnessProfileStore};
use serde::{Deserialize, Serialize};

use crate::tooling::ToolProfile;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PersistentBackend {
    Gemini,
    Vertex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PersistentVertexAuthMode {
    Static,
    Adc,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScreenShareProfile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_id: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProfileConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<PersistentBackend>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gemini_api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vertex_location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vertex_auth: Option<PersistentVertexAuthMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vertex_ai_access_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolProfile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mic_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speak_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_share: Option<ScreenShareProfile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
struct CliProfileConfig {
    #[serde(default)]
    profiles: BTreeMap<String, ProfileConfig>,
}

pub struct ProfileStore {
    inner: HarnessProfileStore<CliProfileConfig>,
}

impl ProfileStore {
    pub fn load(profile_override: Option<&str>) -> io::Result<Self> {
        let mut inner =
            HarnessProfileStore::load("cli", profile_override).map_err(into_io_error)?;
        let active_profile = inner.active_profile_name().to_string();
        inner
            .update_active_profile(|config: &mut CliProfileConfig| {
                config.profiles.entry(active_profile).or_default();
            })
            .map_err(into_io_error)?;
        Ok(Self { inner })
    }

    pub fn active_profile_name(&self) -> &str {
        self.inner.active_profile_name()
    }

    pub fn active_profile(&self) -> &ProfileConfig {
        self.inner
            .active_profile()
            .profiles
            .get(self.inner.active_profile_name())
            .expect("active profile must exist")
    }

    pub fn persist_profile(&mut self, profile: ProfileConfig) -> io::Result<()> {
        let active_profile = self.inner.active_profile_name().to_string();
        self.inner
            .update_active_profile(|config: &mut CliProfileConfig| {
                config.profiles.insert(active_profile, profile);
            })
            .map_err(into_io_error)
    }

    pub fn set_tool_profile(&mut self, tools: ToolProfile) -> io::Result<()> {
        let active_profile = self.inner.active_profile_name().to_string();
        self.inner
            .update_active_profile(|config: &mut CliProfileConfig| {
                config.profiles.entry(active_profile).or_default().tools = Some(tools);
            })
            .map_err(into_io_error)
    }

    pub fn set_system_instruction(&mut self, system_instruction: Option<String>) -> io::Result<()> {
        let active_profile = self.inner.active_profile_name().to_string();
        self.inner
            .update_active_profile(|config: &mut CliProfileConfig| {
                config
                    .profiles
                    .entry(active_profile)
                    .or_default()
                    .system_instruction = system_instruction;
            })
            .map_err(into_io_error)
    }

    pub fn open_harness(&self) -> io::Result<Harness> {
        self.inner.open_harness().map_err(into_io_error)
    }

    #[cfg(any(feature = "mic", feature = "speak"))]
    pub fn set_audio_state(&mut self, mic_enabled: bool, speak_enabled: bool) -> io::Result<()> {
        let active_profile = self.inner.active_profile_name().to_string();
        self.inner
            .update_active_profile(|config: &mut CliProfileConfig| {
                let profile = config.profiles.entry(active_profile).or_default();
                profile.mic_enabled = Some(mic_enabled);
                profile.speak_enabled = Some(speak_enabled);
            })
            .map_err(into_io_error)
    }

    #[cfg(feature = "share-screen")]
    pub fn set_screen_share(
        &mut self,
        enabled: bool,
        target_id: Option<usize>,
        interval_secs: Option<f64>,
    ) -> io::Result<()> {
        let active_profile = self.inner.active_profile_name().to_string();
        self.inner
            .update_active_profile(|config: &mut CliProfileConfig| {
                config
                    .profiles
                    .entry(active_profile)
                    .or_default()
                    .screen_share = Some(ScreenShareProfile {
                    enabled: Some(enabled),
                    target_id,
                    interval_secs,
                });
            })
            .map_err(into_io_error)
    }
}

pub fn config_file_path(profile_override: Option<&str>) -> io::Result<std::path::PathBuf> {
    HarnessProfileStore::<CliProfileConfig>::config_path_for("cli", profile_override)
        .map_err(into_io_error)
}

fn into_io_error(error: gemini_live_harness::HarnessError) -> io::Error {
    io::Error::other(error.to_string())
}
