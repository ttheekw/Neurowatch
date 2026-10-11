# System requirements for release builds

What each downloadable release needs to run. Python, Qt, GStreamer 1.24.13 and
FFmpeg 6.1.5 are bundled; nothing else from that stack needs to be installed.
Building from source has different requirements (see "For developers" in the README).

The Linux minimums come from the build machine (see [How the minimums are
set](#how-the-minimums-are-set)). "Tested in CI" means the release workflow
installs the package in a clean container, starts the service, decodes
generated test streams and uninstalls it. No real GPU, monitor, camera or
Raspberry Pi has been tested; [release qualification](release-qualification.md)
covers that.

## Supported downloads

| OS | CPU | Download | Minimum version | glibc / GLib | Status |
|---|---|---|---|---|---|
| Ubuntu (also Mint 21+, Pop!_OS 22.04+) | x86_64 | `.deb`, `.tar.gz` | 22.04 | 2.35 / 2.72 | Tested in CI |
| Ubuntu | arm64 | `.deb`, `.tar.gz` | 24.04 | 2.39 / 2.80 | Tested in CI |
| Debian | x86_64 | `.deb`, `.tar.gz` | 12 (Bookworm) | 2.35 / 2.72 | Tested in CI |
| Debian | arm64 | `.deb`, `.tar.gz` | 13 (Trixie) | 2.39 / 2.80 | Tested in CI |
| Fedora | x86_64 | `.rpm` | 43 | 2.35 / 2.72 | Tested in CI |
| Fedora | arm64 | `.rpm` | 43 | 2.39 / 2.80 | Tested in CI |
| Arch Linux | x86_64 | `.pkg.tar.zst` | current (rolling) | 2.35 / 2.72 | Tested in CI |
| Raspberry Pi OS 64-bit | arm64 | `.deb` | Trixie (Debian 13) | 2.39 / 2.80 | Untested, see [below](#raspberry-pi) |
| Other Linux (portable) | x86_64 / arm64 | `.tar.gz` | glibc and GLib at the minimum | as above per CPU | May work, not supported |
| Windows 10/11 64-bit | x86_64 | `-setup.exe`, `.zip` | Windows 10 (Qt 6 expects 1809+) | not applicable | Tested in CI |

The glibc / GLib column is the minimum the package declares. A distribution
that ships older versions cannot install the native package, and the portable
bundle fails to start on it.

## Not supported

| OS | Reason |
|---|---|
| Ubuntu 20.04, Debian 11 | glibc 2.31 is older than 2.35 |
| Ubuntu 22.04 or Debian 12 on arm64 | glibc 2.35/2.36 is older than the arm64 minimum, 2.39 |
| Raspberry Pi OS Bookworm (Debian 12) | Same as Debian 12 on arm64 |
| RHEL 9, Rocky 9, Alma 9 | glibc 2.34 is one version short |
| RHEL 10, openSUSE | Versions are new enough, but no package is tested and RPM dependency names may differ |
| Arch Linux ARM | No package is built, see [below](#arch-linux-arm) |
| Alpine | Uses musl, not glibc |
| Any 32-bit system (i386, armhf, armv6) | No 32-bit builds |
| macOS | No build |
| Windows on ARM | The installer accepts it (x64 emulation), but it is not tested |

## What the host provides (Linux)

The `.deb`, `.rpm` and Arch packages declare the required rows and the package
manager installs them. With the portable `.tar.gz`, install them yourself.

| Purpose | Libraries | Required |
|---|---|---|
| Base | glibc, GLib/GIO, `glib-networking` (TLS), libstdc++, dbus | Yes |
| Window (Qt) | libEGL, libGL, GLES, fontconfig, xkbcommon-x11, xcb-cursor, X11/xcb | Yes; X11 or Wayland session |
| Media | libva, libdrm, libgudev, libsoup3, gnutls, nettle, gmp, orc, opus | Yes |
| Sound | PulseAudio or PipeWire client, ALSA | For live and playback audio |
| Hardware decoding, Intel | `intel-media-driver` (VA-API) | Optional; without it decoding uses the CPU |
| Hardware decoding, AMD | Mesa VA drivers (`mesa-va-drivers`) | Optional; recommended by the `.deb` and `.rpm` |
| Hardware decoding, NVIDIA | NVIDIA proprietary driver (NVDEC) | Optional |
| Hardware decoding, ARM boards | Kernel V4L2 stateful decoder (`v4l2h264dec`) | Optional; H.264 only |
| Native video overlay | X11 session | Optional; Wayland uses the shared-memory path |

Only glibc and GLib have version requirements; the other libraries are declared
without versions.

## Windows

| Requirement | Notes |
|---|---|
| Windows 10 or 11, 64-bit | The installer requires 10.0; Qt 6 expects 1809 or newer |
| GPU driver | Optional, for hardware decoding |
| Nothing else | GStreamer and the MSVC runtime are bundled. Installs per user, no administrator rights |

The installer is not signed yet, so SmartScreen warns on first run.

## Raspberry Pi

The `.deb` is a generic Debian arm64 package. It should install on 64-bit
Raspberry Pi OS Trixie, but it has never run on a Pi:

| Issue | Detail |
|---|---|
| Bookworm is too old | Most installed Pis run Raspberry Pi OS Bookworm (Debian 12, glibc 2.36). The arm64 build needs glibc 2.39 because the PySide6 aarch64 wheels do, so the package refuses to install. Trixie (glibc 2.41) is required. |
| 32-bit installs | 32-bit Raspberry Pi OS (armhf) and Pi Zero/1 (armv6) have no build. A Pi 3 or Zero 2 W needs the 64-bit OS. |
| Hardware decoding varies by model | The backend tries VA-API, NVDEC, then V4L2 (`v4l2h264dec`), then the CPU. A Pi 4 has a V4L2 H.264 decoder, which should be picked. A **Pi 5 has no H.264 hardware decoder**, so every H.264 camera decodes on the CPU. H.265 hardware decoding on a Pi 4/5 uses stateless V4L2 decoders, which are not in the bundled GStreamer, so H.265 also decodes on the CPU. The V4L2 path has never been tested in a release build. |
| CPU and memory | With CPU decoding, a dense wall can saturate a Pi. Recording does not decode, so it is less affected. No camera counts have been measured. |
| Not the CI environment | CI tests a plain `debian:13` container on a GitHub arm64 server. Raspberry Pi OS has its own kernel, Mesa (VideoCore), Wayland compositor (labwc) and firmware. Qt's OpenGL tiles, the NV12 GPU path and audio output on Pi hardware are unverified. |

To qualify a Pi, run the [release qualification](release-qualification.md)
record on a Pi 4 and a Pi 5 with 64-bit Trixie.

## Arch Linux ARM

The release builds an Arch package for x86_64 only; the packaging script
refuses `--format arch` on arm64. On an Arch Linux ARM device, try the portable
arm64 `.tar.gz` (unsupported).

| Reason | Detail |
|---|---|
| Not official Arch | Arch Linux supports only x86_64. Arch Linux ARM is a separate community project with its own repositories, mirrors and update timing, and nothing checks that the package's dependency names match there. |
| No CI image | The official `archlinux` Docker image is amd64 only, so an install check on an arm64 runner cannot start. Publishing waits for every install check, so such a check would block every release. |
| Unofficial images | Community-built Arch Linux ARM images exist, but a job that gates publishing should not depend on an unofficial image. |

Adding it back needs a trusted image or a real device to install and test on.

## How the minimums are set

A Linux binary needs at least the glibc of the machine it was built on, and
GStreamer is compiled against the build machine's GLib headers while using
the host's GLib at run time. The minimums are therefore those of the release
build runners, declared in `scripts/package_release_linux.sh`
(`MIN_GLIBC`, `MIN_GLIB`):

| CPU | Build runner | glibc | GLib | What sets it |
|---|---|---|---|---|
| x86_64 | Ubuntu 22.04 | 2.35 | 2.72 | The build runner. PySide6 alone would allow 2.34. |
| arm64 | Ubuntu 24.04 | 2.39 | 2.80 | The PySide6 aarch64 wheels, which need glibc 2.39 |

These values are set by hand; no CI step yet checks the built binaries against
them. To check a bundle locally:

```bash
find /opt/neurowatch -type f -exec file {} + | grep ELF | cut -d: -f1 \
  | xargs objdump -T 2>/dev/null | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1
```

A build on a newer host raises the real minimum to that host's glibc, so build
releases inside the oldest target OS.
