// No default console window createion on Windows
#![windows_subsystem = "windows"]

mod backend;
mod i18n;
mod notify;
mod setup;
mod window_util;

#[macro_use]
extern crate rust_i18n;
i18n!("locales", fallback = "en");

use std::{
    cell::Cell,
    collections::HashMap,
    fs,
    io::{Read, Seek, SeekFrom, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self},
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use base64::{prelude::BASE64_STANDARD, Engine};
use chrono::{DateTime, FixedOffset, Local, Utc};
use reqwest::{
    blocking::Client,
    header::{
        HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, CONTENT_LENGTH, CONTENT_RANGE, DATE,
        RANGE, USER_AGENT,
    },
    StatusCode,
};
use rust_i18n::t;
use serde::Deserialize;
use serde_json::to_string;
use sha2::{Digest, Sha256};
use tauri::{
    image::Image,
    menu::{MenuBuilder, MenuItemBuilder},
    tray::TrayIconBuilder,
    webview::{PageLoadEvent, PageLoadPayload},
    Manager, Url, WebviewWindow,
};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_dialog::FilePath;
#[cfg(windows)]
use tauri_plugin_dialog::MessageDialogButtons;
use tracing::{debug, error, info, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, Layer};

use crate::{
    backend::{ManagedBackend, WebuiLaunchConfig},
    notify::{start_notify_stream, NotificationClickHandler},
    setup::{
        cleanup_runtime_for_rebuild, get_deploy_config, setup_alas_repo, setup_environment,
        SplashUpdate,
    },
};

#[cfg(target_os = "macos")]
const MENUBAR_ICON_2X: &[u8] = include_bytes!("../icons/menubar@2x.png");
#[cfg(target_os = "macos")]
const MENUBAR_ICON_1X: &[u8] = include_bytes!("../icons/menubar.png");
#[cfg(windows)]
const WINDOWS_TRAY_ICON: &[u8] = include_bytes!("../icons/icon.png");
const SPLASH_BG_LIGHT: &[u8] = include_bytes!("../bg/l_bg.webp");
const SPLASH_BG_DARK: &[u8] = include_bytes!("../bg/b_bg.webp");
const BACKEND_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const BACKEND_NAVIGATION_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(any(windows, target_os = "android"))]
const BACKEND_ERROR_URL_BASE: &str = "http://alas-error.localhost/backend";
#[cfg(not(any(windows, target_os = "android")))]
const BACKEND_ERROR_URL_BASE: &str = "alas-error://localhost/backend";
#[cfg(any(windows, target_os = "android"))]
const SPLASH_URL: &str = "http://alas-splash.localhost/";
#[cfg(not(any(windows, target_os = "android")))]
const SPLASH_URL: &str = "alas-splash://localhost/";
const TIME_BOMB_CONFIG_SOURCE: &str = include_str!("../Cargo.toml");
const LAUNCHER_UPDATE_URL: &str = "https://alas.nanoda.work/updata/stable.json";
const LAUNCHER_UPDATE_SKIP_ENV: &str = "AZURPILOT_SKIP_LAUNCHER_UPDATE";
const MINI_LAUNCHER_VERSION: &str = "0.0.1";
const LAUNCHER_UPDATE_MIN_PARALLEL_BYTES: u64 = 4 * 1024 * 1024;
const LAUNCHER_UPDATE_MAX_CHUNK_BYTES: u64 = 500 * 1024;
const LAUNCHER_UPDATE_MIN_CHUNK_BYTES: u64 = 64 * 1024;
const LAUNCHER_UPDATE_BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36 AZURPILOT_LAUNCHER_UPDATE/2.0.4";
#[cfg(windows)]
const LAUNCHER_UPDATE_NO_CONSOLE_ENV: &str = "AZURPILOT_NO_ATTACH_CONSOLE";
#[cfg(windows)]
const LAUNCHER_UPDATE_APPLY_ARG: &str = "--apply-launcher-update";
const PREVIEW_NO_UPDATE_ARGS: &[&str] = &[
    "--preview-no-update",
    "--skip-update",
    "--no-update",
    "--disable-update",
    "/preview-no-update",
    "/skip-update",
    "/no-update",
];
const PREVIEW_CRASH_ARGS: &[&str] = &[
    "--preview-crash",
    "--preview-error",
    "--crash-preview",
    "--error-preview",
    "/preview-crash",
    "/preview-error",
];

#[derive(Clone, Debug)]
struct TimeBombConfig {
    expires_at: DateTime<FixedOffset>,
    network_time_url: String,
    message: String,
}

#[derive(Debug, Deserialize)]
struct LauncherUpdateManifest {
    version: String,
    platforms: HashMap<String, LauncherUpdatePlatform>,
}

#[derive(Debug, Deserialize)]
struct LauncherUpdatePlatform {
    url: String,
    sha256: String,
}

#[derive(Clone, Copy, Debug)]
struct LauncherUpdateProbe {
    total_bytes: Option<u64>,
    supports_ranges: bool,
}

#[cfg(target_os = "macos")]
fn tray_icon_for_platform() -> Image<'static> {
    info!("Loading macOS tray icon from embedded bytes...");
    let result = Image::from_bytes(MENUBAR_ICON_2X)
        .or_else(|_| {
            info!("2x icon failed, trying 1x...");
            Image::from_bytes(MENUBAR_ICON_1X)
        })
        .unwrap_or_else(|err| {
            error!(
                ?err,
                "Failed to load tray icon from embedded menubar icon bytes (2x and 1x)."
            );
            panic!("Failed to load tray icon from embedded menubar icon bytes: {err}");
        });
    info!("Tray icon loaded successfully");
    result
}

#[cfg(windows)]
fn tray_icon_for_platform() -> Image<'static> {
    Image::from_bytes(WINDOWS_TRAY_ICON).unwrap_or_else(|err| {
        error!(?err, "Failed to load tray icon from embedded icon bytes.");
        panic!("Failed to load tray icon from embedded icon bytes: {err}");
    })
}

fn begin_startup_cleanup(
    app_handle: tauri::AppHandle,
    allow_exit: Arc<AtomicBool>,
    setup_cancel_requested: Arc<AtomicBool>,
    setup_running: Arc<AtomicBool>,
    startup_cleanup_started: Arc<AtomicBool>,
) {
    if startup_cleanup_started
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    setup_cancel_requested.store(true, Ordering::SeqCst);
    if let Some(splash) = app_handle.get_webview_window("splash") {
        update_splash(
            &splash,
            &SplashUpdate::loading(
                t!("dialog.cleaning_env"),
                t!("dialog.cleaning_env_detail"),
                99,
            )
            .with_subtitle(t!("dialog.cleaning_wait")),
        );
    }

    app_handle
        .dialog()
        .message(t!("dialog.cleaning_message"))
        .title(t!("dialog.cleaning_env"))
        .show(|_| {});

    thread::spawn(move || {
        let started_at = Instant::now();
        while setup_running.load(Ordering::SeqCst) && started_at.elapsed() < Duration::from_secs(30)
        {
            thread::sleep(Duration::from_millis(100));
        }

        if setup_running.load(Ordering::SeqCst) {
            warn!("Setup thread did not stop before startup cleanup timeout");
        }

        match cleanup_runtime_for_rebuild() {
            Ok(()) => {
                info!("Startup cleanup finished; runtime will be rebuilt on next launch");
            }
            Err(e) => {
                error!("Startup cleanup failed: {:?}", e);
                if let Some(splash) = app_handle.get_webview_window("splash") {
                    update_splash(
                        &splash,
                        &SplashUpdate::error(
                            t!("dialog.cleanup_failed"),
                            t!("dialog.cleanup_failed_detail", error = format!("{e:#}")),
                            99,
                        ),
                    );
                }
                startup_cleanup_started.store(false, Ordering::SeqCst);
                return;
            }
        }

        allow_exit.store(true, Ordering::SeqCst);
        app_handle.exit(0);
    });
}

fn time_bomb_config() -> Result<Option<TimeBombConfig>> {
    let Some(section) = cargo_toml_section("package.metadata.alas-launcher.time-bomb") else {
        return Ok(None);
    };
    let enabled = cargo_toml_value(section, "enabled")
        .and_then(|value| value.parse::<bool>().ok())
        .unwrap_or(false);
    if !enabled {
        return Ok(None);
    }

    let expires_at = cargo_toml_value(section, "expires-at")
        .ok_or_else(|| anyhow!(t!("errors.time_bomb_not_configured")))?;
    let expires_at = DateTime::parse_from_rfc3339(&expires_at)
        .map_err(|err| anyhow!(t!("errors.time_bomb_format_error", error = err.to_string())))?;
    let network_time_url = cargo_toml_value(section, "network-time-url")
        .unwrap_or_else(|| "http://www.gstatic.com/generate_204".to_owned());
    let message = cargo_toml_value(section, "message")
        .unwrap_or_else(|| t!("errors.time_bomb_expired").to_string());

    Ok(Some(TimeBombConfig {
        expires_at,
        network_time_url,
        message,
    }))
}

fn cargo_toml_section(section_name: &str) -> Option<&'static str> {
    let header = format!("[{section_name}]");
    let start = TIME_BOMB_CONFIG_SOURCE.find(&header)? + header.len();
    let rest = &TIME_BOMB_CONFIG_SOURCE[start..];
    let end = rest.find("\n[").unwrap_or(rest.len());
    Some(&rest[..end])
}

fn cargo_toml_value(section: &str, key: &str) -> Option<String> {
    for line in section.lines() {
        let line = line
            .split_once('#')
            .map(|(left, _)| left)
            .unwrap_or(line)
            .trim();
        let Some((left, right)) = line.split_once('=') else {
            continue;
        };
        if left.trim() != key {
            continue;
        }
        let value = right.trim();
        return Some(
            value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(value)
                .to_owned(),
        );
    }
    None
}

fn time_bomb_expiration_message() -> Result<Option<String>> {
    let Some(config) = time_bomb_config()? else {
        return Ok(None);
    };
    let network_time = fetch_network_time(&config.network_time_url)?;
    if network_time >= config.expires_at.with_timezone(&Utc) {
        Ok(Some(config.message))
    } else {
        Ok(None)
    }
}

fn fetch_network_time(url: &str) -> Result<DateTime<Utc>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let response = client.get(url).send()?;
    let date_header = response
        .headers()
        .get(DATE)
        .ok_or_else(|| anyhow!(t!("errors.network_time_missing")))?
        .to_str()?;
    Ok(DateTime::parse_from_rfc2822(date_header)?.with_timezone(&Utc))
}

