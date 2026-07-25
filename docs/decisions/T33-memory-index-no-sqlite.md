# T33 decision — memory retrieval index without SQLite

Date: recorded before any T33 implementation, per the plan's ask-first gate
(spec §2.10, §14 "Ask first"; plan Task 33).

## Decision

The operator has **declined the SQLite/FTS5 dependency at this time**. Task
33 therefore implements the plan's documented fallback: an **in-repo staged
lexical index** over the append-only memory store, meeting the same bounds.

## Trade-off record

| Dimension | SQLite FTS5 (declined) | In-repo staged index (chosen) |
| --- | --- | --- |
| New dependency surface | `rusqlite`/`libsqlite3` (C code, build+audit surface, dependency-review stall risk R9) | none — std + serde + existing private-fs helpers |
| Query power | full FTS (phrase, rank, prefix) | token-exact AND matching + tag/time predicates |
| Query time | sublinear via B-tree/FTS | timestamp-ordered segment merge with early termination; worst case linear in indexed docs (content-free lines, no body parsing) |
| Result memory | proportional to limit | proportional to limit (bounded top-k over streamed segment lines; proven by `max_resident_hits` stats and benchmarks) |
| Crash safety | WAL | immutable segments + atomically renamed manifest; torn tails ignored; invalid/mismatched manifest triggers full derived rebuild |
| Migration risk | schema + file-format commitment | index is fully **derived** from the JSONL store and can be deleted/rebuilt at any time — no migration surface |

Revisiting SQLite later is cheap: the index directory is derived state; a
future FTS backend can replace it without any store migration.

## Bounds the fallback must (and does) meet

- text/tag/project/timestamp indexing: per-doc deduped lexical terms, tags,
  per-project segment directories, ts-desc segment ordering with per-segment
  min/max timestamp skipping;
- bounded pagination: hard result cap + `(timestamp, id)` cursor;
- result-proportional memory: streamed segment lines, one buffered doc per
  open segment, top-k bounded by the requested limit;
- crash-safe append/update: segments and manifest written via
  `write_atomic_private` (tmp + rename); kill-during-append leaves either the
  old manifest (tail re-indexed) or the completed segment — never a torn
  index that parses; unparseable manifests trigger a full rebuild from the
  store;
- no network: the index module performs filesystem I/O only; no HTTP client
  is constructed anywhere in the memory path (`agent-core`'s reqwest dep is
  auth/broker-only and is not imported by `memory::*`);
- embeddings: **disabled** — no embedding hook exists in this module, and no
  implicit remote embedding call can therefore occur. Local embeddings, if
  ever added, are an explicit opt-in feature gated by a separate ask-first
  review (reference backend: Axel, per spec §9.6).

## Benchmarks

1K/10K/100K-record benchmarks (`--ignored`-gated, resource-capped) print
machine-readable build/query timings and the resident-hit bound; results are
recorded in the T33 benchmark commit message. A 1M-record run is documented
as out of local runtime budget if it exceeds the cap.
