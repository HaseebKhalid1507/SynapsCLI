# Subagent context rollover — design note (v3.1)

Status: design; slices 1–3 (dark schema + pure resolver, owner-scoped gate,
lineage validation + owned-history retrieval) are built, and so are the
resume-admission part of slice 4 (§4 "Resume admission") and the core of its
durability part (§3.3 "Built"), and the checkpoint owner wired into the
reactive drive, start and resume behind an off switch (§3.3 "Built", slice 4
drive wiring and launch), and the run deadline policy of slice 5 (§5 "As
built"), all dark; slice 9 turns worker rollover on for start, resume and
the oneshot together (§12 item 9 "As built").
Base: the rollover-target work (`docs/specs/context-continuation.md`).
`E/` = `crates/agent-engine/src/`, `C/` = `crates/agent-core/src/`; lines are at
that base unless marked `rs:` (rolling-summary branch, commit cc7f15ce).

## Changes by round

| Round | Finding | Resolved in |
| --- | --- | --- |
| 1 | B1 one durable record protocol, uncertainty, precedence | §3, tests R*/F* |
| 1 | B2 retrieval authority, lineage, resume identity | §4, tests A* |
| 1 | B3 absolute worker deadline, no renewal | §5, tests D* |
| 1 | Raw per-window files | §3 (dropped) |
| 1 | Per-model ledger on every exit; summary vs rounds | §6, tests L* |
| 1 | Target, constraints, notices, counters | §7, §8, tests C*/V* |
| 1 | Gate by owner; start/resume/oneshot parity | §2, §8, tests G*/O* |
| 1 | Open questions, slice order, reload first | §10, §12 |
| 1 | Rolling-summary conflicts; provider/model matrix | §7, tests P*/M* |
| 2 | Writer quiescence, live-run ownership, late writer | §3.3, F7 |
| 2 | Generation/payload contract, spawn/missing head, digests | §3.1–3.4, R*, F* |
| 2 | Resume/authority schema, provenance, symlinks, mode-off retrieval | §4, A* |
| 2 | Ledger identity: request id, purpose, measured vs liability | §6, L* |
| 2 | Worker summary-model policy | §7.2, P3, M* |
| 2 | Dark slice-1 scope | §12 slice 1 |
| 2 | Completion-lifetime ancestry, Axel negotiation, 64-slot + byte ceilings, slice 8 = all obligations, slice 5 split | §2, §4, §9, §12 |
| 3 | Versioned digest encoding `c1`; predecessor digest scope | §3.1, R14 |
| 3 | Canonical lineage mapping and bounds | §4 "Lineage structure", R11 |
| 3 | Payload-free release; ledger slot reserved before dispatch; cap tests | §6, R13, L |
| 3 | Resolver selects, never certifies durability | §3.2, §3.4, F |
| 3 | Strict v2 meta, binding digest syntax, canonical handles, parse vs validate, v1 policy | §3.1, §3.4 |
| 4 | Descriptor-relative no-follow paths; hash-to-hash ownership; whole-file cap | §4 "Paths", "Validation", A7 |

## Motivation

Operational evidence (not verifiable from code): on a live daemon a resumed
subagent polling a lock file re-sent a context growing from about 609k to 621k
tokens every ~5 minutes for over an hour, with ~180 output tokens per request.
Workers never roll over today, so each request pays the whole history.

## 1. Current behaviour (evidence)

- **Off for every worker.** `spawn_runtime` (`E/tools/subagent/mod.rs:187-196`)
  → `EngineHost::worker_runtime` (`E/host.rs:357-369`) → default
  `ContinuationState` (`E/runtime/mod.rs:1016`; mode `Off`,
  `C/core/config.rs:112-116`). Only `apply_config` copies continuation and
  compaction settings (`E/runtime/mod.rs:2534-2541`); the worker policy does
  not (`E/tools/subagent/mod.rs:106-137`) and reads disk config
  (`start.rs:278-282`), so a live `/context auto` is not inherited.
- **Growth.** No assessment when off (`E/runtime/stream.rs:1107`). Worker
  defaults have no context or elapsed cap (`E/runtime/budget.rs:78-86`;
  `max_elapsed` configurable, `budget.rs:117-119`).
- **At capacity.** Context errors (`E/runtime/openai/stream.rs:1199`) and
  unexpected empty replies (`stream.rs:1412-1424`) become `Flow::Failed`
  (`E/tools/subagent/drive.rs:194-198`); an empty end-turn after tool results is
  a clean finish (`stream.rs:1398-1410`). On error the drive returns at once,
  possibly before later history/usage events.
- **Naive enablement fails closed:** no builtin memory tools in the worker
  registry (`E/tools/registry.rs:137-155`; required at `stream.rs:645-654`,
  `1141-1160`); the pending gate's global set (`stream.rs:1161-1180`,
  `E/orchestration.rs:632-648`) includes the running worker; `persist_head`
  (`E/runtime/continuation.rs:612-648`) is dropped by `WorkerDrive`
  (`drive.rs:198`) and oneshot (`E/tools/subagent/oneshot.rs:334`).
- **Time.** A committed rollover resets elapsed time (`stream.rs:1269`,
  `budget.rs:272-277`); wall-clock expiry becomes a rollover (`stream.rs:822`).
- **Archives.** `<base>/subagent-history/<pid>-<uuid>/<handle>.json`
  (`archive.rs:53-73`), atomic write (`archive.rs:95-123`), `version == 1`
  (`archive.rs:155-164`). `checked_archive_path` canonicalizes and checks
  containment and extension (`archive.rs:129-153`); it does **not** reject an
  in-root symlink. Old directories are pruned by age without owner checks
  (`archive.rs:76-89`).
- **Reload.** The idle probe ignores workers (`E/daemon/mod.rs:421-451`);
  reload then execs (`E/daemon/reload.rs:378,431`).

## 2. Enablement, identity and the pending gate

- On when the **parent runtime's live** mode is `auto` at spawn/resume.
  `ToolCapabilities` (`E/tools/mod.rs:99-161`) gains a `WorkerContextPolicy`
  snapshot (continuation config, summary/compaction settings per §7.2,
  disclosure mode/exclusions, the parent's window override with its model
  identity). Same for start, resume and oneshot.
- Opt-out only: `context_rollover: "inherit" | "off"` on all three tools, other
  values rejected; operator key `subagent.context_rollover = inherit|off`.

  *As built (slice 8b, dark).* `ContextRollover { Inherit, Off }`
  (`C/core/config.rs`, `SubagentLimits::context_rollover`, default inherit).
  The operator key takes `inherit` or `off`; any other value is off with the
  config warning "expected inherit or off; worker context rollover is off"
  (an opt-out fails towards off). The snapshot carries it as
  `WorkerContextPolicy::operator_rollover`. The TUI lists the key among its
  settings exclusions until enablement, which must add a row. The key is
  inert while worker rollover is off; an older binary (a rollback) reports
  `subagent.context_rollover` as an unknown key, so it must not be set
  before enablement. The caller's parameter is read by
  `context_rollover_param` (`E/tools/subagent/launch.rs`) only when worker
  rollover is built in (the tools pass `WORKER_ROLLOVER`): absent or null is
  inherit, the strings "inherit" and "off" are taken, and anything else is
  refused with `context_rollover must be "inherit" or "off"`, checked right
  after the task (start, oneshot) or instructions (resume), before anything
  starts. While it is off the parameter is not read, so a call that passes
  it, with any value, is treated like a call with any other unknown key:
  no caller-visible change. `with_rollover_param` adds the parameter to the
  tools' schemas only when `WORKER_ROLLOVER` is on, so a model is never
  offered a parameter that does nothing; the historical gate `optout_gate`
  pins the three schemas as they were (fixtures written twice at
  0ed5f2cb). A worker
  gets a plan only when `wants_rollover`: `WORKER_ROLLOVER` and
  `rollover_wanted`, which needs the caller and the operator to inherit and
  the parent's live mode to be `auto`; off from either side wins. An
  explicit opt-out is silent (no `rolloverNotice`: the caller or operator
  chose it). The oneshot reads the value and plans its run as start
  does (8c, §8). Tests: `x1`–`x4` in `tools::subagent::launch::tests::optout`
  (the values with the switch on, and nothing read with it off; while it
  is off all three tools treat any value as an unknown key, with nothing
  started; the opt-out table, never a plan while dark; with the switch on
  the schema gains only the optional parameter),
  `the_operator_opt_out_reaches_the_worker_snapshot`, and the core config
  tests for the key and its malformed values.

  *As built (slice 9).* `WORKER_ROLLOVER` is on: the three tools read the
  parameter before anything starts (`x2`: a bad value is refused, the
  others give the outcome of the call without it) and offer it in their
  schemas, optional (`x4`; `optout_gate` re-pinned, the parameter the only
  change). The TUI's Agent settings list `subagent.context_rollover` as a
  cycler of `inherit` and `off` (its exclusion is gone); it shows the value
  as the parser reads it (unset is inherit, a malformed value off), and an
  edit is checked by the config parser (test
  `subagent_context_rollover_row_validates_and_displays_the_opt_out`). An
  opt-out runs the worker as a v1 worker without a notice, on all three
  tools (`e5` in `oneshot_parity`).
- Thresholds are re-validated for the worker window (`C/core/config.rs:209-234`),
  falling back to the window's defaults with a notice (§7.2, as built). The
  window override is inherited only for the same normalized
  model/provider/endpoint.
- Run identity: random `run_id` (128-bit, lowercase hex) per execution;
  continuation logical ID `worker-run:<run_id>`. Handle IDs are process-local
  and are not identity.
- Gate: `unreconciled_descendants_of(owner)` over **completion-lifetime
  ancestry**. Reservation edges are not enough: `release_delegation` deletes
  them (`E/orchestration.rs:324-337`) when finalization runs (`start.rs:360-391`),
  while unreconciled entries outlive it (`orchestration.rs:632-647`). Record the
  owner in the completion record until reconciliation, without extending
  resource reservations. Lock order: tree, then completion registry, single
  snapshot. The session gate and local-registry check (`stream.rs:1164-1172`)
  are unchanged; nested delegation stays unavailable.

## 3. Worker record protocol

### 3.1 Records and digests

Head (`<handle>.head.json`, while running) and final (`<handle>.json`) records,
v2, in the existing per-process directory; dropped history lives only in the
eligible archive store (`continuation.rs:733-737`). No raw per-window copies.
Fields: `version: 2`, `run_id`, `handle_id`, `kind: head|final`, `generation`,
`history_digest` (`sha256-c1:`, below), `binding` (backend kind +
`scope_digest` = `sha256:<64 lowercase hex>` of the host-resolved repository
scope key, compared as an exact string), `predecessor` (§4), `windows`,
`inherited_windows`, `window_ordinal`, `rollovers_this_run`, `ledger` (§6),
`meta`, `status`, `output`, `history` (current window verbatim, including the
whole `_synaps_context` marker with `summary`/`pinned_stub`,
`rs:E/runtime/continuation.rs:648-656`). Generations, digests and run IDs are
host-generated and live only in records, never in the stream event (which
carries session ID, messages and receipt only, `continuation.rs:629-635`).
Digests are integrity checks, not authentication (§4).

