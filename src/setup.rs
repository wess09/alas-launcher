use anyhow::{anyhow, bail, Context, Result};
use chrono::Local;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::collections::HashSet;
use std::env::set_current_dir;
use std::fs;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, RecvTimeoutError, Sender},
    Arc,
};
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

#[derive(Default)]
struct GitProgressState {
    progress: u8,
}

const MAX_UPDATE_RETRIES: usize = 20;
const RETRY_DELAY: Duration = Duration::from_secs(1);
const CLEANUP_RETRIES: usize = 20;
const PYTHON_VERSION: &str = "3.14.3";
const DEFAULT_UV_PYTHON_INSTALL_MIRROR: &str =
    "https://ghfast.top/https://github.com/astral-sh/python-build-standalone/releases/download";
const BOOTSTRAP_UV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bootstrap_uv.bin"));

fn default_deploy_config() -> &'static str {
    #[cfg(windows)]
    {
        include_str!("../deploy.windows.yaml")
    }
    #[cfg(target_os = "macos")]
    {
        include_str!("../deploy.mac.yaml")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        include_str!("../deploy.unix.yaml")
    }
}

fn platform_python_config_path() -> &'static str {
    if cfg!(windows) {
        "./.venv/Scripts/python.exe"
    } else {
        "./.venv/bin/python"
    }
}

fn platform_adb_config_path() -> &'static str {
    if cfg!(windows) {
        "./.venv/Scripts/adb.exe"
    } else {
        "./.venv/bin/adb"
    }
}

fn platform_git_config_path() -> &'static str {
    if cfg!(windows) {
        "./.venv/Scripts/git/cmd/git.exe"
    } else {
        "./.venv/bin/git"
    }
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
    // If it's MacOS, it could be AzurPilot.app/Contents/AzurLaneAutoScript
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
    panic!("Cannot find AzurPilot repo folder");
}

fn prepend_path_to_env(key: &str, path: PathBuf) {
    let mut paths = Vec::new();
    paths.push(path);
    if let Some(ref old_path) = &std::env::var_os(key) {
        paths.extend(std::env::split_paths(old_path));
    }
    std::env::set_var(key, std::env::join_paths(paths).unwrap());
}

fn venv_dir() -> PathBuf {
    alas_repo_dir().join(".venv")
}

fn venv_bin_dir() -> PathBuf {
    let venv = venv_dir();
    if cfg!(windows) {
        venv.join("Scripts")
    } else {
        venv.join("bin")
    }
}

pub fn venv_python() -> PathBuf {
    venv_bin_dir().join(if cfg!(windows) {
        "python.exe"
    } else {
        "python"
    })
}

fn venv_python_install_dir() -> PathBuf {
    venv_dir().join("python")
}

fn venv_uv() -> PathBuf {
    venv_bin_dir().join(if cfg!(windows) { "uv.exe" } else { "uv" })
}

fn venv_adb() -> PathBuf {
    venv_bin_dir().join(if cfg!(windows) { "adb.exe" } else { "adb" })
}

fn venv_git() -> PathBuf {
    if cfg!(windows) {
        venv_bin_dir().join("git").join("cmd").join("git.exe")
    } else {
        venv_bin_dir().join("git")
    }
}

fn venv_git_exec_path() -> PathBuf {
    venv_dir().join("libexec").join("git-core")
}

fn venv_git_template_dir() -> PathBuf {
    venv_dir().join("share").join("git-core").join("templates")
}

fn bootstrap_uv_path() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("azurpilot-bootstrap-{}", std::process::id()));
    fs::create_dir_all(&dir)?;
    let path = dir.join(if cfg!(windows) { "uv.exe" } else { "uv" });
    if !path.exists() {
        if BOOTSTRAP_UV.is_empty() {
            if let Some(path_uv) = std::env::var_os("UV").map(PathBuf::from) {
                return Ok(path_uv);
            }
            if let Some(path_uv) = find_on_path("uv") {
                return Ok(path_uv);
            }
            bail!("启动器未内置 uv，且系统 PATH 中找不到 uv");
        }
        fs::write(&path, BOOTSTRAP_UV)
            .with_context(|| format!("写入内置 uv 到 {} 失败", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&path)?.permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions)?;
        }
    }
    Ok(path)
}

