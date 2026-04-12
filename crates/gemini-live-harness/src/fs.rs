//! Filesystem helpers for durable harness state.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::HarnessError;

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(1);

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time must be after unix epoch")
        .as_millis()
        .try_into()
        .expect("millisecond timestamp should fit into u64")
}

pub(crate) fn next_id(prefix: &str) -> String {
    let counter = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{prefix}_{:013}_{:05}_{counter}",
        now_ms(),
        std::process::id()
    )
}

pub(crate) fn ensure_dir(path: &Path) -> Result<(), HarnessError> {
    std::fs::create_dir_all(path).map_err(|error| HarnessError::io(path, error))
}

pub(crate) fn read_json<T>(path: &Path) -> Result<T, HarnessError>
where
    T: DeserializeOwned,
{
    let bytes = std::fs::read(path).map_err(|error| HarnessError::io(path, error))?;
    serde_json::from_slice(&bytes).map_err(|error| HarnessError::json(path, error))
}

pub(crate) fn write_json_atomic<T>(path: &Path, value: &T) -> Result<(), HarnessError>
where
    T: Serialize,
{
    let parent = path
        .parent()
        .expect("atomic writes require a parent directory");
    ensure_dir(parent)?;

    let tmp_name = format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state"),
        next_id("write")
    );
    let tmp_path = parent.join(tmp_name);
    let mut file = File::create(&tmp_path).map_err(|error| HarnessError::io(&tmp_path, error))?;
    serde_json::to_writer_pretty(&mut file, value)
        .map_err(|error| HarnessError::json(path, error))?;
    file.write_all(b"\n")
        .map_err(|error| HarnessError::io(&tmp_path, error))?;
    file.sync_all()
        .map_err(|error| HarnessError::io(&tmp_path, error))?;
    std::fs::rename(&tmp_path, path).map_err(|error| HarnessError::io(path, error))?;
    Ok(())
}

pub(crate) fn read_json_lines<T>(path: &Path) -> Result<Vec<T>, HarnessError>
where
    T: DeserializeOwned,
{
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = File::open(path).map_err(|error| HarnessError::io(path, error))?;
    let reader = BufReader::new(file);
    let mut entries = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|error| HarnessError::io(path, error))?;
        if line.trim().is_empty() {
            continue;
        }
        let entry = serde_json::from_str(&line).map_err(|error| HarnessError::json(path, error))?;
        entries.push(entry);
    }
    Ok(entries)
}

pub(crate) fn append_json_line<T>(path: &Path, value: &T) -> Result<(), HarnessError>
where
    T: Serialize,
{
    let parent = path.parent().expect("jsonl paths must have a parent");
    ensure_dir(parent)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| HarnessError::io(path, error))?;
    serde_json::to_writer(&mut file, value).map_err(|error| HarnessError::json(path, error))?;
    file.write_all(b"\n")
        .map_err(|error| HarnessError::io(path, error))?;
    file.sync_all()
        .map_err(|error| HarnessError::io(path, error))?;
    Ok(())
}

pub(crate) fn remove_file_if_exists(path: &Path) -> Result<(), HarnessError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(HarnessError::io(path, error)),
    }
}

pub(crate) fn list_child_dirs(path: &Path) -> Result<Vec<PathBuf>, HarnessError> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(path).map_err(|error| HarnessError::io(path, error))? {
        let entry = entry.map_err(|error| HarnessError::io(path, error))?;
        let file_type = entry
            .file_type()
            .map_err(|error| HarnessError::io(entry.path(), error))?;
        if file_type.is_dir() {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    Ok(dirs)
}

pub(crate) fn list_child_files(path: &Path) -> Result<Vec<PathBuf>, HarnessError> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for entry in std::fs::read_dir(path).map_err(|error| HarnessError::io(path, error))? {
        let entry = entry.map_err(|error| HarnessError::io(path, error))?;
        let file_type = entry
            .file_type()
            .map_err(|error| HarnessError::io(entry.path(), error))?;
        if file_type.is_file() {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

pub(crate) fn validate_segment(kind: &'static str, value: &str) -> Result<(), HarnessError> {
    let valid = !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(HarnessError::InvalidSegment {
            kind,
            value: value.to_string(),
        })
    }
}
