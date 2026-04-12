use anyhow::{anyhow, Result};
use serde_json::Value as JsonValue;
use std::collections::HashSet;
use std::env::set_current_dir;
use std::fs;
use std::io::{BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::thread;
use tracing::{info, warn};

use crate::window_util::CreateNoWindow as _;

#[derive(Clone, Debug)]
pub struct SplashUpdate {
    pub subtitle: String,
    pub title: String,
    pub detail: String,
    pub progress: u8,
    pub is_error: bool,
}

impl SplashUpdate {
    pub fn loading(
        title: impl Into<String>,
        detail: impl Into<String>,
        progress: u8,
    ) -> Self {
        Self {
            subtitle: "Connecting to local service...".to_owned(),
            title: title.into(),
            detail: detail.into(),
            progress: progress.min(100),
            is_error: false,
        }
    }

    pub fn error(
        title: impl Into<String>,
        detail: impl Into<String>,
        progress: u8,
    ) -> Self {
        Self {
            subtitle: "Failed to connect".to_owned(),
            title: title.into(),
            detail: detail.into(),
            progress: progress.min(100),
            is_error: true,
        }
    }

    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = subtitle.into();
        self
    }
}

#[derive(Clone, Copy, Debug)]
enum ScriptPhase {
    Git,
    Dependencies { total_packages: usize },
}

fn alas_repo_dir() -> PathBuf {
    // Always check if this is a typical same-folder portable distribution
    let exe_folder = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let mut installer_py = exe_folder.clone();
    installer_py.extend(["deploy", "installer.py"]);
    if fs::exists(installer_py).unwrap() {
        return exe_folder;
    }
    // If it's MacOS, it could be ALAS.app/Contents/AzurLaneAutoScript
    #[cfg(target_os = "macos")]
    {
        use std::ffi::OsStr;
        if exe_folder.file_name() == Some(OsStr::new("MacOS")) {
            let mut repo_folder = exe_folder;
            repo_folder.pop();
            repo_folder.push("AzurLaneAutoScript");
            if fs::exists(&repo_folder).unwrap() {
                return repo_folder;
            }
        }
    }
    panic!("Cannot find ALAS repo folder");
}

fn prepend_path_to_env(key: &str, path: PathBuf) {
    let mut paths = Vec::new();
    paths.push(path);
    if let Some(ref old_path) = &std::env::var_os(key) {
        paths.extend(std::env::split_paths(old_path));
    }
    std::env::set_var(key, std::env::join_paths(paths).unwrap());
}

fn configure_python_package_installation() {
    // Some portable Python builds are marked as externally managed (PEP 668),
    // which blocks `pip install` unless this opt-out is present.
    std::env::set_var("PIP_BREAK_SYSTEM_PACKAGES", "1");
}

#[cfg(unix)]
pub fn setup_environment() -> Result<()> {
    let dir = alas_repo_dir();
    info!("ALAS dir is {:?}", &dir);
    set_current_dir(&dir)?;
    configure_python_package_installation();
    prepend_path_to_env("PATH", dir.join("toolkit").join("libexec").join("git-core"));
    prepend_path_to_env("PATH", dir.join("toolkit").join("bin"));
    prepend_path_to_env("LD_LIBRARY_PATH", dir.join("toolkit").join("lib"));
    Ok(())
}

#[cfg(windows)]
pub fn setup_environment() -> Result<()> {
    let dir = alas_repo_dir();
    info!("ALAS dir is {:?}", &dir);
    set_current_dir(&dir)?;
    configure_python_package_installation();
    prepend_path_to_env("PATH", dir.join("toolkit").join("git").join("cmd"));
    prepend_path_to_env("PATH", dir.join("toolkit").join("Scripts"));
    prepend_path_to_env("PATH", dir.join("toolkit"));
    Ok(())
}

