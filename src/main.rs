// No default console window createion on Windows
#![windows_subsystem = "windows"]

mod backend;
mod setup;
mod window_util;

use std::{
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
    setup::{get_deploy_config, setup_alas_repo, setup_environment},
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
                        let status_updater = |text: &str| {
                            let content = format!("Loading ALAS, please wait..\n\n{}", text);
                            let url = Url::parse(&text_to_splash(&content)).unwrap();
                            splash.navigate(url).unwrap();
                        };
                        status_updater("Initialize ALAS");
                        if let Err(e) = setup_alas_repo(&status_updater) {
                            error!("{e}");
                            let content = format!("Failed loading ALAS, reason: {}\n\nPlease run alas-launcher from terminal for detailed logs", e);
                            let url = Url::parse(&text_to_splash(&content)).unwrap();
                            splash.navigate(url).unwrap();
                            return;
                        }
                        info!("Starting gui.py on http://127.0.0.1:{}/", port);
                        status_updater("Starting GUI");
                        let b = ManagedBackend::new(port).unwrap();
                        *backend.lock().unwrap() = Some(b);
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

fn text_to_splash(s: &str) -> String {
    let normalized = s.replace('\r', "");
    let mut lines = normalized
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let banner = lines
        .first()
        .copied()
        .unwrap_or("Loading ALAS, please wait..");
    let progress = lines.iter().find_map(|line| parse_splash_progress(line));
    let is_error = normalized.contains("Failed loading ALAS");

    if !lines.is_empty() {
        lines.remove(0);
    }

    let status = lines
        .iter()
        .find(|line| !is_progress_bar_line(line))
        .copied()
        .unwrap_or(if is_error {
            "Startup failed"
        } else {
            "Preparing workspace"
        });
    let detail_lines = lines
        .into_iter()
        .filter(|line| !is_progress_bar_line(line) && *line != status)
        .take(3)
        .map(escape_html)
        .collect::<Vec<_>>();
    let detail_html = if detail_lines.is_empty() {
        if is_error {
            "Run alas-launcher from Terminal for the full error log.".to_owned()
        } else {
            "Checking the repository, Python environment, and local WebUI before opening the main window."
                .to_owned()
        }
    } else {
        detail_lines.join("<br>")
    };
    let progress_value = progress.unwrap_or(42).clamp(6, 100);
    let progress_class = if progress.is_some() {
        "progress-fill"
    } else {
        "progress-fill progress-fill-indeterminate"
    };
    let shell_class = if is_error {
        "shell shell-error"
    } else {
        "shell"
    };
    let badge = if is_error { "Error" } else { "Starting" };
    let indicator = if is_error {
        "<div class=\"indicator indicator-error\"></div>"
    } else {
        "<div class=\"indicator indicator-spin\"></div>"
    };
    let html = format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  :root {{
    color-scheme: light;
    --bg-a: #f7f1e7;
    --bg-b: #e7eef8;
    --panel: rgba(255, 255, 255, 0.78);
    --panel-border: rgba(17, 24, 39, 0.08);
    --text: #102033;
    --muted: #5a6b7e;
    --accent: #2762d8;
    --accent-soft: rgba(39, 98, 216, 0.14);
    --track: rgba(39, 98, 216, 0.14);
    --shadow: 0 20px 60px rgba(24, 36, 56, 0.14);
  }}
  * {{ box-sizing: border-box; }}
  html, body {{
    height: 100%;
    margin: 0;
    overflow: hidden;
    font-family: "Segoe UI", "SF Pro Text", "Helvetica Neue", Arial, sans-serif;
    color: var(--text);
    background:
      radial-gradient(circle at top left, rgba(255, 255, 255, 0.92), transparent 38%),
      radial-gradient(circle at bottom right, rgba(39, 98, 216, 0.12), transparent 32%),
      linear-gradient(135deg, var(--bg-a), var(--bg-b));
  }}
  body {{
    position: relative;
    display: grid;
    place-items: center;
  }}
  body::before,
  body::after {{
    content: "";
    position: absolute;
    border-radius: 999px;
    filter: blur(10px);
    opacity: 0.75;
  }}
  body::before {{
    width: 220px;
    height: 220px;
    top: -70px;
    right: 40px;
    background: rgba(39, 98, 216, 0.12);
  }}
  body::after {{
    width: 180px;
    height: 180px;
    left: -60px;
    bottom: -50px;
    background: rgba(255, 179, 71, 0.18);
  }}
  .shell {{
    position: relative;
    width: min(540px, calc(100vw - 32px));
    padding: 24px;
    border-radius: 24px;
    background: var(--panel);
    border: 1px solid var(--panel-border);
    box-shadow: var(--shadow);
    backdrop-filter: blur(18px);
  }}
  .shell-error {{
    --accent: #c94b4b;
    --accent-soft: rgba(201, 75, 75, 0.14);
    --track: rgba(201, 75, 75, 0.16);
  }}
  .header {{
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 18px;
  }}
  .brand {{
    display: flex;
    align-items: center;
    gap: 12px;
    min-width: 0;
  }}
  .brand-mark {{
    width: 42px;
    height: 42px;
    border-radius: 14px;
    display: grid;
    place-items: center;
    background: linear-gradient(135deg, var(--accent), #78a6ff);
    color: #fff;
    font-weight: 700;
    letter-spacing: 0.08em;
    box-shadow: inset 0 1px 0 rgba(255,255,255,0.35);
  }}
  .brand-copy {{
    min-width: 0;
  }}
  .brand-copy strong,
  .brand-copy span {{
    display: block;
  }}
  .brand-copy strong {{
    font-size: 15px;
    line-height: 1.2;
  }}
  .brand-copy span {{
    margin-top: 4px;
    color: var(--muted);
    font-size: 12px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }}
  .badge {{
    padding: 7px 10px;
    border-radius: 999px;
    background: var(--accent-soft);
    color: var(--accent);
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    flex-shrink: 0;
  }}
  .content {{
    display: flex;
    gap: 16px;
    align-items: flex-start;
  }}
  .indicator {{
    width: 18px;
    height: 18px;
    margin-top: 7px;
    border-radius: 50%;
    flex-shrink: 0;
  }}
  .indicator-spin {{
    border: 2px solid rgba(39, 98, 216, 0.18);
    border-top-color: var(--accent);
    animation: spin 0.9s linear infinite;
  }}
  .indicator-error {{
    background:
      radial-gradient(circle at center, var(--accent) 0 30%, transparent 32%),
      radial-gradient(circle at center, rgba(201, 75, 75, 0.18) 0 62%, transparent 64%);
  }}
  .status {{
    min-width: 0;
    width: 100%;
  }}
  .status h1 {{
    margin: 0;
    font-size: 28px;
    line-height: 1.1;
    font-weight: 700;
    letter-spacing: -0.03em;
  }}
  .status p {{
    margin: 10px 0 0;
    color: var(--muted);
    font-size: 13px;
    line-height: 1.55;
  }}
  .progress {{
    margin-top: 18px;
    width: 100%;
    height: 10px;
    border-radius: 999px;
    overflow: hidden;
    background: var(--track);
  }}
  .progress-fill {{
    position: relative;
    height: 100%;
    border-radius: inherit;
    background: linear-gradient(90deg, var(--accent), #78a6ff);
    box-shadow: inset 0 1px 0 rgba(255,255,255,0.28);
  }}
  .progress-fill::after {{
    content: "";
    position: absolute;
    inset: 0;
    background: linear-gradient(90deg, transparent, rgba(255,255,255,0.5), transparent);
    transform: translateX(-100%);
    animation: sweep 1.9s ease-in-out infinite;
  }}
  .progress-fill-indeterminate {{
    min-width: 34%;
    animation: drift 2.4s ease-in-out infinite;
  }}
  .footer {{
    margin-top: 14px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    color: var(--muted);
    font-size: 12px;
  }}
  .hint {{
    opacity: 0.92;
  }}
  .percent {{
    font-variant-numeric: tabular-nums;
    color: var(--accent);
    font-weight: 700;
  }}
  @keyframes spin {{
    to {{ transform: rotate(360deg); }}
  }}
  @keyframes sweep {{
    to {{ transform: translateX(100%); }}
  }}
  @keyframes drift {{
    0% {{ transform: translateX(-18%); }}
    50% {{ transform: translateX(80%); }}
    100% {{ transform: translateX(-18%); }}
  }}
</style>
</head>
<body>
  <main class="{shell_class}">
    <section class="header">
      <div class="brand">
        <div class="brand-mark">AL</div>
        <div class="brand-copy">
          <strong>ALAS Launcher</strong>
          <span>{banner}</span>
        </div>
      </div>
      <div class="badge">{badge}</div>
    </section>
    <section class="content">
      {indicator}
      <div class="status">
        <h1>{status}</h1>
        <p>{detail_html}</p>
        <div class="progress">
          <div class="{progress_class}" style="width: {progress_value}%;"></div>
        </div>
        <div class="footer">
          <span class="hint">The window will open automatically when the local WebUI is ready.</span>
          <span class="percent">{progress_value}%</span>
        </div>
      </div>
    </section>
  </main>
</body>
</html>"#,
        shell_class = shell_class,
        banner = escape_html(banner),
        badge = badge,
        indicator = indicator,
        status = escape_html(status),
        detail_html = detail_html,
        progress_class = progress_class,
        progress_value = progress_value
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

fn is_progress_bar_line(line: &str) -> bool {
    line.starts_with('[') && line.ends_with(']') && line.contains('=')
}

fn parse_splash_progress(line: &str) -> Option<u8> {
    if let Some(value) = parse_percentage(line) {
        return Some(value);
    }

    if !is_progress_bar_line(line) {
        return None;
    }

    let inner = &line[1..line.len() - 1];
    let total = inner.chars().filter(|ch| *ch == '=' || *ch == ' ').count();
    if total == 0 {
        return None;
    }
    let filled = inner.chars().filter(|ch| *ch == '=').count();
    Some(((filled as f32 / total as f32) * 100.0).round() as u8)
}

fn parse_percentage(line: &str) -> Option<u8> {
    line.split('%')
        .next()
        .and_then(|before| {
            before
                .rsplit(|ch: char| !ch.is_ascii_digit() && ch != '.')
                .next()
        })
        .and_then(|value| value.parse::<f32>().ok())
        .map(|value| value.round().clamp(0.0, 100.0) as u8)
}
