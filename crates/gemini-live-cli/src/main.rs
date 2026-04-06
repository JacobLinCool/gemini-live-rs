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
//! Session switchover, runtime event fanout, and tool-call orchestration are
//! delegated to the workspace `gemini-live-runtime` crate, while desktop media
//! adapters live in `gemini-live-io`.
//!
//! `main.rs` is now the CLI entrypoint and event-loop composition layer.
//! Keep product semantics next to their natural homes:
//!
//! - startup/profile resolution and the default session template: `startup.rs`
//! - desktop media host wiring: `desktop.rs`
//! - TUI rendering and terminal lifecycle: `render.rs`

mod app;
mod desktop;
mod desktop_control;
mod input;
mod media;
mod outbound;
mod profile;
mod render;
mod slash;
mod startup;
mod tooling;
mod update;

use std::io;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use app::{App, AppCommand, summarize_optional_system_instruction};
use desktop_control::DesktopControlPort;
use gemini_live_runtime::{GeminiSessionDriver, ManagedRuntime, RuntimeSession};
use startup::{StartupConfig, build_cli_setup, build_runtime_config, resolve_startup_config};

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

    render::install_panic_hook();
    let mut terminal = render::init_terminal()?;
    let result = run(&mut terminal, startup, profile_store).await;
    render::restore_terminal(&mut terminal)?;
    result
}

