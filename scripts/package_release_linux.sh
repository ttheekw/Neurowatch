#!/usr/bin/env bash
# Build a Neurowatch Linux release: a portable PyInstaller + backend bundle,
# then native packages (.deb, .rpm, Arch .pkg.tar.zst on x86_64) from that bundle.
#
# Packages are built natively: --arch must match the host architecture.
# Nothing is published; outputs are written to --out-dir.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PKG_NAME="neurowatch"
PKG_RELEASE="1"
PKG_SUMMARY="Neurowatch NVR - professional video surveillance desktop application"
# Builds that add proprietary extensions name their terms too (NW_PACKAGE_LICENSE).
PKG_LICENSE="${NW_PACKAGE_LICENSE:-AGPL-3.0-or-later}"
PKG_URL="https://github.com/ttheekw/Neurowatch"
PKG_MAINTAINER="Neurowatch Developers <dev@neurowatch.invalid>"
INSTALL_PREFIX="/opt/neurowatch"

# Python, Qt, GStreamer and FFmpeg are bundled. Host libraries provide graphics,
# audio, TLS and GLib (including its system GIO modules).
DEB_DEPENDS=(
    libglib2.0-0
    liborc-0.4-0
    libopus0
    libpulse0
    'libasound2 | libasound2t64'
    libva2
    libva-drm2
    libdrm2
    libgudev-1.0-0
    libsoup-3.0-0
    libgnutls30
    libgmp10
    libstdc++6
    glib-networking
    libxv1
    libx11-xcb1
    libegl1
    libgl1
    libgles2
    libfontconfig1
    libxkbcommon-x11-0
    libxcb-cursor0
    libdbus-1-3
)
DEB_RECOMMENDS=(mesa-va-drivers)
RPM_REQUIRES=(
    glib2 orc opus pulseaudio-libs alsa-lib libva libdrm libgudev
    libsoup3 gnutls gmp libstdc++ glib-networking libXv libX11-xcb
    mesa-libEGL
    libglvnd-glx libglvnd-gles
    fontconfig
    libxkbcommon-x11
    xcb-util-cursor
    dbus-libs
)
RPM_RECOMMENDS=(mesa-va-drivers)
ARCH_DEPENDS=(
    glib2 orc opus libpulse alsa-lib libva libdrm libgudev
    libsoup3 gnutls gmp gcc-libs glib-networking libxv
    libglvnd
    fontconfig
    libxkbcommon-x11
    xcb-util-cursor
    dbus
)
ARCH_OPTDEPENDS=("mesa: AMD VA-API drivers" "intel-media-driver: Intel VA-API drivers")

usage() {
    cat <<'USAGE'
Usage: scripts/package_release_linux.sh --version VERSION --arch ARCH --format FORMAT[,FORMAT...]
                                        [--out-dir DIR] [--dry-run]

  --version   release version; a leading "v" is stripped (v1.2.3 -> 1.2.3)
  --arch      x86_64 | arm64 (aliases: amd64, aarch64); must match the host
  --format    deb | rpm | arch (x86_64 only) | portable; comma separated or repeated
  --out-dir   output directory (default: dist/release)
  --dry-run   validate arguments and print the artifact paths without building
USAGE
}

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

VERSION=""
ARCH=""
FORMATS=()
OUT_DIR="$ROOT/dist/release"
DRY_RUN=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version) VERSION="${2:-}"; shift 2 ;;
        --version=*) VERSION="${1#*=}"; shift ;;
        --arch) ARCH="${2:-}"; shift 2 ;;
        --arch=*) ARCH="${1#*=}"; shift ;;
        --format) IFS=, read -r -a parts <<< "${2:-}"; FORMATS+=("${parts[@]}"); shift 2 ;;
        --format=*) IFS=, read -r -a parts <<< "${1#*=}"; FORMATS+=("${parts[@]}"); shift ;;
        --out-dir) OUT_DIR="${2:-}"; shift 2 ;;
        --out-dir=*) OUT_DIR="${1#*=}"; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; fail "unknown argument: $1" ;;
    esac
done

