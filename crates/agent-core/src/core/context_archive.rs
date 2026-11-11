//! Durable, disclosure-filtered context segments; never a summary or a session log.
//!
//! The trusted host supplies an absolute base directory, scope key and logical
//! conversation id (never model/tool path arguments). Scope and logical id are
//! hashed; project scope determines the directory name. Only user/assistant text
//! and tool-call structure
//! are retained. System/developer messages, reasoning, unknown blocks, binary,
//! non-normal sensitivity and restricted/unknown disclosure are omitted.
//! Arguments/results are WITHHELD unless a trusted redactor is installed. All
//! retained strings pass the disclosure gate and a conservative credential
//! screen. This screen is not a universal secret detector: host classification
//! and the optional redactor remain the authority for unlabelled secrets.
//!
//! Each segment contains its hidden, screened note and source indices in ONE
//! atomic file. Notes are not searched or returned by message fetch. Eligible
//! text is otherwise exact: no truncation, whitespace rewriting or summarizing
//! (one marked exception: a reduced message, below).
//! `fetch_with_provenance` reports indices into the original `seal` slice.
//!
//! A window over the limits of one segment is refused by `seal`, and sealed
//! by [`ArchiveStore::seal_window`] as consecutive parts: it is halved until
//! each part fits, each part is an ordinary segment, and the returned part
//! list is what puts the window back together. Only a single message that is
//! over a limit by itself is stored reduced, by as little as makes it fit:
//! without the tool inputs that are too large, then without any tool input,
//! then with long strings and block lists cut short. Each omission is marked
//! in place with its size, and a reduced message carries `_archive_reduced`
//! ([`REDUCED_KEY`]), which no source message can supply, so it is never read
//! back as a complete one.
//!
//! The store keeps every segment: there is no cap on how many a project holds
//! or on their total size, and no seal deletes, moves or replaces another
//! segment. A project directory holds numbered shelf directories (`s000001`,
//! `s000002`, ...) of at most 128 segments each; a full shelf is followed by a
//! new one and shelves are never merged, renumbered or removed. A segment's
//! file name is its id (the first 32 hex characters of its digest), so finding
//! one is one `openat` per shelf and one file read, never a scan. Directories
//! written before shelves existed keep their numbered slot files (`000.json`
//! to `127.json`); those are read and tombstoned in place and never added to.
//!
//! One segment is limited to 4096 messages, 16 MiB of eligible
//! strings/structure, 65536 visited nodes and depth 32 on input (excluded
//! payload descendants are opaque), an 8 KiB note and a 16 MiB file. Fetch
//! returns <=128 messages and <=16 MiB; search queries <=512 bytes, results
//! <=32 with <=256-byte snippets. Search reads newest first, one file in
//! memory at a time, and stops once it has read 1024 files or 256 MiB
//! ([`ArchiveStore::search_scan`] says when that left older segments unread).
//! The migration export carries at most 128 segments / 256 MiB and refuses a
//! larger store. Budget exhaustion and corruption fail closed, never silently
//! truncate seal.
//!
//! Seal and forget hold an advisory lock on the project directory. Fetch and
//! search read shelf files without it: a file is published by renaming a
//! complete temporary file onto its name, a live record is never modified, and
//! forget renames a complete tombstone over it, so a reader sees a whole live
//! record or a whole tombstone and nothing else. Legacy slot files are read
//! under the lock.
//!
//! Unix-only private, handle-relative no-symlink filesystem; other platforms
//! fail Unsupported. Live records are immutable; forget atomically replaces a
//! record with a content-free id/digest/scope tombstone, and is the only
//! operation that replaces a file. The digest covers ONLY
//! the eligible projection and source indices, not the note or forbidden data.
//! Identical projections in one logical conversation deduplicate permanently;
//! a changed note does not create a new segment. Retrying a tombstoned projection
//! returns PermissionDenied. A successful seal fsyncs data and directory before
//! returning; callers MUST keep source history on any error (even if publication
//! happened before a directory-fsync error). Advisory locks serialize cooperating
//! processes, not malicious processes running as the same OS user.
//!
//! What a seal promises does not rest on that lock (a lock file can be deleted
//! while it is held, and some filesystems do not lock at all). Every write
//! goes to a temporary file that no other write names, and a seal's rename
//! cannot replace (`renameat2(RENAME_NOREPLACE)`, `renameatx_np(RENAME_EXCL)`):
//! two seals at once each end with their own whole file under their own id,
//! or with an error. Only a kernel or filesystem without such a rename falls
//! back to looking and then renaming, which the lock alone makes one step.
//! Temporary files a crash left behind are removed by a later seal into the
//! same shelf, once they are older than any write still running could be.
//!
//! Within one process, seals and forgets on the same archive directory wait for
//! each other (not in FIFO order) and are admitted within [`LOCK_WAIT`] or fail
//! as busy. A lock held by another process is polled for up to
//! [`PROCESS_LOCK_WAIT`]; after that the operation fails closed ("archive
//! busy") and the caller keeps its source. The lock is released explicitly, so
//! a descriptor inherited by a concurrently forked child cannot keep it held.
//! An operation can be abandoned until it holds both locks
//! ([`ArchiveStore::with_abort`]); after that it always completes.

use crate::core::disclosure::{gate_for_model, may_persist, DisclosureClass, ModelVisibility};
use crate::core::stream_types::SharedMessage;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::collections::HashMap;
use std::io::{self, Write};
#[cfg(unix)]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, Weak};
use std::time::{Duration, Instant};

/// Segments per shelf directory. Not a store limit: a full shelf is followed
/// by a new one.
pub const SHELF_FILES: usize = 128;
/// The number of legacy slots, and the most segments a migration export carries.
pub const MAX_SEGMENTS: usize = SHELF_FILES;
pub const MAX_SEGMENT_BYTES: usize = 16 * 1024 * 1024;
/// A search stops once it has read this many segment files...
pub const SEARCH_SCAN_FILES: usize = 1024;
/// ...or this many bytes, whichever comes first.
pub const SEARCH_SCAN_BYTES: usize = 256 * 1024 * 1024;
/// The most bytes the legacy slots hold, and a migration export carries.
pub const MAX_STORE_BYTES: usize = SEARCH_SCAN_BYTES;
pub const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_MESSAGES: usize = 4096;
pub const MAX_NOTE_BYTES: usize = 8192;
pub const MAX_FETCH_MESSAGES: usize = 128;
pub const MAX_SEARCH_RESULTS: usize = 32;
/// Key of a stored message that says it is reduced: [`ArchiveStore::seal_window`]
/// left something out of it, and marked each omission in place. Only the
/// store sets it; a message without it is exactly what the projection gives.
pub const REDUCED_KEY: &str = "_archive_reduced";
/// The most a reduced message keeps of one string that is cut short...
const REDUCED_TEXT_BYTES: usize = 4096;
/// ...and of its content blocks.
const REDUCED_BLOCKS: usize = 256;
/// Longest an operation waits for earlier operations on the same archive in
/// this process before failing closed as busy.
pub const LOCK_WAIT: Duration = Duration::from_secs(30);
/// How often a queued operation with an abort flag rechecks it.
const ABORT_POLL: Duration = Duration::from_millis(20);
/// Longest an operation that needs the archive lock waits for another process
/// to release it before failing closed as busy.
pub const PROCESS_LOCK_WAIT: Duration = Duration::from_secs(5);
/// How often that wait tries the lock again.
const PROCESS_LOCK_POLL: Duration = Duration::from_millis(25);
/// How long a temporary file is left to the write that made it. A write keeps
/// its file for as long as writing one segment takes, so a file this old was
/// left by a write that never finished.
const STALE_TEMP: Duration = Duration::from_secs(60 * 60);
const WITHHELD: &str = "[archive: withheld]";
const BUSY: &str = "archive busy; retain source and retry";
const BUSY_EXPORT: &str = "archive busy; retry export";

/// Host-owned sanitizer, never supplied by model input. Must not expand output
/// beyond the archive budgets. Tool arguments are passed as serialized JSON;
/// if the returned text is not JSON the entire argument payload is withheld.
pub type ArchiveRedactor = dyn Fn(&str) -> String + Send + Sync;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveRef {
    pub id: String,
    /// Eligible messages, not the length of the source slice.
    pub message_count: usize,
    pub source_message_count: usize,
}

/// One of the segments a window was sealed as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivePart {
    pub archive: ArchiveRef,
    /// Index in the window of the first source message this part covers; it
    /// covers `archive.source_message_count` messages from there, and the
    /// `source_index` of its messages counts from there too.
    pub first_source_index: usize,
    /// Source messages in this part stored with an over-limit payload omitted
    /// (0 or 1): the messages that carry [`REDUCED_KEY`].
    pub reduced_messages: usize,
}

/// What [`ArchiveStore::seal_window`] stored. The segments record nothing
/// about each other: this list is what puts the window back together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedWindow {
    /// In source order, contiguous, covering the whole window. Never empty.
    pub parts: Vec<ArchivePart>,
    /// The window's screened note, as stored with the last part: the earlier
    /// note if that part had been sealed before. When the last part is the
    /// same segment as an earlier part of this window, which was stored
    /// without a note, it is the screened note of this call.
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveDescriptor {
    pub id: String,
    pub message_count: usize,
    pub source_message_count: usize,
    /// A bounded excerpt of eligible source JSON, never the hidden note.
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchivedMessage {
    /// Zero-based offset in the slice passed to seal, including omitted messages.
    pub source_index: usize,
    /// Original content-array indices; empty for string content.
    pub block_indices: Vec<usize>,
    pub message: SharedMessage,
}

/// Eligible, screened source evidence without filesystem state. `logical_id`
/// is the legacy 64-hex hash of the raw host session ID; the digest deliberately
/// excludes the hidden note and omitted content. Never log this payload.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveProjection {
    pub logical_id: String,
    pub source_message_count: usize,
    pub messages: Vec<ArchivedMessage>,
    pub note: String,
    pub digest: String,
}

/// Pure projection: no path discovery, directory creation, or archive I/O.
/// The trusted host supplies the raw logical session ID and optional sanitizer.
pub fn project_archive(
    logical_id: &str,
    messages: &[SharedMessage],
    note: &str,
    redactor: Option<Arc<ArchiveRedactor>>,
) -> io::Result<ArchiveProjection> {
    validate_key(logical_id)?;
    project_hashed(
        &digest_parts(&[logical_id.as_bytes()]),
        messages,
        note,
        redactor,
        Reduce::None,
    )
}

/// What is left out of a message that is over a segment budget by itself: it
/// cannot be stored whole, and being alone it cannot be cut out of its part.
/// A step is tried only when the one before it still does not fit, and
/// whatever a step does not name stays exact. Each omission is replaced by a
/// marker that says how large it was, and the stored message carries
/// [`REDUCED_KEY`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reduce {
    /// Nothing: the exact projection, the only one `seal` and
    /// [`project_archive`] ever make.
    None,
    /// Each tool input that is over a budget by itself.
    LargeInputs,
    /// Every tool input.
    Inputs,
    /// Every tool input; of each string, what follows its first
    /// [`REDUCED_TEXT_BYTES`]; the content blocks that follow the first
    /// [`REDUCED_BLOCKS`] kept; and of a tool result's block list, what
    /// follows its first text. Always fits.
    Text,
}

impl Reduce {
    fn next(self) -> Option<Self> {
        match self {
            Reduce::None => Some(Reduce::LargeInputs),
            Reduce::LargeInputs => Some(Reduce::Inputs),
            Reduce::Inputs => Some(Reduce::Text),
            Reduce::Text => None,
        }
    }

    /// Whether the preflight passes over a tool input instead of walking it.
    /// Under `LargeInputs` the projector also leaves out an input whose
    /// screened form is over a budget; the preflight has then charged for it,
    /// which errs on the strict side.
    fn skips(self, input: &Value) -> bool {
        match self {
            Reduce::None => false,
            Reduce::LargeInputs => InputBudget::alone(input).is_err(),
            Reduce::Inputs | Reduce::Text => true,
        }
    }
}

fn project_hashed(
    logical_id: &str,
    messages: &[SharedMessage],
    note: &str,
    redactor: Option<Arc<ArchiveRedactor>>,
    reduce: Reduce,
) -> io::Result<ArchiveProjection> {
    debug_assert!(reduce == Reduce::None || messages.len() <= 1);
    // Sealing fewer messages does not make a note smaller: not `over`.
    if note.len() > MAX_NOTE_BYTES {
        return Err(invalid("archive message/note budget exceeded"));
    }
    if messages.len() > MAX_MESSAGES {
        return Err(over("archive message/note budget exceeded"));
    }
    // A `Text` projection is bounded by what it keeps, whatever the source
    // holds, and walks no tool input.
    if reduce != Reduce::Text {
        let mut budget = InputBudget {
            bytes: 0,
            nodes: 0,
            reduce,
        };
        for message in messages {
            budget.message(message, redactor.is_some())?;
        }
    }
    let projector = Projector {
        redactor,
        reduce,
        omitted: Cell::new(0),
    };
    let mut eligible = Vec::new();
    for (source_index, message) in messages.iter().enumerate() {
        if let Some((message, block_indices)) = projector.project(message)? {
            eligible.push(ArchivedMessage {
                source_index,
                block_indices,
                message: Arc::new(message),
            });
        }
    }
    let digest = projection_digest(logical_id, messages.len(), &eligible)?;
    let note = projector.screen(note, DisclosureClass::ModelVisible)?;
    if note.len() > MAX_NOTE_BYTES {
        return Err(invalid("redacted note exceeds archive budget"));
    }
    Ok(ArchiveProjection {
        logical_id: logical_id.to_owned(),
        source_message_count: messages.len(),
        messages: eligible,
        note,
        digest,
    })
}

struct Projector {
    redactor: Option<Arc<ArchiveRedactor>>,
    reduce: Reduce,
    /// Omissions made so far in the message being projected.
    omitted: Cell<usize>,
}

/// No Debug implementation: do not accidentally log redactor state or content.
pub struct ArchiveStore {
    fs: PrivateDir,
    scope: String,
    logical: String,
    redactor: Option<Arc<ArchiveRedactor>>,
    abort: Option<Arc<AtomicBool>>,
    wait: Duration,
    /// [`PROCESS_LOCK_WAIT`]; tests shorten it.
    process_wait: Duration,
}

/// Process-local mutual exclusion for one archive directory. The advisory
/// file lock excludes other processes, but each operation opens its own
/// descriptor, so without this two threads of one process would see each
/// other as busy. Waiters are not served in FIFO order.
struct Gate {
    held: Mutex<bool>,
    released: Condvar,
    /// Operations that have waited at least once (tests synchronize on it).
    #[cfg(test)]
    waited: std::sync::atomic::AtomicUsize,
}

/// Field order is drop order: unlock the file, then admit the next operation.
struct Locked {
    file: Option<FlockGuard>,
    _gate: GateGuard,
}

struct GateGuard(Arc<Gate>);

/// Device and inode of an archive directory.
type DirIdentity = (u64, u64);
type Gates = Mutex<HashMap<DirIdentity, Weak<Gate>>>;

impl Gate {
    /// One gate per directory identity, however the directory was reached.
    fn for_directory(identity: DirIdentity) -> Arc<Gate> {
        static GATES: OnceLock<Gates> = OnceLock::new();
        let mut gates = GATES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(gate) = gates.get(&identity).and_then(Weak::upgrade) {
            return gate;
        }
        gates.retain(|_, gate| gate.strong_count() > 0);
        let gate = Arc::new(Gate {
            held: Mutex::new(false),
            released: Condvar::new(),
            #[cfg(test)]
            waited: Default::default(),
        });
        gates.insert(identity, Arc::downgrade(&gate));
        gate
    }

    fn enter(
        gate: Arc<Gate>,
        wait: Duration,
        abort: Option<&AtomicBool>,
        busy: &'static str,
    ) -> io::Result<GateGuard> {
        let deadline = Instant::now() + wait;
        {
            let mut held = gate.held.lock().unwrap_or_else(PoisonError::into_inner);
            let mut waited = false;
            loop {
                if abort.is_some_and(|abort| abort.load(Ordering::Acquire)) {
                    return Err(canceled());
                }
                // An uncontended first attempt is admitted at once; after
                // waiting, an expired operation is never admitted, even when
                // the gate has just become free.
                let now = Instant::now();
                if waited && now >= deadline {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, busy));
                }
                if !*held {
                    *held = true;
                    break;
                }
                if now >= deadline {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, busy));
                }
                let mut slice = deadline - now;
                if abort.is_some() {
                    slice = slice.min(ABORT_POLL);
                }
                #[cfg(test)]
                if !waited {
                    gate.waited.fetch_add(1, Ordering::AcqRel);
                }
                waited = true;
                held = gate
                    .released
                    .wait_timeout(held, slice)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        }
        Ok(GateGuard(gate))
    }
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        *self.0.held.lock().unwrap_or_else(PoisonError::into_inner) = false;
        // Every waiter rechecks; an aborted or expired one must not absorb the
        // only wakeup.
        self.0.released.notify_all();
    }
}

/// Unlocks explicitly before closing. A flock belongs to the open file
/// description, and a child forked by any thread holds a copy of it until it
/// execs (O_CLOEXEC applies only at exec); closing our copy alone would leave
/// the archive locked meanwhile.
struct FlockGuard(std::fs::File);

impl Drop for FlockGuard {
    fn drop(&mut self) {
        let _ = fs4::fs_std::FileExt::unlock(&self.0);
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    scope: String,
    logical: String,
    id: String,
    digest: String,
    body: Body,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum Body {
    Live {
        source_message_count: usize,
        messages: Vec<ArchivedMessage>,
        note: String,
    },
    Tombstone,
}

impl Record {
    fn reference(&self) -> io::Result<ArchiveRef> {
        match &self.body {
            Body::Live {
                source_message_count,
                messages,
                ..
            } => Ok(ArchiveRef {
                id: self.id.clone(),
                message_count: messages.len(),
                source_message_count: *source_message_count,
            }),
            Body::Tombstone => Err(not_found()),
        }
    }
}

impl ArchiveStore {
    pub fn new(base: &Path, scope_key: &str, logical_id: &str) -> io::Result<Self> {
        for key in [scope_key, logical_id] {
            validate_key(key)?;
        }
        let scope = digest_parts(&[scope_key.as_bytes()]);
        let logical = digest_parts(&[logical_id.as_bytes()]);
        let fs = PrivateDir::new(base, &scope)?;
        Ok(Self {
            fs,
            scope,
            logical,
            redactor: None,
            abort: None,
            wait: LOCK_WAIT,
            process_wait: PROCESS_LOCK_WAIT,
        })
    }

    /// Opt in to screened tool evidence. Install the SAME trusted policy when
    /// reopening a store; retrieval never restores material withheld at seal.
    pub fn with_redactor(mut self, redactor: Arc<ArchiveRedactor>) -> Self {
        self.redactor = Some(redactor);
        self
    }

    /// Once `abort` is set, an operation that has not been admitted yet (one
    /// waiting behind another operation in this process, or for another
    /// process to release the lock) fails with `Interrupted` without reading
    /// or writing any record. A read that needs no lock looks at the flag
    /// once, before it starts. Setup before admission (opening the store,
    /// projecting a seal) is not undone. An admitted operation always
    /// completes, so callers must still treat an abandoned seal as possibly
    /// published.
    pub fn with_abort(mut self, abort: Arc<AtomicBool>) -> Self {
        self.abort = Some(abort);
        self
    }

    /// Queue behind other operations on this archive in this process, then
    /// take the cross-process lock, trying again for up to `process_wait`
    /// while another process holds it. The gate stays held meanwhile: the
    /// operations behind it would meet the same lock. Leaving early (busy,
    /// abandoned, any other error) releases the gate.
    fn exclusive(&self) -> io::Result<Locked> {
        let gate = self.enter(BUSY)?;
        let deadline = Instant::now() + self.process_wait;
        let file = loop {
            match self.fs.lock() {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.check_abort()?;
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(error);
                    }
                    std::thread::sleep(PROCESS_LOCK_POLL.min(deadline - now));
                }
                result => break result?,
            }
        };
        Ok(Locked {
            file: Some(file),
            _gate: gate,
        })
    }

    /// `Interrupted` once the caller has abandoned this operation.
    fn check_abort(&self) -> io::Result<()> {
        match &self.abort {
            Some(abort) if abort.load(Ordering::Acquire) => Err(canceled()),
            _ => Ok(()),
        }
    }

    fn enter(&self, busy: &'static str) -> io::Result<GateGuard> {
        let gate = Gate::for_directory(self.fs.identity()?);
        Gate::enter(gate, self.wait, self.abort.as_deref(), busy)
    }

    pub fn seal(&self, messages: &[SharedMessage], note: &str) -> io::Result<ArchiveRef> {
        self.seal_with_note(messages, note)
            .map(|(reference, _)| reference)
    }

    /// [`seal`](Self::seal) that also returns the screened note stored with the
    /// segment (the earlier note when this projection was already sealed), in
    /// the same locked operation.
    pub fn seal_with_note(
        &self,
        messages: &[SharedMessage],
        note: &str,
    ) -> io::Result<(ArchiveRef, String)> {
        let (reference, note, _) = self.seal_one(messages, note, Reduce::None)?;
        Ok((reference, note))
    }

    /// Seal one window as one or more segments ("parts"). No per-segment
    /// budget fails it; what still can is what fails any seal (I/O, a busy
    /// lock, an abort, a part that was forgotten) and a note over its limit,
    /// which is refused before anything is written.
    ///
    /// A window within the budgets is one part: the same segment, id and note
    /// as [`seal_with_note`](Self::seal_with_note). A larger one is halved
    /// until each part fits. Every part is an ordinary segment; together they
    /// hold every message of the window exactly once, in order. Only a
    /// message that is over a budget by itself is stored reduced, and is then
    /// a part of its own.
    ///
    /// Parts are sealed first to last, each in its own locked operation, and
    /// the note goes to the last one. After an error the parts already sealed
    /// stay (callers keep the source history, as for `seal`); a retry finds
    /// them by id, writes only what is missing and returns the same list.
    pub fn seal_window(&self, messages: &[SharedMessage], note: &str) -> io::Result<SealedWindow> {
        // Sealing fewer messages does not make a note smaller: one over its
        // limit is refused here, before any part exists.
        let redactor = self.redactor.clone();
        let screened = project_hashed(&self.logical, &[], note, redactor, Reduce::None)?.note;
        let mut window = SealedWindow {
            parts: Vec::new(),
            note: String::new(),
        };
        self.seal_range(messages, 0..messages.len(), note, &mut window)?;
        // Parts with identical content are one segment. When the last part is
        // also an earlier one, that segment was stored for the earlier part,
        // without the note; the window's note is still this one.
        if let Some((last, earlier)) = window.parts.split_last() {
            if earlier.iter().any(|part| part.archive.id == last.archive.id) {
                window.note = screened;
            }
        }
        Ok(window)
    }

    /// Seal `messages[range]` as one part if it fits, otherwise as the parts
    /// of its two halves, the left one first. `note` is the window's note for
    /// the range that ends the window and empty for every other.
    fn seal_range(
        &self,
        messages: &[SharedMessage],
        range: std::ops::Range<usize>,
        note: &str,
        window: &mut SealedWindow,
    ) -> io::Result<()> {
        let mut reduce = Reduce::None;
        let refused = loop {
            match self.seal_one(&messages[range.clone()], note, reduce) {
                Ok((archive, stored, reduced_messages)) => {
                    window.parts.push(ArchivePart {
                        archive,
                        first_source_index: range.start,
                        reduced_messages,
                    });
                    window.note = stored;
                    return Ok(());
                }
                Err(error) if !over_budget(&error) => return Err(error),
                // Reduced only when halving cannot help: one message, alone.
                Err(error) => match reduce.next().filter(|_| range.len() == 1) {
                    Some(next) => reduce = next,
                    None => break error,
                },
            }
        };
        if range.len() < 2 {
            return Err(refused);
        }
        let middle = cut(messages, &range);
        self.seal_range(messages, range.start..middle, "", window)?;
        self.seal_range(messages, middle..range.end, note, window)
    }

    /// [`seal_with_note`](Self::seal_with_note) of the projection `reduce`
    /// selects. Also returns how many of its messages are stored reduced.
    fn seal_one(
        &self,
        messages: &[SharedMessage],
        note: &str,
        reduce: Reduce,
    ) -> io::Result<(ArchiveRef, String, usize)> {
        let redactor = self.redactor.clone();
        let projection = project_hashed(&self.logical, messages, note, redactor, reduce)?;
        let reduced = projection
            .messages
            .iter()
            .filter(|archived| archived.message.get(REDUCED_KEY).is_some())
            .count();
        let _lock = self.exclusive()?;
        let (reference, note) = self.store(projection)?;
        Ok((reference, note, reduced))
    }

    /// What a seal does once it holds the lock: find `projection` stored, or
    /// store it. The lock keeps two seals from doing this at once, but what
    /// comes back does not depend on it: a segment is written to a file of
    /// this call's own and renamed without replacing, so `Ok` always means
    /// that the file under the returned id holds this projection, whole.
    fn store(&self, projection: ArchiveProjection) -> io::Result<(ArchiveRef, String)> {
        let ArchiveProjection {
            digest,
            source_message_count,
            messages,
            note,
            ..
        } = projection;
        // Same projection, same id, same file name.
        let id = digest[..32].to_owned();
        let shelves = self.shelves()?;
        if let Some(sealed) = self.sealed(&shelves, &id, &digest)? {
            return Ok(sealed);
        }
        let record = Record {
            version: 1,
            scope: self.scope.clone(),
            logical: self.logical.clone(),
            id,
            digest,
            body: Body::Live {
                source_message_count,
                messages,
                note: note.clone(),
            },
        };
        let bytes = bounded_json(&record, MAX_SEGMENT_BYTES)?;
        // Created new: a file that is already there is never renamed over.
        let shelf = self.writable_shelf(&shelves)?;
        match shelf.publish(&archive_name(&record.id), &bytes, true) {
            // The name was free a moment ago: another seal has stored
            // something under it since, which the lock should have kept
            // out. It is read like any file found there, never replaced.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => self
                .sealed(&self.shelves()?, &record.id, &record.digest)?
                .ok_or(error),
            result => result.and_then(|()| Ok((record.reference()?, note))),
        }
    }

