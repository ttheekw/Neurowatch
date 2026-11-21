<p align="center">
  <img src="ui/neurowatch/resources/icons/neurowatch.svg" width="96" height="96" alt="NeuroWatch logo">
</p>

<h1 align="center">NeuroWatch</h1>

<p align="center">
  <b>Turn the computer you already have into a security camera recorder.</b><br>
  Watch every camera live, record around the clock to your own drive, and find any moment in seconds.<br>
  Free for up to 16 cameras; NeuroWatch Unlimited is now USD 0 (normally USD 50), with sign-up required to download. Open source. No account needed for local use.
</p>

<p align="center">
  <a href="https://neurowatch.app/downloads?version=latest&platform=windows&arch=x86_64&kind=installer"><img src="https://img.shields.io/badge/Download_for_Windows-10_%26_11,_64--bit-2563eb?style=for-the-badge" alt="Download NeuroWatch for Windows 10 and 11"></a>
  &nbsp;
  <a href="https://neurowatch.app/downloads?version=latest&platform=linux&arch=x86_64&kind=deb"><img src="https://img.shields.io/badge/Download_for_Ubuntu_%2F_Debian-.deb,_64--bit-E95420?style=for-the-badge" alt="Download NeuroWatch for Ubuntu and Debian"></a>
</p>

<p align="center">
  Also for
  <a href="https://neurowatch.app/downloads?version=latest&platform=linux&arch=x86_64&kind=rpm">Fedora (.rpm)</a> ·
  <a href="https://neurowatch.app/downloads?version=latest&platform=linux&arch=x86_64&kind=arch">Arch Linux</a> ·
  <a href="https://neurowatch.app/downloads?version=latest&platform=linux&arch=arm64&kind=deb">Linux ARM (.deb)</a> ·
  <a href="https://neurowatch.app/downloads?version=latest&platform=linux&arch=arm64&kind=rpm">Fedora ARM</a> ·
  portable <a href="https://neurowatch.app/downloads?version=latest&platform=windows&arch=x86_64&kind=portable">Windows .zip</a>,
  <a href="https://neurowatch.app/downloads?version=latest&platform=linux&arch=x86_64&kind=portable">Linux .tar.gz</a>,
  <a href="https://neurowatch.app/downloads?version=latest&platform=linux&arch=arm64&kind=portable">ARM .tar.gz</a><br>
  <a href="https://neurowatch.app">Website: neurowatch.app</a> ·
  <a href="https://github.com/ttheekw/Neurowatch/releases/latest">Release notes and checksums</a> ·
  <a href="#installing">Installation help</a> ·
  <a href="docs/system-requirements.md">System requirements</a>
</p>

<p align="center">
  <a href="https://github.com/ttheekw/Neurowatch/releases/latest"><img src="https://img.shields.io/github/v/release/ttheekw/Neurowatch?include_prereleases&label=latest" alt="Latest version"></a>
  <a href="https://github.com/ttheekw/Neurowatch/actions/workflows/ci.yml?query=branch%3Amain"><img src="https://github.com/ttheekw/Neurowatch/actions/workflows/ci.yml/badge.svg?branch=main" alt="Tests"></a>
  <a href="#installing"><img src="https://img.shields.io/badge/platform-Linux%20%7C%20Windows-informational" alt="Linux and Windows"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="License: Apache-2.0"></a>
  <a href="https://neurowatch.app"><img src="https://img.shields.io/badge/website-neurowatch.app-2563eb" alt="Website: neurowatch.app"></a>
</p>

### Why NeuroWatch