// ── Main loop ────────────────────────────────────────────────────────────────

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    startup: StartupConfig,
    mut profile_store: profile::ProfileStore,
) -> Result<(), Box<dyn std::error::Error>> {
    let workspace_root = std::env::current_dir()?;
    #[cfg(any(feature = "mic", feature = "speak", feature = "share-screen"))]
    let (desktop_control_port_impl, mut desktop_control_server) = desktop_control::channel();
    #[cfg(any(feature = "mic", feature = "speak", feature = "share-screen"))]
    let desktop_control_port: Option<Arc<dyn DesktopControlPort>> =
        Some(Arc::new(desktop_control_port_impl));
    #[cfg(not(any(feature = "mic", feature = "speak", feature = "share-screen")))]
    let desktop_control_port: Option<Arc<dyn DesktopControlPort>> = None;

    let mut app = App::new(
        &startup.connection_label(),
        startup.tool_profile,
        startup.system_instruction.clone(),
    );
    app.refresh_completions();
    let initial_tool_runtime = tooling::ToolRuntime::new(
        workspace_root.clone(),
        app.active_tools,
        desktop_control_port.clone(),
    )?;

    app.sys(format!("profile: {}", startup.profile_name));
    app.sys(format!("tools active: {}", app.active_tools.summary()));
    app.sys(format!(
        "system instruction active: {}",
        summarize_optional_system_instruction(app.active_system_instruction.as_deref())
    ));
    let (mut runtime, mut runtime_events) = ManagedRuntime::new(
        build_runtime_config(
            &startup,
            app.active_tools,
            app.active_system_instruction.as_deref(),
        ),
        GeminiSessionDriver,
        initial_tool_runtime,
    );
    runtime.connect().await?;

    #[cfg(any(feature = "mic", feature = "speak"))]
    let mut audio = desktop::DesktopAudio::new()?;
    #[cfg(feature = "share-screen")]
    let mut screen = desktop::DesktopScreen::new();

    let mut term_events = EventStream::new();

    #[cfg(any(feature = "mic", feature = "speak"))]
    audio
        .autostart(&startup, &mut app, &mut profile_store, &runtime)
        .await?;
    #[cfg(feature = "share-screen")]
    screen.autostart(&startup, &mut app, &mut profile_store)?;

    loop {
        terminal.draw(|f| render::draw(f, &mut app))?;
        if app.quit {
            break;
        }

        #[cfg(feature = "mic")]
        let mic_event = audio.next_captured();
        #[cfg(not(feature = "mic"))]
        let mic_event = std::future::pending::<Option<()>>();

        #[cfg(feature = "share-screen")]
        let screen_event = screen.next_frame();
        #[cfg(not(feature = "share-screen"))]
        let screen_event = std::future::pending::<Option<()>>();

        #[cfg(any(feature = "mic", feature = "speak", feature = "share-screen"))]
        let desktop_control_event = desktop_control_server.recv();
        #[cfg(not(any(feature = "mic", feature = "speak", feature = "share-screen")))]
        let desktop_control_event =
            std::future::pending::<Option<desktop_control::DesktopControlRequest>>();

        tokio::select! {
            Some(request) = desktop_control_event => {
                desktop::handle_control_request(
                    request,
                    &mut app,
                    &mut profile_store,
                    &runtime,
                    #[cfg(any(feature = "mic", feature = "speak"))]
                    &mut audio,
                    #[cfg(feature = "share-screen")]
                    &mut screen,
                ).await;
            }
            Some(Ok(ev)) = term_events.next() => {
                if let Event::Key(key) = ev
                    && key.kind == KeyEventKind::Press
                {
                    let previous_desired_tools = app.desired_tools;
                    let previous_desired_system_instruction =
                        app.desired_system_instruction.clone();
                    let session = runtime
                        .active_session()
                        .expect("runtime session must exist while the CLI event loop is running");
                    if let Some(command) = handle_key(&mut app, key, &session).await? {
                        #[allow(unused_mut)]
                        let mut handled_by_host = false;
                        #[cfg(any(feature = "mic", feature = "speak"))]
                        {
                            handled_by_host = audio
                                .handle_command(
                                    &command,
                                    &mut app,
                                    &mut profile_store,
                                    &runtime,
                                )
                                .await?
                                || handled_by_host;
                        }
                        #[cfg(feature = "share-screen")]
                        {
                            handled_by_host = screen
                                .handle_command(&command, &mut app, &mut profile_store)?
                                || handled_by_host;
                        }
                        if !handled_by_host && command == AppCommand::ApplySessionConfig {
                            if !app.has_staged_session_changes() {
                                app.sys("session config already active".into());
                            } else {
                                runtime.replace_desired_setup(build_cli_setup(
                                    &startup,
                                    app.desired_tools,
                                    app.desired_system_instruction.as_deref(),
                                ));
                                runtime.replace_desired_tool_adapter(tooling::ToolRuntime::new(
                                    workspace_root.clone(),
                                    app.desired_tools,
                                    desktop_control_port.clone(),
                                )?);
                                match runtime.apply().await {
                                    Ok(_report) => {
                                        app.mark_session_config_applied();
                                        profile_store.set_tool_profile(app.active_tools)?;
                                        profile_store.set_system_instruction(
                                            app.active_system_instruction.clone(),
                                        )?;
                                    }
                                    Err(e) => {
                                        app.sys(format!(
                                            "failed to apply staged session config: {e}"
                                        ));
                                    }
                                }
                            }
                        }
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
            Some(event) = runtime_events.recv() => {
                let effect = app.apply_runtime_event(event);
                #[cfg(any(feature = "mic", feature = "speak"))]
                audio.apply_server_effect(effect);
                #[cfg(not(any(feature = "mic", feature = "speak")))]
                match effect {
                    app::ServerEventEffect::None => {}
                }
            }
            Some(captured) = mic_event => {
                #[cfg(feature = "mic")]
                audio.forward_captured(captured, &runtime).await;
                #[cfg(not(feature = "mic"))]
                let _ = captured;
            }
            Some(frame) = screen_event => {
                #[cfg(feature = "share-screen")]
                screen.forward_frame(frame, &runtime).await;
                #[cfg(not(feature = "share-screen"))]
                let _ = frame;
            }
        }
    }

    runtime.close().await?;
    Ok(())
}

// ── Commands ─────────────────────────────────────────────────────────────────

async fn handle_key<S>(
    app: &mut App,
    key: KeyEvent,
    session: &S,
) -> Result<Option<AppCommand>, Box<dyn std::error::Error>>
where
    S: RuntimeSession,
{
    match key.code {
        KeyCode::Enter => {
            let raw = app.input.take_text();
            app.refresh_completions();
            let trimmed = raw.trim().to_string();
            if let Some(command) = slash::parse(&trimmed) {
                match command {
                    Ok(command) => return Ok(app.apply_slash_command(command)),
                    Err(err) => app.sys(format!("[slash] {err}")),
                }
                return Ok(None);
            }
            if !trimmed.is_empty() {
                outbound::send_user_input(app, session, &trimmed).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_config_subcommand_parses() {
        let cli = CliArgs::try_parse_from(["gemini-live", "config"]).expect("cli args");
        assert!(matches!(cli.command, Some(CliCommand::Config)));
    }
}
