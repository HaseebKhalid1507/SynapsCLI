# Automatic context windows — experimental implementation

## Intent

Context pressure is a work-planning signal, not a cliff or a claim about model
intelligence at a particular token count. The operator's starting heuristics:

| Effective window | Begin preparation | Prefer rollover |
| --- | ---: | ---: |
| 200,000 | 140,000 | 180,000 or earlier hard reserve boundary |
| 1,000,000 | 250,000 | 400,000 |

Intermediate sizes interpolate; larger windows keep the 250k/400k bands.
These are tunable heuristics, not provider quality guarantees. Current usage is
the existing request-aware conservative estimate, not exact tokenizer counts
or a learned cost optimizer. Reserves include system/tools, output/thinking,
likely tool result growth, safety margin, and the pressure advisory.

At ~350k of a 1m window, finishing a spec is legitimate. Starting the plan's
large implementation belongs in a fresh window. `plan -> execute` or `new_task`
at/above preparation pressure requests rollover. Ongoing execution can continue
through preparation. At the rollover band, `plan`/`wrap_up` get at most two
additional provider rounds (configurable 0–8). Re-reporting phases, lower token
estimates, or mode toggles cannot replenish this allowance. Hard capacity wins.

Task phases are model reports, not an independent proof of task complexity.
`context_checkpoint` must be called alone; batched calls are rejected before
any tool in that batch executes. This does not authorize any filesystem,
network, deployment, or delegation action.

## Enablement

Default is off. On supported Unix hosts:

```text
/context auto
/context status
/context off
```

These commands change only the current runtime. For persistent opt-in:

```text
context_management.mode = auto
# Optional; omitted/auto thresholds use the table above:
context_management.pressure_tokens = auto
context_management.rollover_tokens = auto
context_management.reserve_tokens = 16000
context_management.finish_rounds = 2
```

User-facing pressure notices show only the estimated token count, not internal
phase names or task-planning instructions. `/context status` reports the
configured capacity, current window, and effective thresholds.

`memory_context` controls continuous-memory consent, not context-window usage.
For a consent status read, agents should send `{"action":"status"}`. Providers
that require every schema property can instead send:

```json
{"action":"status","mode":null,"capture_tools":null,"expires_minutes":null}
```

Only these three optional fields accept null as omission. Non-null `mode` and
`capture_tools` apply only to enable proposals; non-null `expires_minutes` applies
only to enable/recall_once. Other supplied values are rejected without changing
consent. Model calls still cannot enable durable capture or confirm history
import; Axel's control-only tool capability also cannot grant one-shot recall
(use `/memory once`).

For an isolated test profile, `/context auto 20000 120000` supplies lower
session-only thresholds. Do not use test thresholds as production defaults.
Invalid config disables the policy with a warning. Model tools cannot enable
it. Opt-in authorizes local archival of eligible content from the session;
this is independent of continuous-memory capture/recall consent.

`context_checkpoint(phase, note?)` is registered only in enabled streams. The
host assesses before provider rounds and rolls over automatically at a safe
tool boundary. `memory_search` and `memory_fetch` must remain executable and
present in the actual schema; disabled or revoked retrieval prevents rollover.

## What changes at rollover

The **same persisted session and live environment** continue. The active
provider context advances to another window. It does not restart the process,
reset spend or turn limits, migrate workers, acquire new permissions, or call a
summarizer. Headless chat's old automatic in-place compaction is suppressed in
this mode; manual `/compact` is unchanged. `run_single` rejects this mode rather
than silently behaving differently; the TUI/chat streaming engine supports it.

1. Defer while workers are running/unreconciled; at hard capacity stop without
   discarding history. Do not replay unknown tool effects.
2. Prepare a smaller context, preserving actual user-authored turns (including
   image-only turns), system/developer messages, the last assistant/result tail,
   and all tool-call/result dependencies of retained messages.
