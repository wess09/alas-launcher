use std::{
    io::{BufRead, BufReader},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

#[cfg(windows)]
use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Result};
#[cfg(target_os = "linux")]
use notify_rust::{Hint, Notification};
use reqwest::blocking::Client;
use reqwest::header::ACCEPT;
use serde::Deserialize;
#[cfg(all(not(windows), not(target_os = "linux")))]
use tauri_plugin_notification::NotificationExt;
use tracing::{debug, info, warn};

pub type NotificationClickHandler = Arc<dyn Fn() + Send + Sync + 'static>;

#[cfg(windows)]
const WINDOWS_APP_ID: &str = "moe.taiho.alas-launcher.notification";
const WINDOWS_APP_NAME: &str = "有新的信息喵";

const WINDOWS_APP_ID_UPDATE: &str = "moe.taiho.alas-launcher.notification.update";
const WINDOWS_APP_NAME_UPDATE: &str = "有新的更新喵";

const WINDOWS_APP_ID_ANNOUNCEMENT: &str = "moe.taiho.alas-launcher.notification.announcement";
const WINDOWS_APP_NAME_ANNOUNCEMENT: &str = "有新的公告喵";

#[cfg(windows)]
const WINDOWS_NOTIFICATION_ICON: &[u8] = include_bytes!("../icons/icon.png");

#[derive(Debug, Deserialize)]
struct NotifyPayload {
    instance: Option<String>,
    title: Option<String>,
    content: Option<String>,
    updata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy)]
enum NotificationType {
    Normal,
    Update,
    Announcement,
}

pub fn start_notify_stream(
    app: tauri::AppHandle,
    port: u16,
    allow_exit: Arc<AtomicBool>,
    on_click: NotificationClickHandler,
) {
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
            match read_notify_stream(&client, &url, &app, &allow_exit, &on_click) {
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
    on_click: &NotificationClickHandler,
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
            dispatch_sse_data(&mut data_lines, app, on_click);
            continue;
        }

        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start().to_owned());
        }
    }

    dispatch_sse_data(&mut data_lines, app, on_click);
    Ok(())
}

fn dispatch_sse_data(
    data_lines: &mut Vec<String>,
    app: &tauri::AppHandle,
    on_click: &NotificationClickHandler,
) {
    if data_lines.is_empty() {
        return;
    }

    let data = data_lines.join("\n");
    data_lines.clear();

    match serde_json::from_str::<NotifyPayload>(&data) {
        Ok(payload) => show_notification(app, payload, on_click),
        Err(e) => warn!("Ignoring invalid notify payload: {e}; payload={data}"),
    }
}

fn show_notification(
    app: &tauri::AppHandle,
    payload: NotifyPayload,
    on_click: &NotificationClickHandler,
) {
    let NotifyPayload {
        instance,
        title,
        content,
        updata,
    } = payload;

    let (title, notify_type) = if let Some(v) = updata {
        if v.as_bool() == Some(true) || v.as_str() == Some("true") || v.as_str() == Some("ture") {
            ("有新的更新喵~".to_owned(), NotificationType::Update)
        } else if v.as_bool() == Some(false)
            || v.as_str() == Some("false")
            || v.as_str() == Some("fl")
        {
            ("有新的公告喵~".to_owned(), NotificationType::Announcement)
        } else {
            (
                clean_text(title).unwrap_or_else(|| "AzurPilot".to_owned()),
                NotificationType::Normal,
            )
        }
    } else {
        (
            clean_text(title).unwrap_or_else(|| "AzurPilot".to_owned()),
            NotificationType::Normal,
        )
    };
    let body = clean_text(content)
        .or_else(|| clean_text(instance).map(|instance| format!("Instance: {instance}")))
        .unwrap_or_else(|| "New notification".to_owned());

    #[cfg(windows)]
    {
        if let Err(e) = show_windows_notification(&title, &body, notify_type, on_click.clone()) {
            warn!("Failed to show Windows notification: {e}");
        }
        let _ = app;
    }

    #[cfg(target_os = "linux")]
    {
        if let Err(e) = show_linux_notification(&title, &body, on_click.clone()) {
            warn!("Failed to show Linux notification: {e}");
        }
        let _ = app;
    }

    #[cfg(all(not(windows), not(target_os = "linux")))]
    {
        if let Err(e) = app.notification().builder().title(title).body(body).show() {
            warn!("Failed to show system notification: {e}");
        }
        let _ = on_click;
    }
}

