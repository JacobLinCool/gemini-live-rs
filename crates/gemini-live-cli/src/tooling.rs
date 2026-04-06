//! CLI-local tool profile and adapter composition.
//!
//! The CLI owns which tool families it exposes and how they are presented in
//! the TUI. Reusable workspace-local tool definitions and execution now live in
//! `gemini-live-tools`.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use gemini_live::types::{
    FunctionCallRequest, FunctionDeclaration, FunctionResponse, GoogleSearchTool, Tool,
};
use gemini_live_runtime::{ToolAdapter, ToolDescriptor, ToolKind};
use gemini_live_tools::workspace::{WorkspaceToolAdapter, WorkspaceToolId, WorkspaceToolSelection};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[cfg(feature = "share-screen")]
use crate::desktop_control::ScreenShareRequest;
use crate::desktop_control::{DesktopControlAction, DesktopControlPort};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolId {
    GoogleSearch,
    ListFiles,
    ReadFile,
    RunCommand,
    #[cfg(feature = "mic")]
    DesktopMicrophone,
    #[cfg(feature = "speak")]
    DesktopSpeaker,
    #[cfg(feature = "share-screen")]
    DesktopScreenShare,
}

impl ToolId {
    pub const ALL: &'static [Self] = &[
        Self::GoogleSearch,
        Self::ListFiles,
        Self::ReadFile,
        Self::RunCommand,
        #[cfg(feature = "mic")]
        Self::DesktopMicrophone,
        #[cfg(feature = "speak")]
        Self::DesktopSpeaker,
        #[cfg(feature = "share-screen")]
        Self::DesktopScreenShare,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::GoogleSearch => "google-search",
            Self::ListFiles => WorkspaceToolId::ListFiles.key(),
            Self::ReadFile => WorkspaceToolId::ReadFile.key(),
            Self::RunCommand => WorkspaceToolId::RunCommand.key(),
            #[cfg(feature = "mic")]
            Self::DesktopMicrophone => "desktop-microphone",
            #[cfg(feature = "speak")]
            Self::DesktopSpeaker => "desktop-speaker",
            #[cfg(feature = "share-screen")]
            Self::DesktopScreenShare => "desktop-screen-share",
        }
    }

    pub fn kind(self) -> &'static str {
        if matches!(self, Self::GoogleSearch) {
            "built-in"
        } else {
            "local"
        }
    }

    pub fn summary(self) -> &'static str {
        match self {
            Self::GoogleSearch => "Google-managed web search",
            Self::ListFiles => WorkspaceToolId::ListFiles.summary(),
            Self::ReadFile => WorkspaceToolId::ReadFile.summary(),
            Self::RunCommand => WorkspaceToolId::RunCommand.summary(),
            #[cfg(feature = "mic")]
            Self::DesktopMicrophone => "inspect and set microphone capture state",
            #[cfg(feature = "speak")]
            Self::DesktopSpeaker => "inspect and set speaker playback state",
            #[cfg(feature = "share-screen")]
            Self::DesktopScreenShare => "inspect screen targets and set screen-sharing state",
        }
    }

    fn as_workspace_tool(self) -> Option<WorkspaceToolId> {
        match self {
            Self::GoogleSearch => None,
            Self::ListFiles => Some(WorkspaceToolId::ListFiles),
            Self::ReadFile => Some(WorkspaceToolId::ReadFile),
            Self::RunCommand => Some(WorkspaceToolId::RunCommand),
            #[cfg(feature = "mic")]
            Self::DesktopMicrophone => None,
            #[cfg(feature = "speak")]
            Self::DesktopSpeaker => None,
            #[cfg(feature = "share-screen")]
            Self::DesktopScreenShare => None,
        }
    }

    fn as_desktop_tool(self) -> Option<DesktopToolId> {
        match self {
            #[cfg(feature = "mic")]
            Self::DesktopMicrophone => Some(DesktopToolId::Microphone),
            #[cfg(feature = "speak")]
            Self::DesktopSpeaker => Some(DesktopToolId::Speaker),
            #[cfg(feature = "share-screen")]
            Self::DesktopScreenShare => Some(DesktopToolId::ScreenShare),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesktopToolId {
    #[cfg(feature = "mic")]
    Microphone,
    #[cfg(feature = "speak")]
    Speaker,
    #[cfg(feature = "share-screen")]
    ScreenShare,
}

impl DesktopToolId {
    const ALL: &'static [Self] = &[
        #[cfg(feature = "mic")]
        Self::Microphone,
        #[cfg(feature = "speak")]
        Self::Speaker,
        #[cfg(feature = "share-screen")]
        Self::ScreenShare,
    ];

    fn function_names(self) -> &'static [&'static str] {
        match self {
            #[cfg(feature = "mic")]
            Self::Microphone => &["desktop_get_state", "desktop_set_microphone"],
            #[cfg(feature = "speak")]
            Self::Speaker => &["desktop_get_state", "desktop_set_speaker"],
            #[cfg(feature = "share-screen")]
            Self::ScreenShare => &[
                "desktop_get_state",
                "desktop_list_screen_targets",
                "desktop_set_screen_share",
            ],
        }
    }

    fn matches_function_name(self, name: &str) -> bool {
        self.function_names().contains(&name)
    }

    fn from_function_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|tool| tool.matches_function_name(name))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopToolSelection {
    #[cfg(feature = "mic")]
    pub desktop_microphone: bool,
    #[cfg(feature = "speak")]
    pub desktop_speaker: bool,
    #[cfg(feature = "share-screen")]
    pub desktop_screen_share: bool,
}

