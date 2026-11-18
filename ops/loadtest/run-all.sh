#!/usr/bin/env bash
# synthetic — run merge load test scenarios once each (ops/loadtest/README.md).
# Checks every command line first (nothing is taken or started on a wrong one),
# takes the shared memory slot lock, waits until no rustc / nextest / vitest
# runs, samples forge every 10 s, and stops when a scenario fails, when free
# memory drops below the floor or when the sampler dies (the sampler writes
# $GS_LT_OUT/STOP). Never restarts a failed scenario.
#
# The lock is removed on every exit, and only when this run took it and it
# still carries this run's marker. A signal ends the run: the scenario and the
# sampler are stopped, then the lock is released.
#
# Exit: 0 every scenario can be trusted; 2 a wrong command line or environment;
# 3 STOP; 4 every scenario finished, but at least one result is not a pass
# (a scenario that exits 4 does not stop the next one: its result is written);
# 129 / 130 / 143 a signal; anything else: the exit code of the scenario that
# failed (the run stopped there).
#
#   GS_LT_TOKEN_FILE=… GS_LT_OUT=… GS_LT_WORK=… GS_LT_HTTPS_BASE=https://<host> \
#   GS_LT_CA_FILE=<ca.crt> TAG=run1 bash ops/loadtest/run-all.sh [a b c d]
#
# Scenarios: a b b2 c c2 a2 d (the first run), a-burst, a-xref (pairs that
# break a reference together: the train lint of plan 18 S1), e (the
# 10,000-merge soak, about 7 h), e-rehearsal (200 merges with the same code),
# spawn-bench (local only: no server, no token).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
TAG=${TAG:-run1}
# the tag names files, directories (one is removed recursively) and projects
case $TAG in
  *[!A-Za-z0-9._-]*) echo "TAG must match ^[A-Za-z0-9._-]+\$ (got '$TAG')" >&2; exit 2 ;;
esac
case "${OSTYPE:-}" in
  msys*|cygwin*) windows=1; default_lock=/d/APP/.cargo-slot.lock ;;
  # a directory of this user: in the world-writable /tmp anyone could plant or remove the lock
  *)
    windows=0
    if [ -z "${GS_LT_LOCK:-}${XDG_RUNTIME_DIR:-}${HOME:-}" ]; then
      echo "no place for the lock: set GS_LT_LOCK (or HOME / XDG_RUNTIME_DIR)" >&2; exit 2
    fi
    default_lock=${XDG_RUNTIME_DIR:-${HOME:-}/.cache}/gs-loadtest/slot.lock ;;
esac
LOCK=${GS_LT_LOCK:-$default_lock}
need() {
  if [ -z "${!1:-}" ]; then echo "$1 is required" >&2; exit 2; fi
}
need GS_LT_OUT
need GS_LT_WORK
# a first-parent commit of main without its own forge/doc-lint run: reported,
# not fatal. The default stays 0 after plan 18 S1: the train lint records its
# run on the train's last step only (and only when a member changes a
# document), so the steps before it have no run of their own. a-xref reads
# every first-parent commit instead.
export GS_LT_DOC_LINT_MISSING_FATAL=${GS_LT_DOC_LINT_MISSING_FATAL:-0}
if [ $# -eq 0 ]; then set -- a b c d; fi

# the command line of a scenario, in one place for the check and for the run
scenario_argv() {
  case $1 in
    a) argv=(a --n 200 --burst 50 --gap 30 --tag "$TAG") ;;
    # all 200 MRs at once, auto-merge alone drains them (plan 18: the drain rate)
    a-burst) argv=(a-burst --n 200 --tag "$TAG") ;;
    # 20 pairs that break a reference together + 20 plain revisions, at once
    # (plan 18 S1: no first-parent commit of main with a broken reference)
    a-xref) argv=(a-xref --n 20 --tag "$TAG") ;;
    b) argv=(b --n 30 --tag "$TAG") ;;
    # b with a client that retries 503 FG-GIT-0004 (the burst limit found in run1)
    b2) argv=(b --n 30 --retry-busy 10 --tag "${TAG}r") ;;
    c) argv=(c --n 40 --tag "$TAG") ;;
    c2) argv=(c --n 40 --retry-busy 10 --tag "${TAG}r") ;;
    a2) argv=(a --n 200 --burst 50 --gap 30 --retry-busy 10 --tag "${TAG}r") ;;
    d) argv=(d --n 20 --from "$GS_LT_WORK/docs-a-$TAG" --https-base "${GS_LT_HTTPS_BASE:-}" --ca-file "${GS_LT_CA_FILE:-}" --tag "$TAG") ;;
    # the soak: 10,000 merges, 21 a minute with 100 in every 30th minute (about 7 h)
    e) argv=(e --count 10000 --rate 21 --burst 100 --burst-every 30 --tag "$TAG") ;;
    # the same code in about 6 minutes of arrivals
    e-rehearsal) argv=(e --count 200 --rate 21 --burst 100 --burst-every 3 --docs 200 --sample-every 10 --repo-size-every 60 --tag "${TAG}s") ;;
    spawn-bench) argv=(spawn-bench --n 100 --tag "$TAG") ;;
    *) return 1 ;;
  esac
}

