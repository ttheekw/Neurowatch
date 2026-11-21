#!/usr/bin/env bash
# Build the GStreamer release packages bundle (scripts/build_release_gstreamer.sh)
# into .cache/gstreamer-sdk-<version>, so a source checkout can run its backend
# on it: NW_GSTREAMER_SDK=1 ./neurowatch.sh. The system's GStreamer is left alone.
#
# By default the build runs in a container of this system's Ubuntu release
# (Docker), so its packages are installed there, not here; the checkout is
# mounted at the same path, which keeps the SDK's own paths right. --native
# builds on this machine instead (it needs the packages listed in
# .github/workflows/release.yml, package-linux).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="$(sed -n 's/^GST_VERSION=//p' "$ROOT/scripts/build_release_gstreamer.sh")"
SDK="$ROOT/.cache/gstreamer-sdk-$VERSION"
WORK="$ROOT/.cache/gstreamer-sdk-$VERSION.build"

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

mode=docker
case "${1:-}" in
    "") ;;
    --native) mode=native ;;
    -h|--help)
        sed -n '2,10p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *) fail "unknown argument: $1 (expected --native or nothing)" ;;
esac
[[ -n "$VERSION" ]] || fail 'no GST_VERSION in scripts/build_release_gstreamer.sh'
[[ "$(uname -s)" == Linux ]] || fail 'Linux only'

if [[ -d "$SDK/lib/gstreamer-1.0" ]]; then
    printf 'GStreamer %s is already built at %s\n' "$VERSION" "$SDK"
    exit 0
fi
# Left over from an interrupted build.
rm -rf "$SDK" "$WORK"
mkdir -p "$ROOT/.cache"

if [[ "$mode" == native ]]; then
    bash "$ROOT/scripts/build_release_gstreamer.sh" "$SDK" "$WORK"
else
    command -v docker >/dev/null 2>&1 || fail 'docker not found (or use --native)'
    # In a subshell: os-release's own VERSION must not replace ours.
    # shellcheck source=/dev/null
    read -r os_id os_version < <(. /etc/os-release && printf '%s %s\n' "${ID:-}" "${VERSION_ID:-}")
    # The release build copies Ubuntu packages' licence files.
    [[ "$os_id" == ubuntu && -n "$os_version" ]] \
        || fail "the container build matches an Ubuntu host (this is ${os_id:-unknown}); use --native"
    # The release workflow's GStreamer build dependencies (package-linux).
    packages="build-essential pkg-config nasm flex bison curl ca-certificates python3-venv
        libglib2.0-dev liborc-0.4-dev libopus-dev libpulse-dev libasound2-dev
        libva-dev libdrm-dev libgudev-1.0-dev libjpeg-dev librtmp-dev
        libx11-dev libx11-xcb-dev libxext-dev libxv-dev libgl-dev libegl-dev libgles-dev"
    # The Ubuntu releases the project builds on are pinned by digest (the
    # multi-architecture index); another host version uses its moving tag.
    base_image="ubuntu:$os_version"
    case "$os_version" in
        22.04) base_image="$base_image@sha256:f64b2ffc2f10ab8f8505aa56b2664bd846cbb9df87c7e2532e30f7667021c0e6" ;;
        24.04) base_image="$base_image@sha256:9c7f200c07e3d4a03e7249d2c16bf66a51120fb876aa2242cf31451412543289" ;;
    esac
    docker run --rm \
        -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" -e SDK="$SDK" -e WORK="$WORK" \
        -e PACKAGES="$packages" \
        -v "$ROOT:$ROOT:ro" -v "$ROOT/.cache:$ROOT/.cache" \
        "$base_image" bash -c '
            set -euo pipefail
            trap "chown -R \"\$HOST_UID:\$HOST_GID\" \"\$SDK\" \"\$WORK\" 2>/dev/null || true" EXIT
            export DEBIAN_FRONTEND=noninteractive
            apt-get update -qq
            # shellcheck disable=SC2086 # one word per package
            apt-get install -y -qq $PACKAGES >/dev/null
            bash "'"$ROOT"'/scripts/build_release_gstreamer.sh" "$SDK" "$WORK"
        '
fi
rm -rf "$WORK"
printf '\nGStreamer %s ready at %s\nRun: NW_GSTREAMER_SDK=1 ./neurowatch.sh\n' "$VERSION" "$SDK"
