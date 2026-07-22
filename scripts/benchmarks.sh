#!/usr/bin/env bash
# benchmarks.sh — consolidated §13.5 performance benchmark runner (Task 36).
#
# One documented entry point for every performance axis the spec tracks, so
# a regression always requires an explicit budget update in the relevant
# suite rather than silently drifting. Two kinds of coverage:
#
#   1. MACHINE-READABLE BENCHMARKS — `--ignored`-gated tests that print
#      greppable `BENCH <suite> key=value …` lines. Slow by design, so they
#      are excluded from `cargo test` and run here explicitly, serialized
#      (`--test-threads=1`) and resource-capped.
#
#   2. BUDGET ASSERTION SUITES — normal tests that HARD-FAIL when a §13.5
#      axis regresses past its documented budget (first-request schema
#      bytes, bounded stream RSS, cache-prefix byte stability). They run in
#      the ordinary workspace suite too; this script re-runs them so one
#      command produces the complete performance evidence.
#
# Usage:
#   bash scripts/benchmarks.sh            # everything, serialized
#   bash scripts/benchmarks.sh --fast     # budget assertion suites only
#
# §13.5 axis → coverage:
#   initial schema bytes vs dormant tools . phase3_activation (a01/a02 budget)
#   catalog insertion vs activation rebuild phase3_activation (a04 exact-add)
#   cache-prefix reused/rewritten bytes ... phase2_trace_conformance (golden
#                                           fixtures + segment digests)
#   request serialization/retry cost ...... phase2_trace_conformance (exact
#                                           sent-bytes traces)
#   output-stream RSS under backpressure .. stream_backpressure (bounded
#                                           flood turns)
#   memory retrieval 1K/10K/100K (1M*) .... memory_index BENCH lines
#   session save 1/10/100 MiB ............. session_journal BENCH lines
#   program-level phase 5 save/recovery ... phase5_context_memory BENCH line
#
#   * the 1M-record scale is documented as out of the local runtime budget
#     (docs/decisions/T33-memory-index-no-sqlite.md); run it explicitly with
#     `cargo test -p synaps-core --test memory_index --release -- --ignored
#     bench_1m_records --test-threads=1 --nocapture` when the budget allows.

set -euo pipefail
cd "$(dirname "$0")/.."

FAST=0
[[ "${1:-}" == "--fast" ]] && FAST=1

run() {
  echo "── $*"
  "$@"
}

echo "== budget assertion suites (hard-fail on regression) =="
run cargo test --test phase3_activation -- --test-threads=1
run cargo test --test phase2_trace_conformance -- --test-threads=1
run cargo test --test stream_backpressure -- --test-threads=1

if [[ "$FAST" == "1" ]]; then
  echo "== --fast: skipping machine-readable benchmarks =="
  exit 0
fi

echo "== machine-readable benchmarks (BENCH lines below) =="
run cargo test -p synaps-core --test memory_index --release -- \
  --ignored bench_1k_records bench_10k_records bench_100k_records \
  --test-threads=1 --nocapture
run cargo test -p synaps-core --test session_journal --release -- \
  --ignored --test-threads=1 --nocapture
run cargo test --test phase5_context_memory --release -- \
  --ignored --test-threads=1 --nocapture

echo "== done — grep '^BENCH ' above for the machine-readable numbers =="
