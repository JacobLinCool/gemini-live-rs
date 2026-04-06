//! CLI-local tool catalog, slash-command parsing, and tool execution helpers.
//!
//! This module is the canonical home for the CLI's tool surface:
//! - which Live API tools can be enabled
//! - which slash commands manage the staged vs active tool profile
//! - how local function calls are validated and executed
//!
//! Tool availability is part of the Live API session setup, so changes are
//! staged first and only take effect after `/tools apply` reconnects with the
//! new setup payload. When the server has already issued a resumption handle,
//! the reconnect carries conversation state across that apply.
//!
//! `ToolRuntime` also implements the shared `gemini_live_runtime::ToolAdapter`
//! contract so host-specific tool catalogs can be reused outside the desktop
//! CLI.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use futures_util::future::BoxFuture;
use gemini_live::types::{
    FunctionCallRequest, FunctionDeclaration, FunctionResponse, GoogleSearchTool, Tool,
};
use gemini_live_runtime::{ToolAdapter, ToolDescriptor, ToolKind};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

const DEFAULT_READ_LINE_COUNT: usize = 200;
const MAX_READ_LINE_COUNT: usize = 400;
const DEFAULT_LIST_MAX_ENTRIES: usize = 100;
const MAX_LIST_ENTRIES: usize = 200;
const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 10;
const MAX_COMMAND_TIMEOUT_SECS: u64 = 30;
const MAX_ARGV_LEN: usize = 32;
const MAX_OUTPUT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolId {
    GoogleSearch,
    ListFiles,
    ReadFile,
    RunCommand,
}

