//! Persistent global configuration and named profile storage for the CLI.
//!
//! Profiles are stored in a single TOML file under the user's global config
//! directory. The selected profile is loaded at startup, overlaid by
//! environment variables, and then written back so the resolved settings can
//! persist across restarts.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::tooling::ToolProfile;

const DEFAULT_PROFILE_NAME: &str = "default";
const CONFIG_DIR_NAME: &str = "gemini-live";
const CONFIG_FILE_NAME: &str = "config.toml";

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
struct GlobalConfig {
    #[serde(default = "default_profile_name")]
    default_profile: String,
    #[serde(default)]
    profiles: BTreeMap<String, ProfileConfig>,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            default_profile: default_profile_name(),
            profiles: BTreeMap::new(),
        }
    }
}

pub struct ProfileStore {
    path: PathBuf,
    active_profile: String,
    config: GlobalConfig,
}

impl ProfileStore {
    pub fn load(profile_override: Option<&str>) -> io::Result<Self> {
        let path = config_file_path()?;
        let mut config = if path.exists() {
            let raw = fs::read_to_string(&path)?;
            toml::from_str::<GlobalConfig>(&raw).map_err(|e| {
                io::Error::other(format!("invalid config file `{}`: {e}", path.display()))
            })?
        } else {
            GlobalConfig::default()
        };

        if config.default_profile.trim().is_empty() {
            config.default_profile = default_profile_name();
        }

        let active_profile = profile_override
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(config.default_profile.as_str())
            .to_string();
        config
            .profiles
            .entry(active_profile.clone())
            .or_insert_with(ProfileConfig::default);

        Ok(Self {
            path,
            active_profile,
            config,
        })
    }

    pub fn active_profile_name(&self) -> &str {
        &self.active_profile
    }

    pub fn active_profile(&self) -> &ProfileConfig {
        self.config
            .profiles
            .get(&self.active_profile)
            .expect("active profile must exist")
    }

    pub fn persist_profile(&mut self, profile: ProfileConfig) -> io::Result<()> {
        *self.active_profile_mut() = profile;
        self.save()
    }

    pub fn set_tool_profile(&mut self, tools: ToolProfile) -> io::Result<()> {
        self.active_profile_mut().tools = Some(tools);
        self.save()
    }

    pub fn set_system_instruction(&mut self, system_instruction: Option<String>) -> io::Result<()> {
        self.active_profile_mut().system_instruction = system_instruction;
        self.save()
    }

    #[cfg(any(feature = "mic", feature = "speak"))]
    pub fn set_audio_state(&mut self, mic_enabled: bool, speak_enabled: bool) -> io::Result<()> {
        let profile = self.active_profile_mut();
        profile.mic_enabled = Some(mic_enabled);
        profile.speak_enabled = Some(speak_enabled);
        self.save()
    }

    #[cfg(feature = "share-screen")]
    pub fn set_screen_share(
        &mut self,
        enabled: bool,
        target_id: Option<usize>,
        interval_secs: Option<f64>,
    ) -> io::Result<()> {
        self.active_profile_mut().screen_share = Some(ScreenShareProfile {
            enabled: Some(enabled),
            target_id,
            interval_secs,
        });
        self.save()
    }

    fn active_profile_mut(&mut self) -> &mut ProfileConfig {
        self.config
            .profiles
            .get_mut(&self.active_profile)
            .expect("active profile must exist")
    }

    fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
            set_dir_permissions(parent)?;
        }

        let encoded = toml::to_string_pretty(&self.config)
            .map_err(|e| io::Error::other(format!("failed to serialize config: {e}")))?;
        fs::write(&self.path, encoded)?;
        set_file_permissions(&self.path)?;
        Ok(())
    }
}

pub fn config_file_path() -> io::Result<PathBuf> {
    let root = env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .or_else(|| {
            env::var_os("APPDATA")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "could not determine global config directory; set XDG_CONFIG_HOME, HOME, or APPDATA",
            )
        })?;

    Ok(root.join(CONFIG_DIR_NAME).join(CONFIG_FILE_NAME))
}

fn default_profile_name() -> String {
    DEFAULT_PROFILE_NAME.to_string()
}

#[cfg(unix)]
fn set_dir_permissions(path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_dir_permissions(_path: &std::path::Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &std::path::Path) -> io::Result<()> {
    Ok(())
}
