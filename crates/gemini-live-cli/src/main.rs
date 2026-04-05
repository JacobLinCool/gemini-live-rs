//! Interactive TUI client for the Gemini Multimodal Live API.
//!
//! Serves as a living usage example for the `gemini-live` library.
//! Top panel shows the conversation; bottom panel is the always-active input.
//!
//! # Commands
//!
//! - `/mic`          — toggle microphone streaming
//! - `/speak`        — toggle audio playback of model responses
//! - `/share-screen` — share a monitor or window as video
//! - `/tools ...`    — inspect, stage, and apply the Live tool profile
//! - `@file`         — send an image or audio file inline
//!
//! # Backend Selection
//!
//! The CLI can target either:
//!
//! - the public Gemini API (`LIVE_BACKEND=gemini`, default)
//! - Vertex AI Live (`LIVE_BACKEND=vertex`)
//!
//! Vertex mode requires an explicit `VERTEX_MODEL` and `VERTEX_LOCATION`.
//! `VERTEX_AUTH=static` uses `VERTEX_AI_ACCESS_TOKEN`. `VERTEX_AUTH=adc`
//! requires building the CLI with the Cargo feature `vertex-auth`.
//!
//! # Current Default Profile
//!
//! The CLI currently boots into a voice-first session template, then overlays
//! settings from the selected persistent profile:
//!
//! - `responseModalities = ["AUDIO"]`
//! - `inputAudioTranscription = {}`
//! - `outputAudioTranscription = {}`
//! - profile-selected model / backend / credentials
//! - profile-selected tools, mic / speaker auto-start, and optional
//!   screen-share auto-start
//!
//! This module doc is the canonical home for the default CLI session profile.
//! Keep it in sync with the `SetupConfig` built in `main()`.

#[cfg(any(feature = "mic", feature = "speak"))]
mod audio_io;
mod input;
mod media;
mod profile;
#[cfg(feature = "share-screen")]
mod screen;
mod slash;
mod tooling;
mod update;

use std::collections::HashMap;
use std::io;
use std::{error::Error, fmt};

use clap::{Parser, Subcommand};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use gemini_live::session::{ReconnectPolicy, Session, SessionConfig};
use gemini_live::transport::{Auth, Endpoint, TransportConfig};
use gemini_live::types::*;

const DEFAULT_GEMINI_MODEL: &str = "models/gemini-3.1-flash-live-preview";