impl Default for DesktopToolSelection {
    fn default() -> Self {
        Self {
            #[cfg(feature = "mic")]
            desktop_microphone: true,
            #[cfg(feature = "speak")]
            desktop_speaker: true,
            #[cfg(feature = "share-screen")]
            desktop_screen_share: true,
        }
    }
}

impl DesktopToolSelection {
    fn is_enabled(self, tool: DesktopToolId) -> bool {
        match tool {
            #[cfg(feature = "mic")]
            DesktopToolId::Microphone => self.desktop_microphone,
            #[cfg(feature = "speak")]
            DesktopToolId::Speaker => self.desktop_speaker,
            #[cfg(feature = "share-screen")]
            DesktopToolId::ScreenShare => self.desktop_screen_share,
        }
    }

    fn set(&mut self, tool: DesktopToolId, enabled: bool) -> bool {
        match tool {
            #[cfg(feature = "mic")]
            DesktopToolId::Microphone => {
                let changed = self.desktop_microphone != enabled;
                self.desktop_microphone = enabled;
                changed
            }
            #[cfg(feature = "speak")]
            DesktopToolId::Speaker => {
                let changed = self.desktop_speaker != enabled;
                self.desktop_speaker = enabled;
                changed
            }
            #[cfg(feature = "share-screen")]
            DesktopToolId::ScreenShare => {
                let changed = self.desktop_screen_share != enabled;
                self.desktop_screen_share = enabled;
                changed
            }
        }
    }

    fn function_declarations(self) -> Vec<FunctionDeclaration> {
        let mut declarations = Vec::new();
        if Self::any_enabled(self) {
            declarations.push(desktop_get_state_declaration());
        }
        #[cfg(feature = "mic")]
        if self.desktop_microphone {
            declarations.push(desktop_set_microphone_declaration());
        }
        #[cfg(feature = "speak")]
        if self.desktop_speaker {
            declarations.push(desktop_set_speaker_declaration());
        }
        #[cfg(feature = "share-screen")]
        if self.desktop_screen_share {
            declarations.push(desktop_list_screen_targets_declaration());
            declarations.push(desktop_set_screen_share_declaration());
        }

        declarations
    }

