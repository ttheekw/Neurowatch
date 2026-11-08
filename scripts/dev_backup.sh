#!/usr/bin/env bash
# Development only: try Cloud Backup end to end on this machine, with the web
# portal, the backup ingest and this desktop app.
#
#   scripts/dev_backup.sh [--trust-browser] [./neurowatch.sh args...]
#       prepare (once), then start the portal, the ingest and the desktop app
#   scripts/dev_backup.sh --setup [--trust-browser]
#       only prepare; then run the pieces yourself
#
# What it prepares, once, in the portal's `.dev-backup/` folder (git-ignored):
#   - the portal's ticket-signing key: tickets tell the ingest which cameras
#     a recorder may back up; the ingest holds only the public half;
#   - a private development CA and a certificate for `localhost` from it:
#     the ingest's TLS (debug-build recorders are told to trust that CA);
#   - the token the ingest sends with its reports to the portal;
#   - `portal.env`: the portal's backup settings (free plans, the ingest on
#     127.0.0.1, a quick test package as well as the usual ones).
# Archived footage goes to the portal's local object store (the clip-export
# bucket, prefix `backup/`), as it would in production.
#
# Then:
#   1. the portal starts with those settings (NeuroWatch's dev_cloud.sh);
#   2. the ingest starts in the background once the portal answers
#      (../Neurowatch-Backup-Ingest/scripts/dev.sh; log in $TMPDIR);
#   3. this desktop app runs in the foreground. Ctrl-C stops all three.
#
# --trust-browser adds the development CA to Chrome/Chromium's certificate
# store (~/.pki/nssdb), so the portal's player can fetch instant footage from
# https://localhost:<port>. Without it, open that address once in the browser
# and accept the warning. Undo: certutil -d sql:$HOME/.pki/nssdb -D -n "NeuroWatch dev backup CA"
#
# Repositories: NW_WEBAPP_DIR (default ../Neurowatch-webapp) and
# NW_INGEST_DIR (default ../Neurowatch-Backup-Ingest). The ingest's port:
# NW_BACKUP_INGEST_PORT (default 7443), fixed by the first setup.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WEBAPP="${NW_WEBAPP_DIR:-$ROOT/../Neurowatch-webapp}"
INGEST="${NW_INGEST_DIR:-$ROOT/../Neurowatch-Backup-Ingest}"
DEV="$WEBAPP/.dev-backup"
LOG="${TMPDIR:-/tmp}/neurowatch-backup-ingest.log"
CA_NAME="NeuroWatch dev backup CA"

log() { printf '\033[1;32m[dev-backup]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[dev-backup]\033[0m %s\n' "$*" >&2; exit 1; }

setup_only=0
trust=0
args=()
for arg in "$@"; do
  case "$arg" in
    --setup) setup_only=1 ;;
    --trust-browser) trust=1 ;;
    -h|--help) sed -n '2,34p' "$0"; exit 0 ;;
    *) args+=("$arg") ;;
  esac
done

[[ -d "$WEBAPP" ]] || die "web portal not found at $WEBAPP (set NW_WEBAPP_DIR)"
[[ -x "$INGEST/scripts/dev.sh" ]] || die "backup ingest not found at $INGEST (set NW_INGEST_DIR)"
command -v openssl >/dev/null || die "openssl is required"

# --- prepare (once) ------------------------------------------------------------
hex32() { tail -c 32 | od -An -tx1 | tr -d ' \n'; }

