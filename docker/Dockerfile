# syntax=docker/dockerfile:1
# NeuroWatch container images, built from a release .deb (the build context is
# a folder holding this file, entrypoint.sh and neurowatch.deb; `scripts/docker.sh
# build` prepares it). Two targets:
#   headless  the backend alone: cameras, recording, events, NeuroWatch Cloud
#   gui       the backend and the desktop UI, shown on the host's display (X11
#             or Wayland socket passed in by `scripts/docker.sh gui`)
# Ubuntu 24.04: the arm64 packages need glibc 2.39; the x86_64 ones run on it too.
FROM ubuntu:24.04 AS base

# The source label links the GHCR package to the repository.
LABEL org.opencontainers.image.title="NeuroWatch" \
      org.opencontainers.image.description="NeuroWatch network video recorder" \
      org.opencontainers.image.source="https://github.com/ttheekw/Neurowatch" \
      org.opencontainers.image.url="https://neurowatch.app" \
      org.opencontainers.image.licenses="Apache-2.0"

ARG DEBIAN_FRONTEND=noninteractive
COPY neurowatch.deb /tmp/neurowatch.deb
# The package declares its runtime libraries; GStreamer comes from its bundle.
# VA-API drivers (Intel's media driver exists for x86_64 only) let GPU
# decoding work with --device /dev/dri; tini forwards signals to the entrypoint.
RUN set -eux; \
    extra=""; \
    if [ "$(dpkg --print-architecture)" = amd64 ]; then extra="intel-media-va-driver"; fi; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        /tmp/neurowatch.deb ca-certificates tini mesa-va-drivers $extra; \
    rm -rf /var/lib/apt/lists/* /tmp/neurowatch.deb; \
    test -x /opt/neurowatch/bin/nw-backend

# uid 1000 is the usual first desktop account; `scripts/docker.sh` runs the
# container as the calling user (--user) so files in the volumes stay theirs.
RUN set -eux; \
    userdel --remove ubuntu 2>/dev/null || true; \
    useradd --uid 1000 --user-group --home-dir /data --no-create-home \
        --shell /usr/sbin/nologin neurowatch; \
    mkdir -p /data /recordings; \
    chown neurowatch:neurowatch /data /recordings; \
    chmod 0775 /data /recordings

COPY --chmod=0755 entrypoint.sh /usr/local/bin/neurowatch-docker

# Everything NeuroWatch keeps lives in /data (config, database, cloud node key,
# logs); recordings go to /recordings (the default ~/Videos/neurowatch links
# there). The container runs the backend itself, so the UI and launcher
# commands leave the service alone (NW_SERVICE=external). With `--gpus all`
# the NVIDIA Container Toolkit adds the driver's CUDA and NVDEC libraries
# (GStreamer's nvcodec decoders) and its OpenGL libraries (the gui's video
# tiles); its default capabilities leave out video and graphics.
ENV LANG=C.UTF-8 \
    HOME=/data \
    XDG_CONFIG_HOME=/data/config \
    XDG_DATA_HOME=/data/share \
    XDG_STATE_HOME=/data/state \
    XDG_CACHE_HOME=/data/cache \
    XDG_RUNTIME_DIR=/tmp/neurowatch-runtime \
    NW_RECORDINGS=/recordings \
    NW_SERVICE=external \
    NVIDIA_DRIVER_CAPABILITIES=compute,video,utility,graphics \
    PATH=/opt/neurowatch:$PATH

VOLUME ["/data", "/recordings"]
# Camera event push listener (events.push_port); ONVIF discovery needs host networking.
EXPOSE 7681
USER neurowatch
WORKDIR /data
# Recording finishes its files on SIGTERM; give it time (docker stop -t 30).
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=60s --timeout=20s --start-period=60s --retries=3 \
    CMD ["neurowatch-docker", "health"]
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/neurowatch-docker"]

FROM base AS gui
USER root
# Mesa (OpenGL/EGL for the GPU video tiles), Wayland client libraries (Qt's
# wayland platform) and fonts (the fallback one has the UI's fullwidth "＋");
# the X11 libraries come with the package.
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        libgl1-mesa-dri libegl-mesa0 libglx-mesa0 \
        libwayland-client0 libwayland-cursor0 libwayland-egl1 libxkbcommon0 \
        fonts-dejavu-core fonts-droid-fallback; \
    rm -rf /var/lib/apt/lists/*
# The UI bundle (PyInstaller, frozen on the oldest supported Ubuntu) carries
# that release's libX11, libxcb and libstdc++. This Mesa needs its own newer
# ones: with the bundled copies GLX and EGL find no configuration and Qt
# aborts ("Could not initialize GLX"). Use the system's where it has them.
RUN set -eux; \
    internal=/opt/neurowatch/app/neurowatch-ui/_internal; \
    for lib in "$internal"/libX11.so.6 "$internal"/libX11-xcb.so.1 "$internal"/libxcb*.so.* \
               "$internal"/libstdc++.so.6 "$internal"/libgcc_s.so.1; do \
        [ -e "$lib" ] || continue; \
        if ldconfig -p | grep -q "[[:space:]]$(basename "$lib") ("; then rm -f "$lib"; fi; \
    done
USER neurowatch
# X11 shared memory does not cross the container's IPC namespace.
ENV QT_X11_NO_MITSHM=1
CMD ["gui"]

# Last, so a plain `docker build` makes the headless image.
FROM base AS headless
CMD ["headless"]
