// No default console window createion on Windows
#![windows_subsystem = "windows"]

mod backend;
mod setup;
mod window_util;

use std::{
    cell::Cell,
    fs,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self},
};

use anyhow::{anyhow, Result};
use base64::{prelude::BASE64_STANDARD, Engine};
use serde_json::to_string;
use tauri::{
    image::Image,
    menu::{MenuBuilder, MenuItemBuilder},
    tray::TrayIconBuilder,
    webview::{PageLoadEvent, PageLoadPayload},
    Manager, Url, WebviewWindow,
};
use tauri_plugin_dialog::DialogExt;
#[cfg(windows)]
use tauri_plugin_dialog::MessageDialogButtons;
use tauri_plugin_dialog::FilePath;
use tracing::{debug, error, info, warn};

use crate::{
    backend::ManagedBackend,
    setup::{get_deploy_config, setup_alas_repo, setup_environment, SplashUpdate},
};

#[cfg(target_os = "macos")]
const MENUBAR_ICON_2X: &[u8] = include_bytes!("../icons/menubar@2x.png");
#[cfg(target_os = "macos")]
const MENUBAR_ICON_1X: &[u8] = include_bytes!("../icons/menubar.png");
#[cfg(windows)]
const WINDOWS_TRAY_ICON: &[u8] = include_bytes!("../icons/icon.png");

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
    Image::from_bytes(WINDOWS_TRAY_ICON)
        .unwrap_or_else(|err| {
            error!(?err, "Failed to load tray icon from embedded icon bytes.");
            panic!("Failed to load tray icon from embedded icon bytes: {err}");
        })
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
    // Initialize logger with debug level support
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();
    
    info!("=== Alas Launcher starting ===");
    setup_environment()?;

    let port = get_deploy_config()
        .as_ref()
        .and_then(|config| config.get("Deploy"))
        .and_then(|deploy| deploy.get("Webui"))
        .and_then(|webui| webui.get("WebuiPort"))
        .and_then(|port| port.as_u64());
    if port.is_none() {
        warn!("WebuiPort not found in config, using default port 22267");
    }
    let port = port.unwrap_or(22267) as u16;

    let backend = Arc::new(Mutex::new(None));
    let allow_exit = Arc::new(AtomicBool::new(false));
    #[cfg(windows)]
    let close_prompt_active = Arc::new(AtomicBool::new(false));

    let allow_exit_for_setup = allow_exit.clone();
    #[cfg(windows)]
    let close_prompt_active_for_run = close_prompt_active.clone();

    info!("Starting Webview...");
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            save_as,
            window_minimize,
            window_toggle_maximize,
            window_close,
            window_start_dragging,
            window_is_maximized
        ])
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                #[cfg(target_os = "macos")]
                set_macos_activation_policy(app, true);
                let _ = reveal_window(&window);
            }
        }))
        .setup(move |app| {
            let main_window = tauri::WebviewWindowBuilder::from_config(
                app,
                app.config()
                    .app
                    .windows
                    .iter()
                    .find(|w| w.label == "main")
                    .unwrap(),
            )?
            .on_page_load(page_load_injector)
            .build()?;
            main_window.set_resizable(true)?;

            // Windows/Linux: remove native decorations for main window only
            // Splash window keeps native decorations (title bar)
            #[cfg(not(target_os = "macos"))]
            {
                main_window.set_decorations(false)?;
            }

            // Windows and macOS: create system tray
            #[cfg(any(windows, target_os = "macos"))]
            {
                info!("Creating system tray...");
                let allow_exit = allow_exit_for_setup.clone();
                let show_item = MenuItemBuilder::new("Show / Hide")
                    .id("toggle_visibility")
                    .build(app)?;
                let quit_item = MenuItemBuilder::new("Quit")
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
                    .tooltip("Alas Launcher")
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
                            toggle_main_window_visibility(app);
                        }
                        "quit" => {
                            allow_exit.store(true, Ordering::SeqCst);
                            app.exit(0);
                        }
                        _ => {}
                    }
                    })
                    .on_tray_icon_event(|tray, event| {
                        if let tauri::tray::TrayIconEvent::Click {
                            button: tauri::tray::MouseButton::Left,
                            button_state: tauri::tray::MouseButtonState::Up,
                            ..
                        } = event
                        {
                            let app = tray.app_handle();
                            toggle_main_window_visibility(&app);
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
                    debug!("RunEvent::Ready");
                    let allow_exit = allow_exit.clone();
                    let handle1 = app_handle.clone();
                    ctrlc::set_handler(move || {
                        allow_exit.store(true, Ordering::SeqCst);
                        handle1.exit(0);
                    }).expect("Error setting Ctrl-C handler");
                    let app_handle = app_handle.clone();
                    let backend = backend.clone();
                    thread::spawn(move || {
                        let splash = app_handle.get_webview_window("splash").unwrap();
                        initialize_splash(&splash);
                        let last_progress = Cell::new(0u8);
                        let mut status_updater = |update: SplashUpdate| {
                            last_progress.set(update.progress);
                            update_splash(&splash, &update);
                        };

                        status_updater(SplashUpdate::loading(
                            "Starting up",
                            "The local WebUI is initializing. The window will open automatically when ready.",
                            4,
                        ));
                        if let Err(e) = setup_alas_repo(&mut status_updater) {
                            error!("{e}");
                            status_updater(SplashUpdate::error(
                                "Launch failed",
                                format!(
                                    "Unable to prepare ALAS. Please run alas-launcher from Terminal for the detailed error log.\n\n{}",
                                    e
                                ),
                                last_progress.get().max(8),
                            ));
                            return;
                        }
                        info!("Starting gui.py on http://127.0.0.1:{}/", port);
                        status_updater(SplashUpdate::loading(
                            "Starting up",
                            "The local WebUI is initializing. This usually takes a few seconds. The window will open automatically when ready.",
                            97,
                        ));
                        let b = match ManagedBackend::new(port) {
                            Ok(backend) => backend,
                            Err(e) => {
                                error!("{e}");
                                status_updater(SplashUpdate::error(
                                    "Launch failed",
                                    format!(
                                        "Unable to start the local service. Check whether the configured port is already in use.\n\n{}",
                                        e
                                    ),
                                    last_progress.get().max(97),
                                ));
                                return;
                            }
                        };
                        *backend.lock().unwrap() = Some(b);
                        status_updater(SplashUpdate::loading(
                            "Opening window",
                            "The main window is ready and will appear now.",
                            100,
                        ));
                        // 🔑 关键改变：不销毁 splash 窗口，而是隐藏它作为后台保活窗口
                        // 这样当主窗口隐藏到托盘时，应用不会因为"最后一个窗口关闭"而退出
                        let _ = splash.hide();
                        debug!("Hidden splash window to keep app alive in background");
                        
                        info!("Webview is ready");
                        let window = app_handle.get_webview_window("main").unwrap();
                        window.set_resizable(true).unwrap();
                        window
                            .navigate(Url::parse(&format!("http://127.0.0.1:{}/", port)).unwrap())
                            .unwrap();
                        reveal_window(&window).unwrap();
                    });
                }
                tauri::RunEvent::ExitRequested { api, .. } => {
                    let should_allow = allow_exit.load(Ordering::SeqCst);
                    debug!("ExitRequested event: allow_exit={}", should_allow);
                    
                    // Only exit if explicitly allowed (e.g., via tray menu Quit)
                    if !should_allow {
                        api.prevent_exit();
                        debug!("Hiding main window to tray");
                        // Hide main window instead of exiting
                        if let Some(w) = app_handle.get_webview_window("main") {
                            let _ = w.hide();
                        }
                        #[cfg(target_os = "macos")]
                        {
                            set_macos_activation_policy(&app_handle, false);
                        }
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
                tauri::RunEvent::WindowEvent { label, event: tauri::WindowEvent::CloseRequested { ref api, .. }, .. } => {
                    debug!("Window {} close requested", label);
                    
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
                                            } else if let Some(w) =
                                                app_handle_for_dialog.get_webview_window("main")
                                            {
                                                let _ = w.hide();
                                            }
                                        });
                                } else {
                                    close_prompt_active_for_run.store(false, Ordering::SeqCst);
                                    if let Some(w) = app_handle.get_webview_window("main") {
                                        let _ = w.hide();
                                    }
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
                            if let Some(w) = app_handle.get_webview_window("main") {
                                let _ = w.hide();
                            }
                            set_macos_activation_policy(&app_handle, false);
                            return;
                        }
                    }
                    
                    // Linux: just hide to tray
                    #[cfg(target_os = "linux")]
                    {
                        if label == "main" && !allow_exit.load(Ordering::SeqCst) {
                            api.prevent_close();
                            if let Some(w) = app_handle.get_webview_window("main") {
                                let _ = w.hide();
                            }
                            return;
                        }
                    }
                }

                _ => {}
            };
        });
    Ok(())
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
    let html_json = to_string(&splash_shell_html()).unwrap();
    let injected = format!("document.open();document.write({html_json});document.close();");
    if let Err(e) = splash.eval(&injected) {
        error!("Failed to initialize splash page: {:?}", e);
    }
}