3. If retained material cannot meaningfully shrink at a soft boundary, keep the
   current history and continue only if the full request still passes hard
   admission. Reassess after four admitted rounds or an 8,192-token footprint
   change; repeated phase reports do not force another attempt. No archive,
   window advance or budget reset occurs for this no-op. If the current request
   cannot fit safely, stop without clearing history. User constraints are not
   silently selected or summarized away. See `context-rollover-recovery.md`.
4. Seal the old eligible source projection and working note to a private archive,
   with data/directory sync and retry deduplication. Verify the note before commit.
5. Request a frontend durable-head checkpoint. The frontend validates the logical
   session ID, preserves host metadata/accounting and saves a full snapshot in
   either persistence mode. Only a successful durable acknowledgement permits
   the runtime to publish the new window and dispatch another provider round.
   A private message marker identifies the window/archive for resume; it is
   removed before provider wire serialization.
6. Continue with the same turn meter, authority, tools and runtime state.

Archive retry deduplication is bound to the persisted session ID, while retrieval
remains project-scoped. Independent sessions therefore do not accidentally
share a retry/tombstone identity. Resume restores the saved window before the
first request; clear/session switches/manual compaction discard the previous
task note and phase without changing the host opt-in setting. Hidden checkpoint
notes and synthetic continuation envelopes are excluded from source indexing.

The source projection excludes system/developer prompts, private reasoning,
restricted/sensitive content and binary blocks. Ordinary text and eligible tool
evidence are screened/redacted, not summarized. User-authored images remain in
active context; the archive does not claim to preserve every original byte.
Redaction is defense-in-depth, not a universal secret detector.

Cancellation before head publication preserves active history. An already-running
atomic archive write can finish after cancellation, leaving an unreferenced
eligible segment. Archive failures prevent replacement. Once head publication is
requested, cancellation is not raced against its atomic save: the candidate may
already be on disk. A missing/error acknowledgement stops further inference and
blocks ordinary saves/automatic turns until explicit recovery; it never writes
old history back over an ambiguously committed head. TUI recovery re-reads after
any detached writer and durably republishes the loaded head before clearing
latches. Cross-session recovery refuses while deferred work remains, rather than
automatically delivering the previous session's work into another conversation.
Unsupported consumers fail
closed. Host reset epochs prevent late acknowledgements from mutating a reloaded
conversation, including a reload with the same logical session ID.

The archive is synced first; the session snapshot rename is the logical head
commit, followed by directory fsync before acknowledgement. This is an ordered
publication protocol, not a general multi-file transaction: a crash before head
commit can leave an orphan archive and the older session head. After successful
acknowledgement, the new head is durable subject to filesystem/device guarantees.
Exactly-once external effects across a process crash are not promised.

Snapshots now use storage-only `_journal_generation` metadata and matching v2
journals. Stale journals cannot replay into a shortened or same-length replacement
head. Legacy unmarked snapshots/v1 journals remain readable and migrate on save.
Older binaries can read the snapshot but cannot replay v2 deltas; fold the journal
through a JSON-mode save with this implementation before downgrading. Prefix
validation costs O(history) CPU while append writes remain delta-sized. Async
session saves are ordered in-process through blocking-worker completion, even if
the awaiting frontend is dropped; concurrent processes writing the same session
are not supported. Ordinary bound snapshot rotation syncs before discarding old
journal deltas. No existing sessions are proactively migrated.

Known responsiveness limit: TUI checkpoint persistence currently awaits in the
event handler; RPC holds its state lock while saving. A stalled filesystem can
therefore delay cancellation/UI handling. A timeout must never clear the durability
block or permit inference; moving save ownership out of those control paths is
follow-up work.

## Short retrieval tools

```text
memory_search(source="history", query="failed approach", limit=8)
memory_fetch(ids=["ctx-<exact-returned-id>"], start=0, limit=8)
```

`source=notes` (default) retains the existing note-store semantics. History
results contain opaque IDs and bounded snippets. Fetch returns source indices;
large messages support `offset_bytes` within the message at `start`, with a
continuation hint. Output is bounded to about 24 KiB. Existing note IDs remain
unchanged. Fetch/search failures are not replaced with made-up history.