- **Record to your own drive.** Cloud access and external alert notifications are optional; local recording works without a cloud account.
- **Free for up to 16 cameras.** NeuroWatch Unlimited removes the software camera limit; capacity depends on your computer. Both editions are open source and can be used at home or in a business.
- **Connect your existing cameras.** Add RTSP streams or use **Search Network** to discover ONVIF cameras. Compatibility depends on the camera's streams and ONVIF support.
- **Keep recording in the background.** Recording carries on after you close the window, can start automatically, and reconnects cameras when they drop.
- **Watch from anywhere, if you want to.** Optional NeuroWatch Cloud access shows your cameras in a browser. Cloud plans are separate from the desktop editions.
- **Light by design, in Rust.** Recording, browser viewing and clip export never decode video, every reader of a camera stream shares one connection and one buffer, and a picture is decoded at most once however many walls and plugins use it. [How it works](#how-it-works).

Comparing recorders? See [NeuroWatch compared with Frigate, Shinobi, Bluecherry, ZoneMinder, Agent DVR, Viseron, Scrypted and Blue Iris](docs/comparison.md), including when each one is the better choice.

> **Beta:** the release workflow checks installation and generated video on Windows, Ubuntu 22.04 and 24.04, Debian 12 and 13, Fedora 43 and Arch Linux before publishing. These checks do not qualify physical cameras, GPUs or displays. Found a problem? [Tell us](https://github.com/ttheekw/Neurowatch/issues), it makes the next release better.

NeuroWatch is **free and open source** ([Apache-2.0](LICENSE)), including its NeuroWatch Cloud client. The optional NeuroWatch Cloud, for watching from a browser, is an online service (see [Licensing](#licensing)).

## Contents

- [Features](#features)
- [How it works](#how-it-works)
- [Installing](#installing)
- [Getting started](#getting-started)
- [Everyday use](#everyday-use)
- [Running without a screen](#running-without-a-screen)
- [Running in Docker](#running-in-docker)
- [Troubleshooting](#troubleshooting)
- [Privacy and responsible use](#privacy-and-responsible-use)
- [Disclaimer](#disclaimer)
- [Licensing](#licensing)
- [For developers](#for-developers)

---

## Features

### Live video wall
- Watch all your cameras on one screen, in a ready-made or custom grid (2×2, 3×3, 4×4, and so on).
- More cameras than fit? They continue on further pages. Flip through them with the arrows or keyboard, or let the wall **auto-scroll**. NeuroWatch keeps previews ready between page changes.
- **Arrange the wall with the mouse:** drag a camera onto another to swap them, drag it to an empty slot to move it, or drag its corner to make it larger (2×2, 3×2, …). Hold a camera at the edge of the wall to carry it to another page.
- **Double-click** any camera to watch it alone, full size. Hold **Ctrl** and scroll to zoom into a picture.
- **Full screen** with one key, and **Hide panels** to leave nothing but the video. The status readings can stay on screen over the video, with an adjustable see-through background.
- **More windows and monitors:** move a page of cameras into its own window, put it on another monitor, and give it its own grid and layout. A window can hold several pages and cycle through them. Your windows come back where you left them the next time you start NeuroWatch.
- Uses your graphics card to decode video where possible (Intel/AMD and NVIDIA), and shows smaller grid streams in multi-camera views.

### Cameras
- **Search Network** finds ONVIF cameras on your network, shows their model and whether they're ready, need a password, or are already added. Then add them in a few clicks.
- Or add one by its address on the **Camera** tab, two ways: **Automatic (ONVIF)** asks the camera for its streams (enter its address, username and password, then **Load Streams**, and pick the main and grid stream from the lists), or **Stream URLs** for any camera by its stream address: RTSP/RTSPS, RTMP, MJPEG over HTTP(S), USB cameras, or a video file.
- **Edit Camera** opens on the same tab with the camera's saved streams to choose from. Its password is kept unless you type a new one.
- **Several streams per camera:** NeuroWatch records the best-quality stream and shows a lighter one in the grid. You can pick another stream for any tile from its right-click menu.
- Test a camera before saving it, and see each camera's live status, codec and resolution on its tile.
- **Audio:** cameras that send supported sound (AAC, G.711, Opus or PCM) are recorded with it. Tiles of cameras with audio show a speaker at the top right: click it, or right-click → **Listen**, to hear that camera live. One camera at a time; the sound stops when the camera leaves the screen.
- **PTZ controls:** right-click a live camera → **PTZ Controls…** to pan, tilt and zoom a compatible ONVIF camera. Hold a direction or zoom button to move and release to stop; use Home and camera presets where supported.
- **Two-way audio:** right-click a live camera → **Talk…**, then hold **Hold to Talk** or the Space bar to speak through its speaker. Requires a microphone and a camera with a supported ONVIF G.711 audio backchannel; a test tone is available to check the speaker.
- NeuroWatch won't add the same camera twice, and reconnects by itself when a camera drops out.

### Motion detection

Motion detection is a plugin, **[NeuroWatch Motion](https://github.com/ttheekw/Neurowatch-Motion)**, installed separately: [download it](https://github.com/ttheekw/Neurowatch-Motion#download) for the computer NeuroWatch runs on (or another one), then add it once in **Plugins**.

- **How it works:** NeuroWatch sends the plugin each chosen camera's video as the camera sent it (H.264 or H.265), and the plugin sends back motion events and drawings. The events are kept like any other (the **Events** tab, alerts, recording only when something happens); the drawings appear as boxes on the live wall, on the picture they describe. The plugin runs in its own process, so recording goes on even if it stops; the status bar and tray then show **Plugin offline**. Analysis runs whether or not the window is open.
- **Who decodes:** by default (**Automatic**) the plugin reuses pictures NeuroWatch already decodes, for a video wall or for a stream you set to **Decode on this recorder all the time** (Edit Camera → **Motion**), at the rate it asks for, and decodes the camera itself only when there are none. **Always decode in the plugin** (the camera's plugin **Settings**) makes it decode by itself. Diagnostics shows which is happening.
- **Its dashboard:** in NeuroWatch Cloud, a recorder's **Plugin dashboards** and a camera's **Plugin** tab open the motion dashboard: each camera's motion today, counts for the last hour, day and week, an hourly chart and recent motion to play.

  <img src="docs/images/portal-motion-dashboard.webp" width="640" alt="The NeuroWatch Motion dashboard in the web portal: two cameras with today's motion, one camera's counts for the last hour, today and 7 days, an hourly chart and recent motion events with Play buttons. The camera pictures are drawn mock scenes.">

- **Set it up:** install NeuroWatch Motion, open **Plugins** (administrators only), **Add Plugin… → Local socket** (it is found on the same computer), check the key fingerprint and approve it. Then choose NeuroWatch Motion, **Bind Camera…**, then set sensitivity, minimum duration, cooldown, low CPU mode, the video decoder and watched/ignored zones in the camera's **Settings** (on the desktop or in the portal; every setting explains itself). **Live** shows its drawings and **Output** lists its events. Motion settings from earlier versions are kept: once Motion is installed and approved, they apply again.
- **On another computer:** to take the work off the recorder, [download NeuroWatch Motion](https://github.com/ttheekw/Neurowatch-Motion#download) for that computer (Windows, or Linux on a 64-bit PC or ARM), start it with `nw-motion --listen quic:0.0.0.0:7690`, then in **Plugins → Add Plugin → URL** enter `quic://that-computer:7690`, check that the key fingerprint matches the one the plugin prints, and approve it. Its [instructions](https://github.com/ttheekw/Neurowatch-Motion#connect-it-to-neurowatch) also cover [access codes](https://github.com/ttheekw/Neurowatch-Motion#access-codes) that limit which recorders may use it (enter the code under **Vendor Credential…**).
- **Your camera's own detection** is separate and needs no plugin: see Camera motion events below.

### Events

- **Motion detection:** by the NeuroWatch Motion plugin; see [Motion detection](#motion-detection).
- **Camera motion events:** compatible ONVIF cameras can report their own motion detection. Turn on **Camera's own detection** in a camera's **Motion** tab (Edit Camera), and NeuroWatch records each motion event without continuously decoding video for detection. Event pictures use a short, one-frame snapshot. Supported camera analytics also report intrusion, line crossing, tampering and alarm inputs. The Motion tab shows whether the camera is sending its events. Set where and how sensitively the camera looks for motion in the camera's own web settings.
- Cameras send their events straight to NeuroWatch when they can reach your computer (TCP port 7681; allow it in your firewall if one blocks cameras). Otherwise NeuroWatch asks each camera for its events every few seconds, automatically; the Motion tab says which.
- **See events in the app:** the **Events** tab lists activity and alerts as they happen. Search your event history by camera, event or details; filter by camera, event type, ongoing/finished status and date range. Click a column heading to sort the loaded rows, and **Show Older** to go further back. Double-click an event to watch that moment in Playback. The tab counts new events while you're elsewhere; a camera's tile shows an orange **MOTION** badge while the camera sees motion.
- **Motion alerts on tiles:** an orange bell icon at the top right stays after motion ends, including when you change pages or reopen the app. Click the icon to clear the events seen so far; new motion brings it back. Clearing the alert keeps the event history. In **Playback**, orange marks on each camera's timeline show motion, intrusion and line crossing: hover for the event's time and duration, or click there to seek through the recordings.
- **Problems are events too:** a camera going offline (and coming back), a recording that keeps failing, and a recording drive that is almost full are noted with their times, also when NeuroWatch runs without a screen.
- See them from a terminal with `neurowatch events` (the latest 20), `neurowatch events --follow` (and new ones as they happen) or `--camera "Front Door"`. Event history is kept for 30 days by default; kept bookmarks are retained until you release them.

### Alert notifications

- Open **≡ → Alert Notifications…** to add channels for **This computer**, **ntfy push**, **Telegram** or a **Webhook**. External channels can include an event picture and work without linking NeuroWatch Cloud.
- Choose which events and cameras send alerts, set active weekly hours and a cooldown, send a test, and check delivery history.
- Desktop notifications need the desktop app running. ntfy, Telegram and webhook deliveries run in the recorder, including headless mode. Email delivery is not implemented.

### Home Assistant and MQTT

- Open **Settings → Home Assistant**, enter your MQTT broker (for Home Assistant, the Mosquitto broker add-on's address, user name and password), press **Test Connection**, then **Save**.
- Each camera then appears in Home Assistant by itself: **Motion**, **Person**, **Vehicle** and **Animal** sensors, **Online**, and a **Last event** picture. The recorder appears too, with a **Problem** sensor (recording failing, a drive full or missing, a plugin or Cloud Backup failing). Use them in automations like any other sensor.
- Not using Home Assistant? Any MQTT system can read the same topics: `neurowatch/<camera>/person` and the others are `ON` or `OFF`, `neurowatch/events` carries every event as JSON, and `neurowatch/status` says whether the recorder is online. Give each recorder on the same broker its own topic prefix.

### Local accounts and permissions

- Open **≡ → Users & Permissions…** to create local accounts and enable sign-in. Accounts are optional and are off by default; they are separate from your NeuroWatch Cloud account.
- **Viewers** watch live video and recordings and review events. **Operators** can also control PTZ, talk, add bookmarks, export clips and start or stop recording. **Admins** manage cameras, settings, alerts and accounts.
- Limit viewers and operators to selected cameras. Local accounts control access through the app; keep the computer account and configuration files secure too.

### Recording and playback
- Record all cameras or only the ones you choose, around the clock, to your computer's drive.
- **Weekly recording schedules:** right-click a camera tile, choose **Edit Camera → Recording**, then choose Continuous, Event retention or Off, then drag across the weekly grid to paint hours or 15-minute intervals. Use **Edit exact time windows** for minute precision, including overnight. Faint cells use the default; erase restores it and **Undo paint** reverses edits. Schedules follow the recorder’s local clock through daylight-saving changes. Manual Start/Stop overrides the schedule until the recorder restarts. The hosted portal offers the same policy controls.
- **Copy recording policies:** in **Edit Camera → Recording → Copy to cameras…**, select cameras and confirm. This saves the shown modes, schedule, padding and retention settings to those cameras immediately. Each camera keeps its recording enable switch, storage path, quota and motion settings. The source camera still needs **Save**.
- **Event retention** records continuously, then trims footage without events; it saves space, not disk writes. Choose footage before/after events and how long to keep non-event footage. Cleanup waits through a safety grace and pauses if recording or detection is unhealthy; older continuous footage is not enrolled. Quota and drive limits still apply to event footage.
- **Choose Drive…** lists your drives with their free space and records to the one you pick. The new location takes effect immediately.
- **Disk cleanup:** when the recording drive gets fuller than a limit you choose (90% by default), NeuroWatch deletes eligible older recordings to make room. Configure it in **Settings → Recording**; each camera can also have its own **Storage Quota**. Cleanup leaves your other files and footage protected by kept bookmarks alone. It cannot guarantee free space if other files or kept footage fill the drive.
- **Missing-drive protection:** if a recording drive is disconnected or a network share is not mounted, NeuroWatch pauses recording there instead of quietly filling your computer's own disk. The cameras show recording **failed**, a **Recording storage missing** event (and alert, if you set one up) says which folder, and recording carries on by itself once the drive is back. NeuroWatch recognises its folders by a small `.neurowatch-storage` file; please leave it there. A missing marker also blocks recording even if the folder exists.
- **Archive to a NAS:** **Settings → Archive** moves recordings older than a few days (7 by default) to another folder, such as a network share, and Playback and Export still find them there. Each recording is checked against the original before it is removed from the recording drive. You can limit the archive's size, the hours it runs (for example only at night) and its speed. This lets you keep new recordings on the computer while moving older footage to the NAS.
- **Playback:** a timeline with several cameras side by side, with play/pause, seek and speed controls. Turn on **Events only** to skip quiet time and pause at the last recorded event; turn it off for normal review. The **Events** sidebar offers searchable lists or paged **Thumbnails** of saved images, with zone names, peak score and changed-area details when available.
- **Review events while playing recordings:** open **Events** on the desktop timeline or **Review events** in the portal. Filter the loaded range by event type, acknowledgement or text (and camera on desktop), see a saved event picture, and review its recording with a short lead-in. Coloured timeline marks distinguish motion, bookmarks and camera alerts; hover shows the type and full time.
- **Bookmarks:** right-click a live camera → **Add Bookmark…**, or use **Add Bookmark** in Playback, to mark a moment or time span with a note. **Keep this footage** protects the matching recordings, with 30 seconds of padding on each side, from quota, disk and event-retention cleanup until you unmark it. Kept footage still uses disk space.
- **Export an event:** choose **Export event…** on desktop to prepare a clip with that camera's pre/post padding. In portal recording review, select an event and choose **Prepare event clip** with the padding you want (where export is available). Review the yellow handles, then export; missing footage and clip limits are explained.
- **Export Video:** **File → Export Video…** saves a clip as an MP4 file you can share. Load a camera's recordings, drag the yellow handles on the timeline to the part you want (a preview plays it), press **Export…** and choose where to save. The video is copied as recorded, so it is quick and keeps the camera's quality. A clip may start a moment before the time you chose, and gaps where nothing was recorded are left out.
- **Export from the web portal:** a camera's **Export** tab in NeuroWatch Cloud lets you play its recordings and drag the start and end of a clip on the timeline; the recorder makes the MP4 and uploads it for download through the portal.
- **Playback with sound:** recordings with audio play it back. The first camera with audio is heard; click another tile's speaker to switch, or the same one to mute. Sound keeps its normal pitch from 0.5× to 2× speed and is muted at faster or slower speeds.
- **Sound in the web portal:** live view and recordings watched in the portal carry the camera's sound too. It starts muted; click **Sound off** above the video to hear it. Recordings play their sound at 1× speed.

### Always recording, in the background
- NeuroWatch keeps recording after you close the window. By default it starts by itself when you log in. Configure **Settings → Startup** to change this or, on Linux, enable recording from boot before sign-in.
- A **tray icon** shows NeuroWatch's state at a glance. Hover over it for a short summary, or open its menu to open NeuroWatch or start, stop or restart recording.
  - The **bottom dot** is green when everything is fine, amber when something needs attention, red when NeuroWatch is stopped or every camera is offline, and grey while starting or stopping.
  - A small **red dot at the top** appears when there's a problem: cameras offline, a recording that failed, the cloud unreachable, or the computer struggling (CPU, memory or graphics). The menu names each problem.
- **Headless mode** for a computer without a screen: NeuroWatch can run without the desktop window. On Linux with a systemd user service and boot startup enabled, recording starts even if nobody logs in. See [Running without a screen](#running-without-a-screen).

### NeuroWatch Cloud (optional)
NeuroWatch Cloud is an optional online service for browser access. Its plans are separate from the Free and Unlimited desktop editions; see [neurowatch.app](https://neurowatch.app) for current plans. Cloud support is built into both editions and source builds.
- Link NeuroWatch to your NeuroWatch Cloud account with a one-time code (**Settings → Cloud**, or click **Cloud** in the status bar).
- Then watch live video and recordings from a web browser, with camera thumbnails.
- Manage it from the web portal, with or without a screen: add and edit cameras, turn recording on or off, and choose the recording drive. The desktop app shows changes made there at once. You can also see the same diagnostics as in the desktop app.
- Camera addresses and passwords you enter in the portal are encrypted between your browser and your recorder, so the cloud only passes them along and can't read them.
- Linking sends camera names and status, recording availability and requested thumbnails to the cloud. Live viewing, playback, diagnostics and clip exports send the data needed for those features; a clip export uploads the requested MP4.
- Without linking, NeuroWatch does not send recorder data to NeuroWatch Cloud. External alerts, if configured, send data to the destinations you choose independently.

### Status and diagnostics
- A status bar shows disk space, CPU, memory, network speed, whether video is decoded by the graphics card, and the recorder and cloud connections.
- Open **≡ → Diagnostics** to see how hard the computer is working, per-camera frame rates and dropped frames, and graphics card usage where the driver reports it.
- Each camera stream is drawn as a tree: its codec and incoming rate, its shared frame buffer, then everything reading it (Recording, Video wall, Web, plugins), with a box wherever a picture is decoded. The same view is in the portal. See [How it works](#how-it-works).
- Light and dark themes.

### Coming later
Email alerts, a local web interface, a multi-site desktop dashboard and native mobile apps remain planned. Object detection (people, vehicles and animals) comes as a separate plugin, NeuroWatch Detect, which is not yet included in the downloads. It is a separate program on purpose: recording must never fail, so detection runs beside it, not inside it. If the detector crashes, falls behind or is being updated, recording carries on, and it can run on another computer.

---

## How it works

For the technically curious: NeuroWatch is built so that the expensive part of a video recorder, decoding video, happens as rarely as possible.

```mermaid
flowchart LR
  cam([Camera]) -->|"RTSP · H.264 / H.265"| conn["One connection per stream<br/>parsed, never decoded"]
  conn --> buf[("Frame buffer<br/>latest GOP, compressed")]
  buf -->|reference| rec["Recording<br/>MKV segments, as sent"]
  buf -->|reference| web["Browser<br/>WebRTC, forwarded as-is"]
  buf -->|reference| plug["Plugin<br/>local socket · TLS · QUIC"]
  buf -->|reference| dec{{"Decoder<br/>only for a wall on screen<br/>or a stream you choose"}}
  dec --> wall["Desktop wall"]
  dec -.->|"shared pictures"| plug
```

- **One connection, one buffer.** Each camera stream is opened once. Its compressed units go into a buffer that keeps the latest group of pictures (from the last keyframe), and recording, the desktop wall, browsers and plugins all read references to the same units, not copies. A new viewer or plugin starts from that keyframe straight away.
- **Bounded, independent readers.** Every reader has its own bounded queue that skips ahead to the next keyframe or drops its oldest units when it falls behind. A slow browser or a busy plugin never holds up the camera or recording.
- **Recording and viewing without decoding.** Video goes to disk as the camera compressed it (segmented Matroska), browsers receive it forwarded unchanged over WebRTC, and exports copy it into MP4. None of these decodes or re-encodes video.
- **Decode once, share the pictures.** The desktop decodes only the cameras on screen, on the graphics card where it can (VA-API, NVDEC, Direct3D 11/12, Quick Sync, V4L2), with smaller substreams in grids. Plugins on the same computer reuse those pictures, or the pictures of a stream you choose to decode all the time, instead of decoding the camera again; a wall showing that stream borrows them too. Pictures are copied into each plugin's private read-only buffer, and graphics memory never leaves its pool.
- **Pictures never reopen a camera.** Event pictures and portal thumbnails come from a recorded keyframe or a stream that is already open; only when there is neither does NeuroWatch ask the camera.
- **Streams close when unused.** A stream nobody needs lingers a few seconds, then disconnects, and opens again on demand.

**Why Rust.** A recorder runs for months, so the engine (`nw-backend`) is compiled Rust on the Tokio async runtime: no garbage collector to pause it while video arrives, memory safety checked at build time for a program that parses video from devices on your network, and ownership rules that let one compressed unit be shared by many threads without copies or data races. GStreamer parses the media; the code in its streaming callbacks only wraps each unit and hands it on. The desktop window (Qt, Python) and every analytics plugin run as separate processes, so closing the window or a crashing plugin never stops recording, and a supervisor restarts the recorder if it ever exits unexpectedly.

**See it for yourself.** Diagnostics draws this path live for every camera, on the desktop and in the portal. Below, the Car park's sub stream is decoded all the time and shared by its wall and the motion plugin; the Front entrance's plugin reuses the wall's pictures; the Loading bay's plugin decodes by itself.

<img src="docs/images/desktop-diagnostics-flow.webp" alt="Desktop Diagnostics: three camera cards, each stream with its codec and incoming rate, its frame buffer, and its consumers; decoder boxes for an always-on stream, a video wall and a plugin that decodes by itself, and dashed boxes where the motion plugin reuses decoded pictures.">

<img src="docs/images/portal-diagnostics-flow.webp" alt="The same three cameras in the NeuroWatch Cloud portal's Diagnostics page, drawn as trees in the portal's light theme.">

There are no benchmark figures here on purpose: the load depends on your cameras, codecs, graphics hardware and how many are on screen. Diagnostics shows the real numbers for your own recorder. The full design is in [docs/architecture.md](docs/architecture.md), and [docs/analytics-plugins.md](docs/analytics-plugins.md) describes the plugin protocol and SDKs.

---

## Installing

Each link below opens the [downloads page](https://neurowatch.app/downloads) at the newest version. It offers two editions of every download:

- **NeuroWatch** (free): everything described here, for up to 16 cameras.
- **NeuroWatch Unlimited** (~~USD 50~~ USD 0): the same app with **no software camera limit**; capacity depends on your hardware. Includes a **support response within 24 hours** at [hello@neurowatch.app](mailto:hello@neurowatch.app). Create a NeuroWatch account and sign in on the downloads page to download it. No payment is required. Install it over the free version: your cameras and recordings stay.

The desktop purchase and optional NeuroWatch Cloud plans are separate.

If an existing configuration has more than 16 cameras, the free version runs the first 16 (in wall order); the others are kept as they are and can run again when you remove cameras or install Unlimited. Adding another camera at the free limit shows an upgrade prompt. Older versions, release notes and checksums are on the [Releases page](https://github.com/ttheekw/Neurowatch/releases). NeuroWatch is in beta: it works, but hasn't been tested on many computers yet, so please [report problems](https://github.com/ttheekw/Neurowatch/issues).

On Ubuntu, Debian, Linux Mint and Fedora you can **double-click** the downloaded package and choose **Install**; the terminal command next to it does the same, run in the folder you downloaded it to (usually `~/Downloads`):

| System | Download | Install |
|---|---|---|
| Windows 10/11 (64-bit) | [Installer (.exe)](https://neurowatch.app/downloads?version=latest&platform=windows&arch=x86_64&kind=installer) | Run the installer, then open NeuroWatch from Start. Windows may warn that the installer is unrecognised: choose **More info → Run anyway** ([why](#windows-protected-your-pc)). |
| Windows 10/11 (portable) | [.zip](https://neurowatch.app/downloads?version=latest&platform=windows&arch=x86_64&kind=portable) | Extract it, then run `neurowatch.cmd` |
| Ubuntu, Debian, Linux Mint (64-bit PC) | [.deb](https://neurowatch.app/downloads?version=latest&platform=linux&arch=x86_64&kind=deb) | Double-click → **Install**, or `sudo apt install ./neurowatch*_amd64.deb` |
| Ubuntu 24.04+, Debian 13+ (ARM64) | [.deb](https://neurowatch.app/downloads?version=latest&platform=linux&arch=arm64&kind=deb) | Double-click → **Install**, or `sudo apt install ./neurowatch*_arm64.deb` |
| Fedora (64-bit PC) | [.rpm](https://neurowatch.app/downloads?version=latest&platform=linux&arch=x86_64&kind=rpm) | Double-click → **Install**, or `sudo dnf install ./neurowatch*.x86_64.rpm` |
| Fedora (ARM) | [.rpm](https://neurowatch.app/downloads?version=latest&platform=linux&arch=arm64&kind=rpm) | Double-click → **Install**, or `sudo dnf install ./neurowatch*.aarch64.rpm` |
| Arch Linux (64-bit PC) | [.pkg.tar.zst](https://neurowatch.app/downloads?version=latest&platform=linux&arch=x86_64&kind=arch) | `sudo pacman -U ./neurowatch*-x86_64.pkg.tar.zst` |
| Other Linux (portable) | [x86_64](https://neurowatch.app/downloads?version=latest&platform=linux&arch=x86_64&kind=portable) or [ARM](https://neurowatch.app/downloads?version=latest&platform=linux&arch=arm64&kind=portable) .tar.gz | `tar xzf neurowatch*-linux-*.tar.gz && ./neurowatch/neurowatch` (needs the system's desktop and audio libraries) |

Double-clicking opens the package in your software centre (App Center on Ubuntu 24.04 and later, Software on Fedora), which asks for your password and installs the system libraries NeuroWatch needs as well; then open **NeuroWatch** from your applications. If double-clicking opens another program (for example on Ubuntu 22.04), use the terminal command instead, with the `./` so the required libraries are installed too. Downloads include the video components NeuroWatch needs; you don't have to install any yourself. Motion detection is a separate plugin: download [NeuroWatch Motion](https://github.com/ttheekw/Neurowatch-Motion#download) and add it in **Plugins** (see [Motion detection](#motion-detection)).

On Linux the packages also add the account that runs the installation (through `sudo` or `pkexec`) to the `render` and `video` groups where those groups exist, so recording can access the graphics card, hardware video decoder and USB cameras before sign-in. Sign out and in again, or restart, for group changes to take effect. Other accounts, and the portable download, add themselves to the device groups their distribution uses; if both exist, run `sudo usermod -aG render,video "$USER"`. Windows starts NeuroWatch when you sign in.

The Windows installer installs for your account and adds a Start Menu shortcut. Uninstalling stops the service and removes its startup entry; your configuration and recordings are preserved. Before removing a Linux package, run `neurowatch service stop` and `neurowatch service uninstall` from your own account.

<a id="windows-protected-your-pc"></a>**"Windows protected your PC" (SmartScreen).** Windows shows this blue warning, with "Unknown publisher", for programs that are not signed with a paid code-signing certificate, until many people have downloaded them. NeuroWatch's Windows downloads are not signed that way yet: such a certificate costs hundreds of US dollars a year, which this free beta does not pay for yet. The warning means only that Windows does not know who made the file; it does not mean anything was found in it. To install, choose **More info**, then **Run anyway**. Some antivirus programs warn about unsigned programs too; download only from [neurowatch.app/downloads](https://neurowatch.app/downloads) or the [Releases page](https://github.com/ttheekw/Neurowatch/releases). To make sure the file is the one we published, compare it with `SHA256SUMS` (see [Checking a download](#checking-a-download)); on Windows, `Get-FileHash .\neurowatch-*-setup.exe` in PowerShell prints the value to compare. If Windows blocks `neurowatch.cmd` from the portable version, right-click the downloaded `.zip` → **Properties** → tick **Unblock** → **OK**, then extract it again.

**System requirements**
- **64-bit PC (x86_64):** Ubuntu 22.04 or newer, Debian 12 or newer, or a similarly recent distribution; or Windows 10/11.
- **64-bit ARM (arm64):** Ubuntu 24.04 or newer, Debian 13 or newer, 64-bit Raspberry Pi OS based on Debian 13 (Trixie; Bookworm is too old), or a similarly recent distribution. 32-bit systems are not supported.
- **Raspberry Pi:** the ARM64 package has not been tested on Pi hardware; decoder support and performance depend on the model. See [Raspberry Pi requirements](docs/system-requirements.md#raspberry-pi).
- macOS is not supported.
- To decode video on your graphics card, install its driver (on Linux, the Intel/AMD video acceleration driver or the NVIDIA driver). Without it, video is decoded on the processor.
- The release checks cover package installation on Ubuntu 22.04/24.04,
  Debian 12/13, Fedora 43 and Arch Linux. ARM installation checks cover Ubuntu
  24.04, Debian 13 and Fedora 43. Arch Linux ARM, RHEL, openSUSE and Alpine are
  not supported. See [system requirements](docs/system-requirements.md) for the
  minimum version of each distribution.

<a id="checking-a-download"></a>**Checking a download (optional).** Each release includes a `SHA256SUMS` file. Put it next to your download and run `sha256sum --ignore-missing -c SHA256SUMS` (Linux). `SHA256SUMS` is signed without a certificate of ours (Sigstore, from the release workflow's GitHub identity): download `SHA256SUMS.sigstore.json` too and check it with [cosign](https://docs.sigstore.dev/cosign/system_config/installation/):

```
cosign verify-blob --bundle SHA256SUMS.sigstore.json \
  --certificate-identity-regexp '^https://github\.com/<owner>/<repo>/\.github/workflows/release\.yml@refs/tags/v' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com SHA256SUMS
```

Replace `<owner>/<repo>` with this repository's name on GitHub. Windows installers are not Authenticode-signed yet, so Windows may show an "unknown publisher" warning ([why, and what to do](#windows-protected-your-pc)).

---

## Getting started

1. **Open NeuroWatch** from your applications menu, or run `neurowatch`. The first time, it sets up its background service and tray icon, so recording carries on after you close the window and resumes after a restart.
2. **Add a camera:** click **＋ Add Camera**.
   - **Search Network** lists the cameras NeuroWatch finds. Pick one, enter its password, choose its streams, and add it.
   - Or open the **Camera** tab and choose **Automatic (ONVIF)** to load streams by address, or **Stream URLs** to enter one yourself, for example `rtsp://192.168.1.100:554/stream1`. Enter the camera's username and password in their fields. **Test Connection** checks the selected streams before you save.
3. **Turn on recording:** on the camera's **Recording** tab, or for every camera at once in **Settings → Recording**. That page is also where you choose the drive to record to.
4. **Watch recordings** in the **Playback** tab.
5. **Optional:** link to NeuroWatch Cloud from **Settings → Cloud** to watch from a browser.

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
| Double-click a camera | Watch it alone; double-click again to go back |
| `Ctrl` + scroll | Zoom into a camera's picture |
| Drag a camera | Swap it with another, or move it to an empty slot |
| Drag a camera's corner | Make it larger or smaller |
| Right-click a camera | Edit, remove, choose stream or tile size, listen, PTZ, bookmark or talk (depending on the camera and your permissions) |

### The background service

NeuroWatch's recorder keeps running when the window is closed. **Settings → Startup** chooses what happens when you sign in: start NeuroWatch or not, stay in the background (tray icon) or open the window, record from boot before anyone signs in (Linux), and open the window in fullscreen. Use the tray icon to stop or restart it, or these commands (Linux; on Windows, use the tray icon):

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

On a Linux server or a small Linux computer with no display, you can set everything up over SSH.

**1. Install it.** Download the package for the computer from the downloads page ([Ubuntu/Debian 64-bit PC](https://neurowatch.app/downloads?version=latest&platform=linux&arch=x86_64&kind=deb), [Ubuntu/Debian/Raspberry Pi OS ARM64](https://neurowatch.app/downloads?version=latest&platform=linux&arch=arm64&kind=deb), [Fedora](https://neurowatch.app/downloads?version=latest&platform=linux&arch=x86_64&kind=rpm), [Fedora ARM](https://neurowatch.app/downloads?version=latest&platform=linux&arch=arm64&kind=rpm)), copy it across if you downloaded it on another computer (for example with `scp`), and install it:

```bash
sudo apt install ./neurowatch_*.deb      # Ubuntu, Debian, Raspberry Pi OS
sudo dnf install ./neurowatch*.rpm       # Fedora
```

NeuroWatch Unlimited is installed the same way, from the same page once you are signed in.

**2. Link it to NeuroWatch Cloud and start it.** Create a one-time enrollment code on the **Recorders** page of [NeuroWatch Cloud](https://neurowatch.app) (valid five minutes), then, as the account that should own the recordings (not root):

```bash
neurowatch --headless --link CODE --name "Head Office"
```

**3. Add cameras from your browser:** open the recorder in NeuroWatch Cloud, then **Settings**, to search for cameras, turn on recording and choose the drive.

Without NeuroWatch Cloud, start it with just:

```bash
neurowatch --headless
```

NeuroWatch starts as a background service. On Linux with systemd, it enables startup at boot using user lingering; if permission is needed, follow the command shown or see Troubleshooting below. The terminal shows what it's doing: cameras coming online, recording starting, events such as motion or a camera going offline, and the cloud connection. Press **Ctrl+C** to stop watching; NeuroWatch keeps recording. `neurowatch events` lists what happened while you weren't watching.

With NeuroWatch Cloud:
- Once linked, manage its cameras, recording and drive from the web portal (open the recorder, then **Settings**).
- From another terminal: `neurowatch cloud status`, `neurowatch cloud link CODE --name NAME`, `neurowatch cloud unlink --yes`.
- Opening the desktop app on the same computer switches NeuroWatch back to desktop mode.

---

## Running in Docker

NeuroWatch is also published as two container images for Linux (64-bit PC and ARM64):

- `ghcr.io/ttheekw/neurowatch:latest`: the **headless** recorder, without a window. Use it on a server and watch through NeuroWatch Cloud.
- `ghcr.io/ttheekw/neurowatch:gui`: the full app, with its **window** on your screen.

Both keep their settings and recordings in two folders you create first:

```bash
mkdir -p ~/neurowatch/data ~/neurowatch/recordings
```

**Headless recorder** (starts again with Docker until you stop it):

```bash
docker run -d --name neurowatch --restart unless-stopped --network host \
  --user "$(id -u):$(id -g)" --stop-timeout 30 \
  --cap-drop ALL --security-opt no-new-privileges \
  -v ~/neurowatch/data:/data -v ~/neurowatch/recordings:/recordings \
  ghcr.io/ttheekw/neurowatch:latest
```

Link it to NeuroWatch Cloud with an enrollment code from the web portal:

```bash
docker exec --user "$(id -u):$(id -g)" neurowatch neurowatch-docker cloud link CODE --name "Head Office"
```

**With the window** (run from your desktop session; on Wayland desktops this uses XWayland):

```bash
xhost +SI:localuser:$(id -un)
docker run -d --name neurowatch-gui --network host \
  --user "$(id -u):$(id -g)" --stop-timeout 30 --shm-size 1g \
  --cap-drop ALL --security-opt no-new-privileges \
  -e DISPLAY -v /tmp/.X11-unix:/tmp/.X11-unix:ro \
  -v ~/neurowatch/data:/data -v ~/neurowatch/recordings:/recordings \
  ghcr.io/ttheekw/neurowatch:gui
```

Closing the window stops the container and recording, so use the headless image to record around the clock.

**Graphics card and sound.** Add these options to either command, before the image name:

| For | Add | Needs on the computer |
|---|---|---|
| Intel or AMD graphics (VA-API) | `--device /dev/dri --group-add "$(stat -c %g /dev/dri/renderD128)"` | The graphics driver (included with Linux) |
| NVIDIA graphics (NVDEC) | `--gpus all` | The NVIDIA driver and the [NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html) |
| Camera sound through your speakers (window only) | `-v "$XDG_RUNTIME_DIR/pulse/native:/run/host/pulse" -e PULSE_SERVER=unix:/run/host/pulse` | PulseAudio or PipeWire (most desktops); check that `$XDG_RUNTIME_DIR/pulse/native` exists first |

With a graphics card option, the status bar shows **VA-API** or **NVIDIA NVDEC** instead of **CPU Decode** (or check with `docker logs neurowatch 2>&1 | grep "Hardware acceleration"`). Without one, video is decoded on the processor. After changing options, remove the container and run it again (`docker stop neurowatch && docker rm neurowatch`); your settings and recordings stay. Step-by-step setup for each card, the NVIDIA Container Toolkit, and what to do if it still says CPU Decode: [Decoding on the graphics card](docs/docker.md#decoding-on-the-graphics-card-va-api-nvidia).

**Day-to-day:** `docker logs -f neurowatch` shows what it's doing, `docker stop neurowatch` stops it (it finishes its recordings first), `docker start neurowatch` starts it again. Run only one of the two at a time: they share the data folder.

**Updating:** `docker pull ghcr.io/ttheekw/neurowatch:latest` (or `:gui`), then `docker stop neurowatch && docker rm neurowatch` (or `neurowatch-gui`) and run the same command again. Your cameras and recordings stay in the folders.

From a copy of this repository, `scripts/docker.sh headless` or `scripts/docker.sh gui` does all of the above, including the graphics card and sound, and `scripts/docker.sh status | logs | stop` controls it. The images are the free edition; with NeuroWatch Unlimited, build your own from its `.deb`: `scripts/docker.sh build --deb FILE`. Details: [NeuroWatch in Docker](docs/docker.md).

---

## Troubleshooting

**No picture from any camera.** Check each camera with **Test** in its settings, and make sure this computer can reach it.

**One camera shows no picture.** Check its address and password with **Test** in the camera's settings. Make sure the camera is reachable from this computer. TCP (the default) works through most networks; UDP doesn't always.

**The tray icon doesn't appear (Ubuntu/GNOME).** GNOME needs the *AppIndicator* extension to show tray icons. Ubuntu includes it. On other GNOME desktops, install "AppIndicator and KStatusNotifierItem Support". NeuroWatch keeps recording either way.

**NeuroWatch doesn't start, or stops.** Run `neurowatch service status`. The service log is at `~/.local/state/neurowatch/logs/service.log` (Linux) or `%LOCALAPPDATA%\Neurowatch\logs\service.log` (Windows). On Linux with the system service, `journalctl --user -u neurowatch-backend` also shows it. If the recorder stops unexpectedly, it starts again by itself, and the log says how it stopped. To find out more, turn on Settings → **Debug** → **Write an extended debug log**: it writes `backend-debug.log` in the same folder (**Open Folder**). Turn it off when you are done.

**A headless computer doesn't start NeuroWatch at boot.** Some systems don't let an account start its services at boot. Run `sudo loginctl enable-linger $USER` once.

**`Could not load the Qt platform plugin "xcb"`** (portable download). Run `sudo apt install libxcb-cursor0`. The `.deb`, `.rpm` and Arch packages install it for you.

---

## Privacy and responsible use

- Recordings are stored on your own drives. Linking NeuroWatch Cloud sends camera names and status, recording availability and requested thumbnails, and supports remote video, diagnostics and clip exports. Exported clips are uploaded for download through the portal.
- **Cloud Backup** (optional, per camera, paid in the portal) is the one feature that stores your video in the cloud. It sends only cameras with a plan, as they record. Footage stays on the backup server for the package's instant-playback time, then in archive storage for its archive days. Clips you keep, and footage held after a recorder goes offline without warning, are kept 7 days. Video is encrypted in transit and at rest by the storage provider, but not end to end.
- External alerts do not require a cloud link. ntfy, Telegram and webhook channels send event details and, if enabled, pictures to their configured destinations.
- Camera addresses can contain passwords. NeuroWatch stores them in its settings file (`~/.config/neurowatch/config.json` on Linux), so keep your computer account secure and don't share that file.
- **Use NeuroWatch lawfully.** Video surveillance is regulated in many places. You are responsible for following the rules that apply to you, for example privacy and data-protection law, notices and signage, consent, and how long you keep footage. Don't record people where they expect privacy, and don't use NeuroWatch for anything unlawful. The NeuroWatch authors don't operate your local installation; optional cloud access and external alerts transmit data as described above.

---

## Disclaimer

NeuroWatch is free, open-source software provided **as is, without any warranty**, and used at your own risk. To the extent the law allows, its authors and contributors are **not liable** for any loss or damage from using it, including missed, lost or corrupted recordings. It is not a guaranteed security, safety or alarm system: test that it works for you and keep backups of footage you need. The full disclaimer is in [`NOTICE`](NOTICE); the [licence](LICENSE) (sections 7 and 8) governs.

---

## Licensing

This is a summary, not legal advice; the licence texts govern.

- **The NeuroWatch app is open source** under the [Apache License 2.0](LICENSE): live view, recording, playback, export and headless mode, free for any use, including business use. The free downloads run up to 16 cameras; NeuroWatch Unlimited, available for USD 0 after sign-up on the [downloads page](https://neurowatch.app/downloads), has no software camera limit. Capacity depends on your hardware.
- **You may use, change and share it**, also in your own products, open or closed. When you share it, keep the [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE) files and mark the files you changed. The licence includes a patent grant from contributors.
- **NeuroWatch Cloud is an online service with its own terms.** The cloud-related code in this repository is open source under the same licence. Hosted service plans are separate from the desktop purchase; see [neurowatch.app](https://neurowatch.app) for current plans.
- **Trademark:** "NeuroWatch" and its logo are not licensed under the Apache License (see [`NOTICE`](NOTICE)). A modified version must use another name and must not be presented as the official NeuroWatch.

NeuroWatch builds on third-party software under its own licences, including GStreamer, Qt/PySide6 and many Rust and Python libraries. You are responsible for any codec or patent licensing that applies where you use it (for example H.264/H.265).

---

## For developers

Source builds include the same built-in NeuroWatch Cloud support as packaged downloads. A development build can also connect to a local development portal; release builds use NeuroWatch Cloud.

Build and run from source on Ubuntu/Debian:

```bash
git clone https://github.com/ttheekw/Neurowatch.git
cd Neurowatch
./neurowatch.sh --bootstrap --system --yes   # install the build tools and libraries (uses sudo)
./neurowatch.sh                              # build and open NeuroWatch
```

When launched from a source checkout, the desktop app stops the development recorder when you close its window and does not register login startup. Set `NW_DEV_KEEP_SERVICE=1` when launching to keep it running like an installed download. To use the real cloud instead of an automatically loaded local development portal, run `NW_CLOUD_ENDPOINT=https://neurowatch.app ./neurowatch.sh`.

More detail:
- [docs/system-requirements.md](docs/system-requirements.md): minimum OS version for each download
- [docs/releasing.md](docs/releasing.md): building and publishing releases (pushing a `v*` tag builds and publishes one)

**Tests.** Pushes to `main` or `develop`, and pull requests targeting those branches, run Rust formatting, lints and workspace tests (with an 80% line-coverage gate), and Python lint, format, type and app tests, plus release and CI contract checks. Optional media tests are separate. The **Tests** badge at the top shows the result for `main`; open a [run](https://github.com/ttheekw/Neurowatch/actions/workflows/ci.yml) to see its summary, a table of passed, failed and skipped tests per suite with the names of any failures, and the coverage report. To run the checks locally, see [CONTRIBUTING.md](CONTRIBUTING.md).

**Contributing.** Bug reports, ideas and pull requests are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md). Contributions are licensed under the Apache License 2.0, like the rest of NeuroWatch.