setup() {
  mkdir -p "$DEV"
  chmod 700 "$DEV"
  local fresh=0
  if [[ ! -s "$DEV/portal-signing.key" ]]; then
    log "making the portal's ticket-signing key"
    openssl genpkey -algorithm ed25519 -out "$DEV/portal-signing.key" 2>/dev/null
    openssl pkey -in "$DEV/portal-signing.key" -pubout -outform DER | hex32 >"$DEV/portal-keys.txt"
    echo >>"$DEV/portal-keys.txt"
    fresh=1
  fi
  if [[ ! -s "$DEV/ca.pem" || ! -s "$DEV/ingest.pem" ]]; then
    log "making a development CA and a certificate for localhost"
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 825 \
      -keyout "$DEV/ca.key" -out "$DEV/ca.pem" -subj "/CN=$CA_NAME" \
      -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
      -keyout "$DEV/ingest.key" -out "$DEV/ingest.csr" -subj "/CN=localhost" 2>/dev/null
    openssl x509 -req -in "$DEV/ingest.csr" -CA "$DEV/ca.pem" -CAkey "$DEV/ca.key" -CAcreateserial \
      -days 825 -out "$DEV/ingest.pem" -extfile <(printf '%s\n' \
        "subjectAltName=DNS:localhost,IP:127.0.0.1" "basicConstraints=critical,CA:FALSE" \
        "keyUsage=critical,digitalSignature" "extendedKeyUsage=serverAuth") 2>/dev/null
    rm -f "$DEV/ingest.csr"
    fresh=1
  fi
  if [[ ! -s "$DEV/ingest-token" ]]; then
    openssl rand -hex 32 >"$DEV/ingest-token"
    fresh=1
  fi
  if [[ ! -s "$DEV/port" ]]; then
    echo "${NW_BACKUP_INGEST_PORT:-7443}" >"$DEV/port"
    fresh=1
  fi
  if [[ $fresh == 1 || ! -s "$DEV/portal.env" ]]; then
    local port seed ca ingests packages
    port="$(cat "$DEV/port")"
    seed="$(openssl pkey -in "$DEV/portal-signing.key" -outform DER | hex32)"
    ca="$(awk '{printf "%s\\n", $0}' "$DEV/ca.pem")"
    ingests="[{\"name\":\"localhost\",\"quic\":\"127.0.0.1:$port\",\"tcp\":\"127.0.0.1:$port\",\"ca_pem\":\"$ca\"}]"
    # The usual packages, and one that moves footage to the archive within minutes.
    packages='[{"name":"test-quick","label":"Test: 1 hour instant, moved every 5 min, 1 day archived","hot_hours":1,"move_every_minutes":5,"archive_days":1},'
    packages+='{"name":"hot-1d","label":"1 day, instant playback","hot_hours":24,"move_every_minutes":60,"archive_days":0},'
    packages+='{"name":"hot-1d-archive-7d","label":"1 day instant, then 7 days archived","hot_hours":24,"move_every_minutes":60,"archive_days":7},'
    packages+='{"name":"hot-1d-archive-30d","label":"1 day instant, then 30 days archived","hot_hours":24,"move_every_minutes":60,"archive_days":30}]'
    {
      echo "# Cloud Backup settings for the development portal (NeuroWatch scripts/dev_backup.sh)."
      echo "NEUROWATCH_BACKUP_TICKET_SIGNING_KEY=$seed"
      echo "NEUROWATCH_BACKUP_INGESTS='$ingests'"
      echo "NEUROWATCH_BACKUP_INGEST_TOKEN=$(cat "$DEV/ingest-token")"
      echo "NEUROWATCH_BACKUP_PLANS_FREE=true"
      echo "NEUROWATCH_BACKUP_PACKAGES='$packages'"
    } >"$DEV/portal.env"
  fi
  chmod 600 "$DEV"/*.key "$DEV/ingest-token" "$DEV/portal.env"
}

trust_browser() {
  command -v certutil >/dev/null || die "certutil is required for --trust-browser (apt install libnss3-tools)"
  local db="$HOME/.pki/nssdb"
  if [[ ! -f "$db/cert9.db" ]]; then
    mkdir -p "$db"
    certutil -d "sql:$db" -N --empty-password
  fi
  certutil -d "sql:$db" -D -n "$CA_NAME" >/dev/null 2>&1 || true
  certutil -d "sql:$db" -A -t "C,," -n "$CA_NAME" -i "$DEV/ca.pem"
  log "Chrome/Chromium now trust the development CA (restart the browser if it is open)"
}

setup
[[ $trust == 1 ]] && trust_browser
port="$(cat "$DEV/port")"

if [[ $setup_only == 1 ]]; then
  log "prepared in $DEV"
  log "run the pieces yourself, in this order:"
  log "  1. portal with backup:  set -a; source $DEV/portal.env; set +a; $ROOT/scripts/dev_cloud.sh"
  log "  2. ingest:              $INGEST/scripts/dev.sh"
  exit 0
fi

# --- a portal already running without these settings would offer no backup ----
node_env="$WEBAPP/.env.node-dev"
endpoint="$(sed -n 's/^NW_CLOUD_ENDPOINT=//p' "$node_env" 2>/dev/null || true)"
if [[ -n "$endpoint" ]] && curl -fsS -m 2 "$endpoint/healthz" >/dev/null 2>&1; then
  seed="$(sed -n 's/^NEUROWATCH_BACKUP_TICKET_SIGNING_KEY=//p' "$DEV/portal.env")"
  running=0
  for pid in $(pgrep -f "uvicorn app.main:app" || true); do
    tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null | grep -qx "NEUROWATCH_BACKUP_TICKET_SIGNING_KEY=$seed" && running=1
  done
  [[ $running == 1 ]] || die "a portal is already running without the backup settings ($endpoint): stop it (Ctrl-C in its terminal, or kill its scripts/dev.sh) and run this again"
fi

# --- the ingest, once the portal answers ---------------------------------------
INGEST_PID=""
cleanup() {
  trap - INT TERM EXIT
  if [[ -n "$INGEST_PID" ]] && kill -0 "$INGEST_PID" 2>/dev/null; then
    log "stopping the ingest"
    kill -TERM -- "-$INGEST_PID" 2>/dev/null || kill -TERM "$INGEST_PID" 2>/dev/null || true
    wait "$INGEST_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

log "the ingest starts once the portal is up (log: $LOG)"
# Its own process group, so stopping it stops cargo and the ingest too.
setsid bash -c '
  node_env="$1"; ingest="$2"
  for _ in $(seq 1 600); do
    endpoint="$(sed -n "s/^NW_CLOUD_ENDPOINT=//p" "$node_env" 2>/dev/null || true)"
    [[ -n "$endpoint" ]] && curl -fsS -m 2 "$endpoint/healthz" >/dev/null 2>&1 && exec "$ingest/scripts/dev.sh"
    sleep 1
  done
  echo "the portal did not come up within 10 minutes" >&2; exit 1
' _ "$node_env" "$INGEST" >"$LOG" 2>&1 </dev/null &
INGEST_PID=$!

log "then, to try it:"
log "  1. link this recorder: portal Nodes -> Create enrollment; desktop ≡ -> Cloud Link…"
log "  2. portal: the camera's page -> Backup -> Start backup (free here; 'Test' moves footage to the archive within minutes)"
log "  3. within a minute the recorder streams it: the Backup tab fills, Diagnostics shows a Cloud backup box"
[[ $trust == 1 ]] || log "  instant footage plays from https://localhost:$port: open it once in the browser and accept the warning (or rerun with --trust-browser)"

# --- the portal (with the backup settings) and the desktop app -----------------
set -a
# shellcheck source=/dev/null
source "$DEV/portal.env"
set +a
"$ROOT/scripts/dev_cloud.sh" "${args[@]}"