    /// The reference and stored note of the projection `digest` if it is
    /// already sealed, on a shelf under `id` or in a legacy slot. A forgotten
    /// one, or another projection under that id, is an error.
    fn sealed(
        &self,
        shelves: &[String],
        id: &str,
        digest: &str,
    ) -> io::Result<Option<(ArchiveRef, String)>> {
        let (shelf, record) = match self.locate(shelves, id, false)? {
            Some((shelf, record)) => (Some(shelf), record),
            // Sealed before shelves existed, under a random id.
            None => match self.legacy_projection(digest) {
                Some(record) => (None, record),
                None => return Ok(None),
            },
        };
        if record.digest != digest {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "archive id belongs to another projection",
            ));
        }
        let Body::Live { note, .. } = &record.body else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "archive projection was forgotten",
            ));
        };
        // A previous call may have failed AFTER rename but BEFORE fsync.
        shelf.as_ref().unwrap_or(&self.fs).sync()?;
        Ok(Some((record.reference()?, note.clone())))
    }

    pub fn fetch(&self, id: &str, start: usize, limit: usize) -> io::Result<Vec<SharedMessage>> {
        Ok(self
            .fetch_with_provenance(id, start, limit)?
            .into_iter()
            .map(|m| m.message)
            .collect())
    }

    /// `start` indexes ELIGIBLE messages; source_index retains original offsets.
    /// Oversized limits are rejected rather than clamped; start beyond end is empty.
    pub fn fetch_with_provenance(
        &self,
        id: &str,
        start: usize,
        limit: usize,
    ) -> io::Result<Vec<ArchivedMessage>> {
        validate_id(id)?;
        if limit > MAX_FETCH_MESSAGES {
            return Err(invalid("archive fetch limit exceeds 128"));
        }
        match self.find(id)?.body {
            Body::Live { messages, .. } => {
                Ok(messages.into_iter().skip(start).take(limit).collect())
            }
            Body::Tombstone => Err(not_found()),
        }
    }

    /// Explicit host-only access to the hidden, screened note. Never include it
    /// in search descriptors or inject it as system/developer instructions.
    pub fn fetch_note(&self, id: &str) -> io::Result<String> {
        validate_id(id)?;
        match self.find(id)?.body {
            Body::Live { note, .. } => Ok(note),
            Body::Tombstone => Err(not_found()),
        }
    }

    /// Case-insensitive literal substring search of eligible source JSON,
    /// newest first. Empty query lists descriptors. Not an exhaustive ranking
    /// service: output is bounded by limit, with no hidden-note matches, and
    /// the scan by [`SEARCH_SCAN_FILES`] / [`SEARCH_SCAN_BYTES`].
    pub fn search(&self, query: &str, limit: usize) -> io::Result<Vec<ArchiveDescriptor>> {
        self.search_scan(query, limit).map(|(rows, _)| rows)
    }

    /// [`search`](Self::search) that also says whether the scan budget cut it
    /// short: `Some(n)` when it stopped after reading the `n` newest segment
    /// files and left older ones unread, `None` when nothing it was asked for
    /// is missing. Shelves are read newest to oldest, a shelf's files by
    /// modification time, the legacy slots last. A file that is not a valid
    /// record is skipped (one corrupt file must not hide the rest); a link, a
    /// FIFO or a file that is not ours alone where a segment belongs still
    /// fails the search. Shelf files are read without the lock; it is taken
    /// only if the search gets as far as reading a legacy slot.
    pub fn search_scan(
        &self,
        query: &str,
        limit: usize,
    ) -> io::Result<(Vec<ArchiveDescriptor>, Option<usize>)> {
        if query.len() > 512 || limit > MAX_SEARCH_RESULTS {
            return Err(invalid("archive search query/result budget exceeded"));
        }
        if limit == 0 {
            return Ok((Vec::new(), None));
        }
        let query = query.to_lowercase();
        self.check_abort()?;
        let (shelves, mut slots) = self.layout()?;
        let mut rows = Vec::new();
        let (mut files, mut bytes) = (0, 0);
        let mut legacy_lock = None;
        // `None` is the project directory itself: the legacy slots, oldest of all.
        for shelf in shelves.iter().map(Some).chain([None]) {
            let opened;
            let (dir, names) = match shelf {
                Some(shelf) => {
                    let Some(dir) = self.fs.open_child(shelf, true)? else {
                        continue;
                    };
                    let mut entries = dir.names()?;
                    entries.retain(|(name, ..)| archive_id(name).is_some());
                    // Modification time only orders the files of one shelf;
                    // nothing else depends on it.
                    entries.sort_unstable_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
                    opened = dir;
                    let names: Vec<_> = entries.into_iter().map(|(name, ..)| name).collect();
                    (&opened, names)
                }
                None => (&self.fs, std::mem::take(&mut slots)),
            };
            for name in names {
                if rows.len() == limit {
                    return Ok((rows, None));
                }
                if files == SEARCH_SCAN_FILES || bytes >= SEARCH_SCAN_BYTES {
                    return Ok((rows, Some(files)));
                }
                if shelf.is_none() && legacy_lock.is_none() {
                    legacy_lock = Some(self.exclusive()?);
                }
                let raw = match dir.read_with(&name, MAX_SEGMENT_BYTES, shelf.is_some()) {
                    Ok(Some(raw)) => raw,
                    Ok(None) => continue,
                    // Too large to be a segment: not read, and no record below.
                    Err(error) if error.kind() == io::ErrorKind::InvalidData => Vec::new(),
                    Err(error) => return Err(error),
                };
                files += 1;
                bytes += raw.len();
                if let Ok(record) = self.record(&name, &raw) {
                    rows.extend(describe(record, &query)?);
                }
            }
        }
        Ok((rows, None))
    }

    /// Durable, idempotent deletion. Replaces content in the SAME file with a
    /// marker; it never frees the id/digest for reuse. Unknown ids are NotFound.
    /// This is logical deletion, not secure erasure of filesystem snapshots.
    /// The only operation that replaces a file, and only the one for `id`.
    pub fn forget(&self, id: &str) -> io::Result<()> {
        validate_id(id)?;
        let _lock = self.exclusive()?;
        let (shelf, name, mut record) = match self.locate(&self.shelves()?, id, false)? {
            Some((shelf, record)) => (Some(shelf), archive_name(id), record),
            None => {
                let (legacy, _) = self.scan_legacy(false)?;
                let found = legacy.into_iter().find(|(_, record)| record.id == id);
                let (slot, record) = found.ok_or_else(not_found)?;
                (None, slot_name(slot), record)
            }
        };
        let dir = shelf.as_ref().unwrap_or(&self.fs);
        if matches!(record.body, Body::Tombstone) {
            return dir.sync();
        }
        record.body = Body::Tombstone;
        dir.publish(&name, &bounded_json(&record, MAX_SEGMENT_BYTES)?, false)
    }

    /// Migration-only export of validated eligible evidence and permanent
    /// tombstones. Includes hidden notes: this is HOST data, never a model tool.
    /// No files, directories, permissions or archive contents are changed.
    /// Legacy slots first, then shelves oldest to newest. A migration carries
    /// at most [`MAX_SEGMENTS`] segments / [`MAX_STORE_BYTES`]; a larger store
    /// is refused whole rather than exported in part.
    pub fn export_records(&self) -> io::Result<Vec<Value>> {
        let gate = self.enter(BUSY_EXPORT)?;
        let lock = Locked {
            file: self.fs.lock_existing()?,
            _gate: gate,
        };
        let over_limit =
            || invalid("archive store exceeds the migration limit (128 archives / 256 MiB)");
        let (legacy, mut used) = self.scan_legacy(true)?;
        let mut records: Vec<Record> = legacy.into_iter().map(|(_, record)| record).collect();
        for shelf in self.shelves()?.iter().rev() {
            let Some(dir) = self.fs.open_child(shelf, true)? else {
                continue;
            };
            let mut names: Vec<_> = dir.names()?.into_iter().map(|(name, ..)| name).collect();
            names.retain(|name| archive_id(name).is_some());
            names.sort_unstable();
            for name in names {
                if records.len() == MAX_SEGMENTS {
                    return Err(over_limit());
                }
                let Some(raw) = dir.read_with(&name, MAX_SEGMENT_BYTES, true)? else {
                    continue;
                };
                used += raw.len();
                if used > MAX_STORE_BYTES {
                    return Err(over_limit());
                }
                let record = self.record(&name, &raw)?;
                if records
                    .iter()
                    .any(|r| r.id == record.id || r.digest == record.digest)
                {
                    return Err(corrupt());
                }
                records.push(record);
            }
        }
        if lock.file.is_none() && !records.is_empty() {
            return Err(corrupt());
        }
        Ok(records
            .into_iter()
            .map(|record| {
                let (source_message_count, messages, note, tombstone) = match record.body {
                    Body::Live {
                        source_message_count,
                        messages,
                        note,
                    } => (source_message_count, messages, note, false),
                    Body::Tombstone => (0, Vec::new(), String::new(), true),
                };
                json!({
                    "id": record.id, "digest": record.digest, "logical_id": record.logical,
                    "source_message_count": source_message_count, "messages": messages,
                    "note": note, "tombstone": tombstone,
                })
            })
            .collect())
    }

    /// Shelf names in the project directory, newest first.
    fn shelves(&self) -> io::Result<Vec<String>> {
        Ok(self.layout()?.0)
    }

    /// The project directory's shelves and legacy slot files, each newest
    /// first. One directory listing; no segment is opened.
    fn layout(&self) -> io::Result<(Vec<String>, Vec<String>)> {
        let (mut shelves, mut slots) = (Vec::new(), Vec::new());
        for (name, is_dir, _) in self.fs.names()? {
            if let Some(number) = shelf_number(&name) {
                // A link or a file under a shelf's name is refused, not skipped.
                if !is_dir {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "archive shelf must be a private directory",
                    ));
                }
                shelves.push((number, name));
            } else if slot_number(&name).is_some() {
                slots.push(name);
            }
        }
        shelves.sort_unstable();
        slots.sort_unstable();
        let shelves = shelves.into_iter().rev().map(|(_, name)| name).collect();
        slots.reverse();
        Ok((shelves, slots))
    }

    /// The shelf the next segment goes to: the newest one, or a new one after
    /// it once it holds [`SHELF_FILES`]. An empty shelf left by a crash is used.
    fn writable_shelf(&self, shelves: &[String]) -> io::Result<PrivateDir> {
        let newest = shelves.first().and_then(|name| shelf_number(name));
        if let Some(number) = newest {
            // `child` also makes the shelf's own directory entry durable.
            let shelf = self.fs.child(&shelf_name(number), true)?;
            let names = shelf.names()?;
            sweep(&shelf, &names);
            if names.iter().filter(|(name, ..)| name.ends_with(".json")).count() < SHELF_FILES {
                return Ok(shelf);
            }
        }
        let next = newest.map_or(Some(1), |number| number.checked_add(1));
        let next = next.ok_or_else(|| invalid("archive shelf numbers exhausted"))?;
        self.fs.child(&shelf_name(next), true)
    }

    /// The shelf that holds segment `id`, and its record: one `openat` per
    /// shelf, newest first, and one file read. Never a scan.
    fn locate(
        &self,
        shelves: &[String],
        id: &str,
        readonly: bool,
    ) -> io::Result<Option<(PrivateDir, Record)>> {
        let name = archive_name(id);
        for shelf in shelves {
            let Some(dir) = self.fs.open_child(shelf, readonly)? else {
                continue;
            };
            if let Some(record) = self.read_record(&dir, &name, readonly)? {
                return Ok(Some((dir, record)));
            }
        }
        Ok(None)
    }

    /// The record of segment `id`, for a caller that holds no lock: one file
    /// when it is on a shelf, read without the gate or the lock. Only when no
    /// shelf has it and the directory has legacy slot files are those
    /// scanned, under the lock.
    fn find(&self, id: &str) -> io::Result<Record> {
        self.check_abort()?;
        let (shelves, slots) = self.layout()?;
        if let Some((_, record)) = self.locate(&shelves, id, true)? {
            return Ok(record);
        }
        if slots.is_empty() {
            return Err(not_found());
        }
        let _lock = self.exclusive()?;
        find_record(self.scan_legacy(false)?.0, id)
    }

    fn read_record(
        &self,
        dir: &PrivateDir,
        name: &str,
        readonly: bool,
    ) -> io::Result<Option<Record>> {
        dir.read_with(name, MAX_SEGMENT_BYTES, readonly)?
            .map(|raw| self.record(name, &raw))
            .transpose()
    }

    /// Parse and validate the contents of the segment file `name`. Anything
    /// that is not a whole record of this scope is `InvalidData`.
    fn record(&self, name: &str, raw: &[u8]) -> io::Result<Record> {
        let record: Record = serde_json::from_slice(raw).map_err(|_| corrupt())?;
        if record.version != 1
            || record.scope != self.scope
            || record.logical.len() != 64
            || !record.logical.bytes().all(|b| b.is_ascii_hexdigit())
            || validate_id(&record.id).is_err()
            || record.digest.len() != 64
            || !record.digest.bytes().all(|b| b.is_ascii_hexdigit())
            // A shelf file is named by its id; only a legacy slot is not.
            || (name != archive_name(&record.id) && slot_number(name).is_none())
        {
            return Err(corrupt());
        }
        if let Body::Live {
            messages,
            source_message_count,
            note,
        } = &record.body
        {
            if messages.len() > MAX_MESSAGES
                || *source_message_count > MAX_MESSAGES
                || note.len() > MAX_NOTE_BYTES
                || messages
                    .iter()
                    .any(|m| m.source_index >= *source_message_count)
                || messages
                    .windows(2)
                    .any(|m| m[0].source_index >= m[1].source_index)
                || projection_digest(&record.logical, *source_message_count, messages)?
                    != record.digest
            {
                return Err(corrupt());
            }
        }
        Ok(record)
    }

    /// The legacy slot that holds the projection `digest`, live or forgotten:
    /// what a seal asks of the slots. A seal writes to none of them, so it
    /// must not fail because of one. Unlike [`scan_legacy`](Self::scan_legacy),
    /// which fetch, forget and export still use and which fails as a whole,
    /// this passes over a slot that cannot be read (a link, a FIFO, a file
    /// that is not ours alone or is over the size of a segment, an I/O error)
    /// or that is not a valid record of this scope. A slot passed over is
    /// left as it is: a link is not followed, and a file that is not ours
    /// alone is neither read nor made private. One slot is in memory at a
    /// time, so their total size is not bounded here.
    ///
    /// The price: if the slot passed over was the tombstone of this very
    /// projection, the projection is sealed again.
    fn legacy_projection(&self, digest: &str) -> Option<Record> {
        (0..MAX_SEGMENTS).find_map(|slot| {
            let name = slot_name(slot);
            let raw = self.fs.read_with(&name, MAX_SEGMENT_BYTES, false);
            match raw.and_then(|raw| raw.map(|raw| self.record(&name, &raw)).transpose()) {
                Ok(record) => record.filter(|record| record.digest == digest),
                Err(error) => {
                    // The file name and the error only: never content.
                    tracing::warn!(slot = %name, %error, "seal passed over a legacy archive slot");
                    None
                }
            }
        })
    }

    /// Every legacy slot, read and validated together as before shelves
    /// existed: at most [`MAX_SEGMENTS`] files / [`MAX_STORE_BYTES`].
    fn scan_legacy(&self, readonly: bool) -> io::Result<(Vec<(usize, Record)>, usize)> {
        let mut records = Vec::new();
        let mut used = 0;
        for slot in 0..MAX_SEGMENTS {
            let name = slot_name(slot);
            let Some(raw) = self.fs.read_with(&name, MAX_STORE_BYTES - used, readonly)? else {
                continue;
            };
            used += raw.len();
            let record = self.record(&name, &raw)?;
            if records
                .iter()
                .any(|(_, r): &(usize, Record)| r.id == record.id || r.digest == record.digest)
            {
                return Err(corrupt());
            }
            records.push((slot, record));
        }
        Ok((records, used))
    }
}

/// The search row of a live record that matches `query` (already lowercased).
fn describe(record: Record, query: &str) -> io::Result<Option<ArchiveDescriptor>> {
    let Body::Live {
        messages,
        source_message_count,
        ..
    } = &record.body
    else {
        return Ok(None);
    };
    let mut snippet = None;
    for message in messages {
        let text = serde_json::to_string(&message.message).map_err(json_error)?;
        if query.is_empty() || text.to_lowercase().contains(query) {
            snippet = Some(prefix(&text, 256).to_owned());
            break;
        }
    }
    if snippet.is_none() && !(query.is_empty() && messages.is_empty()) {
        return Ok(None);
    }
    Ok(Some(ArchiveDescriptor {
        message_count: messages.len(),
        source_message_count: *source_message_count,
        snippet: snippet.unwrap_or_default(),
        id: record.id,
    }))
}

impl Projector {
    fn screen(&self, text: &str, class: DisclosureClass) -> io::Result<String> {
        if class == DisclosureClass::ModelVisibleAfterRedaction && self.redactor.is_none() {
            return Ok(WITHHELD.to_owned());
        }
        let redactor = |text: &str| {
            let text = self
                .redactor
                .as_ref()
                .map_or_else(|| text.to_owned(), |r| r(text));
            if credential_like(&text) {
                WITHHELD.to_owned()
            } else {
                text
            }
        };
        // All text is screened, including the ordinary model-visible class.
        let text = redactor(text);
        // A `Text` reduction bounds what it stores after screening: a message
        // string by cutting it (`text`), the note by its own limit.
        if text.len() > MAX_SEGMENT_BYTES && self.reduce != Reduce::Text {
            return Err(over("redactor output exceeds archive budget"));
        }
        match gate_for_model(class, &text, false, Some(&|s| s.to_owned())) {
            ModelVisibility::Visible(text) => Ok(text),
            ModelVisibility::Withheld(_) => Ok(WITHHELD.to_owned()),
        }
    }

    /// A message string as stored: screened whole and then, in a `Text`
    /// reduction, cut to its first [`REDUCED_TEXT_BYTES`]. The cut comes last
    /// so that the redactor never sees half of something it would remove.
    fn text(&self, text: &str, class: DisclosureClass) -> io::Result<String> {
        let text = self.screen(text, class)?;
        if self.reduce != Reduce::Text || text.len() <= REDUCED_TEXT_BYTES {
            return Ok(text);
        }
        let kept = prefix(&text, REDUCED_TEXT_BYTES);
        let omitted = self.omission(format_args!("{} bytes", text.len() - kept.len()));
        Ok(format!("{kept}\n{omitted}"))
    }

    /// What stands in place of something a reduction left out: what it was
    /// and how large.
    fn omission(&self, what: std::fmt::Arguments<'_>) -> String {
        self.omitted.set(self.omitted.get() + 1);
        format!("[archive: {what} omitted, over the per-archive limit]")
    }

    /// The stored form of a message. One that a reduction left something out
    /// of says so under [`REDUCED_KEY`], which a source message cannot
    /// supply: only its role and content are ever taken. The markers inside
    /// the content can be imitated by content; this key cannot.
    fn stored(&self, role: &str, content: Value) -> Value {
        let mut message = json!({"role": role, "content": content});
        let omitted = self.omitted.replace(0);
        if omitted > 0 {
            message[REDUCED_KEY] = json!(format!(
                "this message alone is over the per-archive limit; it is stored with \
                 {omitted} omission(s), each marked in place"
            ));
        }
        message
    }

    fn project(&self, message: &Value) -> io::Result<Option<(Value, Vec<usize>)>> {
        let Some((class, role, content)) = projection_content(message) else {
            return Ok(None);
        };
        self.omitted.set(0);
        if let Some(text) = content.as_str() {
            let content = json!(self.text(text, class)?);
            return Ok(Some((self.stored(role, content), Vec::new())));
        }
        let Some(blocks) = content.as_array() else {
            return Ok(None);
        };
        let mut output = Vec::new();
        let mut indices = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            if self.reduce == Reduce::Text && output.len() == REDUCED_BLOCKS {
                // This block and all after it, under the index of this one.
                let rest = self.omission(format_args!("{} blocks", blocks.len() - index));
                output.push(json!({"type":"text", "text":rest}));
                indices.push(index);
                break;
            }
            let Some(mut block_class) = eligible_class(block) else {
                continue;
            };
            if class == DisclosureClass::ModelVisibleAfterRedaction {
                block_class = class;
            }
            let kept = match block.get("type").and_then(Value::as_str) {
                Some("text") => block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| {
                        self.text(text, block_class)
                            .map(|text| json!({"type":"text", "text":text}))
                    })
                    .transpose()?,
                Some("tool_use") if role == "assistant" => {
                    if let (Some(id), Some(name)) =
                        (identifier(block, "id"), identifier(block, "name"))
                    {
                        let input = if name == "context_checkpoint" {
                            // The working note has its own hidden field. Do not
                            // expose it a second time via tool arguments.
                            json!({"_archive_withheld":true})
                        } else if self.redactor.is_some() {
                            self.tool_input(block.get("input"), block_class)?
                        } else {
                            json!({"_archive_withheld":true})
                        };
                        Some(json!({"type":"tool_use", "id":id, "name":name, "input":input}))
                    } else {
                        None
                    }
                }
                Some("tool_result") if role == "user" => {
                    if let Some(id) = identifier(block, "tool_use_id") {
                        let content = if self.redactor.is_some() {
                            self.tool_result(&block["content"], block_class)?
                        } else {
                            Value::String(WITHHELD.to_owned())
                        };
                        let mut result =
                            json!({"type":"tool_result", "tool_use_id":id, "content":content});
                        if let Some(error) = block.get("is_error").and_then(Value::as_bool) {
                            result["is_error"] = json!(error);
                        }
                        Some(result)
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some(kept) = kept {
                output.push(kept);
                indices.push(index);
            }
        }
        if output.is_empty() {
            Ok(None)
        } else {
            Ok(Some((self.stored(role, json!(output)), indices)))
        }
    }

    /// A tool input as stored: exact, or, where a reduction leaves it out, a
    /// marker with its size as JSON.
    fn tool_input(&self, input: Option<&Value>, class: DisclosureClass) -> io::Result<Value> {
        let omit = |input: &Value| {
            let bytes = json_len(input);
            let marker = self.omission(format_args!("tool input of {bytes} bytes"));
            Ok(json!({"_archive_omitted": marker}))
        };
        match (self.reduce, input) {
            (Reduce::Inputs | Reduce::Text, Some(input)) => omit(input),
            (Reduce::LargeInputs, Some(value)) => match self.exact_input(input, class) {
                Err(error) if over_budget(&error) => omit(value),
                exact => exact,
            },
            _ => self.exact_input(input, class),
        }
    }

    fn exact_input(&self, input: Option<&Value>, class: DisclosureClass) -> io::Result<Value> {
        if let Some(input) = input {
            // The full preflight already enforced aggregate limits. Reuse its
            // guarded admission predicate so recursion and withholding cannot
            // drift. JSON escaping expands at most sixfold (node allowance
            // covers punctuation/numbers); redactors may shrink that scratch
            // string below the unchanged final output limit.
            if let Some(bytes) = InputBudget::alone(input)? {
                let limit = bytes
                    .checked_mul(6)
                    .ok_or_else(|| over("archive input byte budget exceeded"))?;
                let encoded = bounded_json(input, limit)?;
                let text =
                    self.screen(std::str::from_utf8(&encoded).map_err(|_| corrupt())?, class)?;
                return Ok(serde_json::from_str(&text)
                    .unwrap_or_else(|_| json!({"_archive_withheld":true})));
            }
        }
        Ok(json!({"_archive_withheld":true}))
    }

    fn tool_result(&self, content: &Value, class: DisclosureClass) -> io::Result<Value> {
        if let Some(text) = content.as_str() {
            return Ok(json!(self.text(text, class)?));
        }
        let mut output = Vec::new();
        if let Some(blocks) = content.as_array() {
            for (index, block) in blocks.iter().enumerate() {
                if let Some(class) = eligible_class(block) {
                    if block["type"] == "text" {
                        if let Some(text) = block["text"].as_str() {
                            let text = self.text(text, class)?;
                            if self.reduce == Reduce::Text {
                                // One string: the first text, then how many
                                // blocks followed it.
                                let rest = blocks.len() - index - 1;
                                if rest == 0 {
                                    return Ok(json!(text));
                                }
                                let rest = self.omission(format_args!("{rest} blocks"));
                                return Ok(json!(format!("{text}\n{rest}")));
                            }
                            output.push(json!({"type":"text", "text":text}));
                        }
                    }
                }
            }
        }
        if output.is_empty() {
            Ok(json!(WITHHELD))
        } else {
            Ok(json!(output))
        }
    }
}

/// Read-only migration preview for an existing project archive. A missing
/// archive is empty; corrupt, unsafe or locked paths fail closed. In particular,
/// this never invokes `ArchiveStore::new`, mkdir, chmod, or lock-file creation.
pub fn export_in(base: &Path, scope_key: &str) -> io::Result<Vec<Value>> {
    match existing_store(base, scope_key)? {
        Some(store) => store.export_records(),
        None => Ok(Vec::new()),
    }
}

/// Absolute path of the file holding segment `id`, or `None`. Opens nothing
/// for writing, creates nothing and takes no lock (files are published by
/// atomic rename). The file is the validated record of `id`, which may be a
/// tombstone, or become one later; a corrupt or unsafe path fails closed.
pub fn archive_file(base: &Path, scope_key: &str, id: &str) -> io::Result<Option<PathBuf>> {
    validate_id(id)?;
    let Some(store) = existing_store(base, scope_key)? else {
        return Ok(None);
    };
    let directory = base.join("context-archives").join(&store.scope);
    for shelf in store.shelves()? {
        let found = store.locate(std::slice::from_ref(&shelf), id, true)?;
        if found.is_some() {
            return Ok(Some(directory.join(shelf).join(archive_name(id))));
        }
    }
    let (legacy, _) = store.scan_legacy(true)?;
    let slot = legacy.into_iter().find(|(_, record)| record.id == id);
    Ok(slot.map(|(slot, _)| directory.join(slot_name(slot))))
}

/// A project archive that is already there, opened read-only: unlike
/// `ArchiveStore::new` this never runs mkdir, chmod or lock-file creation.
fn existing_store(base: &Path, scope_key: &str) -> io::Result<Option<ArchiveStore>> {
    validate_key(scope_key)?;
    let scope = digest_parts(&[scope_key.as_bytes()]);
    let fs = match PrivateDir::open_existing(base, &scope) {
        Ok(fs) => fs,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(Some(ArchiveStore {
        fs,
        scope,
        logical: String::new(), // reads all logical conversations in this scope
        redactor: None,
        abort: None,
        wait: LOCK_WAIT,
        process_wait: PROCESS_LOCK_WAIT,
    }))
}

/// Project-wide archive lookup; base and scope are trusted host values, never
/// model-supplied paths. The same budgets and disclosure policy apply.
pub fn project_search(
    base: &Path,
    scope_key: &str,
    query: &str,
    limit: usize,
) -> io::Result<Vec<ArchiveDescriptor>> {
    ArchiveStore::new(base, scope_key, "project-reader")?.search(query, limit)
}

/// Fetch an opaque reference from any logical conversation in this project.
pub fn project_fetch(
    base: &Path,
    scope_key: &str,
    id: &str,
    start: usize,
    limit: usize,
) -> io::Result<Vec<SharedMessage>> {
    ArchiveStore::new(base, scope_key, "project-reader")?.fetch(id, start, limit)
}

fn eligible_class(value: &Value) -> Option<DisclosureClass> {
    // Provider/UI envelopes sometimes label private reasoning as assistant text.
    // Do not rely on the role or content-block type alone.
    if value
        .get("channel")
        .is_some_and(|v| !matches!(v.as_str(), Some("final")))
        || value.get("content_class").is_some_and(|v| {
            !matches!(
                v.as_str(),
                Some("user_text" | "assistant_text" | "tool_calls" | "tool_results")
            )
        })
        || value
            .get("private")
            .is_some_and(|v| v != &Value::Bool(false))
    {
        return None;
    }
    for key in ["sensitivity", "retention", "retention_class"] {
        if let Some(v) = value.get(key) {
            if !matches!(v.as_str(), Some("normal" | "standard" | "model_visible")) {
                return None;
            }
        }
    }
    let mut class = DisclosureClass::ModelVisible;
    for key in ["disclosure", "disclosure_class"] {
        if let Some(v) = value.get(key) {
            let parsed = DisclosureClass::parse(v.as_str()?)?;
            if !may_persist(parsed)
                || !matches!(
                    parsed,
                    DisclosureClass::ModelVisible | DisclosureClass::ModelVisibleAfterRedaction
                )
            {
                return None;
            }
            class = parsed;
        }
    }
    Some(class)
}

/// Shared message-envelope selection for preflight and projection. Host
/// continuation notes and the host's tool-in-flight note are derived
/// working context, never source evidence (nor the user's words).
fn projection_content(message: &Value) -> Option<(DisclosureClass, &str, &Value)> {
    if message.get("_synaps_context").is_some()
        || crate::core::host_note::is_tool_in_flight_note(message)
    {
        return None;
    }
    let class = eligible_class(message)?;
    let role @ ("user" | "assistant") = message.get("role")?.as_str()? else {
        return None;
    };
    Some((class, role, message.get("content")?))
}

fn identifier<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    let text = value.get(key)?.as_str()?;
    (!text.is_empty()
        && text.len() <= 128
        && !credential_like(text)
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b)))
    .then_some(text)
}

