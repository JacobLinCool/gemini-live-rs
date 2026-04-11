//! Slash-command grammar and completion model for the interactive CLI.
//!
//! Parsing is intentionally structured rather than ad-hoc string matching:
//! `clap` owns the accepted command grammar while the completion engine uses
//! the same command catalog to suggest valid next tokens.

use std::ops::Range;

use clap::{Parser, Subcommand, ValueEnum};

use crate::tooling::{ToolId, ToolsCommand};

#[derive(Debug, Clone, PartialEq)]
pub enum SlashCommand {
    #[cfg(feature = "mic")]
    ToggleMic,
    #[cfg(feature = "speak")]
    ToggleSpeaker,
    #[cfg(feature = "share-screen")]
    ShareScreen(String),
    Tools(ToolsCommand),
    System(SystemCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemCommand {
    Show,
    Set(String),
    Clear,
    Apply,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    pub replacement: String,
    pub replace_range: Range<usize>,
    pub detail: String,
}

pub fn parse(input: &str) -> Option<Result<SlashCommand, String>> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let Some(mut argv) = shlex::split(trimmed) else {
        return Some(Err("invalid quoting in slash command".into()));
    };
    if argv.is_empty() {
        return Some(Err("empty slash command".into()));
    }
    argv[0] = argv[0].trim_start_matches('/').to_string();

    Some(
        CliSlashCommand::try_parse_from(std::iter::once("slash".to_string()).chain(argv))
            .map(Into::into)
            .map_err(|e| e.to_string().trim().to_string()),
    )
}

pub fn completions(input: &str) -> Vec<CompletionItem> {
    let left_trimmed = input.trim_start();
    let leading_offset = input.len() - left_trimmed.len();
    if !left_trimmed.starts_with('/') {
        return Vec::new();
    }

    let trailing_space = input.chars().last().is_some_and(char::is_whitespace);
    let current_range = current_token_range(input);
    let current_fragment = if trailing_space {
        ""
    } else {
        &input[current_range.clone()]
    };
    let parts = left_trimmed.split_whitespace().collect::<Vec<_>>();
    if parts.is_empty() {
        return command_completions("/", leading_offset..input.len(), true);
    }

    let command = parts[0];
    let normalized = command.trim_start_matches('/');
    let recognized = command_specs()
        .iter()
        .any(|spec| spec.name.trim_start_matches('/') == normalized);

    if parts.len() == 1 && !recognized {
        return command_completions(current_fragment, current_range, false);
    }

    match normalized {
        #[cfg(feature = "share-screen")]
        "share-screen" => share_screen_completions(
            parts.as_slice(),
            trailing_space,
            current_fragment,
            current_range,
            input.len(),
        ),
        "system" => system_completions(
            parts.as_slice(),
            trailing_space,
            current_fragment,
            current_range,
            input.len(),
        ),
        "tools" => tools_completions(
            parts.as_slice(),
            trailing_space,
            current_fragment,
            current_range,
            input.len(),
        ),
        _ => Vec::new(),
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "slash",
    disable_help_flag = true,
    disable_help_subcommand = true
)]
struct CliSlashCommand {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    #[cfg(feature = "mic")]
    #[command(name = "mic")]
    Mic,
    #[cfg(feature = "speak")]
    #[command(name = "speak")]
    Speak,
    #[cfg(feature = "share-screen")]
    #[command(name = "share-screen")]
    ShareScreen {
        target: Option<String>,
        interval_secs: Option<f64>,
    },
    #[command(name = "system")]
    System {
        #[command(subcommand)]
        action: Option<CliSystemCommand>,
    },
    #[command(name = "tools")]
    Tools {
        #[command(subcommand)]
        action: Option<CliToolsCommand>,
    },
}

#[derive(Debug, Subcommand)]
enum CliToolsCommand {
    Status,
    List,
    Enable { tool: CliToolArg },
    Disable { tool: CliToolArg },
    Toggle { tool: CliToolArg },
    Apply,
}

#[derive(Debug, Subcommand)]
enum CliSystemCommand {
    Show,
    Set { text: Vec<String> },
    Clear,
    Apply,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum CliToolArg {
    #[value(name = "google-search", alias = "google_search", alias = "search")]
    GoogleSearch,
    #[value(
        name = "timer",
        alias = "set-timer",
        alias = "set_timer",
        alias = "alert",
        alias = "reminder"
    )]
    Timer,
    #[value(name = "list-files", alias = "list_files", alias = "list")]
    ListFiles,
    #[value(name = "read-file", alias = "read_file", alias = "read")]
    ReadFile,
    #[value(
        name = "run-command",
        alias = "run_command",
        alias = "command",
        alias = "terminal",
        alias = "run-terminal",
        alias = "run_terminal"
    )]
    RunCommand,
    #[cfg(feature = "mic")]
    #[value(
        name = "desktop-microphone",
        alias = "desktop_microphone",
        alias = "microphone",
        alias = "mic-tool"
    )]
    DesktopMicrophone,
    #[cfg(feature = "speak")]
    #[value(
        name = "desktop-speaker",
        alias = "desktop_speaker",
        alias = "speaker",
        alias = "speaker-tool"
    )]
    DesktopSpeaker,
    #[cfg(feature = "share-screen")]
    #[value(
        name = "desktop-screen-share",
        alias = "desktop_screen_share",
        alias = "screen-share-tool",
        alias = "screen-tool"
    )]
    DesktopScreenShare,
}