fn find_on_path(executable: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(executable);
        if candidate.exists() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let candidate = dir.join(format!("{executable}.exe"));
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

pub fn setup_environment() -> Result<()> {
    let dir = alas_repo_dir();
    info!("AzurPilot dir is {:?}", &dir);
    set_current_dir(&dir)?;
    prepend_path_to_env("PATH", venv_bin_dir());
    if cfg!(windows) {
        prepend_path_to_env("PATH", venv_bin_dir().join("git").join("cmd"));
    } else {
        let git_exec_path = venv_git_exec_path();
        if git_exec_path.exists() {
            std::env::set_var("GIT_EXEC_PATH", git_exec_path);
        }

        let git_template_dir = venv_git_template_dir();
        if git_template_dir.exists() {
            std::env::set_var("GIT_TEMPLATE_DIR", git_template_dir);
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn setup_git_ca_bundle() {
    let cert_file = openssl_probe::probe().cert_file;
    if let Some(file) = cert_file.as_ref().and_then(|f| f.to_str()) {
        std::env::set_var("GIT_SSL_CAINFO", file);
        let _ = Command::new("git")
            .args(["config", "--local", "http.sslCAInfo", file])
            .status();
    }
}

pub fn setup_alas_repo(
    mut status_updater: impl FnMut(SplashUpdate),
    cancel_requested: Arc<AtomicBool>,
) -> Result<()> {
    info!("Starting setup for AzurPilot repository...");
    #[cfg(target_os = "linux")]
    setup_git_ca_bundle();
    // Similar setup to deploy/installer.py
    status_updater(
        SplashUpdate::loading("准备工作区", "正在清理缓存并检查本地环境。", 8)
            .with_subtitle(format!("检查本地环境中... | Tips：{}", get_tip())),
    );
    let bootstrap_uv = bootstrap_uv_path()?;
    ensure_runtime_tools(&bootstrap_uv, &cancel_requested, &mut status_updater)?;
    atomic_failure_cleanup("./config", &cancel_requested)?;
    migrate_dependency_config()?;
    status_updater(
        SplashUpdate::loading("正在更新", "正在获取最新补丁包", 18)
            .with_subtitle(format!("同步中... | Tips：{}", get_tip())),
    );
    git_update(&mut status_updater, &bootstrap_uv, &cancel_requested)?;
    status_updater(
        SplashUpdate::loading(
            "安装依赖库",
            "正在验证 AzurPilot 运行所需的 Python 依赖包。",
            64,
        )
        .with_subtitle(format!("同步依赖包中... | Tips：{}", get_tip())),
    );
    uv_sync_project(&mut status_updater, &bootstrap_uv, &cancel_requested)?;
    status_updater(
        SplashUpdate::loading("完成启动", "本地界面已准备就绪，即将打开主窗口。", 94)
            .with_subtitle(format!("启动服务中... | Tips：{}", get_tip())),
    );
    Ok(())
}

pub fn get_deploy_config() -> Option<JsonValue> {
    let config_content = fs::read_to_string("./config/deploy.yaml").ok()?;
    let config: JsonValue = serde_yaml::from_str(&config_content).ok()?;
    Some(config)
}

pub fn cleanup_runtime_for_rebuild() -> Result<()> {
    let repo_dir = alas_repo_dir();
    let current_exe = std::env::current_exe()?;
    let current_exe_name = current_exe
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("alas-launcher.exe")
        .to_ascii_lowercase();
    let repo_dir = repo_dir.canonicalize()?;
    let exe_dir = current_exe
        .parent()
        .ok_or_else(|| anyhow!("无法获取启动器所在目录"))?
        .canonicalize()?;
    if !cleanup_target_belongs_to_launcher(&repo_dir, &exe_dir) {
        bail!(
            "拒绝清理非启动器目录：{} != {}",
            repo_dir.display(),
            exe_dir.display()
        );
    }

    kill_runtime_processes(&repo_dir);

    let mut failures = Vec::new();
    for entry in fs::read_dir(&repo_dir)? {
        let entry = entry?;
        let path = entry.path();
        if should_keep_runtime_entry(&path, &current_exe_name) {
            info!("Keeping {}", path.display());
            continue;
        }

        info!("Removing {}", path.display());
        if let Err(err) = remove_runtime_entry_with_retry(&path) {
            failures.push(format!("{}: {err:#}", path.display()));
        }
    }

    if !failures.is_empty() {
        bail!("部分文件清理失败：\n{}", failures.join("\n"));
    }

    Ok(())
}

fn kill_runtime_processes(repo_dir: &Path) {
    let current_pid = std::process::id();
    let sys = sysinfo::System::new_all();
    for (pid, process) in sys.processes() {
        if pid.as_u32() == current_pid {
            continue;
        }

        let should_kill = process
            .exe()
            .map(|exe| path_is_inside(exe, repo_dir))
            .unwrap_or(false)
            || process
                .cwd()
                .map(|cwd| path_is_inside(cwd, repo_dir))
                .unwrap_or(false);

        if should_kill {
            info!(
                "Killing runtime process {} ({}) before cleanup",
                pid,
                process.name().to_string_lossy()
            );
            if !process.kill() {
                warn!("Failed to kill runtime process {}", pid);
            }
        }
    }

    thread::sleep(Duration::from_millis(500));
}

fn path_is_inside(path: &Path, parent: &Path) -> bool {
    path.canonicalize()
        .map(|path| path.starts_with(parent))
        .unwrap_or(false)
}

fn cleanup_target_belongs_to_launcher(repo_dir: &Path, exe_dir: &Path) -> bool {
    if repo_dir == exe_dir {
        return true;
    }

    #[cfg(target_os = "macos")]
    {
        let Some(contents_dir) = exe_dir.parent() else {
            return false;
        };
        let expected_repo_dir = contents_dir.join("AzurLaneAutoScript");
        return exe_dir.file_name() == Some(std::ffi::OsStr::new("MacOS"))
            && repo_dir == expected_repo_dir;
    }

    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

fn should_keep_runtime_entry(path: &Path, current_exe_name: &str) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return true;
    };
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "deploy" | "log" | "config" | "bootstrap" | "unins000.dat" | "unins000.exe"
    ) || name == "alas-launcher.exe"
        || name == current_exe_name
}

fn remove_runtime_entry(path: &Path) -> Result<()> {
    clear_readonly(path)?;
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            remove_runtime_entry(&entry?.path())?;
        }
        fs::remove_dir(path).with_context(|| format!("删除目录失败：{}", path.display()))?;
    } else {
        fs::remove_file(path).with_context(|| format!("删除文件失败：{}", path.display()))?;
    }
    Ok(())
}