    fn any_enabled(self) -> bool {
        DesktopToolId::ALL
            .iter()
            .copied()
            .any(|tool| self.is_enabled(tool))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub struct ToolProfile {
    pub google_search: bool,
    #[serde(flatten)]
    pub workspace: WorkspaceToolSelection,
    #[serde(flatten)]
    pub desktop: DesktopToolSelection,
}

impl ToolProfile {
    pub fn is_enabled(self, tool: ToolId) -> bool {
        match tool {
            ToolId::GoogleSearch => self.google_search,
            _ if tool.as_workspace_tool().is_some() => self
                .workspace
                .is_enabled(tool.as_workspace_tool().expect("workspace tool id")),
            _ => self
                .desktop
                .is_enabled(tool.as_desktop_tool().expect("desktop tool id")),
        }
    }

    pub fn set(&mut self, tool: ToolId, enabled: bool) -> bool {
        match tool {
            ToolId::GoogleSearch => {
                let changed = self.google_search != enabled;
                self.google_search = enabled;
                changed
            }
            _ if tool.as_workspace_tool().is_some() => self.workspace.set(
                tool.as_workspace_tool().expect("workspace tool id"),
                enabled,
            ),
            _ => self
                .desktop
                .set(tool.as_desktop_tool().expect("desktop tool id"), enabled),
        }
    }

    pub fn toggle(&mut self, tool: ToolId) -> bool {
        let next = !self.is_enabled(tool);
        self.set(tool, next);
        next
    }

    pub fn summary(self) -> String {
        let enabled = ToolId::ALL
            .iter()
            .copied()
            .filter(|tool| self.is_enabled(*tool))
            .map(ToolId::key)
            .collect::<Vec<_>>();
        if enabled.is_empty() {
            "none".to_string()
        } else {
            enabled.join(", ")
        }
    }

    pub fn build_live_tools(self) -> Option<Vec<Tool>> {
        let mut tools = Vec::new();
        if self.google_search {
            tools.push(Tool::GoogleSearch(GoogleSearchTool {}));
        }
        let mut functions = self.workspace.function_declarations();
        functions.extend(self.desktop.function_declarations());
        if !functions.is_empty() {
            tools.push(Tool::FunctionDeclarations(functions));
        }
        (!tools.is_empty()).then_some(tools)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolsCommand {
    Status,
    List,
    Enable(ToolId),
    Disable(ToolId),
    Toggle(ToolId),
    Apply,
}

pub fn status_lines(active: ToolProfile, desired: ToolProfile) -> Vec<String> {
    let mut lines = vec![format!("tools active: {}", active.summary())];
    if active == desired {
        lines.push("tools staged: none".to_string());
    } else {
        lines.push(format!("tools staged: {}", desired.summary()));
        lines.push("run `/tools apply` to reconnect with the staged tool profile".to_string());
    }
    lines
}

pub fn catalog_lines(active: ToolProfile, desired: ToolProfile) -> Vec<String> {
    ToolId::ALL
        .iter()
        .copied()
        .map(|tool| {
            let state = match (active.is_enabled(tool), desired.is_enabled(tool)) {
                (true, true) => "active",
                (false, true) => "staged",
                (true, false) => "active; staged off",
                (false, false) => "off",
            };
            format!(
                "{} [{}, {}] — {}",
                tool.key(),
                state,
                tool.kind(),
                tool.summary()
            )
        })
        .collect()
}

#[derive(Clone)]
pub struct ToolRuntime {
    profile: ToolProfile,
    workspace: WorkspaceToolAdapter,
    desktop_control: Option<Arc<dyn DesktopControlPort>>,
}

impl ToolRuntime {
    pub fn new(
        workspace_root: PathBuf,
        profile: ToolProfile,
        desktop_control: Option<Arc<dyn DesktopControlPort>>,
    ) -> io::Result<Self> {
        Ok(Self {
            profile,
            workspace: WorkspaceToolAdapter::new(workspace_root, profile.workspace)?,
            desktop_control,
        })
    }

    pub async fn execute_call(&self, call: FunctionCallRequest) -> FunctionResponse {
        if WorkspaceToolId::from_function_name(call.name.as_str()).is_some() {
            return self.workspace.execute_call(call).await;
        }
        if let Some(tool) = DesktopToolId::from_function_name(call.name.as_str()) {
            return self.execute_desktop_call(tool, call).await;
        }

        let name = call.name.clone();
        FunctionResponse {
            id: call.id,
            name,
            response: json!({
                "ok": false,
                "error": {
                    "message": format!("unknown local tool `{}`", call.name),
                },
            }),
        }
    }

    async fn execute_desktop_call(
        &self,
        tool: DesktopToolId,
        call: FunctionCallRequest,
    ) -> FunctionResponse {
        if !self.profile.desktop.is_enabled(tool) {
            return error_response(
                call,
                format!(
                    "tool `{}` is not enabled in the active profile",
                    tool_name(tool)
                ),
            );
        }
        let Some(port) = &self.desktop_control else {
            return error_response(call, "desktop control host is unavailable".into());
        };

        let result = match desktop_action_from_call(&call) {
            Ok(action) => port
                .execute(action)
                .await
                .map(|response| response.into_json())
                .map_err(|error| error.to_string()),
            Err(error) => Err(error),
        };

        match result {
            Ok(response) => success_response(call, response),
            Err(error) => error_response(call, error),
        }
    }
}

impl ToolAdapter for ToolRuntime {
    fn advertised_tools(&self) -> Option<Vec<Tool>> {
        self.profile.build_live_tools()
    }

    fn descriptors(&self) -> Vec<ToolDescriptor> {
        ToolId::ALL
            .iter()
            .copied()
            .map(|tool| ToolDescriptor {
                key: tool.key().to_string(),
                summary: tool.summary().to_string(),
                kind: if matches!(tool, ToolId::GoogleSearch) {
                    ToolKind::BuiltIn
                } else {
                    ToolKind::Local
                },
            })
            .collect()
    }

    fn execute<'a>(
        &'a self,
        call: FunctionCallRequest,
    ) -> BoxFuture<'a, Result<FunctionResponse, gemini_live_runtime::ToolExecutionError>> {
        Box::pin(async move { Ok(self.execute_call(call).await) })
    }
}

fn success_response(call: FunctionCallRequest, response: serde_json::Value) -> FunctionResponse {
    FunctionResponse {
        id: call.id,
        name: call.name,
        response: json!({
            "ok": true,
            "result": response,
        }),
    }
}

fn error_response(call: FunctionCallRequest, message: String) -> FunctionResponse {
    FunctionResponse {
        id: call.id,
        name: call.name,
        response: json!({
            "ok": false,
            "error": {
                "message": message,
            },
        }),
    }
}

fn tool_name(tool: DesktopToolId) -> &'static str {
    match tool {
        #[cfg(feature = "mic")]
        DesktopToolId::Microphone => "desktop-microphone",
        #[cfg(feature = "speak")]
        DesktopToolId::Speaker => "desktop-speaker",
        #[cfg(feature = "share-screen")]
        DesktopToolId::ScreenShare => "desktop-screen-share",
    }
}

fn desktop_action_from_call(call: &FunctionCallRequest) -> Result<DesktopControlAction, String> {
    let args = call.args.as_object();
    match call.name.as_str() {
        "desktop_get_state" => Ok(DesktopControlAction::GetState),
        #[cfg(feature = "mic")]
        "desktop_set_microphone" => Ok(DesktopControlAction::SetMicrophone {
            enabled: resolve_required_bool(args, "enabled")?,
        }),
        #[cfg(feature = "speak")]
        "desktop_set_speaker" => Ok(DesktopControlAction::SetSpeaker {
            enabled: resolve_required_bool(args, "enabled")?,
        }),
        #[cfg(feature = "share-screen")]
        "desktop_list_screen_targets" => Ok(DesktopControlAction::ListScreenTargets),
        #[cfg(feature = "share-screen")]
        "desktop_set_screen_share" => {
            let enabled = resolve_required_bool(args, "enabled")?;
            let target_id = resolve_optional_usize(args, "targetId")?;
            let interval_secs = resolve_optional_f64(args, "intervalSecs")?;
            if enabled && target_id.is_none() {
                return Err("`targetId` is required when enabling screen share".into());
            }
            Ok(DesktopControlAction::SetScreenShare(ScreenShareRequest {
                enabled,
                target_id,
                interval_secs,
            }))
        }
        _ => Err(format!("unknown desktop tool `{}`", call.name)),
    }
}

fn desktop_get_state_declaration() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "desktop_get_state".into(),
        description: "Inspect the current desktop host state before changing microphone, speaker, or screen-sharing settings.".into(),
        parameters: json!({
            "type": "object",
            "properties": {}
        }),
        scheduling: None,
        behavior: None,
    }
}

