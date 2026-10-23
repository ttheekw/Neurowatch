#!/usr/bin/env bash
# Supported local NeuroWatch launcher. It may be run from any working directory.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# NW_PYTHON_PROJECT, NW_BACKEND_WORKSPACE and NW_BACKEND_PACKAGE run another
# Python project or Cargo workspace (a build that adds to this one).
PYTHON_PROJECT="${NW_PYTHON_PROJECT:-$ROOT}"
VENV="$PYTHON_PROJECT/.venv"
BACKEND_WORKSPACE="${NW_BACKEND_WORKSPACE:-$ROOT}"
BACKEND_PACKAGE="${NW_BACKEND_PACKAGE:-nw-backend}"
MODE="start"
MODE_FLAGS=0
INSTALL_SYSTEM=0
CONFIRM_SYSTEM=0
HEADLESS=0
NO_AUTOSTART=0
FOREGROUND=0
SERVICE_ARGS=()
# Further --headless options (the cloud's --link/--name) and a command with its
# arguments (events, cloud); both go to neurowatch.headless.
EXTRA_ARGS=()
COMMAND_ARGS=()

usage() {
    cat <<'USAGE'
Usage: ./neurowatch.sh [--no-autostart]
       ./neurowatch.sh --headless [--no-autostart | --foreground] [--link CODE --name NAME]
       ./neurowatch.sh service (status | start | stop | restart | install | uninstall)
       ./neurowatch.sh events [--follow] [--camera NAME] [--limit N]
       ./neurowatch.sh cloud (status | link CODE --name NAME | unlink --yes)
       ./neurowatch.sh docker (headless | gui | stop | logs | status | exec ... | build ...)
       ./neurowatch.sh [--check | --bootstrap [--system --yes]]

  (no arguments)  synchronize Python dependencies, build the backend, then
                  open the desktop UI. The UI starts the background service
                  (in this checkout, the dev service, which rebuilds and
                  restarts the backend whenever its code changes) and stops it
                  when the UI closes; nothing is registered to start at login
                  and no tray icon is started. With NW_DEV_KEEP_SERVICE=1 it
                  behaves as installed builds do: the service keeps recording
                  after the UI closes and is registered, with the tray icon,
                  to start at login (a systemd user service on Linux)
  --no-autostart  with NW_DEV_KEEP_SERVICE=1: start the service for this
                  session only: register nothing with systemd or login items
  service ...     control the background service: status, start, stop,
                  restart, install (start at login), uninstall
  --headless       run without the desktop UI: the backend does everything
                   (capture, recording) as the background service in
                   headless mode, registered to start at boot (systemd user
                   service with lingering); this terminal shows status lines
                   (cameras, recording). Ctrl+C closes the status view;
                   the service keeps running (./neurowatch.sh service stop)
  --foreground     with --headless: no service; run the backend in this
                   terminal and stop it with Ctrl+C (containers, or a system
                   service manager of your own)
  events           list recent events (motion, camera events, cameras
                   offline, recording failures); --follow then prints new
                   ones until Ctrl+C
  --link CODE --name NAME
                   with --headless: link to NeuroWatch Cloud first (an
                   enrollment code from the web portal)
  cloud            NeuroWatch Cloud: status, link CODE --name NAME, unlink --yes
  docker           run the container images instead (scripts/docker.sh: headless
                   recorder or desktop window, built from a release package;
                   ./neurowatch.sh docker --help)
  --check          validate the local tools and GStreamer development files;
                  does not synchronize dependencies or start a service
  --bootstrap      synchronize locked Python project dependencies only
  --system         with --bootstrap, install Debian/Ubuntu system packages
  --yes            required together with --system; confirms apt/sudo changes
  --help           show this help

System package installation is never attempted unless both --system and --yes
are supplied. Install Rust and uv using their official instructions if absent.
USAGE
}

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || {
        printf 'missing required command: %s\n' "$1" >&2
        return 1
    }
}

