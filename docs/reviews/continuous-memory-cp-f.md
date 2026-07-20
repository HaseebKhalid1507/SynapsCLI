# CP-F — Program harness, gates, holdout (final checkpoint)

- **Plan:** `axel-continuous-memory-plan/1`, Phase F (Tasks F1–F5)
- **Spec:** `axel-continuous-memory/1`
- **Heads:** host `5ec0a648`, axel `b91dfcb`, plugin `fbdcb08`

## Tasks delivered

| Task | Commit(s) | Summary |
|---|---|---|
| F1 | host `852da0e4` | Headless `continuous_memory` lifecycle harness: consent → `/memory on` → per-prompt recall → capture → session restart → cross-session recall → `/memory why` → `/memory off` → history import, all simulated. 10/10 e2e tests. |
| F2 | host `66d08803` | Consolidated §20.5 adversarial suite (9 named tests): injection inertness, foreign-project no-existence-leak, forged-enable, durable-default denial, crash-no-lease/child-leak; cross-references to existing secret/1GiB/cancellation/symlink oracles. |
| F3 | axel `106116b` | Chat-history recall benchmark 1K/10K/100K (+optional 1M): 100K warm-lexical **p95 = 53.3 ms** (< 100 ms target, status "pass"); ingest/p50/p95/scanned/retained/selected/bytes/peak reported; bounded state (selected=8). |
| F4 | plugin `8229b81`, `fbdcb08`; host `c037bc70`, `5ec0a648` | Docs (README, build-strategy, version 0.2→0.3), interim local-patch build documented (no push — awaits human approval); cross-repo build fix (`RankReason::SemanticMatch`); tools-catalog regeneration for `memory_context` (24→25); **regression fix**: prevented a teardown lease respawn that broke two `extension_lease_lifecycle` spawn-count oracles. |
| F5 | host `<this commit>` | Independent security/privacy holdout — **GATE: PASS**. |

## Full gate results (capped: `CARGO_BUILD_JOBS=8`, `--jobs 8`, `--test-threads≤8`, sequential)

- **Host workspace:** 3522 passed, 0 failed (118 summaries) — `/tmp/cm-host-workspace3.log`
- **Axel workspace:** 198 passed, 0 failed — `/tmp/cm-axel-workspace.log`
- **Plugin (local Axel patch):** 76 lib + 20 integration_wire + provider suites, 0 failed; oversized frame fails closed
- **100K recall p95:** 53.3 ms (target ≤ 100 ms) ✓

## Independent holdout (F5)

- Reviewer: read-only subagent, `anthropic/claude-opus-4-8`, wall-separated from
  the builder (`openai-codex/gpt-5.6-sol`).
- Per-axis: security/privacy 0.92, correctness 0.88, spec fidelity 0.85, code
  quality 0.85, docs 0.85.
- **Weighted 0.8835 ≥ 0.80; spec fidelity 0.85 ≥ 0.70; zero Critical; zero
  Important.** One Minor (cross-reference test clarity), accepted.
- Verdict: `docs/reviews/continuous-memory-holdout.md`. **GATE: PASS.**

## Notable orchestration findings

- The full-workspace gate (not the capped per-task checks) caught two real
  integration issues the per-task runs missed: a cross-repo enum-exhaustiveness
  break (`RankReason::SemanticMatch`) and a lease-teardown respawn regression in
  the pre-existing lifecycle oracles. Both were root-caused and fixed without
  weakening any assertion.
- Repeated worker `cargo fmt` churn and transient provider aborts were handled
  by mid-run steering and independent verify-then-commit recovery.

## Disposition

CP-F passes. All Phase A–F gates green; §25 definition of done satisfied with
evidence. Feature complete on the three local branches (no push/publish —
awaits explicit human approval).
