# Phase 3 Holdout Verdict — Durable CP-9 Gate Record

This document records the Checkpoint CP-9 (Phase 3 gate) evidence and the
independent holdout Judge verdict, captured before any Phase 4 work began.

## CP-9 command evidence at HEAD `42e34c59`

- `CARGO_BUILD_JOBS=8 cargo check --workspace --all-targets --jobs 8` — exit 0
  (run independently by the foreman, in addition to the builder's own run).
- `CARGO_BUILD_JOBS=8 cargo test --workspace --jobs 8 -- --test-threads=8` —
  99 suites, 3095 passed, 0 failed, 10 pre-existing `#[ignore]` harnesses
  (log: `/tmp/cp9-workspace-test.log`), including `phase3_activation` 14/14
  and `phase2_trace_conformance` 19/19 green at 8 threads.
- `cargo test --test phase3_activation` — 14/14; additionally rerun by the
  builder at `--test-threads=1` and `--test-threads=4`, and independently
  rerun by the holdout Judge at `--test-threads=1` (14/14 in 0.06s).
- `git diff --check` — clean.
- Independent foreground syscall audit of the committed phase3 test binary:
  `strace -f -e trace=%network` showed AF_UNIX socketpairs only, zero
  AF_INET/AF_INET6 sockets/connect/send calls
  (`/tmp/phase3-activation-testbin-network.strace`,
  `/tmp/phase3-activation-network.strace`).
- CP-9 gate blocker found by the foreman (`synaps-tui` lib-test fixture missing
  the Task 20 `deferred` field) was fixed in `42e34c59` — 10 insertions across
  3 test/fixture files, no enforcement code touched, verified semantically
  sound by the holdout Judge.

The following verdict is recorded verbatim from the fresh, independent,
read-only holdout Judge (`anthropic/claude-fable-5`).

---

# Phase 3 Holdout Verdict — PASS

- **Verdict:** PASS
- **Weighted total:** (0.35 × 0.92) + (0.30 × 0.93) + (0.20 × 0.88) + (0.10 × 0.90) + (0.05 × 0.92) = 0.322 + 0.279 + 0.176 + 0.090 + 0.046 = **0.913** ≥ 0.80 ✔
- **Spec-fidelity gate:** 0.88 ≥ 0.70 ✔
- **Commit range:** be1a8952..42e34c59 (25 commits), HEAD 42e34c59, branch feat/request-lifecycle-hardening
- **Reviewer:** Independent holdout Judge (read-only inspection; single permitted cached test run; no state mutated)

## Per-axis table

| Axis | Weight | Score | Weighted | Evidence |
|---|---|---|---|---|
| Security/privacy | 0.35 | 0.92 | 0.322 | Fail-closed `ExecutionGate` with 7 typed denial variants incl. `NotCataloged`/`UntrustedSource`/`SourceProvenanceMismatch` (activation.rs:508–650); check order is stale-generation → digest → grant-tuple → trust → acquisition, acquisition strictly last (activation.rs:~600–660); model cannot self-authorize (`ActivationAuthority` host-only, activation.rs:746; a14 proves forged consent JSON denied); deferred MCP tools spawn nothing without a lease capability even off-gate (mcp/descriptors.rs:407); descriptor invalidation revokes the exact grant (descriptors.rs:~430–450, commit ceba5fa6); secret_env launch record made crate-private, no Debug/Serialize (3a4d8bf1 A3); skill metadata control-sanitized + bounded at output boundary (41c25b2b). Deduction: non-stream `run_single` builtin dispatch bypasses the gate (see Findings). |
| Correctness | 0.30 | 0.93 | 0.279 | phase3_activation 14/14 personally rerun green at HEAD; retained log: 99 suites / 3095 passed / 0 failed; sibling-batch authorization done under one registry read guard before any spawn (stream.rs:610–670, no TOCTOU between siblings); `activate_many` all-or-nothing typed batch (activation.rs:726); a10 asserts exact typed variants with id equality, not mere `is_err`. |
| Spec fidelity | 0.20 | 0.88 | 0.176 | Every §7 acceptance bullet maps to a named a01–a14 test (table below); flag-off behavior preserved via `default_core_for_catalog` = all verified-provenance capabilities, zero activations (activation.rs:174); interim side-effect policy and non-live permission re-check are explicitly documented as Task 24/Task 20 scope in "honesty notes" (activation.rs:~560–680) — accepted as spec-consistent interim per plan T16 ("conservative default until T24"). Deduction for run_single scope narrowing (documented at runtime/mod.rs:~1766). |
| Code quality | 0.10 | 0.90 | 0.090 | Typed identities throughout; bounded, metadata-only error messages; poisoned-lock handling explicit; commit series maps 1:1 to plan tasks with review-fix commits; CP-9 fix surgical (10 insertions, 3 files). Minor: pre-existing rustfmt drift left in subagent_wake.rs (acknowledged in commit message). |
| Docs | 0.05 | 0.92 | 0.046 | docs/request-lifecycle-progressive-disclosure.md (budget note, +62 lines); docs/tools.json + extensions/contract.json updated; commit messages carry verification evidence and honest scope caveats. |

## Spec §7 requirement-by-requirement assessment

| Requirement | Status | Evidence |
|---|---|---|
| §7.1 ToolCatalog (typed IDs, digests, generation; insertion grants nothing) | Met | catalog.rs (837 LOC), commits 06c2669f/d0c32b78/665a548a; a03 spawn-spy |
| §7.1 DiscoveryIndex bounded, never full schemas | Met | discovery.rs:35–38 (16 results / 8 KiB budgets); a03 |
| §7.1 SessionToolSet per session, zero inherited activation | Met | activation.rs:140–160 (`new` starts with empty `activated`); a07 |
| §7.1 ExecutionGate: resolve→generation→digest→grant→trust→acquire, typed fail-closed | Met (stream + extension-provider paths) | stream.rs:510, 654; extensions/runtime/process.rs:239; activation.rs:573–672. Non-stream run_single: see Finding M-1 |
| §7.2 search_tools/activate_tools/search_skills/load_skill; credential/network-free | Met | discovery.rs tools; strace logs 0 AF_INET; a03, a04 |
| §7.3 Opt-in minimal core + byte budget; flag-off identical | Met | config.rs:297/338 (default false, opt-in test at :1059); `progressive_core_for_catalog` fixed essential list (activation.rs:189); a01/a02; a05 legacy-set compatibility assertion |
| §7.3 User exact-request ergonomics (no redundant prompt) | Met | `activate_exact_for_user` host API, no authority value (activation.rs docs); used throughout harness |
| §7.4 MCP: no process/network pre-selection; one server; no siblings; lease; invalidation; revocation | Met | descriptors.rs (`dormant_tools_for_config` never spawns); lease.rs; typed `McpLeaseError::{FingerprintDrift,NameNotListed,SchemaMismatch}` → exact grant revocation (descriptors.rs:~425–450); a08, a10 |
| §7.5 Extension classes; deferred coupled to exact permissions; runtime/manifest identity+digest match | Met | e6c41b54, 3a4d8bf1 (deferred.tools requires `tools.register` pre-spawn; hook class without subscription fails closed), 4900a7d5, f9b58753; a09 |
| §7.6 Lazy skill bodies, bounded frontmatter, hash-verified selection | Met | skills/loader.rs ("never one body byte" boot scan, frontmatter SHA-256, body_start offset); a12; sanitization fix 41c25b2b |
| §7.7 Deterministic bulk updates; catalog insert never rebuilds schemas; alias/collision safety intact | Met | 0aba6e3d (strictly additive batch registration); `activate_many` single generation bump; a13, a06 |
| §7 acceptance: forged/alias/staleness/digest/generation/cross-provider/inheritance | Met | a05/a06/a10/a11/a07 with typed-variant assertions (phase3_activation.rs:794, 809, 858) |
| §13.4 adversarial activation scenarios | Met (activation subset) | tests/phase3_activation.rs (1187 LOC) + tests/execution_gate_stream.rs (555 LOC); remaining §13.4 scenarios (loops, 1 GiB output, cancellation races) are Phase 4+ scope |

## Findings

**Critical:** none.

**Important:** none.

**Moderate:**
- **M-1 — Non-stream `run_single` builtin dispatch bypasses the ExecutionGate.** runtime/mod.rs:~1766–1776 and :1943: the legacy synchronous tool loop resolves via `tools_snapshot.get()` with no gate, no digest/generation check. Impact is materially mitigated: deferred MCP/extension tools fail typed with zero spawn because `mcp_leases`/`extension_leases` are `None` on this path (descriptors.rs:407; verified subagent fixtures in 42e34c59 encode the same invariant), the extension-provider interior loop on this path IS gated (tool_session_id threaded, session_tool_set falls back to fresh default-core), and the path exposes legacy full schemas (flag-off-equivalent behavior). The gap is explicitly documented in-source as tracked, and plan Task 16 scoped the pass gate to stream-turn paths. Residual: a flag-on deployment using `run_single` does not get minimal-core disclosure or digest pinning for builtins.
- **M-2 — Gate trust re-check is consistency-only, not live revocation.** activation.rs `check_source_trust` honesty note: manifest-permission revocation after cataloging passes until the catalog entry is rebuilt. Partially closed by Task 20 (descriptor-invalidation revocation, deferred/permission coupling at validation); full live-policy consultation is deferred. Documented; not a silent gap.

**Minor:**
- Side-effect classification is a single `Unclassified` variant permitted only for verified provenance (activation.rs match); real classes are Task 24. Acceptable interim per plan.
- Pre-existing rustfmt drift in tests/subagent_wake.rs left in place (acknowledged in CP-9 commit).
- a05/a06 assert `is_err()` rather than exact variants for some forged spellings (a10/a14 do assert exact variants, so typed-denial coverage exists overall).

**CP-9 fixture-fix commit (42e34c59):** verified semantically sound — `deferred: None` matches the fixture's genuinely eager live-process lifecycle (comment records intent); `mcp_leases: None`/`extension_leases: None` in manual subagent test contexts strengthens rather than weakens enforcement (deferred tools fail typed, spawn nothing). No enforcement code touched; 10 insertions across 3 test/fixture files only.

**Commit-to-task mapping:** all 25 commits map cleanly — T14: 06c2669f, d0c32b78, 665a548a; T15: 76fc81e4, 2d4fecf5; T16: 93ee80a4, b9edec80; T17: 94a85c0c, caa79d25; T18: a3a9dd64, 86042744; T19: 736c2cac, 0aba6e3d, 63ad99a6, c18ed345, ceba5fa6; T20: e6c41b54, 3a4d8bf1, 4900a7d5, f9b58753; T21: 5f953454, 41c25b2b; T22: 48428543 (cross-provider adapter API for a11), cfd723ae; CP-9: 42e34c59. No unmapped or suspicious changes; the unstaged spec edit is confirmed uncommitted (`git status` shows only `M docs/request-lifecycle-hardening-spec.md`; I reviewed the committed version).

## Residual risks / future-scoped
- Gate coverage for the non-stream `run_single` builtin loop (tracked in-source; M-1).
- Live permission/revocation consultation inside the gate (M-2); real side-effect classes and confirmation policy (Task 24).
- Default flip of `progressive_tool_disclosure` awaits tool-selection quality evidence (explicitly out of scope per plan T18).
- Remaining §13.4 scenarios (infinite loops, giant outputs, cancellation-race side effects, cross-project memory) belong to Phase 4+.

## Evidence I personally verified vs. accepted
**Personally verified (read/ran):** committed spec §7 + §13.4 and plan T14–T22/CP-7..9 via `git show 42e34c59:`; full commit log and per-commit diffs for 42e34c59, 41c25b2b, 3a4d8bf1; ExecutionGate source and both stream-loop call sites plus the extension-provider call site; `check_source_trust`, `SessionToolSet` construction (default vs. progressive core), `ActivationAuthority`; deferred MCP tool execute path and revocation-on-invalidation; discovery budgets; skills loader frontmatter-only boot scan; config flag default-off; the `run_single` ungated path; phase3_activation.rs assertions for a05–a08, a10, a14 (non-vacuity confirmed: typed variants, id equality, spawn-spy emptiness, catalog-membership preconditions); **ran** `cargo test --test phase3_activation -- --test-threads=1` at HEAD: 14/14 passed in 0.06s; grepped both strace files myself: zero AF_INET/AF_INET6 occurrences; confirmed /tmp/cp9-workspace-test.log contains 99 suite result lines all "0 failed".
**Accepted from retained evidence (not rerun):** the full-workspace 3095-pass total and per-suite composition of cp9-workspace-test.log; foreman's `cargo check --workspace --all-targets` exit 0 and `git diff --check` clean at HEAD; the strace capture methodology (I verified contents, not collection).
