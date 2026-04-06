//! Terminal setup and TUI rendering for the desktop CLI.
//!
//! Rendering stays in the CLI crate because it is product-specific presentation
//! logic, but it lives outside `main.rs` so state-to-view behavior is easier to
//! audit and test independently from the event loop.

use std::io;

use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::{App, Role, summarize_status_detail};

pub(crate) fn init_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(io::stdout(), EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(io::stdout()))
}

pub(crate) fn restore_terminal(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> io::Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

pub(crate) fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        crossterm::terminal::disable_raw_mode().ok();
        crossterm::execute!(io::stdout(), LeaveAlternateScreen).ok();
        original(info);
    }));
}

pub(crate) fn draw(frame: &mut ratatui::Frame, app: &mut App) {
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

    let lines = message_lines(app);
    let wrapped_lines = total_wrapped_lines(&lines, chat_area.width);
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

    if app.has_completions() {
        let completion = Paragraph::new(Text::from(completion_lines(app)))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" completions: Tab accept, Up/Down select "),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(completion, completion_area);
    }

    let input_widget = app.input.render_widget(status_line(app));
    frame.render_widget(input_widget, input_area);
}

fn message_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for msg in &app.messages {
        let (prefix, prefix_style, text_style) = match msg.role {
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
            Span::styled(prefix.to_string(), prefix_style),
            Span::styled(msg.text.clone(), text_style),
        ]));
    }

    if !app.pending.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                "[model] ".to_string(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(app.pending.clone(), Style::default().fg(Color::DarkGray)),
        ]));
    }

    lines
}

fn total_wrapped_lines(lines: &[Line<'_>], chat_width: u16) -> usize {
    let content_width = chat_width.saturating_sub(2) as usize;
    lines
        .iter()
        .map(|line| {
            let line_width: usize = line.spans.iter().map(|span| span.content.len()).sum();
            if content_width == 0 {
                1
            } else {
                (line_width / content_width) + 1
            }
        })
        .sum()
}

fn completion_lines(app: &App) -> Vec<Line<'static>> {
    app.completion_items()
        .iter()
        .take(app.completion_count())
        .enumerate()
        .map(|(idx, item)| {
            let selected = idx == app.selected_completion_index();
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
                Span::styled(marker.to_string(), marker_style),
                Span::styled(item.label.clone(), label_style),
                Span::raw(" "),
                Span::styled(item.detail.clone(), Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect()
}

fn status_line(app: &App) -> String {
    #[allow(unused_mut)]
    let mut status_parts: Vec<&str> = Vec::new();
    status_parts.push(app.connection_label());
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

    let mut status = format!(" {} ", status_parts.join(" | "));
    if app.lagged_events > 0 {
        status.push_str(&format!(" | lagged: {}", app.lagged_events));
    }
    if app.send_failures > 0 {
        status.push_str(&format!(" | send errors: {}", app.send_failures));
    }
    if let Some(last_send_failure) = app.last_send_failure.as_deref() {
        status.push_str(&format!(
            " | last send: {}",
            summarize_status_detail(last_send_failure, 36)
        ));
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tooling::ToolProfile;

    #[test]
    fn status_line_includes_runtime_counters() {
        let mut app = App::new("demo", ToolProfile::default(), None);
        app.lagged_events = 3;
        app.send_failures = 2;
        app.last_send_failure =
            Some("tool response failed because upstream session closed".to_string());

        let status = status_line(&app);
        assert!(status.contains("lagged: 3"));
        assert!(status.contains("send errors: 2"));
        assert!(status.contains("last send:"));
    }
}