impl ToolId {
    pub const ALL: [Self; 4] = [
        Self::GoogleSearch,
        Self::ListFiles,
        Self::ReadFile,
        Self::RunCommand,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::GoogleSearch => "google-search",
            Self::ListFiles => "list-files",
            Self::ReadFile => "read-file",
            Self::RunCommand => "run-command",
        }
    }

    pub fn kind(self) -> &'static str {
        match self {
            Self::GoogleSearch => "built-in",
            Self::ListFiles | Self::ReadFile | Self::RunCommand => "local",
        }
    }

    pub fn summary(self) -> &'static str {
        match self {
            Self::GoogleSearch => "Google-managed web search",
            Self::ListFiles => "list workspace files and directories",
            Self::ReadFile => "read UTF-8 text files under the workspace root",
            Self::RunCommand => "run a non-interactive argv-only command under the workspace root",
        }
    }

    fn function_name(self) -> Option<&'static str> {
        match self {
            Self::GoogleSearch => None,
            Self::ListFiles => Some("list_files"),
            Self::ReadFile => Some("read_file"),
            Self::RunCommand => Some("run_command"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolProfile {
    pub google_search: bool,
    pub list_files: bool,
    pub read_file: bool,
    pub run_command: bool,
}

impl ToolProfile {
    pub fn is_enabled(self, tool: ToolId) -> bool {
        match tool {
            ToolId::GoogleSearch => self.google_search,
            ToolId::ListFiles => self.list_files,
            ToolId::ReadFile => self.read_file,
            ToolId::RunCommand => self.run_command,
        }
    }

    pub fn set(&mut self, tool: ToolId, enabled: bool) -> bool {
        let slot = match tool {
            ToolId::GoogleSearch => &mut self.google_search,
            ToolId::ListFiles => &mut self.list_files,
            ToolId::ReadFile => &mut self.read_file,
            ToolId::RunCommand => &mut self.run_command,
        };
        let changed = *slot != enabled;
        *slot = enabled;
        changed
    }

    pub fn toggle(&mut self, tool: ToolId) -> bool {
        let next = !self.is_enabled(tool);
        self.set(tool, next);
        next
    }

    pub fn summary(self) -> String {
        let enabled = ToolId::ALL
            .into_iter()
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

        let mut functions = Vec::new();
        if self.list_files {
            functions.push(list_files_declaration());
        }
        if self.read_file {
            functions.push(read_file_declaration());
        }
        if self.run_command {
            functions.push(run_command_declaration());
        }
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
        .into_iter()
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

#[derive(Debug, Clone)]
pub struct ToolRuntime {
    profile: ToolProfile,
    workspace_root: PathBuf,
    workspace_root_real: PathBuf,
}

impl ToolRuntime {
    pub fn new(workspace_root: PathBuf, profile: ToolProfile) -> io::Result<Self> {
        let workspace_root_real = workspace_root.canonicalize()?;
        Ok(Self {
            profile,
            workspace_root,
            workspace_root_real,
        })
    }

    pub async fn execute_call(&self, call: FunctionCallRequest) -> FunctionResponse {
        let result = match ToolId::ALL
            .into_iter()
            .find(|tool| tool.function_name() == Some(call.name.as_str()))
        {
            Some(tool) if self.profile.is_enabled(tool) => {
                self.execute_enabled_call(tool, call.args.as_object()).await
            }
            Some(_) => Err(format!(
                "tool `{}` is not enabled in the active profile",
                call.name
            )),
            None => Err(format!("unknown local tool `{}`", call.name)),
        };

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

    async fn execute_enabled_call(
        &self,
        tool: ToolId,
        args: Option<&Map<String, Value>>,
    ) -> Result<Value, String> {
        let args = args.ok_or("tool arguments must be a JSON object")?;
        match tool {
            ToolId::GoogleSearch => {
                Err("google-search is server-managed and cannot be executed locally".into())
            }
            ToolId::ListFiles => self.list_files(args),
            ToolId::ReadFile => self.read_file(args),
            ToolId::RunCommand => self.run_command(args).await,
        }
    }

    fn list_files(&self, args: &Map<String, Value>) -> Result<Value, String> {
        let path = resolve_optional_string(args, "path")?.unwrap_or(".");
        let recursive = resolve_optional_bool(args, "recursive")?.unwrap_or(false);
        let max_entries = resolve_optional_usize(args, "maxEntries", MAX_LIST_ENTRIES)?
            .unwrap_or(DEFAULT_LIST_MAX_ENTRIES);

        let dir = self.resolve_existing_directory(path)?;
        let mut entries = Vec::new();
        collect_entries(
            &dir,
            recursive,
            max_entries,
            &self.workspace_root_real,
            &mut entries,
        )?;

        Ok(json!({
            "path": self.display_path(&dir),
            "recursive": recursive,
            "entries": entries,
            "truncated": entries.len() >= max_entries,
        }))
    }

    fn read_file(&self, args: &Map<String, Value>) -> Result<Value, String> {
        let path = resolve_required_string(args, "path")?;
        let start_line = resolve_optional_usize(args, "startLine", usize::MAX)?.unwrap_or(1);
        let line_count = resolve_optional_usize(args, "lineCount", MAX_READ_LINE_COUNT)?
            .unwrap_or(DEFAULT_READ_LINE_COUNT);
        if start_line == 0 {
            return Err("`startLine` must be at least 1".into());
        }

        let file = self.resolve_existing_file(path)?;
        let content = std::fs::read_to_string(&file)
            .map_err(|e| format!("failed to read `{}`: {e}", self.display_path(&file)))?;
        let lines = content.lines().collect::<Vec<_>>();
        let start_index = start_line - 1;
        let end_index = start_index.saturating_add(line_count).min(lines.len());
        let slice = if start_index >= lines.len() {
            &[][..]
        } else {
            &lines[start_index..end_index]
        };

        Ok(json!({
            "path": self.display_path(&file),
            "startLine": start_line,
            "endLine": if slice.is_empty() { start_index } else { start_index + slice.len() },
            "content": slice.join("\n"),
            "truncated": end_index < lines.len(),
        }))
    }

    async fn run_command(&self, args: &Map<String, Value>) -> Result<Value, String> {
        let argv = resolve_required_string_array(args, "argv")?;
        if argv.len() > MAX_ARGV_LEN {
            return Err(format!("`argv` may contain at most {MAX_ARGV_LEN} items"));
        }
        let cwd = resolve_optional_string(args, "cwd")?.unwrap_or(".");
        let timeout_secs = resolve_optional_u64(args, "timeoutSecs", MAX_COMMAND_TIMEOUT_SECS)?
            .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECS);
        let cwd = self.resolve_existing_directory(cwd)?;

        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]);
        command.current_dir(&cwd);
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command.kill_on_drop(true);

        let mut child = command.spawn().map_err(|e| {
            format!(
                "failed to start `{}` in `{}`: {e}",
                argv[0],
                self.display_path(&cwd)
            )
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or("failed to capture child stdout")?;
        let stderr = child
            .stderr
            .take()
            .ok_or("failed to capture child stderr")?;
        let stdout_task = tokio::spawn(read_limited(stdout, MAX_OUTPUT_BYTES));
        let stderr_task = tokio::spawn(read_limited(stderr, MAX_OUTPUT_BYTES));

        let (status, timed_out) = tokio::select! {
            status = child.wait() => (
                status.map_err(|e| format!("failed while waiting for `{}`: {e}", argv[0]))?,
                false,
            ),
            _ = tokio::time::sleep(Duration::from_secs(timeout_secs)) => {
                child.kill().await.ok();
                let status = child
                    .wait()
                    .await
                    .map_err(|e| format!("failed while terminating `{}`: {e}", argv[0]))?;
                (status, true)
            }
        };

        let (stdout, stdout_truncated) = join_read_task(stdout_task, "stdout").await?;
        let (stderr, stderr_truncated) = join_read_task(stderr_task, "stderr").await?;

        Ok(json!({
            "argv": argv,
            "cwd": self.display_path(&cwd),
            "timedOut": timed_out,
            "timeoutSecs": timeout_secs,
            "success": status.success() && !timed_out,
            "exitCode": status.code(),
            "stdout": String::from_utf8_lossy(&stdout),
            "stderr": String::from_utf8_lossy(&stderr),
            "stdoutTruncated": stdout_truncated,
            "stderrTruncated": stderr_truncated,
        }))
    }

    fn resolve_existing_directory(&self, raw: &str) -> Result<PathBuf, String> {
        let path = self.resolve_existing_path(raw)?;
        if !path.is_dir() {
            return Err(format!("`{}` is not a directory", self.display_path(&path)));
        }
        Ok(path)
    }

    fn resolve_existing_file(&self, raw: &str) -> Result<PathBuf, String> {
        let path = self.resolve_existing_path(raw)?;
        if !path.is_file() {
            return Err(format!("`{}` is not a file", self.display_path(&path)));
        }
        Ok(path)
    }

    fn resolve_existing_path(&self, raw: &str) -> Result<PathBuf, String> {
        let candidate = normalize_path(&self.workspace_root.join(raw));
        let canonical = candidate.canonicalize().map_err(|e| {
            format!(
                "failed to resolve `{}` under `{}`: {e}",
                raw,
                self.workspace_root.display()
            )
        })?;
        if !canonical.starts_with(&self.workspace_root_real) {
            return Err(format!(
                "`{}` resolves outside the workspace root `{}`",
                raw,
                self.workspace_root.display()
            ));
        }
        Ok(canonical)
    }

    fn display_path(&self, path: &Path) -> String {
        match path.strip_prefix(&self.workspace_root_real) {
            Ok(relative) if relative.as_os_str().is_empty() => ".".to_string(),
            Ok(relative) => relative.display().to_string(),
            Err(_) => path.display().to_string(),
        }
    }
}

impl ToolAdapter for ToolRuntime {
    fn advertised_tools(&self) -> Option<Vec<Tool>> {
        self.profile.build_live_tools()
    }

    fn descriptors(&self) -> Vec<ToolDescriptor> {
        ToolId::ALL
            .into_iter()
            .map(|tool| ToolDescriptor {
                key: tool.key().to_string(),
                summary: tool.summary().to_string(),
                kind: match tool {
                    ToolId::GoogleSearch => ToolKind::BuiltIn,
                    ToolId::ListFiles | ToolId::ReadFile | ToolId::RunCommand => ToolKind::Local,
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

fn list_files_declaration() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "list_files".into(),
        description: "List files and directories under the current workspace root. Use relative paths, keep recursive scans targeted, and inspect the result before reading or executing anything.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative directory to list. Defaults to the workspace root."
                },
                "recursive": {
                    "type": "boolean",
                    "description": "Whether to recurse into subdirectories. Defaults to false."
                },
                "maxEntries": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_LIST_ENTRIES,
                    "description": "Maximum number of returned entries. Defaults to 100."
                }
            }
        }),
        scheduling: None,
        behavior: None,
    }
}

fn read_file_declaration() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "read_file".into(),
        description: "Read a UTF-8 text file under the current workspace root. Prefer this after list_files so the path is already known. Returns numbered slices rather than the entire file when line ranges are provided.".into(),
        parameters: json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative path to a UTF-8 text file."
                },
                "startLine": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based line number to start reading from. Defaults to 1."
                },
                "lineCount": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_READ_LINE_COUNT,
                    "description": "Maximum number of lines to return. Defaults to 200."
                }
            }
        }),
        scheduling: None,
        behavior: None,
    }
}

