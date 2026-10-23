#!/usr/bin/env bash
# Entrypoint of the NeuroWatch container images (docker/Dockerfile).
#
#   headless   run the backend alone in the foreground (the headless image)
#   gui        run the backend, then the desktop UI on the host's display;
#              closing the window stops the backend and the container
#   health     exit 0 while the backend answers (the image's HEALTHCHECK)
#   COMMAND    anything else goes to the NeuroWatch launcher, for example
#              `cloud status`, `cloud link CODE --name NAME`, `events --follow`,
#              `service status` (with `docker exec` into a running container)
set -euo pipefail

BASE=/opt/neurowatch
GST="$BASE/gstreamer"
# The release bundle's GStreamer, as the service manager sets it up
# (ui/neurowatch/service/manager.py bundled_gstreamer_env). Only the backend
# gets it: the UI's PyInstaller libraries must not mix with it.
BACKEND_ENV=(
    "PATH=$GST/bin:$BASE/bin:$PATH"
    "LD_LIBRARY_PATH=$GST/lib"
    "GST_PLUGIN_PATH_1_0=$GST/lib/gstreamer-1.0"
    "GST_PLUGIN_SYSTEM_PATH_1_0=$GST/lib/gstreamer-1.0"
    "GST_PLUGIN_SCANNER_1_0=$GST/libexec/gstreamer-1.0/gst-plugin-scanner"
    "GST_REGISTRY_1_0=$XDG_CACHE_HOME/neurowatch/gstreamer-registry.bin"
)
STARTUP_TIMEOUT="${NW_STARTUP_TIMEOUT:-60}"

say() { printf 'neurowatch-docker: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }

prepare() {
    [[ -w "$HOME" ]] || die "$HOME is not writable by uid $(id -u): mount a folder this user owns" \
        "(scripts/docker.sh runs the container as you), or pass --user with the folder's owner"
    mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME" "$XDG_CACHE_HOME/neurowatch"
    mkdir -p "$XDG_RUNTIME_DIR"
    chmod 0700 "$XDG_RUNTIME_DIR"
    # New configurations record to ~/Videos/neurowatch (config.rs); here that
    # is the recordings volume. An existing folder or link is left as it is.
    if [[ ! -e "$HOME/Videos/neurowatch" && ! -L "$HOME/Videos/neurowatch" ]]; then
        mkdir -p "$HOME/Videos"
        ln -s "$NW_RECORDINGS" "$HOME/Videos/neurowatch"
    fi
    [[ -w "$NW_RECORDINGS" ]] || say "warning: $NW_RECORDINGS is not writable by uid $(id -u); recording there will fail"
    # A render node passed in without its group: the backend would wait for
    # GPU access that never comes (hwaccel GPU_NOT_ACCESSIBLE) and decode on the CPU.
    local node
    for node in /dev/dri/renderD*; do
        [[ -e "$node" && ! ( -r "$node" && -w "$node" ) ]] || continue
        say "warning: $node is not accessible to uid $(id -u); add --group-add $(stat -c %g "$node") for GPU decoding"
    done
}

backend_up() {
    neurowatch service status >/dev/null 2>&1
}

headless() {
    prepare
    say "starting the backend (headless)"
    exec env "${BACKEND_ENV[@]}" NW_HEADLESS=1 "$BASE/bin/nw-backend"
}

gui() {
    prepare
    [[ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]] \
        || die "no display: pass DISPLAY and /tmp/.X11-unix, or WAYLAND_DISPLAY (scripts/docker.sh gui does)"

    local backend ui="" stopping=0 status=0
    env "${BACKEND_ENV[@]}" NW_HEADLESS=0 "$BASE/bin/nw-backend" &
    backend=$!
    trap 'stopping=1' TERM INT

    say "waiting for the backend"
    local waited=0
    until backend_up; do
        kill -0 "$backend" 2>/dev/null || { wait "$backend" || status=$?; die "the backend exited ($status)"; }
        (( stopping )) && break
        (( waited++ < STARTUP_TIMEOUT )) || { kill -TERM "$backend"; wait "$backend" || true; die "the backend did not answer within ${STARTUP_TIMEOUT}s"; }
        sleep 1
    done

    if (( ! stopping )); then
        say "opening the desktop UI"
        neurowatch --no-autostart "$@" &
        ui=$!
        # Until the window closes, the backend stops, or docker stop.
        wait -n "$backend" "$ui" || status=$?
    fi

    if [[ -n "$ui" ]] && kill -0 "$ui" 2>/dev/null; then
        kill -TERM "$ui" 2>/dev/null || true
        wait "$ui" 2>/dev/null || true
    fi
    if kill -0 "$backend" 2>/dev/null; then
        say "stopping the backend"
        kill -TERM "$backend" 2>/dev/null || true
        wait "$backend" 2>/dev/null || true
    fi
    (( stopping )) && exit 0
    exit "$status"
}

case "${1:-headless}" in
    headless) shift || true; headless ;;
    gui) shift; gui "$@" ;;
    health) backend_up ;;
    *) exec neurowatch "$@" ;;
esac