#[derive(Debug, Parser)]
#[command(name = env!("CARGO_BIN_NAME"))]
struct CliArgs {
    #[arg(long)]
    profile: Option<String>,
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    Update,
    Config,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = CliArgs::parse();
    match cli.command {
        Some(CliCommand::Update) => return update::run().await,
        Some(CliCommand::Config) => {
            println!("{}", profile::config_file_path()?.display());
            return Ok(());
        }
        None => {}
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(io::stderr)
        .init();

    let mut profile_store = profile::ProfileStore::load(cli.profile.as_deref())?;
    let startup = resolve_startup_config(
        |key| std::env::var(key).ok(),
        profile_store.active_profile(),
        profile_store.active_profile_name(),
    )?;
    profile_store.persist_profile(startup.persisted_profile.clone())?;

    install_panic_hook();
    let mut terminal = init_terminal()?;
    let result = run(&mut terminal, startup, profile_store).await;
    restore_terminal(&mut terminal)?;
    result
}

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
struct StartupConfig {
    profile_name: String,
    model: String,
    system_instruction: Option<String>,
    transport: TransportConfig,
    backend: Backend,
    location: Option<String>,
    tool_profile: tooling::ToolProfile,
    mic_enabled: bool,
    speak_enabled: bool,
    screen_share: profile::ScreenShareProfile,
    persisted_profile: profile::ProfileConfig,
}

#[derive(Debug, Clone)]
struct PersistedProfileInput {
    backend: Backend,
    model: String,
    system_instruction: Option<String>,
    tools: tooling::ToolProfile,
    mic_enabled: bool,
    speak_enabled: bool,
    screen_share: profile::ScreenShareProfile,
}

impl StartupConfig {
    fn connection_label(&self) -> String {
        match (&self.backend, &self.location) {
            (Backend::Gemini, _) => self.model.clone(),
            (Backend::Vertex, Some(location)) => format!("vertex:{location} {}", self.model),
            (Backend::Vertex, None) => format!("vertex {}", self.model),
        }
    }
}

#[derive(Debug, Clone)]
struct CliConfigError(String);

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

fn resolve_startup_config(
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
    let mic_enabled = stored_profile.mic_enabled.unwrap_or(false);
    let speak_enabled = stored_profile.speak_enabled.unwrap_or(false);
    let screen_share = stored_profile.screen_share.clone().unwrap_or_default();
    let persisted_profile_input = PersistedProfileInput {
        backend,
        model: model.clone(),
        system_instruction: system_instruction.clone(),
        tools: tool_profile,
        mic_enabled,
        speak_enabled,
        screen_share: screen_share.clone(),
    };
    let persisted_profile = build_persisted_profile(&persisted_profile_input, &transport);

    Ok(StartupConfig {
        profile_name: profile_name.to_string(),
        model,
        system_instruction,
        transport,
        backend,
        location,
        tool_profile,
        mic_enabled,
        speak_enabled,
        screen_share,
        persisted_profile,
    })
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
        mic_enabled: Some(input.mic_enabled),
        speak_enabled: Some(input.speak_enabled),
        screen_share: Some(input.screen_share.clone()),
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
mod startup_tests {
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
    fn cli_config_subcommand_parses() {
        let cli = CliArgs::try_parse_from(["gemini-live", "config"]).expect("cli args");
        assert!(matches!(cli.command, Some(CliCommand::Config)));
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
        assert_eq!(startup.backend, Backend::Gemini);
        assert!(startup.location.is_none());
        assert_eq!(startup.profile_name, "default");
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

        assert_eq!(startup.backend, Backend::Vertex);
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
            tools: Some(tooling::ToolProfile {
                read_file: true,
                ..tooling::ToolProfile::default()
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

// ── Terminal ─────────────────────────────────────────────────────────────────

fn init_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(io::stdout(), EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(io::stdout()))
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        crossterm::terminal::disable_raw_mode().ok();
        crossterm::execute!(io::stdout(), LeaveAlternateScreen).ok();
        original(info);
    }));
}

/// AEC processes at 48 kHz — mic output is always at this rate.
#[cfg(feature = "mic")]
const AEC_SEND_RATE: u32 = 48_000;

// ── App state ────────────────────────────────────────────────────────────────

struct App {
    messages: Vec<Msg>,
    pending: String,
    input: input::InputEditor,
    completions: Vec<slash::CompletionItem>,
    completion_index: usize,
    quit: bool,
    title: String,
    active_tools: tooling::ToolProfile,
    desired_tools: tooling::ToolProfile,
    active_system_instruction: Option<String>,
    desired_system_instruction: Option<String>,
    #[cfg(feature = "mic")]
    mic_on: bool,
    #[cfg(feature = "speak")]
    speak_on: bool,
    #[cfg(feature = "share-screen")]
    screen_on: bool,
}

struct Msg {
    role: Role,
    text: String,
}

#[derive(Clone, Copy)]
enum Role {
    User,
    Transcription,
    Model,
    System,
}

enum AppEvent {
    Server {
        generation: u64,
        event: ServerEvent,
    },
    ToolFinished {
        generation: u64,
        call_id: String,
        message: String,
    },
}

impl App {
    fn new(title: &str, tools: tooling::ToolProfile, system_instruction: Option<String>) -> Self {
        Self {
            messages: vec![Msg {
                role: Role::System,
                text: "connected — @file for media, /mic /speak to toggle audio, /share-screen to share screen, /tools to manage Live tools".to_string(),
            }],
            pending: String::new(),
            input: input::InputEditor::new(),
            completions: Vec::new(),
            completion_index: 0,
            quit: false,
            title: title.to_string(),
            active_tools: tools,
            desired_tools: tools,
            active_system_instruction: system_instruction.clone(),
            desired_system_instruction: system_instruction,
            #[cfg(feature = "mic")]
            mic_on: false,
            #[cfg(feature = "speak")]
            speak_on: false,
            #[cfg(feature = "share-screen")]
            screen_on: false,
        }
    }

    fn sys(&mut self, text: String) {
        self.messages.push(Msg {
            role: Role::System,
            text,
        });
    }

    fn refresh_completions(&mut self) {
        let selected = self
            .completions
            .get(self.completion_index)
            .map(|item| item.label.clone());
        self.completions = slash::completions(&self.input.text());
        self.completion_index = selected
            .and_then(|label| self.completions.iter().position(|item| item.label == label))
            .unwrap_or(0);
    }

    fn completion_count(&self) -> usize {
        self.completions.len().min(5)
    }

    fn has_completions(&self) -> bool {
        !self.completions.is_empty()
    }

    fn select_next_completion(&mut self) {
        if self.completions.is_empty() {
            return;
        }
        self.completion_index = (self.completion_index + 1) % self.completions.len();
    }

    fn select_prev_completion(&mut self) {
        if self.completions.is_empty() {
            return;
        }
        self.completion_index = if self.completion_index == 0 {
            self.completions.len() - 1
        } else {
            self.completion_index - 1
        };
    }

    fn apply_selected_completion(&mut self) -> bool {
        let Some(item) = self.completions.get(self.completion_index).cloned() else {
            return false;
        };
        self.input
            .replace_range(item.replace_range, &item.replacement);
        self.refresh_completions();
        true
    }
}

async fn connect_session(
    startup: &StartupConfig,
    tools: tooling::ToolProfile,
    system_instruction: Option<&str>,
) -> Result<Session, gemini_live::SessionError> {
    Session::connect(SessionConfig {
        transport: startup.transport.clone(),
        // The CLI's current built-in profile is intentionally voice-first.
        setup: SetupConfig {
            model: startup.model.clone(),
            generation_config: Some(GenerationConfig {
                response_modalities: Some(vec![Modality::Audio]),
                media_resolution: Some(MediaResolution::MediaResolutionHigh),
                ..Default::default()
            }),
            system_instruction: system_instruction.map(system_instruction_content),
            input_audio_transcription: Some(AudioTranscriptionConfig {}),
            output_audio_transcription: Some(AudioTranscriptionConfig {}),
            tools: tools.build_live_tools(),
            ..Default::default()
        },
        reconnect: ReconnectPolicy::default(),
    })
    .await
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

fn spawn_session_forwarder(
    session: Session,
    generation: u64,
    tx: mpsc::UnboundedSender<AppEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut recv = session;
        while let Some(event) = recv.next_event().await {
            if tx.send(AppEvent::Server { generation, event }).is_err() {
                break;
            }
        }
    })
}

fn spawn_tool_call(
    runtime: tooling::ToolRuntime,
    session: Session,
    tx: mpsc::UnboundedSender<AppEvent>,
    generation: u64,
    call: FunctionCallRequest,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let call_id = call.id.clone();
        let call_name = call.name.clone();
        let response = runtime.execute_call(call).await;
        let message = match session.send_tool_response(vec![response]).await {
            Ok(()) => format!("[tool] responded: {call_name} ({call_id})"),
            Err(e) => format!("[tool error] failed to send {call_name} ({call_id}): {e}"),
        };
        let _ = tx.send(AppEvent::ToolFinished {
            generation,
            call_id,
            message,
        });
    })
}

