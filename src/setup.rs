use anyhow::{anyhow, Result};
use chrono::Local;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::collections::HashSet;
use std::env::set_current_dir;
use std::fs;
use std::io::{BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;
use tracing::{info, warn};

use crate::window_util::CreateNoWindow as _;

#[derive(Clone, Debug, Serialize)]
pub struct SplashUpdate {
    pub subtitle: String,
    pub title: String,
    pub detail: String,
    pub progress: u8,
    pub is_error: bool,
}

impl SplashUpdate {
    pub fn loading(title: impl Into<String>, detail: impl Into<String>, progress: u8) -> Self {
        Self {
            subtitle: "正在连接本地服务...".to_owned(),
            title: title.into(),
            detail: detail.into(),
            progress: progress.min(100),
            is_error: false,
        }
    }

    pub fn error(title: impl Into<String>, detail: impl Into<String>, progress: u8) -> Self {
        Self {
            subtitle: "连接失败".to_owned(),
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

const TIPS: &[&str] = &[
    "指挥官，今天也要加油哦！",
    "听说给舰娘送戒指能变强？",
    "每日任务记得领，蚊子腿也是肉。",
    "后宅的食物记得补充，不然舰娘会饿肚子的。",
    "科研项目虽然慢，但那是变强的必经之路。",
    "不要忘记每日的免费建造哦（虽然大都是狗粮）。",
    "AzurPilot 正在努力为你打工，请稍等片刻。",
    "正在试图潜入塞壬据点...",
    "正在调配燃油，请指挥官稍后...",
    "正在说服蛮啾们加快进度...",
    "适当的休息也是指挥官工作的一部分哦。",
    "听说点击看板娘会有惊喜？",
    "维护补偿的魔方和物资已经... 还没发，再等会儿。",
    "正在拦截塞壬的干扰信号...",
    "正在分析塞壬的技术矩阵...",
    "皇家的红茶已经泡好了，指挥官要来一杯吗？",
    "白鹰的派对已经准备就绪，就等指挥官了。",
    "重樱的樱花落了，但我们的奋斗才刚开始。",
    "铁血的科技世界第一！",
];

pub fn get_tip() -> String {
    let now = Local::now().timestamp() as usize;
    TIPS[now % TIPS.len()].to_string()
}

#[derive(Clone, Copy, Debug)]
enum ScriptPhase {
    Git,
    Dependencies { total_packages: usize },
}

const MAX_UPDATE_RETRIES: usize = 20;
const RETRY_DELAY: Duration = Duration::from_secs(1);

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

#[cfg(unix)]
pub fn setup_environment() -> Result<()> {
    let dir = alas_repo_dir();
    info!("ALAS dir is {:?}", &dir);
    set_current_dir(&dir)?;
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
            "准备工作区",
            "正在清理缓存并检查本地环境。",
            8,
        )
        .with_subtitle(format!("检查本地环境中... | Tips：{}", get_tip())),
    );
    atomic_failure_cleanup("./config")?;
    ensure_python_dependency_config()?;
    status_updater(
        SplashUpdate::loading(
            "正在更新",
            "正在获取最新补丁包",
            18,
        )
        .with_subtitle(format!("同步中... | Tips：{}", get_tip())),
    );
    git_update(&mut status_updater)?;
    status_updater(
        SplashUpdate::loading(
            "安装依赖库",
            "正在验证 AzurPilot 运行所需的 Python 依赖包。",
            64,
        )
        .with_subtitle(format!("同步依赖包中... | Tips：{}", get_tip())),
    );
    uv_pip_install(&mut status_updater)?;
    status_updater(
        SplashUpdate::loading(
            "完成启动",
            "本地界面已准备就绪，即将打开主窗口。",
            94,
        )
        .with_subtitle(format!("启动服务中... | Tips：{}", get_tip())),
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

fn run_command(
    cmd: &mut Command,
    mut status_updater: impl FnMut(SplashUpdate),
    phase: ScriptPhase,
) -> Result<()> {
    let is_deps = matches!(phase, ScriptPhase::Dependencies { .. });

    let mut child = cmd
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
        if let Some(update) =
            splash_update_for_output(&line, phase, &mut seen_packages)
        {
            status_updater(update);
        }

        if is_err {
            if is_deps && is_uv_progress_line(&line) {
                info!("{line}");
            } else {
                warn!("{line}");
                last_err = line;
            }
        } else {
            info!("{line}");
        }
    }

    // Wait for child to exit and check status
    let status = child.wait()?;
    if !status.success() {
        if last_err.is_empty() {
            last_err = match phase {
                ScriptPhase::Git => "更新 AzurPilot 失败".to_owned(),
                ScriptPhase::Dependencies { .. } => "更新依赖库失败".to_owned(),
            };
        }
        return Err(anyhow!(last_err));
    }
    Ok(())
}

fn run_command_with_retry(
    build_cmd: impl Fn() -> Command,
    mut status_updater: impl FnMut(SplashUpdate),
    phase: ScriptPhase,
) -> Result<()> {
    for retry in 0..=MAX_UPDATE_RETRIES {
        match run_command(&mut build_cmd(), &mut status_updater, phase) {
            Ok(()) => return Ok(()),
            Err(err) => {
                if retry == MAX_UPDATE_RETRIES {
                    return Err(err);
                }

                let retry_count = retry + 1;
                let error_text = err.to_string();
                warn!(
                    "{} failed (retry {retry_count}/{MAX_UPDATE_RETRIES}): {error_text}",
                    phase_display_name(phase)
                );
                status_updater(splash_retry_update(phase, retry_count, &error_text));
                thread::sleep(RETRY_DELAY);
            }
        }
    }

    unreachable!()
}

fn phase_display_name(phase: ScriptPhase) -> &'static str {
    match phase {
        ScriptPhase::Git => "代码更新",
        ScriptPhase::Dependencies { .. } => "依赖更新",
    }
}

fn splash_retry_update(phase: ScriptPhase, retry_count: usize, error_text: &str) -> SplashUpdate {
    let detail = format!(
        "重试失败 ({retry_count}/{MAX_UPDATE_RETRIES})，1秒后再次尝试：{error_text}"
    );
    match phase {
        ScriptPhase::Git => SplashUpdate::loading("正在更新 AzurPilot", detail, 18)
            .with_subtitle(format!("同步仓库中... | Tips：{}", get_tip())),
        ScriptPhase::Dependencies { .. } => {
            SplashUpdate::loading("安装依赖库", detail, 64)
                .with_subtitle(format!("同步依赖包中... | Tips：{}", get_tip()))
        }
    }
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
    run_command_with_retry(
        || {
            let mut cmd = Command::new("python");
            cmd.args(["-c", script]);
            cmd
        },
        status_updater,
        ScriptPhase::Git,
    )
}

fn uv_pip_install(status_updater: impl FnMut(SplashUpdate)) -> Result<()> {
    let requirements_file = get_requirements_file_path();
    let total_packages = count_required_packages(&requirements_file);

    let mirror = get_deploy_config()
        .as_ref()
        .and_then(|c| c.get("Deploy"))
        .and_then(|d| d.get("Python"))
        .and_then(|p| p.get("PypiMirror"))
        .and_then(|v| v.as_str())
        .filter(|m| !m.is_empty() && *m != "null")
        .map(|m| m.to_owned());

    run_command_with_retry(
        move || {
            let mut cmd = Command::new("python");
            cmd.args(["-m", "uv", "pip", "install"])
                .arg("--break-system-packages")
                .arg("-r")
                .arg(&requirements_file)
                .env("UV_SYSTEM_PYTHON", "1")
                .env("UV_NO_PROGRESS", "1");
            if let Some(ref m) = mirror {
                cmd.args(["--index-url", m]);
            }
            cmd
        },
        status_updater,
        ScriptPhase::Dependencies { total_packages },
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
            SplashUpdate::loading("正在更新 AzurPilot", detail.trim(), 24)
                .with_subtitle(format!("同步仓库中... | Tips：{}", get_tip())),
        );
    }

    if line.contains("objects:") || line.contains("deltas:") || line.contains("files:") {
        let percentage = find_percentage(line).unwrap_or(0);
        let progress = 18 + ((percentage as u16 * 42) / 100) as u8;
        return Some(
            SplashUpdate::loading("正在更新 AzurPilot", line, progress)
                .with_subtitle(format!("同步仓库中... | Tips：{}", get_tip())),
        );
    }

    None
}

fn splash_update_for_dependency_output(
    line: &str,
    total_packages: usize,
    seen_packages: &mut HashSet<String>,
) -> Option<SplashUpdate> {
    let subtitle = format!("同步依赖包中... | Tips：{}", get_tip());

    // UV: resolution complete
    if line.starts_with("Resolved ") {
        return Some(SplashUpdate::loading("安装依赖库", line, 70).with_subtitle(subtitle));
    }

    // UV: downloading a package
    if line.starts_with("Downloading ") {
        if let Some(pkg) = extract_uv_package_name(line) {
            seen_packages.insert(pkg);
        }
        let progress = uv_download_progress(seen_packages.len(), total_packages);
        return Some(SplashUpdate::loading("安装依赖库", line, progress).with_subtitle(subtitle));
    }

    // UV: preparation complete
    if line.starts_with("Prepared ") {
        return Some(SplashUpdate::loading("安装依赖库", line, 84).with_subtitle(subtitle));
    }

    // UV: install phase complete
    if line.starts_with("Installed ") {
        return Some(SplashUpdate::loading("安装依赖库", line, 88).with_subtitle(subtitle));
    }

    // UV: per-package install confirmation (+ pkg==version) from stdout
    if line.starts_with("+ ") {
        return Some(SplashUpdate::loading("安装依赖库", line, 90).with_subtitle(subtitle));
    }

    // UV: everything already up to date
    if line.starts_with("Audited ") {
        return Some(SplashUpdate::loading("安装依赖库", line, 90).with_subtitle(subtitle));
    }

    None
}

fn is_uv_progress_line(line: &str) -> bool {
    line.starts_with("Resolved ")
        || line.starts_with("Downloading ")
        || line.starts_with("Downloaded ")
        || line.starts_with("Prepared ")
        || line.starts_with("Installed ")
        || line.starts_with("Audited ")
        || line.starts_with("warning: ")
        || line.starts_with("hint: ")
        || line.starts_with("note: ")
}

fn extract_uv_package_name(line: &str) -> Option<String> {
    // "Downloading numpy==2.4.3 (8.2 MiB)" or "Downloading numpy (8.2 MiB)" or "Downloading numpy @ https://..."
    let rest = line.strip_prefix("Downloading ")?;
    let name = rest
        .split_once("==")
        .map(|(n, _)| n)
        .or_else(|| rest.split_once(" @ ").map(|(n, _)| n))
        .or_else(|| rest.split_once(" (").map(|(n, _)| n))
        .unwrap_or(rest);
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty() { None } else { Some(name) }
}

fn uv_download_progress(downloaded: usize, total: usize) -> u8 {
    if total == 0 {
        return 77;
    }
    let clamped = downloaded.min(total) as u16;
    let total = total as u16;
    // 72-82% range for downloads
    (72 + ((clamped * 10) / total) as u8).min(82)
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
