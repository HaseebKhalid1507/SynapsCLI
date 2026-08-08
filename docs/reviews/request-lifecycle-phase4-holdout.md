# Phase 4 Holdout Verdict — FINAL FAIL

- **Final verdict:** FAIL after the configured maximum of two fix iterations.
- **Weighted total:** `0.777` (required: `0.80`).
- **Spec-fidelity gate:** `0.76` (required: `0.70`; this sub-gate passed).
- **Reviewed range:** `8c7c7fa5..3ea1c757`.
- **Final reviewed HEAD:** `3ea1c757b8a79f49f3a1081da914c4c3e2e08ae0`.
- **Reviewer:** fresh independent holdout Judge (`openai-codex/gpt-5.6-sol`), read-only.
- **Gate effect:** Phase 5 must not begin while the Important finding below remains.

## Final score

| Axis | Weight | Score | Weighted |
|---|---:|---:|---:|
| Security/privacy | 0.35 | 0.68 | 0.238 |
| Correctness | 0.30 | 0.88 | 0.264 |
| Spec fidelity | 0.20 | 0.76 | 0.152 |
| Code quality | 0.10 | 0.84 | 0.084 |
| Documentation | 0.05 | 0.78 | 0.039 |
| **Total** | **1.00** |  | **0.777** |

## Final blocking finding

### Important — extension `command.invoke` output remains unbounded

Production `command.invoke` retains an
`mpsc::UnboundedSender<InvokeCommandEvent>` and drains it only after the
call resolves. The rationale that an awaited bounded send would deadlock
against this post-hoc drain explains why a direct channel substitution is
invalid, but does not establish a byte bound.

The existing controls are insufficient:

- one in-flight user action limits concurrency, not bytes;
- a 120-second timeout limits duration, not aggregate output;
- the 4 MiB frame cap limits individual frames, not queued frames;
- the bounded notification queue does not bound its unbounded downstream
  sink while the invocation loop continues forwarding output.

A hostile or malfunctioning extension can therefore make host memory grow
with command output during the invocation window. This violates spec §8.4's
production-time bounded-output requirement.

A future repair needs an invocation-local byte budget with UTF-8-safe
truncation/coalescing and dropped-byte accounting, or an eagerly concurrent
consumer that writes into bounded state/private spill without deadlocking.

## Disposition of earlier findings

- **Outer runtime model stream queue:** resolved by `740f008d`; bounded
  caller relay, preview byte budget, exact accounting, and caller-drop
  cancellation are covered across Anthropic, OpenAI, and Gemini.
- **Extension provider stream/notification queues:** resolved for provider
  streaming by `3ea1c757`; both handoffs are bounded and use awaited
  backpressure. The separate command-output sink above remains blocking.
- **Symlink plus parent traversal path identity:** resolved by `37416b28`;
  existing components are resolved before subsequent `..` handling, with
  conservative serialization on unresolvable paths.
- **Vacuous Phase 4 umbrella tests:** resolved by `1a1c0bd4`; production
  stream, scheduler, bash, cancellation, and delegation paths are exercised.
- **Spill adversarial evidence:** resolved for umask-000 and planted final
  symlink threats.
- **Delegation release and grant inheritance:** substantially resolved by
  `97a077a2` and `7420afe6` with production/race tests.
- **Unsupported RSS claim:** resolved; tests now claim deterministic retained
  byte ceilings rather than process RSS.

## Moderate residuals

1. Canonical path scheduling remains subject to post-resolution symlink
   TOCTOU; source documentation states this honestly.
2. Non-preview relay event boundedness depends on finite turn/tool limits;
   future large event variants must be classified explicitly.
3. Lossless extension notification fan-out permits one stalled subscriber to
   backpressure others. This is memory-safe but is a head-of-line trade-off.

## Verification evidence

Final retained CP-11 evidence at `3ea1c757`:

- `CARGO_BUILD_JOBS=8 cargo check --workspace --all-targets --jobs 8` — exit 0
  (independently rerun by the foreman).
- `/tmp/cp11-fix2-workspace-test.log` — 104 test summaries, 3183 passed,
  0 failed, 10 ignored.
- `git diff --check` — clean for committed changes.
- Working tree contains only the pre-existing unstaged
  `docs/specs/request-lifecycle-hardening-spec.md` edit; it was excluded from the
  reviewed committed specification.

The Judge personally inspected committed source/diffs/tests for the outer
relay, provider handoffs, extension sidecar frame limits and cancellation,
path resolution, and remaining unbounded payload channels. It did not rerun
Cargo tests or modify files.