fn cancel_pending_tools(pending: &mut HashMap<String, JoinHandle<()>>) {
    for (_, handle) in pending.drain() {
        handle.abort();
    }
}

fn persist_audio_state(store: &mut profile::ProfileStore, app: &App) -> io::Result<()> {
    store.set_audio_state(
        {
            #[cfg(feature = "mic")]
            {
                app.mic_on
            }
            #[cfg(not(feature = "mic"))]
            {
                false
            }
        },
        {
            #[cfg(feature = "speak")]
            {
                app.speak_on
            }
            #[cfg(not(feature = "speak"))]
            {
                false
            }
        },
    )
}

fn summarize_optional_system_instruction(text: Option<&str>) -> String {
    match text {
        Some(text) => summarize_system_instruction(text),
        None => "none".into(),
    }
}

fn summarize_system_instruction(text: &str) -> String {
    const MAX_CHARS: usize = 60;
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_CHARS {
        compact
    } else {
        let summary = compact.chars().take(MAX_CHARS).collect::<String>();
        format!("{summary}...")
    }
}

#[cfg(feature = "share-screen")]
#[derive(Debug, Clone, Copy, Default)]
struct ScreenShareState {
    target_id: Option<usize>,
    interval_secs: Option<f64>,
}

#[cfg(feature = "share-screen")]
fn persist_screen_state(
    store: &mut profile::ProfileStore,
    app: &App,
    state: ScreenShareState,
) -> io::Result<()> {
    store.set_screen_share(app.screen_on, state.target_id, state.interval_secs)
}

