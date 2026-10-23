#!/usr/bin/env bash
# Build and run the NeuroWatch container images (docker/Dockerfile, docs/docker.md).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_URL="https://github.com/ttheekw/Neurowatch"
REGISTRY_IMAGE="${NW_DOCKER_REGISTRY_IMAGE:-ghcr.io/ttheekw/neurowatch}"
DATA_ROOT="${NW_DOCKER_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/neurowatch-docker}"

usage() {
    cat <<'USAGE'
Usage: scripts/docker.sh headless [RUN OPTIONS] [-- DOCKER RUN ARGS]
       scripts/docker.sh gui [--wayland] [RUN OPTIONS] [-- DOCKER RUN ARGS]
       scripts/docker.sh stop | logs | status | shell [headless|gui]
       scripts/docker.sh exec [headless|gui] COMMAND...
       scripts/docker.sh build [headless|gui|all] [--deb FILE | --release TAG] [--tag NAME]

  headless   start NeuroWatch without a window in the background (container
             "neurowatch"); it restarts with Docker and records until stopped
  gui        start NeuroWatch with its desktop window on this computer's
             display (container "neurowatch-gui"); closing the window stops it
  stop       stop the container (default: whichever runs)
  logs       follow its log
  status     is it running, and the backend's status
  exec       run a NeuroWatch command in it, for example:
               scripts/docker.sh exec cloud link CODE --name "Front office"
               scripts/docker.sh exec cloud status
               scripts/docker.sh exec events --follow
  shell      open a shell in it
  build      build images locally from a release .deb: dist/release/*.deb for
             this machine's architecture if there is one, else --deb FILE (for
             example a NeuroWatch Unlimited package), else the package of
             --release TAG (default: the latest release) downloaded from GitHub.
             Tags neurowatch:headless and neurowatch:gui unless --tag is given
             (one image only).

Run options:
  --image IMAGE       image to run (default: ghcr.io/ttheekw/neurowatch:latest
                      for headless, ghcr.io/ttheekw/neurowatch:gui for gui;
                      after `build`: --image neurowatch:headless or neurowatch:gui)
  --data DIR          settings, database, cloud link and logs
                      (default: ~/.local/share/neurowatch-docker/data)
  --recordings DIR    where recordings go
                      (default: ~/.local/share/neurowatch-docker/recordings)
  --no-gpu            do not pass /dev/dri (VA-API decoding, OpenGL video tiles)
  --nvidia            pass NVIDIA GPUs (needs the NVIDIA Container Toolkit)
  --shm-size SIZE     shared memory for the live video frames (gui, default 1g)
  --wayland           gui: use the Wayland socket instead of X11/XWayland
  -- ARGS             further `docker run` arguments, e.g. -v /mnt/nas:/mnt/nas
                      to record to another drive

Containers use the host's network (camera discovery, the camera event port
7681) and run as you, so files in the data and recordings folders are yours.
Only one of the two runs at a time: they share the data folder.
USAGE
}

fail() { printf 'error: %s\n' "$*" >&2; exit 1; }
need_docker() { command -v docker >/dev/null || fail 'docker is not installed (https://docs.docker.com/engine/install/)'; }

container_name() {
    case "$1" in
        headless) echo neurowatch ;;
        gui) echo neurowatch-gui ;;
        *) fail "unknown variant: $1 (expected headless or gui)" ;;
    esac
}

container_state() {
    docker container inspect --format '{{.State.Status}}' "$1" 2>/dev/null || true
}

# The running container, from an optional headless|gui argument.
running_container() {
    local variant="${1:-}" name
    if [[ -n "$variant" ]]; then
        container_name "$variant"
        return
    fi
    for name in neurowatch neurowatch-gui; do
        if [[ "$(container_state "$name")" == running ]]; then
            echo "$name"
            return
        fi
    done
    fail 'no NeuroWatch container is running (scripts/docker.sh headless or gui)'
}

deb_arch() {
    case "$(uname -m)" in
        x86_64|amd64) echo amd64 ;;
        aarch64|arm64) echo arm64 ;;
        *) fail "unsupported architecture: $(uname -m) (images exist for x86_64 and arm64)" ;;
    esac
}

cmd_build() {
    local variant=all deb="" release="" tags=()
    while (( $# )); do
        case "$1" in
            headless|gui|all) variant="$1" ;;
            --deb) deb="${2:?--deb needs a file}"; shift ;;
            --release) release="${2:?--release needs a tag, e.g. v0.1.1}"; shift ;;
            --tag) tags+=("${2:?--tag needs an image name}"); shift ;;
            -h|--help) usage; exit 0 ;;
            *) fail "unknown build argument: $1" ;;
        esac
        shift
    done
    [[ -z "$deb" || -z "$release" ]] || fail 'choose --deb or --release, not both'
    [[ "$variant" != all || ${#tags[@]} -eq 0 ]] || fail '--tag names one image: build headless or gui'
    need_docker

    local arch context
    arch="$(deb_arch)"
    context="$(mktemp -d)"
    # shellcheck disable=SC2064  # expand now: $context is local
    trap "rm -rf '$context'" EXIT
    if [[ -z "$deb" && -z "$release" ]]; then
        local local_debs=()
        shopt -s nullglob
        local_debs=("$ROOT"/dist/release/neurowatch*_"$arch".deb)
        shopt -u nullglob
        if (( ${#local_debs[@]} == 1 )); then
            deb="${local_debs[0]}"
        elif (( ${#local_debs[@]} > 1 )); then
            fail "several packages in dist/release; choose one with --deb: ${local_debs[*]}"
        fi
    fi
    if [[ -n "$deb" ]]; then
        [[ -f "$deb" ]] || fail "no such package: $deb"
        cp "$deb" "$context/neurowatch.deb"
    else
        local url
        if [[ -n "$release" ]]; then
            url="$REPO_URL/releases/download/$release/neurowatch_$arch.deb"
        else
            url="$REPO_URL/releases/latest/download/neurowatch_$arch.deb"
        fi
        echo "Downloading $url"
        curl -fL --retry 3 -o "$context/neurowatch.deb" "$url"
        deb="$url"
    fi
    cp "$ROOT/docker/Dockerfile" "$ROOT/docker/entrypoint.sh" "$context/"

    local labels=(--label "org.opencontainers.image.revision=$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unknown)")
    if command -v dpkg-deb >/dev/null; then
        labels+=(--label "org.opencontainers.image.version=$(dpkg-deb --field "$context/neurowatch.deb" Version)")
    fi
    local targets=("$variant")
    [[ "$variant" == all ]] && targets=(headless gui)
    local target names
    for target in "${targets[@]}"; do
        names=("${tags[@]:-neurowatch:$target}")
        local tag_args=()
        for name in "${names[@]}"; do tag_args+=(--tag "$name"); done
        echo "Building the $target image from $deb"
        docker build --pull --target "$target" "${labels[@]}" "${tag_args[@]}" "$context"
        echo "Built ${names[*]}"
    done
    if [[ ${#tags[@]} -eq 0 ]]; then
        echo
        for target in "${targets[@]}"; do
            echo "Run it: scripts/docker.sh $target --image neurowatch:$target"
        done
    fi
}

cmd_run() {
    local variant="$1"
    shift
    local image="" data="$DATA_ROOT/data" recordings="$DATA_ROOT/recordings"
    local gpu=1 nvidia=0 wayland=0 shm=1g extra=()
    while (( $# )); do
        case "$1" in
            --image) image="${2:?--image needs a name}"; shift ;;
            --data) data="${2:?--data needs a folder}"; shift ;;
            --recordings) recordings="${2:?--recordings needs a folder}"; shift ;;
            --no-gpu) gpu=0 ;;
            --nvidia) nvidia=1 ;;
            --shm-size) shm="${2:?--shm-size needs a size, e.g. 1g}"; shift ;;
            --wayland) [[ "$variant" == gui ]] || fail '--wayland is for gui'; wayland=1 ;;
            --) shift; extra=("$@"); break ;;
            -h|--help) usage; exit 0 ;;
            *) fail "unknown argument: $1 (scripts/docker.sh --help)" ;;
        esac
        shift
    done
    need_docker
    if [[ -z "$image" ]]; then
        image="$REGISTRY_IMAGE:latest"
        [[ "$variant" == gui ]] && image="$REGISTRY_IMAGE:gui"
    fi

    local name other
    name="$(container_name "$variant")"
    other="$(container_name "$([[ "$variant" == gui ]] && echo headless || echo gui)")"
    [[ "$(container_state "$other")" != running ]] \
        || fail "$other is running and uses the same data; stop it first: scripts/docker.sh stop"
    case "$(container_state "$name")" in
        running) fail "$name is already running (scripts/docker.sh stop $variant)" ;;
        "") ;;
        *) docker rm "$name" >/dev/null ;;  # stopped: its settings are in the data folder
    esac

    mkdir -p "$data" "$recordings"
    local args=(
        --detach --name "$name"
        --network host
        --user "$(id -u):$(id -g)"
        --stop-timeout 30
        --volume "$(cd "$data" && pwd):/data"
        --volume "$(cd "$recordings" && pwd):/recordings"
    )
    if [[ "$variant" == headless ]]; then
        args+=(--restart unless-stopped)
    fi

    if (( gpu )) && [[ -d /dev/dri ]]; then
        args+=(--device /dev/dri)
        local node gid gids=()
        for node in /dev/dri/*; do
            [[ -c "$node" ]] || continue
            gid="$(stat -c %g "$node")"
            [[ " ${gids[*]} " == *" $gid "* ]] || gids+=("$gid")
        done
        for gid in "${gids[@]}"; do args+=(--group-add "$gid"); done
    fi
    (( nvidia )) && args+=(--gpus all --env NVIDIA_DRIVER_CAPABILITIES=all)

    if [[ "$variant" == gui ]]; then
        args+=(--shm-size "$shm")
        local runtime="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
        if (( wayland )); then
            local socket="${WAYLAND_DISPLAY:-wayland-0}"
            [[ "$socket" == /* ]] || socket="$runtime/$socket"
            [[ -S "$socket" ]] || fail "no Wayland socket at $socket"
            args+=(--volume "$socket:/run/host/wayland-0"
                --env WAYLAND_DISPLAY=/run/host/wayland-0 --env QT_QPA_PLATFORM=wayland)
        else
            [[ -n "${DISPLAY:-}" ]] || fail 'DISPLAY is not set: run this from your desktop session (or use --wayland)'
            args+=(--env "DISPLAY=$DISPLAY" --env QT_QPA_PLATFORM=xcb
                --volume /tmp/.X11-unix:/tmp/.X11-unix:ro)
            local xauth="${XAUTHORITY:-$HOME/.Xauthority}"
            if [[ -f "$xauth" ]]; then
                args+=(--volume "$xauth:/run/host/Xauthority:ro" --env XAUTHORITY=/run/host/Xauthority)
            fi
        fi
        # Sound (live listening, playback) through PulseAudio or PipeWire's Pulse socket.
        if [[ -S "$runtime/pulse/native" ]]; then
            args+=(--volume "$runtime/pulse/native:/run/host/pulse/native"
                --env PULSE_SERVER=unix:/run/host/pulse/native)
            local cookie="${XDG_CONFIG_HOME:-$HOME/.config}/pulse/cookie"
            if [[ -f "$cookie" ]]; then
                args+=(--volume "$cookie:/run/host/pulse/cookie:ro" --env PULSE_COOKIE=/run/host/pulse/cookie)
            fi
        fi
    fi

    docker run "${args[@]}" "${extra[@]}" "$image" "$variant" >/dev/null
    echo "NeuroWatch ($variant) is starting in container $name ($image)."
    echo "  data:       $data"
    echo "  recordings: $recordings"
    echo "  log:        scripts/docker.sh logs"
    echo "  stop:       scripts/docker.sh stop"
    if [[ "$variant" == headless ]]; then
        echo "  link to NeuroWatch Cloud: scripts/docker.sh exec cloud link CODE --name NAME"
    fi
}

docker_exec() {
    local name="$1"
    shift
    local tty=(-i)
    [[ -t 0 && -t 1 ]] && tty=(-it)
    docker exec "${tty[@]}" --user "$(id -u):$(id -g)" "$name" "$@"
}

variant_arg() {
    [[ "${1:-}" == headless || "${1:-}" == gui ]]
}

main() {
    local command="${1:-}"
    (( $# )) && shift
    case "$command" in
        headless|gui) cmd_run "$command" "$@" ;;
        build) cmd_build "$@" ;;
        stop)
            need_docker
            local name
            name="$(running_container "${1:-}")"
            echo "Stopping $name (recordings are finished first; up to 30 s)"
            # A stopped container stays stopped (--restart unless-stopped).
            docker stop --time 30 "$name" >/dev/null
            ;;
        logs) need_docker; docker logs --follow --tail 200 "$(running_container "${1:-}")" ;;
        status)
            need_docker
            local name
            name="$(running_container "${1:-}")"
            docker ps --filter "name=^${name}\$" --format 'Container {{.Names}}: {{.Status}} ({{.Image}})'
            docker_exec "$name" neurowatch-docker service status
            ;;
        exec)
            need_docker
            local name
            if variant_arg "${1:-}"; then
                name="$(container_name "$1")"
                shift
            else
                name="$(running_container)"
            fi
            (( $# )) || fail 'exec needs a command, e.g. cloud status'
            docker_exec "$name" neurowatch-docker "$@"
            ;;
        shell) need_docker; docker_exec "$(running_container "${1:-}")" bash ;;
        -h|--help|help|"") usage ;;
        *) fail "unknown command: $command (scripts/docker.sh --help)" ;;
    esac
}

main "$@"
