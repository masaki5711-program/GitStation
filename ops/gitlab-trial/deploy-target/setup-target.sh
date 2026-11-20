#!/usr/bin/env bash
# Prepare a host as a forge-deploy target for the demo app hello-app
# (ops/gitlab-trial/README.md 「自前の CI/CD」, runbook deploy 「前提」).
# Run as root from a checkout of the repository, on the target host (on the
# PFS trial server: the same host as GitStation). Idempotent: re-running
# installs this kit's files again and rewrites the config and
# authorized_keys from the arguments; it never touches other users,
# containers or sshd's configuration.
#
#   sudo bash ops/gitlab-trial/deploy-target/setup-target.sh \
#     --pubkey /srv/gitstation/deploy-ssh-key.pub \
#     --repo-url http://192.0.2.10:3306/demo/hello-app.git \
#     --port 18090 [--bind 127.0.0.1]
#   sudo bash ops/gitlab-trial/deploy-target/setup-target.sh --check
#
# What it does:
#   - user gs-deploy (system user, password locked, shell bash — sshd runs the
#     forced command through it), member of the docker group: the demo runs
#     as a container. NOTE: the docker group is root on this host; what the
#     key can do is bounded by the forced command (dispatch) alone.
#   - /opt/forge-deploy/{dispatch,scripts/hello-app,hello-app/{Dockerfile,compose.yml}}
#     (root-owned, not writable by gs-deploy)
#   - /etc/forge-deploy/hello-app.env (root:gs-deploy 0640, no secret):
#     FORGE_REPO_URL, DEMO_BIND, DEMO_PORT
#   - /srv/forge-deploy/hello-app and /var/log/forge-deploy (gs-deploy, 0750)
#   - ~gs-deploy/.ssh/authorized_keys = exactly one line:
#       restrict,command="/opt/forge-deploy/dispatch" <the --pubkey key>
#   - prints the host key fingerprint and the base64 to pin in GitStation
# It does NOT write /etc/forge-deploy/hello-app-read.token (a read_repository
# token: the operator writes it, README) and does not change sshd_config.
#
# Rollback (undo everything this script did; as root):
#   docker compose -p gs-demo-hello -f /opt/forge-deploy/hello-app/compose.yml down   # this project only
#   docker image ls --format '{{.Repository}}:{{.Tag}}' gs-demo/hello-app | xargs -r docker image rm
#   userdel -r gs-deploy
#   rm -rf /opt/forge-deploy /etc/forge-deploy /srv/forge-deploy /var/log/forge-deploy
# and delete the environment's target in GitStation (README).
set -Eeuo pipefail

USER_NAME=gs-deploy
BASE=/opt/forge-deploy
ETC=/etc/forge-deploy
STATE=/srv/forge-deploy
LOGDIR=/var/log/forge-deploy
HOST_KEY=/etc/ssh/ssh_host_ed25519_key.pub
KIT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

log() { printf '[setup-target] %s\n' "$*"; }
die() { printf '[setup-target] ERROR: %s\n' "$*" >&2; exit 1; }
usage() { echo "usage: $0 --pubkey <file> --repo-url <http(s)://…/<path>.git> --port <port> [--bind <ipv4>] | --check" >&2; exit 2; }

CHECK_ONLY=0 PUBKEY="" REPO_URL="" PORT="" BIND=127.0.0.1
while [ "$#" -gt 0 ]; do
  case "$1" in
    --check) CHECK_ONLY=1; shift ;;
    --pubkey) [ "$#" -ge 2 ] || usage; PUBKEY="$2"; shift 2 ;;
    --repo-url) [ "$#" -ge 2 ] || usage; REPO_URL="$2"; shift 2 ;;
    --port) [ "$#" -ge 2 ] || usage; PORT="$2"; shift 2 ;;
    --bind) [ "$#" -ge 2 ] || usage; BIND="$2"; shift 2 ;;
    *) usage ;;
  esac
done