sanitize_snap_library_paths() {
    [[ -n "${LD_LIBRARY_PATH:-}" ]] || return 0

    local entry
    local sanitized=""
    local separator=""
    local removed=0
    local -a entries=()
    local IFS=:
    read -r -a entries <<< "$LD_LIBRARY_PATH"

    for entry in "${entries[@]}"; do
        case "$entry" in
            /snap/*|/var/lib/snapd/snap/*)
                removed=1
                ;;
            *)
                sanitized+="${separator}${entry}"
                separator=:
                ;;
        esac
    done

    (( removed )) || return 0
    if [[ -n "$sanitized" ]]; then
        export LD_LIBRARY_PATH="$sanitized"
    else
        unset LD_LIBRARY_PATH
    fi
    printf '%s\n' '==> Ignoring incompatible Snap library paths inherited by the launcher'
}

restore_vscode_host_environment() {
    local variable
    local original
    local value
    local restored=0

    # Snap's VS Code wrapper preserves the host values before overriding these
    # variables. Integrated terminals inherit the overrides, which can make a
    # host GStreamer process load GIO modules and glibc libraries from core20.
    for variable in \
        GDK_BACKEND GDK_PIXBUF_MODULE_FILE GDK_PIXBUF_MODULEDIR GIO_MODULE_DIR \
        GSETTINGS_SCHEMA_DIR GTK_EXE_PREFIX GTK_IM_MODULE_FILE GTK_PATH LOCPATH \
        XDG_CONFIG_DIRS XDG_DATA_DIRS \
        XDG_DATA_HOME
    do
        original="${variable}_VSCODE_SNAP_ORIG"
        if [[ -v "$original" ]]; then
            value="${!original}"
            if [[ -n "$value" ]]; then
                export "$variable=$value"
            else
                unset "$variable"
            fi
            restored=1
        elif [[ -v "$variable" ]]; then
            value="${!variable}"
            case "$value" in
                /snap/*|*/snap/code/*|*/snap/core*/*)
                    unset "$variable"
                    restored=1
                    ;;
            esac
        fi
    done

    if (( restored )); then
        printf '%s\n' '==> Restoring host environment inherited through VS Code Snap'
    fi
}

# Ubuntu 24.04 currently ships the affected GStreamer 1.24.2 VA plugin.  The
# repository-local runtime is built by scripts/install_gstreamer_va_runtime.sh
# from a checksummed upstream bug-fix release.  Keep the system plugin path so
# the rest of GStreamer still comes from the distribution; only the updated
# gst-plugins-bad support libraries and VA plugin are preferred here.
configure_local_gstreamer_va_runtime() {
    local runtime="$ROOT/.cache/gstreamer-1.24.13"
    local plugin
    local plugin_dir
    local lib_dir

    [[ "$(uname -s)" == Linux && -d "$runtime/lib" ]] || return 0
    plugin="$(find "$runtime/lib" -path '*/gstreamer-1.0/libgstva.so' -type f -print -quit)"
    [[ -n "$plugin" ]] || return 0

    plugin_dir="${plugin%/libgstva.so}"
    lib_dir="${plugin_dir%/gstreamer-1.0}"
    export LD_LIBRARY_PATH="$lib_dir${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    export GST_PLUGIN_PATH_1_0="$plugin_dir${GST_PLUGIN_PATH_1_0:+:$GST_PLUGIN_PATH_1_0}"
    export GST_REGISTRY_1_0="$ROOT/.cache/gstreamer-registry-1.24.13.bin"
}

# The cloud's LiveKit publisher links libwebrtc, which is built against
# Chromium's hermetic libc++ and must be compiled with clang >= 21.
configure_cxx() {
    [[ -z "${CXX:-}" ]] || return 0
    local candidate
    for candidate in clang++-21 clang++-22 clang++-23; do
        if command -v "$candidate" >/dev/null 2>&1; then
            export CXX="$candidate"
            return 0
        fi
    done
    printf '%s\n' 'missing clang >= 21 (the backend links libwebrtc): install clang-21 from apt.llvm.org (./neurowatch.sh --bootstrap --system --yes), or set CXX' >&2
    return 1
}

