# Unified Axel memory backend

Status: implemented and verified with synthetic data. Selecting Axel routes
notes, eligible context-window history, consented capture/recall, explicit
history import, migration, and memory retention through **one host-owned service
and one shared user-owned `.r8` database with repository scopes**. It never falls back to a second writable
memory store. Existing legacy data has not been migrated or deleted automatically.

The shared-repository revision supersedes the per-checkout database binding below:
see [shared Axel repositories](shared-axel-repositories.md) for random durable Git
identity, worktree sharing, default paths, explicit user notes, `/2` protocol,
original-owner export batching, and the migration/upgrade boundary. Earlier test
counts in this document are historical; they are not verification of the revision.

## Build and enable

The service remains an independent Cargo workspace so Axel's SQLite/vector/ONNX
build dependencies do not enter the ordinary Synaps dependency graph. It pins
Axel to `edbdea401d66feedb87fcad28c879ece54e3ccd2`; the service constructs the
storage-only `Brain`, never an embedder/model. Linux with procfs is required.

```sh
cargo build --manifest-path sidecars/axel-memory-service/Cargo.toml --locked
cargo build --release --locked --bin synaps
```

Explicit host configuration, applied at runtime startup:

```text
memory.backend = axel
memory.axel.executable = /absolute/path/to/synaps-axel-memory-service
memory.axel.brain = /absolute/private/directory/project.r8
```

Precreate the brain's parent directory with mode 0700. Paths must be absolute and
symlink-free. The service validates private ownership/permissions and anchors
SQLite/WAL access to an opened directory descriptor. Missing/invalid settings,
protocol mismatches and storage errors fail explicitly. Backend/path changes on a
configured runtime require restart, not live swapping of existing providers.
Default `legacy` remains available for backward compatibility; it is not an Axel
failure fallback. No installed binary, plugin or global configuration was changed
by implementation/testing.

## One authority, short tools

- `memory_store/search/fetch/forget`: explicit project-scoped notes. IDs,
  provenance, literal Unicode case-insensitive substring and literal tag-prefix
  semantics remain stable. TTL is based on original creation time.
- `memory_search(source="history")` and `memory_fetch(ids=["ctx-…"], …)` read
  eligible source windows from the selected backend. Public fetch never returns
  the hidden working note. Model output stays bounded to about 24 KiB, with
  intra-message pagination for large evidence.
- `/context auto` works with Axel. Projection/redaction is pure host code;
  **no legacy archive directory is created in Axel mode**. The archive is
  committed before the durable active-head barrier permits another inference.
  Loaded markers are verified through the selected backend before inference.
- Foreground, worker and extension-provider tool loops share the immutable host
  binding. Independent extension/MCP memory tools and legacy reverse-memory RPC
  remain refused in Axel mode. The installed memory-manager plugin is not another
  authority, and no plugin process is needed for these operations.

## Continuous memory and explicit history import

Backend selection does not grant continuous-memory consent. Existing deterministic
host commands retain authority:

```text
/memory status
/memory capture
/memory recall
/memory on
/memory once
/memory off
```

`on` means capture plus recall; `off` revokes both. The model-facing control tool
can inspect/revoke Axel consent but cannot mint a durable or one-shot grant.
Workers inherit storage selection, not the foreground's memory-context leases.

The actual successful terminal stream publishes completed history once. Capture
uses that history—not the pre-inference prompt—and starts at the actual current
user-turn boundary. Error, empty-response, budget and cancelled paths do not
capture as successful turns. Private reasoning, restricted/never-persist content,
raw binary and raw tool output are excluded before normalization. Summaries with
restricted source evidence are skipped or stored under restrictive policy, not
mislabelled as normal. Structured eligible source evidence and its bounded
searchable note commit atomically. Native capture workers own their I/O runtime,
so queued work does not rely on a frontend timer runtime that may have stopped.

Recall searches notes and captured evidence through the same service. It is
bounded by the existing 150 ms hard deadline, validates scope/provider/lease,
filters automatic disclosure, and injects a separately delimited lower-authority
segment. Literal selection and recency are not semantic-quality claims. Disable
invalidates in-flight/retained recall; unavailable recall lets the turn proceed
**without memory**, never with another backend.

Past conversations remain an explicit separate consent operation:

```text
/memory index-history
/memory index-history confirm
```

The first is a metadata-only disclosure preview. Confirmation additionally needs
a live capture lease. Import screens eligible text, excludes foreign sessions,
and uses Axel capture receipts/tombstones for restart-safe deduplication. In Axel
mode no legacy progress file is an independent commit authority. No past private
conversation was imported during implementation.

