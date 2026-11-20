#!/usr/bin/env bash
# Deploy GitStation to the PFS trial server. Run by the GitLab job
# deploy:pfs-trial on the project's own shell runner, as the runner user
# gitstation-ci (docker group). ops/gitlab-trial/README.md.
#
#   IMAGE_TAG=<commit short sha> bash ops/gitlab-trial/deploy.sh
#
# The image gitstation/forge:$IMAGE_TAG must exist on this host (job
# build:image). Steps: preflight (image, .env, secrets, directories) →
# install this commit's compose.yml and forge.toml into $GS_DEPLOY_DIR →
# start the database → pg_dump backup (newest $GS_KEEP_BACKUPS kept) → stop
# serve and worker → database bootstrap when missing or older than this
# commit's (ops/db/bootstrap) → forge db migrate → start serve and worker
# and wait until /readyz answers → append deployed.log.
#
# Never `down -v`, never removes a volume or a backup of this run. On a
# failure after the stop it prints the logs and tries to start the previous
# image again; when this run applied migrations the previous binary refuses
# the newer schema (FG-DB-0004 db_ahead) — then follow README "前の版に戻す".
set -Eeuo pipefail

: "${IMAGE_TAG:?IMAGE_TAG is required (the commit short sha built by build:image)}"
DEPLOY_DIR="${GS_DEPLOY_DIR:-/srv/gitstation}"
PROJECT=gitstation-trial
KEEP_BACKUPS="${GS_KEEP_BACKUPS:-10}"
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ENV_FILE="$DEPLOY_DIR/.env"
IMAGE="gitstation/forge:$IMAGE_TAG"
DB_CONTAINER="$PROJECT-db"
SERVE_CONTAINER="$PROJECT-serve"
BOOTSTRAP="$SRC/ops/db/bootstrap"

