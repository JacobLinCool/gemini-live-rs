//! Profile-scoped filesystem helpers and generic host profile persistence.
//!
//! A harness profile is the durable namespace above one host runtime. The
//! selected profile determines where harness tasks, notifications, and memory
//! live on disk, and also where host-specific profile config is persisted.

use std::path::{Path, PathBuf};

use serde::{Serialize, de::DeserializeOwned};

use crate::error::HarnessError;
use crate::fs::{ensure_dir, now_ms, read_json, validate_segment, write_json_atomic};
use crate::store::{Harness, HarnessPaths};

const DEFAULT_PROFILE_NAME: &str = "default";
const DEFAULT_PROFILE_FILE_NAME: &str = "default-profile.json";
const PROFILES_DIR_NAME: &str = "profiles";
const CONFIG_DIR_NAME: &str = "config";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DefaultProfileRecord {
    default_profile: String,
    updated_at_ms: u64,
}

/// Generic typed host profile store backed by the harness filesystem.
pub struct HarnessProfileStore<T> {
    base_root: PathBuf,
    component: String,
    default_profile: String,
    active_profile: String,
    profile: T,
}

impl<T> HarnessProfileStore<T>
where
    T: Clone + Default + Serialize + DeserializeOwned,
{
    pub fn load(
        component: impl Into<String>,
        profile_override: Option<&str>,
    ) -> Result<Self, HarnessError> {
        Self::load_at_base_root(HarnessPaths::base_root()?, component, profile_override)
    }

    pub fn active_profile_name(&self) -> &str {
        &self.active_profile
    }

    pub fn active_profile(&self) -> &T {
        &self.profile
    }

    pub fn config_path(&self) -> PathBuf {
        Self::component_config_path_for_base(&self.base_root, &self.active_profile, &self.component)
    }

    pub fn active_profile_root(&self) -> PathBuf {
        HarnessPaths::profile_root_for_base(&self.base_root, &self.active_profile)
    }

    pub fn open_harness(&self) -> Result<Harness, HarnessError> {
        Harness::open(self.active_profile_root())
    }

    pub fn persist_profile(&mut self, profile: T) -> Result<(), HarnessError> {
        self.profile = profile;
        self.save()
    }

    pub fn update_active_profile<F>(&mut self, mutate: F) -> Result<(), HarnessError>
    where
        F: FnOnce(&mut T),
    {
        mutate(&mut self.profile);
        self.save()
    }

    pub fn config_path_for(
        component: impl Into<String>,
        profile_override: Option<&str>,
    ) -> Result<PathBuf, HarnessError> {
        let store = Self::load(component, profile_override)?;
        Ok(store.config_path())
    }

    pub fn load_at_base_root(
        base_root: PathBuf,
        component: impl Into<String>,
        profile_override: Option<&str>,
    ) -> Result<Self, HarnessError> {
        let component = component.into();
        validate_segment("component", &component)?;

        ensure_dir(&base_root)?;
        ensure_dir(&Self::profiles_dir_for_base(&base_root))?;

        let default_profile = Self::read_default_profile(&base_root)?
            .unwrap_or_else(|| DEFAULT_PROFILE_NAME.to_string());
        let active_profile = profile_override
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| default_profile.clone());
        validate_segment("profile", &active_profile)?;

        let profile = {
            let path =
                Self::component_config_path_for_base(&base_root, &active_profile, &component);
            if path.exists() {
                read_json(&path)?
            } else {
                T::default()
            }
        };

        Ok(Self {
            base_root,
            component,
            default_profile,
            active_profile,
            profile,
        })
    }

    fn save(&self) -> Result<(), HarnessError> {
        ensure_dir(&self.active_profile_root())?;
        ensure_dir(&self.active_profile_root().join(CONFIG_DIR_NAME))?;
        write_json_atomic(&self.config_path(), &self.profile)?;
        self.write_default_profile_if_missing()?;
        Ok(())
    }

    fn write_default_profile_if_missing(&self) -> Result<(), HarnessError> {
        let path = Self::default_profile_file_for_base(&self.base_root);
        if path.exists() {
            return Ok(());
        }
        write_json_atomic(
            &path,
            &DefaultProfileRecord {
                default_profile: self.default_profile.clone(),
                updated_at_ms: now_ms(),
            },
        )
    }

    fn read_default_profile(base_root: &Path) -> Result<Option<String>, HarnessError> {
        let path = Self::default_profile_file_for_base(base_root);
        if !path.exists() {
            return Ok(None);
        }
        let record: DefaultProfileRecord = read_json(&path)?;
        validate_segment("profile", &record.default_profile)?;
        Ok(Some(record.default_profile))
    }

    fn default_profile_file_for_base(base_root: &Path) -> PathBuf {
        base_root.join(DEFAULT_PROFILE_FILE_NAME)
    }

    fn profiles_dir_for_base(base_root: &Path) -> PathBuf {
        base_root.join(PROFILES_DIR_NAME)
    }

    fn component_config_path_for_base(base_root: &Path, profile: &str, component: &str) -> PathBuf {
        HarnessPaths::profile_root_for_base(base_root, profile)
            .join(CONFIG_DIR_NAME)
            .join(format!("{component}.json"))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct TestProfile {
        enabled: bool,
        label: String,
    }

    fn temp_base_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "gemini-live-harness-profile-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time before unix epoch")
                .as_nanos()
        ))
    }

    #[test]
    fn store_uses_profile_scoped_config_and_harness_roots() {
        let base_root = temp_base_root();
        write_json_atomic(
            &HarnessProfileStore::<TestProfile>::default_profile_file_for_base(&base_root),
            &DefaultProfileRecord {
                default_profile: "work".into(),
                updated_at_ms: now_ms(),
            },
        )
        .expect("write default profile");

        let mut store =
            HarnessProfileStore::<TestProfile>::load_at_base_root(base_root, "cli", None)
                .expect("load profile store");
        assert_eq!(store.active_profile_name(), "work");
        assert!(
            store
                .config_path()
                .ends_with("profiles/work/config/cli.json")
        );
        assert!(store.active_profile_root().ends_with("profiles/work"));

        store
            .persist_profile(TestProfile {
                enabled: true,
                label: "workbench".into(),
            })
            .expect("persist profile");
        let harness = store.open_harness().expect("open harness");
        assert!(harness.paths().root().ends_with("profiles/work"));
    }
}
