//! CLI application state and reducer-style transitions.
//!
//! This module is the canonical home for state transitions that should remain
//! testable without a running terminal, audio device, or Live session.

use gemini_live::types::ServerEvent;
use gemini_live_runtime::{
    RuntimeEvent, RuntimeLifecycleEvent, RuntimeSendOperation, ToolCallOutcome,
};

use crate::input;
use crate::slash;
use crate::tooling::{self, ToolProfile};

/// UI/application command emitted by a reducer transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppCommand {
    #[cfg(feature = "mic")]
    ToggleMic,
    #[cfg(feature = "speak")]
    ToggleSpeaker,
    #[cfg(feature = "share-screen")]
    ShareScreen(String),
    ApplySessionConfig,
}

/// Side effects requested by server-event reduction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerEventEffect {
    None,
    #[cfg(feature = "speak")]
    PlayAudio(Vec<u8>),
    #[cfg(feature = "speak")]
    ClearAudio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionState {
    Connecting,
    Connected,
    Reconnecting,
    Closed,
}

pub(crate) struct App {
    pub(crate) messages: Vec<Msg>,
    pub(crate) pending: String,
    pub(crate) input: input::InputEditor,
    completions: Vec<slash::CompletionItem>,
    completion_index: usize,
    pub(crate) quit: bool,
    pub(crate) title: String,
    pub(crate) connection_state: ConnectionState,
    pub(crate) lagged_events: u64,
    pub(crate) send_failures: usize,
    pub(crate) last_send_failure: Option<String>,
    pub(crate) active_tools: ToolProfile,
    pub(crate) desired_tools: ToolProfile,
    pub(crate) active_system_instruction: Option<String>,
    pub(crate) desired_system_instruction: Option<String>,
    #[cfg(feature = "mic")]
    pub(crate) mic_on: bool,
    #[cfg(feature = "speak")]
    pub(crate) speak_on: bool,
    #[cfg(feature = "share-screen")]
    pub(crate) screen_on: bool,
}

pub(crate) struct Msg {
    pub(crate) role: Role,
    pub(crate) text: String,
}

#[derive(Clone, Copy)]
pub(crate) enum Role {
    User,
    Transcription,
    Model,
    System,
}

impl App {
    pub(crate) fn new(title: &str, tools: ToolProfile, system_instruction: Option<String>) -> Self {
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
            connection_state: ConnectionState::Connecting,
            lagged_events: 0,
            send_failures: 0,
            last_send_failure: None,
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

    pub(crate) fn sys(&mut self, text: String) {
        self.messages.push(Msg {
            role: Role::System,
            text,
        });
    }

    pub(crate) fn push_user_message(&mut self, text: impl Into<String>) {
        self.messages.push(Msg {
            role: Role::User,
            text: text.into(),
        });
    }

    pub(crate) fn refresh_completions(&mut self) {
        let selected = self
            .completions
            .get(self.completion_index)
            .map(|item| item.label.clone());
        self.completions = slash::completions(&self.input.text());
        self.completion_index = selected
            .and_then(|label| self.completions.iter().position(|item| item.label == label))
            .unwrap_or(0);
    }

    pub(crate) fn completion_count(&self) -> usize {
        self.completions.len().min(5)
    }

    pub(crate) fn completion_items(&self) -> &[slash::CompletionItem] {
        &self.completions
    }

    pub(crate) fn selected_completion_index(&self) -> usize {
        self.completion_index
    }

    pub(crate) fn has_completions(&self) -> bool {
        !self.completions.is_empty()
    }

    pub(crate) fn select_next_completion(&mut self) {
        if self.completions.is_empty() {
            return;
        }
        self.completion_index = (self.completion_index + 1) % self.completions.len();
    }

    pub(crate) fn select_prev_completion(&mut self) {
        if self.completions.is_empty() {
            return;
        }
        self.completion_index = if self.completion_index == 0 {
            self.completions.len() - 1
        } else {
            self.completion_index - 1
        };
    }

    pub(crate) fn apply_selected_completion(&mut self) -> bool {
        let Some(item) = self.completions.get(self.completion_index).cloned() else {
            return false;
        };
        self.input
            .replace_range(item.replace_range, &item.replacement);
        self.refresh_completions();
        true
    }

    pub(crate) fn has_staged_session_changes(&self) -> bool {
        self.active_tools != self.desired_tools
            || self.active_system_instruction != self.desired_system_instruction
    }

    pub(crate) fn mark_session_config_applied(&mut self) {
        self.active_tools = self.desired_tools;
        self.active_system_instruction = self.desired_system_instruction.clone();
        self.sys(format!(
            "reconnected with tools: {}",
            self.active_tools.summary()
        ));
        if let Some(system_instruction) = self.active_system_instruction.as_deref() {
            self.sys(format!(
                "system instruction active: {}",
                summarize_system_instruction(system_instruction)
            ));
        } else {
            self.sys("system instruction active: none".into());
        }
    }

    pub(crate) fn connection_label(&self) -> &'static str {
        match self.connection_state {
            ConnectionState::Connecting => "connecting",
            ConnectionState::Connected => "connected",
            ConnectionState::Reconnecting => "reconnecting",
            ConnectionState::Closed => "closed",
        }
    }

