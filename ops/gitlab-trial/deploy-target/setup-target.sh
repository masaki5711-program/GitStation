#!/usr/bin/env bash
# Prepare a host as a forge-deploy target for the demo app hello-app
# (ops/gitlab-trial/README.md 「自前の CI/CD」, runbook deploy 「前提」).
# Run as root from a checkout of the repository, on the target host (on the
# PFS trial server: the same host as GitStation). Idempotent: re-running
# installs this kit's files again and rewrites the config and
# authorized_keys from the arguments; it never installs packages and never
# touches other users, containers or sshd's configuration.
#
#   sudo bash ops/gitlab-trial/deploy-target/setup-target.sh \
#     --pubkey /srv/gitstation/deploy-ssh-key.pub \
#     --from 192.0.2.128/25 \
#     --repo-url http://192.0.2.10:3306/demo/hello-app.git \
#     --port 18090 [--bind 127.0.0.1]
#   sudo bash ops/gitlab-trial/deploy-target/setup-target.sh --check
#
# --from: where forge's ssh connections come from (authorized_keys from=):
# the subnet of the worker's network gitstation-trial-net, e.g.
#   docker network inspect gitstation-trial-net -f '{{(index .IPAM.Config 0).Subnet}}'
#
# Requires podman and uidmap (newuidmap / newgidmap); when missing it prints
# the apt-get line and stops. What it does:
#   - user gs-deploy (system user, password locked, shell bash — sshd runs the
#     forced command through it). NOT in the docker group (removed if it is):
#     the demo runs with ROOTLESS Podman as gs-deploy, so the key cannot reach
#     root even through the app script.
#   - subuid / subgid ranges for gs-deploy (65536 ids after every existing
#     range) and `loginctl enable-linger gs-deploy` (its /run/user/<uid>)
#   - /opt/forge-deploy/{dispatch,scripts/hello-app,hello-app/Containerfile}
#     (root-owned, not writable by gs-deploy)
#   - /etc/forge-deploy/hello-app.env (root:gs-deploy 0640, no secret):
#     FORGE_REPO_URL, DEMO_BIND, DEMO_PORT
#   - /srv/forge-deploy/hello-app and /var/log/forge-deploy (gs-deploy, 0750)
#   - ~gs-deploy/.ssh/authorized_keys = exactly one line:
#       restrict,from="<--from>",command="/opt/forge-deploy/dispatch" <the --pubkey key>
#   - prints the host key fingerprint and the base64 to pin in GitStation
# It does NOT write /etc/forge-deploy/hello-app-read.token (a read_repository
# token: the operator writes it, README).
#
# Rollback (undo everything this script did; as root):
#   runuser -u gs-deploy -- env XDG_RUNTIME_DIR=/run/user/$(id -u gs-deploy) podman rm -f --ignore gs-demo-hello-web
#   runuser -u gs-deploy -- env XDG_RUNTIME_DIR=/run/user/$(id -u gs-deploy) podman system reset --force   # gs-deploy's own storage only
#   loginctl disable-linger gs-deploy
#   userdel -r gs-deploy        # also its lines in /etc/subuid and /etc/subgid (check: grep gs-deploy /etc/sub[ug]id)
#   rm -rf /opt/forge-deploy /etc/forge-deploy /srv/forge-deploy /var/log/forge-deploy /etc/logrotate.d/forge-deploy
# and stop the environment's target in GitStation (README).
set -Eeuo pipefail
export LC_ALL=C

USER_NAME=gs-deploy
BASE=/opt/forge-deploy
ETC=/etc/forge-deploy
STATE=/srv/forge-deploy
LOGDIR=/var/log/forge-deploy
HOST_KEY=/etc/ssh/ssh_host_ed25519_key.pub
KIT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUB_COUNT=65536

log() { printf '[setup-target] %s\n' "$*"; }
die() { printf '[setup-target] ERROR: %s\n' "$*" >&2; exit 1; }
usage() { echo "usage: $0 --pubkey <file> --from <ipv4[/len][,…]> --repo-url <http(s)://…/<path>.git> --port <1025..65535> [--bind <ipv4>] | --check" >&2; exit 2; }

valid_ipv4() { # one dotted quad, every octet 0..255
  local IFS=. o
  [[ "$1" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]] || return 1
  for o in $1; do [ "$((10#$o))" -le 255 ] || return 1; done
}
valid_from() { # comma-separated IPv4 addresses or CIDRs (no wildcard, no 0.0.0.0/0)
  local IFS=, item addr len
  [ -n "$1" ] || return 1
  for item in $1; do
    addr="${item%/*}"; len=32
    [ "$item" = "$addr" ] || len="${item#*/}"
    valid_ipv4 "$addr" || return 1
    [[ "$len" =~ ^[0-9]{1,2}$ ]] && [ "$len" -ge 8 ] && [ "$len" -le 32 ] || return 1
  done
}

