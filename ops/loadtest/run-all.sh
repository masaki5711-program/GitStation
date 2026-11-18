#!/usr/bin/env bash
# synthetic — run merge load test scenarios once each (ops/loadtest/README.md).
# Takes the shared memory slot lock (released on every exit, only when this run
# holds it), waits until no rustc / nextest / vitest runs, samples forge every
# 10 s, and stops at the first failure or when free memory drops below the
# floor (a sampler writes $GS_LT_OUT/STOP). Never restarts a failed scenario.
#
#   GS_LT_TOKEN_FILE=… GS_LT_OUT=… GS_LT_WORK=… GS_LT_HTTPS_BASE=https://<host> \
#   GS_LT_CA_FILE=<ca.crt> TAG=run1 bash ops/loadtest/run-all.sh [a b c d]
#
# Scenarios: a b b2 c c2 a2 d (the first run), a-burst, e (the 10,000-merge
# soak, about 7 h), e-rehearsal (200 merges with the same code), spawn-bench
# (local only: no server, no token).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
TAG=${TAG:-run1}
case "${OSTYPE:-}" in
  msys*|cygwin*) windows=1; default_lock=/d/APP/.cargo-slot.lock ;;
  *) windows=0; default_lock=${TMPDIR:-/tmp}/gs-loadtest.lock ;;
esac
LOCK=${GS_LT_LOCK:-$default_lock}
: "${GS_LT_OUT:?}" "${GS_LT_WORK:?}"
scenarios=${*:-a b c d}

# reject an unknown name before anything is taken or started
needs_server=0
for s in $scenarios; do
  case $s in
    a|a2|a-burst|b|b2|c|c2|d|e|e-rehearsal) needs_server=1 ;;
    spawn-bench) ;;
    *) echo "unknown scenario: $s (a a2 a-burst b b2 c c2 d e e-rehearsal spawn-bench)" >&2; exit 2 ;;
  esac
done
if [ "$needs_server" = 1 ]; then : "${GS_LT_TOKEN_FILE:?}"; fi
mkdir -p "$GS_LT_OUT"

sampler_pid=
held=0
release() {
  [ -n "$sampler_pid" ] && kill "$sampler_pid" 2>/dev/null || true
  if [ "$held" = 1 ]; then rm -f "$LOCK"; held=0; echo "lock released"; fi
}
trap release EXIT INT TERM HUP

echo "$(date -u +%FT%TZ) waiting for the slot lock $LOCK"
until ( set -o noclobber; echo loadtest > "$LOCK" ) 2>/dev/null; do sleep 30; done
held=1
echo "$(date -u +%FT%TZ) lock taken"
busy() {
  if [ "$windows" = 1 ]; then
    powershell -NoProfile -Command "@(Get-CimInstance Win32_Process | Where-Object { \$_.Name -match '^(rustc|cargo-nextest)\.exe$' -or (\$_.Name -eq 'node.exe' -and \$_.CommandLine -match 'vitest') }).Count" | tr -d '\r'
  elif pgrep -x rustc >/dev/null || pgrep -x cargo-nextest >/dev/null || pgrep -f 'node .*vitest' >/dev/null; then
    echo 1
  else
    echo 0
  fi
}
while [ "$(busy)" != "0" ]; do echo "$(date -u +%T) rustc/nextest/vitest still running; waiting"; sleep 30; done
echo "$(date -u +%FT%TZ) no compile/test process; starting"

if [ "$needs_server" = 1 ]; then
  node "$here/merge-load.mjs" sample --tag "$TAG" &
  sampler_pid=$!
fi

for s in $scenarios; do
  [ -f "$GS_LT_OUT/STOP" ] && { echo "STOP: $(cat "$GS_LT_OUT/STOP")"; exit 3; }
  echo "=== $(date -u +%FT%TZ) scenario $s"
  case $s in
    a) node "$here/merge-load.mjs" a --n 200 --burst 50 --gap 30 --tag "$TAG" ;;
    # all 200 MRs at once, auto-merge alone drains them (plan 18: the drain rate)
    a-burst) node "$here/merge-load.mjs" a-burst --n 200 --tag "$TAG" ;;
    b) node "$here/merge-load.mjs" b --n 30 --tag "$TAG" ;;
    # b with a client that retries 503 FG-GIT-0004 (the burst limit found in run1)
    b2) node "$here/merge-load.mjs" b --n 30 --retry-busy 10 --tag "${TAG}r" ;;
    c) node "$here/merge-load.mjs" c --n 40 --tag "$TAG" ;;
    c2) node "$here/merge-load.mjs" c --n 40 --retry-busy 10 --tag "${TAG}r" ;;
    a2) node "$here/merge-load.mjs" a --n 200 --burst 50 --gap 30 --retry-busy 10 --tag "${TAG}r" ;;
    d) node "$here/merge-load.mjs" d --n 20 --from "$GS_LT_WORK/docs-a-$TAG" \
         --https-base "${GS_LT_HTTPS_BASE:-}" --ca-file "${GS_LT_CA_FILE:-}" --tag "$TAG" ;;
    # the soak: 10,000 merges, 21 a minute with 100 in every 30th minute (about 7 h)
    e) node "$here/merge-load.mjs" e --count 10000 --rate 21 --burst 100 --burst-every 30 --tag "$TAG" ;;
    # the same code in about 6 minutes of arrivals
    e-rehearsal) node "$here/merge-load.mjs" e --count 200 --rate 21 --burst 100 --burst-every 3 \
         --docs 200 --sample-every 10 --repo-size-every 60 --tag "${TAG}s" ;;
    spawn-bench) node "$here/merge-load.mjs" spawn-bench --n 100 --tag "$TAG"; continue ;;
  esac
  # let the workers drain before the next scenario
  sleep 60
done
echo "=== $(date -u +%FT%TZ) done"
