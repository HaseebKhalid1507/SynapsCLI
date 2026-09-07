# Final backend wire contract

All envelopes require `synaps-axel/2`, the immutable argument project, the exact
Axel pin, and hello's existing three fields. Exactly hello + one operation per
process. `capabilities {}` is a separate, storage-free operation returning
`{operations:[String],max_frame_bytes,max_large_frame_bytes,history_logical_id,
history_fetch,operator_operations,scope_model:"immutable_membership/1",
user_project:"p0000000000000000",record_schema:"synaps-axel/1"}`. No begin/stage/commit operations: migration is SINGLE APPLY.

## Shared brain and authorized membership (`/2`)

`/1` hello is rejected before opening storage, and all replies use `/2`. Stored
record envelopes continue to use `synaps-axel/1`: transport versioning does not
rewrite provenance. New brains initialize `brain_meta['synaps-axel/2/multi-project']='1'`
and support independent scopes in one file. A legacy
`brain_meta['synaps-axel/1/project']` pin still permits **only that original scope**.
Normal open never widens/adopts an old pinned or unmarked brain.

* `scope_info {}` -> `{canonical_project:String,members:[String],mode:"pinned"|"multi_project",user_scope:bool}`.
  Members are sorted original project keys. Read separately and cache in the host;
  this operation is the authority for validating original-project record outputs.
  Aliases only grow a group; they cannot be detached, reparented, or used to join
  already-owned groups. Old scope and canonical operations see the same group.
* `scope_upgrade {expected_project:String}` requires process flag **`--operator`**.
  Existing file only, exact original pin required; atomically removes the `/1`
  pin and installs the multi-project marker plus immutable upgraded-from receipt.
  Identical retries succeed; a different pin/fresh shared brain conflicts.
  Host operator preview/confirmation must bind its source digest before calling;
  the service verifies the expected pin, not a caller-supplied file digest.
* `scope_alias {alias_project:String,alias_root:absolute_path,canonical_root:absolute_path}`
  also requires **`--operator`**. Envelope project must be the group's canonical
  key. The alias must already be a known singleton in this same database (import
  under its original scope first); unknown alias fails. Exact authorized retry
  returns scope_info. Alias mutation and group suppression are one transaction.
  Verification reads the host-created Git common directory's private
  `synaps-memory-identity.json` (`{version:2,key,initial_root,roots}`). The host
  generates a random durable 64-bit repository key (excluding the reserved
  all-zero key), NOT a hash of its filesystem path. The marker key
  must be a valid project key equal to the envelope canonical key; initial_root
  must occur in the bounded absolute roots list. Version 1 markers are rejected
  (unreleased format, no automatic transition). Only the historical alias key
  is checked against SHA256(alias_root)'s first eight bytes, and that exact root
  must be present in the marker. Path reuse never implies canonical ownership.
  Current root/common-dir and worktree backlinks are checked using read-only Git
  plumbing with sanitized environment and no user/system Git config. Historical
  roots may be absent after moves; an existing reused foreign path fails closed.
  The service never creates identity markers. Non-Git guesses are not proofs.
  Markers must be user-owned, regular, single-link, exactly 0600 files; user-owned
  0775 repository/common-directory ancestors are allowed. World-writable paths
  fail except root-owned sticky ancestors. Brain directories remain private 0700.
* `p0000000000000000` is reserved for explicit user notes. It requires process
  flag **`--user-scope`**, which is rejected for any other key. Repository key
  derivation must reject this reserved value rather than accidentally selecting
  it. User scope cannot participate in repository aliases. Its direct protocol
  allowlist is exactly `store`, `search`, `fetch`, `forget`, `scope_info`, and
  `capabilities` after hello, matching the host. All other operations (including
  capture/history, migration/export, retention and upgrade) are rejected before
  storage opens, even with `--operator`. Its capabilities list only the note and
  scope operations, with no operator operations. No automatic global capture.

