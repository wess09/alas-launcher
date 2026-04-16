use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use reqwest::blocking::Client;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tauri::AppHandle;
use tracing::{info, warn};

use crate::{backend::ManagedBackend, window_util::CreateNoWindow as _};

const DEFAULT_UPDATE_ENDPOINTS: &[&str] = &["https://alas.nanoda.work/updata/stable.json"];
const UPDATE_CHECK_DELAY: Duration = Duration::from_secs(45);

#[derive(Debug, Deserialize)]
struct UpdateManifest {
    version: String,
    notes: Option<String>,
    platforms: HashMap<String, PlatformRelease>,
}

#[derive(Debug, Deserialize, Clone)]
struct PlatformRelease {
    url: String,
    sha256: String,
}

pub fn start_silent_launcher_update(
    app_handle: AppHandle,
    backend: Arc<Mutex<Option<ManagedBackend>>>,
    preserve_backend_on_exit: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        thread::sleep(UPDATE_CHECK_DELAY);
        if let Err(e) = check_and_apply_update(&app_handle, &backend, &preserve_backend_on_exit) {
            warn!("Silent launcher update skipped: {e:#}");
        }
    });
}

fn check_and_apply_update(
    app_handle: &AppHandle,
    backend: &Arc<Mutex<Option<ManagedBackend>>>,
    preserve_backend_on_exit: &Arc<AtomicBool>,
) -> Result<()> {
    let endpoints = update_endpoints();
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(6))
        .timeout(Duration::from_secs(15))
        .build()?;
    let (manifest, endpoint) = fetch_manifest(&client, &endpoints)?;
    let current_version = parse_version(env!("CARGO_PKG_VERSION"))?;
    let target_version = parse_version(&manifest.version)?;
    if target_version <= current_version {
        info!(
            "Launcher is up to date (current={}, latest={})",
            current_version, target_version
        );
        return Ok(());
    }

    let platform_key = platform_key()?;
    let platform_release = manifest
        .platforms
        .get(&platform_key)
        .cloned()
        .ok_or_else(|| anyhow!("No update payload for platform {platform_key}"))?;

    if let Some(notes) = manifest.notes.as_deref() {
        info!(
            "Found launcher update {} via {} ({platform_key}): {}",
            target_version,
            endpoint,
            notes.trim()
        );
    } else {
        info!(
            "Found launcher update {} via {} ({platform_key})",
            target_version, endpoint
        );
    }

    let update_path = download_update_binary(&client, &platform_release)?;
    let current_exe = std::env::current_exe().context("unable to resolve launcher executable")?;
    spawn_update_helper(std::process::id(), &current_exe, &update_path)?;

    preserve_backend_on_exit.store(true, Ordering::SeqCst);
    if let Some(ref mut managed) = *backend.lock().unwrap() {
        managed.detach_for_self_update();
    }

    info!("Launcher update prepared, restarting launcher without stopping backend");
    app_handle.exit(0);
    Ok(())
}

fn update_endpoints() -> Vec<String> {
    if let Ok(raw) = std::env::var("ALAS_LAUNCHER_UPDATE_ENDPOINTS") {
        let configured: Vec<String> = raw
            .split(';')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        if !configured.is_empty() {
            return configured;
        }
    }
    DEFAULT_UPDATE_ENDPOINTS
        .iter()
        .map(|value| (*value).to_string())
        .collect()
}

fn fetch_manifest(client: &Client, endpoints: &[String]) -> Result<(UpdateManifest, String)> {
    let mut last_error = None;
    for endpoint in endpoints {
        match client.get(endpoint).send() {
            Ok(response) => {
                if !response.status().is_success() {
                    last_error = Some(anyhow!(
                        "endpoint {} returned status {}",
                        endpoint,
                        response.status()
                    ));
                    continue;
                }
                let manifest = response
                    .json::<UpdateManifest>()
                    .with_context(|| format!("invalid update manifest from {}", endpoint))?;
                return Ok((manifest, endpoint.clone()));
            }
            Err(err) => {
                last_error = Some(anyhow!("failed to request {}: {}", endpoint, err));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("no update endpoint configured")))
}

fn parse_version(raw: &str) -> Result<Version> {
    let normalized = raw.trim().trim_start_matches('v');
    Version::parse(normalized).with_context(|| format!("invalid semver version: {raw}"))
}

fn platform_key() -> Result<String> {
    let os = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        return Err(anyhow!("unsupported operating system for launcher updater"));
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        "x86" => "i686",
        "arm" => "armv7",
        other => {
            return Err(anyhow!(
                "unsupported architecture for launcher updater: {other}"
            ));
        }
    };
    Ok(format!("{os}-{arch}"))
}

