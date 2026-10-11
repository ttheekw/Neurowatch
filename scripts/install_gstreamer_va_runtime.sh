#!/usr/bin/env bash
# Build the fixed GStreamer VA component for Ubuntu 24.04 without replacing
# distribution packages. The launcher automatically selects the result.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="1.24.13"
SOURCE_BASE="https://gstreamer.freedesktop.org/src/gst-plugins-bad"
RUNTIME="$ROOT/.cache/gstreamer-$VERSION"

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

for command in apt-get curl dpkg-architecture dpkg-deb gst-inspect-1.0 \
    pkg-config sha256sum tar uv; do
    command -v "$command" >/dev/null 2>&1 || fail "missing required command: $command"
done
[[ "$(uname -s)" == Linux ]] || fail 'this installer supports Linux only'

multiarch="$(dpkg-architecture -qDEB_HOST_MULTIARCH)"
plugin="$RUNTIME/lib/$multiarch/gstreamer-1.0/libgstva.so"
if [[ -f "$plugin" ]]; then
    installed_version="$(
        LD_LIBRARY_PATH="$RUNTIME/lib/$multiarch" \
        GST_PLUGIN_PATH_1_0="$RUNTIME/lib/$multiarch/gstreamer-1.0" \
        GST_REGISTRY_1_0="$ROOT/.cache/gstreamer-registry-$VERSION.bin" \
        gst-inspect-1.0 va 2>/dev/null | sed -n 's/^  Version[[:space:]]*//p' | head -n 1
    )"
    [[ "$installed_version" == "$VERSION" ]] \
        || fail "existing private VA runtime reports ${installed_version:-no version}"
    printf 'GStreamer VA runtime %s is already installed at %s\n' "$VERSION" "$RUNTIME"
    exit 0
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/neurowatch-gstreamer.XXXXXX")"
stage="$ROOT/.cache/gstreamer-$VERSION.tmp.$$"
cleanup() {
    rm -rf "$work" "$stage"
}
trap cleanup EXIT

printf '==> Downloading GStreamer gst-plugins-bad %s\n' "$VERSION"
curl -fL --retry 3 -o "$work/gst-plugins-bad-$VERSION.tar.xz" \
    "$SOURCE_BASE/gst-plugins-bad-$VERSION.tar.xz"
curl -fL --retry 3 -o "$work/gst-plugins-bad-$VERSION.tar.xz.sha256sum" \
    "$SOURCE_BASE/gst-plugins-bad-$VERSION.tar.xz.sha256sum"
(
    cd "$work"
    sha256sum -c "gst-plugins-bad-$VERSION.tar.xz.sha256sum"
)

printf '%s\n' '==> Downloading the matching Ubuntu libva development headers'
(
    cd "$work"
    apt-get download libva-dev
)
libva_deb="$(find "$work" -maxdepth 1 -name 'libva-dev_*.deb' -type f -print -quit)"
[[ -n "$libva_deb" ]] || fail 'apt did not download libva-dev'
dpkg-deb -x "$libva_deb" "$work/sysroot"

pc_dir="$work/sysroot/usr/lib/$multiarch/pkgconfig"
for pc in "$pc_dir"/libva*.pc; do
    sed -i "s#^prefix=/usr\$#prefix=$work/sysroot/usr#" "$pc"
done
for library in libva.so.2 libva-drm.so.2; do
    system_library="/usr/lib/$multiarch/$library"
    [[ -e "$system_library" ]] || fail "missing runtime library: $system_library"
    ln -s "$system_library" "$work/sysroot/usr/lib/$multiarch/$library"
done

tar -C "$work" -xf "$work/gst-plugins-bad-$VERSION.tar.xz"
UV_CACHE_DIR="$work/uv-cache" uv venv "$work/venv"
UV_CACHE_DIR="$work/uv-cache" uv pip install --python "$work/venv/bin/python" \
    meson==1.3.2 ninja==1.11.1.4

printf '%s\n' '==> Building the updated VA plugin and its support libraries'
PATH="$work/venv/bin:/usr/bin:/bin" PKG_CONFIG_PATH="$pc_dir" \
    "$work/venv/bin/meson" setup "$work/build" \
    "$work/gst-plugins-bad-$VERSION" \
    --prefix="$stage" --libdir="lib/$multiarch" \
    -Dauto_features=disabled -Dva=enabled -Dudev=enabled \
    -Dexamples=disabled -Dtests=disabled -Ddoc=disabled -Dintrospection=disabled
PATH="$work/venv/bin:/usr/bin:/bin" \
    "$work/venv/bin/meson" compile -C "$work/build"
PATH="$work/venv/bin:/usr/bin:/bin" \
    "$work/venv/bin/meson" install -C "$work/build"

test_plugin="$stage/lib/$multiarch/gstreamer-1.0/libgstva.so"
[[ -f "$test_plugin" ]] || fail 'the VA plugin was not installed'
reported_version="$(
    LD_LIBRARY_PATH="$stage/lib/$multiarch" \
    GST_PLUGIN_PATH_1_0="$stage/lib/$multiarch/gstreamer-1.0" \
    GST_REGISTRY_1_0="$work/registry.bin" \
    gst-inspect-1.0 va 2>/dev/null | sed -n 's/^  Version[[:space:]]*//p' | head -n 1
)"
[[ "$reported_version" == "$VERSION" ]] \
    || fail "built plugin reports ${reported_version:-no version}, expected $VERSION"

cp "$work/gst-plugins-bad-$VERSION.tar.xz.sha256sum" "$stage/SOURCE.sha256sum"
printf '%s\n' "$SOURCE_BASE/gst-plugins-bad-$VERSION.tar.xz" > "$stage/SOURCE.url"
mv "$stage" "$RUNTIME"
printf 'Installed GStreamer VA runtime %s at %s\n' "$VERSION" "$RUNTIME"
printf '%s\n' 'Restart NeuroWatch to activate it.'
