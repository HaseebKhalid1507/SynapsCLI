# Autonomous default context follow-up — 2026-09-06

## Behavior

- External autonomous plugin **0.1.3** sends `context_mode: "auto"` on every
  explicit `/auto start`, unless `--context off` is supplied before the goal's
  mandatory `--`. Mixed flag order is supported; duplicate/unknown flags,
  missing values, mistyped values and `--context=off` fail closed.
- Generic host Start-only field accepts exactly `auto` or `off`. Explicit null
  and other types/values are rejected. Other/legacy drivers omitting the field
  leave the current context mode unchanged. Next/Poll cannot change the mode.
- After explicit user grant validation, the TUI uses the same runtime command
  path as `/context auto|off`. Invalid starts do not change context mode;
  unavailable context setup stops before inference, not as a retryable provider
  error. Existing durability barriers and retrieval/media/safety gates remain.
- This enables eligible local context archival/continuation, not continuous
  memory recall/capture consent. No global config or plugin preference changes.
  Archive exclusions and limitations are disclosed in the Start notice/docs.
- Setting remains in the **current runtime after stop**, like `/context`;
  it is not persisted to disk or restored across restart. New explicit starts
  default to auto again. Resume/restart never re-arm automation.
- Covers the autonomous plugin's **foreground session only**. Actual spawned
  workers are unchanged: enabling context rollover there additionally requires
  their own durable-checkpoint consumer/tool support. No worker authority or
  checkpoint support is claimed by this change.

## Verification

All Rust verification serialized with `CARGO_BUILD_JOBS=8`, `-j 8`, and
`--test-threads=1`. No live provider calls or autonomous smoke run.

- Engine session-driver tests: **20 passed**.
- TUI session-driver tests: **55 passed**.
- Real-process Rust plugin tests: **6 passed**, including default auto and
  explicit off protocol behavior.
- Python plugin policy/stdio suite: **56 passed**.
- Full workspace: **4,173 passed, 0 failed, 34 ignored, 1 filtered**, 123 result
  summaries including doc tests; captured exit **0**. Filter is the previously
  documented ambient Copilot-account-dependent
  `ui_catalog_fetch_github_copilot_returns_prefixed_chat_models`.
- Workspace Clippy exit **0**, existing unrelated warnings remain; no new
  session-driver warning was reported.
- Release build exit **0**. Builtin schema drift check, LOC/ignore ratchets and
  `git diff --check` passed.
- Scoped read-only review found no concrete blocker in this foreground change.

Ephemeral evidence: `/tmp/autonomous-context-{engine,tui,plugin-rust,python,workspace,clippy,release,schema}.log`;
workspace/clippy/release/schema have `.exit` files. Verification driver:
`/tmp/autonomous-context-verify.sh`. Installation receipt:
`/tmp/autonomous-context-install.json`.

## Installation

- PATH host: `/home/jr/.cargo/bin/synaps`, `synaps 0.9.0`.
- SHA-256: `de2118826eddd019c9d81a73ba24fda7877d9058e9ce9d86a18a27e2ba65dc4c`.
- Plugin: `/home/jr/.synaps-cli/plugins/autonomous`, **0.1.3**.
- Previous host/plugin backup:
  `/home/jr/.synaps-cli/.autonomous-context-backup-v_xvp81v/`.

Host staged and atomically replaced before atomic plugin directory exchange.
Installed source-owned plugin files match tested source byte-for-byte; existing
non-code plugin files would be preserved (none were present). An initial staging
script path/string type error occurred before publishing either host or plugin;
corrected before successful installation. Its unpublished staging was moved into
private backup `.autonomous-context-backup-r0xgau4k`, outside discovery, not deleted.

No active host/plugin process was restarted/reloaded, no favorite/config updates,
no commit/reset/cleanup of uncommitted repository changes. **Restart Synaps** to
load the new host and plugin together; running processes retain old code. Older
strict hosts reject new Start fields rather than silently ignoring this setting.
