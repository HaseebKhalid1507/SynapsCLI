# Context rollover recovery validation

Validated 2026-09-07 against the current uncommitted working tree. Existing
unrelated changes were preserved; workspace-wide results are not a claim that
all changes belong to this task.

## Behavior

An unproductive soft rollover is now a typed preparation outcome, not a fatal
configuration error. When the unchanged full request passes hard admission, the
runtime retains its history and checkpoint note and continues automatically.
It neither writes an archive nor publishes a new head, advances the window, or
resets elapsed/cumulative budgets. A bounded cooldown avoids immediately retrying
the same no-op: reassess after four admitted rounds or a footprint change of at
least 8,192 estimated tokens. Request-local guidance tells the model not to repeat
checkpoints merely to force a rollover.

Hard capacity is checked before the cooldown on every round. Cancellation,
missing retrieval tools, archive failures and unacknowledged durable heads remain
blocking. Time boundaries still require a durable successor. Candidate budgets
include request-only overhead and the greater of configured minimum reserve and
computed next-round reserves. Human messages, attachments, constraints and
protocol-complete tool tails are not silently dropped or summarized.

Legacy autonomous prompts remain retained: this patch deliberately avoids
identifying disposable messages by text matching. Prompt provenance/storage
changes are outside this bounded fix.

## Verification

All Cargo operations were serialized with at most eight build jobs. Test suites
used one test thread. Local mock providers and temporary storage only; no live
provider inference, installed-binary replacement or live memory mutation.

| Check | Result | Evidence |
| --- | --- | --- |
| Core context policy tests | 14 passed | `/tmp/context-recovery-policy.log` |
| Engine continuation tests | 20 passed | `/tmp/context-recovery-continuation.log` |
| New runtime integration suite | 3 passed | `/tmp/context-recovery-integration.log` |
| Full offline workspace suite | 4,258 passed, 0 failed, 38 ignored; 124 result summaries | `/tmp/context-recovery-workspace.log`, `.exit` = 0 |
| Strict production Clippy (workspace libraries and binaries) | PASS | `/tmp/context-recovery-clippy-production.log`, `.exit` = 0 |
| Release build | PASS | `/tmp/context-recovery-release.log`, `.exit` = 0 |
| Scoped formatting and `git diff --check` | PASS | Final local checks |

The new runtime tests prove that repeated small checkpoints continue through six
provider rounds without head publication or replay, then stop at the original
provider-round limit; pinned over-capacity history stops before network I/O; and
a productive rollover still waits for the head acknowledgement, with failed
acknowledgement preventing inference. Existing full-suite time-boundary and
archive/head durability regressions also pass.

All-target Clippy was not rerun for this task; pre-existing test-only failures
are documented in `session-recovery-validation.md`. The strict production-code
result above is not an all-target lint claim.

Built artifact (not installed):

```text
814aa7dc85af442c9a158cc54859b47e9396c899a7fc0aaff81b4057ee70afd1  target/release/synaps
```

Deployment requires replacing the installed host and resuming the session in a
new process. An already-stopped autonomous driver is not silently reauthorized;
restart the loop explicitly after resuming. Existing conversations remain usable.
