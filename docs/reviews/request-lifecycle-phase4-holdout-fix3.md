# Phase 4 Holdout Verdict — PASS (Human-Approved Fix Iteration 3)

This record supersedes the gate outcome in
`request-lifecycle-phase4-holdout.md` while preserving that earlier final
failure as historical evidence. The human explicitly authorized one
exceptional third fix iteration for its sole remaining Important finding.

- **Verdict:** PASS
- **Weighted total:** `0.926` (required: `0.80`)
- **Spec-fidelity gate:** `0.91` (required: `0.70`)
- **Severity gate:** no Critical or Important findings
- **Reviewed range:** `8c7c7fa5..d3751827`
- **Reviewed HEAD:** `d37518274df5306a71ec3c61375c34a22bd1e822`
- **Reviewer:** fresh independent holdout Judge (`openai-codex/gpt-5.6-sol`), read-only

## Per-axis scores

| Axis | Weight | Score | Weighted |
|---|---:|---:|---:|
| Security/privacy | 0.35 | 0.94 | 0.329 |
| Correctness | 0.30 | 0.93 | 0.279 |
| Spec fidelity | 0.20 | 0.91 | 0.182 |
| Code quality | 0.10 | 0.90 | 0.090 |
| Documentation | 0.05 | 0.91 | 0.046 |
| **Total** | **1.00** |  | **0.926** |

## Prior blocker disposition

**Resolved.** The former unbounded, post-hoc `InvokeCommandEvent` retention
boundary was replaced by:

- a bounded channel of eight events;
- an eagerly concurrent collector joined directly with invocation;
- a 256-KiB ordinary payload and 1024-event retention budget;
- a separate bounded 16-KiB control reserve;
- exact produced, consumed, retained, truncated, and dropped accounting;
- UTF-8-safe partial retention only for string payloads;
- whole-event retention/drop for tables and task events;
- first-`Done` preservation and bounded `task.done` preservation for retained
  task starts.

No hidden unbounded `InvokeCommandEvent` hop remains. The collector holds no
runtime/manager lock and awaits no UI action. `tokio::join!` supplies concurrent
progress without a detached task or lifetime cycle. Cancellation, timeout, or
transport failure drops the uniquely owned sink, closes the collector, and
releases producers blocked on awaited sends.

The control reserve is bounded and cannot be used as an aggregate bypass:
only the first zero-byte `Done` is retained there; `task.done` eligibility
requires a retained `task.start`; starts are bounded by ordinary budgets; only
one completion per task ID survives; and reserve payload is capped at 16 KiB.
All extension-controlled strings are included in payload accounting.

A sibling audit found and closed the extension widget lane. It is now bounded
to 256 events with nonblocking drop-on-overflow, avoiding deadlock of the shared
lossless notification fan-out. Widget updates are idempotent/upsert UI state;
a later accepted update restores current display state.

## Spec §8 assessment

All Phase 4 requirements are satisfied at the evidence level required by the
program: explicit turn budgets and exact stopping; conservative effects and
conflict-aware scheduling; typed side-effect ledger; bounded high-volume
production channels; explicit backpressure/coalescing/drop policies; limiting
before aggregate materialization; independent UI/history budgets; cancellation
release; correlated lifecycle metadata; model-order preservation; valid
synthetic results; canonical-path serialization; safe read overlap; no blind
non-idempotent replay; huge-output retention bounds; and no surviving
forwarding tasks or delegation leases.

## Findings

- **Critical:** none.
- **Important:** none.
- **Moderate:** none.
- **Minor:** none.

## Residual risks

- One extension frame may transiently occupy up to the existing roughly 4-MiB
  frame cap, and bounded channels may retain a small fixed number of frames.
- The final JSON-RPC response is a single frame-limited response rather than
  part of the 256-KiB aggregate notification budget.
- Retained Rust containers have bounded overhead beyond charged UTF-8 bytes;
  the independent event-count cap bounds this overhead.
- `Done` is preserved when emitted but is not synthesized for a hostile
  extension that never emits it; the invocation result/error remains terminal.
- Widget overflow may temporarily skip presentation updates until a later
  event is accepted; this is the intended memory-safe trade-off.

## Verification evidence

At implementation HEAD `d3751827`:

- `CARGO_BUILD_JOBS=8 cargo check --workspace --all-targets --jobs 8` — exit 0
  (independently rerun by the foreman).
- `/tmp/cp11-fix3-workspace-test.log` — 105 summaries, 3199 passed, 0 failed,
  10 ignored.
- `git diff --check` — clean.
- Targeted `rustfmt --check` — clean.
- RED evidence: the old architecture retained 41,943,040 bytes / 641 events.
- Mutation evidence: disabling the output budget retained the full flood;
  restoring post-hoc collection deadlocked and tripped the 30-second oracle.
- GREEN evidence: retained payload fixed at 262,144 bytes (278,528 bytes with
  control reserve), exact conservation, terminal result preserved, and prompt
  cancellation/transport release.

The Judge personally inspected the committed spec, collector accounting and
reserve invariants, manager/process/TUI production wiring, flood and mutation
oracles, widget policy, and sibling output channels. It did not modify files or
rerun Cargo commands. The unrelated unstaged
`docs/specs/request-lifecycle-hardening-spec.md` edit was excluded from review.