impl From<CliSlashCommand> for SlashCommand {
    fn from(value: CliSlashCommand) -> Self {
        match value.command {
            #[cfg(feature = "mic")]
            CliCommand::Mic => Self::ToggleMic,
            #[cfg(feature = "speak")]
            CliCommand::Speak => Self::ToggleSpeaker,
            #[cfg(feature = "share-screen")]
            CliCommand::ShareScreen {
                target,
                interval_secs,
            } => {
                let args = match (target, interval_secs) {
                    (None, None) => String::new(),
                    (Some(target), None) => target,
                    (Some(target), Some(interval_secs)) => format!("{target} {interval_secs}"),
                    (None, Some(interval_secs)) => interval_secs.to_string(),
                };
                Self::ShareScreen(args)
            }
            CliCommand::System { action } => Self::System(match action {
                None | Some(CliSystemCommand::Show) => SystemCommand::Show,
                Some(CliSystemCommand::Set { text }) => SystemCommand::Set(text.join(" ")),
                Some(CliSystemCommand::Clear) => SystemCommand::Clear,
                Some(CliSystemCommand::Apply) => SystemCommand::Apply,
            }),
            CliCommand::Tools { action } => Self::Tools(match action {
                None | Some(CliToolsCommand::Status) => ToolsCommand::Status,
                Some(CliToolsCommand::List) => ToolsCommand::List,
                Some(CliToolsCommand::Enable { tool }) => ToolsCommand::Enable(tool.into()),
                Some(CliToolsCommand::Disable { tool }) => ToolsCommand::Disable(tool.into()),
                Some(CliToolsCommand::Toggle { tool }) => ToolsCommand::Toggle(tool.into()),
                Some(CliToolsCommand::Apply) => ToolsCommand::Apply,
            }),
        }
    }
}

impl From<CliToolArg> for ToolId {
    fn from(value: CliToolArg) -> Self {
        match value {
            CliToolArg::GoogleSearch => Self::GoogleSearch,
            CliToolArg::Timer => Self::Timer,
            CliToolArg::ListFiles => Self::ListFiles,
            CliToolArg::ReadFile => Self::ReadFile,
            CliToolArg::RunCommand => Self::RunCommand,
            #[cfg(feature = "mic")]
            CliToolArg::DesktopMicrophone => Self::DesktopMicrophone,
            #[cfg(feature = "speak")]
            CliToolArg::DesktopSpeaker => Self::DesktopSpeaker,
            #[cfg(feature = "share-screen")]
            CliToolArg::DesktopScreenShare => Self::DesktopScreenShare,
        }
    }
}

struct CommandSpec {
    name: &'static str,
    detail: &'static str,
}

struct SubcommandSpec {
    name: &'static str,
    detail: &'static str,
    expects_more: bool,
}

fn command_specs() -> Vec<CommandSpec> {
    vec![
        #[cfg(feature = "mic")]
        CommandSpec {
            name: "/mic",
            detail: "toggle microphone streaming",
        },
        #[cfg(feature = "speak")]
        CommandSpec {
            name: "/speak",
            detail: "toggle speaker playback",
        },
        #[cfg(feature = "share-screen")]
        CommandSpec {
            name: "/share-screen",
            detail: "list, start, or stop screen sharing",
        },
        CommandSpec {
            name: "/tools",
            detail: "inspect and stage the Live tool profile",
        },
        CommandSpec {
            name: "/system",
            detail: "inspect and stage the system instruction",
        },
    ]
}