[[ -n "$VERSION" ]] || { usage >&2; fail '--version is required'; }
[[ -n "$ARCH" ]] || { usage >&2; fail '--arch is required'; }
[[ ${#FORMATS[@]} -gt 0 ]] || { usage >&2; fail '--format is required'; }

VERSION="${VERSION#v}"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+(\.[0-9A-Za-z]+)*)?$ ]] \
    || fail "version must look like 1.2.3 or 1.2.3-rc.1, got: $VERSION"

case "$ARCH" in
    x86_64|amd64) ARCH="x86_64"; DEB_ARCH="amd64"; RPM_ARCH="x86_64"; PAC_ARCH="x86_64" ;;
    arm64|aarch64) ARCH="arm64"; DEB_ARCH="arm64"; RPM_ARCH="aarch64"; PAC_ARCH="" ;;
    *) fail "unsupported --arch: $ARCH (expected x86_64 or arm64)" ;;
esac

if [[ "$ARCH" == arm64 ]]; then
    MIN_GLIBC=2.39
    MIN_GLIB=2.80
else
    MIN_GLIBC=2.35
    MIN_GLIB=2.72
fi
DEB_DEPENDS+=("libc6 (>= $MIN_GLIBC)" "libglib2.0-0 (>= $MIN_GLIB)")
RPM_REQUIRES+=("glibc >= $MIN_GLIBC" "glib2 >= $MIN_GLIB")
ARCH_DEPENDS+=("glibc>=$MIN_GLIBC" "glib2>=$MIN_GLIB")

for format in "${FORMATS[@]}"; do
    case "$format" in
        deb|rpm|arch|portable) ;;
        *) fail "unsupported --format: $format (expected deb, rpm, arch, or portable)" ;;
    esac
    # Arch Linux is x86_64 only; Arch Linux ARM is a separate distribution
    # with no official image to test a package in.
    [[ "$format" != arch || -n "$PAC_ARCH" ]] \
        || fail "--format arch is x86_64 only (Arch Linux ARM is not a supported target)"
done

# Pre-release suffixes must sort before the final release in every format:
# deb/rpm use "~" (1.2.3~rc.1); pacman's vercmp treats a trailing alpha segment
# as a pre-release and pkgver may not contain "-" (1.2.3rc.1).
DEB_VERSION="${VERSION/-/\~}-${PKG_RELEASE}"
RPM_VERSION="${VERSION/-/\~}"
PAC_VERSION="${VERSION/-/}"

artifact_path() {
    case "$1" in
        deb) printf '%s/%s_%s_%s.deb\n' "$OUT_DIR" "$PKG_NAME" "$DEB_VERSION" "$DEB_ARCH" ;;
        rpm) printf '%s/%s-%s-%s.%s.rpm\n' "$OUT_DIR" "$PKG_NAME" "$RPM_VERSION" "$PKG_RELEASE" "$RPM_ARCH" ;;
        arch) printf '%s/%s-%s-%s-%s.pkg.tar.zst\n' "$OUT_DIR" "$PKG_NAME" "$PAC_VERSION" "$PKG_RELEASE" "$PAC_ARCH" ;;
        portable) printf '%s/%s-%s-linux-%s.tar.gz\n' "$OUT_DIR" "$PKG_NAME" "$VERSION" "$ARCH" ;;
    esac
}

if [[ $DRY_RUN -eq 1 ]]; then
    for format in "${FORMATS[@]}"; do
        artifact_path "$format"
    done
    exit 0
fi

HOST_ARCH="$(uname -m)"
case "$HOST_ARCH" in
    x86_64|amd64) HOST_ARCH="x86_64" ;;
    aarch64|arm64) HOST_ARCH="arm64" ;;
esac
[[ "$HOST_ARCH" == "$ARCH" ]] \
    || fail "packages are built natively; host is $HOST_ARCH but --arch is $ARCH"

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

require_command cargo
for format in "${FORMATS[@]}"; do
    case "$format" in
        deb) require_command dpkg-deb ;;
        rpm) require_command rpmbuild ;;
        arch) require_command bsdtar; require_command zstd ;;
    esac
done

PYTHON="${PYTHON:-python3}"
require_command "$PYTHON"
"$PYTHON" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 12) else 1)' \
    || fail "$PYTHON must be Python 3.12 or newer (see requires-python in pyproject.toml)"

