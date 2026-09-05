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
3. If retained material cannot fit or meaningfully shrink, fail without clearing
   history. User constraints are not silently selected or summarized away.
4. Seal the old eligible source projection and working note to a private archive,
   with data/directory sync and retry deduplication. Verify the note before commit.
5. Publish replacement history and notify the frontend. A private message marker
   identifies the window/archive for resume; it is removed before provider wire
   serialization. The frontend's existing session save persists this history.
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

Cancellation before replacement preserves active history. An already-running
atomic archive write can finish after cancellation, leaving an unreferenced
eligible segment. Archive failures prevent replacement. This is not yet a
multi-file transaction covering the frontend session save and archive: after a
process crash, normal session/journal recovery may resume the older window.
Exactly-once external effects across a process crash are not promised.

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
tombstones, 16 MiB input/segment, 256 MiB per project, 4,096 source messages.
Storage exhaustion stops rollover; no automatic source eviction or tombstoning.
The index is currently a bounded scan, not a large-corpus search engine.
Non-Unix archive operations fail explicitly; enabling via the command is refused.

## Axel integration: NOT completed in this slice

This implements the automatic policy/rollover and a project history archive,
not the requested final single Axel memory authority. No existing memories were
migrated or tombstoned. No external repository or installed plugin was changed.

Source inspection found incompatible local revisions: the plugin's pinned Axel
revision predates newer retrieval imports, and the checked-out Axel main differs
from the branch with scoped memory/history support. Further incompatibilities:

- host project IDs use `p…`; plugin IDs use `proj_…`, with different path-byte
  normalization;
- built-in fetch takes `ids[]`; plugin fetch takes `id`;
- note search is substring versus plugin lexical FTS;
- sensitivity, retention, minimum content length and capture RPC shapes differ;
- exact-ID tombstones do not suppress re-capture under a newly derived ID.

The follow-up must align versions, define one host-owned memory backend
contract, preserve old ID aliases, map scope/retention without weakening them,
and migrate only with an explicit reversible plan. In Axel mode there must be
no silent fallback to a second writable note store. The current archive can be
a source artifact behind that future service; it must not be mistaken for a
completed Axel migration.

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

Remaining work includes durable thread-level source/derivation graphs, full
Axel migration, atomic archive+active-head recovery, actual-token/cost calibration,
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