#[cfg(feature = "mic")]
fn desktop_set_microphone_declaration() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "desktop_set_microphone".into(),
        description: "Set whether the desktop CLI should capture microphone audio and stream it to the current Gemini Live session.".into(),
        parameters: json!({
            "type": "object",
            "required": ["enabled"],
            "properties": {
                "enabled": {
                    "type": "boolean",
                    "description": "Whether microphone capture should be enabled."
                }
            }
        }),
        scheduling: None,
        behavior: None,
    }
}

#[cfg(feature = "speak")]
fn desktop_set_speaker_declaration() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "desktop_set_speaker".into(),
        description:
            "Set whether the desktop CLI should play model audio through the local speaker output."
                .into(),
        parameters: json!({
            "type": "object",
            "required": ["enabled"],
            "properties": {
                "enabled": {
                    "type": "boolean",
                    "description": "Whether speaker playback should be enabled."
                }
            }
        }),
        scheduling: None,
        behavior: None,
    }
}

#[cfg(feature = "share-screen")]
fn desktop_list_screen_targets_declaration() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "desktop_list_screen_targets".into(),
        description:
            "List available desktop capture targets before enabling or retargeting screen sharing."
                .into(),
        parameters: json!({
            "type": "object",
            "properties": {}
        }),
        scheduling: None,
        behavior: None,
    }
}

