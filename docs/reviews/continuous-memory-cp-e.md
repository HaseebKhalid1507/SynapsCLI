# CP-E — Advanced retrieval and consolidation (informed checkpoint)

- **Plan:** `axel-continuous-memory-plan/1`, Phase E (Tasks E1–E5)
- **Spec:** `axel-continuous-memory/1`
- **Worktree:** axel `axel-continuous-memory` @ `b91dfcb`

## Tasks delivered

| Task | Commit | Summary |
|---|---|---|
| E1 | `7a1e52f` | Supersession + contradiction graph: typed `supersedes`/`contradicts`/`confirmed_by`/`invalidated_at`; `Correction` drives supersession; ranking penalizes stale claims (superseded never shown as current without a conflict label); tombstoned ids never resurface via the graph after rebuild; measurable stale rate. |
| E2 | `d3c832c` | Bounded MMR diversity pass with class mixing + duplicate penalties, within the ≤8 selection and byte bounds; near-duplicates collapse to ≤1 representative. |
| E3 | `1728084` | Optional local embeddings **off by default**: separate explicit enable + guarded (non-fetching) download entry point, 250 ms p95 target, automatic lexical fallback with metadata note. Network oracle proves the default path constructs no request and creates no cache dir. GLiNER stays off. |
| E4 | `4d597ea` | Bounded consolidation **off by default** (`auto_consolidate = Off`): merge/promote/supersede/strengthen/prune/rebuild; output scope ⊆ input scope (property test); tombstones survive consolidation + rebuild; record-bounded with mid-run cancellation rollback. |
| E5 | `b91dfcb` | Retrieval-quality corpus + metrics gate: labeled multi-session fixture with injection/secret/superseded/cross-project traps; machine-readable report with in-test thresholds. |

## Gate evidence (spec §22 Phase E, §20.6 thresholds)

`recall_quality` machine-readable report:

```json
{"recall_at_1":1.0,"recall_at_5":1.0,"recall_at_8":1.0,"mrr":1.0,
 "duplicate_selected_rate":0.0,"stale_superseded_selected_rate":0.0,
 "secret_leakage_count":0,"cross_project_leakage_count":0,
 "context_budget_violations":0,"latency_p50_us":180,"latency_p95_us":201}
```

- recall@5 = 1.0 ≥ 0.85 ✓
- stale/superseded = 0.0 ≤ 0.05 ✓
- duplicate selected = 0.0 ≤ 0.10 ✓
- secret leakage = 0 ✓ · cross-project leakage = 0 ✓ · budget violations = 0 ✓

## Verification (capped)

- `cargo test -p axel --test recall_quality` → 1 passed, 0 failed
- `cargo test --workspace` (axel) → 198 passed, 0 failed

## Orchestration notes

- Every E-task produced a focused commit (1–4 files). The `cargo fmt` churn
  guard in the task prompts held for E1/E2/E4/E5; E3 aborted once early
  (transient empty response) and was resumed from the restored prompt with its
  partial on-disk work intact. No threshold was weakened to pass the gate.

## Disposition

CP-E passes on informed review. Proceed to Phase F (program harness,
adversarial oracles, benchmarks, full gates, holdout).