fn update_splash(splash: &WebviewWindow, update: &SplashUpdate) {
    let payload = to_string(update).unwrap();
    let script = format!("window.__ALAS_SPLASH_UPDATE && window.__ALAS_SPLASH_UPDATE({payload});");
    if let Err(e) = splash.eval(&script) {
        error!("Failed to update splash page: {:?}", e);
    }
}

fn splash_shell_html() -> String {
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
    color-scheme: light;
    --color-background-primary: rgba(255, 255, 255, 0.9);
    --color-background-secondary: #eef3f8;
    --color-border-tertiary: rgba(196, 206, 219, 0.92);
    --color-text-primary: #1f2a37;
    --color-text-secondary: #617084;
    --border-radius-lg: 20px;
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
  }}


  .wrap {{
    width: 100%;
    height: 100%;
    display: flex;
    justify-content: center;
    align-items: center;
    padding-top: 0;
  }}
  .card {{
    width: calc(100% - 44px);
    padding: 1.2rem 1.35rem 1.1rem;
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
    display: block;
    font-size: 15px;
    font-weight: 500;
    color: var(--color-text-primary);
    margin-bottom: 2px;
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
  .prog-pct {{
    font-weight: 500;
    color: #185fa5;
    font-variant-numeric: tabular-nums;
  }}
  .prog-pct-err {{
    color: #a32d2d;
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
          <strong>ALAS Launcher</strong>
          <span id="subtitle"></span>
        </div>
        <div id="badge" class="badge">Loading</div>
      </div>
      <div class="divider"></div>
      <div class="status-row">
        <div id="spinner" class="spinner"></div>
        <div id="error-dot" class="err-dot" style="display:none;">!</div>
        <div class="status-body">
          <h2 id="title"></h2>
          <p id="detail"></p>
        </div>
      </div>
      <div class="prog-wrap">
        <div class="prog-track">
          <div id="progress-fill" class="prog-fill" style="width: 0%;"></div>
        </div>
        <div class="prog-meta">
          <span id="progress-meta">The window opens automatically when ready</span>
          <span id="progress-pct" class="prog-pct">0%</span>
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

      document.getElementById('subtitle').textContent = payload.subtitle || '';
      document.getElementById('title').textContent = payload.title || '';
      document.getElementById('detail').textContent = payload.detail || '';
      document.getElementById('progress-meta').textContent = payload.is_error
        ? 'Stopped during initialization'
        : 'The window opens automatically when ready';

      const progress = Math.max(0, Math.min(100, Number(payload.progress || 0)));
      progressFill.style.width = progress + '%';
      progressPct.textContent = progress + '%';

      if (payload.is_error) {{
        badge.textContent = 'Error';
        badge.className = 'badge badge-err';
        spinner.style.display = 'none';
        errorDot.style.display = 'flex';
        progressFill.className = 'prog-fill prog-fill-err';
        progressPct.className = 'prog-pct prog-pct-err';
      }} else {{
        badge.textContent = 'Loading';
        badge.className = 'badge';
        spinner.style.display = 'block';
        errorDot.style.display = 'none';
        progressFill.className = 'prog-fill';
        progressPct.className = 'prog-pct';
      }}
    }};
  </script>
</body>
</html>"#,
        splash_script = splash_script,
        splash_titlebar = splash_titlebar,
    );

    html
}

fn reveal_window(window: &WebviewWindow) -> tauri::Result<()> {
    if window.is_minimized()? {
        window.unminimize()?;
    }
    window.show()?;
    window.set_focus()?;
    Ok(())
}

fn toggle_main_window_visibility(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let is_visible = window.is_visible().unwrap_or(false);
        let is_minimized = window.is_minimized().unwrap_or(false);
        if is_visible && !is_minimized {
            let _ = window.hide();
            #[cfg(target_os = "macos")]
            set_macos_activation_policy(app, false);
        } else {
            #[cfg(target_os = "macos")]
            set_macos_activation_policy(app, true);
            let _ = reveal_window(&window);
        }
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
                        inset: 0 88px 0 0;
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
