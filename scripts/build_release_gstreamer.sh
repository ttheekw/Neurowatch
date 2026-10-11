#!/usr/bin/env bash
# Build a matched release SDK without changing the host's GStreamer install.
# Ubuntu build prerequisites are declared in .github/workflows/release.yml.
# The prefix must be a new directory; it is also used by cargo's pkg-config.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREFIX="${1:?usage: build_release_gstreamer.sh PREFIX WORK_DIR}"
WORK="${2:?usage: build_release_gstreamer.sh PREFIX WORK_DIR}"
GST_VERSION=1.24.13
FFMPEG_VERSION=6.1.5
[[ ! -e "$PREFIX" ]] || { echo "prefix already exists: $PREFIX" >&2; exit 1; }
mkdir -p "$PREFIX" "$WORK/sources"
PREFIX="$(realpath "$PREFIX")"
WORK="$(realpath "$WORK")"
PYTHON="${PYTHON:-python3}"
if [[ -n "${MEDIA_BUILD_TOOLS:-}" ]]; then
    export PATH="$MEDIA_BUILD_TOOLS:$PATH"
    [[ "$(meson --version)" == 1.3.2 ]] || { echo 'Meson 1.3.2 required' >&2; exit 1; }
else
    "$PYTHON" -m venv "$WORK/tools"
    "$WORK/tools/bin/python" -m pip install meson==1.3.2 ninja==1.11.1.4
    export PATH="$WORK/tools/bin:$PATH"
fi
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
export LD_LIBRARY_PATH="$PREFIX/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
JOBS="${CMAKE_BUILD_PARALLEL_LEVEL:-$(nproc)}"

for project in gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-libav ffmpeg; do
    version="$GST_VERSION"
    url="https://gstreamer.freedesktop.org/src/$project"
    if [[ "$project" == ffmpeg ]]; then
        version="$FFMPEG_VERSION"
        url=https://ffmpeg.org/releases
    fi
    archive="$project-$version.tar.xz"
    if [[ ! -f "$WORK/sources/$archive" ]]; then
        curl -fL --retry 3 "$url/$archive" -o "$WORK/sources/$archive"
    fi
done
(cd "$WORK/sources" && sha256sum -c "$ROOT/scripts/packaging/media-sources.sha256")
for archive in "$WORK/sources/"*.tar.xz; do tar -xf "$archive" -C "$WORK"; done

# Build LGPL FFmpeg libraries ourselves: distro FFmpeg builds can pull in
# optional GPL codecs and distro-specific SONAMEs. No external codec libraries.
(
    cd "$WORK/ffmpeg-$FFMPEG_VERSION"
    ./configure --prefix="$PREFIX" --libdir="$PREFIX/lib" \
        --enable-shared --disable-static --disable-programs --disable-doc \
        --disable-autodetect --disable-everything --disable-network \
        --enable-decoder=h264,hevc,mjpeg,aac,opus,pcm_alaw,pcm_mulaw,pcm_s16le,pcm_s16be \
        --enable-encoder=aac --enable-parser=h264,hevc,mjpeg,aac,opus \
        --enable-pic
    make -j "$JOBS"
    make install
)

build_gst() {
    local project="$1"
    shift
    meson setup "$WORK/build-$project" "$WORK/$project-$GST_VERSION" \
        --prefix="$PREFIX" --libdir=lib --buildtype=release --wrap-mode=nofallback \
        -Dauto_features=disabled "$@"
    meson compile -C "$WORK/build-$project" -j "$JOBS"
    meson install -C "$WORK/build-$project"
}
build_gst gstreamer -Dtools=enabled
build_gst gst-plugins-base \
    -Dapp=enabled -Daudioconvert=enabled -Daudioresample=enabled \
    -Daudiotestsrc=enabled -Dplayback=enabled -Dtypefind=enabled \
    -Dvideoconvertscale=enabled -Dvideotestsrc=enabled -Dvolume=enabled \
    -Dtcp=enabled -Dgio=enabled -Dopus=enabled -Dalsa=enabled \
    -Dx11=enabled -Dxshm=enabled -Dxvideo=enabled -Dorc=enabled \
    -Dgl=enabled -Dgl_api=opengl,gles2 -Dgl_platform=egl,glx -Dgl_winsys=x11
