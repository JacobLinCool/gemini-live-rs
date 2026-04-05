//! Single-line input widget wrapper for the interactive CLI.
//!
//! The CLI intentionally uses a dedicated editor widget instead of manually
//! pushing chars into a `String`, so cursor movement, deletion semantics, and
//! future history/search support all have a stable home.

use std::ops::Range;

use crossterm::event::KeyEvent;
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders};
use tui_textarea::{CursorMove, Input, Key, TextArea};

pub struct InputEditor {
    textarea: TextArea<'static>,
}

impl InputEditor {
    pub fn new() -> Self {
        Self {
            textarea: new_textarea(""),
        }
    }

    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    pub fn set_text(&mut self, text: impl AsRef<str>) {
        self.textarea = new_textarea(text.as_ref());
    }

    pub fn take_text(&mut self) -> String {
        let text = self.text();
        self.clear();
        text
    }

    pub fn clear(&mut self) {
        self.set_text("");
    }

    pub fn replace_range(&mut self, range: Range<usize>, replacement: &str) {
        let mut text = self.text();
        text.replace_range(range, replacement);
        self.set_text(text);
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        match Input::from(key) {
            Input {
                key: Key::Enter, ..
            } => {}
            input => {
                self.textarea.input(input);
            }
        }
    }

    pub fn render_widget(&mut self, status_title: impl Into<String>) -> &TextArea<'static> {
        self.textarea.set_block(
            Block::default()
                .borders(Borders::ALL)
                .title(status_title.into()),
        );
        &self.textarea
    }
}

fn new_textarea(text: &str) -> TextArea<'static> {
    let mut textarea = TextArea::from([text.replace(['\n', '\r'], " ")]);
    textarea.set_cursor_line_style(Style::default());
    textarea.set_block(Block::default().borders(Borders::ALL));
    textarea.move_cursor(CursorMove::End);
    textarea
}