Original records retain project/namespace/provenance; capture evidence and its
payload digest, history projection/logical IDs/digests, IDs and migration receipts
are untouched by alias registration. Existing deletion fingerprints become
member-group-wide atomically. New ordinary store records still must name the
exact envelope project; replay/migration/capture retries may retain an authorized
member project's identity. Unrelated scopes are never accepted for these inputs.

Axel's globally unique IDs remain globally unique: cross-project collisions fail
closed, including tombstones and migration receipt IDs. No ID rewriting and no
silent foreign tombstone/replay suppression. Read/forget/search/sweep queries
are group constrained; export is the stricter exact-original-owner exception
below. Global ID lookups only detect conflicts.

Admin operations are absent from normal operations and rejected without the
launch flag. Hosts must never expose admin/full export/migration as model tools.
The same-UID operator is trusted; this protocol is not a sandbox against that user.

## History

* `history_seal {logical_id:64hex,source_message_count:usize,messages:[{source_index,block_indices,message}],note:String,digest:64hex}` -> `{id:32hex,message_count,source_message_count,note}`. `logical_id` is ALREADY the host's length-prefixed SHA256 logical-session hash. Digest is SHA256 over length-prefixed parts `[logical_id.as_bytes(), source_count_u64_be, serde_json::to_vec(typed ArchivedMessage array)]`; each prefix is u64 BE. Typed row serialization order: source_index,block_indices,message; nested Value maps sorted. Service recomputes exactly. Empty eligible projection/source count0 accepted. Random UUIDv4 IDs, retry reuses original ID/note. Same projection under a forgotten digest or content fingerprint fails permanently, including existing-row seal retries.
* `history_search {query:String,limit?:usize}` -> `[{id,message_count,source_message_count,snippet}]`. Literal lowercase substring over eligible messages, never note. Limit default8/cap32; snippet <=256 UTF8 bytes.
* `history_fetch {id,start?:usize,limit?:usize,offset_bytes?:usize}` -> **Vec ArchivedMessage** JSON array, never an object and never note. start is archived-row index/default0; limit default32/cap128. offset_bytes must0; host handles intra-message paging; nonzero returns invalid_request.
* `history_note {id}` -> String. `history_forget {id}` -> bool.
* 128 history IDs incl tombstones; <=256MiB serialized history/authorized group; <=16MiB messages+note/segment; <=4096 source messages; note<=8192 bytes. No rollover/pruning/automatic archive deletion. Content fingerprints exclude positional/logical fields, preventing recapture under another logical session. Nested private reasoning/disclosure metadata and nonprojected block grammar are rejected defensively; host remains responsible for text screening.

## Capture

* `capture` payload is EXACT existing `chat_turn_capture/1` or `conversation_summary/1` host wire object. Authorized member project_id required; capture_id/source digests lowercase64hex. Summary source digest is source_turn_range.digest. -> `{capture_id,committed:true}`.
* `capture_query {capture_id}` -> `{capture_id,committed:bool,tombstoned:bool}`. Committed remains true after deletion, including imported nonexistent `mem-cap-<id>` tombstones.
* Full canonical structured evidence<=128KiB plus derived note<=16KiB are committed atomically in the same database. Note ID `mem-cap-<capture_id>`, namespace `captures`, source `capture`; meta includes capture_id/source_digest/capture_schema/local_only. Normal note search/fetch recall uses this note, not raw evidence. Local-only/secret/restricted summaries are excluded from normal search/fetch. Declared reasoning/never-persist summaries rejected. Identical retries idempotent; changed same-key payload conflicts even after deletion. Forget clears evidence and note but preserves payload/source digest and key suppression. Imported fingerprints suppress existing and future source copies.

## Atomic migration and local-operator export