fn launcher_update_browser_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(LAUNCHER_UPDATE_BROWSER_UA),
    );
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/octet-stream,application/json,text/plain,*/*;q=0.8"),
    );
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static("zh-CN,zh;q=0.9,en;q=0.8"),
    );
    headers
}

fn launcher_update_http_client(timeout: Option<Duration>) -> Result<Client> {
    let mut builder = Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .no_proxy()
        .default_headers(launcher_update_browser_headers());
    builder = match timeout {
        Some(timeout) => builder.timeout(timeout),
        None => builder.timeout(None),
    };
    Ok(builder.build()?)
}

fn launcher_version_is_mini(version: &str) -> bool {
    version.strip_prefix('v').unwrap_or(version) == MINI_LAUNCHER_VERSION
}

fn check_launcher_update_and_restart(mut status_updater: impl FnMut(SplashUpdate)) -> Result<bool> {
    if std::env::var_os(LAUNCHER_UPDATE_SKIP_ENV).is_some() {
        info!("Skipping launcher update check after restart");
        std::env::remove_var(LAUNCHER_UPDATE_SKIP_ENV);
        return Ok(false);
    }

    let current_version = env!("CARGO_PKG_VERSION");
    let mini_launcher = launcher_version_is_mini(current_version);
    let platform_key = launcher_update_platform_key();
    let manifest_client = match launcher_update_http_client(Some(Duration::from_secs(10))) {
        Ok(client) => client,
        Err(err) => {
            warn!("Unable to create launcher update client: {err:#}");
            if mini_launcher {
                return Err(anyhow!(t!(
                    "launcher_update.mini_check_failed",
                    error = format!("{err:#}")
                )));
            }
            return Ok(false);
        }
    };
    let manifest_text = match (|| -> Result<String> {
        Ok(manifest_client
            .get(LAUNCHER_UPDATE_URL)
            .send()?
            .error_for_status()?
            .text()?)
    })() {
        Ok(text) => text,
        Err(err) => {
            warn!("Unable to fetch launcher update manifest: {err:#}");
            if mini_launcher {
                return Err(anyhow!(t!(
                    "launcher_update.mini_check_failed",
                    error = format!("{err:#}")
                )));
            }
            return Ok(false);
        }
    };
    let manifest: LauncherUpdateManifest = match serde_json::from_str(&manifest_text) {
        Ok(manifest) => manifest,
        Err(err) => {
            warn!("Unable to parse launcher update manifest: {err:#}");
            if mini_launcher {
                return Err(anyhow!(t!(
                    "launcher_update.mini_check_failed",
                    error = format!("{err:#}")
                )));
            }
            return Ok(false);
        }
    };
    if !launcher_version_is_newer(current_version, &manifest.version) {
        info!(
            "Launcher is up to date: current={}, latest={}",
            current_version, manifest.version
        );
        if mini_launcher {
            return Err(anyhow!(t!(
                "launcher_update.mini_update_missing",
                current = current_version,
                latest = manifest.version
            )));
        }
        return Ok(false);
    }

    let Some(platform) = manifest.platforms.get(platform_key) else {
        warn!("No launcher update payload for platform {platform_key}");
        if mini_launcher {
            return Err(anyhow!(t!(
                "launcher_update.mini_payload_missing",
                platform = platform_key
            )));
        }
        return Ok(false);
    };

    info!(
        "Launcher update available: {} -> {}",
        current_version, manifest.version
    );
    status_updater(
        SplashUpdate::loading(
            t!("launcher_update.updating"),
            t!(
                "launcher_update.available_detail",
                version = manifest.version.clone()
            ),
            8,
        )
        .with_subtitle(t!("launcher_update.status")),
    );

    let current_exe = std::env::current_exe()?;
    let update_path = launcher_update_temp_path(&current_exe);
    if let Err(err) = download_launcher_update(
        &platform.url,
        &update_path,
        &platform.sha256,
        &mut status_updater,
    ) {
        warn!("Launcher update download failed: {err:#}");
        if mini_launcher {
            return Err(err);
        }
        return Ok(false);
    }
    make_executable(&update_path)?;
    status_updater(
        SplashUpdate::loading(
            t!("launcher_update.restart_title"),
            t!("launcher_update.restarting_detail"),
            100,
        )
        .with_subtitle(t!("launcher_update.restart_status")),
    );
    if let Err(err) = replace_launcher_and_restart(&current_exe, &update_path) {
        warn!("Launcher update replacement failed: {err:#}");
        if mini_launcher {
            return Err(err);
        }
        return Ok(false);
    }
    Ok(true)
}

fn download_launcher_update(
    url: &str,
    update_path: &Path,
    expected_sha256: &str,
    mut status_updater: impl FnMut(SplashUpdate),
) -> Result<()> {
    let client = launcher_update_http_client(None)?;
    info!("Downloading launcher update from {url}");
    let probe = launcher_update_probe(&client, url);
    let downloaded = if let Some(total_bytes) = probe.total_bytes {
        if total_bytes >= LAUNCHER_UPDATE_MIN_PARALLEL_BYTES && probe.supports_ranges {
            match download_launcher_update_parallel(
                &client,
                url,
                update_path,
                total_bytes,
                &mut status_updater,
            ) {
                Ok(downloaded) => downloaded,
                Err(err) => {
                    warn!(
                        "Parallel launcher update download failed, falling back to sequential: {err:#}"
                    );
                    let _ = fs::remove_file(update_path);
                    download_launcher_update_sequential(
                        &client,
                        url,
                        update_path,
                        Some(total_bytes),
                        &mut status_updater,
                    )?
                }
            }
        } else {
            download_launcher_update_sequential(
                &client,
                url,
                update_path,
                Some(total_bytes),
                &mut status_updater,
            )?
        }
    } else {
        download_launcher_update_sequential(&client, url, update_path, None, &mut status_updater)?
    };

    status_updater(
        SplashUpdate::loading(
            t!("launcher_update.updating"),
            t!("launcher_update.verifying_detail"),
            92,
        )
        .with_subtitle(t!("launcher_update.status")),
    );

    let digest_hex = sha256_file(update_path)?;
    if !digest_hex.eq_ignore_ascii_case(expected_sha256) {
        let _ = fs::remove_file(update_path);
        return Err(anyhow!(
            "launcher update sha256 mismatch: expected {}, got {}",
            expected_sha256,
            digest_hex
        ));
    }

    info!(
        "Launcher update downloaded: {} bytes -> {}",
        downloaded,
        update_path.display()
    );
    Ok(())
}

fn launcher_update_probe(client: &Client, url: &str) -> LauncherUpdateProbe {
    let head_total = launcher_update_head_content_length(client, url);
    let range_probe = launcher_update_range_probe(client, url);

    LauncherUpdateProbe {
        total_bytes: head_total.or(range_probe.total_bytes),
        supports_ranges: range_probe.supports_ranges,
    }
}

fn launcher_update_head_content_length(client: &Client, url: &str) -> Option<u64> {
    let response = match client.head(url).send() {
        Ok(response) => response,
        Err(err) => {
            warn!("Unable to probe launcher update size with HEAD: {err:#}");
            return None;
        }
    };

    if !response.status().is_success() {
        warn!(
            "Launcher update HEAD probe returned unexpected status: {}",
            response.status()
        );
        return None;
    }

    response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
}

fn launcher_update_range_probe(client: &Client, url: &str) -> LauncherUpdateProbe {
    let response = match client.get(url).header(RANGE, "bytes=0-0").send() {
        Ok(response) => response,
        Err(err) => {
            warn!("Unable to probe launcher update range support: {err:#}");
            return LauncherUpdateProbe {
                total_bytes: None,
                supports_ranges: false,
            };
        }
    };

    if response.status() != StatusCode::PARTIAL_CONTENT {
        warn!(
            "Launcher update range probe returned unexpected status: {}",
            response.status()
        );
        return LauncherUpdateProbe {
            total_bytes: None,
            supports_ranges: false,
        };
    }

    let total_bytes = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(content_range_total_bytes);

    LauncherUpdateProbe {
        total_bytes,
        supports_ranges: true,
    }
}

fn content_range_total_bytes(value: &str) -> Option<u64> {
    value
        .rsplit_once('/')
        .and_then(|(_, total)| total.trim().parse::<u64>().ok())
        .filter(|total| *total > 0)
}

fn download_launcher_update_sequential(
    client: &Client,
    url: &str,
    update_path: &Path,
    expected_total_bytes: Option<u64>,
    mut status_updater: impl FnMut(SplashUpdate),
) -> Result<u64> {
    let mut response = client.get(url).send()?.error_for_status()?;
    let total_bytes = expected_total_bytes.or_else(|| response.content_length());
    let mut file = fs::File::create(update_path).with_context(|| {
        t!(
            "errors.write_update_failed",
            error = update_path.display().to_string()
        )
    })?;
    let mut downloaded = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    let mut last_reported_progress = 8u8;
    let mut last_reported_at = Instant::now() - Duration::from_secs(1);
    let download_started_at = Instant::now();

    loop {
        let size = response
            .read(&mut buffer)
            .with_context(|| t!("errors.download_update_failed", url = url))?;
        if size == 0 {
            break;
        }
        file.write_all(&buffer[..size]).with_context(|| {
            t!(
                "errors.write_update_failed",
                error = update_path.display().to_string()
            )
        })?;
        downloaded += size as u64;

        let (progress, detail) =
            launcher_download_progress_detail(downloaded, total_bytes, download_started_at);
        if progress > last_reported_progress
            || last_reported_at.elapsed() >= Duration::from_millis(250)
        {
            last_reported_progress = progress;
            last_reported_at = Instant::now();
            status_updater(
                SplashUpdate::loading(t!("launcher_update.updating"), detail, progress)
                    .with_subtitle(t!("launcher_update.status")),
            );
        }
    }
    file.flush().with_context(|| {
        t!(
            "errors.write_update_failed",
            error = update_path.display().to_string()
        )
    })?;

    if let Some(total_bytes) = total_bytes {
        if downloaded != total_bytes {
            return Err(anyhow!(
                "launcher update download incomplete: expected {} bytes, got {} bytes",
                total_bytes,
                downloaded
            ));
        }
    }

    Ok(downloaded)
}

fn download_launcher_update_parallel(
    client: &Client,
    url: &str,
    update_path: &Path,
    total_bytes: u64,
    status_updater: &mut impl FnMut(SplashUpdate),
) -> Result<u64> {
    let worker_limit = launcher_update_parallel_threads();
    let worker_count = launcher_update_worker_count(total_bytes, worker_limit);
    let file = fs::File::create(update_path).with_context(|| {
        t!(
            "errors.write_update_failed",
            error = update_path.display().to_string()
        )
    })?;
    file.set_len(total_bytes).with_context(|| {
        t!(
            "errors.write_update_failed",
            error = update_path.display().to_string()
        )
    })?;
    drop(file);

    info!(
        "Downloading launcher update with {} dynamic range workers",
        worker_count
    );
    let next_start = Arc::new(AtomicU64::new(0));
    let downloaded = Arc::new(AtomicU64::new(0));
    let cancel_requested = Arc::new(AtomicBool::new(false));
    let started_at = Instant::now();
    let (result_tx, result_rx) = mpsc::channel::<Result<()>>();

    thread::scope(|scope| {
        for index in 0..worker_count {
            let client = client.clone();
            let url = url.to_owned();
            let update_path = update_path.to_path_buf();
            let next_start = next_start.clone();
            let downloaded = downloaded.clone();
            let cancel_requested = cancel_requested.clone();
            let result_tx = result_tx.clone();
            scope.spawn(move || {
                let result = download_launcher_update_worker(
                    &client,
                    &url,
                    &update_path,
                    total_bytes,
                    worker_count,
                    &next_start,
                    &downloaded,
                    &cancel_requested,
                )
                .with_context(|| format!("launcher update dynamic worker {} failed", index + 1));
                let _ = result_tx.send(result);
            });
        }
        drop(result_tx);
        monitor_launcher_parallel_download(
            total_bytes,
            &downloaded,
            &cancel_requested,
            started_at,
            worker_count,
            &result_rx,
            status_updater,
        )
    })
}

fn download_launcher_update_worker(
    client: &Client,
    url: &str,
    update_path: &Path,
    total_bytes: u64,
    worker_count: usize,
    next_start: &AtomicU64,
    downloaded: &AtomicU64,
    cancel_requested: &AtomicBool,
) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(update_path)
        .with_context(|| {
            t!(
                "errors.write_update_failed",
                error = update_path.display().to_string()
            )
        })?;
    let mut buffer = [0u8; 128 * 1024];

    while !cancel_requested.load(Ordering::Relaxed) {
        let Some((start, end)) =
            launcher_update_next_range(total_bytes, worker_count, next_start, downloaded)
        else {
            break;
        };
        download_launcher_update_range(
            client,
            url,
            &mut file,
            start,
            end,
            downloaded,
            cancel_requested,
            &mut buffer,
        )
        .with_context(|| format!("range bytes={start}-{end} failed"))?;
    }

    Ok(())
}

fn launcher_update_next_range(
    total_bytes: u64,
    worker_count: usize,
    next_start: &AtomicU64,
    downloaded: &AtomicU64,
) -> Option<(u64, u64)> {
    loop {
        let start = next_start.load(Ordering::Relaxed);
        if start >= total_bytes {
            return None;
        }

        let chunk_size = launcher_update_chunk_size(
            total_bytes,
            start,
            downloaded.load(Ordering::Relaxed),
            worker_count,
        );
        let end = (start + chunk_size - 1).min(total_bytes - 1);
        let next = end + 1;

        if next_start
            .compare_exchange(start, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return Some((start, end));
        }
    }
}

fn launcher_update_chunk_size(
    total_bytes: u64,
    next_start: u64,
    downloaded: u64,
    worker_count: usize,
) -> u64 {
    let remaining_unassigned = total_bytes.saturating_sub(next_start);
    if remaining_unassigned <= LAUNCHER_UPDATE_MIN_CHUNK_BYTES {
        return remaining_unassigned.max(1);
    }

    let remaining_download = total_bytes.saturating_sub(downloaded);
    let target_chunks = (worker_count as u64).saturating_mul(2).max(1);
    let adaptive = remaining_download.div_ceil(target_chunks).clamp(
        LAUNCHER_UPDATE_MIN_CHUNK_BYTES,
        LAUNCHER_UPDATE_MAX_CHUNK_BYTES,
    );

    adaptive.min(remaining_unassigned).max(1)
}

fn download_launcher_update_range(
    client: &Client,
    url: &str,
    file: &mut fs::File,
    start: u64,
    end: u64,
    downloaded: &AtomicU64,
    cancel_requested: &AtomicBool,
    buffer: &mut [u8],
) -> Result<()> {
    let range = format!("bytes={start}-{end}");
    let mut response = client
        .get(url)
        .header(RANGE, range)
        .send()
        .with_context(|| t!("errors.download_update_failed", url = url))?;

    if response.status() != StatusCode::PARTIAL_CONTENT {
        return Err(anyhow!(
            "range request returned unexpected status: {}",
            response.status()
        ));
    }

    file.seek(SeekFrom::Start(start))
        .with_context(|| t!("errors.write_update_failed", error = start.to_string()))?;

    let expected_len = end - start + 1;
    let mut written = 0u64;
    while written < expected_len && !cancel_requested.load(Ordering::Relaxed) {
        let size = response
            .read(buffer)
            .with_context(|| t!("errors.download_update_failed", url = url))?;
        if size == 0 {
            break;
        }

        let remaining = (expected_len - written) as usize;
        if size > remaining {
            return Err(anyhow!(
                "range response exceeded expected length: expected {} bytes, got at least {} bytes",
                expected_len,
                written + size as u64
            ));
        }

        file.write_all(&buffer[..size])
            .with_context(|| t!("errors.write_update_failed", error = start.to_string()))?;
        written += size as u64;
        downloaded.fetch_add(size as u64, Ordering::Relaxed);
    }

    if cancel_requested.load(Ordering::Relaxed) {
        return Ok(());
    }

    if written != expected_len {
        return Err(anyhow!(
            "range response incomplete: expected {} bytes, got {} bytes",
            expected_len,
            written
        ));
    }

    Ok(())
}

fn monitor_launcher_parallel_download(
    total_bytes: u64,
    downloaded: &AtomicU64,
    cancel_requested: &AtomicBool,
    started_at: Instant,
    worker_count: usize,
    result_rx: &mpsc::Receiver<Result<()>>,
    status_updater: &mut impl FnMut(SplashUpdate),
) -> Result<u64> {
    let mut finished_workers = 0usize;
    let mut last_reported_progress = 8u8;
    let mut last_reported_at = Instant::now() - Duration::from_secs(1);

    while finished_workers < worker_count {
        match result_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(())) => finished_workers += 1,
            Ok(Err(err)) => {
                cancel_requested.store(true, Ordering::Relaxed);
                return Err(err);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                cancel_requested.store(true, Ordering::Relaxed);
                return Err(anyhow!("launcher update worker channel disconnected"));
            }
        }

        let current = downloaded.load(Ordering::Relaxed).min(total_bytes);
        let (progress, detail) =
            launcher_download_progress_detail(current, Some(total_bytes), started_at);
        if progress > last_reported_progress
            || last_reported_at.elapsed() >= Duration::from_millis(250)
        {
            last_reported_progress = progress;
            last_reported_at = Instant::now();
            status_updater(
                SplashUpdate::loading(t!("launcher_update.updating"), detail, progress)
                    .with_subtitle(t!("launcher_update.status")),
            );
        }
    }

    let downloaded = downloaded.load(Ordering::Relaxed);
    if downloaded != total_bytes {
        return Err(anyhow!(
            "launcher update download incomplete: expected {} bytes, got {} bytes",
            total_bytes,
            downloaded
        ));
    }

    Ok(downloaded)
}

fn launcher_update_parallel_threads() -> usize {
    thread::available_parallelism()
        .map(|parallelism| parallelism.get().saturating_mul(2))
        .unwrap_or(16)
        .clamp(16, 128)
}

fn launcher_update_worker_count(total_bytes: u64, max_workers: usize) -> usize {
    total_bytes
        .div_ceil(LAUNCHER_UPDATE_MIN_CHUNK_BYTES)
        .max(1)
        .min(max_workers as u64) as usize
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];

    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        hasher.update(&buffer[..size]);
    }

    let digest = hasher.finalize();
    Ok(bytes_to_hex(&digest))
}

fn launcher_download_progress_detail(
    downloaded: u64,
    total_bytes: Option<u64>,
    started_at: Instant,
) -> (u8, String) {
    let speed = format_speed(download_speed_bytes_per_second(downloaded, started_at));
    if let Some(total) = total_bytes.filter(|total| *total > 0) {
        let percentage = ((downloaded.min(total) * 100) / total) as u8;
        let detail = t!(
            "launcher_update.downloading_detail",
            downloaded = format_bytes(downloaded),
            total = format_bytes(total),
            percent = percentage.to_string(),
            speed = speed
        )
        .to_string();
        return (percentage, detail);
    }

    let mib_downloaded = downloaded / (1024 * 1024);
    let progress = (12 + mib_downloaded.min(76) as u8).min(88);
    let detail = t!(
        "launcher_update.downloading_detail_unknown",
        downloaded = format_bytes(downloaded),
        speed = speed
    )
    .to_string();
    (progress, detail)
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;

    let bytes_f = bytes as f64;
    if bytes_f >= GIB {
        format!("{:.1} GiB", bytes_f / GIB)
    } else if bytes_f >= MIB {
        format!("{:.1} MiB", bytes_f / MIB)
    } else if bytes_f >= KIB {
        format!("{:.1} KiB", bytes_f / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn download_speed_bytes_per_second(downloaded: u64, started_at: Instant) -> f64 {
    let elapsed = started_at.elapsed().as_secs_f64().max(0.1);
    downloaded as f64 / elapsed
}

fn format_speed(bytes_per_second: f64) -> String {
    format_bytes(bytes_per_second.max(0.0).round() as u64)
}

fn launcher_update_platform_key() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-aarch64",
        ("macos", "x86_64") => "darwin-x86_64",
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("windows", "x86_64") => "windows-x86_64",
        ("windows", "x86") => "windows-i686",
        ("windows", "aarch64") => "windows-aarch64",
        _ => "unknown",
    }
}

fn launcher_version_is_newer(current: &str, latest: &str) -> bool {
    let current = parse_launcher_version(current);
    let latest = parse_launcher_version(latest);
    latest > current
}

fn parse_launcher_version(version: &str) -> (u64, u64, u64, u64) {
    let version = version.strip_prefix('v').unwrap_or(version);
    let (core, suffix) = version.split_once('-').unwrap_or((version, ""));
    let mut nums = core.split('.').map(|part| part.parse::<u64>().unwrap_or(0));
    let major = nums.next().unwrap_or(0);
    let minor = nums.next().unwrap_or(0);
    let patch = nums.next().unwrap_or(0);
    let suffix_rank = suffix
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>()
        .parse::<u64>()
        .unwrap_or(0);
    (major, minor, patch, suffix_rank)
}

fn launcher_arg_present(flags: &[&str]) -> bool {
    std::env::args().skip(1).any(|arg| {
        let arg = arg.to_ascii_lowercase();
        flags.iter().any(|flag| arg == *flag)
    })
}

fn preview_no_update_arg_present() -> bool {
    launcher_arg_present(PREVIEW_NO_UPDATE_ARGS)
}

fn preview_crash_arg_present() -> bool {
    launcher_arg_present(PREVIEW_CRASH_ARGS)
}

fn launcher_update_temp_path(current_exe: &Path) -> PathBuf {
    let file_name = current_exe
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("alas-launcher");
    std::env::temp_dir().join(format!(
        "azurpilot-launcher-update-{}-{file_name}",
        std::process::id()
    ))
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_launcher_and_restart(current_exe: &Path, update_path: &Path) -> Result<()> {
    fs::rename(update_path, current_exe).with_context(|| {
        t!(
            "errors.replace_launcher_failed",
            error = current_exe.display().to_string()
        )
    })?;
    Command::new(current_exe)
        .env(LAUNCHER_UPDATE_SKIP_ENV, "1")
        .spawn()
        .with_context(|| {
            t!(
                "errors.restart_launcher_failed",
                error = current_exe.display().to_string()
            )
        })?;
    Ok(())
}

#[cfg(windows)]
fn replace_launcher_and_restart(current_exe: &Path, update_path: &Path) -> Result<()> {
    let helper_path = std::env::temp_dir().join(format!(
        "azurpilot-launcher-update-helper-{}.exe",
        std::process::id()
    ));

    fs::copy(current_exe, &helper_path).with_context(|| {
        t!(
            "errors.copy_file_failed",
            src = current_exe.display().to_string(),
            dest = helper_path.display().to_string()
        )
    })?;

    use std::os::windows::process::CommandExt;
    use winapi::um::winbase::CREATE_NO_WINDOW;
    Command::new(&helper_path)
        .arg(LAUNCHER_UPDATE_APPLY_ARG)
        .arg(current_exe)
        .arg(update_path)
        .env(LAUNCHER_UPDATE_SKIP_ENV, "1")
        .env(LAUNCHER_UPDATE_NO_CONSOLE_ENV, "1")
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .with_context(|| {
            t!(
                "errors.start_update_script_failed",
                error = helper_path.display().to_string()
            )
        })?;
    Ok(())
}

#[cfg(windows)]
fn try_apply_launcher_update_from_args() -> Result<bool> {
    use std::ffi::OsStr;

    let mut args = std::env::args_os();
    let _ = args.next();
    let Some(mode) = args.next() else {
        return Ok(false);
    };
    if mode != OsStr::new(LAUNCHER_UPDATE_APPLY_ARG) {
        return Ok(false);
    }

    let target_path = args
        .next()
        .ok_or_else(|| anyhow!("missing launcher update target path"))?;
    let update_path = args
        .next()
        .ok_or_else(|| anyhow!("missing launcher update payload path"))?;
    apply_launcher_update_and_restart(PathBuf::from(target_path), PathBuf::from(update_path))?;
    Ok(true)
}

#[cfg(windows)]
fn apply_launcher_update_and_restart(target_path: PathBuf, update_path: PathBuf) -> Result<()> {
    let mut last_error = None;
    for _ in 0..60 {
        match move_file_replace(&update_path, &target_path) {
            Ok(()) => {
                restart_launcher_after_update(&target_path)?;
                schedule_file_delete_on_reboot(&std::env::current_exe()?);
                return Ok(());
            }
            Err(err) => {
                last_error = Some(err);
                thread::sleep(Duration::from_secs(1));
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("launcher update replacement timed out")))
}

#[cfg(windows)]
fn move_file_replace(from: &Path, to: &Path) -> Result<()> {
    use winapi::um::winbase::{
        MoveFileExW, MOVEFILE_COPY_ALLOWED, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let from_wide = path_to_wide(from);
    let to_wide = path_to_wide(to);
    let flags = MOVEFILE_REPLACE_EXISTING | MOVEFILE_COPY_ALLOWED | MOVEFILE_WRITE_THROUGH;
    let moved = unsafe { MoveFileExW(from_wide.as_ptr(), to_wide.as_ptr(), flags) };
    if moved == 0 {
        return Err(anyhow!(
            "{}: {}",
            t!(
                "errors.replace_launcher_failed",
                error = to.display().to_string()
            ),
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn restart_launcher_after_update(target_path: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    use winapi::um::winbase::CREATE_NO_WINDOW;

    Command::new(target_path)
        .env(LAUNCHER_UPDATE_SKIP_ENV, "1")
        .env(LAUNCHER_UPDATE_NO_CONSOLE_ENV, "1")
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .with_context(|| {
            t!(
                "errors.restart_launcher_failed",
                error = target_path.display().to_string()
            )
        })?;
    Ok(())
}

#[cfg(windows)]
fn schedule_file_delete_on_reboot(path: &Path) {
    use std::ptr;
    use winapi::um::winbase::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};

    let path_wide = path_to_wide(path);
    let _ = unsafe { MoveFileExW(path_wide.as_ptr(), ptr::null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
}

#[cfg(windows)]
fn path_to_wide(path: &Path) -> Vec<u16> {
    use std::{iter, os::windows::ffi::OsStrExt};

    path.as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_time_bomb_config_parses_when_enabled() {
        let section =
            cargo_toml_section("package.metadata.alas-launcher.time-bomb").expect("section exists");
        let enabled = cargo_toml_value(section, "enabled").as_deref() == Some("true");
        let config = time_bomb_config().expect("time bomb config parses");
        assert_eq!(config.is_some(), enabled);
    }

    #[test]
    fn test_cargo_toml_value_reads_time_bomb_fields() {
        let section =
            cargo_toml_section("package.metadata.alas-launcher.time-bomb").expect("section exists");
        assert!(cargo_toml_value(section, "expires-at").is_some());
        assert_eq!(
            Some("测试已结束，请安装正式版".to_owned()),
            cargo_toml_value(section, "message")
        );
    }

    #[test]
    fn test_english_splash_i18n_uses_json_literals() {
        rust_i18n::set_locale("en");

        let html = splash_redesigned_shell_html("light", "dark");

        assert!(html.contains(r#""defaultTip":"Sakura Empire's cherry blossoms"#));
        assert!(!html.contains("const defaultTip = '"));
        assert!(html.contains("window.__ALAS_SPLASH_READY = true;"));
    }
}

/// Set macOS activation policy to Regular (show in dock) or Accessory (hide from dock).
#[cfg(target_os = "macos")]
fn set_macos_activation_policy(app: &tauri::AppHandle, regular: bool) {
    let policy = if regular {
        tauri::ActivationPolicy::Regular
    } else {
        tauri::ActivationPolicy::Accessory
    };
    if let Err(e) = app.set_activation_policy(policy) {
        error!("Failed to set activation policy: {}", e);
    }
}

fn main() -> Result<()> {
    #[cfg(windows)]
    if try_apply_launcher_update_from_args()? {
        return Ok(());
    }

    #[cfg(windows)]
    unsafe {
        use crate::window_util::HAS_CONSOLE;
        use std::sync::atomic::Ordering;
        use winapi::um::wincon::{AttachConsole, ATTACH_PARENT_PROCESS};
        if std::env::var_os(LAUNCHER_UPDATE_NO_CONSOLE_ENV).is_some() {
            std::env::remove_var(LAUNCHER_UPDATE_NO_CONSOLE_ENV);
        } else {
            HAS_CONSOLE.store(AttachConsole(ATTACH_PARENT_PROCESS) != 0, Ordering::Relaxed);
        }
    }
    setup_environment()?;
    let _log_guard = initialize_logging()?;
    crate::i18n::init();
    let preview_crash = preview_crash_arg_present();
    let preview_no_update = preview_crash || preview_no_update_arg_present();

    info!("=== AzurPilot starting ===");
    info!("Launcher log file: log/{}", today_launcher_log_filename());
    if preview_no_update {
        info!("Preview no-update mode enabled; skipping launcher update check");
    }
    if preview_crash {
        info!("Preview crash mode enabled; splash will stop on an artificial error state");
    }

    let deploy_config = get_deploy_config();
    let webui_config = WebuiLaunchConfig::from_deploy_config(deploy_config.as_ref());
    if deploy_config.is_none() {
        warn!("config/deploy.yaml not found or invalid, using default WebUI launch config");
    }
    let port = webui_config.port;

    let backend = Arc::new(Mutex::new(None));
    let allow_exit = Arc::new(AtomicBool::new(false));
    let launch_blocked = Arc::new(AtomicBool::new(false));
    let setup_cancel_requested = Arc::new(AtomicBool::new(false));
    let setup_running = Arc::new(AtomicBool::new(false));
    let setup_completed = Arc::new(AtomicBool::new(false));
    let startup_cleanup_started = Arc::new(AtomicBool::new(false));
    let recreating_main_window = Arc::new(AtomicBool::new(false));
    #[cfg(windows)]
    let close_prompt_active = Arc::new(AtomicBool::new(false));

    let allow_exit_for_setup = allow_exit.clone();
    let launch_blocked_for_setup = launch_blocked.clone();
    let recreating_main_window_for_single_instance = recreating_main_window.clone();
    let recreating_main_window_for_setup = recreating_main_window.clone();
    let recreating_main_window_for_run = recreating_main_window.clone();
    let launch_blocked_for_run = launch_blocked.clone();
    #[cfg(windows)]
    let close_prompt_active_for_run = close_prompt_active.clone();

    info!("Starting Webview...");
    tauri::Builder::default()
        .register_uri_scheme_protocol("alas-error", |_ctx, request| {
            backend_error_response(request)
        })
        .register_uri_scheme_protocol("alas-splash", |_ctx, _request| splash_response())
        .invoke_handler(tauri::generate_handler![
            save_as,
            download_today_gui_log,
            download_today_launcher_log,
            retry_backend_connection,
            window_hide,
            window_minimize,
            window_toggle_maximize,
            window_close,
            window_start_dragging,
            window_is_maximized
        ])
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_single_instance::init(
            move |app, _argv, _cwd| {
                restore_main_window_from_tray(
                    app,
                    port,
                    recreating_main_window_for_single_instance.clone(),
                );
            },
        ))
        .setup(move |app| {
            match time_bomb_expiration_message() {
                Ok(Some(message)) => {
                    launch_blocked_for_setup.store(true, Ordering::SeqCst);
                    allow_exit_for_setup.store(true, Ordering::SeqCst);
                    let app_handle = app.handle().clone();
                    app.dialog()
                        .message(message)
                        .title(t!("dialog.test_ended"))
                        .show(move |_| {
                            app_handle.exit(0);
                        });
                    return Ok(());
                }
                Ok(None) => {}
                Err(err) => {
                    warn!("Unable to verify test expiration time: {:?}", err);
                }
            }

            create_main_window(&app.handle(), port)?;

            // Windows and macOS: create system tray
            #[cfg(any(windows, target_os = "macos"))]
            {
                info!("Creating system tray...");
                let allow_exit = allow_exit_for_setup.clone();
                let recreating_main_window_for_menu = recreating_main_window_for_setup.clone();
                #[cfg(windows)]
                let recreating_main_window_for_tray = recreating_main_window_for_setup.clone();
                let show_item = MenuItemBuilder::new(t!("tray.toggle_visibility"))
                    .id("toggle_visibility")
                    .build(app)?;
                let quit_item = MenuItemBuilder::new(t!("tray.quit"))
                    .id("quit")
                    .build(app)?;
                let tray_menu = MenuBuilder::new(app)
                    .item(&show_item)
                    .separator()
                    .item(&quit_item)
                    .build()?;

                info!("Tray menu created successfully");

                // Use embedded icon bytes so packaged apps always load the tray icon correctly.
                let icon = tray_icon_for_platform();

                info!("Building tray icon...");
                let mut tray_builder = TrayIconBuilder::with_id("main-tray")
                    .icon(icon)
                    .tooltip("AzurPilot")
                    .menu(&tray_menu);

                // On Windows, show menu on right click
                #[cfg(windows)]
                {
                    tray_builder = tray_builder.show_menu_on_left_click(false);
                }

                // On macOS, show menu on left click
                #[cfg(target_os = "macos")]
                {
                    info!("Setting macOS tray to show menu on left click");
                    tray_builder = tray_builder.show_menu_on_left_click(true);
                }

                match tray_builder
                    .on_menu_event(move |app, event| {
                        debug!("Tray menu event: {:?}", event.id());
                        match event.id().as_ref() {
                            "toggle_visibility" => {
                                toggle_main_window_visibility(
                                    app,
                                    port,
                                    recreating_main_window_for_menu.clone(),
                                );
                            }
                            "quit" => {
                                allow_exit.store(true, Ordering::SeqCst);
                                app.exit(0);
                            }
                            _ => {}
                        }
                    })
                    .on_tray_icon_event(move |tray, event| {
                        #[cfg(windows)]
                        if let tauri::tray::TrayIconEvent::Click {
                            button: tauri::tray::MouseButton::Left,
                            button_state: tauri::tray::MouseButtonState::Up,
                            ..
                        } = event
                        {
                            let app = tray.app_handle();
                            toggle_main_window_visibility(
                                &app,
                                port,
                                recreating_main_window_for_tray.clone(),
                            );
                        }

                        #[cfg(target_os = "macos")]
                        {
                            let _ = tray;
                            let _ = event;
                        }
                    })
                    .build(app)
                {
                    Ok(_) => {
                        info!("System tray created successfully!");
                    }
                    Err(e) => {
                        error!("Failed to create system tray: {:?}", e);
                        return Err(Box::new(e));
                    }
                }
            }

            Ok(())
        })
        .build(tauri::generate_context!())?
        .run(move |app_handle, event| {
            match event {
                tauri::RunEvent::Ready => {
                    if launch_blocked_for_run.load(Ordering::SeqCst) {
                        debug!("Launch blocked by test expiration");
                        return;
                    }

                    debug!("RunEvent::Ready");
                    let allow_exit = allow_exit.clone();
                    let allow_exit_for_ctrlc = allow_exit.clone();
                    let handle1 = app_handle.clone();
                    ctrlc::set_handler(move || {
                        allow_exit_for_ctrlc.store(true, Ordering::SeqCst);
                        handle1.exit(0);
                    })
                    .expect("Error setting Ctrl-C handler");
                    let app_handle = app_handle.clone();
                    let backend = backend.clone();
                    let webui_config = webui_config.clone();
                    let setup_cancel_requested = setup_cancel_requested.clone();
                    let setup_running = setup_running.clone();
                    let setup_completed = setup_completed.clone();
                    let recreating_main_window_for_notify = recreating_main_window_for_run.clone();
                    thread::spawn(move || {
                        setup_running.store(true, Ordering::SeqCst);
                        let splash = app_handle.get_webview_window("splash").unwrap();
                        initialize_splash(&splash);
                        let last_progress = Cell::new(0u8);
                        let mut status_updater = |mut update: SplashUpdate| {
                            update.progress = update.progress.max(last_progress.get());
                            last_progress.set(update.progress);
                            update_splash(&splash, &update);
                        };

                        status_updater(
                            SplashUpdate::loading(
                                t!("splash.starting"),
                                t!("splash.webui_init"),
                                4,
                            )
                            .with_subtitle(format!(
                                "{} | Tips:{}",
                                t!("splash.initializing"),
                                crate::setup::get_tip()
                            )),
                        );

                        if !preview_no_update {
                            let launcher_progress = Cell::new(0u8);
                            let mut launcher_status_updater = |mut update: SplashUpdate| {
                                update.progress = update.progress.max(launcher_progress.get());
                                launcher_progress.set(update.progress);
                                update_splash(&splash, &update);
                            };

                            match check_launcher_update_and_restart(&mut launcher_status_updater) {
                                Ok(true) => {
                                    info!("Launcher update installed, restarting");
                                    setup_completed.store(true, Ordering::SeqCst);
                                    setup_running.store(false, Ordering::SeqCst);
                                    allow_exit.store(true, Ordering::SeqCst);
                                    app_handle.exit(0);
                                    return;
                                }
                                Ok(false) => {}
                                Err(e) => {
                                    warn!("Required launcher update failed: {e:#}");
                                    launcher_status_updater(SplashUpdate::error(
                                        t!("launcher_update.failed"),
                                        t!(
                                            "launcher_update.failed_detail",
                                            error = format!("{e:#}")
                                        ),
                                        launcher_progress.get().max(8),
                                    ));
                                    setup_completed.store(true, Ordering::SeqCst);
                                    setup_running.store(false, Ordering::SeqCst);
                                    return;
                                }
                            }
                        }

                        if preview_crash {
                            status_updater(
                                SplashUpdate::error(
                                    t!("dialog.startup_failed"),
                                    t!("splash.preview_crash_detail"),
                                    42,
                                )
                                .with_subtitle(format!(
                                    "{} | Tips：{}",
                                    t!("splash.preview_crash_mode"),
                                    crate::setup::get_tip()
                                )),
                            );
                            setup_completed.store(true, Ordering::SeqCst);
                            setup_running.store(false, Ordering::SeqCst);
                            return;
                        }
                        if let Err(e) = setup_alas_repo(
                            &mut status_updater,
                            setup_cancel_requested.clone(),
                            preview_no_update,
                        ) {
                            error!("{e}");
                            setup_running.store(false, Ordering::SeqCst);
                            if setup_cancel_requested.load(Ordering::SeqCst) {
                                return;
                            }
                            status_updater(SplashUpdate::error(
                                t!("dialog.startup_failed"),
                                t!("dialog.repo_setup_failed", error = e.to_string()),
                                last_progress.get().max(8),
                            ));
                            return;
                        }
                        info!("Starting gui.py on http://127.0.0.1:{}/", port);
                        status_updater(
                            SplashUpdate::loading(
                                t!("splash.starting"),
                                t!("splash.webui_init_slow"),
                                97,
                            )
                            .with_subtitle(format!(
                                "{} | Tips:{}",
                                t!("splash.starting_backend"),
                                crate::setup::get_tip()
                            )),
                        );
                        let b = match ManagedBackend::new(&webui_config) {
                            Ok(backend) => backend,
                            Err(e) => {
                                error!("{e}");
                                setup_running.store(false, Ordering::SeqCst);
                                status_updater(SplashUpdate::error(
                                    t!("dialog.startup_failed"),
                                    t!("dialog.backend_launch_failed", error = e.to_string()),
                                    last_progress.get().max(97),
                                ));
                                return;
                            }
                        };
                        *backend.lock().unwrap() = Some(b);
                        let notification_click: NotificationClickHandler = {
                            let app_handle = app_handle.clone();
                            let recreating_main_window = recreating_main_window_for_notify.clone();
                            Arc::new(move || {
                                restore_main_window_from_any_thread(
                                    app_handle.clone(),
                                    port,
                                    recreating_main_window.clone(),
                                );
                            })
                        };
                        start_notify_stream(
                            app_handle.clone(),
                            port,
                            allow_exit.clone(),
                            notification_click,
                        );
                        status_updater(
                            SplashUpdate::loading(t!("splash.opening"), t!("splash.ready"), 100)
                                .with_subtitle(format!(
                                    "{} | Tips:{}",
                                    t!("splash.startup_complete"),
                                    crate::setup::get_tip()
                                )),
                        );
                        let _ = splash.destroy();
                        debug!("Destroyed splash window after startup");

                        info!("Webview is ready");
                        let window = app_handle.get_webview_window("main").unwrap();
                        window.set_resizable(true).unwrap();
                        if let Err(e) = navigate_backend_or_error(&window, port) {
                            error!("Failed to navigate main window: {:?}", e);
                        }
                        reveal_window(&window).unwrap();
                        setup_completed.store(true, Ordering::SeqCst);
                        setup_running.store(false, Ordering::SeqCst);
                    });
                }
                tauri::RunEvent::ExitRequested { api, .. } => {
                    if !setup_completed.load(Ordering::SeqCst)
                        && !startup_cleanup_started.load(Ordering::SeqCst)
                    {
                        api.prevent_exit();
                        begin_startup_cleanup(
                            app_handle.clone(),
                            allow_exit.clone(),
                            setup_cancel_requested.clone(),
                            setup_running.clone(),
                            startup_cleanup_started.clone(),
                        );
                        return;
                    }

                    let should_allow = allow_exit.load(Ordering::SeqCst);
                    debug!("ExitRequested event: allow_exit={}", should_allow);

                    // Only exit if explicitly allowed (e.g., via tray menu Quit)
                    if !should_allow {
                        api.prevent_exit();
                        debug!("Minimizing main window to tray");
                        minimize_main_window_to_tray(&app_handle);
                        return;
                    }

                    debug!("allow_exit is TRUE, proceeding with app shutdown");
                    info!("App exit allowed, shutting down backend...");
                    if let Some(ref mut b) = *backend.lock().unwrap() {
                        if let Err(e) = b.terminate() {
                            warn!("Failed to terminate backend process: {:?}", e);
                        }
                    }
                }
                #[cfg(target_os = "macos")]
                tauri::RunEvent::Reopen { .. } => {
                    restore_main_window_from_any_thread(
                        app_handle.clone(),
                        port,
                        recreating_main_window_for_run.clone(),
                    );
                }
                tauri::RunEvent::WindowEvent {
                    label,
                    event: tauri::WindowEvent::CloseRequested { ref api, .. },
                    ..
                } => {
                    debug!("Window {} close requested", label);

                    if label == "splash" && !setup_completed.load(Ordering::SeqCst) {
                        api.prevent_close();
                        begin_startup_cleanup(
                            app_handle.clone(),
                            allow_exit.clone(),
                            setup_cancel_requested.clone(),
                            setup_running.clone(),
                            startup_cleanup_started.clone(),
                        );
                        return;
                    }

                    if label == "splash" && !allow_exit.load(Ordering::SeqCst) {
                        api.prevent_close();
                        allow_exit.store(true, Ordering::SeqCst);
                        app_handle.exit(0);
                        return;
                    }

                    // Windows: ask whether to quit or minimize to tray.
                    #[cfg(windows)]
                    {
                        if label == "main" && !allow_exit.load(Ordering::SeqCst) {
                            api.prevent_close();
                            if close_prompt_active_for_run
                                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                                .is_ok()
                            {
                                let app_handle_for_dialog = app_handle.clone();
                                let allow_exit_for_dialog = allow_exit.clone();
                                let close_prompt_active_for_dialog =
                                    close_prompt_active_for_run.clone();

                                if let Some(main_window) = app_handle.get_webview_window("main") {
                                    app_handle
                                        .dialog()
                                        .message(t!("dialog.confirm_exit"))
                                        .title(t!("dialog.exit"))
                                        .buttons(MessageDialogButtons::OkCancelCustom(
                                            t!("dialog.exit").to_string(),
                                            t!("dialog.minimize_to_tray").to_string(),
                                        ))
                                        .parent(&main_window)
                                        .show(move |should_exit| {
                                            close_prompt_active_for_dialog
                                                .store(false, Ordering::SeqCst);
                                            if should_exit {
                                                allow_exit_for_dialog.store(true, Ordering::SeqCst);
                                                app_handle_for_dialog.exit(0);
                                            } else {
                                                minimize_main_window_to_tray(
                                                    &app_handle_for_dialog,
                                                );
                                            }
                                        });
                                } else {
                                    close_prompt_active_for_run.store(false, Ordering::SeqCst);
                                    minimize_main_window_to_tray(&app_handle);
                                }
                            }
                            return;
                        }
                    }

                    // macOS: switch to Accessory policy so the app does not terminate
                    // when no Regular windows are visible.
                    #[cfg(target_os = "macos")]
                    {
                        if label == "main" && !allow_exit.load(Ordering::SeqCst) {
                            api.prevent_close();
                            minimize_main_window_to_tray(&app_handle);
                            return;
                        }
                    }

                    // Linux: just hide to tray
                    #[cfg(target_os = "linux")]
                    {
                        if label == "main" && !allow_exit.load(Ordering::SeqCst) {
                            api.prevent_close();
                            minimize_main_window_to_tray(&app_handle);
                            return;
                        }
                    }
                }

                _ => {}
            };
        });
    Ok(())
}

fn initialize_logging() -> Result<WorkerGuard> {
    fs::create_dir_all("log")?;
    let log_filename = today_launcher_log_filename();
    let file_appender = tracing_appender::rolling::never("log", log_filename);
    let (non_blocking_file, guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking_file)
        .with_ansi(false)
        .with_target(false)
        .with_filter(tracing::level_filters::LevelFilter::DEBUG);
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(tracing::level_filters::LevelFilter::DEBUG);

    tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .init();

    Ok(guard)
}

#[tauri::command]
fn save_as(app_handle: tauri::AppHandle, filename: &str, data: &str) {
    match BASE64_STANDARD.decode(data) {
        Ok(decoded_data) => app_handle
            .dialog()
            .file()
            .set_file_name(filename)
            .save_file(move |path| {
                let result: Result<()> = (move || {
                    let file_path = path
                        .as_ref()
                        .and_then(FilePath::as_path)
                        .ok_or_else(|| anyhow!("Invalid file path {:?}", &path))?;
                    fs::write(file_path, &decoded_data)?;
                    info!("Saved file to {:?}", file_path);
                    Ok(())
                })();
                if let Err(e) = result {
                    error!("Failed to save file: {:?}", e);
                }
            }),
        Err(e) => {
            error!("Failed to decode file content: {:?}", e);
        }
    }
}

#[tauri::command]
fn download_today_gui_log(app_handle: tauri::AppHandle) -> std::result::Result<String, String> {
    download_log_file(app_handle, today_gui_log_filename(), "GUI")
}

#[tauri::command]
fn download_today_launcher_log(
    app_handle: tauri::AppHandle,
) -> std::result::Result<String, String> {
    download_log_file(app_handle, today_launcher_log_filename(), "launcher")
}

fn download_log_file(
    app_handle: tauri::AppHandle,
    filename: String,
    log_name: &str,
) -> std::result::Result<String, String> {
    let log_name = log_name.to_owned();
    let source_path = std::env::current_dir()
        .map_err(|e| e.to_string())?
        .join("log")
        .join(&filename);
    let data = fs::read(&source_path).map_err(|e| {
        t!(
            "errors.read_log_file",
            path = source_path.to_string_lossy().to_string(),
            error = e.to_string()
        )
    })?;

    app_handle
        .dialog()
        .file()
        .set_file_name(&filename)
        .save_file(move |path| {
            let log_name_for_save = log_name.clone();
            let result: Result<()> = (move || {
                let file_path = path
                    .as_ref()
                    .and_then(FilePath::as_path)
                    .ok_or_else(|| anyhow!("Invalid file path {:?}", &path))?;
                fs::write(file_path, &data)?;
                info!("Saved {} log to {:?}", log_name_for_save, file_path);
                Ok(())
            })();
            if let Err(e) = result {
                error!("Failed to save {} log: {:?}", log_name, e);
            }
        });

    Ok(filename)
}

fn today_gui_log_filename() -> String {
    format!("{}_gui.txt", Local::now().format("%Y-%m-%d"))
}

fn today_launcher_log_filename() -> String {
    format!("{}_launcher.txt", Local::now().format("%Y-%m-%d"))
}

#[tauri::command]
fn window_hide(app_handle: tauri::AppHandle) -> tauri::Result<()> {
    minimize_main_window_to_tray(&app_handle);
    Ok(())
}

#[tauri::command]
fn window_minimize(window: WebviewWindow) -> tauri::Result<()> {
    window.minimize()
}

#[tauri::command]
fn window_toggle_maximize(window: WebviewWindow) -> tauri::Result<bool> {
    if window.is_maximized()? {
        window.unmaximize()?;
        Ok(false)
    } else {
        window.maximize()?;
        Ok(true)
    }
}

#[tauri::command]
fn window_close(window: WebviewWindow) -> tauri::Result<()> {
    window.close()
}

#[tauri::command]
fn window_start_dragging(window: WebviewWindow) -> tauri::Result<()> {
    window.start_dragging()
}

#[tauri::command]
fn window_is_maximized(window: WebviewWindow) -> tauri::Result<bool> {
    window.is_maximized()
}

#[tauri::command]
fn retry_backend_connection(window: WebviewWindow, port: u16) -> std::result::Result<bool, String> {
    navigate_backend_or_error(&window, port).map_err(|e| {
        error!("Failed to retry backend connection: {:?}", e);
        e.to_string()
    })
}

fn page_load_injector(webview: WebviewWindow, payload: PageLoadPayload<'_>) {
    if payload.event() == PageLoadEvent::Finished {
        info!(
            "Injecting saveFile function to loaded page: {}",
            payload.url()
        );
        let injected_js = r#"
if (!window.alas_launcher_injected) {
    window.alas_launcher_injected = true;
    (function () {
        // Prevent going back
        history.pushState(null, document.title, location.href);
        window.addEventListener('popstate', event => {
            history.pushState(null, document.title, location.href);
        });
        // Overwrite original saveAs function
        window.saveAs = function (blob, filename) {
            const reader = new FileReader();
            reader.onload = async () => {
                const data = reader.result.split(',')[1];
                console.log(data);
                const tauriInvoke =
                    (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke)
                    || (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke);
                if (typeof tauriInvoke === 'function') {
                    tauriInvoke('save_as', { filename, data });
                }
            };
            reader.readAsDataURL(blob);
        };
__ALAS_TITLEBAR_SCRIPT__
    })();
}
"#
        .replace(
            "__ALAS_TITLEBAR_SCRIPT__",
            &main_window_titlebar_injection_script(),
        );
        if let Err(e) = webview.eval(&injected_js) {
            error!("Failed to inject JS to webview: {:?}", e);
        }
    }
}

fn initialize_splash(splash: &WebviewWindow) {
    match Url::parse(SPLASH_URL) {
        Ok(url) => {
            if let Err(e) = splash.navigate(url) {
                error!("Failed to navigate splash page: {:?}", e);
            }
            if !wait_for_splash_ready(splash, Duration::from_secs(2)) {
                warn!("Timed out waiting for splash page readiness; showing splash anyway");
            }
            if let Err(e) = splash.show() {
                error!("Failed to show splash window: {:?}", e);
            }
        }
        Err(e) => {
            error!("Failed to parse splash URL: {:?}", e);
        }
    }
}

fn wait_for_splash_ready(splash: &WebviewWindow, timeout: Duration) -> bool {
    let started_at = Instant::now();
    while started_at.elapsed() < timeout {
        if splash
            .eval(
                r#"
                if (!window.__ALAS_SPLASH_READY) {
                    throw new Error("splash page is not ready");
                }
                "#,
            )
            .is_ok()
        {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

fn update_splash(splash: &WebviewWindow, update: &SplashUpdate) {
    let payload = to_string(update).unwrap();
    let script = format!("window.__ALAS_SPLASH_UPDATE && window.__ALAS_SPLASH_UPDATE({payload});");
    if let Err(e) = splash.eval(&script) {
        error!("Failed to update splash page: {:?}", e);
    }
}

fn backend_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/")
}

fn splash_response() -> tauri::http::Response<Vec<u8>> {
    let light_bg_b64 = BASE64_STANDARD.encode(SPLASH_BG_LIGHT);
    let dark_bg_b64 = BASE64_STANDARD.encode(SPLASH_BG_DARK);
    tauri::http::Response::builder()
        .header(
            tauri::http::header::CONTENT_TYPE,
            "text/html; charset=utf-8",
        )
        .body(splash_redesigned_shell_html(&light_bg_b64, &dark_bg_b64).into_bytes())
        .unwrap()
}

fn check_backend_connection(port: u16) -> Result<()> {
    let address: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    TcpStream::connect_timeout(&address, BACKEND_CONNECT_TIMEOUT)
        .map(|_| ())
        .map_err(|e| anyhow!("Unable to connect to local backend at {address}: {e}"))
}

fn wait_for_backend_connection(port: u16, timeout: Duration) -> Result<()> {
    let started_at = Instant::now();
    let mut last_error = None;
    while started_at.elapsed() < timeout {
        match check_backend_connection(port) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_error = Some(e);
                thread::sleep(Duration::from_millis(200));
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!(t!("errors.backend_timeout"))))
}

fn navigate_backend_or_error(window: &WebviewWindow, port: u16) -> Result<bool> {
    match wait_for_backend_connection(port, BACKEND_NAVIGATION_TIMEOUT) {
        Ok(()) => {
            let url = backend_url(port);
            window.navigate(Url::parse(&url)?)?;
            Ok(true)
        }
        Err(e) => {
            warn!("Backend connection check failed before navigation: {:?}", e);
            navigate_to_backend_error(window, port, &e.to_string())?;
            Ok(false)
        }
    }
}

fn navigate_to_backend_error(window: &WebviewWindow, port: u16, error_detail: &str) -> Result<()> {
    let url = backend_error_url(port, error_detail)?;
    window.navigate(url)?;
    Ok(())
}

fn backend_error_url(port: u16, error_detail: &str) -> Result<Url> {
    let port = port.to_string();
    Ok(Url::parse_with_params(
        BACKEND_ERROR_URL_BASE,
        [("port", port.as_str()), ("detail", error_detail)],
    )?)
}

fn backend_error_response(
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    let (port, detail) = backend_error_request_params(request.uri().to_string().as_str());
    let html = backend_error_html(port, &detail);

    tauri::http::Response::builder()
        .header(
            tauri::http::header::CONTENT_TYPE,
            "text/html; charset=utf-8",
        )
        .body(html.into_bytes())
        .unwrap()
}

fn backend_error_request_params(uri: &str) -> (u16, String) {
    let mut port = 22267;
    let mut detail = t!("error_page.unable_connect").to_string();

    if let Ok(url) = Url::parse(uri) {
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "port" => {
                    if let Ok(parsed_port) = value.parse::<u16>() {
                        port = parsed_port;
                    }
                }
                "detail" => detail = value.into_owned(),
                _ => {}
            }
        }
    }

    (port, detail)
}

fn handle_backend_navigation(app: tauri::AppHandle, port: u16, url: &Url) -> bool {
    if !is_backend_url(url, port) {
        return true;
    }

    match check_backend_connection(port) {
        Ok(()) => true,
        Err(e) => {
            let blocked_url = url.to_string();
            warn!(
                "Blocked navigation to unreachable backend {}: {:?}",
                blocked_url, e
            );
            let error_detail = e.to_string();
            thread::spawn(move || {
                if let Some(window) = app.get_webview_window("main") {
                    if let Err(e) = navigate_to_backend_error(&window, port, &error_detail) {
                        error!("Failed to show backend error page: {:?}", e);
                    }
                }
            });
            false
        }
    }
}

fn is_backend_url(url: &Url, port: u16) -> bool {
    matches!(url.scheme(), "http" | "https")
        && matches!(url.host_str(), Some("127.0.0.1") | Some("localhost"))
        && url.port_or_known_default() == Some(port)
}

fn escape_html(input: impl AsRef<str>) -> String {
    input
        .as_ref()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn backend_error_html(port: u16, error_detail: &str) -> String {
    let backend_url_json = to_string(&backend_url(port)).unwrap();
    let error_detail_json = to_string(error_detail).unwrap();
    let titlebar_script = main_window_titlebar_injection_script();
    let i18n = serde_json::json!({
        "title": t!("error_page.title"),
        "heading": t!("error_page.heading"),
        "description": t!("error_page.description"),
        "address": t!("error_page.address"),
        "errorLabel": t!("error_page.error_label"),
        "retry": t!("error_page.retry"),
        "downloadGuiLog": t!("error_page.download_gui_log"),
        "downloadLauncherLog": t!("error_page.download_launcher_log"),
        "reconnecting": t!("error_page.reconnecting"),
        "stillFailed": t!("error_page.still_failed"),
        "retryFailed": t!("error_page.retry_failed"),
        "preparing": t!("error_page.preparing"),
        "saved": t!("error_page.saved"),
        "downloadFailed": t!("error_page.download_failed"),
    });
    let i18n_json = to_string(&i18n).unwrap();

    format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
  :root {{
    color-scheme: light;
    --bg: #f5f7fb;
    --panel: #ffffff;
    --border: #d9e2ee;
    --text: #182230;
    --muted: #5f6f84;
    --danger: #c03434;
    --danger-bg: #fff1f1;
    --primary: #1f66ad;
    --primary-hover: #18558f;
  }}
  * {{
    box-sizing: border-box;
  }}
  html, body {{
    min-height: 100%;
    margin: 0;
    font-family: "Segoe UI", "SF Pro Text", "Helvetica Neue", Arial, sans-serif;
    color: var(--text);
    background: var(--bg);
  }}
  body {{
    display: grid;
    place-items: center;
    padding: 72px 28px 32px;
  }}
  .panel {{
    width: min(680px, 100%);
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--panel);
    box-shadow: 0 18px 44px rgba(21, 35, 54, 0.10);
    padding: 32px;
  }}
  .mark {{
    width: 38px;
    height: 38px;
    border-radius: 8px;
    display: grid;
    place-items: center;
    background: var(--danger-bg);
    color: var(--danger);
    border: 1px solid #f0caca;
    font-size: 24px;
    line-height: 1;
    font-weight: 600;
    margin-bottom: 20px;
  }}
  h1 {{
    margin: 0;
    font-size: 28px;
    font-weight: 600;
    line-height: 1.2;
  }}
  .lead {{
    margin: 12px 0 0;
    color: var(--muted);
    font-size: 15px;
    line-height: 1.7;
  }}
  .details {{
    margin: 24px 0 0;
    border: 1px solid var(--border);
    border-radius: 8px;
    overflow: hidden;
    background: #fbfdff;
  }}
  .row {{
    display: grid;
    grid-template-columns: 72px minmax(0, 1fr);
    gap: 14px;
    padding: 13px 16px;
    border-top: 1px solid var(--border);
    font-size: 13px;
    line-height: 1.55;
  }}
  .row:first-child {{
    border-top: 0;
  }}
  .label {{
    color: var(--muted);
  }}
  .value {{
    min-width: 0;
    overflow-wrap: anywhere;
    font-family: Consolas, "SFMono-Regular", Menlo, monospace;
  }}
  .actions {{
    display: flex;
    align-items: center;
    gap: 12px;
    flex-wrap: wrap;
    margin-top: 24px;
  }}
  .action-button {{
    min-height: 38px;
    border: 1px solid transparent;
    border-radius: 6px;
    padding: 0 16px;
    font: inherit;
    font-size: 14px;
    cursor: pointer;
    color: #ffffff;
    background: var(--primary);
  }}
  .action-button:hover {{
    background: var(--primary-hover);
  }}
  .action-button:disabled {{
    cursor: default;
    opacity: 0.65;
  }}
  .status {{
    min-height: 20px;
    color: var(--muted);
    font-size: 13px;
  }}
  @media (max-width: 560px) {{
    body {{
      padding: 64px 16px 22px;
      place-items: start stretch;
    }}
    .panel {{
      padding: 24px;
    }}
    h1 {{
      font-size: 23px;
    }}
    .row {{
      grid-template-columns: 1fr;
      gap: 4px;
    }}
    .action-button {{
      width: 100%;
    }}
  }}
</style>
</head>
<body>
  <main class="panel">
    <div class="mark">!</div>
    <h1>{heading}</h1>
    <p class="lead">{description}</p>
    <section class="details" aria-label="{connection_info}">
      <div class="row">
        <div class="label">{address}</div>
        <div id="backend-url" class="value"></div>
      </div>
      <div class="row">
        <div class="label">{error_label}</div>
        <div id="error-detail" class="value"></div>
      </div>
    </section>
    <div class="actions">
      <button id="retry-button" class="action-button" type="button">{retry}</button>
      <button id="gui-log-button" class="action-button" type="button">{download_gui_log}</button>
      <button id="launcher-log-button" class="action-button" type="button">{download_launcher_log}</button>
      <span id="retry-status" class="status"></span>
    </div>
  </main>
  <script>
    (function () {{
{titlebar_script}
    }})();

    const i18n = {i18n_json};
    const backendUrl = {backend_url_json};
    const errorDetail = {error_detail_json};
    const port = {port};
    const retryButton = document.getElementById('retry-button');
    const guiLogButton = document.getElementById('gui-log-button');
    const launcherLogButton = document.getElementById('launcher-log-button');
    const retryStatus = document.getElementById('retry-status');
    const invoke =
      (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke)
      || (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke);

    document.getElementById('backend-url').textContent = backendUrl;
    document.getElementById('error-detail').textContent = errorDetail;

    retryButton.addEventListener('click', async () => {{
      retryButton.disabled = true;
      retryStatus.textContent = i18n.reconnecting;
      try {{
        if (typeof invoke !== 'function') {{
          throw new Error('Tauri invoke is unavailable');
        }}
        const connected = await invoke('retry_backend_connection', {{ port }});
        if (!connected) {{
          retryStatus.textContent = i18n.stillFailed;
          retryButton.disabled = false;
        }}
      }} catch (error) {{
        retryStatus.textContent = i18n.retryFailed + (error && error.message ? error.message : error);
        retryButton.disabled = false;
      }}
    }});

    async function downloadLog(button, command, label) {{
      button.disabled = true;
      retryStatus.textContent = i18n.preparing.replace('%{{label}}', label);
      try {{
        if (typeof invoke !== 'function') {{
          throw new Error('Tauri invoke is unavailable');
        }}
        const filename = await invoke(command);
        retryStatus.textContent = i18n.saved.replace('%{{filename}}', filename);
      }} catch (error) {{
        retryStatus.textContent = i18n.downloadFailed.replace('%{{label}}', label) + (error && error.message ? error.message : error);
      }} finally {{
        button.disabled = false;
      }}
    }}

    guiLogButton.addEventListener('click', () => {{
      downloadLog(guiLogButton, 'download_today_gui_log', '{gui_log_label}');
    }});

    launcherLogButton.addEventListener('click', () => {{
      downloadLog(launcherLogButton, 'download_today_launcher_log', '{launcher_log_label}');
    }});

    // 每秒尝试自动刷新（重试连接）
    setInterval(() => {{
      if (!retryButton.disabled) {{
        retryButton.click();
      }}
    }}, 1000);
  </script>
</body>
</html>"#,
        title = t!("error_page.title"),
        heading = t!("error_page.heading"),
        description = t!("error_page.description"),
        address = t!("error_page.address"),
        error_label = t!("error_page.error_label"),
        retry = t!("error_page.retry"),
        download_gui_log = t!("error_page.download_gui_log"),
        download_launcher_log = t!("error_page.download_launcher_log"),
        gui_log_label = t!("error_page.download_gui_log"),
        launcher_log_label = t!("error_page.download_launcher_log"),
        connection_info = t!("error_page.connection_info"),
    )
}

fn splash_redesigned_shell_html(light_bg_b64: &str, dark_bg_b64: &str) -> String {
    let i18n = serde_json::json!({
        "defaultTip": t!("tips.17"),
        "loading": t!("splash.loading_badge"),
        "webuiInit": t!("splash.webui_init"),
        "starting": t!("splash.starting"),
        "errorBadge": t!("splash.error_badge"),
        "initStopped": t!("splash.init_stopped"),
        "progressMetaReady": t!("splash.progress_meta_ready"),
        "preparingLog": t!("splash.preparing_log"),
        "logSavedPrefix": t!("splash.log_saved_prefix"),
        "logFailed": t!("splash.log_failed"),
    });
    let i18n_json = to_string(&i18n).unwrap();

    r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  :root {
    --primary-color: #4facfe;
    --secondary-color: #00f2fe;
    --text-main: #ffffff;
    --text-sub: rgba(255, 255, 255, 0.76);
    --text-muted: rgba(255, 255, 255, 0.52);
    --surface-soft: rgba(255, 255, 255, 0.16);
    --surface-border: rgba(255, 255, 255, 0.15);
    --danger: #ff5f57;
    --warning: #ffbd2e;
  }
  * {
    box-sizing: border-box;
    margin: 0;
    padding: 0;
    user-select: none;
  }
  html,
  body {
    width: 100%;
    height: 100%;
    overflow: hidden;
    background: #111827;
  }
  body {
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, "Microsoft YaHei", sans-serif;
    color: var(--text-main);
  }
  button {
    font: inherit;
  }
  .launcher-window {
    position: relative;
    width: 100%;
    height: 100%;
    overflow: hidden;
    border-radius: 0;
    background: url(data:image/webp;base64,$LIGHT_BG) center/cover no-repeat;
    box-shadow: none;
    display: flex;
    flex-direction: column;
    justify-content: space-between;
  }
  @media (prefers-color-scheme: dark) {
    .launcher-window {
      background-image: url(data:image/webp;base64,$DARK_BG);
    }
  }
  .launcher-window::before {
    content: "";
    position: absolute;
    inset: 0;
    z-index: 0;
    background:
      linear-gradient(to bottom, rgba(0, 0, 0, 0.16) 0%, rgba(0, 0, 0, 0.08) 42%, rgba(0, 0, 0, 0.58) 100%),
      linear-gradient(115deg, rgba(12, 30, 72, 0.22), rgba(255, 126, 117, 0.12));
    pointer-events: none;
  }
  body.error-state .launcher-window::before {
    background:
      linear-gradient(to bottom, rgba(56, 0, 10, 0.28) 0%, rgba(78, 0, 13, 0.18) 42%, rgba(60, 0, 12, 0.68) 100%),
      linear-gradient(115deg, rgba(255, 95, 87, 0.34), rgba(255, 189, 46, 0.08));
  }
  .top-bar {
    position: relative;
    z-index: 2;
    display: flex;
    justify-content: space-between;
    align-items: center;
    min-height: 60px;
    padding: 18px 24px;
  }
  .brand-zone {
    display: flex;
    align-items: center;
    min-width: 0;
    gap: 10px;
  }
  .app-title {
    color: var(--text-main);
    font-size: 18px;
    font-weight: 700;
    letter-spacing: 0;
    text-shadow: 0 2px 6px rgba(0, 0, 0, 0.22);
  }
  .app-version {
    color: var(--text-sub);
    font-size: 12px;
    line-height: 1;
    background: rgba(255, 255, 255, 0.14);
    border: 1px solid rgba(255, 255, 255, 0.11);
    padding: 4px 9px;
    border-radius: 999px;
    backdrop-filter: blur(8px);
  }
  .top-right {
    display: flex;
    align-items: center;
    gap: 18px;
    min-width: 0;
  }
  .status-badge {
    max-width: 260px;
    min-height: 28px;
    display: inline-flex;
    align-items: center;
    gap: 7px;
    border-radius: 999px;
    padding: 6px 14px;
    color: var(--text-main);
    background: var(--surface-soft);
    border: 1px solid var(--surface-border);
    backdrop-filter: blur(12px);
    box-shadow: 0 10px 24px rgba(0, 0, 0, 0.12);
    font-size: 12px;
    font-weight: 500;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    animation: pulse 2.2s ease-in-out infinite;
  }
  .status-badge::before {
    content: "";
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--secondary-color);
    box-shadow: 0 0 12px rgba(0, 242, 254, 0.7);
    flex: 0 0 auto;
  }
  .window-controls {
    display: flex;
    align-items: center;
    gap: 8px;
    flex: 0 0 auto;
  }
  .win-btn {
    width: 13px;
    height: 13px;
    border: 0;
    border-radius: 50%;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    cursor: pointer;
    padding: 0;
    transition: filter 140ms ease, transform 140ms ease;
  }
  .win-btn:hover {
    filter: brightness(1.07);
    transform: scale(1.04);
  }
  .win-btn:active {
    filter: brightness(0.9);
    transform: scale(0.97);
  }
  .win-btn svg {
    width: 7px;
    height: 7px;
    stroke: rgba(50, 42, 35, 0.72);
    stroke-width: 1.45;
    stroke-linecap: round;
    opacity: 0;
    transition: opacity 140ms ease;
  }
  .window-controls:hover .win-btn svg {
    opacity: 1;
  }
  .win-btn.minimize {
    background: var(--warning);
    box-shadow: 0 0 0 0.5px rgba(156, 110, 6, 0.55);
  }
  .win-btn.close {
    background: var(--danger);
    box-shadow: 0 0 0 0.5px rgba(160, 32, 28, 0.55);
  }
  .main-content {
    position: relative;
    z-index: 2;
    padding: 0 40px 35px;
  }
  .update-status {
    margin-bottom: 25px;
    max-width: min(650px, 100%);
  }
  .title-group {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-bottom: 8px;
  }
  .spinner {
    width: 22px;
    height: 22px;
    border: 2.5px solid rgba(255, 255, 255, 0.24);
    border-top-color: var(--text-main);
    border-radius: 50%;
    animation: spin 0.9s linear infinite;
    flex: 0 0 auto;
  }
  .err-dot {
    width: 22px;
    height: 22px;
    border-radius: 50%;
    background: #ffffff;
    color: #c73532;
    align-items: center;
    justify-content: center;
    font-size: 14px;
    font-weight: 800;
    box-shadow: 0 5px 16px rgba(0, 0, 0, 0.2);
    flex: 0 0 auto;
  }
  .main-action-text {
    min-width: 0;
    color: var(--text-main);
    font-size: 24px;
    line-height: 1.2;
    font-weight: 650;
    letter-spacing: 0;
    text-shadow: 0 2px 10px rgba(0, 0, 0, 0.32);
  }
  .sub-action-text {
    color: var(--text-sub);
    font-size: 12px;
    font-weight: 650;
    letter-spacing: 1.2px;
    line-height: 1.45;
    margin: 0;
    max-width: min(650px, 100%);
    max-height: 54px;
    overflow: hidden;
    text-shadow: 0 1px 5px rgba(0, 0, 0, 0.28);
    text-transform: uppercase;
    white-space: pre-line;
  }
  .progress-container {
    position: relative;
    margin-bottom: 15px;
  }
  .progress-bar-bg {
    width: 100%;
    height: 6px;
    border-radius: 999px;
    background: rgba(255, 255, 255, 0.22);
    overflow: hidden;
    backdrop-filter: blur(5px);
  }
  .progress-bar-fill {
    width: 4%;
    height: 100%;
    border-radius: inherit;
    background: linear-gradient(90deg, var(--primary-color), var(--secondary-color));
    box-shadow: 0 0 14px rgba(0, 242, 254, 0.5);
    position: relative;
    overflow: hidden;
    transition: width 0.35s ease, background 0.2s ease;
  }
  .progress-bar-fill::after {
    content: "";
    position: absolute;
    inset: 0;
    background: linear-gradient(90deg, transparent, rgba(255, 255, 255, 0.48), transparent);
    transform: translateX(-100%);
    animation: sweep 2s ease-in-out infinite;
  }
  .progress-bar-fill-error {
    background: linear-gradient(90deg, #ff5f57, #ffbd2e);
    box-shadow: 0 0 14px rgba(255, 95, 87, 0.46);
  }
  .progress-bar-fill-error::after {
    display: none;
  }
  .progress-percentage {
    position: absolute;
    right: 0;
    top: -25px;
    color: var(--text-main);
    font-size: 14px;
    font-weight: 750;
    font-variant-numeric: tabular-nums;
    text-shadow: 0 2px 6px rgba(0, 0, 0, 0.32);
  }
  .footer-info {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 16px;
    min-height: 28px;
    font-size: 12px;
  }
  .tip-text {
    min-width: 0;
    max-width: 520px;
    color: var(--text-sub);
    background: rgba(0, 0, 0, 0.16);
    border-left: 3px solid var(--primary-color);
    border-radius: 4px;
    padding: 5px 12px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    backdrop-filter: blur(7px);
  }
  .footer-right {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 10px;
    flex: 0 0 auto;
  }
  .notice-text {
    color: var(--text-muted);
    white-space: nowrap;
  }
  .splash-actions {
    display: none;
  }
  .splash-actions-err {
    display: block;
  }
  .splash-log-button {
    min-height: 28px;
    border: 1px solid rgba(255, 255, 255, 0.28);
    border-radius: 6px;
    padding: 0 11px;
    color: var(--text-main);
    background: rgba(255, 255, 255, 0.14);
    backdrop-filter: blur(10px);
    cursor: pointer;
    font-size: 12px;
    font-weight: 600;
  }
  .splash-log-button:hover {
    background: rgba(255, 255, 255, 0.23);
  }
  .splash-log-button:disabled {
    cursor: default;
    opacity: 0.65;
  }
  body.error-state .status-badge {
    background: rgba(255, 255, 255, 0.18);
    animation: none;
  }
  body.error-state .status-badge::before {
    background: #ff5f57;
    box-shadow: 0 0 12px rgba(255, 95, 87, 0.76);
  }
  body.error-state .tip-text {
    border-left-color: #ffbd2e;
  }
  @media (max-width: 720px) {
    .top-bar {
      padding: 16px 20px;
    }
    .status-badge {
      max-width: 180px;
    }
    .main-content {
      padding: 0 28px 28px;
    }
    .main-action-text {
      font-size: 22px;
    }
  }
  @media (max-width: 560px), (max-height: 340px) {
    .top-right {
      gap: 12px;
    }
    .status-badge {
      display: none;
    }
    .footer-info {
      flex-direction: column;
      align-items: flex-start;
      gap: 8px;
    }
    .footer-right {
      width: 100%;
      justify-content: space-between;
    }
    .tip-text {
      max-width: 100%;
    }
  }
  @media (max-height: 340px) {
    .main-content {
      padding-bottom: 24px;
    }
    .update-status {
      margin-bottom: 18px;
    }
    .sub-action-text {
      max-height: 36px;
    }
  }
  @keyframes spin {
    to { transform: rotate(360deg); }
  }
  @keyframes pulse {
    0%, 100% { opacity: 0.9; transform: scale(1); }
    50% { opacity: 1; transform: scale(1.015); box-shadow: 0 0 18px rgba(255, 255, 255, 0.18); }
  }
  @keyframes sweep {
    to { transform: translateX(200%); }
  }
</style>
</head>
<body>
  <div class="launcher-window">
    <div id="splash-drag-region" class="top-bar">
      <div class="brand-zone">
        <span class="app-title">AzurPilot</span>
        <span class="app-version">v$LAUNCHER_VERSION</span>
      </div>
      <div class="top-right">
        <div id="badge" class="status-badge">
          <span id="badge-text">$I18N_INITIALIZING</span>
        </div>
        <div class="window-controls">
          <button id="window-minimize" class="win-btn minimize" type="button" aria-label="$I18N_MINIMIZE" title="$I18N_MINIMIZE">
            <svg viewBox="0 0 8 8" aria-hidden="true"><line x1="2" y1="4" x2="6" y2="4"></line></svg>
          </button>
          <button id="window-close" class="win-btn close" type="button" aria-label="$I18N_CLOSE" title="$I18N_CLOSE">
            <svg viewBox="0 0 8 8" aria-hidden="true"><line x1="2" y1="2" x2="6" y2="6"></line><line x1="6" y1="2" x2="2" y2="6"></line></svg>
          </button>
        </div>
      </div>
    </div>

    <div class="main-content">
      <div class="update-status">
        <div class="title-group">
          <div id="spinner" class="spinner"></div>
          <div id="error-dot" class="err-dot" style="display:none;">!</div>
          <h1 id="title" class="main-action-text">$I18N_STARTING</h1>
        </div>
        <p id="detail" class="sub-action-text">$I18N_WEBUI_INIT</p>
      </div>

      <div class="progress-container">
        <div id="progress-pct" class="progress-percentage">4%</div>
        <div class="progress-bar-bg">
          <div id="progress-fill" class="progress-bar-fill" style="width: 4%;"></div>
        </div>
      </div>

      <div class="footer-info">
        <div id="tip-text" class="tip-text">Tips: $I18N_DEFAULT_TIP</div>
        <div class="footer-right">
          <div id="progress-meta" class="notice-text">$I18N_PROGRESS_META</div>
          <div id="splash-actions" class="splash-actions">
            <button id="splash-log-button" class="splash-log-button" type="button">$I18N_DOWNLOAD_LOG</button>
          </div>
        </div>
      </div>
    </div>
  </div>

  <script>
    const i18n = $I18N_JSON;
    const defaultTip = i18n.defaultTip;
    const invoke =
      (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke)
      || (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke);

    window.addEventListener('contextmenu', event => {
      event.preventDefault();
    }, { capture: true });

    function splitSubtitle(value) {
      const text = String(value || '').trim();
      if (!text) {
        return { status: i18n.loading, tip: defaultTip };
      }
      const match = text.match(/^(.*?)\s*\|\s*Tips[:：]\s*(.*)$/);
      if (!match) {
        return { status: text, tip: defaultTip };
      }
      return {
        status: match[1].trim() || i18n.loading,
        tip: match[2].trim() || defaultTip,
      };
    }

    function normalizeDetail(value) {
      const text = String(value || '').trim();
      return text || i18n.webuiInit;
    }

    window.__ALAS_SPLASH_UPDATE = function (payload) {
      const badge = document.getElementById('badge');
      const badgeText = document.getElementById('badge-text');
      const spinner = document.getElementById('spinner');
      const errorDot = document.getElementById('error-dot');
      const progressFill = document.getElementById('progress-fill');
      const progressPct = document.getElementById('progress-pct');
      const progressMeta = document.getElementById('progress-meta');
      const splashActions = document.getElementById('splash-actions');
      const subtitle = splitSubtitle(payload.subtitle);

      badgeText.textContent = payload.is_error ? i18n.errorBadge : subtitle.status;
      document.getElementById('tip-text').textContent = 'Tips: ' + subtitle.tip;
      document.getElementById('title').textContent = payload.title || i18n.starting;
      document.getElementById('detail').textContent = normalizeDetail(payload.detail);
      progressMeta.textContent = payload.is_error
        ? i18n.initStopped
        : i18n.progressMetaReady;

      const progress = Math.max(0, Math.min(100, Number(payload.progress || 0)));
      progressFill.style.width = progress + '%';
      progressPct.textContent = progress + '%';

      if (payload.is_error) {
        document.body.classList.add('error-state');
        badge.className = 'status-badge status-badge-err';
        spinner.style.display = 'none';
        errorDot.style.display = 'flex';
        progressFill.className = 'progress-bar-fill progress-bar-fill-error';
        splashActions.className = 'splash-actions splash-actions-err';
      } else {
        document.body.classList.remove('error-state');
        badge.className = 'status-badge';
        spinner.style.display = 'block';
        errorDot.style.display = 'none';
        progressFill.className = 'progress-bar-fill';
        splashActions.className = 'splash-actions';
      }
    };

    document.getElementById('splash-drag-region').addEventListener('mousedown', event => {
      if (event.button !== 0 || event.target.closest('button')) {
        return;
      }
      if (typeof invoke === 'function') {
        invoke('window_start_dragging').catch(error => {
          console.error('Failed to drag splash window', error);
        });
      }
    });

    document.getElementById('window-minimize').addEventListener('click', event => {
      event.stopPropagation();
      if (typeof invoke === 'function') {
        invoke('window_minimize').catch(error => {
          console.error('Failed to minimize splash window', error);
        });
      }
    });

    document.getElementById('window-close').addEventListener('click', event => {
      event.stopPropagation();
      if (typeof invoke === 'function') {
        invoke('window_close').catch(error => {
          console.error('Failed to close splash window', error);
        });
      }
    });

    document.getElementById('splash-log-button').addEventListener('click', async () => {
      const button = document.getElementById('splash-log-button');
      const progressMeta = document.getElementById('progress-meta');
      button.disabled = true;
      progressMeta.textContent = i18n.preparingLog;
      try {
        if (typeof invoke !== 'function') {
          throw new Error('Tauri invoke is unavailable');
        }
        const filename = await invoke('download_today_launcher_log');
        progressMeta.textContent = i18n.logSavedPrefix + filename;
      } catch (error) {
        progressMeta.textContent = i18n.logFailed + (error && error.message ? error.message : error);
      } finally {
        button.disabled = false;
      }
    });

    window.__ALAS_SPLASH_READY = true;
  </script>
</body>
</html>"#
    .replace("$LIGHT_BG", light_bg_b64)
    .replace("$DARK_BG", dark_bg_b64)
    .replace("$LAUNCHER_VERSION", env!("CARGO_PKG_VERSION"))
    .replace("$I18N_JSON", &i18n_json)
    .replace("$I18N_INITIALIZING", &escape_html(t!("splash.initializing")))
    .replace("$I18N_MINIMIZE", &escape_html(t!("titlebar.minimize")))
    .replace("$I18N_CLOSE", &escape_html(t!("titlebar.close")))
    .replace("$I18N_STARTING", &escape_html(t!("splash.starting")))
    .replace("$I18N_WEBUI_INIT", &escape_html(t!("splash.webui_init")))
    .replace("$I18N_DEFAULT_TIP", &escape_html(t!("tips.17")))
    .replace("$I18N_PROGRESS_META", &escape_html(t!("splash.progress_meta_ready")))
    .replace("$I18N_DOWNLOAD_LOG", &escape_html(t!("splash.download_log")))
}

fn create_main_window(app: &tauri::AppHandle, port: u16) -> Result<WebviewWindow> {
    let main_config = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == "main")
        .ok_or_else(|| anyhow!("Main window config not found"))?;

    let app_for_navigation = app.clone();
    let main_window = tauri::WebviewWindowBuilder::from_config(app, main_config)?
        .on_navigation(move |url| handle_backend_navigation(app_for_navigation.clone(), port, url))
        .on_page_load(page_load_injector)
        .build()?;
    main_window.set_resizable(true)?;

    // Windows/Linux: remove native decorations for the main window as well.
    // Splash is configured as borderless in tauri.conf.json.
    #[cfg(not(target_os = "macos"))]
    {
        main_window.set_decorations(false)?;
    }

    Ok(main_window)
}

fn reveal_window(window: &WebviewWindow) -> tauri::Result<()> {
    if window.is_minimized()? {
        window.unminimize()?;
    }
    window.show()?;
    window.set_focus()?;
    Ok(())
}

fn minimize_main_window_to_tray(app: &tauri::AppHandle) {
    #[cfg(windows)]
    {
        if let Some(window) = app.get_webview_window("main") {
            info!("Destroying main window to release WebView resources while trayed");
            if let Err(e) = window.destroy() {
                warn!("Failed to destroy main window for tray mode: {:?}", e);
            }
        }
    }

    #[cfg(not(windows))]
    {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.hide();
        }
    }

    #[cfg(target_os = "macos")]
    {
        set_macos_activation_policy(app, false);
    }
}

fn restore_main_window_from_any_thread(
    app: tauri::AppHandle,
    port: u16,
    recreating_main_window: Arc<AtomicBool>,
) {
    let app_for_restore = app.clone();
    if let Err(e) = app.run_on_main_thread(move || {
        restore_main_window_from_tray(&app_for_restore, port, recreating_main_window);
    }) {
        warn!("Failed to schedule main window restore: {:?}", e);
    }
}

fn restore_main_window_from_tray(
    app: &tauri::AppHandle,
    port: u16,
    recreating_main_window: Arc<AtomicBool>,
) {
    if let Some(window) = app.get_webview_window("main") {
        #[cfg(target_os = "macos")]
        set_macos_activation_policy(app, true);
        let _ = reveal_window(&window);
        return;
    }

    if recreating_main_window
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        debug!("Main window recreation already in progress");
        return;
    }

    let app_handle = app.clone();
    thread::spawn(move || {
        #[cfg(target_os = "macos")]
        set_macos_activation_policy(&app_handle, true);

        let result = (|| -> Result<()> {
            let window = create_main_window(&app_handle, port)?;
            navigate_backend_or_error(&window, port)?;
            reveal_window(&window)?;
            Ok(())
        })();

        recreating_main_window.store(false, Ordering::SeqCst);

        if let Err(e) = result {
            error!("Failed to recreate main window from tray: {:?}", e);
        }
    });
}

fn toggle_main_window_visibility(
    app: &tauri::AppHandle,
    port: u16,
    recreating_main_window: Arc<AtomicBool>,
) {
    if let Some(window) = app.get_webview_window("main") {
        let is_visible = window.is_visible().unwrap_or(false);
        let is_minimized = window.is_minimized().unwrap_or(false);
        if is_visible && !is_minimized {
            minimize_main_window_to_tray(app);
        } else {
            restore_main_window_from_tray(app, port, recreating_main_window);
        }
    } else {
        restore_main_window_from_tray(app, port, recreating_main_window);
    }
}

fn main_window_titlebar_injection_script() -> String {
    #[cfg(target_os = "macos")]
    {
        String::new()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let i18n = serde_json::json!({
            "hideLabel": t!("titlebar.minimize_to_tray"),
            "minimizeLabel": t!("titlebar.minimize_window"),
            "minimizeTitle": t!("titlebar.minimize"),
            "maximizeLabel": t!("titlebar.maximize_restore_window"),
            "maximizeTitle": t!("titlebar.maximize"),
            "closeLabel": t!("titlebar.close_window"),
            "closeTitle": t!("titlebar.close"),
            "restoreTitle": t!("titlebar.restore"),
            "maximizeActionTitle": t!("titlebar.maximize_action"),
            "restoreLabel": t!("titlebar.restore_window"),
            "maximizeLabelText": t!("titlebar.maximize_window"),
        });
        let i18n_json = serde_json::to_string(&i18n).unwrap();
        let mut s = String::with_capacity(4096);
        s.push_str("const i18n = ");
        s.push_str(&i18n_json);
        s.push_str(r#";
        const invoke =
            (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke)
            || (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke);
        if (typeof invoke !== 'function') {
            return;
        }
        const ensureTitlebar = () => {
            if (!document.body || document.getElementById('alas-launcher-titlebar')) {
                return;
            }
            if (!document.getElementById('alas-launcher-titlebar-style')) {
                const style = document.createElement('style');
                style.id = 'alas-launcher-titlebar-style';
                style.textContent = ':root{--alas-titlebar-height:44px}#alas-launcher-titlebar{position:fixed;top:0;left:0;right:0;height:var(--alas-titlebar-height);z-index:2147483647;user-select:none;pointer-events:none;background:transparent}#alas-launcher-titlebar *{box-sizing:border-box}.alas-titlebar-drag-zone{position:absolute;inset:0 120px 0 0;height:100%;pointer-events:auto;background:transparent}.header-icon{display:flex;align-items:center;gap:8px;padding:0 12px;position:absolute;top:0;right:0;height:100%;pointer-events:auto}.icon{width:12px;height:12px;border-radius:50%;border:none;cursor:pointer;flex:0 0 auto;position:relative;transition:filter 120ms ease;display:inline-flex;align-items:center;justify-content:center}.icon:active{filter:brightness(0.85)}.icon-hide{background:#3b82f6;box-shadow:0 0 0 .5px #2563eb}.icon-close{background:#ff5f57;box-shadow:0 0 0 .5px #e0443e}.icon-minimize{background:#febc2e;box-shadow:0 0 0 .5px #d4a017}.icon-maximize{background:#28c840;box-shadow:0 0 0 .5px #14ae35}.icon svg{width:7px;height:7px;stroke:rgba(0,0,0,.72);fill:none;stroke-width:1.35;stroke-linecap:round;stroke-linejoin:round;opacity:0;transition:opacity 150ms ease}.header-icon:hover .icon svg{opacity:1}@media(max-width:680px){.alas-titlebar-drag-zone{inset-right:88px}}';
                document.head.appendChild(style);
            }
            const titlebar = document.createElement('div');
            titlebar.id = 'alas-launcher-titlebar';
            titlebar.innerHTML = '<div class="alas-titlebar-drag-zone" aria-hidden="true"></div><div class="header-icon"><button type="button" class="icon icon-hide" data-action="hide" aria-label="'+i18n.hideLabel+'" title="'+i18n.hideLabel+'"><svg viewBox="0 0 6 6"><rect x="1" y="1" width="4" height="4" rx="1"/><path d="M2 3h2"/></svg></button><button type="button" class="icon icon-minimize" data-action="minimize" aria-label="'+i18n.minimizeLabel+'" title="'+i18n.minimizeTitle+'"><svg viewBox="0 0 6 6"><line x1="1" y1="3" x2="5" y2="3"/></svg></button><button type="button" class="icon icon-maximize" data-action="maximize" aria-label="'+i18n.maximizeLabel+'" title="'+i18n.maximizeTitle+'"><svg viewBox="0 0 6 6" class="svg-restore" style="display:none"><polyline points="1,3 1,1 3,1"/><polyline points="3,5 5,5 5,3"/></svg><svg viewBox="0 0 6 6" class="svg-maximize"><polyline points="1,2.5 1,1 2.5,1"/><polyline points="3.5,5 5,5 5,3.5"/></svg></button><button type="button" class="icon icon-close" data-action="close" aria-label="'+i18n.closeLabel+'" title="'+i18n.closeTitle+'"><svg viewBox="0 0 6 6"><line x1="1" y1="1" x2="5" y2="5"/><line x1="5" y1="1" x2="1" y2="5"/></svg></button></div>';
            document.body.dataset.alasCustomTitlebar = 'true';
            document.body.prepend(titlebar);
            const dragZone = titlebar.querySelector('.alas-titlebar-drag-zone');
            const maximizeButton = titlebar.querySelector('[data-action="maximize"]');
            const syncMaximizeState = async () => {
                if (!maximizeButton) return;
                try {
                    const maximized = await invoke('window_is_maximized');
                    maximizeButton.dataset.maximized = maximized ? 'true' : 'false';
                    maximizeButton.title = maximized ? i18n.restoreTitle : i18n.maximizeActionTitle;
                    maximizeButton.setAttribute('aria-label', maximized ? i18n.restoreLabel : i18n.maximizeLabelText);
                    maximizeButton.querySelector('.svg-maximize').style.display = maximized ? 'none' : '';
                    maximizeButton.querySelector('.svg-restore').style.display = maximized ? '' : 'none';
                } catch (e) {
                    console.error('Failed to sync maximize state', e);
                }
            };
            titlebar.querySelectorAll('button[data-action]').forEach(button => {
                button.addEventListener('click', async event => {
                    event.stopPropagation();
                    try {
                        switch (button.dataset.action) {
                            case 'hide': await invoke('window_hide'); break;
                            case 'minimize': await invoke('window_minimize'); break;
                            case 'maximize': await invoke('window_toggle_maximize'); await syncMaximizeState(); break;
                            case 'close': await invoke('window_close'); break;
                        }
                    } catch (error) {
                        console.error('Failed to handle ' + button.dataset.action + ' window action', error);
                    }
                });
            });
            dragZone.addEventListener('mousedown', event => {
                if (event.button !== 0 || event.target.closest('button')) return;
                invoke('window_start_dragging').catch(error => { console.error('Failed to start dragging from titlebar', error); });
            });
            dragZone.addEventListener('dblclick', async event => {
                if (event.target.closest('button')) return;
                try { await invoke('window_toggle_maximize'); await syncMaximizeState(); }
                catch (error) { console.error('Failed to toggle maximize from titlebar', error); }
            });
            window.addEventListener('resize', () => { void syncMaximizeState(); });
            void syncMaximizeState();
        };
        ensureTitlebar();
        if (!document.body) {
            window.addEventListener('DOMContentLoaded', ensureTitlebar, { once: true });
        }
        "#);
        s
    }
}
