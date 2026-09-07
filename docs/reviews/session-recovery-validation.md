# Session delegation / interrupted-stream recovery validation

Validated 2026-09-07 against the current uncommitted working tree. This tree
contains other ongoing features; results below are workspace-wide, not a claim
that all changes belong to this task.

## Fixed behavior

- Manifestless engine policies count outstanding workers rather than lifetime
  launches. A terminal worker continues to consume capacity until explicit
  collection/reconciliation. Explicit manifest/constructed cumulative limits,
  concurrency, model authority, and write-scope checks remain intact.
- A separate monotonic worker identity prevents rollback from reusing IDs.
  Reaper retirement uses only exact removed runtime IDs; it cannot accidentally
  retire a worker registered after a snapshot. One-shot workers retire after
  their normal terminal/collect/reconcile lifecycle.
- Built-in Codex HTTP and generic Chat Completions/xAI broker streams retry
  interrupted response bodies and EOF without a successful wire terminal.
  Request bytes are unchanged; retries are bounded, cancellation-aware, and
  never execute tools from an unsuccessful response. A complete response followed
  by transport disconnect is not replayed.
- Response-attempt start/reset events remove discarded previews from TUI,
  worker accumulation, autonomous feedback, and server display history. RPC and
  WebSocket clients receive explicit boundary events; plain terminal output
  marks the discarded attempt. Prior rounds, human steering and reported usage
  remain. Exhausted transient retries retain auto-driver transient classification.

## Verification

All Cargo operations serialized, with `CARGO_BUILD_JOBS=8` / `-j 8`; tests used
`--test-threads=1`. No live provider inference or installed-binary replacement.

| Check | Result | Evidence |
| --- | --- | --- |
| `cargo test --workspace --offline -j 8 -- --test-threads=1` | PASS: 4,251 passed, 0 failed, 38 ignored across 123 test result summaries | `/tmp/session-recovery-final-workspace.log`, `.exit` = 0 |
| `cargo clippy --workspace --lib --bins --offline -j 8 -- -D warnings` | PASS | `/tmp/session-recovery-final-clippy-production.log`, `.exit` = 0 |
| `cargo clippy --workspace --all-targets --offline -j 8 -- -D warnings` | NOT GREEN: existing test-only lint findings outside this fix | See below |
| Scoped `rustfmt --check` and `git diff --check` | PASS | Final local checks |
| `cargo build --release --offline -j 8` | PASS | `/tmp/session-recovery-final-release.log`, `.exit` = 0 |

Regression coverage includes 150 reconciled dispatches in one baseline session,
terminal-unreconciled/concurrent limits, explicit cumulative limits, rollback ID
uniqueness, exact-ID retirement, real chunked HTTP EOF with partial text/tool
arguments, identical replay bytes, finite retries, cancellation during backoff,
completed-then-disconnected responses, broker policy-denial non-retry,
reported-usage retention, suppressed preview handling, and TUI steering retention.

Release artifact SHA-256:

```text
29295849375dd9925b1f1f6ee1ee8ab6ed33679118a0f97c160143be68c50e18  target/release/synaps
```

## All-target Clippy limitation

Initial runs found pre-existing lints in core Copilot test helpers,
`provider_confinement`, and `tool_discovery_session`. A bounded diagnostic attempt
corrected those locally and exposed additional test lints in
`storm_incident_replay`, `tools/test_helpers`, `runtime/api` tests,
`google_gemini/setup` tests, trace diagnostics/google fixtures, transport tests,
and `tools/util` item order. The unrelated diagnostic edits were restored exactly;
this change does not broaden into workspace-wide test-lint cleanup. Evidence:
`/tmp/session-recovery-clippy.log`,
`/tmp/session-recovery-clippy-targets.log`, and
`/tmp/session-recovery-final-clippy-all.log` (the last describes the temporary
lint-unblocking state, not the final tree).

No install, service restart, provider/config edit, migration, or live memory
mutation was performed for this task. Already running hosts cannot hot-load
these Rust changes; use a newly installed host and resume the existing session
when deploying. Existing conversations do not need to be discarded.