`memory_forget(id="ctx-...")` explicitly deletes that archive projection and
retains a tombstone. Normal rollover never invokes forget. This does NOT erase
other sessions, active context, derived notes, OS backups, or copies under other
IDs. Retry of the same sealed projection is denied after forgetting; a different
projection may still contain duplicated material. This is exact-projection
suppression, not content-wide erasure.

Archives live under the private `context-archives/<scope-hash>/` directory,
separate from ordinary session-retention cleanup. Limits: 128 slots including
tombstones, 16 MiB eligible input/serialized segment, 256 MiB per project,
4,096 source messages, 65,536 visited input nodes and depth 32. Excluded message
and media payloads and ignored metadata do not consume eligible byte budget;
their opaque descendants are not traversed. Admitted tool arguments/results
still count, and genuinely oversized eligible evidence fails without truncation.
See [eligible-input budgeting](archive-eligible-input-budget.md).
Storage exhaustion stops rollover; no automatic source eviction or tombstoning.
The index is currently a bounded scan, not a large-corpus search engine.
Non-Unix archive operations fail explicitly; enabling via the command is refused.

## Axel integration: one selected memory authority

**Shared-repository update:** the host now uses one shared brain with durable Git
repository scopes. Worktrees share repository memory; migration of existing path
scopes is explicit. Context-head durability and capture consent remain independent.
See [shared Axel repositories](shared-axel-repositories.md).


The unified host service now covers explicit notes, eligible source archives,
consented capture/recall, prior-session import, atomic legacy migration and memory
retention. See [unified Axel backend](axel-host-backend.md). In Axel mode rollover
projects source in memory and persists it directly in the same `.r8` database;
short history tools never fall back to the legacy archive. The durable frontend
head acknowledgement still gates further inference. Independent plugin/MCP memory
routes are disabled, rather than remain a second writable memory authority.

Migration is implemented but never automatic: metadata preview, exact digest and
explicit source/target scope precede one bounded transaction. Existing source
notes/archives and the installed plugin are untouched until the operator elects
cutover. Absolute expiry, restrictive policy and deletion evidence are preserved.
No real existing memories were migrated by implementation/testing.

## Evidence and remaining limits

Tests cover anchors, Plan->Execute at 350k, bounded extensions/hard limits,
config defaults/validation, private atomic archives, redaction, retry/tombstones,
large-message pagination, mixed tool pairs and images, cancellation, and an
actual loopback stream that rolls over between tool rounds. The off path
retains existing provider request behavior.

Live test in tmux `8:0.1`, isolated synthetic profile, extensions disabled:
Astra called plan then execute; the UI announced context window 2; history
search/fetch recovered `ROLLOVER_EVIDENCE_COBALT` and checksum `blue-27`.
Lower thresholds exercised the real control flow without sending a 350k-token
live request. No cost/quality improvement claim is inferred from that smoke.

Remaining work includes richer thread-level derivation graphs,
multi-process active-head coordination, actual-token/cost calibration,
large-corpus indexing, and portable archive support. The prototype intentionally
retains every real user turn; instruction-heavy histories may stop instead of
shrinking. Keep it opt-in until these limits have been reviewed.

### Validation record

- `cargo test --workspace --no-fail-fast -- --test-threads=1 --skip ui_catalog_fetch_github_copilot_returns_prefixed_chat_models`: **3,866 passed, 21 ignored, 1 filtered**, 118 targets. The filtered test is the previously reproduced ambient Copilot-account catalog test.
- `cargo clippy --workspace --all-targets`: completed with existing warnings; no new warning in the added modules.
- `cargo build --release --locked --offline --bin synaps`: succeeded.
- Release binary was started in test pane `8:0.1` with the isolated synthetic profile; after restart, context status restored window 2 and a new history search recovered the same source ID. No global config or installed binary was replaced.

