#!/bin/sh
# Structural checks of a hello-app build (the runtime health is checked on the target).
set -eu
d=${1:?usage: check.sh <dist>}
test -s "$d/index.html" || { echo "index.html missing"; exit 1; }
grep -q '<h1>hello-app</h1>' "$d/index.html" || { echo "index.html has no title"; exit 1; }
test -s "$d/health" || { echo "health missing"; exit 1; }
echo "checks ok"