#[cfg(windows)]
fn show_windows_notification(
    title: &str,
    body: &str,
    notify_type: NotificationType,
    on_click: NotificationClickHandler,
) -> Result<()> {
    let (app_id, app_name) = match notify_type {
        NotificationType::Normal => (WINDOWS_APP_ID, WINDOWS_APP_NAME),
        NotificationType::Update => (WINDOWS_APP_ID_UPDATE, WINDOWS_APP_NAME_UPDATE),
        NotificationType::Announcement => {
            (WINDOWS_APP_ID_ANNOUNCEMENT, WINDOWS_APP_NAME_ANNOUNCEMENT)
        }
    };

    let icon_path = ensure_windows_app_user_model_id(app_id, app_name)?;
    let icon_uri_path = icon_path.to_string_lossy().replace('\\', "/");
    tauri_winrt_notification::Toast::new(app_id)
        .icon(
            Path::new(&icon_uri_path),
            tauri_winrt_notification::IconCrop::Square,
            app_name,
        )
        .title(title)
        .text1(body)
        .duration(tauri_winrt_notification::Duration::Short)
        .on_activated(move |_| {
            on_click();
            Ok(())
        })
        .show()
        .map_err(|e| anyhow!("{e:?}"))
}

#[cfg(windows)]
fn ensure_windows_app_user_model_id(id: &str, name: &str) -> Result<PathBuf> {
    let icon_path = ensure_windows_notification_icon()?;
    let key = windows_registry::CURRENT_USER
        .create(format!(r"SOFTWARE\Classes\AppUserModelId\{id}"))
        .map_err(|e| anyhow!("{e:?}"))?;

    key.set_string("DisplayName", name)
        .map_err(|e| anyhow!("{e:?}"))?;
    key.set_string("IconBackgroundColor", "0")
        .map_err(|e| anyhow!("{e:?}"))?;
    key.set_hstring("IconUri", &icon_path.as_path().into())
        .map_err(|e| anyhow!("{e:?}"))?;
    Ok(icon_path)
}

#[cfg(windows)]
fn ensure_windows_notification_icon() -> Result<PathBuf> {
    let data_dir = dirs::data_local_dir()
        .ok_or_else(|| anyhow!("Unable to resolve local app data directory"))?
        .join("AzurPilotLauncher");
    fs::create_dir_all(&data_dir)?;

    let icon_path = data_dir.join("notification-icon.png");
    let should_write = fs::read(&icon_path)
        .map(|current| current != WINDOWS_NOTIFICATION_ICON)
        .unwrap_or(true);
    if should_write {
        fs::write(&icon_path, WINDOWS_NOTIFICATION_ICON)?;
    }

    Ok(icon_path)
}

#[cfg(target_os = "linux")]
fn show_linux_notification(
    title: &str,
    body: &str,
    on_click: NotificationClickHandler,
) -> Result<()> {
    let mut notification = Notification::new();
    notification
        .summary(title)
        .body(body)
        .auto_icon()
        .action("default", "Open")
        .hint(Hint::Resident(true));
    let handle = notification.show()?;

    thread::spawn(move || {
        handle.wait_for_action(move |action| {
            if action == "default" {
                on_click();
            }
        });
    });

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
            r#"{"instance":"alas","title":"AzurPilot <alas> 警告","content":"<alas> 游戏卡住"}"#,
        )
        .unwrap();

        assert_eq!(payload.instance.as_deref(), Some("alas"));
        assert_eq!(payload.title.as_deref(), Some("AzurPilot <alas> 警告"));
        assert_eq!(payload.content.as_deref(), Some("<alas> 游戏卡住"));
    }
}
