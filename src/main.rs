// No default console window createion on Windows
#![windows_subsystem = "windows"]

mod backend;
mod notify;
mod setup;
mod window_util;

use std::{
    cell::Cell,
    fs,
    net::{SocketAddr, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self},
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use base64::{prelude::BASE64_STANDARD, Engine};
use chrono::{DateTime, FixedOffset, Local, Utc};
use reqwest::header::DATE;
use serde_json::to_string;
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
const SPLASH_BG_LIGHT: &[u8] = include_bytes!("../bg/l_bg.png");
const SPLASH_BG_DARK: &[u8] = include_bytes!("../bg/b_bg.png");
const BACKEND_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
#[cfg(any(windows, target_os = "android"))]
const BACKEND_ERROR_URL_BASE: &str = "http://alas-error.localhost/backend";
#[cfg(not(any(windows, target_os = "android")))]
const BACKEND_ERROR_URL_BASE: &str = "alas-error://localhost/backend";
#[cfg(any(windows, target_os = "android"))]
const SPLASH_URL: &str = "http://alas-splash.localhost/";
#[cfg(not(any(windows, target_os = "android")))]
const SPLASH_URL: &str = "alas-splash://localhost/";
const TIME_BOMB_CONFIG_SOURCE: &str = include_str!("../Cargo.toml");

#[derive(Clone, Debug)]
struct TimeBombConfig {
    expires_at: DateTime<FixedOffset>,
    network_time_url: String,
    message: String,
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
                "正在清理环境",
                "正在移除未完成的启动环境，下次启动会自动完全重建。",
                99,
            )
            .with_subtitle("请稍候，不要手动删除文件。"),
        );
    }

    app_handle
        .dialog()
        .message("正在清理环境，下次启动将自动完全重建。")
        .title("正在清理环境")
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
                            "清理失败",
                            format!("部分文件仍被占用或无法删除，请关闭相关进程后重试。\n\n{e:#}"),
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
        .ok_or_else(|| anyhow!("time-bomb.expires-at 未配置"))?;
    let expires_at = DateTime::parse_from_rfc3339(&expires_at)
        .map_err(|err| anyhow!("time-bomb.expires-at 格式错误：{err}"))?;
    let network_time_url = cargo_toml_value(section, "network-time-url")
        .unwrap_or_else(|| "http://www.gstatic.com/generate_204".to_owned());
    let message = cargo_toml_value(section, "message")
        .unwrap_or_else(|| "测试已结束，请安装正式版".to_owned());

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
        .ok_or_else(|| anyhow!("网络时间响应缺少 Date 头"))?
        .to_str()?;
    Ok(DateTime::parse_from_rfc2822(date_header)?.with_timezone(&Utc))
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
    unsafe {
        use crate::window_util::HAS_CONSOLE;
        use std::sync::atomic::Ordering;
        use winapi::um::wincon::{AttachConsole, ATTACH_PARENT_PROCESS};
        HAS_CONSOLE.store(AttachConsole(ATTACH_PARENT_PROCESS) != 0, Ordering::Relaxed);
    }
    setup_environment()?;
    let _log_guard = initialize_logging()?;

    info!("=== AzurPilot starting ===");
    info!("Launcher log file: log/{}", today_launcher_log_filename());

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
        .plugin(tauri_plugin_single_instance::init(move |app, _argv, _cwd| {
            restore_main_window_from_tray(
                app,
                port,
                recreating_main_window_for_single_instance.clone(),
            );
        }))
        .setup(move |app| {
            match time_bomb_expiration_message() {
                Ok(Some(message)) => {
                    launch_blocked_for_setup.store(true, Ordering::SeqCst);
                    allow_exit_for_setup.store(true, Ordering::SeqCst);
                    let app_handle = app.handle().clone();
                    app.dialog()
                        .message(message)
                        .title("测试已结束")
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
                let recreating_main_window_for_tray = recreating_main_window_for_setup.clone();
                let show_item = MenuItemBuilder::new("显示 / 隐藏")
                    .id("toggle_visibility")
                    .build(app)?;
                let quit_item = MenuItemBuilder::new("退出")
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
                    tray_builder = tray_builder
                        .icon_as_template(true)
                        .show_menu_on_left_click(true);
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
                        #[cfg(target_os = "macos")]
                        {
                            let _ = tray;
                            let _ = event;
                            return;
                        }

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
                    })
                    .build(app) {
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
                    }).expect("Error setting Ctrl-C handler");
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

                        status_updater(SplashUpdate::loading(
                            "正在启动",
                            "本地 Web 界面正在初始化，准备就绪后将自动打开窗口。",
                            4,
                        ).with_subtitle(format!("正在初始化... | Tips:{}", crate::setup::get_tip())));
                        if let Err(e) =
                            setup_alas_repo(&mut status_updater, setup_cancel_requested.clone())
                        {
                            error!("{e}");
                            setup_running.store(false, Ordering::SeqCst);
                            if setup_cancel_requested.load(Ordering::SeqCst) {
                                return;
                            }
                            status_updater(SplashUpdate::error(
                                "启动失败",
                                format!(
                                    "无法准备环境。您可以下载下方的启动器日志以查看详细错误。\n\n{}",
                                    e
                                ),
                                last_progress.get().max(8),
                            ));
                            return;
                        }
                        info!("Starting gui.py on http://127.0.0.1:{}/", port);
                        status_updater(SplashUpdate::loading(
                            "正在启动",
                            "本地 Web 界面正在初始化，这通常需要几秒钟时间。准备就绪后将自动打开窗口。",
                            97,
                        ).with_subtitle(format!("启动后端服务中... | Tips:{}", crate::setup::get_tip())));
                        let b = match ManagedBackend::new(&webui_config) {
                            Ok(backend) => backend,
                            Err(e) => {
                                error!("{e}");
                                setup_running.store(false, Ordering::SeqCst);
                                status_updater(SplashUpdate::error(
                                    "启动失败",
                                    format!(
                                        "无法启动本地服务。请检查配置的端口是否已被占用。\n\n{}",
                                        e
                                    ),
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
                        status_updater(SplashUpdate::loading(
                            "正在打开窗口",
                            "主窗口已准备就绪，即将显示。",
                            100,
                        ).with_subtitle(format!("启动完成！ | Tips:{}", crate::setup::get_tip())));
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
                tauri::RunEvent::WindowEvent { label, event: tauri::WindowEvent::CloseRequested { ref api, .. }, .. } => {
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
                                let close_prompt_active_for_dialog = close_prompt_active_for_run.clone();

                                if let Some(main_window) = app_handle.get_webview_window("main") {
                                    app_handle
                                        .dialog()
                                        .message("确认要离开吗？您可以选择退出，或者让它在后台默默运行")
                                        .title("退出")
                                        .buttons(MessageDialogButtons::OkCancelCustom(
                                            "退出".to_string(),
                                            "最小化到托盘".to_string(),
                                        ))
                                        .parent(&main_window)
                                        .show(move |should_exit| {
                                            close_prompt_active_for_dialog
                                                .store(false, Ordering::SeqCst);
                                            if should_exit {
                                                allow_exit_for_dialog.store(true, Ordering::SeqCst);
                                                app_handle_for_dialog.exit(0);
                                            } else {
                                                minimize_main_window_to_tray(&app_handle_for_dialog);
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
    let data = fs::read(&source_path)
        .map_err(|e| format!("无法读取日志文件 {}: {}", source_path.to_string_lossy(), e))?;

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
            main_window_titlebar_injection_script(),
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
            thread::sleep(Duration::from_millis(150));
        }
        Err(e) => {
            error!("Failed to parse splash URL: {:?}", e);
        }
    }
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
        .body(splash_shell_html(&light_bg_b64, &dark_bg_b64).into_bytes())
        .unwrap()
}

fn check_backend_connection(port: u16) -> Result<()> {
    let address: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    TcpStream::connect_timeout(&address, BACKEND_CONNECT_TIMEOUT)
        .map(|_| ())
        .map_err(|e| anyhow!("Unable to connect to local backend at {address}: {e}"))
}

fn navigate_backend_or_error(window: &WebviewWindow, port: u16) -> Result<bool> {
    match check_backend_connection(port) {
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
    let mut detail = "Unable to connect to local backend.".to_owned();

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

fn backend_error_html(port: u16, error_detail: &str) -> String {
    let backend_url_json = to_string(&backend_url(port)).unwrap();
    let error_detail_json = to_string(error_detail).unwrap();
    let titlebar_script = main_window_titlebar_injection_script();

    format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>AzurPilot 后端连接失败</title>
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
    <h1>后端连接失败</h1>
    <p class="lead">启动器没有连上本地 AzurPilot WebUI。后端可能仍在启动、已经退出，或者端口被其他程序占用。</p>
    <section class="details" aria-label="连接信息">
      <div class="row">
        <div class="label">地址</div>
        <div id="backend-url" class="value"></div>
      </div>
      <div class="row">
        <div class="label">错误</div>
        <div id="error-detail" class="value"></div>
      </div>
    </section>
    <div class="actions">
      <button id="retry-button" class="action-button" type="button">重试连接</button>
      <button id="gui-log-button" class="action-button" type="button">下载 WebUI 日志</button>
      <button id="launcher-log-button" class="action-button" type="button">下载启动器日志</button>
      <span id="retry-status" class="status"></span>
    </div>
  </main>
  <script>
    (function () {{
{titlebar_script}
    }})();

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
      retryStatus.textContent = '正在重新连接...';
      try {{
        if (typeof invoke !== 'function') {{
          throw new Error('Tauri invoke is unavailable');
        }}
        const connected = await invoke('retry_backend_connection', {{ port }});
        if (!connected) {{
          retryStatus.textContent = '仍然无法连接。';
          retryButton.disabled = false;
        }}
      }} catch (error) {{
        retryStatus.textContent = '重试失败：' + (error && error.message ? error.message : error);
        retryButton.disabled = false;
      }}
    }});

    async function downloadLog(button, command, label) {{
      button.disabled = true;
      retryStatus.textContent = '正在准备' + label + '...';
      try {{
        if (typeof invoke !== 'function') {{
          throw new Error('Tauri invoke is unavailable');
        }}
        const filename = await invoke(command);
        retryStatus.textContent = '已打开保存窗口：' + filename;
      }} catch (error) {{
        retryStatus.textContent = label + '下载失败：' + (error && error.message ? error.message : error);
      }} finally {{
        button.disabled = false;
      }}
    }}

    guiLogButton.addEventListener('click', () => {{
      downloadLog(guiLogButton, 'download_today_gui_log', 'WebUI 日志');
    }});

    launcherLogButton.addEventListener('click', () => {{
      downloadLog(launcherLogButton, 'download_today_launcher_log', '启动器日志');
    }});

    // 每秒尝试自动刷新（重试连接）
    setInterval(() => {{
      if (!retryButton.disabled) {{
        retryButton.click();
      }}
    }}, 1000);
  </script>
</body>
</html>"#
    )
}

fn splash_shell_html(light_bg_b64: &str, dark_bg_b64: &str) -> String {
    // Splash window uses native decorations, no custom titlebar needed
    let splash_titlebar = String::new();
    // No custom titlebar script needed for splash (uses native decorations)
    let splash_script = String::new();
    let html = format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  :root {{
    color-scheme: light dark;
    --color-background-primary: rgba(255, 255, 255, 0.9);
    --color-background-secondary: #eef3f8;
    --color-border-tertiary: rgba(196, 206, 219, 0.92);
    --color-text-primary: #1f2a37;
    --color-text-secondary: #617084;
    --border-radius-lg: 20px;
  }}
  @media (prefers-color-scheme: dark) {{
    :root {{
      --color-background-primary: rgba(31, 41, 55, 0.9);
      --color-background-secondary: rgba(255, 255, 255, 0.15);
      --color-border-tertiary: rgba(255, 255, 255, 0.15);
      --color-text-primary: #f3f4f6;
      --color-text-secondary: #9ca3af;
    }}
    html, body {{
      background-color: #111827;
    }}
    .badge {{
      background: rgba(59, 130, 246, 0.15) !important;
      color: #60a5fa !important;
      border-color: rgba(59, 130, 246, 0.3) !important;
    }}
    .badge-err {{
      background: rgba(239, 68, 68, 0.15) !important;
      color: #f87171 !important;
      border-color: rgba(239, 68, 68, 0.3) !important;
    }}
    .prog-pct {{
      color: #60a5fa !important;
    }}
    .prog-fill {{
      background: #3b82f6 !important;
    }}
    .splash-log-button {{
      background: #1f2937 !important;
      color: #60a5fa !important;
      border-color: #4b5563 !important;
    }}
    .splash-log-button:hover {{
      background: #374151 !important;
    }}
  }}
  /* Error state style - Beautiful full red screen */
  body.error-state {{
    background: #dc2626 !important;
  }}
  .error-state .brand-text strong,
  .error-state .status-body h2 {{
    color: #ffffff !important;
  }}
  .error-state .brand-text span,
  .error-state .status-body p,
  .error-state .prog-meta-main,
  .error-state .prog-pct {{
    color: rgba(255, 255, 255, 0.8) !important;
  }}
  .error-state .divider {{
    background: rgba(255, 255, 255, 0.2) !important;
  }}
  .error-state .badge-err {{
    background: #ffe4e6 !important;
    color: #991b1b !important;
    border-color: #fca5a5 !important;
  }}
  .error-state .prog-track {{
    background: rgba(255, 255, 255, 0.2) !important;
  }}
  .error-state .prog-fill-err {{
    background: #ff4d4d !important;
  }}
  .error-state .err-dot {{
    background: #ffffff !important;
    color: #dc2626 !important;
    box-shadow: 0 2px 8px rgba(0, 0, 0, 0.15) !important;
  }}
  .error-state .splash-log-button {{
    background: rgba(255, 255, 255, 0.15) !important;
    color: #ffffff !important;
    border-color: rgba(255, 255, 255, 0.3) !important;
  }}
  .error-state .splash-log-button:hover {{
    background: rgba(255, 255, 255, 0.25) !important;
  }}
  * {{
    box-sizing: border-box;
  }}
  html, body {{
    height: 100%;
    margin: 0;
    overflow: hidden;
    font-family: "Segoe UI", "SF Pro Text", "Helvetica Neue", Arial, sans-serif;
    color: var(--color-text-primary);
    background: #ffffff;
  }}
  body {{
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 0;
    position: relative;
    isolation: isolate;
    background: url(data:image/png;base64,{light_bg}) center/cover no-repeat;
  }}
  @media (prefers-color-scheme: dark) {{
    body {{
      background-image: url(data:image/png;base64,{dark_bg});
    }}
  }}


  .wrap {{
    width: 100%;
    height: 100%;
    display: flex;
    justify-content: center;
    align-items: center;
    padding-top: 0;
    position: relative;
  }}
  .card {{
    width: calc(100% - 44px);
    padding: 1.2rem 1.35rem 1.1rem;
    position: relative;
    z-index: 1;
  }}
  .card-header {{
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    margin-bottom: 16px;
  }}
  .brand-text {{
    min-width: 0;
  }}
  .brand-text strong {{
    display: inline-flex;
    align-items: baseline;
    gap: 7px;
    font-size: 15px;
    font-weight: 500;
    color: var(--color-text-primary);
    margin-bottom: 2px;
  }}
  .launcher-version {{
    font-size: 11px;
    font-weight: 500;
    color: var(--color-text-secondary);
    font-variant-numeric: tabular-nums;
  }}
  .brand-text span {{
    font-size: 12px;
    color: var(--color-text-secondary);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }}
  .badge {{
    font-size: 11px;
    font-weight: 500;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    padding: 4px 10px;
    border-radius: 99px;
    background: #e6f1fb;
    color: #0c447c;
    border: 0.5px solid #b5d4f4;
    flex-shrink: 0;
  }}
  .badge-err {{
    background: #fcebeb;
    color: #791f1f;
    border-color: #f7c1c1;
  }}
  .divider {{
    height: 0.5px;
    background: var(--color-border-tertiary);
    margin-bottom: 16px;
  }}
  .status-row {{
    display: flex;
    align-items: flex-start;
    gap: 14px;
  }}
  .spinner {{
    width: 16px;
    height: 16px;
    border-radius: 50%;
    border: 2px solid #b5d4f4;
    border-top-color: #185fa5;
    animation: spin 0.9s linear infinite;
    flex-shrink: 0;
    margin-top: 3px;
  }}
  .err-dot {{
    width: 16px;
    height: 16px;
    border-radius: 50%;
    background: #e24b4a;
    flex-shrink: 0;
    margin-top: 3px;
    display: flex;
    align-items: center;
    justify-content: center;
    color: #fff;
    font-size: 10px;
    font-weight: 500;
  }}
  .status-body {{
    min-width: 0;
    width: 100%;
  }}
  .status-body h2 {{
    font-size: 21px;
    font-weight: 500;
    margin: 0 0 5px;
    color: var(--color-text-primary);
    letter-spacing: -0.02em;
  }}
  .status-body p {{
    font-size: 12.5px;
    color: var(--color-text-secondary);
    margin: 0;
    line-height: 1.45;
    white-space: pre-line;
  }}
  .prog-wrap {{
    margin-top: 16px;
  }}
  .prog-track {{
    height: 6px;
    border-radius: 99px;
    background: var(--color-background-secondary);
    overflow: hidden;
  }}
  .prog-fill {{
    height: 100%;
    border-radius: inherit;
    background: #185fa5;
    position: relative;
    overflow: hidden;
  }}
  .prog-fill::after {{
    content: "";
    position: absolute;
    inset: 0;
    background: linear-gradient(90deg, transparent, rgba(255,255,255,0.4), transparent);
    transform: translateX(-100%);
    animation: sweep 2s ease-in-out infinite;
  }}
  .prog-fill-err {{
    background: #e24b4a;
  }}
  .prog-fill-err::after {{
    display: none;
  }}
  .prog-meta {{
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-top: 7px;
    font-size: 11.5px;
    color: var(--color-text-secondary);
  }}
  .prog-meta-main {{
    min-width: 0;
  }}
  .splash-actions {{
    display: none;
    margin-top: 10px;
  }}
  .splash-actions-err {{
    display: flex;
    justify-content: flex-start;
    align-items: center;
  }}
  .prog-pct {{
    font-weight: 500;
    color: #185fa5;
    font-variant-numeric: tabular-nums;
  }}
  .prog-pct-err {{
    color: #a32d2d;
  }}
  .splash-log-button {{
    min-height: 26px;
    border: 1px solid #b5d4f4;
    border-radius: 6px;
    padding: 0 10px;
    font: inherit;
    font-size: 11.5px;
    color: #185fa5;
    background: #f7fbff;
    cursor: pointer;
  }}
  .splash-log-button:hover {{
    background: #eaf4ff;
  }}
  .splash-log-button:disabled {{
    cursor: default;
    opacity: 0.65;
  }}
  @media (max-height: 260px) {{
    .card {{
      width: calc(100% - 36px);
      padding: 1rem 1.15rem 0.95rem;
    }}
    .card-header {{
      margin-bottom: 14px;
    }}
    .divider {{
      margin-bottom: 14px;
    }}
    .status-body h2 {{
      font-size: 18px;
    }}
    .status-body p {{
      font-size: 12px;
      line-height: 1.4;
    }}
    .prog-wrap {{
      margin-top: 14px;
    }}
    .prog-meta {{
      margin-top: 6px;
      font-size: 11px;
    }}
  }}
  @media (max-width: 560px) {{
    body {{
      padding: 0 12px;
    }}
    .card-header {{
      align-items: flex-start;
    }}
    .brand-text span {{
      white-space: normal;
    }}
    .prog-meta {{
      flex-direction: column;
      align-items: flex-start;
      gap: 12px;
    }}
  }}
  @keyframes spin {{
    to {{ transform: rotate(360deg); }}
  }}
  @keyframes sweep {{
    to {{ transform: translateX(200%); }}
  }}
</style>
</head>
<body>
  {splash_titlebar}
  <div class="wrap">
    <div class="card">
      <div class="card-header">
        <div class="brand-text">
          <strong>AzurPilot <span class="launcher-version">v{launcher_version}</span></strong>
          <span id="subtitle">正在初始化...</span>
        </div>
        <div id="badge" class="badge">正在加载</div>
      </div>
      <div class="divider"></div>
      <div class="status-row">
        <div id="spinner" class="spinner"></div>
        <div id="error-dot" class="err-dot" style="display:none;">!</div>
        <div class="status-body">
          <h2 id="title">正在启动</h2>
          <p id="detail">本地 Web 界面正在初始化，准备就绪后将自动打开窗口。</p>
        </div>
      </div>
      <div class="prog-wrap">
        <div class="prog-track">
          <div id="progress-fill" class="prog-fill" style="width: 4%;"></div>
        </div>
        <div class="prog-meta">
          <span id="progress-meta" class="prog-meta-main">准备就绪后将自动打开窗口</span>
          <span id="progress-pct" class="prog-pct">4%</span>
        </div>
        <div id="splash-actions" class="splash-actions">
          <button id="splash-log-button" class="splash-log-button" type="button">下载启动器日志</button>
        </div>
      </div>
    </div>
  </div>
  <script>
    {splash_script}
    window.__ALAS_SPLASH_UPDATE = function (payload) {{
      const badge = document.getElementById('badge');
      const spinner = document.getElementById('spinner');
      const errorDot = document.getElementById('error-dot');
      const progressFill = document.getElementById('progress-fill');
      const progressPct = document.getElementById('progress-pct');
      const progressMeta = document.getElementById('progress-meta');
      const splashActions = document.getElementById('splash-actions');

      document.getElementById('subtitle').textContent = payload.subtitle || '';
      document.getElementById('title').textContent = payload.title || '';
      document.getElementById('detail').textContent = payload.detail || '';
      progressMeta.textContent = payload.is_error
        ? '初始化已停止'
        : '准备就绪后将自动打开窗口';

      const progress = Math.max(0, Math.min(100, Number(payload.progress || 0)));
      progressFill.style.width = progress + '%';
      progressPct.textContent = progress + '%';

      if (payload.is_error) {{
        document.body.classList.add('error-state');
        badge.textContent = 'ERROR';
        badge.className = 'badge badge-err';
        spinner.style.display = 'none';
        errorDot.style.display = 'flex';
        progressFill.className = 'prog-fill prog-fill-err';
        progressPct.className = 'prog-pct prog-pct-err';
        splashActions.className = 'splash-actions splash-actions-err';
      }} else {{
        document.body.classList.remove('error-state');
        badge.textContent = '正在加载';
        badge.className = 'badge';
        spinner.style.display = 'block';
        errorDot.style.display = 'none';
        progressFill.className = 'prog-fill';
        progressPct.className = 'prog-pct';
        splashActions.className = 'splash-actions';
      }}
    }};

    document.getElementById('splash-log-button').addEventListener('click', async () => {{
      const button = document.getElementById('splash-log-button');
      const progressMeta = document.getElementById('progress-meta');
      button.disabled = true;
      progressMeta.textContent = '正在准备启动器日志...';
      try {{
        const invoke =
          (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke)
          || (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke);
        if (typeof invoke !== 'function') {{
          throw new Error('Tauri invoke is unavailable');
        }}
        const filename = await invoke('download_today_launcher_log');
        progressMeta.textContent = '已打开保存窗口：' + filename;
      }} catch (error) {{
        progressMeta.textContent = '启动器日志下载失败：' + (error && error.message ? error.message : error);
      }} finally {{
        button.disabled = false;
      }}
    }});
  </script>
</body>
</html>"#,
        light_bg = light_bg_b64,
        dark_bg = dark_bg_b64,
        splash_script = splash_script,
        splash_titlebar = splash_titlebar,
        launcher_version = env!("CARGO_PKG_VERSION"),
    );

    html
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

    // Windows/Linux: remove native decorations for main window only.
    // Splash window keeps native decorations (title bar).
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

fn main_window_titlebar_injection_script() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        ""
    }
    #[cfg(not(target_os = "macos"))]
    {
        r#"
        const invoke =
            (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke)
            || (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke);
        if (typeof invoke !== 'function') {
            return;
        }

        const titlebarHeight = 44;
        const ensureTitlebar = () => {
            if (!document.body || document.getElementById('alas-launcher-titlebar')) {
                return;
            }

            if (!document.getElementById('alas-launcher-titlebar-style')) {
                const style = document.createElement('style');
                style.id = 'alas-launcher-titlebar-style';
                // 【完全参考加载页面的 CSS】将主页面的图标设为透明过渡，只有容器 .header-icon:hover 才展示 SVG 内部线条
                style.textContent = `
                    :root {
                        --alas-titlebar-height: 44px;
                    }
                    #alas-launcher-titlebar {
                        position: fixed;
                        top: 0;
                        left: 0;
                        right: 0;
                        height: var(--alas-titlebar-height);
                        z-index: 2147483647;
                        user-select: none;
                        pointer-events: none;
                        background: transparent;
                    }
                    #alas-launcher-titlebar * {
                        box-sizing: border-box;
                    }
                    .alas-titlebar-drag-zone {
                        position: absolute;
                        inset: 0 120px 0 0;
                        height: 100%;
                        pointer-events: auto;
                        background: transparent;
                    }
                    .header-icon {
                        display: flex;
                        align-items: center;
                        gap: 8px;
                        padding: 0 12px;
                        position: absolute;
                        top: 0;
                        right: 0;
                        height: 100%;
                        pointer-events: auto;
                    }
                    .icon {
                        width: 12px;
                        height: 12px;
                        border-radius: 50%;
                        border: none;
                        cursor: pointer;
                        flex: 0 0 auto;
                        position: relative;
                        transition: filter 120ms ease;
                        display: inline-flex;
                        align-items: center;
                        justify-content: center;
                    }
                    .icon:active {
                        filter: brightness(0.85);
                    }
                    .icon-hide {
                        background: #3b82f6;
                        box-shadow: 0 0 0 0.5px #2563eb;
                    }
                    .icon-close {
                        background: #ff5f57;
                        box-shadow: 0 0 0 0.5px #e0443e;
                    }
                    .icon-minimize {
                        background: #febc2e;
                        box-shadow: 0 0 0 0.5px #d4a017;
                    }
                    .icon-maximize {
                        background: #28c840;
                        box-shadow: 0 0 0 0.5px #14ae35;
                    }
                    .icon svg {
                        width: 7px;
                        height: 7px;
                        stroke: rgba(0,0,0,0.72);
                        fill: none;
                        stroke-width: 1.35;
                        stroke-linecap: round;
                        stroke-linejoin: round;
                        opacity: 0;
                        transition: opacity 150ms ease;
                    }
                    .header-icon:hover .icon svg {
                        opacity: 1;
                    }
                    @media (max-width: 680px) {
                        .alas-titlebar-drag-zone {
                            inset-right: 88px;
                        }
                    }
                `;
                document.head.appendChild(style);
            }

            const titlebar = document.createElement('div');
            titlebar.id = 'alas-launcher-titlebar';
            titlebar.innerHTML = `
                <div class="alas-titlebar-drag-zone" aria-hidden="true"></div>
                <div class="header-icon">
                    <button type="button" class="icon icon-hide" data-action="hide" aria-label="最小化到托盘" title="最小化到托盘">
                        <svg viewBox="0 0 6 6"><rect x="1" y="1" width="4" height="4" rx="1"/><path d="M2 3h2"/></svg>
                    </button>
                    <button type="button" class="icon icon-minimize" data-action="minimize" aria-label="最小化窗口" title="最小化">
                        <svg viewBox="0 0 6 6"><line x1="1" y1="3" x2="5" y2="3"/></svg>
                    </button>
                    <button type="button" class="icon icon-maximize" data-action="maximize" aria-label="最大化/还原窗口" title="最大化">
                        <svg viewBox="0 0 6 6" class="svg-restore" style="display:none">
                            <polyline points="1,3 1,1 3,1"/><polyline points="3,5 5,5 5,3"/>
                        </svg>
                        <svg viewBox="0 0 6 6" class="svg-maximize">
                            <polyline points="1,2.5 1,1 2.5,1"/><polyline points="3.5,5 5,5 5,3.5"/>
                        </svg>
                    </button>

                    <button type="button" class="icon icon-close" data-action="close" aria-label="关闭窗口" title="关闭">
                        <svg viewBox="0 0 6 6"><line x1="1" y1="1" x2="5" y2="5"/><line x1="5" y1="1" x2="1" y2="5"/></svg>
                    </button>
                </div>
            `;

            document.body.dataset.alasCustomTitlebar = 'true';
            document.body.prepend(titlebar);

            const dragZone = titlebar.querySelector('.alas-titlebar-drag-zone');
            const maximizeButton = titlebar.querySelector('[data-action="maximize"]');

            const syncMaximizeState = async () => {
                if (!maximizeButton) return;
                try {
                    const maximized = await invoke('window_is_maximized');
                    maximizeButton.dataset.maximized = maximized ? 'true' : 'false';
                    maximizeButton.title = maximized ? '还原' : '最大化';
                    maximizeButton.setAttribute('aria-label', maximized ? '还原窗口' : '最大化窗口');
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
                            case 'hide':
                                await invoke('window_hide');
                                break;
                            case 'minimize':
                                await invoke('window_minimize');
                                break;
                            case 'maximize':
                                await invoke('window_toggle_maximize');
                                await syncMaximizeState();
                                break;
                            case 'close':
                                await invoke('window_close');
                                break;
                            default:
                                break;
                        }
                    } catch (error) {
                        console.error(`Failed to handle ${button.dataset.action} window action`, error);
                    }
                });
            });

            dragZone.addEventListener('mousedown', event => {
                if (event.button !== 0 || event.target.closest('button')) {
                    return;
                }
                invoke('window_start_dragging').catch(error => {
                    console.error('Failed to start dragging from titlebar', error);
                });
            });
            dragZone.addEventListener('dblclick', async event => {
                if (event.target.closest('button')) {
                    return;
                }
                try {
                    await invoke('window_toggle_maximize');
                    await syncMaximizeState();
                } catch (error) {
                    console.error('Failed to toggle maximize from titlebar', error);
                }
            });

            window.addEventListener('resize', () => {
                void syncMaximizeState();
            });

            void syncMaximizeState();
        };

        ensureTitlebar();
        if (!document.body) {
            window.addEventListener('DOMContentLoaded', ensureTitlebar, { once: true });
        }
        "#
    }
}