// ── Main loop ────────────────────────────────────────────────────────────────

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    startup: StartupConfig,
    mut profile_store: profile::ProfileStore,
) -> Result<(), Box<dyn std::error::Error>> {
    let workspace_root = std::env::current_dir()?;
    let mut app = App::new(
        &startup.connection_label(),
        startup.tool_profile,
        startup.system_instruction.clone(),
    );
    app.refresh_completions();
    let mut tool_runtime = tooling::ToolRuntime::new(workspace_root.clone(), app.active_tools)?;

    app.sys(format!("profile: {}", startup.profile_name));
    app.sys(format!("tools active: {}", app.active_tools.summary()));
    app.sys(format!(
        "system instruction active: {}",
        summarize_optional_system_instruction(app.active_system_instruction.as_deref())
    ));
    let mut session = connect_session(
        &startup,
        app.active_tools,
        app.active_system_instruction.as_deref(),
    )
    .await?;

    // Session / tool events → channel
    let (app_tx, mut app_rx) = mpsc::unbounded_channel();
    let mut generation = 0_u64;
    let mut session_task = spawn_session_forwarder(session.clone(), generation, app_tx.clone());
    let mut pending_tools: HashMap<String, JoinHandle<()>> = HashMap::new();

    // Shared AEC processor for echo cancellation between mic and speaker.
    #[cfg(any(feature = "mic", feature = "speak"))]
    let aec = audio_io::create_aec();

    // Audio I/O state (channels always exist; data only flows when active)
    let (_mic_tx, mut mic_rx) = mpsc::channel::<Vec<u8>>(32);
    #[cfg(feature = "mic")]
    let mic_tx = _mic_tx;
    #[cfg(feature = "mic")]
    let mut mic: Option<audio_io::Mic> = None;
    #[cfg(feature = "speak")]
    let mut speaker: Option<audio_io::Speaker> = None;

    // Screen share state
    let (_screen_tx, mut screen_rx) = mpsc::channel::<Vec<u8>>(2);
    #[cfg(feature = "share-screen")]
    let screen_tx = _screen_tx;
    #[cfg(feature = "share-screen")]
    let mut screen_share: Option<screen::ScreenShare> = None;

    let mut term_events = EventStream::new();

    #[cfg(feature = "speak")]
    if startup.speak_enabled {
        toggle_speaker(&mut app, &mut speaker, &aec);
        persist_audio_state(&mut profile_store, &app)?;
    }

    #[cfg(feature = "mic")]
    if startup.mic_enabled {
        toggle_mic(&mut app, &mut mic, &mic_tx, &aec, &session).await;
        persist_audio_state(&mut profile_store, &app)?;
    }

    #[cfg(feature = "share-screen")]
    if startup.screen_share.enabled == Some(true) {
        if let Some(target_id) = startup.screen_share.target_id {
            let interval = startup.screen_share.interval_secs.unwrap_or(1.0);
            let args = format!("{target_id} {interval}");
            let screen_state = handle_share_screen(&mut app, &mut screen_share, &screen_tx, &args);
            persist_screen_state(&mut profile_store, &app, screen_state)?;
        } else {
            app.sys(
                "screen share profile requested auto-start, but no target id is configured".into(),
            );
            profile_store.set_screen_share(false, None, startup.screen_share.interval_secs)?;
        }
    }

    loop {
        terminal.draw(|f| render(f, &mut app))?;
        if app.quit {
            break;
        }

        tokio::select! {
            Some(Ok(ev)) = term_events.next() => {
                if let Event::Key(key) = ev
                    && key.kind == KeyEventKind::Press
                {
                    let previous_desired_tools = app.desired_tools;
                    let previous_desired_system_instruction =
                        app.desired_system_instruction.clone();
                    match handle_key(&mut app, key, &session).await? {
                        #[cfg(feature = "mic")]
                        Some(Cmd::ToggleMic) => {
                            toggle_mic(&mut app, &mut mic, &mic_tx, &aec, &session).await;
                            persist_audio_state(&mut profile_store, &app)?;
                        }
                        #[cfg(feature = "speak")]
                        Some(Cmd::ToggleSpeaker) => {
                            toggle_speaker(&mut app, &mut speaker, &aec);
                            persist_audio_state(&mut profile_store, &app)?;
                        }
                        #[cfg(feature = "share-screen")]
                        Some(Cmd::ShareScreen(args)) => {
                            let state = handle_share_screen(&mut app, &mut screen_share, &screen_tx, &args);
                            persist_screen_state(&mut profile_store, &app, state)?;
                        }
                        Some(Cmd::ApplySessionConfig) => {
                            if app.active_tools == app.desired_tools
                                && app.active_system_instruction == app.desired_system_instruction
                            {
                                app.sys("session config already active".into());
                            } else {
                                match connect_session(
                                    &startup,
                                    app.desired_tools,
                                    app.desired_system_instruction.as_deref(),
                                )
                                .await
                                {
                                    Ok(new_session) => {
                                        generation += 1;
                                        let new_generation = generation;
                                        let new_task = spawn_session_forwarder(
                                            new_session.clone(),
                                            new_generation,
                                            app_tx.clone(),
                                        );
                                        cancel_pending_tools(&mut pending_tools);
                                        session_task.abort();
                                        let old_session = std::mem::replace(&mut session, new_session);
                                        session_task = new_task;
                                        app.active_tools = app.desired_tools;
                                        app.active_system_instruction =
                                            app.desired_system_instruction.clone();
                                        tool_runtime = tooling::ToolRuntime::new(
                                            workspace_root.clone(),
                                            app.active_tools,
                                        )?;
                                        profile_store.set_tool_profile(app.active_tools)?;
                                        profile_store.set_system_instruction(
                                            app.active_system_instruction.clone(),
                                        )?;
                                        old_session.close().await.ok();
                                        app.sys(format!("reconnected with tools: {}", app.active_tools.summary()));
                                        if let Some(system_instruction) =
                                            app.active_system_instruction.as_deref()
                                        {
                                            app.sys(format!(
                                                "system instruction active: {}",
                                                summarize_system_instruction(system_instruction)
                                            ));
                                        } else {
                                            app.sys("system instruction active: none".into());
                                        }
                                    }
                                    Err(e) => {
                                        app.sys(format!("failed to apply staged tools: {e}"));
                                    }
                                }
                            }
                        }
                        None => {}
                    }
                    if app.desired_tools != previous_desired_tools {
                        profile_store.set_tool_profile(app.desired_tools)?;
                    }
                    if app.desired_system_instruction != previous_desired_system_instruction {
                        profile_store
                            .set_system_instruction(app.desired_system_instruction.clone())?;
                    }
                }
            }
            Some(event) = app_rx.recv() => {
                match event {
                    AppEvent::Server { generation: event_generation, event } => {
                        if event_generation != generation {
                            continue;
                        }
                        match event {
                            ServerEvent::ToolCall(calls) => {
                                for call in calls {
                                    app.sys(format!(
                                        "[tool] requested {} ({})",
                                        call.name, call.id
                                    ));
                                    let call_id = call.id.clone();
                                    let handle = spawn_tool_call(
                                        tool_runtime.clone(),
                                        session.clone(),
                                        app_tx.clone(),
                                        generation,
                                        call,
                                    );
                                    pending_tools.insert(call_id, handle);
                                }
                            }
                            ServerEvent::ToolCallCancellation(ids) => {
                                for id in ids {
                                    if let Some(handle) = pending_tools.remove(&id) {
                                        handle.abort();
                                        app.sys(format!("[tool] cancelled {id}"));
                                    }
                                }
                            }
                            other => handle_server_event(&mut app, other,
                                #[cfg(feature = "speak")] &speaker,
                            ),
                        }
                    }
                    AppEvent::ToolFinished {
                        generation: event_generation,
                        call_id,
                        message,
                    } => {
                        if event_generation != generation {
                            continue;
                        }
                        pending_tools.remove(&call_id);
                        app.sys(message);
                    }
                }
            }
            Some(pcm) = mic_rx.recv() => {
                #[cfg(feature = "mic")]
                {
                    // Audio has already been echo-cancelled by the AEC in audio_io.
                    session.send_audio_at_rate(&pcm, AEC_SEND_RATE).await.ok();
                }
                #[cfg(not(feature = "mic"))]
                drop(pcm);
            }
            Some(jpeg) = screen_rx.recv() => {
                session.send_video(&jpeg, "image/jpeg").await.ok();
            }
        }
    }

    cancel_pending_tools(&mut pending_tools);
    session_task.abort();
    session.close().await.ok();
    Ok(())
}

