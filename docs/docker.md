# NeuroWatch in Docker

Two container images, built from the free release `.deb` for x86_64 and arm64
(Linux hosts):

| Image | What runs | Use it for |
|---|---|---|
| `ghcr.io/ttheekw/neurowatch:latest` (also `:headless`, `:<version>`) | the backend alone, as in `neurowatch --headless` | a recorder on a server or small computer; watch through NeuroWatch Cloud |
| `ghcr.io/ttheekw/neurowatch:gui` (also `:<version>-gui`) | the backend and the desktop window, shown on the host's display | a monitoring station that runs NeuroWatch in a container |

## Running

`scripts/docker.sh` (also `./neurowatch.sh docker …`) runs either one with the
right Docker options:

```bash
scripts/docker.sh headless                       # recorder in the background, restarts with Docker
scripts/docker.sh exec cloud link CODE --name "Head Office"   # link it to NeuroWatch Cloud
scripts/docker.sh gui                            # desktop window (X11, or XWayland on Wayland desktops)
scripts/docker.sh gui --wayland                  # desktop window on the Wayland socket
scripts/docker.sh status | logs | stop           # the running one
scripts/docker.sh exec events --follow           # any `neurowatch` command inside it
```

Without a checkout, plain `docker run` (the README's "Running in Docker" has
the same commands for users):

```bash
mkdir -p ~/neurowatch/data ~/neurowatch/recordings
# headless
docker run -d --name neurowatch --restart unless-stopped --network host \
  --user "$(id -u):$(id -g)" --stop-timeout 30 \
  -v ~/neurowatch/data:/data -v ~/neurowatch/recordings:/recordings \
  ghcr.io/ttheekw/neurowatch:latest
docker exec --user "$(id -u):$(id -g)" neurowatch neurowatch-docker cloud link CODE --name "Head Office"
# gui (X11 / XWayland)
xhost +SI:localuser:$(id -un)
docker run -d --name neurowatch-gui --network host \
  --user "$(id -u):$(id -g)" --stop-timeout 30 --shm-size 1g \
  -e DISPLAY -v /tmp/.X11-unix:/tmp/.X11-unix:ro \
  -v ~/neurowatch/data:/data -v ~/neurowatch/recordings:/recordings \
  ghcr.io/ttheekw/neurowatch:gui
```

`docker/compose.yml` has both as Compose services (gui under the `gui` profile).

### What the run script sets up

- **Data:** `/data` holds the configuration, database, cloud link key and logs
  (`HOME` and the `XDG_*` folders point there); `/recordings` is where new
  configurations record (`~/Videos/neurowatch` is a link to it). Defaults:
  `~/.local/share/neurowatch-docker/{data,recordings}`, or `--data`/`--recordings`.
  To record to another drive, mount it (`-- -v /mnt/nas:/mnt/nas`) and choose
  it in **Settings → Recording**, or from the web portal.
- **User:** the container runs as you (`--user $(id -u):$(id -g)`), so the
  files are yours, and `exec` uses the same user (the IPC socket accepts only
  its own user).
- **Network:** host networking, for ONVIF discovery (multicast) and the
  camera event port 7681.
