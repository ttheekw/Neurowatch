# NeuroWatch package scripts: access to the graphics card and video devices from boot.
#
# Included in the .deb postinst/postrm, the .rpm %post/%postun and the Arch
# .INSTALL (scripts/package_release_linux.sh). POSIX sh, run as root.
#
# NeuroWatch's recorder runs as a per-user service that can start at boot,
# before anyone signs in. A desktop user gets the graphics card's render node
# (/dev/dri/renderD*, group `render` on Debian and Ubuntu) and V4L2 devices
# (/dev/videoN, group `video`: an ARM board's hardware video decoder, such as
# the Raspberry Pi's, and USB cameras) only once signed in (logind), so a
# recorder started earlier decodes on the CPU until it restarts, and cannot
# record a USB camera. Membership of these groups gives them from boot
# (upstream systemd makes render nodes accessible to everyone anyway). The
# memberships added here are recorded ("user group" lines), so removing the
# package takes back only those.

NEUROWATCH_GPU_STATE=/var/lib/neurowatch/device-group-users
NEUROWATCH_GPU_GROUPS="render video"

neurowatch_regular_users() {
    uid_min=$(awk '$1 == "UID_MIN" { print $2 }' /etc/login.defs 2>/dev/null || true)
    uid_max=$(awk '$1 == "UID_MAX" { print $2 }' /etc/login.defs 2>/dev/null || true)
    getent passwd | awk -F: -v min="${uid_min:-1000}" -v max="${uid_max:-60000}" \
        '$3 >= min && $3 <= max && $7 !~ /(nologin|false)$/ { print $1 }'
    # Whoever ran the installer (sudo, pkexec), even outside that range.
    if [ -n "${SUDO_USER:-}" ] && [ "${SUDO_USER}" != root ]; then
        echo "$SUDO_USER"
    fi
    if [ -n "${PKEXEC_UID:-}" ]; then
        getent passwd "$PKEXEC_UID" | cut -d: -f1
    fi
}

neurowatch_grant_gpu_access() {
    command -v usermod >/dev/null 2>&1 || return 0
    added=""
    for group in $NEUROWATCH_GPU_GROUPS; do
        getent group "$group" >/dev/null 2>&1 || continue
        for user in $(neurowatch_regular_users | sort -u); do
            id -nG "$user" 2>/dev/null | tr ' ' '\n' | grep -qx "$group" && continue
            if usermod -aG "$group" "$user" 2>/dev/null; then
                mkdir -p "$(dirname "$NEUROWATCH_GPU_STATE")"
                echo "$user $group" >> "$NEUROWATCH_GPU_STATE"
                added="$added $user ($group)"
            fi
        done
    done
    if [ -n "$added" ]; then
        echo "NeuroWatch: added${added}, so recording can use the graphics card and video"
        echo "devices from boot. This takes effect after signing out and in again, or a restart."
    fi
    return 0
}

neurowatch_revoke_gpu_access() {
    [ -f "$NEUROWATCH_GPU_STATE" ] || return 0
    if command -v gpasswd >/dev/null 2>&1; then
        sort -u "$NEUROWATCH_GPU_STATE" | while read -r user group; do
            [ -n "$user" ] && [ -n "$group" ] && gpasswd -d "$user" "$group" >/dev/null 2>&1 || true
        done
    fi
    rm -f "$NEUROWATCH_GPU_STATE"
    rmdir "$(dirname "$NEUROWATCH_GPU_STATE")" 2>/dev/null || true
    return 0
}