**Reserve caveat:** the table is a soft policy, not a promise of dispatch at
those counts. Existing `max_tokens_for_model` reserves up to 64k output (128k
for Opus), in addition to other reserves; on a 200k configuration this can
force rollover substantially before 140k. Calibrating that reserve requires
matching actual provider output limits, not silently lowering the hard-budget
check to satisfy a preferred threshold. The 350k/1m policy scenario is tested
without changing those safeguards.


### Post-review hardening

- Shared boot, `/resume`, clear/new session and manual compaction reset
  continuation bookkeeping at the appropriate lifecycle boundary. Status is
  correct before the first model request, without displaying internal phases.
- Progressive-disclosure catalog rebuilds preserve the opted-in retrieval
  surface; removed/disabled tools are not restored.
- Archive sealing uses the persisted session ID for deduplication, while
  read/search stay project-scoped. Forgetting one session's identical source
  projection does not tombstone an independent session's record.
- Neither synthetic continuation envelopes nor `context_checkpoint` note
  arguments are indexed as original source evidence on later rollovers.
- History-tool scope and base-directory bindings are captured before moving
  work onto a blocking thread.
- Final release pane check (before inference): `window 2 | capacity 1000000 |
  pressure 250000 | rollover 400000`; the earlier synthetic test overrides
  have been removed from the test profile. Installed/global Synaps is unchanged.

### Resumed durability follow-up

The resume experiment located `CONTEXT-ROLLOVER-RESUME-C12D603E`, fetched its exact
returned checkpoint ID, and verified clean branch `feat/context-continuation` at
`c12d603e`. The existing test pane still showed window 2 with 1m capacity and
250k/400k thresholds; it was inspected without sending a prompt or restarting it.

The follow-up adds the archive-before-durable-head barrier described above,
generation-safe journal recovery, frontend save/auto-turn failure latches and
runtime reset-epoch checks. Synthetic tests cover acknowledgement ordering,
rejected/unhandled checkpoints, blocked retries, stale journal crash states,
post-publication errors, session identity and detached-writer ordering. The real
loopback stream test verifies no second provider request before successful head
persistence and no second request at all after a failed save. This is not a new
large live-context or cost/quality benchmark.

The subsequent full backend integration resolves the independent notes/history/
capture routing gaps; see [unified Axel backend](axel-host-backend.md). The earlier
[source audit](../reviews/context-resume-axel-boundary.md) is historical, not the
current implementation status. No global
configuration, installed binary, plugin, external checkout or stored memories
were migrated by this follow-up.

Follow-up validation (synthetic/offline except the previously established ambient
catalog exclusion):

- Workspace: **3,898 passed, 21 ignored, 1 filtered**, 118 targets, using
  `cargo test --workspace --offline --no-fail-fast -- --test-threads=1 --skip ui_catalog_fetch_github_copilot_returns_prefixed_chat_models`.
- `cargo clippy --workspace --all-targets --offline`: succeeded with existing
  warnings, no warning in the new durability modules.
- `cargo build --release --locked --offline --bin synaps`: succeeded.
- Tool-schema drift, LOC ratchet, ignore ratchet, and `git diff --check`: passed.
- Workspace formatting check still reports pre-existing differences in untouched
  files; changed Rust files were formatted without unrelated churn.
- Logs: `/tmp/context-resume-workspace.log`, `/tmp/context-resume-clippy.log`,
  `/tmp/context-resume-release.log` and their `.exit` files. The release artifact
  was rebuilt locally; the running test pane and installed binary were not replaced.


### Wall-clock successor semantics (2026-09-06)

A successfully archived, durably acknowledged successor now starts a fresh
wall-clock segment; it no longer carries the original window's elapsed time.
This resets elapsed time only, not permissions, cumulative tool/cost/usage limits,
worker dispatch limits or autonomous grant deadlines. `/context auto` also makes
elapsed-time exhaustion request a successor even below context pressure. A
low-pressure time successor need not shrink the history, but must fit the request
budget and pass the same archive, tool-admission, worker and durable-head barriers.
Zero allowance and inference-free successor loops stop. Cancellation remains
checked before the next request; failed saves never renew time. A normal completed
answer still ends the stream; this is not an implicit autonomous plugin loop.
