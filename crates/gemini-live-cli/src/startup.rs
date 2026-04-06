//! CLI startup resolution and session-template construction.
//!
//! This module is the canonical home for how the desktop CLI turns:
//!
//! - environment variables
//! - persisted profile values
//! - built-in CLI defaults
//!
//! into a concrete Live transport plan plus the voice-first `SetupConfig`
//! used by the desktop product.
//!
//! The built-in desktop session template currently opts into:
//!
//! - `responseModalities = ["AUDIO"]`
//! - `inputAudioTranscription = {}`
//! - `outputAudioTranscription = {}`
//! - `sessionResumption = {}`
//!
//! Then the selected profile overlays:
//!
//! - model / backend / credentials
//! - staged tool profile
//! - microphone / speaker auto-start flags
//! - optional screen-share auto-start settings
//!
//! Startup precedence is `environment > active profile > built-in defaults`,
//! and the resolved values are written back into the active profile.

use std::{error::Error, fmt};

use gemini_live::session::{ReconnectPolicy, SessionConfig};
use gemini_live::transport::{Auth, Endpoint, TransportConfig};
use gemini_live::types::{
    AudioTranscriptionConfig, Content, GenerationConfig, MediaResolution, Modality, Part,
    SessionResumptionConfig, SetupConfig,
};
use gemini_live_runtime::RuntimeConfig;

use crate::profile;
use crate::tooling::ToolProfile;

