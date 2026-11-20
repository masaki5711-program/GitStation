#!/usr/bin/env bash
# OPT-IN forge-runner on the PFS trial server (README 「自前の CI/CD」).
# Run as the runner user gitstation-ci (docker group) from a checkout of
# the repository; never from a pipeline.
#
#   bash ops/gitlab-trial/runner.sh init    # directories + runner/config.toml (from PUBLIC_URL in .env)
#   bash ops/gitlab-trial/runner.sh up      # build gitstation/forge-runner:trial-<sha>, check, start
#   bash ops/gitlab-trial/runner.sh check   # `forge-runner check` in a one-off container
#   bash ops/gitlab-trial/runner.sh stop    # stop the runner (keeps its volumes)
#
# The token is NOT created here: register the runner with
# `forge admin runner register` and write only its fgr_… line to
# /srv/gitstation/secrets-runner/token (README). Everything this script
# touches is named gitstation-trial-runner*; it never prunes anything.
#
# Rollback (as gitstation-ci): runner.sh stop; then
#   RUNNER_TAG=x RUNNER_FILES_GID=0 docker compose -p gitstation-trial-runner --project-directory /srv/gitstation \
#     -f /srv/gitstation/compose.runner.yml down -v      # this project's container, network and volumes only
#   docker rm -f gitstation-trial-runner                 # fallback when compose.runner.yml is already gone
#   docker network rm gitstation-trial-runner-net; docker volume rm gitstation-trial-runner-podman gitstation-trial-runner-work
#   docker image ls --format '{{.Repository}}:{{.Tag}}' gitstation/forge-runner | grep ':trial-' | xargs -r docker image rm
#   rm -rf /srv/gitstation/runner /srv/gitstation/secrets-runner /srv/gitstation/compose.runner.yml
# and disable the runner in GitStation (forge admin runner disable).
set -Eeuo pipefail

DEPLOY_DIR=/srv/gitstation
PROJECT=gitstation-trial-runner
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ENV_FILE="$DEPLOY_DIR/.env"
CONFIG="$DEPLOY_DIR/runner/config.toml"
TOKEN="$DEPLOY_DIR/secrets-runner/token"

log() { printf '[runner %s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
die() { log "ERROR: $*"; exit 1; }

compose() { # <tag> <args…>
  local tag="$1"; shift
  RUNNER_TAG="$tag" RUNNER_FILES_GID="$(id -g)" docker compose -p "$PROJECT" \
    --project-directory "$DEPLOY_DIR" -f "$DEPLOY_DIR/compose.runner.yml" "$@"
}

group_only() { # <path> <mode>: owned by me, exactly that mode
  [ -O "$1" ] && [ "$(stat -c %a "$1")" = "$2" ]
}

init() {
  { [ -d "$DEPLOY_DIR" ] && [ -O "$DEPLOY_DIR" ]; } || die "$DEPLOY_DIR must exist and be owned by $(id -un) (setup-server.sh)"
  [ -r "$ENV_FILE" ] || die "$ENV_FILE is missing (setup-server.sh)"
  install -d -m 0750 "$DEPLOY_DIR/runner" "$DEPLOY_DIR/secrets-runner"
  install -m 0644 "$SRC/ops/runner/containers.conf" "$DEPLOY_DIR/runner/containers.conf"
  if [ -f "$CONFIG" ]; then
    log "kept $CONFIG"
  else
    local url
    # shellcheck disable=SC1090 # the server's own .env (owner-only, setup-server.sh)
    url="$(set -a; . "$ENV_FILE"; printf '%s' "${PUBLIC_URL:-}")"
    [[ "$url" =~ ^https?://[A-Za-z0-9.:-]+$ ]] || die "PUBLIC_URL in $ENV_FILE is not http(s)://host:port"
    ( umask 027
      cat > "$CONFIG" <<EOF
# forge-runner on the PFS trial server (ops/gitlab-trial/README.md 「自前の CI/CD」).
# Written once by runner.sh init; edit here. Keys: crates/forge-runner/src/config.rs.
server = "$url"
# The trial GitStation is plain HTTP on the LAN (as its web UI): trial only.
allow_insecure_http = true
token_file = "/etc/forge-runner/token"
work_dir = "/var/lib/forge-runner"
# Job images: pinned by digest, from Docker Hub's official images only.
image_registries = ["docker.io/library/"]
# forge-runner itself refuses a job image that is not pinned by digest
# (<name>@sha256:<64 hex>, crates/forge-runner/src/podman.rs); runner.sh
# checks this list before every start.
# One job at a time inside compose.runner.yml's mem_limit 3g.
memory = "1g"
work_dir_max_bytes = 4294967296
EOF
    )
    log "wrote $CONFIG (server $url)"
  fi
  chmod 0640 "$CONFIG"
  if [ -s "$TOKEN" ]; then log "kept $TOKEN"; else log "MISSING: $TOKEN (README: forge admin runner register)"; fi
}

preflight() {
  # Job images only from the official Docker Hub images (the runner adds
  # the digest pin itself); never an empty or wildcard registry list.
  grep -qx 'image_registries = \["docker.io/library/"\]' "$CONFIG" \
    || die "$CONFIG must have exactly image_registries = [\"docker.io/library/\"] (runner.sh init)"
  group_only "$CONFIG" 640 || die "$CONFIG must exist, owned by $(id -un), mode 640 (runner.sh init)"
  [ -s "$TOKEN" ] || die "$TOKEN is missing (README: forge admin runner register)"
  group_only "$TOKEN" 640 || die "$TOKEN must be owned by $(id -un) with mode 640"
  grep -Eqx 'fgr_[A-Za-z0-9_]+' "$TOKEN" || die "$TOKEN must hold only the fgr_… line"
  cp "$SRC/ops/gitlab-trial/compose.runner.yml" "$DEPLOY_DIR/compose.runner.yml"
  chmod 0640 "$DEPLOY_DIR/compose.runner.yml"
}

current_tag() {
  docker inspect -f '{{.Config.Image}}' "$PROJECT" 2>/dev/null | sed 's/.*://' || true
}

case "${1:-}" in
  init) init ;;
  up)
    preflight
    TAG="trial-$(git -C "$SRC" rev-parse --short=12 HEAD)"
    if docker image inspect "gitstation/forge-runner:$TAG" >/dev/null 2>&1; then
      log "image gitstation/forge-runner:$TAG exists"
    else
      log "build gitstation/forge-runner:$TAG (cargo, 2 jobs, nice)"
      nice -n 10 docker build -f "$SRC/ops/runner/Dockerfile" --build-arg RUNNER_BUILD_JOBS=2 \
        -t "gitstation/forge-runner:$TAG" "$SRC"
    fi
    compose "$TAG" config -q
    compose "$TAG" run --rm --no-deps runner check
    compose "$TAG" up -d runner
    log "running gitstation/forge-runner:$TAG (docker logs gitstation-trial-runner)"
    ;;
  check)
    preflight
    TAG="$(current_tag)"
    [ -n "$TAG" ] || die "no runner container yet: runner.sh up"
    compose "$TAG" run --rm --no-deps runner check
    ;;
  stop)
    TAG="$(current_tag)"
    [ -n "$TAG" ] || { log "no runner container"; exit 0; }
    compose "$TAG" stop runner
    ;;
  *) echo "usage: $0 init|up|check|stop" >&2; exit 2 ;;
esac
