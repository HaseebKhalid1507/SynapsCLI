#!/bin/sh
# seatbelt-proof.sh: proves the kill guard's Seatbelt profile on macOS
# (crates/agent-engine/src/seatbelt.rs; docs/daemon-mode.md, "Kill guard on
# tool children"). Run it on the Mac, outside any sandbox:
#   sh seatbelt-proof.sh
#
# SAFE: it never signals every process or a process group. Each real signal
# goes to a `sleep` this script started itself, checked first to be a number
# above 1 that is still a sleep. kill -0 sends nothing; it only asks whether
# a signal would be allowed.
#
# Checks (PASS/FAIL):
#   1. probe: sandbox-exec -p "$PROFILE" /usr/bin/true exits 0 with an empty
#      stderr (what Synaps runs once before it wraps any tool child).
#   Inside one sandbox-exec run (/bin/sh -c):
#   2. kill -0 of a sleep started outside fails: "Operation not permitted";
#   3. the same with /bin/kill (a SIP-protected program it execs);
#   4. kill -0 of its own parent (this script, outside) fails the same way;
#   5. kill -0 $$ (itself) succeeds;
#   6. a sleep it started itself can be killed (kill -TERM $!; status 143).
#   Outside again:
#   7. this script (outside, as the daemon is) can signal a job the sandboxed
#      run left behind, and stops it.
# Report only (no pass/fail):
#   - a second, separate sandbox-exec run tries kill -0 on that job: can a
#     later tool call stop an earlier call's background job? yes/no.
# Then it stops only its own sleeps and prints a PASS/FAIL summary.
# Exit status: 0 when every check passed, 1 otherwise.

set -u

SANDBOX_EXEC=/usr/bin/sandbox-exec
PROFILE='(version 1)
(allow default)
(deny signal)
(allow signal (target same-sandbox))
'

checks=0
fails=0
pass() { checks=$((checks + 1)); printf 'PASS  %s\n' "$1"; }
fail() { checks=$((checks + 1)); fails=$((fails + 1)); printf 'FAIL  %s\n' "$1"; }
info() { printf 'INFO  %s\n' "$1"; }

# A pid this script may signal: a number above 1 that is still a sleep.
own_sleep() {
  case ${1:-} in '' | *[!0-9]*) return 1 ;; esac
  [ "$1" -gt 1 ] || return 1
  comm=$(ps -o comm= -p "$1" 2>/dev/null) || return 1
  case $comm in *sleep) return 0 ;; esac
  return 1
}

# Stops one of this script's own sleeps, from outside any sandbox.
stop_own() {
  if own_sleep "${1:-}"; then
    kill -TERM "$1" 2>/dev/null
  fi
  return 0
}

summary() {
  if [ "$fails" -eq 0 ] && [ "$checks" -gt 0 ]; then
    printf 'SUMMARY: PASS (%s checks)\n' "$checks"
    exit 0
  fi
  printf 'SUMMARY: FAIL (%s of %s checks failed)\n' "$fails" "$checks"
  exit 1
}

