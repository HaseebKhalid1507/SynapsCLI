# Convergence verdict — Tasks 1–3 (synaps rpc)

**Date:** 2026
**Mode:** informed
**Threshold:** 0.8 (locked before run, per Plan §0)
**Axis weights:** spec=0.35, quality=0.20, coverage=0.20, edge=0.15, security=0.10
**Budgets:** max_fix_iterations=2, max_total_calls=10
**Calls used:** 2 / 10  (Tester, Judge)
**Fix iterations:** 0 / 2  (none triggered — PROCEED on first scoring)

## Subject under test
Branch `feat/synaps-rpc-mode` @ `331742a`, 3 commits ahead of `origin/dev`:
- `965874d` Task 1: RPC protocol types
- `3a07cf6` Task 2: `synaps rpc` headless line-JSON subcommand
- `331742a` Task 3: subprocess e2e harness

## Tester result
- **A1 cargo build --release:** PASS (3m 18s)
- **A2 cargo build --tests:** FAIL — pre-existing TUI breakage on `dev` (out of scope, documented)
- **A3 cargo clippy:** FAIL — clippy not installed in env (tooling absence, not code defect)
- **B4 rpc_protocol tests:** 37/37 PASS
- **B5 rpc_dispatch tests:** 31/31 PASS
- **C6 rpc_e2e single-thread:** 14/14 PASS in 1.02 s
- **C7 rpc_e2e --test-threads=4:** 14/14 PASS in 0.32 s
- **C8 flakiness re-run:** 14/14 PASS, no nondeterminism
- **D9 11/11 RpcCommand variants present**
- **D10 8/8 RpcEvent variants present** (prescribed grep was wrong; direct inspection confirms)
- **D11 docs/rpc-protocol.md present** (164 lines)
- **D12 Python tier-2 fixture present** (133 lines)
- **E13 git status clean**
- **E14 3 commits**, **E15 file sizes match plan exactly**

→ 12 PASS / 3 FAIL; all 3 FAILs are environmental/pre-existing, no regressions introduced.

## Judge axis scores

| Axis | Weight | Score | Contribution |
|---|---|---|---|
| Spec compliance | 0.35 | 0.88 | 0.308 |
| Code quality | 0.20 | 0.88 | 0.176 |
| Test coverage | 0.20 | 0.80 | 0.160 |
| Edge cases | 0.15 | 0.84 | 0.126 |
| Security | 0.10 | 0.90 | 0.090 |
| **Weighted total** | **1.00** | — | **0.860** |

**Verdict: PROCEED ✅** (0.860 ≥ 0.80)

Two-stage gate (spec_compliance ≥ 0.70): not triggered.

## Outstanding non-blocking concerns (recorded for next iteration / Phase 2)

These did not move the verdict below threshold but are worth addressing in a follow-up commit before the PR merges, or in Phase 2 cleanup:

1. **Missing e2e test `continue_resumes_history`** — Spec §10.1 names it; Plan Task 3 AC does not explicitly require it. The `--continue <id>` code path is fully implemented and unit-covered, but no subprocess-level regression test exists. Low effort to add.
2. **Missing e2e test `subagent_events`** — Spec §10.1 names it; requires a subagent-emitting fixture extension which doesn't exist yet. Lib-level mapping is unit-tested; e2e proof deferred.
3. **`concurrent_prompt_rejected` assertion is soft** — Test prints a warning rather than asserting on fast machines. Guard exists in implementation (`rpc.rs:259–265`) but CI cannot detect a regression. Recommend hardening with a synchronisation point.
4. **`NewSession` while streaming not guarded** — Implementation does not check `is_streaming()` before overwriting `api_messages` on `NewSession`. Spec doc §6 says only `abort`/`get_state`/`get_session_stats` are safe-while-in-flight, so the bridge protocol forbids this, but defence-in-depth would add the same guard used by `Prompt`. Low effort to add.

Items #5+ deferred per spec/plan (correctly out-of-scope for Phase 1):
- File-attachment binary reading (Task 10)
- `session_cost` price-lookup (Task 4+)
- Bounded shutdown drain (current 50 ms polling acceptable for v0)
- Path validation on `RpcAttachment::path` (Task 10 when file reads are added)

## Convergence audit checklist (per skill)

- [x] Plan declared `convergence: informed` *before* the loop ran (Plan §0, Task 1/2/3 headers)
- [x] Threshold (0.8) was fixed before the loop started (Plan §0)
- [x] Axis weights were fixed before the loop started (default code-review weights)
- [x] Every role dispatch was a fresh blocking one-shot subagent (`subagent` tool, sequential)
- [x] No convergence role used `subagent_start`, `subagent_resume`, or `subagent_steer`
- [x] No two convergence roles overlapped in time (Tester finished before Judge dispatched)
- [x] Final verdict is PROCEED
- [x] Structured feedback preserved (none was needed since PROCEED on first score)
- [x] Spec_compliance ≥ 0.7 before other axes lifted overall score (0.88)
- [x] Builder commits live on a worktree branch (`feat/synaps-rpc-mode`), not integration branch
- [x] Loop did not exceed `max_fix_iterations` (0 used) or `max_total_calls` (2 used)

## Decision

PROCEED to Phase 2 (Tasks 4–15, bridge-side, `convergence: none`).

Recommended (but not required by the threshold) before opening the PR against `dev`:
- Address concerns #3 and #4 above (low-effort defensive fixes; harden the
  concurrent-prompt assertion + add `is_streaming()` guard to `NewSession`).
- Address concern #1 (`continue_resumes_history` e2e test) — direct evidence
  for the most user-visible recovery path.

Concerns #2 (subagent e2e fixture) is a larger fixture build that should be
deferred until after Tasks 4–10 land, since the bridge will exercise that path.