// ── Commands ─────────────────────────────────────────────────────────────────

enum Cmd {
    #[cfg(feature = "mic")]
    ToggleMic,
    #[cfg(feature = "speak")]
    ToggleSpeaker,
    #[cfg(feature = "share-screen")]
    ShareScreen(String),
    ApplySessionConfig,
}

async fn handle_key(
    app: &mut App,
    key: KeyEvent,
    session: &Session,
) -> Result<Option<Cmd>, Box<dyn std::error::Error>> {
    match key.code {
        KeyCode::Enter => {
            let raw = app.input.take_text();
            app.refresh_completions();
            let trimmed = raw.trim().to_string();
            if let Some(command) = slash::parse(&trimmed) {
                match command {
                    #[cfg(feature = "mic")]
                    Ok(slash::SlashCommand::ToggleMic) => return Ok(Some(Cmd::ToggleMic)),
                    #[cfg(feature = "speak")]
                    Ok(slash::SlashCommand::ToggleSpeaker) => {
                        return Ok(Some(Cmd::ToggleSpeaker));
                    }
                    #[cfg(feature = "share-screen")]
                    Ok(slash::SlashCommand::ShareScreen(args)) => {
                        return Ok(Some(Cmd::ShareScreen(args)));
                    }
                    Ok(slash::SlashCommand::Tools(tooling::ToolsCommand::Status)) => {
                        for line in tooling::status_lines(app.active_tools, app.desired_tools) {
                            app.sys(line);
                        }
                    }
                    Ok(slash::SlashCommand::Tools(tooling::ToolsCommand::List)) => {
                        for line in tooling::catalog_lines(app.active_tools, app.desired_tools) {
                            app.sys(line);
                        }
                    }
                    Ok(slash::SlashCommand::Tools(tooling::ToolsCommand::Enable(tool))) => {
                        if app.desired_tools.set(tool, true) {
                            app.sys(format!(
                                "staged tool enable: {} (run `/tools apply` to reconnect)",
                                tool.key()
                            ));
                        } else {
                            app.sys(format!("tool already staged on: {}", tool.key()));
                        }
                    }
                    Ok(slash::SlashCommand::Tools(tooling::ToolsCommand::Disable(tool))) => {
                        if app.desired_tools.set(tool, false) {
                            app.sys(format!(
                                "staged tool disable: {} (run `/tools apply` to reconnect)",
                                tool.key()
                            ));
                        } else {
                            app.sys(format!("tool already staged off: {}", tool.key()));
                        }
                    }
                    Ok(slash::SlashCommand::Tools(tooling::ToolsCommand::Toggle(tool))) => {
                        let enabled = app.desired_tools.toggle(tool);
                        let action = if enabled { "enable" } else { "disable" };
                        app.sys(format!(
                            "staged tool {action}: {} (run `/tools apply` to reconnect)",
                            tool.key()
                        ));
                    }
                    Ok(slash::SlashCommand::Tools(tooling::ToolsCommand::Apply)) => {
                        return Ok(Some(Cmd::ApplySessionConfig));
                    }
                    Ok(slash::SlashCommand::System(slash::SystemCommand::Show)) => {
                        app.sys(format!(
                            "system instruction active: {}",
                            summarize_optional_system_instruction(
                                app.active_system_instruction.as_deref()
                            )
                        ));
                        if app.active_system_instruction != app.desired_system_instruction {
                            app.sys(format!(
                                "system instruction staged: {}",
                                summarize_optional_system_instruction(
                                    app.desired_system_instruction.as_deref()
                                )
                            ));
                            app.sys("run `/system apply` to reconnect with the staged system instruction".into());
                        } else {
                            app.sys("system instruction staged: none".into());
                        }
                    }
                    Ok(slash::SlashCommand::System(slash::SystemCommand::Set(text))) => {
                        let normalized = text.trim().to_string();
                        if normalized.is_empty() {
                            app.sys("[system] system instruction cannot be empty; use `/system clear` instead".into());
                        } else {
                            app.desired_system_instruction = Some(normalized.clone());
                            app.sys(format!(
                                "staged system instruction: {} (run `/system apply` to reconnect)",
                                summarize_system_instruction(&normalized)
                            ));
                        }
                    }
                    Ok(slash::SlashCommand::System(slash::SystemCommand::Clear)) => {
                        app.desired_system_instruction = None;
                        app.sys(
                            "staged system instruction clear (run `/system apply` to reconnect)"
                                .into(),
                        );
                    }
                    Ok(slash::SlashCommand::System(slash::SystemCommand::Apply)) => {
                        return Ok(Some(Cmd::ApplySessionConfig));
                    }
                    Err(err) => app.sys(format!("[slash] {err}")),
                }
                return Ok(None);
            }
            if !trimmed.is_empty() {
                send_user_input(app, session, &trimmed).await?;
            }
        }
        KeyCode::Tab => {
            if !app.apply_selected_completion() {
                app.input.handle_key(key);
                app.refresh_completions();
            }
        }
        KeyCode::BackTab => {
            app.select_prev_completion();
        }
        KeyCode::Up if app.has_completions() => app.select_prev_completion(),
        KeyCode::Down if app.has_completions() => app.select_next_completion(),
        KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.quit = true
        }
        KeyCode::Esc => app.quit = true,
        _ => {
            app.input.handle_key(key);
            app.refresh_completions();
        }
    }
    Ok(None)
}

