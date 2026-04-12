// No default console window createion on Windows
#![windows_subsystem = "windows"]

mod backend;
mod setup;
mod window_util;

use std::{
    cell::Cell,
    fs,
    sync::{Arc, Mutex},
    thread::{self},
};

use anyhow::{anyhow, Result};
use base64::{prelude::BASE64_STANDARD, Engine};
use tauri::{
    webview::{PageLoadEvent, PageLoadPayload},
    Manager, Url, WebviewWindow,
};
use tauri_plugin_dialog::{DialogExt, FilePath};
use tracing::{error, info, warn};

use crate::{
    backend::ManagedBackend,
    setup::{get_deploy_config, setup_alas_repo, setup_environment, SplashUpdate},
};

fn main() -> Result<()> {
    #[cfg(windows)]
    unsafe {
        use crate::window_util::HAS_CONSOLE;
        use std::sync::atomic::Ordering;
        use winapi::um::wincon::{AttachConsole, ATTACH_PARENT_PROCESS};
        HAS_CONSOLE.store(AttachConsole(ATTACH_PARENT_PROCESS) != 0, Ordering::Relaxed);
    }
    tracing_subscriber::fmt::init();
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

    info!("Starting Webview...");
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![save_as])
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            let _ = app
                .get_webview_window("main")
                .and_then(|w| w.set_focus().ok());
        }))
        .setup(|app| {
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
            Ok(())
        })
        .build(tauri::generate_context!())?
        .run(move |app_handle, event| {
            match event {
                tauri::RunEvent::Ready => {
                    let handle1 = app_handle.clone();
                    ctrlc::set_handler(move || {
                        info!("Received Ctrl-C, shutting down...");
                        handle1.exit(0);
                    }).expect("Error setting Ctrl-C handler");
                    let app_handle = app_handle.clone();
                    let backend = backend.clone();
                    thread::spawn(move || {
                        let splash = app_handle.get_webview_window("splash").unwrap();
                        let last_progress = Cell::new(0u8);
                        let mut status_updater = |update: SplashUpdate| {
                            last_progress.set(update.progress);
                            let url = Url::parse(&text_to_splash(&update)).unwrap();
                            splash.navigate(url).unwrap();
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
                        splash.destroy().unwrap();
                        info!("Webview is ready");
                        let window = app_handle.get_webview_window("main").unwrap();
                        window.set_resizable(true).unwrap();
                        window
                            .navigate(Url::parse(&format!("http://127.0.0.1:{}/", port)).unwrap())
                            .unwrap();
                        window.show().unwrap();
                    });
                }
                tauri::RunEvent::ExitRequested { .. } => {
                    info!("Webview closed, shutting down backend...");
                    if let Some(ref mut b) = *backend.lock().unwrap() {
                        if let Err(e) = b.terminate() {
                            warn!("Failed to terminate backend process: {:?}", e);
                        }
                    }
                }
                tauri::RunEvent::WindowEvent { label, event: tauri::WindowEvent::CloseRequested { .. }, .. } => {
                    info!("Window {} closed", label);
                    app_handle.exit(0);
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
                window.__TAURI__.core.invoke('save_as', { filename, data });
            };
            reader.readAsDataURL(blob);
        };
    })();
}
"#;
        if let Err(e) = webview.eval(injected_js) {
            error!("Failed to inject JS to webview: {:?}", e);
        }
    }
}

fn text_to_splash(update: &SplashUpdate) -> String {
    let badge_class = if update.is_error {
        "badge badge-err"
    } else {
        "badge"
    };
    let badge_text = if update.is_error { "Error" } else { "Loading" };
    let indicator = if update.is_error {
        "<div class=\"err-dot\">!</div>"
    } else {
        "<div class=\"spinner\"></div>"
    };
    let progress_class = if update.is_error {
        "prog-fill prog-fill-err"
    } else {
        "prog-fill"
    };
    let progress_pct_class = if update.is_error {
        "prog-pct prog-pct-err"
    } else {
        "prog-pct"
    };
    let progress_meta = if update.is_error {
        "Stopped during initialization"
    } else {
        "The window opens automatically when ready"
    };
    let detail_html = escape_html(&update.detail).replace('\n', "<br>");
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
  }}
  .wrap {{
    width: 100%;
    height: 100%;
    display: flex;
    justify-content: center;
    align-items: center;
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
  <div class="wrap">
    <div class="card">
      <div class="card-header">
        <div class="brand-text">
          <strong>ALAS Launcher</strong>
          <span>{subtitle}</span>
        </div>
        <div class="{badge_class}">{badge_text}</div>
      </div>
      <div class="divider"></div>
      <div class="status-row">
        {indicator}
        <div class="status-body">
          <h2>{title}</h2>
          <p>{detail_html}</p>
        </div>
      </div>
      <div class="prog-wrap">
        <div class="prog-track">
          <div class="{progress_class}" style="width: {progress}%;"></div>
        </div>
        <div class="prog-meta">
          <span>{progress_meta}</span>
          <span class="{progress_pct_class}">{progress}%</span>
        </div>
      </div>
    </div>
  </div>
</body>
</html>"#,
        subtitle = escape_html(&update.subtitle),
        badge_class = badge_class,
        badge_text = badge_text,
        indicator = indicator,
        title = escape_html(&update.title),
        detail_html = detail_html,
        progress_class = progress_class,
        progress = update.progress.min(100),
        progress_meta = progress_meta,
        progress_pct_class = progress_pct_class
    );

    let b64 = BASE64_STANDARD.encode(html.as_bytes());
    format!("data:text/html;charset=utf-8;base64,{}", b64)
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}
