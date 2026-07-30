# Provider reasoning modes implementation plan

**Branch:** `feat/provider-reasoning-modes`
**Worktree:** `/home/jr/Projects/Maha-Media/.worktrees/SynapsCLI-provider-reasoning-modes`
**Convergence:** informed; threshold 0.8; axis weights correctness 0.35, architecture/types 0.25, security 0.15, tests 0.15, readability/performance 0.10; max fix iterations 2; max total calls 10.

## Dependency graph

Typed named level + compatibility mapping
→ exact catalog capability metadata
→ runtime state and persistence
→ command/settings validation
→ Codex request body
→ headless verification and independent review

## Task 1: Typed named reasoning level

**Description:** Add a core `ReasoningLevel` enum with exact parsing/display and explicit conversion to legacy Anthropic budgets where defined. Preserve custom numeric budget support separately.

**Acceptance criteria:**
- `xhigh`, `max`, and `ultra` are distinct values and round-trip exactly.
- Existing level and numeric config values remain compatible.
- No `max → xhigh` alias or silent fallback exists.

**Verification:** focused core/config/command tests and `cargo check -p synaps-core`.

**Files likely touched:** core reasoning/models/config and engine command parsing.
**Scope:** M.

## Task 2: Exact Codex capability metadata

**Description:** Extend normalized catalog reasoning metadata and parse `supported_reasoning_levels[].effort` plus `default_reasoning_level` from the brokered Codex live response. Add current safe static fallback capabilities keyed by exact catalog row.

**Acceptance criteria:**
- Sol and Terra advertise through Ultra; Luna through Max; 5.5/5.4/5.4-mini/Spark through XHigh.
- Unknown values are rejected or ignored safely at the external parse boundary.
- Hidden/internal models remain excluded and provider-qualified IDs remain unchanged.

**Verification:** fixture parser tests and catalog integration tests.

**Files likely touched:** catalog domain type, Codex parser/fixture.
**Scope:** M.

## Checkpoint 1

- Tasks 1–2 committed as independently revertible slices.
- Core and catalog tests green.
- Exact capability table represented by tests, not substring rules.

## Task 3: Runtime state, config, and session preservation

**Description:** Store the canonical named level in Runtime alongside the legacy token budget. Thread exact values through apply-config, status accessors, session creation/resume, and subagent runtime policy.

**Acceptance criteria:**
- `max`/`ultra` survive config load, runtime read, session save, and resume.
- Legacy numeric budgets still build Anthropic fixed-budget requests unchanged.
- Model changes do not silently mutate the selected reasoning level.

**Verification:** config, runtime, session, resume, and existing Anthropic golden tests.

**Files likely touched:** runtime state, config type/parser, session/resume integration.
**Scope:** M.

## Task 4: Model-aware command and settings behavior

**Description:** Validate mutations against exact active-model capability metadata and expose dynamic Codex settings options. Unsupported mutations fail without changing state.

**Acceptance criteria:**
- `/thinking max` and `/thinking ultra` preserve exact values on supported models.
- Ultra is rejected for Luna; Max and Ultra are rejected for 5.5/5.4/5.4-mini/Spark.
- Settings presents model-specific options and applies the same validation.
- Providers lacking authoritative metadata do not gain Max/Ultra.

**Verification:** command tests and headless TUI settings tests.

**Files likely touched:** engine commands/runtime validation and TUI settings schema/input/snapshot.
**Scope:** M.

## Checkpoint 2

- Tasks 3–4 committed.
- Persistence and both user entry points proven.
- No unsupported selection changes runtime state.

## Task 5: Codex wire enforcement

**Description:** Pass the typed level to Codex Responses request construction, validate before credential/network side effects, and emit exact `reasoning.effort`.

**Acceptance criteria:**
- Sol/Terra bodies emit exact `xhigh`, `max`, or `ultra`.
- Luna emits Max but rejects Ultra before network.
- 5.5 family emits XHigh but rejects Max/Ultra before network.
- Off/adaptive semantics are explicit and tested; no field is invented when omission is required.

**Verification:** extracted pure body-builder tests plus zero-network rejection harness.

**Files likely touched:** OpenAI route dispatch and Codex stream/body builder.
**Scope:** M.

## Task 6: Automated aggregate harness and review

**Description:** Run unattended aggregate verification and an independent review/corrective loop.

**Acceptance criteria:**
- Core, engine, and TUI focused suites pass.
- Release checks produce no warnings introduced by this feature.
- `git diff --check` passes and changed-file formatting is clean.
- Independent reviewer scores at least 0.8 with no critical/important unresolved findings.

**Verification:** commands from the spec; no human interaction required.

**Dependencies:** Tasks 1–5.
**Scope:** S.

## Checkpoint 3

- Feature commit(s) landed on the isolated branch.
- Aggregate evidence recorded.
- Review verdict reconciled before fast-forward integration.
