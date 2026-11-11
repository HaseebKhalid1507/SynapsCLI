# Archive capacity fit: rollover must never fail on a full history inventory

> **Status (2026-11-10): the bounded-ring decision is SUPERSEDED** by
> [Current rule](#current-rule-keep-every-chats-archives). Seals no longer evict.

## Problem (evidence)

Session error during automatic context rollover:

```
archive commit failed; history retained: memory: Axel operation failed [size_limit]:
request or reply exceeds the byte limit ... [correlation=turn-3871045-6]
```

Root cause is **not** the wire frame limit (host and sidecar both allow 24 MiB
for `history_seal`). The sidecar's `history_bounds` (sidecars/axel-memory-service/
src/history.rs) rejects any insert once the scope holds `MAX_HISTORY_RECORDS = 128`
rows (tombstones included) or > 256 MiB, returning `Error::TooLarge` -> `size_limit`.
The SynapsCLI project scope (`p192650875bccb6ca`) holds exactly 128 live history
rows, so every further rollover seal fails and the session stays under pressure.
CONTRACT.md line 84 currently states "No rollover/pruning/automatic archive deletion".

## Current rule: keep every chat's archives

**Why the ring was wrong.** Rollover seals the old window into Axel; the session
keeps only `_synaps_context.archive = <id>`, a note and its recent messages, and
`validate_restored_history` (crates/agent-engine/src/runtime/continuation.rs)
refuses to resume when that archive is missing. The archive is the only copy of
the older conversation. All daemon sessions share one scope (daemon cwd = home),
so busy chats evicted other saved chats' archives: on one machine 8 of 24 saved
sessions pointed at archives that no longer existed, with the home scope pinned
at 128 rows.

**Rule.**

1. `history_seal` never deletes or evicts a history row, live or tombstoned.
   Tombstones are body-free (`messages='[]'`, `note=''`) and are kept, so export
   inventories and suppression are unchanged.
2. There is no per-scope history count or byte cap. Nothing refuses a seal, an
   import or a `scope_alias` union because the target already holds many rows.
   Rollover never fails on capacity.
3. Per-request limits still fail `size_limit`: one seal > 16 MiB messages+note,
   > 4096 source messages/rows, note > 8192 bytes.
4. `migration_apply` bounds the incoming batch, not the target: <= 128 histories
   (`MAX_HISTORY_IMPORT`) inside the 24 MiB request frame, in one atomic
   transaction.
5. Search/fetch output stays bounded (search cap 32 descriptors with 256-byte
   snippets; fetch cap 128 rows within the 24 MiB reply). `history_search` scans
   every live row in the scope (`instr(synaps_lower(messages), q)` ordered by
   `created_ms`), so its latency grows with total archived bytes.
6. Removing the archives of chats that no longer exist is a future, explicit
   operator action (`history_forget` per ID exists today). Nothing does it
   implicitly.

The legacy file `ArchiveStore` (crates/agent-core/src/core/context_archive.rs,
128 slots including tombstones, 256 MiB) is unchanged: at capacity it fails the
seal closed ("archive segment budget exhausted" / "archive storage byte budget
exhausted"), never deletes a segment, and the host keeps the source history. The
ring's follow-up to add eviction there is cancelled.

Tests (sidecar): 200+ seals in one scope are all live and fetchable, the oldest
included; tombstones beside > 128 live rows never cost a live row; a > 16 MiB seal
still fails in a large scope; an in-bound import into a > 128-row scope succeeds
while a 129-history or > 24 MiB batch fails atomically; an alias union past 128
rows succeeds.

## Superseded decision: bounded ring (2026-09-10)

Kept for history. Do not implement: it deleted saved chats' only archives.

Treat the history inventory as a **bounded ring**: a `history_seal` that would exceed
the per-scope record or byte cap evicts the oldest entries until the new seal fits,
instead of failing. Rollover therefore never fails on capacity; only genuine
per-request limits (single seal > 16 MiB, > 4096 messages, note > 8192) still fail.

### Eviction order (sidecar, inside the seal transaction, after insert)

1. Tombstoned rows (`tombstoned=1`) oldest first. Their suppression is already
   durable in `synaps_fingerprints` (delete_history records digest+fingerprint
   before tombstoning), so physical deletion loses nothing.
2. Live rows oldest first by `(created_ms ASC, id ASC)`, never the row just inserted.
3. Repeat until `count <= 128` and `bytes <= 256 MiB`. If still over (the new row alone
   exceeds the byte cap) fail with `TooLarge` as today.

Evicted live rows are physically deleted, **not** tombstoned and **not** fingerprinted:
eviction is capacity reclamation, not a forget; re-sealing identical evidence later
must remain allowed. Eviction is scoped to the authorized scope group only.

`migration_apply` imports keep the strict fail-closed cap (no eviction on import).

### Host

- `crates/agent-engine/src/memory_backend/process.rs`: keep `size_limit` mapping; no
  host retry. The host already retains history on failure; after this change the
  failure cannot occur for capacity.
- Local file `ArchiveStore` (crates/agent-core/src/core/context_archive.rs,
  `MAX_SEGMENTS = 128`, "archive segment budget exhausted") has the same failure mode
  for non-Axel users; follow-up: apply the same oldest-live-first eviction there.

### Contract/docs

Update `sidecars/axel-memory-service/CONTRACT.md` line 84: "128 history IDs incl
tombstones; seal evicts oldest tombstoned then oldest live rows to fit; imports do not
evict." Bump nothing else (payload/reply shapes unchanged).

### Tests (sidecar)

- Seal 129 distinct archives -> success, count stays 128, oldest live row gone, newest
  fetchable, forgotten fingerprints untouched.
- With tombstoned rows present, they are evicted before any live row.
- A single seal above 16 MiB still fails `TooLarge`.
- `migration_apply` with > 128 histories still fails.

### Rollout

Rebuild + install sidecar (`cargo install --path sidecars/axel-memory-service`), rerun
a rollover in this project to confirm the seal succeeds.
