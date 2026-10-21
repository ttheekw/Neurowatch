#!/usr/bin/env bash
# Check the private downloads bucket settings the release workflow uses
# (the DOWNLOADS_S3_* repository secrets) before a release run finds out.
#
#   scripts/check_downloads_bucket.sh [--read-only]
#
# Reads DOWNLOADS_S3_BUCKET, DOWNLOADS_S3_ENDPOINT, DOWNLOADS_S3_REGION,
# DOWNLOADS_S3_ACCESS_KEY_ID and DOWNLOADS_S3_SECRET_ACCESS_KEY from the
# environment and asks for any that are missing (when run in a terminal).
# Then it checks, with the same values the workflow would use:
#   1. the values themselves (stray spaces, s3:// prefixes, a bucket in the
#      endpoint URL, a region that does not match the endpoint);
#   2. that the bucket exists at that endpoint (no credentials needed);
#   3. that the key can list it, and (unless --read-only) write, read back
#      and delete a small object under staging/.
# At the end it prints the values to put in the GitHub secrets.
# Needs only bash and curl >= 7.75 (signed requests with --aws-sigv4).
set -euo pipefail

READ_ONLY=0
for arg in "$@"; do
  case "$arg" in
    --read-only) READ_ONLY=1 ;;
    -h|--help) sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) printf 'unknown argument: %s\n' "$arg" >&2; exit 2 ;;
  esac
done

ok()   { printf '\033[1;32m  ok\033[0m  %s\n' "$*"; }
warn() { printf '\033[1;33mwarn\033[0m  %s\n' "$*"; WARNINGS=$((WARNINGS + 1)); }
bad()  { printf '\033[1;31mfail\033[0m  %s\n' "$*"; FAILURES=$((FAILURES + 1)); }
info() { printf '      %s\n' "$*"; }
die()  { printf '\033[1;31m[check-downloads-bucket]\033[0m %s\n' "$*" >&2; exit 1; }
WARNINGS=0
FAILURES=0

command -v curl >/dev/null || die "curl is required"
curl --help all 2>/dev/null | grep -q -- '--aws-sigv4' || die "curl is too old: --aws-sigv4 needs curl 7.75 or newer"

sha256() {
  if command -v sha256sum >/dev/null; then sha256sum | cut -d' ' -f1; else shasum -a 256 | cut -d' ' -f1; fi
}
trim() { local v="$1"; v="${v#"${v%%[![:space:]]*}"}"; printf '%s' "${v%"${v##*[![:space:]]}"}"; }
xml_field() { grep -o "<$1>[^<]*</$1>" <<<"$2" | head -1 | sed -E "s#</?$1>##g" || true; }

# --- 1. Values -------------------------------------------------------------

ask() { # name prompt secret(0/1) default
  local name=$1 prompt=$2 secret=$3 default=${4:-} value
  [[ -n "${!name:-}" ]] && return
  [[ -t 0 ]] || return 0
  if [[ $secret == 1 ]]; then
    read -r -s -p "$prompt: " value; echo
  elif [[ -n $default ]]; then
    read -r -p "$prompt [$default]: " value; value=${value:-$default}
  else
    read -r -p "$prompt: " value
  fi
  printf -v "$name" '%s' "$value"
}

region_for_endpoint() {
  local host=${1#*://}; host=${host%%/*}
  case "$host" in
    s3.wasabisys.com) echo us-east-1 ;;
    s3.*.wasabisys.com) host=${host#s3.}; echo "${host%.wasabisys.com}" ;;
    *.r2.cloudflarestorage.com) echo auto ;;
    s3.*.amazonaws.com) host=${host#s3.}; echo "${host%.amazonaws.com}" ;;
    *) echo "" ;;
  esac
}

echo "Downloads bucket settings (empty values are asked for; secrets are not shown)"
ask DOWNLOADS_S3_ENDPOINT "DOWNLOADS_S3_ENDPOINT (e.g. https://s3.ap-southeast-1.wasabisys.com, empty for AWS)" 0
ask DOWNLOADS_S3_BUCKET "DOWNLOADS_S3_BUCKET (the bucket name only)" 0
ask DOWNLOADS_S3_REGION "DOWNLOADS_S3_REGION" 0 "$(region_for_endpoint "${DOWNLOADS_S3_ENDPOINT:-}")"
ask DOWNLOADS_S3_ACCESS_KEY_ID "DOWNLOADS_S3_ACCESS_KEY_ID" 0
ask DOWNLOADS_S3_SECRET_ACCESS_KEY "DOWNLOADS_S3_SECRET_ACCESS_KEY" 1
echo

