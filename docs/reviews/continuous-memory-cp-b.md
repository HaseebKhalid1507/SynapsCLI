# CP-B — Axel recall provider (informed checkpoint)

- **Plan:** `axel-continuous-memory-plan/1`, Phase B (Tasks B1-B6)
- **Spec:** `axel-continuous-memory/1`
- **Worktrees:** `SynapsCLI-continuous-memory` @ `feat/continuous-memory-host`;
  `synaps-skills-continuous-memory` @ `feat/continuous-memory-plugin`;
  `axel-continuous-memory` @ `feat/continuous-memory-axel`
- **Reviewed by:** foreman (orchestrator), independent re-run of every
  coder-reported test command from a clean state

## Commits

| Task | Repo | Commit | Summary |
|---|---|---|---|
| B1 | host | `c7610f44` | Full `RecallRequest`/validator, `DisclosureClass` (Task 34) reuse replacing the placeholder `Sensitivity`, duplicate-ID and permitted-class rejection |
| B1 | plugin | `e257b10` | Wire-compatible mirror types + outgoing disclosure guard |
| B2 | axel | `2a65073` | Bounded (≤128 candidates/≤8 selected by construction) chat-history candidate generation + explainable `RankReason` ranking, reusing the existing T32-T36 FTS5 store |
| B3 | plugin | `9ab181a` | `context_provider.recall` RPC handler wired to B2, manifest+initialize exact-match context-provider declaration |
| B4 | host | `18a7f786` | Per-prompt recall flow: 150ms hard timeout, one-shot/retry-exact semantics, typed synthetic-message injection (separate message, never merged into the user's own text), injection-string neutralization, byte-identity-when-absent proven |
| B5 | host | `4b6f0ef7` | `/memory why` rendering, metadata-only `memory_recall.*`/`memory_context.*` observability events with adversarial leak-sentinel tests |
| B6 | host | `983265ed` | Real fixture-process (framed JSON-RPC, genuine child spawn) headless end-to-end harness — 8/8 |

## Independent verification (foreman re-run)

```
cargo test -p synaps-engine memory --jobs 8 -- --test-threads=8            → 82 passed (host, cumulative B1-B5)
cargo test -p synaps-engine --test memory_context_e2e -- --test-threads=1  → 8 passed (B6, real spawned fixture)
cargo test -p synaps-engine --test extension_lease_lifecycle \
  --test deferred_host_context -- --test-threads=1                        → 24 passed (regression, fixture edits safe)
cargo check --workspace --all-targets --jobs 8                             → 0 errors
cargo test --workspace --jobs 8 -- --test-threads=8 (axel repo)            → 0 failed (B2)
cargo test --jobs 8 <local-patch> -- --test-threads=8 (plugin repo)        → 0 failed (B1/B3)
```

## Gate (spec §22 Phase B)

- [x] Real provider-turn tests (not mocks) prove bounded per-prompt
      injection: `tests/memory_context_e2e.rs` spawns the actual fixture
      extension process over real framed JSON-RPC and asserts the exact
      synthetic-message shape in the constructed `messages` Vec.
- [x] No user/system-role confusion: the contribution is its own separate
      message object, delimited, never merged into the user's own content
      block, never system policy (`docs/decisions/T-continuous-memory-message-injection.md`
      records the tool_result-pairing alternative considered and rejected).
- [x] Disclosure enforced at both plugin (outgoing guard) and host
      (`validate_contribution` + `gate_for_model` reuse) — defense in depth,
      not single-sided trust.
- [x] One-shot and per-prompt semantics are exact under retries and across
      new sessions (B4 unit tests + B6 real-process harness agree).
- [x] `/memory why` explains selected IDs/classes/rank reasons without ever
      exposing memory body content (B5).

## Notable engineering decisions surfaced during Phase B

1. **B1**: replaced the Phase-A placeholder `Sensitivity` enum with the
   already-proven `agent_core::core::disclosure::DisclosureClass`/`gate_for_model`
   (Task 34) instead of inventing a parallel vocabulary — the two disclosure
   systems now share one enforcement point.
2. **B2**: reused the existing FTS5-backed `project_memory.rs` store
   completely; chat-history record classes are encoded via the existing
   free-text `category` column rather than a schema migration.
3. **B4**: resolved the "typed lower-authority segment vs. Anthropic's strict
   user/assistant alternation" tension by inserting a wholly separate
   synthetic message (not a fabricated `tool_use`/`tool_result` pair, not a
   splice into the user's own content array) — documented as a durable
   decision record.
4. **B5**: root-caused a `tracing`-core global-callsite-interest race under
   parallel tests and replaced subscriber-capture with a deterministic
   `cfg(test)` thread-local capture rather than serializing/skipping the
   flaky tests.
5. **B6**: extended the existing checked-in Python fixture extension
   (already used by Task 20 lease tests) additively — new argv slot, new
   RPC method, new MODE values — instead of building a parallel fixture
   harness, keeping exactly one source of protocol-fixture truth.

## Verdict

CP-B **PASSES** the Phase B gate. No Critical or Important findings.
Proceeding to Phase C (structured chat capture).
