#!/usr/bin/env bash
# Development only: run NeuroWatch from this checkout.
#
#   scripts/dev.sh [--motion] [./neurowatch.sh args...]
#
# Builds and opens the desktop app (./neurowatch.sh; `--headless` runs the
# recorder headless). Any other arguments go to ./neurowatch.sh.
#   --motion   run the motion plugin from its own repository
#              (../Neurowatch-Motion, or NW_MOTION_DIR), built here, in place
#              of the copy in crates/nw-motion. Debug builds only.
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
    -h|--help) sed -n '2,10p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) args+=("$arg") ;;
  esac
done

# The recorder starts the motion plugin itself; debug builds run
# NW_MOTION_BIN in place of their own copy when it is set.
unset NW_MOTION_BIN
if [[ $motion == 1 ]]; then
  [[ -f "$MOTION/Cargo.toml" ]] || die "--motion: motion plugin not found at $MOTION (set NW_MOTION_DIR)"
  log "building the motion plugin in $MOTION"
  (cd "$MOTION" && cargo build --locked) || die "--motion: the motion plugin did not build"
  NW_MOTION_BIN="$(cd "$MOTION" && pwd)/target/debug/nw-motion"
  [[ -x "$NW_MOTION_BIN" ]] || die "--motion: no program at $NW_MOTION_BIN"
  export NW_MOTION_BIN
fi

exec "$ROOT/neurowatch.sh" "${args[@]}"