#[cfg(target_os = "linux")]
fn setup_git_ca_bundle() {
    let cert_file = openssl_probe::probe().cert_file;
    if let Some(file) = cert_file.as_ref().and_then(|f| f.to_str()) {
        let _ = Command::new("git")
            .args(["config", "--local", "http.sslCAInfo", file])
            .status();
    }
}

pub fn setup_alas_repo(mut status_updater: impl FnMut(SplashUpdate)) -> Result<()> {
    info!("Starting setup for ALAS repository...");
    #[cfg(target_os = "linux")]
    setup_git_ca_bundle();
    // Similar setup to deploy/installer.py
    status_updater(
        SplashUpdate::loading(
            "Preparing workspace",
            "Cleaning cached config state and checking the local ALAS setup.",
            8,
        )
        .with_subtitle("Checking local environment..."),
    );
    atomic_failure_cleanup("./config")?;
    ensure_python_dependency_config()?;
    status_updater(
        SplashUpdate::loading(
            "Updating ALAS",
            "Fetching repository changes and validating the local working copy.",
            18,
        )
        .with_subtitle("Syncing repository..."),
    );
    git_update(&mut status_updater)?;
    status_updater(
        SplashUpdate::loading(
            "Installing dependencies",
            "Verifying the Python packages required by ALAS.",
            64,
        )
        .with_subtitle("Syncing Python packages..."),
    );
    pip_install(&mut status_updater)?;
    status_updater(
        SplashUpdate::loading(
            "Finalizing startup",
            "The local WebUI is almost ready. Opening the main window next.",
            94,
        )
        .with_subtitle("Connecting to local service..."),
    );
    Ok(())
}

pub fn get_deploy_config() -> Option<JsonValue> {
    let config_content = fs::read_to_string("./config/deploy.yaml").ok()?;
    let config: JsonValue = serde_yaml::from_str(&config_content).ok()?;
    Some(config)
}

fn pipe_lines(read: impl Read + Send + 'static, tx: Sender<(bool, String)>, is_err: bool) {
    thread::spawn(move || {
        let mut reader = BufReader::new(read);
        let mut buffer = "".to_owned();
        loop {
            let mut line = [0u8; 64];
            match reader.read(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(size) => {
                    for c in &line[0..size] {
                        if *c < 32 || *c > 127 {
                            if !buffer.is_empty() {
                                let _ = tx.send((is_err, buffer));
                                buffer = "".to_owned();
                            }
                        } else if *c as char == ':' {
                            let mut cut = 0usize;
                            if let Some((l, r)) = buffer.split_once(':') {
                                if r.ends_with(l) {
                                    cut = r.len() + 1;
                                }
                            }
                            if cut > 0 {
                                let (l, r) = buffer.split_at(cut);
                                let _ = tx.send((is_err, l.to_owned()));
                                buffer = r.to_owned();
                            }
                            buffer.push(*c as char);
                        } else {
                            buffer.push(*c as char);
                        }
                    }
                }
            }
        }
        if !buffer.is_empty() {
            let _ = tx.send((is_err, buffer));
        }
    });
}

fn run_python_script(
    script: &str,
    mut status_updater: impl FnMut(SplashUpdate),
    phase: ScriptPhase,
) -> Result<()> {
    // Spawn the child with piped stdout/stderr so we can tee them.
    let mut child = Command::new("python")
        .args(["-c", script])
        .create_no_window()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Channels to receive lines from reader threads. (is_err, line)
    let (tx, rx) = mpsc::channel::<(bool, String)>();

    // Spawn a reader thread for stdout
    if let Some(stdout) = child.stdout.take() {
        pipe_lines(stdout, tx.clone(), false);
    }

    // Spawn a reader thread for stderr
    if let Some(stderr) = child.stderr.take() {
        pipe_lines(stderr, tx.clone(), true);
    }

    // Drop the original sender so rx will close when both reader threads finish.
    drop(tx);

    let mut last_err = "".to_owned();
    let mut seen_packages = HashSet::new();

    // Receive lines and tee them to stdout/stderr and the status_updater callback.
    while let Ok((is_err, line)) = rx.recv() {
        if let Some(update) = splash_update_for_output(&line, phase, &mut seen_packages) {
            status_updater(update);
        }

        if is_err {
            warn!("{line}");
            last_err = line;
        } else {
            info!("{line}");
        }
    }

    // Wait for child to exit and check status
    let status = child.wait()?;
    if !status.success() {
        if last_err.is_empty() {
            last_err = match phase {
                ScriptPhase::Git => "Failed to update ALAS".to_owned(),
                ScriptPhase::Dependencies { .. } => "Failed to update dependencies".to_owned(),
            };
        }
        return Err(anyhow!(last_err));
    }
    Ok(())
}

