# Codex Max/Ultra Semantics Implementation Plan

**Goal:** Implement exact Codex 0.144.3 Max/Ultra wire and multi-agent semantics while retaining Synaps authorization and worker-recursion invariants.
**Architecture:** Parse exact multi-agent capability into the normalized Codex catalog, derive a typed request plan from provider-qualified identity and runtime role, and serialize only that plan. Compose the official tagged mode fragment as a developer input item, with shared preflight before credentials and existing orchestration tools executing model-driven Ultra delegation.
**Design Doc:** `docs/plans/2026-07-14-codex-max-ultra-semantics-design.md`
**Estimated Tasks:** 7
**Complexity:** Large

## Task 1: Lock the corrected oracle into RED request tests

**Files:**
- Modify: `crates/agent-engine/src/runtime/openai/stream.rs`

**Step 1: Write failing tests**

Change the Ultra body expectation to wire `"max"`, retain Max=`"max"` and XHigh=`"xhigh"`, and add tests requiring a separate proactive developer item for Ultra and explicit-only item for non-Ultra v2 foreground requests.

**Step 2: Verify RED**

Run:

```bash
CARGO_BUILD_JOBS=8 cargo test -p synaps-engine runtime::openai::stream::codex_wire_tests -- --nocapture
```

Expected: FAIL showing current Ultra wire effort `"ultra"` instead of `"max"`; new typed/protocol symbols may also be absent once those tests are introduced.

**Step 3: Preserve evidence**

Record the exact failing test names and assertion output for the final report. Do not change production code before RED is observed.

## Task 2: Parse authoritative multi-agent capability and add the mode planner

**Files:**
- Modify: `crates/agent-engine/src/runtime/openai/catalog/mod.rs`
- Modify: `crates/agent-engine/src/runtime/openai/catalog/codex.rs`
- Modify: `crates/agent-engine/src/runtime/openai/catalog/capability_cache.rs`
- Modify: `crates/agent-engine/src/runtime/openai/catalog/fixtures/openai_codex_models.json`
- Modify constructor sites found by `rg 'CodexNamed \{' crates`

**Step 1: Write failing catalog/planner tests**

Cover:

- fixture versions Sol=v2, Terra=v2, Luna=v1;
- live Ultra+missing/v1/unknown version denied;
- live metadata never borrows static v2;
- Sol/Terra Ultra plan: wire Max + proactive;
- Sol Max plan: wire Max + explicit-only;
- Luna Max plan: wire Max and no v2 fragment;
- XHigh plan: wire XHigh;
- wrong provider, unknown slug, unsupported Max/Ultra denied;
- worker Ultra plan: wire Max with no proactive mode.

**Step 2: Verify RED**

Run the exact new catalog test filter with `CARGO_BUILD_JOBS=8`; expect missing types/fields or failed assertions.

**Step 3: Implement minimally**

Add closed enums for wire effort, logical execution mode, multi-agent version/mode, capability source, and plan error code. Couple version to `ReasoningSupport::CodexNamed`, preserve live precedence, and export a provider-qualified pure planner.

**Step 4: Verify GREEN**

Run the same focused catalog tests, then existing catalog/validation tests.

## Task 3: Serialize the typed plan and compose official mode context

**Files:**
- Modify: `crates/agent-engine/src/runtime/openai/stream.rs`
- Modify: `crates/agent-engine/src/runtime/openai/mod.rs`

**Step 1: Write any remaining failing integration tests**

Assert:

- serializer cannot receive a raw logical level;
- Ultra and Max bodies both contain wire `"max"`;
- XHigh remains wire `"xhigh"`;
- mode text is byte-exact to tagged Codex 0.144.3 constants;
- exactly one mode developer item appears before the final user item;
- no undocumented body field is added;
- direct provider entry rejects Ultra without actionable subagent tools before broker access.

**Step 2: Verify RED**

Run the focused stream tests and capture the missing protocol behavior.

**Step 3: Implement minimally**

Build the plan before broker access, pass it to `build_codex_body`, and inject the optional developer item into request input. Add sanitized allow/deny tracing without request content or credentials.