/// Conservatively withhold an entire field rather than risk partial redaction.
fn credential_like(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "password",
        "passwd",
        "api_key",
        "apikey",
        "api-key",
        "access_token",
        "refresh_token",
        "secret",
        "authorization",
        "bearer ",
        "private key",
        "sk-",
        "ghp_",
        "github_pat_",
        "xoxb-",
        "akia",
        "eyjh",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

struct InputBudget {
    bytes: usize,
    nodes: usize,
    reduce: Reduce,
}
impl InputBudget {
    fn node(&mut self, depth: usize) -> io::Result<()> {
        self.nodes += 1;
        if depth > 32 || self.nodes > 65536 {
            return Err(over("archive input structural budget exceeded"));
        }
        Ok(())
    }

    fn charge(&mut self, bytes: usize) -> io::Result<()> {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes > MAX_INPUT_BYTES {
            return Err(over("archive input byte budget exceeded"));
        }
        Ok(())
    }

    /// One tool input against a budget of its own: its cost when admitted
    /// (`None`: withheld whole), an error when it is over a budget by itself.
    fn alone(input: &Value) -> io::Result<Option<usize>> {
        let mut budget = InputBudget {
            bytes: 0,
            nodes: 0,
            reduce: Reduce::None,
        };
        let bytes = budget.input(input, 3)?;
        if let Some(bytes) = bytes {
            budget.charge(bytes)?;
        }
        Ok(bytes)
    }

    // Only selected fields are charged; ignored metadata is opaque. Include the
    // old per-node/key allowance, never more than the corresponding raw subtree.
    fn field(&mut self, key: &str, text: &str, depth: usize) -> io::Result<()> {
        self.node(depth)?;
        self.charge(key.len().saturating_add(8).saturating_add(text.len()))
    }

    fn text_field(
        &mut self,
        key: &str,
        text: &str,
        class: DisclosureClass,
        redactor: bool,
        depth: usize,
    ) -> io::Result<()> {
        if class == DisclosureClass::ModelVisibleAfterRedaction && !redactor {
            return self.node(depth); // screen() returns the fixed placeholder
        }
        self.field(key, text, depth)
    }

    /// Allocation-free semantic preflight. Complete for ALL messages before
    /// invoking a redactor or materializing the projection. Keep the selected
    /// branches in sync with Projector; skipped payload descendants are opaque.
    fn message(&mut self, message: &Value, redactor: bool) -> io::Result<()> {
        self.node(0)?;
        let Some((class, role, content)) = projection_content(message) else {
            return Ok(());
        };
        self.charge(8)?;
        self.field("role", role, 1)?;
        if let Some(text) = content.as_str() {
            return self.text_field("content", text, class, redactor, 1);
        }
        self.node(1)?;
        let Some(blocks) = content.as_array() else {
            return Ok(());
        };
        self.charge("content".len() + 8)?;
        for block in blocks {
            self.node(2)?;
            let Some(mut block_class) = eligible_class(block) else {
                continue;
            };
            if class == DisclosureClass::ModelVisibleAfterRedaction {
                block_class = class;
            }
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        self.charge(8)?;
                        self.field("type", "text", 3)?;
                        self.text_field("text", text, block_class, redactor, 3)?;
                    }
                }
                Some("tool_use") if role == "assistant" => {
                    if let (Some(id), Some(name)) =
                        (identifier(block, "id"), identifier(block, "name"))
                    {
                        self.charge(8)?;
                        self.field("type", "tool_use", 3)?;
                        self.field("id", id, 3)?;
                        self.field("name", name, 3)?;
                        if name != "context_checkpoint" && redactor {
                            let input = block.get("input");
                            if let Some(input) = input.filter(|input| !self.reduce.skips(input)) {
                                if let Some(bytes) = self.input(input, 3)? {
                                    self.charge("input".len().saturating_add(bytes))?;
                                }
                            }
                        }
                    }
                }
                Some("tool_result") if role == "user" => {
                    if let Some(id) = identifier(block, "tool_use_id") {
                        self.charge(8)?;
                        self.field("type", "tool_result", 3)?;
                        self.field("tool_use_id", id, 3)?;
                        if block.get("is_error").and_then(Value::as_bool).is_some() {
                            self.field("is_error", "", 3)?;
                        }
                        if redactor {
                            if let Some(content) = block.get("content") {
                                self.tool_result(content, block_class)?;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn tool_result(&mut self, content: &Value, class: DisclosureClass) -> io::Result<()> {
        if let Some(text) = content.as_str() {
            return self.text_field("content", text, class, true, 3);
        }
        self.node(3)?;
        if let Some(blocks) = content.as_array() {
            self.charge("content".len() + 8)?;
            for block in blocks {
                self.node(4)?;
                if let Some(class) = eligible_class(block) {
                    if block["type"] == "text" {
                        if let Some(text) = block["text"].as_str() {
                            self.charge(8)?;
                            self.field("type", "text", 5)?;
                            self.text_field("text", text, class, true, 5)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Guarded whole-argument admission, shared with Projector. Byte accounting
    /// is provisional until every visited child is eligible: a later excluded
    /// child withholds the WHOLE input, regardless of tentative byte cost.
    /// Structural visits are never rolled back. Arbitrary tool JSON is not a
    /// content-block array; unclassified data/base64 fields remain eligible.
    fn input(&mut self, value: &Value, depth: usize) -> io::Result<Option<usize>> {
        self.node(depth)?;
        if eligible_class(value).is_none() {
            return Ok(None);
        }
        let mut bytes: usize = 8;
        match value {
            Value::String(text) => bytes = bytes.saturating_add(text.len()),
            Value::Array(values) => {
                for value in values {
                    let Some(cost) = self.input(value, depth + 1)? else {
                        return Ok(None);
                    };
                    bytes = bytes.saturating_add(cost);
                }
            }
            Value::Object(values) => {
                for (key, value) in values {
                    let Some(cost) = self.input(value, depth + 1)? else {
                        return Ok(None);
                    };
                    bytes = bytes.saturating_add(key.len()).saturating_add(cost);
                }
            }
            _ => {}
        }
        Ok(Some(bytes))
    }
}

fn projection_digest(
    scope: &str,
    count: usize,
    messages: &[ArchivedMessage],
) -> io::Result<String> {
    let bytes = bounded_json(messages, MAX_SEGMENT_BYTES)?;
    Ok(digest_parts(&[
        scope.as_bytes(),
        &(count as u64).to_be_bytes(),
        &bytes,
    ]))
}
fn digest_parts(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    format!("{:x}", hash.finalize())
}
fn bounded_json<T: Serialize + ?Sized>(value: &T, limit: usize) -> io::Result<Vec<u8>> {
    struct Sink {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit - self.bytes.len() {
                return Err(over("archive serialized byte budget exceeded"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut sink = Sink {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut sink, value).map_err(json_error)?;
    Ok(sink.bytes)
}
/// The length of `value` as JSON, without building it.
fn json_len(value: &Value) -> usize {
    struct Count(usize);
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    // Cannot fail: the sink takes everything and a `Value` always serializes.
    let _ = serde_json::to_writer(&mut count, value);
    count.0
}
/// Where a range that does not fit one segment is cut: its midpoint, moved
/// forward past tool results so that a call and the result that follows it
/// stay in one part where they can. A courtesy to readers, not a rule: the
/// store pairs nothing.
fn cut(messages: &[SharedMessage], range: &std::ops::Range<usize>) -> usize {
    let is_result = |message: &Value| {
        let blocks = message["content"].as_array();
        message["role"] == "user"
            && blocks.is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"))
    };
    let middle = range.start + range.len() / 2;
    (middle..range.end)
        .find(|&index| !is_result(&messages[index]))
        .unwrap_or(middle)
}
fn find_record(records: Vec<(usize, Record)>, id: &str) -> io::Result<Record> {
    records
        .into_iter()
        .find(|(_, r)| r.id == id)
        .map(|(_, r)| r)
        .ok_or_else(not_found)
}
/// File name of a numbered legacy slot in the project directory.
fn slot_name(slot: usize) -> String {
    format!("{slot:03}.json")
}
fn slot_number(name: &str) -> Option<usize> {
    let slot = name.strip_suffix(".json")?.parse().ok()?;
    (slot < MAX_SEGMENTS && slot_name(slot) == name).then_some(slot)
}
/// File name of segment `id` in its shelf.
fn archive_name(id: &str) -> String {
    format!("{id}.json")
}
fn archive_id(name: &str) -> Option<&str> {
    name.strip_suffix(".json")
        .filter(|id| validate_id(id).is_ok())
}
/// Shelves are `s` and six or more digits, numbered from 1 in creation order.
fn shelf_name(number: u64) -> String {
    format!("s{number:06}")
}
/// Only the one spelling `shelf_name` produces is a shelf.
fn shelf_number(name: &str) -> Option<u64> {
    let number = name.strip_prefix('s')?.parse().ok()?;
    (shelf_name(number) == name).then_some(number)
}
/// The name of one write's temporary file. No two writes have the same, so no
/// write removes, reuses or renames the file of another. Readers open segment
/// and slot names only, never this.
#[cfg(unix)]
fn temp_name() -> String {
    format!("pending.{}.tmp", uuid::Uuid::new_v4().simple())
}
/// Whether `name` is a temporary file's: one of `temp_name`'s, or the one
/// name (`pending.tmp`) every write shared before.
fn is_temp_name(name: &str) -> bool {
    name.starts_with("pending.") && name.ends_with(".tmp")
}
/// Remove what unfinished writes left in `dir`: of `names`, its listing, the
/// temporary files last written more than [`STALE_TEMP`] ago. A younger one
/// may belong to a write that is still running, here or in another process,
/// and is left to it; so is anything under such a name that is not a plain
/// file of ours. Called by a seal, under the lock, for the shelf it writes
/// to. A leftover costs space and nothing else, so a failure is only logged.
fn sweep(dir: &PrivateDir, names: &[(String, bool, i64)]) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as i128);
    for (name, is_dir, modified) in names {
        let stale = now - i128::from(*modified) > STALE_TEMP.as_nanos() as i128;
        if is_temp_name(name) && !is_dir && stale {
            if let Err(error) = dir.discard(name) {
                tracing::warn!(file = %name, %error, "stale temporary archive file not removed");
            }
        }
    }
}
fn prefix(text: &str, bytes: usize) -> &str {
    let mut end = bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
fn validate_key(key: &str) -> io::Result<()> {
    if key.is_empty() || key.len() > 4096 {
        return Err(invalid("archive scope/logical id must be 1..4096 bytes"));
    }
    Ok(())
}

fn validate_id(id: &str) -> io::Result<()> {
    if id.len() != 32
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Err(invalid("invalid opaque archive id"))
    } else {
        Ok(())
    }
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn corrupt() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid archive record")
}
fn not_found() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "archive not available in this scope",
    )
}
fn canceled() -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "archive operation canceled before it started",
    )
}
fn json_error(_: serde_json::Error) -> io::Error {
    over("archive JSON serialization failed or exceeded budget")
}
/// The payload of an error that says one segment's budget was exceeded: the
/// only kind of error that sealing fewer messages, or less of one message,
/// can cure. It prints as its message.
#[derive(Debug)]
struct OverBudget(&'static str);
impl std::fmt::Display for OverBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for OverBudget {}
fn over(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, OverBudget(message))
}
fn over_budget(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<OverBudget>())
}

// ConfinedDir has no public directory fsync API. Keep a private directory fd
// using the same openat/O_NOFOLLOW policy and modes as private_fs. Every method
// takes a single name inside this directory, never a path.
#[cfg(unix)]
struct PrivateDir(std::fs::File);
#[cfg(unix)]
impl PrivateDir {
    fn new(base: &Path, scope: &str) -> io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        if !base.is_absolute() || base == Path::new("/") {
            return Err(invalid("archive base must be an absolute non-root path"));
        }
        let mut dir = Self(
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open("/")?,
        );
        let components: Vec<_> = base.components().collect();
        for (index, component) in components.iter().enumerate() {
            match component {
                std::path::Component::RootDir => {}
                std::path::Component::Normal(name) => {
                    let name = name
                        .to_str()
                        .ok_or_else(|| invalid("archive path must be UTF-8"))?;
                    dir = dir.child(name, index == components.len() - 1)?;
                }
                _ => return Err(invalid("archive base contains traversal")),
            }
        }
        dir = dir.child("context-archives", true)?;
        dir.child(scope, true)
    }
    fn open_existing(base: &Path, scope: &str) -> io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        if !base.is_absolute() || base == Path::new("/") {
            return Err(invalid("archive base must be an absolute non-root path"));
        }
        let mut dir = Self(
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open("/")?,
        );
        for component in base.components() {
            match component {
                std::path::Component::RootDir => {}
                std::path::Component::Normal(name) => {
                    let name = c_name(
                        name.to_str()
                            .ok_or_else(|| invalid("archive path must be UTF-8"))?,
                    )?;
                    dir = Self(dir.open(&name, libc::O_RDONLY | libc::O_DIRECTORY)?);
                }
                _ => return Err(invalid("archive base contains traversal")),
            }
        }
        for name in ["context-archives", scope] {
            dir = Self(dir.open(&c_name(name)?, libc::O_RDONLY | libc::O_DIRECTORY)?);
        }
        Ok(dir)
    }

    fn fd(&self) -> libc::c_int {
        use std::os::fd::AsRawFd;
        self.0.as_raw_fd()
    }
    fn child(&self, name: &str, private: bool) -> io::Result<Self> {
        use crate::core::private_fs::DIR_MODE;
        use std::os::unix::fs::PermissionsExt;
        let name = c_name(name)?;
        // `mode_t` is u16 on macOS, u32 on Linux; DIR_MODE (0o700) fits both.
        let created =
            unsafe { libc::mkdirat(self.fd(), name.as_ptr(), DIR_MODE as libc::mode_t) } == 0;
        if !created {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EEXIST) {
                return Err(error);
            }
        }
        let file = self.open(&name, libc::O_RDONLY | libc::O_DIRECTORY)?;
        if private || created {
            file.set_permissions(std::fs::Permissions::from_mode(DIR_MODE))?;
        }
        // A previous mkdir may have succeeded but its parent fsync failed.
        // EEXIST on retry is not evidence that the directory entry is durable.
        self.sync()?;
        Ok(Self(file))
    }
    /// An existing child directory, or `None`. A link is refused, never
    /// followed. Unless `readonly`, its mode is made private again, as `child`
    /// does.
    fn open_child(&self, name: &str, readonly: bool) -> io::Result<Option<Self>> {
        use crate::core::private_fs::DIR_MODE;
        use std::os::unix::fs::PermissionsExt;
        let file = match self.open(&c_name(name)?, libc::O_RDONLY | libc::O_DIRECTORY) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !readonly {
            file.set_permissions(std::fs::Permissions::from_mode(DIR_MODE))?;
        }
        Ok(Some(Self(file)))
    }
    /// Direct children as (name, is a directory, modification time in ns), in
    /// no order. Nothing is followed, so a link is not a directory. Names that
    /// are not valid components are left out; a failed read is an error, never
    /// a shorter listing.
    fn names(&self) -> io::Result<Vec<(String, bool, i64)>> {
        use std::os::fd::IntoRawFd;
        // The stream owns its own handle on this directory, opened afresh so
        // that listings share no read position (a duplicate would).
        let fd = self
            .open(c".", libc::O_RDONLY | libc::O_DIRECTORY)?
            .into_raw_fd();
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(error);
        }
        let names = self.read_names(stream);
        unsafe { libc::closedir(stream) }; // closes its handle too
        names
    }
    /// The rest of `names`: every entry `stream` has left, which must be a
    /// stream on this directory.
    fn read_names(&self, stream: *mut libc::DIR) -> io::Result<Vec<(String, bool, i64)>> {
        let mut names = Vec::new();
        loop {
            // `readdir` returns null at the end of the directory and when the
            // read failed, and only `errno` tells the two apart: clear it,
            // call, look. (Not `readdir_r` into a `dirent` of our own: musl
            // copies an entry of any length into it, and glibc fails the
            // whole listing on a name that does not fit.)
            unsafe { *errno() = 0 };
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                return match unsafe { *errno() } {
                    0 => Ok(names),
                    code => Err(io::Error::from_raw_os_error(code)),
                };
            }
            // The name is read where the C library keeps it, up to its NUL
            // and however long it is. No `dirent` is copied, and no reference
            // to a whole one is made: the entry may be shorter than the type.
            let (raw, name) = unsafe {
                let raw = std::ptr::addr_of!((*entry).d_name).cast::<libc::c_char>();
                (raw, std::ffi::CStr::from_ptr(raw))
            };
            let Some(name) = name.to_str().ok().filter(|name| c_name(name).is_ok()) else {
                continue;
            };
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            let found =
                unsafe { libc::fstatat(self.fd(), raw, &mut stat, libc::AT_SYMLINK_NOFOLLOW) };
            if found != 0 {
                let error = io::Error::last_os_error();
                // Gone since it was listed: whoever opens it next decides.
                if error.kind() == io::ErrorKind::NotFound {
                    continue;
                }
                return Err(error);
            }
            let kind = stat.st_mode & libc::S_IFMT;
            #[allow(clippy::useless_conversion)]
            let modified = i64::from(stat.st_mtime)
                .saturating_mul(1_000_000_000)
                .saturating_add(i64::from(stat.st_mtime_nsec));
            names.push((name.to_owned(), kind == libc::S_IFDIR, modified));
        }
    }
    fn open(&self, name: &std::ffi::CStr, flags: i32) -> io::Result<std::fs::File> {
        use std::os::fd::FromRawFd;
        let fd = unsafe {
            libc::openat(
                self.fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                crate::core::private_fs::FILE_MODE,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { std::fs::File::from_raw_fd(fd) })
        }
    }
    fn check_readonly(file: &std::fs::File) -> io::Result<()> {
        Self::check_links(file, 1)
    }
    /// The check for a read that changes nothing and may hold no lock. A
    /// forget can rename its tombstone over the name between that read's open
    /// and this check; the record the read opened is still whole and then has
    /// no name at all. Only a second name (a hard link) is refused here; where
    /// the store is about to write, `check` still wants exactly one.
    fn check_unlocked(file: &std::fs::File) -> io::Result<()> {
        Self::check_links(file, 0)
    }
    fn check_links(file: &std::fs::File, fewest: u64) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        let named = (fewest..=1).contains(&meta.nlink());
        if !meta.is_file() || !named || meta.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "archive requires owned regular single-link files",
            ));
        }
        Ok(())
    }
    fn check(file: &std::fs::File) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        Self::check_readonly(file)?;
        file.set_permissions(std::fs::Permissions::from_mode(
            crate::core::private_fs::FILE_MODE,
        ))
    }
    /// Directory identity for the in-process gate.
    fn identity(&self) -> io::Result<DirIdentity> {
        use std::os::unix::fs::MetadataExt;
        let meta = self.0.metadata()?;
        Ok((meta.dev(), meta.ino()))
    }
    fn lock_existing(&self) -> io::Result<Option<FlockGuard>> {
        let file = match self.open(&c_name("lock")?, libc::O_RDONLY) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        Self::check_readonly(&file)?;
        if !fs4::fs_std::FileExt::try_lock_shared(&file)? {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, BUSY_EXPORT));
        }
        Ok(Some(FlockGuard(file)))
    }
    /// Never waits: a lock held by another process fails closed at once;
    /// `ArchiveStore::exclusive` polls it.
    fn lock(&self) -> io::Result<FlockGuard> {
        let file = self.open(&c_name("lock")?, libc::O_RDWR | libc::O_CREAT)?;
        Self::check(&file)?;
        if !fs4::fs_std::FileExt::try_lock_exclusive(&file)? {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, BUSY));
        }
        Ok(FlockGuard(file))
    }
    fn read_with(
        &self,
        name: &str,
        remaining: usize,
        readonly: bool,
    ) -> io::Result<Option<Vec<u8>>> {
        let file = match self.open(&c_name(name)?, libc::O_RDONLY) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        Self::read_open(file, remaining, readonly).map(Some)
    }
    /// The rest of `read_with` once the file is open: whatever happens to its
    /// name from here on, these are the bytes of the file that was opened.
    /// `readonly` changes nothing on disk.
    fn read_open(file: std::fs::File, remaining: usize, readonly: bool) -> io::Result<Vec<u8>> {
        if readonly {
            Self::check_unlocked(&file)?;
        } else {
            Self::check(&file)?;
        }
        let limit = remaining.min(MAX_SEGMENT_BYTES);
        if file.metadata()?.len() > limit as u64 {
            return Err(corrupt());
        }
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(corrupt());
        }
        Ok(bytes)
    }
    /// Write `bytes` to a temporary file of this call's own, sync it and
    /// rename it onto `name`: a name only ever points at a whole file. With
    /// `create_new` the rename cannot replace, so an existing `name` is
    /// `AlreadyExists` and is left as it is, whatever it is. Without it
    /// (forget) the file at `name` is replaced.
    fn publish(&self, name: &str, bytes: &[u8], create_new: bool) -> io::Result<()> {
        let target = c_name(name)?;
        // Created new under a name no other write has: nothing else writes
        // to this file, removes it while it is fresh or renames it.
        let tmp = c_name(&temp_name())?;
        let mut file = self.open(&tmp, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
        let result = (|| {
            file.write_all(bytes)?;
            file.sync_all()?;
            if create_new {
                self.rename_new(&tmp, &target)?;
            } else {
                // Refuse a target that is a symlink or has a second link.
                match self.open(&target, libc::O_RDONLY) {
                    Ok(existing) => Self::check(&existing)?,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                self.rename(&tmp, &target)?;
            }
            self.sync()
        })();
        if result.is_err() {
            // This call's own file, or nothing if it was renamed already.
            unsafe {
                libc::unlinkat(self.fd(), tmp.as_ptr(), 0);
            }
        }
        result
    }
    /// Rename `from` onto `to` unless there is a `to`, which is then
    /// `AlreadyExists` whatever it is (a link is not followed): one step,
    /// in the kernel, that holds with or without the archive lock.
    fn rename_new(&self, from: &std::ffi::CStr, to: &std::ffi::CStr) -> io::Result<()> {
        // The system call itself: libc declares the `renameat2` wrapper for
        // glibc only (which has it from 2.28) and not for musl. Linux 3.15.
        #[cfg(target_os = "linux")]
        let code = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                self.fd(),
                from.as_ptr(),
                self.fd(),
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        let code = unsafe {
            libc::renameatx_np(
                self.fd(),
                from.as_ptr(),
                self.fd(),
                to.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            if code == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            // A kernel without the call (ENOSYS) or a filesystem without the
            // flag (EINVAL, ENOTSUP) is the only way on to the two steps.
            let unsupported = [libc::ENOSYS, libc::EINVAL, libc::ENOTSUP];
            if !error.raw_os_error().is_some_and(|code| unsupported.contains(&code)) {
                return Err(error);
            }
        }
        self.rename_probed(from, to)
    }
    /// `rename_new` where nothing renames without replacing: look, then
    /// rename. Two steps, which only the caller's lock makes one.
    fn rename_probed(&self, from: &std::ffi::CStr, to: &std::ffi::CStr) -> io::Result<()> {
        match self.open(to, libc::O_RDONLY) {
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "archive file already exists",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => self.rename(from, to),
            Err(error) => Err(error),
        }
    }
    /// The plain rename, which replaces.
    fn rename(&self, from: &std::ffi::CStr, to: &std::ffi::CStr) -> io::Result<()> {
        match unsafe { libc::renameat(self.fd(), from.as_ptr(), self.fd(), to.as_ptr()) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }
    /// Remove the file `name` if it is a plain file of ours with no other
    /// name. A directory, a link or a FIFO under that name is left where it
    /// is.
    fn discard(&self, name: &str) -> io::Result<()> {
        let name = c_name(name)?;
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let found = unsafe {
            libc::fstatat(self.fd(), name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW)
        };
        if found != 0 {
            return Err(io::Error::last_os_error());
        }
        let ours = stat.st_uid == unsafe { libc::geteuid() } && stat.st_nlink == 1;
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG || !ours {
            return Ok(());
        }
        match unsafe { libc::unlinkat(self.fd(), name.as_ptr(), 0) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }
    fn sync(&self) -> io::Result<()> {
        self.0.sync_all()
    }
}
// Where the C library keeps this thread's `errno`: the only thing that tells a
// failed `readdir` from the end of a directory. Each platform names it
// differently and `libc` has no common name for it; a Unix that is not listed
// here does not compile until its name is added.
#[cfg(target_os = "linux")]
use libc::__errno_location as errno;
#[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
use libc::__error as errno;
#[cfg(any(target_os = "android", target_os = "netbsd", target_os = "openbsd"))]
use libc::__errno as errno;
#[cfg(unix)]
fn c_name(name: &str) -> io::Result<std::ffi::CString> {
    if name.is_empty() || matches!(name, "." | "..") || name.contains(['/', '\\']) {
        return Err(invalid("invalid archive component"));
    }
    std::ffi::CString::new(name).map_err(|_| invalid("invalid archive component"))
}

#[cfg(not(unix))]
struct PrivateDir;
#[cfg(not(unix))]
impl PrivateDir {
    fn unsupported<T>() -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private context archive requires Unix",
        ))
    }
    fn new(_: &Path, _: &str) -> io::Result<Self> {
        Self::unsupported()
    }
    fn identity(&self) -> io::Result<DirIdentity> {
        Self::unsupported()
    }
    fn lock(&self) -> io::Result<FlockGuard> {
        Self::unsupported()
    }
    fn open_existing(_: &Path, _: &str) -> io::Result<Self> {
        Self::unsupported()
    }
    fn child(&self, _: &str, _: bool) -> io::Result<Self> {
        Self::unsupported()
    }
    fn open_child(&self, _: &str, _: bool) -> io::Result<Option<Self>> {
        Self::unsupported()
    }
    fn names(&self) -> io::Result<Vec<(String, bool, i64)>> {
        Self::unsupported()
    }
    fn lock_existing(&self) -> io::Result<Option<FlockGuard>> {
        Self::unsupported()
    }
    fn read_with(&self, _: &str, _: usize, _: bool) -> io::Result<Option<Vec<u8>>> {
        Self::unsupported()
    }
    fn publish(&self, _: &str, _: &[u8], _: bool) -> io::Result<()> {
        Self::unsupported()
    }
    fn discard(&self, _: &str) -> io::Result<()> {
        Self::unsupported()
    }
    fn sync(&self) -> io::Result<()> {
        Self::unsupported()
    }
}

#[cfg(test)]
mod projection_tests {
    use super::*;

    fn excluded_fixture(blob: &str) -> Vec<SharedMessage> {
        vec![
            Arc::new(json!({"role":"system","content":blob})),
            Arc::new(json!({"role":"developer","content":blob})),
            Arc::new(json!({"role":"user","_synaps_context":{"note":blob},"content":blob})),
            Arc::new(json!({"role":"user","retention":"local_only","content":blob})),
            Arc::new(json!({"role":"assistant","metadata":blob,"content":[
                {"type":"thinking","thinking":blob},
                {"type":"text","private":true,"text":blob},
                {"type":"image","source":{"data":blob}},
                {"type":"document","source":{"data":blob}},
                {"type":"text","text":" exact evidence αβ\n","metadata":blob},
                {"type":"tool_use","id":"cp","name":"context_checkpoint","input":{"note":blob}}
            ]})),
            Arc::new(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"r1","metadata":blob,"content":[
                    {"type":"image","source":{"data":blob}},
                    {"type":"text","sensitivity":"secret","text":blob},
                    {"type":"text","text":"exact tool evidence\n","metadata":blob}
                ]}
            ]})),
        ]
    }

    #[test]
    fn excluded_aggregate_does_not_consume_eligible_budget_or_change_digest() {
        // Every blob fits by itself; collectively the raw input exceeds 16MiB.
        let messages = excluded_fixture(&"x".repeat(2 * 1024 * 1024));
        let source = messages.clone();
        for redactor in [None, Some(Arc::new(str::to_owned) as Arc<ArchiveRedactor>)] {
            let small =
                project_archive("excluded", &excluded_fixture("x"), "note", redactor.clone())
                    .unwrap();
            let large = project_archive("excluded", &messages, "note", redactor).unwrap();
            assert!(small == large);
            assert_eq!(large.source_message_count, 6);
            assert_eq!(large.messages[0].source_index, 4);
            assert_eq!(large.messages[0].block_indices, vec![4, 5]);
        }
        for (old, new) in source.iter().zip(&messages) {
            assert!(Arc::ptr_eq(old, new));
        }
    }

    #[test]
    fn eligible_aggregate_refuses_before_any_redactor_call() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let redactor: Arc<ArchiveRedactor> = Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            "small".to_owned()
        });
        let piece = "x".repeat(MAX_INPUT_BYTES / 2);
        for messages in [
            vec![
                Arc::new(json!({"role":"user","content":piece})),
                Arc::new(json!({"role":"assistant","content":piece})),
            ],
            vec![Arc::new(json!({"role":"user","content":[{
                "type":"tool_result","tool_use_id":"r1","content":[
                    {"type":"text","text":piece}, {"type":"text","text":piece}
                ]
            }]}))],
        ] {
            let before = messages.clone();
            let error = project_archive("overflow", &messages, "", Some(redactor.clone()))
                .err()
                .unwrap();
            assert!(error.to_string().contains("input byte budget"));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(before, messages);
        }
    }

    #[test]
    fn large_tool_results_exact_with_redactor_opaque_without_it() {
        let text = "evidence αβ\n".repeat(250_000);
        let make = |text: &str| {
            vec![Arc::new(json!({"role":"user","content":[{
                "type":"tool_result","tool_use_id":"r1","content":text
            }]}))]
        };
        let p = project_archive("logs", &make(&text), "", Some(Arc::new(str::to_owned))).unwrap();
        assert_eq!(p.messages[0].message["content"][0]["content"], text);
        let huge = make(&"x".repeat(MAX_INPUT_BYTES + 1));
        let p = project_archive("logs", &huge, "", None).unwrap();
        assert_eq!(p.messages[0].message["content"][0]["content"], WITHHELD);
        assert!(project_archive("logs", &huge, "", Some(Arc::new(str::to_owned))).is_err());
    }

    #[test]
    fn whole_argument_withholding_discards_tentative_bytes_not_structural_guards() {
        let huge = "x".repeat(MAX_INPUT_BYTES + 1);
        let make = |input: Value| {
            vec![Arc::new(json!({"role":"assistant","content":[{
                "type":"tool_use","id":"r1","name":"read","input":input
            }]}))]
        };
        // Both orders: the otherwise eligible field can precede the exclusion.
        for input in [
            json!({"a":huge,"z":{"sensitivity":"secret","body":huge}}),
            json!({"a":{"sensitivity":"secret","body":huge},"z":huge}),
        ] {
            let p =
                project_archive("input", &make(input), "", Some(Arc::new(str::to_owned))).unwrap();
            assert_eq!(
                p.messages[0].message["content"][0]["input"],
                json!({"_archive_withheld":true})
            );
        }
        // Arbitrary binary-looking tool arguments are NOT content blocks.
        let input = json!({"type":"image","data":"YWJj"});
        let p = project_archive(
            "input",
            &make(input.clone()),
            "",
            Some(Arc::new(str::to_owned)),
        )
        .unwrap();
        assert_eq!(p.messages[0].message["content"][0]["input"], input);
        let large = make(json!({"type":"image","data":huge}));
        assert!(project_archive("input", &large, "", Some(Arc::new(str::to_owned))).is_err());
        assert!(project_archive("input", &large, "", None).is_ok());
    }

    #[test]
    fn visited_structure_is_bounded_but_excluded_descendants_are_opaque() {
        let mut deep = json!("leaf");
        for _ in 0..40 {
            deep = json!([deep]);
        }
        let make = |input: Value| {
            vec![Arc::new(json!({"role":"assistant","content":[{
                "type":"tool_use","id":"r1","name":"read","input":input
            }]}))]
        };
        let identity = Some(Arc::new(str::to_owned) as Arc<ArchiveRedactor>);
        assert!(project_archive("depth", &make(deep.clone()), "", identity.clone()).is_err());
        assert!(project_archive("depth", &make(deep.clone()), "", None).is_ok());
        assert!(project_archive(
            "depth",
            &make(json!({"private":true,"blob":deep})),
            "",
            identity.clone()
        )
        .is_ok());
        // A malformed content block isn't recursively interpreted as a block array.
        assert!(project_archive(
            "depth",
            &[Arc::new(json!({"role":"user","content":deep}))],
            "",
            identity.clone()
        )
        .is_ok());
        let wide = vec![Value::Null; 65536];
        assert!(project_archive("nodes", &make(json!(wide)), "", identity.clone()).is_err());
        let excluded = vec![Arc::new(json!({"role":"user","content":wide}))];
        assert!(project_archive("nodes", &excluded, "", identity).is_err());
    }

    #[test]
    fn withheld_after_redaction_text_is_not_scanned_without_redactor() {
        let messages = vec![Arc::new(json!({"role":"user",
            "disclosure":"model_visible_after_redaction", "content":"x".repeat(MAX_INPUT_BYTES+1)
        }))];
        let p = project_archive("withheld", &messages, "", None).unwrap();
        assert_eq!(p.messages[0].message["content"], WITHHELD);
        assert!(project_archive("withheld", &messages, "", Some(Arc::new(str::to_owned))).is_err());
    }

    #[test]
    fn json_escape_scratch_can_exceed_segment_limit_before_redaction() {
        // 3MiB source ->18MiB JSON, accepted by the old raw preflight and then
        // reduced by the trusted redactor. Do not introduce a new 16MiB scratch cap.
        let input = json!({"text":"\0".repeat(3 * 1024 * 1024)});
        let messages = vec![Arc::new(json!({"role":"assistant","content":[{
            "type":"tool_use","id":"r1","name":"read","input":input
        }]}))];
        let redactor: Arc<ArchiveRedactor> = Arc::new(|s| {
            if s.starts_with('{') {
                assert!(s.len() > MAX_SEGMENT_BYTES);
                "{\"text\":\"screened\"}".to_owned()
            } else {
                s.to_owned()
            }
        });
        let p = project_archive("escaping", &messages, "", Some(redactor)).unwrap();
        assert_eq!(
            p.messages[0].message["content"][0]["input"],
            json!({"text":"screened"})
        );
        assert!(project_archive("escaping", &messages, "", Some(Arc::new(str::to_owned))).is_err());
    }

    fn compatibility_fixture() -> Vec<SharedMessage> {
        vec![
            Arc::new(json!({"role":"system","content":"omitted"})),
            Arc::new(json!({"role":"user","content":" exact αβ\n"})),
            Arc::new(json!({"role":"assistant","content":[
                {"type":"thinking","thinking":"private"},
                {"type":"text","text":"classified-value"},
                {"type":"tool_use","id":"r1","name":"read","input":{"path":"src/lib.rs","n":7,"binary":{"type":"image","data":"YWJj"}}},
                {"type":"tool_use","id":"c1","name":"context_checkpoint","input":{"note":"hidden"}},
                {"type":"tool_use","id":"x1","name":"read","input":{"nested":{"disclosure":"local_only","body":"omitted"}}}
            ]})),
            Arc::new(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"r1","is_error":false,"content":[
                    {"type":"image","data":"omitted"},
                    {"type":"text","text":"exact result\n"},
                    {"type":"text","text":"classified-value","disclosure":"model_visible_after_redaction"}
                ]}
            ]})),
            Arc::new(
                json!({"role":"user","disclosure":"model_visible_after_redaction","content":"classified-value"}),
            ),
            Arc::new(json!({"role":"user","_synaps_context":{},"content":"hidden"})),
        ]
    }

    #[test]
    fn legacy_projection_golden() {
        // Captured against the legacy raw-input preflight before this fix.
        for (redactor, digest) in [
            (
                None,
                "aeb544a71629f741def049575eefb1a765117d65aa914bdcc1657fa677622031",
            ),
            (
                Some(
                    Arc::new(|s: &str| s.replace("classified-value", "[redacted]"))
                        as Arc<ArchiveRedactor>,
                ),
                "d70b321210ca3d2bc531b31a35cc02ee267eeb572e8a6dd88f1fb62725b082ec",
            ),
        ] {
            let p = project_archive(
                "compat-session",
                &compatibility_fixture(),
                "hidden note",
                redactor,
            )
            .unwrap();
            assert_eq!(p.digest, digest);
        }
    }

    #[test]
    fn pure_projection_keeps_legacy_logical_digest_and_never_opens_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let logical = tmp
            .path()
            .join("not-created/session")
            .to_str()
            .unwrap()
            .to_owned();
        let messages = vec![
            Arc::new(json!({"role":"system","content":"OMITTED"})),
            Arc::new(json!({"role":"assistant","content":[
                {"type":"thinking","thinking":"OMITTED"},
                {"type":"text","text":" exact αβ evidence\n"}
            ]})),
        ];
        let source = serde_json::to_vec(&messages).unwrap();
        let first = project_archive(&logical, &messages, "hidden note", None).unwrap();
        assert_eq!(first.logical_id, digest_parts(&[logical.as_bytes()]));
        assert_eq!(
            first.digest,
            projection_digest(&first.logical_id, 2, &first.messages).unwrap()
        );
        assert_eq!(first.messages[0].source_index, 1);
        assert_eq!(first.messages[0].block_indices, vec![1]);
        assert_eq!(first.source_message_count, 2);
        let mut changed = messages.clone();
        changed[0] = Arc::new(json!({"role":"developer","content":"DIFFERENT OMITTED"}));
        let second = project_archive(&logical, &changed, "changed note", None).unwrap();
        assert_eq!(first.digest, second.digest);
        assert_ne!(
            first.digest,
            project_archive("other-session", &messages, "hidden note", None)
                .unwrap()
                .digest
        );
        assert_eq!(serde_json::to_vec(&messages).unwrap(), source);
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn pure_projection_shares_disclosure_redaction_and_input_limits() {
        let redactor: Arc<ArchiveRedactor> =
            Arc::new(|s| s.replace("classified-value", "[redacted]"));
        let messages = vec![
            Arc::new(json!({"role":"assistant","content":[
                {"type":"tool_use","id":"read_1","name":"read","input":{"path":"src/lib.rs"}},
                {"type":"tool_use","id":"cp","name":"context_checkpoint","input":{"note":"NOTE_ONLY"}}
            ]})),
            Arc::new(
                json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"read_1","content":"classified-value"}]}),
            ),
            Arc::new(json!({"role":"user","sensitivity":"secret","content":"OMITTED"})),
            Arc::new(json!({"role":"user","_synaps_context":{},"content":"NOTE_ONLY"})),
            // The host's tool-in-flight note: never archived as user evidence.
            Arc::new(json!({"role":"user","content":crate::core::host_note::tool_in_flight_note_text(
                "Canceled by user",
                &["bash".into()],
            )})),
        ];
        let projected =
            project_archive("session", &messages, "classified-value", Some(redactor)).unwrap();
        assert_eq!(projected.note, "[redacted]");
        let json = serde_json::to_string(&projected.messages).unwrap();
        assert!(!json.contains("classified-value"));
        assert!(!json.contains("NOTE_ONLY"));
        assert!(!json.contains("partially applied"), "the tool note is not archived");
        assert!(!json.contains("OMITTED"));
        assert!(json.contains("src/lib.rs"));
        assert_eq!(projected.messages.len(), 2);
        let withheld = project_archive("session", &messages, "password=bad", None).unwrap();
        assert_eq!(withheld.note, WITHHELD);
        assert!(!serde_json::to_string(&withheld.messages)
            .unwrap()
            .contains("src/lib.rs"));
        assert!(project_archive("", &[], "", None).is_err());
        assert!(project_archive(&"l".repeat(4097), &[], "", None).is_err());
        assert!(project_archive("s", &[], &"n".repeat(MAX_NOTE_BYTES + 1), None).is_err());
        assert!(
            project_archive("s", &vec![messages[0].clone(); MAX_MESSAGES + 1], "", None).is_err()
        );
    }
}