    pub(crate) fn apply_slash_command(
        &mut self,
        command: slash::SlashCommand,
    ) -> Option<AppCommand> {
        match command {
            #[cfg(feature = "mic")]
            slash::SlashCommand::ToggleMic => Some(AppCommand::ToggleMic),
            #[cfg(feature = "speak")]
            slash::SlashCommand::ToggleSpeaker => Some(AppCommand::ToggleSpeaker),
            #[cfg(feature = "share-screen")]
            slash::SlashCommand::ShareScreen(args) => Some(AppCommand::ShareScreen(args)),
            slash::SlashCommand::Tools(tooling::ToolsCommand::Status) => {
                for line in tooling::status_lines(self.active_tools, self.desired_tools) {
                    self.sys(line);
                }
                None
            }
            slash::SlashCommand::Tools(tooling::ToolsCommand::List) => {
                for line in tooling::catalog_lines(self.active_tools, self.desired_tools) {
                    self.sys(line);
                }
                None
            }
            slash::SlashCommand::Tools(tooling::ToolsCommand::Enable(tool)) => {
                if self.desired_tools.set(tool, true) {
                    self.sys(format!(
                        "staged tool enable: {} (run `/tools apply` to reconnect)",
                        tool.key()
                    ));
                } else {
                    self.sys(format!("tool already staged on: {}", tool.key()));
                }
                None
            }
            slash::SlashCommand::Tools(tooling::ToolsCommand::Disable(tool)) => {
                if self.desired_tools.set(tool, false) {
                    self.sys(format!(
                        "staged tool disable: {} (run `/tools apply` to reconnect)",
                        tool.key()
                    ));
                } else {
                    self.sys(format!("tool already staged off: {}", tool.key()));
                }
                None
            }
            slash::SlashCommand::Tools(tooling::ToolsCommand::Toggle(tool)) => {
                let enabled = self.desired_tools.toggle(tool);
                let action = if enabled { "enable" } else { "disable" };
                self.sys(format!(
                    "staged tool {action}: {} (run `/tools apply` to reconnect)",
                    tool.key()
                ));
                None
            }
            slash::SlashCommand::Tools(tooling::ToolsCommand::Apply) => {
                Some(AppCommand::ApplySessionConfig)
            }
            slash::SlashCommand::System(slash::SystemCommand::Show) => {
                self.sys(format!(
                    "system instruction active: {}",
                    summarize_optional_system_instruction(
                        self.active_system_instruction.as_deref()
                    )
                ));
                if self.active_system_instruction != self.desired_system_instruction {
                    self.sys(format!(
                        "system instruction staged: {}",
                        summarize_optional_system_instruction(
                            self.desired_system_instruction.as_deref()
                        )
                    ));
                    self.sys(
                        "run `/system apply` to reconnect with the staged system instruction"
                            .into(),
                    );
                } else {
                    self.sys("system instruction staged: none".into());
                }
                None
            }
            slash::SlashCommand::System(slash::SystemCommand::Set(text)) => {
                let normalized = text.trim().to_string();
                if normalized.is_empty() {
                    self.sys(
                        "[system] system instruction cannot be empty; use `/system clear` instead"
                            .into(),
                    );
                } else {
                    self.desired_system_instruction = Some(normalized.clone());
                    self.sys(format!(
                        "staged system instruction: {} (run `/system apply` to reconnect)",
                        summarize_system_instruction(&normalized)
                    ));
                }
                None
            }
            slash::SlashCommand::System(slash::SystemCommand::Clear) => {
                self.desired_system_instruction = None;
                self.sys(
                    "staged system instruction clear (run `/system apply` to reconnect)".into(),
                );
                None
            }
            slash::SlashCommand::System(slash::SystemCommand::Apply) => {
                Some(AppCommand::ApplySessionConfig)
            }
        }
    }

