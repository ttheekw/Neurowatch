#!/usr/bin/env bash
# Exercise a NeuroWatch container image (docker/Dockerfile) with no cameras:
#   scripts/docker_smoke.sh IMAGE headless
#   scripts/docker_smoke.sh IMAGE gui      (needs DISPLAY: an X server, e.g. Xvfb)
# NW_SMOKE_DOCKER_ARGS adds `docker run` arguments (e.g. --network host for an
# X server on TCP: DISPLAY=localhost:99).
# The backend must come up healthy as a non-root user writing to the mounted
# volumes, report the expected mode (and edition, with NW_EXPECT_EDITION), the
# gui image must open its window on the X server, and `docker stop` must end
# the container cleanly. Release CI runs it on every image before publishing.
set -euo pipefail

IMAGE="${1:?usage: scripts/docker_smoke.sh IMAGE headless|gui}"
VARIANT="${2:?usage: scripts/docker_smoke.sh IMAGE headless|gui}"
[[ "$VARIANT" == headless || "$VARIANT" == gui ]] || { echo "unknown variant: $VARIANT" >&2; exit 2; }
NAME="nw-smoke-$VARIANT-$$"
WORK="$(mktemp -d)"
mkdir -p "$WORK/data" "$WORK/recordings"

cleanup() {
    docker rm --force "$NAME" >/dev/null 2>&1 || true
    rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
    echo "FAIL: $*" >&2
    echo "--- container log ---" >&2
    docker logs "$NAME" >&2 2>&1 || true
    exit 1
}

running() {
    [[ "$(docker container inspect --format '{{.State.Running}}' "$NAME" 2>/dev/null)" == true ]]
}

in_container() {
    docker exec --user "$(id -u):$(id -g)" "$NAME" "$@"
}

args=(
    --detach --name "$NAME"
    --user "$(id -u):$(id -g)"
    --volume "$WORK/data:/data"
    --volume "$WORK/recordings:/recordings"
)
if [[ "$VARIANT" == gui ]]; then
    [[ -n "${DISPLAY:-}" ]] || { echo 'gui needs DISPLAY (start Xvfb first)' >&2; exit 2; }
    args+=(--shm-size 512m --env "DISPLAY=$DISPLAY" --env QT_QPA_PLATFORM=xcb
        --volume /tmp/.X11-unix:/tmp/.X11-unix:ro)
fi
read -r -a extra <<<"${NW_SMOKE_DOCKER_ARGS:-}"
echo "Starting $IMAGE ($VARIANT)"
docker run "${args[@]}" "${extra[@]}" "$IMAGE" "$VARIANT" >/dev/null

echo "Waiting for the backend"
for _ in $(seq 1 90); do
    running || fail "the container exited"
    if in_container neurowatch-docker health >/dev/null 2>&1; then
        break
    fi
    sleep 1
done
in_container neurowatch-docker health >/dev/null 2>&1 || fail "the backend did not answer within 90 s"

status_json="$(in_container neurowatch-docker service status --json)" || fail "service status failed"
echo "$status_json"
STATUS_JSON="$status_json" VARIANT="$VARIANT" python3 - <<'PY'
import json
import os

info = json.loads(os.environ["STATUS_JSON"])
status = info["status"]
assert info["running"] is True, info
assert status["headless"] is (os.environ["VARIANT"] == "headless"), status
expected = os.environ.get("NW_EXPECT_EDITION")
if expected:
    assert status.get("edition") == expected, status
print("backend:", status.get("version"), "edition", status.get("edition"),
      "headless", status["headless"])
PY

# Everything it keeps lands in the volumes, owned by the user it runs as.
[[ -f "$WORK/data/share/neurowatch/neurowatch.db" ]] || fail "no database in the data volume"
[[ -L "$WORK/data/Videos/neurowatch" ]] || fail "recordings do not default to /recordings"
[[ "$(stat -c %u "$WORK/data/share/neurowatch/neurowatch.db")" == "$(id -u)" ]] \
    || fail "the database is not owned by the calling user"
in_container neurowatch-docker cloud status || fail "cloud status failed"

if [[ "$VARIANT" == gui ]]; then
    echo "Waiting for the window"
    found=0
    for _ in $(seq 1 60); do
        running || fail "the container exited before its window opened"
        if command -v xwininfo >/dev/null; then
            if xwininfo -root -tree 2>/dev/null | grep -qi 'neurowatch'; then
                found=1
                break
            fi
        elif in_container bash -c 'grep -qa neurowatch-ui /proc/[0-9]*/cmdline' 2>/dev/null; then
            found=1  # no xwininfo here: the UI process at least runs
            break
        fi
        sleep 1
    done
    (( found )) || fail "no NeuroWatch window on $DISPLAY"
    sleep 5
    running || fail "the container exited after opening its window"
    echo "Window open on $DISPLAY"
fi

echo "Stopping"
docker stop --time 30 "$NAME" >/dev/null
code="$(docker container inspect --format '{{.State.ExitCode}}' "$NAME")"
[[ "$code" == 0 ]] || fail "the container exited with $code after docker stop"
if docker logs "$NAME" 2>&1 | grep -E "panicked at|Could not load the Qt platform plugin"; then
    fail "errors in the log"
fi
echo "OK: $IMAGE ($VARIANT)"
