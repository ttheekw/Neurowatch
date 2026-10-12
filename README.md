<p align="center">
  <img src="ui/neurowatch/resources/icons/neurowatch.svg" width="96" height="96" alt="NeuroWatch logo">
</p>

<h1 align="center">NeuroWatch</h1>

<p align="center">
  <b>Turn the computer you already have into a security camera recorder.</b><br>
  Watch every camera live, record around the clock to your own drive, and find any moment in seconds.<br>
  Free and open source. No camera limit. No account. No subscription.
</p>

<p align="center">
  <a href="https://github.com/ttheekw/Neurowatch/releases/latest/download/neurowatch-windows-x86_64-setup.exe"><img src="https://img.shields.io/badge/Download_for_Windows-10_%26_11,_64--bit-2563eb?style=for-the-badge" alt="Download NeuroWatch for Windows 10 and 11"></a>
  &nbsp;
  <a href="https://github.com/ttheekw/Neurowatch/releases/latest/download/neurowatch_amd64.deb"><img src="https://img.shields.io/badge/Download_for_Ubuntu_%2F_Debian-.deb,_64--bit-E95420?style=for-the-badge" alt="Download NeuroWatch for Ubuntu and Debian"></a>
</p>

<p align="center">
  Also for
  <a href="https://github.com/ttheekw/Neurowatch/releases/latest/download/neurowatch.x86_64.rpm">Fedora (.rpm)</a> ·
  <a href="https://github.com/ttheekw/Neurowatch/releases/latest/download/neurowatch-x86_64.pkg.tar.zst">Arch Linux</a> ·
  <a href="https://github.com/ttheekw/Neurowatch/releases/latest/download/neurowatch_arm64.deb">Raspberry Pi &amp; ARM (.deb)</a> ·
  <a href="https://github.com/ttheekw/Neurowatch/releases/latest/download/neurowatch.aarch64.rpm">Fedora ARM</a> ·
  portable <a href="https://github.com/ttheekw/Neurowatch/releases/latest/download/neurowatch-windows-x86_64.zip">Windows .zip</a>,
  <a href="https://github.com/ttheekw/Neurowatch/releases/latest/download/neurowatch-linux-x86_64.tar.gz">Linux .tar.gz</a>,
  <a href="https://github.com/ttheekw/Neurowatch/releases/latest/download/neurowatch-linux-arm64.tar.gz">ARM .tar.gz</a><br>
  <a href="https://github.com/ttheekw/Neurowatch/releases/latest">Release notes and checksums</a> ·
  <a href="#installing">Installation help</a> ·
  <a href="docs/system-requirements.md">System requirements</a>
</p>

<p align="center">
  <a href="https://github.com/ttheekw/Neurowatch/releases/latest"><img src="https://img.shields.io/github/v/release/ttheekw/Neurowatch?include_prereleases&label=latest" alt="Latest version"></a>
  <a href="https://github.com/ttheekw/Neurowatch/actions/workflows/ci.yml?query=branch%3Amain"><img src="https://github.com/ttheekw/Neurowatch/actions/workflows/ci.yml/badge.svg?branch=main" alt="Tests"></a>
  <a href="#installing"><img src="https://img.shields.io/badge/platform-Linux%20%7C%20Windows-informational" alt="Linux and Windows"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPL--3.0--or--later-blue" alt="License: AGPL-3.0-or-later"></a>
</p>

### Why NeuroWatch

- **Your footage stays yours.** Everything is recorded to your own drive. Nothing leaves your computer unless you choose to turn on the cloud.
- **No limits, no fees.** Add as many cameras as your computer can handle. The app is free and open source, for home and business use.
- **Works with the cameras you already own.** Any RTSP or ONVIF IP camera; **Search Network** finds them for you in a click.
- **Never misses a moment.** Recording carries on in the background after you close the window, starts with your computer, and reconnects cameras by itself when they drop.
- **Watch from anywhere, if you want to.** The optional NeuroWatch Cloud shows your cameras in a browser, free for up to 16 cameras.

