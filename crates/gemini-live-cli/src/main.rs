mod media;

use std::io;

use base64::Engine;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;
use tokio::sync::mpsc;

use gemini_live::session::{ReconnectPolicy, Session, SessionConfig};
use gemini_live::transport::{Auth, TransportConfig};
use gemini_live::types::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(io::stderr)
        .init();

    let api_key =
        std::env::var("GEMINI_API_KEY").expect("set GEMINI_API_KEY environment variable");
    let model = std::env::var("GEMINI_MODEL")
        .unwrap_or_else(|_| "models/gemini-3.1-flash-live-preview".into());

    // Connect before entering TUI
    eprintln!("connecting to {model}…");
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

    // Setup terminal
    install_panic_hook();
    let mut terminal = init_terminal()?;
    let result = run(&mut terminal, session, &model).await;
    restore_terminal(&mut terminal)?;
    result
}

// ── Terminal setup ───────────────────────────────────────────────────────────

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
    /// Accumulates streaming model response until TurnComplete.
    pending: String,
    input: String,
    quit: bool,
    title: String,
}

struct Msg {
    role: Role,
    text: String,
}

#[derive(Clone, Copy)]
enum Role {
    User,
    Model,
    System,
}

impl App {
    fn new(model: &str) -> Self {
        Self {
            messages: vec![Msg {
                role: Role::System,
                text: "connected — @file.jpg / @file.wav for media".to_string(),
            }],
            pending: String::new(),
            input: String::new(),
            quit: false,
            title: model.to_string(),
        }
    }
}

// ── Main loop ────────────────────────────────────────────────────────────────

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    session: Session,
    model: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new(model);

    // Bridge session events into a channel so we can select! on them.
    let (srv_tx, mut srv_rx) = mpsc::unbounded_channel();
    let mut recv = session.clone();
    tokio::spawn(async move {
        while let Some(ev) = recv.next_event().await {
            if srv_tx.send(ev).is_err() {
                break;
            }
        }
    });

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
                    handle_key(&mut app, key.code, key.modifiers, &session).await?;
                }
            }
            Some(srv) = srv_rx.recv() => {
                handle_server_event(&mut app, srv);
            }
        }
    }

    session.close().await.ok();
    Ok(())
}

// ── Key handling ─────────────────────────────────────────────────────────────

async fn handle_key(
    app: &mut App,
    code: KeyCode,
    mods: KeyModifiers,
    session: &Session,
) -> Result<(), Box<dyn std::error::Error>> {
    match code {
        KeyCode::Enter => {
            let raw = std::mem::take(&mut app.input);
            let trimmed = raw.trim().to_string();
            if !trimmed.is_empty() {
                send_user_input(app, session, &trimmed).await?;
            }
        }
        KeyCode::Char('c' | 'd') if mods.contains(KeyModifiers::CONTROL) => {
            app.quit = true;
        }
        KeyCode::Esc => {
            app.quit = true;
        }
        KeyCode::Char(c) => {
            app.input.push(c);
        }
        KeyCode::Backspace => {
            app.input.pop();
        }
        _ => {}
    }
    Ok(())
}

async fn send_user_input(
    app: &mut App,
    session: &Session,
    line: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let (text, file_paths) = media::parse_input(line);

    // Show the full input as a user message.
    app.messages.push(Msg {
        role: Role::User,
        text: line.to_string(),
    });

    // Send media files.
    for path in &file_paths {
        match media::load(path) {
            Ok(m) => {
                app.messages.push(Msg {
                    role: Role::System,
                    text: media::describe(path, &m),
                });
                match m {
                    media::Media::Image { data, mime } => {
                        session.send_video(&data, mime).await?;
                    }
                    media::Media::Audio { pcm, sample_rate } => {
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&pcm);
                        session
                            .send_raw(ClientMessage::RealtimeInput(RealtimeInput {
                                audio: Some(Blob {
                                    data: b64,
                                    mime_type: format!("audio/pcm;rate={sample_rate}"),
                                }),
                                video: None,
                                text: None,
                                activity_start: None,
                                activity_end: None,
                                audio_stream_end: None,
                            }))
                            .await?;
                    }
                }
            }
            Err(e) => {
                app.messages.push(Msg {
                    role: Role::System,
                    text: format!("[skip] @{path}: {e}"),
                });
            }
        }
    }

    // Send text (with brief delay after media for model processing).
    if !text.is_empty() {
        if !file_paths.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        session.send_text(&text).await?;
    }

    Ok(())
}

// ── Server events ────────────────────────────────────────────────────────────

fn handle_server_event(app: &mut App, event: ServerEvent) {
    match event {
        ServerEvent::OutputTranscription(text) => {
            app.pending.push_str(&text);
        }
        ServerEvent::TurnComplete => {
            if !app.pending.is_empty() {
                app.messages.push(Msg {
                    role: Role::Model,
                    text: std::mem::take(&mut app.pending),
                });
            }
        }
        ServerEvent::ModelText(text) => {
            // Fallback for text-mode models (no audio transcription).
            app.pending.push_str(&text);
        }
        ServerEvent::Error(e) => {
            app.messages.push(Msg {
                role: Role::System,
                text: format!("[error] {}", e.message),
            });
        }
        ServerEvent::Closed { reason } => {
            if !reason.is_empty() {
                app.messages.push(Msg {
                    role: Role::System,
                    text: format!("[closed] {reason}"),
                });
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

    // ── Chat ─────────────────────────────────────────────────────────────
    let mut lines: Vec<Line> = Vec::new();
    for msg in &app.messages {
        let (prefix, prefix_style, text_style) = match msg.role {
            Role::User => (
                "[you] ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
                Style::default(),
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
            Span::styled(prefix, prefix_style),
            Span::styled(msg.text.as_str(), text_style),
        ]));
    }

    // Streaming partial response.
    if !app.pending.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                "[model] ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                app.pending.as_str(),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    // Auto-scroll: estimate scroll offset so the bottom is visible.
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

    // ── Input ────────────────────────────────────────────────────────────
    let input = Paragraph::new(Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Cyan)),
        Span::raw(app.input.as_str()),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(input, input_area);

    // Cursor
    let cx = (input_area.x + 3 + app.input.len() as u16).min(input_area.right() - 2);
    let cy = input_area.y + 1;
    frame.set_cursor_position(Position::new(cx, cy));
}