log() { printf '[deploy %s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
die() { log "ERROR: $*"; exit 1; }

compose() { # <tag> <args…>
  local tag="$1"; shift
  IMAGE_TAG="$tag" docker compose -p "$PROJECT" --project-directory "$DEPLOY_DIR" \
    --env-file "$ENV_FILE" -f "$DEPLOY_DIR/compose.yml" "$@"
}

# psql as the superuser through the container's local socket (no password).
psql_su() { # <database> <psql args…>
  local db="$1"; shift
  docker exec -i -u postgres "$DB_CONTAINER" psql -X -q -v ON_ERROR_STOP=1 -d "$db" "$@"
}

owner_only() { # <path>: owned by me, no group / other bits
  [ -O "$1" ] && [ $(( 8#$(stat -c %a "$1") & 8#077 )) -eq 0 ]
}

# ── 0. preflight (nothing changes before all of it passes) ──
docker image inspect "$IMAGE" >/dev/null 2>&1 \
  || die "image $IMAGE not found on this host — run build:image of this pipeline first (README 前の版に戻す)"
[ -r "$ENV_FILE" ] || die "$ENV_FILE is missing — run setup-server.sh as root (README 手順 2)"
owner_only "$ENV_FILE" || die "$ENV_FILE must be owned by $(id -un) with mode 600"
if grep -q '<' "$ENV_FILE"; then die "$ENV_FILE still has <placeholders> (README 手順 2)"; fi
for d in "$DEPLOY_DIR/secrets" "$DEPLOY_DIR/secrets-privileged" "$DEPLOY_DIR/backups"; do
  if ! { [ -d "$d" ] && owner_only "$d"; }; then
    die "$d must be a directory owned by $(id -un) with mode 700 (setup-server.sh)"
  fi
done
REPOS="$DEPLOY_DIR/data/repositories"
if ! { [ -d "$REPOS" ] && [ -O "$REPOS" ] && [ -w "$REPOS" ]; }; then
  die "$REPOS must exist and be owned by $(id -un) (setup-server.sh)"
fi
for f in secrets/db-app.pw secrets/cursor-keys secrets/audit-seal-keys secrets/web-session-keys \
         secrets/ci-secret-keys secrets/webhook-secret-keys secrets/oidc-client-secret \
         secrets-privileged/db-migrate.pw secrets-privileged/db-maint.pw secrets-privileged/pg-superuser.pw; do
  [ -s "$DEPLOY_DIR/$f" ] || die "secret $DEPLOY_DIR/$f is missing or empty (setup-server.sh; oidc-client-secret: README 手順 3)"
  owner_only "$DEPLOY_DIR/$f" || die "secret $DEPLOY_DIR/$f must be owned by $(id -un) with mode 600"
done
# The role passwords go into SQL at the bootstrap: hex only (setup-server.sh generates them).
for f in secrets/db-app.pw secrets-privileged/db-migrate.pw secrets-privileged/db-maint.pw; do
  grep -Eqx '[0-9a-f]{32,128}' "$DEPLOY_DIR/$f" || die "$f must be one line of hex (setup-server.sh generates it)"
done
[ -f "$BOOTSTRAP/03-database-objects.sql" ] || die "$BOOTSTRAP is missing from this checkout"

PREV_TAG="$(docker inspect -f '{{.Config.Image}}' "$SERVE_CONTAINER" 2>/dev/null | sed 's/.*://' || true)"
log "deploying $IMAGE_TAG (running now: ${PREV_TAG:-none}) to project $PROJECT in $DEPLOY_DIR"

# ── 1. this commit's compose.yml and forge.toml, validated before they replace the old ones ──
cp "$SRC/ops/gitlab-trial/compose.yml" "$DEPLOY_DIR/compose.yml.new"
cp "$SRC/ops/gitlab-trial/forge.toml" "$DEPLOY_DIR/forge.toml.new"
chmod 640 "$DEPLOY_DIR/compose.yml.new" "$DEPLOY_DIR/forge.toml.new"
IMAGE_TAG="$IMAGE_TAG" docker compose -p "$PROJECT" --project-directory "$DEPLOY_DIR" --env-file "$ENV_FILE" \
  -f "$DEPLOY_DIR/compose.yml.new" config -q \
  || die "compose.yml of this commit does not validate against $ENV_FILE (a new required variable?)"
mv -f "$DEPLOY_DIR/compose.yml.new" "$DEPLOY_DIR/compose.yml"
mv -f "$DEPLOY_DIR/forge.toml.new" "$DEPLOY_DIR/forge.toml"

# ── 2. the database ──
log "database"
compose "$IMAGE_TAG" up -d --wait --wait-timeout 180 db
DB_EXISTS="$(psql_su postgres -tAc "SELECT 1 FROM pg_catalog.pg_database WHERE datname = 'forge'")"

# ── 3. backup of this run (custom format, pg_restore). Newest $KEEP_BACKUPS kept. ──
BACKUP=""
if [ "$DB_EXISTS" = "1" ]; then
  BACKUP="$DEPLOY_DIR/backups/pre-deploy-$(date -u +%Y%m%d%H%M%S)-$IMAGE_TAG.dump"
  log "backup → $BACKUP"
  ( umask 077; docker exec -u postgres "$DB_CONTAINER" pg_dump -Fc -d forge > "$BACKUP" )
  [ -s "$BACKUP" ] || die "the backup $BACKUP is empty"
  # shellcheck disable=SC2012 # names are ours: pre-deploy-<UTC>-<sha>.dump
  ls -1t "$DEPLOY_DIR"/backups/pre-deploy-*.dump 2>/dev/null | tail -n +"$((KEEP_BACKUPS + 1))" | xargs -r rm -f
else
  log "no database 'forge' yet: first deploy, nothing to back up"
fi

on_failure() {
  local code=$?
  trap - ERR
  log "FAILED (exit $code)"
  compose "$IMAGE_TAG" ps -a || true
  compose "$IMAGE_TAG" logs --no-color --tail 80 serve worker || true
  if [ -n "$PREV_TAG" ] && [ "$PREV_TAG" != "$IMAGE_TAG" ] && docker image inspect "gitstation/forge:$PREV_TAG" >/dev/null 2>&1; then
    log "starting the previous version $PREV_TAG again (DB backup of this run: ${BACKUP:-none})"
    compose "$PREV_TAG" up -d --wait --wait-timeout 180 serve worker \
      || log "the previous version did not become healthy — if its log says FG-DB-0004 db_ahead, see README 前の版に戻す"
  else
    log "no previous version to start (DB backup of this run: ${BACKUP:-none})"
  fi
  exit "$code"
}
trap on_failure ERR

# ── 4. stop the old version (no forge process during bootstrap / migrate) ──
log "stop serve worker"
compose "$IMAGE_TAG" stop serve worker

# ── 5. bootstrap (ADR-0018 §1, runbook db-bootstrap): first deploy, or a newer 03 ──
SHIPPED="$(sed -n 's/.*bootstrap_log (bootstrap_version) VALUES (\([0-9][0-9]*\)).*/\1/p' "$BOOTSTRAP/03-database-objects.sql" | tail -n 1)"
[ -n "$SHIPPED" ] || die "cannot read the bootstrap version of $BOOTSTRAP/03-database-objects.sql"
if [ "$DB_EXISTS" != "1" ]; then
  log "bootstrap (first deploy): roles, passwords, database, objects v$SHIPPED"
  psql_su postgres -f - < "$BOOTSTRAP/01-cluster-roles.sql"
  # Passwords on stdin, never in argv or this log (hex only: checked above).
  {
    printf 'SET search_path = pg_catalog, pg_temp;\n'
    printf "ALTER ROLE forge_app PASSWORD '%s';\n" "$(head -n 1 "$DEPLOY_DIR/secrets/db-app.pw")"
    printf "ALTER ROLE forge_migrator PASSWORD '%s';\n" "$(head -n 1 "$DEPLOY_DIR/secrets-privileged/db-migrate.pw")"
    printf "ALTER ROLE forge_maint PASSWORD '%s';\n" "$(head -n 1 "$DEPLOY_DIR/secrets-privileged/db-maint.pw")"
  } | psql_su postgres -f -
  psql_su postgres -v dbname=forge -f - < "$BOOTSTRAP/02-create-database.sql"
  psql_su forge -f - < "$BOOTSTRAP/03-database-objects.sql"
else
  CURRENT=0
  # A first deploy that stopped inside 03 leaves the database without the log.
  if [ "$(psql_su forge -tAc "SELECT to_regclass('forge_audit.bootstrap_log') IS NOT NULL")" = "t" ]; then
    CURRENT="$(psql_su forge -tAc 'SELECT coalesce(max(bootstrap_version), 0) FROM forge_audit.bootstrap_log')"
  fi
  if [ "$CURRENT" -lt "$SHIPPED" ]; then
    log "bootstrap update v$CURRENT → v$SHIPPED (01 and 03 are re-runnable)"
    psql_su postgres -f - < "$BOOTSTRAP/01-cluster-roles.sql"
    psql_su forge -f - < "$BOOTSTRAP/03-database-objects.sql"
  else
    log "bootstrap v$CURRENT is current"
  fi
fi

# ── 6. migrate (forge_migrator, only the migrate service sees its password) ──
log "migrate"
compose "$IMAGE_TAG" --profile ops run --rm migrate

# ── 7. the new version ──
log "start serve worker"
compose "$IMAGE_TAG" up -d --wait --wait-timeout 240 serve worker

# ── 8. smoke (read-only): ready inside the container, and on the published address ──
# shellcheck disable=SC1090 # the server's own .env (checked above: owner-only)
PUBLISH="$(set -a; . "$ENV_FILE"; printf '%s:%s' "$PUBLISH_ADDR" "${PUBLISH_PORT:-3306}")"
ready=""
for _ in $(seq 1 30); do
  if docker exec "$SERVE_CONTAINER" curl -fsS -o /dev/null --max-time 5 http://127.0.0.1:8080/readyz; then ready=1; break; fi
  sleep 3
done
[ -n "$ready" ] || { log "/readyz did not answer 200 within 90 s"; false; }
if command -v curl >/dev/null 2>&1; then
  curl -fsS -o /dev/null --max-time 5 "http://$PUBLISH/healthz" \
    || { log "http://$PUBLISH/healthz is not reachable from this host (PUBLISH_ADDR / firewall)"; false; }
fi
trap - ERR

printf '%s %s pipeline=%s by=%s previous=%s\n' "$(date -u +%FT%TZ)" "$IMAGE_TAG" \
  "${CI_PIPELINE_ID:-manual}" "${GITLAB_USER_LOGIN:-$(id -un)}" "${PREV_TAG:-none}" >> "$DEPLOY_DIR/deployed.log"
log "done: $IMAGE_TAG is live on http://$PUBLISH (previous: ${PREV_TAG:-none}; backup: ${BACKUP:-none})"