- **GPU:** `/dev/dri` with its device groups (VA-API decoding, OpenGL video
  tiles) whenever the host has it; `--no-gpu` leaves it out, `--nvidia` adds
  `--gpus all` (needs the NVIDIA Container Toolkit). See
  [Decoding on the graphics card](#decoding-on-the-graphics-card-va-api-nvidia).
- **gui only:** the X11 socket and your `XAUTHORITY` (or, with `--wayland`, the
  Wayland socket), the PulseAudio/PipeWire socket for sound, and
  `--shm-size 1g` for the live frames (Docker's default 64 MB is too small for
  a wall). If the window does not open on X11, allow your user once per
  session: `xhost +SI:localuser:$(id -un)`.
- **Lifetime:** headless restarts with Docker until `scripts/docker.sh stop`.
  Closing the gui window stops its backend and the container, so recording
  stops too; use headless for around-the-clock recording. Only one runs at a
  time (they share the data folder). `docker stop` gives the backend 30 s to
  finish its recordings.

## Decoding on the graphics card (VA-API, NVIDIA)

Without a graphics card, the container decodes video on the processor
(**CPU Decode** in the status bar). Decoding on the card takes far less CPU on
a busy wall, and it is what the desktop install uses too. Pick the section for
your card; the options go before the image name in either `docker run`
command.

| Graphics card | Interface | `docker run` options |
|---|---|---|
| Intel (Broadwell, 2014, and newer) | VA-API | `--device /dev/dri --group-add "$(stat -c %g /dev/dri/renderD128)"` |
| AMD | VA-API | the same |
| NVIDIA (GeForce, Quadro, RTX, data-centre cards) | NVDEC | `--gpus all` |

### Intel or AMD (VA-API)

**1. Check the host.** The graphics driver comes with Linux; the VA-API
drivers are inside the image, so nothing else needs installing. The card must
show up as a render node:

```bash
ls -l /dev/dri
# crw-rw----+ 1 root video  226,   1 ... card1
# crw-rw----+ 1 root render 226, 128 ... renderD128
```

No `/dev/dri` means no usable graphics driver on the host (common on virtual
machines and cloud servers): use the processor, or the NVIDIA section.

**2. Run with the card.** Pass the device and its group. The container runs as
you, and you are usually not in the `render` group, so the group must be added
or the card can't be opened:

```bash
# headless
docker run -d --name neurowatch --restart unless-stopped --network host \
  --user "$(id -u):$(id -g)" --stop-timeout 30 \
  --device /dev/dri --group-add "$(stat -c %g /dev/dri/renderD128)" \
  -v ~/neurowatch/data:/data -v ~/neurowatch/recordings:/recordings \
  ghcr.io/ttheekw/neurowatch:latest

# with the window
xhost +SI:localuser:$(id -un)
docker run -d --name neurowatch-gui --network host \
  --user "$(id -u):$(id -g)" --stop-timeout 30 --shm-size 1g \
  --device /dev/dri --group-add "$(stat -c %g /dev/dri/renderD128)" \
  -e DISPLAY -v /tmp/.X11-unix:/tmp/.X11-unix:ro \
  -v ~/neurowatch/data:/data -v ~/neurowatch/recordings:/recordings \
  ghcr.io/ttheekw/neurowatch:gui
```

With the window, the same option also gives the video tiles the card's OpenGL
instead of software rendering.

**3. Check it** (see [Checking which decoder runs](#checking-which-decoder-runs)):
the status bar shows **VA-API**.

### NVIDIA (NVDEC)

**1. Install the driver and the NVIDIA Container Toolkit on the host.** The
NVIDIA driver must work on the host (`nvidia-smi` lists the card). Then install
the toolkit by following NVIDIA's guide for your distribution:
[Installing the NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html).
Afterwards, connect it to Docker and check that a container can see the card:

```bash
sudo nvidia-ctk runtime configure --runtime=docker
sudo systemctl restart docker
docker run --rm --gpus all ubuntu nvidia-smi    # prints the same card table as on the host
```

**2. Run with the card.** Add `--gpus all`. The image already asks the toolkit
for the video decoding and OpenGL parts of the driver
(`NVIDIA_DRIVER_CAPABILITIES=compute,video,utility,graphics`), so nothing else
is needed:

```bash
# headless
docker run -d --name neurowatch --restart unless-stopped --network host \
  --user "$(id -u):$(id -g)" --stop-timeout 30 \
  --gpus all \
  -v ~/neurowatch/data:/data -v ~/neurowatch/recordings:/recordings \
  ghcr.io/ttheekw/neurowatch:latest

# with the window
xhost +SI:localuser:$(id -un)
docker run -d --name neurowatch-gui --network host \
  --user "$(id -u):$(id -g)" --stop-timeout 30 --shm-size 1g \
  --gpus all \
  -e DISPLAY -v /tmp/.X11-unix:/tmp/.X11-unix:ro \
  -v ~/neurowatch/data:/data -v ~/neurowatch/recordings:/recordings \
  ghcr.io/ttheekw/neurowatch:gui
```

To use one card of several, name it: `--gpus '"device=0"'` (numbers as in
`nvidia-smi`).

**3. Check it:** the status bar shows **NVIDIA NVDEC**.

### Computers with Intel and NVIDIA graphics

NeuroWatch tries VA-API first, then NVIDIA, so with both options it decodes on
the Intel (or AMD) graphics. To decode on the NVIDIA card in a headless
container, pass only `--gpus all`. For the window on a laptop whose screen runs
on the Intel graphics, use the VA-API option.

### Checking which decoder runs

The decoder is chosen when the container starts. Any of these shows it:

- **The window:** the badge at the bottom right of the status bar: **VA-API**,
  **NVIDIA NVDEC** or **CPU Decode** (hover it for the reason).
- **The log:**

  ```bash
  docker logs neurowatch 2>&1 | grep "Hardware acceleration"
  # ... Hardware acceleration: VA-API
  ```

- **The status:**

  ```bash
  docker exec --user "$(id -u):$(id -g)" neurowatch neurowatch-docker service status --json | grep -A5 video_acceleration
  # "kind": "vaapi"   (or "nvdec", or "software")
  ```

Use `neurowatch-gui` instead of `neurowatch` for the window container. After
changing the options, remove the container (`docker stop neurowatch && docker
rm neurowatch`) and run it again: Docker can't add a device to an existing
container. Your settings and recordings stay in the folders.

### With the run script or Compose

- `scripts/docker.sh headless` and `scripts/docker.sh gui` pass `/dev/dri`
  with the right groups automatically whenever the host has it (`--no-gpu`
  leaves it out). Add `--nvidia` for `--gpus all`.
- `docker/compose.yml` has both, commented out, in its shared settings:
  uncomment the `devices`/`group_add` lines for VA-API (and set
  `NW_RENDER_GID`), or the `deploy` block for NVIDIA.

### When it still says CPU Decode

| What you see | Cause | Fix |
|---|---|---|
| `docker logs` shows `warning: /dev/dri/renderD128 is not accessible to uid 1000; add --group-add 110` | `/dev/dri` passed without its group | Run again with that `--group-add` |
| CPU Decode, no warning, `--device /dev/dri` not in the command | The card was not passed | Add the option and run the container again |
| `docker: Error response from daemon: could not select device driver "" with capabilities: [[gpu]]` | NVIDIA Container Toolkit missing or not connected to Docker | Install it, run `sudo nvidia-ctk runtime configure --runtime=docker`, restart Docker |
| `nvidia-smi` works in `docker run --rm --gpus all ubuntu nvidia-smi` but NeuroWatch shows CPU Decode | Both Intel/AMD and NVIDIA passed? VA-API wins; otherwise the driver may be too old for NVDEC | See the section above; update the NVIDIA driver |
| Two Intel/AMD cards (`renderD128`, `renderD129`) and the wrong one decodes | NeuroWatch uses the first card it can open | Pass only the card you want instead of all of `/dev/dri`: `--device /dev/dri/renderD129 --group-add "$(stat -c %g /dev/dri/renderD129)"` (not tested) |
| Intel graphics older than Broadwell (before 2014) | The image's Intel driver (`intel-media-va-driver`) starts at Broadwell | Video decodes on the processor |
| Raspberry Pi or another ARM board | Their decoders are V4L2 devices, not VA-API or NVIDIA | Video decodes on the processor in the container; the native ARM package can use the board's decoder |

### How it works

The bundled GStreamer has the `va` (VA-API) and `nvcodec` (NVIDIA) plugins.
At start `hwaccel::probe` picks VA-API (`vah264dec`), then NVDEC
(`nvh264dec`), then software, as on a normal install, and reports it in
`system.status` `video_acceleration`. The image carries Mesa's VA drivers
(AMD) and, on x86_64, Intel's media driver (`intel-media-va-driver`, iHD).
`NVIDIA_DRIVER_CAPABILITIES` is set because the toolkit's default
(`compute,utility`) leaves out NVDEC (`libnvcuvid`) and the OpenGL libraries
the gui's video tiles need. A render node the container's user can't open makes
the backend wait for GPU access that never comes (`GPU_NOT_ACCESSIBLE`), so the
entrypoint warns with the `--group-add` to use. GStreamer's plugin registry
(in `/data/cache`) refreshes by itself when a card is added later.

## Building locally

```bash
scripts/docker.sh build                     # both images from dist/release/*.deb, else the latest release
scripts/docker.sh build gui --deb FILE      # one image from a package (e.g. NeuroWatch Unlimited)
scripts/docker.sh build --release v0.1.1    # from a published version
scripts/docker.sh headless --image neurowatch:headless
```

`build` tags `neurowatch:headless` and `neurowatch:gui`. The images install the
`.deb` into Ubuntu 24.04 (the arm64 packages need glibc 2.39), plus VA-API
drivers and tini; `gui` adds Mesa, Wayland client libraries and fonts.
`docker/entrypoint.sh` (`neurowatch-docker` in the image) runs the backend
directly with the bundle's GStreamer environment (the image has no systemd;
`NW_SERVICE=external` keeps the UI and launcher commands from managing a
service). Its `health` command backs the image's `HEALTHCHECK`.

The gui image deletes the UI bundle's copies of `libX11`, `libxcb*`,
`libstdc++` and `libgcc_s` where the system has them: the bundle is frozen on
Ubuntu 22.04, and with those copies Ubuntu 24.04's Mesa finds no GLX/EGL
configuration and Qt aborts ("Could not initialize GLX").

## Release CI

`.github/workflows/release.yml`:

- `docker` (needs `package-linux`): for `headless` and `gui` on x86_64
  (`ubuntu-24.04`) and arm64 (`ubuntu-24.04-arm`) runners, builds the image from
  the free `.deb` artifact and runs `scripts/docker_smoke.sh`: the backend comes
  up healthy as a non-root user writing to the mounted volumes, reports the
  mode and the free edition, the gui image opens its window on Xvfb, and
  `docker stop` ends it with exit code 0. Tag builds push
  `ghcr.io/<owner>/neurowatch:<version>-<variant>-<amd64|arm64>`.
- `publish` waits for these checks like the package checks.
- `publish-docker` (after `publish`) joins the architectures into
  `:<version>`, `:latest`, `:<version>-headless`, `:headless`,
  `:<version>-gui` and `:gui`. Every release, betas included, moves `latest`
  (as the GitHub Release is marked Latest). It checks every tag's manifest with
  an empty Docker configuration to verify anonymous access.

Manual workflow runs build and check the images without pushing. Only the free
edition is published; NeuroWatch Unlimited buyers build their own image from
their `.deb` (`scripts/docker.sh build --deb FILE`).

### First publication: enable public pulls

GHCR creates packages as **private**, even when the source repository is public.
A successful authenticated push does not mean users can pull the image.
After the first push, open the
[NeuroWatch package](https://github.com/users/ttheekw/packages/container/package/neurowatch)
(forks: use your owner's package page), then **Package settings → Danger Zone →
Change visibility → Public**. Confirm with the package name `neurowatch`.
This applies to all tags, including `latest` and `gui`; no rebuild is needed.
See [GitHub's package visibility documentation](https://docs.github.com/en/packages/learn-github-packages/configuring-a-packages-access-control-and-visibility#configuring-visibility-of-packages-for-your-personal-account).

If `publish-docker` failed its anonymous access check, rerun that job after the
visibility change. You can verify access locally without using saved logins:

```bash
anonymous_config="$(mktemp -d)"
docker --config "$anonymous_config" manifest inspect ghcr.io/ttheekw/neurowatch:latest
rm -rf "$anonymous_config"
```

## Not tested

The images are checked with no cameras, on Xvfb, on GitHub's runners.
VA-API was checked by hand on an Intel UHD 620 (headless image: the backend
chose VA-API, `vah264dec` decoded H.264 to NV12; also after a first run
without the GPU). NVIDIA (NVDEC and OpenGL), hardware OpenGL in the gui
window, sound through the PulseAudio socket, the Wayland path, X11 native
overlays and real cameras have not been tested in a container. Docker Desktop
(macOS, Windows) has no usable host networking or host display for these
images.
