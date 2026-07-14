# Anthropic Max and Ultracode implementation plan

**Specification:** `docs/anthropic-max-ultracode-spec.md`
**Base:** `b1646af`
**Method:** strict RED → minimal GREEN → refactor; one heavy Cargo command at a
time with `CARGO_BUILD_JOBS=8`.

## Constraints and commands

Only implementation work after approval may edit product code. Do not use the
read-only Claude Code pane to code. Do not infer positive model IDs, use live
credentials, install globally, push, or change Codex Max/Ultra. Format only
changed Rust files (not `cargo fmt --all`). Useful discovery where `rg` is not
installed uses `grep -RIn` and `find`.

Core verification:

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
git status --short
```

## Tasks

### 1. Evidence-locked manifest and logical type

**Scope/files:** `crates/agent-core/src/core/reasoning.rs` (or actual existing
reasoning module), `crates/agent-engine/src/runtime/openai/catalog/anthropic.rs`,
manifest fixture/module, colocated tests. No request/TUI changes.

**Dependencies:** none.
**RED:** tests distinguish `Ultracode` from Codex `Ultra`; reject unknown parse
values; assert the production manifest has zero Max/Ultracode positives at this
base; synthetic exact IDs exercise valid flags; malformed version, duplicate,
unqualified ID, contradiction, unknown value, and broad-name inference deny.
Record test names and failure output.
**Implementation:** add closed logical/capability types and versioned exact-ID
manifest validation. Each future positive row must include an evidence citation.
No wildcard allow.
**Acceptance:** checked-in evidence is represented exactly, with no production
positive invented; Max, Ultra, Ultracode serialize distinctly in logical
persistence.
**Verification:** focused core/catalog tests, then `cargo test -p synaps-core`.

### 2. Pure typed planner and preflight ordering

**Scope/files:** catalog validation/planner and
`crates/agent-engine/src/runtime/{mod,api,api_sync}.rs`; broker/transport test
fakes only. No body or TUI behavior yet.

**Dependencies:** Task 1.
**RED:** table tests for every selection/role/capability combination; exact
provider/model mismatch; absent/narrower live data; missing orchestration
manifest/tools/limits; worker/internal Ultracode; and fake counters asserting
credential, broker, DNS/transport calls remain zero.
**Implementation:** derive `AnthropicExecutionPlan` from qualified ID, logical
selection, role, authoritative capability, and typed prerequisites. Invoke the
same pure validation at mutation and request boundaries before auth/network.
Keep auth broker-only.
**Acceptance:** Max plans only with exact Max evidence; foreground Ultracode is
xhigh+standing; worker/internal deny; all malformed/unknown states fail closed
with stable safe codes and `*_attempted=false`.
**Verification:** focused planner and preflight tests; inspect call order and
sanitized captured diagnostics.

### 3. Plan-only body serialization and cache ordering

**Scope/files:** `crates/agent-engine/src/runtime/request.rs`, body golden tests,
cache helper integration. No worker or UI changes.

**Dependencies:** Task 2.
**RED:** body tests require Max wire `"max"`, Ultracode wire `"xhigh"`, zero
wire occurrences of `ultracode`, serializer rejection of raw selection, exact
canonical key/block ordering, stable cache markers, and byte-identical unchanged
requests. Numeric fixed-budget tests retain exact integers and omit
`output_config`.
**Implementation:** serializer consumes only the typed plan. Compose standing
policy deterministically before existing cache marker application. Preserve
body and prompt-cache order exactly.
**Acceptance:** no undocumented field, no reorder, no logical leak, unchanged
body goldens byte-identical, numeric compatibility exact.
**Verification:** focused request/body/cache tests and a source/test scan for
wire-building uses of `ultracode`.

### 4. Foreground workflow and recursion boundary

**Scope/files:** runtime orchestration policy and
`crates/agent-engine/src/tools/subagent/{mod,start,oneshot,resume,...}.rs` only as
needed. No TUI changes.

**Dependencies:** Tasks 2–3.
**RED:** every fresh/resumed worker and internal route denies Ultracode before
broker/network, receives no standing policy, and lacks recursive subagent-start;
foreground requires all lifecycle tools and authorization; cancellation/final
completion leaves no detached work; policy appears exactly once.
**Implementation:** install typed standing policy only on foreground plans;
centralize role assignment and recursion denial; use existing bounded tools,
completion gates, and provider-qualified authorization. Never pre-spawn.
**Acceptance:** foreground model-directed orchestration works through existing
lifecycle, workers cannot recurse, and no orchestration survives foreground
lifecycle.
**Verification:** focused orchestration/subagent tests with fake tools/broker,
then engine tests.

### 5. Persistence, resume, commands, and TUI race guards

**Scope/files:** core session/config tests and
`crates/agent-tui/src/tui/{effort,settings,commands,input}.rs` plus event-loop
apply path. No provider transport changes.

**Dependencies:** Tasks 1–4.
**RED:** exact option-order/filter matrix; Max/Ultracode/Codex Ultra distinct
round trips through config/session/compaction/continue/resume; explicit
provenance survives model switches; numeric budgets exact; rejection unchanged
state/disk; stale model/capability generation, streaming, duplicate key,
stream-start and stream-completion races cannot apply.
**Implementation:** derive options from exact capability; modal snapshots model
and generation; event-loop atomically rechecks all guards and is sole mutation
and persistence path. Revalidate rather than lower on resume/model switch.
**Acceptance:** exact ordered options, no stale apply, no streaming mutation,
idempotent duplicate/race handling, distinct durable logical state. Codex UI is
unchanged.
**Verification:** focused TUI/session tests, then core and TUI suites.

### 6. Unattended fake-broker/transport harness

**Scope/files:** engine integration tests/fixtures only; no live endpoint.

**Dependencies:** Tasks 2–5.
**RED:** scenario table initially fails for missing behavior. Include valid
synthetic Max and foreground Ultracode captures, unsupported/unknown/malformed
manifest, wrong provider, missing workflow/tools, worker/internal recursion,
resume, numeric budget, prompt-cache ordering, cancellation, and Codex
non-regression.
**Implementation:** in-process fake broker records credential/proxy calls;
recording transport captures body/order and can gate stream completion with
barriers. Fixed seed, bounded timeouts, temporary isolated config/session dirs,
no ambient credential/env use. A deny asserts zero broker/transport calls; allow
asserts one expected call and sanitized output.
**Acceptance:** fully non-interactive and deterministic; exits nonzero on
mismatch/timeout; cleans temporary state; never logs secrets or requires network.

**Verification:** a named filtered integration command documented by the test
module, run twice to expose order/race flakiness.

### 7. Regression, security, and final verification

**Scope/files:** tests and necessary narrow fixes only.

**Dependencies:** Tasks 1–6.
**RED/GREEN:** add any missing regression before fixing it; no speculative
refactor. Run core commands above serially. Inspect diff for direct credentials,
raw body/secret logging, family inference, wildcard allows, and Codex changes.
If a PTY-only documented flake occurs, retry the identical command once and
record both outcomes.
**Acceptance:** all checks pass; only intended files changed; no positive ID
without exact cited evidence; no live/global side effect; clean diff/check.

## Checkpoints

1. **Evidence checkpoint (after Task 1):** reviewer can trace every manifest row;
   positive list is empty unless exact new evidence is checked in.
2. **Security checkpoint (after Task 2):** deny scenarios prove zero credential
   and network attempts; broker-only boundary is intact.
3. **Wire checkpoint (after Task 3):** Max=`max`, Ultracode=`xhigh`, no wire
   `ultracode`, cache/body order and numeric compatibility pass.
4. **Role checkpoint (after Task 4):** foreground-only doctrine and all worker /
   internal recursion denials pass.
5. **UX durability checkpoint (after Task 5):** exact options, race guards, and
   distinct persistence/resume pass.
6. **Release checkpoint:** unattended harness twice, all workspace/release/
   ratchet/diff commands green.

The autonomous implementer does not pause at checkpoints; it records evidence,
runs verification, and continues unless blocked by an unrecoverable error.

## Convergence holdout

Use an independent holdout scenario set not exposed to implementation fixtures.
Score:

| Dimension | Weight |
|---|---:|
| Exact semantic/spec compliance | .35 |
| Security/auth/preflight/role boundaries | .20 |
| Wire, cache-order, numeric, Codex compatibility | .20 |
| Persistence/TUI/race behavior | .15 |
| Code quality and diagnostics | .10 |

Threshold is **0.80** weighted total, with no critical security failure and no
wire serialization of `ultracode`. Budget: **maximum 10 evaluation calls** and
**maximum 2 fix rounds**. Each fix round begins with a new failing test and
reruns affected plus holdout scenarios. If threshold is unmet after either
budget is exhausted, stop implementation, preserve artifacts, and report
non-convergence rather than weakening tests or inferring capabilities.

## Convergence declaration

Planning converges on one architecture: exact evidence-backed capability → pure
typed plan → pre-auth preflight → plan-only serialization; logical Ultracode is
foreground xhigh plus standing orchestration, while Anthropic Max is genuine
wire max. Worker/internal recursion is denied, the manifest fails closed,
logical persistence remains distinct, and Codex Max/Ultra remain untouched.
There are no competing open implementation semantics. The sole deliberate
holdout is the documented model-ID evidence gap: production positives remain
empty until exact source-controlled evidence exists.

## Final handoff record

Report base/final commits, exact evidence and positive IDs (expected none), RED
failures, changed files, every command/outcome, harness and holdout scores/call
count/fix count, remaining uncertainties, clean `git status`, and confirmation
that nothing was pushed and Claude Code was read-only.
