use anyhow::{anyhow, Result};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct AutostartStatus {
    pub enabled: bool,
    pub supported: bool,
    pub value: Option<String>,
}

#[cfg(windows)]
const RUN_VALUE_NAME: &str = "AzurPilot";

#[cfg(windows)]
const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(windows)]
const START_MINIMIZED_ARG: &str = "--start-minimized";

pub fn query() -> Result<AutostartStatus> {
    query_platform()
}

pub fn set_enabled(enabled: bool) -> Result<AutostartStatus> {
    set_enabled_platform(enabled)
}

#[cfg(windows)]
fn query_platform() -> Result<AutostartStatus> {
    let value = read_run_value()?;
    Ok(AutostartStatus {
        enabled: value
            .as_deref()
            .map(is_current_launcher_command)
            .unwrap_or(false),
        supported: true,
        value,
    })
}

#[cfg(windows)]
fn set_enabled_platform(enabled: bool) -> Result<AutostartStatus> {
    let key = windows_registry::CURRENT_USER
        .create(RUN_KEY_PATH)
        .map_err(|e| anyhow!("{e:?}"))?;

    if enabled {
        let command = current_launcher_command()?;
        key.set_string(RUN_VALUE_NAME, &command)
            .map_err(|e| anyhow!("{e:?}"))?;
    } else {
        let _ = key.remove_value(RUN_VALUE_NAME);
    }

    query_platform()
}

#[cfg(windows)]
fn read_run_value() -> Result<Option<String>> {
    let key = match windows_registry::CURRENT_USER.open(RUN_KEY_PATH) {
        Ok(key) => key,
        Err(_) => return Ok(None),
    };
    match key.get_string(RUN_VALUE_NAME) {
        Ok(value) => Ok(Some(value)),
        Err(_) => Ok(None),
    }
}

#[cfg(windows)]
fn is_current_launcher_command(value: &str) -> bool {
    let command = match current_launcher_command() {
        Ok(command) => command,
        Err(_) => return false,
    };
    normalize_command(value) == normalize_command(&command)
}

#[cfg(windows)]
fn current_launcher_command() -> Result<String> {
    let exe = std::env::current_exe()?;
    let path = exe.to_string_lossy().replace('"', r#"\""#);
    Ok(format!(r#""{path}" {START_MINIMIZED_ARG}"#))
}

#[cfg(windows)]
fn normalize_command(value: &str) -> String {
    value.trim().replace('/', "\\").to_ascii_lowercase()
}

#[cfg(not(windows))]
fn query_platform() -> Result<AutostartStatus> {
    Ok(AutostartStatus {
        enabled: false,
        supported: false,
        value: None,
    })
}

#[cfg(not(windows))]
fn set_enabled_platform(_enabled: bool) -> Result<AutostartStatus> {
    Err(anyhow!("Autostart is only supported on Windows"))
}