CHECK_ONLY=0 PUBKEY="" FROM="" REPO_URL="" PORT="" BIND=127.0.0.1
while [ "$#" -gt 0 ]; do
  case "$1" in
    --check) CHECK_ONLY=1; shift ;;
    --pubkey) [ "$#" -ge 2 ] || usage; PUBKEY="$2"; shift 2 ;;
    --from) [ "$#" -ge 2 ] || usage; FROM="$2"; shift 2 ;;
    --repo-url) [ "$#" -ge 2 ] || usage; REPO_URL="$2"; shift 2 ;;
    --port) [ "$#" -ge 2 ] || usage; PORT="$2"; shift 2 ;;
    --bind) [ "$#" -ge 2 ] || usage; BIND="$2"; shift 2 ;;
    *) usage ;;
  esac
done

missing=0
todo() { missing=$((missing + 1)); log "MISSING: $*"; }

[ "$(id -u)" -eq 0 ] || die "run as root (sudo)"
for tool in git curl flock timeout ssh-keygen loginctl; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool is not installed"
done
if ! command -v podman >/dev/null 2>&1 || ! command -v newuidmap >/dev/null 2>&1 || ! command -v newgidmap >/dev/null 2>&1; then
  printf '[setup-target] ERROR: rootless Podman is not installed. Install it (this script never installs packages):\n' >&2
  printf '  apt-get install podman uidmap\n' >&2
  exit 1
fi
[ -s "$HOST_KEY" ] || die "$HOST_KEY is missing (is openssh-server installed?)"

