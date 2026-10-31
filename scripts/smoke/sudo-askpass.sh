#!/usr/bin/env bash
# sudo-askpass.sh — manual smoke test for the bash tool's sudo askpass path.
#
# Uses the REAL sudo, so run it by hand in a terminal, never from CI or an
# agent. Linux only. Nothing here authenticates: the automated steps make
# sudo call the helper and then fail (each counts as one failed sudo
# attempt in the auth log). The interactive steps run inside Synaps.
#
# Usage: bash scripts/smoke/sudo-askpass.sh [path/to/synaps]
#   Default binary: target/debug/synaps (build it first: cargo build --bin synaps)

set -euo pipefail

BIN="${1:-${SYNAPS_BIN:-target/debug/synaps}}"
BIN="$(realpath "$BIN")"
[[ -x "$BIN" ]] || { echo "no synaps binary at $BIN" >&2; exit 2; }
[[ "$(uname -s)" == Linux ]] || { echo "Linux only" >&2; exit 2; }

pass=0
fail=0
ok() { echo "  PASS: $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL: $*"; fail=$((fail + 1)); }

# A private dir laid out like the bash tool's (link + token), with no
# broker listening.
dir="$(mktemp -d)"
trap 'rm -rf "$dir"' EXIT
chmod 700 "$dir"
ln -s "$BIN" "$dir/synaps-sudo-askpass"
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$dir/token"
chmod 600 "$dir/token"

echo "1. The helper started by a shell (not sudo) refuses before anything else"
set +e
out="$("$dir/synaps-sudo-askpass" "[sudo] password for you: " 2>&1 >/dev/null)"
code=$?
set -e
if [[ $code -ne 0 && "$out" == *"the parent process is not sudo"* ]]; then
    ok "refused: $out"
else
    bad "exit $code: $out"
fi

echo
echo "2. Started by the real sudo, the helper accepts its parent and only then"
echo "   fails, because no bash tool call is listening (no password is asked)"
set +e
out="$(SUDO_ASKPASS="$dir/synaps-sudo-askpass" command sudo -k -A true 2>&1 </dev/null)"
code=$?
set -e
if [[ "$out" == *"cannot reach the bash tool call"* ]]; then
    ok "parent check passed for the system sudo; sudo said: $(echo "$out" | tr '\n' ' ')"
elif [[ $code -eq 0 ]]; then
    echo "  SKIP: sudo did not ask (NOPASSWD rule?), the helper never ran"
else
    bad "exit $code: $out"
fi

cat <<'STEPS'

3. Inside Synaps (run each twice: the in-process TUI with SYNAPS_DAEMON=0,
   and attached to the daemon), ask the agent to run these with the bash tool:

   a. sudo -k; sudo true && echo SUDO_OK
      Expect a password box titled "sudo password required", reading
      "sudo is asking for your password to run:" / "sudo -A true" /
      "[sudo] password for <you>:". Type the password: the result shows
      SUDO_OK and never the password.

   b. sudo -k; sudo true; sleep 20; echo SHOULD_NOT_RUN
      Press Esc at the password box: the tool fails with "Command canceled
      while waiting for password" at once, no SHOULD_NOT_RUN.

   c. sudo -k; sudo true   then type a wrong password
      sudo asks again (a new box per attempt), up to its retry limit.

   d. printf '[sudo] password for x: ' >&2; printf 'Password: ' >&2; read -r x || echo EOF
      No password box at all; the output ends in EOF.

   e. sudo -n true; echo exit=$?
      With a password required: "sudo: a password is required", no box.

4. No prompt capability: ask the agent to have a subagent run
   `sudo -k; sudo true; echo exit=$?`. Subagents cannot raise prompts, so
   the helper fails closed: "no interactive client to ask for the sudo
   password", then sudo's own error, and exit=1. (Headless `synaps chat`
   does raise the prompt but answers it with a cancel, so there the command
   stops with "Command canceled while waiting for password", as before.)
STEPS

echo
echo "automated: $pass passed, $fail failed"
[[ $fail -eq 0 ]]
