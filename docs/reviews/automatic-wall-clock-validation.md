# Automatic wall-clock continuation validation — 2026-09-06

## Implemented

The user's clarification supersedes the initial manual-only recovery design:

- `/context auto`: every committed durable successor starts a fresh elapsed-time segment. Time expiry can force the same archive/head barrier below context pressure. Only elapsed time resets; other resource/cost counters remain cumulative. Save failure, unverified history, pending workers, cancellation and zero allowance do not bypass safeguards. A normal completed answer still ends the stream.
- External autonomous plugin **0.1.4**: Start-only `time_checkpoint_version: 1` opts into typed `time_checkpoint` / `wall_clock` callbacks. The plugin continues retained work in a new turn on the same exact model/effort, including with context mode off. Checkpoints are not success, provider failures or repetition feedback, and do not increment successful-turn counts. Original deadlines, cancellation, lifecycle and permission gates remain authoritative. Legacy drivers remain blocked on time exhaustion.
- `/budget status` and `/budget time 4h` remain available as explicit runtime-only controls. No global configuration changes or model-callable budget renewal tool.

See `docs/specs/turn-wall-clock-recovery.md` for semantics and boundaries.

## Final verification

Serialized builds with `CARGO_BUILD_JOBS=8` / `-j 8`; Rust harnesses used `--test-threads=1`. Offline dependencies, synthetic temporary state and loopback providers only; no live provider inference.

- Python policy and stdio suite: **58 passed**.
- Engine session-driver tests: **21 passed**.
- TUI session-driver tests: **56 passed**.
- Stream-focused tests: **31 passed**, **2 ignored** (explicit real-Axel integrations, not enabled in this run).
- Actual external-plugin host integration: **7 passed**, including same-model context-off time continuation, idempotent callbacks and successful-turn counting.
- Full workspace: **4,227 passed, 0 failed, 37 ignored, 1 filtered**, across **123** summaries including doc tests. The filtered test is the previously documented ambient-account-dependent `ui_catalog_fetch_github_copilot_returns_prefixed_chat_models`.
- Workspace all-target Clippy: exit **0**; existing unrelated warnings remain. No warning reported against the touched budget/continuation/driver/command implementation files.
- Release build: exit **0**, `synaps --version` reports **0.9.0**.
- Release-binary builtin schema drift, LOC ratchet, ignore ratchet and `git diff --check`: all passed.

Initial focused failures were test plumbing (missing qualified imports/attribute and expected Start-field sets); corrected and rerun before the final full suite.

Evidence driver: `/tmp/auto-time-final.sh`; final stage receipts `/tmp/auto-time-final-{focused,workspace,clippy,release,schema,loc,ignore,diff}.{log,exit}`. Focused details `/tmp/auto-time-{python,engine,tui,stream,plugin}.{log,exit}`. All eight final stage exit files were rechecked as zero. No build/test process remained at final verification.

## Artifacts and installation status

**Built, not installed.** No user configuration, live session, installed plugin or installed binary was modified by this task. No session restart, migration, deletion, git reset/clean or commit. Existing uncommitted work remains intact.

SHA-256:

- New `target/release/synaps`: `dfbf37dfb10164a4a2757c03093ea988f87ae5fd38bd722cce2d61ff0cc60432`
- Unchanged `/home/jr/.cargo/bin/synaps`: `a6011dc36d52806381e30e482716f18160ea4e4b9d8b3a40f539de5bc77cfdd1`
- Plugin source `examples/extensions/autonomous/main.py`: `5e7f313dfe5b8e437b75ca9ad4b9768113c90bbd095ab4f6783fa56e6c533125`
- Plugin manifest `examples/extensions/autonomous/.synaps-plugin/plugin.json`: `8775cd3ef2bc7da7bbcd0d2074af036b50f0b52a93a18fb52ed1f8f66e761bd8`

Install the new host and plugin together to use the versioned autonomous continuation contract. The Axel service did not change in this task. Existing processes do not acquire new code merely because a release artifact was built.