build_gst gst-plugins-good \
    -Daudiofx=enabled -Daudioparsers=enabled -Dautodetect=enabled \
    -Disomp4=enabled -Dlaw=enabled -Dmatroska=enabled -Dmultifile=enabled \
    -Dmultipart=enabled -Drtp=enabled -Drtpmanager=enabled -Drtsp=enabled \
    -Dudp=enabled -Dflv=enabled -Djpeg=enabled -Dpulse=enabled \
    -Dsoup=enabled -Dsoup-version=3 -Dorc=enabled -Dv4l2=enabled -Dv4l2-gudev=enabled
build_gst gst-plugins-bad \
    -Dvideoparsers=enabled -Djpegformat=enabled -Dopus=enabled \
    -Dva=enabled -Ddrm=enabled -Dudev=enabled -Dnvcodec=enabled -Drtmp=enabled
build_gst gst-libav

# Ubuntu's JPEG SONAME differs from Fedora's. Ship that one dependency;
# GLib, TLS/audio client libraries and graphics drivers remain host libraries.
jpeg="$(ldd "$PREFIX/lib/gstreamer-1.0/libgstjpeg.so" | awk '/libjpeg.so/ {print $3}')"
[[ -f "$jpeg" ]] || { echo 'JPEG runtime dependency missing' >&2; exit 1; }
cp -L "$jpeg" "$PREFIX/lib/$(basename "$jpeg")"
rtmp="$(ldd "$PREFIX/lib/gstreamer-1.0/libgstrtmp.so" | awk '/librtmp.so/ {print $3}')"
[[ -f "$rtmp" ]] || { echo 'RTMP runtime dependency missing' >&2; exit 1; }
cp -L "$rtmp" "$PREFIX/lib/$(basename "$rtmp")"
mkdir -p "$PREFIX/share/licenses/neurowatch-media"
for project in gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-libav; do
    cp "$WORK/$project-$GST_VERSION/COPYING" "$PREFIX/share/licenses/neurowatch-media/$project.txt"
done
cp "$WORK/ffmpeg-$FFMPEG_VERSION/COPYING.LGPLv2.1" "$PREFIX/share/licenses/neurowatch-media/ffmpeg.txt"
cp /usr/share/doc/libjpeg-turbo8/copyright "$PREFIX/share/licenses/neurowatch-media/libjpeg-turbo.txt"
cp /usr/share/doc/librtmp1/copyright "$PREFIX/share/licenses/neurowatch-media/librtmp.txt"
cp "$ROOT/scripts/packaging/media-sources.sha256" "$PREFIX/share/licenses/neurowatch-media/"
printf 'GStreamer %s\nFFmpeg %s (LGPL, no external codecs)\n' "$GST_VERSION" "$FFMPEG_VERSION" > "$PREFIX/VERSIONS.txt"

# Keep the SDK intact until cargo has linked; packaging copies only runtime files.
export GST_PLUGIN_SYSTEM_PATH_1_0="$PREFIX/lib/gstreamer-1.0"
export GST_PLUGIN_PATH_1_0="$PREFIX/lib/gstreamer-1.0"
export GST_PLUGIN_SCANNER_1_0="$PREFIX/libexec/gstreamer-1.0/gst-plugin-scanner"
export GST_REGISTRY_1_0="$WORK/registry.bin"
"$PREFIX/bin/gst-inspect-1.0" --version
"$PREFIX/bin/gst-inspect-1.0" va
echo "Release GStreamer SDK ready: $PREFIX"