* `migration_apply {migration_id:64hex,manifest_digest:64hex,records?:[MemoryRecord],tombstones?:[String],histories?:[HistoryImport],captures?:[CaptureExport],fingerprints?:[Fingerprint]}` -> `{records,tombstones,histories}` newly inserted/deleted import counts. Empty arrays default. <=16384 combined notes/tombstones, <=128 history rows, <=16384 captures, <=32768 fingerprints. One transaction includes all tables; conflicting rows roll back everything. Manifest digest is an immutable opaque host manifest binding; service additionally hashes canonical entire payload. Exact retry returns original counts, changed same migration key conflicts. Existing tombstones dominate live note imports; independent identical bodies without matching source lineage remain valid.
* Migration also accepts the exact inventory fields `format`, `source_project`, `target_project`, and optional legacy `source_digest`. Existing host-built payloads may omit the entire identity group; when supplied, all three version/scope fields are required, format must be `synaps-axel-export/1`, source must be nonempty (<=4096 bytes), target must be an authorized member of the immutable host scope, and source digest must be lowercase64hex. Identity participates in the immutable apply digest, including tombstone-only migrations. Unknown top-level/typed fields remain rejected; arbitrary `MemoryRecord.meta` JSON is preserved. The host CLI must require versioned identity on external inventories.
* HistoryImport: `{id:32hex,logical_id:64hex,digest:64hex,source_message_count?:usize,messages?:[ArchivedMessage],note?:String,tombstone?:bool}`. Defaults count0/messages[]/note""/tombstonefalse. Host converts legacy archive records into this shape; original IDs/digests preserved. Tombstones cannot carry bodies. All live digest/grammar validations apply. Imported deletion fingerprints dominate stale live rows: restore them as body-free tombstones retaining their IDs/logical IDs/digests, rather than failing the whole inventory or resurrecting evidence. Foreign-owner or changed-identity collisions still fail the transaction.
* CaptureExport: `{capture_id,note_id,payload_digest,source_digest,evidence:JSON|null,withheld:bool,tombstoned:bool}`. Full export evidence is original payload; tombstone evidence null. Apply validates hash/project/key/source/policy and requires a matching live note unless tombstoned; restores capture_query state. Evidence is not placed in model-visible note metadata.
* Fingerprint: `{kind:"note"|"capture"|"history",digest:64hex}`. Export always includes inventory. Apply imports it FIRST and deletes matching already-present destination notes/captures/histories in the same transaction; suppression is also checked at read/insert boundaries. Never pruned. Preserve this optional inventory in CLI manifests/export/import!
* `export {full?:bool}` -> `{format:"synaps-axel-export/1",source_project:host_project,target_project:host_project,records,tombstones,histories,captures,fingerprints}`. This is exactly ONE ORIGINAL OWNER (the envelope project), never the alias group. Every inventory collection is filtered by that original owner. Both identity keys remain the original project even when it is an alias member. Version/scope fields are mandatory even for empty or tombstone-only inventories. Default metadata-only (content/messages/note empty,evidence null), intentionally not a restorable full snapshot. Full export includes restricted bodies for LOCAL OPERATOR ONLY; host MUST NOT expose export as a model tool. Oversized output fails whole, never truncates. Full note TTL/disclosure preserved through reserved `meta._axel.{disclosure,expires_ms}`; service enforces absolute expiry and original policy. No separate writable staging/export store.
* Native service `.r8` sources are detected by service markers: source project
  MUST equal envelope project. They use the native full exporter read-only,
  preserving original record schema/namespace/project/provenance plus that
  owner's **captures, histories, tombstones, and fingerprints** (not the legacy
  note-only conversion). They never flatten the alias union or retarget records.
* To copy a full alias group, the host MUST enumerate `scope_info.members`, then
  export each member independently with `full:true` (or native `legacy_export`
  using matching source/envelope keys), and import each inventory under that same
  original key with its own immutable migration identity. Only AFTER all imports
  may an explicit verified `scope_alias` reauthorize membership in the destination.
  An inventory never grants alias authority. Do not concatenate inventories under
  a canonical key: histories, tombstones and fingerprints have implicit ownership.
  Quiesce writers during a group backup; separate member exports are not one
  atomic group snapshot. Re-registering aliases atomically reapplies the union of
  deletion evidence. Existing source migration receipts stay unchanged; restore
  creates new idempotent per-inventory apply receipts, not copies of receipt tables.
