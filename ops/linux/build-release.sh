#!/usr/bin/env bash
# Build the GitStation Linux release tarball.
#
#   ops/linux/build-release.sh [--target <triple>] [--version <v>] [--out <dir>]
#                              [--skip-web] [--allow-dirty] [--dry-run]
#
#   --target   x86_64-unknown-linux-gnu (default) or aarch64-unknown-linux-gnu
#   --version  release name (default: <workspace version>+g<short commit>[.dirty])
#   --out      output directory (default: dist/)
#   --skip-web use the existing web/dist (already built by `npm run build`)
#   --allow-dirty  build although tracked files are modified (the release
#              is marked .dirty; never for production)
#   --dry-run  check the inputs and print every command without running any
#
# Output: <out>/gitstation-<version>-<target>.tar.gz, its .sha256, and
# <out>/SHA256SUMS rewritten for every tarball in <out>. A checksum is not a
# signature: carry it over a separate channel (runbook linux-install 2). Inside: bin/forge (UI embedded:
# forge-cli feature embed-ui, as the CI release job builds it), share/ (the
# systemd units, example configuration, forgectl, nginx, PostgreSQL, the
# database bootstrap SQL, the runbooks) and BUILD-INFO.
#
# Build on Linux. The binary links the build host's glibc dynamically: build
# on a distribution no newer than the production server (or in the CI image,
# ops/ci-image) so it runs there. For the other architecture either build on
# such a host natively or cross-compile: install the target
# (`rustup target add aarch64-unknown-linux-gnu`) and a C cross compiler for
# `ring` (gcc-aarch64-linux-gnu), then set
#   CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
#   CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
# or run with CARGO=cross (https://github.com/cross-rs/cross).
#
# Runbooks: ops/runbooks/linux-install.md, ops/runbooks/linux-upgrade.md.
set -euo pipefail

target="x86_64-unknown-linux-gnu"
version=""
out=""
skip_web=0
allow_dirty=0
dry_run=0

die() {
  printf 'build-release: %s\n' "$*" >&2
  exit 2
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --target) [ "$#" -ge 2 ] || die "--target needs a value"; target="$2"; shift 2 ;;
    --version) [ "$#" -ge 2 ] || die "--version needs a value"; version="$2"; shift 2 ;;
    --out) [ "$#" -ge 2 ] || die "--out needs a value"; out="$2"; shift 2 ;;
    --skip-web) skip_web=1; shift ;;
    --allow-dirty) allow_dirty=1; shift ;;
    --dry-run) dry_run=1; shift ;;
    -h | --help) sed -n '2,32p' "$0"; exit 0 ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
done

case "$target" in
  x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu) ;;
  *) die "unsupported target: $target (x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu)" ;;
esac

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="${out:-$root/dist}"
CARGO="${CARGO:-cargo}"

# Every packaged input must exist, in a dry run too.
inputs=(
  "$here/systemd/forge-serve.service"
  "$here/systemd/forge-worker.service"
  "$here/systemd/forge-backup.service"
  "$here/systemd/forge-backup.timer"
  "$here/systemd/forge.sysusers"
  "$here/systemd/forge.tmpfiles"
  "$here/etc/forge.toml.example"
  "$here/etc/forgectl.env.example"
  "$here/bin/forgectl"
  "$here/nginx/gitstation.conf"
  "$here/postgresql/README.md"
  "$here/postgresql/forge-postgresql.conf"
  "$here/postgresql/compose.yml"
  "$here/README.md"
  "$root/ops/db/bootstrap/01-cluster-roles.sql"
  "$root/ops/db/bootstrap/02-create-database.sql"
  "$root/ops/db/bootstrap/03-database-objects.sql"
  "$root/ops/runbooks/linux-install.md"
  "$root/ops/runbooks/linux-upgrade.md"
)
for f in "${inputs[@]}"; do
  [ -f "$f" ] || die "missing input: ${f#"$root"/}"
done

need() {
  command -v "$1" >/dev/null 2>&1 || die "$1 not found"
}
need git
need tar
if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  die "sha256sum (or shasum) not found"
fi
if [ "$dry_run" -eq 0 ]; then
  need "$CARGO"
  [ "$skip_web" -eq 1 ] || need npm
fi

commit="$(git -C "$root" rev-parse --short=12 HEAD)"
dirty=""
if [ -n "$(git -C "$root" status --porcelain --untracked-files=no)" ]; then
  dirty=".dirty"
fi
if [ -z "$version" ]; then
  ws_version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p' "$root/Cargo.toml" | head -n1)"
  [ -n "$ws_version" ] || die "cannot read the workspace version from Cargo.toml"
  version="${ws_version}+g${commit}${dirty}"
fi
case "$version" in
  *[!A-Za-z0-9._+-]* | '' | .* | -*) die "invalid version: $version" ;;