fn download_update_binary(client: &Client, release: &PlatformRelease) -> Result<PathBuf> {
    let response = client
        .get(&release.url)
        .send()
        .with_context(|| format!("failed to download update payload from {}", release.url))?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "update payload request failed with status {}",
            response.status()
        ));
    }
    let bytes = response.bytes()?;
    let actual_sha = format!("{:x}", Sha256::digest(&bytes));
    let expected_sha = release.sha256.trim().to_ascii_lowercase();
    if actual_sha != expected_sha {
        return Err(anyhow!(
            "sha256 mismatch for update payload (expected {}, got {})",
            expected_sha,
            actual_sha
        ));
    }

    let temp_dir =
        std::env::temp_dir().join(format!("alas-launcher-update-{}", std::process::id()));
    fs::create_dir_all(&temp_dir)?;
    let binary_path = temp_dir.join(update_binary_filename());
    fs::write(&binary_path, &bytes)?;
    Ok(binary_path)
}

fn update_binary_filename() -> &'static str {
    if cfg!(windows) {
        "alas-launcher.next.exe"
    } else {
        "alas-launcher.next"
    }
}

fn spawn_update_helper(parent_pid: u32, target_path: &Path, source_path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        spawn_windows_update_helper(parent_pid, target_path, source_path)
    }
    #[cfg(not(windows))]
    {
        spawn_unix_update_helper(parent_pid, target_path, source_path)
    }
}

#[cfg(windows)]
fn spawn_windows_update_helper(
    parent_pid: u32,
    target_path: &Path,
    source_path: &Path,
) -> Result<()> {
    let script_dir = source_path
        .parent()
        .ok_or_else(|| anyhow!("invalid update payload path"))?;
    let script_path = script_dir.join("apply-update.ps1");
    let script = r#"
param(
  [int]$ParentPid,
  [string]$Source,
  [string]$Target
)
$ErrorActionPreference = "SilentlyContinue"
for ($i = 0; $i -lt 240; $i++) {
  if (-not (Get-Process -Id $ParentPid -ErrorAction SilentlyContinue)) {
    break
  }
  Start-Sleep -Milliseconds 250
}
Copy-Item -LiteralPath $Source -Destination $Target -Force
$env:ALAS_LAUNCHER_REATTACH = "1"
$env:ALAS_LAUNCHER_REATTACH_OWNER_PID = "$ParentPid"
Start-Process -FilePath $Target -WindowStyle Hidden
Remove-Item -LiteralPath $Source -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $PSCommandPath -Force -ErrorAction SilentlyContinue
"#;
    fs::write(&script_path, script)?;

    let mut cmd = Command::new("powershell");
    cmd.create_no_window();
    cmd.args([
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-WindowStyle",
        "Hidden",
        "-File",
    ])
    .arg(&script_path)
    .arg("-ParentPid")
    .arg(parent_pid.to_string())
    .arg("-Source")
    .arg(source_path)
    .arg("-Target")
    .arg(target_path);
    cmd.spawn()
        .with_context(|| format!("failed to spawn update helper script {:?}", script_path))?;
    Ok(())
}

#[cfg(not(windows))]
fn spawn_unix_update_helper(parent_pid: u32, target_path: &Path, source_path: &Path) -> Result<()> {
    let script_dir = source_path
        .parent()
        .ok_or_else(|| anyhow!("invalid update payload path"))?;
    let script_path = script_dir.join("apply-update.sh");
    let script = format!(
        r#"#!/bin/sh
set -eu
PARENT_PID={parent_pid}
SOURCE={source}
TARGET={target}
i=0
while [ "$i" -lt 240 ]; do
  if ! kill -0 "$PARENT_PID" 2>/dev/null; then
    break
  fi
  i=$((i + 1))
  sleep 0.25
done
cp "$SOURCE" "$TARGET"
chmod +x "$TARGET"
ALAS_LAUNCHER_REATTACH=1 ALAS_LAUNCHER_REATTACH_OWNER_PID={owner_pid} "$TARGET" >/dev/null 2>&1 &
rm -f "$SOURCE" "$0"
"#,
        parent_pid = parent_pid,
        owner_pid = parent_pid,
        source = sh_quote(source_path),
        target = sh_quote(target_path),
    );
    fs::write(&script_path, script)?;
    make_script_executable(&script_path)?;

    let mut cmd = Command::new("sh");
    cmd.create_no_window();
    cmd.arg(&script_path);
    cmd.spawn()
        .with_context(|| format!("failed to spawn update helper script {:?}", script_path))?;
    Ok(())
}

#[cfg(not(windows))]
fn make_script_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(windows))]
fn sh_quote(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