WORK_DIR="$ROOT/build/release-linux-$ARCH"
BUNDLE_DIR="$WORK_DIR/bundle/$PKG_NAME"
VENV="$WORK_DIR/venv"
rm -rf "$WORK_DIR"
mkdir -p "$BUNDLE_DIR/bin" "$BUNDLE_DIR/app" "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"

echo "[1/5] Building the matched GStreamer runtime"
bash "$ROOT/scripts/build_release_gstreamer.sh" "$WORK_DIR/gstreamer-sdk" "$WORK_DIR/media-build"
export PKG_CONFIG_PATH="$WORK_DIR/gstreamer-sdk/lib/pkgconfig"
export LD_LIBRARY_PATH="$WORK_DIR/gstreamer-sdk/lib"
mkdir -p "$BUNDLE_DIR/gstreamer/lib"
cp -a "$WORK_DIR/gstreamer-sdk/"{bin,libexec,share,VERSIONS.txt} "$BUNDLE_DIR/gstreamer/"
cp -a "$WORK_DIR/gstreamer-sdk/lib/"*.so* "$BUNDLE_DIR/gstreamer/lib/"
cp -a "$WORK_DIR/gstreamer-sdk/lib/gstreamer-1.0" "$BUNDLE_DIR/gstreamer/lib/"

echo "[2/5] Building backend (release)"
# NW_BACKEND_MANIFEST / NW_BACKEND_PACKAGE: build the `nw-backend` binary from
# another Cargo workspace (one that adds a backend extension to this one).
BACKEND_MANIFEST="${NW_BACKEND_MANIFEST:-$ROOT/Cargo.toml}"
BACKEND_TARGET="${CARGO_TARGET_DIR:-$(dirname "$BACKEND_MANIFEST")/target}"
cargo build --manifest-path "$BACKEND_MANIFEST" --release --locked -p "${NW_BACKEND_PACKAGE:-nw-backend}"
install -m 0755 "$BACKEND_TARGET/release/nw-backend" "$BUNDLE_DIR/bin/nw-backend"
# The licence and notice ship with every artifact, and the terms of any
# extensions built in (NW_EXTRA_LICENSE_FILE).
install -m 0644 "$ROOT/LICENSE" "$BUNDLE_DIR/LICENSE"
install -m 0644 "$ROOT/NOTICE" "$BUNDLE_DIR/NOTICE"
if [[ -n "${NW_EXTRA_LICENSE_FILE:-}" ]]; then
    install -m 0644 "$NW_EXTRA_LICENSE_FILE" "$BUNDLE_DIR/LICENSE-EXTENSIONS"
fi

echo "[3/5] Packaging UI from pyproject.toml with PyInstaller"
"$PYTHON" -m venv "$VENV"
"$VENV/bin/python" -m pip install --upgrade pip
"$VENV/bin/python" -m pip install pyinstaller==6.16.0 uv==0.11.6
"$VENV/bin/uv" export --project "$ROOT" --locked --no-dev --no-emit-project \
    --format requirements-txt --output-file "$WORK_DIR/requirements.txt"
"$VENV/bin/python" -m pip install --require-hashes -r "$WORK_DIR/requirements.txt"
"$VENV/bin/python" -m pip install --no-deps "$ROOT"
# NW_UI_EXTENSION_DIR / _DIST / _MODULE: bundle a UI extension (see
# ui/neurowatch/extensions.py) with its entry-point metadata, so it is found.
extension_args=()
if [[ -n "${NW_UI_EXTENSION_DIR:-}" ]]; then
    [[ -n "${NW_UI_EXTENSION_DIST:-}" && -n "${NW_UI_EXTENSION_MODULE:-}" ]] \
        || fail 'NW_UI_EXTENSION_DIR needs NW_UI_EXTENSION_DIST and NW_UI_EXTENSION_MODULE'
    "$VENV/bin/python" -m pip install --no-deps "$NW_UI_EXTENSION_DIR"
    extension_args=(--copy-metadata "$NW_UI_EXTENSION_DIST" --collect-submodules "$NW_UI_EXTENSION_MODULE")
