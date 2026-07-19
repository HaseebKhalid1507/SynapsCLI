# T35 decision — opt-in session journal + snapshots, no schema flip

Date: recorded before any T35 implementation, per the plan's ask-first gate
(spec §14 "Ask first"; plan Task 35).

## Approval scope

The operator has **not** approved a persisted-schema migration. The operator
instruction for this task authorizes **only**:

- an **opt-in**, backward-compatible journal + snapshot persistence path;
- **additive files only** — no change to the existing `<id>.json` schema;
- **no default flip** — legacy JSON load/write remains the default;
- **full rollback** — opting back out must restore pure-legacy state;
- **no irreversible or destructive migration** of any existing session.

Anything beyond this scope (default flip, `<id>.json` schema change,
destructive migration) remains gated on separate explicit sign-off.

## Decision

Add `session_persistence = json | journal` (default **`json`**) to the
config file. The default path is byte-for-byte the current behavior: every
`Session::save` rewrites `<id>.json` atomically. When — and only when — the
operator sets `journal`, saves use an append-only journal with periodic
atomic snapshots (spec §9.8), so steady-state save cost is proportional to
the **delta** since the last save, not to total history size.

## Format (journal mode)

Files live in the existing private `sessions/` dir (0700 dir, 0600 files,
symlink-refusing — T4 `private_fs` helpers, unchanged):

- `sessions/<id>.json` — the **snapshot**: a full `Session` serialization in
  the **unchanged legacy schema**. Always present; written atomically via
  `write_atomic_private`. Every existing reader (header listing, partial-ID
  match, chains, retention, old builds) keeps working against this file.
- `sessions/<id>.journal` — the **journal**: append-only JSONL of deltas
  since the snapshot, created/appended via `open_private_append` and synced
  per save. Record schema v1, one JSON object per line:

  | record | shape | meaning |
  | --- | --- | --- |
  | open | `{"v":1,"k":"open","base":N}` | first line; snapshot held `N` messages when this journal began |
  | msg  | `{"v":1,"k":"msg","i":I,"m":{…}}` | message at **absolute** history index `I` |
  | meta | `{"v":1,"k":"meta","meta":{…}}` | full session metadata (the `Session` object minus `api_messages`) |

### Load / recovery (always journal-aware, mode-independent)

`Session::load` loads the snapshot, then — if a journal exists — replays it
with **idempotent** rules, so stale or duplicated journals can never corrupt
a session:

- `msg` with `i == len(history)` → append; `i < len` → skip (already in the
  snapshot or already replayed); `i > len` → gap ⇒ stop replay at the last
  consistent prefix;
- `meta` applies only when `meta.updated_at >= session.updated_at` (a meta
  older than the snapshot can never regress state);
- a torn/unparseable line (kill during append) ends replay at the previous
  complete record — the session recovers to the last durable consistent
  state, exactly matching the T33 torn-tail discipline.

### Save (journal mode, stateless with respect to in-memory bookkeeping)

1. No `<id>.json` yet (new session) or no journal yet (first opt-in save of
   a legacy session) → write a full snapshot + a fresh journal containing
   only the `open` record. This is the *entire* "migration": one ordinary
   full save, identical in cost and content to a legacy save.
2. Otherwise read the journal (bounded — see threshold) to compute the
   durable message count, then append `msg` records for the new tail plus
   one small `meta` record in a single synced append.
3. Consistency tripwires force a fresh snapshot + journal reset instead of
   appending: in-memory history shorter than durable history (in-place
   compaction), or the last durable `msg` record no longer matching the
   in-memory message at that index (history edited). Journal mode assumes
   append-only histories between saves — true of every current call site;
   the tripwires catch the known rewrite paths.

### Periodic atomic snapshots