# Development links to the local web portal, as scripts/dev_cloud.sh does:
# its settings file names the cloud (http://127.0.0.1:8000) and LiveKit.
# Settings already in the environment win; release builds ignore all of these.
load_portal_settings() {
    local portal_env="${NW_WEBAPP_DIR:-$ROOT/../Neurowatch-webapp}/.env.node-dev"
    [[ -z "${NW_CLOUD_ENDPOINT:-}" && -f "$portal_env" ]] || return 0
    set -a
    # shellcheck disable=SC1090
    . "$portal_env"
    set +a
    printf 'Cloud: %s (local web portal settings from %s)\n' "${NW_CLOUD_ENDPOINT:-not set}" "$portal_env" >&2
}

check_tools() {
    local failed=0
    require_command cargo || failed=1
    require_command uv || failed=1
    if (( failed )); then
        printf '%s\n' 'Install missing tools, then retry. No system changes were made.' >&2
        return 1
    fi
}

check_dependencies() {
    local failed=0
    check_tools || failed=1
    configure_cxx || failed=1
    if command -v pkg-config >/dev/null 2>&1; then
        if ! pkg-config --exists gstreamer-1.0 gstreamer-app-1.0 gstreamer-video-1.0; then
            printf '%s\n' 'missing GStreamer development packages (pkg-config gstreamer-1.0, gstreamer-app-1.0, gstreamer-video-1.0)' >&2
            failed=1
        fi
    else
        printf '%s\n' 'missing required command: pkg-config (needed to validate GStreamer)' >&2
        failed=1
    fi
    if (( failed )); then
        printf '%s\n' 'For Debian/Ubuntu, review then run: ./neurowatch.sh --bootstrap --system --yes' >&2
        return 1
    fi
}

sync_python() {
    require_command uv
    printf '%s\n' '==> Synchronizing locked Python dependencies'
    (cd "$PYTHON_PROJECT" && uv sync --locked)
}

install_system() {
    [[ -r /etc/os-release ]] || fail '--system is supported only on Debian/Ubuntu hosts'
    # shellcheck disable=SC1091
    . /etc/os-release
    case "${ID:-}" in
        debian|ubuntu) ;;
        *) fail "--system is supported only on Debian/Ubuntu hosts (detected: ${ID:-unknown})" ;;
    esac
    require_command sudo
    require_command apt-get
    printf '%s\n' '==> Installing Debian/Ubuntu system dependencies (confirmed by --yes)'
    sudo apt-get update
    sudo apt-get install -y \
        build-essential pkg-config curl git git-lfs \
        libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
        libgstreamer-plugins-bad1.0-dev gstreamer1.0-plugins-base \
        gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
        gstreamer1.0-plugins-ugly gstreamer1.0-libav gstreamer1.0-tools \
        libxcb-cursor0 libxcb1 libx11-xcb1 lsb-release wget gnupg
    git lfs install
    if ! command -v clang++-21 >/dev/null 2>&1; then
        printf '%s\n' '==> Installing clang 21 from apt.llvm.org (the backend links libwebrtc)'
        local llvm_script
        llvm_script="$(mktemp)"
        curl -fsSL https://apt.llvm.org/llvm.sh -o "$llvm_script"
        sudo bash "$llvm_script" 21
        rm -f "$llvm_script"
    fi
}

configure_ipc_endpoint() {
    if [[ -z "${NW_IPC_ENDPOINT:-}" ]]; then
        export NW_IPC_ENDPOINT="unix:${XDG_RUNTIME_DIR:-/tmp}/neurowatch/control.sock"
    fi
    [[ "$NW_IPC_ENDPOINT" == unix:* ]] || fail 'local launcher requires a Unix NW_IPC_ENDPOINT (for example unix:/tmp/neurowatch/control.sock)'
    IPC_SOCKET="${NW_IPC_ENDPOINT#unix:}"
    [[ -n "$IPC_SOCKET" && "$IPC_SOCKET" != /control.sock ]] || fail 'NW_IPC_ENDPOINT must name a concrete Unix socket path'
}