if [ "$CHECK_ONLY" -eq 0 ]; then
  { [ -n "$PUBKEY" ] && [ -n "$REPO_URL" ] && [ -n "$PORT" ]; } || usage
  valid_from "$FROM" || die "--from is required: the worker network's IPv4 address or CIDR (/8../32), comma-separated (see the header)"
  [ -r "$PUBKEY" ] || die "cannot read $PUBKEY"
  [ "$(grep -c '' "$PUBKEY")" = 1 ] || die "$PUBKEY must be exactly one line"
  [[ "$(cat "$PUBKEY")" =~ ^ssh-ed25519\ [A-Za-z0-9+/=]+(\ [^\"]*)?$ ]] || die "$PUBKEY is not an ed25519 public key"
  ssh-keygen -l -f "$PUBKEY" >/dev/null 2>&1 || die "$PUBKEY is not a valid public key"
  [[ "$REPO_URL" =~ ^https?://[A-Za-z0-9.:-]+/[A-Za-z0-9._/-]+\.git$ ]] || die "--repo-url must be http(s)://host[:port]/<path>.git"
  { [[ "$PORT" =~ ^[0-9]{4,5}$ ]] && [ "$PORT" -ge 1025 ] && [ "$PORT" -le 65535 ]; } || die "--port must be 1025..65535 (rootless)"
  { valid_ipv4 "$BIND" && [ "$BIND" != 0.0.0.0 ]; } || die "--bind must be one IPv4 address, never 0.0.0.0"
fi

# ── 1. the user (never in the docker group) ──
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
    if [ "$CHECK_ONLY" -eq 1 ]; then
      todo "remove $USER_NAME from the docker group (root-equivalent)"
    else
      gpasswd -d "$USER_NAME" docker >/dev/null
      log "removed $USER_NAME from the docker group"
    fi
  fi
fi

# ── 2. rootless Podman: subordinate ids and linger ──
next_free_id() { # the first id after every range of /etc/subuid and /etc/subgid
  awk -F: 'BEGIN { m = 100000 } NF == 3 { e = $2 + $3; if (e > m) m = e } END { print m }' /etc/subuid /etc/subgid 2>/dev/null
}
if id "$USER_NAME" >/dev/null 2>&1; then
  changed=0
  for f in /etc/subuid /etc/subgid; do
    if grep -q "^$USER_NAME:" "$f" 2>/dev/null; then
      log "$f has a range for $USER_NAME"
    elif [ "$CHECK_ONLY" -eq 1 ]; then
      todo "$f range for $USER_NAME"
    else
      start="$(next_free_id)"
      [ "$f" = /etc/subuid ] && usermod --add-subuids "$start-$((start + SUB_COUNT - 1))" "$USER_NAME"
      [ "$f" = /etc/subgid ] && usermod --add-subgids "$start-$((start + SUB_COUNT - 1))" "$USER_NAME"
      log "added $f range $start+$SUB_COUNT for $USER_NAME"
      changed=1
    fi
  done
  if [ -e "/var/lib/systemd/linger/$USER_NAME" ]; then
    log "linger is enabled for $USER_NAME"
  elif [ "$CHECK_ONLY" -eq 1 ]; then
    todo "loginctl enable-linger $USER_NAME"
  else
    loginctl enable-linger "$USER_NAME"
    log "enabled linger for $USER_NAME"
  fi
  if [ "$changed" -eq 1 ]; then
    uid="$(id -u "$USER_NAME")"
    for _ in $(seq 1 10); do [ -d "/run/user/$uid" ] && break; sleep 1; done
    runuser -u "$USER_NAME" -- env XDG_RUNTIME_DIR="/run/user/$uid" podman system migrate \
      || log "WARNING: podman system migrate failed for $USER_NAME"
  fi
fi

# ── 3. the kit (root-owned; gs-deploy cannot change what runs) ──
if [ "$CHECK_ONLY" -eq 1 ]; then
  for f in "$BASE/dispatch" "$BASE/scripts/hello-app" "$BASE/hello-app/Containerfile"; do
    [ -f "$f" ] || todo "$f"
  done
else
  install -d -o root -g root -m 0755 "$BASE" "$BASE/scripts" "$BASE/hello-app"
  install -o root -g root -m 0755 "$KIT/dispatch" "$BASE/dispatch"
  install -o root -g root -m 0755 "$KIT/hello-app" "$BASE/scripts/hello-app"
  install -o root -g root -m 0644 "$KIT/hello-app.Containerfile" "$BASE/hello-app/Containerfile"
  rm -f "$BASE/hello-app/Dockerfile" "$BASE/hello-app/compose.yml"   # an earlier kit's files
  log "installed the kit into $BASE"
fi

# ── 4. config (no secret) and the state / log directories ──
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

# ── 5. authorized_keys: forge's key, from the worker's network, behind the dispatcher only ──
SSH_DIR="/home/$USER_NAME/.ssh"
if [ "$CHECK_ONLY" -eq 1 ]; then
  if ! grep -Eqx 'restrict,from="[0-9./,]+",command="/opt/forge-deploy/dispatch" ssh-ed25519 .*' "$SSH_DIR/authorized_keys" 2>/dev/null \
     || [ "$(grep -c '' "$SSH_DIR/authorized_keys")" != 1 ]; then
    todo "$SSH_DIR/authorized_keys (one restrict,from=\"…\",command=\"/opt/forge-deploy/dispatch\" line)"
  fi
else
  install -d -o "$USER_NAME" -g "$USER_NAME" -m 0700 "$SSH_DIR"
  tmp="$(mktemp "$SSH_DIR/.authorized_keys.XXXXXX")"
  printf 'restrict,from="%s",command="%s/dispatch" %s\n' "$FROM" "$BASE" "$(cat "$PUBKEY")" > "$tmp"
  chown "$USER_NAME:$USER_NAME" "$tmp"; chmod 0600 "$tmp"
  mv -f "$tmp" "$SSH_DIR/authorized_keys"
  log "wrote $SSH_DIR/authorized_keys (restrict, from=$FROM, forced command $BASE/dispatch)"
fi

# ── 6. checks that change nothing ──
if command -v sshd >/dev/null 2>&1; then
  if sshd -T 2>/dev/null | grep -Eiq '^(allowusers|allowgroups) '; then
    log "WARNING: sshd has AllowUsers/AllowGroups: make sure $USER_NAME may log in (sshd -T | grep -i allow). This script does not change the sshd configuration."
  fi
fi
if [ -n "$PORT" ] && command -v ss >/dev/null 2>&1; then
  if [ -n "$(ss -ltnpH "sport = :$PORT" 2>/dev/null | grep -v rootlessport || true)" ]; then
    log "WARNING: port $PORT may be in use by something else (ss -ltnp); choose another --port"
  fi
fi

# ── 7. the host key to pin in GitStation (compare over another channel) ──
log "host key fingerprint: $(ssh-keygen -l -f "$HOST_KEY")"
log "host_key_type: $(cut -d' ' -f1 "$HOST_KEY")"
log "host_key (base64, for PUT …/deploy/environments/dev/targets/<name>): $(cut -d' ' -f2 "$HOST_KEY")"

if [ "$missing" -gt 0 ]; then
  log "$missing item(s) still to do (see above)"
  [ "$CHECK_ONLY" -eq 1 ] && exit 1
  exit 0
fi
log "target ready for script hello-app"