fn git_update(status_updater: impl FnMut(SplashUpdate)) -> Result<()> {
    // Decorate execute() to get fetch progress
    let script = r#"
import deploy.git
def decorate_execute(fn):
    def new_fn(*args, **kwargs):
        if len(args) >= 1 and ' fetch ' in args[0] and '--progress' not in args[0]:
            args = (args[0].replace(' fetch ', ' fetch --progress '),) + args[1:]
        return fn(*args, **kwargs)
    return new_fn
gm = deploy.git.GitManager()
gm.execute = decorate_execute(gm.execute)
gm.git_install()
"#;
    run_python_script(script, status_updater, ScriptPhase::Git)
}

fn pip_install(status_updater: impl FnMut(SplashUpdate)) -> Result<()> {
    let requirements_file = get_requirements_file_path();
    let script = r#"
import deploy.pip
pm = deploy.pip.PipManager()
pm.pip_install()
"#;
    run_python_script(
        script,
        status_updater,
        ScriptPhase::Dependencies {
            total_packages: count_required_packages(&requirements_file),
        },
    )
}

fn ensure_python_dependency_config() -> Result<()> {
    let path = "./config/deploy.yaml";
    let Ok(content) = fs::read_to_string(path) else {
        return Ok(());
    };

    let mut changed = false;
    let mut found_install_dependencies = false;
    let mut output = String::with_capacity(content.len());

    for line in content.lines() {
        let indent_len = line.len() - line.trim_start().len();
        let indent = &line[..indent_len];
        if line.trim_start().starts_with("InstallDependencies:") {
            found_install_dependencies = true;
            if line.trim() != "InstallDependencies: true" {
                output.push_str(indent);
                output.push_str("InstallDependencies: true");
                changed = true;
            } else {
                output.push_str(line);
            }
        } else {
            output.push_str(line);
        }
        output.push('\n');
    }

    if changed {
        fs::write(path, output)?;
        info!("Updated Deploy.Python dependency settings in {path}");
    } else {
        if !found_install_dependencies {
            warn!("InstallDependencies not found in {path}");
        }
    }

    Ok(())
}

fn atomic_failure_cleanup(path: &str) -> Result<()> {
    let _ = Command::new("python")
        .args([
            "-c",
            "import sys; from deploy.atomic import atomic_failure_cleanup; atomic_failure_cleanup(sys.argv[1])",
            path,
        ])
        .create_no_window()
        .status()?;
    Ok(())
}

fn splash_update_for_output(
    line: &str,
    phase: ScriptPhase,
    seen_packages: &mut HashSet<String>,
) -> Option<SplashUpdate> {
    let sanitized = line.trim();
    if sanitized.is_empty() {
        return None;
    }

    match phase {
        ScriptPhase::Git => splash_update_for_git_output(sanitized),
        ScriptPhase::Dependencies { total_packages } => {
            splash_update_for_dependency_output(sanitized, total_packages, seen_packages)
        }
    }
}

