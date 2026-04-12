//! Workspace-local tools shared by Gemini Live hosts.
//!
//! These tools operate only on the current workspace root and do not require
//! host-specific runtime state such as desktop devices or UI state.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use futures_util::future::BoxFuture;
use gemini_live::types::{FunctionCallRequest, FunctionDeclaration, FunctionResponse, Tool};
use gemini_live_harness::{
    ToolCapability, ToolDescriptor, ToolExecutionError, ToolExecutor, ToolKind, ToolProvider,
    ToolSpecification,
};
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
pub enum WorkspaceToolId {
    ListFiles,
    ReadFile,
    RunCommand,
}

impl WorkspaceToolId {
    pub const ALL: [Self; 3] = [Self::ListFiles, Self::ReadFile, Self::RunCommand];

    pub fn key(self) -> &'static str {
        match self {
            Self::ListFiles => "list-files",
            Self::ReadFile => "read-file",
            Self::RunCommand => "run-command",
        }
    }

    pub fn summary(self) -> &'static str {
        match self {
            Self::ListFiles => "list workspace files and directories",
            Self::ReadFile => "read UTF-8 text files under the workspace root",
            Self::RunCommand => "run a non-interactive argv-only command under the workspace root",
        }
    }

    pub fn function_name(self) -> &'static str {
        match self {
            Self::ListFiles => "list_files",
            Self::ReadFile => "read_file",
            Self::RunCommand => "run_command",
        }
    }

    pub fn from_function_name(name: &str) -> Option<Self> {
        match name {
            "list_files" => Some(Self::ListFiles),
            "read_file" => Some(Self::ReadFile),
            "run_command" => Some(Self::RunCommand),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct WorkspaceToolSelection {
    pub list_files: bool,
    pub read_file: bool,
    pub run_command: bool,
}

impl Default for WorkspaceToolSelection {
    fn default() -> Self {
        Self {
            list_files: true,
            read_file: true,
            run_command: false,
        }
    }
}

impl WorkspaceToolSelection {
    pub fn is_enabled(self, tool: WorkspaceToolId) -> bool {
        match tool {
            WorkspaceToolId::ListFiles => self.list_files,
            WorkspaceToolId::ReadFile => self.read_file,
            WorkspaceToolId::RunCommand => self.run_command,
        }
    }

    pub fn set(&mut self, tool: WorkspaceToolId, enabled: bool) -> bool {
        let slot = match tool {
            WorkspaceToolId::ListFiles => &mut self.list_files,
            WorkspaceToolId::ReadFile => &mut self.read_file,
            WorkspaceToolId::RunCommand => &mut self.run_command,
        };
        let changed = *slot != enabled;
        *slot = enabled;
        changed
    }

    pub fn toggle(&mut self, tool: WorkspaceToolId) -> bool {
        let next = !self.is_enabled(tool);
        self.set(tool, next);
        next
    }

    pub fn summary(self) -> String {
        let enabled = WorkspaceToolId::ALL
            .into_iter()
            .filter(|tool| self.is_enabled(*tool))
            .map(WorkspaceToolId::key)
            .collect::<Vec<_>>();
        if enabled.is_empty() {
            "none".to_string()
        } else {
            enabled.join(", ")
        }
    }

    pub fn function_declarations(self) -> Vec<FunctionDeclaration> {
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
        functions
    }

    pub fn build_live_tool(self) -> Option<Tool> {
        let functions = self.function_declarations();
        (!functions.is_empty()).then_some(Tool::FunctionDeclarations(functions))
    }
}

#[derive(Debug, Clone)]
pub struct WorkspaceToolAdapter {
    selection: WorkspaceToolSelection,
    workspace_root: PathBuf,
    workspace_root_real: PathBuf,
}

impl WorkspaceToolAdapter {
    pub fn new(workspace_root: PathBuf, selection: WorkspaceToolSelection) -> io::Result<Self> {
        let workspace_root_real = workspace_root.canonicalize()?;
        Ok(Self {
            selection,
            workspace_root,
            workspace_root_real,
        })
    }

    pub fn selection(&self) -> WorkspaceToolSelection {
        self.selection
    }

    pub async fn execute_call(&self, call: FunctionCallRequest) -> FunctionResponse {
        let result = match WorkspaceToolId::from_function_name(call.name.as_str()) {
            Some(tool) if self.selection.is_enabled(tool) => {
                self.execute_enabled_call(tool, call.args.as_object()).await
            }
            Some(_) => Err(format!(
                "tool `{}` is not enabled in the active profile",
                call.name
            )),
            None => Err(format!("unknown workspace tool `{}`", call.name)),
        };

        function_response(call, result)
    }

    async fn execute_enabled_call(
        &self,
        tool: WorkspaceToolId,
        args: Option<&Map<String, Value>>,
    ) -> Result<Value, String> {
        let args = args.ok_or("tool arguments must be a JSON object")?;
        match tool {
            WorkspaceToolId::ListFiles => self.list_files(args),
            WorkspaceToolId::ReadFile => self.read_file(args),
            WorkspaceToolId::RunCommand => self.run_command(args).await,
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

impl ToolProvider for WorkspaceToolAdapter {
    fn advertised_tools(&self) -> Option<Vec<Tool>> {
        self.selection.build_live_tool().map(|tool| vec![tool])
    }

    fn descriptors(&self) -> Vec<ToolDescriptor> {
        WorkspaceToolId::ALL
            .into_iter()
            .map(|tool| ToolDescriptor {
                key: tool.key().to_string(),
                summary: tool.summary().to_string(),
                kind: ToolKind::Local,
            })
            .collect()
    }

    fn specifications(&self) -> Vec<ToolSpecification> {
        WorkspaceToolId::ALL
            .into_iter()
            .filter(|tool| self.selection.is_enabled(*tool))
            .map(|tool| ToolSpecification::new(tool.function_name(), ToolCapability::INLINE_ONLY))
            .collect()
    }
}

impl ToolExecutor for WorkspaceToolAdapter {
    fn execute<'a>(
        &'a self,
        call: FunctionCallRequest,
    ) -> BoxFuture<'a, Result<FunctionResponse, ToolExecutionError>> {
        Box::pin(async move { Ok(self.execute_call(call).await) })
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

fn list_files_declaration() -> FunctionDeclaration {
    FunctionDeclaration {
        name: WorkspaceToolId::ListFiles.function_name().into(),
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
        name: WorkspaceToolId::ReadFile.function_name().into(),
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
        name: WorkspaceToolId::RunCommand.function_name().into(),
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
    fn workspace_selection_builds_declared_functions() {
        let selection = WorkspaceToolSelection {
            list_files: true,
            read_file: true,
            run_command: false,
        };

        let tool = selection.build_live_tool().expect("workspace tool");
        match tool {
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
    fn summary_lists_enabled_workspace_tools() {
        let selection = WorkspaceToolSelection {
            list_files: false,
            read_file: true,
            run_command: true,
        };

        assert_eq!(selection.summary(), "read-file, run-command");
    }
}