**History digest `c1`.** `history_digest = "sha256-c1:" + hex(SHA-256(c1))`,
lowercase hex, where `c1` is `b"synaps.worker-history.c1\0"` followed by the
`history` array in a tag-length-value encoding (all lengths and counts u64
big-endian): `n` null, `f` false, `t` true; `u` + 8 bytes for an integer in
`0..=u64::MAX`; `i` + 8 bytes (two's complement) for a negative integer down to
`i64::MIN`; `d` + 8 bytes for any other number as its finite IEEE-754 binary64
bits (so `1` ≠ `1.0` and `0.0` ≠ `-0.0`); `s` + length + UTF-8 bytes verbatim
(no escaping or Unicode normalization); `a` + count + elements; `o` + count +
entries sorted by the key's UTF-8 bytes at every depth, each key as length +
bytes then its value. Nesting is limited to 256. It hashes the parsed value,
independent of serde_json map order or text formatting; JSON numbers outside the
integer range are binary64 once parsed (the engine parses with
`float_roundtrip`). Golden vectors are fixed in the tests; any change to the
encoding is a new prefix. The digest commits to `history` only, not to lineage,
binding, ledger, metadata, status or output; so does `predecessor.digest`.

**Parsing and admission.** v2 records are strict: unknown fields at every
typed level (record, binding, predecessor, window refs, ledger entries,
amounts, usage, and a v2-only `meta` type) and repeated keys are rejected;
`history` stays open JSON. v1 archives keep the permissive legacy parser.
Parsing is syntactic only; a record is admitted only by `resolve`, which
validates it (§3.4). Handles are `sa_<n>` with `n` the canonical decimal of a
u64 in `1..=u64::MAX` (no sign, no leading zeros).

### 3.2 Generations and payloads

- **Spawn head:** generation 0 is published before the first inference of a
  rollover-enabled run (initial messages, empty ledger). No head ⇒ the run never
  became rollover-capable; resolution falls back to the final, or `NotFound`.
- **Rollover head:** generation g+1 holds the successor; it is committed only by
  rename + directory fsync.
- **Final:** carries `generation = g`, the last committed head it supersedes,
  and the latest *valid* history: the in-memory history after the last round
  when publication of g+1 is **proven not to have happened** (temp write/fsync
  failed before rename), so completed rounds are not lost; the reconciled head
  after an uncertain publication (§3.3).
- **Ordering:** a final supersedes heads with generation ≤ its own; a head with
  a greater generation than the final is newer and unresolved. Equal run IDs and
  handle IDs are required between head and final.
- **Receipt loss** (consumer gone, task panic): treated as uncertain.
- **Final-write uncertainty** (rename/fsync error on the final): the head stays
  authoritative for the checkpoint owner; retry later; never delete the head
  until the final is durable. The pure resolver (§3.4) only selects a disk
  candidate and may select such a final; the owner keeps the uncertainty and
  durably republishes the selected final before acknowledging it, reporting it
  durable or deleting the head.

### 3.3 Writer quiescence and ownership

**Invariant:** no uncertainty resolution, finalization, replacement writer,
normal reload or pruning of the run's directory while an earlier writer of that
run may still publish. The persistence task is owned (§5) and its completion is
the only quiescence proof in-process; after process death no in-process writer
survives. A live run holds exclusive ownership (an in-process lock keyed by
`run_id`, plus a directory lock file checked by resume and pruning), so a
concurrent resume of a live or unresolved run is refused.

States, in one `WorkerCheckpoint` shared by the reactive drive and oneshot:
`Committed(g)`; `PrePublicationFailure` (final from latest valid history at g);
`Committed(g+1)`; `Uncertain(g+1)` (rename/fsync error after rename, receipt
loss, or task still pending). Uncertain blocks inference and ordinary
finalization; after quiescence the resolver re-reads the disk head: the g+1
candidate (run ID, generation, digest) is adopted and republished durably; g is
used as is (in-memory evidence beyond it goes to a non-authoritative
`<handle>.report.json`); anything else stays unresolved with
`archive_error = "publication uncertain"`.

**Built (slice 4 durability core, dark):** `E/tools/subagent/record_store.rs`
publishes a record as a fresh private temp file in the process directory,
`fsync`, `renameat` over the record name, then `fsync` of the directory, all
relative to the directory handle without following links; the outcome is
durable, not published (failure before the rename) or uncertain (rename or
directory sync failed). Opening the process directory syncs the root after
creating (or finding) it, before anything is published into it; the root and
its ancestors are host-configured and assumed durable. Temp and report
cleanup is best effort (readers ignore both; directory-age pruning removes
residue). `RunOwnership` is the run's in-process entry by run ID plus a
`flock` on `<handle>.lock` (a private, single-linked regular file of this
user) in the process directory; resume (`admission::writer_state`) and
pruning (`archive.rs`) test that lock and treat a held or uncheckable lock as
a live writer, for runs of any process; the kernel drops it with the process.
Every lock (the run lock, a probe's instant shared lock, the lifecycle lock)
is released with an explicit `LOCK_UN` before its descriptor closes, so a
child forked meanwhile (sharing the open file description until it execs)
cannot keep it held; if this process dies abruptly while such a child is
still before exec, the lock stays held until the child execs or exits, and
the run counts as live meanwhile (fail closed). An acquire racing with a probe
fails closed (`LockHeld`). The root's `.lifecycle.lock` is never waited for
unboundedly. Pruning runs once per root and process on a thread of its own
(never on the tool-execution path), tries the lock exclusively once without
waiting, holds it for its whole scan and delete, and prunes nothing when it
is held elsewhere. Acquiring ownership polls it shared without blocking for
at most 2 s (`STARTUP_WAIT`, monotonic from the call, opening the lock
included; cancel-aware; a blocking call, for a blocking worker): the first
attempt is made at once, every later one only before the deadline, so a late
wake-up never acquires past it, and an interrupted attempt goes back through
the cancellation and deadline checks. It fails with `LifecycleBusy` at the
deadline and never starts without it, then checks that its process directory
is still the one named in the root. The drive wiring runs a worker whose
ownership fails (`LifecycleBusy` or any other error) with rollover off, never
as a failed start. A lock file that is not a private, single-linked regular file is
refused and never repaired (pruning warns and prunes nothing, ownership
fails); the host removes it while no process uses the root. A root that
cannot be opened or read gets its own warning (its access needs repair;
removing the lock would not help). v1 users get the
root's `.lifecycle.lock` (0600) on its first use already, before any
enablement. `E/tools/subagent/checkpoint.rs` is the `WorkerCheckpoint` above: each
publication is an owned `spawn_blocking` task holding the ownership, and the
checkpoint installs it before any await, so a waiter that times out or is
dropped leaves it as a barrier (`Pending`): nothing else (inference, another
head, finalization, retry, reconciliation) runs until `quiesce` joins it and
applies its outcome; an owner that is dropped leaves the run owned until the
task ends. `may_infer` is true only in `Committed` with no unjoined task; a
passed deadline refuses a checkpoint before anything starts. Nothing creates
a checkpoint while worker rollover is off. Unix only (macOS: CI `cargo
check` only); elsewhere no v2 writer exists and pruning keeps its legacy
behaviour.

**Built (slice 4 drive wiring, dark):** `E/tools/subagent/persist.rs`
(`WorkerPersistence`) is the checkpoint owner at the reactive drive
(`drive.rs` `run_with`). `claim` takes the run's ownership on a blocking
worker with the 2 s startup wait and the worker's cancellation; any failure
means rollover off for that worker. `begin` publishes the spawn head
(generation 0) before the first inference: not published, or an uncertain
write the re-read disk shows never landed (only a non-authoritative report),
runs the worker with rollover off and releases ownership; an uncertain write
that cannot be decided refuses the run before any inference. Each runtime
context-head checkpoint must carry this run's logical ID and exactly one
marker naming the next window; it becomes the next head (the sealed window
is appended at the current ordinal) and the receipt is acknowledged only once
that head is durable; anything else (refused, not published, uncertain, lost
receipt) fails the receipt, which stops the runtime before further
inference. An unstarted checkpoint after cancellation or the deadline is
refused. When the run deadline passes while a head is in flight, the drive
cancels the worker, fails the receipt, waits up to 30 s for the write
(`BARRIER_WAIT`; the write keeps the ownership even if the wait ends first),
then takes its timeout path; with persistence the drive's deadline is the
run deadline. At the end, `finish` joins any started write (up to 30 s),
reconciles an uncertain head from disk, and publishes the final from the
terminal status and output; its history is the latest in-memory history when
that continues the committed head's lineage (rounds after a head that was
provably not published are kept), otherwise the committed head's own (a
successor the runtime never took). An uncertain final is retried once; an
undecided or unpublished final leaves the head as the crash checkpoint and
reports the error. Without persistence (production today) the drive answers
a checkpoint with an explicit `worker rollover is off` error.

