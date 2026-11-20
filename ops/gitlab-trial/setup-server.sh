#!/usr/bin/env bash
# One-time (and re-runnable) root part of the GitStation deploy to the PFS
# trial server through GitLab CI/CD (ops/gitlab-trial/README.md).
#
#   sudo bash ops/gitlab-trial/setup-server.sh            # create what is missing
#   sudo bash ops/gitlab-trial/setup-server.sh --check    # report only, change nothing
#
# Creates, only when missing (idempotent; an existing secret is NEVER
# overwritten or printed):
#   - the runner user gitstation-ci (member of the docker group) and its home
#   - /srv/gitstation (0750) with data/repositories (0750), backups/ (0700),
#     secrets/ and secrets-privileged/ (0700), all owned by gitstation-ci
#   - generated secrets (0600): the forge keys as in runbook linux-install
#     step 5, the role passwords of forge_app / forge_migrator / forge_maint
#     and the PostgreSQL superuser password
#   - /srv/gitstation/.env (0600) from a template the operator then edits
#   - the forge-deploy ssh key (ed25519): the private half
#     secrets/deploy-ssh-key (0600), the public half deploy-ssh-key.pub
#     (0644, not a secret; it goes to the deploy target's authorized_keys,
#     deploy-target/setup-target.sh). Only the PUBLIC key is ever printed.
# It does NOT create secrets/oidc-client-secret (the Keycloak client's
# secret: the operator writes it) and does not register the GitLab runner
# (README "runner の登録").
set -Eeuo pipefail

GS_USER="${GS_USER:-gitstation-ci}"
GS_DIR="${GS_DIR:-/srv/gitstation}"
CHECK_ONLY=0
case "${1:-}" in
  "") ;;
  --check) CHECK_ONLY=1 ;;
  *) echo "usage: $0 [--check]" >&2; exit 2 ;;
esac

log() { printf '[setup] %s\n' "$*"; }
die() { printf '[setup] ERROR: %s\n' "$*" >&2; exit 1; }
missing=0
todo() { missing=$((missing + 1)); log "MISSING: $*"; }

[ "$(id -u)" -eq 0 ] || die "run as root (sudo)"
command -v docker >/dev/null 2>&1 || die "docker is not installed"
getent group docker >/dev/null || die "the docker group does not exist"

# 64 hex characters from the kernel CSPRNG (no openssl needed).
hex32() { od -An -N32 -tx1 /dev/urandom | tr -d ' \n'; }

# ── 1. the runner user ──
if id "$GS_USER" >/dev/null 2>&1; then
  log "user $GS_USER exists"
elif [ "$CHECK_ONLY" -eq 1 ]; then
  todo "user $GS_USER"
else
  useradd --create-home --shell /bin/bash "$GS_USER"
  log "created user $GS_USER"
fi
if id "$GS_USER" >/dev/null 2>&1; then
  if id -nG "$GS_USER" | tr ' ' '\n' | grep -qx docker; then
    log "$GS_USER is in the docker group"
  elif [ "$CHECK_ONLY" -eq 1 ]; then
    todo "$GS_USER in the docker group"
  else
    usermod -aG docker "$GS_USER"
    log "added $GS_USER to the docker group"
  fi
  # Ubuntu's default ~/.bash_logout clears the console and fails shell-executor jobs.
  home="$(getent passwd "$GS_USER" | cut -d: -f6)"
  if [ -f "$home/.bash_logout" ]; then
    if [ "$CHECK_ONLY" -eq 1 ]; then todo "remove $home/.bash_logout"; else rm -f "$home/.bash_logout"; log "removed $home/.bash_logout"; fi
  fi
fi

# ── 2. directories ──
dir() { # <path> <mode>
  if [ -d "$1" ]; then
    local owner mode
    owner="$(stat -c %U "$1")"; mode="$(stat -c %a "$1")"
    [ "$owner" = "$GS_USER" ] || log "WARNING: $1 is owned by $owner (expected $GS_USER)"
    [ "$mode" = "$2" ] || log "WARNING: $1 has mode $mode (expected $2)"
  elif [ "$CHECK_ONLY" -eq 1 ]; then
    todo "directory $1"
  else
    install -d -m "$2" -o "$GS_USER" -g "$GS_USER" "$1"
    log "created $1 ($2)"
  fi
}
dir "$GS_DIR" 750
dir "$GS_DIR/data" 750
dir "$GS_DIR/data/repositories" 750
dir "$GS_DIR/backups" 700
dir "$GS_DIR/secrets" 700
dir "$GS_DIR/secrets-privileged" 700

# ── 3. secrets (never overwritten, never printed) ──
secret() { # <path> <kind: keyline|hex>
  if [ -s "$1" ]; then
    local mode
    mode="$(stat -c %a "$1")"
    [ "$mode" = "600" ] || log "WARNING: $1 has mode $mode (forge refuses anything but owner-only: chmod 600)"
    log "kept $1"
    return
  fi
  if [ "$CHECK_ONLY" -eq 1 ]; then todo "secret $1"; return; fi
  local tmp
  tmp="$(mktemp "$1.XXXXXX")"
  chmod 600 "$tmp"
  case "$2" in
    keyline) printf '1 %s\n' "$(hex32)" > "$tmp" ;;   # <kid> <64 hex>, ADR-0013
    hex) printf '%s\n' "$(hex32)" > "$tmp" ;;
  esac
  chown "$GS_USER:$GS_USER" "$tmp"
  mv -n "$tmp" "$1"
  rm -f "$tmp"
  log "generated $1"
}
for name in cursor-keys audit-seal-keys web-session-keys ci-secret-keys webhook-secret-keys; do
  secret "$GS_DIR/secrets/$name" keyline
