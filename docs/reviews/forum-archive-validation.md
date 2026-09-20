# Project forum and archive budgeting validation

## Implemented

The project forum adds `forum_post`, `forum_read` and `forum_forget`, with a shared pure contract, content-addressed digests, host-assigned worker authors, captured repository identity, Axel transactions, bounded keyset pages, domain separation from notes/automatic recall, deletion-safe retries, and full export/migration preservation. It is project-wide across verified worktrees, not a private swarm ACL. Peers poll explicitly; forum data does not grant authority or wake workers. See `docs/agent-forum.md` and `docs/specs/agent-forum.md`.

The archive fix budgets eligible source rather than raw excluded payloads. It preserves source indices, accepted projection digests, withholding, structural/output bounds and the durable-head barrier. It does not truncate oversized eligible evidence or alter consent. See `docs/specs/archive-eligible-input-budget.md`.

## Verification

All builds were serialized with `CARGO_BUILD_JOBS=8` / `-j 8`; Rust test harnesses used `--test-threads=1`. Synthetic repositories, temporary private brains, test doubles and loopback providers only. No live provider inference, private session/trace reads, actual memory migration, config changes or active-session restart.

- Core archive tests: **28 passed**, including legacy golden digests captured before replacing the raw budget walk, excluded aggregate >16 MiB, transactional tool-input withholding, recursion/node bounds, exact multi-MiB evidence and JSON escape expansion before redaction.
- Focused engine eligible-budget tests: **3 passed**, **1 explicit-service test ignored** in the ordinary run; that service test was subsequently run successfully against debug and release Axel.
- Full workspace final run: **4,216 passed, 0 failed, 37 ignored, 1 filtered**, 123 result summaries including doc tests; captured exit **0**. Filter is the previously documented ambient Copilot-account-dependent `ui_catalog_fetch_github_copilot_returns_prefixed_chat_models`.
- Independent Axel service suite: **57 passed**, **0 failed**, **0 ignored**.
- Explicit ignored host integrations against the newly built **release** Axel service: **7 real-Axel tests passed**, including actual loopback runtime rollover/durable resume, new eligible archive budgeting, capture/recall, history import and terminal worker capture. **2 additional forum integrations passed**, including 8 concurrent peer writers, paging, export/import/deletion and Git-worktree persistence. No unrelated ignored tests were enabled.
- Workspace Clippy: exit **0**, existing unrelated warnings remain. No new archive/forum warning reported. Independent service all-target Clippy with `-D warnings`: exit **0**.
- Both release builds: exit **0**. Release `synaps --version` reports **0.9.0**. Builtin schema drift check, LOC/ignore ratchets and `git diff --check` passed.

### Failures found and corrected

The first full workspace pass failed only `tools_export` and `tools_registry_iter`: the three new forum tools made the builtin count 28 instead of 25, and `docs/tools.json` had not yet been regenerated. Updated exact expected names/counts (also included the existing `memory_context` in the export-name fixture), regenerated the schema and verified existing tool entries were unchanged from the pre-forum snapshot. The subsequent **full workspace** pass succeeded; this was not waived as an unrelated failure.

The first new rollover unit-test attempt left its synthetic continuation mode off before calling `checkpoint`. Fixed the test setup to enable Auto, then reran successfully. The earlier forum integration fixture needed its own temporary directory made 0700; production private-path checks were not relaxed.

The bounded independent forum reviewer timed out without a final verdict. The archive reviewer supplied a useful diagnosis/design but also timed out; its guarded whole-argument and escaping-compatibility recommendations were implemented and tested. Foreground source review and verification were performed; **no completed independent security-review verdict is claimed**.

## Artifacts and operational state

Release artifact SHA-256:

- `target/release/synaps`: `a6011dc36d52806381e30e482716f18160ea4e4b9d8b3a40f539de5bc77cfdd1`
- `sidecars/axel-memory-service/target/release/synaps-axel-memory-service`: `356a8ef71efb6664448c02faadcd6a4355a29e3d1a9d647cc5217ab94129ecbe`

**Built, not installed.** PATH binaries were verified unchanged:

- `/home/jr/.cargo/bin/synaps`: `de2118826eddd019c9d81a73ba24fda7877d9058e9ce9d86a18a27e2ba65dc4c`
- `/home/jr/.cargo/bin/synaps-axel-memory-service`: `d368c9da0005254fdd10fcd082dd3da2a5384a2948e3418ed734e8d7b797461d`

Both new host and service must be installed together to use the forum in a fresh runtime. No host/plugin reload or restart was performed. Existing uncommitted work remains intact; no commits, reset, clean, push or source deletion.

Ephemeral evidence: `/tmp/archive-{golden,core,engine,real-service}.log`, corresponding `.exit` files, `/tmp/forum-{check,targeted,service-tests,real-service}.*`, and `/tmp/forum-archive-{workspace,clippy,service-tests,service-clippy,service-release,real-axel,forum-real,release,schema}.{log,exit}`. Initial full-suite failure retained as `/tmp/forum-archive-workspace-initial.{log,exit}`. Driver `/tmp/forum-archive-verify.sh` finished all stages; no worker remains active.

## Subsequent user-authorized installation — 2026-09-06

User requested `install` after source verification. Rechecked all nine successful
validation exit files and exact release hashes; no rebuild was needed. Installed
both release artifacts to their PATH-resolved locations in `/home/jr/.cargo/bin/`
using staged, fsynced files and per-file atomic rename (service first, then host).
The pair is not a single filesystem transaction; the installer retained backups
and rollback logic for a detected failure. Both final hashes match the artifacts
listed above. This supersedes the earlier **built, not installed** state.

Validated staged and installed binaries with isolated `--version`, builtin tool
export (28 tools including all three forum tools), and storage-free Axel Hello
plus capabilities (`synaps-axel/2`, forum operations advertised). No brain was
created. No live session, configuration, plugin, memory or repository state was
migrated or restarted.

Private backup and durable installation receipt:
`/home/jr/.local/state/synaps-cli/binary-backups/20260906T160546Z-forum-archive-bf4vabij/`.
Ephemeral receipt: `/tmp/forum-archive-install.json`. Restart Synaps to load the
new host; already running hosts retain their mapped old executable.
