**| [English](README_en.md) | 简体中文 |**

ALAS Launcher: 一种新型的 [AzurLaneAutoScript](https://github.com/LmeSzinc/AzurLaneAutoScript) 启动器
===
故事背景：自从用上了 Mac Mini，PC 的开机键都懒得去按了。但是不开个 ALAS 怎么都不舒服不是……

前人大佬 binss 写的[这篇博客](https://www.binss.me/blog/run-azurlaneautoscript-on-arm64/)给了很多启发，
但这篇文章里用的方法不是走了转译就是多少要套层 docker。作为一个原生主义者，我实在不想套层壳跑用户端程序，
也不想把系统环境搞得乱七八糟。所以为什么不能，在MacOS，在林檎硅上，原生的，把 ALAS 给跑起来呢？

于是就有了这个 Repo。

简单易懂的使用方法
---
去右边 Releases 里，下载你对应系统和 CPU 的压缩包，解压。
- Windows: 打开 `alas-launcher.exe`。如果使用 Windows 7, 8 或 10，请确保已经安装 [WebView2](https://developer.microsoft.com/zh-cn/microsoft-edge/webview2)
- MacOS: 打开 `AzurLaneAutoScript.app`。如果报错则需要先打开终端，运行 `xattr -dr com.apple.quarantine AzurLaneAutoScript.app` （因为我没有林檎开发者给程序签名）
- Linux: 打开 `alas-launcher`。注意程序依赖 `libwebkit2gtk-4.1` 和较新的 `glibc` （用 Ubuntu 22.04 跑的 CI）。如果没有，可能这启动器没法跑，但是 ALAS 本体跑起来应该没问题的

许可协议
---
因为 ALAS 用 GPLv3 所以咱也用 GPLv3。依赖软件大多是Apache2，BSD3啥的，请自行去上游找吧。。。

截图
---
<table><tr>
<td><img src="screenshots/mac-cn.webp" width="640px"></td>
<td><img src="screenshots/win-cn.webp" width="580px"></td>
</tr></table>

和原版的区别
---
1. 当然是全平台。
2. 原版启动时除了更新 git repo 还会杀掉现有进程，更新 pip，更新 electron 资源，重启 adb。这个版本的启动器会更新 repo，并用 `.venv` 内置的 uv 同步依赖；如果重复启动，只会重新聚焦已有窗口。
3. Python 包版本由 `pyproject.toml` 和 `uv.lock` 锁定，自动同步默认已启用。
4. 重启和替换adb不好搞，没做。
5. 目录结构变动了一下。

具体折腾了些啥？
---
1. 用启动器内嵌的 uv 创建可重定位 `.venv`，这样用户机器无需预装 uv 或 Python。
2. 打包时按根目录的 `pyproject.toml` 和 `uv.lock` 同步依赖，运行时也继续用 uv 自动补齐。
3. 用 Tauri 搓了层壳。理论上原 GUI 用的 Electron 不是不能用吧，在 Mac 上应该可以跑，但怎么看都很草，我研究两下就放弃了。
4. 打包脚本，全程 GitHub Actions，见 `.github/workflows`。
5. 稍微去了一下重复文件，不知道为啥 *-nix 应该是符号链接的全给包成了复制，还是说原本应该是硬链接 `cp` 导致的？不知道，反正直接硬链接去重了。懒得研究深度缩小体积了。

目录结构
---
ALAS 根目录
* Windows: AzurLaneAutoScript
* MacOS: AzurLaneAutoScript.app/Contents/AzurLaneAutoScript
* Linux: AzurLaneAutoScript

ALAS 启动器
* Windows: AzurLaneAutoScript/alas-launcher.exe
* MacOS: AzurLaneAutoScript.app/Contents/MacOS/alas-launcher
* Linux: AzurLaneAutoScript/alas-launcher

Python / uv
* 所有系统: `.venv`

Git
* Unix: `.venv/bin/git`
* Windows: `.venv/Scripts/git/cmd/git.exe`

Adb
* Unix: `.venv/bin/adb`
* Windows: `.venv/Scripts/adb.exe`

启动器会加的环境变量
* Unix:
  - `.venv/bin`
* Windows:
  - `.venv/Scripts`
  - `.venv/Scripts/git/cmd`