# reject an unknown name, a wrong option or a wrong environment before
# anything is taken or started (--preflight: the scenario checks itself, exit 2)
needs_server=0
for s in "$@"; do
  scenario_argv "$s" || { echo "unknown scenario: $s (a a2 a-burst a-xref b b2 c c2 d e e-rehearsal spawn-bench)" >&2; exit 2; }
  if [ "$s" != spawn-bench ]; then needs_server=1; fi
  node "$here/merge-load.mjs" "${argv[@]}" --preflight >/dev/null || exit 2
done
if [ "$needs_server" = 1 ]; then
  need GS_LT_TOKEN_FILE
  node "$here/merge-load.mjs" sample --tag "$TAG" --preflight >/dev/null || exit 2
fi
mkdir -p "$GS_LT_OUT"
if [ -z "${GS_LT_LOCK:-}" ] && [ "$windows" = 0 ]; then mkdir -p -m 700 "$(dirname "$LOCK")"; fi

sampler_pid=
child_pid=
nap_pid=
held=0
marker="loadtest pid=$$ $(date -u +%FT%TZ)"
release() {
  # the children first: no scenario outlives the lock
  for pid in $nap_pid $child_pid $sampler_pid; do kill "$pid" 2>/dev/null || true; done
  for pid in $child_pid $sampler_pid; do wait "$pid" 2>/dev/null || true; done
  nap_pid=; child_pid=; sampler_pid=
  if [ "$held" = 1 ]; then
    held=0
    # only a lock that still carries this run's marker is this run's to remove
    if [ "$(cat "$LOCK" 2>/dev/null)" = "$marker" ]; then
      rm -f "$LOCK"
      echo "lock released"
    else
      echo "the lock $LOCK does not carry this run's marker any more: left in place" >&2
    fi
  fi
}
# a signal ends the run (the EXIT trap then cleans up); it never goes on to the next step
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
trap release EXIT
# sleep in the background: bash runs a trap only when the foreground command
# returns, and `wait` returns at once on a signal
nap() {
  sleep "$1" &
  nap_pid=$!
  wait "$nap_pid" || true
  nap_pid=
}
stop_check() {
  if [ -f "$GS_LT_OUT/STOP" ]; then echo "STOP: $(cat "$GS_LT_OUT/STOP")" >&2; exit 3; fi
  # a sampler that ended without a STOP file: nobody watches the memory floor
  if [ -n "$sampler_pid" ] && ! kill -0 "$sampler_pid" 2>/dev/null; then
    echo "the sampler is not running any more: stopping" >&2
    exit 1
  fi
}

echo "$(date -u +%FT%TZ) waiting for the slot lock $LOCK"
until ( set -o noclobber; echo "$marker" > "$LOCK" ) 2>/dev/null; do nap 30; done
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
while [ "$(busy)" != "0" ]; do echo "$(date -u +%T) rustc/nextest/vitest still running; waiting"; nap 30; done
echo "$(date -u +%FT%TZ) no compile/test process; starting"

if [ "$needs_server" = 1 ]; then
  node "$here/merge-load.mjs" sample --tag "$TAG" &
  sampler_pid=$!
fi

not_ok=
for s in "$@"; do
  stop_check
  echo "=== $(date -u +%FT%TZ) scenario $s"
  scenario_argv "$s"
  # in the background: a signal is handled at once, not when the scenario ends
  node "$here/merge-load.mjs" "${argv[@]}" &
  child_pid=$!
  rc=0
  wait "$child_pid" || rc=$?
  child_pid=
  case $rc in
    0) ;;
    # it finished and wrote its result, but the result is not a pass: the
    # next scenario still runs (its own project), the run ends with 4
    4) not_ok="$not_ok $s"; echo "=== scenario $s is NOT OK (exit 4): its result file says why" >&2 ;;
    *) echo "=== scenario $s failed (exit $rc): stopping" >&2; exit "$rc" ;;
  esac
  if [ "$s" = spawn-bench ]; then continue; fi
  # let the workers drain before the next scenario
  nap 60
done
stop_check
if [ -n "$not_ok" ]; then
  echo "=== $(date -u +%FT%TZ) done, NOT OK:$not_ok" >&2
  exit 4
fi
echo "=== $(date -u +%FT%TZ) done"
