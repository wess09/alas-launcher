use std::{
    collections::BTreeSet,
    net::TcpStream,
    process::{Command, ExitStatus},
    thread::sleep,
    time::Duration,
};

use anyhow::{anyhow, Result};
use command_group::{CommandGroup, GroupChild};
use serde_json::Value as JsonValue;
use tracing::{info, warn};

use crate::setup::venv_python;
use crate::window_util::CreateNoWindow as _;

#[derive(Clone, Debug)]
pub struct WebuiLaunchConfig {
    pub host: String,
    pub port: u16,
    pub password: Option<String>,
    pub cdn: bool,
    pub ssl_key: Option<String>,
    pub ssl_cert: Option<String>,
    pub run: Vec<String>,
}

impl WebuiLaunchConfig {
    pub fn from_deploy_config(config: Option<&JsonValue>) -> Self {
        let webui = config
            .and_then(|config| config.get("Deploy"))
            .and_then(|deploy| deploy.get("Webui"));

        Self {
            host: webui
                .and_then(|webui| webui.get("WebuiHost"))
                .and_then(value_as_string)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "127.0.0.1".to_owned()),
            port: webui
                .and_then(|webui| webui.get("WebuiPort"))
                .and_then(value_as_u16)
                .unwrap_or(22267),
            password: webui
                .and_then(|webui| webui.get("Password"))
                .and_then(value_as_string)
                .filter(|value| !value.trim().is_empty()),
            cdn: webui
                .and_then(|webui| webui.get("CDN"))
                .and_then(value_as_bool)
                .unwrap_or(false),
            ssl_key: webui
                .and_then(|webui| webui.get("WebuiSSLKey"))
                .and_then(value_as_string)
                .filter(|value| !value.trim().is_empty()),
            ssl_cert: webui
                .and_then(|webui| webui.get("WebuiSSLCert"))
                .and_then(value_as_string)
                .filter(|value| !value.trim().is_empty()),
            run: webui
                .and_then(|webui| webui.get("Run"))
                .map(value_as_string_list)
                .unwrap_or_default(),
        }
    }

    fn args(&self) -> Vec<String> {
        let mut args = vec![
            "gui.py".to_owned(),
            "--host".to_owned(),
            self.host.clone(),
            "--port".to_owned(),
            self.port.to_string(),
        ];

        if let Some(password) = &self.password {
            args.push("--key".to_owned());
            args.push(password.clone());
        }
        if self.cdn {
            args.push("--cdn".to_owned());
        }
        if let Some(ssl_key) = &self.ssl_key {
            args.push("--ssl-key".to_owned());
            args.push(ssl_key.clone());
        }
        if let Some(ssl_cert) = &self.ssl_cert {
            args.push("--ssl-cert".to_owned());
            args.push(ssl_cert.clone());
        }
        if !self.run.is_empty() {
            args.push("--run".to_owned());
            args.extend(self.run.iter().cloned());
        }

        args
    }
}

fn value_as_string(value: &JsonValue) -> Option<String> {
    if let Some(value) = value.as_str() {
        Some(value.to_owned())
    } else if value.is_null() {
        None
    } else {
        Some(value.to_string())
    }
}

fn value_as_u16(value: &JsonValue) -> Option<u16> {
    if let Some(value) = value.as_u64() {
        u16::try_from(value).ok()
    } else {
        value.as_str()?.parse::<u16>().ok()
    }
}

fn value_as_bool(value: &JsonValue) -> Option<bool> {
    if let Some(value) = value.as_bool() {
        Some(value)
    } else {
        match value.as_str()?.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        }
    }
}

fn value_as_string_list(value: &JsonValue) -> Vec<String> {
    match value {
        JsonValue::Array(values) => values
            .iter()
            .filter_map(value_as_string)
            .filter(|value| !value.trim().is_empty())
            .collect(),
        _ => value_as_string(value)
            .filter(|value| !value.trim().is_empty())
            .into_iter()
            .collect(),
    }
}

pub struct ManagedBackend {
    child: Option<GroupChild>,
}