**Step 4: Verify GREEN**

Run focused stream and broker-order tests.

## Task 4: Add request-role and pre-credential runtime preflight

**Files:**
- Modify: `crates/agent-engine/src/runtime/mod.rs`
- Modify: `crates/agent-engine/src/runtime/stream.rs`
- Modify: `crates/agent-engine/src/runtime/api.rs`
- Modify: `crates/agent-engine/src/runtime/api_sync.rs`
- Modify: `crates/agent-engine/src/runtime/openai/mod.rs`

**Step 1: Write failing tests**

Cover unsupported restored/configured Max/Ultra rejection before refresh/broker/network, foreground Ultra without orchestration, foreground Ultra without required tools, and worker-role Ultra suppression.

**Step 2: Verify RED**

Run the smallest runtime/openai filters; expect current code to reach auth or lack the role/prerequisite errors.

**Step 3: Implement minimally**

Add a request role to runtime/API options, shared pure/runtime preflight before `refresh_if_needed`, and pass the role through the OpenAI route. Keep non-Codex request behavior unchanged.

**Step 4: Verify GREEN**

Run the focused preflight and route tests.

## Task 5: Enforce worker and restore invariants

**Files:**
- Modify: `crates/agent-engine/src/tools/subagent/mod.rs`
- Modify: `crates/agent-engine/src/tools/subagent/{oneshot,start,resume}.rs` only if central policy cannot cover all paths
- Modify: `crates/agent-tui/src/tui/commands.rs`
- Test: `crates/agent-core/src/core/session.rs`
- Test: `crates/agent-engine/src/engine/setup.rs`
- Test: relevant colocated worker/TUI tests

**Step 1: Write failing tests**

Assert central subagent policy marks every fresh runtime as Worker, worker plans never emit proactive context, recursive tools remain absent, Ultra session JSON/compaction round-trips logically, and TUI-style resume restores Max/Ultra with explicit provenance.

**Step 2: Verify RED**

Run focused worker/restore tests; expect missing worker marker and non-explicit TUI restore behavior.

**Step 3: Implement minimally**

Mark Worker in the central spawn policy, preserve `without_subagent`, and use the explicit reasoning restore setter. Persist no derived plan fields.

**Step 4: Verify GREEN**

Run focused core, engine, and TUI tests.

## Task 6: Verify affected crates and release builds

**Files:**
- Format only files changed by Tasks 1–5.

Run serially with `CARGO_BUILD_JOBS=8`:

```bash
cargo fmt --check
CARGO_BUILD_JOBS=8 cargo test -p synaps-core
CARGO_BUILD_JOBS=8 cargo test -p synaps-engine
CARGO_BUILD_JOBS=8 cargo test -p synaps-tui
CARGO_BUILD_JOBS=8 cargo check --workspace --all-targets
CARGO_BUILD_JOBS=8 cargo check --release -p synaps-engine
CARGO_BUILD_JOBS=8 cargo check --release -p synaps-tui
bash scripts/loc-ratchet.sh
bash scripts/ignore-ratchet.sh
git diff --check
```

Use changed-file `rustfmt` before the check rather than formatting unrelated files. If a PTY-only test flakes with the documented stream-fd symptom, retry that unchanged command once and record both outcomes.

## Task 7: Blind behavior testing and convergence review

**Files:**
- Generate ignored artifacts under `.convergence/`

Prepare the informed black-box pipeline from this plan/design. Sage writes behavior scenarios; Glitch runs real focused and regression tests; Arbiter scores spec compliance, quality, coverage, edge cases, and security. Require overall score at least 0.8. Address any REWORK feedback with another RED/GREEN slice, rerun Glitch/Arbiter, then commit verified logical slices.

Final handoff reports:

- exact upstream and live evidence;
- captured RED output;
- changed behavior and intentional Synaps worker divergence;
- commit hashes;
- all test/check commands and outcomes;
- remaining uncertainties;
- confirmation that nothing was pushed or globally installed.