#[cfg(feature = "mic")]
async fn toggle_mic(
    app: &mut App,
    mic: &mut Option<audio_io::Mic>,
    tx: &mpsc::Sender<Vec<u8>>,
    aec: &std::sync::Arc<webrtc_audio_processing::Processor>,
    session: &Session,
) {
    if mic.is_some() {
        *mic = None;
        app.mic_on = false;
        app.sys("mic off".into());
        session.audio_stream_end().await.ok();
    } else {
        match audio_io::Mic::start(tx.clone(), aec.clone()) {
            Ok(m) => {
                app.sys(format!(
                    "mic on ({}Hz → AEC {}Hz)",
                    m.sample_rate, AEC_SEND_RATE
                ));
                app.mic_on = true;
                *mic = Some(m);
            }
            Err(e) => app.sys(format!("mic failed: {e}")),
        }
    }
}

#[cfg(feature = "speak")]
fn toggle_speaker(
    app: &mut App,
    speaker: &mut Option<audio_io::Speaker>,
    aec: &std::sync::Arc<webrtc_audio_processing::Processor>,
) {
    if speaker.is_some() {
        *speaker = None;
        app.speak_on = false;
        app.sys("speaker off".into());
    } else {
        match audio_io::Speaker::start(aec.clone()) {
            Ok(s) => {
                app.sys(format!("speaker on ({}Hz, AEC enabled)", s.device_rate));
                app.speak_on = true;
                *speaker = Some(s);
            }
            Err(e) => app.sys(format!("speaker failed: {e}")),
        }
    }
}

