//! Reusable timer tool family for Gemini Live hosts.
//!
//! The timer tool is intentionally simple: it waits for a caller-specified
//! duration and then returns a completion payload. When wrapped by the harness
//! with a short inline budget, longer waits naturally spill into a durable
//! background task and later passive notification, which makes this tool a good
//! end-to-end probe for the harness lifecycle.

use std::time::Duration;

use futures_util::future::BoxFuture;
use gemini_live::types::{FunctionCallRequest, FunctionDeclaration, FunctionResponse, Tool};
use gemini_live_harness::{
    ToolCapability, ToolDescriptor, ToolExecutionError, ToolExecutor, ToolKind, ToolProvider,
    ToolSpecification,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

const MAX_TIMER_DURATION_SECS: u64 = 366 * 24 * 60 * 60;
const MAX_LABEL_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerToolId {
    SetTimer,
}

impl TimerToolId {
    pub const ALL: [Self; 1] = [Self::SetTimer];

    pub fn key(self) -> &'static str {
        match self {
            Self::SetTimer => "timer",
        }
    }

    pub fn summary(self) -> &'static str {
        match self {
            Self::SetTimer => {
                "wait for a duration and notify later when it exceeds the inline budget"
            }
        }
    }

    pub fn function_name(self) -> &'static str {
        match self {
            Self::SetTimer => "set_timer",
        }
    }

    pub fn from_function_name(name: &str) -> Option<Self> {
        match name {
            "set_timer" => Some(Self::SetTimer),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct TimerToolSelection {
    pub timer: bool,
}

impl Default for TimerToolSelection {
    fn default() -> Self {
        Self { timer: true }
    }
}

impl TimerToolSelection {
    pub fn is_enabled(self, tool: TimerToolId) -> bool {
        match tool {
            TimerToolId::SetTimer => self.timer,
        }
    }

    pub fn set(&mut self, tool: TimerToolId, enabled: bool) -> bool {
        let slot = match tool {
            TimerToolId::SetTimer => &mut self.timer,
        };
        let changed = *slot != enabled;
        *slot = enabled;
        changed
    }

    pub fn function_declarations(self) -> Vec<FunctionDeclaration> {
        let mut functions = Vec::new();
        if self.timer {
            functions.push(set_timer_declaration());
        }
        functions
    }

    pub fn build_live_tool(self) -> Option<Tool> {
        let functions = self.function_declarations();
        (!functions.is_empty()).then_some(Tool::FunctionDeclarations(functions))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerToolAdapter {
    selection: TimerToolSelection,
}

impl TimerToolAdapter {
    pub fn new(selection: TimerToolSelection) -> Self {
        Self { selection }
    }

    pub fn selection(&self) -> TimerToolSelection {
        self.selection
    }

    pub async fn execute_call(&self, call: FunctionCallRequest) -> FunctionResponse {
        let result = match TimerToolId::from_function_name(call.name.as_str()) {
            Some(tool) if self.selection.is_enabled(tool) => {
                self.execute_enabled_call(call.args.as_object()).await
            }
            Some(_) => Err(format!(
                "tool `{}` is not enabled in the active profile",
                call.name
            )),
            None => Err(format!("unknown timer tool `{}`", call.name)),
        };

        function_response(call, result)
    }

    async fn execute_enabled_call(
        &self,
        args: Option<&Map<String, Value>>,
    ) -> Result<Value, String> {
        let request = TimerRequest::from_args(args)?;
        tokio::time::sleep(request.duration()).await;

        let duration_text = format_duration(request.total_secs);
        let message = match request.label.as_deref() {
            Some(label) => format!("Timer finished after {duration_text}: {label}"),
            None => format!("Timer finished after {duration_text}."),
        };

        Ok(json!({
            "finished": true,
            "durationSecs": request.total_secs,
            "durationText": duration_text,
            "label": request.label,
            "message": message,
        }))
    }
}

impl ToolProvider for TimerToolAdapter {
    fn advertised_tools(&self) -> Option<Vec<Tool>> {
        self.selection.build_live_tool().map(|tool| vec![tool])
    }

    fn descriptors(&self) -> Vec<ToolDescriptor> {
        TimerToolId::ALL
            .into_iter()
            .map(|tool| ToolDescriptor {
                key: tool.key().to_string(),
                summary: tool.summary().to_string(),
                kind: ToolKind::Local,
            })
            .collect()
    }

    fn specifications(&self) -> Vec<ToolSpecification> {
        TimerToolId::ALL
            .into_iter()
            .filter(|tool| self.selection.is_enabled(*tool))
            .map(|tool| {
                ToolSpecification::new(tool.function_name(), ToolCapability::BACKGROUND_CONTINUABLE)
            })
            .collect()
    }
}

impl ToolExecutor for TimerToolAdapter {
    fn execute<'a>(
        &'a self,
        call: FunctionCallRequest,
    ) -> BoxFuture<'a, Result<FunctionResponse, ToolExecutionError>> {
        Box::pin(async move { Ok(self.execute_call(call).await) })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TimerRequest {
    total_secs: u64,
    label: Option<String>,
}

impl TimerRequest {
    fn from_args(args: Option<&Map<String, Value>>) -> Result<Self, String> {
        let args = args.ok_or("tool arguments must be a JSON object")?;
        let days = resolve_optional_u64(args, "days")?.unwrap_or(0);
        let hours = resolve_optional_u64(args, "hours")?.unwrap_or(0);
        let minutes = resolve_optional_u64(args, "minutes")?.unwrap_or(0);
        let seconds = resolve_optional_u64(args, "seconds")?.unwrap_or(0);
        let label = resolve_optional_label(args, "label")?;

        let total_secs = checked_component_to_secs(days, 24 * 60 * 60, "days")?
            .checked_add(checked_component_to_secs(hours, 60 * 60, "hours")?)
            .and_then(|value| {
                value.checked_add(checked_component_to_secs(minutes, 60, "minutes").ok()?)
            })
            .and_then(|value| value.checked_add(seconds))
            .ok_or("timer duration is too large")?;

        if total_secs == 0 {
            return Err(
                "set_timer requires at least one positive duration field (`days`, `hours`, `minutes`, or `seconds`)"
                    .into(),
            );
        }
        if total_secs > MAX_TIMER_DURATION_SECS {
            return Err(format!(
                "timer duration may not exceed {MAX_TIMER_DURATION_SECS} seconds"
            ));
        }

        Ok(Self { total_secs, label })
    }

    fn duration(&self) -> Duration {
        Duration::from_secs(self.total_secs)
    }
}

fn checked_component_to_secs(value: u64, multiplier: u64, name: &str) -> Result<u64, String> {
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("`{name}` is too large"))
}

fn resolve_optional_u64(args: &Map<String, Value>, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None => Ok(None),
        Some(Value::Number(value)) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a non-negative integer")),
        Some(_) => Err(format!("`{key}` must be an integer")),
    }
}

fn resolve_optional_label(args: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            if trimmed.chars().count() > MAX_LABEL_CHARS {
                return Err(format!(
                    "`{key}` may contain at most {MAX_LABEL_CHARS} characters"
                ));
            }
            Ok(Some(trimmed.to_string()))
        }
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

fn function_response(call: FunctionCallRequest, result: Result<Value, String>) -> FunctionResponse {
    match result {
        Ok(response) => FunctionResponse {
            id: call.id,
            name: call.name,
            response: json!({
                "ok": true,
                "result": response,
            }),
        },
        Err(message) => FunctionResponse {
            id: call.id,
            name: call.name,
            response: json!({
                "ok": false,
                "error": {
                    "message": message,
                },
            }),
        },
    }
}

fn set_timer_declaration() -> FunctionDeclaration {
    FunctionDeclaration {
        name: TimerToolId::SetTimer.function_name().into(),
        description: "Wait for a fixed duration and then report completion. Use this for reminders, alerts, or to deliberately exercise the harness background-task and passive-notification path when the requested wait exceeds the inline budget.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "days": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Whole days to wait."
                },
                "hours": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Whole hours to wait."
                },
                "minutes": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Whole minutes to wait."
                },
                "seconds": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Whole seconds to wait."
                },
                "label": {
                    "type": "string",
                    "description": "Optional short reminder text to include when the timer finishes."
                }
            }
        }),
        scheduling: None,
        behavior: None,
    }
}