fn run_command_declaration() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "run_command".into(),
        description: "Run a non-interactive command under the current workspace root without using a shell. Use this only when file inspection is insufficient and keep commands short, deterministic, and argv-based.".into(),
        parameters: json!({
            "type": "object",
            "required": ["argv"],
            "properties": {
                "argv": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_ARGV_LEN,
                    "items": { "type": "string" },
                    "description": "Program name plus argv items. Shell syntax is not supported."
                },
                "cwd": {
                    "type": "string",
                    "description": "Workspace-relative working directory. Defaults to the workspace root."
                },
                "timeoutSecs": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_COMMAND_TIMEOUT_SECS,
                    "description": "Execution timeout in seconds. Defaults to 10."
                }
            }
        }),
        scheduling: None,
        behavior: None,
    }
}

fn collect_entries(
    root: &Path,
    recursive: bool,
    max_entries: usize,
    workspace_root: &Path,
    out: &mut Vec<Value>,
) -> Result<(), String> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = std::fs::read_dir(&dir)
            .map_err(|e| format!("failed to read directory `{}`: {e}", dir.display()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("failed to iterate directory `{}`: {e}", dir.display()))?;
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            if out.len() >= max_entries {
                return Ok(());
            }

            let path = entry.path();
            let canonical = path.canonicalize().unwrap_or(path.clone());
            if !canonical.starts_with(workspace_root) {
                continue;
            }

            let file_type = entry
                .file_type()
                .map_err(|e| format!("failed to inspect `{}`: {e}", path.display()))?;
            let kind = if file_type.is_dir() {
                "dir"
            } else if file_type.is_file() {
                "file"
            } else if file_type.is_symlink() {
                "symlink"
            } else {
                "other"
            };
            let relative = canonical
                .strip_prefix(workspace_root)
                .unwrap_or(canonical.as_path());
            let metadata = entry.metadata().ok();
            out.push(json!({
                "path": if relative.as_os_str().is_empty() {
                    ".".to_string()
                } else {
                    relative.display().to_string()
                },
                "kind": kind,
                "sizeBytes": metadata.filter(|_| file_type.is_file()).map(|m| m.len()),
            }));

            if recursive && file_type.is_dir() {
                stack.push(canonical);
            }
        }
    }

    Ok(())
}

