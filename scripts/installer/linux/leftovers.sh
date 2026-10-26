# NeuroWatch package scripts: an update leaves nothing of the old version behind.
#
# Included in the .deb preinst/postinst, the .rpm pre/posttrans scriptlets and
# the Arch .INSTALL (scripts/package_release_linux.sh). POSIX sh, run as root.
# No percent signs: rpm would expand them as macros.
#
# Every build lists its own program files in <root>/.neurowatch-files, one
# relative path per line, folders ending in "/". After an update, anything
# else under the program folders (app, bin, gstreamer) was left by an earlier
# version: a stale neurowatch-<old>.dist-info made the window show the old
# version, and old libraries or GStreamer plugins could be loaded. Settings,
# cameras and recordings live in each user's home folder and are never touched.

NEUROWATCH_ROOT=/opt/neurowatch

neurowatch_update_notice() {
    # $1: the version installed now (may be empty), $2: the new version.
    echo "NeuroWatch ${1:+$1 }is installed: updating it to $2."
    echo "The old version's program files are replaced and any it leaves behind are removed."
    echo "Settings, cameras and recordings are kept."
}

neurowatch_remove_leftovers() {
    nw_root="${1:-$NEUROWATCH_ROOT}"
    [ -f "$nw_root/.neurowatch-files" ] || return 0
    nw_work=$(mktemp -d) || return 0
    LC_ALL=C sort -u "$nw_root/.neurowatch-files" > "$nw_work/listed"
    (
        cd "$nw_root" || exit 0
        for nw_dir in app bin gstreamer; do
            [ -d "$nw_dir" ] || continue
            find "$nw_dir" -type d -exec sh -c 'for d in "$@"; do echo "$d/"; done' sh {} +
            find "$nw_dir" ! -type d -print
        done
    ) | LC_ALL=C sort -u > "$nw_work/present"
    LC_ALL=C comm -23 "$nw_work/present" "$nw_work/listed" > "$nw_work/extra"
    nw_files=0
    while IFS= read -r nw_path; do
        case "$nw_path" in
            */) ;;
            *) rm -f -- "$nw_root/$nw_path" && nw_files=$((nw_files + 1)) ;;
        esac
    done < "$nw_work/extra"
    # Deepest first, so a folder is empty once what it held is gone.
    LC_ALL=C sort -r "$nw_work/extra" | while IFS= read -r nw_path; do
        case "$nw_path" in
            */) rmdir -- "$nw_root/$nw_path" 2>/dev/null || true ;;
        esac
    done
    rm -rf "$nw_work"
    if [ "$nw_files" -gt 0 ]; then
        echo "NeuroWatch: removed $nw_files file(s) left over from an earlier version."
    fi
    return 0
}
