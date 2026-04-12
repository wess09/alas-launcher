**| English | [简体中文](README.md) |**

ALAS Launcher: A New Type of [AzurLaneAutoScript](https://github.com/LmeSzinc/AzurLaneAutoScript) Launcher
===
Background: Since getting a Mac Mini, I've been too lazy to press the power button on my PC. But it feels wrong not running ALAS...

This [blog post](https://www.binss.me/blog/run-azurlaneautoscript-on-arm64/) by binss was very inspiring,
but the methods used either rely on translation layer or Docker containers. As a native purist, I really don't want to run user applications in containers, nor do I want to mess up my system environment. So why not run ALAS natively on MacOS, on Apple Silicon?

Thus this repo was born.

Simple Usage Instructions
---
Go to Releases on the right, download the archive for your system and CPU architecture, and extract it.
- Windows: Run `alas-launcher.exe`. If using Windows 7, 8, or 10, please make sure [WebView2](https://developer.microsoft.com/en-us/Microsoft-edge/webview2) is installed
- MacOS: Open `AzurLaneAutoScript.app`. If there's an error, open Terminal and run `xattr -dr com.apple.quarantine AzurLaneAutoScript.app` (because I don't have an Apple developer certificate to sign the program)
- Linux: Run `alas-launcher`. Note that the program depends on `libwebkit2gtk-4.1` and a recent `glibc` (CI runs on Ubuntu 22.04). If you don't have these, the launcher might not work, but ALAS itself should run fine

License
---
Since ALAS uses GPLv3, we use GPLv3 too. Most dependencies use Apache2, BSD3, etc. - please check upstream repos for details.

Screenshots
---
<table><tr>
<td><img src="screenshots/mac-en.webp" width="640px"></td>
<td><img src="screenshots/win-en.webp" width="580px"></td>
</tr></table>

Differences from Original Version
---
1. Cross-platform, of course.
2. The original launcher updates git repo, kills existing processes, updates pip, updates electron resources, and restarts adb on startup. This version updates the repo and installs pip dependencies according to the `deploy` config; if launched multiple times, it only refocuses the existing window.
3. Python package versions differ from original, but it works fine. Automatic pip updates are enabled by default.
4. Restarting and replacing adb is tricky, not implemented.
5. Directory structure has been modified slightly.

Technical Details
---
1. Used `uv` to download portable Python 3.14.3, so it can run anywhere.
2. Packaged dependencies are now installed from the repo-root `requirements.txt`, instead of relying on the old `deploy/launcher2/requirements.txt`.
3. Used Tauri for the shell. Original GUI's Electron could probably work on Mac, but it looked messy so I gave up after brief research.
4. Packaging scripts, all on GitHub Actions, see `.github/workflows`.
5. Removed some duplicate files. Not sure why *-nix symlinks were all packed as copies, or if it was due to `cp` with hardlinks? Anyway, just deduped with hardlinks. Too lazy to investigate deeper compression.

Directory Structure
---
ALAS Root Directory
* Windows: AzurLaneAutoScript
* MacOS: AzurLaneAutoScript.app/Contents/AzurLaneAutoScript
* Linux: AzurLaneAutoScript

ALAS Launcher
* Windows: AzurLaneAutoScript/alas-launcher.exe
* MacOS: AzurLaneAutoScript.app/Contents/MacOS/alas-launcher
* Linux: AzurLaneAutoScript/alas-launcher

Python
* All systems: toolkit (similar to venv structure)

Git
* Unix: Installed with Unix directory structure to toolkit
* Windows: MinGit extracted to toolkit/git

Adb
* Unix: toolkit/bin/adb
* Windows: toolkit/adb.exe

Environment Variables Added by Launcher
* Unix:
  - toolbox/bin
  - toolbox/libexec/git-core
  - toolbox/lib (LD_LIBRARY_PATH)
* Windows:
  - toolbox
  - toolbox/Scripts
  - toolbox/git/cmd
