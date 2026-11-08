# NeuroWatch Motion (nw-motion)

[![CI](https://github.com/ttheekw/Neurowatch-Motion/actions/workflows/ci.yml/badge.svg)](https://github.com/ttheekw/Neurowatch-Motion/actions/workflows/ci.yml)
[![Release](https://github.com/ttheekw/Neurowatch-Motion/actions/workflows/release.yml/badge.svg)](https://github.com/ttheekw/Neurowatch-Motion/releases/latest)

The motion detection plugin for [NeuroWatch](https://github.com/ttheekw/Neurowatch). NeuroWatch
sends it the cameras' compressed H.264/H.265 video; it decodes and analyses that video itself and
answers with motion events and drawings (the boxes on the live wall). The recorder never decodes
video for it. ONVIF camera events stay in NeuroWatch.

NeuroWatch's own packages already include this plugin and start it for you. Download it from here
to run motion detection on **another computer** (a server with spare CPU or a GPU), to limit who
may use it with [access codes](#access-codes), or to get a newer version before the next
NeuroWatch release.

## Download

Every release is built and tested by [GitHub Actions](.github/workflows/release.yml) and carries
its own GStreamer and FFmpeg decoders: nothing else to install.

| System | Download |
|---|---|
| Windows 10/11 (64-bit) | [nw-motion-windows-x86_64.zip](https://github.com/ttheekw/Neurowatch-Motion/releases/latest/download/nw-motion-windows-x86_64.zip) |
| Ubuntu 22.04+, Debian 12+ (64-bit PC) | [nw-motion_amd64.deb](https://github.com/ttheekw/Neurowatch-Motion/releases/latest/download/nw-motion_amd64.deb) |
| Ubuntu 22.04+, Debian 12+ (ARM 64-bit) | [nw-motion_arm64.deb](https://github.com/ttheekw/Neurowatch-Motion/releases/latest/download/nw-motion_arm64.deb) |
| Fedora and other RPM systems (64-bit PC) | [nw-motion.x86_64.rpm](https://github.com/ttheekw/Neurowatch-Motion/releases/latest/download/nw-motion.x86_64.rpm) |
| Fedora and other RPM systems (ARM 64-bit) | [nw-motion.aarch64.rpm](https://github.com/ttheekw/Neurowatch-Motion/releases/latest/download/nw-motion.aarch64.rpm) |
| Arch Linux | [nw-motion-x86_64.pkg.tar.zst](https://github.com/ttheekw/Neurowatch-Motion/releases/latest/download/nw-motion-x86_64.pkg.tar.zst) |
| Any other Linux (64-bit PC), no install | [nw-motion-linux-x86_64.tar.gz](https://github.com/ttheekw/Neurowatch-Motion/releases/latest/download/nw-motion-linux-x86_64.tar.gz) |
| Any other Linux (ARM 64-bit), no install | [nw-motion-linux-arm64.tar.gz](https://github.com/ttheekw/Neurowatch-Motion/releases/latest/download/nw-motion-linux-arm64.tar.gz) |

Older versions, release notes and `SHA256SUMS`: [all releases](https://github.com/ttheekw/Neurowatch-Motion/releases).
Linux builds need glibc 2.35 or newer. The ARM builds are tested in Ubuntu 22.04, Debian 12/13
and Fedora containers; 64-bit Raspberry Pi OS (bookworm) meets the requirement but has not been
tried on a Pi.

### Install

```sh
sudo apt install ./nw-motion_amd64.deb          # Ubuntu, Debian
sudo dnf install ./nw-motion.x86_64.rpm         # Fedora
sudo pacman -U nw-motion-x86_64.pkg.tar.zst     # Arch Linux
tar -xzf nw-motion-linux-x86_64.tar.gz          # portable: run ./nw-motion/nw-motion
sha256sum -c SHA256SUMS --ignore-missing        # optional: check the download
```

Packages install to `/opt/nw-motion` with `nw-motion` on the PATH. On Windows, unzip anywhere and
run `nw-motion\bin\nw-motion.exe` from a terminal. `nw-motion --check` confirms that every
decoder and element the analysis needs is present.

### Connect it to NeuroWatch

**On another computer** (open UDP port 7690, or TCP for `tls:`):

```sh
nw-motion --listen quic:0.0.0.0:7690 --identity ~/.local/state/nw-motion/identity.key
```

In NeuroWatch open **Plugins → Add Plugin → URL**, enter `quic://that-computer:7690`, check that
the fingerprint shown matches the one the daemon logs, and approve it. Then bind cameras on the
plugin's page. Keep the identity file: a new key needs approving again. `tls:` works where UDP is
blocked.

**On the same computer**, the included copy already runs. To run this one beside it, give it its
own socket in NeuroWatch's plugin folder and add it under **Plugins → Add Plugin → Local**:

```sh
nw-motion --listen unix:$XDG_RUNTIME_DIR/neurowatch/plugins/nw-motion-2.sock
```

## Run from source

```sh
cargo build --release
# Default: private Unix socket, or local Windows named pipe.
target/release/nw-motion
# Remote encrypted transport with a persistent identity:
target/release/nw-motion --listen quic:0.0.0.0:7690 --identity /private/path/motion.key
# Encrypted fallback when UDP is blocked:
target/release/nw-motion --listen tls:0.0.0.0:7690 --identity /private/path/motion.key
# Loopback development:
target/release/nw-motion --listen tcp:127.0.0.1:7690
```

Building from source needs a NeuroWatch checkout beside this one (see [Develop](#develop)) and
GStreamer >=1.24 with h264parse/h265parse and gst-libav (the release packages bring
their own). Started by hand, it runs on that checkout's release GStreamer
(`.cache/gstreamer-sdk-<newest>`, from `scripts/install_gstreamer_dev_sdk.sh`) when it is there,
as NeuroWatch runs its backend: Ubuntu 24.04's GStreamer 1.24.2 logs a
`gst_mini_object_unref` critical for every picture the analysis skips. `NW_GSTREAMER_SDK=0`
keeps the system's, a directory names another SDK; a set `GST_PLUGIN_SYSTEM_PATH_1_0` wins. Each camera's **Video decoder**
setting (Automatic, VA-API, NVIDIA NVDEC, Intel Quick Sync, V4L2 or CPU; Direct3D on Windows)
names the decoder used. Automatic tries VA-API, NVDEC, Direct3D 11, then V4L2, as NeuroWatch
does; a choice that cannot be used, or a hardware decoder with no picture after 10 s, moves on
and ends on the CPU, with the reason in the camera's status. VA-API is refused on GStreamer
1.24.0–1.24.5 (it leaks). `--hardware-decoding` is ignored. `--check` lists the hardware decoders
found. Preserve the identity file across restarts. Unix
socket directories are private (0700), sockets and secret files owner-only (0600); Windows uses
local named pipes and DPAPI-protected keys. QUIC binds its TLS certificate to the same plugin key.
The host must approve that key and permissions before sending media or credentials.

Packaged NeuroWatch starts its own included daemon and preapproves its local key. It manages
only that child. An external daemon is never killed by the recorder. A managed child also exits
when its parent dies. Up to 1024 camera bindings may be attached per session, subject to actual
CPU/decoder capacity; callbacks and media are bounded.

## Access codes

Without a code file, every recorder you approve may use the daemon. To admit only recorders
that know a code, and only so many at a time, write a code file and start the daemon with it:

```sh
target/release/nw-motion access-codes add /private/path/codes.json site-a --max-connections 10
# prints a new random code once; or bring your own:
echo "$CODE" | target/release/nw-motion access-codes add /private/path/codes.json site-b \
    --max-connections 2 --code-stdin
target/release/nw-motion --listen quic:0.0.0.0:7690 --identity /private/path/motion.key \
    --access-codes /private/path/codes.json
```

The file never holds a code: only a random salt and, per code, its PBKDF2-HMAC-SHA256 hash
(600,000 rounds), its name and its limit. It is written owner-only (0600). `access-codes list`
shows names and limits, `limit PATH NAME N` changes a limit and `remove PATH NAME` removes a
code. Giving an existing name a new code with `add` replaces the old one.

A recorder sends its code as the plugin's vendor credential (NeuroWatch: Plugins → Vendor
Credential…). Each connected recorder holds one place under its code from sign-in until it
disconnects; probes from the Add Plugin dialog take none. A missing or unknown code is refused
with `ACCESS_CODE`, one over the limit with `CONNECTION_LIMIT`; NeuroWatch shows the message on
the plugin's page and tries again every 2 s, so it gets in once a place is free.

- **Edits apply while running.** The daemon reads the file again every 2 s. A removed code, or
  a name given a new code, disconnects the recorders using it; a lowered limit disconnects the
  newest connections above it. A file that is broken, missing or unreadable keeps the list in
  force (one warning per version of the file); at start it stops the daemon instead of serving
  everyone.
- **Lost recorders give their place back.** NeuroWatch pings every 5 s; a connection silent for
  15 s (a lost network can leave one open for minutes) is closed and its place freed.
- **Restarts start from zero.** Places are kept in memory only: after a restart every recorder
  reconnects within seconds and is counted again. If more recorders share a code than its
  limit, which of them get in after a restart is first come, first served.

## Updating from 0.1.0

Version 0.2.0 includes mandatory signed session identities. Older 0.1.0 binaries built before
that feature connect and disconnect before any camera is bound. The recorder reports that the
daemon needs updating; adding a new server entry does not upgrade the executable.

Stop the older daemon, rebuild it (`cargo build --release` in this checkout), and start
`target/release/nw-motion` again. Keep the new persistent identity file across later restarts.
For a separately launched daemon, approve its displayed key and permissions in NeuroWatch's
Plugins tab before it receives cameras. The included managed daemon is preapproved locally.

## Cameras and dashboard

Open NeuroWatch's Plugins tab to bind cameras, select Main/Sub, tune sensitivity, minimum
duration, cooldown, low CPU and watched/ignored zones. Live shows all drawing layers; Output
lists actual events. The motion browser dashboard uses the host SDK's camera picker, live view,
zone editor and event list. Plugin pages are authenticated and sandboxed by NeuroWatch.

Two layers: `detections` shows the moving area/score (wall default); `debug` shows changed blocks,
zones and analysis metrics. Drawings expire after 1.5 seconds and clear on detach/loss. Events
close when a session is lost; NeuroWatch reconnects with 2/5/10/30 second backoff.

A verified local session can reuse existing stable desktop RGB/NV12 pictures at <=5 fps through
a read-only capability ring. A suitable larger Main tile can supply pictures while compressed
Sub is cached. All conversion/scaling happens here. The SDK resumes from the cached compressed
GOP after one second without desktop pictures. Low-CPU mode always uses compressed keyframes
and can miss short movement. Source switches reset detector background without ending a healthy
open event unnecessarily.

## Develop

`nw-analytics-protocol` and `nw-analytics-sdk` are path dependencies in the sibling NeuroWatch
checkout. `src/detector.rs` is now canonical plugin logic: the recorder's builtin detector has
been removed. `src/worker.rs` owns decoding/sharing, `activity.rs` the event lifecycle,
`drawing.rs` overlays, `server.rs` authenticated protocol, and `web/index.html` the dashboard.

```sh
env -u GIO_MODULE_DIR cargo test
cargo clippy --all-targets -- -D warnings
```

Tests include real x264 moving-ball video over the daemon protocol, settings, detector, event
lifecycle and drawing behavior. Loopback motion qualification fixture remains available in
NeuroWatch's `tests/fixtures/motion_rtsp.py` for isolated recorder integration.

Releases include a source snapshot at NeuroWatch `crates/nw-motion`. After changing this
standalone repo, run `python3 ../Neurowatch/scripts/sync_motion_plugin.py`, then `--check`.

The separate [CPU motion evaluator](tools/motion-eval/README.md) compares the current
detector with pinned SuBSENSE/PAWCS reference code and OpenCV MOG2/KNN on labeled
video. [Initial results](docs/research/cpu-motion-comparison/README.md) include mask
quality, CPU/memory measurements, seed repeats, and regression fixtures. This
evaluation tool does not change the production detector or its build dependencies.
Only Cargo dependency paths and workspace profile declarations differ.

## Releasing

1. Set the new version in `Cargo.toml` and run `cargo build` (updates `Cargo.lock`).
2. Point `neurowatch.ref` at the NeuroWatch commit or tag to build against. Its protocol crates
   and its GStreamer build (`scripts/build_release_gstreamer.sh`) go into the release.
3. Push, wait for CI, then tag: `git tag v0.2.0 && git push origin v0.2.0`.

The [Release workflow](.github/workflows/release.yml) builds the Windows zip and the Linux
packages (x86_64 and arm64), installs each one in clean Ubuntu, Debian, Fedora and Arch
containers, runs `--check`, serves with an access code, and only then publishes the GitHub
Release with `SHA256SUMS` and the version-free names the download table links to. A tag with
a suffix (`v0.3.0-rc.1`) is published as a pre-release. **Run workflow** on the Release
workflow builds test packages without publishing. While NeuroWatch's repository is private, add
a `NEUROWATCH_REPO_TOKEN` secret (a token that can read it).

Locally: `scripts/package_linux.sh --version v0.2.0 --arch x86_64 --format portable,deb`
(builds NeuroWatch's GStreamer once into `build/gstreamer-sdk`), and on Windows, from an MSVC
developer shell with GStreamer's MSVC packages, `scripts/package_windows.ps1 -Version v0.2.0`.
