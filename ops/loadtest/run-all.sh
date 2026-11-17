#!/usr/bin/env bash
# synthetic — run the merge load test scenarios A–D once (ops/loadtest/README.md).
# Takes the shared memory slot lock D:/APP/.cargo-slot.lock (released on every
# exit), waits until no rustc / nextest / vitest runs, samples forge every 10 s,
# and stops at the first failure or when free memory drops below the floor
# (the sampler writes $GS_LT_OUT/STOP). Never restarts a failed scenario.
#
#   GS_LT_TOKEN_FILE=… GS_LT_OUT=… GS_LT_WORK=… GS_LT_HTTPS_BASE=https://<host> \
#   GS_LT_CA_FILE=<ca.crt> TAG=run1 bash ops/loadtest/run-all.sh [a b c d]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
TAG=${TAG:-run1}
LOCK=${GS_LT_LOCK:-/d/APP/.cargo-slot.lock}
: "${GS_LT_TOKEN_FILE:?}" "${GS_LT_OUT:?}" "${GS_LT_WORK:?}"
mkdir -p "$GS_LT_OUT"
scenarios=${*:-a b c d}

sampler_pid=
release() {
  [ -n "$sampler_pid" ] && kill "$sampler_pid" 2>/dev/null || true
  if [ "$(cat "$LOCK" 2>/dev/null)" = "loadtest" ]; then rm -f "$LOCK"; echo "lock released"; fi
}
trap release EXIT INT TERM

echo "$(date -u +%FT%TZ) waiting for the slot lock $LOCK"
until ( set -o noclobber; echo loadtest > "$LOCK" ) 2>/dev/null; do sleep 30; done
echo "$(date -u +%FT%TZ) lock taken"
busy() {
  powershell -NoProfile -Command "@(Get-CimInstance Win32_Process | Where-Object { \$_.Name -match '^(rustc|cargo-nextest)\.exe$' -or (\$_.Name -eq 'node.exe' -and \$_.CommandLine -match 'vitest') }).Count" | tr -d '\r'
}
while [ "$(busy)" != "0" ]; do echo "$(date -u +%T) rustc/nextest/vitest still running; waiting"; sleep 30; done
echo "$(date -u +%FT%TZ) no compile/test process; starting"

node "$here/merge-load.mjs" sample --tag "$TAG" &
sampler_pid=$!

for s in $scenarios; do
  [ -f "$GS_LT_OUT/STOP" ] && { echo "STOP: $(cat "$GS_LT_OUT/STOP")"; exit 3; }
  echo "=== $(date -u +%FT%TZ) scenario $s"
  case $s in
    a) node "$here/merge-load.mjs" a --n 200 --burst 50 --gap 30 --tag "$TAG" ;;
    b) node "$here/merge-load.mjs" b --n 30 --tag "$TAG" ;;
    # b with a client that retries 503 FG-GIT-0004 (the burst limit found in run1)
    b2) node "$here/merge-load.mjs" b --n 30 --retry-busy 10 --tag "${TAG}r" ;;
    c) node "$here/merge-load.mjs" c --n 40 --tag "$TAG" ;;
    c2) node "$here/merge-load.mjs" c --n 40 --retry-busy 10 --tag "${TAG}r" ;;
    a2) node "$here/merge-load.mjs" a --n 200 --burst 50 --gap 30 --retry-busy 10 --tag "${TAG}r" ;;
    d) node "$here/merge-load.mjs" d --n 20 --from "$GS_LT_WORK/docs-a-$TAG" \
         --https-base "${GS_LT_HTTPS_BASE:-}" --ca-file "${GS_LT_CA_FILE:-}" --tag "$TAG" ;;
  esac
  # let the workers drain before the next scenario
  sleep 60
done
echo "=== $(date -u +%FT%TZ) done"
