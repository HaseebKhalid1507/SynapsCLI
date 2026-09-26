# Turn wall-clock continuation and explicit recovery

## Diagnosis and corrected behavior

`budget_exceeded dimension=WallClock` is a local elapsed-time boundary, not a provider/account failure. Compiled defaults remain foreground 7,200 seconds, autonomous/watcher 900 seconds and delegated worker 3,600 seconds. The local TUI's external `/auto` plugin drives the foreground runtime; it is not the watcher execution role.

Previously the original stream's meter survived all context rollovers, eventually stopping legitimate long-running work. The first patch added explicit `/budget` recovery only. The user clarified that **automatic continuation** is required in `/auto` or `/context auto`; the behavior below supersedes the original manual-only design.

During investigation, the shared metadata log contained elapsed 7,211 seconds against 7,200 seconds. This was not independently attributable to `crm-1` / `20260905-120206-a607`. No conversation bodies were needed. Checks occur at safe budget boundaries rather than forcibly interrupting every in-flight operation, so elapsed time can overshoot.

## Context auto: durable time segments

- Every successful, durably acknowledged context successor starts a fresh wall-clock segment. Only elapsed time resets; cumulative cost, tool calls/results, provider rounds/renewal allowance, permissions and worker authority do not.
- Elapsed-time exhaustion with `/context auto` forces the existing archive + durable-head barrier even below context-pressure thresholds. A low-pressure time successor need not shrink, but must fit the request budget. Pressure-triggered rollover still requires meaningful shrinkage.
- The clock resets **after** archive success, frontend durable save acknowledgement and successor commit, never on model checkpoint text, attempted save or failure. Cancellation is checked before another provider request.
- Required builtin retrieval tools, backend availability, pending workers and durable-head checks remain authoritative. A failed/unsupported save stops before successor inference; no fallback to an uncommitted history.
- A zero allowance stops; a time segment with no completed provider round cannot roll over repeatedly without inference.
- Provider-round renewals alone do not reset time. A normally completed assistant answer still ends the stream; `/context auto` does not independently create an infinite prompt loop.

## External autonomous plugin: fresh turns

Plugin 0.1.4 explicitly sends Start-only `time_checkpoint_version: 1`. The host pins this opt-in in the grant; legacy grants stay blocked on wall-clock exhaustion. Unsupported versions/types fail closed. An older strict host rejects the new Start; update both host and plugin.

Only typed `BudgetExceeded { WallClock }` becomes a `time_checkpoint`/`wall_clock` callback. It is not provider failure, success or repetition feedback. The host observes it exactly once after normal retained-history processing. The existing lifecycle, cancellation, deadline, worker, permission, media and durable-head gates still govern polling and submission. Zero allowance stops.

The plugin proposes a fresh turn on the same exact model/effort after its ordinary one-second delay, using retained history and latest human steering. It does not replay completed actions or fallback to another provider. Checkpoints do not increment `--turns` (which counts successful turns); retry/repetition streaks reset. `--for` continues to use the original run deadline. Duplicate callbacks are idempotent. This also works with `/auto start --context off -- ...`. Other budgets, policy/tool/storage failures and ambiguous side effects still stop; Escape and revocation are unchanged. Loading/restarting never restores a run.

## Manual recovery remains available

- `/budget` or `/budget status`: configured limits for future turns, not live usage.
- `/budget time 4h`: only the elapsed allowance for future turns in this runtime. Positive whole numbers with lowercase `s`, `m` or `h`; 1 second through 24 hours. Zero, signs, fractions, compounds, missing units, unknown subcommands, overflow and extra arguments are rejected atomically.
- The command does not write config/session storage, start inference, mutate history, change workers or alter already-running budget snapshots. Available in the shared engine command handler (idle local TUI, headless chat, WebSocket server), with builtin reservation/completion/help. No model-callable renewal tool or new RPC operation.
- Explicit `/budget` commands retain the local driver's existing control-command revocation behavior; reauthorize `/auto start` if needed.
- Outside automatic modes, send a new prompt to continue retained history with a fresh budget. To persist a foreground allowance, set `turn_budget.foreground.max_elapsed_secs = 14400` in active-profile config before starting a new host. This implementation does not edit user config or restart live sessions.

## Diagnostics and regression coverage

The wall-clock diagnostic includes elapsed/configured seconds, local-limit explanation, retained-history recovery and explicit command/config paths. The engine preserves the typed budget outcome; other dimensions retain existing messages. Stream history is emitted before terminal error.

Tests cover strict duration parsing/status/atomic mutation, unchanged in-flight snapshots and defaults, clock-only context renewal, low-pressure time rollover with exact tool-result preservation and durable-save success/failure, versioned driver opt-in, zero-budget stop, exactly-once callback observation, same-model continuation with context off, success/deadline accounting, duplicate callbacks and replay-safe prompts. Existing context pressure/durability, autonomous lifecycle, budget enforcement, workspace tests and lints remain required.
