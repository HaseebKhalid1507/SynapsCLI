# CP-A — Host lease and context-provider protocol (informed checkpoint)

- **Plan:** `axel-continuous-memory-plan/1`, Phase A (Tasks A1-A7)
- **Spec:** `axel-continuous-memory/1`
- **Worktree:** `SynapsCLI-continuous-memory` @ `feat/continuous-memory-host`
- **Reviewed by:** foreman (orchestrator), independent re-run of every
  coder-reported test command from a clean state (not a trust of self-report)

## Commits

| Task | Commit | Summary |
|---|---|---|
| A1 | `7d384d44` | Typed `MemoryContextMode`/`MemoryContextLease`/`UserIntentProof`/`AuthorizedMemoryAction`/`SessionMemoryState`, host-private lease construction, compile-fail visibility proofs |
| A2 | `f4a1eea6` | `memory.*` config surface, fail-closed `default_mode`, explicit consent gate |
| A3 | `0007ea68` | `DeclaredExtensionContextProvider` (distinct from the pre-existing model-provider concept), `ContextProviderId`, dormant descriptor, permission gate |
| A4 | `18613d0e` | `memory_context` tool restricted to `disable`/`status`/`recall_once`; `enable`/`index_history` typed-refuse via the tool; excluded from recursive subagents |
| A5 | `5efb7390` | `/memory` commands via the shared `handle_engine_command` (same pattern as `/trace`); `ExplicitCommand` proof minted only by the deterministic command path; non-inheritance regression test |
| A6 | `ce3ffe60` | Enable-time provider-identity validation against the installed extension catalog (fail closed on unknown/ambiguous); disable/session-end revocation reuses the existing `ExtensionRuntimeManager`/`ExtensionSessionEndGuard` reap path |
| A7 | `21885d52` | `ContextSegment::Memory`/`MemoryContextContribution` types; `memory_budget_tokens` (`min(4096, 10%)`, 512 floor); T29 `memory_contents`/`memory_tokens` lane wired to real (currently empty-by-default) content; reserves proven byte-identical with/without a contribution |

## Independent verification (foreman re-run, not coder self-report)

```
cargo test -p synaps-engine memory_context --jobs 8 -- --test-threads=8      → 13 passed (A1)
cargo test -p synaps-core memory --jobs 8 -- --test-threads=8                → 6 new config tests passed (A2)
cargo test -p synaps-engine extensions:: --jobs 8 -- --test-threads=8        → 271 → 275 passed across A3/A6
cargo test -p synaps-engine memory_context --jobs 8 -- --test-threads=8      → 28 passed (A4)
cargo test -p synaps-engine memory --jobs 8 -- --test-threads=8              → 44 → 49 → 55 passed across A5/A6/A7
cargo test -p synaps-engine body_golden -- --test-threads=8                  → 4 passed (A7 regression)
cargo test -p synaps-engine context:: -- --test-threads=8                    → 41 passed (A7)
cargo check --workspace --all-targets --jobs 8                               → 0 errors
```

## Gate (spec §22 Phase A)

- [x] Forged model-initiated `enable`/`index_history` denied before any spawn
      (A4: refused via `MemoryContextError::RequiresHostConfirmation`, no
      lease installed).
- [x] Discovery/status spawn zero processes (A3 dormant descriptor; A4
      `status` with no capability answers a deterministic `Off`).
- [x] Exact enable requires validated provider identity; unknown/ambiguous
      provider ids fail closed and grant nothing (A6).
- [x] New sessions and subagents inherit no lease — `Runtime::new()` always
      initializes `Off`; subagents are built via a fresh `Runtime::new()` +
      `apply_subagent_runtime_policy`, never a state-carrying clone (A5/A6
      regression tests, re-confirmed after A6's registry-validation addition).
- [x] Cross-frontend single-transition state — `/memory` and `memory_context`
      both route through one `handle_engine_command`/`SessionMemoryState`
      implementation (A5); no frontend duplicates lease logic.
- [x] `ContextSegment::Memory` typed, budget-reserved, and proven not to
      perturb protected reserves (A7); actual per-provider wire injection is
      explicitly deferred to task B4, where real recall content first exists
      (documented scope split — not a gap).

## Scope adjustments made during Phase A (recorded, not silent)

1. **A4 narrowed**: the model-callable `memory_context` tool supports only
   `disable`/`status`/`recall_once` (all locally safe/revocable). `enable` and
   `index_history` require the deterministic `/memory` command path (A5),
   which mints `UserIntentProof::ExplicitCommand` directly — a model tool
   call cannot manufacture that proof through JSON parameters alone. This is
   a stricter interpretation of spec §7.2, not a deviation from it.
2. **A6 narrowed**: real process spawning is Phase B's concern (the actual
   recall/capture RPC does not exist until B1-B4). A6 delivers the
   authorization/validation and revocation *wiring* against the existing,
   already-proven `ExtensionRuntimeManager` lease/reap machinery, so Phase B
   inherits spawn-once and session-end-reap guarantees for free instead of
   re-deriving them.
3. **A7 narrowed**: wire-level injection of the segment into the four
   provider adapters (Anthropic, OpenAI-compatible, Gemini, broker) is moved
   to task B4, where a real `MemoryContextContribution` first exists to
   inject. A7 delivers the type, the validator, and the T29 budget math only.
   This avoids touching prompt-cache byte-identity–sensitive wire-builder code
   before there is real content to justify the change, and keeps the existing
   `body_golden` fixtures green throughout Phase A.

## Verdict

CP-A **PASSES** the Phase A gate as adjusted above. No Critical or Important
findings. Proceeding to Phase B (Axel recall provider).