cleanup_backend() {
    local status=$?
    local backend_status=0
    trap '' INT TERM
    if [[ -n "${UI_PID:-}" ]] && kill -0 "$UI_PID" 2>/dev/null; then
        kill -TERM "$UI_PID" 2>/dev/null || true
        wait "$UI_PID" 2>/dev/null || true
    fi
    if [[ -n "${BACKEND_PID:-}" ]] && kill -0 "$BACKEND_PID" 2>/dev/null; then
        printf '%s\n' '==> Stopping backend'
        kill "$BACKEND_PID" 2>/dev/null || true
        # The backend bounds its own shutdown (15 s); never hang here if it cannot.
        for _ in $(seq 1 300); do
            kill -0 "$BACKEND_PID" 2>/dev/null || break
            sleep 0.1
        done
        if kill -0 "$BACKEND_PID" 2>/dev/null; then
            printf '%s\n' '==> Backend did not stop within 30 s; killing it' >&2
            kill -KILL "$BACKEND_PID" 2>/dev/null || true
        fi
        wait "$BACKEND_PID" 2>/dev/null || true
    elif [[ -n "${BACKEND_PID:-}" ]]; then
        wait "$BACKEND_PID" 2>/dev/null || backend_status=$?
        if (( backend_status != 0 )); then
            printf 'Backend exited with status %s. Log retained: %s\n' "$backend_status" "$backend_log" >&2
            tail -n 40 "$backend_log" >&2 || true
            (( status != 0 )) || status=1
        fi
    fi
    if (( status == 0 )) && [[ -n "${backend_log:-}" ]]; then
        rm -f "$backend_log"
    fi
    trap - EXIT INT TERM
    exit "$status"
}

backend_ipc_is_ready() {
    [[ -S "$IPC_SOCKET" ]] || return 1
    "$VENV/bin/python" - "$IPC_SOCKET" <<'PY' >/dev/null 2>&1
import json
import socket
import sys

try:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(1.0)
        client.connect(sys.argv[1])
        client.sendall(b'{"jsonrpc":"2.0","id":1,"method":"system.status","params":{}}\n')
        response = json.loads(client.recv(65536))
except ConnectionRefusedError:
    sys.exit(1)
except OSError:
    sys.exit(2)
except (UnicodeDecodeError, json.JSONDecodeError):
    sys.exit(2)

if response.get("jsonrpc") != "2.0" or response.get("id") != 1:
    sys.exit(2)
if response.get("result", {}).get("status") != "running":
    sys.exit(2)
PY
}

prepare_ipc_socket() {
    if [[ -e "$IPC_SOCKET" || -L "$IPC_SOCKET" ]]; then
        local readiness_status
        if backend_ipc_is_ready; then
            fail "a Neurowatch backend is already accepting requests at $NW_IPC_ENDPOINT (the background service? stop it first: ./neurowatch service stop)"
        else
            readiness_status=$?
        fi
        if (( readiness_status != 1 )); then
            fail "IPC endpoint exists but did not return a healthy Neurowatch response: $IPC_SOCKET"
        fi
        [[ -S "$IPC_SOCKET" ]] || fail "IPC endpoint exists but is not a Unix socket: $IPC_SOCKET"
        rm -f "$IPC_SOCKET"
    fi
}

wait_for_socket() {
    local deadline=$((SECONDS + NW_STARTUP_TIMEOUT))
    while (( SECONDS < deadline )); do
        if backend_ipc_is_ready; then
            return 0
        fi
        if ! kill -0 "$BACKEND_PID" 2>/dev/null; then
            printf '%s\n' 'Backend exited before its IPC socket became ready.' >&2
            return 1
        fi
        sleep 0.1
    done
    printf 'Timed out after %ss waiting for backend IPC socket: %s\n' "$NW_STARTUP_TIMEOUT" "$IPC_SOCKET" >&2
    return 1
}

