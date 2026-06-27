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
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::autostart;

#[derive(Debug, Deserialize)]
struct LauncherCommand {
    id: String,
    #[serde(rename = "type")]
    command_type: String,
    payload: Option<Value>,
}

pub fn start_launcher_control_stream(port: u16, allow_exit: Arc<AtomicBool>) {
    thread::spawn(move || {
        let stream_url = format!("http://127.0.0.1:{port}/api/launcher/stream");
        let report_url = format!("http://127.0.0.1:{port}/api/launcher/report");
        let client = match Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                warn!("Unable to create launcher control client: {e}");
                return;
            }
        };

        while !allow_exit.load(Ordering::SeqCst) {
            info!("Connecting to launcher control stream: {stream_url}");
            match read_launcher_control_stream(&client, &stream_url, &report_url, &allow_exit) {
                Ok(()) => debug!("Launcher control stream ended"),
                Err(e) => warn!("Launcher control stream disconnected: {e}"),
            }

            if !allow_exit.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_secs(3));
            }
        }
    });
}

fn read_launcher_control_stream(
    client: &Client,
    stream_url: &str,
    report_url: &str,
    allow_exit: &AtomicBool,
) -> Result<()> {
    let response = client
        .get(stream_url)
        .header(ACCEPT, "text/event-stream")
        .send()?;

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
            dispatch_sse_data(&mut data_lines, client, report_url);
            continue;
        }

        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start().to_owned());
        }
    }

    dispatch_sse_data(&mut data_lines, client, report_url);
    Ok(())
}

fn dispatch_sse_data(data_lines: &mut Vec<String>, client: &Client, report_url: &str) {
    if data_lines.is_empty() {
        return;
    }

    let data = data_lines.join("\n");
    data_lines.clear();

    match serde_json::from_str::<LauncherCommand>(&data) {
        Ok(command) => handle_command(client, report_url, command),
        Err(e) => warn!("Ignoring invalid launcher command: {e}; payload={data}"),
    }
}

fn handle_command(client: &Client, report_url: &str, command: LauncherCommand) {
    let report = match command.command_type.as_str() {
        "startup.query" => match autostart::query() {
            Ok(status) => success_report(&command.id, status),
            Err(e) => error_report(&command.id, e.to_string()),
        },
        "startup.set" => {
            let enabled = command
                .payload
                .as_ref()
                .and_then(|payload| payload.get("enabled"))
                .and_then(Value::as_bool);
            match enabled {
                Some(enabled) => match autostart::set_enabled(enabled) {
                    Ok(status) => success_report(&command.id, status),
                    Err(e) => error_report(&command.id, e.to_string()),
                },
                None => error_report(&command.id, "missing enabled".to_owned()),
            }
        }
        other => error_report(&command.id, format!("unknown command: {other}")),
    };

    if let Err(e) = client
        .post(report_url)
        .header(CONTENT_TYPE, "application/json")
        .body(report.to_string())
        .send()
    {
        warn!("Failed to report launcher command result: {e}");
    }
}

fn success_report<T: serde::Serialize>(id: &str, data: T) -> Value {
    json!({
        "id": id,
        "success": true,
        "data": data,
    })
}

fn error_report(id: &str, error: String) -> Value {
    json!({
        "id": id,
        "success": false,
        "error": error,
    })
}