impl ManagedBackend {
    pub fn new(config: &WebuiLaunchConfig) -> Result<Self> {
        std::env::set_var("ALAS_LAUNCHER_PID", format!("{}", std::process::id()));
        kill_processes_using_port(config.port)?;

        let child = Command::new(venv_python())
            .args(config.args())
            .group()
            .create_no_window()
            .spawn()?;
        let res = Self { child: Some(child) };

        let address = format!("127.0.0.1:{}", config.port).parse().unwrap();
        let start_time = std::time::Instant::now();
        while start_time.elapsed() < Duration::from_secs(60) {
            if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
                return Ok(res);
            }
            sleep(Duration::from_millis(100));
        }
        Err(anyhow!(
            "Timeout waiting for port {} to be ready",
            config.port
        ))
    }

    pub fn terminate(&mut self) -> Result<ExitStatus> {
        if let Some(mut child) = self.child.take() {
            #[cfg(unix)]
            {
                use command_group::{Signal, UnixChildExt};
                let _ = child.signal(Signal::SIGTERM);
                let start_time = std::time::Instant::now();
                while start_time.elapsed() < Duration::from_millis(500) {
                    if let Ok(Some(exit_status)) = child.try_wait() {
                        return Ok(exit_status);
                    }
                    sleep(Duration::from_millis(100));
                }
                warn!("gui.py didn't exit, killing it...");
            }
            child.kill()?;
            Ok(child.wait()?)
        } else {
            Ok(ExitStatus::default())
        }
    }
}

fn kill_processes_using_port(port: u16) -> Result<()> {
    let pids = match pids_using_tcp_port(port) {
        Ok(pids) => pids,
        Err(e) => {
            warn!("Unable to scan processes using port {}: {}", port, e);
            return Ok(());
        }
    };
    if pids.is_empty() {
        return Ok(());
    }

    let current_pid = std::process::id();
    let sys = sysinfo::System::new_all();
    for pid in pids {
        if pid == 0 || pid == current_pid {
            continue;
        }

        let sys_pid = sysinfo::Pid::from_u32(pid);
        match sys.process(sys_pid) {
            Some(process) => {
                info!(
                    "Killing process {} ({}) using configured WebUI port {}",
                    pid,
                    process.name().to_string_lossy(),
                    port
                );
                if !process.kill() {
                    warn!("Failed to kill process {} using port {}", pid, port);
                }
            }
            None => {
                warn!(
                    "Process {} was using port {}, but exited before it could be killed",
                    pid, port
                );
            }
        }
    }

    let start_time = std::time::Instant::now();
    while start_time.elapsed() < Duration::from_secs(5) {
        match pids_using_tcp_port(port) {
            Ok(pids) if pids.is_empty() => return Ok(()),
            Ok(_) => sleep(Duration::from_millis(100)),
            Err(e) => {
                warn!("Unable to verify port {} was released: {}", port, e);
                return Ok(());
            }
        }
    }

    warn!("Timed out waiting for port {} to be released", port);
    Ok(())
}

#[cfg(windows)]
fn pids_using_tcp_port(port: u16) -> Result<BTreeSet<u32>> {
    let output = Command::new("netstat")
        .args(["-ano", "-p", "tcp"])
        .create_no_window()
        .output()?;
    if !output.status.success() {
        return Err(anyhow!("netstat failed with status {}", output.status));
    }

    Ok(parse_windows_netstat_pids(&output.stdout, port))
}

#[cfg(windows)]
fn parse_windows_netstat_pids(output: &[u8], port: u16) -> BTreeSet<u32> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| {
            let parts: Vec<_> = line.split_whitespace().collect();
            if parts.len() < 5
                || !parts[0].eq_ignore_ascii_case("TCP")
                || !parts[3].eq_ignore_ascii_case("LISTENING")
                || !local_address_uses_port(parts[1], port)
            {
                return None;
            }
            parts.last()?.parse::<u32>().ok()
        })
        .collect()
}

#[cfg(unix)]
fn pids_using_tcp_port(port: u16) -> Result<BTreeSet<u32>> {
    let output = Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
        .create_no_window()
        .output()?;
    if !output.status.success() && output.stdout.is_empty() {
        return Ok(BTreeSet::new());
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect())
}

#[cfg(windows)]
fn local_address_uses_port(address: &str, port: u16) -> bool {
    address
        .rsplit_once(':')
        .and_then(|(_, port_part)| port_part.parse::<u16>().ok())
        == Some(port)
}

impl Drop for ManagedBackend {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            match child.kill() {
                Ok(_) => {}
                Err(e) => warn!("Failed to kill gui.py process: {:?}", e),
            }
        }
        // Kill potential leaked processes
        let sys = sysinfo::System::new_all();
        for (pid, process) in sys.processes() {
            for var in process.environ() {
                if pid.as_u32() != std::process::id()
                    && var.to_str().unwrap_or_default()
                        == format!("ALAS_LAUNCHER_PID={}", std::process::id())
                {
                    process.kill();
                }
            }
        }
    }
}
