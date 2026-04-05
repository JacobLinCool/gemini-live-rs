//! Self-update: downloads the latest CLI binary from GitHub Releases.

use std::env;
use std::fs;
use std::process::Command;

const REPO: &str = "JacobLinCool/gemini-live-rs";
const BIN_NAME: &str = "gemini-live-cli";

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const TARGET: &str = "x86_64-unknown-linux-gnu";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const TARGET: &str = "aarch64-unknown-linux-gnu";
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const TARGET: &str = "x86_64-apple-darwin";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const TARGET: &str = "aarch64-apple-darwin";

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let current = env!("CARGO_PKG_VERSION");
    let asset_name = format!("{BIN_NAME}-{TARGET}.tar.gz");

    println!("{BIN_NAME} v{current} ({TARGET})");
    println!("Checking for updates...");

    let client = reqwest::Client::new();
    let resp: serde_json::Value = client
        .get(format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .header("User-Agent", format!("{BIN_NAME}/{current}"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let tag = resp["tag_name"]
        .as_str()
        .ok_or("missing tag_name in release")?;
    let latest = tag.trim_start_matches('v');

    if latest == current {
        println!("Already up to date!");
        return Ok(());
    }
    println!("New version available: v{latest}");

    // Find the matching asset
    let assets = resp["assets"].as_array().ok_or("missing assets")?;
    let download_url = assets
        .iter()
        .find(|a| a["name"].as_str() == Some(&asset_name))
        .and_then(|a| a["browser_download_url"].as_str())
        .ok_or_else(|| format!("no pre-built binary for {TARGET}"))?
        .to_string();

    println!("Downloading {asset_name}...");
    let bytes = client
        .get(&download_url)
        .header("User-Agent", format!("{BIN_NAME}/{current}"))
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    // Write archive to temp location
    let tmp_dir = env::temp_dir();
    let archive_path = tmp_dir.join(&asset_name);
    let extract_dir = tmp_dir.join(format!("{BIN_NAME}-update"));
    let _ = fs::remove_dir_all(&extract_dir);
    fs::create_dir_all(&extract_dir)?;
    fs::write(&archive_path, &bytes)?;

    // Extract using system tar
    let status = Command::new("tar")
        .arg("xzf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&extract_dir)
        .status()?;
    if !status.success() {
        return Err("failed to extract archive".into());
    }

    // Replace current binary (atomic rename on same filesystem)
    let new_bin = extract_dir.join(BIN_NAME);
    if !new_bin.exists() {
        return Err("extracted binary not found".into());
    }
    let current_exe = env::current_exe()?.canonicalize()?;
    let tmp_bin = current_exe.with_extension("new");

    fs::copy(&new_bin, &tmp_bin)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp_bin, fs::Permissions::from_mode(0o755))?;
    }
    fs::rename(&tmp_bin, &current_exe)?;

    // Cleanup
    let _ = fs::remove_file(&archive_path);
    let _ = fs::remove_dir_all(&extract_dir);

    println!("Updated to v{latest}!");
    Ok(())
}
