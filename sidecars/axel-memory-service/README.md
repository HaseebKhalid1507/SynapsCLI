# Multi-project deployment update

This is a single user-owned shared brain service, **wire contract `/2` only**.
See [CONTRACT.md](CONTRACT.md#shared-brain-and-authorized-membership-2) for the
exact `scope_info`, operator `scope_upgrade`, verified `scope_alias`, reserved user
scope and scoped logical-byte retention contracts. New files are multi-project;
old `/1/project`-pinned files stay pinned until explicit operator upgrade. No
normal open upgrades an existing pin. Stored record envelopes remain `/1` to
preserve provenance. Alias membership never rewrites content or capture digests.
Repository proof requires a private **version 2** common-directory marker with
an independently random durable repository key, not a path-derived key. Version 1
markers are rejected; historical path keys are exposed only by explicit migration
and verified linking. Reusing a filesystem path does not inherit repository memory.

# Unified Axel memory service

The extended capture/history/migration/retention operations are specified in
[CONTRACT.md](CONTRACT.md), which supersedes the original notes-only limits below.
Exports always include `format:"synaps-axel-export/1"`, `source_project`, and
`target_project`, including empty/tombstone-only inventories. Service exports use
the envelope's **original project** for both keys, exporting only that owner's
rows, **not its alias group**. Native service `legacy_export` has the same boundary
and preserves that owner's capture/history/suppression inventory. Non-native
legacy exports retain the explicit source key and map records to namespace
`project-<target_project>`, preserving the original category in `meta._axel`.
Migration accepts these exact inventory fields and binds supplied identity
immutably to replay; other unknown typed fields are rejected.

For a complete group backup/restore, the host must quiesce writers, enumerate
`scope_info.members`, export each member in full, and import each inventory under
its unchanged original key with a distinct migration identity. Then explicitly
re-register verified aliases in the destination. Member exports are not an atomic
group snapshot, and inventories never authorize aliases. Do not flatten the
members into one canonical inventory: histories, tombstones and fingerprints
carry ownership implicitly. Metadata-only exports are not restorable snapshots.
Deletion fingerprints dominate stale imported histories without rejecting
unrelated valid rows; suppressed histories become body-free tombstones, and
seal retries cannot return their hidden notes.
Synthetic coverage includes multi-project/alias/upgrade tests plus existing privacy tests, including full
normal/restricted capture metadata round trips and tombstone-only source identity.

Independent Cargo workspace; no SQLite/ONNX dependencies are added to SynapsCLI.
Both `axel` and `axel-memkoshi` are pinned to
`https://github.com/maha-media/axel.git`, revision
`edbdea401d66feedb87fcad28c879ece54e3ccd2`. The checked-in lockfile uses these
Git sources, including transitive `velocirag`; there are no local path patches.

## Build and verify

From this directory (Linux with procfs, Rust 1.93 tested):

```sh
cargo build --locked
cargo test --locked --offline
cargo clippy --locked --offline --all-targets -- -D warnings
```

First build needs access to dependency registries/Git unless already cached.
Initial offline build was blocked by uncached crates; after the authorized
normal dependency download, the pinned build and synthetic offline tests pass.
`ort` is explicitly feature-unified with `load-dynamic`, which enables
`ort-sys/disable-linking`: **no ONNX runtime download or native ONNX library is
required to build or run this service**. The service only constructs
`axel::r8::Brain`, never `AxelBrain`, an embedder, a model, or a network client.
Tests launch the actual executable with an empty environment.

Binary for host adapter/E2E:

```text
sidecars/axel-memory-service/target/debug/synaps-axel-memory-service
```

Use `cargo build --release --locked` for a release binary under `target/release`.
No installed plugin, external checkout, global configuration, or host source
needs modification to build this workspace.

## Unified operations

The service owns notes, eligible context histories, idempotent terminal/summary
capture, capture receipts, migration and deletion fingerprints in the same `.r8`.
Operator-only legacy source export is read-only and never opens the target.
See [the final wire contract](CONTRACT.md) for history/capture/migration/retention
operations, exact schemas, large-frame bounds and privacy gates. All source
projections are host-screened; the service independently validates their grammar.

## Invocation and wire protocol

```sh
# Host provisions the dedicated private directory; service does not create parents.
install -d -m 700 /absolute/private/notes
./target/debug/synaps-axel-memory-service \
  --brain /absolute/private/notes/notes.r8 --project p0123456789abcdef
```

Optional `--operator` enables only explicit scope upgrade/alias operations.
Optional `--user-scope` is required only for reserved `p0000000000000000`.
That scope accepts only explicit note store/search/fetch/forget, scope_info and
capabilities. Direct capture/history, migration/export, retention and admin calls
are refused before opening storage, even with `--operator`. The key is
`p` followed by exactly 16 lowercase hex digits. The host owns this key; it is
not derived from the path. There is no environment/path fallback.

The host writes one JSON object per line and reads one reply per line. First:

```json
{"schema":"synaps-axel/2","project":"p0123456789abcdef","operation":"hello","payload":{}}
```

The immediate, flushed response has `ok:true` and:

```json
{"backend":"axel","revision":"edbdea401d66feedb87fcad28c879ece54e3ccd2","contract":"synaps-axel/2"}
```

**Hello does not inspect, open, or create the brain or its parent directory.**
It is a capability handshake, not a storage health check. On the same process,
send exactly one operation from [CONTRACT.md](CONTRACT.md). It exits after that
reply. EOF after hello is allowed; partial non-newline frames are rejected.
The host should enforce a process deadline for stalled stdin or filesystem IO.

Replies always carry `schema`, the immutable argument `project`, and either
`ok:true,result:...` or `ok:false,error:{code,message}`. Codes/messages are
static: errors never include request content, filesystem paths, SQL, or IDs.
`invalid_request`, `project_mismatch`, `protocol_error`, `size_limit`,
`not_found`, `id_conflict`, `storage_error`, `commit_unknown`, `unsafe_path`, `unsupported_brain`.
Post-commit checkpoint/fsync failures explicitly return `commit_unknown`; the
host must reconcile the requested record ID before any retry.
Invalid CLI arguments exit 2 with static usage text. Stdout is protocol only.
Ordinary frames including newline are capped at 1 MiB; bounded history/migration
and operator export frames use 24 MiB as specified in CONTRACT.md. Store success
size is preflighted before committing; oversized fetch/search replies fail as
a whole rather than truncating records. Fetch accepts at most 25 IDs.

### Payloads and semantics

* **Store:** full host `MemoryRecord` (`namespace`, `timestamp_ms`, `content`,
  `tags`, optional `meta`, `id`, `project`, `provenance:{source,session?}`,
  `sensitivity`, `retention`). Returns the full record; secret `content` is
  empty. IDs are opaque ASCII alphanumeric/`-`/`_`, 1–128 bytes. No ID rewriting,
  body padding, trimming, or artificial minimum length; empty/one-byte host
  notes are valid, maximum body 16 KiB. Namespace validation matches the host.
* **Search:** nullable `content_contains`, `tag_prefix`, `since_ms`, `until_ms`,
  `limit`, `snippet_bytes`. Literal Rust Unicode-lowercased content substring;
  **case-sensitive literal tag prefix**, matching host `store.rs`. No FTS
  operators, SQL wildcard expansion, normalization, or embeddings. Inclusive
  timestamp bounds, newest first (ID tie-break). Default limit 8/cap 25;
  default snippet 160/cap 400 UTF-8 bytes. Zero is respected. Descriptors have
  exactly `id,project,timestamp_ms,tags,snippet,truncated,content_bytes,
  sensitivity,retention`. Secret probes never match, including empty needles;
  recency/tag listings can return secret descriptors with empty snippets and
  `truncated:false`. Length, tags and other host metadata are not secret bodies.
* **Fetch:** `{ "ids": ["mem-..."] }` returns records in requested order,
  preserving duplicates. Missing/foreign/expired/withheld IDs fail the entire
  call with `not_found`; no partial disclosure.
* **Forget:** `{ "id": "mem-..." }` returns a boolean. Axel's transactional
  scoped API inserts a tombstone and removes record/FTS data. Tombstoned IDs
  cannot resurrect; duplicate live IDs also fail rather than overwrite.
* Retention is `"standard"` or `{ "max_age_days": N }` (`u32`). Expiry is
  absolute `timestamp_ms + N * 86400000`, not time-of-import/store. Expired
  records are not fetched/searched; forget can still tombstone them. This is
  visibility enforcement, not a physical retention sweep or secure erasure.

## Compatibility, privacy and persistence guarantees

One Axel `.r8` database only. Host compatibility metadata is a versioned JSON
**provenance envelope** in Axel's existing `memories.provenance`; it contains
the exact record metadata, an empty content placeholder, and original byte
length. Body lives only in Axel's normal content column. Host namespaces,
IDs, timestamps, tags, nested metadata, provenance, sensitivity and retention
round-trip. Axel scoped store/delete APIs perform the transactions and
permanent tombstone checks. Scoped SQL on `Brain.conn()` implements host
substring semantics, exact byte snippets, all-or-nothing fetch, and integer
TTL (avoiding upstream RFC3339 lexical fractional-second edge cases).

**Sensitive compatibility:** host `sensitive` is preserved in the envelope but
mapped conservatively to Axel `secret`, NOT `normal`. Both secret and sensitive
have empty Axel titles/abstracts: Axel FTS holds no indexed tokens for them.
Older scoped Axel readers therefore withhold their bodies. This service can
return/search sensitive bodies only after enforcing the host metadata and
underlying disclosure gates. Secret bodies are never returned, searched or
snippeted. Host `meta` (including title) stays metadata; callers must not put
secret bodies into ordinary tags/provenance/meta. Raw/local SQLite access is
not a model-visible boundary. Unknown envelopes/classes are rejected or
excluded; Axel non-standard disclosure retention is never returned or probed.
Do not use older writers against this dedicated compatibility brain: they do
not implement the host envelope contract.

New files get Axel's schema plus its project-memory schema initialization and
a scope marker in existing `brain_meta`. Existing files must already have the
supported multi-project marker or exact original pin and scoped schema, checked
read-only **before** `Brain::open`. Unmarked/legacy brains and foreign access to
a pinned brain are refused, never silently adopted or migrated.
An interrupted first initialization can leave an unusable file; fail closed,
do not automatically adopt it. Explicit operator migration is available through
`synaps retention migrate-memory`; it never adopts a legacy brain in place or
changes backend configuration. See [the migration guide](../../docs/specs/memory-migration.md).

The parent must already exist, be owned by the current UID, and have no
other-user permission bits. Parent traversal uses `openat(O_NOFOLLOW)`;
brain/WAL/SHM/journal symlinks, hardlinks, non-regular files, foreign ownership
and public permissions are refused. The executable sets umask 077 before any
SQLite call. Existing files are not chmod-repaired. A directory
flock serializes cooperating service processes with a bounded eight-second
pre-write wait (25 ms polling), not an immediate contention failure. No database
operation is retried. Timeout fails before opening/writing SQLite; cancellation
releases the CLOEXEC descriptor lock when the host terminates/reaps the process. All SQLite opens, preflight,
sidecar checks and fsync paths are anchored to the held directory descriptor
through `/proc/<pid>/fd/<fd>/<basename>`. The proc path must have procfs magic
and resolve to the held device/inode. A minimal process-local SQLite VFS wrapper
preserves this anchored full pathname (the stock VFS would resolve it back to
a mutable pathname). The actual SQLite/WAL ancestor-replacement regression
passes. Non-Linux builds fail closed. This still is not an isolation boundary
against the OS owner modifying files *inside* the private directory.

Axel's default `synchronous=NORMAL` is overridden to **FULL before service
schema/store/delete writes**. Success follows full WAL checkpoint, database
`sync_all`, and parent directory `sync_all`. Durability failure is reported,
not hidden (a failed response can still follow a committed operation; a retry
must reconcile by ID). Brain creation itself initially uses upstream NORMAL;
no success is acknowledged until the full durability barrier. Forget is
logical deletion, not guaranteed media erasure of old SQLite pages/backups.

## Synthetic coverage

35 executable-level tests plus 2 unit tests use only new temporary directories.
They cover two projects sharing one file, Git-worktree and moved-root aliases,
unknown/reused foreign roots, explicit pin upgrade, reserved user scope,
concurrent writers, cross-project ID/receipt/tombstone collisions, alias deletion
union, exact-original-scope export/import/re-alias round trips, native read-only
full export, v2 random identity/path-reuse rejection, stale history suppression
and retry safety, held-lock release/timeout/cancellation without external retries,
and the existing privacy cases: interactive
hello without creation; exact short/empty/sensitive round trips across
processes; secret body/title non-indexing; Unicode/wildcard literal filters;
UTF-8 snippet and result/body caps; absolute TTL; disclosure gates;
all-or-nothing fetch; tombstone permanence; wrong scope/protocol; input/reply
bounds; legacy-file byte preservation; private modes; main/ancillary/parent
symlink rejection. No actual user brain or private memory data is accessed.

### Explicit project agent forum

The service advertises `forum: {schema: 1}` through repository capabilities and
implements `forum_post`, `forum_read`, and `forum_forget`. It includes the shared
pure contract in `crates/agent-core/src/memory/forum.rs` by path (the sidecar must
be built from the source-tree layout). `uuid` v1 with `v4` is already a dependency.
No model/provider calls, parallel content store, automatic polling, or user-wide
forum are added. See `CONTRACT.md` for exact typed row and transport constraints.

Forum posts use the same Axel memories table, scoped retention/deletion/export
pipeline, private-path process lock and durable acknowledgement as notes, but
are isolated from ordinary store/search/fetch and automatic recall. Full backups
include them; descriptor-only export has cleared bodies and is not a forum
restore payload. Restore under original project identities before separately
reauthorizing worktree aliases. Peer content is lower-authority data, not trusted
instructions, and authorship must come from host execution bindings, not model
parameters.

Read pages are chronological, bounded to 16 entries, 16 KiB body and 24 KiB
serialized Page. `next` means more remain **now**. After draining, retain the
last emitted entry's `(timestamp_ms,id)` as the next poll's `after`; on an empty
poll retain the previous cursor. Cursors are not snapshots: backdated import or
deletion may require restarting a listing. Deleting/expiring a root retains its
live replies, but refuses new replies; committed duplicates still reconcile.

Synthetic coverage is in `tests/forum.rs`, plus the verified worktree alias and
user-scope coverage in `tests/multi_project.rs`. It covers cross-process durability
and concurrent siblings, ordinary-recall isolation, strict forged-envelope and
physical-row rejection, root/reply ownership, export/import replay, expiry and
sweep tombstones, timestamp non-refresh, escaped-JSON/UTF-8 budgets, tied cursor
ordering and polling after drain. These additions require foreground-coordinated
build/test verification; no build, tests, install, or provider call was run while
implementing this service change.
