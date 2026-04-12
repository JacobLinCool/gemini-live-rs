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
//! Session switchover and runtime event fanout are delegated to the workspace
//! `gemini-live-runtime` crate. Shared tool execution contracts, harness-owned
//! budget wrapping, and durable background tasks live in `gemini-live-harness`,
//! while desktop media adapters live in `gemini-live-io`.
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
mod session;
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

use app::{App, AppCommand, ConnectionState, summarize_optional_system_instruction};
use desktop_control::DesktopControlPort;
use gemini_live::ServerEvent;
use gemini_live_harness::{
    Harness, HarnessController, HarnessRuntimeBridge, HarnessToolBudget,
    HarnessToolCompletionDisposition,
};
use gemini_live_runtime::{RuntimeEvent, RuntimeLifecycleEvent, RuntimeSession, WakeReason};
use session::{
    ApplySessionConfigDisposition, CLI_IDLE_TIMEOUT, apply_staged_setup, ensure_hot_session,
    new_session_manager,
};
use startup::{StartupConfig, build_cli_setup_with_tools, resolve_startup_config};

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

enum KeyAction {
    Command(AppCommand),
    SendUserInput(String),
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = CliArgs::parse();
    match cli.command {
        Some(CliCommand::Update) => return update::run().await,
        Some(CliCommand::Config) => {
            println!(
                "{}",
                profile::config_file_path(cli.profile.as_deref())?.display()
            );
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
    let harness = profile_store.open_harness()?;
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
    let initial_host_tools = tooling::ToolRuntime::new(
        workspace_root.clone(),
        app.active_tools,
        desktop_control_port.clone(),
    )?;
    let mut active_harness_controller =
        build_harness_controller(harness.clone(), initial_host_tools)?;
    let mut active_harness_runtime_bridge =
        HarnessRuntimeBridge::new(active_harness_controller.clone());
    let recovered_notifications = active_harness_controller.recover_orphaned_deliveries()?;
    if recovered_notifications > 0 {
        app.sys(format!(
            "requeued {} orphaned harness notifications",
            recovered_notifications
        ));
    }
    app.refresh_completions();
    let initial_setup_tools = active_harness_controller.advertised_tools();

    app.sys(format!("profile: {}", startup.profile_name));
    app.sys(format!("tools active: {}", app.active_tools.summary()));
    app.sys(format!(
        "system instruction active: {}",
        summarize_optional_system_instruction(app.active_system_instruction.as_deref())
    ));
    let (mut session_manager, mut runtime_events) = new_session_manager(
        &startup,
        initial_setup_tools,
        app.active_system_instruction.as_deref(),
    );
    let mut passive_notification_version =
        active_harness_controller.passive_notification_queue_version();
    let idle_sleep = tokio::time::sleep(CLI_IDLE_TIMEOUT);
    tokio::pin!(idle_sleep);
    let mut dormant_close_in_flight = false;
    let mut turn_in_flight = false;
    let mut tool_calls_in_flight = 0usize;

    #[cfg(any(feature = "mic", feature = "speak"))]
    let mut audio = desktop::DesktopAudio::new()?;
    #[cfg(feature = "share-screen")]
    let mut screen = desktop::DesktopScreen::new();

    let mut term_events = EventStream::new();

    #[cfg(any(feature = "mic", feature = "speak"))]
    audio
        .autostart(&startup, &mut app, &mut profile_store, &session_manager)
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
                    &session_manager,
                    #[cfg(any(feature = "mic", feature = "speak"))]
                    &mut audio,
                    #[cfg(feature = "share-screen")]
                    &mut screen,
                ).await;
                if try_dispatch_passive_notification(
                    &mut app,
                    &mut session_manager,
                    &active_harness_controller,
                    turn_in_flight,
                    tool_calls_in_flight,
                )
                .await?
                .is_some()
                {
                    turn_in_flight = true;
                    idle_sleep
                        .as_mut()
                        .reset(tokio::time::Instant::now() + CLI_IDLE_TIMEOUT);
                }
                passive_notification_version =
                    active_harness_controller.passive_notification_queue_version();
            }
            Some(Ok(ev)) = term_events.next() => {
                if let Event::Key(key) = ev
                    && key.kind == KeyEventKind::Press
                {
                    let previous_desired_tools = app.desired_tools;
                    let previous_desired_system_instruction =
                        app.desired_system_instruction.clone();
                    if let Some(action) = handle_key(&mut app, key)? {
                        #[allow(unused_mut)]
                        let mut handled_by_host = false;
                        let mut reset_idle_deadline = false;
                        match action {
                            KeyAction::SendUserInput(line) => {
                                let session = ensure_hot_session(
                                    &mut session_manager,
                                    WakeReason::TextInput,
                                )
                                .await?;
                                outbound::send_user_input(&mut app, &session, &line).await?;
                                turn_in_flight = true;
                                reset_idle_deadline = true;
                            }
                            KeyAction::Command(command) => {
                                #[cfg(any(feature = "mic", feature = "speak"))]
                                {
                                    handled_by_host = audio
                                        .handle_command(
                                            &command,
                                            &mut app,
                                            &mut profile_store,
                                            &session_manager,
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
                                        let desired_host_tools = tooling::ToolRuntime::new(
                                            workspace_root.clone(),
                                            app.desired_tools,
                                            desktop_control_port.clone(),
                                        )?;
                                        let desired_harness_controller =
                                            build_harness_controller(
                                                harness.clone(),
                                                desired_host_tools,
                                            )?;
                                        let desired_setup = build_cli_setup_with_tools(
                                            &startup,
                                            desired_harness_controller.advertised_tools(),
                                            app.desired_system_instruction.as_deref(),
                                        );
                                        match apply_staged_setup(
                                            &mut session_manager,
                                            desired_setup,
                                        )
                                        .await
                                        {
                                            Ok(ApplySessionConfigDisposition::Reconnected) => {
                                                active_harness_controller =
                                                    desired_harness_controller;
                                                active_harness_runtime_bridge =
                                                    HarnessRuntimeBridge::new(
                                                        active_harness_controller.clone(),
                                                    );
                                                app.mark_session_config_applied();
                                                profile_store.set_tool_profile(
                                                    app.active_tools,
                                                )?;
                                                profile_store.set_system_instruction(
                                                    app.active_system_instruction.clone(),
                                                )?;
                                                reset_idle_deadline = true;
                                            }
                                            Ok(
                                                ApplySessionConfigDisposition::ArmedForNextWake,
                                            ) => {
                                                active_harness_controller =
                                                    desired_harness_controller;
                                                active_harness_runtime_bridge =
                                                    HarnessRuntimeBridge::new(
                                                        active_harness_controller.clone(),
                                                    );
                                                app.mark_session_config_armed_for_next_wake();
                                                profile_store.set_tool_profile(
                                                    app.active_tools,
                                                )?;
                                                profile_store.set_system_instruction(
                                                    app.active_system_instruction.clone(),
                                                )?;
                                            }
                                            Err(error) => {
                                                app.sys(format!(
                                                    "failed to apply staged session config: {error}"
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if reset_idle_deadline {
                            idle_sleep
                                .as_mut()
                                .reset(tokio::time::Instant::now() + CLI_IDLE_TIMEOUT);
                        }
                    }
                    if app.desired_tools != previous_desired_tools {
                        profile_store.set_tool_profile(app.desired_tools)?;
                    }
                    if app.desired_system_instruction != previous_desired_system_instruction {
                        profile_store
                            .set_system_instruction(app.desired_system_instruction.clone())?;
                    }
                    if try_dispatch_passive_notification(
                        &mut app,
                        &mut session_manager,
                        &active_harness_controller,
                        turn_in_flight,
                        tool_calls_in_flight,
                    )
                    .await?
                    .is_some()
                    {
                        turn_in_flight = true;
                        idle_sleep
                            .as_mut()
                            .reset(tokio::time::Instant::now() + CLI_IDLE_TIMEOUT);
                    }
                    passive_notification_version =
                        active_harness_controller.passive_notification_queue_version();
                }
            }
            Some(event) = runtime_events.recv() => {
                if dormant_close_in_flight
                    && matches!(
                        event,
                        RuntimeEvent::Lifecycle(RuntimeLifecycleEvent::Closed { .. })
                    )
                {
                    dormant_close_in_flight = false;
                    continue;
                }
                let _ = active_harness_runtime_bridge.handle_runtime_event(&event);
                match &event {
                    RuntimeEvent::Server(ServerEvent::TurnComplete) => {
                        let _ = active_harness_controller.acknowledge_in_flight_notification()?;
                        turn_in_flight = false;
                    }
                    RuntimeEvent::Server(ServerEvent::Interrupted) => {
                        let _ = active_harness_controller.requeue_in_flight_notification()?;
                        turn_in_flight = false;
                    }
                    RuntimeEvent::ToolCallRequested { .. } => {
                        tool_calls_in_flight = tool_calls_in_flight.saturating_add(1);
                    }
                    RuntimeEvent::Lifecycle(RuntimeLifecycleEvent::Closed { .. }) => {
                        turn_in_flight = false;
                        tool_calls_in_flight = 0;
                    }
                    _ => {}
                }
                idle_sleep
                    .as_mut()
                    .reset(tokio::time::Instant::now() + CLI_IDLE_TIMEOUT);
                let effect = app.apply_runtime_event(event);
                #[cfg(any(feature = "mic", feature = "speak"))]
                audio.apply_server_effect(effect);
                #[cfg(not(any(feature = "mic", feature = "speak")))]
                match effect {
                    app::ServerEventEffect::None => {}
                }
                if try_dispatch_passive_notification(
                    &mut app,
                    &mut session_manager,
                    &active_harness_controller,
                    turn_in_flight,
                    tool_calls_in_flight,
                )
                .await?
                .is_some()
                {
                    turn_in_flight = true;
                    idle_sleep
                        .as_mut()
                        .reset(tokio::time::Instant::now() + CLI_IDLE_TIMEOUT);
                }
                passive_notification_version =
                    active_harness_controller.passive_notification_queue_version();
            }
            Some(forwarded) = active_harness_runtime_bridge.recv_and_forward_tool_completion(|responses| async {
                let session = ensure_hot_session(
                    &mut session_manager,
                    WakeReason::PassiveNotification,
                )
                .await?;
                session
                    .send_tool_response(responses)
                    .await
                    .map_err(gemini_live_runtime::RuntimeError::from)
            }) => {
                tool_calls_in_flight = tool_calls_in_flight.saturating_sub(1);
                match forwarded {
                    Ok(outcome) => {
                        if matches!(
                            outcome.disposition,
                            HarnessToolCompletionDisposition::Responded
                                | HarnessToolCompletionDisposition::Failed
                        ) {
                            turn_in_flight = true;
                            idle_sleep
                                .as_mut()
                                .reset(tokio::time::Instant::now() + CLI_IDLE_TIMEOUT);
                        }
                        match outcome.disposition {
                            HarnessToolCompletionDisposition::Responded => {
                                app.sys(format!(
                                    "[tool] responded: {} ({})",
                                    outcome.call_name, outcome.call_id
                                ));
                            }
                            HarnessToolCompletionDisposition::Failed => {
                                app.sys(format!(
                                    "[tool error] {} ({}): execution failed",
                                    outcome.call_name, outcome.call_id
                                ));
                            }
                            HarnessToolCompletionDisposition::Cancelled => {
                                app.sys(format!("[tool] cancelled {}", outcome.call_id));
                            }
                        }
                    }
                    Err(error) => {
                        app.sys(format!(
                            "[tool error] {} ({}): {}",
                            error.call_name, error.call_id, error.source
                        ));
                    }
                }
                if try_dispatch_passive_notification(
                    &mut app,
                    &mut session_manager,
                    &active_harness_controller,
                    turn_in_flight,
                    tool_calls_in_flight,
                )
                .await?
                .is_some()
                {
                    turn_in_flight = true;
                    idle_sleep
                        .as_mut()
                        .reset(tokio::time::Instant::now() + CLI_IDLE_TIMEOUT);
                }
                passive_notification_version =
                    active_harness_controller.passive_notification_queue_version();
            }
            _ = active_harness_controller.wait_for_passive_notification_signal(passive_notification_version) => {
                if try_dispatch_passive_notification(
                    &mut app,
                    &mut session_manager,
                    &active_harness_controller,
                    turn_in_flight,
                    tool_calls_in_flight,
                )
                .await?
                .is_some()
                {
                    turn_in_flight = true;
                    idle_sleep
                        .as_mut()
                        .reset(tokio::time::Instant::now() + CLI_IDLE_TIMEOUT);
                }
                passive_notification_version =
                    active_harness_controller.passive_notification_queue_version();
            }
            Some(captured) = mic_event => {
                #[cfg(feature = "mic")]
                {
                    audio.forward_captured(captured, &mut session_manager).await;
                    idle_sleep
                        .as_mut()
                        .reset(tokio::time::Instant::now() + CLI_IDLE_TIMEOUT);
                }
                #[cfg(not(feature = "mic"))]
                let _ = captured;
            }
            Some(frame) = screen_event => {
                #[cfg(feature = "share-screen")]
                {
                    screen.forward_frame(frame, &mut session_manager).await;
                    idle_sleep
                        .as_mut()
                        .reset(tokio::time::Instant::now() + CLI_IDLE_TIMEOUT);
                }
                #[cfg(not(feature = "share-screen"))]
                let _ = frame;
            }
            _ = &mut idle_sleep => {
                idle_sleep
                    .as_mut()
                    .reset(tokio::time::Instant::now() + CLI_IDLE_TIMEOUT);
                if session_manager.active_session().is_some()
                    && can_enter_dormant(
                        &app,
                        turn_in_flight,
                        tool_calls_in_flight,
                        active_harness_controller.current_in_flight_notification_id().is_some(),
                    )
                {
                    dormant_close_in_flight = true;
                    session_manager.enter_dormant().await?;
                    app.connection_state = ConnectionState::Dormant;
                    app.sys("session dormant".into());
                }
            }
        }
    }

    active_harness_controller.abort_all_tool_calls();
    if session_manager.active_session().is_some() {
        session_manager.enter_dormant().await?;
    }
    Ok(())
}

fn build_harness_controller(
    harness: Harness,
    host_tools: tooling::ToolRuntime,
) -> Result<HarnessController<tooling::ToolRuntime>, gemini_live_harness::HarnessError> {
    HarnessController::with_host_tools(harness, host_tools)
        .map(|controller| controller.with_budget(HarnessToolBudget::default()))
}

async fn try_dispatch_passive_notification(
    app: &mut App,
    session_manager: &mut session::CliSessionManager,
    harness_controller: &HarnessController<tooling::ToolRuntime>,
    turn_in_flight: bool,
    tool_calls_in_flight: usize,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    if !can_deliver_passive_notification(app, turn_in_flight, tool_calls_in_flight) {
        return Ok(None);
    }

    let had_in_flight = harness_controller.current_in_flight_notification_id();
    harness_controller
        .dispatch_passive_notification_once(|delivery| async {
            let prompt = delivery.prompt;
            let session = ensure_hot_session(session_manager, WakeReason::PassiveNotification)
                .await
                .map_err(|error| error.to_string())?;
            session
                .send_text(&prompt)
                .await
                .map_err(|error| error.to_string())
        })
        .await?;

    let current_in_flight = harness_controller.current_in_flight_notification_id();
    if had_in_flight.is_none()
        && let Some(notification_id) = current_in_flight
    {
        app.sys(format!(
            "[harness] delivered background notification {notification_id}"
        ));
        return Ok(Some(notification_id));
    }
    Ok(None)
}

fn can_deliver_passive_notification(
    app: &App,
    turn_in_flight: bool,
    tool_calls_in_flight: usize,
) -> bool {
    !turn_in_flight
        && tool_calls_in_flight == 0
        && app.pending.is_empty()
        && app.input.text().trim().is_empty()
        && {
            #[cfg(feature = "mic")]
            {
                !app.mic_on
            }
            #[cfg(not(feature = "mic"))]
            {
                true
            }
        }
        && {
            #[cfg(feature = "share-screen")]
            {
                !app.screen_on
            }
            #[cfg(not(feature = "share-screen"))]
            {
                true
            }
        }
}

fn can_enter_dormant(
    app: &App,
    turn_in_flight: bool,
    tool_calls_in_flight: usize,
    notification_in_flight: bool,
) -> bool {
    !turn_in_flight
        && tool_calls_in_flight == 0
        && !notification_in_flight
        && app.pending.is_empty()
        && {
            #[cfg(feature = "mic")]
            {
                !app.mic_on
            }
            #[cfg(not(feature = "mic"))]
            {
                true
            }
        }
        && {
            #[cfg(feature = "share-screen")]
            {
                !app.screen_on
            }
            #[cfg(not(feature = "share-screen"))]
            {
                true
            }
        }
}

// ── Commands ─────────────────────────────────────────────────────────────────

fn handle_key(
    app: &mut App,
    key: KeyEvent,
) -> Result<Option<KeyAction>, Box<dyn std::error::Error>> {
    match key.code {
        KeyCode::Enter => {
            let raw = app.input.take_text();
            app.refresh_completions();
            let trimmed = raw.trim().to_string();
            if let Some(command) = slash::parse(&trimmed) {
                match command {
                    Ok(command) => {
                        return Ok(app.apply_slash_command(command).map(KeyAction::Command));
                    }
                    Err(err) => app.sys(format!("[slash] {err}")),
                }
                return Ok(None);
            }
            if !trimmed.is_empty() {
                return Ok(Some(KeyAction::SendUserInput(trimmed)));
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