    pub(crate) fn apply_server_event(&mut self, event: ServerEvent) -> ServerEventEffect {
        match event {
            ServerEvent::InputTranscription(text) => {
                self.messages.push(Msg {
                    role: Role::Transcription,
                    text,
                });
                ServerEventEffect::None
            }
            ServerEvent::OutputTranscription(text) => {
                self.pending.push_str(&text);
                ServerEventEffect::None
            }
            ServerEvent::ModelText(text) => {
                self.pending.push_str(&text);
                ServerEventEffect::None
            }
            ServerEvent::TurnComplete => {
                if !self.pending.is_empty() {
                    self.messages.push(Msg {
                        role: Role::Model,
                        text: std::mem::take(&mut self.pending),
                    });
                }
                ServerEventEffect::None
            }
            #[cfg(feature = "speak")]
            ServerEvent::ModelAudio(data) => ServerEventEffect::PlayAudio(data),
            ServerEvent::Interrupted => {
                #[cfg(feature = "speak")]
                {
                    ServerEventEffect::ClearAudio
                }
                #[cfg(not(feature = "speak"))]
                {
                    ServerEventEffect::None
                }
            }
            ServerEvent::Error(e) => {
                self.sys(format!("[error] {}", e.message));
                ServerEventEffect::None
            }
            ServerEvent::Closed { reason } => {
                self.connection_state = ConnectionState::Closed;
                if !reason.is_empty() {
                    self.sys(format!("[closed] {reason}"));
                }
                self.quit = true;
                ServerEventEffect::None
            }
            _ => ServerEventEffect::None,
        }
    }

