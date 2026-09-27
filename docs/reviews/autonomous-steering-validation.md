# Autonomous human steering follow-up — 2026-09-06

## Behavior implemented

- Editing the input box (typing, paste, deletion, history recall) never revokes an
  armed run and never sends the draft. Automatic work can continue while typing.
- Submitted ordinary text uses same-run steering, not the ordinary single-slot
  `queued_message` / user-takeover path. The grant, worker epoch, deadline and
  model/effort allowlist remain unchanged.
- Bounded FIFO: 16 pending messages, 256 KiB total UTF-8. Distinct and repeated
  submissions preserve order. Channel acceptance is not delivery: unacknowledged
  messages survive end-of-stream and join the next authorized proposal as
  separate user messages. Latest history is revalidated at commit after async
  preparation; no duplicate policy call or overlapping stream is started.
- Delivery acknowledgment provisionally retains the human message in frontend
  history, so abort/error-before-history cannot lose it. The engine's later
  authoritative history replaces that vector rather than duplicating the append.
- Steering waiting during asynchronous setup is drained before the first provider
  round; normal request validation still runs. Later in-flight steering is
  cooperative, not an interrupt/rollback of already-started external work.
- Human steering clears local repetition comparisons. A callback already sent to
  the plugin settles once with its existing selection/delay/limit decision. It is
  deliberately not aborted/replayed to reset accounting or bypass cooldowns;
  queued steering waits for its next authorized turn. This is a linearization
  limit, not an immediate-fallback-reset guarantee.
- Unsent steering is restored to the input draft on stop/failure and never starts
  an unowned automatic turn. ESC/Ctrl-C, explicit stop/quit/control commands
  (including status), deadlines, lifecycle and safety gates still stop the run.
- Staged attachments never drain into automated prompts. Armed attachment
  submission is rejected with draft retained and guidance to press Escape first.
- External plugin 0.1.2 prompts explicitly honor latest human steering; the
  original goal is historical context, not an override. No host loop policy or
  favorite order was added to Rust.

## Verification

All Rust builds/tests orchestrated sequentially with `CARGO_BUILD_JOBS=8`, `-j 8`,
serial test harness (`--test-threads=1`). No live provider smoke or autonomous run.

- TUI driver tests: **55 passed** including new ordered steering, ACK/abort,
  drafts, attachment retention, feedback, pending-task and timer regressions.
- Engine loopback first-request steering test: **1 passed**.
- Full workspace: **4,169 passed, 0 failed, 34 ignored, 1 filtered**, 123 result
  summaries including doc tests. Filter is the previously documented ambient
  Copilot-account-dependent `ui_catalog_fetch_github_copilot_returns_prefixed_chat_models`.
  The terminal tool timed out its wrapper at 300s; the cargo child continued and
  was supervised until exit. Log contains every final test/doc-test result and no
  failure markers; wrapper exit file was not written, so no captured exit-code
  claim is made for this run.
- Python plugin: **50 passed** (serial offline subprocess/policy tests).
- Workspace Clippy exit 0 with existing unrelated warnings. Removed the new
  duplicate test cfg warning; final TUI Clippy exit 0 with no driver-test warning.
- Release build exit 0; builtin tool schema, LOC and ignore ratchets, diff check
  passed. `synaps plugin validate` is not an available CLI subcommand in this host;
  manifest/load/protocol validation is covered by the real-plugin Rust tests.

Logs: `/tmp/autonomous-steering-{check,tui-test,engine-test,workspace,clippy,final-clippy,python,release}.log`.
Install receipt: `/tmp/autonomous-steering-install.json`. These are ephemeral.

## Installed

- PATH host: `/home/jr/.cargo/bin/synaps`, version `synaps 0.9.0`
- SHA-256: `981a7f17c51e495074b55b7b3e0acde8d2f89f5ca3fffcdd67cf44de38374ea2`
- Plugin: `/home/jr/.synaps-cli/plugins/autonomous`, **0.1.2**
- Private previous host/plugin backup:
  `/home/jr/.synaps-cli/.autonomous-steering-backup-mp9z1_ny/`

Host installed by staged atomic replacement; plugin by atomic directory exchange.
Existing installed plugin data were copied before replacing only code/docs/tests/
manifest. No config/default/favorite changes, live-session restart, commit, reset,
or cleanup of the existing uncommitted work. Already-running host/plugin processes
keep old code until restarted; no live-session reload was forced.
