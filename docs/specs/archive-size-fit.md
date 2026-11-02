# Archive capacity fit: rollover must never fail on a full history inventory

## Problem (evidence)

Session error during automatic context rollover:

```
archive commit failed; history retained: memory: Axel operation failed [size_limit]:
request or reply exceeds the byte limit ... [correlation=turn-3871045-6]
```

Root cause is **not** the wire frame limit (host and sidecar both allow 24 MiB
for `history_seal`). The sidecar's `history_bounds` (sidecars/axel-memory-service/
src/history.rs) rejected any insert once the scope held `MAX_HISTORY_RECORDS` (then
128) rows (tombstones included) or > 256 MiB, returning `Error::TooLarge` -> `size_limit`.
The SynapsCLI project scope (`p192650875bccb6ca`) held exactly 128 live history
rows, so every further rollover seal failed and the session stayed under pressure.

## Decision

Treat the history inventory as a **bounded ring**: a `history_seal` that would exceed
the per-scope record or byte cap evicts entries until the new seal fits, instead of
failing. Rollover therefore never fails on capacity; only genuine per-request limits
(single seal > 16 MiB, > 4096 messages, note > 8192) and a seal that alone exceeds the
scope byte cap still fail.

### Capacity

- `MAX_HISTORY_RECORDS = 256` rows per authorized scope group, tombstones included
  (raised from 128: a live store had 5 of 16 scopes at 128, one with 128 rows from
  126 logical sessions spanning 8 days, so the old order evicted almost nothing but
  a running worker's previous window).
- `MAX_HISTORY_SCOPE_BYTES = 256 MiB` of stored messages+note per scope (unchanged).

### Eviction order (sidecar, inside the seal transaction, before the insert)

While `count >= 256` or `bytes + incoming > 256 MiB`, delete ONE victim, chosen within
the authorized scope group by `(class, created_ms, id)` ascending:

1. `tombstoned`: tombstoned rows, oldest first. Their suppression is already durable
   in `synaps_fingerprints` (delete_history records digest+fingerprint before
   tombstoning), so physical deletion loses nothing. Tombstones are never protected.
2. `stale`: live rows with `created_ms < now - 72 h`, any kind, oldest first.
3. `superseded`: live rows that are not their logical session's newest, oldest first.
4. `newest`: the oldest remaining live row (each one is some logical's newest).
5. `floor`: only when every remaining row is floor-protected, the oldest of them.

**Floor** (applies to classes 2-4): a live row that is its logical session's FIRST
(earliest) or LATEST row while that logical is active (its latest row is less than
24 h old) is never chosen before class 5. The floor wins over stale: an active
session's first window keeps its lineage anchor even when it is days old. Once the
logical's newest row is 24 h old, its first/latest rows fall back to the normal
classes. Class 5 guarantees a seal never fails for capacity while the new seal fits
alone; the loop deletes exactly one row per pass and stops when the scope is empty,
so it always terminates. If the new seal alone exceeds the byte cap, the post-insert
`history_bounds` check still fails closed with `TooLarge` and the transaction
(including any evictions) rolls back.

Ages use one injectable clock reading per eviction pass (`Service.clock`; production
reads the wall clock, only in-crate unit tests can pin it). Windows rank live rows
only, by `(created_ms, id)`.

Evicted rows are physically deleted, **not** tombstoned and **not** fingerprinted:
eviction is capacity reclamation, not a forget; re-sealing identical evidence later
must remain allowed. Eviction is scoped to the authorized scope group only.

### Eviction log

After the seal commits, the sidecar writes one stderr line per evicted row:

```
axel_history_evict scope=<p + first 8 hex of the canonical scope key> class=<tombstoned|stale|superseded|newest|floor> age_h=<whole hours> id=<evicted row id>
```

Never content, notes, logical IDs, other sessions' IDs or paths. stdout remains the
protocol channel. Note: the host currently spawns the sidecar with stderr discarded
(`crates/agent-engine/src/memory_backend/process.rs`, `Stdio::null()`), so the line is
visible only when the sidecar runs under a stderr-capturing parent.

`migration_apply` imports keep the strict fail-closed cap (no eviction on import);
one apply may carry up to `MAX_HISTORY_RECORDS` (256) history rows, so a full scope's
own export remains importable into an empty scope.

### Host

- `crates/agent-engine/src/memory_backend/process.rs`: keep `size_limit` mapping; no
  host retry. The host already retains history on failure; after this change the
  failure cannot occur for capacity.
- Follow-up: the host migration export still bails above 128 history rows
  (`crates/agent-engine/src/memory_backend/migration.rs`, "export exceeds
  single-transaction record limits"); align it with 256.
- Local file `ArchiveStore` (crates/agent-core/src/core/context_archive.rs,
  `MAX_SEGMENTS = 128`, "archive segment budget exhausted") has the same failure mode
  for non-Axel users; follow-up: apply the same eviction there.

## Tests (sidecar)

Unit (`src/history.rs`, `eviction_tests`, pinned clock):
- class order tombstoned -> stale -> superseded -> newest -> floor;
- a stale row goes before a recent superseded row, but the floor beats stale;
- floor protects an active logical's first/latest rows and releases them after 24 h;
- the cap is 256 (seal 256 evicts nothing, seal 257 evicts one) and a fully
  floor-protected scope evicts exactly the oldest floor row per seal;
- the byte cap evicts floor rows until the seal fits, terminates when all are gone,
  and an oversize seal fails closed with no eviction;
- the log line carries scope prefix, class and whole hours, never content/note.

Integration (`tests/operations.rs`): active sessions' first and newest archives
survive a busy session filling the scope; tombstone then floor eviction with exact
stderr lines; forgotten evidence stays suppressed; a 256-row export imports into an
empty scope and fails `size_limit` into a non-empty one.

## Rollout

Rebuild + install the sidecar binary (`cargo build --release` in
`sidecars/axel-memory-service`, placed as `synaps-axel-memory-service` next to the
`synaps` executable or at the configured `memory.executable`), then rerun a rollover in
this project to confirm the seal succeeds.
