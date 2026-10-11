# Release qualification

Recommended before a stable release; optional for prereleases (betas and
release candidates), which users test (see [releasing](releasing.md)). Run the
non-publishing Release workflow for the exact commit first, download all
artifacts and `SHA256SUMS`, then copy this page into the release issue and fill
it in. The stable tag must point to the same commit. Release CI proves build,
installation and basic media operation; this record covers behavior that needs
real displays, GPUs, cameras and elapsed time.

## Candidate identity

- Version:
- Commit SHA:
- Release workflow run URL:
- `SHA256SUMS` verified on every tested artifact: [ ]
- Tester and date:

If any binary changes after qualification, start a new record. Do not reuse the
results from another commit or rebuild.

## Required machines

Record the OS build, CPU architecture, GPU model, driver version, display
server, monitor count/scaling and installed artifact for each machine.

| Result | Platform | Required coverage |
|---|---|---|
| [ ] | Windows x86_64 | Current Windows 11, installer and NVIDIA decode when supported by the GPU |
| [ ] | Linux x86_64 | Supported Ubuntu release, Intel or AMD VA-API decode |
| [ ] | Linux x86_64 | Supported Linux release, NVIDIA/NVDEC decode |
| [ ] | Linux arm64 | A supported arm64 distribution and device |
| [ ] | Multi-monitor desktop | At least two monitors, including mixed scale when available |

For every row, confirm **Diagnostics → Active decoder** names the intended
hardware decoder. A software-decoder pass is not evidence for VA-API or NVDEC.
The Linux bundle's `va` plugin must report 1.24.13; versions 1.24.0–1.24.5 are
release blockers.

## Install, upgrade and removal

Run these checks with the downloadable artifact, not a source checkout.

- [ ] Clean install launches the UI and starts the background service.
- [ ] Upgrade from the previous production version preserves configuration,
      cameras, licence, recordings and the recording index.
- [ ] The upgraded backend uses the bundled GStreamer runtime.
- [ ] Closing the UI leaves the service running and recording.
- [ ] Log out/in or reboot starts the configured desktop or headless mode.
- [ ] Uninstall stops and unregisters the service and tray entry, removes the
      application, and preserves user configuration and recordings.
- [ ] Reinstall after uninstall finds the preserved data and starts normally.

## Camera and media behavior

Use at least one real camera for every codec, transport and audio format claimed
for the release. Record camera make/model, firmware, stream codec/resolution/FPS,
transport and audio codec.

- [ ] Live main and sub streams connect, disconnect and reconnect.
- [ ] VA-API/NVDEC is actually selected for H.264; test H.265 where claimed.
- [ ] Manual page changes and automatic page cycling keep pictures current.
- [ ] Solo, grid changes and extra monitor windows do not leak decoders.
- [ ] Camera sound can be selected, switched and muted.
- [ ] Continuous recording survives closing and reopening the UI.
- [ ] Playback opens every tested camera, seeks across segment boundaries and
      plays audio at 0.5x, 1x and 2x.
- [ ] MP4 export opens in an independent player with expected video and audio.
- [ ] An unplugged camera becomes offline and records again after reconnection.

## 24-hour soak

Run the installed application for at least 24 hours with the intended camera
count and normal page-cycle interval. Include concurrent recording, repeated
playback seeks and portal thumbnail/live requests when those features ship.
Capture measurements after warmup and at least hourly.

| Time | Backend PID/restarts | Open files or handles | RSS | GPU memory | CPU | Recording gaps |
|---|---:|---:|---:|---:|---:|---|
| Warmup | | | | | | |
| +1 h | | | | | | |
| +6 h | | | | | | |
| +12 h | | | | | | |
| +24 h | | | | | | |

Linux can read the backend PID from `neurowatch service status --json`, count
`/proc/<pid>/fd`, and read RSS from `/proc/<pid>/status`. On Windows, record
`HandleCount`, `WorkingSet64` and the process start time with `Get-Process`.
Keep the backend log and a list of recording files with their durations.

Pass criteria:

- [ ] The backend never crashes or restarts.
- [ ] Open files/handles and memory stabilize after warmup; there is no steady
      growth correlated with decoder creation or page cycling.
- [ ] Every recording is playable and no unexplained timeline gap appears.
- [ ] The active hardware decoder stays unchanged and the GPU reports no reset.
- [ ] CPU, memory, storage latency and GPU use retain operational headroom.

Also run the 1,000-cycle decoder recreation test from
[VA decoder reliability](gstreamer-va-reliability.md) on each Linux GPU/driver
combination and attach its factory names and descriptor counts.

## Decision

- [ ] All required rows passed.
- [ ] Every failure has a linked blocking issue or an explicitly removed support
      claim.
- [ ] Release approved by:
- Approval date:

Windows ARM, macOS, Alpine, RHEL and openSUSE require their own build,
installation and this full qualification before they can be added to the
supported-platform list.