fn tool_subcommand_specs() -> [SubcommandSpec; 6] {
    [
        SubcommandSpec {
            name: "status",
            detail: "show active and staged tool profiles",
            expects_more: false,
        },
        SubcommandSpec {
            name: "list",
            detail: "list known tools and their current state",
            expects_more: false,
        },
        SubcommandSpec {
            name: "enable",
            detail: "stage a tool for the next applied session",
            expects_more: true,
        },
        SubcommandSpec {
            name: "disable",
            detail: "stage a tool removal",
            expects_more: true,
        },
        SubcommandSpec {
            name: "toggle",
            detail: "flip a tool in the staged profile",
            expects_more: true,
        },
        SubcommandSpec {
            name: "apply",
            detail: "reconnect with the staged tool profile",
            expects_more: false,
        },
    ]
}

fn system_subcommand_specs() -> [SubcommandSpec; 4] {
    [
        SubcommandSpec {
            name: "show",
            detail: "show active and staged system instruction",
            expects_more: false,
        },
        SubcommandSpec {
            name: "set",
            detail: "stage a new system instruction",
            expects_more: true,
        },
        SubcommandSpec {
            name: "clear",
            detail: "stage removal of the system instruction",
            expects_more: false,
        },
        SubcommandSpec {
            name: "apply",
            detail: "reconnect with the staged system instruction",
            expects_more: false,
        },
    ]
}

fn command_completions(
    prefix: &str,
    replace_range: Range<usize>,
    append_space: bool,
) -> Vec<CompletionItem> {
    command_specs()
        .into_iter()
        .filter(|spec| spec.name.starts_with(prefix))
        .map(|spec| CompletionItem {
            label: spec.name.to_string(),
            replacement: if append_space {
                format!("{} ", spec.name)
            } else {
                spec.name.to_string()
            },
            replace_range: replace_range.clone(),
            detail: spec.detail.to_string(),
        })
        .collect()
}

#[cfg(feature = "share-screen")]
fn share_screen_completions(
    parts: &[&str],
    trailing_space: bool,
    current_fragment: &str,
    current_range: Range<usize>,
    input_len: usize,
) -> Vec<CompletionItem> {
    if parts.len() == 1 {
        let replace_range = input_len..input_len;
        return vec![CompletionItem {
            label: "list".into(),
            replacement: " list".into(),
            replace_range,
            detail: "list available capture targets".into(),
        }];
    }

    if parts.len() == 2 && !trailing_space && "list".starts_with(current_fragment) {
        return vec![CompletionItem {
            label: "list".into(),
            replacement: "list".into(),
            replace_range: current_range,
            detail: "list available capture targets".into(),
        }];
    }

    Vec::new()
}

fn tools_completions(
    parts: &[&str],
    trailing_space: bool,
    current_fragment: &str,
    current_range: Range<usize>,
    input_len: usize,
) -> Vec<CompletionItem> {
    if parts.len() == 1 {
        let replace_range = input_len..input_len;
        return tool_subcommand_specs()
            .into_iter()
            .map(|spec| CompletionItem {
                label: spec.name.into(),
                replacement: format!(" {}", spec.name_with_suffix()),
                replace_range: replace_range.clone(),
                detail: spec.detail.into(),
            })
            .collect();
    }

    if parts.len() == 2 && !trailing_space {
        return tool_subcommand_specs()
            .into_iter()
            .filter(|spec| spec.name.starts_with(current_fragment))
            .map(|spec| CompletionItem {
                label: spec.name.into(),
                replacement: spec.name_with_suffix().into(),
                replace_range: current_range.clone(),
                detail: spec.detail.into(),
            })
            .collect();
    }

    let subcommand = parts[1];
    if matches!(subcommand, "enable" | "disable" | "toggle") {
        if parts.len() == 2 && trailing_space {
            return tool_name_completions("", input_len..input_len);
        }
        if parts.len() == 3 && !trailing_space {
            return tool_name_completions(current_fragment, current_range);
        }
    }

    Vec::new()
}