#[cfg(feature = "share-screen")]
fn handle_share_screen(
    app: &mut App,
    share: &mut Option<screen::ScreenShare>,
    tx: &mpsc::Sender<Vec<u8>>,
    args: &str,
) -> ScreenShareState {
    // "/share-screen list"
    if args == "list" {
        let targets = screen::list();
        if targets.is_empty() {
            app.sys("no capture targets found".into());
        } else {
            for t in &targets {
                app.sys(format!(
                    "  {}: [{}] {} ({}x{})",
                    t.id, t.kind, t.name, t.width, t.height
                ));
            }
        }
        return ScreenShareState::default();
    }

    // "/share-screen" (no args) — stop if active
    if args.is_empty() {
        if share.is_some() {
            *share = None;
            app.screen_on = false;
            app.sys("screen share stopped".into());
        } else {
            app.sys("usage: /share-screen list | /share-screen <id> [interval_secs]".into());
        }
        return ScreenShareState::default();
    }

    // "/share-screen <id> [interval]" — stop any active, start new
    let mut parts = args.split_whitespace();
    let id: usize = match parts.next().and_then(|s| s.parse().ok()) {
        Some(id) => id,
        None => {
            app.sys("invalid id — use /share-screen list".into());
            return ScreenShareState::default();
        }
    };
    let interval_secs: f64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1.0);
    let interval = std::time::Duration::from_secs_f64(interval_secs);

    // Stop previous
    *share = None;

    match screen::start(id, interval, tx.clone()) {
        Ok(s) => {
            app.sys(format!(
                "sharing \"{}\" every {:.1}s",
                s.target_name, interval_secs
            ));
            app.screen_on = true;
            *share = Some(s);
            ScreenShareState {
                target_id: Some(id),
                interval_secs: Some(interval_secs),
            }
        }
        Err(e) => {
            app.screen_on = false;
            app.sys(format!("screen share failed: {e}"));
            ScreenShareState {
                target_id: Some(id),
                interval_secs: Some(interval_secs),
            }
        }
    }
}

// ── Send user input ──────────────────────────────────────────────────────────

async fn send_user_input(
    app: &mut App,
    session: &Session,
    line: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let (text, file_paths) = media::parse_input(line);

    app.messages.push(Msg {
        role: Role::User,
        text: line.to_string(),
    });

    for path in &file_paths {
        match media::load(path) {
            Ok(m) => {
                app.sys(media::describe(path, &m));
                match m {
                    media::Media::Image { data, mime } => {
                        session.send_video(&data, mime).await?;
                    }
                    media::Media::Audio { pcm, sample_rate } => {
                        session.send_audio_at_rate(&pcm, sample_rate).await?;
                    }
                }
            }
            Err(e) => app.sys(format!("[skip] @{path}: {e}")),
        }
    }

    if !text.is_empty() {
        if !file_paths.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        session.send_text(&text).await?;
    }

    Ok(())
}

// ── Server events ────────────────────────────────────────────────────────────

