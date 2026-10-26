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

# An update says so and removes what the old version left behind (here the
# same package again: the package manager runs its update scripts).
stale_meta=/opt/neurowatch/app/neurowatch-ui/_internal/neurowatch-0.0.1.dist-info
stale_plugin=/opt/neurowatch/gstreamer/lib/gstreamer-1.0/libgstneurowatchold.so
mkdir -p "$stale_meta"
printf 'Metadata-Version: 2.1\nName: neurowatch\nVersion: 0.0.1\n' > "$stale_meta/METADATA"
touch "$stale_plugin"
case "$1" in
    deb) apt-get install --reinstall -y /work/dist/release/*.deb 2>&1 | tee /tmp/update.log ;;
    rpm) dnf reinstall -y /work/dist/release/*.rpm 2>&1 | tee /tmp/update.log ;;
    arch) pacman -U --noconfirm /work/dist/release/*.pkg.tar.zst 2>&1 | tee /tmp/update.log ;;
esac
[[ ! -e "$stale_meta" && ! -e "$stale_plugin" ]] || { echo 'The update left old files behind' >&2; exit 1; }
[[ -x /opt/neurowatch/bin/nw-backend && -f /opt/neurowatch/.neurowatch-files ]]
# dnf may not echo scriptlet output; apt and pacman do.
if [[ "$1" != rpm ]]; then
    grep -q 'is installed: updating it to' /tmp/update.log
    grep -q 'left over from an earlier version' /tmp/update.log
fi

case "$1" in
    deb) apt-get remove -y neurowatch ;;
    rpm) dnf remove -y neurowatch ;;
    arch) pacman -R --noconfirm neurowatch ;;
esac
[[ ! -e /opt/neurowatch/bin/nw-backend ]]