// Archive locks are non-blocking: a concurrently spawned test process can
// briefly hold an inherited lock descriptor until it executes, see
// memory::repository tests.
#[cfg(all(test, unix))]
#[serial_test::parallel(process_spawn)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn msg(role: &str, content: Value) -> SharedMessage {
        Arc::new(json!({"role":role,"content":content}))
    }
    fn setup() -> (tempfile::TempDir, ArchiveStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store =
            ArchiveStore::new(tmp.path(), "synthetic-project", "synthetic-logical").unwrap();
        (tmp, store)
    }
    fn directory(tmp: &tempfile::TempDir, store: &ArchiveStore) -> std::path::PathBuf {
        tmp.path().join("context-archives").join(&store.scope)
    }
    /// Every segment file on the shelves of a project directory, in path order.
    fn shelf_files(dir: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for shelf in std::fs::read_dir(dir).unwrap() {
            let shelf = shelf.unwrap().path();
            if std::fs::symlink_metadata(&shelf).unwrap().is_dir() {
                let entries = std::fs::read_dir(&shelf).unwrap();
                files.extend(entries.map(|entry| entry.unwrap().path()));
            }
        }
        files.retain(|file| file.extension().is_some_and(|extension| extension == "json"));
        files.sort();
        files
    }
    /// The one segment file of a store that has sealed once.
    fn only_archive_file(dir: &Path) -> PathBuf {
        let files = shelf_files(dir);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].parent().unwrap(), dir.join("s000001"));
        files[0].clone()
    }
    /// A segment as stores wrote it before shelves existed: a random id in a
    /// numbered slot of the project directory.
    fn seal_legacy_slot(
        store: &ArchiveStore,
        slot: usize,
        messages: &[SharedMessage],
        note: &str,
    ) -> ArchiveRef {
        let projection =
            project_hashed(&store.logical, messages, note, None, Reduce::None).unwrap();
        let record = Record {
            version: 1,
            scope: store.scope.clone(),
            logical: store.logical.clone(),
            id: uuid::Uuid::new_v4().simple().to_string(),
            digest: projection.digest,
            body: Body::Live {
                source_message_count: projection.source_message_count,
                messages: projection.messages,
                note: projection.note,
            },
        };
        let bytes = bounded_json(&record, MAX_SEGMENT_BYTES).unwrap();
        let _lock = store.exclusive().unwrap();
        store.fs.publish(&slot_name(slot), &bytes, false).unwrap();
        record.reference().unwrap()
    }
    fn text(content: &str) -> Vec<SharedMessage> {
        vec![msg("user", json!(content))]
    }
    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
    /// What tells a file from another one with the same bytes under the same
    /// name: its device and inode.
    fn identity(path: &Path) -> (u64, u64) {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(path).unwrap();
        (meta.dev(), meta.ino())
    }
    /// Every temporary file under a project directory.
    fn temporary_files(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(&directory).unwrap() {
                let path = entry.unwrap().path();
                if is_temp_name(path.file_name().unwrap().to_str().unwrap()) {
                    found.push(path);
                } else if std::fs::symlink_metadata(&path).unwrap().is_dir() {
                    pending.push(path);
                }
            }
        }
        found
    }
    /// Make the plain file `path` one that was last written `age` ago.
    fn written_ago(path: &Path, age: Duration) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(std::time::SystemTime::now() - age).unwrap();
    }
    /// Path -> SHA-256 and identity of every file under a directory: equal
    /// only if no file was changed, added, removed or replaced, even by one
    /// with the same bytes.
    fn snapshot(dir: &Path) -> std::collections::BTreeMap<PathBuf, String> {
        let mut files = std::collections::BTreeMap::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(&directory).unwrap() {
                let path = entry.unwrap().path();
                if std::fs::symlink_metadata(&path).unwrap().is_dir() {
                    pending.push(path);
                } else {
                    let hash = digest_parts(&[&std::fs::read(&path).unwrap()]);
                    let file = format!("{hash} {:?}", identity(&path));
                    files.insert(path, file);
                }
            }
        }
        files
    }

    #[test]
    fn export_preview_is_read_only_and_matches_projection_including_tombstones() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        assert!(export_in(&tmp.path().join("missing"), "synthetic-project")
            .unwrap()
            .is_empty());
        assert!(export_in(tmp.path(), "synthetic-project")
            .unwrap()
            .is_empty());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
        let store =
            ArchiveStore::new(tmp.path(), "synthetic-project", "synthetic-logical").unwrap();
        let dir = directory(&tmp, &store);
        // Even a newly opened empty store has no lock until its first operation.
        assert!(export_in(tmp.path(), "synthetic-project")
            .unwrap()
            .is_empty());
        assert!(!dir.join("lock").exists());
        let messages = vec![msg("user", json!("migration evidence"))];
        let reference = store.seal(&messages, "hidden note").unwrap();
        let projection =
            project_archive("synthetic-logical", &messages, "hidden note", None).unwrap();
        let tombstone = store
            .seal(&[msg("assistant", json!("remove me"))], "remove note")
            .unwrap();
        store.forget(&tombstone.id).unwrap();
        let mut paths = vec![
            tmp.path().to_path_buf(),
            tmp.path().join("context-archives"),
            dir.clone(),
        ];
        paths.extend(std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()));
        paths.extend(shelf_files(&dir));
        assert_eq!(paths.len(), 7, "lock, one shelf and its two segments");
        // Export must not silently repair permissions, unlike normal archive IO.
        for path in &paths {
            let mode = if path.is_dir() { 0o750 } else { 0o640 };
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let snapshot = || {
            paths
                .iter()
                .map(|p| {
                    let m = std::fs::metadata(p).unwrap();
                    (
                        m.mode(),
                        m.mtime(),
                        m.mtime_nsec(),
                        if m.is_file() {
                            std::fs::read(p).unwrap()
                        } else {
                            vec![]
                        },
                    )
                })
                .collect::<Vec<_>>()
        };
        let before = snapshot();
        let rows = export_in(tmp.path(), "synthetic-project").unwrap();
        assert_eq!(rows, store.export_records().unwrap());
        assert_eq!(before, snapshot());
        assert_eq!(rows.len(), 2);
        let row = |id: &str| rows.iter().find(|row| row["id"] == id).unwrap();
        assert_eq!(
            row(&reference.id),
            &json!({"id":reference.id,"digest":projection.digest,"logical_id":projection.logical_id,
            "source_message_count":1,"messages":projection.messages,"note":"hidden note","tombstone":false})
        );
        let forgotten = row(&tombstone.id);
        assert_eq!(forgotten["tombstone"], true);
        assert_eq!(forgotten["source_message_count"], 0);
        assert_eq!(forgotten["messages"], json!([]));
        assert_eq!(forgotten["note"], "");
        let lock = store.fs.lock().unwrap();
        assert_eq!(
            export_in(tmp.path(), "synthetic-project")
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(lock);
        std::fs::write(&shelf_files(&dir)[0], b"corrupt").unwrap();
        assert!(export_in(tmp.path(), "synthetic-project").is_err());
    }

    #[test]
    fn export_preview_refuses_links_and_never_creates_a_missing_lock() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let r = store
            .seal(&[msg("user", json!("evidence"))], "note")
            .unwrap();
        let path = only_archive_file(&dir);
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(dir.join("lock")).unwrap();
        assert!(export_in(tmp.path(), "synthetic-project").is_err());
        assert!(!dir.join("lock").exists());
        // The normal API may recreate its lock, but preview cannot.
        assert!(store.fetch(&r.id, 0, 1).is_ok());
        std::fs::remove_file(&path).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::write(&outside, &bytes).unwrap();
        symlink(&outside, &path).unwrap();
        assert!(export_in(tmp.path(), "synthetic-project").is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(&outside, &path).unwrap();
        assert!(export_in(tmp.path(), "synthetic-project").is_err());
        let alias = tmp.path().join("alias");
        symlink(tmp.path(), &alias).unwrap();
        assert!(export_in(&alias, "synthetic-project").is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), bytes);
    }

    #[test]
    fn attachments_remain_outside_eligible_memory_archive() {
        let (tmp, store) = setup();
        let messages = vec![msg(
            "user",
            json!([
                {"type":"text","text":"Compare the selected files"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"ATTACHMENT_IMAGE_SECRET"}},
                {"type":"document","title":"ATTACHMENT_FILENAME_SECRET","source":{"type":"base64","media_type":"application/pdf","data":"ATTACHMENT_PDF_SECRET"}},
                {"type":"document","source":{"type":"text","media_type":"text/plain","data":"ATTACHMENT_TEXT_SECRET"}}
            ]),
        )];
        let reference = store.seal(&messages, "safe checkpoint").unwrap();
        let fetched = store.fetch(&reference.id, 0, 10).unwrap();
        let serialized = serde_json::to_string(&fetched).unwrap();
        assert!(serialized.contains("Compare the selected files"));
        assert!(!serialized.contains("ATTACHMENT_"));
        let raw = std::fs::read_to_string(only_archive_file(&directory(&tmp, &store))).unwrap();
        assert!(!raw.contains("ATTACHMENT_"));
        assert!(messages[0].to_string().contains("ATTACHMENT_TEXT_SECRET"));
    }

    #[test]
    fn exact_text_source_indices_note_and_restart() {
        let (tmp, store) = setup();
        let messages = vec![
            msg("system", json!("FORBIDDEN SYSTEM")),
            msg("user", json!(" exact\n αβ  ")),
            msg(
                "assistant",
                json!([
                    {"type":"thinking","thinking":"FORBIDDEN REASONING"},
                    {"type":"text","text":"evidence✓", "cache_control":{"type":"ephemeral"}},
                    {"type":"image","data":"FORBIDDEN IMAGE"}
                ]),
            ),
            msg("developer", json!("FORBIDDEN DEVELOPER")),
        ];
        let reference = store.seal(&messages, "hidden routing note").unwrap();
        assert_eq!(reference.message_count, 2);
        assert_eq!(reference.source_message_count, 4);
        let reopened =
            ArchiveStore::new(tmp.path(), "synthetic-project", "synthetic-logical").unwrap();
        let fetched = reopened
            .fetch_with_provenance(&reference.id, 0, 128)
            .unwrap();
        assert_eq!(fetched[0].message, messages[1]);
        assert_eq!(fetched[0].source_index, 1);
        assert_eq!(fetched[1].source_index, 2);
        assert_eq!(fetched[1].block_indices, vec![1]);
        assert_eq!(
            reopened.fetch_note(&reference.id).unwrap(),
            "hidden routing note"
        );
        assert!(reopened.search("hidden routing", 10).unwrap().is_empty());
        assert_eq!(reopened.search("evidence", 10).unwrap()[0].id, reference.id);
        assert!(reopened
            .fetch(&reference.id, usize::MAX, 10)
            .unwrap()
            .is_empty());
        let raw = std::fs::read_to_string(only_archive_file(&directory(&tmp, &store))).unwrap();
        assert!(!raw.contains("FORBIDDEN"));
        assert!(!raw.contains("cache_control"));
        assert_eq!(messages[0]["content"], "FORBIDDEN SYSTEM");
    }

    #[test]
    fn duplicate_retry_ignores_note_but_not_logical_id() {
        let (tmp, store) = setup();
        let messages = vec![msg("user", json!("same eligible words"))];
        let first = store.seal(&messages, "first").unwrap();
        assert_eq!(store.seal(&messages, "second").unwrap(), first);
        assert_eq!(store.fetch_note(&first.id).unwrap(), "first");
        let other = ArchiveStore::new(tmp.path(), "synthetic-project", "other-logical").unwrap();
        let second = other.seal(&messages, "").unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(
            project_search(tmp.path(), "synthetic-project", "", 10)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            project_fetch(tmp.path(), "synthetic-project", &first.id, 0, 10).unwrap(),
            messages
        );
        assert_eq!(other.fetch(&first.id, 0, 1).unwrap(), messages);
        store.forget(&first.id).unwrap();
        assert!(store.seal(&messages, "third").is_err());
        assert_eq!(other.fetch(&second.id, 0, 1).unwrap(), messages);
        assert_eq!(other.seal(&messages, "third").unwrap(), second);
    }

    #[test]
    fn deletion_is_content_free_durable_and_cannot_resurrect() {
        let (tmp, store) = setup();
        let messages = vec![msg("user", json!("uniquely removable content"))];
        let reference = store.seal(&messages, "removable note").unwrap();
        store.forget(&reference.id).unwrap();
        store.forget(&reference.id).unwrap();
        let raw = std::fs::read_to_string(only_archive_file(&directory(&tmp, &store))).unwrap();
        assert!(raw.contains("tombstone"));
        assert!(!raw.contains("removable"));
        assert!(!raw.contains("source_message_count"));
        let store =
            ArchiveStore::new(tmp.path(), "synthetic-project", "synthetic-logical").unwrap();
        assert_eq!(
            store.fetch(&reference.id, 0, 1).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(store.fetch_note(&reference.id).is_err());
        assert!(store.search("", 10).unwrap().is_empty());
        assert_eq!(
            store.seal(&messages, "changed note").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn foreign_scope_copied_records_and_traversal_fail_closed() {
        let (tmp, store) = setup();
        let reference = store
            .seal(&[msg("user", json!("scope evidence"))], "")
            .unwrap();
        let foreign =
            ArchiveStore::new(tmp.path(), "foreign-project", "synthetic-logical").unwrap();
        assert!(foreign.fetch(&reference.id, 0, 1).is_err());
        for id in [
            "../000.json",
            "/tmp/file",
            "",
            "a/../b",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ] {
            assert_eq!(
                store.fetch(id, 0, 1).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert!(store.forget(id).is_err());
        }
        // Copied into the foreign project, as a shelf file or as a legacy slot,
        // the record is invalid there: never fetched, and search passes over it.
        let source = only_archive_file(&directory(&tmp, &store));
        let foreign_dir = directory(&tmp, &foreign);
        std::fs::create_dir(foreign_dir.join("s000001")).unwrap();
        for target in [
            foreign_dir.join("s000001").join(source.file_name().unwrap()),
            foreign_dir.join("000.json"),
        ] {
            std::fs::copy(&source, &target).unwrap();
            assert_eq!(
                foreign.fetch(&reference.id, 0, 1).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert!(foreign.search("", 1).unwrap().is_empty());
            assert!(foreign.export_records().is_err());
            std::fs::remove_file(&target).unwrap();
        }
        assert!(ArchiveStore::new(&tmp.path().join("../escape"), "p", "l").is_err());
    }

    #[test]
    fn disclosure_and_raw_tool_data_are_withheld() {
        let (tmp, store) = setup();
        let mut messages = vec![
            msg(
                "assistant",
                json!([{"type":"tool_use","id":"call_1","name":"read","input":{"path":"unclassified raw argument"}}]),
            ),
            msg(
                "user",
                json!([{"type":"tool_result","tool_use_id":"call_1","content":"unclassified raw output"}]),
            ),
            msg("user", json!("password=synthetic-credential")),
        ];
        for class in [
            "local_only",
            "persist_never_transmit",
            "never_persist",
            "model_visible_after_consent",
            "unknown",
        ] {
            messages.push(Arc::new(
                json!({"role":"user","content":"FORBIDDEN DISCLOSURE","disclosure":class}),
            ));
        }
        messages.push(Arc::new(
            json!({"role":"user","content":"FORBIDDEN SENSITIVITY","sensitivity":"secret"}),
        ));
        let reference = store.seal(&messages, "").unwrap();
        assert_eq!(reference.message_count, 3);
        let raw = std::fs::read_to_string(only_archive_file(&directory(&tmp, &store))).unwrap();
        for forbidden in ["unclassified raw", "synthetic-credential", "FORBIDDEN"] {
            assert!(!raw.contains(forbidden));
        }
        assert!(raw.contains("call_1"));
        assert!(raw.contains("_archive_withheld"));
    }

    #[test]
    fn derived_context_envelope_is_not_searchable_source() {
        let (_tmp, store) = setup();
        let first = store
            .seal(
                &[msg("assistant", json!("original evidence"))],
                "NOTE_ONLY_SENTINEL",
            )
            .unwrap();
        let second=store.seal(&[
            Arc::new(json!({"role":"user","_synaps_context":{"schema":"synaps-context-window/1","archive":first.id},"content":"NOTE_ONLY_SENTINEL"})),
            Arc::new(json!({"role":"assistant","content":[{"type":"tool_use","id":"checkpoint-note","name":"context_checkpoint","input":{"phase":"plan","note":"NOTE_ONLY_SENTINEL"}}]})),
            msg("assistant",json!("second-window evidence")),
        ],"next note").unwrap();
        assert!(store.search("NOTE_ONLY_SENTINEL", 10).unwrap().is_empty());
        let fetched = store.fetch_with_provenance(&second.id, 0, 10).unwrap();
        assert_eq!(fetched.len(), 2);
        assert_eq!(fetched[0].source_index, 1);
        assert_eq!(fetched[1].source_index, 2);
        assert!(!serde_json::to_string(&fetched)
            .unwrap()
            .contains("NOTE_ONLY_SENTINEL"));
    }

    #[test]
    fn checkpoint_tool_note_stays_hidden_even_with_redactor() {
        let (_tmp, store) = setup();
        let store = store.with_redactor(Arc::new(str::to_owned));
        let reference = store
            .seal(
                &[msg(
                    "assistant",
                    json!([
                        {"type":"tool_use","id":"checkpoint-note","name":"context_checkpoint",
                         "input":{"phase":"plan","note":"NOTE_ARGUMENT_SENTINEL"}}
                    ]),
                )],
                "NOTE_ARGUMENT_SENTINEL",
            )
            .unwrap();
        assert!(store
            .search("NOTE_ARGUMENT_SENTINEL", 10)
            .unwrap()
            .is_empty());
        assert!(
            !serde_json::to_string(&store.fetch(&reference.id, 0, 10).unwrap())
                .unwrap()
                .contains("NOTE_ARGUMENT_SENTINEL")
        );
        assert_eq!(
            store.fetch_note(&reference.id).unwrap(),
            "NOTE_ARGUMENT_SENTINEL"
        );
    }

    #[test]
    fn trusted_redactor_retains_exact_eligible_tool_evidence() {
        let (_tmp, store) = setup();
        let store = store.with_redactor(Arc::new(|s| s.replace("classified-value", "[redacted]")));
        let messages = vec![
            msg(
                "assistant",
                json!([{"type":"tool_use","id":"call_1","name":"read","input":{"path":"src/lib.rs","line":7}}]),
            ),
            msg(
                "user",
                json!([{"type":"tool_result","tool_use_id":"call_1","is_error":false,"content":"line 7\n  exact evidence\nclassified-value"}]),
            ),
        ];
        let reference = store.seal(&messages, "").unwrap();
        let fetched = store.fetch(&reference.id, 0, 10).unwrap();
        assert_eq!(fetched[0], messages[0]);
        assert_eq!(
            fetched[1]["content"][0]["content"],
            "line 7\n  exact evidence\n[redacted]"
        );
    }

    #[test]
    fn oversized_input_output_limits_and_depth_leave_source_intact() {
        let (_tmp, store) = setup();
        let huge = vec![msg("user", json!("x".repeat(MAX_INPUT_BYTES + 1)))];
        assert!(store.seal(&huge, "").is_err());
        assert_eq!(
            huge[0]["content"].as_str().unwrap().len(),
            MAX_INPUT_BYTES + 1
        );
        let file_too_large = vec![msg("user", json!("\n".repeat(MAX_SEGMENT_BYTES / 2)))];
        assert!(store.seal(&file_too_large, "").is_err());
        assert!(store
            .seal(&vec![msg("user", json!("a")); MAX_MESSAGES + 1], "")
            .is_err());
        assert!(store.seal(&[], &"n".repeat(MAX_NOTE_BYTES + 1)).is_err());
        let mut deep = json!("value");
        for _ in 0..40 {
            deep = json!([deep]);
        }
        // Unknown content blocks are opaque; admitted tool JSON still has a
        // strict recursion limit before serialization or redaction.
        let deep = vec![msg(
            "assistant",
            json!([{
                "type":"tool_use", "id":"r1", "name":"read", "input":deep
            }]),
        )];
        let store = store.with_redactor(Arc::new(str::to_owned));
        assert!(store.seal(&deep, "").is_err());
        assert!(store.search(&"q".repeat(513), 1).is_err());
        assert!(store.search("", MAX_SEARCH_RESULTS + 1).is_err());
        assert!(store
            .fetch(&"a".repeat(32), 0, MAX_FETCH_MESSAGES + 1)
            .is_err());
        assert!(store.search("", 10).unwrap().is_empty());
    }

    #[test]
    fn torn_final_fails_closed_stale_temporary_is_not_visible() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let reference = store.seal(&text("complete"), "").unwrap();
        let file = only_archive_file(&dir);
        let stale = dir.join("s000001").join("pending.tmp");
        std::fs::write(&stale, b"unpublished partial").unwrap();
        written_ago(&stale, STALE_TEMP * 2);
        assert_eq!(store.search("", 2).unwrap().len(), 1);
        let other = store.seal(&text("another"), "").unwrap();
        assert!(!stale.exists());
        std::fs::write(&file, b"{\"version\":1,").unwrap();
        for result in [
            store.fetch(&reference.id, 0, 1).map(drop),
            store.fetch_note(&reference.id).map(drop),
            store.forget(&reference.id),
            // The torn file is neither trusted as the same projection nor replaced.
            store.seal(&text("complete"), "").map(drop),
        ] {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
        assert_eq!(std::fs::read(&file).unwrap(), b"{\"version\":1,");
        // Other segments do not depend on it.
        assert_eq!(store.fetch(&other.id, 0, 1).unwrap(), text("another"));
        assert!(store.seal(&text("next"), "").is_ok());
    }

    #[test]
    fn private_modes_symlink_hardlink_fifo_and_lock_refusal() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let reference = store.seal(&text("private"), "").unwrap();
        let file = only_archive_file(&dir);
        for path in [
            tmp.path().to_path_buf(),
            tmp.path().join("context-archives"),
            dir.clone(),
            dir.join("s000001"),
        ] {
            assert_eq!(mode(&path), 0o700);
        }
        for path in [&file, &dir.join("lock")] {
            assert_eq!(mode(path), 0o600);
        }
        // Loosened modes are made private again by the next seal or forget,
        // but not by the read-only path lookup.
        std::fs::set_permissions(dir.join("s000001"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let located = archive_file(tmp.path(), "synthetic-project", &reference.id).unwrap();
        assert_eq!(located, Some(file.clone()));
        assert_eq!((mode(&dir.join("s000001")), mode(&file)), (0o755, 0o644));
        store.seal(&text("private"), "").unwrap();
        assert_eq!((mode(&dir.join("s000001")), mode(&file)), (0o700, 0o600));
        let victim = tmp.path().join("synthetic-victim");
        std::fs::write(&victim, "do not change").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
        let refused = || {
            assert!(store.search("", 1).is_err());
            assert!(store.fetch(&reference.id, 0, 1).is_err());
            assert!(store.fetch_note(&reference.id).is_err());
            assert!(store.forget(&reference.id).is_err());
            // The same projection would be published under this very name.
            assert!(store.seal(&text("private"), "").is_err());
            assert!(store.export_records().is_err());
            assert!(archive_file(tmp.path(), "synthetic-project", &reference.id).is_err());
            assert_eq!(std::fs::read_to_string(&victim).unwrap(), "do not change");
            assert_eq!(mode(&victim), 0o644);
        };
        std::fs::remove_file(&file).unwrap();
        symlink(&victim, &file).unwrap();
        refused();
        std::fs::remove_file(&file).unwrap();
        std::fs::hard_link(&victim, &file).unwrap();
        refused();
        std::fs::remove_file(&file).unwrap();
        let fifo = std::ffi::CString::new(file.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        refused();
        let alias = tmp.path().join("alias");
        symlink(tmp.path().join("context-archives"), &alias).unwrap();
        assert!(ArchiveStore::new(&alias.join("nested"), "p", "l").is_err());
        std::fs::remove_file(dir.join("lock")).unwrap();
        symlink(&victim, dir.join("lock")).unwrap();
        assert!(store.fs.lock().is_err());
    }

    #[test]
    fn lock_free_read_does_not_change_file_mode() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let reference = store.seal(&text("private"), "note").unwrap();
        let (shelf, file) = (dir.join("s000001"), only_archive_file(&dir));
        std::fs::set_permissions(&shelf, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(store.fetch(&reference.id, 0, 1).unwrap(), text("private"));
        assert_eq!(store.fetch_note(&reference.id).unwrap(), "note");
        assert_eq!(store.search("private", 1).unwrap().len(), 1);
        assert_eq!((mode(&shelf), mode(&file)), (0o755, 0o644));
        // The next operation that holds the lock repairs both.
        assert_eq!(store.seal(&text("private"), "").unwrap(), reference);
        assert_eq!((mode(&shelf), mode(&file)), (0o700, 0o600));
    }

    /// A fetch and a search write nothing: no mode, no change time, no lock
    /// file. So they also work where nothing can be written.
    #[test]
    fn lock_free_reads_write_nothing_and_work_in_unwritable_directories() {
        use std::collections::BTreeMap;
        use std::os::unix::fs::MetadataExt;
        type Stamp = (u32, (i64, i64), (i64, i64));
        /// Mode, change time and modification time of everything under
        /// `root`, itself included.
        fn stamps(root: &Path) -> BTreeMap<PathBuf, Stamp> {
            let mut found = BTreeMap::new();
            let mut pending = vec![root.to_path_buf()];
            while let Some(path) = pending.pop() {
                let meta = std::fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    let entries = std::fs::read_dir(&path).unwrap();
                    pending.extend(entries.map(|entry| entry.unwrap().path()));
                }
                let changed = (meta.ctime(), meta.ctime_nsec());
                let modified = (meta.mtime(), meta.mtime_nsec());
                found.insert(path, (meta.mode(), changed, modified));
            }
            found
        }
        /// Lets the temporary directory be removed again, on every path.
        struct Writable(Vec<PathBuf>);
        impl Drop for Writable {
            fn drop(&mut self) {
                for dir in &self.0 {
                    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
                }
            }
        }
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let reference = store.seal(&text("read only"), "note").unwrap();
        let forgotten = store.seal(&text("forgotten"), "").unwrap();
        store.forget(&forgotten.id).unwrap();
        let file = dir.join("s000001").join(archive_name(&reference.id));
        // With no lock file, taking the lock would mean creating one.
        std::fs::remove_file(dir.join("lock")).unwrap();
        let context = tmp.path().join("context-archives");
        let dirs = vec![tmp.path().to_path_buf(), context, dir.clone(), dir.join("s000001")];
        let _writable = Writable(dirs.clone());
        for path in &dirs {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o500)).unwrap();
        }
        for path in shelf_files(&dir) {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o400)).unwrap();
        }
        // Whatever is changed from here on gets a later change time.
        std::thread::sleep(Duration::from_millis(50));
        let before = stamps(tmp.path());
        assert_eq!(before.len(), 6, "four directories and two segment files");

        assert_eq!(store.fetch(&reference.id, 0, 1).unwrap(), text("read only"));
        assert_eq!(store.fetch_note(&reference.id).unwrap(), "note");
        let rows = store.search("", 8).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, reference.id);
        for id in [forgotten.id.clone(), "a".repeat(32)] {
            let error = store.fetch(&id, 0, 1).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
        }
        let located = archive_file(tmp.path(), "synthetic-project", &reference.id);
        assert_eq!(located.unwrap(), Some(file));
        assert_eq!(stamps(tmp.path()), before);
    }

    #[test]
    fn a_shelf_that_is_a_link_or_a_file_is_refused_not_followed() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let reference = store.seal(&text("shelved"), "").unwrap();
        let shelf = dir.join("s000001");
        let outside = tmp.path().join("outside-shelf");
        std::fs::rename(&shelf, &outside).unwrap();
        let before = snapshot(&outside);
        let refused = || {
            assert!(store.fetch(&reference.id, 0, 1).is_err());
            assert!(store.fetch_note(&reference.id).is_err());
            assert!(store.search("", 1).is_err());
            assert!(store.forget(&reference.id).is_err());
            assert!(store.seal(&text("shelved"), "").is_err());
            assert!(store.seal(&text("something new"), "").is_err());
            assert!(store.export_records().is_err());
            assert!(export_in(tmp.path(), "synthetic-project").is_err());
            assert!(archive_file(tmp.path(), "synthetic-project", &reference.id).is_err());
        };
        symlink(&outside, &shelf).unwrap();
        refused();
        std::fs::remove_file(&shelf).unwrap();
        std::fs::write(&shelf, b"not a directory").unwrap();
        refused();
        // Nothing was read into, written through or created beside the link.
        assert_eq!(snapshot(&outside), before);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2, "lock and s000001");
        // A newer shelf does not make an unsafe older one acceptable.
        std::fs::create_dir(dir.join("s000002")).unwrap();
        refused();
    }

    #[test]
    fn multi_mib_context_roundtrips_without_truncation() {
        let (_tmp, store) = setup();
        let text = "eligible exact evidence\n".repeat(400_000);
        assert!(text.len() > 8 * 1024 * 1024);
        let messages = vec![msg("user", json!(text))];
        let reference = store.seal(&messages, "large source window").unwrap();
        assert_eq!(store.fetch(&reference.id, 0, 1).unwrap(), messages);
        assert_eq!(store.seal(&messages, "retry").unwrap(), reference);
    }

    #[test]
    fn private_reasoning_disguised_as_assistant_text_is_omitted() {
        let (_tmp, store) = setup();
        let messages = vec![
            Arc::new(json!({"role":"assistant","channel":"analysis","content":"private analysis"})),
            Arc::new(
                json!({"role":"assistant","content_class":"private_reasoning","content":"private reasoning"}),
            ),
            Arc::new(json!({"role":"assistant","private":true,"content":"private text"})),
            Arc::new(json!({"role":"assistant","channel":"final","content":"public evidence"})),
        ];
        let reference = store.seal(&messages, "").unwrap();
        assert_eq!(reference.message_count, 1);
        let fetched = store.fetch_with_provenance(&reference.id, 0, 1).unwrap();
        assert_eq!(fetched[0].source_index, 3);
        assert_eq!(fetched[0].message["content"], "public evidence");
    }

    #[test]
    fn after_redaction_requires_host_redactor_including_parent_class() {
        let (_tmp, store) = setup();
        let messages = vec![Arc::new(json!({
            "role":"assistant", "disclosure":"model_visible_after_redaction",
            "content":[{"type":"text", "text":"sensitive host classified material"}]
        }))];
        let reference = store.seal(&messages, "").unwrap();
        let fetched = store.fetch(&reference.id, 0, 1).unwrap();
        assert_eq!(fetched[0]["content"][0]["text"], WITHHELD);
    }

    #[test]
    fn bounded_search_utf8() {
        let (_tmp, store) = setup();
        for index in 0..MAX_SEARCH_RESULTS + 8 {
            let words = format!("{index} {}", "é".repeat(200));
            store.seal(&text(&words), "").unwrap();
        }
        let descriptors = store.search("", MAX_SEARCH_RESULTS).unwrap();
        assert_eq!(descriptors.len(), MAX_SEARCH_RESULTS);
        assert!(descriptors.iter().all(|d| d.snippet.len() <= 256));
    }

    #[test]
    fn seal_129_succeed() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let seal = |index: usize| store.seal(&text(&format!("archive {index}")), "").unwrap();
        let mut ids = vec![seal(0).id];
        let first = only_archive_file(&dir);
        let bytes = std::fs::read(&first).unwrap();
        ids.extend((1..=SHELF_FILES).map(|index| seal(index).id));
        assert_eq!(ids.len(), 129);
        assert_eq!(first, dir.join("s000001").join(archive_name(&ids[0])));
        for (index, id) in ids.iter().enumerate() {
            assert_eq!(
                store.fetch(id, 0, 1).unwrap(),
                text(&format!("archive {index}"))
            );
        }
        assert_eq!(std::fs::read(&first).unwrap(), bytes);
        // The 129th starts the second shelf; nothing was put in a legacy slot.
        let files = shelf_files(&dir);
        assert_eq!(files.len(), 129);
        assert_eq!(files[128], dir.join("s000002").join(archive_name(&ids[128])));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 3, "lock and two shelves");
        // Forgetting does not make room in a full shelf, and none is needed.
        store.forget(&ids[0]).unwrap();
        let next = seal(129);
        assert_eq!(shelf_files(&dir).len(), 130);
        assert!(dir.join("s000002").join(archive_name(&next.id)).is_file());
    }

    #[test]
    #[ignore = "seals 10,000 archives (30,000 fsyncs): 4 s on tmpfs, far longer on a real disk"]
    fn seal_10000_succeed() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let seal = |index: usize| store.seal(&text(&format!("archive {index}")), "").unwrap();
        let ids: Vec<_> = (0..10_000).map(|index| seal(index).id).collect();
        let shelves = store.shelves().unwrap();
        assert_eq!(shelves.len(), 79);
        assert_eq!(shelves[0], "s000079");
        assert_eq!(shelf_files(&dir).len(), 10_000);
        for index in [0, 9_999] {
            assert_eq!(
                store.fetch(&ids[index], 0, 1).unwrap(),
                text(&format!("archive {index}"))
            );
        }
        let started = Instant::now();
        store.fetch(&ids[0], 0, 1).unwrap();
        eprintln!("fetch of the oldest of 10,000: {:?}", started.elapsed());
    }

    #[test]
    fn no_seal_replaces_a_file() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        seal_legacy_slot(&store, 0, &text("legacy kept"), "");
        let forgotten = seal_legacy_slot(&store, 1, &text("legacy forgotten"), "");
        store.forget(&forgotten.id).unwrap();
        let mut before = snapshot(&dir);
        assert_eq!(before.len(), 3, "lock and two legacy slots");
        for index in 0..300 {
            store.seal(&text(&format!("archive {index}")), "").unwrap();
            let after = snapshot(&dir);
            for (path, hash) in &before {
                assert_eq!(after.get(path), Some(hash), "seal {index} touched {path:?}");
            }
            assert_eq!(after.len(), before.len() + 1);
            before = after;
        }
        // Neither does sealing something again, forgotten or not.
        store.seal(&text("archive 7"), "changed note").unwrap();
        assert!(store.seal(&text("legacy forgotten"), "").is_err());
        assert_eq!(snapshot(&dir), before);
    }

    #[test]
    #[ignore = "writes and re-reads about 280 MiB: about 55 s in a debug build"]
    fn byte_cap_is_gone() {
        let (tmp, store) = setup();
        let large = |index: usize| {
            let filler = "x".repeat(15 * 1024 * 1024 + 512 * 1024);
            text(&format!("archive-{index:02} {filler}"))
        };
        // 18 segments of 15.5 MiB: 279 MiB, where the store used to stop at 256.
        let ids: Vec<_> = (0..18)
            .map(|index| store.seal(&large(index), "").unwrap().id)
            .collect();
        let files = shelf_files(&directory(&tmp, &store));
        let stored: u64 = files.iter().map(|f| f.metadata().unwrap().len()).sum();
        assert!(stored > MAX_STORE_BYTES as u64 + MAX_SEGMENT_BYTES as u64);
        for (index, id) in ids.iter().enumerate() {
            let fetched = store.fetch(id, 0, 1).unwrap();
            let content = fetched[0]["content"].as_str().unwrap();
            assert!(content.starts_with(&format!("archive-{index:02} ")));
            assert_eq!(content.len(), large(index)[0]["content"].as_str().unwrap().len());
        }
        // The search budget is bytes as well as files: 17 files pass 256 MiB.
        let (rows, stopped) = store.search_scan("archive-00", 1).unwrap();
        assert!(rows.is_empty());
        assert_eq!(stopped, Some(17));
        let (rows, stopped) = store.search_scan("archive-17", 1).unwrap();
        assert_eq!((rows[0].id.as_str(), stopped), (ids[17].as_str(), None));
        // A migration cannot carry it, and says so instead of exporting a part.
        let error = store.export_records().unwrap_err();
        assert!(error.to_string().contains("exceeds the migration limit"));
    }

    #[test]
    fn legacy_slots_read_by_new_code() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let a = seal_legacy_slot(&store, 0, &text("legacy alpha"), "note a");
        let b = seal_legacy_slot(&store, 1, &text("legacy beta"), "note b");
        let c = seal_legacy_slot(&store, 2, &text("legacy gamma"), "note c");
        store.forget(&c.id).unwrap();
        let slots = ["000.json", "001.json", "002.json"].map(|name| dir.join(name));
        assert!(std::fs::read_to_string(&slots[2]).unwrap().contains("tombstone"));
        let kept = std::fs::read(&slots[1]).unwrap();
        assert_eq!(store.fetch(&a.id, 0, 1).unwrap(), text("legacy alpha"));
        assert_eq!(store.fetch_note(&b.id).unwrap(), "note b");
        assert_eq!(
            store.fetch(&c.id, 0, 1).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        let found = |query: &str| -> Vec<String> {
            let rows = store.search(query, 10).unwrap();
            rows.into_iter().map(|row| row.id).collect()
        };
        assert_eq!(found("legacy"), [b.id.as_str(), a.id.as_str()]);
        assert_eq!(
            archive_file(tmp.path(), "synthetic-project", &b.id).unwrap(),
            Some(slots[1].clone())
        );
        store.forget(&a.id).unwrap();
        store.forget(&a.id).unwrap();
        assert!(std::fs::read_to_string(&slots[0]).unwrap().contains("tombstone"));
        assert_eq!(found(""), [b.id.as_str()]);
        assert!(shelf_files(&dir).is_empty());
        // Only what is sealed from now on goes to a shelf, under its digest id.
        let new = store.seal(&text("after shelves"), "").unwrap();
        assert_eq!(
            only_archive_file(&dir),
            dir.join("s000001").join(archive_name(&new.id))
        );
        assert_eq!(found(""), [new.id.as_str(), b.id.as_str()]);
        assert_eq!(std::fs::read(&slots[1]).unwrap(), kept);
        assert!(slots.iter().all(|slot| slot.is_file()));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 5);
        assert_eq!(store.export_records().unwrap().len(), 4);
    }

    #[test]
    fn legacy_full_store_accepts_seal() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let legacy: Vec<_> = (0..MAX_SEGMENTS)
            .map(|slot| seal_legacy_slot(&store, slot, &text(&format!("legacy {slot}")), ""))
            .collect();
        let before = snapshot(&dir);
        let new = store.seal(&text("the 129th"), "").unwrap();
        assert_eq!(store.fetch(&new.id, 0, 1).unwrap(), text("the 129th"));
        assert_eq!(
            only_archive_file(&dir),
            dir.join("s000001").join(archive_name(&new.id))
        );
        for slot in [0, MAX_SEGMENTS - 1] {
            assert_eq!(
                store.fetch(&legacy[slot].id, 0, 1).unwrap(),
                text(&format!("legacy {slot}"))
            );
        }
        let after = snapshot(&dir);
        assert!(before.iter().all(|(path, hash)| after.get(path) == Some(hash)));
        // Newest first: the shelf, then the highest slot.
        let rows = store.search("", 2).unwrap();
        assert_eq!(rows[0].id, new.id);
        assert_eq!(rows[1].id, legacy[MAX_SEGMENTS - 1].id);
    }

    #[test]
    fn legacy_duplicate_and_tombstone_still_honoured() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let live = seal_legacy_slot(&store, 0, &text("sealed long ago"), "first note");
        let gone = seal_legacy_slot(&store, 1, &text("forgotten long ago"), "");
        store.forget(&gone.id).unwrap();
        let (again, note) = store
            .seal_with_note(&text("sealed long ago"), "second note")
            .unwrap();
        assert_eq!((again, note.as_str()), (live, "first note"));
        assert_eq!(
            store.seal(&text("forgotten long ago"), "").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(shelf_files(&dir).is_empty());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 3, "lock and two slots");
    }

    /// Legacy slots are still read under the lock, and only they are.
    #[test]
    fn legacy_fetch_waits_for_the_lock() {
        let (_tmp, mut store) = setup();
        let legacy = seal_legacy_slot(&store, 0, &text("sealed long ago"), "old note");
        let shelved = store.seal(&text("sealed today"), "").unwrap();
        let hold = Duration::from_millis(300);
        let started = Instant::now();
        let foreign = store.fs.lock().unwrap();
        let holder = std::thread::spawn(move || {
            std::thread::sleep(hold);
            drop(foreign);
        });
        assert_eq!(store.fetch(&legacy.id, 0, 1).unwrap(), text("sealed long ago"));
        assert!(started.elapsed() >= hold);
        holder.join().unwrap();

        // Held past the deadline: busy, for what has to read a slot.
        store.process_wait = Duration::from_millis(100);
        let _foreign = store.fs.lock().unwrap();
        let started = Instant::now();
        for result in [
            store.fetch(&legacy.id, 0, 1).map(drop),
            store.fetch_note(&legacy.id).map(drop),
            // On no shelf, so it may be in a slot.
            store.fetch(&"a".repeat(32), 0, 1).map(drop),
            // The shelves do not fill this page.
            store.search("", 2).map(drop),
        ] {
            let error = result.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            assert_eq!(error.to_string(), BUSY);
        }
        assert!(started.elapsed() >= store.process_wait * 4);
        // What the shelves answer never gets as far as the slots.
        assert_eq!(store.fetch(&shelved.id, 0, 1).unwrap(), text("sealed today"));
        assert_eq!(store.search("", 1).unwrap()[0].id, shelved.id);
    }

    #[test]
    fn duplicate_seal_returns_same_id_across_shelves() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let seal = |index: usize, note: &str| {
            let words = format!("archive {index}");
            store.seal_with_note(&text(&words), note).unwrap()
        };
        let first = seal(0, "first note");
        for index in 1..=SHELF_FILES {
            seal(index, "");
        }
        let before = snapshot(&dir);
        assert_eq!(shelf_files(&dir).len(), 129);
        // The newest shelf is the second; the first projection is on the first.
        assert_eq!(seal(0, "another note"), first);
        assert_eq!(seal(SHELF_FILES, "another note").1, "");
        assert_eq!(snapshot(&dir), before);
        assert_eq!(first.0.id.len(), 32);
        let digest = project_archive("synthetic-logical", &text("archive 0"), "", None)
            .unwrap()
            .digest;
        assert_eq!(first.0.id, digest[..32]);
    }

    #[test]
    fn forgotten_projection_cannot_be_resealed() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let reference = store.seal(&text("forget me"), "note").unwrap();
        store.forget(&reference.id).unwrap();
        let before = snapshot(&dir);
        for note in ["note", "another note"] {
            assert_eq!(
                store.seal(&text("forget me"), note).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
        }
        assert_eq!(snapshot(&dir), before);
        // Another conversation's identical words are another projection.
        let other = ArchiveStore::new(tmp.path(), "synthetic-project", "other-logical").unwrap();
        assert!(other.seal(&text("forget me"), "").is_ok());
        assert!(store.seal(&text("remember me"), "").is_ok());
    }

    #[test]
    fn forget_is_in_place_and_idempotent() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let kept = store.seal(&text("keep me"), "").unwrap();
        let reference = store.seal(&text("forget me"), "note").unwrap();
        let file = dir.join("s000001").join(archive_name(&reference.id));
        let neighbour = dir.join("s000001").join(archive_name(&kept.id));
        let untouched = snapshot(&dir)[&neighbour].clone();
        store.forget(&reference.id).unwrap();
        assert_eq!(shelf_files(&dir).len(), 2, "same name, same shelf");
        let tombstone = std::fs::read_to_string(&file).unwrap();
        assert!(tombstone.contains("tombstone") && !tombstone.contains("forget me"));
        let written = file.metadata().unwrap().modified().unwrap();
        // Again: nothing is rewritten.
        store.forget(&reference.id).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), tombstone);
        assert_eq!(file.metadata().unwrap().modified().unwrap(), written);
        assert!(temporary_files(&dir).is_empty());
        assert_eq!(std::fs::read_dir(dir.join("s000001")).unwrap().count(), 2);
        assert_eq!(snapshot(&dir)[&neighbour], untouched);
        assert_eq!(store.fetch(&kept.id, 0, 1).unwrap(), text("keep me"));
        assert_eq!(
            store.forget(&"a".repeat(32)).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn fetch_reads_one_file() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let ids: Vec<_> = (0..300)
            .map(|index| store.seal(&text(&format!("archive {index}")), "note").unwrap().id)
            .collect();
        // On the oldest shelf, behind two newer ones.
        let target = dir.join("s000001").join(archive_name(&ids[5]));
        for file in shelf_files(&dir) {
            if file != target {
                std::fs::write(file, b"corrupt").unwrap();
            }
        }
        assert_eq!(store.fetch(&ids[5], 0, 1).unwrap(), text("archive 5"));
        assert_eq!(store.fetch_note(&ids[5]).unwrap(), "note");
        assert_eq!(store.seal(&text("archive 5"), "").unwrap().id, ids[5]);
        assert!(store.seal(&text("archive 300"), "").is_ok());
        store.forget(&ids[5]).unwrap();
        assert_eq!(
            store.fetch(&ids[6], 0, 1).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn search_is_newest_first_and_budgeted() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let epoch = std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        // Inside a shelf the order is by modification time, which a fast
        // filesystem clock does not tell apart: give every file its own.
        let seal = |index: usize| {
            let words = match index {
                0 => "needle oldest-only".to_owned(),
                _ => format!("needle {index}"),
            };
            let id = store.seal(&text(&words), "").unwrap().id;
            let shelf = dir.join(shelf_name((index / SHELF_FILES + 1) as u64));
            let file = std::fs::File::options()
                .write(true)
                .open(shelf.join(archive_name(&id)))
                .unwrap();
            file.set_modified(epoch + Duration::from_secs(index as u64))
                .unwrap();
            id
        };
        let mut ids: Vec<_> = (0..100).map(&seal).collect();
        let (rows, stopped) = store.search_scan("oldest-only", 32).unwrap();
        assert_eq!((rows.len(), rows[0].id.as_str(), stopped), (1, ids[0].as_str(), None));

        ids.extend((100..300).map(&seal));
        let rows = store.search("needle", 32).unwrap();
        let newest: Vec<_> = rows.iter().map(|row| row.id.as_str()).collect();
        let expected: Vec<_> = ids.iter().rev().take(32).map(String::as_str).collect();
        assert_eq!(newest, expected);
        // A full page is not a budget stop.
        assert_eq!(store.search_scan("needle", 32).unwrap().1, None);

        // Exactly the budget: every file is read, so nothing is left unread.
        ids.extend((300..SEARCH_SCAN_FILES).map(&seal));
        let (rows, stopped) = store.search_scan("oldest-only", 32).unwrap();
        assert_eq!((rows.len(), stopped), (1, None));
        // One more, and the oldest is beyond it.
        ids.push(seal(SEARCH_SCAN_FILES));
        let (rows, stopped) = store.search_scan("oldest-only", 32).unwrap();
        assert!(rows.is_empty());
        assert_eq!(stopped, Some(SEARCH_SCAN_FILES));
        assert_eq!(store.search("needle", 1).unwrap()[0].id, ids[SEARCH_SCAN_FILES]);
        assert_eq!(store.fetch(&ids[0], 0, 1).unwrap(), text("needle oldest-only"));
    }

    #[test]
    fn search_skips_a_corrupt_file() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let ids: Vec<_> = ["first", "second", "third"]
            .iter()
            .map(|words| store.seal(&text(words), "").unwrap().id)
            .collect();
        let legacy = seal_legacy_slot(&store, 0, &text("legacy first"), "");
        seal_legacy_slot(&store, 1, &text("legacy second"), "");
        std::fs::write(dir.join("s000001").join(archive_name(&ids[1])), b"corrupt").unwrap();
        std::fs::write(dir.join("001.json"), b"{}").unwrap();
        // Larger than any segment: not even read.
        let huge = std::fs::File::create(dir.join("s000001").join(archive_name(&"f".repeat(32))));
        huge.unwrap().set_len(MAX_SEGMENT_BYTES as u64 + 1).unwrap();
        // Not segment names: never opened.
        std::fs::write(dir.join("s000001").join("notes.json"), b"stray").unwrap();
        std::fs::write(dir.join("s0000001"), b"stray").unwrap();
        let (rows, stopped) = store.search_scan("", 10).unwrap();
        let mut found: Vec<_> = rows.into_iter().map(|row| row.id).collect();
        found.sort();
        let mut expected = vec![ids[0].clone(), ids[2].clone(), legacy.id];
        expected.sort();
        assert_eq!(found, expected);
        assert_eq!(stopped, None);
        assert_eq!(store.search("second", 10).unwrap(), []);
    }

    #[test]
    fn file_name_must_match_record_id() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let a = store.seal(&text("the original"), "").unwrap();
        let b = "b".repeat(32);
        let copy = dir.join("s000001").join(archive_name(&b));
        std::fs::copy(only_archive_file(&dir), &copy).unwrap();
        for result in [
            store.fetch(&b, 0, 1).map(drop),
            store.fetch_note(&b).map(drop),
            store.forget(&b),
            archive_file(tmp.path(), "synthetic-project", &b).map(drop),
        ] {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
        let original = dir.join("s000001").join(archive_name(&a.id));
        assert_eq!(std::fs::read(&copy).unwrap(), std::fs::read(original).unwrap());
        // Search lists the original once and passes over the copy.
        let rows = store.search("", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, a.id);
        assert_eq!(store.fetch(&a.id, 0, 1).unwrap(), text("the original"));
    }

    #[test]
    fn shelf_gap_is_tolerated() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let seal = |index: usize| store.seal(&text(&format!("archive {index}")), "").unwrap().id;
        let ids: Vec<_> = (0..2 * SHELF_FILES + 1).map(seal).collect();
        assert_eq!(store.shelves().unwrap(), ["s000003", "s000002", "s000001"]);
        std::fs::remove_dir_all(dir.join("s000002")).unwrap();
        assert_eq!(store.fetch(&ids[256], 0, 1).unwrap(), text("archive 256"));
        assert_eq!(store.fetch(&ids[0], 0, 1).unwrap(), text("archive 0"));
        assert_eq!(
            store.fetch(&ids[200], 0, 1).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        // The next seal goes to the newest shelf; the gap is not filled in.
        let next = seal(1_000);
        assert!(dir.join("s000003").join(archive_name(&next)).is_file());
        assert_eq!(store.shelves().unwrap(), ["s000003", "s000001"]);
        assert_eq!(store.search("", 1).unwrap().len(), 1);
    }

    #[test]
    fn crash_leftovers() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let first = store.seal(&text("before the crash"), "").unwrap();
        // A crash after mkdir and after writing the temporary file, before
        // the rename: an empty newer shelf, and in it a temporary file of
        // this version and one of the version before, both from long ago.
        let empty = dir.join("s000002");
        std::fs::create_dir(&empty).unwrap();
        let stale = [empty.join(temp_name()), empty.join("pending.tmp")];
        // Written a moment ago: perhaps by a write that is still running.
        // And one a minute short of the age at which no write can be.
        let fresh = [empty.join(temp_name()), empty.join(temp_name())];
        for path in stale.iter().chain(&fresh) {
            std::fs::write(path, b"unpublished partial").unwrap();
        }
        let minute = Duration::from_secs(60);
        written_ago(&stale[0], STALE_TEMP + minute);
        written_ago(&stale[1], STALE_TEMP * 24);
        written_ago(&fresh[1], STALE_TEMP - minute);
        // No reader sees any of them, before or after.
        assert_eq!(store.search("", 10).unwrap().len(), 1);
        assert_eq!(store.export_records().unwrap().len(), 1);
        assert_eq!(temporary_files(&dir).len(), 4);
        let next = store.seal(&text("after the crash"), "").unwrap();
        assert!(empty.join(archive_name(&next.id)).is_file());
        // The seal removed what no write can still be using, and only that.
        assert!(stale.iter().all(|path| !path.exists()));
        for path in &fresh {
            assert_eq!(std::fs::read(path).unwrap(), b"unpublished partial");
        }
        assert_eq!(temporary_files(&dir).len(), 2);
        assert_eq!(store.fetch(&first.id, 0, 1).unwrap(), text("before the crash"));
        assert_eq!(store.fetch(&next.id, 0, 1).unwrap(), text("after the crash"));
        assert_eq!(store.search("", 10).unwrap().len(), 2);
        assert_eq!(store.export_records().unwrap().len(), 2);
        // A forget writes beside them too, and leaves nothing of its own.
        store.forget(&next.id).unwrap();
        assert_eq!(temporary_files(&dir).len(), 2);
    }

    /// The one temporary name every write used to share, taken by something
    /// no write made. Each used to stop every seal into that directory (a
    /// directory cannot be unlinked) or was removed without a look. Now no
    /// write uses the name, and the sweep removes plain files only.
    #[test]
    fn a_temporary_name_that_is_a_directory_a_fifo_or_a_link_is_left_alone() {
        use std::os::unix::fs::FileTypeExt;
        /// As `written_ago`, for a path that cannot be opened for writing;
        /// the time of a link itself, not of what it points to.
        fn aged(path: &Path) {
            let then = std::time::SystemTime::now() - STALE_TEMP * 2;
            let since = then.duration_since(std::time::UNIX_EPOCH).unwrap();
            let mut time: libc::timespec = unsafe { std::mem::zeroed() };
            time.tv_sec = since.as_secs() as libc::time_t;
            let name = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
            let (times, flags) = ([time; 2], libc::AT_SYMLINK_NOFOLLOW);
            let set = unsafe { libc::utimensat(libc::AT_FDCWD, name.as_ptr(), times.as_ptr(), flags) };
            assert_eq!(set, 0);
        }
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let first = store.seal(&text("first"), "").unwrap();
        let legacy = seal_legacy_slot(&store, 0, &text("legacy"), "");
        let victim = tmp.path().join("synthetic-victim");
        std::fs::write(&victim, "do not change").unwrap();
        written_ago(&victim, STALE_TEMP * 2);
        let unchanged = snapshot(tmp.path());
        // In the shelf a seal writes to, and in the project directory, where
        // a forget of a legacy slot writes.
        let places = [dir.join("s000001").join("pending.tmp"), dir.join("pending.tmp")];
        let kind = |path: &Path| std::fs::symlink_metadata(path).unwrap().file_type();
        for shape in ["directory", "FIFO", "symlink", "hard link"] {
            for path in &places {
                match shape {
                    "directory" => std::fs::create_dir(path).unwrap(),
                    "FIFO" => {
                        let name = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
                        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                    }
                    "symlink" => symlink(&victim, path).unwrap(),
                    _ => std::fs::hard_link(&victim, path).unwrap(),
                }
                aged(path);
            }
            let words = format!("sealed beside a {shape}");
            let sealed = store.seal(&text(&words), "").unwrap();
            assert_eq!(store.fetch(&sealed.id, 0, 1).unwrap(), text(&words), "{shape}");
            assert_eq!(store.fetch(&first.id, 0, 1).unwrap(), text("first"), "{shape}");
            assert_eq!(store.search(&words, 2).unwrap().len(), 1, "{shape}");
            store.forget(&sealed.id).unwrap();
            // A legacy slot is tombstoned in the project directory itself.
            let old = seal_legacy_slot(&store, 1, &text(&words), "");
            store.forget(&old.id).unwrap();
            assert_eq!(store.fetch(&legacy.id, 0, 1).unwrap(), text("legacy"), "{shape}");
            std::fs::remove_file(dir.join("001.json")).unwrap();
            for path in &places {
                let found = kind(path);
                match shape {
                    "directory" => assert!(found.is_dir()),
                    "FIFO" => assert!(found.is_fifo()),
                    "symlink" => assert!(found.is_symlink()),
                    // A second name of a file that is not the store's alone
                    // is not a leftover of the store's either.
                    _ => assert_eq!(identity(path), identity(&victim)),
                }
                if found.is_dir() {
                    std::fs::remove_dir(path).unwrap();
                } else {
                    std::fs::remove_file(path).unwrap();
                }
            }
            assert_eq!(std::fs::read_to_string(&victim).unwrap(), "do not change");
        }
        // Nothing but the four tombstones was added, changed or removed.
        let after = snapshot(tmp.path());
        assert!(unchanged.iter().all(|(path, file)| after.get(path) == Some(file)));
        assert_eq!(after.len(), unchanged.len() + 4);
        assert!(temporary_files(&dir).is_empty());
    }

    /// `create_new` is how a seal publishes. It never replaces what is under
    /// the name, whatever that is and even when the bytes would be the same.
    #[test]
    fn publish_create_new_never_replaces() {
        let (tmp, store) = setup();
        let shelf_path = directory(&tmp, &store).join("s000001");
        let shelf = store.fs.child("s000001", true).unwrap();
        let name = archive_name(&"a".repeat(32));
        let path = shelf_path.join(&name);
        let victim = tmp.path().join("synthetic-victim");
        std::fs::write(&victim, "do not change").unwrap();
        shelf.publish(&name, b"first", true).unwrap();
        let first = identity(&path);
        let refused = |bytes: &[u8]| {
            let error = shelf.publish(&name, bytes, true).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(std::fs::read_dir(&shelf_path).unwrap().count(), 1, "no temporary file");
        };
        for bytes in [&b"first"[..], b"second"] {
            refused(bytes);
            assert_eq!(std::fs::read(&path).unwrap(), b"first");
            assert_eq!(identity(&path), first, "replaced by a file with the same bytes");
        }
        // Nor a link (which is not followed), a directory or a FIFO.
        std::fs::remove_file(&path).unwrap();
        for target in [victim.clone(), tmp.path().join("missing")] {
            symlink(&target, &path).unwrap();
            let link = identity(&path);
            refused(b"through the link");
            assert_eq!(identity(&path), link);
            assert!(!tmp.path().join("missing").exists());
            std::fs::remove_file(&path).unwrap();
        }
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "do not change");
        std::fs::create_dir(&path).unwrap();
        refused(b"over the directory");
        assert!(path.is_dir());
        std::fs::remove_dir(&path).unwrap();
        // The two steps taken where the kernel or the filesystem has no
        // rename that cannot replace: the same answers, under the lock.
        let from = c_name(&temp_name()).unwrap();
        let to = c_name(&name).unwrap();
        let temporary = shelf_path.join(from.to_str().unwrap());
        std::fs::write(&temporary, b"probed").unwrap();
        let written = identity(&temporary);
        shelf.rename_probed(&from, &to).unwrap();
        assert_eq!((identity(&path), temporary.exists()), (written, false));
        std::fs::write(&temporary, b"probed again").unwrap();
        let error = shelf.rename_probed(&from, &to).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(identity(&path), written);
        assert_eq!(std::fs::read(&temporary).unwrap(), b"probed again");
        // Forget is the one caller that replaces, and it does.
        shelf.publish(&name, b"tombstone", false).unwrap();
        assert_ne!(identity(&path), written);
        assert_eq!(std::fs::read(&path).unwrap(), b"tombstone");
    }

    #[test]
    fn export_fails_closed_above_migration_limit() {
        let (tmp, store) = setup();
        // Legacy slots and shelves count together.
        let mut ids: Vec<_> = (0..100)
            .map(|slot| seal_legacy_slot(&store, slot, &text(&format!("legacy {slot}")), "").id)
            .collect();
        let seal = |index: usize| store.seal(&text(&format!("archive {index}")), "").unwrap().id;
        ids.extend((100..MAX_SEGMENTS).map(seal));
        let rows = export_in(tmp.path(), "synthetic-project").unwrap();
        assert_eq!(rows, store.export_records().unwrap());
        let exported: Vec<_> = rows.iter().map(|row| row["id"].as_str().unwrap()).collect();
        assert_eq!(exported.len(), MAX_SEGMENTS);
        assert!(ids.iter().all(|id| exported.contains(&id.as_str())));
        assert_eq!(exported[..100], ids[..100], "legacy slots first, in slot order");
        seal(MAX_SEGMENTS);
        for error in [
            export_in(tmp.path(), "synthetic-project").unwrap_err(),
            store.export_records().unwrap_err(),
        ] {
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert_eq!(
                error.to_string(),
                "archive store exceeds the migration limit (128 archives / 256 MiB)"
            );
        }
        // Only the export is limited.
        assert_eq!(store.fetch(&ids[0], 0, 1).unwrap(), text("legacy 0"));
        assert!(store.seal(&text("and one more"), "").is_ok());
    }

    #[test]
    fn archive_file_returns_path_and_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let unknown = "a".repeat(32);
        for base in [tmp.path().join("missing"), tmp.path().to_path_buf()] {
            assert_eq!(archive_file(&base, "synthetic-project", &unknown).unwrap(), None);
        }
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
        let store =
            ArchiveStore::new(tmp.path(), "synthetic-project", "synthetic-logical").unwrap();
        let dir = directory(&tmp, &store);
        let reference = store.seal(&text("where am I"), "").unwrap();
        let file = only_archive_file(&dir);
        let locate = |project: &str, id: &str| archive_file(tmp.path(), project, id);
        // No lock is taken or made, and a held one is not waited for.
        let held = store.fs.lock().unwrap();
        assert_eq!(locate("synthetic-project", &reference.id).unwrap(), Some(file.clone()));
        drop(held);
        std::fs::remove_file(dir.join("lock")).unwrap();
        let before = snapshot(tmp.path());
        assert_eq!(locate("synthetic-project", &reference.id).unwrap(), Some(file.clone()));
        assert_eq!(locate("synthetic-project", &unknown).unwrap(), None);
        assert_eq!(locate("another-project", &reference.id).unwrap(), None);
        assert_eq!(
            locate("synthetic-project", "../s000001/x").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(snapshot(tmp.path()), before);
        assert_eq!(std::fs::read_dir(tmp.path().join("context-archives")).unwrap().count(), 1);
        // A forgotten segment still has a file: its tombstone.
        store.forget(&reference.id).unwrap();
        assert_eq!(locate("synthetic-project", &reference.id).unwrap(), Some(file));
    }

    /// What a window holds however it is cut: the projection of each source
    /// message by itself, under its index in the window.
    fn exact(store: &ArchiveStore, messages: &[SharedMessage]) -> Vec<ArchivedMessage> {
        let mut expected = Vec::new();
        for (index, message) in messages.iter().enumerate() {
            let one = std::slice::from_ref(message);
            let redactor = store.redactor.clone();
            let alone = project_hashed(&store.logical, one, "", redactor, Reduce::None).unwrap();
            expected.extend(alone.messages.into_iter().map(|mut archived| {
                archived.source_index = index;
                archived
            }));
        }
        expected
    }
    /// A sealed window of `len` messages read back and put together again
    /// from its part list alone: the parts are contiguous and cover it, and
    /// each part's messages follow under their index in the window.
    fn reassemble(
        store: &ArchiveStore,
        window: &SealedWindow,
        len: usize,
    ) -> Vec<ArchivedMessage> {
        let mut messages = Vec::new();
        let mut next = 0;
        for part in &window.parts {
            let ArchiveRef {
                id,
                message_count,
                source_message_count,
            } = &part.archive;
            assert_eq!(part.first_source_index, next);
            assert!(*source_message_count <= MAX_MESSAGES);
            next += source_message_count;
            let mut start = 0;
            while start < *message_count {
                let page = store
                    .fetch_with_provenance(id, start, MAX_FETCH_MESSAGES)
                    .unwrap();
                assert!(!page.is_empty());
                start += page.len();
                messages.extend(page.into_iter().map(|mut archived| {
                    assert!(archived.source_index < *source_message_count);
                    archived.source_index += part.first_source_index;
                    archived
                }));
            }
            assert_eq!(start, *message_count);
        }
        assert_eq!(next, len, "the parts cover the window");
        messages
    }
    /// `exact` for a window of plain text that is all eligible: each message
    /// is its own projection.
    fn assert_whole(read: &[ArchivedMessage], messages: &[SharedMessage]) {
        assert_eq!(read.len(), messages.len());
        for (index, archived) in read.iter().enumerate() {
            assert_eq!(archived.source_index, index);
            assert!(archived.message == messages[index], "message {index} differs");
        }
    }
    fn lines(len: usize) -> Vec<SharedMessage> {
        (0..len)
            .map(|index| msg("user", json!(format!("line {index} of {len}"))))
            .collect()
    }
    /// A tool input nested deeper than one segment admits.
    fn deep_input() -> Value {
        let mut deep = json!("leaf");
        for _ in 0..40 {
            deep = json!([deep]);
        }
        deep
    }
    const OVER_LIMIT: &str = "over the per-archive limit]";

    #[test]
    fn window_within_caps_is_one_part_identical_to_seal() {
        let messages = vec![
            msg("system", json!("not eligible")),
            msg("user", json!("a question")),
            msg("assistant", json!([{"type":"text","text":"an answer"}])),
        ];
        let (strict_tmp, strict) = setup();
        let (tmp, store) = setup();
        let (reference, note) = strict.seal_with_note(&messages, "the note").unwrap();
        let window = store.seal_window(&messages, "the note").unwrap();
        let part = ArchivePart {
            archive: reference.clone(),
            first_source_index: 0,
            reduced_messages: 0,
        };
        let expected = SealedWindow {
            parts: vec![part],
            note,
        };
        assert_eq!(window, expected);
        assert_eq!(
            std::fs::read(only_archive_file(&directory(&tmp, &store))).unwrap(),
            std::fs::read(only_archive_file(&directory(&strict_tmp, &strict))).unwrap()
        );
        // In one store the two calls find each other's segment.
        assert_eq!(strict.seal_window(&messages, "another note").unwrap(), window);
        assert_eq!(
            store.seal_with_note(&messages, "another note").unwrap(),
            (reference, "the note".to_owned())
        );
        // An empty window is one empty part, as it is one empty segment.
        let empty = store.seal_window(&[], "").unwrap();
        assert_eq!(empty.parts.len(), 1);
        assert_eq!(empty.parts[0].archive, store.seal(&[], "").unwrap());
    }

    #[test]
    fn window_over_message_cap_splits() {
        let (tmp, store) = setup();
        for (len, parts) in [(MAX_MESSAGES + 1, 2), (10_000, 4)] {
            // Every seventh message is not eligible: source indices have gaps.
            let messages: Vec<_> = (0..len)
                .map(|index| {
                    let role = ["user", "assistant", "user", "system", "assistant"][index % 7 % 5];
                    msg(role, json!(format!("window of {len}, line {index}")))
                })
                .collect();
            assert!(store.seal(&messages, "").is_err());
            let window = store.seal_window(&messages, "").unwrap();
            assert_eq!(window.parts.len(), parts);
            assert!(window.parts.iter().all(|part| part.reduced_messages == 0));
            let read = reassemble(&store, &window, len);
            assert_eq!(read.len(), len - (len + 3) / 7);
            assert!(read == exact(&store, &messages));
        }
        assert_eq!(shelf_files(&directory(&tmp, &store)).len(), 6);
    }

    #[test]
    fn window_over_byte_cap_splits() {
        let (_tmp, store) = setup();
        // 17 MiB of eligible text, in messages far below any other cap.
        let messages: Vec<_> = (0..34)
            .map(|index| msg("user", json!(format!("{index:02} {}", "x".repeat(512 * 1024)))))
            .collect();
        let error = store.seal(&messages, "").unwrap_err();
        assert!(error.to_string().contains("input byte budget"));
        let window = store.seal_window(&messages, "").unwrap();
        assert_eq!(window.parts.len(), 2);
        assert_whole(&reassemble(&store, &window, messages.len()), &messages);
    }

    #[test]
    fn window_over_node_cap_splits() {
        let (_tmp, store) = setup();
        let store = store.with_redactor(Arc::new(str::to_owned));
        // 3,000 tool calls of 40 arguments: 48 visited nodes each, 144,000
        // in all, where one segment admits 65,536.
        let input: serde_json::Map<String, Value> = (0..40)
            .map(|key| (format!("k{key:02}"), json!(key)))
            .collect();
        let messages: Vec<_> = (0..3000)
            .map(|index| {
                let id = format!("call-{index}");
                msg(
                    "assistant",
                    json!([{"type":"tool_use","id":id,"name":"plot","input":input}]),
                )
            })
            .collect();
        let error = store.seal(&messages, "").unwrap_err();
        assert!(error.to_string().contains("structural budget"));
        let window = store.seal_window(&messages, "").unwrap();
        assert_eq!(window.parts.len(), 4);
        assert!(window.parts.iter().all(|part| part.reduced_messages == 0));
        let read = reassemble(&store, &window, messages.len());
        assert_eq!(read[2999].message, messages[2999]);
        assert!(read == exact(&store, &messages));
    }

    #[test]
    fn serialized_overflow_after_projection_splits() {
        let (_tmp, store) = setup();
        // 3 MiB of control characters passes the input budget, and is 18 MiB
        // once each is written as "\u0001".
        let messages: Vec<_> = (0..4)
            .map(|index| msg("user", json!(format!("{index}{}", "\u{1}".repeat(768 * 1024)))))
            .collect();
        let error = store.seal(&messages, "").unwrap_err();
        assert!(error.to_string().contains("serialization failed or exceeded budget"));
        let window = store.seal_window(&messages, "").unwrap();
        assert_eq!(window.parts.len(), 2);
        assert_whole(&reassemble(&store, &window, messages.len()), &messages);
    }

    #[test]
    fn single_deep_input_is_withheld_not_failed() {
        let (_tmp, store) = setup();
        let store = store.with_redactor(Arc::new(str::to_owned));
        let messages = vec![
            msg("user", json!("please run both")),
            msg(
                "assistant",
                json!([
                    {"type":"text","text":"running both"},
                    {"type":"tool_use","id":"t1","name":"nest","input":deep_input()},
                    {"type":"tool_use","id":"t2","name":"read","input":{"path":"src/lib.rs"}}
                ]),
            ),
            msg(
                "user",
                json!([
                    {"type":"tool_result","tool_use_id":"t1","content":"nested"},
                    {"type":"tool_result","tool_use_id":"t2","content":"fn main() {}"}
                ]),
            ),
            msg("assistant", json!("both ran")),
        ];
        assert!(store.seal(&messages, "").is_err());
        let window = store.seal_window(&messages, "").unwrap();
        // The message that does not fit by itself is a part of its own, and
        // the only one that is not exact.
        let reduced: Vec<_> = window
            .parts
            .iter()
            .map(|part| (part.first_source_index, part.reduced_messages))
            .collect();
        assert_eq!(reduced, [(0, 0), (1, 1), (2, 0), (3, 0)]);
        let read = reassemble(&store, &window, messages.len());
        for index in [0, 2, 3] {
            assert_eq!(read[index].source_index, index);
            assert_eq!(read[index].message, messages[index]);
        }
        // Of that message only the one input that is too deep is left out:
        // its text and the other call's input are exact.
        let bytes = serde_json::to_string(&deep_input()).unwrap().len();
        let omitted = format!("[archive: tool input of {bytes} bytes omitted, {OVER_LIMIT}");
        let expected = json!({
            "role":"assistant",
            "_archive_reduced":"this message alone is over the per-archive limit; it is stored \
                with 1 omission(s), each marked in place",
            "content":[
                {"type":"text","text":"running both"},
                {"type":"tool_use","id":"t1","name":"nest","input":{"_archive_omitted":omitted}},
                {"type":"tool_use","id":"t2","name":"read","input":{"path":"src/lib.rs"}}
            ]
        });
        assert_eq!(*read[1].message, expected);
        assert_eq!(read[1].block_indices, [0, 1, 2]);
        // However it is read back it says so first: the key sorts before
        // `content` and `role`.
        let found = store.search("running both", 1).unwrap();
        assert!(found[0].snippet.starts_with("{\"_archive_reduced\":\"this message alone"));
    }

    #[test]
    fn inputs_too_large_together_are_all_omitted_and_text_stays_exact() {
        let (_tmp, store) = setup();
        let store = store.with_redactor(Arc::new(str::to_owned));
        // 30,001 visited nodes each: every input fits by itself, the three
        // together do not.
        let wide = json!(vec![0; 30_000]);
        let long = "exact words ".repeat(1000);
        let message = msg(
            "assistant",
            json!([
                {"type":"text","text":long},
                {"type":"tool_use","id":"t1","name":"plot","input":wide},
                {"type":"tool_use","id":"t2","name":"plot","input":wide},
                {"type":"tool_use","id":"t3","name":"plot","input":wide}
            ]),
        );
        let window = store.seal_window(&[message], "").unwrap();
        assert_eq!(window.parts.len(), 1);
        assert_eq!(window.parts[0].reduced_messages, 1);
        let stored = &store.fetch(&window.parts[0].archive.id, 0, 1).unwrap()[0];
        assert_eq!(stored["content"][0]["text"], long);
        let bytes = serde_json::to_string(&wide).unwrap().len();
        let omitted = format!("[archive: tool input of {bytes} bytes omitted, {OVER_LIMIT}");
        for block in 1..=3 {
            assert_eq!(stored["content"][block]["input"], json!({"_archive_omitted":omitted}));
        }
        assert!(stored[REDUCED_KEY].as_str().unwrap().contains("with 3 omission(s)"));
    }

    #[test]
    fn single_huge_message_is_reduced_with_markers() {
        let (_tmp, store) = setup();
        // One byte over the input cap, with a two-byte character across the
        // 4,096th byte.
        let huge = format!("a{}", "é".repeat(MAX_INPUT_BYTES / 2));
        assert_eq!(huge.len(), MAX_INPUT_BYTES + 1);
        let messages = vec![
            msg("user", json!("before")),
            msg("user", json!(huge)),
            msg("assistant", json!("after")),
        ];
        assert!(store.seal(&messages, "").is_err());
        let window = store.seal_window(&messages, "").unwrap();
        let reduced: Vec<_> = window.parts.iter().map(|part| part.reduced_messages).collect();
        assert_eq!(reduced, [0, 1, 0]);
        let read = reassemble(&store, &window, messages.len());
        assert_eq!(read[0].message, messages[0]);
        assert_eq!(read[2].message, messages[2]);
        let kept = &huge[..4095];
        let omitted = huge.len() - 4095;
        let stored = read[1].message["content"].as_str().unwrap();
        assert_eq!(stored, format!("{kept}\n[archive: {omitted} bytes omitted, {OVER_LIMIT}"));
        assert!(read[1].message[REDUCED_KEY].as_str().unwrap().contains("with 1 omission(s)"));
    }

    #[test]
    fn reduced_wide_message_keeps_its_first_blocks_and_counts_the_rest() {
        let (_tmp, store) = setup();
        let store = store.with_redactor(Arc::new(str::to_owned));
        // 70,000 small blocks are more nodes than one segment admits, with
        // or without tool inputs.
        let mut blocks = vec![json!({"type":"tool_result","tool_use_id":"t1","content":[
            {"type":"text","text":"x".repeat(5000)},
            {"type":"text","text":"second"},
            {"type":"text","text":"third"}
        ]})];
        let small = |index: usize| json!({"type":"text","text":format!("block {index}")});
        blocks.extend((1..70_000).map(small));
        let window = store.seal_window(&[msg("user", json!(blocks))], "").unwrap();
        assert_eq!(window.parts.len(), 1);
        assert_eq!(window.parts[0].reduced_messages, 1);
        let id = &window.parts[0].archive.id;
        let fetched = &store.fetch_with_provenance(id, 0, 1).unwrap()[0];
        let content = fetched.message["content"].as_array().unwrap();
        assert_eq!(content.len(), 257);
        // A tool result's block list becomes its first text, cut like any
        // string, and says how many blocks followed.
        let short = format!("[archive: 904 bytes omitted, {OVER_LIMIT}");
        let followed = format!("[archive: 2 blocks omitted, {OVER_LIMIT}");
        let result = format!("{}\n{short}\n{followed}", "x".repeat(REDUCED_TEXT_BYTES));
        assert_eq!(content[0]["content"], result);
        assert_eq!(content[1], json!({"type":"text","text":"block 1"}));
        assert_eq!(content[255], json!({"type":"text","text":"block 255"}));
        let rest = format!("[archive: 69744 blocks omitted, {OVER_LIMIT}");
        assert_eq!(content[256], json!({"type":"text","text":rest}));
        assert_eq!(fetched.block_indices, (0..=256).collect::<Vec<_>>());
        assert!(fetched.message[REDUCED_KEY].as_str().unwrap().contains("with 3 omission(s)"));
    }

    #[test]
    fn only_the_store_marks_a_message_as_reduced() {
        let (_tmp, store) = setup();
        let store = store.with_redactor(Arc::new(str::to_owned));
        // A source message that imitates every marker of a reduced one.
        let marker = format!("[archive: 9 bytes omitted, {OVER_LIMIT}");
        let source = Arc::new(json!({"role":"assistant","_archive_reduced":"imitation","content":[
            {"type":"text","text":format!("cut short\n{marker}")},
            {"type":"tool_use","id":"t1","name":"read","input":{"_archive_omitted":marker}}
        ]}));
        let window = store.seal_window(std::slice::from_ref(&source), "").unwrap();
        assert_eq!(window.parts[0].reduced_messages, 0);
        let stored = &store.fetch(&window.parts[0].archive.id, 0, 1).unwrap()[0];
        // Its content is kept as it is; the key is not.
        assert_eq!(stored["content"], source["content"]);
        assert_eq!(stored.as_object().unwrap().len(), 2, "role and content");
    }

    #[test]
    fn retry_returns_the_same_parts_and_writes_nothing() {
        let (tmp, store) = setup();
        let store = store.with_redactor(Arc::new(str::to_owned));
        let dir = directory(&tmp, &store);
        // A window that is split, with one message that is reduced.
        let mut messages = lines(MAX_MESSAGES + 10);
        let call = json!([{"type":"tool_use","id":"t1","name":"nest","input":deep_input()}]);
        messages[7] = msg("assistant", call);
        let first = store.seal_window(&messages, "note").unwrap();
        assert!(first.parts.len() > 2);
        let reduced: usize = first.parts.iter().map(|part| part.reduced_messages).sum();
        assert_eq!(reduced, 1);
        let before = snapshot(&dir);
        assert_eq!(before.len(), first.parts.len() + 1, "one file a part, and the lock");
        assert_eq!(store.seal_window(&messages, "note").unwrap(), first);
        // A changed note changes nothing either: the first one is returned.
        assert_eq!(store.seal_window(&messages, "another note").unwrap(), first);
        assert_eq!(snapshot(&dir), before);
    }

    #[test]
    fn partial_failure_then_retry() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let messages = lines(MAX_MESSAGES + 2);
        let (left, right) = messages.split_at(messages.len() / 2);
        let id = |part: &[SharedMessage]| {
            let projection = project_hashed(&store.logical, part, "", None, Reduce::None);
            projection.unwrap().digest[..32].to_owned()
        };
        // Something that is not a file sits where the second part belongs.
        let shelf = dir.join("s000001");
        std::fs::create_dir(&shelf).unwrap();
        let obstacle = shelf.join(archive_name(&id(right)));
        std::fs::create_dir(&obstacle).unwrap();
        let error = store.seal_window(&messages, "note").unwrap_err();
        assert!(!over_budget(&error));
        // The first part is on disk, an ordinary segment.
        let first = shelf.join(archive_name(&id(left)));
        let bytes = std::fs::read(&first).unwrap();
        assert_eq!(store.fetch(&id(left), 0, 1).unwrap(), messages[..1]);
        std::fs::remove_dir(&obstacle).unwrap();
        let window = store.seal_window(&messages, "note").unwrap();
        let ids: Vec<_> = window.parts.iter().map(|part| part.archive.id.clone()).collect();
        assert_eq!(ids, [id(left), id(right)]);
        assert_eq!(std::fs::read(&first).unwrap(), bytes);
        assert_eq!(window.note, "note");
        assert!(reassemble(&store, &window, messages.len()) == exact(&store, &messages));
    }

    #[test]
    fn note_is_stored_on_the_last_part_only() {
        let (_tmp, store) = setup();
        let messages = lines(2 * MAX_MESSAGES + 1);
        let window = store.seal_window(&messages, "the working note").unwrap();
        assert_eq!(window.note, "the working note");
        let notes: Vec<_> = window
            .parts
            .iter()
            .map(|part| store.fetch_note(&part.archive.id).unwrap())
            .collect();
        assert_eq!(notes, ["", "", "the working note"]);
        // Sealed again, the window has the note it was first sealed with.
        let again = store.seal_window(&messages, "a later note").unwrap();
        assert_eq!(again.note, "the working note");
    }

    #[test]
    fn identical_halves_share_one_id() {
        let (tmp, store) = setup();
        let messages = vec![msg("user", json!("again")); 2 * MAX_MESSAGES];
        let window = store.seal_window(&messages, "the working note").unwrap();
        assert_eq!(window.parts.len(), 2);
        assert_eq!(window.parts[0].archive, window.parts[1].archive);
        assert_eq!(window.parts[0].first_source_index, 0);
        assert_eq!(window.parts[1].first_source_index, MAX_MESSAGES);
        assert_eq!(shelf_files(&directory(&tmp, &store)).len(), 1);
        assert!(reassemble(&store, &window, messages.len()) == exact(&store, &messages));
        // The one segment was stored for the first part, which has no note.
        // The window's note is not lost with it.
        assert_eq!(store.fetch_note(&window.parts[1].archive.id).unwrap(), "");
        assert_eq!(window.note, "the working note");
    }

    #[test]
    fn oversized_note_is_not_treated_as_splittable() {
        let (tmp, store) = setup();
        let window = lines(MAX_MESSAGES + 1);
        let note = "n".repeat(9 * 1024);
        for messages in [&window[..1], &window[..]] {
            let error = store.seal_window(messages, &note).unwrap_err();
            assert_eq!(error.to_string(), "archive message/note budget exceeded");
            assert!(!over_budget(&error));
        }
        // So is a note that only the redactor makes too long.
        let tripling = ArchiveStore::new(tmp.path(), "synthetic-project", "synthetic-logical")
            .unwrap()
            .with_redactor(Arc::new(|text: &str| text.repeat(3)));
        let error = tripling.seal_window(&window, &"n".repeat(4096)).unwrap_err();
        assert_eq!(error.to_string(), "redacted note exceeds archive budget");
        assert!(shelf_files(&directory(&tmp, &store)).is_empty());
    }

    #[test]
    fn strict_seal_still_refuses() {
        let (tmp, store) = setup();
        let messages = lines(MAX_MESSAGES + 1);
        for error in [
            store.seal(&messages, "").unwrap_err(),
            store.seal_with_note(&messages, "").unwrap_err(),
            project_archive("synthetic-logical", &messages, "", None).err().unwrap(),
        ] {
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert_eq!(error.to_string(), "archive message/note budget exceeded");
            assert!(over_budget(&error));
        }
        assert!(shelf_files(&directory(&tmp, &store)).is_empty());
        assert_eq!(store.seal_window(&messages, "").unwrap().parts.len(), 2);
    }

    #[test]
    fn a_cut_keeps_a_tool_result_with_its_call_where_it_can() {
        let call = msg("assistant", json!([{"type":"tool_use","id":"t","name":"read","input":{}}]));
        let result = msg("user", json!([{"type":"tool_result","tool_use_id":"t","content":"ok"}]));
        let words = msg("user", json!("words"));
        let window = [words.clone(), call.clone(), result.clone(), words];
        assert_eq!(cut(&window, &(0..4)), 3, "moved past the result at the midpoint");
        assert_eq!(cut(&window, &(0..3)), 1);
        assert_eq!(cut(&window, &(1..3)), 2, "it cannot: the plain midpoint");
        let results = [call, result.clone(), result.clone(), result];
        assert_eq!(cut(&results, &(0..4)), 2, "nothing but results to the end");
    }

    /// A project directory from before shelves: a live segment in slot 0, a
    /// forgotten one in slot 2, and slot 1 between them left to the test.
    fn legacy_store() -> (tempfile::TempDir, ArchiveStore, ArchiveRef) {
        let (tmp, store) = setup();
        let live = seal_legacy_slot(&store, 0, &text("legacy live"), "first note");
        let gone = seal_legacy_slot(&store, 2, &text("legacy forgotten"), "");
        store.forget(&gone.id).unwrap();
        (tmp, store, live)
    }
    /// With slot 1 unusable a seal works, and still finds what the slots on
    /// either side of it hold; whatever reads the legacy slots as a whole
    /// reports the error it always did.
    fn assert_seal_passes_over_slot_1(
        tmp: &tempfile::TempDir,
        store: &ArchiveStore,
        live: &ArchiveRef,
        words: &str,
    ) {
        let new = store.seal(&text(words), "").unwrap();
        assert_eq!(store.fetch(&new.id, 0, 1).unwrap(), text(words), "{words}");
        let again = store.seal_with_note(&text("legacy live"), "second note");
        assert_eq!(again.unwrap(), (live.clone(), "first note".to_owned()), "{words}");
        let forgotten = store.seal(&text("legacy forgotten"), "").unwrap_err();
        assert!(forgotten.to_string().contains("was forgotten"), "{words}");
        assert!(store.fetch(&live.id, 0, 1).is_err(), "{words}");
        assert!(store.fetch_note(&live.id).is_err(), "{words}");
        assert!(store.forget(&live.id).is_err(), "{words}");
        assert!(store.export_records().is_err(), "{words}");
        assert!(export_in(tmp.path(), "synthetic-project").is_err(), "{words}");
        assert!(archive_file(tmp.path(), "synthetic-project", &live.id).is_err(), "{words}");
    }

    #[test]
    fn seal_passes_over_a_legacy_slot_that_is_not_a_valid_record() {
        let (tmp, store, live) = legacy_store();
        let dir = directory(&tmp, &store);
        let slot = dir.join("001.json");
        // A valid record of another project, and this project's own live
        // record with a word of its content changed.
        let foreign =
            ArchiveStore::new(tmp.path(), "foreign-project", "synthetic-logical").unwrap();
        seal_legacy_slot(&foreign, 0, &text("foreign words"), "");
        let foreign = std::fs::read(directory(&tmp, &foreign).join("000.json")).unwrap();
        let altered = std::fs::read_to_string(dir.join("000.json")).unwrap();
        let altered = altered.replace("legacy live", "legacy LIVE");
        let cases = [
            ("bad JSON", b"{\"version\":1,".to_vec()),
            ("empty", Vec::new()),
            ("wrong scope", foreign),
            ("digest mismatch", altered.into_bytes()),
        ];
        for (kind, bytes) in cases {
            std::fs::write(&slot, &bytes).unwrap();
            assert_seal_passes_over_slot_1(&tmp, &store, &live, kind);
            assert_eq!(std::fs::read(&slot).unwrap(), bytes, "{kind}");
            let error = store.fetch(&live.id, 0, 1).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{kind}");
            // Search passed over such a file already.
            assert_eq!(store.search("legacy", 2).unwrap()[0].id, live.id, "{kind}");
        }
        // Over the size of a segment: refused by its length, never read.
        let large = MAX_SEGMENT_BYTES as u64 + 1;
        std::fs::File::create(&slot).unwrap().set_len(large).unwrap();
        assert_seal_passes_over_slot_1(&tmp, &store, &live, "over the size bound");
        assert_eq!(std::fs::metadata(&slot).unwrap().len(), large);
        let error = store.fetch(&live.id, 0, 1).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        // Five seals, five new files, and no other change to the directory.
        assert_eq!(shelf_files(&dir).len(), 5);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 5, "lock, 3 slots, 1 shelf");
    }

    #[test]
    fn seal_passes_over_a_legacy_slot_that_is_not_a_private_file() {
        use std::os::unix::fs::FileTypeExt;
        let (tmp, store, live) = legacy_store();
        let slot = directory(&tmp, &store).join("001.json");
        let kind = || std::fs::symlink_metadata(&slot).unwrap().file_type();
        let victim = tmp.path().join("synthetic-victim");
        std::fs::write(&victim, "do not change").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
        // Nothing was read into, written through or made private by way of
        // the slot, and reading the legacy slots still fails on it.
        let passed_over = |words: &str| {
            assert_seal_passes_over_slot_1(&tmp, &store, &live, words);
            assert!(store.search("legacy", 2).is_err(), "{words}");
            assert_eq!(std::fs::read_to_string(&victim).unwrap(), "do not change");
            assert_eq!(mode(&victim), 0o644, "{words}");
        };
        symlink(&victim, &slot).unwrap();
        passed_over("past a symlink");
        assert!(kind().is_symlink());
        std::fs::remove_file(&slot).unwrap();
        std::fs::hard_link(&victim, &slot).unwrap();
        passed_over("past a hard link");
        std::fs::remove_file(&slot).unwrap();
        let fifo = std::ffi::CString::new(slot.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        passed_over("past a FIFO");
        assert!(kind().is_fifo());
        std::fs::remove_file(&slot).unwrap();
        std::fs::create_dir(&slot).unwrap();
        passed_over("past a directory");
        assert!(kind().is_dir());
    }

    #[test]
    fn a_legacy_tombstone_that_cannot_be_read_no_longer_forbids_its_projection() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let slot = dir.join("000.json");
        let gone = seal_legacy_slot(&store, 0, &text("forgotten words"), "");
        store.forget(&gone.id).unwrap();
        let refused = store.seal(&text("forgotten words"), "").unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        // The price of passing over what cannot be read: with the tombstone
        // torn, nothing says that this projection was forgotten.
        let tombstone = std::fs::read(&slot).unwrap();
        let torn = &tombstone[..tombstone.len() / 2];
        std::fs::write(&slot, torn).unwrap();
        let again = store.seal(&text("forgotten words"), "").unwrap();
        assert_eq!(store.fetch(&again.id, 0, 1).unwrap(), text("forgotten words"));
        assert_eq!(only_archive_file(&dir), dir.join("s000001").join(archive_name(&again.id)));
        assert_eq!(std::fs::read(&slot).unwrap(), torn);
    }

    /// `readdir` says that a read failed only through `errno`; its null is
    /// also the end of the directory. A listing that did not look would come
    /// back short, and a seal would take a full shelf for one with room.
    #[test]
    fn a_failed_directory_read_is_an_error_not_a_shorter_listing() {
        use std::os::fd::{AsRawFd, IntoRawFd};
        let (tmp, store) = setup();
        store.seal(&text("listed"), "").unwrap();
        let shelf = store.fs.open_child("s000001", true).unwrap().unwrap();
        assert_eq!(shelf.names().unwrap().len(), 1);
        // A stream on the shelf, opened as `names` opens it...
        let handle = shelf.open(c".", libc::O_RDONLY | libc::O_DIRECTORY);
        let fd = handle.unwrap().into_raw_fd();
        let stream = unsafe { libc::fdopendir(fd) };
        assert!(!stream.is_null());
        // ...whose handle is then one on a plain file: the read cannot work.
        let file = std::fs::File::open(only_archive_file(&directory(&tmp, &store))).unwrap();
        assert_eq!(unsafe { libc::dup2(file.as_raw_fd(), fd) }, fd);
        let listed = shelf.read_names(stream);
        unsafe { libc::closedir(stream) };
        let error = listed.unwrap_err();
        #[cfg(target_os = "linux")]
        assert_eq!(error.raw_os_error(), Some(libc::ENOTDIR));
        assert_ne!(error.raw_os_error(), Some(0));
    }

    /// The other half of the same rule: `errno` is cleared before each read,
    /// so what an earlier call left there does not fail a listing that worked.
    #[test]
    fn a_listing_is_not_failed_by_an_error_from_before_it() {
        let (_tmp, store) = setup();
        store.seal(&text("listed"), "").unwrap();
        let shelf = store.fs.open_child("s000001", true).unwrap().unwrap();
        assert_eq!(unsafe { libc::close(-1) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
        assert_eq!(shelf.names().unwrap().len(), 1);
        // The same for a directory with nothing in it.
        let empty = store.fs.child("s000002", true).unwrap();
        assert_eq!(unsafe { libc::close(-1) }, -1);
        assert!(empty.names().unwrap().is_empty());
    }

    /// A listing opens the directory again, and where that is refused it
    /// fails; it does not come back empty.
    #[test]
    fn a_directory_that_cannot_be_opened_again_is_an_error_not_an_empty_listing() {
        if unsafe { libc::geteuid() } == 0 {
            return; // nothing is refused to root
        }
        let (tmp, store) = setup();
        store.seal(&text("listed"), "").unwrap();
        let path = directory(&tmp, &store).join("s000001");
        let shelf = store.fs.open_child("s000001", true).unwrap().unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let listed = shelf.names();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(listed.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(shelf.names().unwrap().len(), 1);
    }

    #[test]
    fn names_that_are_not_store_names_are_ignored_whatever_they_look_like() {
        use std::os::unix::ffi::OsStringExt;
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let first = store.seal(&text("first"), "").unwrap();
        let legacy = seal_legacy_slot(&store, 0, &text("legacy"), "");
        let id = &first.id;
        let odd: Vec<std::ffi::OsString> = vec![
            // As long as a name can be, with and without the ending of a
            // segment; not UTF-8; and what no path component may hold.
            "x".repeat(255).into(),
            format!("{}.json", "9".repeat(250)).into(),
            std::ffi::OsString::from_vec(b"\xff\xfe.json".to_vec()),
            "back\\slash.json".into(),
            "new\nline.json".into(),
            // Nearly a shelf, a slot, a segment, and a temporary file.
            "s00001".into(),
            "s0000001".into(),
            "S000001".into(),
            "s000001.json".into(),
            "0.json".into(),
            "128.json".into(),
            "0000.json".into(),
            format!("{}.json", &id[1..]).into(),
            format!("{}.json", id.to_uppercase()).into(),
            format!("{id}.json.tmp").into(),
            format!("{id}.JSON").into(),
            format!(".{id}.json").into(),
            ".json".into(),
            "lock.json".into(),
        ];
        let mut strays = Vec::new();
        for parent in [dir.clone(), dir.join("s000001")] {
            for name in &odd {
                // A filesystem may refuse some of these names.
                let path = parent.join(name);
                if std::fs::write(&path, b"stray").is_ok() {
                    strays.push(path);
                }
            }
        }
        #[cfg(target_os = "linux")]
        assert_eq!(strays.len(), 2 * odd.len());
        assert!(strays.len() >= 2 * 10);

        let second = store.seal(&text("second"), "").unwrap();
        assert!(dir.join("s000001").join(archive_name(&second.id)).is_file());
        assert_eq!(store.seal(&text("first"), "").unwrap(), first);
        assert_eq!(store.fetch(&first.id, 0, 1).unwrap(), text("first"));
        assert_eq!(store.fetch(&legacy.id, 0, 1).unwrap(), text("legacy"));
        let mut found: Vec<_> = store.search("", 32).unwrap().into_iter().map(|row| row.id).collect();
        found.sort();
        let mut expected = vec![first.id.clone(), second.id.clone(), legacy.id.clone()];
        expected.sort();
        assert_eq!(found, expected);
        assert_eq!(store.export_records().unwrap().len(), 3);
        store.forget(&second.id).unwrap();
        assert_eq!(store.shelves().unwrap(), ["s000001"]);
        for stray in strays {
            assert_eq!(std::fs::read(&stray).unwrap(), b"stray", "{stray:?}");
        }
    }
}

#[cfg(all(test, unix))]
mod lock_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn msg(role: &str, content: Value) -> SharedMessage {
        Arc::new(json!({"role":role,"content":content}))
    }
    fn setup() -> (tempfile::TempDir, ArchiveStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store =
            ArchiveStore::new(tmp.path(), "synthetic-project", "synthetic-logical").unwrap();
        (tmp, store)
    }

    /// Wait (bounded) for a condition another thread establishes.
    fn wait_until(condition: impl Fn() -> bool) {
        let started = Instant::now();
        while !condition() {
            assert!(started.elapsed() < Duration::from_secs(30), "timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The process-local gate of `store`'s archive directory.
    fn gate_of(store: &ArchiveStore) -> Arc<Gate> {
        Gate::for_directory(store.fs.identity().unwrap())
    }

    /// A flock belongs to the open file description. A child forked by another
    /// thread holds copies of every descriptor (O_CLOEXEC applies only at
    /// exec), so closing our copy alone does not release the lock. The child
    /// stays between fork and exec until the parent has sealed again, without
    /// waiting: the lock has to be free already.
    #[test]
    fn a_descriptor_inherited_by_a_forked_child_does_not_keep_the_archive_locked() {
        use std::io::{Read, Write};
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        use std::process::ExitStatus;
        use std::thread::JoinHandle;
        /// Releases the child and reaps it on every path, unwinding included.
        struct Child {
            release: Option<io::PipeWriter>,
            spawner: Option<JoinHandle<io::Result<ExitStatus>>>,
        }
        impl Child {
            fn release_and_reap(&mut self) -> Option<std::thread::Result<io::Result<ExitStatus>>> {
                // The byte, or failing that EOF, ends the child's wait.
                if let Some(mut release) = self.release.take() {
                    let _ = release.write_all(b"r");
                }
                // The spawner returns once the child has exited and is reaped.
                self.spawner.take().map(JoinHandle::join)
            }
        }
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.release_and_reap();
            }
        }
        let (_tmp, mut store) = setup();
        store.process_wait = Duration::ZERO;
        let reference = store
            .seal(&[msg("user", json!("evidence"))], "note")
            .unwrap();
        // Close-on-exec, like every descriptor the archive opens.
        let (mut forked, signal) = io::pipe().unwrap();
        let (hold, release) = io::pipe().unwrap();
        let (signal_fd, hold_fd) = (signal.as_raw_fd(), hold.as_raw_fd());
        let held = store.fs.lock().unwrap();
        let mut child = Child {
            release: Some(release),
            spawner: Some(std::thread::spawn(move || {
                let mut command = std::process::Command::new("true");
                // Only async-signal-safe calls between fork and exec.
                unsafe {
                    command.pre_exec(move || {
                        let interrupted =
                            || io::Error::last_os_error().raw_os_error() == Some(libc::EINTR);
                        loop {
                            match libc::write(signal_fd, b"f".as_ptr().cast(), 1) {
                                1 => break,
                                -1 if interrupted() => {}
                                // EOF on the parent's side fails the test.
                                _ => libc::_exit(1),
                            }
                        }
                        let mut byte = 0u8;
                        while libc::read(hold_fd, (&mut byte as *mut u8).cast(), 1) < 0
                            && interrupted()
                        {}
                        Ok(())
                    });
                }
                let status = command.status();
                drop((signal, hold));
                status
            })),
        };
        // Bounded: a child that never signals fails the test, and the guard
        // releases and reaps it.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let mut ready = libc::pollfd {
                fd: forked.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let left = deadline.saturating_duration_since(Instant::now());
            let timeout = left.as_millis().try_into().unwrap_or(i32::MAX);
            match unsafe { libc::poll(&mut ready, 1, timeout) } {
                1 => break,
                0 => panic!("the forked child never signaled"),
                _ => assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EINTR)),
            }
        }
        forked.read_exact(&mut [0; 1]).unwrap();
        drop(held);
        // The child is blocked before exec, holding an inherited copy.
        let sealed = store.seal(&[msg("user", json!("more evidence"))], "");
        let status = child.release_and_reap().unwrap().unwrap().unwrap();
        assert!(status.success());
        assert_ne!(sealed.unwrap(), reference);
    }

    /// After waiting, an operation past its deadline is refused even when the
    /// gate has become free by the time it rechecks.
    #[test]
    fn an_expired_waiter_is_not_admitted_when_the_gate_frees_late() {
        let gate = Gate::for_directory((u64::MAX, u64::MAX));
        let holder = Gate::enter(gate.clone(), Duration::ZERO, None, BUSY).unwrap();
        let wait = Duration::from_millis(50);
        let waiter = {
            let gate = gate.clone();
            std::thread::spawn(move || Gate::enter(gate, wait, None, BUSY).map(drop))
        };
        wait_until(|| gate.waited.load(Ordering::Acquire) == 1);
        {
            // Free the gate but keep the waiter from rechecking until its
            // deadline has certainly passed.
            let mut held = gate.held.lock().unwrap();
            *held = false;
            std::thread::sleep(wait * 2);
        }
        let error = waiter.join().unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(error.to_string(), BUSY);
        drop(holder);
        // An uncontended first attempt is still admitted at once.
        Gate::enter(gate, Duration::ZERO, None, BUSY).unwrap();
    }

    #[test]
    fn concurrent_operations_in_one_process_wait_instead_of_reporting_busy() {
        let tmp = tempfile::tempdir().unwrap();
        let workers = 8;
        let barrier = Arc::new(std::sync::Barrier::new(workers));
        let handles: Vec<_> = (0..workers)
            .map(|worker| {
                let base = tmp.path().to_path_buf();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let logical = format!("synthetic-logical-{worker}");
                    let store = ArchiveStore::new(&base, "synthetic-project", &logical)?;
                    barrier.wait();
                    for round in 0..8 {
                        let text = format!("worker {worker} round {round}");
                        let reference = store.seal(&[msg("user", json!(text))], "note")?;
                        assert_eq!(store.fetch(&reference.id, 0, 1)?.len(), 1);
                        assert_eq!(store.fetch_note(&reference.id)?, "note");
                        store.search("worker", 4)?;
                    }
                    Ok::<_, io::Error>(())
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for result in results {
            result.unwrap();
        }
        let store = ArchiveStore::new(tmp.path(), "synthetic-project", "synthetic-reader").unwrap();
        assert_eq!(store.export_records().unwrap().len(), workers * 8);
    }

    /// A separate open file description is what another process holds. With
    /// no time to wait for it, a seal and a forget fail closed at once.
    #[test]
    fn a_lock_held_outside_this_process_fails_closed_at_once_when_the_wait_is_zero() {
        let (_tmp, mut store) = setup();
        store.process_wait = Duration::ZERO;
        let reference = store
            .seal(&[msg("user", json!("evidence"))], "note")
            .unwrap();
        let foreign = store.fs.lock().unwrap();
        let started = Instant::now();
        let results = [
            store
                .seal(&[msg("user", json!("other evidence"))], "")
                .map(drop),
            store.forget(&reference.id),
        ];
        for result in results {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        }
        assert!(started.elapsed() < PROCESS_LOCK_WAIT);
        drop(foreign);
        assert_eq!(store.fetch(&reference.id, 0, 1).unwrap().len(), 1);
        assert_eq!(store.export_records().unwrap().len(), 1);
    }

    fn other(tmp: &tempfile::TempDir) -> ArchiveStore {
        ArchiveStore::new(tmp.path(), "synthetic-project", "synthetic-other").unwrap()
    }

    #[test]
    fn an_operation_queues_behind_the_one_in_progress_in_this_process() {
        let (tmp, store) = setup();
        let held = store.exclusive().unwrap();
        let queued = other(&tmp);
        let waiter =
            std::thread::spawn(move || queued.seal(&[msg("user", json!("queued evidence"))], ""));
        let gate = gate_of(&store);
        wait_until(|| gate.waited.load(Ordering::Acquire) == 1);
        assert!(!waiter.is_finished());
        drop(held);
        let reference = waiter.join().unwrap().unwrap();
        assert_eq!(store.fetch(&reference.id, 0, 1).unwrap().len(), 1);
    }

    #[test]
    fn a_stuck_operation_in_this_process_fails_closed_after_the_bounded_wait() {
        let (tmp, store) = setup();
        let held = store.exclusive().unwrap();
        let mut queued = other(&tmp);
        queued.wait = Duration::from_millis(50);
        let started = Instant::now();
        let evidence = [msg("user", json!("queued evidence"))];
        let error = queued.seal(&evidence, "").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(error.to_string(), BUSY);
        assert!(started.elapsed() >= queued.wait);
        drop(held);
        assert!(queued.search("", 1).unwrap().is_empty());
        queued.seal(&evidence, "").unwrap();
    }

    #[test]
    fn an_aborted_operation_never_starts_even_while_queued() {
        let (tmp, store) = setup();
        let abort = Arc::new(AtomicBool::new(true));
        let error = other(&tmp)
            .with_abort(abort.clone())
            .seal(&[msg("user", json!("abandoned before start"))], "")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);

        abort.store(false, Ordering::Release);
        let held = store.exclusive().unwrap();
        let queued = other(&tmp).with_abort(abort.clone());
        let waiter = std::thread::spawn(move || {
            queued.seal(&[msg("user", json!("abandoned while queued"))], "")
        });
        let gate = gate_of(&store);
        wait_until(|| gate.waited.load(Ordering::Acquire) == 1);
        assert!(!waiter.is_finished());
        abort.store(true, Ordering::Release);
        // Returns while the earlier operation still holds the archive.
        let error = waiter.join().unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        drop(held);
        assert!(store.export_records().unwrap().is_empty());
        assert!(store.search("", 1).unwrap().is_empty());
    }

    #[test]
    fn seal_with_note_returns_the_note_stored_with_the_segment() {
        let (_tmp, store) = setup();
        let messages = [msg("user", json!("evidence"))];
        let (first, note) = store.seal_with_note(&messages, "first note").unwrap();
        assert_eq!(note, "first note");
        let (again, note) = store.seal_with_note(&messages, "second note").unwrap();
        assert_eq!(again, first);
        assert_eq!(note, "first note");
        assert_eq!(store.fetch_note(&first.id).unwrap(), "first note");
        store.forget(&first.id).unwrap();
        let error = store.seal_with_note(&messages, "").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    /// The names in `store`'s project directory, sorted.
    fn project_entries(tmp: &tempfile::TempDir, store: &ArchiveStore) -> Vec<String> {
        let dir = tmp.path().join("context-archives").join(&store.scope);
        let entries = std::fs::read_dir(dir).unwrap();
        let mut names: Vec<_> = entries
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// Hold the archive lock for `hold`, as another process would: through a
    /// separate open file description.
    fn foreign_lock_for(store: &ArchiveStore, hold: Duration) -> std::thread::JoinHandle<()> {
        let foreign = store.fs.lock().unwrap();
        std::thread::spawn(move || {
            std::thread::sleep(hold);
            drop(foreign);
        })
    }

    /// Sets its flag when dropped, so that threads which loop until it is set
    /// stop on every path, a failed assertion included.
    struct Raise<'a>(&'a AtomicBool);
    impl Drop for Raise<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    fn evidence() -> [SharedMessage; 1] {
        [msg("user", json!("evidence"))]
    }

    /// Fetch, note and search of the one segment sealed from `evidence()`.
    fn assert_reads(reader: &ArchiveStore, reference: &ArchiveRef) {
        assert_eq!(reader.fetch(&reference.id, 0, 8).unwrap(), evidence());
        assert_eq!(reader.fetch_note(&reference.id).unwrap(), "note");
        let rows = reader.search("evidence", 8).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, reference.id);
    }

    fn assert_busy(result: io::Result<()>) {
        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(error.to_string(), BUSY);
    }

    #[test]
    fn seal_waits_for_a_foreign_lock_then_succeeds() {
        let (_tmp, store) = setup();
        let hold = Duration::from_millis(300);
        let started = Instant::now();
        let holder = foreign_lock_for(&store, hold);
        let reference = store.seal(&evidence(), "note").unwrap();
        assert!(started.elapsed() >= hold);
        holder.join().unwrap();
        assert_reads(&store, &reference);
    }

    #[test]
    fn forget_waits_for_a_foreign_lock_then_succeeds() {
        let (_tmp, store) = setup();
        let reference = store.seal(&evidence(), "note").unwrap();
        let hold = Duration::from_millis(300);
        let started = Instant::now();
        let holder = foreign_lock_for(&store, hold);
        store.forget(&reference.id).unwrap();
        assert!(started.elapsed() >= hold);
        holder.join().unwrap();
        let error = store.fetch(&reference.id, 0, 1).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn seal_reports_busy_after_the_wait() {
        let (tmp, mut store) = setup();
        assert_eq!(store.process_wait, PROCESS_LOCK_WAIT);
        assert_eq!(PROCESS_LOCK_WAIT, Duration::from_secs(5));
        store.process_wait = Duration::from_millis(200);
        let foreign = store.fs.lock().unwrap();
        let started = Instant::now();
        assert_busy(store.seal(&evidence(), "note").map(drop));
        assert!(started.elapsed() >= store.process_wait);
        // Nothing was written; the caller keeps its source and can retry.
        assert_eq!(project_entries(&tmp, &store), ["lock"]);
        drop(foreign);
        let reference = store.seal(&evidence(), "note").unwrap();
        // The same for a forget: busy after the wait, the segment untouched.
        let foreign = store.fs.lock().unwrap();
        let started = Instant::now();
        assert_busy(store.forget(&reference.id));
        assert!(started.elapsed() >= store.process_wait);
        drop(foreign);
        assert_reads(&store, &reference);
    }

    #[test]
    fn abandon_while_polling_returns_interrupted() {
        let (tmp, store) = setup();
        let foreign = store.fs.lock().unwrap();
        let abort = Arc::new(AtomicBool::new(false));
        let mut abandoned = other(&tmp).with_abort(abort.clone());
        // Only the abort can end this wait in time.
        abandoned.process_wait = Duration::from_secs(30);
        let started = Instant::now();
        let waiter = std::thread::spawn(move || abandoned.seal(&evidence(), ""));
        let gate = gate_of(&store);
        // Through the gate, so trying the lock.
        wait_until(|| *gate.held.lock().unwrap());
        abort.store(true, Ordering::Release);
        let error = waiter.join().unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(started.elapsed() < Duration::from_secs(30));
        // It wrote nothing and did not keep the gate.
        assert_eq!(project_entries(&tmp, &store), ["lock"]);
        assert!(!*gate.held.lock().unwrap());
        drop(foreign);
        let reference = store.seal(&evidence(), "note").unwrap();
        assert_reads(&store, &reference);
    }

    #[test]
    fn fetch_and_search_ignore_a_foreign_lock() {
        let (_tmp, mut store) = setup();
        let reference = store.seal(&evidence(), "note").unwrap();
        // A read that tried the lock would now fail at once.
        store.process_wait = Duration::ZERO;
        let _foreign = store.fs.lock().unwrap();
        assert_reads(&store, &reference);
        assert_busy(store.forget(&reference.id));
    }

    #[test]
    fn fetch_and_search_ignore_the_in_process_gate() {
        let (tmp, store) = setup();
        let reference = store.seal(&evidence(), "note").unwrap();
        let _held = store.exclusive().unwrap();
        let mut reader = other(&tmp);
        // A read that queued behind the gate would now fail at once.
        reader.wait = Duration::ZERO;
        assert_reads(&reader, &reference);
        assert_busy(reader.forget(&reference.id));
    }

    #[test]
    fn missing_id_without_legacy_files_is_not_found_without_locking() {
        let (tmp, store) = setup();
        let mut reader = other(&tmp);
        (reader.wait, reader.process_wait) = (Duration::ZERO, Duration::ZERO);
        let missing = "a".repeat(32);
        // With the gate and the lock both held by someone else.
        let absent = |id: &str| {
            let _held = store.exclusive().unwrap();
            for result in [reader.fetch(id, 0, 1).map(drop), reader.fetch_note(id).map(drop)] {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
            }
            assert!(reader.search("", 8).unwrap().is_empty());
        };
        // No shelf yet, then a shelf without the id, then the id's tombstone.
        absent(&missing);
        let reference = store.seal(&evidence(), "note").unwrap();
        store.forget(&reference.id).unwrap();
        absent(&missing);
        absent(&reference.id);
    }

    /// A read holds no lock, so a forget can land anywhere inside it. Whatever
    /// the timing, the read is the whole live record or "not found": never a
    /// partial record (`InvalidData`), never a refused file
    /// (`PermissionDenied`), and never the live record once forget has returned.
    #[test]
    fn reader_racing_forget_sees_live_or_not_found() {
        let (tmp, store) = setup();
        let reader = other(&tmp);
        for round in 0..100 {
            let words = format!("evidence {round};");
            let messages = [msg("user", json!(words)), msg("assistant", json!("reply"))];
            let id = store.seal(&messages, "note").unwrap().id;
            // Whether the segment was there: eight fetches, a note and a search.
            let live = |kind: usize| {
                let found = match kind {
                    8 => reader.fetch_note(&id).map(|note| assert_eq!(note, "note")),
                    9 => reader.search(&words, 2).and_then(|rows| match rows.as_slice() {
                        [] => Err(not_found()),
                        [row] => {
                            assert_eq!((row.id.as_str(), row.message_count), (id.as_str(), 2));
                            Ok(())
                        }
                        rows => panic!("one segment, {} rows", rows.len()),
                    }),
                    _ => reader
                        .fetch(&id, 0, MAX_FETCH_MESSAGES)
                        .map(|found| assert_eq!(found, messages)),
                };
                match found {
                    Ok(()) => true,
                    Err(error) => {
                        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
                        false
                    }
                }
            };
            let forgotten = AtomicBool::new(false);
            let start = std::sync::Barrier::new(12);
            std::thread::scope(|scope| {
                // The path lookup holds no lock either; the file is always there.
                scope.spawn(|| {
                    start.wait();
                    loop {
                        let after = forgotten.load(Ordering::Acquire);
                        let located = archive_file(tmp.path(), "synthetic-project", &id);
                        assert!(located.unwrap().is_some());
                        if after {
                            break;
                        }
                    }
                });
                for kind in 0..10 {
                    let (live, forgotten, start) = (&live, &forgotten, &start);
                    scope.spawn(move || {
                        start.wait();
                        let mut gone = false;
                        loop {
                            // Read first: set means forget returned before this read began.
                            let after = forgotten.load(Ordering::Acquire);
                            if live(kind) {
                                assert!(!gone && !after, "read after it was forgotten");
                            } else if after {
                                break;
                            } else {
                                gone = true;
                            }
                        }
                    });
                }
                start.wait();
                // Raised once forget has returned.
                let _returned = Raise(&forgotten);
                store.forget(&id).unwrap();
            });
        }
    }

    /// The race above without the timing: a read has opened the file, the
    /// forget replaces it, and the read then runs to its end on what it holds.
    #[test]
    fn reader_holding_the_old_file_reads_the_whole_live_record() {
        let (tmp, store) = setup();
        let reference = store.seal(&evidence(), "note").unwrap();
        let name = archive_name(&reference.id);
        let project = tmp.path().join("context-archives").join(&store.scope);
        let path = project.join("s000001").join(&name);
        let original = std::fs::read(&path).unwrap();
        // Opened the way `read_with` opens it, once for each kind of read.
        let shelf = store.fs.open_child("s000001", true).unwrap().unwrap();
        let open = || shelf.open(&c_name(&name).unwrap(), libc::O_RDONLY).unwrap();
        let (unlocked, locked) = (open(), open());
        store.forget(&reference.id).unwrap();
        // The open file has lost its name to the tombstone and has no link
        // left. The read that holds no lock still gets all of the record.
        let bytes = PrivateDir::read_open(unlocked, MAX_SEGMENT_BYTES, true).unwrap();
        assert_eq!(bytes, original);
        let Body::Live { messages, note, .. } = store.record(&name, &bytes).unwrap().body else {
            panic!("not the live record");
        };
        let messages: Vec<_> = messages.into_iter().map(|m| m.message).collect();
        assert_eq!((messages.as_slice(), note.as_str()), (&evidence()[..], "note"));
        // The read made under the lock, which repairs the mode of what it
        // reads and where nothing can replace a file, still wants one name.
        let error = PrivateDir::read_open(locked, MAX_SEGMENT_BYTES, false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        // Every read that starts now gets the tombstone.
        assert_ne!(std::fs::read(&path).unwrap(), original);
        let error = store.fetch(&reference.id, 0, 1).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let located = archive_file(tmp.path(), "synthetic-project", &reference.id);
        assert_eq!(located.unwrap(), Some(path));
    }

    /// Readers hold no lock while segments, and the shelves they go to, are
    /// added under it. A search never fails, every row is a sealed segment,
    /// and a segment whose seal returned before the search began is found. A
    /// fetch that asks for a segment from before its seal until it is there
    /// gets "not found" or the whole record, never part of one.
    #[test]
    fn searcher_racing_seals_never_errors() {
        let (tmp, store) = setup();
        let reader = other(&tmp);
        let words = |index: usize| format!("racing seal {index};");
        let window = |index: usize| [msg("user", json!(words(index)))];
        // An id is the digest of what is sealed, so it is known beforehand.
        // Three shelves: the readers also meet shelves being created.
        let ids: Vec<String> = (0..300)
            .map(|index| {
                let projection = project_archive("synthetic-logical", &window(index), "", None);
                projection.unwrap().digest[..32].to_owned()
            })
            .collect();
        let sealed = std::sync::atomic::AtomicUsize::new(0);
        let done = AtomicBool::new(false);
        let (searching, fetching) = (AtomicBool::new(false), AtomicBool::new(false));
        std::thread::scope(|scope| {
            scope.spawn(|| loop {
                let finished = done.load(Ordering::Acquire);
                // Seals that returned before this search began.
                let count = sealed.load(Ordering::Acquire);
                if let Some(newest) = count.checked_sub(1) {
                    let (rows, unread) = reader.search_scan(&words(newest), 2).unwrap();
                    let found: Vec<_> = rows.iter().map(|row| row.id.as_str()).collect();
                    assert_eq!((found, unread), (vec![ids[newest].as_str()], None));
                }
                let rows = reader.search("racing seal", MAX_SEARCH_RESULTS).unwrap();
                assert!(rows.len() >= count.min(MAX_SEARCH_RESULTS));
                for (index, row) in rows.iter().enumerate() {
                    assert!(ids.contains(&row.id));
                    assert_eq!((row.message_count, row.source_message_count), (1, 1));
                    assert!(rows[..index].iter().all(|earlier| earlier.id != row.id));
                }
                searching.store(true, Ordering::Release);
                if finished {
                    break;
                }
            });
            let fetcher = scope.spawn(|| {
                let mut next = 0;
                while next < ids.len() {
                    let finished = done.load(Ordering::Acquire);
                    match reader.fetch(&ids[next], 0, 2) {
                        Ok(found) => {
                            assert_eq!(found, window(next));
                            next += 1;
                        }
                        Err(error) => {
                            assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
                            fetching.store(true, Ordering::Release);
                            if finished {
                                break;
                            }
                        }
                    }
                }
                next
            });
            let stop = Raise(&done);
            // Both readers are running before the first seal.
            wait_until(|| searching.load(Ordering::Acquire) && fetching.load(Ordering::Acquire));
            for (index, id) in ids.iter().enumerate() {
                assert_eq!(&store.seal(&window(index), "").unwrap().id, id);
                sealed.store(index + 1, Ordering::Release);
            }
            drop(stop);
            assert_eq!(fetcher.join().unwrap(), ids.len());
        });
        assert_eq!(store.shelves().unwrap().len(), 3);
    }

    /// Every file name in the shelves of `store`'s project directory.
    fn shelved(tmp: &tempfile::TempDir, store: &ArchiveStore) -> Vec<String> {
        let project = tmp.path().join("context-archives").join(&store.scope);
        let mut names = Vec::new();
        for shelf in store.shelves().unwrap() {
            let entries = std::fs::read_dir(project.join(shelf)).unwrap();
            names.extend(entries.map(|entry| entry.unwrap().file_name().into_string().unwrap()));
        }
        names
    }

    /// Writers inside `publish` at the same moment with no lock between them:
    /// what is left of a seal when the lock file was deleted under it, or on
    /// a filesystem that does not lock. Each write is a file of its own and
    /// the rename cannot replace, so a writer told `Ok` has its bytes under
    /// its name, whole, and a name is never given to two of them.
    #[test]
    fn writers_inside_publish_at_once_without_the_lock() {
        const WRITERS: usize = 4;
        const ROUNDS: usize = 150;
        let (tmp, store) = setup();
        let shelf = store.fs.child("s000001", true).unwrap();
        // A writer's bytes are its own from the first to the last.
        let bytes = |writer: usize, round: usize| {
            format!("writer {writer} round {round};").repeat(4096).into_bytes()
        };
        let own = |writer: usize, round: usize| format!("{:032x}.json", writer * ROUNDS + round);
        let wanted = |round: usize| format!("{:032x}.json", WRITERS * ROUNDS + round);
        let start = std::sync::Barrier::new(WRITERS);
        let won: Vec<Vec<usize>> = std::thread::scope(|scope| {
            let writers: Vec<_> = (0..WRITERS)
                .map(|writer| {
                    let (shelf, start) = (&shelf, &start);
                    scope.spawn(move || {
                        start.wait();
                        let mut won = Vec::new();
                        for round in 0..ROUNDS {
                            // A name of its own: always stored.
                            let stored = shelf.publish(&own(writer, round), &bytes(writer, round), true);
                            stored.unwrap();
                            // A name every writer wants: stored by one.
                            match shelf.publish(&wanted(round), &bytes(writer, round), true) {
                                Ok(()) => won.push(round),
                                Err(error) => assert_eq!(error.kind(), io::ErrorKind::AlreadyExists),
                            }
                        }
                        won
                    })
                })
                .collect();
            writers.into_iter().map(|writer| writer.join().unwrap()).collect()
        });
        let read = |name: &str| shelf.read_with(name, MAX_SEGMENT_BYTES, true).unwrap().unwrap();
        for round in 0..ROUNDS {
            for writer in 0..WRITERS {
                assert!(read(&own(writer, round)) == bytes(writer, round), "{writer} {round}");
            }
            let winners: Vec<_> = (0..WRITERS).filter(|writer| won[*writer].contains(&round)).collect();
            assert_eq!(winners.len(), 1, "round {round}");
            assert!(read(&wanted(round)) == bytes(winners[0], round), "round {round}");
        }
        // And no temporary file is left.
        assert_eq!(shelved(&tmp, &store).len(), (WRITERS + 1) * ROUNDS);
    }

    /// The same one level up: what a seal does once it holds the lock, done
    /// by writers that hold nothing. Every window is stored, and is what the
    /// writer was told it is.
    #[test]
    fn seals_at_once_without_the_lock_each_store_their_window() {
        const WRITERS: usize = 4;
        let (tmp, store) = setup();
        let window = |words: String| [msg("user", json!(words))];
        let project = |messages: &[SharedMessage], note: &str| {
            project_hashed(&store.logical, messages, note, None, Reduce::None).unwrap()
        };
        // `rounds` windows by each writer, at once: the same windows (with a
        // note of the writer's own) or windows of its own.
        let seal_all = |same: bool, rounds: usize| -> Vec<Vec<(ArchiveRef, String)>> {
            let start = std::sync::Barrier::new(WRITERS);
            std::thread::scope(|scope| {
                let writers: Vec<_> = (0..WRITERS)
                    .map(|writer| {
                        let (store, start) = (&store, &start);
                        scope.spawn(move || {
                            start.wait();
                            let note = format!("note of writer {writer}");
                            let name = if same { "any".to_owned() } else { format!("writer {writer}") };
                            let seal = |round| store.store(project(&window(format!("{name} window {round};")), &note));
                            (0..rounds).map(|round| seal(round).unwrap()).collect()
                        })
                    })
                    .collect();
                writers.into_iter().map(|writer| writer.join().unwrap()).collect()
            })
        };
        // One window sealed by all, within one shelf: one segment. Every
        // writer is given its id and the note that is stored with it, which
        // is the note of whichever writer's file it is.
        let same = seal_all(true, 30);
        for (round, (reference, note)) in same[0].iter().enumerate() {
            let words = format!("any window {round};");
            assert_eq!(store.fetch(&reference.id, 0, 2).unwrap(), window(words));
            assert_eq!(&store.fetch_note(&reference.id).unwrap(), note);
            assert!(note.starts_with("note of writer "));
            for other in &same[1..] {
                assert_eq!(other[round], (reference.clone(), note.clone()));
            }
        }
        assert_eq!(shelved(&tmp, &store).len(), 30);
        // Windows of their own, enough to start two more shelves while the
        // writers race for them.
        let own = seal_all(false, 80);
        for (writer, sealed) in own.iter().enumerate() {
            for (round, (reference, note)) in sealed.iter().enumerate() {
                let words = format!("writer {writer} window {round};");
                assert_eq!(store.fetch(&reference.id, 0, 2).unwrap(), window(words));
                assert_eq!(note, &format!("note of writer {writer}"));
            }
        }
        // One file each, nothing replaced, and no temporary file left.
        let mut names = shelved(&tmp, &store);
        assert_eq!(names.len(), 30 + WRITERS * 80);
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 30 + WRITERS * 80);
        assert!(names.iter().all(|name| archive_id(name).is_some()));
        assert!(store.shelves().unwrap().len() >= 3);
        // The lock still works afterwards, and finds all of it.
        let (reference, note) = store.seal_with_note(&window("any window 0;".into()), "").unwrap();
        assert_eq!((&reference, &note), (&same[0][0].0, &same[0][0].1));
    }

    const SEAL_CHILD: &str = "SYNTHETIC_ARCHIVE_SEAL_CHILD";
    const CHILD_SEALS: usize = 100;

    /// The other process of `two_processes_seal_concurrently`, which runs this
    /// test binary again with the archive base in the environment. Does
    /// nothing in an ordinary run.
    #[test]
    fn two_processes_seal_child() {
        let Some(base) = std::env::var_os(SEAL_CHILD).map(PathBuf::from) else {
            return;
        };
        let name = format!("child-{}", std::process::id());
        let store = ArchiveStore::new(&base, "synthetic-project", &name).unwrap();
        std::fs::write(base.join(format!("ready-{}", std::process::id())), b"").unwrap();
        wait_until(|| base.join("go").exists());
        for round in 0..CHILD_SEALS {
            let words = format!("{name} round {round};");
            store.seal(&[msg("user", json!(words))], "note").unwrap();
        }
    }

    /// Two real processes seal into one project at the same moment. Each
    /// waits for the other's lock instead of failing as busy.
    #[test]
    #[serial_test::parallel(process_spawn)]
    fn two_processes_seal_concurrently() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, module) = module_path!().split_once("::").unwrap();
        let child = format!("{module}::two_processes_seal_child");
        let mut children: Vec<_> = (0..2)
            .map(|_| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", &child, "--nocapture"])
                    .env(SEAL_CHILD, tmp.path())
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        // Both have opened the store: let them seal at once.
        let ready = |child: &std::process::Child| format!("ready-{}", child.id());
        wait_until(|| children.iter().all(|child| tmp.path().join(ready(child)).exists()));
        std::fs::write(tmp.path().join("go"), b"").unwrap();
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        let store = ArchiveStore::new(tmp.path(), "synthetic-project", "synthetic-reader").unwrap();
        let project = tmp.path().join("context-archives").join(&store.scope);
        let sealed: usize = ["s000001", "s000002"]
            .iter()
            .map(|shelf| std::fs::read_dir(project.join(shelf)).unwrap().count())
            .sum();
        assert_eq!(sealed, 2 * CHILD_SEALS);
        for child in &children {
            for round in [0, CHILD_SEALS - 1] {
                let words = format!("child-{} round {round};", child.id());
                assert_eq!(store.search(&words, 2).unwrap().len(), 1, "{words}");
            }
        }
    }
}