fn handle_server_event(
    app: &mut App,
    event: ServerEvent,
    #[cfg(feature = "speak")] speaker: &Option<audio_io::Speaker>,
) {
    match event {
        ServerEvent::InputTranscription(text) => {
            app.messages.push(Msg {
                role: Role::Transcription,
                text,
            });
        }
        ServerEvent::OutputTranscription(text) => app.pending.push_str(&text),
        ServerEvent::ModelText(text) => app.pending.push_str(&text),
        ServerEvent::TurnComplete => {
            if !app.pending.is_empty() {
                app.messages.push(Msg {
                    role: Role::Model,
                    text: std::mem::take(&mut app.pending),
                });
            }
        }
        #[cfg(feature = "speak")]
        ServerEvent::ModelAudio(data) => {
            if let Some(s) = speaker {
                s.push(&data);
            }
        }
        ServerEvent::Interrupted => {
            // Discard speaker buffer to stop talking over the user.
            #[cfg(feature = "speak")]
            if let Some(s) = speaker {
                s.clear();
            }
        }
        ServerEvent::Error(e) => app.sys(format!("[error] {}", e.message)),
        ServerEvent::Closed { reason } => {
            if !reason.is_empty() {
                app.sys(format!("[closed] {reason}"));
            }
            app.quit = true;
        }
        _ => {}
    }
}

// ── Rendering ────────────────────────────────────────────────────────────────

fn render(frame: &mut ratatui::Frame, app: &mut App) {
    let completion_height = if app.has_completions() {
        app.completion_count() as u16 + 2
    } else {
        0
    };
    let [chat_area, completion_area, input_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(completion_height),
            Constraint::Length(3),
        ])
        .areas(frame.area());

    // Chat
    let mut lines: Vec<Line> = Vec::new();
    for msg in &app.messages {
        let (prefix, ps, ts) = match msg.role {
            Role::User => (
                "[you] ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
                Style::default(),
            ),
            Role::Transcription => (
                "[you] (transcription) ",
                Style::default().fg(Color::Cyan),
                Style::default().fg(Color::DarkGray),
            ),
            Role::Model => (
                "[model] ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
                Style::default(),
            ),
            Role::System => (
                "  ",
                Style::default().fg(Color::DarkGray),
                Style::default().fg(Color::DarkGray),
            ),
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, ps),
            Span::styled(msg.text.as_str(), ts),
        ]));
    }
    if !app.pending.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                "[model] ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(app.pending.as_str(), Style::default().fg(Color::DarkGray)),
        ]));
    }

    // Estimate total rendered lines accounting for word-wrap.
    let content_width = chat_area.width.saturating_sub(2) as usize; // minus borders
    let wrapped_lines: usize = lines
        .iter()
        .map(|line| {
            let line_width: usize = line.spans.iter().map(|s| s.content.len()).sum();
            if content_width == 0 {
                1
            } else {
                (line_width / content_width) + 1
            }
        })
        .sum();
    let visible = chat_area.height.saturating_sub(2) as usize;
    let scroll = wrapped_lines.saturating_sub(visible) as u16;

    let chat = Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(app.title.as_str()),
        )
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(chat, chat_area);

    // Input + status
    #[allow(unused_mut)]
    let mut status_parts: Vec<&str> = Vec::new();
    #[cfg(feature = "mic")]
    status_parts.push(if app.mic_on { "mic: ON" } else { "mic: off" });
    #[cfg(feature = "speak")]
    status_parts.push(if app.speak_on {
        "speak: ON"
    } else {
        "speak: off"
    });
    #[cfg(feature = "share-screen")]
    status_parts.push(if app.screen_on {
        "screen: ON"
    } else {
        "screen: off"
    });
    let status = format!(" {} ", status_parts.join(" | "));

    if app.has_completions() {
        let lines = app
            .completions
            .iter()
            .take(app.completion_count())
            .enumerate()
            .map(|(idx, item)| {
                let selected = idx == app.completion_index;
                let marker = if selected { "› " } else { "  " };
                let marker_style = if selected {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let label_style = if selected {
                    Style::default().fg(Color::White)
                } else {
                    Style::default().fg(Color::Cyan)
                };
                Line::from(vec![
                    Span::styled(marker, marker_style),
                    Span::styled(item.label.as_str(), label_style),
                    Span::raw(" "),
                    Span::styled(item.detail.as_str(), Style::default().fg(Color::DarkGray)),
                ])
            })
            .collect::<Vec<_>>();
        let completion = Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" completions: Tab accept, Up/Down select "),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(completion, completion_area);
    }

    let input_widget = app.input.render_widget(status);
    frame.render_widget(input_widget, input_area);
}
