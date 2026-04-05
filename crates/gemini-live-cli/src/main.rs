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
//! - `@file`         — send an image or audio file inline

#[cfg(any(feature = "mic", feature = "speak"))]
mod audio_io;
mod media;
#[cfg(feature = "share-screen")]
mod screen;

use std::io;

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use tokio::sync::mpsc;

use gemini_live::session::{ReconnectPolicy, Session, SessionConfig};
use gemini_live::transport::{Auth, TransportConfig};
use gemini_live::types::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(io::stderr)
        .init();

    let api_key = std::env::var("GEMINI_API_KEY").expect("set GEMINI_API_KEY environment variable");
    let model = std::env::var("GEMINI_MODEL")
        .unwrap_or_else(|_| "models/gemini-3.1-flash-live-preview".into());

    eprintln!("connecting to {model}...");
    let session = Session::connect(SessionConfig {
        transport: TransportConfig {
            auth: Auth::ApiKey(api_key),
            ..Default::default()
        },
        setup: SetupConfig {
            model: model.clone(),
            generation_config: Some(GenerationConfig {
                response_modalities: Some(vec![Modality::Audio]),
                media_resolution: Some(MediaResolution::MediaResolutionLow),
                ..Default::default()
            }),
            output_audio_transcription: Some(AudioTranscriptionConfig {}),
            ..Default::default()
        },
        reconnect: ReconnectPolicy::default(),
    })
    .await?;

    install_panic_hook();
    let mut terminal = init_terminal()?;
    let result = run(&mut terminal, session, &model).await;
    restore_terminal(&mut terminal)?;
    result
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

// ── App state ────────────────────────────────────────────────────────────────

struct App {
    messages: Vec<Msg>,
    pending: String,
    input: String,
    quit: bool,
    title: String,
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

impl App {
    fn new(model: &str) -> Self {
        Self {
            messages: vec![Msg {
                role: Role::System,
                text: "connected — @file for media, /mic /speak to toggle audio".to_string(),
            }],
            pending: String::new(),
            input: String::new(),
            quit: false,
            title: model.to_string(),
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
}

// ── Main loop ────────────────────────────────────────────────────────────────

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    session: Session,
    model: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new(model);

    // Session events → channel
    let (srv_tx, mut srv_rx) = mpsc::unbounded_channel();
    let mut recv = session.clone();
    tokio::spawn(async move {
        while let Some(ev) = recv.next_event().await {
            if srv_tx.send(ev).is_err() {
                break;
            }
        }
    });

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

    loop {
        terminal.draw(|f| render(f, &app))?;
        if app.quit {
            break;
        }

        tokio::select! {
            Some(Ok(ev)) = term_events.next() => {
                if let Event::Key(key) = ev
                    && key.kind == KeyEventKind::Press
                {
                    match handle_key(&mut app, key.code, key.modifiers, &session).await? {
                        #[cfg(feature = "mic")]
                        Some(Cmd::ToggleMic) => toggle_mic(&mut app, &mut mic, &mic_tx, &session).await,
                        #[cfg(feature = "speak")]
                        Some(Cmd::ToggleSpeaker) => toggle_speaker(&mut app, &mut speaker),
                        #[cfg(feature = "share-screen")]
                        Some(Cmd::ShareScreen(args)) => handle_share_screen(&mut app, &mut screen_share, &screen_tx, &args),
                        None => {}
                    }
                }
            }
            Some(srv) = srv_rx.recv() => {
                handle_server_event(&mut app, srv,
                    #[cfg(feature = "speak")] &speaker,
                );
            }
            Some(pcm) = mic_rx.recv() => {
                #[cfg(feature = "mic")]
                {
                    let rate = mic.as_ref().map(|m| m.sample_rate).unwrap_or(16_000);
                    session.send_audio_at_rate(&pcm, rate).await.ok();
                }
                #[cfg(not(feature = "mic"))]
                drop(pcm);
            }
            Some(jpeg) = screen_rx.recv() => {
                session.send_video(&jpeg, "image/jpeg").await.ok();
            }
        }
    }

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
}

async fn handle_key(
    app: &mut App,
    code: KeyCode,
    mods: KeyModifiers,
    session: &Session,
) -> Result<Option<Cmd>, Box<dyn std::error::Error>> {
    match code {
        KeyCode::Enter => {
            let raw = std::mem::take(&mut app.input);
            let trimmed = raw.trim().to_string();
            #[cfg(feature = "mic")]
            if trimmed == "/mic" {
                return Ok(Some(Cmd::ToggleMic));
            }
            #[cfg(feature = "speak")]
            if trimmed == "/speak" {
                return Ok(Some(Cmd::ToggleSpeaker));
            }
            #[cfg(feature = "share-screen")]
            if let Some(args) = trimmed.strip_prefix("/share-screen") {
                return Ok(Some(Cmd::ShareScreen(args.trim().to_string())));
            }
            if !trimmed.is_empty() {
                send_user_input(app, session, &trimmed).await?;
            }
        }
        KeyCode::Char('c' | 'd') if mods.contains(KeyModifiers::CONTROL) => app.quit = true,
        KeyCode::Esc => app.quit = true,
        KeyCode::Char(c) => app.input.push(c),
        KeyCode::Backspace => {
            app.input.pop();
        }
        _ => {}
    }
    Ok(None)
}

#[cfg(feature = "mic")]
async fn toggle_mic(
    app: &mut App,
    mic: &mut Option<audio_io::Mic>,
    tx: &mpsc::Sender<Vec<u8>>,
    session: &Session,
) {
    if mic.is_some() {
        *mic = None;
        app.mic_on = false;
        app.sys("mic off".into());
        session.audio_stream_end().await.ok();
    } else {
        match audio_io::Mic::start(tx.clone()) {
            Ok(m) => {
                app.sys(format!("mic on ({}Hz)", m.sample_rate));
                app.mic_on = true;
                *mic = Some(m);
            }
            Err(e) => app.sys(format!("mic failed: {e}")),
        }
    }
}

#[cfg(feature = "speak")]
fn toggle_speaker(app: &mut App, speaker: &mut Option<audio_io::Speaker>) {
    if speaker.is_some() {
        *speaker = None;
        app.speak_on = false;
        app.sys("speaker off".into());
    } else {
        match audio_io::Speaker::start() {
            Ok(s) => {
                app.sys(format!("speaker on ({}Hz)", s.device_rate));
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
) {
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
        return;
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
        return;
    }

    // "/share-screen <id> [interval]" — stop any active, start new
    let mut parts = args.split_whitespace();
    let id: usize = match parts.next().and_then(|s| s.parse().ok()) {
        Some(id) => id,
        None => {
            app.sys("invalid id — use /share-screen list".into());
            return;
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
        }
        Err(e) => {
            app.screen_on = false;
            app.sys(format!("screen share failed: {e}"));
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

fn render(frame: &mut ratatui::Frame, app: &App) {
    let [chat_area, input_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
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

    let visible = chat_area.height.saturating_sub(2) as usize;
    let scroll = lines.len().saturating_sub(visible) as u16;

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

    let input = Paragraph::new(Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Cyan)),
        Span::raw(app.input.as_str()),
    ]))
    .block(Block::default().borders(Borders::ALL).title(status));
    frame.render_widget(input, input_area);

    let cx = (input_area.x + 3 + app.input.len() as u16).min(input_area.right().saturating_sub(2));
    frame.set_cursor_position(Position::new(cx, input_area.y + 1));
}
