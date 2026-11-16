#!/usr/bin/env bash
# Development only: run NeuroWatch from this checkout.
#
#   scripts/dev.sh [--motion] [./neurowatch.sh args...]
#
# Builds and opens the desktop app (./neurowatch.sh; `--headless` runs the
# recorder headless). Any other arguments go to ./neurowatch.sh.
#   --motion   also build and run the Motion plugin from its own repository
#              (../Neurowatch-Motion, or NW_MOTION_DIR) on its default
#              address, stopped when NeuroWatch exits. Add it once on the
#              Plugins page (Local socket) and approve its key.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MOTION="${NW_MOTION_DIR:-$ROOT/../Neurowatch-Motion}"
log() { printf '[dev] %s\n' "$*"; }
die() { printf '[dev] %s\n' "$*" >&2; exit 1; }

motion=0
args=()
for arg in "$@"; do
  case "$arg" in
    --motion) motion=1 ;;
    -h|--help) sed -n '2,11p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) args+=("$arg") ;;
  esac
done

if [[ $motion == 0 ]]; then
  exec "$ROOT/neurowatch.sh" "${args[@]}"
fi

# Motion is a separate plugin: NeuroWatch never starts it.
[[ -f "$MOTION/Cargo.toml" ]] || die "--motion: Motion not found at $MOTION (set NW_MOTION_DIR)"
log "building Motion in $MOTION"
(cd "$MOTION" && cargo build --locked) || die "--motion: Motion did not build"
program="$(cd "$MOTION" && pwd)/target/debug/nw-motion"
[[ -x "$program" ]] || die "--motion: no program at $program"
log "starting Motion on its default address"
"$program" --parent-pid "$$" &
motion_pid=$!
trap 'kill "$motion_pid" 2>/dev/null || true' EXIT
"$ROOT/neurowatch.sh" "${args[@]}"