#[cfg(feature = "share-screen")]
fn desktop_set_screen_share_declaration() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "desktop_set_screen_share".into(),
        description: "Set whether the desktop CLI should share a screen target with the current Gemini Live session. Always list targets first before enabling.".into(),
        parameters: json!({
            "type": "object",
            "required": ["enabled"],
            "properties": {
                "enabled": {
                    "type": "boolean",
                    "description": "Whether screen sharing should be enabled."
                },
                "targetId": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Desktop capture target id returned by desktop_list_screen_targets. Required when enabling."
                },
                "intervalSecs": {
                    "type": "number",
                    "description": "Seconds between captured frames. Must be greater than 0. Defaults to 1.0."
                }
            }
        }),
        scheduling: None,
        behavior: None,
    }
}

fn resolve_required_bool(
    args: Option<&serde_json::Map<String, serde_json::Value>>,
    key: &str,
) -> Result<bool, String> {
    match args.and_then(|args| args.get(key)) {
        Some(serde_json::Value::Bool(value)) => Ok(*value),
        Some(_) => Err(format!("`{key}` must be a boolean")),
        None => Err(format!("missing required boolean `{key}`")),
    }
}

#[cfg(feature = "share-screen")]
fn resolve_optional_usize(
    args: Option<&serde_json::Map<String, serde_json::Value>>,
    key: &str,
) -> Result<Option<usize>, String> {
    match args.and_then(|args| args.get(key)) {
        None => Ok(None),
        Some(serde_json::Value::Number(value)) => {
            let Some(number) = value.as_u64() else {
                return Err(format!("`{key}` must be a non-negative integer"));
            };
            Ok(Some(number as usize))
        }
        Some(_) => Err(format!("`{key}` must be an integer")),
    }
}