* For non-native legacy Axel sources, `legacy_export {source_brain:absolute_r8_path,source_project:exact_String}` -> `{format:"synaps-axel-export/1",source_project:requested_source_project,target_project:host_project,records,tombstones,histories:[],captures:[],fingerprints:[],source_digest:64hex}`. Version/scope/source digest are always present, including tombstone-only inventories. Destination namespace is `project-<target_project>`; original category stays in `meta._axel.category`. HOST OPERATOR ONLY. Explicitly maps source project to target envelope project. Does NOT open/create target. Source must satisfy private path rules and be quiescent/checkpointed: nonempty WAL/journal rejected rather than silently ignored. SQLite immutable/read-only; no source schema initialization/writes. Unknown/unscoped/never-persist policies refused. Original ID, timestamp, tags, source policy/provenance/category/topic/title/abstract/sessions and exact expiry retained; restricted sensitivity mapped conservatively. Source digest SHA256 length-prefixed `[full_database_bytes,source_project_bytes,target_project_bytes]`, streamed without full file allocation. Source metadata checked unchanged; same-UID external writer exclusion remains operator responsibility.

## Retention

* `stats {}` -> `{notes,history,tombstones,bytes,bytes_kind:"scope_logical_payload",database_bytes}`.
  Counts and `bytes` concern only authorized members. `bytes` sums live note
  body/envelope, capture evidence, and protected history messages/note bytes.
  `database_bytes` is explicitly global allocated SQLite main-file bytes, an
  informational measurement only, never a project quota/deletion input.
* `sweep {max_age_days?:u32,max_disk_bytes?:u64}` -> stats plus `target_met` when
  target supplied. Service clock only; TTL always swept. Age/byte limits remove
  oldest authorized notes and capture evidence with permanent tombstones, never
  unrelated projects or protected history. Byte target concerns **logical scoped
  payload**, not physical disk usage; history can prevent attainment. No global
  VACUUM or global-page-driven project deletion. Tombstones/receipt overhead,
  FTS/free pages/WAL are not charged to another project. Logical deletion is not
  secure media erasure.

## Concurrency and cancellation

The private directory flock is acquired before any SQLite open/schema/write.
Contention uses `LOCK_NB` polling every 25 ms for at most eight seconds, below the
host's 15-second process deadline. Expiry returns `storage_error` before writes;
no committed operation is blindly retried. The held CLOEXEC descriptor releases
on process exit/cancellation; there is no persistent lock file or lock-helper
child. Host cancellation must terminate/reap its service process. Operations
still require commit-unknown reconciliation if a failure follows a commit.

## Frames and errors

Input cap including newline:24MiB for history_seal/migration_apply; 1MiB for all
others. Output cap:24MiB for history_fetch/export/legacy_export; 1MiB otherwise.
Existing note bodies<=16KiB, fetch<=25 IDs and exact existing hello unchanged.
Static existing error codes remain; post-commit sync failure is commit_unknown.
No paths, SQL, IDs or request body appear in error messages.

## Project forum (`forum.schema = 1`)

Repository capabilities add `forum:{schema:1}` and `forum_post`, `forum_read`,
`forum_forget`. Hello is unchanged. User-scope capabilities omit these, and direct
user-scope calls refuse before opening storage. The canonical pure Rust contract
is included by path from `crates/agent-core/src/memory/forum.rs`; IDs/digests and
bounds are not independently implemented by this service.

* `forum_post {author:Author,post:Post}` -> `Receipt`. This service-side request
  takes host-generated authorship, **not model-selectable authorship**. The outer
  transport project is authoritative. Post fields and controls must pass the
  shared validator. Timestamp is assigned by the service. Roots require title;
  replies require an exact root thread ID, empty title, and optionally a live
  same-thread parent. Owner checks, tombstone/fingerprint suppression, complete
  duplicate detection, reference validation and insertion share one transaction.
  A duplicate is resolved before parent validation; deleting/expiring a root does
  not invalidate an existing reply's retry. Status is `created`, `duplicate`, or
  `tombstoned`. Expired replay does not refresh or resurrect; it materializes the
  existing note tombstone. Foreign live/tombstone ownership is checked before a
  local tombstone response. Tombstones retain no forum title/body.
