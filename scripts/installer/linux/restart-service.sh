# NeuroWatch package scripts: after an update, every running NeuroWatch
# service runs the new version.
#
# Included in the .deb postinst, the .rpm posttrans scriptlet and the Arch
# .INSTALL post_upgrade (scripts/package_release_linux.sh), after the new
# files are in place and the old version's leftovers are gone. POSIX sh, run
# as root. No percent signs: rpm would expand them as macros.
#
# The service is per user (a systemd user unit, or a detached
# `nw-backend --supervise`), so the package manager does not know it: a
# backend still running the replaced binary kept recording with the old
# version until the next login, and could load the new version's GStreamer
# plugins into the old program. Each running backend from this install is
# restarted as its own user by the new version's `neurowatch service restart
# --no-autostart`, with the backend's own settings (its IPC endpoint, config
# and state folders): it stops the old backend and starts the new one in the
# same mode, the same way (systemd unit or detached), and changes nothing
# about what starts at login. Nothing here can fail the update.

neurowatch_restart_services() {
    nw_root="${1:-${NEUROWATCH_ROOT:-/opt/neurowatch}}"
    nw_backend="$nw_root/bin/nw-backend"
    nw_launcher="$nw_root/neurowatch"
    [ -x "$nw_launcher" ] || return 0
    nw_self=$(id -u)
    nw_seen=" "
    nw_work=$(mktemp -d) || return 0
    for nw_proc in /proc/[0-9]*; do
        nw_exe=$(readlink "$nw_proc/exe" 2>/dev/null) || continue
        case "$nw_exe" in
            "$nw_backend" | "$nw_backend (deleted)") ;;
            *) continue ;;
        esac
        tr '\000' '\n' < "$nw_proc/environ" > "$nw_work/env" 2>/dev/null || continue
        # A supervised backend is restarted with its supervisor.
        grep -q '^NW_SUPERVISED=1$' "$nw_work/env" && continue
        nw_uid=$(awk '/^Uid:/ { print $2 }' "$nw_proc/status" 2>/dev/null) || continue
        [ -n "$nw_uid" ] || continue
        nw_endpoint=$(sed -n 's/^NW_IPC_ENDPOINT=//p' "$nw_work/env")
        nw_key="$nw_uid:$nw_endpoint"
        case "$nw_seen" in
            *" $nw_key "*) continue ;;
        esac
        nw_seen="$nw_seen$nw_key "
        nw_user=$(getent passwd "$nw_uid" | cut -d: -f1)
        [ -n "$nw_user" ] || continue
        echo "NeuroWatch: restarting the recorder of $nw_user with the new version..."
        if (
            set --
            while IFS= read -r nw_line; do
                case "$nw_line" in
                    NW_SUPERVISED=* | NW_RESTARTS=* | NW_PREVIOUS_EXIT=* | NW_SUPERVISOR_PID=* | NW_HEADLESS=*) ;;
                    HOME=* | USER=* | LOGNAME=* | LANG=* | LC_ALL=* | XDG_*=* | DBUS_SESSION_BUS_ADDRESS=* \
                        | DISPLAY=* | WAYLAND_DISPLAY=* | XAUTHORITY=* | NW_*=* | RUST_LOG=* | GST_DEBUG=*)
                        set -- "$@" "$nw_line" ;;
                esac
            done < "$nw_work/env"
            # `systemctl --user` needs the user's runtime folder and bus.
            if [ -d "/run/user/$nw_uid" ]; then
                grep -q '^XDG_RUNTIME_DIR=' "$nw_work/env" || set -- "$@" "XDG_RUNTIME_DIR=/run/user/$nw_uid"
                if [ -S "/run/user/$nw_uid/bus" ] && ! grep -q '^DBUS_SESSION_BUS_ADDRESS=' "$nw_work/env"; then
                    set -- "$@" "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$nw_uid/bus"
                fi
            fi
            set -- "$@" "PATH=/usr/local/bin:/usr/bin:/bin"
            if [ "$nw_uid" = "$nw_self" ]; then
                timeout 180 env -i "$@" "$nw_launcher" service restart --no-autostart
            else
                timeout 180 runuser -u "$nw_user" -- env -i "$@" "$nw_launcher" service restart --no-autostart
            fi
        ) > "$nw_work/out" 2>&1; then
            echo "NeuroWatch: the recorder of $nw_user runs the new version."
        else
            echo "NeuroWatch: could not restart the recorder of $nw_user; it runs the old version until it is"
            echo "restarted (sign out and in, or run: neurowatch service restart). Its output:"
            sed 's/^/  /' "$nw_work/out" | tail -n 5
        fi
    done
    rm -rf "$nw_work"
    return 0
}
