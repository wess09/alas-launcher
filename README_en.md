**| English | [简体中文](README.md) |**

AzurPilot: A New Type of [AzurLaneAutoScript](https://github.com/LmeSzinc/AzurLaneAutoScript) Launcher
===
Background: Since getting a Mac Mini, I've been too lazy to press the power button on my PC. But it feels wrong not running AzurPilot...

This [blog post](https://www.binss.me/blog/run-azurlaneautoscript-on-arm64/) by binss was very inspiring,
but the methods used either rely on translation layer or Docker containers. As a native purist, I really don't want to run user applications in containers, nor do I want to mess up my system environment. So why not run AzurPilot natively on MacOS, on Apple Silicon?

Thus this repo was born.

Simple Usage Instructions
---
Go to Releases on the right, download the archive for your system and CPU architecture, and extract it.
- Windows: Run `alas-launcher.exe`. If using Windows 7, 8, or 10, please make sure [WebView2](https://developer.microsoft.com/en-us/Microsoft-edge/webview2) is installed
- MacOS: Open `AzurPilot.app`. If there's an error, open Terminal and run `xattr -dr com.apple.quarantine AzurPilot.app` (because I don't have an Apple developer certificate to sign the program)
- Linux: Run `alas-launcher`. Note that the program depends on `libwebkit2gtk-4.1` and a recent `glibc` (CI runs on Ubuntu 22.04). If you don't have these, the launcher might not work, but AzurPilot itself should run fine

License
---
Since AzurPilot uses GPLv3, we use GPLv3 too. Most dependencies use Apache2, BSD3, etc. - please check upstream repos for details.

Screenshots
---
<table><tr>
<td><img src="screenshots/mac-en.webp" width="640px"></td>
<td><img src="screenshots/win-en.webp" width="580px"></td>
</tr></table>

Differences from Original Version
---
1. Cross-platform, of course.
2. The original launcher updates git repo, kills existing processes, updates pip, updates electron resources, and restarts adb on startup. This version updates the repo and syncs dependencies with the uv embedded in `.venv`; if launched multiple times, it only refocuses the existing window.
3. Python package versions are locked by `pyproject.toml` and `uv.lock`. Automatic sync is enabled by default.
4. Restarting and replacing adb is tricky, not implemented.
5. Directory structure has been modified slightly.

Technical Details
---
1. Uses the launcher's embedded uv to create a relocatable `.venv`, so users do not need system uv or Python.
2. Packaged dependencies are synced from the repo-root `pyproject.toml` and `uv.lock`, and runtime sync uses the same uv project metadata.
3. Used Tauri for the shell. Original GUI's Electron could probably work on Mac, but it looked messy so I gave up after brief research.
4. Packaging scripts, all on GitHub Actions, see `.github/workflows`.
5. Removed some duplicate files. Not sure why *-nix symlinks were all packed as copies, or if it was due to `cp` with hardlinks? Anyway, just deduped with hardlinks. Too lazy to investigate deeper compression.

Directory Structure
---
AzurPilot Root Directory
* Windows: AzurLaneAutoScript
* MacOS: AzurPilot.app/Contents/AzurLaneAutoScript
* Linux: AzurLaneAutoScript

AzurPilot
* Windows: AzurLaneAutoScript/alas-launcher.exe
* MacOS: AzurPilot.app/Contents/MacOS/alas-launcher
* Linux: AzurLaneAutoScript/alas-launcher

Python / uv
* All systems: `.venv`

Git
* Unix: `.venv/bin/git`
* Windows: `.venv/Scripts/git/cmd/git.exe`

Adb
* Unix: `.venv/bin/adb`
* Windows: `.venv/Scripts/adb.exe`

Environment Variables Added by Launcher
* Unix:
  - `.venv/bin`
* Windows:
  - `.venv/Scripts`
  - `.venv/Scripts/git/cmd`