fi
"$VENV/bin/pyinstaller" \
    --noconfirm \
    --clean \
    --name neurowatch-ui \
    --distpath "$WORK_DIR/pyinstaller-dist" \
    --workpath "$WORK_DIR/pyinstaller-build" \
    --specpath "$WORK_DIR" \
    --collect-data neurowatch \
    --copy-metadata neurowatch \
    "${extension_args[@]}" \
    "$ROOT/scripts/entrypoints/neurowatch_ui.py"
cp -a "$WORK_DIR/pyinstaller-dist/neurowatch-ui" "$BUNDLE_DIR/app/neurowatch-ui"

cat > "$BUNDLE_DIR/neurowatch" <<'LAUNCH'
#!/usr/bin/env bash
# Neurowatch launcher: opens the desktop UI. The backend (bin/nw-backend) runs
# as a background service that keeps recording after the UI closes; the UI
# starts it if it is not running and registers it, with the tray icon, to
# start at login (a systemd user service). `neurowatch --no-autostart`
# registers nothing; `neurowatch service status|start|stop|restart|install|
# uninstall` controls the service.
set -euo pipefail

BASE="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"
exec "$BASE/app/neurowatch-ui/neurowatch-ui" "$@"
LAUNCH
chmod 0755 "$BUNDLE_DIR/neurowatch"

cat > "$WORK_DIR/neurowatch.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Version=1.0
Name=Neurowatch
GenericName=Network Video Recorder
Comment=$PKG_SUMMARY
Exec=neurowatch
Icon=camera-video
Terminal=false
Categories=AudioVideo;Video;Security;
Keywords=NVR;CCTV;camera;surveillance;
StartupNotify=true
DESKTOP
cp "$WORK_DIR/neurowatch.desktop" "$BUNDLE_DIR/neurowatch.desktop"

# Shared install tree used by every native format.
STAGE="$WORK_DIR/stage"
mkdir -p "$STAGE$INSTALL_PREFIX" "$STAGE/usr/bin" "$STAGE/usr/share/applications"
cp -a "$BUNDLE_DIR/." "$STAGE$INSTALL_PREFIX/"
rm -f "$STAGE$INSTALL_PREFIX/neurowatch.desktop"
ln -s "$INSTALL_PREFIX/neurowatch" "$STAGE/usr/bin/neurowatch"
install -m 0644 "$WORK_DIR/neurowatch.desktop" "$STAGE/usr/share/applications/neurowatch.desktop"
# Licence files live in the distro's licence directory (rpm, Arch); build_deb
# moves them to /usr/share/doc/$PKG_NAME, the Debian location.
LICENSE_DIR="/usr/share/licenses/$PKG_NAME"
rm -f "$STAGE$INSTALL_PREFIX/LICENSE" "$STAGE$INSTALL_PREFIX/NOTICE"
install -d -m 0755 "$STAGE$LICENSE_DIR"
install -m 0644 "$ROOT/LICENSE" "$STAGE$LICENSE_DIR/LICENSE"
install -m 0644 "$ROOT/NOTICE" "$STAGE$LICENSE_DIR/NOTICE"
INSTALLED_SIZE_KB="$(du -sk "$STAGE" | cut -f1)"

join_by() {
    local separator="$1"
    shift
    local first="$1"
    shift
    printf '%s' "$first" "${@/#/$separator}"
}

build_portable() {
    local output
    output="$(artifact_path portable)"
    tar -C "$WORK_DIR/bundle" -czf "$output" "$PKG_NAME"
    echo "  portable: $output"
}

build_deb() {
    local output root
    output="$(artifact_path deb)"
    root="$WORK_DIR/deb-root"
    rm -rf "$root"
    cp -a "$STAGE" "$root"
    mkdir -p "$root/usr/share/doc/$PKG_NAME"
    mv "$root$LICENSE_DIR/LICENSE" "$root$LICENSE_DIR/NOTICE" "$root/usr/share/doc/$PKG_NAME/"
    rmdir "$root$LICENSE_DIR"
    rmdir --ignore-fail-on-non-empty "$root/usr/share/licenses"
    mkdir -p "$root/DEBIAN"
    chmod 0755 "$root/DEBIAN"
    cat > "$root/DEBIAN/control" <<CONTROL
Package: $PKG_NAME
Version: $DEB_VERSION
Architecture: $DEB_ARCH
Maintainer: $PKG_MAINTAINER
Installed-Size: $INSTALLED_SIZE_KB
Depends: $(join_by ', ' "${DEB_DEPENDS[@]}")
Recommends: $(join_by ', ' "${DEB_RECOMMENDS[@]}")
Section: video
Priority: optional
Homepage: $PKG_URL
Description: $PKG_SUMMARY
 Desktop network video recorder with a Rust/GStreamer backend and a
 PySide6 user interface. Installed under $INSTALL_PREFIX.
CONTROL
    dpkg-deb --root-owner-group --build "$root" "$output" >/dev/null
    echo "  deb: $output"
}

