#!/usr/bin/env bash
# cargo-test-nonzero.sh
#
# `cargo test "$@"`, but fail if no test actually ran. A test-name filter
# that matches nothing (a renamed module, a typo, a cfg that compiled the
# tests out) makes `cargo test` print "running 0 tests" and exit 0, so a CI
# step meant to exercise something would pass while checking nothing.
#
# Sums "N passed" over every `test result:` line. Ignored tests don't count.
#
# Usage: bash scripts/cargo-test-nonzero.sh -p synaps-engine --lib tools::registry
#   Exit status is cargo's when it fails, else 1 when 0 tests passed.

set -euo pipefail

out="$(mktemp)"
trap 'rm -f "$out"' EXIT

cargo test "$@" 2>&1 | tee "$out"

passed="$(grep -E '^test result: ' "$out" \
    | sed -E 's/.* ([0-9]+) passed.*/\1/' \
    | awk '{ s += $1 } END { print s + 0 }')"

if [[ "$passed" -eq 0 ]]; then
    echo "::error::cargo test $* ran 0 tests: the filter matches nothing" >&2
    exit 1
fi
echo "cargo-test-nonzero: $passed tests passed"
