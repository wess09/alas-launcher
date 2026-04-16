use std::{
    net::TcpStream,
    process::{Command, ExitStatus},
    thread::sleep,
    time::Duration,
};

use anyhow::{anyhow, Result};
use command_group::{CommandGroup, GroupChild};
use tracing::warn;

use crate::window_util::CreateNoWindow as _;

pub struct ManagedBackend {
    child: Option<GroupChild>,
    preserve_on_drop: bool,
}

impl ManagedBackend {
    pub fn new(port: u16) -> Result<Self> {
        std::env::set_var("ALAS_LAUNCHER_PID", format!("{}", std::process::id()));
        let child = Command::new("python")
            .args(["gui.py", "--host", "127.0.0.1", "--port", &port.to_string()])
            .group()
            .create_no_window()
            .spawn()?;
        let res = Self {
            child: Some(child),
            preserve_on_drop: false,
        };

        let address = format!("127.0.0.1:{}", port).parse().unwrap();
        let start_time = std::time::Instant::now();
        while start_time.elapsed() < Duration::from_secs(60) {
            if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
                return Ok(res);
            }
            sleep(Duration::from_millis(100));
        }
        Err(anyhow!("Timeout waiting for port {} to be ready", port))
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

    pub fn detach_for_self_update(&mut self) {
        self.preserve_on_drop = true;
        if let Some(child) = self.child.take() {
            // Keep gui.py alive while the launcher restarts itself.
            std::mem::forget(child);
        }
    }
}

impl Drop for ManagedBackend {
    fn drop(&mut self) {
        if self.preserve_on_drop {
            return;
        }
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

pub fn terminate_detached_backend(owner_pid: u32) {
    let mut terminated = 0usize;
    let matcher = format!("ALAS_LAUNCHER_PID={owner_pid}");
    let sys = sysinfo::System::new_all();
    for process in sys.processes().values() {
        if process
            .environ()
            .iter()
            .any(|var| var.to_str().unwrap_or_default() == matcher)
            && process.kill()
        {
            terminated += 1;
        }
    }
    if terminated == 0 {
        warn!("No detached backend process found for launcher pid {owner_pid}");
    }
}
