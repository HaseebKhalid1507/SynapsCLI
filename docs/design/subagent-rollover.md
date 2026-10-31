# Automatic context rollover for subagent workers

Status: implemented (#332).

## Why

Telemetry for 10-24 to 10-29: workers are 66% of all usage, and their
contexts never shrink. Before 10-28 a worker request averaged 287k tokens;
now one named worker averages 265k (max 486k), another 221k, and one reached about
999k. Every round re-reads the whole context, and cache reads are the largest
single cost line (47%). The foreground session already rolls over
(`context_management.mode = auto`). Workers get none of it.

Rough economics on Opus 5.5 at the 5-minute TTL workers use: one rollover
writes a ~100k successor (about $0.50), and each later round reads ~150k
fewer tokens (about $0.03), so a rollover pays back in about 17 rounds. A
worker grows by a few thousand tokens a round, so it takes 20 to 50 rounds to
climb from the ~100k successor back to the 200k threshold: every window pays
for itself before the next rollover, and workers run hundreds of rounds.

## How the foreground rollover works today

1. **Policy (pure).** `agent_core::core::context_policy::assess_context`
   takes the config, a per-context `ContextState`, and a `ContextBudget`
   (window, estimated footprint, hard remaining, reserve for the next round).
   It returns an action: `Continue`, `Advisory` (pressure band),
   `FinishBounded` (a Plan/WrapUp round after rollover is due), `Rollover`,
   or `HardStop` (no room for another round). `ContextThresholds::resolve`
   gives the bands (140k/180k at a 200k window, 250k/400k at 1m,
   interpolated, with a lone override moving the other band only if
   ordering needs it). `target_tokens` is the successor footprint:
   `min(100k, 2/3 of pressure)` unless set.
2. **Admission (runtime/stream.rs, "CONTEXT CONTINUATION: pre-request
   assessment").** Before every provider round, when
   `continuation.config.mode == Auto`, the stream loop estimates the full
   request (`pre_request_context_budget`: system + tool schemas + messages +
   reserves), runs `assess_context`, persists `next_state`, and on
   `Rollover`/`HardStop` (or a wall-clock checkpoint):
   - requires builtin `memory_search`/`memory_fetch` to be admitted;
   - waits up to `WORKER_ROLLOVER_GRACE_ROUNDS` (4) for the *foreground's*
     pending workers to be collected, then carries a manifest of them
     (`render_worker_manifest`) into the successor;
   - sizes the successor with `successor_budget` (target in provider units,
     converted through the latest `TokenCalibration` sample);
   - `rollover_for_boundary` builds a candidate successor, refuses a no-shrink
     candidate (`Unproductive`, deferred by `defer_unproductive_rollover`),
     seals the full history with `MemoryBinding::history_seal`, rebuilds the
     successor around the archive id, and `persist_head` sends a
     `ContextHeadCheckpoint` that the frontend must durably acknowledge
     before the next inference (unacknowledged = fail closed);
   - commits (`PreparedRollover::commit` resets the policy state and note),
     renews the wall-clock segment, publishes the history, and emits
     "Continued automatically in context window N …".
3. **Successor shape (`continuation::successor`).** A continuation message
   (archive id, a bounded index of dropped *user/event requests*, the
   working note, the worker manifest) followed by retained messages: the last
   assistant message and everything after it, the request that started the
   turn, attachments, the session's first request if it fits 30% of the
   target, then other *user requests* newest first. Tool call/result groups
   are kept whole (`tool_groups`), assistant messages are never edited, so
   signed thinking blocks travel verbatim.
4. **Archive.** `history_seal` writes to the selected memory backend: the
   legacy local `ArchiveStore` (`context-windows-v1`, redacted) or, when Axel
   is exclusive, an Axel RPC. The model retrieves evicted context with
   `memory_search(source=history)` and `memory_fetch`.
5. **`context_checkpoint` and the working note.** Registered only in auto
   mode. The model reports a phase (`plan`, `execute`, `wrap_up`,
   `new_task`) and an optional note. A boundary report in the pressure band
   rolls over early; the note rides into the successor. The rollover itself
   does **not** depend on it: the rollover band triggers regardless.
6. **Mode and consent.** `Runtime::apply_config` copies
   `context_management` into `continuation.config`; `/context auto|off`
   (`context_management_command`) toggles it per session. Enabling auto is
   the user's consent to archive eligible session text into the memory
   backend. The default is `off`.

## Why workers get none of it (verified)

- Worker runtimes come from `EngineHost::worker_runtime()` →
  `Runtime::from_parts`, which builds `ContinuationState::default()` (mode
  `Off`). Workers never run `apply_config`, and
  `apply_subagent_runtime_policy` (tools/subagent/mod.rs) sets cwd/env,
  telemetry, retries, memory backend, the Worker role, the forced 5m TTL and
  the worker turn budget, but never touches `continuation`. So
  `context_enabled` is false and the whole assessment block is skipped.
- Flipping the mode on would not work either. `WorkerDrive::on_event`
  (tools/subagent/drive.rs) ignores `ContextHeadCheckpoint`, so the receipt
  is dropped and `persist_head` fails closed: the first rollover would kill
  the worker. The successor shape keeps user requests plus the last tail; a
  worker transcript is one brief followed by hundreds of assistant /
  tool_result rounds, so it would keep the brief and one round. And the
  evicted history would be sealed into the parent's memory backend, which can
  be the Axel brain.
- **What happens at hard capacity today:** nothing checks. The worker keeps
  sending its whole history; when it no longer fits, Anthropic returns
  `invalid_request_error` ("prompt is too long"), which
  `classify_stream_outcome` treats as terminal (not retried). The drive turns
  `SessionEvent::Error` into `Flow::Failed`; the worker ends `failed`, and
  the finalizer archives its full history. A `subagent_resume` of that
  archive replays the same oversized history (or, if cold and large, #315's
  compact context).

## Design for workers

A host-only rollover that reuses the policy engine and keeps evicted history
in the worker's own archive directory.

### Trigger and thresholds

- Same admission point as the foreground (top of every provider round), same
  estimator (`pre_request_context_budget`), same `assess_context`. The worker
  owns its own `ContextState` for the turn.
- Config: `subagent_context_management = auto | off` (default `auto`) and
  `subagent_rollover_tokens` (default 200000, minimum 20000). The worker's
  policy is a `ContextManagementConfig { mode: Auto, rollover_tokens:
  min(subagent_rollover_tokens, the window's default rollover band),
  finish_rounds: 0, .. }`, so pressure and target are derived exactly like
  the foreground's: at a 1m window, rollover 200k, pressure 199,999, target
  100k; at a 200k window the window's own band (180k) wins and the target is
  93,333. `finish_rounds` is 0 because workers never report phases.
- Why 200k, not the foreground's 400k: workers are long-lived loops on the 5m
  TTL with no human pauses, so the cache stays warm and every round pays the
  read of the whole context. The payback arithmetic above holds with margin at
  a 100k → 200k cycle. The key is tunable per user.
- `Rollover` and `HardStop` both attempt a rollover. Hard capacity therefore
  becomes a rollover instead of a terminal provider error, even with a high
  configured threshold. If a rollover cannot help, the round proceeds exactly
  as it does today. That includes a window the reserves dominate (below).

### Context window

A worker budgets against the configured `context_window`, as the foreground
does (`apply_config`): `apply_subagent_runtime_policy` copies it. The policy
runs before the spawn path picks the model, so `fit_subagent_context_window`
runs after `set_model` on every path (oneshot, start, resume) and drops a
window above the model default on a model without 1M support
(`model_supports_1m`): the request builder never sends that model the 1M
opt-in, so its provider window is the default and the worker must budget
against that. The setting is one per user, not per model, so a worker on a
different model than its parent (an agent file or the call can pin one) gets
it whenever its model can open it: a 1m parent's Sonnet 4.6 worker gets 1m,
its Haiku 4.5 worker 200k. Non-Anthropic models get the model default (200k
in `context_window_for_model`, the conservative side). Like the rest of
the policy, the value comes from the config file: `/context` and the
`/settings` row write it there, but a session-only window change that is
not persisted is not inherited.

Until this was fixed (#340) workers never got the
setting, and the reserves made that expensive. `pre_request_context_budget`
then reserved the next round's whole output ceiling (`max_tokens`, 128k for
Opus), the next tool result (10k), a 15% safety margin and 512 advisory
tokens; on a 200k window that is about 168k for Opus, so hard capacity sat
near 32k against a 93k target. Every hard-capacity successor was two thirds
of the ~20k left (about 9k on the wire), full again about five tool calls
later: four Opus 5.5 workers rolled over 28 to 37 times in 20 minutes with
requests of about 20k.

**The output reserve** (#341). `max_tokens` is a
ceiling, not headroom a request needs: on Claude 4.5 and later a request
whose input plus `max_tokens` exceeds the window is accepted, and
generation that reaches the window stops with
`stop_reason: "model_context_window_exceeded"`; earlier models return a
validation error ([context windows, "Context window overflow
behavior"](https://platform.claude.com/docs/en/build-with-claude/context-windows#context-window-overflow-behavior)).
The shared reserve model (`runtime::context::assess`, used by the
foreground, workers, keep-warm and `Runtime::assess_context`) now reserves
what one round realistically produces: its thinking budget
(`ZERO_BUDGET_THINKING_RESERVE_TOKENS` = 16,384 for adaptive/off, the
request builder's own fallback) plus `EXPECTED_RESPONSE_TOKENS` (16,384),
capped at `max_tokens`. With the tool result, margin and advisory:

| Model, window (adaptive or high thinking) | Required next round | Hard capacity | Before |
| --- | ---: | ---: | ---: |
| Opus, 200k | 73,280 | 126,720 | 31,488 |
| Sonnet, 200k | 73,280 | 126,720 | 95,488 |
| Opus or Sonnet, 1m | 193,280 | 806,720 | 711,488 / 775,488 |

Medium thinking (4,096) gives 139,008 on 200k, xhigh (32,768) on Opus
110,336. For the models that reject the overflow (Opus 4 / 4.1, Sonnet 4,
3.x: `model_rejects_output_overflow`), every Anthropic request builder
(stream, sync, compaction) clamps `max_tokens` to the provider window
minus the conservative input estimate (`fit_max_tokens_to_window`), never
below the thinking budget plus 4,096. The provider window is the 1M window
exactly when the request sends the 1M opt-in, else the model default.
Claude 4.5+ request bodies are byte-identical.

**Reserve-dominated windows.** `WorkerPolicy::hard_capacity_has_room`: a
rollover at hard capacity is attempted only when the hard-capacity line
(`window − required next round`) is above the policy's successor target,
the footprint the policy itself calls a useful context. Below it no
successor of that size passes hard admission, and any the window does hold
refills to hard capacity within a few rounds. Such a worker does not roll
over at hard capacity; its requests go out as before worker rollover and
the provider judges them. It logs one warning per turn. With the realistic
reserve (thinking up to xhigh) no 200k or larger window is reserve-dominated (200k: line 126.7k,
target 93.3k); windows below about 110k still can be (100k: 41.7k against
46.7k).

**Which comes first, band or hard capacity.** On a 200k window the 15%
margin and the tool result alone take 40.5k, so hard capacity (at most
~159.5k, ~126.7k with the output reserve) always comes before the default
180k band, for workers and the foreground alike (the context-management
spec's "180,000 or earlier hard reserve boundary"). A worker on 200k
therefore rolls over at ~126.7k into a successor of at most two thirds of
the room left, and windows last a dozen rounds or more; with
`subagent_rollover_tokens` below the line (say 100k) it rolls at its band.
On 1m the 200k worker band governs.

### `context_checkpoint`: host-only boundary

Workers do **not** get `context_checkpoint`. The foreground rollover does not
depend on it (it only allows an earlier roll at task boundaries and carries a
model note). A worker is one task: there are no task boundaries to report.
Adding the tool and `GUIDANCE` would grow every worker request (schema and
system prompt) for nothing, and invite checkpoint rounds. The host writes the
note instead (below).

### What the successor keeps

In order:

1. **The brief, verbatim** (message 0: the task for `subagent_start`, the
   replayed history's first message for a full resume, the single compact
   message for a cold resume). Always kept, even over budget. The worker's
   system prompt is not in `messages`, so it survives automatically.
2. **Later orchestrator instructions** (steering messages, the
   "[Resumed by the orchestrator …]" text that resume merges into a
   tool_result message) from the evicted range, verbatim, newest first within
   30% of the target. A plain user message is kept as is (images included);
   from a mixed tool_result + text message only the text blocks are carried
   (the tool_result cannot travel without its tool_use). Host bookkeeping
   (interruption markers, tool-in-flight notes, an earlier rollover note) is
   not carried.
3. **A host-written rollover note** (one user message, framed as host data,
   not an instruction): the window number, how much was moved out, the
   absolute paths of every earlier window's transcript, and a bounded index
   (about 2k tokens) of the evicted tool calls, newest first ("→ bash: cargo
   test …"). It tells the worker to grep/read the transcripts only for a
   specific missing detail and not to redo completed steps. Host metadata
   (`_synaps_worker_context`: schema, window, transcripts) rides on the
   message and is stripped from every wire like every `_synaps_*` key.
4. **The most recent rounds, verbatim**: a contiguous suffix starting at an
   assistant message, as long as fits the target. The last assistant message
   and everything after it are always kept.

**Tool pairing.** The cut is only ever placed at an assistant message that no
tool call/result group (`tool_groups`, shared with the foreground) straddles.
Everything after the cut is kept whole, so every `tool_use` keeps its
`tool_result` and no result is invented. The successor is checked before it
is used; a failed check means no rollover.

**Thinking blocks.** Same rule as the foreground path: assistant messages are
never edited, so signed `thinking`/`redacted_thinking` blocks travel
verbatim, and the last assistant message of a tool loop still starts with its
thinking block. A rollover in the middle of a tool loop sends: brief, carried
instructions, rollover note (user turns), then the assistant/tool_result
suffix ending on the pending tool results. The request builder merges
consecutive user turns, so on the wire the first message is one user turn
(the brief's text block first, verbatim, then the note), followed by the
retained assistant turn with its signed thinking block; the integration test
checks exactly this on the recorded request bodies.

**Target.** `successor_budget(target, pressure, overhead, calibration)`, with
the calibration sample recorded on the worker's own `ContinuationState`
exactly as the foreground does. The cut is chosen against an estimate, then
the real successor is measured and the cut stepped to the next valid
boundary while it is over target, so the target holds unless the must-keep
part alone exceeds it (then it is kept whole, with a warning, as the
foreground does). At hard capacity the target is also capped at two thirds
of the history room the next round leaves: when the reserves put hard
capacity below the rollover band, it comes first, and the configured target
can leave the successor little room to grow (a window that cannot hold the
target at all does not roll over at hard capacity; see "Context window").
A candidate that does not shrink by more than 1024 tokens, or would start
where the next round triggers another rollover (inside the pressure band
below hard capacity, at or past the hard-capacity line at it), is
`Unproductive` and deferred with `defer_unproductive_rollover`, the
foreground's no-thrash rule.

**Bounded note.** The note's index of evicted tool calls is capped at about
2k tokens (the foreground's dropped-index budget), and it lists the 12 most
recent earlier windows by path, naming older ones by directory, so the note
stays bounded however long a worker runs.

### Where evicted history goes

To the worker archive directory that `subagent_resume` already uses:
`<base>/subagent-history/<process>/<handle>.window-<N>.md` (directory 0700,
file 0600, atomic write, pruned with the directory after 14 days). The file is
a plain-text rendering of every evicted message (text, tool calls with their
JSON input, tool results in full; thinking omitted as private reasoning,
images named only), so the worker can grep it or read it by line range with
the tools it already has.

This is the same data class, location, permissions and retention as the
finalize archive, which already stores a worker's full history without any
consent step. Nothing goes to the memory backend or the Axel brain, and no
consent semantics change. That is why this did not hit the stop condition. A
transcript that cannot be written means no rollover: the worker continues as
it would today (and is told nothing).

### Resume and #315 cold-resume compaction

- The finalize archive stores the worker's *current* window (the
  `MessageHistory` the drive last saw), so it is at most about one window in
  size. A warm resume replays it: brief, carried instructions, the rollover
  note with transcript paths, recent rounds. Window numbering continues from
  the note's metadata, and later transcripts are named after the new handle.
- `full_history: true` replays the full *current window*, not evicted
  windows; those stay on disk, listed in the note.
- A cold resume (#315) of a rolled worker still compacts above
  `subagent_cold_resume_compact_tokens` (default 100k). It keeps the brief as
  "original task" and now adds an "Earlier context windows" section with the
  transcript paths, and carries the window metadata on its single message, so
  a resumed worker can still find, and keep numbering, earlier windows.
- The forced 5m TTL, keep-warm, and request-local worker image capping are
  untouched: rollover changes only the durable message list.

### Visibility

- **Telemetry (api-log).** Two optional fields: `rollover_window` (the
  context window number, present from window 2) and `rollover: true` on the
  first request of a new window (the request that writes the successor
  cache). Both are absent otherwise, so records without a rollover are
  byte-identical. The same fields are stamped on foreground records after a
  foreground rollover, so the meaning is uniform.
- **Worker stream.** A `Notice`: "Worker context rolled over to window N:
  ~X → ~Y tokens; earlier messages saved to <path>."
- **Subagent status / collect.** A `context` object
  (`{"window", "rollovers", "transcripts"}`) once the worker has rolled over;
  absent otherwise, so existing outputs are unchanged.
- **TUI subagent panel.** The worker's step line ("↻ context window N:
  ~X → ~Y tokens") through the existing `SubagentUpdate` path, which the
  panel already renders. No panel code changes.

## Out of scope

- Model-authored working notes for workers (no `context_checkpoint`).
- A worker-side search tool over transcripts (grep/read suffice).
- Changes to the foreground rollover itself; only the telemetry fields are shared.
- Redacting worker transcripts (the finalize archive isn't redacted either).
