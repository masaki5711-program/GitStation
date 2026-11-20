#!/usr/bin/env bash
# Tests of the dispatcher's argument validation (no root, no ssh, no
# network). Runs a copy of dispatch with BASE and LOG moved to a temporary
# directory and a stub hello-app that records its argument vector.
#   bash ops/gitlab-trial/deploy-target/test-dispatch.sh
# Also run by crates/forge-cli/tests/gitlab_trial_package.rs on Unix.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/base/scripts" "$T/log"
sed -e "s|^readonly BASE=.*|readonly BASE='$T/base'|" \
    -e "s|^readonly LOG=.*|readonly LOG='$T/log/dispatch.log'|" \
    "$HERE/dispatch" > "$T/dispatch"
grep -q "^readonly BASE='$T/base'" "$T/dispatch" || { echo "FAIL: could not relocate BASE"; exit 1; }
cat > "$T/base/scripts/hello-app" <<EOF
#!/usr/bin/env bash
printf '%s|' "\$#" "\$@" > '$T/ran'
EOF
chmod +x "$T/dispatch" "$T/base/scripts/hello-app"

SHA=0123456789abcdef0123456789abcdef01234567
pass=0 failed=0
ok() { pass=$((pass + 1)); }
bad() { failed=$((failed + 1)); echo "FAIL: $*"; }

accept() { # <command> <expected argv record>
  rm -f "$T/ran"
  if SSH_ORIGINAL_COMMAND="$1" "$T/dispatch" 2>/dev/null && [ "$(cat "$T/ran" 2>/dev/null)" = "$2" ]; then ok
  else bad "accept [$1] (ran: $(cat "$T/ran" 2>/dev/null || echo nothing))"; fi
}
refuse() { # <command>
  rm -f "$T/ran" "$T/pwned"
  local code=0
  SSH_ORIGINAL_COMMAND="$1" "$T/dispatch" 2>/dev/null || code=$?
  if [ "$code" -eq 64 ] && [ ! -e "$T/ran" ] && [ ! -e "$T/pwned" ]; then ok
  else bad "refuse [$(printf '%q' "$1")] (exit $code, ran: $([ -e "$T/ran" ] && echo yes || echo no))"; fi
}

for verb in deploy health rollback; do
  accept "hello-app $verb $SHA" "2|$verb|$SHA|"
done

refuse ""
refuse "hello-app"
refuse "hello-app deploy"
refuse "hello-app deploy $SHA extra"
refuse "hello-app  deploy $SHA"
refuse " hello-app deploy $SHA"
refuse "hello-app deploy $SHA "
refuse "hello-app	deploy $SHA"
refuse "hello-app deploy $SHA
id"
refuse "hello-app deploy ${SHA}0"
refuse "hello-app deploy ${SHA%?}"
refuse "hello-app deploy ${SHA}${SHA:0:24}"
refuse "hello-app deploy $(printf '%s' "$SHA" | tr 'a-f' 'A-F')"
refuse "hello-app restart $SHA"
refuse "hello-app Deploy $SHA"
refuse "other-app deploy $SHA"
refuse "../scripts/hello-app deploy $SHA"
refuse "/bin/sh deploy $SHA"
refuse "sh -c id"
refuse "hello-app deploy \$(touch $T/pwned)"
refuse "hello-app;touch $T/pwned deploy $SHA"
refuse "hello-app deploy \`touch $T/pwned\`"
refuse "hello-app deploy $SHA;touch $T/pwned"
refuse "hello-app deploy $SHA&&touch $T/pwned"
refuse "hello-app deploy *"
refuse "$(head -c 400 /dev/zero | tr '\0' a)"
# SSH_ORIGINAL_COMMAND unset (an interactive login attempt)
rm -f "$T/ran"; code=0
env -u SSH_ORIGINAL_COMMAND "$T/dispatch" 2>/dev/null || code=$?
if [ "$code" -eq 64 ] && [ ! -e "$T/ran" ]; then ok; else bad "refuse unset SSH_ORIGINAL_COMMAND (exit $code)"; fi
# The refused text is never written to the log.
if grep -q pwned "$T/log/dispatch.log"; then bad "the log contains client text"; else ok; fi
if grep -q "run hello-app deploy $SHA" "$T/log/dispatch.log"; then ok; else bad "the log has no run line"; fi

echo "dispatch tests: $pass passed, $failed failed"
[ "$failed" -eq 0 ]
