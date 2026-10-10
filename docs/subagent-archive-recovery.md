# Reactive worker finalization and archive recovery

Managed start/resume workers remain publicly `running` until their stream has
stopped (including timeout cancellation draining) and the finalizer has attempted
to save their full message checkpoint. Partial output can still be inspected.
Raw internal terminal status is not permission to collect, reconcile, resume, or
reap. `finished_at`, completion wakes, TUI completion, and the result channel are
published only after this recovery outcome. Legacy handles without archive
metadata retain their old status/text-resume behavior.

Successful archives release the in-memory checkpoint. On write failure,
`subagent_status` and terminal `subagent_collect` report `archiveError`; the full
checkpoint remains in memory. Resume uses that checkpoint, never a text-only
substitute. If no full checkpoint exists, resume returns an explicit error.
Start workers seed the original opening user message at launch; resumed workers
seed their full opening history (including prior messages and new instructions).
Both retain these seeds if runtime initialization fails or panics before the
first runtime history update. These are in-memory seeds, not crash checkpoints.

After fixing disk space/permissions, call collect or resume on the original
handle to retry its archive. Retries use the same handle filename and are
serialized by the registry lock. Disk I/O during these explicit retries can
delay other registry operations; retries are not background work. If the retry still fails, resume can continue
from memory, but the original handle remains protected until it is saved.
Missing archive metadata requires maintainer recovery; it does not discard the
checkpoint. There is no automatic background retry queue or persistent fallback
store.

**Retention exception:** cleanup, including collection/reconciliation cleanup,
must not discard an unsaved full checkpoint. Such handles can exceed the normal
15-minute retention period and consume memory for the session lifetime. Repeated
storage failures can accumulate retained histories; repair storage promptly and
retry collection on affected original handles. This is an accepted short-term, unbounded retention exception, not a
memory-budget guarantee: accumulated recovery pins must not be silently dropped.
Process exit still loses these
unsaved histories. A startup failure with no checkpoint has nothing to protect
and follows normal cleanup rather than being pinned indefinitely.

Files are written through unique private temporary files, synced, then renamed;
on Unix the directory is synced too. This does not make active workers crash
recoverable: archives are completed-worker snapshots, not continuously persisted
checkpoints. Timeout draining is bounded (30 seconds); recovery uses the latest
actual message checkpoint, not invented tool results or replayed side effects.

Existing limitations remain: archives occupy a user-wide namespace, each worker captures its archive directory at launch. Later base-directory
changes affect new workers only, not existing workers’ finalization or retries.
Explicit archive-path validation still uses the current configured base: an
operator resuming an archive from another base must select the matching config;
resume does not silently adopt that base. Directory-mtime pruning removes
archive directories older than 14 days on first use of each base. These changes do not add
profile isolation, active-worker crash persistence, or guaranteed indefinite
retention.
