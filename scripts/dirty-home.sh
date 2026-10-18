#!/usr/bin/env bash
# dirty-home.sh
#
# Run the test suite under a HOME that looks like a real developer machine,
# and fail if any test wrote into it (06 I3).
#
# CI's runners have an empty HOME, so a test that silently reads
# ~/.synaps-cli passes there and fails only on a maintainer's laptop: the
# process-wide identity latch, `memory.backend = axel` turning every
# ExtensionManager exclusive, and so on. This seeds a throwaway HOME with:
#
#   - `identity = You are Claude Code, …` (the line that broke the helpers
#     identity assertion and silently skipped the oauth golden fixture)
#   - `memory.backend = axel`
#   - `context_management.mode = auto`
#   - an `auth.json` with populated, obviously dummy OAuth slots
#
# then runs `cargo test` with HOME pointed at it. CI's `test` job runs the
# whole workspace this way, so a new test binary that forgets to pin its
# base dir is exercised too; there is no list to keep up to date.
#
# The seeded tree is fingerprinted before and after (type, mode, symlink
# target and path of every entry, sha256 of every file): a test that
# creates, changes, re-points or deletes anything under that HOME fails the
# run. That's the integration-test counterpart of the unit-test tripwire in
# `extensions/audit.rs`, for every writer, not just audit. Not caught: a
# write that is undone before the run ends, and writes outside HOME.
#
# Never touches your real HOME: it always seeds a fresh temp dir. Needs GNU
# find and sha256sum (Linux).
#
# Usage: bash scripts/dirty-home.sh [cargo test args]
#   Default args: --workspace. CI: `bash scripts/dirty-home.sh --workspace --locked`.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

if [[ $# -eq 0 ]]; then
    set -- --workspace
fi

# Keep the toolchain reachable once HOME moves.
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"

dirty="$(mktemp -d "${TMPDIR:-/tmp}/synaps-dirty-home.XXXXXX")"
log="$(mktemp "${TMPDIR:-/tmp}/synaps-dirty-home-log.XXXXXX")"
# The seeded HOME is made read-only for the run (below); restore write
# permission first or `rm -rf` can't remove it.
trap 'chmod -R u+w "$dirty" 2>/dev/null; rm -rf "$dirty" "$log"' EXIT

mkdir -p "$dirty/.synaps-cli"
chmod 700 "$dirty/.synaps-cli"
cat > "$dirty/.synaps-cli/config" <<'EOF'
# Seeded by scripts/dirty-home.sh: a realistic developer config.
identity = You are Claude Code, Anthropic's official CLI for Claude.
memory.backend = axel
context_management.mode = auto
EOF
# Dummy values only. Far-future expiry, so nothing has a reason to refresh.
cat > "$dirty/.synaps-cli/auth.json" <<'EOF'
{
  "anthropic": {
    "type": "oauth",
    "refresh": "dirty-home-dummy-refresh-not-a-secret",
    "access": "dirty-home-dummy-access-not-a-secret",
    "expires": 4102444800000
  },
  "openai-codex": {
    "type": "oauth",
    "refresh": "dirty-home-dummy-codex-refresh-not-a-secret",
    "access": "dirty-home-dummy-codex-access-not-a-secret",
    "expires": 4102444800000,
    "accountId": "dirty-home-dummy-account"
  }
}
EOF
chmod 600 "$dirty/.synaps-cli/auth.json"

fingerprint() {
    (cd "$dirty" && find . -mindepth 1 -printf '%y %m %p -> %l\n' | LC_ALL=C sort \
        && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum)
}

# Build first, under the normal HOME, so the dirty HOME sees only test runs.
cargo test "$@" --no-run

# Read-only for the run, so a test that tries to create something here
# fails AT THAT POINT, under its own name, instead of only showing up as an
# anonymous diff at the end. The file logger, for example, panics with
# "initializing rolling file appender failed" when it can't create
# synaps.log.<date>. Not a substitute for the diff below: a writer that
# ignores the error is still caught there.
chmod a-w "$dirty" "$dirty/.synaps-cli"

before="$(fingerprint)"

# Unset anything that would pin the base dir and hide the dirty HOME.
unset SYNAPS_BASE_DIR SYNAPS_PROFILE
# A failing test must not skip the write check: run it either way, then
# report cargo's status.
status=0
HOME="$dirty" USERPROFILE="$dirty" bash "$SCRIPT_DIR/cargo-test-nonzero.sh" "$@" 2>&1 \
    | tee "$log" || status=$?  # pipefail: cargo's status, not tee's

after="$(fingerprint)"
# A subprocess (a spawned `synaps`) that hit the read-only HOME may have
# had its stderr captured by its test, which could still pass. Its panic
# text reaches this log only when the child's stderr was inherited, so
# this is best effort; the diff below remains the backstop.
if grep -n "initializing rolling file appender failed" "$log" >&2; then
    echo "::error::a test or a process it spawned tried to create its log file in the seeded HOME (see the lines above for which)" >&2
    exit 1
fi
if [[ "$before" != "$after" ]]; then
    echo "::error::tests wrote into the seeded HOME instead of an isolated base dir:" >&2
    diff <(echo "$before") <(echo "$after") >&2 || true
    exit 1
fi
echo "dirty-home: seeded HOME unchanged"
exit "$status"
