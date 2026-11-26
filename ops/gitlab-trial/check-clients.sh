#!/bin/sh
# Validates the distributed gstn binaries inside a built image (runbook
# cli-distribution). Run by .gitlab-ci.yml build:image as
#   docker run --rm -i --entrypoint /bin/sh <image> -s < ops/gitlab-trial/check-clients.sh
# Both targets must be there, non-empty, and match manifest.json (sha256 is
# 64 lower-case hex digits, size is the file's). POSIX sh: the runtime image
# has no bash.
set -eu
dir=/usr/local/share/forge/clients
cd "$dir"
test -s manifest.json || { echo "check-clients: $dir/manifest.json missing or empty" >&2; exit 1; }
manifest="$(cat manifest.json)"
printf '%s\n' "$manifest"
for pair in linux-x86_64:gstn-linux-x86_64 windows-x86_64:gstn-windows-x86_64.exe; do
  target="${pair%%:*}"
  file="${pair#*:}"
  test -s "$file" || { echo "check-clients: $file missing or empty" >&2; exit 1; }
  sum="$(sha256sum "$file" | cut -d' ' -f1)"
  size="$(stat -c %s "$file")"
  printf '%s' "$sum" | grep -Eqx '[0-9a-f]{64}' || { echo "check-clients: bad sha256 for $file" >&2; exit 1; }
  printf '%s' "$manifest" | grep -qF "\"target\":\"$target\"" \
    || { echo "check-clients: $target not in the manifest" >&2; exit 1; }
  printf '%s' "$manifest" | grep -qF "\"sha256\":\"$sum\",\"size\":$size}" \
    || { echo "check-clients: manifest does not match $file (sha256 $sum, size $size)" >&2; exit 1; }
done
./gstn-linux-x86_64 --version
echo "check-clients: ok"