**Built (slice 4 launch, dark):** `E/tools/subagent/launch.rs` is where
`subagent_start` and `subagent_resume` run a worker (`run_worker`, given the
configured runtime's stream start). A `LaunchPlan` (archive root, process
directory, handle, the binding the worker runs on, metadata, and a v2
predecessor's lineage for a resume) is built only while `WORKER_ROLLOVER` is
on (it was `false` until slice 9) and the worker `wants_rollover` (§2);
without a plan the run is exactly the previous drive: nothing is claimed, created or published,
a checkpoint is refused, and the finalizer writes the v1 archive. With a plan,
before the first inference: a binding that cannot confine retrieval to a
lineage, a run that cannot be claimed (a busy lifecycle lock after the 2 s
wait included), a spawn head that cannot be built or was provably not
published, each run the worker with rollover off, as a v1 worker, never as a
failed start; an undecidable spawn head fails the run without inference
(history kept in memory, no v1 archive). A claim creates a missing root
private (0700), like the v1 writer. Resume admission returns the admitted v2
record's lineage (`successor_lineage` of the selected head or final), so a
resumed run's spawn head continues it; a v1 archive, an in-memory history or
the prior run's text starts a fresh lineage. Once the spawn head is decided
the run is a v2 run (`SubagentState::v2_records`): the finalizer and the
collect/resume archive retry never write its v1 archive (it would take the
final's name); if the run ends without a final (a panic), the finalizer
reports that and keeps the history in memory. Cancellation and the run
deadline (one monotonic start, taken before the launch) are checked again
after it, however it ended; either ends the run before its first inference
(failed or timed out: a v2 run publishes its final, a run that fell back to
rollover off is archived by the finalizer as v1). A fallback keeps that run
deadline; it never starts a fresh one. After the drive the final carries the
status the finalizer will report (a cancellation requested later is not in
it), the run's output and its latest history; the handle then points at the
final, records the run that published it (`SubagentState::v2_final_run`, kept
with the path when the handle is reaped) and releases the history.
`subagent_collect` of a reaped handle reads a v2 final only for that recorded
run, through the bounded record-file reader and never the v1 reader; every
other handle's record is a v1 archive, read as before. The lookup and read
run on a blocking worker. Record locations are built from the root a
`RecordDir` canonicalized when it was opened, without filesystem access. A
fallback to rollover off is reported only to tracing today; slice 9 adds the
caller-visible notice.

**Root relocation (host rule).** Hosts must not relocate, rename or replace
the history root (`<base>/subagent-history`) or a process directory below it
while any process may use it. A run writes through the process directory it
opened at its claim, but every location it reports, and every later read
(reconcile, collect, resume admission), names the record by the root's
pathname as canonicalized at that claim. If the root is moved under a live
run, its writes follow the open directory to the new place while the
reported `archive_path` still names the old one: a reconcile then finds no
head there and leaves the publication unresolved (nothing adopted, no
inference), and collect or resume of that path finds nothing (or, at a root
recreated in its place, nothing of this run) and refuses. Relocation can thus
lose a run's result to the caller, never substitute another record for it.
Reconciliation stays path-based (a read through the claim's directory handle
would decide this one case but not collect or resume, which have no handle).

### 3.4 Resolution (pure, slice 1)

`resolve(input)` decides over **supplied, parsed** candidates and never
performs I/O or recovery. Inputs: the head and final candidates (absent,
unreadable, v1, or v2), writer state (quiescent or may-publish), and
expectations (handle, run ID, binding). Outcomes: `NotFound`; `Legacy(v1)`;
`Authoritative{kind, record, loss_warning}` (head ⇒ crash checkpoint, warning
set); `Unresolved(reason)`. Rules: may-publish ⇒ unresolved; unreadable head ⇒
unresolved (atomic rename never leaves a torn head); v1 final with a v2 head ⇒
unresolved; head-only and final-only are each validated (kind, version, digest,
lineage structure and cap, expectations); pairs apply §3.2 ordering. Locators
are sibling names derived from a validated handle; no global index by run ID.
`Authoritative` is candidate selection, never a durability certificate (§3.2).
`Legacy` satisfies no v2 expectation: an expected run ID or binding refuses a
v1 archive, and a v1 archive is re-checked for version 1 even when supplied
already parsed. While the writer may publish, nothing else is examined.

## 4. Lineage, authority and retrieval

- **Trust:** records are trusted host storage (0600 files under a 0700 root the
  host created); integrity is checked, but a record never grants access by
  itself. Grants come from records the host wrote for this binding plus
  host-sealed archive references; model text, marker IDs, ID prefixes and
  caller-supplied files create no grants.
- **Predecessor:** `{run_id, kind: head|final, generation, digest}`; a validated
  head is a valid resume source with a loss warning. The predecessor's own and
  inherited refs are copied by value; later pruning of its directory does not
  invalidate the copy.
- **Validation before inference** (also when rollover is now off): binding
  equals the current backend/repository; every ref the store still has is
  live in that binding and owned by the chain member the record lists for it
  (sealing stores only the hashed logical ID; the host compares that stored
  hash with `logical_digest("worker-run:<run_id>")`, the store's exact
  length-prefixed SHA-256 derivation, hash to hash); foreign, malformed or
  tombstoned refs refuse resume (never re-sealed); the latest marker archive
  equals the record's latest window; total own + inherited refs, ≤ 128 (§4
  "Lineage structure"). A ref absent from a *successful* inspection was
  evicted for capacity (Axel keeps ≤ 256 history rows per authorized scope
  group, tombstones included, and ≤ 256 MiB; a seal at a cap deletes rows by
  class, then oldest, without a tombstone: tombstoned, stale, superseded,
  newest, and only then an active lineage's floor rows; CONTRACT.md), unless
  the store says it never evicted (below): it is left out of the grant, never allowed, and
  the resumed worker gets one notice in its opening turn (after the resume
  header, before the caller's instructions; after the daemon's resume text on
  a daemon resume) that those windows are no longer in memory. This holds for
  the latest window too; if every ref is evicted the worker gets an empty
  owned grant (its history tools allow nothing), never full history. A failed
  or malformed inspection is a lookup refusal, never eviction. The successor
  keeps evicted refs in its lineage, ordinals unchanged, so the lineage and
  marker checks hold; its own resume finds them absent again and leaves them
  out again. One INFO log line per admission gives the listed and evicted
  counts, the evicted archive IDs and the store's eviction count. The parent
  is told the count only (§8 "Evicted windows").
  **A store that never evicted** (B31 follow-up): an absent ref reads as
  evicted only where the store may have evicted it. Axel advertising
  `history_owned` version 2 answers `history_inspect {ids, version: 2}` with
  `{entries, evictions}`: the rows of version 1, and the brain's eviction
  count (`brain_meta` key `synaps-axel/2/history-evictions`, counted by
  SQLite as below; `null` when unknown). The host asks for version 2 only when `versions` lists 2; the
  version 1 reply is unchanged, byte for byte. The legacy store never evicts
  (count 0). When listed refs are absent and the count is exactly 0, the
  store has never deleted a history row, so it is not the store the worker
  ran against (brain wiped, replaced or misconfigured): the resume is refused
  before anything is reserved (`Refusal::NotHeld`), counts only: "this memory
  store holds {absent} of this worker's {listed} windows nowhere and has
  never evicted history: it is not the store the worker ran against (brain
  wiped, replaced or misconfigured)". A count above 0, `null`, or a version 1
  service keeps the eviction reading; every ref live is admitted whatever the
  count. Residual: the counter is SQL-trigger-based for brains created by
  this revision: a new brain gets the row at `0` and an
  `AFTER DELETE ON synaps_history` trigger (`synaps_history_evictions`) in
  one transaction, and SQLite adds one per deleted row inside the deleting
  transaction, whichever sidecar deletes it. An eviction by an older sidecar
  that counts nothing (a rollback to 5a876241) is therefore still counted,
  and any other delete only over-counts, which weakens a 0, never fakes one.
  The sidecar adds nothing itself. The count is `null` unless both the row
  and the trigger exist. Brains that predate this revision read unknown and
  never gain either, so they keep the eviction reading. The count is
  brain-wide, not per scope or lineage, so a store that has evicted anything
  still reads a missing window as evicted. That includes a replaced brain
  that has evicted before, so a swapped or partly restored store with a
  count above 0 is still read as evicted. Every applied migration
  (`migration_apply`, with or without history; an exact replay changes
  nothing) removes the row in its own transaction, so the brain reads
  unknown from then on. An Axel to Axel migration carries a source brain's
  history, and windows the source evicted are absent from its inventory, so
  a kept 0 would refuse resumes the source would admit. That inventory's
  history can even be empty although the source evicted: eviction picks its
  victims across the whole scope group, but an export takes one project, so
  an alias member whose rows were all evicted exports `histories:[]`. A
  non-native legacy export, which never carries history, marks the count
  unknown too: every apply is treated alike. A failed apply rolls the
  removal back with the import. v1 archives resume through an explicit
  legacy path with no lineage.
- **Lineage structure** (checked by `validate`, slice 1): the lineage is a
  linear chain of runs. `inherited_windows` then `windows` list every sealed
  window of the lineage exactly once with ordinals 1, 2, … in list order
  (inherited first, oldest run first, each inherited run's windows contiguous
  and never resumed after a later run's); so each archive ID maps to exactly
  one (owner run, ordinal), and exact or conflicting duplicates are rejected,
  not normalized. No inherited owner and no predecessor is the record's own
  run; inherited windows require a predecessor. `rollovers_this_run` equals the
  number of own windows and `window_ordinal` is the ordinal after the last
  sealed window. The serialized vectors together hold at most 128 entries
  (checked before any other lineage work). Relations of `generation` to these
  counters are left to the durability slice, which defines checkpoint heads; a
  byte limit on the whole file belongs to the file adapter (slice 3/4).
- **Paths:** explicit `archive_path` is validated first by spelling (absolute;
  no `.`, `..` or empty components; exactly `<root>/<pid>-<uuid>/<sa_N>.json`
  or `<sa_N>.head.json`); only the trusted root is resolved (canonicalized, so
  either spelling of it matches). The sibling is derived from that name. Then
  descriptor-relative opens: the
  trusted root (owned, not group/other-writable), the process directory via
  `openat(O_NOFOLLOW|O_DIRECTORY)` and each record via
  `openat(O_NOFOLLOW|O_NONBLOCK)` relative to it, so a component swapped for a
  link at any time fails the open itself; no `lstat`-then-open window. Opened
  objects must be this user's with no group/other bits; records regular and
  single-link; size checked on the handle (≤ 64 MiB) before allocation and the
  read capped (`E/tools/subagent/record_file.rs`). Trust model: the root and
  its ancestors are host configuration, never model-controlled, and trusted;
  this is not a defence against a malicious same-user writer (record
  integrity and backend ownership checks still apply). Unix only; macOS is
  covered only by CI's `cargo check`.
- **Resume admission** (`E/tools/subagent/admission.rs`, wired in
  `resume.rs`): every file source (explicit `archive_path`, and this
  process's own archive of a reaped or collected handle) is located, loaded
  and resolved as above and the selected record admitted: v1 only when not
  shortened, v2 through "Validation before inference" against the binding
  the worker will run on. A history kept in memory because its archive could
  not be written is admitted only when not shortened. Refusals carry their
  reason and happen before a new ID, tree reservation, model grant, handle or
  runtime exists. Before admission, finding a handle of this process may
  retry that worker's own failed archive write (the existing archive
  recovery, which can write the old worker's archive), and admission reads
  files and asks the backend (bounded); a refusal changes nothing else.
  Callers do not wait for another caller's retry in flight (transient): a
  collect meanwhile reports the failed write as it stands, and a resume may
  continue the finalized history kept in memory (the history being
  written) instead of the archive. A
  head resume returns the loss warning to the caller. For an owned grant the
  resumed worker's registry is built before the ID is taken, from the worker
  registry (`retrieval::owned_worker_registry`): every non-builtin memory
  capability is left out, and both owned-history tools are added, strictly
  additively (a builtin of either name refuses the resume). A memory
  capability is recognized by the operation its extension, MCP server or
  plugin registered (`Tool::source_operation`, `ToolOrigin`), or by a
  runtime name that sanitizes to a memory tool's wire name (`memory:search`).
  A custom tool with any other name cannot be classified generically, and
  this is no sandbox: shell and file tools stay. Full-history workers keep
  their merged extension tools. The worker runs on that registry and on the
  grant's binding. Writer quiescence:
  a worker of this registry that is running, or has neither finished
  finalizing nor published, refuses the resume, and so does a held or
  uncheckable run lock of any process (§3.3 "Built"); a v1 worker takes no
  lock and relies on the single atomic publication of its final.
- **Known dead end (slice 9; slice 10 fixes it).** A successor admitted
  with an owned grant that then runs with no plan (its caller or the
  operator opted out, an n1-n3 fallback, or a disabled `memory_search` or
  `memory_fetch`) writes a v1 archive, and that archive holds the
  shortened history it continued. A v1 archive resumes only when not
  shortened, so that worker's next resume is refused ("legacy worker
  archive holds a shortened history without lineage"). This is known;
  slice 10 fixes it.
- **Retrieval:** builtin `memory_search`/`memory_fetch` in `OwnedHistory(allow)`
  mode, registered for any admitted v2 continuation whose history is shortened,
  even when new rollovers are off; otherwise that resume is refused before
  inference. Only `source=history`; notes, omitted/null source, note IDs, mixed
  batches, `scope=user` and project selection are rejected. Filtering happens
  inside the backend before matching, ranking, limits and snippets
  (`history_search_in(query, limit, allow)`; legacy: skip non-allowed records in
  the scan, `ArchiveStore::search_in`). Axel needs the negotiated capability
  `history_owned` version 1 (`max_ids` ≥ 128, `history_logical_id =
  sha256_length_prefixed`, operations `history_search` with bounded,
  validated `ids` and host-only `history_inspect`; `ids=[]` ⇒ no results and no
  request; version 2, when also listed, adds the eviction count above); returned IDs
  outside the allowlist are rejected as defense in depth; no fallback to
  project-wide search. Without the capability, new worker rollovers are off and
  shortened-context resume is refused. Every fetch ID is checked first.
- Parent history visibility is unchanged; collect labels worker windows with run
  provenance.

**As built (slice 8g, dark): a fresh v2 run's owned history.** A run whose
lineage is fresh (a start, a resume admitted without a grant, the oneshot)
gets a grant of its own when it becomes a v2 run, and its runtime runs on
its own owned-history tools from its first inference; nothing else of a
worker changes.
- *Grant:* `OwnedHistoryGrant` holds its allowed archive IDs behind a lock
  (sorted, unique; `allows` and a snapshot `allow`). A fresh run's grant
  (`OwnedHistoryGrant::fresh`) is made in `launch` right after the binding
  is found to confine retrieval (`worker_rollover_support`, which also
  gives the negotiated support), on the run's own `run_id`, empty: a search
  asks the backend nothing (`ids=[]`) and no fetch passes. It grows only in
  `WorkerPersistence::on_head`: once the head that lists a window is
  committed (durable), that window's ID joins the grant (`grow`,
  idempotent), before the head's receipt is acknowledged, so the run's next
  request can retrieve the window it just sealed. A refused head, one that
  failed before publication and an uncertain one add nothing. The ID is the
  one the host sealed under the run's logical ID, so growth asks the backend
  nothing (no `history_inspect`). A resume admitted with a grant keeps that
  grant and its registry (built at admission, above) and grows it with its
  own new windows the same way; the 128-ref lineage cap holds through the
  head checks.
- *Registry:* start, resume and the oneshot arm the run's plan with the
  worker's registry as configured, after every other policy is applied
  (`launch::arm_owned_history`, beside the run clock and the request
  ledger; for a resume admitted with a grant only the operator's disabled
  tools are armed, slice 9). `launch` builds the
  run's registry from it with `retrieval::owned_worker_registry` (other
  memory capabilities out, both owned tools in, strictly additive), before
  anything is written. If that can't be built (a builtin of an owned name
  is already there, the operator disabled `memory_search` or
  `memory_fetch`, or the registry was never armed), the run runs with
  rollover off, with the usual notice ("rollover disabled for this worker:
  owned history tools could not be installed: ..."). Only `Launch::On`
  carries the registry, and only the start closures install it, through
  `launch::start_v2_runtime` (`set_tools`, then `start_worker_run`), so a
  run without a plan and every fallback keep the worker registry unchanged.
- *What this replaces:* the oneshot's test-only seam
  (`RecordSite::test_grant`), which installed tools over an empty grant
  before the launch, fallbacks included. The tests now run on the real
  worker registry, with no seam, through start and resume as well (S0's
  sites: `SubagentStartTool::dispatch`, `ResumeEnv::planned`).
- *Tests:* `g1` (a fresh start and a oneshot roll over, then a search finds
  their own sealed window and only it, and a fetch of it returns its
  messages), `g2` (a resume of a v1 run's handle gets a fresh lineage and
  retrieves its new window), `g4` (with the parent's history and an earlier
  sibling's sealed window in the same store, both matching, a oneshot's and
  a start's search find neither and fetches of both are refused), `g6` (a
  run without a plan, a fallback and a v1 start offer the same tools, none
  of them a memory tool; a v2 run adds only its owned tools and the context
  checkpoint), `g8` (a resumed v2 run finds and fetches its predecessor's
  window and its own new one), `o9` flipped, in
  `tools::subagent::oneshot::oneshot_parity`; `g5` (only a durable head
  grows the grant, before its acknowledgement) in `persist`; `g7` (a
  builtin of an owned name, an operator-disabled owned tool and an unarmed
  registry fall back before any write, with the notice) and `w7` (only
  `start_v2_runtime` calls `start_worker_run`, and only the three start
  closures call it) in `launch`. Mutants: no growth at commit (`g1`, `g5`,
  `g8` red); the owned tools installed at arming, fallbacks included (`g6`
  red); fresh lineages sharing one grant (`g4` red).

## 5. Deadlines and cancellation

- Effective deadline = the earlier of run start + `timeout_secs` (0 = none) and
  run start + configured worker `max_elapsed`, on one monotonic run start,
  checked in `begin_round` (`budget.rs:282`). Worker role: no segment reset, no
  time-checkpoint rollover (`stream.rs:822`), no counter reset.
- Cancel and deadline are re-checked before the successor's first request;
  summary dispatch is bounded by the remaining deadline.
- Persistence runs as an owned `spawn_blocking` task; the drive keeps polling
  its deadline. Unstarted checkpoints after cancel/deadline are rejected; a
  started one is never raced: cancel, wait on its barrier, record the outcome,
  stop. A timed-out waiter never authorizes finalization (§3.3).
- Explicit resume is a new run with a new deadline; there is no automatic
  reload recovery.

**As built (slice 5, dark).** A run with a launch plan (worker rollover on)
arms one `RunClock` on its runtime before the launch
(`launch::arm_run_clock`, `Runtime::fix_run_clock`): one monotonic start, and
the earlier of start + `timeout_secs` (0 = none) and start + the worker
budget's `max_elapsed`. The drive and the runtime share that deadline: the
claim, the spawn head and every head publication wait at most until it, the
drive times the run out at it, and the runtime checks it in `begin_round`
before every request, a successor's first request included. On a run clock
the turn meter never resets its start at a committed rollover, never turns a
wall-clock expiry into a time checkpoint, and resets no counter; the
committed-rollover notice says the run deadline is unchanged instead of
offering a fresh wall-clock allowance. The runtime's own stop on the run
deadline (`BudgetExceeded { WallClock }`) is the run timing out (status
`timed_out`, a timeout report naming the run's limit), not a failure. After a
timeout the drive drains the cancelled worker for at most 30 s: the usage it
reports then is booked, and a checkpoint it asks for then is refused before
anything is written. A cancel is honoured at every stage of a rollover: at the
seal (the runtime stops before any checkpoint), with a checkpoint queued
(refused before anything is written), during a started write (the receipt is
refused at once, the write is never raced: its task stays a barrier, the run
waits for it and records its outcome), and after the acknowledgement (the
runtime stops before the successor's first request; the final carries the new
window). Without a plan nothing changes: no run clock is armed, a v1 run keeps
the drive's own timeout and the runtime's per-turn clock, and a wall-clock
stop keeps its v1 outcome. The summary stage is bounded as well (since the
rebase onto the rolling-summary line, a rollover sends one summary request
between the seal and the head checkpoint): a cancel stops it as it stops a
session's (nothing is sent after the cancel; an attempt in flight is
abandoned and the previous summary carried forward), and on a run clock no
summary attempt is started once the run deadline has passed, none waits
past it, and a summary body is refused when the gate's final clock check
sees the deadline passed, a cold attempt after an unsent warm one included
(`SummaryRequest::run_deadline`). The
deadline is the run's own instant (`RunClock::timer_deadline`: in production
exactly `RunClock::deadline`; a controlled test clock converts its advanced
time there), never rebuilt from the time remaining. Each attempt waits until
the earlier of that instant and its summary timeout from its own start. The
warm lease and the native cold snapshot gate carry the instant to the body
gate. The gate checks the lease or snapshot first (that check takes the
continuation lock, and the lease also its registry check) and the clock
last, right before the publication compare-exchange; when that final clock
check sees the deadline passed, the body is refused, whatever the caller's
wait does (Tokio's `Timeout` polls the request before its timer, so a
stalled executor may service a transport after the deadline). A validity
check that waits on the lock while the deadline passes therefore closes the
gate. A body whose publication won is sent and cannot be retracted. `summary_reason`
`run_deadline` means only that the deadline had passed at the seal, so no
summary request was started; a deadline that passes later, during
preparation or a request, ends the attempt as a normal timeout or failure
(`summary_reason` `failed`). A warm or native cold summary call that ends,
times out or is dropped unfinished withdraws its pending body (sessions too;
`keepwarm::WithdrawOnDrop`), so the client's connection task, which can
still hold the body, never produces it later. The same holds when the
rollover's own enclosing future is dropped (the turn task aborted while its
caller's stream stays alive, so no token is cancelled): a body held before
publication is never produced, no successor head is checkpointed or
committed, the window and head stay as they were, and the dropped attempt
keeps its reservation like any attempt dropped unsettled; a body published
first stays sent (`od1`–`od3` in
`rolling_summary::…::publication_tests::outer_drop_tests`, through a v2
worker's stream turn, warm and native cold). Known limits of the summary
stage:

- A timed-out warm or native cold attempt keeps its reservation as an
  unknown charge even when withdrawing its body proved it unsent (inherited,
  conservative). A refund would have to decide the withdrawal before
  settlement and never release on a lost publication.
- Routed summary transports (OpenAI-compatible, Codex, cloud) have no gated
  body: the run deadline bounds their pre-send checks and the caller's wait,
  not what the HTTP client's connection task does after the caller dropped
  the request.
- The final clock check and the publication compare-exchange are two
  steps: a task preempted between them can still publish a body at or after
  the deadline.

A run's terminal publication (the final, its directory sync and the head's
removal) starts after the drive and can outlast the deadline on a slow
disk. A oneshot's caller waits for it only a bounded time and the run's own
thread finishes it (§8, slice 8c). A reactive worker's run still waits for
it to its end (`run_worker_until`, unchanged; `t5`), and its finalizer runs
after it. A daemon reload sees a terminal publication still in flight (its
run is a busy writer in the writer table, §9) and, after a bounded wait,
refuses a normal reload on it (8e). Reactive runs keep that inline
publication; they do not take the oneshot's bounded, detached one. Why
not: the oneshot's bound exists because its caller's tool call blocks the
parent's turn, and nothing blocks on a reactive run's publication. A
detached publication would also let the completion's wake, a collect and
an immediate resume race the run lock the publication still holds (a
resume refuses while the writer may publish). The reload check already
covers it. Also tracked: a dropped oneshot's guard ends the
handle at its 30 s bound when the run is still launching, for example
behind a stalled spawn-head write, though that write may still land; only
the terminal publication keeps the handle pending past the bound.

The worker summary-model policy (§7.2) is built dark in slice 7: a v2
run's runtime uses it once the launch hands it the parent's settings; every
other runtime resolves its summarizer as a session does.

## 6. Accounting

- **Entry identity:** each ledger entry has a host `request_id` (stable per
  upstream request), `purpose` (`conversation` | `rollover_summary`), `model`
  (absent model on older events ⇒ the worker's model, `purpose = conversation`),
  and `amount`: `measured` (provider-reported tokens/cache splits) or
  `liability` (estimated micro-USD, not tokens).
- **Settlement is idempotent:** a duplicate delivery of the same entry is a
  no-op; `liability → measured | released` is the only transition (a request
  is first recorded as `liability` or `measured`; `released` carries no
  amount, so a release cannot restate the estimate); rejected updates change
  nothing; `measured`
  and `released` are terminal; conflicting updates are rejected. Measured cost
  and its reservation are never both counted.
- **Ledger capacity:** at most 4096 requests per run. Dispatch reserves the
  request's ledger slot *before* sending it (slice 6), so a full ledger refuses
  a new request instead of losing its usage; existing entries can still settle
  and redeliver at capacity. A stored ledger is a snapshot: over 4096 entries
  or a repeated request ID is malformed. Parent idempotence upserts a
  request's latest state rather than ignoring a seen ID.
- **Every exit:** the ledger lives in shared host state fed by the stream, with a
  bounded terminal drain after `Error`, cancel, panic or producer drop;
  finalization snapshots after settlement or keeps open liabilities. Usage
  events gain `request_id` and `purpose` (shared change with the summary
  branch, `rs:E/runtime/rolling_summary.rs:314-334`); a typed `UsageLiability`
  event carries `{request_id, model, purpose, estimate, state}`. Parent
  publication and collect deduplicate by `(run_id, request_id)`.
- Legacy total fields remain worker-model conversation usage only.
- Summary requests do not consume provider-round allowances (shared runtime
  policy: `SummaryAccounting` carries usage and cost only,
  `rs:...rolling_summary.rs:54-58`); one attempt per rollover, cost
  reservation, deadline and ledger request counts still apply.

**As built (slice 6, part 1, dark).** Identity is per upstream *attempt*:
one host `RequestId` (`req-<n>`, one sequence per run, never reused) is
minted right before each billable attempt is sent, so a round that retries
has one ledger entry per attempt and the slice-1 transitions are unchanged
(measured and released stay terminal). The run's ledger is a
`RequestLedgerHandle` (`E/tools/subagent/ledger.rs`), created with the
worker's model (purpose `conversation`) by the run's `WorkerPersistence`;
`for_requests(model, purpose)` shares it for a summary's requests (purpose
`rollover_summary`, on the worker's own model too). `reserve(estimate,
reports)` records the attempt as an open liability before it is sent;
`Attempt::observe` takes the route's usage reports by its semantics
(cumulative: the last report; deltas: their sum; not established: the
liability stays open); `finish` replaces the liability with the measured
usage, or leaves it open when nothing was reported; `release` only when the
attempt provably cost nothing (never sent, or a first-party status the
transport's typed classification already treats as no charge). A proxy's
status, 409/413/429, a transport failure after the send, a panic or a
dropped attempt all leave the liability open.
- **Estimate:** the request's input estimate at the model's input rate plus
  its whole output allowance at the output rate, in micro-USD rounded up (an
  unpriced model is priced at the conservative default rates). It
  saturates at `u64::MAX` and is never free by accident: a price beyond
  `u64` micro-USD, positive infinity or one that is not a number is
  `u64::MAX`; only a zero price is zero. Floating-point rounding can put it
  one micro-USD above the exact price, not below. It is a
  liability amount only and never refuses a request: there is no default
  spend cap, and only a user-set `max_cost_usd` refuses on cost.
- **Cap:** the one refusal this slice adds is the 4096-requests-per-run
  ledger cap, on v2 runs only (runs without a ledger dispatch as before),
  before anything is sent: the ledger refuses the reservation with
  `ReserveError::RunFull`, "this run reached 4096 provider requests; no
  further request is sent". Requests already reserved still settle at
  capacity. The reason reaches a caller as text, not as a type it can
  downcast: each transport maps the refusal into its own error (§6 part 2
  says how per route), and only the message is kept.
- **Events:** two `SessionEvent` variants, emitted only by a runtime with a
  request ledger: `UsageAttempt { request, usage }` in place of the plain
  `Usage` for that attempt (never both, so nothing counts twice), and
  `UsageLiability { request, estimate_micro_usd, state: open | released }`;
  `request` is `{request_id, purpose, model}`. Without a ledger (every
  session; workers while rollover is off) the `Usage` events are unchanged.
  The variants are host-internal and never serialized:
  `SessionEvent::for_clients` maps a `UsageAttempt` to the plain `Usage` it
  stands in for (attributed to the attempt's model) and drops a liability.
  The session actor applies it at both stream intakes (the turn and the
  cancel drain) and again in `emit`, so neither variant reaches
  `SessionEventWire::Stream` or the socket. The wire conversion is filtered
  itself: `WireEnvelope::for_client` (and `WireSessionEvent::for_client`,
  `WireStreamEvent::for_client`) is the only conversion from host events to
  the wire and builds no frame for a liability, so a direct conversion
  cannot produce one either (the actor numbers envelopes after its filter,
  so its frames' `seq`s stay gapless). A ledgered attempt has exactly the
  wire form of its plain `Usage`; there is no new wire variant, so older
  clients have nothing to skip. The engine's stream consumer and the TUI
  handle the variants the same way.
- **Legacy totals:** the drive's totals stay the worker's conversation
  usage: a `UsageAttempt` counts only for purpose `conversation`; an untagged
  `Usage` counts as before (an older event is the worker's conversation). A
  v1 run sees no ledger events, so its totals are unchanged.
- **Records:** every rollover head and the final snapshot the ledger, open
  liabilities included (the spawn head's is empty). On a provider error a v2
  run cancels and drains its stream (at most the 30 s cancel drain,
  checkpoints refused) before it finalizes, so the requests settled
  meanwhile and the runtime's last history reach the final; a v1 run still
  ends at the error.

**As built (slice 6, part 2, dark).**
- **Off gate:** `runtime::ledger_gate_tests` pins, per case, the requests
  the provider receives (headers and body bytes), the runtime's event
  sequence and its client-wire lines against fixtures written at the
  pre-ledger base: a streamed Anthropic turn, its 429 retry and its
  overload stream retry; an OpenAI-compatible Chat Completions turn, its
  cut-stream retry and its terminal 429; and, driven at the transport, a
  Codex request and one with a resend and a stream retry (written at the
  code before the Codex hook, whose Codex transport is the pre-ledger
  base's). Without a ledger every case is byte-identical.
- **Who has a ledger:** a `Runtime` holds an armed `LedgerSlot` only when
  the launch of a run with a plan arms it, and the slot holds the run's
  ledger only once the run is a v2 run (a fallback to rollover off leaves
  it empty). The stream turn passes it to its requests (`ApiOptions`);
  keep-warm pings and the non-stream loop pass none. That every other
  route (sessions, oneshot, rpc, the agent command, keep-warm, the summary
  transports) has none rests on an audit of the call graph and on behavioural
  tests: a v2 run's runtime holds the ledger and a fallback's does not, a
  oneshot worker's stream carries no ledger event, and the off gate's
  requests and events are unchanged. The summary *stage* is not such a
  route: it books a v2 run's summaries with the turn's handle ("Rollover
  summaries" below), while the summary requests it sends carry none. A
  source scan (`g3`) is a lexical tripwire on top: it fails when another file names the calls that arm or
  fill a slot or sets the options' ledger, but it proves nothing about
  data flow (a copy such as `ApiOptions { ..old.clone() }` carries a
  ledger past it).
- **Anthropic Messages:** the attempt is reserved right before each send,
  after every local check, so only the send itself can fail after it. Its
  usage events are the attempt's (`UsageAttempt`); it ends measured by its
  final report, or open. The reports are cumulative (`message_start`, then
  `message_delta`), and the attempt settles only on counts they state:
  output on a `message_delta` (the start's is a running count, so a stream
  that ends before its delta stays open), input and cache reads and writes
  on the delta or else on `message_start`. An explicit zero is a measured
  zero; a count stated by neither leaves the attempt open. Settlement reads
  the stream's presence-kept capture, not the display emission, which
  zero-fills absent counts, falls back to the start's count for a zero and
  skips an all-zero report; the display events and legacy totals are
  unchanged, so for an explicit zero output after a nonzero running count
  the display shows the running count while the ledger has the zero. The
  conversation client follows redirects, so the
  route is `Unverified`: an error status and every transport failure,
  a refused connection included, leave the attempt open (the broker types a
  refused connection after a redirect the same way). A run at its cap fails
  the turn with a configuration error (`RuntimeError::Config`) carrying the
  reason's text.
- **Chat Completions:** one attempt per broker `proxy_stream` call. Usage
  reports are running totals (OpenAI sends one with `include_usage`; some
  compatible servers report on several chunks), so the attempt settles with
  its last report, and only when that report states both counts; a stream
  cut off before it completed, a report without the output count, an error
  status (on this broker-proxied route, a 429 included), a cancel or any
  other failure leave it open. It is released only on the broker's typed
  pre-send failure (`BrokerError::request_not_sent`). A run at its cap
  fails the request with the reason's text, marked not dispatched
  (`NotDispatched`) only when no earlier attempt of it was sent; after a
  sent attempt it is a plain error with the same text.
- **Codex (Responses over direct HTTP):** one attempt per HTTP send, so a
  request that `send_with_retries` re-sends (408/429/5xx, transport
  failures) and one the stream loop retries (no terminal event, a completed
  response without usable output) has one entry per send. Each is reserved
  right before its send. The client follows redirects, so an error status,
  a transport failure (a refused connection included) and a cancel leave
  the attempt open; nothing on this route releases. The Responses stream
  states a response's whole usage once, on its terminal event
  (`response.completed`, `response.failed`, `response.incomplete`, and the
  parser's aliases `response.done` and a top-level `error`); a repeated
  terminal report restates it, so the attempt settles with the last
  terminal event's report as sent, and only when that states both counts.
  An explicit zero is a measured zero. An absent count, an empty or
  count-less usage object, and a last terminal event without usage (absent
  or `null`) leave the attempt open: "last" is the last terminal event, not
  the last non-empty report, and no alias is exempt (none is known to carry
  no usage by design). So a completed response (also one retried for want
  of output) and a failed or incomplete one that reports its usage are
  measured; a stream without its terminal event stays open. The SSE
  `[DONE]` sentinel is not a JSON terminal usage report and does not replace
  the preceding report. The reply's own usage (`reported_usage`) still keeps
  the last non-empty report (unchanged here; on the follow-up list). It is
  not the ledger's evidence, and no summary settles from it: a Codex model
  is never a summary model (see "Rollover summaries" below). A run
  at its cap fails the request with the reason's text, marked not
  dispatched (`NotDispatched`) only when no earlier send of it was made.
  Without a ledger the requests, events and result are those before the
  hook: `runtime::ledger_gate_tests` pins an ordinary request and one with a
  resend and a stream retry against fixtures written at the pre-hook code
  (the route has no base-URL override, so the gate drives the transport
  directly), and `codex_ledger_tests` checks that a ledger changes no
  request byte.
- **Backstop:** `books_requests(model)` is true only for those three wires.
  A ledgered request on any other route (OpenAI Responses, Gemini Code
  Assist, the cloud providers, extension providers) is refused before
  anything is sent: "'<model>' can't book its requests in the run's request
  ledger; request not sent". Nothing reaches it while rollover is off.
- **Routes that can't book run with rollover off:** the launch checks the
  worker's model first (`books_requests`). A worker whose route can't book
  its requests runs with rollover off from the start, before any record,
  lock or directory, and its runtime gets no ledger, so a v2 run does not
  reach the backstop, which stays as the guard.
- **The caller is told:** every launch that falls back to rollover off (a
  route that can't book, a binding that can't confine retrieval, a run that
  can't be claimed, a busy lifecycle lock, a spawn head not published, ...)
  sets the run's notice "rollover disabled for this worker: <reason>": one
  line, control characters blanked, the reason cut to 200 characters.
  Status and collect show it as `rolloverNotice` while the handle lives;
  the completion event says it in its text (" Note: <notice>.") and as
  `rollover_notice` in its data. A run without a plan (worker rollover off
  everywhere) and a v2 run carry none, so their outputs are unchanged.
  Known limit: three surfaces read what outlives the handle and do not
  repeat the notice: the collect of a reaped handle (it reads its record),
  the `subagent_status` of a reaped handle (`archived_status` reads the v1
  archive), and the wake a reload carries for a stopped worker
  (`CarriedWorker` has no notice field).
- **Cloud routes are unbooked:** there is no cloud hook. Every worker
  request carries tools (a worker's registry is never empty) and all three
  cloud providers refuse tools (`supports_tools` is false), so
  `cloud_invoke_stream` refuses a worker's request in its pre-flight, before
  any broker or network I/O: a hook would never run. A cloud worker runs
  with rollover off (above) and fails at the pre-flight as today. A
  tripwire test fails if a cloud provider starts accepting tools while its
  route does not book requests; then add the hook, or confirm that the
  launch's check covers it, before enabling. The Vertex and Bedrock usage
  semantics stay on the follow-up list.
- **Rollover summaries:** a v2 run books each of its rollover summaries in
  the run's ledger as an attempt of its own, with purpose
  `rollover_summary` and the summary model as the entry's model
  (`RequestLedgerHandle::for_summary`). The stream turn hands its options'
  ledger to the summary's accounting (`SummaryAccounting.ledger`), and the
  summary booking runs in the summary stage itself. The summary transports
  never carry a ledger, and nothing depends on forwarding the summary's
  events. Every route sends a summary request at most once (a single
  attempt, on a client that refuses redirects), so one request is one
  entry. An unsent warm summary followed by its cold one makes two entries,
  the warm one released.
  - *Reservation:* right before the send, after the summary's own checks
    (model policy, admission, the turn's bounded allowance, the run
    deadline). The amount is what the summary reserves on the turn's
    counters: for cold, the request plus its output cap at the model's
    price; for warm, its worst case. It is a liability amount only. At the
    run's request cap nothing is sent, warm or cold. The turn's reservation
    is released, the summary is skipped with reason `run_full` ("Rolling
    summary skipped: this run reached 4096 provider requests."), and the
    rollover carries the previous summary forward, as after any skip.
  - *Settlement* uses the same typed evidence that settles the turn's
    counters:
    - Cold: a reported usage (input and output stated) is measured. A
      request not dispatched, or rejected before generation (the summary
      line's first-party no-charge classification), is released. An
      unknown spend, a timeout or a cancel stays open at the estimate, the
      native cold path included: it does not decide its body's withdrawal
      before settling (the known limit in §5).
    - Warm: a full report (all four counts) is measured. A reply known not
      dispatched is released. A partial report stays open. When the reply
      never came, the body's withdrawal decides: if the withdrawal won,
      nothing was sent and the attempt is released; if the body was already
      published, the attempt stays open. For the withdrawal case the ledger
      releases while the turn's counters keep their reservation (the
      inherited hold in §5's known limits); the ledger's release rests only
      on the withdrawal having won.
    - A summary dropped mid-call leaves its attempt open (an unsettled
      attempt's drop reports it open).
  - *Events:* a ledgered summary's report is its `UsageAttempt` (purpose
    `rollover_summary`) in place of the plain `Usage`, never both. The
    turn's counters book it as before, and the drive's legacy totals count
    only conversation attempts, so a worker's reported totals are
    unchanged. Without a ledger the summary's events are exactly as before
    (`sb5`).
  - *Cache counts:* the cold report fills in zero for a cache count the
    reply does not state (`reported_usage`). The native cold Anthropic
    request sets no `cache_control`, so it neither reads nor writes a prompt
    cache. A Chat Completions report without a cached count prices every
    input token at the full rate, never below. Chat keeps the last report
    with its presence, so it has no stale report. The warm report requires
    all four counts.
  - *A summary model that can't book* (its route is not one of the three
    booked wires, e.g. a cloud summary model) is not refused for that: its
    attempt is reserved and its report is still emitted as the attempt's,
    but the attempt stays open whatever it reports, since that route's
    report semantics are not established (`sb6`).
  - *No Codex summaries,* so no stale report reaches a summary. The Codex
    route has no enforceable output ceiling (`output_ceiling_field` has none
    for Codex Responses), so the summary policy refuses every Codex model
    (`no_enforceable_ceiling`), explicit or under auto. Warm summaries are
    Anthropic-only. So the Codex reply's last-non-empty `reported_usage` can
    never settle a summary, and there is no stale-report edge to close on
    the summary path. A tripwire (`sb7`) checks the policy and catalog side:
    every catalog Codex model resolves to Codex Responses, has no output
    ceiling field and is refused as a summary model. If that changes, settle
    a Codex summary from the terminal report, as the conversation hook does.
    `sb7` drives no transport call. As defence in depth, the transport
    refuses a single-attempt request with a ceiling on a route without an
    enforceable one as not dispatched (`openai/mod.rs`, the bounded route
    check), which the ledger would settle as a release.
  - Tests: `runtime::rolling_summary::ledger_tests` (`sb1`–`sb7`, cold) and
    `…::warm_tests::ledger_tests` (`sw1`–`sw7`: warm; one end-to-end test
    through a v2 runtime's stream, one `rollover_summary` entry next to the
    conversation entries; and a cold summary through the real routed
    transport whose endpoint refuses the connection, released). The
    no-ledger gate
    `…::warm_tests::gate_tests` (`summary_gate_*`) runs the session path,
    with no ledger, through two real streamed turns that roll over: a cold
    summary written, a cold one answered with a server error, a cold one
    whose endpoint refuses the connection, a warm one written, and an unsent
    warm one with its cold fallback. It compares the requests the loopback
    stand-in received (method, path, headers other than `host`, exact body
    bytes), both turns' events and their client-wire lines with fixtures
    written on the code before the summary booking (`e240e3e4`, twice,
    identical). The only normalization replaces run-specific identifiers
    with placeholders: a turn error's correlation ID, random UUIDs (host
    turn tags, the ephemeral session ID) and a window's 32-hex archive ID;
    body byte counts are taken before it.
- **Next:** slice 7 (summary and config wiring), built dark: §7.2 "As
  built".
- **Slice 8:** no per-request usage reaches a parent. Reactive workers,
  v1 and v2 alike, publish no usage to their parent, as today. A v2 oneshot
  publishes what a oneshot publishes today: one aggregate `Usage` when it
  returns `Ok` (a timeout-partial `Ok` included), with the same shape and
  timing, computed from its ledger, at most once per `run_id` (a retry or
  recovery never publishes twice; slice 8f is that guard and its test).
  As built (8f): one publisher per mode. In a host with a spend sink (the
  daemon: the oneshot holds a spend lease) the worker's tap books each
  request as it is made and is the only publisher: no aggregate is sent,
  v1 or v2 (`f5`). Without a sink a v2 run's aggregate is its ledger's
  measured conversation entries on the worker's model, summed
  (`ledger::conversation_aggregate`): a rollover summary (on any model), a
  conversation request on another model, an open liability and a released
  attempt add nothing, and a report delivered again counts once (`f1`). A
  request cut at the deadline is open in the ledger, so it is not in it: a
  planned run's timed-out usage is an unplanned run's (`o2`; before 8f the
  drive's totals counted that request). The oneshot sets its result's
  counts from the ledger once the run returns, so the parent's `Usage` and
  the log's Tokens line say the same (`f6`, a run that rolled over). The
  guard is `ledger::claim_aggregate(run_id)`, a check-and-insert at the
  one publisher (`oneshot::aggregate_usage`) over an in-process set (`f3`,
  `f4`). The run id reaches the publisher next to the result, not on it
  (`SubagentResult` keeps its shape, so a v1 worker's result reads as
  before: the visibility gate; `SubagentState` gains `v2_run`, so its
  Debug output gains that field), and `f6` checks that the run's
  aggregate was claimed. Limit: the mark is in-process only, and a
  process that restarts forgets it; slice 10's recovery, the first path
  that could publish a run's aggregate again after a restart, adds a
  durable mark. Until then the set gains one run id per v2 oneshot run per
  process and never shrinks; that growth is accepted. A run without worker
  rollover (no plan, or a fallback to rollover off) sends its drive's
  totals each time, as before.
  Open product question, outside rollover and for every worker: whether a
  worker's spend should count toward its parent session's totals and
  `max_cost_usd`.

## 7. Rollover, summary and target

### 7.1 Rollover

- Trigger unchanged: rollover band or hard capacity
  (`C/core/context_policy.rs:271-327`); `finish_rounds` after `plan`/`wrap_up`.
- `target_tokens` is an upper-footprint objective: mandatory retention can leave
  a successor above it (hard-stop if unfit, `continuation.rs:720-737`); little
  available history can leave it below. Seal errors fail at any boundary.
- Reuse the branch unchanged: summary-reserved target and stream wiring
  (`rs:E/runtime/stream.rs:1242`), one invocation; on skip or failure the previous
  summary is carried forward (bounded).
- Worker guidance and commit notice (`rs:E/runtime/stream.rs:1320`) drop "fresh
  wall-clock allowance".
- Resume tags its merged instruction message with a fresh `_synaps_turn`
  (`archive.rs:260-275`, `continuation.rs:1027-1050`).
- A resumed run restores its window and latest archive from the store its own
  binding seals into (`restore_window`), never from the process's
  working-directory scope. On a legacy binding a worker's store need not be
  the daemon's, and a window restored from the wrong store leaves the run at
  window 1: its first rollover marker then names a window its record refuses
  (§4, `check_marker`) and the run stops. A miss still restores nothing, as
  for sessions. Tests `restore_scope_tests::e1`-`e5`; a CLI binding is the
  process's store and restores as before (`e3`).

### 7.2 Worker summary-model policy

1. Snapshot the parent's live `context_management.summary_model` **setting**
   (`auto` or explicit), its compaction-model setting **as configured** (unset
   stays unset) and disclosure mode/exclusions, never the parent's resolved
   summarizer.
2. Resolve `auto` inside the worker after its model/provider/endpoint is set,
   in the session order: the **worker's own model** if its route can write a
   summary (so the warm path can reuse the worker's cache); else the inherited
   compaction model, resolved exactly as the parent resolves it (unset is the
   same default), with the reason said once; else skip visibly. A compaction
   candidate that is the worker's own model is not tried twice. The parent's
   model and the parent's resolved summarizer are never added as candidates.
   This is not an exclusion by id: a compaction model the parent configured
   equal to its own model is still the configured compaction candidate.
3. An explicit summary model permits only that model.
4. Capability chooses; availability does not re-choose. Capability is a
   property of the route and the credential source: extension-hosted
   (`ExtensionHosted`), no enforceable output ceiling (`NoEnforceableCeiling`),
   proxied by the remote credential broker (`RemoteProxy`). Only the chosen
   model's credential is then checked; if it is missing, skip with the
   existing `SummarySkip::MissingCredentials` and carry forward, never another
   model. Sessions keep their choice (a session's own model without a
   credential still falls to the compaction model).
5. Local-only mode and exclusions control; price against the selected model in
   the worker's ledger and budget.

**As built (slice 7, dark).**

- *Snapshot.* `Runtime::worker_context_policy()` copies the parent runtime's
  settings as configured (`E/runtime/mod.rs`): continuation config, summary
  model setting and its invalid flag, `compaction_model` as an `Option`,
  disclosure mode and exclusions, and the window override with the model it
  was set for. A stream turn takes it once; each tool call that can start or
  resume a worker gets it with the continuation settings read at that moment
  (`worker_context_now`, `E/runtime/stream.rs`), in `ToolCapabilities`'
  `worker_context`. Contexts without a stream turn (the non-streaming loop,
  hooks, extension routes, fixtures) carry none, so their workers get no plan.
  A resume takes a fresh snapshot, never its predecessor's.
- *Plan.* start and resume build a `LaunchPlan` only when worker rollover is
  on and the snapshot's mode is `auto` (`launch::wants_rollover`); otherwise
  the worker runs exactly as before. With `WORKER_ROLLOVER = false` no worker
  has a plan.
- *Resolution* (`WorkerContextPolicy::resolve`, pure,
  `E/tools/subagent/context_policy.rs`): the window is the parent's override
  when `same_model_identity` holds (an extension-hosted id only itself; a
  routed id by its resolved provider, endpoint and wire model, so
  `anthropic/claude-x` is `claude-x`), else the worker model's own. Thresholds
  that pass `validate_for_window` are kept. Otherwise pressure, rollover,
  target and summary size are reset to that window's defaults (reserve, finish
  rounds and summary timeout stay) and validated again: if they pass, rollover
  stays on and the caller is told "rollover thresholds reset to this worker's
  defaults: <reason> (window N tokens)"; if they fail, the launch goes Off with
  "rollover disabled for this worker: <reason>". Both reach the caller through
  the same `rolloverNotice` field (status, collect, completion); the reasons
  are the validator's fixed sentences plus a number, so the line is bounded
  and plain. The stream's silent fallback (thresholds its window can't
  resolve treated as unbounded with no summary) is unreachable for a worker
  with inherited settings: every resolved context resolves for its window
  (pinned by `no_worker_context_has_thresholds_its_window_cannot_resolve` and
  `thresholds_that_dont_fit_never_reach_a_workers_stream`). Then, when the
  snapshot has a resume refusal line (`subagent.resume_refuse_tokens`, 250k
  by default; `off`: no clamp), the rollover trigger is clamped below it
  (`clamp_below_resume_refusal`, §12 item 9), so a worker that ends a run
  near its trigger can still be resumed: the trigger is at most
  `refuse - (1 + finish_rounds) x reserve_tokens`, because the round that
  crosses the trigger and the finish rounds allowed after it must fit too
  (250k - 3 x 16k = 202k with the defaults). A trigger at or below that cap
  is kept. Otherwise the trigger becomes the cap; a pressure threshold not
  below it becomes 7/9 of it (the defaults' 140k:180k ratio, so the worker
  keeps an advisory band: 157,111/202,000 on a 1M window), an explicit one
  below it stays; a target or summary size that no longer fits goes back
  to its default; the result is validated again. When that fails, or the
  headroom leaves no cap of two tokens, the launch goes Off with
  "rollover disabled for this worker: <reason>". A clamp gives no notice:
  it is a default, like the window's own thresholds. The session path
  is unchanged.
- *Application.* `Runtime::apply_worker_context` is the one function that
  applies it: window override, continuation config, compaction model as
  configured, summary model setting (or its invalid flag), disclosure mode and
  exclusions, and the worker summary choice. The launch passes the resolved
  context to the runtime's start closure only for a v2 run (`Launch::On`);
  any fallback to rollover off starts the runtime with nothing inherited. It
  is called only from `Runtime::start_worker_run`, which first binds the
  runtime's continuation to the run (8c, §8), and that only from the start
  closures in start, resume and the oneshot's `run_planned` (a lexical
  tripwire, `w7`).
- *Choice* (`worker_summary_model_choice`): items 2–4 above. The capability
  half is `summary_route_capable`, split out of the session's
  `summary_model_unusable` without changing the session result.
- *Oneshot*: its run with a plan gets the same application through its own
  start closure in `run_planned` (slice 8c, §8).
- *Tests.* M: `m1_m5_…`, `m6_…`, `m7_…` (summary booked at the chosen model,
  through the stream), `m8_m9_…`, `m11_…` and the thresholds test in
  `runtime::rolling_summary::…::warm_tests::worker_tests`; m9/m10/m12 and the
  copied-as-configured cases in `tools::subagent::context_policy::tests`; the
  launch cases `w1`–`w7` (no plan inherits nothing; a v2 run starts with the
  resolved settings; a reset keeps rollover on with its notice; settings that
  can't resolve go Off with nothing written; any fallback inherits nothing;
  no plan while worker rollover is off; the tripwire). The clamp below the
  resume refusal: `c1`–`c5` in `tools::subagent::context_policy::tests`
  (the 1M numbers, an explicit trigger clamped and one below the cap kept,
  no clamp with the line off, Off when there is no room, every clamped
  shape resolving for its window below the line) and
  `the_resume_refusal_line_reaches_the_worker_snapshot` in `runtime`.

## 8. Visibility

Counters `window_ordinal` (lineage-wide), `rollovers_this_run`,
`inherited_windows` appear in status, collect, completion and records. The
drive and oneshot route `SessionEvent::Notice` (dropped today, `drive.rs:198`)
into a bounded ring (8 × 512 bytes, UTF-8-safe truncation) plus
`SubagentUpdate`; terminal uncertainty/error is kept in typed state outside the
ring; one-time skips are deduplicated by typed reason. Steering queued during a
rollover reaches the new window (`stream.rs:792`).

**As built (slice 8a, dark).** Reactive workers (start and resume); a
oneshot's run with a plan shares their drive (8c, below).
- **State:** `SubagentState::visibility` (`E/runtime/subagent.rs`), a
  `WorkerVisibility` of `WindowCounters` and a notice ring of
  `NOTICE_RING` = 8 notices. `bounded_notice` makes each notice one plain
  line (control characters and line or paragraph separators become spaces,
  the ends are trimmed, the rule of the worker manifest's labels) and then
  cuts it to at most `NOTICE_BYTES` = 512 bytes at a char boundary (with a
  trailing `…`); the oldest is evicted first and counted. A notice with
  nothing left once flattened is skipped: it takes no slot, evicts nothing,
  sends no update and leaves the progress line. Set from the spawn
  head when a launch goes on; `None` for a v1 run, a fallback to rollover
  off, and always while worker rollover is off.
- **Counters:** from the run's last durable head
  (`WorkerPersistence::windows`, `persist::window_counters`):
  `window_ordinal` (the active window, lineage-wide), `rollovers_this_run`,
  `inherited_windows` (a count). The drive refreshes them after every
  checkpoint, so a refused or undecided head counts nothing. A reaped v2
  run's collect reads them from its final; its notices are not kept in
  records.
- **Notices:** the drive of a v2 run (one with record persistence) keeps
  each `SessionEvent::Notice` in the ring and sends it as a live
  `SubagentUpdate`, at most one a second (slice 9 (b), `drive::NoticePace`):
  the first at once; one that comes sooner after the last is held, a later
  one replacing it (the latest wins), and goes out once its second is up,
  on the drive's timer arm, or when the drive ends, before the run's
  outcome. During a stop's drain (the deadline, a lease stop, an error)
  the timer arm is gone: a held notice waits for the drain's end (up to
  `CANCEL_DRAIN`, 30 s, for a reactive worker; `ONESHOT_DRAIN`, 500 ms,
  for a oneshot) and goes out before the result. Nothing waits on the
  consumer: the send is the unbounded progress channel's, and at most one
  notice is held, so a burst to a
  stalled consumer queues at most one update a second (`v9`, a thousand at
  once: two; `v10`, the trailing flush while the worker runs on). A v1 run
  drops notices, as before. Like other progress, a sent update also
  replaces the worker's latest progress line in memory (`note_progress`);
  nothing is written to a record per notice. One-time
  skips are deduplicated at the source: the runtime's `SummaryNotices`, per
  (skip reason, displayed model), once per worker runtime.
- **Surfaces:** `subagent_status` and `subagent_collect` (running, final)
  add `rolloverWindows` and `rolloverNotices` (and `rolloverNoticesDropped`
  when the ring evicted any); the completion event's data adds
  `rollover_windows`, `rollover_notices` (and `rollover_notices_dropped`).
  Typed terminal state stays outside the ring: `rolloverNotice`,
  `archiveError`, the terminal cause and a failure's error.
- **Evicted windows** (B31 follow-up): a resume whose admission found listed
  windows evicted (§4) tells its caller how many, never which: the tool
  reply's `warning` adds "N earlier context window(s) of the resumed worker
  is/are no longer in memory; it continues from its summary" (after a crash
  checkpoint's warning, before a short timeout's, joined with "; ");
  `subagent_status` and `subagent_collect` (running, final) add
  `evictedWindows`; the completion event's data adds `evicted_windows` and
  its text a " Note: N earlier context window(s) was/were no longer in
  memory."; the daemon's own resume of a reload-stopped worker carries the
  count and the note in its `subagent_resumed` wake. Set when the resumed
  run's state is created and kept whether its run goes on with rollover on
  or falls back to off (`LaunchPlan::evicted_windows`). Absent at 0, so every
  other run's surfaces are unchanged; not kept in records, so a reaped run's
  collect and a carried worker's wake show none. Tests: `v11`, resume
  `b31a_*`, finalize `*_counts_evicted_windows_only_above_zero`.
- **With worker rollover off:** v1 workers and every new worker are
  unchanged (no plan, so no visibility and no new keys). A reaped v2 worker
  whose final was written while rollover was on keeps its v2 counters
  (`rolloverWindows`) in collect, read from that final; that is the one
  surface where the new keys can appear with the switch off.
- **Tests:** the historical gate `visibility_gate` (a v1 worker's updates,
  state, result, status, completion, and its collect while running, once
  finished and after its handle was reaped, with notices, tools, usage and
  a refused checkpoint in its stream; fixtures written twice at 2ad187ce
  with no difference; red when the drive turns a notice into a progress
  update, and when a key is added to the running or the reaped collect
  body), and `v1`–`v8` in `tools::subagent::launch::tests::visibility` (the
  ring's bounds; notices reach status, collect and completion; typed state
  survives eviction; counters follow durable heads and the lineage,
  distinct; a reaped final's counters; a fallback shows nothing; a notice
  with controls and separators is the same plain line in the ring, the
  update, the progress line and every surface; a blank one is skipped).

**As built (slice 8c, dark): oneshot parity.** `subagent`
(`E/tools/subagent/oneshot.rs`) reads the caller's `context_rollover` as
start does (only when worker rollover is built in) and plans its run as
start does: `wants_rollover` and the same `LaunchPlan` (its handle is its
orchestration ID `sa_<n>`, its records go under the same root and process
directory, its meta is its label, model, system prompt and timeout). The
plan is decided in `SubagentTool::dispatch`, which `execute` calls with the
production decision (a plan only when the run wants one, so never while
worker rollover is off); tests give a run a plan there.
- *Without a plan* the oneshot's own loop runs as before. Its timed-out
  result is now worded by `timed_out_text`, which the drive shares; the
  historical gate proves the loop unchanged.
- *With a plan* `run_planned` sets the runtime up as before, then runs the
  shared launch, records and drive (`launch::run_worker`): one
  `WorkerCheckpoint`, the spawn head before the first inference, the final
  after, the ledger and the parent's settings only once the run is a v2
  run. On `Launch::On` the start closure (start's, resume's and the
  oneshot's alike) calls `Runtime::start_worker_run`, which binds the
  runtime's continuation to the run and then applies the parent's
  settings: `reset_context_continuation` with the run's
  `worker_logical_id`, so every head it checkpoints carries that ID and
  every window it seals is sealed under it; a fresh state (new epoch; no
  note, phase, advisory, warm record or calibration of an earlier
  binding; window and latest archive only from the opening's marker; the
  warm generations kept); the archive scope stays the run's memory
  binding. A v1 run and every fallback are not bound. One clock:
  `arm_run_clock` arms the run clock from the oneshot's
  `timeout` (the deadline is the earlier of start + timeout and start + the
  worker's `max_elapsed`, §5), and nothing else times the run. The drive
  words the result as the oneshot's own loop does (`DriveShape::Oneshot`;
  start and resume use `Reactive`, unchanged): responses concatenated as
  they came, a timeout as `timed_out_text` ("[TIMED OUT after Ns — partial
  results below]", the tool log, "[partial response]:" and the text; N is
  the run's limit), a provider error as "<message> [<category>]". The
  oneshot then returns, publishes and logs its result as before: one
  aggregate `Usage` on `Ok` (a timeout-partial `Ok` included; none in a
  host with a spend sink, whose tap books each request) from the run's
  ledger, at most once per `run_id` (8f, §6 "Slice 8"), and one
  `SubagentDone`. A caller that drops the
  oneshot before its run ends marks the run cancelled and cancels it; its
  final says `cancelled`. The call's drop guard (`oneshot/reap.rs`, as for
  a oneshot without a plan) ends its handle once the worker thread has
  ended, exactly once, as Failed ("parent turn cancelled"), publishing
  nothing, so the parent admits new work again (`o8`, `o10`). While the
  run's final record is being written its reaper keeps waiting past its
  30 s bound (`r5`); once the write has outlasted the run's own bound, or
  when no reaper thread can be started, the guard hands the end to the
  worker's thread, which ends the handle as Failed once the write has
  (`r5i`, `r7`, `o12`). One atomic state (`WorkerEnd`) makes the end
  exactly once across the caller, the reaper and the worker's thread
  (`r8`). Only a dropped call with no record write in flight keeps L's
  inline end when no reaper thread can be started (`r9`). What else
  differs from a run without a plan is
  the drive's: after its deadline or a provider error it stops the worker
  and drains it at most `ONESHOT_DRAIN` (500 ms; a reactive worker's
  drain is 30 s, its caller is not waiting), so the result still returns
  on time, and it counts the usage reported meanwhile. So a timed-out
  run's one `Usage` and its log's `Tokens:` line include what the cut
  attempt reported, as the run's ledger does, where a oneshot without a
  plan counts nothing after its timer (deliberate: the ledger's number is
  the true one; `o2`).
- *Deadline inside a rollover:* the slice-5 rule (§5): the summary waits
  at most until the run deadline and is never adopted after it (`summary_reason`
  `failed`, not `run_deadline`, which means expiry at the seal); the run
  times out at that deadline (the drive's and the runtime's are one
  instant) and the oneshot returns its timed-out result, with the earlier
  window's text, within its timeout and a second (`o6`), long before
  `summary_timeout_secs`. The one wait past the deadline is a head write
  already in flight when it passes: the run waits for that write (the
  durability barrier, at most `BARRIER_WAIT`) before it returns.
- *The final record after the deadline:* the run's terminal publication
  (the final, its directory sync, the head's removal) is the worker's own.
  Its caller waits for it at most `TERMINAL_GRACE` (800 ms) past the
  deadline, at least `TERMINAL_MIN` (100 ms) once the drive has ended
  (after a head-write barrier the grace has passed), and at most
  `BARRIER_WAIT` when the run ended before its deadline or has none
  (`launch::terminal_bound`). A publication still running then goes on
  detached: the worker's thread drives it to its end after sending the
  result, and the run's state takes its outcome when it ends. Nothing
  says it failed while it may still land: the handle stays pending
  (unreconciled, so it holds its slot and is listed as a pending worker)
  and the head stays the crash checkpoint until the write ends. The
  caller hands the end to the worker's thread (`DropReap::hand_over`),
  the one lifecycle that knows when the write has ended: its last act
  (`WorkerEnd::exit`) ends the handle once, with the run's terminal, and
  logs one WARN line if the write is still going after 30 s. Nothing
  waits on a second thread, so no failure to start one can end the handle
  early. The run's `Usage` and then its `SubagentDone` still go out with
  the result, once each (`o11`; `t1`–`t4` and `t7` for a held final
  rename, directory sync, head removal and temporary write, and a final
  written in time; `t6` the bound).
- *Surfaces:* a oneshot has no status, collect or completion event; what
  it shows besides its output follows the output in its result
  (`rollover_trailer`), and so in its log and, when the output is short,
  in its `SubagentDone` preview. Nothing unless something happened: a run
  with a plan that did not fall back, had no thresholds reset, made no
  rollover and kept no notice reads exactly as a run without one.
  Otherwise a blank line, then one line each, in this order:
  `[rollover]: <notice>` (the fallback or thresholds-reset notice),
  `[rollover windows]: window <W>; rollovers this run: <R>` (only when R >
  0; a oneshot is never resumed, so it inherits nothing),
  `[rollover notice]: <notice>` for each kept notice, oldest first, and
  `[rollover notices dropped]: <D>` (only when D > 0): at most 8 notices of
  at most 512 bytes and three other lines. The shared drive also sends
  the kept notices live as `SubagentUpdate`s, paced as for reactive
  workers (at most one a second, the latest winning).
- *What the injected tests could not catch:* the launch, persist and
  drive tests give the drive its stream and inject `ContextHeadCheckpoint`
  events carrying the right session ID. Two gaps of the real path showed
  only when the parity tests ran the real runtime: a worker runtime's
  continuation kept its `ephemeral-<uuid>` logical ID, so its first
  rollover head was refused ("not this run's logical ID") and its window
  sealed under the wrong ID (fixed here for all three entry points; `w8`,
  `starting_a_worker_run_…`, and `o5`/`o7` check the sealed window's ID
  and a resume's lineage check of it); and a worker registry holds no
  memory tools, so a fresh run in `auto` failed closed at stream start
  (`o9`; 8g gave it owned history, §4 "As built (slice 8g)", with
  end-to-end tests per entry point).
- **Tests:** the historical gate `oneshot_gate` (the real `subagent` tool
  against a loopback Anthropic stand-in: a finished run with a tool call,
  a timed-out run, a provider refusal; pins the provider requests (method,
  path, headers but `host`, exact bodies), the parent's events, the result
  and the log file, normalizing only the subagent ID, durations, the log's
  timestamp and a turn error's correlation ID; fixtures written twice at
  0c7f010b with no difference; red when the timeout text, the worker's
  system prompt, the tool update or the log's tool line change);
  `o1`–`o9` in `tools::subagent::oneshot::oneshot_parity` (finished,
  timed-out and refused runs with a plan return the same result, events
  and log as without one, with the spawn head on disk before the first
  request and the final after; a fallback says why after the output and
  writes nothing; a rollover keeps the earlier window's text and shows its
  windows and notices, one usage event, one completion; a deadline inside
  the summary times out in the oneshot's words within the deadline, the
  summary never adopted; a provider error after a rollover; a parent drop
  during the summary ends the run cancelled in its first window, nothing
  sent or adopted after, no usage and no completion published; and `o9`,
  a fresh planned run on the real worker registry, which holds no memory
  tools, failed closed at stream start before any request, its history
  unchanged, until 8g flipped it: it now runs on its own owned-history
  tools and answers). Until 8g the parity cases gave the planned worker
  owned-history tools over an empty grant through a test-only seam
  (`RecordSite::test_grant`); 8g removed it, and the cases run on the
  real worker registry; `o10` (after a dropped run with a plan has ended, the parent
  admits the next oneshot under a concurrency limit of one) and `o11`
  (a final record's write held past the deadline: the timed-out result on
  time, its usage then its completion once each, the handle pending until
  the write lands, then ended once, the final published once); `o2`
  compares the usage event with only its numbers blanked and checks its
  model and TTL split itself (a model-only change goes red); the launch's
  `t1`–`t7` and the guard's `r5`–`r9` (`oneshot/reap.rs`, with a
  test-only fault that fails the reaper's thread start: `r5i`, `r6`,
  `r7`, `r9`, and end to end `o12`; with `r1`–`r4` and the dropped-call
  tests `d1`–`d4` from the fix for a dropped oneshot without a plan, `d4`
  now waiting for the worker thread's last act instead of a fixed sleep);
  `the_trailer_says_nothing_unless_something_happened`; the drive's
  `a_oneshot_drive_*` (with `…_returns_on_time_when_its_worker_does_not_stop`);
  `w8_a_v2_runs_runtime_starts_bound_to_its_run`;
  `starting_a_worker_run_binds_its_runtime_to_the_run_and_nothing_else`
  (`worker_tests`); `w7` found the three start closures calling
  `start_worker_run`, and `apply_worker_context` called only there (since
  8g: only `start_v2_runtime` calls `start_worker_run`, and only the three
  start closures call it).

## 9. Storage limits and reload

- Shared project store. Initial worker ceiling: 64 of 128 legacy slots and a
  worker byte ceiling (128 MiB of 256 MiB), constants for now
  (`MAX_WORKER_SEGMENTS`, `MAX_WORKER_BYTES` in `C/core/context_archive.rs`),
  not guaranteed. Enforced atomically under the store lock, counting worker
  records including tombstones and orphans, after the digest dedup (a retried
  seal takes no slot) and before anything is written; resumed refs take no new
  slot. A record's class is set at seal and carried in its segment id (UUID
  version 8 for a worker segment, version 4 otherwise), so the stored record
  format is unchanged and an older binary reads a worker segment as an
  ordinary one. A refusal is an archive failure (§7.1): the worker keeps its
  history and window. No automatic eviction. Axel limits are verified
  separately.
- Active and unresolved run directories are protected from age pruning (owner
  lock, §3.3); record size is capped (lineage ≤ 128 refs, history ≤ the window).
- Reload: bounded drain, then coordinated cancel and checkpoint/finalize. Normal
  reload requires a durable terminal or recoverable record and quiescent writers
  for every worker; otherwise it is refused. `--force` waives preservation and
  reports it. No automatic tool replay.
- Reload check (8e, `E/daemon/worker_check.rs`). It covers the v2 runs this
  process owns, and only those: the writer table (`record_store`) has one entry
  per owned run for as long as its ownership lives, an orphaned write's
  included. A run that ended released its ownership, and the exec changes
  nothing it left on disk. Each entry's verdict:
  - durable: a committed head or the final, and no record write in flight. A
    pending report, a head removal after a durable final, and an uncertain
    later head (the disk holds it or its predecessor) all count as durable.
  - publishing: a head, republication or final being written ("a record write
    is still in flight").
  - starting: no committed spawn head yet ("its spawn record is not written
    yet").
  - unresolved: the disk head matches neither candidate ("no durable record:
    its publication is unresolved").

  Three steps:
  1. Before anything is quiesced, an unresolved run refuses a normal reload at
     once, so no turn pauses.
  2. The drain counts a publishing or starting run as busy, within its cap.
  3. After the drain and before anything is stopped, writes still in flight get
     up to 5 s to land (`WORKER_RECORD_WAIT_SECS`; an owner's one retry of an
     uncertain final lands inside it). Whatever is still not durable then
     refuses. A reload's time to its checkpoints is at most the drain's cap
     plus those 5 s, outside the reload fence's worst case.

  A refusal stops nothing. The CLI exits 3 with one stderr line, "reload
  refused: refused: worker records are not durable: sa_3 (session s, a record
  write is still in flight), ...; the daemon keeps running this image; no
  worker was stopped and any paused turn resumes in place; retry, or reload
  with --force to go ahead without them" (at most 8 runs named, then "and N
  more"). The daemon logs one WARN line. After a refusal at step 3, every
  session's quiesce lifts, and each session lifts its own: the daemon tells
  them all at once (`ReloadRefused`) and lifts a quiesce itself only for a
  session that command cannot reach within 5 s (gone, or its queue full),
  which keeps working while a turn the drain paused there stays paused. So a
  turn whose end the session handles before the refusal still finds the
  quiesce up and gets its marker. A turn the drain paused at a round boundary
  resumes in place with one message ("resume: a daemon reload was refused
  (HH:MM UTC) after your turn paused at a round boundary; nothing else
  changed; continue.") and no second marker, unless something re-prompted it
  since. A turn whose stream stopped for the reload just as the refusal
  arrived (the stream marks its stop in the session's event queue) resumes
  the same way at its end. `--force` goes ahead without those records: stdout
  is unchanged, stderr gets one line "reload forced: worker records not
  preserved: ...", `--json` adds `"waived": [{worker, session, why}]`, and the
  parent's wake for such a worker says its record was not preserved. A final
  that misses the stop bound after the check passed is logged with the head
  generation that stays the recoverable record. Known gap, fixed in slice 10:
  nothing checks again after the checkpoints (`checkpoint_all`), so a
  spawn-head or head write that starts after step 3's wait (`settle`) can
  still be in flight at the exec, neither waived nor logged.

## 10. Answers to open questions (adopted)

Shared store with worker quotas; v2 written, v1 accepted, no downgrade; head at
rollover plus terminal/reload checkpoint (not a current-round guarantee);
parent visibility unchanged, prefixes are not an ACL; no window-size cutoff;
window override only for the same model identity; bounded drain with refusal on
any undurable required record unless forced.

## 11. Test obligations

- **R (slice 1):** `r1_v2_round_trip_and_v1_legacy`, `r2_final_supersedes_only_le_generation`,
  `r3_newer_head_than_final_unresolved`, `r4_head_only_crash_checkpoint_warns`,
  `r5_final_only_resolves`, `r6_mismatched_run_or_handle_unresolved`,
  `r7_digest_or_kind_mismatch_unresolved`, `r8_writer_may_publish_unresolved`,
  `r9_unreadable_head_unresolved`, `r10_v1_final_with_v2_head_unresolved`,
  `r11_lineage_cap_and_canonical_mapping` (plus duplicate, self-lineage and
  ordinal/counter cases), `r12_binding_expectation_enforced`,
  `r13_ledger_settlement_idempotent` (plus release lifecycle, cap boundary and
  no mutation on rejection), R14 `c1` golden vectors and digest stability; strict
  unknown-field, malformed-field, pair-direction, writer-precedence and v1
  policy cases.
- **F:** temp-write failure saves latest history at g; rename-ok/fsync-fail is
  uncertain then republished; receipt loss blocks inference and finalization;
  crash after rename recovers the head; final rename/fsync ambiguity keeps the
  head, and a final selected by the resolver after rename-ok/fsync-fail is
  durably republished before it is acknowledged or the head is deleted; head-delete failure harmless; `f7_late_rename_after_timeout_is_not_overwritten`.
  Assert zero successor requests before a durable ACK.
- **D:** no elapsed reset; no time-checkpoint rollover; min of both deadlines;
  barrier on deadline during persistence; cancel at seal/summary/queued/
  post-rename/post-ACK (timeout 0 too); drain usage booked.
- **A:** sibling/session/project/note IDs rejected (legacy and Axel); filter
  before limit and snippets; omitted source/user scope rejected; full lineage
  with mode off; foreign/tombstoned refs refuse; refs absent from a successful
  inspection (older, latest, all) are left out of the grant with one worker
  notice, all absent gives an empty owned grant, a failed inspection refuses,
  and the successor resumes again; resume from head and from final
  after two rollovers and restart; in-root symlink rejected; older Axel refuses.
- **C (≥3 windows, rollover on and later off):** task, resume and steering
  constraints retained or retrievable; attachments and tool groups intact; no
  external-action replay; small/unknown windows stop cleanly; settles at or
  below target when feasible.
- **L:** per-model ledger on error/cancel/drain/panic; Error followed by Usage;
  same-model summary attributed by purpose; duplicate delivery; unknown spend;
  producer drop.
- **V/G/O:** no per-request usage reaches a parent; a v2 oneshot's one
  aggregate `Usage` is published at most once per `run_id` (§6 "Slice 8";
  an enablement prerequisite);
  notices reach status/collect/completion and survive ring eviction;
  counters distinct; self/siblings do not defer; terminal-after-release child and
  grandchild under a released edge defer; session gate unchanged; oneshot
  parity; opt-out/unknown values on all three tools.
- **P** (fake transports, real bodies; Anthropic, OpenAI, ChatGPT-Codex, Gemini,
  Kimi, xAI, Muse, OpenAI-compatible; start, resume and oneshot): host keys
  stripped; second-window inference; `p3_selected_summarizer_route_ceiling_or_visible_skip`
  (a Codex worker still summarizes through an eligible compaction model);
  single attempt and usage booked.
- **M:** auto takes the worker's own eligible model; ineligible worker model +
  eligible compaction; both ineligible ⇒ skip; explicit ineligible ⇒ skip;
  selected model without credentials ⇒ `MissingCredentials`, no re-choice;
  parent A / worker B never tries A unless A is the configured compaction
  model; configured compaction C priced per model; local-only/exclusions
  inherited; window override not inherited across models; thresholds that
  don't fit reset with a notice or turn rollover off.
- **S:** concurrent quota admission at the boundary, retry dedup, tombstones,
  orphans, byte ceiling; active directory survives 14-day pruning; reload drains
  and finalizes; reload refuses on any undurable record without force.

## 12. Slices (workers stay off until slice 9)

1. **Dark schema + pure resolver** (`E/tools/subagent/record.rs`): v2 record,
   run ID, digest, lineage refs, ledger identity types and idempotent
   settlement, `resolve` over supplied candidates; R* tests. No change to live
   archive writers, worker launch, collect or resume; no filesystem discovery.
2. Completion-lifetime ancestry and owner-scoped gate (G*).
3. Lineage validation, path rules, owned-history retrieval, Axel capability
   negotiation (A*), dark.
4. Durability: owned writer, ownership lock, publication states, spawn head,
   terminal drains, reactive finalize/collect/resume via the resolver (F*).
5. Deadline policy (D*).
6. Accounting and event transport: request id/purpose, `UsageLiability`,
   ledger-slot reservation before dispatch (L*).
7. Summary/config wiring: snapshot, summary-model policy, thresholds (M*).
8. Visibility and notices; oneshot parity; coordinated reload; quotas; a v2
   oneshot's aggregate usage published at most once per `run_id`, no
   per-request usage to a parent (§6 "Slice 8"; V*, O*, S*). Split into 8a
   visibility, 8b opt-out, 8c oneshot parity, 8g a fresh run's owned
   history, 8d storage, 8e reload, 8f parent publication, in that order,
   each dark and reviewed on its own; a part that touches a live path
   starts with a historical gate of that path without worker rollover.
   8g (after 8c, so the later parts are tested through the first real
   end-to-end path): a fresh v2 run's registry (start, a resume without a
   grant, the oneshot) carries owned-history `memory_search` and
   `memory_fetch` over its own lineage only, with a grant that grows as
   its windows seal, never the project-wide builtins; a v1 run's registry
   unchanged (a gate). Before 8g such a run failed closed at stream start
   (§1, "naive enablement fails closed"; `o9`, §8). Tests: end to end
   through `spawn_runtime` on the real worker registry, no seam; a
   retrieval that cannot reach the parent's or another worker's history;
   `o9` flipped. Built: §4 "As built (slice 8g)". 8f (after 8g): a v2
   oneshot's aggregate `Usage` from its ledger's measured conversation,
   at most once per `run_id`, and none in a host with a sink. Built: §6
   "Slice 8" (as built, 8f).
9. Enable on start, resume and oneshot together only after **all** R/F/D/A/C/L/
   V/G/O/P/M/S obligations pass, including older Axel. Also before enabling:
   a launch that falls back to rollover off gives a bounded caller-visible
   notice ("rollover disabled for this worker: <reason>") in status, collect
   and completion, not only a tracing line (built dark in slice 6, §6; check
   it end to end at enablement). And the outer-drop test: the real
   `prepare_rollover` driven through its enclosing rollover/stream future,
   warm and native cold, held at the post-seal pre-publication body seam,
   then that outer future dropped (not its token cancelled, not a summary
   wrapper dropped) and the connection task released: zero body bytes, no
   successor head checkpoint or commit, the active window and head
   unchanged, no late summary adoption (an existing seal follows the
   recovery rules; it is not asserted absent); a published-first control
   stays sent; reservations kept or released by the existing policy, no
   refund added. And bounded live notice updates: each notice of a v2 run
   went out over the worker's unbounded progress channel, so a burst with
   a stalled consumer queued one update per notice (the ring itself stays
   bounded). Built ((b)): the drive coalesces them, at most one a second,
   the latest winning, with a trailing flush on its timer arm, without
   blocking checkpoint or usage processing behind the consumer (§8
   "Notices"; `v9`, a large burst with a receiver that does not drain;
   `v10`). And a worker's
   rollover trigger below the resume refusal: `subagent_resume` refuses a
   worker whose measured request is above `subagent.resume_refuse_tokens`
   (default 250k provider tokens) and has no exemption, while a parent may
   roll over later (around 300k), so a v2 worker that inherits its
   parent's thresholds could end a run between the two and then be
   refused on resume. `WorkerContextPolicy::resolve` clamps the worker's
   resolved rollover trigger below `resume_refuse_tokens` with
   (1 + `finish_rounds`) x `reserve_tokens` of headroom, not one round's:
   the finish rounds allowed after rollover is due must fit below the line
   too (250k - 3 x 16k = 202k with the defaults); a pressure threshold not
   below the clamped trigger becomes 7/9 of it, and a clamp gives no notice
   (as built: §7.2 *Resolution*), with a test. And a resumed run's window
   restore on a legacy binding reads the store the run's own binding seals
   into, not the process's working-directory scope (as built: §7.1). The
   terminal publication of reactive runs (§5) is done in 8e: the reload
   check refuses on one in flight, and reactive runs keep their inline
   publication (§5 says why not the detached one).

   *As built (slice 9).* `launch::WORKER_ROLLOVER` is `true`: a start, a
   resume and a oneshot whose parent's live context management is auto,
   with no opt-out (§2), get a plan at their production sites (start and
   the oneshot: `RecordSite::live`; resume: the archive root it resolved
   its predecessor in), and a turn hands its tool calls the parent's live
   worker context policy (`Runtime::run_stream_tracked`'s snapshot, and
   `Runtime::daemon_tool_context` for the daemon's own resume). The live
   sidecar must offer `history_owned` (Axel's capabilities): until the
   sidecar is built from this source, every worker on the Axel binding
   falls back to rollover off with the notice (`n1`-`n3`), which changes
   nothing else for it. One consistency fix: a resume
   admitted with a grant runs on its owned registry, without the tools the
   operator disabled; when that is `memory_search` or `memory_fetch` its v2
   run used to fail closed at its stream's start (automatic context
   management needs both), while a fresh lineage fell back. Both now fall
   back before anything is written, with the same notice:
   `arm_owned_history` arms the operator's disabled tools for every plan,
   and `owned_history` checks them before it keeps an admitted grant (`g7`,
   `e5`). The end-to-end tests run the tools' production entries
   (`execute`) against the loopback stand-in: `e1` (a start rolls over and
   retrieves its own window at the live site), `e2` (a resume continues its
   lineage on the default legacy store and retrieves both windows: item 9's
   restore fix), `e3` (a oneshot's usage is published once, by its ledger's
   aggregate without a sink and by the tap with one), `e4` (the notice
   cases `n1`-`n3` through the production entries), `e5` (the opt-outs; the
   disabled tool on a granted resume), `e6` (the switch and
   `wants_rollover`; a route that can't book stays `lr1`'s fallback).
10. Later: recovery discovery; typed watch-journal entry.