done
secret "$GS_DIR/secrets/db-app.pw" hex
for name in db-migrate.pw db-maint.pw pg-superuser.pw; do
  secret "$GS_DIR/secrets-privileged/$name" hex
done
# The forge-deploy key (ADR-0031): generated here, never overwritten; the
# private file is never printed, only the public key.
DEPLOY_KEY="$GS_DIR/secrets/deploy-ssh-key"
DEPLOY_PUB="$GS_DIR/deploy-ssh-key.pub"
if [ -s "$DEPLOY_KEY" ]; then
  mode="$(stat -c %a "$DEPLOY_KEY")"
  [ "$mode" = "600" ] || log "WARNING: $DEPLOY_KEY has mode $mode (ssh and forge refuse it: chmod 600)"
  log "kept $DEPLOY_KEY"
elif [ "$CHECK_ONLY" -eq 1 ]; then
  todo "secret $DEPLOY_KEY"
elif [ -d "$GS_DIR/secrets" ]; then
  command -v ssh-keygen >/dev/null 2>&1 || die "ssh-keygen is not installed (openssh-client)"
  keydir="$(mktemp -d "$GS_DIR/secrets/.deploy-ssh-key.XXXXXX")"
  ssh-keygen -q -t ed25519 -N '' -C "gitstation-trial-deploy" -f "$keydir/key" </dev/null >/dev/null
  chmod 600 "$keydir/key"
  chown "$GS_USER:$GS_USER" "$keydir/key"
  mv -n "$keydir/key" "$DEPLOY_KEY"
  rm -rf "$keydir"
  log "generated $DEPLOY_KEY"
fi
if [ -s "$DEPLOY_KEY" ]; then
  # The public half, derived from the private key (kept in step with it).
  pub="$(ssh-keygen -y -f "$DEPLOY_KEY" 2>/dev/null)" || die "cannot read the public half of $DEPLOY_KEY"
  if [ "$(cat "$DEPLOY_PUB" 2>/dev/null)" != "$pub" ]; then
    if [ "$CHECK_ONLY" -eq 1 ]; then
      todo "$DEPLOY_PUB (the public half of $DEPLOY_KEY)"
    else
      printf '%s\n' "$pub" > "$DEPLOY_PUB.tmp"
      chmod 644 "$DEPLOY_PUB.tmp"
      chown "$GS_USER:$GS_USER" "$DEPLOY_PUB.tmp"
      mv -f "$DEPLOY_PUB.tmp" "$DEPLOY_PUB"
      log "wrote $DEPLOY_PUB"
    fi
  fi
  log "deploy public key (for deploy-target/setup-target.sh --pubkey $DEPLOY_PUB): $pub"
fi

if [ -s "$GS_DIR/secrets/oidc-client-secret" ]; then
  log "kept $GS_DIR/secrets/oidc-client-secret"
else
  todo "$GS_DIR/secrets/oidc-client-secret — write the Keycloak client 'gitstation' secret (one line, mode 600, owner $GS_USER; README 手順 3)"
fi

# ── 4. the server's .env (no secret in it) ──
ENV_FILE="$GS_DIR/.env"
if [ -f "$ENV_FILE" ]; then
  log "kept $ENV_FILE"
  if grep -v '^[[:space:]]*#' "$ENV_FILE" | grep -q '<'; then todo "$ENV_FILE still has <placeholders>"; fi
elif [ "$CHECK_ONLY" -eq 1 ]; then
  todo "$ENV_FILE"
else
  tmp="$(mktemp "$ENV_FILE.XXXXXX")"
  chmod 600 "$tmp"
  cat > "$tmp" <<'EOF'
# GitStation on this server (ops/gitlab-trial/README.md). No secret here:
# secrets live in secrets/ and secrets-privileged/ next to this file.
# The server's LAN address that publishes the web UI, and the port.
PUBLISH_ADDR=<server LAN address>
PUBLISH_PORT=3306
# What people type into the browser (web.base_url). Must match the
# Keycloak client's redirect URI: <PUBLIC_URL>/-/auth/callback
PUBLIC_URL=http://<server LAN address>:3306
# The PFS Keycloak realm, exactly as its discovery document's "issuer".
OIDC_ISSUER=http://<server LAN address>:18180/realms/iotpf
# Optional: where forge-deploy may connect (CIDRs, comma-separated), e.g.
# this server's LAN address with /32 for the demo app (README 自前の CI/CD).
#DEPLOY_ALLOWED_NETWORKS=
EOF
  chown "$GS_USER:$GS_USER" "$tmp"
  mv -n "$tmp" "$ENV_FILE"
  rm -f "$tmp"
  log "created $ENV_FILE — replace every <…> before the first deploy"
  todo "$ENV_FILE has <placeholders>"
fi

if [ "$missing" -gt 0 ]; then
  log "$missing item(s) still to do (see above)"
  [ "$CHECK_ONLY" -eq 1 ] && exit 1
  exit 0
fi
log "server ready for deploy:pfs-trial"