fn system_completions(
    parts: &[&str],
    trailing_space: bool,
    current_fragment: &str,
    current_range: Range<usize>,
    input_len: usize,
) -> Vec<CompletionItem> {
    if parts.len() == 1 {
        let replace_range = input_len..input_len;
        return system_subcommand_specs()
            .into_iter()
            .map(|spec| CompletionItem {
                label: spec.name.into(),
                replacement: format!(" {}", spec.name_with_suffix()),
                replace_range: replace_range.clone(),
                detail: spec.detail.into(),
            })
            .collect();
    }

    if parts.len() == 2 && !trailing_space {
        return system_subcommand_specs()
            .into_iter()
            .filter(|spec| spec.name.starts_with(current_fragment))
            .map(|spec| CompletionItem {
                label: spec.name.into(),
                replacement: spec.name_with_suffix().into(),
                replace_range: current_range.clone(),
                detail: spec.detail.into(),
            })
            .collect();
    }

    Vec::new()
}

fn tool_name_completions(prefix: &str, replace_range: Range<usize>) -> Vec<CompletionItem> {
    ToolId::ALL
        .iter()
        .copied()
        .filter(|tool| tool.key().starts_with(prefix))
        .map(|tool| CompletionItem {
            label: tool.key().into(),
            replacement: format!("{} ", tool.key()),
            replace_range: replace_range.clone(),
            detail: tool.summary().into(),
        })
        .collect()
}

impl SubcommandSpec {
    fn name_with_suffix(&self) -> &'static str {
        if self.expects_more {
            match self.name {
                "enable" => "enable ",
                "disable" => "disable ",
                "toggle" => "toggle ",
                _ => self.name,
            }
        } else {
            self.name
        }
    }
}

fn current_token_range(input: &str) -> Range<usize> {
    let start = input
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map(|(idx, ch)| idx + ch.len_utf8())
        .unwrap_or(0);
    start..input.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tools_defaults_to_status() {
        let command = parse("/tools")
            .expect("slash command")
            .expect("valid command");
        assert_eq!(command, SlashCommand::Tools(ToolsCommand::Status));
    }

    #[test]
    fn parse_tools_enable_alias() {
        let command = parse("/tools enable search")
            .expect("slash command")
            .expect("valid command");
        assert_eq!(
            command,
            SlashCommand::Tools(ToolsCommand::Enable(ToolId::GoogleSearch))
        );
    }

    #[test]
    fn parse_tools_enable_timer_alias() {
        let command = parse("/tools enable reminder")
            .expect("slash command")
            .expect("valid command");
        assert_eq!(
            command,
            SlashCommand::Tools(ToolsCommand::Enable(ToolId::Timer))
        );
    }

    #[test]
    fn parse_system_set() {
        let command = parse("/system set You are concise")
            .expect("slash command")
            .expect("valid command");
        assert_eq!(
            command,
            SlashCommand::System(SystemCommand::Set("You are concise".into()))
        );
    }

    #[test]
    fn complete_partial_command_name() {
        let items = completions("/to");
        assert!(items.iter().any(|item| item.label == "/tools"));
    }

    #[test]
    fn complete_tool_name_after_enable() {
        let items = completions("/tools enable rea");
        assert!(items.iter().any(|item| item.label == "read-file"));
    }

    #[test]
    fn complete_tool_name_after_enable_for_timer() {
        let items = completions("/tools enable ti");
        assert!(items.iter().any(|item| item.label == "timer"));
    }

    #[cfg(feature = "mic")]
    #[test]
    fn parse_tools_enable_desktop_microphone() {
        let command = parse("/tools enable desktop-microphone")
            .expect("slash command")
            .expect("valid command");
        assert_eq!(
            command,
            SlashCommand::Tools(ToolsCommand::Enable(ToolId::DesktopMicrophone))
        );
    }

    #[cfg(feature = "share-screen")]
    #[test]
    fn complete_tool_name_after_enable_for_desktop_tool() {
        let items = completions("/tools enable desktop-sc");
        assert!(
            items
                .iter()
                .any(|item| item.label == "desktop-screen-share")
        );
    }

    #[test]
    fn complete_system_subcommand() {
        let items = completions("/system s");
        assert!(items.iter().any(|item| item.label == "set"));
        assert!(items.iter().any(|item| item.label == "show"));
    }
}