fn remove_runtime_entry_with_retry(path: &Path) -> Result<()> {
    let mut last_error = None;
    for attempt in 0..CLEANUP_RETRIES {
        match remove_runtime_entry(path) {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_error = Some(err);
                if !path.exists() {
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(250 + attempt as u64 * 100));
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("删除失败：{}", path.display())))
}

fn clear_readonly(path: &Path) -> Result<()> {
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(());
    };
    let mut permissions = metadata.permissions();
    if permissions.readonly() {
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("修改只读权限失败：{}", path.display()))?;
    }
    Ok(())
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
    cancel_requested: &AtomicBool,
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
    let mut git_progress = GitProgressState::default();
    let mut seen_packages = HashSet::new();
    let mut dependency_progress = 64u8;
    let mut dependency_elapsed_secs = 0u16;

    // Receive lines and tee them to stdout/stderr and the status_updater callback.
    loop {
        if cancel_requested.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("启动已取消，正在清理环境");
        }
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok((is_err, line)) => {
                if let Some(mut update) =
                    splash_update_for_output(&line, phase, &mut git_progress, &mut seen_packages)
                {
                    if is_deps {
                        update.progress = update.progress.max(dependency_progress);
                        dependency_progress = update.progress;
                    }
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
            Err(RecvTimeoutError::Timeout) => {
                if is_deps {
                    dependency_elapsed_secs = dependency_elapsed_secs.saturating_add(1);
                    let update =
                        dependency_wait_update(dependency_elapsed_secs, dependency_progress);
                    dependency_progress = update.progress;
                    status_updater(update);
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                break;
            }
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
    cancel_requested: &AtomicBool,
) -> Result<()> {
    for retry in 0..=MAX_UPDATE_RETRIES {
        if cancel_requested.load(Ordering::SeqCst) {
            bail!("启动已取消，正在清理环境");
        }

        match run_command(
            &mut build_cmd(),
            &mut status_updater,
            phase,
            cancel_requested,
        ) {
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

fn run_status_command(
    cmd: &mut Command,
    cancel_requested: &AtomicBool,
) -> Result<std::process::ExitStatus> {
    run_status_command_with_tick(cmd, cancel_requested, || {})
}

fn run_status_command_with_tick(
    cmd: &mut Command,
    cancel_requested: &AtomicBool,
    mut on_tick: impl FnMut(),
) -> Result<std::process::ExitStatus> {
    let mut child = cmd.create_no_window().spawn()?;
    loop {
        if cancel_requested.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("启动已取消，正在清理环境");
        }

        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        on_tick();
        thread::sleep(Duration::from_millis(100));
    }
}

fn phase_display_name(phase: ScriptPhase) -> &'static str {
    match phase {
        ScriptPhase::Git => "代码更新",
        ScriptPhase::Dependencies { .. } => "依赖更新",
    }
}

fn splash_retry_update(phase: ScriptPhase, retry_count: usize, error_text: &str) -> SplashUpdate {
    let detail =
        format!("重试失败 ({retry_count}/{MAX_UPDATE_RETRIES})，1秒后再次尝试：{error_text}");
    match phase {
        ScriptPhase::Git => SplashUpdate::loading("正在更新 AzurPilot", detail, 18)
            .with_subtitle(format!("同步仓库中... | Tips：{}", get_tip())),
        ScriptPhase::Dependencies { .. } => SplashUpdate::loading("安装依赖库", detail, 64)
            .with_subtitle(format!("同步依赖包中... | Tips：{}", get_tip())),
    }
}

fn dependency_wait_update(elapsed_secs: u16, current_progress: u8) -> SplashUpdate {
    let synthetic_progress = (64 + (elapsed_secs / 4) as u8).min(89);
    let progress = current_progress.max(synthetic_progress);
    let detail = if elapsed_secs < 10 {
        "uv 正在解析和同步依赖，请稍候。".to_owned()
    } else {
        format!("uv 正在同步依赖，已运行约 {elapsed_secs} 秒。")
    };

    SplashUpdate::loading("安装依赖库", detail, progress)
        .with_subtitle(format!("同步依赖包中... | Tips：{}", get_tip()))
}

fn git_update(
    status_updater: impl FnMut(SplashUpdate),
    bootstrap_uv: &Path,
    cancel_requested: &AtomicBool,
) -> Result<()> {
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
    let python = venv_python();
    let bootstrap_uv = bootstrap_uv.to_path_buf();
    run_command_with_retry(
        || {
            let mut cmd = Command::new(&python);
            cmd.args(["-c", script])
                .env("AZURPILOT_BOOTSTRAP_UV", &bootstrap_uv);
            cmd
        },
        status_updater,
        ScriptPhase::Git,
        cancel_requested,
    )
}

fn uv_sync_project(
    status_updater: impl FnMut(SplashUpdate),
    bootstrap_uv: &Path,
    cancel_requested: &AtomicBool,
) -> Result<()> {
    let mirror = deploy_pypi_mirror();

    let bootstrap_uv = bootstrap_uv.to_path_buf();
    run_command_with_retry(
        move || {
            let mut cmd = Command::new(&bootstrap_uv);
            cmd.args(["sync", "--frozen", "--no-dev", "--no-install-project"])
                .env("UV_NO_PROGRESS", "1")
                .env("UV_PYTHON_INSTALL_DIR", venv_python_install_dir());
            if let Some(ref m) = mirror {
                cmd.args(["--default-index", m]);
            }
            cmd
        },
        status_updater,
        ScriptPhase::Dependencies { total_packages: 0 },
        cancel_requested,
    )
}

fn migrate_dependency_config() -> Result<()> {
    let path = "./config/deploy.yaml";
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent)?;
    }

    let mut changed = false;
    let content = fs::read_to_string(path).unwrap_or_default();
    let content = if content.trim().is_empty() {
        changed = true;
        default_deploy_config().to_owned()
    } else {
        content
    };

    let mut found_python_executable = false;
    let mut found_adb_executable = false;
    let mut found_git_executable = false;
    let mut found_install_dependencies = false;
    let mut output = String::with_capacity(content.len());

    for line in content.lines() {
        let indent_len = line.len() - line.trim_start().len();
        let indent = &line[..indent_len];
        let trimmed = line.trim_start();
        if trimmed.starts_with("RequirementsFile:") {
            changed = true;
            continue;
        } else if trimmed.starts_with("PythonExecutable:") {
            found_python_executable = true;
            output.push_str(indent);
            output.push_str("PythonExecutable: ");
            output.push_str(platform_python_config_path());
            changed = true;
        } else if trimmed.starts_with("AdbExecutable:") {
            found_adb_executable = true;
            output.push_str(indent);
            output.push_str("AdbExecutable: ");
            output.push_str(platform_adb_config_path());
            changed = true;
        } else if trimmed.starts_with("GitExecutable:") {
            found_git_executable = true;
            output.push_str(indent);
            output.push_str("GitExecutable: ");
            output.push_str(platform_git_config_path());
            changed = true;
        } else if trimmed.starts_with("InstallDependencies:") {
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

    if !found_git_executable {
        output.push_str("\nGitExecutable: ");
        output.push_str(platform_git_config_path());
        output.push('\n');
        changed = true;
    }
    if !found_python_executable {
        output.push_str("PythonExecutable: ");
        output.push_str(platform_python_config_path());
        output.push('\n');
        changed = true;
    }
    if !found_adb_executable {
        output.push_str("AdbExecutable: ");
        output.push_str(platform_adb_config_path());
        output.push('\n');
        changed = true;
    }
    if !found_install_dependencies {
        output.push_str("InstallDependencies: true\n");
        changed = true;
    }

    if changed {
        fs::write(path, output)?;
        info!("Updated self-contained .venv settings in {path}");
    }

    Ok(())
}

fn atomic_failure_cleanup(path: &str, cancel_requested: &AtomicBool) -> Result<()> {
    let mut cmd = Command::new(venv_python());
    cmd.args([
        "-c",
        "import sys; from deploy.atomic import atomic_failure_cleanup; atomic_failure_cleanup(sys.argv[1])",
        path,
    ]);
    let _ = run_status_command(&mut cmd, cancel_requested)?;
    Ok(())
}

fn runtime_tools_update(title: &str, detail: impl Into<String>, progress: u8) -> SplashUpdate {
    SplashUpdate::loading(title, detail, progress)
        .with_subtitle(format!("重建运行环境中... | Tips：{}", get_tip()))
}

fn runtime_wait_update(
    title: &str,
    action: &str,
    elapsed_ticks: u16,
    start_progress: u8,
    end_progress: u8,
) -> SplashUpdate {
    let elapsed_secs = elapsed_ticks / 10;
    let progress = scale_progress(elapsed_secs.min(120) as u8, start_progress, end_progress);
    let detail = if elapsed_secs < 8 {
        format!("{action}，请稍候。")
    } else {
        format!("{action}，已运行约 {elapsed_secs} 秒。")
    };
    runtime_tools_update(title, detail, progress)
}

fn ensure_runtime_tools(
    bootstrap_uv: &Path,
    cancel_requested: &AtomicBool,
    mut status_updater: impl FnMut(SplashUpdate),
) -> Result<()> {
    status_updater(runtime_tools_update(
        "准备运行环境",
        "正在检查内置 Python 和基础工具。",
        9,
    ));
    ensure_self_contained_python(bootstrap_uv, cancel_requested, &mut status_updater)?;
    ensure_deploy_python_dependencies(bootstrap_uv, cancel_requested, &mut status_updater)?;

    status_updater(runtime_tools_update(
        "准备运行环境",
        "正在复制启动器运行所需工具。",
        16,
    ));
    copy_file_if_exists(bootstrap_uv, &venv_uv())?;
    ensure_adb_in_venv()?;
    ensure_git_in_venv()?;
    Ok(())
}

fn deploy_pypi_mirror() -> Option<String> {
    get_deploy_config()
        .as_ref()
        .and_then(|c| c.get("Deploy"))
        .and_then(|d| d.get("Python"))
        .and_then(|p| p.get("PypiMirror"))
        .and_then(|v| v.as_str())
        .filter(|m| !m.is_empty() && *m != "null")
        .map(|m| m.to_owned())
}

fn ensure_deploy_python_dependencies(
    bootstrap_uv: &Path,
    cancel_requested: &AtomicBool,
    mut status_updater: impl FnMut(SplashUpdate),
) -> Result<()> {
    status_updater(runtime_tools_update(
        "准备运行环境",
        "正在检查 deploy 基础依赖 requests。",
        14,
    ));
    let mut import_check = Command::new(venv_python());
    import_check.args(["-c", "import requests"]);
    let status = run_status_command(&mut import_check, cancel_requested)?;
    if status.success() {
        return Ok(());
    }

    let mut cmd = Command::new(bootstrap_uv);
    status_updater(runtime_tools_update(
        "准备运行环境",
        "正在安装 deploy 基础依赖 requests。",
        15,
    ));
    cmd.args(["pip", "install", "--python"])
        .arg(venv_python())
        .arg("requests")
        .env("UV_NO_PROGRESS", "1")
        .env("UV_PYTHON_INSTALL_DIR", venv_python_install_dir());
    if let Some(mirror) = deploy_pypi_mirror() {
        cmd.args(["--default-index", &mirror]);
    }
    let mut elapsed_ticks = 0u16;
    let status = run_status_command_with_tick(&mut cmd, cancel_requested, || {
        elapsed_ticks = elapsed_ticks.saturating_add(1);
        if elapsed_ticks == 1 || elapsed_ticks % 10 == 0 {
            status_updater(runtime_wait_update(
                "准备运行环境",
                "正在安装 deploy 基础依赖 requests",
                elapsed_ticks,
                15,
                16,
            ));
        }
    })?;
    if !status.success() {
        bail!("安装 deploy 基础依赖 requests 失败");
    }
    Ok(())
}

fn uv_python_env(cmd: &mut Command) {
    cmd.env("UV_NO_PROGRESS", "1")
        .env("UV_PYTHON_INSTALL_DIR", venv_python_install_dir());
    if std::env::var_os("UV_PYTHON_INSTALL_MIRROR").is_none() {
        cmd.env("UV_PYTHON_INSTALL_MIRROR", DEFAULT_UV_PYTHON_INSTALL_MIRROR);
    }
}

fn ensure_self_contained_python(
    bootstrap_uv: &Path,
    cancel_requested: &AtomicBool,
    mut status_updater: impl FnMut(SplashUpdate),
) -> Result<()> {
    status_updater(runtime_tools_update(
        "准备运行环境",
        format!("正在检查 Python {PYTHON_VERSION}。"),
        10,
    ));
    if venv_python_works() && managed_python_executable().is_some() {
        return Ok(());
    }

    if managed_python_executable().is_none() {
        status_updater(runtime_tools_update(
            "下载 Python",
            format!("正在下载 Python {PYTHON_VERSION}，首次启动可能需要几分钟。"),
            11,
        ));
        let mut cmd = Command::new(bootstrap_uv);
        cmd.args(["python", "install", "--install-dir"])
            .arg(venv_python_install_dir())
            .args(["--no-bin", "--managed-python", PYTHON_VERSION]);
        uv_python_env(&mut cmd);
        let mut elapsed_ticks = 0u16;
        let status = run_status_command_with_tick(&mut cmd, cancel_requested, || {
            elapsed_ticks = elapsed_ticks.saturating_add(1);
            if elapsed_ticks == 1 || elapsed_ticks % 10 == 0 {
                status_updater(runtime_wait_update(
                    "下载 Python",
                    &format!("正在下载 Python {PYTHON_VERSION}"),
                    elapsed_ticks,
                    11,
                    13,
                ));
            }
        })?;
        if !status.success() {
            bail!("下载 Python {PYTHON_VERSION} 失败");
        }
    }

    let managed_python = managed_python_executable()
        .ok_or_else(|| anyhow!("找不到 .venv 内的 Python {PYTHON_VERSION}"))?;
    status_updater(runtime_tools_update(
        "创建虚拟环境",
        "正在创建 .venv 并绑定内置 Python。",
        13,
    ));
    let mut cmd = Command::new(bootstrap_uv);
    cmd.args(["venv", "--allow-existing", "--relocatable", "--python"])
        .arg(managed_python)
        .arg(venv_dir());
    uv_python_env(&mut cmd);
    let status = run_status_command(&mut cmd, cancel_requested)?;
    if !status.success() {
        bail!("创建 .venv 失败");
    }
    Ok(())
}

fn managed_python_executable() -> Option<PathBuf> {
    let install_dir = venv_python_install_dir();
    let entries = fs::read_dir(install_dir).ok()?;
    let prefix = format!("cpython-{PYTHON_VERSION}-");
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) {
            continue;
        }
        let candidates = if cfg!(windows) {
            vec![path.join("python.exe")]
        } else {
            vec![
                path.join("bin").join("python3.14"),
                path.join("bin").join("python"),
            ]
        };
        for candidate in candidates {
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

fn venv_python_works() -> bool {
    let python = venv_python();
    if !python.exists() {
        return false;
    }
    Command::new(python)
        .args([
            "-c",
            "import sys; raise SystemExit(0 if sys.version_info[:2] == (3, 14) else 1)",
        ])
        .create_no_window()
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn copy_file_if_exists(from: &Path, to: &Path) -> Result<()> {
    if !from.exists() {
        return Ok(());
    }
    if to.exists() && files_match(from, to).unwrap_or(false) {
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建目录 {} 失败", parent.display()))?;
    }
    fs::copy(from, to)
        .with_context(|| format!("复制文件失败：{} -> {}", from.display(), to.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(to)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(to, permissions)?;
    }
    Ok(())
}

fn files_match(left: &Path, right: &Path) -> Result<bool> {
    let left_metadata =
        fs::metadata(left).with_context(|| format!("读取文件信息失败：{}", left.display()))?;
    let right_metadata =
        fs::metadata(right).with_context(|| format!("读取文件信息失败：{}", right.display()))?;
    if left_metadata.len() != right_metadata.len() {
        return Ok(false);
    }

    let left_bytes = fs::read(left).with_context(|| format!("读取文件失败：{}", left.display()))?;
    let right_bytes =
        fs::read(right).with_context(|| format!("读取文件失败：{}", right.display()))?;
    Ok(left_bytes == right_bytes)
}

fn ensure_adb_in_venv() -> Result<()> {
    if venv_adb().exists() {
        return Ok(());
    }
    if cfg!(windows) {
        copy_first_packaged_tool(&["adb.exe"], &venv_bin_dir())?;
        copy_matching_packaged_tools("Adb", "dll", &venv_bin_dir())?;
    } else {
        copy_first_packaged_tool(&["adb"], &venv_bin_dir())?;
    }
    Ok(())
}

fn ensure_git_in_venv() -> Result<()> {
    if cfg!(windows) {
        if !venv_git().exists() {
            let src = PathBuf::from("bootstrap").join("git");
            let dst = venv_bin_dir().join("git");
            if src.exists() {
                copy_dir_all(&src, &dst)?;
            }
        }
    } else {
        if !venv_git().exists() {
            copy_first_packaged_tool(&["git"], &venv_bin_dir())?;
        }

        let git_core_src = PathBuf::from("bootstrap").join("git-core");
        let git_core_dst = venv_git_exec_path();
        if !git_core_dst.exists() && git_core_src.exists() {
            copy_dir_all(&git_core_src, &git_core_dst)?;
        }

        let templates_src = PathBuf::from("bootstrap").join("git-templates");
        let templates_dst = venv_git_template_dir();
        if !templates_dst.exists() && templates_src.exists() {
            copy_dir_all(&templates_src, &templates_dst)?;
        }
    }
    Ok(())
}

fn copy_first_packaged_tool(names: &[&str], target_dir: &Path) -> Result<()> {
    for name in names {
        let source = PathBuf::from("bootstrap").join(name);
        if source.exists() {
            copy_file_if_exists(&source, &target_dir.join(name))?;
            return Ok(());
        }
    }
    Ok(())
}

fn copy_matching_packaged_tools(prefix: &str, extension: &str, target_dir: &Path) -> Result<()> {
    let dir = PathBuf::from("bootstrap");
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(prefix) && name.ends_with(extension) {
            copy_file_if_exists(&entry.path(), &target_dir.join(name.as_ref()))?;
        }
    }
    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            copy_file_if_exists(&entry.path(), &target)?;
        }
    }
    Ok(())
}

fn splash_update_for_output(
    line: &str,
    phase: ScriptPhase,
    git_progress: &mut GitProgressState,
    seen_packages: &mut HashSet<String>,
) -> Option<SplashUpdate> {
    let sanitized = line.trim();
    if sanitized.is_empty() {
        return None;
    }

    match phase {
        ScriptPhase::Git => splash_update_for_git_output(sanitized, git_progress),
        ScriptPhase::Dependencies { total_packages } => {
            splash_update_for_dependency_output(sanitized, total_packages, seen_packages)
        }
    }
}

fn splash_update_for_git_output(line: &str, state: &mut GitProgressState) -> Option<SplashUpdate> {
    let subtitle = format!("同步仓库中... | Tips：{}", get_tip());

    if line.contains("=====") {
        let detail = line.replace('=', " ");
        let progress = git_section_progress(detail.trim()).unwrap_or(24);
        return Some(git_update_splash(
            line,
            detail.trim(),
            progress,
            state,
            subtitle,
        ));
    }

    if let Some(progress) = git_line_progress(line) {
        return Some(git_update_splash(line, line, progress, state, subtitle));
    }

    None
}

fn git_update_splash(
    raw_line: &str,
    detail: &str,
    progress: u8,
    state: &mut GitProgressState,
    subtitle: String,
) -> SplashUpdate {
    state.progress = state.progress.max(progress);
    let display_detail = if detail.trim().is_empty() {
        raw_line
    } else {
        detail
    };
    SplashUpdate::loading("正在更新 AzurPilot", display_detail, state.progress)
        .with_subtitle(subtitle)
}

fn git_section_progress(section: &str) -> Option<u8> {
    if section.contains("SHOW DEPLOY CONFIG") {
        Some(18)
    } else if section.contains("UPDATE AZURPILOT") {
        Some(20)
    } else if section.contains("GIT INIT") {
        Some(22)
    } else if section.contains("SET GIT PROXY") {
        Some(23)
    } else if section.contains("SET GIT REPOSITORY") {
        Some(24)
    } else if section.contains("FETCH REPOSITORY BRANCH") {
        Some(25)
    } else if section.contains("PULL REPOSITORY BRANCH") {
        Some(58)
    } else if section.contains("SHOW VERSION") {
        Some(63)
    } else {
        None
    }
}

fn git_line_progress(line: &str) -> Option<u8> {
    let percentage = find_percentage(line)?;
    if line.contains("Counting objects:") || line.contains("Compressing objects:") {
        Some(scale_progress(percentage, 25, 29))
    } else if line.contains("Receiving objects:") {
        Some(scale_progress(percentage, 29, 52))
    } else if line.contains("Resolving deltas:") {
        Some(scale_progress(percentage, 52, 58))
    } else if line.contains("Updating files:") {
        Some(scale_progress(percentage, 58, 62))
    } else {
        None
    }
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
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn uv_download_progress(downloaded: usize, total: usize) -> u8 {
    if total == 0 {
        return 77;
    }
    let clamped = downloaded.min(total) as u16;
    let total = total as u16;
    // 72-82% range for downloads
    scale_progress(((clamped * 100) / total) as u8, 72, 82)
}

fn scale_progress(percentage: u8, start: u8, end: u8) -> u8 {
    let percentage = percentage.min(100) as u16;
    let start = start as u16;
    let end = end as u16;
    (start + ((percentage * (end - start)) / 100)) as u8
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

    #[test]
    fn test_git_line_progress_ranges() {
        assert_eq!(
            Some(25),
            git_line_progress("remote: Counting objects:   1% (1/66)")
        );
        assert_eq!(
            Some(40),
            git_line_progress("Receiving objects:  50% (43546/87092), 179.10 MiB | 5.03 MiB/s")
        );
        assert_eq!(
            Some(58),
            git_line_progress("Resolving deltas: 100% (66157/66157), done.")
        );
        assert_eq!(
            Some(62),
            git_line_progress("Updating files: 100% (9881/9881), done.")
        );
    }

    #[test]
    fn test_git_section_progress_ranges() {
        assert_eq!(Some(25), git_section_progress("FETCH REPOSITORY BRANCH"));
        assert_eq!(Some(58), git_section_progress("PULL REPOSITORY BRANCH"));
        assert_eq!(Some(63), git_section_progress("SHOW VERSION"));
    }
}
