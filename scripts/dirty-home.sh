#!/usr/bin/env bash
# dirty-home.sh
#
# Run the tests that used to read the developer's real HOME under a HOME
# that looks like a real developer machine, and fail if any of them wrote
# into it (06 I3).
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
# then runs the affected test binaries with HOME pointed at it. The seeded
# tree is fingerprinted before and after: a test that creates, changes or
# deletes anything under that HOME (instead of pinning its own base dir)
# fails the job. That's the integration-test counterpart of the unit-test
# tripwire in `extensions/audit.rs`, for every writer, not just audit.
#
# Never touches your real HOME: it always seeds a fresh temp dir.
#
# Usage: bash scripts/dirty-home.sh            (from anywhere in the repo)
#   Extra args go to every cargo invocation, e.g. `--locked`.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

# Keep the toolchain reachable once HOME moves.
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"

dirty="$(mktemp -d "${TMPDIR:-/tmp}/synaps-dirty-home.XXXXXX")"
trap 'rm -rf "$dirty"' EXIT

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
    (cd "$dirty" && find . -mindepth 1 -printf '%y %m %p\n' | LC_ALL=C sort \
        && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum)
}

# Build first, under the normal HOME, so the dirty HOME sees only test runs.
engine_tests=(
    --test continuous_memory_adversarial
    --test deferred_host_context
    --test extension_lease_lifecycle
    --test extension_lifecycle
)
root_tests=(
    --test autonomous_plugin
    --test extension_command_invoke_bounds
    --test extension_provider_streaming
    --test extension_provider_streaming_transport
    --test extensions_info
    --test extensions_process
    --test extensions_sidecar_spawn_args
    --test phase3_activation
)
# The whole engine lib binary, not a filter: the identity latch is set by
# whichever of its tests loads a config first, so only a full run exercises
# the order dependence (helpers' identity assertion, the oauth goldens).
# Its full run is about 25 s.

cargo test "$@" -p synaps-engine --lib "${engine_tests[@]}" --no-run
cargo test "$@" -p synaps "${root_tests[@]}" --no-run

before="$(fingerprint)"

run() { HOME="$dirty" USERPROFILE="$dirty" bash "$SCRIPT_DIR/cargo-test-nonzero.sh" "$@"; }
# Unset anything that would pin the base dir and hide the dirty HOME.
unset SYNAPS_BASE_DIR SYNAPS_PROFILE

run "$@" -p synaps-engine --lib "${engine_tests[@]}"
run "$@" -p synaps "${root_tests[@]}"

after="$(fingerprint)"
if [[ "$before" != "$after" ]]; then
    echo "::error::tests wrote into the seeded HOME instead of an isolated base dir:" >&2
    diff <(echo "$before") <(echo "$after") >&2 || true
    exit 1
fi
echo "dirty-home: seeded HOME unchanged"
