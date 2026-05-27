# CLAUDE.md

本文件为 Claude Code (claude.ai/code) 在本仓库中工作时提供指引。

**必须使用中文与用户交流。**

## 项目概述

AzurPilot Launcher 是 [AzurPilot](https://github.com/wess09/AzurPilot) 的跨平台（Windows/macOS/Linux）桌面启动器，基于 **Tauri 2 + Rust** 构建。它通过内嵌的 `uv` 二进制文件管理独立的 Python 3.14.3 环境，处理基于 git 的更新，启动 Python WebUI 后端（`gui.py`），并提供原生 webview 壳（含启动画面、系统托盘、通知和自定义标题栏）。

## 构建与开发命令

```bash
# 构建（debug）
cargo build

# 构建（release — 去除符号，启用 LTO）
cargo build --release

# 开发模式运行
cargo tauri dev

# 运行全部测试
cargo test

# 运行单个测试
cargo test test_name_here

# 仅检查编译
cargo check
```

构建脚本（`build.rs`）需要 `ALAS_BOOTSTRAP_UV` 环境变量指向要内嵌的 `uv` 二进制文件。本地开发不设置时使用空占位符，启动器会在运行时从 PATH 中查找 `uv` 或通过环境变量 `UV` 指定。

## 架构

### 源码模块（`src/`）

| 模块 | 行数 | 职责 |
|---|---|---|
| `main.rs` | ~2140 | Tauri 应用入口、窗口管理、启动画面、系统托盘、时间炸弹、自定义标题栏注入、错误页面 |
| `backend.rs` | ~330 | 启动/终止 `gui.py` 子进程、端口扫描与占用进程清理、后端生命周期管理 |
| `setup.rs` | ~1460 | 环境配置：Python/uv/adb/git 安装、git 更新、uv 依赖同步、deploy.yaml 迁移、运行时清理 |
| `notify.rs` | ~320 | SSE 通知流、平台原生桌面通知（Windows Toast / Linux notify-rust / macOS Tauri 插件） |
| `window_util.rs` | ~45 | Windows `CREATE_NO_WINDOW` trait，控制子进程是否创建控制台窗口 |

### 运行时流程

1. `main()` 初始化日志（写入 `log/{date}_launcher.txt`），读取 `config/deploy.yaml` 获取 WebUI 配置
2. 创建 Tauri 应用：显示 splash 窗口（`alas-splash://` 自定义协议），隐藏 main 窗口
3. 后台线程执行 `setup_alas_repo()`：
   - `ensure_runtime_tools()`：下载 Python 3.14.3（通过 uv managed python）、创建可重定位 `.venv`、复制 uv/adb/git 到 `.venv`
   - `git_update()`：通过 Python 脚本调用 `deploy.git.GitManager` 拉取最新代码（带重试，最多 20 次）
   - `uv_sync_project()`：执行 `uv sync --frozen --no-dev --no-install-project` 安装依赖
4. `ManagedBackend::new()` 启动 `gui.py`，设置 `ALAS_LAUNCHER_PID` 环境变量，等待端口就绪（60 秒超时）
5. 启动 SSE 通知流（`/api/notify_stream`），销毁 splash 窗口，显示 main 窗口

### 自定义 URI 协议

- `alas-splash://`（Windows/Android 用 `http://alas-splash.localhost/`）：内嵌启动画面 HTML/CSS/JS，进度条通过 `window.__ALAS_SPLASH_UPDATE()` 回调更新
- `alas-error://`（Windows/Android 用 `http://alas-error.localhost/`）：后端连接失败页面，每秒自动重试连接

### 自定义标题栏

Windows 和 Linux 移除原生窗口装饰（`set_decorations(false)`），通过 `page_load_injector` 在每个页面加载完成后注入自定义标题栏 JS（红绿灯按钮 + 拖拽区域）。macOS 保留原生标题栏。注入脚本还会：
- 覆盖 `window.saveAs` 使其通过 Tauri 的 `save_as` 命令保存文件
- 阻止浏览器后退（`history.pushState` + `popstate` 监听）

### 窗口关闭行为

- **Windows**：弹出对话框（"退出" / "最小化到托盘"），最小化时 `destroy()` 主窗口释放 WebView 资源，恢复时重新创建
- **macOS**：最小化到托盘，切换 `ActivationPolicy` 为 `Accessory`（隐藏 Dock 图标），恢复时切回 `Regular`
- **Linux**：直接隐藏窗口

### Tauri 前端命令

`save_as`、`download_today_gui_log`、`download_today_launcher_log`、`retry_backend_connection`、`window_hide`、`window_minimize`、`window_toggle_maximize`、`window_close`、`window_start_dragging`、`window_is_maximized`

### 端口占用清理

`backend.rs` 在启动 `gui.py` 前会清理占用目标端口的进程：
- **Windows**：解析 `netstat -ano -p tcp` 输出获取 PID
- **Unix**：通过 `lsof -nP -iTCP:{port} -sTCP:LISTEN -t` 获取 PID
- 使用 `sysinfo` 库 kill 进程，等待端口释放（最多 5 秒）

### 子进程泄漏清理

`ManagedBackend` 的 `Drop` 实现会扫描所有进程的环境变量，查找包含 `ALAS_LAUNCHER_PID={当前PID}` 的子进程并 kill，防止 `gui.py` 子进程泄漏。

### 平台相关代码

大量使用 `#[cfg(target_os = "...")]`。`Cargo.toml` 中的平台依赖：
- **Windows**：`winapi`、`tauri-winrt-notification`、`windows-registry`
- **Linux**：`notify-rust`、`openssl-probe`（CA 证书探测）
- **Unix**：`nix`（信号处理，SIGTERM 优雅退出）
- **桌面端（非移动端）**：`tauri-plugin-single-instance`（单实例，第二次启动时恢复窗口）

### 部署配置

六个 `deploy.*.yaml` 文件配置不同平台/镜像组合。带 `-cn` 后缀的变体使用中国大陆可访问的镜像源。`setup.rs` 中的 `migrate_dependency_config()` 会在启动时自动迁移 `config/deploy.yaml`：
- 更新 Python/Adb/Git 可执行文件路径为 `.venv` 内路径
- 移除已废弃的 `RequirementsFile` 配置项
- 强制设置 `InstallDependencies: true`

### 时间炸弹机制

`Cargo.toml` 中包含 `[package.metadata.alas-launcher.time-bomb]` 配置段。`main.rs` 通过 `include_str!("../Cargo.toml")` 在运行时解析此配置（非编译时）。当 `enabled = true` 时，启动器通过 HTTP 请求获取网络时间（`Date` 头），与过期日期比较，过期则弹窗拒绝运行。

### CI/CD

GitHub Actions（`.github/workflows/package.yml`）：
- 触发条件：tag push 或手动 `workflow_dispatch`
- 构建矩阵：`ubuntu-22.04`、`macos-latest`、`windows-latest`
- Linux/macOS 从源码编译 Git v2.49.1，Windows 下载 MinGit v2.51.0
- 下载 Android platform-tools（adb）
- 创建可重定位 `.venv`（Python 3.14.3 + uv + adb + git + requests）
- 打包为 `tar.xz` 归档（国际版 + CN 镜像版）
- 部署启动器自动更新载荷到 `alas.nanoda.work`（通过 SSH）

### 关键常量与配置

- 默认 WebUI 端口：`22267`
- Python 版本：`3.14.3`（`setup.rs` 中 `PYTHON_VERSION`）
- Git 更新最大重试：20 次，间隔 1 秒
- 后端端口等待超时：60 秒
- 后端连接检查超时：500 毫秒
- 通知流断线重连间隔：3 秒
- `UV_PYTHON_INSTALL_MIRROR` 默认使用 npmmirror 加速 Python standalone 下载，并以 python-standalone.org 作为备用源