#[cfg(feature = "share-screen")]
fn resolve_optional_f64(
    args: Option<&serde_json::Map<String, serde_json::Value>>,
    key: &str,
) -> Result<Option<f64>, String> {
    match args.and_then(|args| args.get(key)) {
        None => Ok(None),
        Some(serde_json::Value::Number(value)) => {
            let Some(number) = value.as_f64() else {
                return Err(format!("`{key}` must be a number"));
            };
            if number <= 0.0 {
                return Err(format!("`{key}` must be greater than 0"));
            }
            Ok(Some(number))
        }
        Some(_) => Err(format!("`{key}` must be a number")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desktop_tools_disabled() -> DesktopToolSelection {
        DesktopToolSelection {
            #[cfg(feature = "mic")]
            desktop_microphone: false,
            #[cfg(feature = "speak")]
            desktop_speaker: false,
            #[cfg(feature = "share-screen")]
            desktop_screen_share: false,
        }
    }

    #[test]
    fn tool_profile_builds_google_search_and_local_functions() {
        let profile = ToolProfile {
            google_search: true,
            workspace: WorkspaceToolSelection {
                list_files: true,
                read_file: true,
                run_command: false,
            },
            desktop: desktop_tools_disabled(),
        };

        let tools = profile.build_live_tools().expect("live tools");
        assert!(matches!(tools[0], Tool::GoogleSearch(_)));
        match &tools[1] {
            Tool::FunctionDeclarations(functions) => {
                let names = functions
                    .iter()
                    .map(|function| function.name.as_str())
                    .collect::<Vec<_>>();
                assert_eq!(names, vec!["list_files", "read_file"]);
            }
            other => panic!("unexpected tool variant: {other:?}"),
        }
    }

    #[test]
    fn catalog_lists_staged_state() {
        let active = ToolProfile::default();
        let desired = ToolProfile {
            workspace: WorkspaceToolSelection {
                run_command: true,
                ..WorkspaceToolSelection::default()
            },
            desktop: active.desktop,
            ..ToolProfile::default()
        };
        let lines = catalog_lines(active, desired);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("run-command [staged"))
        );
    }

    #[test]
    fn default_tool_profile_matches_cli_safety_defaults() {
        let profile = ToolProfile::default();

        assert!(!profile.google_search);
        assert!(profile.workspace.list_files);
        assert!(profile.workspace.read_file);
        assert!(!profile.workspace.run_command);
        #[cfg(feature = "mic")]
        assert!(profile.desktop.desktop_microphone);
        #[cfg(feature = "speak")]
        assert!(profile.desktop.desktop_speaker);
        #[cfg(feature = "share-screen")]
        assert!(profile.desktop.desktop_screen_share);
    }

    #[test]
    fn tool_schemas_avoid_unsupported_exclusive_minimum_keyword() {
        let tools = ToolProfile::default()
            .build_live_tools()
            .expect("live tools");
        for tool in tools {
            let Tool::FunctionDeclarations(functions) = tool else {
                continue;
            };
            for function in functions {
                assert!(
                    !contains_json_key(&function.parameters, "exclusiveMinimum"),
                    "function `{}` uses unsupported `exclusiveMinimum`",
                    function.name
                );
            }
        }
    }

    #[cfg(any(feature = "mic", feature = "speak", feature = "share-screen"))]
    #[test]
    fn desktop_tools_declare_shared_state_once() {
        let mut desktop = DesktopToolSelection::default();
        #[cfg(feature = "mic")]
        {
            desktop.desktop_microphone = true;
        }
        #[cfg(feature = "speak")]
        {
            desktop.desktop_speaker = true;
        }
        #[cfg(feature = "share-screen")]
        {
            desktop.desktop_screen_share = true;
        }

        let tools = ToolProfile {
            desktop,
            ..ToolProfile::default()
        }
        .build_live_tools()
        .expect("desktop live tools");

        let Tool::FunctionDeclarations(functions) = &tools[0] else {
            panic!("expected function declarations");
        };
        let names = functions
            .iter()
            .map(|function| function.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            names
                .iter()
                .filter(|name| **name == "desktop_get_state")
                .count(),
            1
        );
        #[cfg(feature = "mic")]
        assert!(names.contains(&"desktop_set_microphone"));
        #[cfg(feature = "speak")]
        assert!(names.contains(&"desktop_set_speaker"));
        #[cfg(feature = "share-screen")]
        {
            assert!(names.contains(&"desktop_list_screen_targets"));
            assert!(names.contains(&"desktop_set_screen_share"));
        }
    }

    fn contains_json_key(value: &serde_json::Value, key: &str) -> bool {
        match value {
            serde_json::Value::Object(map) => {
                map.contains_key(key) || map.values().any(|value| contains_json_key(value, key))
            }
            serde_json::Value::Array(items) => {
                items.iter().any(|value| contains_json_key(value, key))
            }
            _ => false,
        }
    }
}
