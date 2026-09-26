# Archive capacity fit: rollover must never fail on a full history inventory

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

## Decision

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

## Tests (sidecar)

- Seal 129 distinct archives -> success, count stays 128, oldest live row gone, newest
  fetchable, forgotten fingerprints untouched.
- With tombstoned rows present, they are evicted before any live row.
- A single seal above 16 MiB still fails `TooLarge`.
- `migration_apply` with > 128 histories still fails.

## Rollout

Rebuild + install sidecar (`cargo install --path sidecars/axel-memory-service`), rerun
a rollover in this project to confirm the seal succeeds.