RAW_BUCKET=${DOWNLOADS_S3_BUCKET:-}
RAW_ENDPOINT=${DOWNLOADS_S3_ENDPOINT:-}
RAW_REGION=${DOWNLOADS_S3_REGION:-}
KEY=$(trim "${DOWNLOADS_S3_ACCESS_KEY_ID:-}")
SECRET=$(trim "${DOWNLOADS_S3_SECRET_ACCESS_KEY:-}")

[[ -n $RAW_BUCKET ]] || die "DOWNLOADS_S3_BUCKET is not set"
[[ -n $KEY && -n $SECRET ]] || die "DOWNLOADS_S3_ACCESS_KEY_ID and DOWNLOADS_S3_SECRET_ACCESS_KEY are required"
[[ $KEY == "${DOWNLOADS_S3_ACCESS_KEY_ID}" ]] || warn "DOWNLOADS_S3_ACCESS_KEY_ID has spaces or a line break around it; set it again"
[[ $SECRET == "${DOWNLOADS_S3_SECRET_ACCESS_KEY}" ]] || warn "DOWNLOADS_S3_SECRET_ACCESS_KEY has spaces or a line break around it; set it again"

echo "Values"
BUCKET=$(trim "$RAW_BUCKET")
BUCKET=${BUCKET#s3://}
BUCKET=${BUCKET%%/*}
if [[ $BUCKET != "$RAW_BUCKET" ]]; then
  warn "DOWNLOADS_S3_BUCKET is not a bare bucket name (spaces, s3:// or a path); it should be: $BUCKET"
fi
if [[ $BUCKET =~ ^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$ ]]; then
  ok "bucket name looks valid (${#BUCKET} characters)"
else
  bad "\"$BUCKET\" is not a valid bucket name (3-63 lowercase letters, digits, dots, hyphens)"
fi

ENDPOINT=$(trim "$RAW_ENDPOINT")
REGION=$(trim "$RAW_REGION")
if [[ -z $ENDPOINT ]]; then
  [[ -n $REGION ]] || die "with no DOWNLOADS_S3_ENDPOINT (AWS) a DOWNLOADS_S3_REGION is required"
  ENDPOINT="https://s3.$REGION.amazonaws.com"
  info "no endpoint: using AWS ($ENDPOINT)"
else
  [[ $ENDPOINT == *://* ]] || ENDPOINT="https://$ENDPOINT"
  ENDPOINT=${ENDPOINT%/}
  path=${ENDPOINT#*://}
  if [[ $path == */* ]]; then
    warn "DOWNLOADS_S3_ENDPOINT has a path (/${path#*/}); it should end at the host: ${ENDPOINT%%://*}://${path%%/*}"
    ENDPOINT="${ENDPOINT%%://*}://${path%%/*}"
  elif [[ $ENDPOINT != "$RAW_ENDPOINT" && "https://$ENDPOINT" != "https://$RAW_ENDPOINT" ]]; then
    warn "DOWNLOADS_S3_ENDPOINT has spaces or a trailing slash; it should be: $ENDPOINT"
  fi
  [[ $ENDPOINT == https://* ]] || warn "the endpoint is not HTTPS: $ENDPOINT"
  ok "endpoint $ENDPOINT"
fi

expected_region=$(region_for_endpoint "$ENDPOINT")
if [[ -z $REGION ]]; then
  REGION=${expected_region:-auto}
  if [[ $REGION != auto ]]; then
    bad "DOWNLOADS_S3_REGION is not set: the workflow then signs for 'auto', which $ENDPOINT refuses; set it to $REGION"
  else
    ok "region auto (the workflow's default)"
  fi
elif [[ -n $expected_region && $REGION != "$expected_region" ]]; then
  bad "DOWNLOADS_S3_REGION is '$REGION' but $ENDPOINT is region '$expected_region'"
else
  ok "region $REGION"
fi
echo

# --- 2. Does the bucket exist? ----------------------------------------------

echo "Bucket"
body=$(curl -sS --max-time 20 "$ENDPOINT/$BUCKET/?max-keys=0" 2>&1) || die "cannot reach $ENDPOINT: $body"
code=$(xml_field Code "$body")
case "$code" in
  ""|AccessDenied|AllAccessDisabled)
    ok "bucket \"$BUCKET\" exists at $ENDPOINT" ;;
  NoSuchBucket)
    bad "no bucket \"$BUCKET\" at $ENDPOINT (check the name for a typo, and that the endpoint is the bucket's region)" ;;
  PermanentRedirect|AuthorizationHeaderMalformed|IllegalLocationConstraintException)
    right=$(xml_field Endpoint "$body"); right=${right#"$BUCKET".}
    bad "bucket \"$BUCKET\" is in another region: use the endpoint https://$right" ;;
  *)
    warn "unexpected answer for the bucket: $code $(xml_field Message "$body")" ;;
