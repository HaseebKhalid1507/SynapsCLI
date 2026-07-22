# CP-C — Structured chat capture (informed checkpoint)

- **Plan:** `axel-continuous-memory-plan/1`, Phase C (Tasks C1–C5)
- **Spec:** `axel-continuous-memory/1`
- **Worktrees:**
  - host `SynapsCLI-continuous-memory` @ `a2af90c1`
  - plugin `synaps-skills-continuous-memory` @ `da47e95`
  - axel `axel-continuous-memory` @ `9aeb837`

## Tasks delivered

| Task | Commit(s) | Summary |
|---|---|---|
| C1 | host `89f24427` | Typed terminal-only `ChatTurnCapture` builder: bounded user/assistant/tool summaries, project/session/turn/time/ordinal provenance, compaction linkage, deterministic source digest + idempotency key, disclosure/retention/sensitivity, forbidden-class filtering, foreign-project rejection before content inspection, explicit interrupted non-idempotent outcomes. |
| C2 | axel `9aeb837` | Durable episodic storage classes (EpisodicTurn, ConversationSummary, Decision, Preference, UnresolvedTask, EntityFact, ToolOutcome, Correction); durable-before-ack, stable 64-bit IDs, tombstone-safe, model-free heuristic enrichment; `.r8` migrate-safe. GLiNER/embeddings stay off. |
| C3 | host `d3b6706e`+`944181df`, plugin `c303eee` | Engine capture path: capture-capable-lease check, disclosure/persistence gates, bounded worker (fixed queue, exact overflow accounting, ≤50 ms p95 sync delay, decoupled durable commit), capture failure never invalidates the turn, idempotent retry queue. Plugin capture RPC durable + idempotent. |
| C4 | host `738c1ea3` | Compaction-summary capture on the unified transition: source range + digest, provider/local-only marker, prompt-stack digest, redaction policy, classes, timestamp/schema; links to source without replacing provenance; capture-disabled leases emit nothing. |
| C5 | host `a2af90c1`, plugin `da47e95` | Crash/retry/cancellation adversarial suite: cross-session recall after kill/reopen (**Phase C gate**), kill-9 survives reopen, kill-after-commit recovers without duplicate, possibly-committed capture queried by idempotency key, oversized/1 GiB frame fails closed within fixed bounds, cancellation releases leases with no blocked producer. |

## Gate evidence (spec §22 Phase C)

- **Cross-session recall works after kill/reopen:**
  `cross_session_recall_finds_capture_after_kill_and_reopen ... ok`
- **No duplicate capture:**
  `kill_after_commit_reopen_recovers_capture_without_duplicate ... ok`,
  host idempotency-at-the-seam tests.

## Verification (capped: `CARGO_BUILD_JOBS=8`, `--jobs 8`, `--test-threads≤8`)

- host `cargo test -p synaps-engine memory` → 91 passed, 0 failed
- host `cargo test -p synaps-engine --test memory_context_e2e -- --test-threads=1` → 9 passed, 0 failed
- axel `cargo test --workspace` → all suites pass, 0 failed
- plugin (local Axel patch) `cargo test` → 76 lib + 20 integration_wire + provider suites pass, 0 failed;
  oversized frame logs `frame exceeds size limit` (fails closed)

## Recovery note

C1 and C5 workers (openai-codex/gpt-5.6-sol at θ:xhigh) exhausted their
context windows after writing complete, correct code but before committing.
The foreman independently re-ran the capped verification for each and
committed the verified on-disk work (mechanical recovery; worker-authored
code unchanged). No adversarial assertion was weakened to pass a gate.

## Disposition

CP-C passes on informed review. Proceed to Phase D (existing-history import).
