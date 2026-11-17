#!/bin/sh
# Deploy target start-up: host key (generated once, kept in the volume),
# forge's public key bound to the dispatcher, the app supervisor, sshd.
set -eu
mkdir -p /etc/ssh/keys
[ -f /etc/ssh/keys/ssh_host_ed25519_key ] || ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/keys/ssh_host_ed25519_key
chmod 600 /etc/ssh/keys/ssh_host_ed25519_key
install -d -o deploy -g deploy -m 700 /home/deploy/.ssh
[ "$(grep -c '' /run/forge-deploy/forge-deploy.pub)" = 1 ] || { echo "entrypoint: forge-deploy.pub must be exactly one line" >&2; exit 1; }
pub=$(cat /run/forge-deploy/forge-deploy.pub)
case "$pub" in ssh-ed25519\ *) ;; *) echo "entrypoint: forge-deploy.pub is not an ed25519 public key" >&2; exit 1 ;; esac
printf 'restrict,command="/opt/forge-deploy/dispatch" %s\n' "$pub" > /home/deploy/.ssh/authorized_keys
chown deploy:deploy /home/deploy/.ssh/authorized_keys; chmod 600 /home/deploy/.ssh/authorized_keys
install -d -o root -g deploy -m 750 /etc/forge-deploy
install -o deploy -g deploy -m 600 /run/forge-deploy/read-token /etc/forge-deploy/read-token
printf 'FORGE_REPO_URL=%s\n' "${FORGE_REPO_URL:?}" > /etc/forge-deploy/hello-app.env
chmod 644 /etc/forge-deploy/hello-app.env
install -d -o deploy -g deploy -m 755 /srv/hello-app /srv/hello-app/releases
# The app: busybox httpd as `deploy`, restarted whenever a deploy stops it.
su deploy -s /bin/sh -c /opt/forge-deploy/app-supervisor &
ssh-keygen -lf /etc/ssh/keys/ssh_host_ed25519_key.pub
exec /usr/sbin/sshd -D -e