* `forum_read {thread_id?:String,query?:String,after?:Cursor,limit?:usize}` ->
  `Page {entries:[Entry],next:Cursor|null}`. Absent limit defaults to 8; range is
  1..16. Query is a bounded, case-folded literal substring of body/title, **not**
  SQL wildcard, FTS or semantic search. No thread lists roots with UTF-8-safe
  snippets of at most 400 bytes. Exact thread returns full posts including its
  root if live. Foreign/unknown threads both produce empty pages, not ownership
  errors. Deleted/expired roots are not joined against replies: surviving replies
  remain readable, but new replies refuse. Ascending `(timestamp_ms,id)` keyset
  pagination applies all normal scope/disclosure/retention/tombstone/fingerprint
  gates before limits. Digest validation loads the whole body before producing a
  snippet. A page has at most 16 KiB of body text and at most 24 KiB of serialized
  `Page` JSON (including escaping and cursor). It stops **before** an unreturned
  record and never advances the cursor past it. `next` is the last emitted cursor
  only when more records remain. For polling after a fully drained page, retain
  `entries.last().cursor()` even though `next` is null; retain the prior cursor on
  an empty poll. A cursor is neither authority nor a snapshot. Later appends are
  pollable; backdated imports/deletion can change inventory, requiring a restart.
* `forum_forget {id:String}` -> boolean. Exact `msg-<64 lowercase hex>` only;
  scoped deletion reuses note tombstones, fingerprints, FTS/graph cleanup and
  durable acknowledgement. Unknown ID returns false, foreign ownership refuses.
  Deleting a root never deletes a whole thread.

Wire objects deny unknown fields and mistyped values. Forum optional request
fields may be absent, not explicit null. Requests cannot name a body table, a
provenance source, retention/disclosure override, or a record timestamp.

Forum content lives in the existing `memories` row. Its outer persisted record
schema remains `synaps-axel/1`, namespace is exactly `forum`, ID is exactly
`msg-<digest>`, tags are empty, sensitivity is `normal`, retention is
`MaxAgeDays(envelope.retention_days)`. `meta._synaps_forum` is the strict shared
Envelope. Record project must equal its original envelope project and belong to
the authorized group; alias registration never rewrites it. Provenance is exactly
`{source:envelope.source(),session:envelope.author.group}` (source is
`forum:msg-<digest>`). Distinct messages with identical bodies therefore do not
suppress each other when one is deleted. Only meta keys `_synaps_forum` and
optional `_axel` are accepted. `_axel` may contain only `disclosure:"standard"`
and an `expires_ms` exactly matching timestamp plus envelope retention; no
repository metadata or alternate privacy/lifetime policy is accepted.

Ordinary `store` reserves the namespace, all `msg-` IDs and `_synaps_forum` metadata
independently, including malformed partial envelopes. Ordinary fetch/search
exclude all three markers **before** result limits, including automatic note
recall. Ordinary forget refuses `msg-` IDs in favor of the dedicated operation.
Generic internal insert and migration accept forum only after validating the
complete body/digest and record agreement. Metadata-only persisted/deleted-body
paths validate metadata separately; a cleared body is never accepted as a full
import. Native export validates physical tags, expiry, disclosure, timestamp,
ID/project and full digest before disclosure; full export preserves forum rows
and its standard absolute-expiry annotation. A full native export imported into
the same brain is a duplicate despite that annotation. A replay with a later
unhashed timestamp preserves the first timestamp, never extending retention.
Imports intentionally do not require live thread roots: inventory may contain
surviving orphan replies. No forum content/index table or private parallel store
is created. Existing scoped sweeps, logical quotas, owner-specific exports,
migration transactions, private-path cross-process locking, WAL/fsync durability,
commit-unknown handling, and deletion fingerprints apply unchanged.