    pub(crate) fn apply_runtime_event(&mut self, event: RuntimeEvent) -> ServerEventEffect {
        match event {
            RuntimeEvent::Server(event) => self.apply_server_event(event),
            RuntimeEvent::Lifecycle(RuntimeLifecycleEvent::Connecting) => {
                self.connection_state = ConnectionState::Connecting;
                self.sys("connecting".into());
                ServerEventEffect::None
            }
            RuntimeEvent::Lifecycle(RuntimeLifecycleEvent::Connected) => {
                self.connection_state = ConnectionState::Connected;
                ServerEventEffect::None
            }
            RuntimeEvent::Lifecycle(RuntimeLifecycleEvent::Reconnecting) => {
                self.connection_state = ConnectionState::Reconnecting;
                self.sys("reconnecting".into());
                ServerEventEffect::None
            }
            RuntimeEvent::Lifecycle(RuntimeLifecycleEvent::AppliedResumedSession) => {
                self.connection_state = ConnectionState::Connected;
                self.sys("session carryover applied".into());
                ServerEventEffect::None
            }
            RuntimeEvent::Lifecycle(RuntimeLifecycleEvent::AppliedFreshSession) => {
                self.connection_state = ConnectionState::Connected;
                self.sys("fresh session applied".into());
                ServerEventEffect::None
            }
            RuntimeEvent::Lifecycle(RuntimeLifecycleEvent::Closed { reason }) => {
                self.connection_state = ConnectionState::Closed;
                if !reason.is_empty() {
                    self.sys(format!("[closed] {reason}"));
                }
                self.quit = true;
                ServerEventEffect::None
            }
            RuntimeEvent::Lagged { count } => {
                self.lagged_events = self.lagged_events.saturating_add(count);
                self.sys(format!("[lagged] missed {count} events"));
                ServerEventEffect::None
            }
            RuntimeEvent::ToolCallStarted { id, name } => {
                self.sys(format!("[tool] requested {name} ({id})"));
                ServerEventEffect::None
            }
            RuntimeEvent::ToolCallFinished { id, name, outcome } => {
                match outcome {
                    ToolCallOutcome::Responded => {
                        self.sys(format!("[tool] responded: {name} ({id})"));
                    }
                    ToolCallOutcome::Cancelled => {
                        self.sys(format!("[tool] cancelled {id}"));
                    }
                    ToolCallOutcome::Failed { reason } => {
                        self.sys(format!("[tool error] {name} ({id}): {reason}"));
                    }
                }
                ServerEventEffect::None
            }
            RuntimeEvent::SendFailed(failure) => {
                self.send_failures = self.send_failures.saturating_add(1);
                self.last_send_failure = Some(format!(
                    "{}: {}",
                    summarize_send_operation(failure.operation),
                    failure.reason
                ));
                self.sys(format!(
                    "[send error] {}: {}",
                    summarize_send_operation(failure.operation),
                    failure.reason
                ));
                ServerEventEffect::None
            }
        }
    }
}

pub(crate) fn summarize_optional_system_instruction(text: Option<&str>) -> String {
    match text {
        Some(text) => summarize_system_instruction(text),
        None => "none".into(),
    }
}

pub(crate) fn summarize_system_instruction(text: &str) -> String {
    summarize_status_detail(text, 60)
}

pub(crate) fn summarize_status_detail(text: &str, max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let max_chars = max_chars.max(1);
    if compact.chars().count() <= max_chars {
        compact
    } else {
        let summary = compact.chars().take(max_chars).collect::<String>();
        format!("{summary}...")
    }
}

fn summarize_send_operation(operation: RuntimeSendOperation) -> &'static str {
    match operation {
        RuntimeSendOperation::Raw => "raw send",
        RuntimeSendOperation::Text => "text send",
        RuntimeSendOperation::Audio => "audio send",
        RuntimeSendOperation::Video => "video send",
        RuntimeSendOperation::ToolResponse => "tool response send",
        RuntimeSendOperation::SessionClose => "session close",
    }
}

#[cfg(test)]
mod tests {
    use gemini_live::types::ApiError;
    use gemini_live_tools::workspace::WorkspaceToolSelection;

    use super::*;

    #[test]
    fn tools_enable_stages_profile_and_emits_message() {
        let mut app = App::new("test", ToolProfile::default(), None);

        let command = app.apply_slash_command(slash::SlashCommand::Tools(
            tooling::ToolsCommand::Enable(tooling::ToolId::RunCommand),
        ));

        assert_eq!(command, None);
        assert!(app.desired_tools.workspace.run_command);
        assert!(
            matches!(app.messages.last(), Some(Msg { text, .. }) if text.contains("staged tool enable: run-command"))
        );
    }

    #[test]
    fn system_show_reports_staged_instruction() {
        let mut app = App::new("test", ToolProfile::default(), Some("active".into()));
        app.desired_system_instruction = Some("staged".into());

        let command =
            app.apply_slash_command(slash::SlashCommand::System(slash::SystemCommand::Show));

        assert_eq!(command, None);
        assert!(
            app.messages
                .iter()
                .any(|msg| msg.text.contains("system instruction active: active"))
        );
        assert!(
            app.messages
                .iter()
                .any(|msg| msg.text.contains("system instruction staged: staged"))
        );
    }