fn format_duration(total_secs: u64) -> String {
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3_600;
    let minutes = (total_secs % 3_600) / 60;
    let seconds = total_secs % 60;

    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format_unit(days, "day"));
    }
    if hours > 0 {
        parts.push(format_unit(hours, "hour"));
    }
    if minutes > 0 {
        parts.push(format_unit(minutes, "minute"));
    }
    if seconds > 0 {
        parts.push(format_unit(seconds, "second"));
    }
    if parts.is_empty() {
        "0 seconds".into()
    } else {
        parts.join(" ")
    }
}

fn format_unit(value: u64, singular: &str) -> String {
    if value == 1 {
        format!("1 {singular}")
    } else {
        format!("{value} {singular}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_selection_builds_declared_functions() {
        let tool = TimerToolSelection { timer: true }
            .build_live_tool()
            .expect("timer tool");
        let Tool::FunctionDeclarations(functions) = tool else {
            panic!("expected function declarations");
        };
        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].name, "set_timer");
    }

    #[test]
    fn timer_request_requires_positive_duration() {
        let error = TimerRequest::from_args(Some(
            &serde_json::from_value::<Map<String, Value>>(json!({})).expect("args"),
        ))
        .expect_err("missing duration should fail");
        assert!(error.contains("requires at least one positive duration field"));
    }

    #[tokio::test]
    async fn timer_tool_returns_completion_message_after_wait() {
        let adapter = TimerToolAdapter::new(TimerToolSelection { timer: true });
        let response = adapter
            .execute_call(FunctionCallRequest {
                id: "call_1".into(),
                name: "set_timer".into(),
                args: json!({
                    "seconds": 1,
                    "label": "stretch"
                }),
            })
            .await;
        assert_eq!(response.response["ok"], true);
        assert_eq!(response.response["result"]["durationSecs"], 1);
        assert_eq!(
            response.response["result"]["message"],
            "Timer finished after 1 second: stretch"
        );
    }

    #[test]
    fn timer_specification_is_background_continuable() {
        let adapter = TimerToolAdapter::new(TimerToolSelection { timer: true });
        let specs = adapter.specifications();
        assert_eq!(specs.len(), 1);
        assert!(specs[0].capability.can_continue_async_after_timeout);
    }

    #[test]
    fn format_duration_normalizes_units() {
        assert_eq!(format_duration(90), "1 minute 30 seconds");
        assert_eq!(format_duration(3_661), "1 hour 1 minute 1 second");
    }
}
