# Forum tool compatibility and diagnostics validation — 2026-09-07

## Fix

- Model-facing `forum_post`, `forum_read` and `forum_forget` accept omitted or null optional fields. Null defaults normalize at the tool boundary; the Axel wire, stored envelopes, content digests, and project identity do not change. Required values and members of a non-null cursor remain strictly typed/non-null. Empty or mismatched project confirmations still fail closed.
- Obvious all-zero/all-f thread/reply/cursor placeholders fail with recovery guidance. They are never silently converted to a wider read or a new root. Other unknown thread IDs remain exact scoped queries, not project selectors.
- Codex and xAI Responses routes use the same tested tool-definition helper with explicit `strict: false`. This avoids relying on implicit strict normalization of ordinary optional schemas. Chat Completions retains its existing schema behavior. Nullable optional fields also support callers that materialize every property.
- Axel errors retain vetted codes and static host-authored guidance without echoing arbitrary service error text. Negative replies require clean EOF and successful child exit too. Transport, storage, protocol, unknown, and size-limit failures retain conservative write-outcome uncertainty; no automatic retry or fallback was added.
- Forum post failures now say there is no success receipt instead of instructing an unconditional retry. Worker guidance distinguishes attempted tool activity from publication and gives valid listing/new-thread/reply patterns.
- Updated `docs/agent-forum.md`, the forum specification, and exported tool schemas. Export comparison verified that only the three forum schema entries changed.

## Verification

All builds were serialized with `CARGO_BUILD_JOBS=8` / `-j 8`; Rust harnesses used `--test-threads=1`. Offline dependencies and synthetic temporary state only. No provider inference or writes to real forums.

- Forum tool tests: **17 passed**.
- Axel transport/error tests: **3 passed**, including safe diagnostic labels and rejection of error replies followed by trailing output or nonzero process exit.
- OpenAI translation tests: **11 passed**, including Responses strict opt-out, nullable schema preservation, unchanged required fields, and tool-name mapping.
- Explicit real-Axel forum integrations against the installed service: **3 passed**. These cover nullable model-facing root/reply/list/delete calls, exact retry/digest stability, project refusals, actionable missing-reference errors, reopen persistence, eight concurrent peers, paging/export/import/deletion, and verified Git-worktree identity. Only private disposable synthetic brains were used.
- Full workspace: **4,232 passed, 0 failed, 38 ignored, 1 filtered**, across 123 summaries including doc tests. The single filter is the previously documented ambient-account-dependent `ui_catalog_fetch_github_copilot_returns_prefixed_chat_models`. The new ignored test requires an explicit Axel executable and was run successfully in the integration stage above.
- Workspace all-target Clippy: **exit 0**, with preexisting unrelated warnings. No warning was reported against the changed forum/transport/translation files.
- Debug and release builds: **exit 0**.
- Release builtin schema drift, LOC ratchet, ignore ratchet and `git diff --check`: **passed**.

Initial verification caught an over-broad scripted replacement of the stream translation region and a missing test-only `Value` import. Both were corrected before the successful focused and full-suite runs; the restored stream region retains the existing trace-scrubbing seam and other dirty multimedia changes. Independent delegation was unavailable due to the session worker limit; no independent review verdict is claimed.

Evidence: `/tmp/forum-fix-{targeted,process,translate,real}.{log,exit}` and `/tmp/forum-fix-final-*.{log,exit}`. The serial driver `/tmp/forum-fix-final.sh` completed all stages; `/tmp/forum-fix-final.exit` is zero. No build/test process remained after completion.

## Operational state

**Built, not installed.** The new release is `target/release/synaps`. The Axel service did not change and does not need a rebuild for this fix. Running sessions still retain their current host code until restarted using an installed new host.

SHA-256:

- New `target/release/synaps`: `f5a2bb1889ecd6befbebafebe7076eb7fabd2856784156dded8dd76283e13c22`
- Unchanged PATH host `/home/jr/.cargo/bin/synaps`: `a6011dc36d52806381e30e482716f18160ea4e4b9d8b3a40f539de5bc77cfdd1`
- Unchanged PATH service `/home/jr/.cargo/bin/synaps-axel-memory-service`: `356a8ef71efb6664448c02faadcd6a4355a29e3d1a9d647cc5217ab94129ecbe`

No installed binary/plugin, configuration, live memory, or session was modified. No migration, restart, commit, reset, clean, or push. Existing uncommitted work is preserved.
