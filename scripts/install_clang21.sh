#!/usr/bin/env bash
# Install clang 21 (the backend links libwebrtc) from apt.llvm.org on Debian/Ubuntu.
#
# Replaces `curl https://apt.llvm.org/llvm.sh | sudo bash`, which ran an unverified
# script as root. This adds the apt.llvm.org repository instead, and only after
# the repository's signing key matches the fingerprint pinned below; apt then
# verifies every package list against that key.
#
#   scripts/install_clang21.sh                 # as a user with sudo, or as root
#   scripts/install_clang21.sh --verify-only   # check $NW_LLVM_KEY_FILE (a key file), install nothing
#
# If apt.llvm.org ever rotates its key, the check fails with the new fingerprint;
# confirm it at https://apt.llvm.org/ before changing LLVM_KEY_FINGERPRINT.
set -euo pipefail

LLVM_VERSION=21
# "Debian LLVM packages" signing key, https://apt.llvm.org/llvm-snapshot.gpg.key
LLVM_KEY_FINGERPRINT="6084F3CF814B57C1CF12EFD515CF4D18AF4F7421"
LLVM_KEY_URL="https://apt.llvm.org/llvm-snapshot.gpg.key"
KEYRING="/etc/apt/keyrings/apt.llvm.org.gpg"
SOURCE_LIST="/etc/apt/sources.list.d/apt-llvm-org.sources"

fail() { printf 'error: %s\n' "$*" >&2; exit 1; }

# The primary key's fingerprint is the first "fpr" record of `gpg --with-colons`.
key_fingerprint() {
    gpg --batch --show-keys --with-colons --fingerprint "$1" 2>/dev/null \
        | awk -F: '$1 == "fpr" { print $10; exit }' || true
}

verify_key() {
    local actual
    actual="$(key_fingerprint "$1")"
    [[ -n "$actual" ]] || fail "$1 is not a readable OpenPGP key"
    [[ "$actual" == "$LLVM_KEY_FINGERPRINT" ]] \
        || fail "the apt.llvm.org signing key has fingerprint $actual, not the pinned $LLVM_KEY_FINGERPRINT; nothing was installed"
}

as_root() {
    if [[ "$(id -u)" == 0 ]]; then "$@"; else sudo "$@"; fi
}

if [[ "${1:-}" == --verify-only ]]; then
    [[ -n "${NW_LLVM_KEY_FILE:-}" ]] || fail '--verify-only needs NW_LLVM_KEY_FILE'
    verify_key "$NW_LLVM_KEY_FILE"
    echo "key fingerprint matches"
    exit 0
fi

command -v gpg >/dev/null || fail 'gpg is not installed (apt-get install gnupg)'
command -v curl >/dev/null || fail 'curl is not installed'
# shellcheck disable=SC1091
codename="$(. /etc/os-release && echo "${VERSION_CODENAME:-}")"
[[ -n "$codename" ]] || fail 'cannot tell the distribution codename (/etc/os-release VERSION_CODENAME)'

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
curl -fsSL --retry 3 "$LLVM_KEY_URL" -o "$work/llvm.asc"
verify_key "$work/llvm.asc"
gpg --batch --dearmor --output "$work/llvm.gpg" "$work/llvm.asc"

as_root install -d -m 0755 /etc/apt/keyrings
as_root install -m 0644 "$work/llvm.gpg" "$KEYRING"
cat > "$work/llvm.sources" <<SOURCES
Types: deb
URIs: https://apt.llvm.org/$codename/
Suites: llvm-toolchain-$codename-$LLVM_VERSION
Components: main
Signed-By: $KEYRING
SOURCES
as_root install -m 0644 "$work/llvm.sources" "$SOURCE_LIST"
as_root apt-get update
as_root apt-get install -y \
    "clang-$LLVM_VERSION" "clang-tools-$LLVM_VERSION" "libclang-common-$LLVM_VERSION-dev" \
    "libclang-$LLVM_VERSION-dev" "lld-$LLVM_VERSION" "llvm-$LLVM_VERSION"
echo "clang-$LLVM_VERSION installed from apt.llvm.org (key $LLVM_KEY_FINGERPRINT)"