> **Beta:** every release is install-tested on Windows, Ubuntu 22.04 and 24.04, Debian 12 and 13, Fedora 43 and Arch Linux before it is published. Found a problem? [Tell us](https://github.com/ttheekw/Neurowatch/issues), it makes the next release better.

NeuroWatch is **free and open source** ([AGPL-3.0-or-later](LICENSE)). The optional NeuroWatch Cloud, for watching from a browser, is a separate service in the official downloads (see [Licensing](#licensing)).

## Contents

- [Features](#features)
- [Installing](#installing)
- [Getting started](#getting-started)
- [Everyday use](#everyday-use)
- [Running without a screen](#running-without-a-screen)
- [Troubleshooting](#troubleshooting)
- [Privacy and responsible use](#privacy-and-responsible-use)
- [Disclaimer](#disclaimer)
- [Licensing](#licensing)
- [For developers](#for-developers)

---

## Features

### Live video wall
- Watch all your cameras on one screen, in a ready-made or custom grid (2×2, 3×3, 4×4, and so on).
- More cameras than fit? They continue on further pages. Flip through them with the arrows or keyboard, or let the wall **auto-scroll**. Pages switch instantly, without black screens.
- **Arrange the wall with the mouse:** drag a camera onto another to swap them, drag it to an empty slot to move it, or drag its corner to make it larger (2×2, 3×2, …). Hold a camera at the edge of the wall to carry it to another page.
- **Double-click** any camera to watch it alone, full size. Hold **Ctrl** and scroll to zoom into a picture.
- **Full screen** with one key, and **Hide panels** to leave nothing but the video. The status readings can stay on screen over the video, with an adjustable see-through background.
- **More windows and monitors:** move a page of cameras into its own window, put it on another monitor, and give it its own grid and layout. A window can hold several pages and cycle through them. Your windows come back where you left them the next time you start NeuroWatch.
- Uses your graphics card to decode video where possible (Intel/AMD and NVIDIA), and shows smaller grid streams in multi-camera views.

### Cameras
- **Search Network** finds ONVIF cameras on your network, shows their model and whether they're ready, need a password, or are already added. Then add them in a few clicks.
- Or add any camera by its address: RTSP/RTSPS, RTMP, MJPEG over HTTP(S), USB cameras, or a video file.
- **Several streams per camera:** NeuroWatch records the best-quality stream and shows a lighter one in the grid. You can pick another stream for any tile from its right-click menu.
- Test a camera before saving it, and see each camera's live status, codec and resolution on its tile.
- **Audio:** cameras that send sound (AAC, G.711 or Opus) are recorded with it. Tiles of cameras with audio show a speaker at the top right: click it, or right-click → **Listen**, to hear that camera live. One camera at a time; the sound stops when the camera leaves the screen.
- NeuroWatch won't add the same camera twice, and reconnects by itself when a camera drops out.

### Recording and playback
- Record all cameras or only the ones you choose, around the clock, to your computer's drive.
- **Choose Drive…** lists your drives with their free space and records to the one you pick. The new location takes effect immediately.
- **Disk cleanup:** when the recording drive gets fuller than a limit you choose (90% by default), the oldest recordings are deleted to make room, so recording never stops for lack of space. Set it in **Settings → Recording → Disk Cleanup**. Only NeuroWatch's own recordings are deleted, never your other files. Each camera can also have its own **Storage Quota**.
- **Playback:** a timeline with several cameras side by side, with play/pause, seek and speed controls.
- **Export Video:** **File → Export Video…** saves a clip as an MP4 file you can share. Load a camera's recordings, drag the yellow handles on the timeline to the part you want (a preview plays it), press **Export…** and choose where to save. The video is copied as recorded, so it is quick and keeps the camera's quality. A clip may start a moment before the time you chose, and gaps where nothing was recorded are left out.
- **Export from the web portal:** a camera's **Export** tab in NeuroWatch Cloud lets you play its recordings and drag the start and end of a clip on the timeline; the recorder makes the MP4 and the portal offers it for download for 7 days.
- **Playback with sound:** recordings with audio play it back. The first camera with audio is heard; click another tile's speaker to switch, or the same one to mute. Sound keeps its normal pitch from 0.5× to 2× speed and is muted at faster or slower speeds.
- **Sound in the web portal:** live view and recordings watched in the portal carry the camera's sound too. It starts muted; click **Sound off** above the video to hear it. Recordings play their sound at 1× speed.

### Always recording, in the background
- NeuroWatch keeps recording after you close the window. By default it starts by itself when you log in, so nothing is missed after a restart.
- A **tray icon** shows NeuroWatch's state at a glance. Hover over it for a short summary, or open its menu to open NeuroWatch or start, stop or restart recording.
  - The **bottom dot** is green when everything is fine, amber when something needs attention, red when NeuroWatch is stopped or every camera is offline, and grey while starting or stopping.
  - A small **red dot at the top** appears when there's a problem: cameras offline, a recording that failed, the cloud unreachable, or the computer struggling (CPU, memory or graphics). The menu names each problem.
- **Headless mode** for a computer without a screen: NeuroWatch runs as a background service that starts when the computer boots, even if nobody logs in. See [Running without a screen](#running-without-a-screen).

### NeuroWatch Cloud (optional, official downloads)
NeuroWatch Cloud is an online service, free for up to 16 cameras, with paid plans for more. It is included in the official downloads from the Releases page, not in builds made from this source code.
- Link NeuroWatch to your NeuroWatch Cloud account with a one-time code (**≡ menu → Cloud Link…**).
- Then watch live video and recordings from a web browser, with camera thumbnails.
- On a computer without a screen, manage everything from the web portal: add and edit cameras, turn recording on or off, and choose the recording drive. You can also see the same diagnostics as in the desktop app.
- Camera addresses and passwords you enter in the portal are encrypted between your browser and your recorder, so the cloud only passes them along and can't read them.
- Without linking, nothing leaves your computer.

### Status and diagnostics
- A status bar shows disk space, CPU, memory, network speed, whether video is decoded by the graphics card, and the recorder connection (and the cloud connection, in official downloads).
- The **Diagnostics** tab shows how hard the computer is working, per-camera frame rates and dropped frames, and graphics card usage where the driver reports it.
- Light and dark themes.

### Coming later
Motion detection and events, recording schedules, AI object detection, multi-site management, user accounts and permissions, PTZ camera control and a mobile app are planned. They are not part of NeuroWatch yet.

---

## Installing

Each link below downloads the newest version. Older versions, release notes and checksums are on the [Releases page](https://github.com/ttheekw/Neurowatch/releases). NeuroWatch is in beta: it works, but hasn't been tested on many computers yet, so please [report problems](https://github.com/ttheekw/Neurowatch/issues).

On Ubuntu, Debian, Linux Mint and Fedora you can **double-click** the downloaded package and choose **Install**; the terminal command next to it does the same, run in the folder you downloaded it to (usually `~/Downloads`):

| System | Download | Install |
|---|---|---|
| Windows 10/11 (64-bit) | [Installer (.exe)](https://github.com/ttheekw/Neurowatch/releases/latest/download/neurowatch-windows-x86_64-setup.exe) | Run the installer, then open NeuroWatch from Start. Windows may warn that the installer is unrecognised: choose **More info → Run anyway**. |
| Windows 10/11 (portable) | [.zip](https://github.com/ttheekw/Neurowatch/releases/latest/download/neurowatch-windows-x86_64.zip) | Extract it, then run `neurowatch.cmd` |
| Ubuntu, Debian, Linux Mint (64-bit PC) | [.deb](https://github.com/ttheekw/Neurowatch/releases/latest/download/neurowatch_amd64.deb) | Double-click → **Install**, or `sudo apt install ./neurowatch*_amd64.deb` |
| Ubuntu, Debian, Raspberry Pi OS 64-bit (ARM) | [.deb](https://github.com/ttheekw/Neurowatch/releases/latest/download/neurowatch_arm64.deb) | Double-click → **Install**, or `sudo apt install ./neurowatch*_arm64.deb` |
| Fedora (64-bit PC) | [.rpm](https://github.com/ttheekw/Neurowatch/releases/latest/download/neurowatch.x86_64.rpm) | Double-click → **Install**, or `sudo dnf install ./neurowatch*.x86_64.rpm` |
| Fedora (ARM) | [.rpm](https://github.com/ttheekw/Neurowatch/releases/latest/download/neurowatch.aarch64.rpm) | Double-click → **Install**, or `sudo dnf install ./neurowatch*.aarch64.rpm` |
| Arch Linux (64-bit PC) | [.pkg.tar.zst](https://github.com/ttheekw/Neurowatch/releases/latest/download/neurowatch-x86_64.pkg.tar.zst) | `sudo pacman -U ./neurowatch*-x86_64.pkg.tar.zst` |
| Other Linux (portable) | [x86_64](https://github.com/ttheekw/Neurowatch/releases/latest/download/neurowatch-linux-x86_64.tar.gz) or [ARM](https://github.com/ttheekw/Neurowatch/releases/latest/download/neurowatch-linux-arm64.tar.gz) .tar.gz | `tar xzf neurowatch*-linux-*.tar.gz && ./neurowatch/neurowatch` (needs the system's desktop and audio libraries) |

Double-clicking opens the package in your software centre (App Center on Ubuntu 24.04 and later, Software on Fedora), which asks for your password and installs the system libraries NeuroWatch needs as well; then open **NeuroWatch** from your applications. If double-clicking opens another program (for example on Ubuntu 22.04), use the terminal command instead, with the `./` so the required libraries are installed too. Downloads include the video components NeuroWatch needs; you don't have to install any yourself.

The Windows installer installs for your account and adds a Start Menu shortcut. Uninstalling stops the service and removes its startup entry; your configuration and recordings are preserved. Before removing a Linux package, run `neurowatch service stop` and `neurowatch service uninstall` from your own account.

**System requirements**
- **64-bit PC (x86_64):** Ubuntu 22.04 or newer, Debian 12 or newer, or a similarly recent distribution; or Windows 10/11.
- **64-bit ARM (arm64):** Ubuntu 24.04 or newer, Debian 13 or newer, 64-bit Raspberry Pi OS based on Debian 13 (Trixie; Bookworm is too old), or a similarly recent distribution. 32-bit systems are not supported.
- macOS is not supported.
- To decode video on your graphics card, install its driver (on Linux, the Intel/AMD video acceleration driver or the NVIDIA driver). Without it, video is decoded on the processor.
- The release checks cover package installation on Ubuntu 22.04/24.04,
  Debian 12/13, Fedora 43 and Arch Linux. ARM installation checks cover Ubuntu
  24.04, Debian 13 and Fedora 43. Arch Linux ARM, RHEL, openSUSE and Alpine are
  not supported. See [system requirements](docs/system-requirements.md) for the
  minimum version of each distribution.

**Checking a download (optional).** Each release includes a `SHA256SUMS` file. Put it next to your download and run `sha256sum --ignore-missing -c SHA256SUMS` (Linux).

---

## Getting started

1. **Open NeuroWatch** from your applications menu, or run `neurowatch`. The first time, it sets up its background service and tray icon, so recording carries on after you close the window and resumes after a restart.
2. **Add a camera:** click **＋ Add Camera**.
   - **Search Network** lists the cameras NeuroWatch finds. Pick one, enter its password, choose its streams, and add it.
   - Or, under **Advanced**, enter a name and the camera's stream address, for example `rtsp://admin:password@192.168.1.100:554/stream1`. **Test** checks it before you save.
3. **Turn on recording:** on the camera's **Recording** tab, or for every camera at once in **Settings → Recording**. That page is also where you choose the drive to record to.
4. **Watch recordings** in the **Playback** tab.
5. **Optional:** link to NeuroWatch Cloud from **≡ menu → Cloud Link…** to watch from a browser.

---

## Everyday use

### Keyboard and mouse

| Input | Action |
|---|---|
| `F11` | Full screen on/off |
| `Escape` | Leave single-camera view, or leave full screen |
| `Page Up` / `Page Down` | Previous / next page of cameras |
| `Home` / `End` | First / last page |
| `Ctrl+Shift+H` | Hide or show the panels around the video |
| `Ctrl+Shift+N` | Move a page of cameras to a new window |
| Double-click a camera | Watch it alone; click again to go back |
| `Ctrl` + scroll | Zoom into a camera's picture |
| Drag a camera | Swap it with another, or move it to an empty slot |
| Drag a camera's corner | Make it larger or smaller |
| Right-click a camera | Edit, remove, tile size, choose stream |

### The background service

NeuroWatch's recorder keeps running when the window is closed. Use the tray icon to stop or restart it, or these commands (Linux; on Windows, use the tray icon):

| Command | What it does |
|---|---|
| `neurowatch service status` | Is NeuroWatch running, and does it start at login or boot? |
| `neurowatch service stop` | Stop recording (until you open NeuroWatch or start it again) |
| `neurowatch service start` / `restart` | Start or restart it |
| `neurowatch service uninstall` | Don't start NeuroWatch at login any more (it keeps running for now) |
| `neurowatch --no-autostart` | Open NeuroWatch without setting it up to start at login |

**Exit** in the tray menu asks whether to keep recording (only the icon closes) or to stop recording too.

---

## Running without a screen

On a server or a small computer with no display:

```bash
neurowatch --headless
```

NeuroWatch starts as a background service that also starts when the computer boots, without anyone logging in. The terminal shows what it's doing: cameras coming online, recording starting, and the cloud connection in official downloads. Press **Ctrl+C** to stop watching; NeuroWatch keeps recording.

With NeuroWatch Cloud (official downloads):
- Link it to NeuroWatch Cloud straight away with `neurowatch --headless --link CODE --name "Head Office"`, using an enrollment code from the web portal (**Nodes → Create enrollment**, valid five minutes).
- Once linked, manage its cameras, recording and drive from the web portal (open the recorder, then **Settings**).
- From another terminal: `neurowatch cloud status`, `neurowatch cloud link CODE --name NAME`, `neurowatch cloud unlink --yes`.
- Opening the desktop app on the same computer switches NeuroWatch back to desktop mode.

---

## Troubleshooting

**No picture from any camera.** Check each camera with **Test** in its settings, and make sure this computer can reach it.

**One camera shows no picture.** Check its address and password with **Test** in the camera's settings. Make sure the camera is reachable from this computer. TCP (the default) works through most networks; UDP doesn't always.

**The tray icon doesn't appear (Ubuntu/GNOME).** GNOME needs the *AppIndicator* extension to show tray icons. Ubuntu includes it. On other GNOME desktops, install "AppIndicator and KStatusNotifierItem Support". NeuroWatch keeps recording either way.

**NeuroWatch doesn't start, or stops.** Run `neurowatch service status`. The service log is at `~/.local/state/neurowatch/logs/service.log` (Linux) or `%LOCALAPPDATA%\Neurowatch\logs\service.log` (Windows). On Linux with the system service, `journalctl --user -u neurowatch-backend` also shows it.

**A headless computer doesn't start NeuroWatch at boot.** Some systems don't let an account start its services at boot. Run `sudo loginctl enable-linger $USER` once.

**`Could not load the Qt platform plugin "xcb"`** (portable download). Run `sudo apt install libxcb-cursor0`. The `.deb`, `.rpm` and Arch packages install it for you.

---

## Privacy and responsible use

- Your cameras, recordings and settings stay on your computer. Nothing is sent anywhere unless you link NeuroWatch Cloud, and then only what you choose to watch from the browser.
- Camera addresses can contain passwords. NeuroWatch stores them in its settings file (`~/.config/neurowatch/config.json` on Linux), so keep your computer account secure and don't share that file.
- **Use NeuroWatch lawfully.** Video surveillance is regulated in many places. You are responsible for following the rules that apply to you, for example privacy and data-protection law, notices and signage, consent, and how long you keep footage. Don't record people where they expect privacy, and don't use NeuroWatch for anything unlawful. The NeuroWatch authors don't operate your installation and never receive your footage.

---

## Disclaimer

NeuroWatch is free, open-source software provided **as is, without any warranty**, and used at your own risk. To the extent the law allows, its authors and contributors are **not liable** for any loss or damage from using it, including missed, lost or corrupted recordings. It is not a guaranteed security, safety or alarm system: test that it works for you and keep backups of footage you need. The full disclaimer is in [`NOTICE`](NOTICE); the [licence](LICENSE) (sections 15 and 16) governs.

---

## Licensing

This is a summary, not legal advice; the licence texts govern.

- **The NeuroWatch app is open source** under the [GNU Affero General Public License v3.0 or later](LICENSE): live view, recording, playback, export and headless mode, with no camera limit, free for any use, including business use.
- **If you change NeuroWatch and share it**, or let others use your changed version over a network, you must publish your changes under the same licence. Using it unchanged, or changing it only for yourself, needs nothing.
- **NeuroWatch Cloud is a separate, proprietary service.** Its code is not in this repository and not covered by the AGPL. It is free for up to 16 cameras; more cameras need a paid plan. The official downloads include its client.
- **Trademark:** "NeuroWatch" and its logo are not licensed under the AGPL (see [`NOTICE`](NOTICE)). A modified version must use another name and must not be presented as the official NeuroWatch.

NeuroWatch builds on third-party software under its own licences, including GStreamer, Qt/PySide6 and many Rust and Python libraries. You are responsible for any codec or patent licensing that applies where you use it (for example H.264/H.265).

---

## For developers

**Overview.** NeuroWatch has two parts that run as separate programs on the same computer: a background recorder, written in Rust on GStreamer, that connects to the cameras, records and serves video; and the desktop app, written in Python with Qt (PySide6), that shows it. Both can be extended: official builds add NeuroWatch Cloud that way, and this repository builds the app without it.

Build and run from source on Ubuntu/Debian:

```bash
git clone https://github.com/ttheekw/Neurowatch.git
cd Neurowatch
./neurowatch.sh --bootstrap --system --yes   # install the build tools and libraries (uses sudo)
./neurowatch.sh                              # build and open NeuroWatch
```

More detail:
- [docs/system-requirements.md](docs/system-requirements.md): minimum OS version for each download
- [docs/releasing.md](docs/releasing.md): building and publishing releases (pushing a `v*` tag builds and publishes one)

**Tests.** Every merge to `main`, and every pull request, runs the full test suite on GitHub: Rust formatting, lints and tests (with at least 80% line coverage), and Python lint, type checks and tests. The **Tests** badge at the top shows the result for `main`; open a [run](https://github.com/ttheekw/Neurowatch/actions/workflows/ci.yml) to see its summary, a table of passed, failed and skipped tests per suite with the names of any failures, and the coverage report. To run the same checks locally, see [CONTRIBUTING.md](CONTRIBUTING.md).

**Contributing.** Bug reports, ideas and pull requests are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md). Contributors sign the [Contributor License Agreement](CLA.md) once, on their first pull request.