esac
echo

# --- 3. Can the key use it? -------------------------------------------------

EMPTY_SHA=$(printf '' | sha256)
STATUS=000
BODY=""
s3() { # method key-path [curl args...]; sets STATUS and BODY
  local method=$1 object=$2 hash=$EMPTY_SHA out; shift 2
  [[ ${1:-} == --hash ]] && { hash=$2; shift 2; }
  out=$(mktemp)
  STATUS=$(curl -sS --max-time 30 -o "$out" -w '%{http_code}' -X "$method" \
    --aws-sigv4 "aws:amz:$REGION:s3" --user "$KEY:$SECRET" \
    -H "x-amz-content-sha256: $hash" "$@" "$ENDPOINT/$BUCKET/$object" 2>/dev/null) || STATUS=000
  BODY=$(cat "$out"); rm -f "$out"
}
explain() { # what
  local code msg
  code=$(xml_field Code "$BODY"); msg=$(xml_field Message "$BODY")
  case "$code" in
    InvalidAccessKeyId) bad "$1: the access key ID is not known at $ENDPOINT (wrong key, or a key from another provider/account)" ;;
    SignatureDoesNotMatch) bad "$1: the secret access key does not match the key ID" ;;
    AuthorizationHeaderMalformed) bad "$1: wrong region '$REGION' (the store expects '$(xml_field Region "$BODY")')" ;;
    AccessDenied) bad "$1: the key has no permission for this (give it read and write access to the bucket)" ;;
    NoSuchBucket) bad "$1: no such bucket" ;;
    "") bad "$1: HTTP $STATUS" ;;
    *) bad "$1: $code $msg" ;;
  esac
}

echo "Access with the key"
LIST_OK=0
s3 GET "?list-type=2&max-keys=1&prefix=releases"
if [[ $STATUS == 200 ]]; then
  LIST_OK=1
  ok "the key can list the bucket"
  s3 GET "releases/index.json" -r 0-0
  if [[ $STATUS == 200 || $STATUS == 206 ]]; then
    info "releases/index.json exists (a release was published before)"
  else
    info "no releases/index.json yet (normal before the first published release)"
  fi
else
  explain "listing the bucket"
fi

if [[ $READ_ONLY == 1 ]]; then
  info "--read-only: write test skipped"
elif [[ $LIST_OK == 1 ]]; then
  object="staging/bucket-check/$(date -u +%Y%m%dT%H%M%SZ)-$$.txt"
  payload="NeuroWatch downloads bucket check $(date -u +%FT%TZ)"
  s3 PUT "$object" --hash "$(printf '%s' "$payload" | sha256)" -H 'Content-Type: text/plain' --data-binary "$payload"
  if [[ $STATUS == 200 ]]; then
    ok "the key can write ($object)"
    s3 GET "$object"
    if [[ $STATUS == 200 && $BODY == "$payload" ]]; then ok "read back what was written"; else explain "reading the test object back"; fi
    s3 DELETE "$object"
    if [[ $STATUS == 204 || $STATUS == 200 ]]; then ok "deleted the test object"; else explain "deleting the test object $object"; fi
  else
    explain "writing under staging/ (the release workflow needs this)"
  fi
fi
echo

# --- Summary ----------------------------------------------------------------

echo "GitHub repository secrets (Settings > Secrets and variables > Actions):"
printf '  %-34s %s\n' DOWNLOADS_S3_BUCKET "$BUCKET" DOWNLOADS_S3_ENDPOINT "${RAW_ENDPOINT:+$ENDPOINT}" \
  DOWNLOADS_S3_REGION "$REGION" DOWNLOADS_S3_ACCESS_KEY_ID "(the key you entered)" \
  DOWNLOADS_S3_SECRET_ACCESS_KEY "(its secret)"
echo "The web portal reads the same bucket as NEUROWATCH_DOWNLOADS_S3_* (a read-only key is enough)."
echo
if [[ $FAILURES -gt 0 ]]; then
  printf '\033[1;31m%d problem(s)\033[0m, %d warning(s)\n' "$FAILURES" "$WARNINGS"; exit 1
elif [[ $WARNINGS -gt 0 ]]; then
  printf '\033[1;33mworks, with %d warning(s)\033[0m: fix the values above before copying them into the secrets\n' "$WARNINGS"
else
  printf '\033[1;32mall checks passed\033[0m\n'
fi