When the journal exceeds `max(256 KiB, snapshot_bytes / 4)` the save writes
a fresh full snapshot (atomic rename) and then atomically resets the journal
to a lone `open` record. A crash between the two leaves a stale journal
whose records all satisfy `i < len` — the idempotent replay rules make that
window self-healing. Amortized write amplification is bounded (~5× bytes
appended) instead of the legacy O(history) rewrite per save.

## Crash windows (all recover to a consistent session)

| window | on-disk state | recovery |
| --- | --- | --- |
| kill during snapshot write | old `<id>.json` + tmp file | rename never happened; old snapshot + journal still authoritative |
| kill during journal append | torn final line | replay stops at last complete record |
| kill between snapshot and journal reset | new snapshot + stale journal | replay skips every `i < len`, ignores older meta |
| kill during journal reset | old or new journal (atomic replace) | either is consistent |

## Rollback

Setting `session_persistence = json` (or removing the key) restores legacy
behavior on the **next save**: the save writes the full `<id>.json` exactly
as today and deletes the now-folded `<id>.journal`. Deleting `*.journal`
files by hand is also safe at any time — the snapshot alone is a valid,
consistent (possibly slightly older) legacy session. No state is ever only
in the journal for longer than the snapshot threshold.

## Reader / subsystem integration (additive, legacy-neutral)

- `latest_session` / `list_recent_sessions`: journal mtimes attribute to
  their session id, so "most recent" stays exact when snapshots lag.
- Session listing headers: when a journal exists, a bounded tail read of the
  last complete `meta` record refreshes `updated_at`/cost; legacy sessions
  read exactly as before.
- `delete_session_file` (T30 compaction rollback, retention) removes the
  sibling journal — a session and its journal live and die together.
- Retention (T34): journal bytes already count toward the disk budget
  (same directory); sweeps delete `<id>.json`+`<id>.journal` as one
  artifact, chain-head protection covers both (same file stem), and orphan
  journals (snapshot deleted out-of-band) are swept as strays.

## Trade-off record

| Dimension | Legacy JSON (default, unchanged) | Opt-in journal + snapshots |
| --- | --- | --- |
| Save cost | O(total history) serialize + rewrite + fsync per save | O(delta) append + fsync; amortized bounded snapshot rewrites |
| Save memory | full-history JSON string per save | delta-proportional buffer (receipt-verified in tests) |
| Crash safety | atomic rename | atomic rename (snapshot) + torn-tail-tolerant idempotent replay |
| Old sessions / old builds | unchanged | snapshot stays legacy-schema; old builds read the (≤ threshold stale) snapshot |
| Rollback | n/a | one save in json mode folds + deletes the journal |
| New dependency surface | none | none — std + serde + existing `private_fs` |

## Benchmarks (plan T35 acceptance)

`--ignored`-gated, resource-capped save benchmarks at 1 MiB / 10 MiB /
100 MiB histories print machine-readable `BENCH session_save …` lines
(legacy-vs-journal save time, bytes written per save, recovery/load time
after a simulated kill); a fast non-ignored proportionality test asserts the
journal append for one new message stays orders of magnitude below history
size. Results are recorded in the T35 benchmark commit message and the
invocation is documented in the consolidated benchmark script (T36).

## Fix iteration 1 addenda (final Judge I1 + M1)

- **Confined reads (I1):** every session read path — journal state on the
  save side, snapshot load, journal replay, and the listing meta tail —
  resolves relative to an `O_NOFOLLOW`-opened sessions-dir handle
  (`ConfinedDir`), verifies the opened handle is a regular file, and reads
  at most `MAX_PERSISTED_READ_BYTES` from that handle. A symlinked root,
  ancestor, or artifact — including one swapped in concurrently — fails
  closed with zero victim bytes read or echoed.
- **Version enforcement (M1):** `v == JOURNAL_SCHEMA_VERSION` is enforced
  on every record. An unsupported `open` version invalidates the whole
  journal (nothing replays; the next save resnapshots); an unknown-version
  record later in the file ends the valid prefix (replay stops before it;
  the next save resnapshots rather than appending behind records it cannot
  interpret).