missing=0
todo() { missing=$((missing + 1)); log "MISSING: $*"; }

[ "$(id -u)" -eq 0 ] || die "run as root (sudo)"
for tool in git curl flock timeout docker ssh-keygen; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool is not installed"
done
docker compose version >/dev/null 2>&1 || die "docker compose (v2 plugin) is not available"
getent group docker >/dev/null || die "the docker group does not exist"
[ -s "$HOST_KEY" ] || die "$HOST_KEY is missing (is openssh-server installed?)"

if [ "$CHECK_ONLY" -eq 0 ]; then
  { [ -n "$PUBKEY" ] && [ -n "$REPO_URL" ] && [ -n "$PORT" ]; } || usage
  [ -r "$PUBKEY" ] || die "cannot read $PUBKEY"
  [ "$(grep -c '' "$PUBKEY")" = 1 ] || die "$PUBKEY must be exactly one line"
  [[ "$(cat "$PUBKEY")" =~ ^ssh-ed25519\ [A-Za-z0-9+/=]+(\ [^\"]*)?$ ]] || die "$PUBKEY is not an ed25519 public key"
  ssh-keygen -l -f "$PUBKEY" >/dev/null 2>&1 || die "$PUBKEY is not a valid public key"
  [[ "$REPO_URL" =~ ^https?://[A-Za-z0-9.:-]+/[A-Za-z0-9._/-]+\.git$ ]] || die "--repo-url must be http(s)://host[:port]/<path>.git"
  { [[ "$PORT" =~ ^[0-9]{2,5}$ ]] && [ "$PORT" -ge 1024 ] && [ "$PORT" -le 65535 ]; } || die "--port must be 1024..65535"
  { [[ "$BIND" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]] && [ "$BIND" != 0.0.0.0 ]; } || die "--bind must be one IPv4 address, never 0.0.0.0"
fi

# ── 1. the user ──
if id "$USER_NAME" >/dev/null 2>&1; then
  log "user $USER_NAME exists"
elif [ "$CHECK_ONLY" -eq 1 ]; then
  todo "user $USER_NAME"
else
  useradd --system --create-home --home-dir "/home/$USER_NAME" --shell /bin/bash "$USER_NAME"
  log "created user $USER_NAME (password locked)"
fi
if id "$USER_NAME" >/dev/null 2>&1; then
  if id -nG "$USER_NAME" | tr ' ' '\n' | grep -qx docker; then
    log "$USER_NAME is in the docker group"
  elif [ "$CHECK_ONLY" -eq 1 ]; then
    todo "$USER_NAME in the docker group"
  else
    usermod -aG docker "$USER_NAME"
    log "added $USER_NAME to the docker group"
  fi
fi

# ── 2. the kit (root-owned; gs-deploy cannot change what runs) ──
if [ "$CHECK_ONLY" -eq 1 ]; then
  for f in "$BASE/dispatch" "$BASE/scripts/hello-app" "$BASE/hello-app/Dockerfile" "$BASE/hello-app/compose.yml"; do
    [ -f "$f" ] || todo "$f"
  done
else
  install -d -o root -g root -m 0755 "$BASE" "$BASE/scripts" "$BASE/hello-app"
  install -o root -g root -m 0755 "$KIT/dispatch" "$BASE/dispatch"
  install -o root -g root -m 0755 "$KIT/hello-app" "$BASE/scripts/hello-app"
  install -o root -g root -m 0644 "$KIT/hello-app.Dockerfile" "$BASE/hello-app/Dockerfile"
  install -o root -g root -m 0644 "$KIT/hello-app.compose.yml" "$BASE/hello-app/compose.yml"
  log "installed the kit into $BASE"
fi

# ── 3. config (no secret) and the state / log directories ──
if [ "$CHECK_ONLY" -eq 1 ]; then
  [ -f "$ETC/hello-app.env" ] || todo "$ETC/hello-app.env"
  for d in "$STATE/hello-app" "$LOGDIR"; do [ -d "$d" ] || todo "directory $d"; done
else
  install -d -o root -g "$USER_NAME" -m 0750 "$ETC"
  tmp="$(mktemp "$ETC/.hello-app.env.XXXXXX")"
  printf 'FORGE_REPO_URL=%s\nDEMO_BIND=%s\nDEMO_PORT=%s\n' "$REPO_URL" "$BIND" "$PORT" > "$tmp"
  chown "root:$USER_NAME" "$tmp"; chmod 0640 "$tmp"
  mv -f "$tmp" "$ETC/hello-app.env"
  install -d -o "$USER_NAME" -g "$USER_NAME" -m 0750 "$STATE" "$STATE/hello-app" "$LOGDIR"
  log "wrote $ETC/hello-app.env (repo $REPO_URL, $BIND:$PORT)"
fi
TOKEN="$ETC/hello-app-read.token"
if [ -s "$TOKEN" ]; then
  [ "$(stat -c '%U %a' "$TOKEN")" = "$USER_NAME 600" ] || log "WARNING: $TOKEN must be owned by $USER_NAME with mode 600"
  log "kept $TOKEN"
else
  todo "$TOKEN — a read_repository token of the demo project (README 「自前の CI/CD」; one line, owner $USER_NAME, mode 600)"
fi

# ── 4. authorized_keys: forge's key, behind the dispatcher only ──
SSH_DIR="/home/$USER_NAME/.ssh"
if [ "$CHECK_ONLY" -eq 1 ]; then
  if ! grep -qx 'restrict,command="/opt/forge-deploy/dispatch" ssh-ed25519 .*' "$SSH_DIR/authorized_keys" 2>/dev/null \
     || [ "$(grep -c '' "$SSH_DIR/authorized_keys")" != 1 ]; then
    todo "$SSH_DIR/authorized_keys (one restrict,command=\"/opt/forge-deploy/dispatch\" line)"
  fi
else
  install -d -o "$USER_NAME" -g "$USER_NAME" -m 0700 "$SSH_DIR"
  tmp="$(mktemp "$SSH_DIR/.authorized_keys.XXXXXX")"
  printf 'restrict,command="%s/dispatch" %s\n' "$BASE" "$(cat "$PUBKEY")" > "$tmp"
  chown "$USER_NAME:$USER_NAME" "$tmp"; chmod 0600 "$tmp"
  mv -f "$tmp" "$SSH_DIR/authorized_keys"
  log "wrote $SSH_DIR/authorized_keys (restrict, forced command $BASE/dispatch)"
fi

# ── 5. checks that change nothing ──
if command -v sshd >/dev/null 2>&1; then
  if sshd -T 2>/dev/null | grep -Eiq '^(allowusers|allowgroups) '; then
    log "WARNING: sshd has AllowUsers/AllowGroups: make sure $USER_NAME may log in (sshd -T | grep -i allow). This script does not change the sshd configuration."
  fi
fi
if [ -n "$PORT" ] && command -v ss >/dev/null 2>&1; then
  if [ -n "$(ss -ltnH "sport = :$PORT" 2>/dev/null)" ] && ! docker port gs-demo-hello-web 8080/tcp >/dev/null 2>&1; then
    log "WARNING: port $PORT is already in use by something else (ss -ltnp); choose another --port"
  fi
fi

# ── 6. the host key to pin in GitStation (compare over another channel) ──
log "host key fingerprint: $(ssh-keygen -l -f "$HOST_KEY")"
log "host_key_type: $(cut -d' ' -f1 "$HOST_KEY")"
log "host_key (base64, for PUT …/deploy/environments/dev/targets/<name>): $(cut -d' ' -f2 "$HOST_KEY")"

if [ "$missing" -gt 0 ]; then
  log "$missing item(s) still to do (see above)"
  [ "$CHECK_ONLY" -eq 1 ] && exit 1
  exit 0
fi
log "target ready for script hello-app"