fn splash_update_for_git_output(line: &str) -> Option<SplashUpdate> {
    if line.contains("=====") {
        let detail = line.replace('=', " ");
        return Some(
            SplashUpdate::loading(
                "Updating ALAS",
                detail.trim(),
                24,
            )
            .with_subtitle("Syncing repository..."),
        );
    }

    if line.contains("objects:") || line.contains("deltas:") || line.contains("files:") {
        let percentage = find_percentage(line).unwrap_or(0);
        let progress = 18 + ((percentage as u16 * 42) / 100) as u8;
        return Some(
            SplashUpdate::loading("Updating ALAS", line, progress)
                .with_subtitle("Syncing repository..."),
        );
    }

    None
}

fn splash_update_for_dependency_output(
    line: &str,
    total_packages: usize,
    seen_packages: &mut HashSet<String>,
) -> Option<SplashUpdate> {
    if line.contains("=====") {
        let detail = line.replace('=', " ");
        let lowered = detail.to_ascii_lowercase();
        let (title, progress) = if lowered.contains("check python") {
            ("Checking Python runtime", 66)
        } else if lowered.contains("update dependencies") {
            ("Installing dependencies", 72)
        } else {
            ("Installing dependencies", 70)
        };
        return Some(
            SplashUpdate::loading(title, detail.trim(), progress)
                .with_subtitle("Syncing Python packages..."),
        );
    }

    if let Some(package_name) = extract_dependency_name(line) {
        seen_packages.insert(package_name);
        let progress = dependency_progress(seen_packages.len(), total_packages);
        return Some(
            SplashUpdate::loading("Installing dependencies", line, progress)
                .with_subtitle("Syncing Python packages..."),
        );
    }

    if line.starts_with("Installing collected packages") {
        return Some(
            SplashUpdate::loading(
                "Installing dependencies",
                "Applying downloaded package updates to the local Python toolkit.",
                92,
            )
            .with_subtitle("Syncing Python packages..."),
        );
    }

    None
}

fn dependency_progress(processed_packages: usize, total_packages: usize) -> u8 {
    if total_packages == 0 {
        return 78;
    }

    let clamped = processed_packages.min(total_packages) as u16;
    let total = total_packages as u16;
    let span = 24u16;
    (64 + ((clamped * span) / total) as u8).min(90)
}

fn extract_dependency_name(line: &str) -> Option<String> {
    let prefixes = ["Collecting ", "Requirement already satisfied: "];
    let prefix = prefixes.iter().find(|prefix| line.starts_with(**prefix))?;
    let rest = line.strip_prefix(prefix)?;
    let end = rest
        .find("==")
        .or_else(|| rest.find(" ("))
        .or_else(|| rest.find(" in "))
        .unwrap_or(rest.len());
    let package = rest[..end].trim();
    if package.is_empty() {
        None
    } else {
        Some(package.to_ascii_lowercase())
    }
}

fn count_required_packages(path: &str) -> usize {
    fs::read_to_string(path)
        .ok()
        .map(|content| {
            content
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#') && line.contains("=="))
                .count()
        })
        .unwrap_or(0)
}

fn get_requirements_file_path() -> String {
    get_deploy_config()
        .as_ref()
        .and_then(|config| config.get("Deploy"))
        .and_then(|deploy| deploy.get("Python"))
        .and_then(|python| python.get("RequirementsFile"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.to_owned())
        .unwrap_or_else(|| "./requirements.txt".to_owned())
}

fn find_percentage(s: &str) -> Option<u8> {
    s.split('%')
        .next()
        .and_then(|before| {
            before
                .rsplit(|c: char| !c.is_ascii_digit() && c != '.')
                .next()
        })
        .and_then(|num| {
            num.parse::<f32>()
                .ok()
                .map(|v| v.round().clamp(0.0, u8::MAX as f32) as u8)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_find_percentage() {
        assert_eq!(Some(8), find_percentage("8%"));
        assert_eq!(Some(25), find_percentage("loading 25%..."));
        assert_eq!(Some(100), find_percentage("100%..."));
        assert_eq!(None, find_percentage("%1"));
    }
}
