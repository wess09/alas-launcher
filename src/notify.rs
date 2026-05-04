use std::{
    io::{BufRead, BufReader},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use anyhow::{anyhow, Result};
use reqwest::blocking::Client;
use reqwest::header::ACCEPT;
use serde::Deserialize;
#[cfg(not(windows))]
use tauri_plugin_notification::NotificationExt;
use tracing::{debug, info, warn};

#[cfg(windows)]
const WINDOWS_APP_ID: &str = "Alas Launcher";
#[cfg(windows)]
const WINDOWS_APP_NAME: &str = "Alas Launcher";

#[derive(Debug, Deserialize)]
struct NotifyPayload {
    instance: Option<String>,
    title: Option<String>,
    content: Option<String>,
}

pub fn start_notify_stream(app: tauri::AppHandle, port: u16, allow_exit: Arc<AtomicBool>) {
    thread::spawn(move || {
        let url = format!("http://127.0.0.1:{port}/api/notify_stream");
        let client = match Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                warn!("Unable to create notify stream client: {e}");
                return;
            }
        };

        while !allow_exit.load(Ordering::SeqCst) {
            info!("Connecting to notify stream: {url}");
            match read_notify_stream(&client, &url, &app, &allow_exit) {
                Ok(()) => debug!("Notify stream ended"),
                Err(e) => warn!("Notify stream disconnected: {e}"),
            }

            if !allow_exit.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_secs(3));
            }
        }
    });
}

fn read_notify_stream(
    client: &Client,
    url: &str,
    app: &tauri::AppHandle,
    allow_exit: &AtomicBool,
) -> Result<()> {
    let response = client.get(url).header(ACCEPT, "text/event-stream").send()?;

    if !response.status().is_success() {
        return Err(anyhow!("server returned {}", response.status()));
    }

    let mut reader = BufReader::new(response);
    let mut data_lines = Vec::new();

    while !allow_exit.load(Ordering::SeqCst) {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            dispatch_sse_data(&mut data_lines, app);
            continue;
        }

        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start().to_owned());
        }
    }

    dispatch_sse_data(&mut data_lines, app);
    Ok(())
}

fn dispatch_sse_data(data_lines: &mut Vec<String>, app: &tauri::AppHandle) {
    if data_lines.is_empty() {
        return;
    }

    let data = data_lines.join("\n");
    data_lines.clear();

    match serde_json::from_str::<NotifyPayload>(&data) {
        Ok(payload) => show_notification(app, payload),
        Err(e) => warn!("Ignoring invalid notify payload: {e}; payload={data}"),
    }
}

fn show_notification(app: &tauri::AppHandle, payload: NotifyPayload) {
    let title = clean_text(payload.title).unwrap_or_else(|| "Alas".to_owned());
    let body = clean_text(payload.content)
        .or_else(|| clean_text(payload.instance).map(|instance| format!("Instance: {instance}")))
        .unwrap_or_else(|| "New notification".to_owned());

    #[cfg(windows)]
    {
        if let Err(e) = show_windows_notification(&title, &body) {
            warn!("Failed to show Windows notification: {e}");
        }
        let _ = app;
    }

    #[cfg(not(windows))]
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        warn!("Failed to show system notification: {e}");
    }
}

#[cfg(windows)]
fn show_windows_notification(title: &str, body: &str) -> Result<()> {
    ensure_windows_app_user_model_id()?;
    tauri_winrt_notification::Toast::new(WINDOWS_APP_ID)
        .title(title)
        .text1(body)
        .duration(tauri_winrt_notification::Duration::Short)
        .show()
        .map_err(|e| anyhow!("{e:?}"))
}

#[cfg(windows)]
fn ensure_windows_app_user_model_id() -> Result<()> {
    let exe = std::env::current_exe()?;
    let key = windows_registry::CURRENT_USER
        .create(format!(r"SOFTWARE\Classes\AppUserModelId\{WINDOWS_APP_ID}"))
        .map_err(|e| anyhow!("{e:?}"))?;

    key.set_string("DisplayName", WINDOWS_APP_NAME)
        .map_err(|e| anyhow!("{e:?}"))?;
    key.set_string("IconBackgroundColor", "0")
        .map_err(|e| anyhow!("{e:?}"))?;
    key.set_hstring("IconUri", &exe.as_path().into())
        .map_err(|e| anyhow!("{e:?}"))?;
    Ok(())
}

fn clean_text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::NotifyPayload;

    #[test]
    fn parses_notify_payload() {
        let payload: NotifyPayload = serde_json::from_str(
            r#"{"instance":"alas","title":"Alas <alas> 警告","content":"<alas> 游戏卡住"}"#,
        )
        .unwrap();

        assert_eq!(payload.instance.as_deref(), Some("alas"));
        assert_eq!(payload.title.as_deref(), Some("Alas <alas> 警告"));
        assert_eq!(payload.content.as_deref(), Some("<alas> 游戏卡住"));
    }
}
