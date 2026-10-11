#!/usr/bin/env bash
# CI only: run inside an ephemeral container with the repo mounted at /work.
set -euo pipefail
[[ -f /.dockerenv ]] || { echo 'Run this check inside Docker' >&2; exit 1; }
case "${1:?expected deb, rpm or arch}" in
    deb)
        apt-get update
        DEBIAN_FRONTEND=noninteractive apt-get install -y python3 /work/dist/release/*.deb
        ;;
    rpm) dnf install -y python3 /work/dist/release/*.rpm ;;
    arch) pacman -Syu --noconfirm python; pacman -U --noconfirm /work/dist/release/*.pkg.tar.zst ;;
    *) exit 2 ;;
esac
# GStreamer itself must come entirely from our bundle.
python3 /work/scripts/smoke_release.py /opt/neurowatch
case "$1" in
    deb) apt-get remove -y neurowatch ;;
    rpm) dnf remove -y neurowatch ;;
    arch) pacman -R --noconfirm neurowatch ;;
esac
[[ ! -e /opt/neurowatch/bin/nw-backend ]]