const DEFAULT_GEMINI_MODEL: &str = "models/gemini-3.1-flash-live-preview";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Gemini,
    Vertex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VertexAuthMode {
    Static,
    Adc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TransportPlan {
    Gemini {
        api_key: String,
    },
    Vertex {
        location: String,
        auth: VertexTransportAuthPlan,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VertexTransportAuthPlan {
    StaticToken(String),
    Adc,
}

#[derive(Debug, Clone)]
pub(crate) struct StartupConfig {
    pub(crate) profile_name: String,
    pub(crate) model: String,
    pub(crate) system_instruction: Option<String>,
    pub(crate) transport: TransportConfig,
    backend: Backend,
    location: Option<String>,
    pub(crate) tool_profile: ToolProfile,
    #[cfg(feature = "mic")]
    pub(crate) mic_enabled: bool,
    #[cfg(feature = "speak")]
    pub(crate) speak_enabled: bool,
    #[cfg(feature = "share-screen")]
    pub(crate) screen_share: profile::ScreenShareProfile,
    pub(crate) persisted_profile: profile::ProfileConfig,
}

#[derive(Debug, Clone)]
struct PersistedProfileInput {
    backend: Backend,
    model: String,
    system_instruction: Option<String>,
    tools: ToolProfile,
    #[cfg(feature = "mic")]
    mic_enabled: bool,
    #[cfg(feature = "speak")]
    speak_enabled: bool,
    #[cfg(feature = "share-screen")]
    screen_share: profile::ScreenShareProfile,
}

impl StartupConfig {
    pub(crate) fn connection_label(&self) -> String {
        match (&self.backend, &self.location) {
            (Backend::Gemini, _) => self.model.clone(),
            (Backend::Vertex, Some(location)) => format!("vertex:{location} {}", self.model),
            (Backend::Vertex, None) => format!("vertex {}", self.model),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CliConfigError(String);

impl CliConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for CliConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for CliConfigError {}

pub(crate) fn resolve_startup_config(
    env: impl Fn(&str) -> Option<String>,
    stored_profile: &profile::ProfileConfig,
    profile_name: &str,
) -> Result<StartupConfig, CliConfigError> {
    let backend = parse_backend(&env, stored_profile)?;
    let plan = resolve_transport_plan(&env, stored_profile, backend)?;
    let backend = match &plan {
        TransportPlan::Gemini { .. } => Backend::Gemini,
        TransportPlan::Vertex { .. } => Backend::Vertex,
    };
    let location = match &plan {
        TransportPlan::Gemini { .. } => None,
        TransportPlan::Vertex { location, .. } => Some(location.clone()),
    };
    let model = resolve_model(&env, stored_profile, backend)?;
    let system_instruction = env("GEMINI_SYSTEM_INSTRUCTION")
        .or_else(|| stored_profile.system_instruction.clone())
        .filter(|value| !value.trim().is_empty());
    let transport = build_transport(plan)?;
    let tool_profile = stored_profile.tools.unwrap_or_default();
    #[cfg(feature = "mic")]
    let mic_enabled = stored_profile.mic_enabled.unwrap_or(false);
    #[cfg(feature = "speak")]
    let speak_enabled = stored_profile.speak_enabled.unwrap_or(false);
    #[cfg(feature = "share-screen")]
    let screen_share = stored_profile.screen_share.clone().unwrap_or_default();

    let persisted_profile = build_persisted_profile(
        &PersistedProfileInput {
            backend,
            model: model.clone(),
            system_instruction: system_instruction.clone(),
            tools: tool_profile,
            #[cfg(feature = "mic")]
            mic_enabled,
            #[cfg(feature = "speak")]
            speak_enabled,
            #[cfg(feature = "share-screen")]
            screen_share: screen_share.clone(),
        },
        &transport,
    );

    Ok(StartupConfig {
        profile_name: profile_name.to_string(),
        model,
        system_instruction,
        transport,
        backend,
        location,
        tool_profile,
        #[cfg(feature = "mic")]
        mic_enabled,
        #[cfg(feature = "speak")]
        speak_enabled,
        #[cfg(feature = "share-screen")]
        screen_share,
        persisted_profile,
    })
}

pub(crate) fn build_runtime_config(
    startup: &StartupConfig,
    tools: ToolProfile,
    system_instruction: Option<&str>,
) -> RuntimeConfig {
    RuntimeConfig {
        session: SessionConfig {
            transport: startup.transport.clone(),
            setup: build_cli_setup(startup, tools, system_instruction),
            reconnect: ReconnectPolicy::default(),
        },
    }
}

pub(crate) fn build_cli_setup(
    startup: &StartupConfig,
    tools: ToolProfile,
    system_instruction: Option<&str>,
) -> SetupConfig {
    SetupConfig {
        model: startup.model.clone(),
        generation_config: Some(GenerationConfig {
            response_modalities: Some(vec![Modality::Audio]),
            media_resolution: Some(MediaResolution::MediaResolutionHigh),
            ..Default::default()
        }),
        system_instruction: system_instruction.map(system_instruction_content),
        input_audio_transcription: Some(AudioTranscriptionConfig {}),
        output_audio_transcription: Some(AudioTranscriptionConfig {}),
        session_resumption: Some(SessionResumptionConfig::default()),
        tools: tools.build_live_tools(),
        ..Default::default()
    }
}

fn system_instruction_content(text: &str) -> Content {
    Content {
        role: None,
        parts: vec![Part {
            text: Some(text.to_string()),
            inline_data: None,
        }],
    }
}

fn resolve_transport_plan(
    env: &impl Fn(&str) -> Option<String>,
    stored_profile: &profile::ProfileConfig,
    backend: Backend,
) -> Result<TransportPlan, CliConfigError> {
    match backend {
        Backend::Gemini => Ok(TransportPlan::Gemini {
            api_key: required_setting(
                env("GEMINI_API_KEY"),
                stored_profile.gemini_api_key.clone(),
                "GEMINI_API_KEY",
            )?,
        }),
        Backend::Vertex => Ok(TransportPlan::Vertex {
            location: required_setting(
                env("VERTEX_LOCATION"),
                stored_profile.vertex_location.clone(),
                "VERTEX_LOCATION",
            )?,
            auth: match parse_vertex_auth_mode(env, stored_profile)? {
                VertexAuthMode::Static => VertexTransportAuthPlan::StaticToken(required_setting(
                    env("VERTEX_AI_ACCESS_TOKEN"),
                    stored_profile.vertex_ai_access_token.clone(),
                    "VERTEX_AI_ACCESS_TOKEN",
                )?),
                VertexAuthMode::Adc => VertexTransportAuthPlan::Adc,
            },
        }),
    }
}

fn resolve_model(
    env: &impl Fn(&str) -> Option<String>,
    stored_profile: &profile::ProfileConfig,
    backend: Backend,
) -> Result<String, CliConfigError> {
    match backend {
        Backend::Gemini => Ok(env("GEMINI_MODEL")
            .or_else(|| stored_profile.model.clone())
            .unwrap_or_else(|| DEFAULT_GEMINI_MODEL.into())),
        Backend::Vertex => required_setting(
            env("VERTEX_MODEL"),
            stored_profile.model.clone(),
            "VERTEX_MODEL",
        ),
    }
}

fn parse_backend(
    env: &impl Fn(&str) -> Option<String>,
    stored_profile: &profile::ProfileConfig,
) -> Result<Backend, CliConfigError> {
    match env("LIVE_BACKEND")
        .or_else(|| stored_profile.backend.map(persistent_backend_to_env))
        .unwrap_or_else(|| "gemini".to_string())
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "gemini" => Ok(Backend::Gemini),
        "vertex" => Ok(Backend::Vertex),
        other => Err(CliConfigError::new(format!(
            "unsupported LIVE_BACKEND `{other}`; expected `gemini` or `vertex`"
        ))),
    }
}

fn parse_vertex_auth_mode(
    env: &impl Fn(&str) -> Option<String>,
    stored_profile: &profile::ProfileConfig,
) -> Result<VertexAuthMode, CliConfigError> {
    match env("VERTEX_AUTH")
        .or_else(|| {
            stored_profile
                .vertex_auth
                .map(persistent_vertex_auth_to_env)
        })
        .unwrap_or_else(|| "static".to_string())
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "static" => Ok(VertexAuthMode::Static),
        "adc" => Ok(VertexAuthMode::Adc),
        other => Err(CliConfigError::new(format!(
            "unsupported VERTEX_AUTH `{other}`; expected `static` or `adc`"
        ))),
    }
}

fn required_setting(
    primary: Option<String>,
    secondary: Option<String>,
    key: &str,
) -> Result<String, CliConfigError> {
    primary
        .or(secondary)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            CliConfigError::new(format!(
                "set {key} in the environment or the active profile"
            ))
        })
}

fn build_persisted_profile(
    input: &PersistedProfileInput,
    transport: &TransportConfig,
) -> profile::ProfileConfig {
    let (backend_value, gemini_api_key, vertex_location, vertex_auth, vertex_ai_access_token) =
        match (input.backend, &transport.auth, &transport.endpoint) {
            (Backend::Gemini, Auth::ApiKey(key), _) => (
                Some(profile::PersistentBackend::Gemini),
                Some(key.clone()),
                None,
                None,
                None,
            ),
            (Backend::Vertex, Auth::BearerToken(token), Endpoint::VertexAi { location }) => (
                Some(profile::PersistentBackend::Vertex),
                None,
                Some(location.clone()),
                Some(profile::PersistentVertexAuthMode::Static),
                Some(token.clone()),
            ),
            (Backend::Vertex, Auth::BearerTokenProvider(_), Endpoint::VertexAi { location }) => (
                Some(profile::PersistentBackend::Vertex),
                None,
                Some(location.clone()),
                Some(profile::PersistentVertexAuthMode::Adc),
                None,
            ),
            _ => (None, None, None, None, None),
        };

    profile::ProfileConfig {
        backend: backend_value,
        model: Some(input.model.clone()),
        system_instruction: input.system_instruction.clone(),
        gemini_api_key,
        vertex_location,
        vertex_auth,
        vertex_ai_access_token,
        tools: Some(input.tools),
        #[cfg(feature = "mic")]
        mic_enabled: Some(input.mic_enabled),
        #[cfg(not(feature = "mic"))]
        mic_enabled: None,
        #[cfg(feature = "speak")]
        speak_enabled: Some(input.speak_enabled),
        #[cfg(not(feature = "speak"))]
        speak_enabled: None,
        #[cfg(feature = "share-screen")]
        screen_share: Some(input.screen_share.clone()),
        #[cfg(not(feature = "share-screen"))]
        screen_share: None,
    }
}

fn persistent_backend_to_env(value: profile::PersistentBackend) -> String {
    match value {
        profile::PersistentBackend::Gemini => "gemini".to_string(),
        profile::PersistentBackend::Vertex => "vertex".to_string(),
    }
}

fn persistent_vertex_auth_to_env(value: profile::PersistentVertexAuthMode) -> String {
    match value {
        profile::PersistentVertexAuthMode::Static => "static".to_string(),
        profile::PersistentVertexAuthMode::Adc => "adc".to_string(),
    }
}

fn build_transport(plan: TransportPlan) -> Result<TransportConfig, CliConfigError> {
    match plan {
        TransportPlan::Gemini { api_key } => Ok(TransportConfig {
            endpoint: Endpoint::GeminiApi,
            auth: Auth::ApiKey(api_key),
            ..Default::default()
        }),
        TransportPlan::Vertex { location, auth } => Ok(TransportConfig {
            endpoint: Endpoint::VertexAi { location },
            auth: build_vertex_auth(auth)?,
            ..Default::default()
        }),
    }
}

fn build_vertex_auth(auth: VertexTransportAuthPlan) -> Result<Auth, CliConfigError> {
    match auth {
        VertexTransportAuthPlan::StaticToken(token) => Ok(Auth::BearerToken(token)),
        VertexTransportAuthPlan::Adc => build_vertex_adc_auth(),
    }
}

#[cfg(feature = "vertex-auth")]
fn build_vertex_adc_auth() -> Result<Auth, CliConfigError> {
    Auth::vertex_ai_application_default()
        .map_err(|e| CliConfigError::new(format!("failed to initialize Vertex ADC auth: {e}")))
}

#[cfg(not(feature = "vertex-auth"))]
fn build_vertex_adc_auth() -> Result<Auth, CliConfigError> {
    Err(CliConfigError::new(
        "VERTEX_AUTH=adc requires building gemini-live-cli with --features vertex-auth",
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn empty_profile() -> profile::ProfileConfig {
        profile::ProfileConfig::default()
    }

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars = vars
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect::<HashMap<_, _>>();
        move |key| vars.get(key).cloned()
    }

    #[test]
    fn gemini_backend_defaults_model() {
        let startup = resolve_startup_config(
            env(&[("GEMINI_API_KEY", "test-key")]),
            &empty_profile(),
            "default",
        )
        .expect("startup config");

        assert_eq!(startup.model, DEFAULT_GEMINI_MODEL);
        assert_eq!(startup.profile_name, "default");
        assert!(startup.location.is_none());
        assert!(matches!(startup.transport.endpoint, Endpoint::GeminiApi));
        assert!(matches!(startup.transport.auth, Auth::ApiKey(_)));
    }

    #[test]
    fn vertex_backend_requires_explicit_model() {
        let err = resolve_startup_config(
            env(&[
                ("LIVE_BACKEND", "vertex"),
                ("VERTEX_LOCATION", "us-central1"),
                ("VERTEX_AI_ACCESS_TOKEN", "token"),
            ]),
            &empty_profile(),
            "default",
        )
        .expect_err("missing model should fail");

        assert_eq!(
            err.to_string(),
            "set VERTEX_MODEL in the environment or the active profile"
        );
    }

    #[test]
    fn vertex_backend_static_token_path_builds_transport() {
        let startup = resolve_startup_config(
            env(&[
                ("LIVE_BACKEND", "vertex"),
                ("VERTEX_LOCATION", "us-central1"),
                (
                    "VERTEX_MODEL",
                    "projects/p/locations/us-central1/publishers/google/models/test",
                ),
                ("VERTEX_AI_ACCESS_TOKEN", "token"),
            ]),
            &empty_profile(),
            "default",
        )
        .expect("startup config");

        assert_eq!(startup.location.as_deref(), Some("us-central1"));
        assert!(matches!(
            startup.transport.endpoint,
            Endpoint::VertexAi { ref location } if location == "us-central1"
        ));
        assert!(matches!(startup.transport.auth, Auth::BearerToken(_)));
    }

    #[test]
    fn unsupported_backend_is_rejected() {
        let err = resolve_startup_config(
            env(&[("LIVE_BACKEND", "other"), ("GEMINI_API_KEY", "test-key")]),
            &empty_profile(),
            "default",
        )
        .expect_err("unsupported backend should fail");

        assert!(err.to_string().contains("unsupported LIVE_BACKEND"));
    }

    #[cfg(not(feature = "vertex-auth"))]
    #[test]
    fn vertex_adc_requires_feature() {
        let err = resolve_startup_config(
            env(&[
                ("LIVE_BACKEND", "vertex"),
                ("VERTEX_LOCATION", "us-central1"),
                (
                    "VERTEX_MODEL",
                    "projects/p/locations/us-central1/publishers/google/models/test",
                ),
                ("VERTEX_AUTH", "adc"),
            ]),
            &empty_profile(),
            "default",
        )
        .expect_err("adc should require feature");

        assert_eq!(
            err.to_string(),
            "VERTEX_AUTH=adc requires building gemini-live-cli with --features vertex-auth"
        );
    }

    #[test]
    fn startup_uses_profile_values_when_env_is_missing() {
        let stored = profile::ProfileConfig {
            backend: Some(profile::PersistentBackend::Gemini),
            model: Some("models/custom-live".into()),
            system_instruction: Some("Profile instruction".into()),
            gemini_api_key: Some("profile-key".into()),
            tools: Some(ToolProfile {
                read_file: true,
                ..ToolProfile::default()
            }),
            mic_enabled: Some(true),
            speak_enabled: Some(false),
            ..Default::default()
        };

        let startup =
            resolve_startup_config(env(&[]), &stored, "persisted").expect("startup config");
        assert_eq!(startup.model, "models/custom-live");
        assert_eq!(startup.profile_name, "persisted");
        assert_eq!(
            startup.system_instruction.as_deref(),
            Some("Profile instruction")
        );
        assert!(startup.tool_profile.read_file);
        #[cfg(feature = "mic")]
        assert!(startup.mic_enabled);
        assert!(matches!(startup.transport.auth, Auth::ApiKey(_)));
    }

    #[test]
    fn env_overrides_profile_values() {
        let stored = profile::ProfileConfig {
            backend: Some(profile::PersistentBackend::Gemini),
            model: Some("models/from-profile".into()),
            system_instruction: Some("Profile instruction".into()),
            gemini_api_key: Some("profile-key".into()),
            ..Default::default()
        };

        let startup = resolve_startup_config(
            env(&[
                ("GEMINI_MODEL", "models/from-env"),
                ("GEMINI_SYSTEM_INSTRUCTION", "Env instruction"),
                ("GEMINI_API_KEY", "env-key"),
            ]),
            &stored,
            "default",
        )
        .expect("startup config");

        assert_eq!(startup.model, "models/from-env");
        assert_eq!(
            startup.system_instruction.as_deref(),
            Some("Env instruction")
        );
        match startup.transport.auth {
            Auth::ApiKey(key) => assert_eq!(key, "env-key"),
            other => panic!("unexpected auth: {other:?}"),
        }
    }
}