async fn read_limited<R>(mut reader: R, limit: usize) -> io::Result<(Vec<u8>, bool)>
where
    R: AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        if buf.len() < limit {
            let remaining = limit - buf.len();
            let to_take = remaining.min(read);
            buf.extend_from_slice(&chunk[..to_take]);
            if to_take < read {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
    Ok((buf, truncated))
}

async fn join_read_task(
    task: tokio::task::JoinHandle<io::Result<(Vec<u8>, bool)>>,
    label: &str,
) -> Result<(Vec<u8>, bool), String> {
    task.await
        .map_err(|e| format!("failed to join child {label} reader: {e}"))?
        .map_err(|e| format!("failed to read child {label}: {e}"))
}

fn resolve_required_string<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    resolve_optional_string(args, key)?.ok_or_else(|| format!("missing required string `{key}`"))
}

fn resolve_optional_string<'a>(
    args: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

fn resolve_optional_bool(args: &Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    match args.get(key) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("`{key}` must be a boolean")),
    }
}

fn resolve_optional_u64(
    args: &Map<String, Value>,
    key: &str,
    max: u64,
) -> Result<Option<u64>, String> {
    match args.get(key) {
        None => Ok(None),
        Some(Value::Number(value)) => {
            let Some(number) = value.as_u64() else {
                return Err(format!("`{key}` must be a non-negative integer"));
            };
            if number == 0 {
                return Err(format!("`{key}` must be at least 1"));
            }
            if number > max {
                return Err(format!("`{key}` must be at most {max}"));
            }
            Ok(Some(number))
        }
        Some(_) => Err(format!("`{key}` must be an integer")),
    }
}

fn resolve_optional_usize(
    args: &Map<String, Value>,
    key: &str,
    max: usize,
) -> Result<Option<usize>, String> {
    resolve_optional_u64(args, key, max as u64).map(|value| value.map(|v| v as usize))
}

fn resolve_required_string_array(
    args: &Map<String, Value>,
    key: &str,
) -> Result<Vec<String>, String> {
    let Some(value) = args.get(key) else {
        return Err(format!("missing required array `{key}`"));
    };
    let Some(items) = value.as_array() else {
        return Err(format!("`{key}` must be an array of strings"));
    };
    if items.is_empty() {
        return Err(format!("`{key}` must contain at least one string"));
    }
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("`{key}` must contain only strings"))
        })
        .collect()
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                normalized.push(component.as_os_str())
            }
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_profile_builds_google_search_and_functions() {
        let profile = ToolProfile {
            google_search: true,
            list_files: true,
            read_file: true,
            run_command: false,
        };

        let tools = profile.build_live_tools().expect("live tools");
        assert!(matches!(tools[0], Tool::GoogleSearch(_)));
        match &tools[1] {
            Tool::FunctionDeclarations(functions) => {
                let names = functions
                    .iter()
                    .map(|f| f.name.as_str())
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
            read_file: true,
            ..ToolProfile::default()
        };
        let lines = catalog_lines(active, desired);
        assert!(lines.iter().any(|line| line.contains("read-file [staged")));
    }
}