work=$(mktemp -d "${TMPDIR:-/tmp}/seatbelt-proof.XXXXXX") || {
  fail "mktemp -d"
  summary
}
A=''
cleanup() {
  stop_own "$A"
  for f in "$work"/*.pid; do
    [ -f "$f" ] && stop_own "$(cat "$f")"
  done
  [ -d "${work:?}" ] && rm -rf -- "${work:?}"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM

printf 'seatbelt-proof: macOS %s, %s, /bin/sh is %s\n' \
  "$(sw_vers -productVersion 2>/dev/null || echo '?')" "$(uname -m)" \
  "$(readlink /private/var/select/sh 2>/dev/null || echo '?')"

if [ ! -x "$SANDBOX_EXEC" ]; then
  fail "$SANDBOX_EXEC is missing"
  summary
fi

# 1. The probe.
err=$("$SANDBOX_EXEC" -p "$PROFILE" /usr/bin/true 2>&1 >/dev/null </dev/null)
rc=$?
if [ "$rc" -eq 0 ] && [ -z "$err" ]; then
  pass "1 probe: sandbox-exec -p PROFILE /usr/bin/true exits 0, stderr empty"
else
  fail "1 probe: exit $rc, stderr: $err"
fi

# A process outside any sandbox.
sleep 600 >/dev/null 2>&1 &
A=$!

# 2-6. One sandboxed run. It leaves a job behind for 7 and the report.
"$SANDBOX_EXEC" -p "$PROFILE" /bin/sh -c '
  A=$1
  work=$2
  if kill -0 "$A" 2>"$work/outside.err"; then echo outside=allowed; else echo outside=denied; fi
  if [ -x /bin/kill ]; then
    if /bin/kill -0 "$A" 2>"$work/binkill.err"; then echo binkill=allowed; else echo binkill=denied; fi
  else
    echo binkill=missing
  fi
  if kill -0 "$PPID" 2>"$work/parent.err"; then echo parent=allowed; else echo parent=denied; fi
  if kill -0 "$$" 2>"$work/self.err"; then echo self=allowed; else echo self=denied; fi
  sleep 600 >/dev/null 2>&1 &
  c=$!
  echo "$c" >"$work/child.pid"
  if kill -TERM "$c" 2>"$work/child.err"; then
    wait "$c"
    echo "child=killed status=$?"
  else
    echo child=denied
  fi
  sleep 600 >/dev/null 2>&1 &
  echo "$!" >"$work/job.pid"
' seatbelt-proof-run1 "$A" "$work" >"$work/run1.out" 2>"$work/run1.err"
rc=$?
[ "$rc" -eq 0 ] || info "the sandboxed run exited $rc"
[ -s "$work/run1.err" ] && info "the sandboxed run's stderr: $(cat "$work/run1.err")"
# A child the run stopped itself is gone: cleanup must not signal its pid.
grep -qx 'child=killed status=143' "$work/run1.out" && rm -f -- "$work/child.pid"

said() { grep -qx "$1" "$work/run1.out"; }
eperm() { [ -f "$work/$1" ] && grep -qi 'operation not permitted' "$work/$1"; }
why() { printf '%s; %s' "$(grep "^$1" "$work/run1.out" | head -n 1)" "$(cat "$work/$2" 2>/dev/null)"; }

if said outside=denied && eperm outside.err; then
  pass "2 inside: kill -0 of a process outside fails, Operation not permitted"
else
  fail "2 inside: kill -0 of a process outside: $(why outside= outside.err)"
fi
if said binkill=denied && eperm binkill.err; then
  pass "3 inside: /bin/kill -0 of a process outside fails, Operation not permitted"
else
  fail "3 inside: /bin/kill -0 of a process outside: $(why binkill= binkill.err)"
fi
if said parent=denied && eperm parent.err; then
  pass "4 inside: kill -0 of its parent (outside) fails, Operation not permitted"
else
  fail "4 inside: kill -0 of its parent: $(why parent= parent.err)"
fi
if said self=allowed; then
  pass "5 inside: kill -0 \$\$ (itself) succeeds"
else
  fail "5 inside: kill -0 \$\$: $(why self= self.err)"
fi
if said 'child=killed status=143'; then
  pass "6 inside: kill -TERM of its own child succeeds (status 143)"
else
  fail "6 inside: kill -TERM of its own child: $(why child= child.err)"
fi

# The job the sandboxed run left behind.
B=$(cat "$work/job.pid" 2>/dev/null)
if own_sleep "$B"; then
  # Report only: a second, separate sandbox.
  later=$("$SANDBOX_EXEC" -p "$PROFILE" /bin/sh -c \
    'if kill -0 "$1" 2>/dev/null; then echo yes; else echo no; fi' seatbelt-proof-run2 "$B" 2>&1)
  info "a later sandbox-exec run can signal an earlier run's job: $later"

  # 7. Outside, as the daemon is: signal it, then stop it.
  if own_sleep "$B" && kill -0 "$B" 2>/dev/null && kill -TERM "$B" 2>/dev/null; then
    n=0
    while kill -0 "$B" 2>/dev/null && [ "$n" -lt 20 ]; do
      sleep 0.1 2>/dev/null || sleep 1
      n=$((n + 1))
    done
    if kill -0 "$B" 2>/dev/null; then
      fail "7 outside: the sandboxed run's job $B is still there after kill -TERM"
    else
      pass "7 outside: this script can signal and stop a sandboxed job"
      rm -f -- "$work/job.pid"
    fi
  else
    fail "7 outside: kill of the sandboxed run's job $B was refused"
  fi
else
  info "a later sandbox-exec run can signal an earlier run's job: unknown (no job from the first run)"
  fail "7 outside: the sandboxed run left no job to signal (job.pid: '$B')"
fi

summary