build_rpm() {
    local output topdir spec
    output="$(artifact_path rpm)"
    topdir="$WORK_DIR/rpmbuild"
    spec="$topdir/SPECS/$PKG_NAME.spec"
    rm -rf "$topdir"
    mkdir -p "$topdir"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS}
    cat > "$spec" <<SPEC
Name:           $PKG_NAME
Version:        $RPM_VERSION
Release:        $PKG_RELEASE
Summary:        $PKG_SUMMARY
License:        $PKG_LICENSE
URL:            $PKG_URL
BuildArch:      $RPM_ARCH
AutoReqProv:    no
Requires:       $(join_by ' ' "${RPM_REQUIRES[@]}")
Recommends:     $(join_by ' ' "${RPM_RECOMMENDS[@]}")

# The bundle is prebuilt by PyInstaller; do not strip or rewrite it.
%global __os_install_post %{nil}
%global debug_package %{nil}
%global _build_id_links none

%description
Desktop network video recorder with a Rust/GStreamer backend and a
PySide6 user interface. Installed under $INSTALL_PREFIX.

%install
mkdir -p %{buildroot}
cp -a "$STAGE/." %{buildroot}/

%files
%defattr(-,root,root,-)
$INSTALL_PREFIX
/usr/bin/neurowatch
/usr/share/applications/neurowatch.desktop
%dir $LICENSE_DIR
%license $LICENSE_DIR/LICENSE
%license $LICENSE_DIR/NOTICE
SPEC
    rpmbuild -bb \
        --define "_topdir $topdir" \
        --target "$RPM_ARCH" \
        "$spec" >/dev/null
    cp "$topdir/RPMS/$RPM_ARCH/$PKG_NAME-$RPM_VERSION-$PKG_RELEASE.$RPM_ARCH.rpm" "$output"
    echo "  rpm: $output"
}

build_arch() {
    local output root builddate dep
    output="$(artifact_path arch)"
    root="$WORK_DIR/arch-root"
    rm -rf "$root"
    cp -a "$STAGE" "$root"
    builddate="${SOURCE_DATE_EPOCH:-$(date +%s)}"
    {
        echo "# Generated by scripts/package_release_linux.sh"
        echo "pkgname = $PKG_NAME"
        echo "pkgbase = $PKG_NAME"
        echo "pkgver = $PAC_VERSION-$PKG_RELEASE"
        echo "pkgdesc = $PKG_SUMMARY"
        echo "url = $PKG_URL"
        echo "builddate = $builddate"
        echo "packager = $PKG_MAINTAINER"
        echo "size = $((INSTALLED_SIZE_KB * 1024))"
        echo "arch = $PAC_ARCH"
        echo "license = $PKG_LICENSE"
        for dep in "${ARCH_DEPENDS[@]}"; do
            echo "depend = $dep"
        done
        for dep in "${ARCH_OPTDEPENDS[@]}"; do
            echo "optdepend = $dep"
        done
    } > "$root/.PKGINFO"
    # Same metadata layout makepkg produces: .PKGINFO, .MTREE, then the payload.
    (
        cd "$root"
        LANG=C bsdtar -czf .MTREE --format=mtree --uid 0 --gid 0 --uname root --gname root \
            --options='!all,use-set,type,uid,gid,mode,time,size,md5,sha256,link' \
            .PKGINFO opt usr
        LANG=C bsdtar -cf - --uid 0 --gid 0 --uname root --gname root \
            .PKGINFO .MTREE opt usr | zstd -q -19 -T0 -o "$output" -f
    )
    echo "  arch: $output"
}

echo "[4/5] Emitting packages into $OUT_DIR"
for format in "${FORMATS[@]}"; do
    "build_$format"
done

echo "[5/5] Done"