while (( $# )); do
    case "$1" in
        --bootstrap) mode="bootstrap"; ((MODE_FLAGS += 1)) ;;
        --check) mode="check"; ((MODE_FLAGS += 1)) ;;
        --system) INSTALL_SYSTEM=1 ;;
        --yes) CONFIRM_SYSTEM=1 ;;
        --headless) HEADLESS=1 ;;
        --no-autostart) NO_AUTOSTART=1 ;;
        --foreground) FOREGROUND=1 ;;
        service)
            mode="service"; ((MODE_FLAGS += 1)); shift
            SERVICE_ARGS=("$@")
            break ;;
        docker) shift; exec "$ROOT/scripts/docker.sh" "$@" ;;
        --help|-h) usage; exit 0 ;;
        -*) EXTRA_ARGS+=("$1") ;;  # a further --headless option (--link, --name)
        *)
            if (( ${#EXTRA_ARGS[@]} )); then
                EXTRA_ARGS+=("$1")  # that option's value
            else
                mode="command"; ((MODE_FLAGS += 1))
                COMMAND_ARGS=("$@")
                break
            fi ;;
    esac
    shift
done

(( MODE_FLAGS <= 1 )) || fail 'choose only one of --check, --bootstrap, service or a command'
if (( ${#EXTRA_ARGS[@]} )) && (( ! HEADLESS )); then
    fail "unknown argument: ${EXTRA_ARGS[0]} (run ./neurowatch.sh --help)"
fi
if (( HEADLESS )) && [[ -n "${mode:-}" ]]; then
    fail '--headless cannot be combined with --check, --bootstrap, service or a command'
fi
if (( NO_AUTOSTART )) && [[ -n "${mode:-}" ]]; then
    fail '--no-autostart applies to opening NeuroWatch (for the service: ./neurowatch.sh service start --no-autostart)'
fi
if (( FOREGROUND )) && (( ! HEADLESS )); then
    fail '--foreground requires --headless'
fi
if (( FOREGROUND && NO_AUTOSTART )); then
    fail '--foreground registers nothing already; drop --no-autostart'
fi

# The parser uses a lowercase temporary so the contract remains obvious and the
# runtime mode uses the conventional uppercase shell variable below.
MODE="${mode:-$MODE}"
[[ "${NW_STARTUP_TIMEOUT:-15}" =~ ^[1-9][0-9]*$ ]] || fail 'NW_STARTUP_TIMEOUT must be a positive integer (seconds)'
NW_STARTUP_TIMEOUT="${NW_STARTUP_TIMEOUT:-15}"

# Snap-packaged IDEs can leak their private glibc into integrated terminals.
# Host-built Rust/Python processes must use the host glibc instead.
sanitize_snap_library_paths
restore_vscode_host_environment
configure_local_gstreamer_va_runtime

case "$MODE" in
    service)
        (( INSTALL_SYSTEM == 0 && CONFIRM_SYSTEM == 0 )) || fail '--system/--yes require --bootstrap'
        [[ -x "$VENV/bin/python" ]] || fail 'Python environment missing; run ./neurowatch.sh --bootstrap'
        exec "$VENV/bin/python" -m neurowatch.service "${SERVICE_ARGS[@]}"
        ;;
    command)
        # `events` or `cloud`; neurowatch.headless rejects unknown ones.
        (( INSTALL_SYSTEM == 0 && CONFIRM_SYSTEM == 0 )) || fail '--system/--yes require --bootstrap'
        [[ -x "$VENV/bin/python" ]] || fail 'Python environment missing; run ./neurowatch.sh --bootstrap'
        configure_ipc_endpoint
        exec "$VENV/bin/python" -m neurowatch.headless "${COMMAND_ARGS[@]}"
        ;;
    check)
        (( INSTALL_SYSTEM == 0 && CONFIRM_SYSTEM == 0 )) || fail '--system/--yes require --bootstrap'
        printf '%s\n' '==> Checking local NeuroWatch prerequisites'
        check_dependencies
        printf '%s\n' "OK: cargo, uv, $CXX and GStreamer development packages are available."
        ;;
    bootstrap)
        (( INSTALL_SYSTEM == 0 || CONFIRM_SYSTEM == 1 )) || fail '--system requires explicit --yes confirmation'
        if (( INSTALL_SYSTEM )); then
            install_system
        fi
        sync_python
        printf '%s\n' 'Bootstrap complete. Start NeuroWatch with: ./neurowatch.sh'
        ;;
    start)
        (( INSTALL_SYSTEM == 0 && CONFIRM_SYSTEM == 0 )) || fail '--system/--yes require --bootstrap'
        check_dependencies
        sync_python
        load_portal_settings
        configure_ipc_endpoint
        if (( ! FOREGROUND )); then
            # Build here, where its output and errors are visible; the dev
            # service (started now, or running already) uses this build and
            # rebuilds by itself after later changes.
            printf '%s\n' '==> Building Rust backend'
            (cd "$BACKEND_WORKSPACE" && cargo build -p "$BACKEND_PACKAGE" --locked)
        fi
        if (( HEADLESS && ! FOREGROUND )); then
            printf '%s\n' '==> Headless: NeuroWatch runs as a background service; status follows'
            printf '%s\n' '    (Ctrl+C closes this view only; stop the service with: ./neurowatch.sh service stop)'
            monitor_args=(monitor --service)
            (( ! NO_AUTOSTART )) || monitor_args+=(--no-autostart)
            monitor_args+=("${EXTRA_ARGS[@]}")
            exec "$VENV/bin/python" -m neurowatch.headless "${monitor_args[@]}"
        fi
        if (( ! HEADLESS )); then
            if [[ "${NW_DEV_KEEP_SERVICE:-}" == 1 ]]; then
                printf '%s\n' '==> Opening NeuroWatch (NW_DEV_KEEP_SERVICE=1: the background service keeps running'
                printf '%s\n' '    after it closes; stop it from the tray icon or with: ./neurowatch.sh service stop)'
            else
                printf '%s\n' '==> Opening NeuroWatch (development: the background service stops when it closes;'
                printf '%s\n' '    NW_DEV_KEEP_SERVICE=1 keeps it running, with the tray, as installed builds do)'
            fi
            ui_args=()
            (( ! NO_AUTOSTART )) || ui_args+=(--no-autostart)
            exec "$VENV/bin/neurowatch-ui" "${ui_args[@]}"
        fi
        # --headless --foreground: the backend in this terminal, no service.
        prepare_ipc_socket
        # Headless from here on: tells the backend no desktop UI will read
        # frames, so it skips the per-camera SHM rings and RGB conversion.
        export NW_HEADLESS=1
        backend_log="$(mktemp "${TMPDIR:-/tmp}/neurowatch-backend.XXXXXX.log")"
        printf '%s\n' '==> Building Rust backend'
        cd "$BACKEND_WORKSPACE"
        cargo build -p "$BACKEND_PACKAGE" --locked
        printf '%s\n' '==> Starting Rust backend'
        cargo run -p "$BACKEND_PACKAGE" --locked >"$backend_log" 2>&1 &
        BACKEND_PID=$!
        trap cleanup_backend EXIT
        trap 'exit 130' INT
        trap 'exit 143' TERM
        if ! wait_for_socket; then
            printf '%s\n' "Backend log ($backend_log):" >&2
            tail -n 40 "$backend_log" >&2 || true
            exit 1
        fi
        printf '==> Backend ready at %s\n' "$NW_IPC_ENDPOINT"
        printf '==> Headless: no desktop UI; status follows (Ctrl+C stops NeuroWatch)\n'
        printf '==> Backend log: %s\n' "$backend_log"
        monitor_args=(monitor "${EXTRA_ARGS[@]}")
        # The status monitor runs in the foreground's place, so the same
        # supervision (traps, backend cleanup, retained crash log) applies.
        "$VENV/bin/python" -m neurowatch.headless "${monitor_args[@]}" &
        UI_PID=$!
        wait "$UI_PID"
        ;;
esac