## Migration and retention

See [operator migration commands](memory-migration.md). Supported explicit sources:

1. The current project's built-in JSONL notes plus eligible context archives.
2. An operator-selected legacy scoped Axel/plugin `.r8`, opened read-only without
   migration. The source must be quiescent and checkpointed; pending WAL/journal
   content causes refusal. Source-to-target project mapping is explicit, not a
   `proj_…`→`p…` prefix substitution.
3. A versioned private full export, including normal/restricted captures and
   deletion fingerprints.

Preview returns counts and digests, not bodies. Apply requires that exact freshly
verified digest and commits one bounded transaction. IDs, timestamps, absolute
expiry, restrictive disclosure, provenance, archive digests, capture receipts and
permanent tombstones/fingerprints are retained. Conflicts roll back all rows.
Bodyless inventories still require exact version/source/target identity. Source
files and configuration are untouched; rollback to the old source is possible,
but new target writes must be reconciled before switching back. Do not run legacy
writers during cutover; old versions do not participate in the new host's locks.

`retention inspect/export/sweep/forget` routes selected project memory to Axel.
Age/disk sweeping deletes notes/captured evidence with durable tombstones, never
silently evicts context archives/active heads. It reports `target_met` if protected
content prevents a disk target. Exact history forgetting is available explicitly.
Legacy sessions/traces/logs are separate persistence domains, labelled in CLI
output rather than misrepresented as part of the Axel memory sweep.

## Durability, deletion and bounds

Every write acknowledgement follows SQLite commit/checkpoint and filesystem sync.
An absent/error acknowledgement may be **commit unknown**; no automatic alternate
store or duplicate-ID retry is used. Store errors retain the generated exact ID
for reconciliation. Capture query distinguishes absent from committed/tombstoned;
malformed query replies never mean absent.

Forgetting removes live content and records permanent ID/source fingerprints.
Capture IDs, source digests, archive digests and exported suppression evidence
survive retry/migration. Imported fingerprints suppress matching existing target
copies as well as later insertions. This protects recorded lineage—not arbitrary
semantic paraphrases, independent copies without provenance, active prompts, or
OS backups. Rollover itself never calls forget. Logical deletion is not secure
media erasure and external effects are not exactly-once across process death.

- Ordinary RPC frames: 1 MiB; history/migration/operator exports: at most 24 MiB.
- Note body: 16 KiB; fetch: 25 IDs; search: 25 descriptors, 400-byte snippets.
- History: 128 segments including tombstones, 16 MiB/segment, 256 MiB/project,
  4,096 source messages, 8 KiB working note. No implicit source eviction.
- Atomic migration: 24 MiB, 16,384 note/tombstone entries, 128 histories. Larger
  inventories refuse without partial import; no unbounded or streaming claim.
- One operation per process; shared bindings serialize operations; a directory
  lock excludes competing service writers. Default operation deadline: 15 seconds.

## Verification

- Workspace: **3,939 passed, 29 ignored, 1 filtered**, 118 targets. Filter is the
  previously reproduced ambient Copilot-account catalog test.
- **8 explicitly run real-service integration tests passed**: short tools;
  runtime capture/recall/disable; production Runtime loopback final capture;
  rollover, durable-head resume and history retrieval; consented prior-history
  import; source migration/deletion-safe retry; full capture export/import.
- Standalone service: **22 tests passed**, including actual process protocol,
  transaction rollback, immutable source export, TTL/disclosure, tombstone and
  fingerprint union, large/empty histories and filesystem ancestor replacement.
- Host Clippy succeeds with existing unrelated warnings; service Clippy succeeds
  with `-D warnings`. Root dependency/lockfile graph is unchanged.

All tests used synthetic temporary data. No live filler prompts or model-quality
benchmarks were repeated. Logs: `/tmp/axel-unified-workspace.log`,
`/tmp/axel-unified-e2e.log`, `/tmp/axel-unified-service-tests.log`,
`/tmp/axel-unified-clippy.log` and `/tmp/axel-unified-service-clippy.log`.

Release CLI verification additionally passed in an isolated synthetic project:
preview without target creation, digest-gated apply, inspect, private export,
forget, deletion-safe migration retry, and sweep. Source bytes/config were unchanged.
Release build, schema drift, LOC/ignore ratchets and diff whitespace checks passed.
Workspace formatting still reports pre-existing differences in untouched files.