    #[test]
    fn turn_complete_flushes_pending_model_text() {
        let mut app = App::new("test", ToolProfile::default(), None);
        app.apply_server_event(ServerEvent::ModelText("hello".into()));

        let effect = app.apply_server_event(ServerEvent::TurnComplete);

        assert_eq!(effect, ServerEventEffect::None);
        assert!(app.pending.is_empty());
        assert!(matches!(
            app.messages.last(),
            Some(Msg {
                role: Role::Model,
                text
            }) if text == "hello"
        ));
    }

    #[test]
    fn closed_event_sets_quit_and_logs_reason() {
        let mut app = App::new("test", ToolProfile::default(), None);

        app.apply_server_event(ServerEvent::Closed {
            reason: "bye".into(),
        });

        assert!(app.quit);
        assert!(
            app.messages
                .iter()
                .any(|msg| msg.text.contains("[closed] bye"))
        );
    }

    #[test]
    fn server_error_is_rendered_as_system_message() {
        let mut app = App::new("test", ToolProfile::default(), None);

        app.apply_server_event(ServerEvent::Error(ApiError {
            message: "oops".into(),
        }));

        assert!(
            app.messages
                .iter()
                .any(|msg| msg.text.contains("[error] oops"))
        );
    }

    #[test]
    fn mark_session_config_applied_promotes_staged_values() {
        let mut app = App::new("test", ToolProfile::default(), None);
        app.desired_tools.workspace = WorkspaceToolSelection {
            run_command: true,
            ..WorkspaceToolSelection::default()
        };
        app.desired_system_instruction = Some("be concise".into());

        app.mark_session_config_applied();

        assert_eq!(app.active_tools, app.desired_tools);
        assert_eq!(
            app.active_system_instruction,
            app.desired_system_instruction
        );
        assert!(
            app.messages
                .iter()
                .any(|msg| msg.text.contains("run-command"))
        );
    }

    #[test]
    fn ignored_setup_complete_event_has_no_effect() {
        let mut app = App::new("test", ToolProfile::default(), None);
        let effect = app.apply_server_event(ServerEvent::SetupComplete);
        assert_eq!(effect, ServerEventEffect::None);
    }

    #[test]
    fn runtime_tool_failure_is_rendered_as_system_message() {
        let mut app = App::new("test", ToolProfile::default(), None);

        let effect = app.apply_runtime_event(RuntimeEvent::ToolCallFinished {
            id: "call-1".into(),
            name: "read_file".into(),
            outcome: ToolCallOutcome::Failed {
                reason: "boom".into(),
            },
        });

        assert_eq!(effect, ServerEventEffect::None);
        assert!(
            app.messages
                .iter()
                .any(|msg| msg.text.contains("[tool error] read_file (call-1): boom"))
        );
    }

    #[test]
    fn reconnecting_runtime_event_updates_connection_state() {
        let mut app = App::new("test", ToolProfile::default(), None);

        let effect =
            app.apply_runtime_event(RuntimeEvent::Lifecycle(RuntimeLifecycleEvent::Reconnecting));

        assert_eq!(effect, ServerEventEffect::None);
        assert_eq!(app.connection_state, ConnectionState::Reconnecting);
        assert!(
            app.messages
                .iter()
                .any(|msg| msg.text.contains("reconnecting"))
        );
    }

    #[test]
    fn lagged_runtime_event_accumulates_missed_events() {
        let mut app = App::new("test", ToolProfile::default(), None);

        let effect = app.apply_runtime_event(RuntimeEvent::Lagged { count: 3 });

        assert_eq!(effect, ServerEventEffect::None);
        assert_eq!(app.lagged_events, 3);
        assert!(
            app.messages
                .iter()
                .any(|msg| msg.text.contains("[lagged] missed 3 events"))
        );
    }
}