esac

name="gitstation-${version}-${target}"
stage_root="$out/.stage"
stage="$stage_root/$name"
tarball="$out/$name.tar.gz"

run() {
  if [ "$dry_run" -eq 1 ]; then
    printf '+'
    printf ' %q' "$@"
    printf '\n'
  else
    "$@"
  fi
}

printf 'build-release: %s (commit %s%s)\n' "$name" "$commit" "$dirty"
if [ -n "$dirty" ]; then
  [ "$allow_dirty" -eq 1 ] || die "tracked files are modified: commit them, or pass --allow-dirty (never for production)"
  printf 'build-release: WARNING: tracked files are modified; the release is marked .dirty\n' >&2
fi

# 1. The web UI (embedded into the binary by the embed-ui feature).
if [ "$skip_web" -eq 0 ]; then
  run npm --prefix "$root/web" ci --ignore-scripts
  run npm --prefix "$root/web" run build
fi
if [ "$dry_run" -eq 0 ]; then
  [ -f "$root/web/dist/index.html" ] || die "web/dist/index.html missing (build the UI or drop --skip-web)"
fi

# 2. The binary.
run "$CARGO" build --release --locked -p forge-cli --features embed-ui --target "$target"
bin="$root/target/$target/release/forge"
if [ -n "${CARGO_TARGET_DIR:-}" ]; then
  bin="$CARGO_TARGET_DIR/$target/release/forge"
fi

# 3. The tree.
run rm -rf -- "$stage"
run mkdir -p "$stage/bin" "$stage/share/systemd" "$stage/share/etc" "$stage/share/bin" \
  "$stage/share/nginx" "$stage/share/postgresql" "$stage/share/db/bootstrap" "$stage/share/runbooks"
run install -m 0755 "$bin" "$stage/bin/forge"
run install -m 0644 "$here"/systemd/forge-serve.service "$here"/systemd/forge-worker.service \
  "$here"/systemd/forge-backup.service "$here"/systemd/forge-backup.timer \
  "$here"/systemd/forge.sysusers "$here"/systemd/forge.tmpfiles "$stage/share/systemd/"
run install -m 0644 "$here"/etc/forge.toml.example "$here"/etc/forgectl.env.example "$stage/share/etc/"
run install -m 0755 "$here/bin/forgectl" "$stage/share/bin/forgectl"
run install -m 0644 "$here/nginx/gitstation.conf" "$stage/share/nginx/"
run install -m 0644 "$here"/postgresql/README.md "$here"/postgresql/forge-postgresql.conf \
  "$here"/postgresql/compose.yml "$stage/share/postgresql/"
run install -m 0644 "$root"/ops/db/bootstrap/*.sql "$stage/share/db/bootstrap/"
run install -m 0644 "$root"/ops/runbooks/*.md "$stage/share/runbooks/"
run install -m 0644 "$here/README.md" "$stage/README.md"

ui_sha="-"
if [ "$dry_run" -eq 0 ]; then
  ui_sha="$(sha256 "$root/web/dist/index.html")"
fi
info="$(printf '%s\n' \
  "name=$name" \
  "version=$version" \
  "target=$target" \
  "commit=$(git -C "$root" rev-parse HEAD)$dirty" \
  "built_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  "rustc=$(rustc --version 2>/dev/null || echo unknown)" \
  "web_index_sha256=$ui_sha")"
if [ "$dry_run" -eq 1 ]; then
  printf '+ write %s/BUILD-INFO:\n%s\n' "$stage" "$info"
else
  printf '%s\n' "$info" >"$stage/BUILD-INFO"
fi

# A native build can run the binary: it must print its version.
host="$(rustc -vV 2>/dev/null | sed -n 's/^host: //p' || true)"
if [ "$host" = "$target" ]; then
  run "$stage/bin/forge" --version
fi

# 4. The tarball (owned by root inside, stable order) and its checksum.
run mkdir -p "$out"
run tar --sort=name --owner=0 --group=0 --numeric-owner -C "$stage_root" -czf "$tarball" "$name"
if [ "$dry_run" -eq 1 ]; then
  printf '+ sha256 %s; rewrite %s/SHA256SUMS\n' "$tarball" "$out"
else
  sum="$(sha256 "$tarball")"
  printf '%s  %s\n' "$sum" "$name.tar.gz" >"$tarball.sha256"
  sums="$out/SHA256SUMS.tmp"
  : >"$sums"
  for t in "$out"/gitstation-*.tar.gz; do
    printf '%s  %s\n' "$(sha256 "$t")" "$(basename "$t")" >>"$sums"
  done
  mv -f "$sums" "$out/SHA256SUMS"
  rm -rf -- "$stage_root"
  printf 'build-release: %s\nbuild-release: sha256 %s\n' "$tarball" "$sum"
fi
