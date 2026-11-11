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
//! by [`ArchiveStore::seal_window`] as consecutive parts: each is filled from
//! where the one before ended until the next message would not fit, each is
//! an ordinary segment, and the returned part list is what puts the window
//! back together. Messages added to the end of a window change its last
//! part only. Only a single message that is
//! over a limit by itself is stored reduced, by as little as makes it fit.
//! First nothing is left out: a tool input that is nested too deep or has
//! too many nodes is stored as one string, its JSON text
//! ([`INPUT_JSON_KEY`]). Then what is too large as text too goes: the tool
//! inputs that are too large, then every tool input, then long strings and
//! block lists are cut short. Each omission is marked in place with its
//! size, and a reduced message carries `_archive_reduced` ([`REDUCED_KEY`]),
//! which no source message can supply, so it is never read back as an exact
//! one.
//!
//! The store keeps every segment: there is no cap on how many a project holds
//! or on their total size, and no seal deletes, moves or replaces another
//! segment. A project directory holds numbered shelf directories (`s000001`,
//! `s000002`, ...) of at most 128 segments each; a full shelf is followed by a
//! new one and shelves are never merged, renumbered or removed. A segment's
//! file name is its id (one half of its digest: the first 32 hex characters,
//! or the last 32 when something else has that name), so finding
//! one is one `openat` per shelf and one file read, never a scan. Directories
//! written before shelves existed keep their numbered slot files (`000.json`
//! to `127.json`); those are read and tombstoned in place and never added to.
//!
//! A seal is not refused because something else in the directory is odd. A
//! shelf name that is not a directory, a shelf that cannot be opened or made
//! private and a legacy slot that cannot be read are passed over, never
//! followed and never written to; the segment goes to the next shelf number
//! if the newest is one of them. That is for a shelf or a slot that is itself
//! the trouble, an I/O error reading it included, and it has a price: what
//! it holds is not seen, so a window in it is stored again and a tombstone in
//! it no longer refuses. An error that will pass (no descriptor or memory
//! left, an interrupted call: EMFILE, ENFILE, ENOMEM, EINTR, EAGAIN) is not
//! about the shelf, and the seal fails on it instead and is retried. If the file under a segment's id is not that
//! segment (corrupt, a link, another projection) it is left as it is and the
//! segment is stored under the other half of its digest instead. A seal
//! looks under both halves before it writes anything, so a later seal of the
//! same projection finds it under either, whatever has become of the other
//! name. Only what a file is sends a segment to its other name: a read that
//! fails for another reason (an I/O error, no descriptor left) fails the
//! seal, which is retried, rather than store a second copy. The one thing a seal still
//! honours in a file it cannot validate is a tombstone: JSON that names the
//! projection and says it was forgotten refuses that seal. A fetch or a
//! forget is stricter: when it does not find its segment and had to pass
//! over a shelf or a legacy slot, the error says so instead of "not found".
//! Neither is failed by a damaged neighbour: a segment that can be read is
//! found whatever the shelf or slot next to it is, and a search passes over
//! a legacy slot it cannot read as it passes over a corrupt shelf file. Only
//! the migration export reads the legacy slots as a whole and fails as a
//! whole.
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
//! under the lock, shared, if the directory has a lock file.
//!
//! The lock file (`lock` in the project directory) is only ever locked: the
//! store never writes to it, truncates it or reads it. So it is usable as
//! long as it is a regular file of this user, whatever its link count (a
//! hard-link backup gives it a second name) and whatever its mode (read-only
//! after a restore: it is then opened for reading, and a lock on that
//! descriptor excludes as any other does). A link (never followed), anything
//! that is not a regular file, a file of another user and a file its mode
//! does not let its owner open are refused, with an error that names the file.
//!
//! A read writes nothing. [`ArchiveStore::new`] is for a seal or a forget: it
//! creates the directories, makes them private and syncs them.
//! [`ArchiveStore::open_existing`] is for a fetch or a search: it opens what
//! is there, and a project with no directory has nothing to read. Whichever
//! opened the store, a fetch, a note read and a search create no file (not
//! the lock file either), change no mode and sync nothing.
//!
//! Unix-only private, handle-relative no-symlink filesystem; other platforms
//! fail Unsupported. Live records are immutable; forget atomically replaces a
//! record with a content-free id/digest/scope tombstone, and is the only
//! operation that replaces a file. The digest covers ONLY
//! the eligible projection and source indices, not the note or forbidden data.
//! Nor does it cover `created_ms`, the time a live record was first written:
//! a seal writes the time of the write, a record from before the field has
//! none, and a tombstone never has one.
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
/// The highest shelf number. Shelves are numbered in the order they are made,
/// so the store gets here after 128 million segments; a directory named with
/// a higher number was not made by the store and is not a shelf.
const LAST_SHELF: u64 = 999_999;
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
/// Key of a tool input that is stored as one string: the input's JSON text,
/// screened like any string. [`ArchiveStore::seal_window`] stores an input
/// this way when it is nested too deep or has too many nodes for a segment,
/// instead of leaving it out. Content can imitate the key; only in a message
/// that carries [`REDUCED_KEY`] is it the store's.
pub const INPUT_JSON_KEY: &str = "_archive_json";
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
/// What a fetch says of an archive that is not there, was forgotten, or is in
/// another project: the three are not told apart.
pub const NOT_FOUND: &str = "archive not available in this scope";
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

/// What [`ArchiveStore::forget`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forgotten {
    /// The id of every file that held the content live and is a tombstone
    /// now: the id asked for first if it is one of them, then every other
    /// file the same content was stored in. An id is here twice when two
    /// shelves held a file under it. Empty when every copy that could be
    /// read had been forgotten before.
    pub ids: Vec<String>,
    /// `Some` when a shelf, a legacy slot or a file that may hold another
    /// copy could not be read, or was read and is not a valid record: how
    /// many such places, and the first with its error. A copy there was not reached and is still live. Forgetting
    /// the id again once the place can be read reaches it.
    pub unread: Option<String>,
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

/// What is changed in a message that is over a segment budget by itself: it
/// cannot be stored whole, and being alone it cannot be cut out of its part.
/// A step is tried only when the one before it still does not fit, and
/// whatever a step does not name stays exact. The first two steps leave
/// nothing out: the structural budgets (depth, nodes) are about the shape of
/// a tool input, and as one string, its JSON text under
/// [`INPUT_JSON_KEY`], the same input has no shape to count. Only what is
/// over the byte budget as text too is left out. Each omission is replaced
/// by a marker that says how large it was, and the stored message carries
/// [`REDUCED_KEY`] in every case.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reduce {
    /// Nothing: the exact projection, the only one `seal` and
    /// [`project_archive`] ever make.
    None,
    /// Each tool input that is over a budget by itself: stored as its JSON
    /// text, or left out if that text is over the byte budget.
    LargeInputs,
    /// Every tool input as its JSON text: for inputs that each fit and
    /// together have too many nodes.
    InputsAsText,
    /// Every tool input left out.
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
            Reduce::LargeInputs => Some(Reduce::InputsAsText),
            Reduce::InputsAsText => Some(Reduce::Inputs),
            Reduce::Inputs => Some(Reduce::Text),
            Reduce::Text => None,
        }
    }

    /// What the preflight charges for a tool input that this step does not
    /// store exact, instead of walking it: the bytes of its JSON text, or 0
    /// when it is left out. `None` when the input is stored exact. Under
    /// `LargeInputs` the projector also stores as text an input whose
    /// screened form is over a budget; the preflight has then charged for it
    /// exact, which errs on the strict side.
    fn in_place_of(self, input: &Value) -> Option<usize> {
        let as_text = || input_text_cost(input).unwrap_or(0);
        match self {
            Reduce::None => None,
            Reduce::LargeInputs => InputBudget::alone(input).is_err().then(as_text),
            Reduce::InputsAsText => Some(as_text()),
            Reduce::Inputs | Reduce::Text => Some(0),
        }
    }
}

/// What a tool input costs against the byte budget when it is stored as one
/// string, its JSON text under [`INPUT_JSON_KEY`]. `None` when that is over
/// the budget of one segment by itself: such an input is left out.
fn input_text_cost(input: &Value) -> Option<usize> {
    let cost = json_len(input).saturating_add(INPUT_JSON_KEY.len() + 16);
    (cost <= MAX_INPUT_BYTES).then_some(cost)
}

/// Whether nothing anywhere in a tool input is marked as not to be stored:
/// the rule of [`InputBudget::input`], which withholds the whole input for
/// one such object, without its structural budget. Not recursive, so it does
/// not matter how deep the input is.
fn input_eligible(input: &Value) -> bool {
    let mut pending = vec![input];
    while let Some(value) = pending.pop() {
        if eligible_class(value).is_none() {
            return false;
        }
        match value {
            Value::Array(values) => pending.extend(values),
            Value::Object(values) => pending.extend(values.values()),
            _ => {}
        }
    }
    true
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
        as_text: Cell::new(0),
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
    /// Tool inputs of that message stored as their JSON text so far.
    as_text: Cell<usize>,
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
    /// The project directory as a path, for messages only: nothing is
    /// opened by it.
    directory: PathBuf,
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

/// Why the lock file of a project directory is not one the store uses.
/// [`ArchiveStore::lock_error`] makes the message, with the file's path.
#[derive(Debug)]
struct LockRefused(&'static str);

impl std::fmt::Display for LockRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "archive lock file {}", self.0)
    }
}

impl std::error::Error for LockRefused {}

/// What a lookup by id found on the shelves: the newest valid record under
/// the id.
struct Located {
    /// The name of the shelf it is on.
    shelf: String,
    record: Record,
    /// A newer shelf has a file under the id that is not a valid record and
    /// still says this record's content was forgotten.
    forgotten: bool,
}

/// What a seal finds under the id it would store a projection as.
enum Shelved {
    Nothing,
    /// The projection itself, live or forgotten, and the shelf it is on.
    This(PrivateDir, Record),
    /// Something else, or something that cannot be read.
    Other,
}

/// What a search did not read, so that an empty or a short result is not
/// taken for "the project holds nothing else".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unscanned {
    /// `Some(n)`: the scan budget stopped the search after the `n` newest
    /// segment files, and older ones were not read.
    pub stopped_after: Option<usize>,
    /// The shelves, legacy slots and segment files the search passed over
    /// because they could not be read or do not hold a valid record: how
    /// many, and the first by name with its error. Names and errors only,
    /// never content.
    pub unread: Option<String>,
}

/// The shelves, legacy slots and files an operation had to pass over
/// because they could not be read. What it was looking for may be in one of
/// them, so "not found" or "that is all" would be a guess.
#[derive(Default)]
struct Unread {
    places: usize,
    first: Option<(String, io::Error)>,
}

impl Unread {
    fn pass(&mut self, place: &str, error: io::Error) {
        // The name and the error only: never content.
        tracing::warn!(%place, %error, "archive lookup passed over what it could not read");
        self.places += 1;
        self.first.get_or_insert_with(|| (place.to_owned(), error));
    }

    /// What an operation that went on without these places tells its caller:
    /// how many there were and the first, by name, with its error. `None`
    /// when everything could be read.
    fn notice(&self) -> Option<String> {
        let (place, error) = self.first.as_ref()?;
        let places = self.places;
        Some(format!("{places} place(s) could not be read, first {place}: {error}"))
    }

    /// The error of a lookup that did not find its segment: "not found" when
    /// everything could be read, otherwise what could not be, with the kind
    /// of the first failure and never `NotFound`.
    fn not_found(self) -> io::Error {
        let Some((place, error)) = self.first else {
            return not_found();
        };
        let kind = match error.kind() {
            io::ErrorKind::NotFound => io::ErrorKind::Other,
            kind => kind,
        };
        let places = self.places;
        io::Error::new(
            kind,
            format!(
                "archive not found in what could be read; {places} place(s) that may hold it \
                 could not be read, first {place}: {error}"
            ),
        )
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
    /// When the record was first written, in milliseconds since the Unix
    /// epoch. Optional: a record from before the field existed has none and
    /// is as valid as one that has it, and a tombstone never has one (a
    /// forget leaves no time behind). Not part of the digest, so it says
    /// nothing about which content a record holds. No other field may be
    /// added to a version 1 record: any other unknown key is still refused.
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    created_ms: Option<u64>,
    body: Body,
}

/// A `created_ms` that is in a record is a whole number of milliseconds:
/// `null` is not a way to leave it out.
fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    u64::deserialize(deserializer).map(Some)
}

/// When a record is to say it was written, and what time its file is to
/// carry: what an import supplies for a record that was first written
/// somewhere else. A seal has neither to give and uses [`Written::now`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Written {
    /// The record's `created_ms`, stored as given. `None` stores a record
    /// without the field.
    pub(crate) created_ms: Option<u64>,
    /// The time of last access and modification of the record's file.
    /// `None` leaves it the time of the write.
    pub(crate) modified: Option<std::time::SystemTime>,
}

impl Written {
    /// What a seal writes: the time of the write. A clock set before 1970
    /// has no such number, and the record then goes without the field
    /// rather than the save be refused.
    fn now() -> Self {
        let since_epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
        Written {
            created_ms: since_epoch.ok().and_then(|since| u64::try_from(since.as_millis()).ok()),
            modified: None,
        }
    }
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
    /// Open a project's archive for a seal or a forget. Creates the base,
    /// `context-archives` and project directories if they are missing, makes
    /// them private again and syncs them, every time. A read must not go
    /// through here: see [`open_existing`](Self::open_existing).
    pub fn new(base: &Path, scope_key: &str, logical_id: &str) -> io::Result<Self> {
        let (scope, logical) = Self::keys(scope_key, logical_id)?;
        let fs = PrivateDir::new(base, &scope)?;
        Ok(Self::opened(fs, base, scope, logical))
    }

    /// Open a project's archive for a read (fetch, fetch_note, search), if
    /// it is there. Nothing is created, no mode is changed and nothing is
    /// synced, so this also works where nothing can be written. `None` when
    /// the project has no archive directory: a search of it finds nothing and
    /// a fetch is [`NOT_FOUND`]. A link anywhere on the way is an error, as
    /// it is for `new`.
    pub fn open_existing(
        base: &Path,
        scope_key: &str,
        logical_id: &str,
    ) -> io::Result<Option<Self>> {
        let (scope, logical) = Self::keys(scope_key, logical_id)?;
        match PrivateDir::open_existing(base, &scope) {
            Ok(fs) => Ok(Some(Self::opened(fs, base, scope, logical))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The directory name of a project and the stored name of a
    /// conversation: the hashes of the two keys.
    fn keys(scope_key: &str, logical_id: &str) -> io::Result<(String, String)> {
        for key in [scope_key, logical_id] {
            validate_key(key)?;
        }
        let scope = digest_parts(&[scope_key.as_bytes()]);
        Ok((scope, digest_parts(&[logical_id.as_bytes()])))
    }

    fn opened(fs: PrivateDir, base: &Path, scope: String, logical: String) -> Self {
        Self {
            directory: base.join("context-archives").join(&scope),
            fs,
            scope,
            logical,
            redactor: None,
            abort: None,
            wait: LOCK_WAIT,
            process_wait: PROCESS_LOCK_WAIT,
        }
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
        self.locked(|fs| fs.lock().map(Some))
    }

    /// The error of a lock file that is not one the store uses, with the
    /// file by its path and what to do about it; any other error as it is.
    fn lock_error(&self, error: io::Error) -> io::Error {
        let refused = error.get_ref().and_then(|inner| inner.downcast_ref::<LockRefused>());
        let Some(LockRefused(what)) = refused else {
            return error;
        };
        let file = self.directory.join("lock");
        let file = file.display();
        io::Error::new(
            error.kind(),
            format!(
                "context archive lock file {file} {what}; nothing is saved or forgotten while \
                 it is. It holds no data: remove it (rm '{file}') and try again"
            ),
        )
    }

    /// What a read of the legacy slots holds instead: the gate and, with the
    /// same wait, the lock shared. The lock file is opened only if it is
    /// there and only for reading, because a read creates nothing and
    /// changes no mode. Where there is none the read goes on without it: a
    /// slot name points at a whole file either way.
    fn shared(&self) -> io::Result<Locked> {
        self.locked(PrivateDir::lock_existing)
    }

    fn locked(
        &self,
        lock: impl Fn(&PrivateDir) -> io::Result<Option<FlockGuard>>,
    ) -> io::Result<Locked> {
        let gate = self.enter(BUSY)?;
        // The wait the notes store shares (`lock_file::lock_within`).
        let file = crate::core::lock_file::lock_within(
            self.process_wait,
            PROCESS_LOCK_POLL,
            || match lock(&self.fs) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.check_abort()?;
                    Ok(None)
                }
                result => result.map(Some).map_err(|error| self.lock_error(error)),
            },
        )?
        .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, BUSY))?;
        Ok(Locked { file, _gate: gate })
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
    /// as [`seal_with_note`](Self::seal_with_note). A larger one is filled
    /// from its start: a part ends before the first message that would take
    /// it over a budget, and the next part starts there. Every part is an
    /// ordinary segment; together they hold every message of the window
    /// exactly once, in order. Only a message that is over a budget by
    /// itself is stored reduced, and is then a part of its own: it costs the
    /// window two parts more, the one that ends before it and itself.
    ///
    /// Where a part ends depends on the messages from its start to the one
    /// after its end, and on nothing after that, so every part but the last
    /// is the same however long the window is. (One corner: a last part that
    /// was over the size of a segment only with the note in it was cut
    /// shorter, and is not cut when it is no longer the last.) Parts are
    /// sealed first to last, each in its own locked operation, and the note
    /// goes to the last one. After an error the parts already sealed stay
    /// (callers keep the source history, as for `seal`). A retry on the same
    /// window finds them by id, writes only what is missing and returns the
    /// same list. A retry on a window that has grown since (a failed turn
    /// leaves the user's next message in it) finds every part but the last
    /// one of the earlier attempt; that one, if it was written, stays as a
    /// segment no list names, and its messages are stored again in the new
    /// last part.
    pub fn seal_window(&self, messages: &[SharedMessage], note: &str) -> io::Result<SealedWindow> {
        // Sealing fewer messages does not make a note smaller: one over its
        // limit is refused here, before any part exists.
        let redactor = self.redactor.clone();
        let screened = project_hashed(&self.logical, &[], note, redactor, Reduce::None)?.note;
        let mut window = SealedWindow {
            parts: Vec::new(),
            note: String::new(),
        };
        let mut start = 0;
        loop {
            // An abandoned seal projects no further part.
            self.check_abort()?;
            let mut end = self.fill(messages, start);
            // The fill knows the budgets a part is refused on before it is
            // projected. Its size once screened and written is known only
            // then. A part over that is tried again at the largest power of
            // two messages below its length, and so on down: the part ends
            // up as the largest power of two messages that fits, whatever
            // length it was first tried at.
            loop {
                let note = if end == messages.len() { note } else { "" };
                match self.seal_part(messages, start..end, note, &mut window) {
                    Ok(()) => break,
                    Err(error) if over_budget(&error) && end - start > 1 => {
                        end = start + (1 << (end - start - 1).ilog2());
                    }
                    Err(error) => return Err(error),
                }
            }
            start = end;
            if start == messages.len() {
                break;
            }
        }
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

    /// Where the part that starts at `start` ends when it is filled: before
    /// the first message that would take it over the message count or an
    /// input budget of one segment, which is what [`project_hashed`] refuses
    /// before it projects anything. Never before `start + 1` in a window
    /// that has a message there: a message that is over a budget by itself
    /// is a part of its own. One pass over the messages, without the
    /// redactor and without allocating.
    fn fill(&self, messages: &[SharedMessage], start: usize) -> usize {
        let mut budget = InputBudget {
            bytes: 0,
            nodes: 0,
            reduce: Reduce::None,
        };
        let limit = messages.len().min(start.saturating_add(MAX_MESSAGES));
        let redactor = self.redactor.is_some();
        let over = (start..limit).find(|&index| budget.message(&messages[index], redactor).is_err());
        let end = over.unwrap_or(limit).max(start + 1).min(messages.len());
        before_results(messages, start, end)
    }

    /// Seal `messages[range]` as one part, or fail with the budget it is
    /// over. `note` is the window's note for the part that ends the window
    /// and empty for every other.
    fn seal_part(
        &self,
        messages: &[SharedMessage],
        range: std::ops::Range<usize>,
        note: &str,
        window: &mut SealedWindow,
    ) -> io::Result<()> {
        let mut reduce = Reduce::None;
        loop {
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
                // Reduced only when a shorter part cannot help: one message,
                // alone.
                Err(error) => match reduce.next().filter(|_| range.len() == 1) {
                    Some(next) => reduce = next,
                    None => return Err(error),
                },
            }
        }
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
        let (reference, note) = self.seal_projection(projection, None)?;
        Ok((reference, note, reduced))
    }

    /// Store `projection` under the lock, as every seal does. `written` is
    /// `None` for a seal: the record gets the time of the write as its
    /// `created_ms`. An import, which stores records that were first written
    /// somewhere else, gives [`Written`] instead, and then that is what is
    /// stored: nothing on the way to the file puts the time of this write in
    /// its place, and a `created_ms` of `None` stores a record without the
    /// field. Content that is already stored is the answer either way and is
    /// left as it is, its `created_ms` and the time of its file included.
    ///
    /// The projection is not taken on trust ([`Self::admit`]): one of
    /// another chat, or one whose digest is not the digest of its content,
    /// is refused as `InvalidInput` before the lock is taken, so a refusal
    /// writes nothing at all.
    pub(crate) fn seal_projection(
        &self,
        projection: ArchiveProjection,
        written: Option<Written>,
    ) -> io::Result<(ArchiveRef, String)> {
        self.admit(&projection)?;
        let _lock = self.exclusive()?;
        self.store(projection, written.unwrap_or_else(Written::now))
    }

    /// Whether `projection` is one this store may write: of this chat, and
    /// with the digest of its own content. A projection has public fields
    /// and can be deserialized, so neither is a given. Stored unchecked, a
    /// record of another chat or with a digest that is not its content's is
    /// a file no read accepts, under a name that is then taken for good.
    /// The digest is looked at as text first: it is sliced into the two ids
    /// later, and only 64 ASCII characters can be.
    fn admit(&self, projection: &ArchiveProjection) -> io::Result<()> {
        let digest = projection.digest.as_str();
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid("archive projection digest is not 64 hex characters"));
        }
        if projection.logical_id != self.logical {
            return Err(invalid("archive projection is of another chat"));
        }
        let count = projection.source_message_count;
        if projection_digest(&projection.logical_id, count, &projection.messages)? != digest {
            return Err(invalid("archive projection digest is not the digest of its content"));
        }
        Ok(())
    }

    /// What a seal does once it holds the lock: find `projection` stored, or
    /// store it. The lock keeps two seals from doing this at once, but what
    /// comes back does not depend on it: a segment is written to a file of
    /// this call's own and renamed without replacing, so `Ok` always means
    /// that the file under the returned id holds this projection, whole.
    ///
    /// A projection has two names, the two halves of its digest, and both
    /// are looked under before anything is written: a copy under either is
    /// the answer, and a tombstone under either refuses the seal, whatever
    /// the other name holds. Only then is it written, under the first half
    /// if that name is free and under the second if something else has the
    /// first. Nothing else in the directory refuses it.
    ///
    /// `projection` has passed [`Self::admit`]: its digest is 64 ASCII
    /// characters, which the slices below rely on.
    fn store(
        &self,
        projection: ArchiveProjection,
        written: Written,
    ) -> io::Result<(ArchiveRef, String)> {
        let ArchiveProjection {
            digest,
            source_message_count,
            messages,
            note,
            ..
        } = projection;
        let mut record = Record {
            version: 1,
            scope: self.scope.clone(),
            logical: self.logical.clone(),
            id: String::new(),
            digest,
            created_ms: written.created_ms,
            body: Body::Live {
                source_message_count,
                messages,
                note: note.clone(),
            },
        };
        let mut legacy_read = false;
        // A name that was free when it was looked at can be taken by the
        // time of the rename, by another seal that the lock should have
        // kept out. Whatever is there then is looked at once more, like any
        // file found under the name: once for each of the two names.
        for _ in 0..3 {
            let shelves = self.shelves()?;
            let (mut stored, mut free) = (None, None);
            // Same projection, same names in the same order.
            for half in [0..32, 32..64] {
                let id = &record.digest[half];
                match self.shelved(&shelves, id, &record.digest)? {
                    Shelved::This(_, found) if matches!(found.body, Body::Tombstone) => {
                        return Err(forgotten());
                    }
                    Shelved::This(shelf, found) => {
                        stored.get_or_insert((shelf, found));
                    }
                    Shelved::Other => {}
                    Shelved::Nothing => {
                        free.get_or_insert_with(|| id.to_owned());
                    }
                }
            }
            if let Some((shelf, found)) = stored {
                return Self::already(&shelf, &found);
            }
            if !legacy_read {
                // Sealed before shelves existed, under a random id.
                if let Some(found) = self.legacy_projection(&record.digest)? {
                    return Self::already(&self.fs, &found);
                }
                legacy_read = true;
            }
            let Some(id) = free else {
                break;
            };
            record.id = id;
            let bytes = bounded_json(&record, MAX_SEGMENT_BYTES)?;
            // Created new: a file that is already there is never renamed over.
            let shelf = self.writable_shelf(&shelves)?;
            let name = archive_name(&record.id);
            match shelf.publish_dated(&name, &bytes, true, written.modified) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                result => return result.and_then(|()| Ok((record.reference()?, note))),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "both archive ids of this projection are taken by other files",
        ))
    }

    /// What a seal finds under `id` on the shelves, newest first. A shelf
    /// name that is not a directory, or a shelf that cannot be opened or made
    /// private, is passed over: not followed, not changed, and so not looked
    /// in. The price is the one a legacy slot has: a duplicate or a tombstone
    /// in such a shelf is not seen, and the projection is stored again. That
    /// price is paid only for a shelf that is itself the trouble. An error
    /// that will pass ([`PrivateDir::passing`]) fails the seal instead: the
    /// shelf is healthy, and what it holds must be seen first.
    ///
    /// A file under the name that is not this projection, or is not a file
    /// the store reads (a link, a FIFO, a directory, a file that is not ours
    /// alone or is too large), has the name taken, and it is left exactly as
    /// it is. It does not end the search: an older shelf may hold the
    /// projection or its tombstone under the same name, and that is the
    /// answer. `Other` is the answer only when no shelf holds either. A read
    /// that fails for any other reason says nothing about what the file is,
    /// and it may be this very segment: that error fails the seal, which the
    /// caller retries, instead of sending the projection to its other name
    /// as a second copy.
    fn shelved(&self, shelves: &[String], id: &str, digest: &str) -> io::Result<Shelved> {
        let name = archive_name(id);
        let mut taken = false;
        for shelf in shelves {
            let dir = match self.fs.open_child(shelf, false) {
                Ok(Some(dir)) => dir,
                Ok(None) => continue,
                Err(error) if PrivateDir::passing(&error) => return Err(error),
                Err(error) => {
                    tracing::warn!(%shelf, %error, "seal passed over an archive shelf");
                    continue;
                }
            };
            let raw = match dir.read_with(&name, MAX_SEGMENT_BYTES, false) {
                Ok(Some(raw)) => raw,
                Ok(None) => continue,
                Err(error) if PrivateDir::not_a_segment(&error) => {
                    tracing::warn!(%shelf, file = %name, %error, "seal found its archive id taken");
                    taken = true;
                    continue;
                }
                Err(error) => return Err(error),
            };
            match self.record(&name, &raw) {
                Ok(record) if record.digest == digest => return Ok(Shelved::This(dir, record)),
                Err(_) if says_forgotten(&raw, digest) => return Err(forgotten()),
                // Another projection, or not a record: never replaced. An
                // older shelf is still looked in.
                _ => {
                    tracing::warn!(%shelf, file = %name, "seal found its archive id taken");
                    taken = true;
                }
            }
        }
        Ok(if taken { Shelved::Other } else { Shelved::Nothing })
    }

    /// What a seal returns for a projection that is already stored, as
    /// `record` in `dir`: its reference and the note stored with it, or the
    /// refusal if it was forgotten.
    fn already(dir: &PrivateDir, record: &Record) -> io::Result<(ArchiveRef, String)> {
        let Body::Live { note, .. } = &record.body else {
            return Err(forgotten());
        };
        // A previous call may have failed AFTER rename but BEFORE fsync.
        dir.sync()?;
        Ok((record.reference()?, note.clone()))
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

    /// [`search`](Self::search) that also says what it did not read:
    /// `None` when nothing it was asked for is missing, otherwise an
    /// [`Unscanned`]. Shelves are read newest to oldest, a shelf's files by
    /// modification time, the legacy slots last.
    ///
    /// One place that cannot be read hides only itself. A shelf name that is
    /// not a directory that can be opened and listed, a link, a FIFO or a
    /// file that is not ours alone where a segment belongs, a legacy slot
    /// that cannot be read, and a file that is read and is not a valid
    /// record are each passed over, and each is counted in
    /// [`Unscanned::unread`]: the rows are what the rest of the project
    /// holds, and the caller is told that they are not all of it. Only the
    /// project directory itself failing to list fails the search.
    ///
    /// Shelf files are read without the lock; it is taken, shared and only
    /// if there is a lock file, when the search gets as far as reading a
    /// legacy slot. Nothing is written either way.
    pub fn search_scan(
        &self,
        query: &str,
        limit: usize,
    ) -> io::Result<(Vec<ArchiveDescriptor>, Option<Unscanned>)> {
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
        let mut unread = Unread::default();
        // A full page and the end of the scan have nothing more to report
        // than what was passed over on the way; the budget adds its count.
        let done = |rows: Vec<ArchiveDescriptor>, stopped_after: Option<usize>, unread: &Unread| {
            let unread = unread.notice();
            let unscanned = (stopped_after.is_some() || unread.is_some()).then_some(Unscanned {
                stopped_after,
                unread,
            });
            Ok::<_, io::Error>((rows, unscanned))
        };
        // `None` is the project directory itself: the legacy slots, oldest of all.
        for shelf in shelves.iter().map(Some).chain([None]) {
            let opened;
            let (dir, names) = match shelf {
                Some(shelf) => {
                    let listed = self.fs.open_child(shelf, true).and_then(|dir| match dir {
                        Some(dir) => Ok(Some((dir.names()?, dir))),
                        None => Ok(None),
                    });
                    let (mut entries, dir) = match listed {
                        Ok(Some(listed)) => listed,
                        Ok(None) => continue,
                        // The rest of the project is still searched.
                        Err(error) => {
                            unread.pass(shelf, error);
                            continue;
                        }
                    };
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
                    return done(rows, None, &unread);
                }
                if files == SEARCH_SCAN_FILES || bytes >= SEARCH_SCAN_BYTES {
                    return done(rows, Some(files), &unread);
                }
                if shelf.is_none() && legacy_lock.is_none() {
                    legacy_lock = Some(self.shared()?);
                }
                let place = || match shelf {
                    Some(shelf) => format!("{shelf}/{name}"),
                    None => name.clone(),
                };
                let raw = match dir.read_with(&name, MAX_SEGMENT_BYTES, true) {
                    Ok(Some(raw)) => raw,
                    Ok(None) => continue,
                    // Too large to be a segment, a link, a FIFO, a file that
                    // is not ours alone, a read that failed: it hides only
                    // itself. It still counts against the scan budget.
                    Err(error) => {
                        unread.pass(&place(), error);
                        files += 1;
                        continue;
                    }
                };
                files += 1;
                bytes += raw.len();
                match self.record(&name, &raw) {
                    Ok(record) => rows.extend(describe(record, &query)?),
                    Err(error) => unread.pass(&place(), error),
                }
            }
        }
        done(rows, None, &unread)
    }

    /// Durable, idempotent deletion of the content of segment `id`, in every
    /// file that holds it. Each is replaced, under the same name, by a
    /// marker; the id and digest are never freed for reuse. Unknown ids are
    /// NotFound. This is logical deletion, not secure erasure of filesystem
    /// snapshots. The only operation that replaces a file.
    ///
    /// The store can hold one projection more than once: under each half of
    /// its digest, under one id on two shelves (stored again while a shelf
    /// could not be read), in a legacy slot, under an id an import kept. So
    /// forget goes by content. The record found under `id` names the
    /// projection, and every live record of it becomes a tombstone: under
    /// `id` and under both halves of the digest on every shelf that can be
    /// opened, and in every legacy slot that can be read.
    ///
    /// When it returns, a tombstone of the projection is on a shelf under
    /// one of the two ids its digest gives it, which is where a later seal
    /// of the same window looks. If the projection was only stored under
    /// another id (a legacy slot's, an import's), that tombstone is written
    /// new, on the newest shelf. It is written before any other file is
    /// replaced, and a copy under one of those two ids is replaced before
    /// the others: an error or a crash part-way leaves the content refused
    /// to a seal, and forgetting again finishes. An id that is already a
    /// tombstone is not an early return for the same reason.
    ///
    /// A shelf that cannot be opened, and a legacy slot or a file under one
    /// of the names that cannot be read or is not a valid record (and does
    /// not say "forgotten" for this projection), may hold a copy that was
    /// not reached: a damaged copy can still have the window's text in it.
    /// That is never silent: [`Forgotten::unread`] says so, and forgetting
    /// again when the place can be read reaches it. A file under `id` on a
    /// newer shelf that is not a valid record is such a place too: the valid
    /// record under `id` on an older shelf is still found and forgotten.
    pub fn forget(&self, id: &str) -> io::Result<Forgotten> {
        validate_id(id)?;
        let _lock = self.exclusive()?;
        let (shelves, slots) = self.layout()?;
        // The record asked for says which content to forget.
        let mut unread = Unread::default();
        let asked = match self.locate(&shelves, id, false, &mut unread)? {
            Some(found) => found.record,
            None => match self.legacy_segment(&slots, id, false, &mut unread) {
                Some((_, record)) => record,
                None => return Err(unread.not_found()),
            },
        };
        let digest = asked.digest.as_str();
        let derived = [&digest[..32], &digest[32..]];
        let mut names: Vec<_> = derived.iter().map(|id| archive_name(id)).collect();
        if !derived.contains(&id) {
            names.push(archive_name(id));
        }
        // Every copy that can be read, as (shelf or, for a slot, none; file
        // name; record), and the places that could not be: counted afresh,
        // each once.
        let mut unread = Unread::default();
        let mut copies = Vec::new();
        // Whether a seal already finds a tombstone under one of its two
        // names, and which of the two names something has.
        let (mut guarded, mut taken) = (false, [false; 2]);
        for shelf in &shelves {
            let dir = match self.fs.open_child(shelf, false) {
                Ok(Some(dir)) => dir,
                Ok(None) => continue,
                Err(error) => {
                    unread.pass(shelf, error);
                    continue;
                }
            };
            for (index, name) in names.iter().enumerate() {
                let raw = match dir.read_with(name, MAX_SEGMENT_BYTES, false) {
                    Ok(Some(raw)) => Some(raw),
                    Ok(None) => continue,
                    Err(error) => {
                        unread.pass(&format!("{shelf}/{name}"), error);
                        None
                    }
                };
                if let Some(held) = taken.get_mut(index) {
                    *held = true;
                }
                let Some(raw) = raw else {
                    continue;
                };
                match self.record(name, &raw) {
                    Ok(record) if record.digest == digest => {
                        copies.push((Some(shelf), name.clone(), record));
                    }
                    // Another projection: not a copy.
                    Ok(_) => {}
                    // What a seal of this projection takes for a tombstone.
                    Err(_) if says_forgotten(&raw, digest) => guarded |= index < 2,
                    // Not a record, under the window's own name: it may be
                    // a damaged copy, with the window's text still in it.
                    Err(error) => unread.pass(&format!("{shelf}/{name}"), error),
                }
            }
        }
        for slot in &slots {
            let raw = self.fs.read_with(slot, MAX_SEGMENT_BYTES, false);
            match raw.and_then(|raw| raw.map(|raw| self.record(slot, &raw)).transpose()) {
                Ok(Some(record)) if record.digest == digest => {
                    copies.push((None, slot.clone(), record));
                }
                Ok(_) => {}
                // A legacy id is not in the file name: this may be a copy.
                Err(error) => unread.pass(slot, error),
            }
        }
        // The copies a seal looks under come first.
        let seen_by_a_seal = |(shelf, name, _): &(Option<&String>, String, Record)| {
            shelf.is_some() && archive_id(name).is_some_and(|id| derived.contains(&id))
        };
        copies.sort_by_key(|copy| !seen_by_a_seal(copy));
        guarded |= copies.first().is_some_and(seen_by_a_seal);
        if !guarded {
            // Stored under no name a seal looks under: the tombstone it will
            // find, on the newest shelf, before anything is replaced. With
            // both names taken by other files there is none to write and
            // none is needed, because a seal has no name to store under.
            for (index, id) in derived.into_iter().enumerate() {
                if taken[index] {
                    continue;
                }
                let guard = Record {
                    version: 1,
                    scope: self.scope.clone(),
                    logical: asked.logical.clone(),
                    id: id.to_owned(),
                    digest: digest.to_owned(),
                    created_ms: None,
                    body: Body::Tombstone,
                };
                let bytes = bounded_json(&guard, MAX_SEGMENT_BYTES)?;
                let shelf = self.writable_shelf(&shelves)?;
                match shelf.publish(&archive_name(id), &bytes, true) {
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    result => {
                        result?;
                        break;
                    }
                }
            }
        }
        let mut ids = Vec::new();
        for (shelf, name, mut record) in copies {
            let opened;
            let dir = match shelf {
                Some(shelf) => {
                    opened = self.fs.open_child(shelf, false)?.ok_or_else(not_found)?;
                    &opened
                }
                None => &self.fs,
            };
            if matches!(record.body, Body::Tombstone) {
                // A previous call may have failed after rename, before fsync.
                dir.sync()?;
                continue;
            }
            // A tombstone keeps what names the record and nothing else: not
            // the content, and not when it was written.
            record.body = Body::Tombstone;
            record.created_ms = None;
            dir.publish(&name, &bounded_json(&record, MAX_SEGMENT_BYTES)?, false)?;
            ids.push(record.id);
        }
        ids.sort_by_key(|forgotten| forgotten != id);
        Ok(Forgotten {
            ids,
            unread: unread.notice(),
        })
    }

    /// Migration-only export of validated eligible evidence and permanent
    /// tombstones. Includes hidden notes: this is HOST data, never a model tool.
    /// No files, directories, permissions or archive contents are changed.
    /// Legacy slots first, then shelves oldest to newest. A migration carries
    /// at most [`MAX_SEGMENTS`] segments / [`MAX_STORE_BYTES`]; a larger store
    /// is refused whole rather than exported in part.
    ///
    /// One projection can be in several files (under each half of its
    /// digest, under one id on two shelves, in a slot and on a shelf). The
    /// export carries it once: the first record read, or the tombstone if
    /// any copy is one, because a forgotten projection must not come out of
    /// a migration live. The ids of the other copies are not carried. Two
    /// records with one id and different content are still an error: only
    /// one of them could go under that id.
    ///
    /// Unlike a search, an export stays strict about what it cannot read: a
    /// shelf name that is not a directory that can be opened, a link or a
    /// FIFO where a segment belongs and a file that is not a valid record
    /// each fail it whole. Its caller takes the rows for everything the
    /// project holds and moves the project by them; rows that silently
    /// lacked a shelf would leave that history behind with nothing to say
    /// so.
    pub fn export_records(&self) -> io::Result<Vec<Value>> {
        let gate = self.enter(BUSY_EXPORT)?;
        let lock = Locked {
            file: self.fs.lock_existing().map_err(|error| self.lock_error(error))?,
            _gate: gate,
        };
        let (mut records, mut used) = self.scan_legacy(true)?;
        for shelf in self.shelves()?.iter().rev() {
            let Some(dir) = self.fs.open_child(shelf, true)? else {
                continue;
            };
            let mut names: Vec<_> = dir.names()?.into_iter().map(|(name, ..)| name).collect();
            names.retain(|name| archive_id(name).is_some());
            names.sort_unstable();
            for name in names {
                let Some(raw) = dir.read_with(&name, MAX_SEGMENT_BYTES, true)? else {
                    continue;
                };
                used += raw.len();
                if used > MAX_STORE_BYTES {
                    return Err(over_migration_limit());
                }
                carry(&mut records, self.record(&name, &raw)?)?;
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

    /// The project directory's shelf names and legacy slot files, each newest
    /// first. One directory listing; nothing is opened, so a shelf name here
    /// may turn out to be a file or a link, or a directory that cannot be
    /// opened. A seal passes over such a name, a fetch reports it if it
    /// finds its segment nowhere else, a search passes over it and says so
    /// with its rows, and an export fails on it.
    fn layout(&self) -> io::Result<(Vec<String>, Vec<String>)> {
        let (mut shelves, mut slots) = (Vec::new(), Vec::new());
        for (name, ..) in self.fs.names()? {
            if let Some(number) = shelf_number(&name) {
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
    /// it once it holds [`SHELF_FILES`]. An empty shelf left by a crash is
    /// used. When the newest shelf name is not a directory that can be
    /// opened and made private, nothing is written to it and the segment
    /// goes to a new shelf, numbered after it. An error that will pass
    /// fails the seal instead of starting a shelf. A new shelf that was made
    /// and then could not be opened stays, empty, and is the newest shelf of
    /// the next seal: one failed attempt after another leaves that one.
    fn writable_shelf(&self, shelves: &[String]) -> io::Result<PrivateDir> {
        let highest = shelves.first().and_then(|name| shelf_number(name));
        let highest = highest.unwrap_or(0);
        // With no number left for a new shelf, the newest shelf that can be
        // written to takes more than its share rather than refuse.
        let last = highest >= LAST_SHELF;
        for (index, name) in shelves.iter().enumerate() {
            // `child` also makes the shelf's own directory entry durable.
            let shelf = self.fs.child(name, true);
            match shelf.and_then(|shelf| Ok((shelf.names()?, shelf))) {
                Ok((names, shelf)) => {
                    sweep(&shelf, &names);
                    let held = names.iter().filter(|(name, ..)| name.ends_with(".json"));
                    if last || (index == 0 && held.count() < SHELF_FILES) {
                        return Ok(shelf);
                    }
                    break;
                }
                // The shelf is not the trouble: no other one is started
                // because of it.
                Err(error) if PrivateDir::passing(&error) => return Err(error),
                Err(error) => {
                    tracing::warn!(shelf = %name, %error, "seal passed over an archive shelf");
                    if !last {
                        break;
                    }
                }
            }
        }
        if last {
            return Err(invalid("archive shelf numbers exhausted"));
        }
        self.fs.child(&shelf_name(highest + 1), true)
    }

    /// The shelf that holds segment `id`, by name and opened, and its
    /// record: one `openat` per shelf, newest first, and one file read. Never
    /// a scan. A shelf name that is not a directory that can be opened is
    /// passed over and noted in `unread`: the segment may be in it.
    ///
    /// A file under the name that is not a valid record, or is not a file
    /// the store reads ([`PrivateDir::not_a_segment`]), is passed over too,
    /// as a seal passes over it: the segment is the valid record under the
    /// name on an older shelf, if there is one. If there is none, the error
    /// is that of the newest such file: there, and not readable as its
    /// record. A read that fails for any other reason says nothing about
    /// what the file is and is the error at once.
    ///
    /// A passed-over file may be a damaged tombstone of the very content the
    /// older record holds. [`Located::forgotten`] says so, and a caller that
    /// would hand out that record's content must not.
    fn locate(
        &self,
        shelves: &[String],
        id: &str,
        readonly: bool,
        unread: &mut Unread,
    ) -> io::Result<Option<Located>> {
        let name = archive_name(id);
        // The error of the newest file passed over, and what was read of
        // each one that could be read.
        let (mut first, mut passed) = (None, Vec::<Vec<u8>>::new());
        for shelf in shelves {
            let dir = match self.fs.open_child(shelf, readonly) {
                Ok(Some(dir)) => dir,
                Ok(None) => continue,
                Err(error) => {
                    unread.pass(shelf, error);
                    continue;
                }
            };
            let error = match dir.read_with(&name, MAX_SEGMENT_BYTES, readonly) {
                Ok(None) => continue,
                Ok(Some(raw)) => match self.record(&name, &raw) {
                    Ok(record) => {
                        let forgotten =
                            passed.iter().any(|raw| says_forgotten(raw, &record.digest));
                        return Ok(Some(Located {
                            shelf: shelf.clone(),
                            record,
                            forgotten,
                        }));
                    }
                    Err(error) => {
                        passed.push(raw);
                        error
                    }
                },
                Err(error) if PrivateDir::not_a_segment(&error) => error,
                Err(error) => return Err(error),
            };
            tracing::warn!(%shelf, file = %name, %error, "archive lookup passed over a file under its id");
            first.get_or_insert(error);
        }
        first.map_or(Ok(None), Err)
    }

    /// The record of segment `id`, for a caller that holds no lock: one file
    /// when it is on a shelf, read without the gate or the lock. Only when no
    /// shelf has it and the directory has legacy slot files are those read,
    /// under the shared lock, one at a time until one of them is the
    /// segment. Nothing is written: no lock file, no mode.
    fn find(&self, id: &str) -> io::Result<Record> {
        self.check_abort()?;
        let (shelves, slots) = self.layout()?;
        let mut unread = Unread::default();
        if let Some(found) = self.locate(&shelves, id, true, &mut unread)? {
            // Forgotten, by a file that no longer validates: not handed out.
            return if found.forgotten { Err(not_found()) } else { Ok(found.record) };
        }
        if !slots.is_empty() {
            let _lock = self.shared()?;
            if let Some((_, record)) = self.legacy_segment(&slots, id, true, &mut unread) {
                return Ok(record);
            }
        }
        Err(unread.not_found())
    }

    /// The legacy slot that holds segment `id`, by file name, and its
    /// record: what a fetch or a forget asks of the slots. They are read one
    /// at a time and the first valid record with that id is the answer,
    /// whatever its neighbours are. A slot that cannot be read or is not a
    /// valid record is passed over, left as it is and noted in `unread`: a
    /// legacy id is not in the file name, so such a slot cannot be told
    /// apart from the one asked for.
    fn legacy_segment(
        &self,
        slots: &[String],
        id: &str,
        readonly: bool,
        unread: &mut Unread,
    ) -> Option<(String, Record)> {
        for name in slots {
            let raw = self.fs.read_with(name, MAX_SEGMENT_BYTES, readonly);
            match raw.and_then(|raw| raw.map(|raw| self.record(name, &raw)).transpose()) {
                Ok(Some(record)) if record.id == id => return Some((name.clone(), record)),
                Ok(_) => {}
                Err(error) => unread.pass(name, error),
            }
        }
        None
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
            // A tombstone is what names the record and nothing else.
            || (matches!(record.body, Body::Tombstone) && record.created_ms.is_some())
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
    /// which the export still uses and which fails as a whole, this passes
    /// over a slot that cannot be read (a link, a FIFO, a file
    /// that is not ours alone or is over the size of a segment, an I/O error)
    /// or that is not a valid record of this scope. Only an error that will
    /// pass ([`PrivateDir::passing`]) fails the seal. A slot passed over is
    /// left as it is: a link is not followed, and a file that is not ours
    /// alone is neither read nor made private. One slot is in memory at a
    /// time, so their total size is not bounded here.
    ///
    /// One thing is taken from a slot that is not a valid record: JSON that
    /// names this projection and says it was forgotten (`says_forgotten`)
    /// refuses the seal, as the tombstone it once was would.
    ///
    /// The price: if the slot passed over was the tombstone of this very
    /// projection and can no longer be read as that much, the projection is
    /// sealed again.
    fn legacy_projection(&self, digest: &str) -> io::Result<Option<Record>> {
        for slot in 0..MAX_SEGMENTS {
            let name = slot_name(slot);
            let record = match self.fs.read_with(&name, MAX_SEGMENT_BYTES, false) {
                Ok(Some(raw)) => match self.record(&name, &raw) {
                    Err(_) if says_forgotten(&raw, digest) => return Err(forgotten()),
                    record => record,
                },
                Ok(None) => continue,
                Err(error) => Err(error),
            };
            match record {
                Ok(record) if record.digest == digest => return Ok(Some(record)),
                Ok(_) => {}
                // Not the slot's fault, and it may hold this projection.
                Err(error) if PrivateDir::passing(&error) => return Err(error),
                // The file name and the error only: never content.
                Err(error) => {
                    tracing::warn!(slot = %name, %error, "seal passed over a legacy archive slot");
                }
            }
        }
        Ok(None)
    }

    /// Every legacy slot, read and validated together as before shelves
    /// existed: at most [`MAX_SEGMENTS`] files / [`MAX_STORE_BYTES`]. Only
    /// the migration export reads them this way; one bad slot fails it.
    /// What it returns is what a migration carries of them ([`carry`]).
    fn scan_legacy(&self, readonly: bool) -> io::Result<(Vec<Record>, usize)> {
        let mut records = Vec::new();
        let mut used = 0;
        for slot in 0..MAX_SEGMENTS {
            let name = slot_name(slot);
            let Some(raw) = self.fs.read_with(&name, MAX_STORE_BYTES - used, readonly)? else {
                continue;
            };
            used += raw.len();
            carry(&mut records, self.record(&name, &raw)?)?;
        }
        Ok((records, used))
    }
}

fn over_migration_limit() -> io::Error {
    invalid("archive store exceeds the migration limit (128 archives / 256 MiB)")
}

/// Add `record` to what a migration carries. Another copy of a projection
/// that is already there is not added: one is kept, the tombstone if either
/// is one. Two records with one id and different content are an error.
fn carry(records: &mut Vec<Record>, record: Record) -> io::Result<()> {
    if records.iter().any(|r| r.id == record.id && r.digest != record.digest) {
        return Err(corrupt());
    }
    if let Some(kept) = records.iter_mut().find(|r| r.digest == record.digest) {
        if matches!(kept.body, Body::Live { .. }) && matches!(record.body, Body::Tombstone) {
            *kept = record;
        }
        return Ok(());
    }
    if records.len() == MAX_SEGMENTS {
        return Err(over_migration_limit());
    }
    records.push(record);
    Ok(())
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

    /// The stored form of a message. One that a reduction changed says so
    /// under [`REDUCED_KEY`], which a source message cannot supply: only its
    /// role and content are ever taken. The markers inside the content can
    /// be imitated by content; this key cannot. It says how many things were
    /// left out and how many tool inputs are stored as text, of which only
    /// the first is a loss.
    fn stored(&self, role: &str, content: Value) -> Value {
        let mut message = json!({"role": role, "content": content});
        let (omitted, as_text) = (self.omitted.replace(0), self.as_text.replace(0));
        let changed = match (omitted, as_text) {
            (0, 0) => return message,
            (omitted, 0) => format!("{omitted} omission(s)"),
            (0, as_text) => format!("{as_text} tool input(s) as JSON text"),
            (omitted, as_text) => {
                format!("{omitted} omission(s) and {as_text} tool input(s) as JSON text")
            }
        };
        message[REDUCED_KEY] = json!(format!(
            "this message alone is over the per-archive limit; it is stored with {changed}, \
             each marked in place"
        ));
        message
    }

    fn project(&self, message: &Value) -> io::Result<Option<(Value, Vec<usize>)>> {
        let Some((class, role, content)) = projection_content(message) else {
            return Ok(None);
        };
        self.omitted.set(0);
        self.as_text.set(0);
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

    /// A tool input as stored: exact, or, where a reduction says so, as its
    /// JSON text or left out with a marker.
    fn tool_input(&self, input: Option<&Value>, class: DisclosureClass) -> io::Result<Value> {
        match (self.reduce, input) {
            (Reduce::Inputs | Reduce::Text, Some(input)) => Ok(self.omitted_input(input)),
            (Reduce::InputsAsText, Some(input)) => self.input_as_text(input, class),
            (Reduce::LargeInputs, Some(value)) => match self.exact_input(input, class) {
                Err(error) if over_budget(&error) => self.input_as_text(value, class),
                exact => exact,
            },
            _ => self.exact_input(input, class),
        }
    }

    /// What stands in place of a tool input that is left out: a marker with
    /// its size as JSON.
    fn omitted_input(&self, input: &Value) -> Value {
        let bytes = json_len(input);
        let marker = self.omission(format_args!("tool input of {bytes} bytes"));
        json!({"_archive_omitted": marker})
    }

    /// A tool input as one string under [`INPUT_JSON_KEY`]: its JSON text,
    /// screened as a whole like the exact form is. The depth and node
    /// budgets do not apply to a string, so an input that is only too deep
    /// or has too many nodes is all there. It is withheld on the terms of
    /// the exact form (something in it is marked as not to be stored, or the
    /// screen withholds it), and left out only when the text is over the
    /// byte budget. The screened text is stored as it is: the redactor may
    /// have changed it, and nothing here needs it to parse.
    fn input_as_text(&self, input: &Value, class: DisclosureClass) -> io::Result<Value> {
        if !input_eligible(input) {
            return Ok(json!({"_archive_withheld":true}));
        }
        if input_text_cost(input).is_none() {
            return Ok(self.omitted_input(input));
        }
        let encoded = bounded_json(input, MAX_INPUT_BYTES)?;
        let text = std::str::from_utf8(&encoded).map_err(|_| corrupt())?;
        match self.screen(text, class) {
            Ok(text) if text == WITHHELD => Ok(json!({"_archive_withheld":true})),
            Ok(text) => {
                self.as_text.set(self.as_text.get() + 1);
                Ok(json!({INPUT_JSON_KEY: text}))
            }
            Err(error) if over_budget(&error) => Ok(self.omitted_input(input)),
            Err(error) => Err(error),
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

/// The conversation name of a store that is opened only to read a project:
/// a read finds the segments of every conversation in it.
const READER: &str = "project-reader";

/// Read-only migration preview for an existing project archive. A missing
/// archive is empty; corrupt, unsafe or locked paths fail closed. In particular,
/// this never invokes `ArchiveStore::new`, mkdir, chmod, or lock-file creation.
pub fn export_in(base: &Path, scope_key: &str) -> io::Result<Vec<Value>> {
    match ArchiveStore::open_existing(base, scope_key, READER)? {
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
    let Some(store) = ArchiveStore::open_existing(base, scope_key, READER)? else {
        return Ok(None);
    };
    let directory = base.join("context-archives").join(&store.scope);
    let mut unread = Unread::default();
    let (shelves, slots) = store.layout()?;
    if let Some(found) = store.locate(&shelves, id, true, &mut unread)? {
        if found.forgotten {
            // A newer file under the id says so and is not a valid record.
            return Err(corrupt());
        }
        return Ok(Some(directory.join(found.shelf).join(archive_name(id))));
    }
    match store.legacy_segment(&slots, id, true, &mut unread) {
        Some((slot, _)) => Ok(Some(directory.join(slot))),
        None if unread.places == 0 => Ok(None),
        None => Err(unread.not_found()),
    }
}

/// Project-wide archive lookup; base and scope are trusted host values, never
/// model-supplied paths. The same budgets and disclosure policy apply. A
/// read: nothing is created, and a project without archives has no rows.
pub fn project_search(
    base: &Path,
    scope_key: &str,
    query: &str,
    limit: usize,
) -> io::Result<Vec<ArchiveDescriptor>> {
    match ArchiveStore::open_existing(base, scope_key, READER)? {
        Some(store) => store.search(query, limit),
        None => Ok(Vec::new()),
    }
}

/// Fetch an opaque reference from any logical conversation in this project.
/// A read: nothing is created, and in a project without archives nothing is
/// found.
pub fn project_fetch(
    base: &Path,
    scope_key: &str,
    id: &str,
    start: usize,
    limit: usize,
) -> io::Result<Vec<SharedMessage>> {
    validate_id(id)?;
    match ArchiveStore::open_existing(base, scope_key, READER)? {
        Some(store) => store.fetch(id, start, limit),
        None => Err(not_found()),
    }
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
                            let in_place = input.and_then(|input| self.reduce.in_place_of(input));
                            match (input, in_place) {
                                // Stored exact: walked.
                                (Some(input), None) => {
                                    if let Some(bytes) = self.input(input, 3)? {
                                        self.charge("input".len().saturating_add(bytes))?;
                                    }
                                }
                                // Left out: its marker is not charged.
                                (_, None | Some(0)) => {}
                                // One string in its place.
                                (_, Some(bytes)) => {
                                    self.node(4)?;
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
/// Where the part `messages[start..end]` is cut so that the next part does
/// not begin with tool results: `end`, moved back to before the call those
/// results follow, so that a call and its results stay in one part where
/// they can. It cannot when that would leave nothing in this part, and it
/// need not when `end` ends the window. A courtesy to readers, not a rule:
/// the store pairs nothing.
fn before_results(messages: &[SharedMessage], start: usize, end: usize) -> usize {
    let is_result = |index: usize| {
        let message = &messages[index];
        let blocks = message["content"].as_array();
        message["role"] == "user"
            && blocks.is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"))
    };
    if end >= messages.len() {
        return end;
    }
    let mut cut = end;
    while cut > start + 1 && is_result(cut) {
        cut -= 1;
    }
    // Nothing but results back to the start of the part: cut where it was.
    if is_result(cut) {
        end
    } else {
        cut
    }
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
/// Shelves are `s` and six digits, numbered from 1 in creation order.
fn shelf_name(number: u64) -> String {
    format!("s{number:06}")
}
/// Only the one spelling `shelf_name` produces is a shelf, and only up to
/// [`LAST_SHELF`]: a name with a number no store has counted to is not one.
fn shelf_number(name: &str) -> Option<u64> {
    let number = name.strip_prefix('s')?.parse().ok()?;
    (number <= LAST_SHELF && shelf_name(number) == name).then_some(number)
}
/// Whether `raw`, the contents of a file that is not a valid record, still
/// reads as JSON that names the projection `digest` and says it was
/// forgotten. Forgetting is a promise to the user, and a seal keeps it on
/// this much evidence. It is the one deliberate refusal a damaged file can
/// cause, and only for its own projection. Nothing else in the file is read.
fn says_forgotten(raw: &[u8], digest: &str) -> bool {
    #[derive(Deserialize)]
    struct Marked {
        digest: String,
        body: State,
    }
    #[derive(Deserialize)]
    struct State {
        state: String,
    }
    serde_json::from_slice::<Marked>(raw)
        .is_ok_and(|marked| marked.digest == digest && marked.body.state == "tombstone")
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
    io::Error::new(io::ErrorKind::NotFound, NOT_FOUND)
}
fn forgotten() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "archive projection was forgotten",
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
        #[cfg(test)]
        if let Some(error) = fault("open", name) {
            return Err(error);
        }
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
    /// Whether a failed read of the file under a segment's name says what
    /// that file is, rather than that reading failed: the store's own
    /// refusal (not a plain file of ours alone, or too large for a segment),
    /// which carries no system error, and the system's refusal to open it
    /// for what it is (a link, a file closed to us or that cannot be made
    /// private, a socket). Such a file is odd and stays odd. Everything
    /// else (an I/O error, no descriptor or memory left, an interrupted
    /// call) can happen to a healthy file.
    fn not_a_segment(error: &io::Error) -> bool {
        let of_the_file = [libc::ELOOP, libc::EACCES, libc::EPERM, libc::ENXIO];
        error.raw_os_error().is_none_or(|code| of_the_file.contains(&code))
    }
    /// Whether an error is about the state of this process or the system at
    /// this moment (no descriptor or memory left, an interrupted call) and
    /// not about the shelf, slot or file it was met at. Such an error passes
    /// and a seal fails on it: the caller keeps its history and retries.
    /// Every other error from a shelf or a slot (not a directory, a link,
    /// closed to us, an I/O error reading it) is taken to stay, and a seal
    /// passes over that shelf or slot rather than be refused for good.
    fn passing(error: &io::Error) -> bool {
        let passing = [libc::EMFILE, libc::ENFILE, libc::ENOMEM, libc::EINTR, libc::EAGAIN];
        error.raw_os_error().is_some_and(|code| passing.contains(&code))
    }
    /// Directory identity for the in-process gate.
    fn identity(&self) -> io::Result<DirIdentity> {
        use std::os::unix::fs::MetadataExt;
        let meta = self.0.metadata()?;
        Ok((meta.dev(), meta.ino()))
    }
    /// The lock file, opened and looked at, or `None` if there is none.
    /// Only ever locked: never written, truncated or read, so neither a
    /// second name nor its mode matters. With `create` it is made if it is
    /// missing and opened read-write, or for reading if its mode refuses
    /// that; a group or other bit in its mode is cleared. Without `create`
    /// nothing is changed. A link is not followed.
    fn lock_file(&self, create: bool) -> io::Result<Option<std::fs::File>> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let refused = |what| io::Error::new(io::ErrorKind::PermissionDenied, LockRefused(what));
        let name = c_name("lock")?;
        let mut opened = match create {
            true => self.open(&name, libc::O_RDWR | libc::O_CREAT),
            false => self.open(&name, libc::O_RDONLY),
        };
        if create && opened.as_ref().is_err_and(|error| error.raw_os_error() == Some(libc::EACCES)) {
            // Read-only, or in a directory nothing can be made in.
            match self.open(&name, libc::O_RDONLY) {
                // There is none and none can be made: the error as it is.
                Err(error) if error.kind() == io::ErrorKind::NotFound => return opened.map(Some),
                again => opened = again,
            }
        }
        let file = match opened {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(match error.raw_os_error() {
                    // `O_NOFOLLOW`; FreeBSD says EMLINK.
                    Some(libc::ELOOP | libc::EMLINK) => refused("is a link"),
                    Some(libc::EISDIR | libc::ENXIO) => refused("is not a regular file"),
                    Some(libc::EACCES) => refused("cannot be opened by its owner with its mode"),
                    _ => error,
                });
            }
        };
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(refused("is not a regular file"));
        }
        if meta.uid() != unsafe { libc::geteuid() } {
            return Err(refused("is owned by another user"));
        }
        let mode = meta.permissions().mode() & 0o7777;
        if create && mode & 0o077 != 0 {
            file.set_permissions(std::fs::Permissions::from_mode(mode & 0o700))?;
        }
        Ok(Some(file))
    }
    fn lock_existing(&self) -> io::Result<Option<FlockGuard>> {
        let Some(file) = self.lock_file(false)? else {
            return Ok(None);
        };
        if !fs4::fs_std::FileExt::try_lock_shared(&file)? {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, BUSY_EXPORT));
        }
        Ok(Some(FlockGuard(file)))
    }
    /// Never waits: a lock held by another process fails closed at once;
    /// `ArchiveStore::exclusive` polls it.
    fn lock(&self) -> io::Result<FlockGuard> {
        let file = self.lock_file(true)?.ok_or_else(not_found)?;
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
        self.publish_dated(name, bytes, create_new, None)
    }
    /// [`publish`](Self::publish) that can also give the file a time of last
    /// access and modification, for a record that was first written
    /// somewhere else. The time is set on the temporary file, through its
    /// own descriptor, before it is synced and renamed: the file appears
    /// under its name with that time, and the time is on disk with the
    /// content. No directory's time is ever set.
    fn publish_dated(
        &self,
        name: &str,
        bytes: &[u8],
        create_new: bool,
        modified: Option<std::time::SystemTime>,
    ) -> io::Result<()> {
        let target = c_name(name)?;
        // Created new under a name no other write has: nothing else writes
        // to this file, removes it while it is fresh or renames it.
        let tmp = c_name(&temp_name())?;
        let mut file = self.open(&tmp, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
        let result = (|| {
            file.write_all(bytes)?;
            if let Some(time) = modified {
                let times = std::fs::FileTimes::new().set_accessed(time).set_modified(time);
                file.set_times(times)?;
            }
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
        #[cfg(test)]
        if let Some(error) = fault("rename", to) {
            return Err(error);
        }
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
// Failures a test asks for: system calls that fail only when a disk or the
// system is in trouble, which no temporary directory brings about. Each is
// (the call, the start of the names it fails for, the error number), for
// calls made on the thread that set it and until that thread clears it.
#[cfg(all(test, unix))]
thread_local! {
    static FAULTS: std::cell::RefCell<Vec<(&'static str, String, i32)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}
#[cfg(all(test, unix))]
fn fault(call: &str, name: &std::ffi::CStr) -> Option<io::Error> {
    FAULTS.with_borrow(|faults| {
        let hit = |(failing, prefix, _): &&(&str, String, i32)| {
            *failing == call && name.to_bytes().starts_with(prefix.as_bytes())
        };
        let code = faults.iter().find(hit).map(|(.., code)| *code);
        code.map(io::Error::from_raw_os_error)
    })
}
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
    fn not_a_segment(_: &io::Error) -> bool {
        false
    }
    fn passing(_: &io::Error) -> bool {
        false
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
    fn publish_dated(
        &self,
        _: &str,
        _: &[u8],
        _: bool,
        _: Option<std::time::SystemTime>,
    ) -> io::Result<()> {
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
        let store = setup_in(tmp.path(), "synthetic-project");
        (tmp, store)
    }
    fn setup_in(base: &Path, project: &str) -> ArchiveStore {
        ArchiveStore::new(base, project, "synthetic-logical").unwrap()
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
            // As every record was then: without the field.
            created_ms: None,
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

    /// The store can hold one window in more than one file. The export
    /// carries it once, and as forgotten if any copy is.
    #[test]
    fn export_carries_a_window_stored_twice_once_and_forgotten_if_any_copy_is() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let (older, newer) = (dir.join("s000001"), dir.join("s000002"));
        let twice = store.seal(&text("one id on two shelves"), "").unwrap();
        let halves = store.seal(&text("under both halves"), "the note").unwrap();
        let alone = store.seal(&text("stored once"), "").unwrap();
        std::fs::create_dir(&newer).unwrap();
        let name = archive_name(&twice.id);
        std::fs::copy(older.join(&name), newer.join(&name)).unwrap();
        let digest = digest_of("under both halves");
        assert_eq!(halves.id, digest[..32]);
        let second = record_bytes(&store, &digest[32..], &text("under both halves"), |_| {});
        std::fs::write(newer.join(archive_name(&digest[32..])), second).unwrap();
        let exported = || {
            let rows = store.export_records().unwrap();
            assert_eq!(rows, export_in(tmp.path(), "synthetic-project").unwrap());
            let mut rows: Vec<_> = rows
                .iter()
                .map(|row| (row["id"].as_str().unwrap().to_owned(), row["tombstone"] == true))
                .collect();
            rows.sort();
            rows
        };
        let sorted = |rows: [(&str, bool); 3]| {
            let mut rows = rows.map(|(id, gone)| (id.to_owned(), gone)).to_vec();
            rows.sort();
            rows
        };
        // The copy read first is the one carried: the oldest shelf's.
        let (twice_id, alone_id) = (twice.id.as_str(), alone.id.as_str());
        let live = sorted([(twice_id, false), (halves.id.as_str(), false), (alone_id, false)]);
        assert_eq!(exported(), live);
        let rows = store.export_records().unwrap();
        let row = rows.iter().find(|row| row["id"] == halves.id.as_str()).unwrap();
        assert_eq!(row["note"], "the note");
        // A tombstone in any copy, whichever file it is, makes it forgotten.
        let tombstone = |file: &Path| {
            let mut record = json_of(file);
            record["body"] = json!({"state":"tombstone"});
            record.as_object_mut().unwrap().remove("created_ms");
            std::fs::write(file, serde_json::to_vec(&record).unwrap()).unwrap();
        };
        tombstone(&newer.join(&name));
        tombstone(&newer.join(archive_name(&digest[32..])));
        let forgotten = sorted([(twice_id, true), (&digest[32..], true), (alone_id, false)]);
        assert_eq!(exported(), forgotten);
        // Two records with one id and different content are not one window
        // stored twice: only one of them could go under that id.
        let name = archive_name(&alone.id);
        let other = record_bytes(&store, &alone.id, &text("other content"), |_| {});
        std::fs::write(newer.join(&name), other).unwrap();
        let error = store.export_records().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
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
        let torn = identity(&file);
        for result in [
            store.fetch(&reference.id, 0, 1).map(drop),
            store.fetch_note(&reference.id).map(drop),
            store.forget(&reference.id).map(drop),
        ] {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
        // The torn file is neither trusted as the same projection nor
        // replaced, and it does not refuse the seal: the window is stored
        // under its other id.
        let again = store.seal(&text("complete"), "").unwrap();
        assert_ne!(again.id, reference.id);
        assert_eq!(store.fetch(&again.id, 0, 1).unwrap(), text("complete"));
        assert_eq!(std::fs::read(&file).unwrap(), b"{\"version\":1,");
        assert_eq!(identity(&file), torn);
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
            // A search passes over the file and says so.
            let (_, unscanned) = store.search_scan("", 2).unwrap();
            let unread = unscanned.unwrap().unread.unwrap();
            let place = format!("first s000001/{}: ", archive_name(&reference.id));
            assert!(unread.starts_with("1 place(s) could not be read, ") && unread.contains(&place));
            assert!(store.fetch(&reference.id, 0, 1).is_err());
            assert!(store.fetch_note(&reference.id).is_err());
            assert!(store.forget(&reference.id).is_err());
            // The same projection would be published under this very name.
            // What has the name is neither followed nor replaced, and the
            // seal is not refused: it uses the projection's other id.
            let other = store.seal(&text("private"), "").unwrap();
            assert_ne!(other.id, reference.id);
            assert_eq!(store.fetch(&other.id, 0, 1).unwrap(), text("private"));
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

    /// The lock file is only ever locked, so a second name (a hard-link
    /// backup) or a read-only mode (a restore) does not refuse a save. What
    /// is not a regular file of ours is refused, and the error names it.
    #[test]
    fn a_lock_file_with_two_names_or_read_only_is_used_and_a_link_is_refused_by_name() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let lock = dir.join("lock");
        let mut other = setup_in(tmp.path(), "synthetic-project");
        other.process_wait = Duration::ZERO;
        store.seal(&text("first"), "").unwrap();
        let works = |words: &str| {
            let sealed = store.seal(&text(words), "").unwrap();
            assert_eq!(store.forget(&sealed.id).unwrap(), forgotten(&[&sealed.id]));
            assert!(export_in(tmp.path(), "synthetic-project").is_ok(), "{words}");
            // And it still excludes: a save by another store is busy while
            // the lock is held, and goes through once it is not.
            let held = store.fs.lock().unwrap();
            assert!(store.fs.lock().is_err_and(|error| error.kind() == io::ErrorKind::WouldBlock));
            let busy = other.seal(&text(words), "").unwrap_err();
            assert_eq!(busy.kind(), io::ErrorKind::WouldBlock, "{words}: {busy}");
            assert_eq!(busy.to_string(), BUSY);
            drop(held);
            assert!(other.seal(&text(&format!("{words}, after")), "").is_ok());
        };
        // A second name, as `cp -al` of the base gives every file.
        let second = tmp.path().join("a-backup-of-the-lock");
        std::fs::hard_link(&lock, &second).unwrap();
        works("with two names");
        // Read-only, with the second name still there and then without.
        for links in [2, 1] {
            std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o400)).unwrap();
            works(&format!("read-only, {links} name(s)"));
            assert_eq!(mode(&lock), 0o400, "left as it is");
            if links == 2 {
                assert_eq!(identity(&lock), identity(&second), "not replaced");
                std::fs::remove_file(&second).unwrap();
            }
        }
        assert_eq!(std::fs::metadata(&lock).unwrap().len(), 0, "never written");
        // Open to others: closed to them again, as it always was.
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
        works("loosened");
        assert_eq!(mode(&lock), 0o600);

        // Not a regular file of ours: refused, named, and left as it is.
        let victim = tmp.path().join("synthetic-victim");
        std::fs::write(&victim, "do not change").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
        let sealed = store.seal(&text("to forget later"), "").unwrap();
        let refused = |what: &str| {
            let before = snapshot(&dir.join("s000001"));
            let errors = [
                store.seal(&text("refused"), "").unwrap_err(),
                store.forget(&sealed.id).unwrap_err(),
                export_in(tmp.path(), "synthetic-project").unwrap_err(),
            ];
            for error in errors {
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{what}: {error}");
                let message = error.to_string();
                let named = format!("context archive lock file {} {what}", lock.display());
                assert!(message.starts_with(&named), "{message}");
                assert!(message.contains(&format!("rm '{}'", lock.display())), "{message}");
            }
            assert_eq!(snapshot(&dir.join("s000001")), before, "{what}");
            // A read by id needs no lock.
            assert_eq!(store.fetch(&sealed.id, 0, 1).unwrap(), text("to forget later"));
        };
        std::fs::remove_file(&lock).unwrap();
        symlink(&victim, &lock).unwrap();
        refused("is a link");
        assert!(std::fs::symlink_metadata(&lock).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "do not change");
        assert_eq!(mode(&victim), 0o644);
        std::fs::remove_file(&lock).unwrap();
        std::fs::create_dir(&lock).unwrap();
        refused("is not a regular file");
        std::fs::remove_dir(&lock).unwrap();
        let fifo = std::ffi::CString::new(lock.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        refused("is not a regular file");
        std::fs::remove_file(&lock).unwrap();
        if unsafe { libc::geteuid() } != 0 {
            std::fs::write(&lock, b"").unwrap();
            std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o000)).unwrap();
            refused("cannot be opened by its owner with its mode");
            std::fs::remove_file(&lock).unwrap();
        }
        // The fix the message gives is the fix.
        assert_eq!(store.forget(&sealed.id).unwrap(), forgotten(&[&sealed.id]));
        assert_eq!(mode(&lock), 0o600);
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

    /// Mode, change time and modification time.
    type Stamp = (u32, (i64, i64), (i64, i64));
    /// The stamp of everything under `root`, itself included: equal only if
    /// nothing was created, removed, written, renamed or had its mode set
    /// (even to what it already was).
    fn stamps(root: &Path) -> std::collections::BTreeMap<PathBuf, Stamp> {
        use std::os::unix::fs::MetadataExt;
        let mut found = std::collections::BTreeMap::new();
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

    /// Every read there is, each through a store opened for it there and
    /// then: what the first read in a process does.
    fn read_everything(
        base: &Path,
        live: &[(&ArchiveRef, &str, &str)],
        gone: &[String],
        rows: usize,
    ) {
        let open = || ArchiveStore::open_existing(base, "synthetic-project", "any-reader");
        for (reference, words, note) in live {
            let id = &reference.id;
            assert_eq!(open().unwrap().unwrap().fetch(id, 0, 1).unwrap(), text(words));
            assert_eq!(&open().unwrap().unwrap().fetch_note(id).unwrap(), note);
            let found = project_fetch(base, "synthetic-project", id, 0, 1);
            assert_eq!(found.unwrap(), text(words));
            let located = archive_file(base, "synthetic-project", id).unwrap().unwrap();
            assert!(std::fs::read_to_string(located).unwrap().contains(words));
            let found = open().unwrap().unwrap().search(words, 8).unwrap();
            assert_eq!(found.len(), 1, "{words}");
            assert_eq!(found[0].id, *id);
        }
        for id in gone {
            for result in [
                open().unwrap().unwrap().fetch(id, 0, 1).map(drop),
                open().unwrap().unwrap().fetch_note(id).map(drop),
                project_fetch(base, "synthetic-project", id, 0, 1).map(drop),
            ] {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::NotFound);
                assert_eq!(error.to_string(), NOT_FOUND);
            }
        }
        assert_eq!(open().unwrap().unwrap().search("", 8).unwrap().len(), rows);
        assert_eq!(project_search(base, "synthetic-project", "", 8).unwrap().len(), rows);
        assert_eq!(open().unwrap().unwrap().search_scan("", 8).unwrap().1, None);
    }

    /// A read writes nothing, the first one included: no directory, no lock
    /// file, no mode, no change time, no sync. So reads also work where
    /// nothing can be written. `ArchiveStore::new` did all of that on every
    /// open, and the legacy slots were read with the lock file created and
    /// their modes set.
    #[test]
    fn reads_write_nothing_and_work_where_nothing_can_be_written() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let shelved = store.seal(&text("read only"), "note").unwrap();
        let forgotten = store.seal(&text("forgotten"), "").unwrap();
        store.forget(&forgotten.id).unwrap();
        let legacy = seal_legacy_slot(&store, 0, &text("sealed long ago"), "old note");
        let old_forgotten = seal_legacy_slot(&store, 1, &text("forgotten long ago"), "");
        store.forget(&old_forgotten.id).unwrap();
        drop(store);
        let live = [(&shelved, "read only", "note"), (&legacy, "sealed long ago", "old note")];
        let gone = [forgotten.id.clone(), old_forgotten.id.clone(), "a".repeat(32)];
        let context = tmp.path().join("context-archives");
        let dirs = vec![tmp.path().to_path_buf(), context, dir.clone(), dir.join("s000001")];
        let _reopen = Reopen(dirs.clone());
        // Once with the lock file a seal left, and once without one: taking
        // the lock as a seal does would then mean creating it.
        for lock in [true, false] {
            if !lock {
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
                std::fs::remove_file(dir.join("lock")).unwrap();
            }
            // Looser than the store makes them: a read does not repair.
            for path in &dirs {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            for path in shelf_files(&dir).into_iter().chain([dir.join("000.json")]) {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
            // Whatever is changed from here on gets a later change time.
            std::thread::sleep(Duration::from_millis(50));
            let before = stamps(tmp.path());
            assert_eq!(before.len(), if lock { 10 } else { 9 }, "4 directories, 5 files, lock");
            read_everything(tmp.path(), &live, &gone, 2);
            // The export wants the lock file a seal leaves, and does not
            // make one.
            let exported = export_in(tmp.path(), "synthetic-project");
            assert_eq!(exported.map(|rows| rows.len()).ok(), lock.then_some(4));
            assert_eq!(stamps(tmp.path()), before, "lock file: {lock}");

            // And where nothing could be written at all.
            for path in &dirs {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o500)).unwrap();
            }
            for path in shelf_files(&dir).into_iter().chain([dir.join("000.json")]) {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o400)).unwrap();
            }
            if lock {
                let read_only = std::fs::Permissions::from_mode(0o400);
                std::fs::set_permissions(dir.join("lock"), read_only).unwrap();
            }
            std::thread::sleep(Duration::from_millis(50));
            let before = stamps(tmp.path());
            read_everything(tmp.path(), &live, &gone, 2);
            assert_eq!(stamps(tmp.path()), before, "read-only, lock file: {lock}");
        }
        // What a read must not go through: opening the store to write makes
        // its three directories private again, whoever asked.
        ArchiveStore::new(tmp.path(), "synthetic-project", "any").unwrap();
        assert_eq!((mode(tmp.path()), mode(&dir), mode(&dir.join("s000001"))), (0o700, 0o700, 0o500));
    }

    /// A read of a project that has archived nothing, or of a base that
    /// does not exist: nothing is found, and nothing is created to say so.
    #[test]
    fn a_read_of_a_project_without_archives_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "a".repeat(32);
        let absent = |base: &Path, project: &str| {
            assert!(ArchiveStore::open_existing(base, project, "reader").unwrap().is_none());
            assert!(project_search(base, project, "", 8).unwrap().is_empty());
            let error = project_fetch(base, project, &id, 0, 1).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
            assert_eq!(error.to_string(), NOT_FOUND);
            assert_eq!(archive_file(base, project, &id).unwrap(), None);
            assert!(export_in(base, project).unwrap().is_empty());
        };
        absent(&tmp.path().join("missing"), "synthetic-project");
        absent(tmp.path(), "synthetic-project");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
        // Another project of the same base has archives; this one still has
        // no directory, before and after it is read.
        let other = setup_in(tmp.path(), "another-project");
        other.seal(&text("elsewhere"), "").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let before = stamps(tmp.path());
        absent(tmp.path(), "synthetic-project");
        assert_eq!(stamps(tmp.path()), before);
        assert_eq!(std::fs::read_dir(tmp.path().join("context-archives")).unwrap().count(), 1);
        // The same checks of its arguments as a store that exists makes.
        for (project, id) in [("", id.as_str()), ("synthetic-project", "../x")] {
            let error = project_fetch(tmp.path(), project, id, 0, 1).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        // A link on the way is refused, not followed and not "nothing there".
        let alias = tmp.path().join("alias");
        symlink(tmp.path(), &alias).unwrap();
        assert!(ArchiveStore::open_existing(&alias, "another-project", "reader").is_err());
        assert!(project_search(&alias, "another-project", "", 8).is_err());
    }

    #[test]
    fn a_shelf_name_that_is_a_link_or_a_file_is_not_followed_and_does_not_stop_a_seal() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let reference = store.seal(&text("shelved"), "").unwrap();
        let shelf = dir.join("s000001");
        let outside = tmp.path().join("outside-shelf");
        std::fs::rename(&shelf, &outside).unwrap();
        let before = snapshot(&outside);
        let unread = |result: io::Result<()>| {
            let error = result.unwrap_err();
            assert_ne!(error.kind(), io::ErrorKind::NotFound, "{error}");
            assert!(error.to_string().contains("s000001"), "{error}");
            assert!(error.to_string().contains("could not be read"), "{error}");
        };
        // A segment that is found nowhere is not called "not found" while
        // there is a shelf name that could not be looked into. A search
        // answers from the other shelves and names this one; an export,
        // which has to carry every shelf, fails on it.
        let refused = |id: &str| {
            unread(store.fetch(id, 0, 1).map(drop));
            unread(store.fetch_note(id).map(drop));
            unread(store.forget(id).map(drop));
            unread(archive_file(tmp.path(), "synthetic-project", id).map(drop));
            let (_, unscanned) = store.search_scan("", MAX_SEARCH_RESULTS).unwrap();
            let passed = unscanned.unwrap().unread.unwrap();
            assert!(passed.starts_with("1 place(s) could not be read, first s000001: "), "{passed}");
            assert!(store.export_records().is_err());
            assert!(export_in(tmp.path(), "synthetic-project").is_err());
        };
        symlink(&outside, &shelf).unwrap();
        refused(&reference.id);
        std::fs::remove_file(&shelf).unwrap();
        std::fs::write(&shelf, b"not a directory").unwrap();
        refused(&reference.id);
        // Nothing was read into, written through or created beside the link.
        assert_eq!(snapshot(&outside), before);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2, "lock and s000001");

        // A seal passes over the name and takes the number after it.
        let new = store.seal(&text("something new"), "").unwrap();
        assert!(dir.join("s000002").join(archive_name(&new.id)).is_file());
        assert_eq!(std::fs::read(&shelf).unwrap(), b"not a directory");
        std::fs::remove_file(&shelf).unwrap();
        symlink(&outside, &shelf).unwrap();
        // What is behind the link is not seen, so it is stored again.
        assert_eq!(store.seal(&text("shelved"), "").unwrap(), reference);
        assert!(dir.join("s000002").join(archive_name(&reference.id)).is_file());
        assert_eq!(snapshot(&outside), before);
        // A segment on a shelf that can be read is found whatever its
        // neighbour is; one that is not found is still not "not found".
        assert_eq!(store.fetch(&new.id, 0, 1).unwrap(), text("something new"));
        assert_eq!(store.fetch(&reference.id, 0, 1).unwrap(), text("shelved"));
        let located = archive_file(tmp.path(), "synthetic-project", &new.id).unwrap();
        assert_eq!(located, Some(dir.join("s000002").join(archive_name(&new.id))));
        refused(&"a".repeat(32));
        // With such a name as the newest, the next number is used.
        std::fs::write(dir.join("s000003"), b"not a directory").unwrap();
        let next = store.seal(&text("after a file"), "").unwrap();
        assert!(dir.join("s000004").join(archive_name(&next.id)).is_file());
        symlink(&outside, dir.join("s000005")).unwrap();
        let last = store.seal(&text("after a link"), "").unwrap();
        assert!(dir.join("s000006").join(archive_name(&last.id)).is_file());
        assert_eq!(std::fs::read(dir.join("s000003")).unwrap(), b"not a directory");
        assert_eq!(snapshot(&outside), before);
        std::fs::remove_file(&shelf).unwrap();
        std::fs::remove_file(dir.join("s000003")).unwrap();
        std::fs::remove_file(dir.join("s000005")).unwrap();
        assert_eq!(store.search("", MAX_SEARCH_RESULTS).unwrap().len(), 4);
    }

    /// Makes directories removable again when a test ends, however it ends.
    struct Reopen(Vec<PathBuf>);
    impl Drop for Reopen {
        fn drop(&mut self) {
            for dir in &self.0 {
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
        }
    }

    #[test]
    fn a_shelf_that_cannot_be_opened_does_not_stop_a_seal() {
        if unsafe { libc::geteuid() } == 0 {
            return; // nothing is closed to root
        }
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let seal = |index: usize| store.seal(&text(&format!("archive {index}")), "").unwrap();
        let ids: Vec<_> = (0..=SHELF_FILES).map(|index| seal(index).id).collect();
        let (older, newer) = (dir.join("s000001"), dir.join("s000002"));
        let _reopen = Reopen(vec![older.clone(), newer.clone()]);
        let close = |shelf: &Path, mode: u32| {
            std::fs::set_permissions(shelf, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        // An older shelf: passed over when the seal looks for a duplicate.
        close(&older, 0o000);
        let next = seal(1_000);
        assert!(newer.join(archive_name(&next.id)).is_file());
        // The price: a window that is in it is not seen, and stored again.
        assert_eq!(seal(0).id, ids[0]);
        assert!(newer.join(archive_name(&ids[0])).is_file());
        // A read finds what it can reach, and says what it could not.
        assert_eq!(store.fetch(&next.id, 0, 1).unwrap(), text("archive 1000"));
        assert_eq!(store.fetch(&ids[0], 0, 1).unwrap(), text("archive 0"));
        for id in [ids[5].clone(), "a".repeat(32)] {
            for result in [store.fetch(&id, 0, 1).map(drop), store.forget(&id).map(drop)] {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
                assert!(error.to_string().contains("s000001"), "{error}");
                assert!(error.to_string().contains("could not be read"), "{error}");
            }
        }
        assert_eq!(mode(&older), 0o000);
        // The newest shelf: nothing is written to it, and the next is used.
        close(&older, 0o700);
        close(&newer, 0o000);
        let later = seal(2_000);
        assert!(dir.join("s000003").join(archive_name(&later.id)).is_file());
        assert_eq!(store.fetch(&ids[5], 0, 1).unwrap(), text("archive 5"));
        assert_eq!(mode(&newer), 0o000);
        close(&newer, 0o700);
        assert_eq!(std::fs::read_dir(&newer).unwrap().count(), 3);
        assert_eq!(std::fs::read_dir(&older).unwrap().count(), SHELF_FILES);
        for (id, index) in [(&ids[5], 5), (&ids[SHELF_FILES], SHELF_FILES), (&later.id, 2_000)] {
            assert_eq!(store.fetch(id, 0, 1).unwrap(), text(&format!("archive {index}")));
        }
    }

    #[test]
    fn a_directory_with_an_absurd_shelf_number_is_not_a_shelf() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let seal = |index: usize| store.seal(&text(&format!("archive {index}")), "").unwrap();
        let first = seal(0);
        let file = only_archive_file(&dir);
        // The largest number there is, the one after the last shelf's, and
        // other spellings of numbers: none was made by the store. Each holds
        // a copy of a segment, which would be found if it were looked into.
        let absurd = ["s18446744073709551615", "s1000000", "s0999999", "s99999999999999999999"];
        for name in absurd {
            std::fs::create_dir(dir.join(name)).unwrap();
            std::fs::copy(&file, dir.join(name).join(archive_name(&first.id))).unwrap();
        }
        let before = snapshot(&dir);
        assert_eq!(store.shelves().unwrap(), ["s000001"]);
        let second = seal(1);
        assert!(dir.join("s000001").join(archive_name(&second.id)).is_file());
        assert_eq!(store.search("", 8).unwrap().len(), 2);
        assert_eq!(store.export_records().unwrap().len(), 2);
        let after = snapshot(&dir);
        assert!(before.iter().all(|(path, file)| after.get(path) == Some(file)));
        assert_eq!(after.len(), before.len() + 1);

        // The last number there is belongs to a shelf like any other. With
        // no number after it, it takes more than 128 rather than refuse.
        let last = dir.join("s999999");
        std::fs::create_dir(&last).unwrap();
        let ids: Vec<_> = (2..SHELF_FILES + 5).map(|index| seal(index).id).collect();
        assert_eq!(std::fs::read_dir(&last).unwrap().count(), SHELF_FILES + 3);
        assert_eq!(mode(&last), 0o700);
        assert_eq!(store.shelves().unwrap(), ["s999999", "s000001"]);
        for (id, index) in [(&ids[0], 2), (&ids[SHELF_FILES + 2], SHELF_FILES + 4)] {
            assert_eq!(store.fetch(id, 0, 1).unwrap(), text(&format!("archive {index}")));
        }
        assert_eq!(store.seal(&text("archive 0"), "").unwrap(), first);
        // And if that name is not a directory at all, the newest shelf
        // that can be written to does.
        std::fs::rename(&last, tmp.path().join("moved-away")).unwrap();
        std::fs::write(&last, b"not a directory").unwrap();
        let fallback = seal(1_000);
        assert!(dir.join("s000001").join(archive_name(&fallback.id)).is_file());
        assert_eq!(std::fs::read(&last).unwrap(), b"not a directory");
    }

    /// The bytes of a record of `store` as a seal of `messages` would write
    /// it, under `id` and with the changes a test asks for.
    fn record_bytes(
        store: &ArchiveStore,
        id: &str,
        messages: &[SharedMessage],
        change: impl FnOnce(&mut Record),
    ) -> Vec<u8> {
        let projection = project_hashed(&store.logical, messages, "", None, Reduce::None).unwrap();
        let mut record = Record {
            version: 1,
            scope: store.scope.clone(),
            logical: store.logical.clone(),
            id: id.to_owned(),
            digest: projection.digest,
            created_ms: None,
            body: Body::Live {
                source_message_count: projection.source_message_count,
                messages: projection.messages,
                note: projection.note,
            },
        };
        change(&mut record);
        bounded_json(&record, MAX_SEGMENT_BYTES).unwrap()
    }

    #[test]
    fn a_file_under_the_id_that_is_not_the_segment_is_left_and_the_other_id_is_used() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let shelf = dir.join("s000001");
        store.seal(&text("a shelf to begin with"), "").unwrap();
        let digest = |words: &str| {
            let projection = project_archive("synthetic-logical", &text(words), "", None);
            projection.unwrap().digest
        };
        // Not a record at all; a record of another window that was given
        // this name, valid in itself; and the same with this window's own
        // digest but content that does not match it.
        let other = |id: &str| record_bytes(&store, id, &text("another window"), |_| {});
        let forged = |id: &str, digest: &str| {
            record_bytes(&store, id, &text("another window"), |record| record.digest = digest.into())
        };
        for (case, words) in ["torn", "another projection", "forged digest"].into_iter().enumerate() {
            let digest = digest(words);
            let (first, second) = (&digest[..32], &digest[32..]);
            let taken = shelf.join(archive_name(first));
            let bytes = match case {
                0 => b"{\"version\":1,".to_vec(),
                1 => other(first),
                _ => forged(first, &digest),
            };
            std::fs::write(&taken, &bytes).unwrap();
            let before = snapshot(&dir);
            let (sealed, note) = store.seal_with_note(&text(words), "first note").unwrap();
            assert_eq!((sealed.id.as_str(), note.as_str()), (second, "first note"), "{words}");
            assert_eq!(store.fetch(second, 0, 1).unwrap(), text(words), "{words}");
            assert!(shelf.join(archive_name(second)).is_file(), "{words}");
            // One file more; the one that has the name is as it was.
            let after = snapshot(&dir);
            assert!(before.iter().all(|(path, file)| after.get(path) == Some(file)), "{words}");
            assert_eq!(after.len(), before.len() + 1, "{words}");
            // A second seal of the window looks in the same order, finds
            // it, and writes nothing.
            let again = store.seal_with_note(&text(words), "second note").unwrap();
            assert_eq!(again, (sealed.clone(), "first note".to_owned()), "{words}");
            assert_eq!(snapshot(&dir), after, "{words}");
            // The first id still answers for the file that has its name.
            match store.fetch(first, 0, 1) {
                Ok(found) => assert_eq!((case, found), (1, text("another window"))),
                Err(error) => assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{words}"),
            }
            // Forgotten under its second id, it stays forgotten.
            store.forget(second).unwrap();
            let refused = store.seal(&text(words), "").unwrap_err();
            assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{words}");
            assert_eq!(std::fs::read(&taken).unwrap(), bytes, "{words}");
        }
        // Both names taken by other things: the one seal that is refused
        // for what a file is, and it writes nothing.
        let digest = digest("no name left");
        for id in [&digest[..32], &digest[32..]] {
            std::fs::write(shelf.join(archive_name(id)), other(id)).unwrap();
        }
        let before = snapshot(&dir);
        let refused = store.seal(&text("no name left"), "").unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(snapshot(&dir), before);
        assert!(store.seal(&text("any other window"), "").is_ok());
    }

    /// The one refusal a file that cannot be validated still causes: it
    /// reads as JSON, names this projection and says it was forgotten.
    #[test]
    fn a_tombstone_that_no_longer_validates_still_forbids_its_projection() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let words = "forgotten words";
        let gone = seal_legacy_slot(&store, 0, &text(words), "");
        store.forget(&gone.id).unwrap();
        let digest = project_archive("synthetic-logical", &text(words), "", None);
        let digest = digest.unwrap().digest;
        let tombstone = std::fs::read_to_string(dir.join("000.json")).unwrap();
        // Still a tombstone of this projection to a reader, and no longer a
        // valid record: a later version, a field this version does not
        // know, a record of another project.
        let altered = [
            tombstone.replace("\"version\":1", "\"version\":2"),
            tombstone.replacen('{', "{\"added\":true,", 1),
            tombstone.replace(&store.scope, &"0".repeat(64)),
        ];
        let refused = |kind: &str| {
            let error = store.seal(&text(words), "").unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{kind}");
            assert!(error.to_string().contains("was forgotten"), "{kind}");
        };
        // In a legacy slot, and on a shelf under either id of the projection.
        let shelf = dir.join("s000001");
        let places = [
            dir.join("000.json"),
            shelf.join(archive_name(&digest[..32])),
            shelf.join(archive_name(&digest[32..])),
        ];
        // Each place is tried by itself: neither the slot's tombstone nor
        // the one the forget left under the first id stays.
        std::fs::remove_file(&places[0]).unwrap();
        std::fs::remove_file(&places[1]).unwrap();
        // The second id is looked under whether the first is free or taken.
        for (index, place) in places.iter().enumerate() {
            for bytes in &altered {
                assert!(store.record("000.json", bytes.as_bytes()).is_err());
                std::fs::write(place, bytes).unwrap();
                let before = snapshot(&dir);
                refused(bytes);
                assert_eq!(snapshot(&dir), before);
            }
            if index < 2 {
                std::fs::remove_file(place).unwrap();
            }
        }
        std::fs::write(&places[1], b"not a record").unwrap();
        refused("with the first name taken");
        // Only that projection, and only while the file says so.
        assert!(store.seal(&text("other words"), "").is_ok());
        std::fs::write(&places[2], tombstone.replace("tombstone", "tombstoned")).unwrap();
        let error = store.seal(&text(words), "").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "no name left: {error}");
        std::fs::remove_file(&places[2]).unwrap();
        let again = store.seal(&text(words), "").unwrap();
        assert_eq!(again.id, digest[32..]);
    }

    /// Makes the system call `call` fail with `code` for every name that
    /// starts with `prefix`, on this thread, until it is dropped.
    struct Fault;
    impl Fault {
        fn on(call: &'static str, prefix: &str, code: i32) -> Self {
            FAULTS.with_borrow_mut(|faults| faults.push((call, prefix.to_owned(), code)));
            Fault
        }
    }
    impl Drop for Fault {
        fn drop(&mut self) {
            FAULTS.with_borrow_mut(Vec::clear);
        }
    }
    fn digest_of(words: &str) -> String {
        let projection = project_archive("synthetic-logical", &text(words), "", None);
        projection.unwrap().digest
    }

    /// A window that went under its second id because something odd had the
    /// first is still that one segment when the odd thing is gone.
    #[test]
    fn a_segment_under_its_second_id_is_found_when_the_first_name_is_free_again() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let shelf = dir.join("s000001");
        store.seal(&text("a shelf to begin with"), "").unwrap();
        let digest = digest_of("stored once");
        let (first, second) = (&digest[..32], &digest[32..]);
        let odd = shelf.join(archive_name(first));
        std::fs::write(&odd, b"{\"version\":1,").unwrap();
        let sealed = store.seal_with_note(&text("stored once"), "first note").unwrap();
        assert_eq!(sealed.0.id, second);
        // The odd file is removed. A seal of the same window finds it where
        // it is and writes nothing under the name that is free again.
        std::fs::remove_file(&odd).unwrap();
        let before = snapshot(&dir);
        let again = store.seal_with_note(&text("stored once"), "second note").unwrap();
        assert_eq!(again, sealed);
        assert_eq!(snapshot(&dir), before);
        assert_eq!(store.search("stored once", 8).unwrap().len(), 1);
        // Forgotten under its second id, it stays forgotten although the
        // first name is free.
        store.forget(second).unwrap();
        let forgotten = snapshot(&dir);
        let refused = store.seal(&text("stored once"), "").unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        assert!(refused.to_string().contains("was forgotten"));
        assert_eq!(snapshot(&dir), forgotten);
        assert!(!odd.exists());
    }

    /// With a copy under each name, one live and one forgotten (which a
    /// store from before both names were looked under can hold), the
    /// tombstone decides, whichever name has it.
    #[test]
    fn a_tombstone_under_either_name_refuses_whatever_the_other_holds() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let shelf = dir.join("s000001");
        store.seal(&text("a shelf to begin with"), "").unwrap();
        for (words, forgotten) in [("tombstone first", 0), ("tombstone second", 1)] {
            let digest = digest_of(words);
            let ids = [&digest[..32], &digest[32..]];
            for (index, id) in ids.into_iter().enumerate() {
                let bytes = record_bytes(&store, id, &text(words), |record| {
                    if index == forgotten {
                        record.body = Body::Tombstone;
                    }
                });
                std::fs::write(shelf.join(archive_name(id)), bytes).unwrap();
            }
            assert_eq!(store.fetch(ids[1 - forgotten], 0, 1).unwrap(), text(words));
            let before = snapshot(&dir);
            let refused = store.seal(&text(words), "").unwrap_err();
            assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{words}");
            assert_eq!(snapshot(&dir), before, "{words}");
        }
    }

    /// A newer shelf with something else under the window's name does not
    /// hide what an older shelf holds under it: a tombstone there still
    /// refuses the seal, and a live copy there is still the answer.
    #[test]
    fn a_file_under_the_name_on_a_newer_shelf_does_not_hide_an_older_tombstone_or_copy() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let (older, newer) = (dir.join("s000001"), dir.join("s000002"));
        store.seal(&text("a shelf to begin with"), "").unwrap();
        std::fs::create_dir(&newer).unwrap();
        std::fs::write(tmp.path().join("elsewhere"), b"not in the store").unwrap();
        // What has the name on the newer shelf: a file that is not a record,
        // or one the store does not read at all.
        let odd = |name: &str, link: bool| {
            if link {
                symlink(tmp.path().join("elsewhere"), newer.join(name)).unwrap();
            } else {
                std::fs::write(newer.join(name), b"not a record").unwrap();
            }
        };
        let refused = |words: &str| {
            let before = snapshot(&dir);
            let error = store.seal(&text(words), "").unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{words}: {error}");
            assert_eq!(snapshot(&dir), before, "{words}");
            assert!(store.search(words, 8).unwrap().is_empty(), "{words}");
        };
        let imported = ["0123456789abcdef0123456789abcdef", "fedcba9876543210fedcba9876543210"];
        for (link, imported) in [false, true].into_iter().zip(imported) {
            // Forgotten with the odd file already there. The forget goes by
            // another id of the window, as a forget of an imported copy does.
            let words = format!("forgotten behind an odd file, link {link}");
            let sealed = store.seal(&text(&words), "").unwrap();
            let name = archive_name(&sealed.id);
            assert_eq!(sealed.id, digest_of(&words)[..32]);
            std::fs::rename(newer.join(&name), older.join(&name)).unwrap();
            let copy = record_bytes(&store, imported, &text(&words), |_| {});
            std::fs::write(older.join(archive_name(imported)), copy).unwrap();
            odd(&name, link);
            let mut ids = store.forget(imported).unwrap().ids;
            ids.sort_unstable();
            let mut expected = [imported.to_owned(), sealed.id.clone()];
            expected.sort_unstable();
            assert_eq!(ids, expected);
            assert!(is_tombstone_of(&store, &older.join(&name), &digest_of(&words)));
            refused(&words);

            // No forget in between: an older tombstone, whole or one that no
            // longer validates, behind a newer odd file of the same name.
            for whole in [true, false] {
                let words = format!("an older tombstone, link {link}, whole {whole}");
                let digest = digest_of(&words);
                let id = &digest[..32];
                let bytes = record_bytes(&store, id, &text(&words), |record| {
                    record.body = Body::Tombstone;
                    if !whole {
                        record.version = 2;
                    }
                });
                std::fs::write(older.join(archive_name(id)), bytes).unwrap();
                odd(&archive_name(id), link);
                refused(&words);
            }

            // A live copy on the older shelf is the segment: no second copy.
            let words = format!("an older copy, link {link}");
            let sealed = store.seal_with_note(&text(&words), "first note").unwrap();
            let name = archive_name(&sealed.0.id);
            std::fs::rename(newer.join(&name), older.join(&name)).unwrap();
            odd(&name, link);
            let before = snapshot(&dir);
            assert_eq!(store.seal_with_note(&text(&words), "a later note").unwrap(), sealed);
            assert_eq!(snapshot(&dir), before);
        }
        // With nothing of the window on any shelf, the name is only taken.
        let words = "only an odd file";
        let digest = digest_of(words);
        odd(&archive_name(&digest[..32]), false);
        assert_eq!(store.seal(&text(words), "").unwrap().id, digest[32..]);
    }

    /// A read of the file under the id that fails says nothing about what
    /// the file is. The seal fails and is tried again; it does not store the
    /// window a second time under its other id.
    #[test]
    fn a_failed_read_of_the_file_under_the_id_fails_the_seal_and_stores_no_second_copy() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let sealed = store.seal_with_note(&text("stored once"), "first note").unwrap();
        let before = snapshot(&dir);
        for code in [libc::EIO, libc::EMFILE, libc::ENFILE, libc::ENOMEM, libc::EINTR] {
            let failing = Fault::on("open", &sealed.0.id, code);
            let error = store.seal(&text("stored once"), "").unwrap_err();
            assert_eq!(error.raw_os_error(), Some(code));
            drop(failing);
            assert_eq!(snapshot(&dir), before, "error {code}");
        }
        let again = store.seal_with_note(&text("stored once"), "a later note").unwrap();
        assert_eq!(again, sealed);
        assert_eq!(snapshot(&dir), before);
        // What a file is still never refuses a save: one that cannot be
        // opened by its owner has the name taken, like a link or a FIFO.
        for code in [libc::ELOOP, libc::EACCES, libc::EPERM, libc::ENXIO] {
            assert!(PrivateDir::not_a_segment(&io::Error::from_raw_os_error(code)));
        }
        assert!(PrivateDir::not_a_segment(&corrupt()));
        if unsafe { libc::geteuid() } == 0 {
            return; // nothing is closed to root
        }
        let file = only_archive_file(&dir);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        let other = store.seal(&text("stored once"), "").unwrap();
        assert_eq!(other.id, digest_of("stored once")[32..]);
        assert_eq!(mode(&file), 0o000);
    }

    /// Whether the file is a tombstone of `digest` under the id its name gives.
    fn is_tombstone_of(store: &ArchiveStore, file: &Path, digest: &str) -> bool {
        let name = file.file_name().unwrap().to_str().unwrap();
        let record = store.record(name, &std::fs::read(file).unwrap()).unwrap();
        record.digest == digest && matches!(record.body, Body::Tombstone)
    }
    fn forgotten(ids: &[&str]) -> Forgotten {
        Forgotten {
            ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            unread: None,
        }
    }

    /// One window in several files: under both halves of its digest, under
    /// one id on two shelves, in a legacy slot. Forgetting any one id
    /// forgets the window.
    #[test]
    fn forget_reaches_every_copy_of_the_window() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let (older, newer) = (dir.join("s000001"), dir.join("s000002"));
        let gone = |words: &str| {
            assert!(store.search(words, 8).unwrap().is_empty(), "{words}");
            let refused = store.seal(&text(words), "").unwrap_err();
            assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{words}");
        };
        // Under both halves of the digest, as a build before both names were
        // looked under could leave it. Forgotten by the second.
        let words = "under both halves";
        let digest = digest_of(words);
        let (first, second) = (&digest[..32], &digest[32..]);
        assert_eq!(store.seal(&text(words), "").unwrap().id, first);
        let copy = record_bytes(&store, second, &text(words), |_| {});
        std::fs::write(older.join(archive_name(second)), copy).unwrap();
        assert_eq!(store.search(words, 8).unwrap().len(), 2);
        assert_eq!(store.forget(second).unwrap(), forgotten(&[second, first]));
        for id in [first, second] {
            assert_eq!(store.fetch(id, 0, 1).unwrap_err().kind(), io::ErrorKind::NotFound);
            assert!(is_tombstone_of(&store, &older.join(archive_name(id)), &digest));
        }
        gone(words);
        // Forgotten again: nothing is left to do, and nothing is written.
        let before = snapshot(&dir);
        assert_eq!(store.forget(first).unwrap(), forgotten(&[]));
        assert_eq!(snapshot(&dir), before);

        // Under one id on two shelves, as a seal that could not open the
        // older shelf leaves it.
        let words = "one id on two shelves";
        let twice = store.seal(&text(words), "").unwrap();
        let name = archive_name(&twice.id);
        std::fs::create_dir(&newer).unwrap();
        std::fs::copy(older.join(&name), newer.join(&name)).unwrap();
        assert_eq!(store.search(words, 8).unwrap().len(), 2);
        assert_eq!(store.forget(&twice.id).unwrap(), forgotten(&[&twice.id, &twice.id]));
        for shelf in [&older, &newer] {
            assert!(is_tombstone_of(&store, &shelf.join(&name), &digest_of(words)));
        }
        gone(words);

        // In a legacy slot and on a shelf. Forgotten by the slot's id.
        let words = "in a slot and on a shelf";
        let slot = seal_legacy_slot(&store, 0, &text(words), "");
        let digest = digest_of(words);
        let copy = record_bytes(&store, &digest[32..], &text(words), |_| {});
        std::fs::write(older.join(archive_name(&digest[32..])), copy).unwrap();
        // The copy a seal looks under is replaced first.
        let done = store.forget(&slot.id).unwrap();
        assert_eq!(done, forgotten(&[&slot.id, &digest[32..]]));
        assert!(is_tombstone_of(&store, &dir.join("000.json"), &digest));
        gone(words);
        assert!(temporary_files(&dir).is_empty());
    }

    /// A later seal of a forgotten window looks under the two ids its digest
    /// gives it. A window that was only stored under another id (a legacy
    /// slot's, an import's) leaves a tombstone there too, written first.
    #[test]
    fn a_forget_leaves_a_tombstone_where_a_seal_looks() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let shelf = dir.join("s000001");
        let imported = "0123456789abcdef0123456789abcdef";
        // In a legacy slot, in a project that has no shelf yet.
        let words = "only in a slot";
        let digest = digest_of(words);
        let slot = seal_legacy_slot(&store, 0, &text(words), "");
        assert_eq!(store.forget(&slot.id).unwrap(), forgotten(&[&slot.id]));
        let guard = shelf.join(archive_name(&digest[..32]));
        assert!(is_tombstone_of(&store, &guard, &digest));
        assert!(is_tombstone_of(&store, &dir.join("000.json"), &digest));
        // The slot can go bad; the window stays forgotten.
        std::fs::write(dir.join("000.json"), b"{\"version\":1,").unwrap();
        let refused = store.seal(&text(words), "").unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        std::fs::remove_file(dir.join("000.json")).unwrap();

        // On a shelf under an id its digest does not give it, with the first
        // of the two names taken by something else: the second is used.
        let words = "under an imported id";
        let digest = digest_of(words);
        let file = shelf.join(archive_name(imported));
        std::fs::write(&file, record_bytes(&store, imported, &text(words), |_| {})).unwrap();
        std::fs::write(shelf.join(archive_name(&digest[..32])), b"not a record").unwrap();
        assert_eq!(store.fetch(imported, 0, 1).unwrap(), text(words));
        // The tombstone a seal will find is written before the archive is
        // replaced: when that fails, the window is still there and can
        // already not be sealed again.
        let failing = Fault::on("rename", imported, libc::EIO);
        assert_eq!(store.forget(imported).unwrap_err().raw_os_error(), Some(libc::EIO));
        drop(failing);
        let guard = shelf.join(archive_name(&digest[32..]));
        assert!(is_tombstone_of(&store, &guard, &digest));
        assert_eq!(store.fetch(imported, 0, 1).unwrap(), text(words));
        let refused = store.seal(&text(words), "").unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        // Forgetting again finishes, and writes no second tombstone.
        let files = shelf_files(&dir).len();
        // The file that has the first name is not a record: it may be a
        // damaged copy, so neither forget calls itself complete.
        let place = format!("1 place(s) could not be read, first s000001/{}: ", archive_name(&digest[..32]));
        let done = store.forget(imported).unwrap();
        assert_eq!(done.ids, [imported]);
        assert!(done.unread.unwrap().starts_with(&place));
        assert!(is_tombstone_of(&store, &file, &digest));
        assert_eq!(shelf_files(&dir).len(), files);
        assert_eq!(std::fs::read(shelf.join(archive_name(&digest[..32]))).unwrap(), b"not a record");
        // An archive that was forgotten before this rule gets its tombstone
        // the next time it is forgotten.
        std::fs::remove_file(&guard).unwrap();
        let done = store.forget(imported).unwrap();
        assert!(done.ids.is_empty());
        assert!(done.unread.unwrap().starts_with(&place));
        assert!(is_tombstone_of(&store, &guard, &digest));

        // Stored under one of its two ids, the archive's own tombstone is
        // the one a seal finds: nothing is added.
        let native = store.seal(&text("under its own id"), "").unwrap();
        let files = shelf_files(&dir).len();
        assert_eq!(store.forget(&native.id).unwrap(), forgotten(&[&native.id]));
        assert_eq!(shelf_files(&dir).len(), files);
        assert!(temporary_files(&dir).is_empty());
    }

    /// A copy in a place that cannot be read is not reached. The forget
    /// says so, and forgetting again reaches it.
    #[test]
    fn a_forget_says_which_places_it_could_not_read() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let (older, newer) = (dir.join("s000001"), dir.join("s000002"));
        let words = "one id on two shelves";
        let twice = store.seal(&text(words), "").unwrap();
        let name = archive_name(&twice.id);
        std::fs::create_dir(&newer).unwrap();
        std::fs::copy(older.join(&name), newer.join(&name)).unwrap();
        // The older shelf cannot be opened: its copy stays live.
        let failing = Fault::on("open", "s000001", libc::EIO);
        let done = store.forget(&twice.id).unwrap();
        drop(failing);
        assert_eq!(done.ids, std::slice::from_ref(&twice.id));
        let unread = done.unread.unwrap();
        assert!(unread.starts_with("1 place(s) could not be read, first s000001: "), "{unread}");
        assert!(is_tombstone_of(&store, &newer.join(&name), &digest_of(words)));
        let rows = store.search(words, 8).unwrap();
        assert_eq!(rows.len(), 1, "the copy that was not reached is still found");
        // Forgotten again, with the shelf readable: reached.
        assert_eq!(store.forget(&twice.id).unwrap(), forgotten(&[&twice.id]));
        assert!(store.search(words, 8).unwrap().is_empty());

        // A file under one of the window's names that the store does not
        // read may be a copy too.
        let words = "beside a file with two names";
        let digest = digest_of(words);
        let sealed = store.seal(&text(words), "").unwrap();
        let linked = newer.join(archive_name(&digest[32..]));
        std::fs::hard_link(newer.join(archive_name(&sealed.id)), tmp.path().join("a-second-name")).unwrap();
        std::fs::write(&linked, b"x").unwrap();
        std::fs::hard_link(&linked, tmp.path().join("another-second-name")).unwrap();
        let error = store.forget(&sealed.id).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "its own file has two names");
        std::fs::remove_file(tmp.path().join("a-second-name")).unwrap();
        let done = store.forget(&sealed.id).unwrap();
        assert_eq!(done.ids, std::slice::from_ref(&sealed.id));
        let place = format!("first s000002/{}: ", archive_name(&digest[32..]));
        assert!(done.unread.unwrap().contains(&place));
        assert_eq!(std::fs::read(&linked).unwrap(), b"x");
    }

    /// A file under the id on a newer shelf that is not a valid record does
    /// not hide the valid record under the id on an older shelf: a fetch
    /// returns it, and a forget by that id forgets it and names the odd file
    /// as not reached.
    #[test]
    fn a_lookup_by_id_passes_over_a_file_that_is_not_a_record_to_an_older_shelf() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let (older, newer) = (dir.join("s000001"), dir.join("s000002"));
        std::fs::create_dir(&older).unwrap();
        std::fs::create_dir(&newer).unwrap();
        std::fs::write(tmp.path().join("elsewhere"), b"not in the store").unwrap();
        let base = tmp.path();
        for link in [false, true] {
            let words = format!("behind an odd file of its name, link {link}");
            let digest = digest_of(&words);
            let sealed = store.seal_with_note(&text(&words), "its note").unwrap().0;
            let name = archive_name(&sealed.id);
            assert_eq!(sealed.id, digest[..32]);
            std::fs::rename(newer.join(&name), older.join(&name)).unwrap();
            if link {
                symlink(tmp.path().join("elsewhere"), newer.join(&name)).unwrap();
            } else {
                std::fs::write(newer.join(&name), b"not a record").unwrap();
            }
            // Read by id: the valid copy.
            assert_eq!(store.fetch(&sealed.id, 0, 1).unwrap(), text(&words));
            assert_eq!(store.fetch_note(&sealed.id).unwrap(), "its note");
            let file = archive_file(base, "synthetic-project", &sealed.id).unwrap();
            assert_eq!(file, Some(older.join(&name)));
            // Forgotten by that id: the valid copy, and never a plain "done".
            let done = store.forget(&sealed.id).unwrap();
            assert_eq!(done.ids, std::slice::from_ref(&sealed.id));
            let unread = done.unread.unwrap();
            let place = format!("1 place(s) could not be read, first s000002/{name}: ");
            assert!(unread.starts_with(&place), "{unread}");
            assert!(is_tombstone_of(&store, &older.join(&name), &digest));
            let kind = std::fs::symlink_metadata(newer.join(&name)).unwrap().file_type();
            assert_eq!(kind.is_symlink(), link, "the odd file is left as it is");
            let error = store.fetch(&sealed.id, 0, 1).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
            let before = snapshot(&dir);
            let refused = store.seal(&text(&words), "").unwrap_err();
            assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(snapshot(&dir), before);
            assert!(store.search(&words, 8).unwrap().is_empty());
        }

        // With no valid record under the id on any shelf, the error is the
        // odd file's, as it was.
        let lone = "0123456789abcdef0123456789abcdef";
        std::fs::write(newer.join(archive_name(lone)), b"not a record").unwrap();
        std::fs::write(older.join(archive_name(lone)), b"{}").unwrap();
        for error in [store.fetch(lone, 0, 1).unwrap_err(), store.forget(lone).unwrap_err()] {
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
        assert!(archive_file(base, "synthetic-project", lone).is_err());
        // A read that failed says nothing about the file: still the error.
        let words = "behind a file that cannot be read now";
        let sealed = store.seal(&text(words), "").unwrap();
        let name = archive_name(&sealed.id);
        std::fs::copy(newer.join(&name), older.join(&name)).unwrap();
        let failing = Fault::on("open", &sealed.id, libc::EIO);
        assert_eq!(store.fetch(&sealed.id, 0, 1).unwrap_err().raw_os_error(), Some(libc::EIO));
        assert_eq!(store.forget(&sealed.id).unwrap_err().raw_os_error(), Some(libc::EIO));
        drop(failing);

        // The odd file is a tombstone of this very window that no longer
        // validates: the older copy is not handed out, and a forget by the
        // id still reaches it.
        let words = "behind a damaged tombstone";
        let digest = digest_of(words);
        let sealed = store.seal(&text(words), "").unwrap();
        let name = archive_name(&sealed.id);
        std::fs::rename(newer.join(&name), older.join(&name)).unwrap();
        let damaged = record_bytes(&store, &sealed.id, &text(words), |record| {
            record.body = Body::Tombstone;
            record.version = 2;
        });
        std::fs::write(newer.join(&name), damaged).unwrap();
        assert_eq!(store.fetch(&sealed.id, 0, 1).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(store.fetch_note(&sealed.id).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(archive_file(base, "synthetic-project", &sealed.id).is_err());
        assert_eq!(store.forget(&sealed.id).unwrap(), forgotten(&[&sealed.id]));
        assert!(is_tombstone_of(&store, &older.join(&name), &digest));
    }

    /// A file under one of the window's own names that was read and is not
    /// a valid record may be a damaged copy, with the window's text still on
    /// disk. The forget does not call itself complete over it: `unread` is
    /// what `memory_forget` turns into "NOT COMPLETE".
    #[test]
    fn a_forget_is_not_complete_over_a_damaged_copy_under_the_windows_name() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let shelf = dir.join("s000001");
        let words = "a window with a damaged copy";
        let digest = digest_of(words);
        let (first, second) = (&digest[..32], &digest[32..]);
        assert_eq!(store.seal(&text(words), "").unwrap().id, first);
        // A second copy, cut short: not a record, and the text is in it.
        let copy = record_bytes(&store, second, &text(words), |_| {});
        let damaged = &copy[..copy.len() - 2];
        assert!(String::from_utf8_lossy(damaged).contains(words));
        let file = shelf.join(archive_name(second));
        std::fs::write(&file, damaged).unwrap();

        let done = store.forget(first).unwrap();
        assert_eq!(done.ids, [first]);
        assert_ne!(done, forgotten(&[first]), "not a plain 'forgot'");
        let unread = done.unread.unwrap();
        let place = format!("1 place(s) could not be read, first s000001/{}: ", archive_name(second));
        assert!(unread.starts_with(&place), "{unread}");
        assert!(is_tombstone_of(&store, &shelf.join(archive_name(first)), &digest));
        assert_eq!(std::fs::read(&file).unwrap(), damaged, "left as it is");
        // Said again for as long as the file is there.
        assert!(store.forget(first).unwrap().unread.is_some());

        // A file there that is a valid record of another window is not a
        // copy, and a damaged tombstone of this one says "forgotten":
        // neither makes the forget incomplete.
        let other = record_bytes(&store, second, &text("another window"), |_| {});
        std::fs::write(&file, other).unwrap();
        assert_eq!(store.forget(first).unwrap(), forgotten(&[]));
        let tombstone = record_bytes(&store, second, &text(words), |record| {
            record.body = Body::Tombstone;
            record.version = 2;
        });
        std::fs::write(&file, tombstone).unwrap();
        assert_eq!(store.forget(first).unwrap(), forgotten(&[]));
    }

    const PASSING: [i32; 5] = [libc::EMFILE, libc::ENFILE, libc::ENOMEM, libc::EINTR, libc::EAGAIN];

    /// An error that is not about the shelf (the process is out of
    /// descriptors, say) is not a reason to pass over it: what the shelf
    /// holds is not seen then, and a window in it would be stored again and
    /// a forgotten one would be back.
    #[test]
    fn a_passing_error_on_a_shelf_or_a_slot_fails_the_seal_and_changes_nothing() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let slot = seal_legacy_slot(&store, 0, &text("in a slot"), "");
        store.forget(&slot.id).unwrap();
        // As a build from before a forget left a tombstone on a shelf
        // forgot it: only the slot says so.
        std::fs::remove_file(only_archive_file(&dir)).unwrap();
        let kept = store.seal(&text("kept"), "").unwrap();
        let gone = store.seal(&text("forgotten"), "").unwrap();
        store.forget(&gone.id).unwrap();
        // Later seals go to a newer shelf.
        std::fs::create_dir(dir.join("s000002")).unwrap();
        let before = snapshot(&dir);
        let entries = || std::fs::read_dir(&dir).unwrap().count();
        let listed = entries();
        let refused = |words: &str, code: i32| {
            let error = store.seal(&text(words), "").unwrap_err();
            assert_eq!(error.raw_os_error(), Some(code), "{words}: {error}");
        };
        for code in PASSING {
            assert!(PrivateDir::passing(&io::Error::from_raw_os_error(code)));
            // The older shelf, the newest one, every shelf, the slot.
            for place in ["s000001", "s000002", "s", "000.json"] {
                let failing = Fault::on("open", place, code);
                // A window found on a shelf is not looked for in the slots.
                if place != "000.json" {
                    refused("kept", code);
                    refused("forgotten", code);
                }
                refused("in a slot", code);
                refused("something new", code);
                drop(failing);
                assert_eq!(snapshot(&dir), before, "{place}, error {code}");
                assert_eq!(entries(), listed, "{place}, error {code}: a shelf was started");
            }
        }
        // The trouble over, everything is what it was.
        assert_eq!(store.seal(&text("kept"), "").unwrap(), kept);
        for words in ["forgotten", "in a slot"] {
            let error = store.seal(&text(words), "").unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{words}");
        }
        assert_eq!(snapshot(&dir), before);
    }

    /// A shelf or a slot that is itself the trouble, an I/O error reading it
    /// included, is still passed over: one bad shelf never stops a save.
    #[test]
    fn an_io_error_on_a_shelf_or_a_slot_is_passed_over() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        seal_legacy_slot(&store, 0, &text("in a slot"), "");
        let kept = store.seal(&text("kept"), "").unwrap();
        for code in [libc::EIO, libc::EACCES, libc::ENOTDIR, libc::ELOOP, libc::ESTALE] {
            assert!(!PrivateDir::passing(&io::Error::from_raw_os_error(code)));
        }
        // An older shelf: the new window goes to the newest.
        std::fs::create_dir(dir.join("s000002")).unwrap();
        let failing = Fault::on("open", "s000001", libc::EIO);
        let new = store.seal(&text("something new"), "").unwrap();
        assert!(dir.join("s000002").join(archive_name(&new.id)).is_file());
        // The price: a window in it is not seen and is stored again.
        assert_eq!(store.seal(&text("kept"), "").unwrap(), kept);
        assert!(dir.join("s000002").join(archive_name(&kept.id)).is_file());
        drop(failing);
        // The newest shelf: the next number is started.
        let failing = Fault::on("open", "s000002", libc::EIO);
        let next = store.seal(&text("after the newest"), "").unwrap();
        assert!(dir.join("s000003").join(archive_name(&next.id)).is_file());
        drop(failing);
        // A slot: the window in it is stored again, on a shelf.
        let _failing = Fault::on("open", "000.json", libc::EIO);
        let again = store.seal(&text("in a slot"), "").unwrap();
        assert!(dir.join("s000003").join(archive_name(&again.id)).is_file());
    }

    /// A shelf that was made and then could not be opened is the one the
    /// next seal uses. Failed attempts do not leave one shelf each.
    #[test]
    fn a_failed_attempt_leaves_at_most_one_empty_shelf_and_the_next_seal_uses_it() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        store.seal(&text("a lock to begin with"), "").unwrap();
        // The newest shelf name is not a shelf, so a seal starts the next
        // one; out of descriptors, it makes the directory and cannot open it.
        std::fs::write(dir.join("s000002"), b"not a directory").unwrap();
        let shelves = || {
            let mut names: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
            names.retain(|entry| entry.as_ref().unwrap().file_name() != "lock");
            names.len()
        };
        let failing = Fault::on("open", "s000003", libc::EMFILE);
        for _ in 0..3 {
            let error = store.seal(&text("after the trouble"), "").unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
            assert_eq!(shelves(), 3, "s000001, the file s000002 and the empty s000003");
        }
        assert_eq!(std::fs::read_dir(dir.join("s000003")).unwrap().count(), 0);
        drop(failing);
        let sealed = store.seal(&text("after the trouble"), "").unwrap();
        assert!(dir.join("s000003").join(archive_name(&sealed.id)).is_file());
        assert_eq!(shelves(), 3);
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
        assert_eq!(before.len(), 4, "lock, two legacy slots and the forget's tombstone on a shelf");
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
        assert_eq!(stopped, Some(Unscanned { stopped_after: Some(17), unread: None }));
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
        // On the shelf so far: the tombstone each forget left where a seal
        // of that window looks, and no archive.
        let shelf = dir.join("s000001");
        let guards = ["legacy alpha", "legacy gamma"].map(|words| {
            let digest = digest_of(words);
            let guard = shelf.join(archive_name(&digest[..32]));
            assert!(is_tombstone_of(&store, &guard, &digest), "{words}");
            guard
        });
        let mut guards = guards.to_vec();
        guards.sort();
        assert_eq!(shelf_files(&dir), guards);
        // Only what is sealed from now on goes to a shelf, under its digest id.
        let new = store.seal(&text("after shelves"), "").unwrap();
        assert!(shelf.join(archive_name(&new.id)).is_file());
        assert_eq!(shelf_files(&dir).len(), 3);
        assert_eq!(found(""), [new.id.as_str(), b.id.as_str()]);
        assert_eq!(std::fs::read(&slots[1]).unwrap(), kept);
        assert!(slots.iter().all(|slot| slot.is_file()));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 5);
        // A forgotten window is carried once, though it has two tombstones.
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
        // Neither seal wrote anything. The one file on a shelf is the
        // tombstone the forget left where a seal looks.
        let guard = only_archive_file(&dir);
        assert!(is_tombstone_of(&store, &guard, &digest_of("forgotten long ago")));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 4, "lock, two slots, one shelf");
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
        let budget = Unscanned { stopped_after: Some(SEARCH_SCAN_FILES), unread: None };
        assert_eq!(stopped, Some(budget));
        assert_eq!(store.search("needle", 1).unwrap()[0].id, ids[SEARCH_SCAN_FILES]);
        assert_eq!(store.fetch(&ids[0], 0, 1).unwrap(), text("needle oldest-only"));
    }

    /// Inside a shelf a search reads newest first by modification time,
    /// which a coarse filesystem clock gives to a whole burst of seals at
    /// once. Files with the same time are read in the order of their names,
    /// so the order, and with it what a short page holds, is the same every
    /// time and does not depend on how the directory lists them.
    #[test]
    fn search_reads_files_with_the_same_modification_time_in_name_order() {
        let (tmp, store) = setup();
        let shelf = directory(&tmp, &store).join("s000001");
        let ids: Vec<_> = (0..8)
            .map(|index| store.seal(&text(&format!("tied {index}")), "").unwrap().id)
            .collect();
        let epoch = std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let written = |id: &str, seconds: u64| {
            let path = shelf.join(archive_name(id));
            let file = std::fs::File::options().write(true).open(path).unwrap();
            file.set_modified(epoch + Duration::from_secs(seconds)).unwrap();
        };
        // Six in the same instant, one before it and one after.
        for id in &ids[2..] {
            written(id, 100);
        }
        written(&ids[0], 50);
        written(&ids[1], 200);
        let mut tied = ids[2..].to_vec();
        tied.sort();
        assert_ne!(tied, ids[2..], "the order of sealing is not the order of names");
        let mut expected = vec![ids[1].clone()];
        expected.extend(tied);
        expected.push(ids[0].clone());
        let order = |limit: usize| -> Vec<String> {
            let rows = store.search("tied", limit).unwrap();
            rows.into_iter().map(|row| row.id).collect()
        };
        assert_eq!(order(8), expected);
        // A shorter page is the beginning of the same order, every time.
        for limit in 1..8 {
            assert_eq!(order(limit), expected[..limit]);
            assert_eq!(order(limit), expected[..limit]);
        }
        // A file added in the same instant takes its place by name; the
        // others keep theirs.
        let late = store.seal(&text("tied late"), "").unwrap().id;
        written(&late, 100);
        expected.insert(1 + expected[1..7].partition_point(|id| *id < late), late);
        assert_eq!(order(9), expected);
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
        let (rows, unscanned) = store.search_scan("", 10).unwrap();
        let mut found: Vec<_> = rows.into_iter().map(|row| row.id).collect();
        found.sort();
        let mut expected = vec![ids[0].clone(), ids[2].clone(), legacy.id];
        expected.sort();
        assert_eq!(found, expected);
        // The budget did not stop it, and it says what it skipped: the
        // corrupt file, the one too large to read and the slot.
        let unscanned = unscanned.unwrap();
        assert_eq!(unscanned.stopped_after, None);
        let unread = unscanned.unread.unwrap();
        assert!(unread.starts_with("3 place(s) could not be read, first s000001/"), "{unread}");
        assert_eq!(store.search("second", 10).unwrap(), []);
    }

    /// One place that cannot be read hides only itself from a search, and
    /// the search says that its rows are not all the project holds.
    #[test]
    fn a_search_passes_over_what_it_cannot_read_and_says_so() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let old = store.seal(&text("on the oldest shelf"), "").unwrap();
        let slot = seal_legacy_slot(&store, 0, &text("in a slot"), "");
        let mut all = vec![old.id.clone(), slot.id.clone()];
        // What the search still finds, and what it says it did not read.
        let searched = |all: &[String], limit: usize| {
            let (rows, unscanned) = store.search_scan("", limit).unwrap();
            let mut found: Vec<_> = rows.into_iter().map(|row| row.id).collect();
            found.sort();
            let mut all = all.to_vec();
            all.sort();
            if limit >= all.len() {
                assert_eq!(found, all);
            } else {
                assert_eq!(found.len(), limit);
            }
            match unscanned {
                Some(unscanned) => {
                    assert_eq!(unscanned.stopped_after, None);
                    unscanned.unread.unwrap()
                }
                None => String::new(),
            }
        };
        assert_eq!(searched(&all, 8), "");

        // A shelf name that is a file. The seal after it takes the next number.
        std::fs::write(dir.join("s000002"), b"not a directory").unwrap();
        let unread = searched(&all, 8);
        assert!(unread.starts_with("1 place(s) could not be read, first s000002: "), "{unread}");
        let new = store.seal(&text("on the newest shelf"), "").unwrap();
        let newest = dir.join("s000003");
        assert!(newest.join(archive_name(&new.id)).is_file());
        all.push(new.id.clone());
        // The same with a link to a directory under that name.
        std::fs::remove_file(dir.join("s000002")).unwrap();
        symlink(&newest, dir.join("s000002")).unwrap();
        let unread = searched(&all, 8);
        assert!(unread.starts_with("1 place(s) could not be read, first s000002: "), "{unread}");

        // Where a segment belongs: a link, a FIFO, a file with a second
        // name and a file that is not a record.
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, b"do not change").unwrap();
        let odd = |letter: &str| newest.join(archive_name(&letter.repeat(32)));
        symlink(&victim, odd("a")).unwrap();
        let fifo = std::ffi::CString::new(odd("b").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        std::fs::write(odd("c"), record_bytes(&store, &"c".repeat(32), &text("two names"), |_| {})).unwrap();
        std::fs::hard_link(odd("c"), tmp.path().join("a-second-name")).unwrap();
        std::fs::write(odd("d"), b"corrupt").unwrap();
        let unread = searched(&all, 8);
        assert!(unread.starts_with("5 place(s) could not be read, first s000003/"), "{unread}");
        // A legacy slot that is a link.
        symlink(&victim, dir.join("001.json")).unwrap();
        let unread = searched(&all, 8);
        assert!(unread.starts_with("6 place(s) could not be read, first s000003/"), "{unread}");
        // A shelf that cannot be opened for an error that will pass: the
        // search is a read, so the rest is still an answer.
        let rest = [old.id.clone(), slot.id.clone()];
        let failing = Fault::on("open", "s000003", libc::EMFILE);
        let unread = searched(&rest, 8);
        assert!(unread.starts_with("3 place(s) could not be read, first s000003: "), "{unread}");
        // A page that fills is still not all there was to read on the way.
        let unread = searched(&rest, 1);
        assert!(unread.starts_with("2 place(s) could not be read, first s000003: "), "{unread}");
        drop(failing);
        // And one whose mode refuses it.
        if unsafe { libc::geteuid() } != 0 {
            let _reopen = Reopen(vec![newest.clone()]);
            std::fs::set_permissions(&newest, std::fs::Permissions::from_mode(0o000)).unwrap();
            let unread = searched(&rest, 8);
            assert!(unread.starts_with("3 place(s) could not be read, first s000003: "), "{unread}");
        }
        // Nothing was followed, read into or changed.
        assert_eq!(std::fs::read(&victim).unwrap(), b"do not change");
        assert!(std::fs::symlink_metadata(odd("a")).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(odd("d")).unwrap(), b"corrupt");
        // The plain search gives the same rows and no account of the rest.
        assert_eq!(store.search("", 8).unwrap().len(), 3);
    }

    /// Legacy slots go by the same rule as shelves in an export: a window in
    /// two of them is carried once, forgotten if either says so, and two
    /// records with one id and different content are an error.
    #[test]
    fn export_carries_a_window_in_two_legacy_slots_once() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let slot = seal_legacy_slot(&store, 0, &text("in two slots"), "");
        std::fs::copy(dir.join("000.json"), dir.join("001.json")).unwrap();
        let rows = store.export_records().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((&rows[0]["id"], &rows[0]["tombstone"]), (&json!(slot.id), &json!(false)));
        let live = std::fs::read_to_string(dir.join("000.json")).unwrap();
        let body = &live[live.find("\"body\":").unwrap()..];
        let tombstone = live.replace(body, "\"body\":{\"state\":\"tombstone\"}}");
        std::fs::write(dir.join("001.json"), tombstone).unwrap();
        let rows = store.export_records().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((&rows[0]["id"], &rows[0]["tombstone"]), (&json!(slot.id), &json!(true)));
        let other = record_bytes(&store, &slot.id, &text("other content"), |_| {});
        std::fs::write(dir.join("002.json"), other).unwrap();
        let error = store.export_records().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
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
            store.forget(&b).map(drop),
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
        // The same record, but for when each was written.
        let undated = |dir: PathBuf| {
            let mut record: Value = serde_json::from_slice(&std::fs::read(dir).unwrap()).unwrap();
            assert!(record["created_ms"].take().is_u64());
            record
        };
        assert_eq!(
            undated(only_archive_file(&directory(&tmp, &store))),
            undated(only_archive_file(&directory(&strict_tmp, &strict)))
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
        for (len, parts) in [(MAX_MESSAGES + 1, 2), (10_000, 3)] {
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
            // Each part is full before the next is started.
            let starts: Vec<_> = window.parts.iter().map(|part| part.first_source_index).collect();
            let full: Vec<_> = (0..parts).map(|part| part * MAX_MESSAGES).collect();
            assert_eq!(starts, full);
        }
        assert_eq!(shelf_files(&directory(&tmp, &store)).len(), 5);
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
        // 1,365 calls are 65,520 nodes; one more would be over.
        let starts: Vec<_> = window.parts.iter().map(|part| part.first_source_index).collect();
        assert_eq!(starts, [0, 1365, 2730]);
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

    /// The depth and node limits are about the shape of a tool input. As one
    /// string it has none, so an input that is only too deep is all there.
    #[test]
    fn single_deep_input_is_stored_as_its_json_text_not_left_out() {
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
        assert_eq!(reduced, [(0, 0), (1, 1), (2, 0)]);
        let read = reassemble(&store, &window, messages.len());
        for index in [0, 2, 3] {
            assert_eq!(read[index].source_index, index);
            assert_eq!(read[index].message, messages[index]);
        }
        // Of that message only the one input that is too deep is not stored
        // as it was, and nothing of it is missing: it is one string. The
        // text and the other call's input are exact.
        let as_text = serde_json::to_string(&deep_input()).unwrap();
        let expected = json!({
            "role":"assistant",
            "_archive_reduced":"this message alone is over the per-archive limit; it is stored \
                with 1 tool input(s) as JSON text, each marked in place",
            "content":[
                {"type":"text","text":"running both"},
                {"type":"tool_use","id":"t1","name":"nest","input":{"_archive_json":as_text}},
                {"type":"tool_use","id":"t2","name":"read","input":{"path":"src/lib.rs"}}
            ]
        });
        assert_eq!(*read[1].message, expected);
        assert_eq!(read[1].block_indices, [0, 1, 2]);
        // What was stored is the input: it reads back equal.
        let stored = read[1].message["content"][1]["input"][INPUT_JSON_KEY].as_str().unwrap();
        assert_eq!(serde_json::from_str::<Value>(stored).unwrap(), deep_input());
        // However it is read back it says so first: the key sorts before
        // `content` and `role`.
        let found = store.search("running both", 1).unwrap();
        assert!(found[0].snippet.starts_with("{\"_archive_reduced\":\"this message alone"));
    }

    /// One message with three tool calls, sealed alone and read back.
    fn sealed_calls(store: &ArchiveStore, text: &str, inputs: [&Value; 3]) -> SharedMessage {
        let mut blocks = vec![json!({"type":"text","text":text})];
        for (index, input) in inputs.into_iter().enumerate() {
            let id = format!("t{}", index + 1);
            blocks.push(json!({"type":"tool_use","id":id,"name":"plot","input":input}));
        }
        let window = store.seal_window(&[msg("assistant", json!(blocks))], "").unwrap();
        assert_eq!(window.parts.len(), 1);
        assert_eq!(window.parts[0].reduced_messages, 1);
        store.fetch(&window.parts[0].archive.id, 0, 1).unwrap().remove(0)
    }

    #[test]
    fn inputs_with_too_many_nodes_together_are_all_stored_as_text() {
        let (_tmp, store) = setup();
        let store = store.with_redactor(Arc::new(str::to_owned));
        // 30,001 visited nodes each: every input fits by itself, the three
        // together do not. As three strings they are three nodes.
        let wide = json!(vec![0; 30_000]);
        let long = "exact words ".repeat(1000);
        let stored = sealed_calls(&store, &long, [&wide; 3]);
        assert_eq!(stored["content"][0]["text"], long);
        let as_text = serde_json::to_string(&wide).unwrap();
        for block in 1..=3 {
            assert_eq!(stored["content"][block]["input"], json!({"_archive_json":as_text}));
        }
        assert_eq!(
            stored[REDUCED_KEY],
            "this message alone is over the per-archive limit; it is stored with 3 tool input(s) \
             as JSON text, each marked in place"
        );
    }

    #[test]
    fn inputs_too_large_together_are_all_omitted_and_text_stays_exact() {
        let (_tmp, store) = setup();
        let store = store.with_redactor(Arc::new(str::to_owned));
        // 6 MiB each: every input fits by itself, the three together are
        // over the byte budget, as values and as text alike.
        let large = json!("x".repeat(6 * 1024 * 1024));
        let long = "exact words ".repeat(1000);
        let stored = sealed_calls(&store, &long, [&large; 3]);
        assert_eq!(stored["content"][0]["text"], long);
        let bytes = serde_json::to_string(&large).unwrap().len();
        let omitted = format!("[archive: tool input of {bytes} bytes omitted, {OVER_LIMIT}");
        for block in 1..=3 {
            assert_eq!(stored["content"][block]["input"], json!({"_archive_omitted":omitted}));
        }
        assert!(stored[REDUCED_KEY].as_str().unwrap().ends_with("with 3 omission(s), each marked in place"));
    }

    /// Text is tried for each input by itself. One that is over the byte
    /// budget as text too is left out; one the screen would not store is
    /// withheld whole, as it would be if it were stored exact.
    #[test]
    fn an_input_over_the_byte_budget_is_left_out_and_a_withheld_one_stays_withheld() {
        let (_tmp, store) = setup();
        let store = store.with_redactor(Arc::new(str::to_owned));
        let nested = |leaf: Value| (0..40).fold(leaf, |deep, _| json!([deep]));
        // Too deep and too large; too deep around a word the screen
        // withholds; too deep around an object marked as not to be stored.
        let large = nested(json!("x".repeat(MAX_INPUT_BYTES + 1)));
        let credential = nested(json!("the password is example-only"));
        let private = nested(json!({"private":true,"value":"kept out"}));
        let stored = sealed_calls(&store, "three calls", [&large, &credential, &private]);
        let bytes = serde_json::to_string(&large).unwrap().len();
        let omitted = format!("[archive: tool input of {bytes} bytes omitted, {OVER_LIMIT}");
        assert_eq!(stored["content"][1]["input"], json!({"_archive_omitted":omitted}));
        for block in [2, 3] {
            assert_eq!(stored["content"][block]["input"], json!({"_archive_withheld":true}));
        }
        assert!(stored[REDUCED_KEY].as_str().unwrap().ends_with("with 1 omission(s), each marked in place"));
        // And both in one message: one input left out, one as text.
        let deep = deep_input();
        let small = json!({"path":"src/lib.rs"});
        let stored = sealed_calls(&store, "three calls", [&large, &deep, &small]);
        assert_eq!(stored["content"][1]["input"], json!({"_archive_omitted":omitted}));
        let as_text = serde_json::to_string(&deep).unwrap();
        assert_eq!(stored["content"][2]["input"], json!({"_archive_json":as_text}));
        assert_eq!(stored["content"][3]["input"], small);
        assert_eq!(
            stored[REDUCED_KEY],
            "this message alone is over the per-archive limit; it is stored with 1 omission(s) \
             and 1 tool input(s) as JSON text, each marked in place"
        );
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
            {"type":"tool_use","id":"t1","name":"read","input":{"_archive_omitted":marker}},
            {"type":"tool_use","id":"t2","name":"read","input":{"_archive_json":"[1]"}}
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
        let (left, right) = messages.split_at(MAX_MESSAGES);
        let digest = |part: &[SharedMessage]| {
            let projection = project_hashed(&store.logical, part, "", None, Reduce::None);
            projection.unwrap().digest
        };
        let id = |part: &[SharedMessage]| digest(part)[..32].to_owned();
        // Something that is not a file sits where the second part belongs,
        // under the one id and under the other: no name is left for it.
        let shelf = dir.join("s000001");
        std::fs::create_dir(&shelf).unwrap();
        let obstacle = shelf.join(archive_name(&id(right)));
        let other = shelf.join(archive_name(&digest(right)[32..]));
        std::fs::create_dir(&obstacle).unwrap();
        std::fs::create_dir(&other).unwrap();
        let error = store.seal_window(&messages, "note").unwrap_err();
        assert!(!over_budget(&error));
        std::fs::remove_dir(&other).unwrap();
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
    fn a_part_does_not_end_between_a_call_and_its_results_where_it_can() {
        let call = msg("assistant", json!([{"type":"tool_use","id":"t","name":"read","input":{}}]));
        let result = msg("user", json!([{"type":"tool_result","tool_use_id":"t","content":"ok"}]));
        let words = msg("user", json!("words"));
        let window = [words.clone(), call.clone(), result.clone(), words.clone()];
        assert_eq!(before_results(&window, 0, 2), 1, "moved back to before the call");
        assert_eq!(before_results(&window, 0, 3), 3, "the next part starts with words");
        assert_eq!(before_results(&window, 1, 2), 2, "it cannot: the part would be empty");
        assert_eq!(before_results(&window, 0, 4), 4, "the end of the window");
        let results = [words.clone(), call.clone(), result.clone(), result.clone(), words.clone()];
        assert_eq!(before_results(&results, 0, 3), 1, "back over every result of the call");
        assert_eq!(before_results(&results, 1, 3), 3, "nothing but results back to the start");

        // Through a seal: the message limit falls between a call and its
        // result, and the part ends one message earlier.
        let (_tmp, store) = setup();
        let mut messages = numbered(0..MAX_MESSAGES + 5);
        messages[MAX_MESSAGES - 1] = call;
        messages[MAX_MESSAGES] = result;
        let window = store.seal_window(&messages, "").unwrap();
        let starts: Vec<_> = window.parts.iter().map(|part| part.first_source_index).collect();
        assert_eq!(starts, [0, MAX_MESSAGES - 1]);
    }

    /// Plain messages whose text does not say how many there are.
    fn numbered(range: std::ops::Range<usize>) -> Vec<SharedMessage> {
        range.map(|index| msg("user", json!(format!("message {index}")))).collect()
    }
    fn starts_and_ids(window: &SealedWindow) -> (Vec<usize>, Vec<String>) {
        let parts = window.parts.iter();
        parts.map(|part| (part.first_source_index, part.archive.id.clone())).unzip()
    }

    /// A part ends where the next message would not fit, so what is added
    /// to the end of a window changes its last part only. A retry after a
    /// failed turn finds one more message in the window: it writes one file.
    /// Cut at the midpoint of its length, every part was a new one.
    #[test]
    fn parts_do_not_change_when_the_window_grows() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let mut messages = numbered(0..2 * MAX_MESSAGES + 10);
        let (starts, first) = starts_and_ids(&store.seal_window(&messages, "note").unwrap());
        assert_eq!(starts, [0, MAX_MESSAGES, 2 * MAX_MESSAGES]);
        assert_eq!(shelf_files(&dir).len(), 3);

        // One more message, as after a failed turn.
        messages.extend(numbered(messages.len()..messages.len() + 1));
        let (starts, second) = starts_and_ids(&store.seal_window(&messages, "note").unwrap());
        assert_eq!(starts, [0, MAX_MESSAGES, 2 * MAX_MESSAGES]);
        assert_eq!(second[..2], first[..2]);
        assert_ne!(second[2], first[2]);
        assert_eq!(shelf_files(&dir).len(), 4, "one file more: the new last part");

        // Enough more to fill the last part and start another.
        messages.extend(numbered(messages.len()..3 * MAX_MESSAGES + 5));
        let window = store.seal_window(&messages, "note").unwrap();
        let (starts, third) = starts_and_ids(&window);
        assert_eq!(starts, [0, MAX_MESSAGES, 2 * MAX_MESSAGES, 3 * MAX_MESSAGES]);
        assert_eq!(third[..2], first[..2]);
        assert_eq!(shelf_files(&dir).len(), 6, "the part that is full now, and the last");
        assert_whole(&reassemble(&store, &window, messages.len()), &messages);
        assert_eq!(window.note, "note");
    }

    /// The same when a part was cut because of its size once written, which
    /// the fill cannot know: it is cut to a number of messages that does not
    /// depend on how many follow.
    #[test]
    fn a_part_cut_for_its_written_size_does_not_change_when_the_window_grows() {
        let (_tmp, store) = setup();
        // 768 KiB of control characters is 4.5 MiB once each is "\u0001":
        // three such messages fit one segment, four do not.
        let large = |index: usize| msg("user", json!(format!("{index}{}", "\u{1}".repeat(768 * 1024))));
        let mut cuts = Vec::new();
        // Halved, four were cut after two and six after three.
        for len in [4, 6] {
            let messages: Vec<_> = (0..len).map(large).collect();
            let window = store.seal_window(&messages, "").unwrap();
            let (starts, ids) = starts_and_ids(&window);
            assert_eq!(starts[..2], [0, 2], "{len} messages");
            cuts.push(ids[0].clone());
        }
        cuts.dedup();
        assert_eq!(cuts.len(), 1, "the first part is the same segment every time");
    }

    /// A message that is over a limit by itself costs the window two parts:
    /// the one that ends before it and itself. Halving cost one part for
    /// each time the range around it was halved.
    #[test]
    fn an_oversized_message_costs_two_parts_more() {
        let (_tmp, store) = setup();
        let store = store.with_redactor(Arc::new(str::to_owned));
        let deep = || msg("assistant", json!([{"type":"tool_use","id":"t1","name":"nest","input":deep_input()}]));
        let cut = |len: usize, oversized: &[usize]| {
            let mut messages = numbered(0..len);
            for &index in oversized {
                messages[index] = deep();
            }
            let window = store.seal_window(&messages, "").unwrap();
            let parts = window.parts.iter();
            parts.map(|part| (part.first_source_index, part.reduced_messages)).collect::<Vec<_>>()
        };
        // One at index 7 of 4,106: thirteen parts when the window was halved.
        let after = 8 + MAX_MESSAGES;
        assert_eq!(cut(MAX_MESSAGES + 10, &[7]), [(0, 0), (7, 1), (8, 0), (after, 0)]);
        // Three, evenly, in 4,000: about 34 then.
        let seven = [(0, 0), (1000, 1), (1001, 0), (2000, 1), (2001, 0), (3000, 1), (3001, 0)];
        assert_eq!(cut(4000, &[1000, 2000, 3000]), seven);
        // First, last and next to each other: no part is empty.
        assert_eq!(cut(4, &[0, 1, 3]), [(0, 1), (1, 1), (2, 0), (3, 1)]);
    }

    fn now_ms() -> u64 {
        let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
        since.unwrap().as_millis() as u64
    }
    fn json_of(path: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }
    fn top_level(record: &Value) -> Vec<&str> {
        record.as_object().unwrap().keys().map(String::as_str).collect()
    }

    /// A seal writes when it wrote. The time is not part of the digest, so
    /// the same content sealed later is found, and what is stored keeps the
    /// time it was first written with.
    #[test]
    fn a_seal_records_the_time_of_the_write_and_a_later_seal_leaves_it() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let messages = text("sealed once");
        let before = now_ms();
        let sealed = store.seal(&messages, "note").unwrap();
        let after = now_ms();
        let file = only_archive_file(&dir);
        let record = json_of(&file);
        // serde_json keeps the keys of a map sorted.
        let fields = ["body", "created_ms", "digest", "id", "logical", "scope", "version"];
        assert_eq!(top_level(&record), fields);
        let created = record["created_ms"].as_u64().unwrap();
        assert!((before..=after).contains(&created), "{before} <= {created} <= {after}");
        // In the file it follows the digest and comes before the body.
        let raw = std::fs::read_to_string(&file).unwrap();
        let digest = record["digest"].as_str().unwrap();
        let expected = format!("\"digest\":\"{digest}\",\"created_ms\":{created},\"body\":{{");
        assert!(raw.contains(&expected));

        // The digest is what it was before the field existed: the content's.
        let projection = project_archive("synthetic-logical", &messages, "note", None).unwrap();
        assert_eq!(record["digest"], projection.digest.as_str());
        assert_eq!(sealed.id, projection.digest[..32]);

        // Sealed again, later: the same segment, and not a byte of it changed.
        let snapshot_before = snapshot(&dir);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(store.seal(&messages, "another note").unwrap(), sealed);
        assert_eq!(store.seal_window(&messages, "").unwrap().parts[0].archive, sealed);
        assert_eq!(snapshot(&dir), snapshot_before);
        assert_eq!(json_of(&file)["created_ms"], created);
    }

    /// `seal_projection` does not trust the projection it is handed: one of
    /// another chat, or with a digest that is not its content's, is refused,
    /// no digest panics, and a refusal writes nothing.
    #[test]
    fn a_projection_of_another_chat_or_with_a_wrong_digest_is_refused_and_nothing_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        // Every path under the base, directories too, and every file's bytes.
        let state = || {
            let mut paths = Vec::new();
            let mut pending = vec![tmp.path().to_path_buf()];
            while let Some(directory) = pending.pop() {
                for entry in std::fs::read_dir(&directory).unwrap() {
                    let path = entry.unwrap().path();
                    if std::fs::symlink_metadata(&path).unwrap().is_dir() {
                        pending.push(path.clone());
                    }
                    paths.push(path);
                }
            }
            paths.sort();
            (paths, snapshot(tmp.path()))
        };
        let store = setup_in(tmp.path(), "synthetic-project");
        let words = "handed to the store";
        let project = |logical: &str, words: &str| {
            project_hashed(logical, &text(words), "", None, Reduce::None).unwrap()
        };
        let good = || project(&store.logical, words);
        let with_digest = |digest: &str| ArchiveProjection {
            digest: digest.to_owned(),
            ..good()
        };
        let other_chat = digest_parts(&[b"another-chat"]);
        assert_ne!(other_chat, store.logical);
        let half_way = format!("{}é{}", "a".repeat(31), "a".repeat(31));
        assert!(half_way.len() == 64 && !half_way.is_char_boundary(32));
        let refused = [
            // Whole and consistent, of another chat.
            ("another chat", project(&other_chat, words)),
            // This chat's name on another chat's content and digest.
            ("renamed", ArchiveProjection {
                logical_id: store.logical.clone(),
                ..project(&other_chat, words)
            }),
            ("another digest", with_digest(&project(&store.logical, "other words").digest)),
            ("other messages", ArchiveProjection {
                messages: project(&store.logical, "other words").messages,
                ..good()
            }),
            ("another count", ArchiveProjection {
                source_message_count: 2,
                ..good()
            }),
            ("upper case", with_digest(&good().digest.to_uppercase())),
            ("ten characters", with_digest("0123456789")),
            ("empty", with_digest("")),
            ("not hex", with_digest(&"z".repeat(64))),
            ("not ASCII", with_digest(&"é".repeat(32))),
            ("not ASCII, cut at the half", with_digest(&half_way)),
        ];
        // On a store that has written nothing yet, then on one that has.
        for sealed_before in [false, true] {
            if sealed_before {
                store.seal(&text("a shelf to begin with"), "").unwrap();
            }
            let before = state();
            for (what, projection) in &refused {
                for written in [None, Some(Written { created_ms: Some(1), modified: None })] {
                    let error = store.seal_projection(projection.clone(), written).unwrap_err();
                    assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{what}: {error}");
                    assert_eq!(state(), before, "{what}");
                }
            }
        }
        // The projection as it was made is stored, as a seal stores it.
        let (sealed, _) = store.seal_projection(good(), None).unwrap();
        assert_eq!(sealed.id, good().digest[..32]);
        assert_eq!(store.fetch(&sealed.id, 0, 1).unwrap(), text(words));
        assert_eq!(store.seal(&text(words), "").unwrap(), sealed);
    }

    /// What an import gives is what is stored: the record's `created_ms` and
    /// the time of its file, set before the file has its name. No directory
    /// gets that time, and content that is already stored is not touched.
    #[test]
    fn a_supplied_created_ms_and_file_time_are_stored_as_given() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let project = |words: &str| {
            project_hashed(&store.logical, &text(words), "", None, Reduce::None).unwrap()
        };
        // 9 September 2001, and a file time that is not the same instant.
        let created_ms = 1_000_000_000_123;
        let modified = std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_500);
        let given = Written {
            created_ms: Some(created_ms),
            modified: Some(modified),
        };
        let started = std::time::SystemTime::now() - Duration::from_secs(5);
        let (sealed, _) = store.seal_projection(project("from another store"), Some(given)).unwrap();
        let file = dir.join("s000001").join(archive_name(&sealed.id));
        // Before the file is read here, which may move its time of access.
        let metadata = std::fs::metadata(&file).unwrap();
        assert_eq!(metadata.modified().unwrap(), modified);
        assert_eq!(metadata.accessed().unwrap(), modified);
        assert_eq!(json_of(&file)["created_ms"], created_ms);
        for directory in [dir.clone(), dir.join("s000001")] {
            let changed = std::fs::metadata(&directory).unwrap().modified().unwrap();
            assert!(changed >= started, "{directory:?} was given the time of a file");
        }
        assert_eq!(store.fetch(&sealed.id, 0, 1).unwrap(), text("from another store"));

        // The same content with another time: found, and left as it is.
        let other = Written {
            created_ms: Some(5),
            modified: None,
        };
        let before = snapshot(&dir);
        let again = store.seal_projection(project("from another store"), Some(other)).unwrap();
        assert_eq!(again.0, sealed);
        assert_eq!(store.seal(&text("from another store"), "").unwrap(), sealed);
        assert_eq!(snapshot(&dir), before);
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), modified);

        // A record that never had a time is stored without one, not with
        // the time of the import; its file has the time of the write.
        let undated = Written {
            created_ms: None,
            modified: None,
        };
        let (plain, _) = store.seal_projection(project("never had a time"), Some(undated)).unwrap();
        let file = dir.join("s000001").join(archive_name(&plain.id));
        let fields = ["body", "digest", "id", "logical", "scope", "version"];
        assert_eq!(top_level(&json_of(&file)), fields);
        assert!(std::fs::metadata(&file).unwrap().modified().unwrap() >= started);
        assert_eq!(store.fetch(&plain.id, 0, 1).unwrap(), text("never had a time"));
    }

    /// `created_ms` is the one field a version 1 record may have or not
    /// have. With it or without it the record is read; with anything else
    /// at the top level, or with a `created_ms` that is not a whole number
    /// of milliseconds, it is not a record.
    #[test]
    fn created_ms_is_optional_and_no_other_field_is_accepted() {
        let (tmp, store) = setup();
        let shelf = directory(&tmp, &store).join("s000001");
        std::fs::create_dir(&shelf).unwrap();
        let id = "a".repeat(32);
        let file = shelf.join(archive_name(&id));
        let words = text("with or without");
        let without = record_bytes(&store, &id, &words, |_| {});
        assert!(!String::from_utf8_lossy(&without).contains("created_ms"));
        let with = |value: Value| {
            let mut record: Value = serde_json::from_slice(&without).unwrap();
            record["created_ms"] = value;
            serde_json::to_vec(&record).unwrap()
        };
        for valid in [without.clone(), with(json!(0)), with(json!(1_757_000_000_000u64)), with(json!(u64::MAX))] {
            std::fs::write(&file, valid).unwrap();
            assert_eq!(store.fetch(&id, 0, 1).unwrap(), words);
            assert_eq!(store.search("without", 4).unwrap().len(), 1);
        }
        let mut extra: Value = serde_json::from_slice(&without).unwrap();
        extra["created_at"] = json!(5);
        let mut nested: Value = serde_json::from_slice(&without).unwrap();
        nested["body"]["created_ms"] = json!(5);
        let invalid = [
            with(Value::Null),
            with(json!(-1)),
            with(json!(1.5)),
            with(json!("1757000000000")),
            with(json!(true)),
            serde_json::to_vec(&extra).unwrap(),
            serde_json::to_vec(&nested).unwrap(),
        ];
        for (index, bytes) in invalid.into_iter().enumerate() {
            std::fs::write(&file, bytes).unwrap();
            let error = store.fetch(&id, 0, 1).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "case {index}: {error}");
        }
    }

    /// A forget leaves no time behind: the tombstone keeps what names the
    /// record and nothing else, whatever the live record had.
    #[test]
    fn a_tombstone_carries_no_created_ms() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let sealed = store.seal(&text("to be forgotten"), "").unwrap();
        let file = only_archive_file(&dir);
        assert!(json_of(&file)["created_ms"].is_u64());
        store.forget(&sealed.id).unwrap();
        let tombstone = json_of(&file);
        let fields = ["body", "digest", "id", "logical", "scope", "version"];
        assert_eq!(top_level(&tombstone), fields);
        assert_eq!(tombstone["body"], json!({"state":"tombstone"}));

        // Nor does the tombstone a forget adds for a copy that was stored
        // under no name of its digest: here, a legacy slot.
        let slot = seal_legacy_slot(&store, 0, &text("forgotten in a slot"), "");
        store.forget(&slot.id).unwrap();
        for path in shelf_files(&dir).into_iter().chain([dir.join(slot_name(0))]) {
            let record = json_of(&path);
            assert_eq!(top_level(&record), fields, "{path:?}");
            assert_eq!(record["body"]["state"], "tombstone");
        }
        assert_eq!(shelf_files(&dir).len(), 2);
    }

    /// A tombstone that has `created_ms` is not a record: nothing is read
    /// from it. It still says that its content was forgotten, as any file
    /// that names the digest with the state `tombstone` does.
    #[test]
    fn a_tombstone_with_created_ms_is_not_a_record_and_still_blocks_a_seal() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let words = text("forgotten somewhere else");
        let digest = project_hashed(&store.logical, &words, "", None, Reduce::None).unwrap().digest;
        let id = &digest[..32];
        let shelf = dir.join("s000001");
        std::fs::create_dir(&shelf).unwrap();
        let file = shelf.join(archive_name(id));
        let dated = |created_ms| {
            record_bytes(&store, id, &words, |record| {
                record.body = Body::Tombstone;
                record.created_ms = created_ms;
            })
        };
        // Without the field it is the tombstone a forget writes.
        std::fs::write(&file, dated(None)).unwrap();
        assert_eq!(store.fetch(id, 0, 1).unwrap_err().kind(), io::ErrorKind::NotFound);
        // With it, it is not a record.
        let invalid = dated(Some(1_757_000_000_000));
        std::fs::write(&file, &invalid).unwrap();
        assert_eq!(store.fetch(id, 0, 1).unwrap_err().kind(), io::ErrorKind::InvalidData);
        let error = store.seal(&words, "").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
        assert_eq!(shelf_files(&dir), std::slice::from_ref(&file));
        assert_eq!(std::fs::read(&file).unwrap(), invalid);
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
    /// either side of it hold. So does a read of the segment in slot 0. A
    /// read of a segment that is found nowhere says that slot 1 could not be
    /// read (and how: `kind`), since it may be the one. Only the export,
    /// which reads the legacy slots as a whole, fails as it always did.
    fn assert_slot_1_is_passed_over(
        tmp: &tempfile::TempDir,
        store: &ArchiveStore,
        live: &ArchiveRef,
        words: &str,
        kind: io::ErrorKind,
    ) {
        let new = store.seal(&text(words), "").unwrap();
        assert_eq!(store.fetch(&new.id, 0, 1).unwrap(), text(words), "{words}");
        let again = store.seal_with_note(&text("legacy live"), "second note");
        assert_eq!(again.unwrap(), (live.clone(), "first note".to_owned()), "{words}");
        let forgotten = store.seal(&text("legacy forgotten"), "").unwrap_err();
        assert!(forgotten.to_string().contains("was forgotten"), "{words}");

        assert_eq!(store.fetch(&live.id, 0, 1).unwrap(), text("legacy live"), "{words}");
        assert_eq!(store.fetch_note(&live.id).unwrap(), "first note", "{words}");
        let slot = directory(tmp, store).join("000.json");
        let located = archive_file(tmp.path(), "synthetic-project", &live.id);
        assert_eq!(located.unwrap(), Some(slot), "{words}");
        let rows = store.search("legacy", 2).unwrap();
        assert_eq!(rows.len(), 1, "{words}");
        assert_eq!(rows[0].id, live.id, "{words}");

        let missing = "a".repeat(32);
        for result in [
            store.fetch(&missing, 0, 1).map(drop),
            store.fetch_note(&missing).map(drop),
            store.forget(&missing).map(drop),
            archive_file(tmp.path(), "synthetic-project", &missing).map(drop),
        ] {
            let error = result.unwrap_err();
            assert_eq!(error.kind(), kind, "{words}: {error}");
            assert!(error.to_string().contains("first 001.json"), "{words}: {error}");
            assert!(error.to_string().contains("could not be read"), "{words}: {error}");
        }
        assert!(store.export_records().is_err(), "{words}");
        assert!(export_in(tmp.path(), "synthetic-project").is_err(), "{words}");
    }
    /// A forget of the healthy segment in slot 0 works beside slot 1 too.
    /// It replaces slot 0, adds the tombstone a seal looks for on the shelf
    /// and changes nothing else. Slot 1 may hold a copy of the same window
    /// and could not be read, which the forget says. (Slot 1 itself, which
    /// may be a directory or a FIFO, is for the caller to look at.)
    fn assert_forgets_beside_slot_1(tmp: &tempfile::TempDir, store: &ArchiveStore, live: &ArchiveRef) {
        let dir = directory(tmp, store);
        let slot = dir.join("000.json");
        let guard = dir.join("s000001").join(archive_name(&digest_of("legacy live")[..32]));
        let others = || -> Vec<_> {
            let mut files = shelf_files(&dir);
            files.retain(|file| *file != guard);
            files.push(dir.join("002.json"));
            files.into_iter().map(|path| (identity(&path), path)).collect()
        };
        let (before, entries) = (others(), std::fs::read_dir(&dir).unwrap().count());
        let live_file = identity(&slot);
        assert!(!guard.exists());
        let done = store.forget(&live.id).unwrap();
        assert_eq!(done.ids, std::slice::from_ref(&live.id));
        let unread = done.unread.unwrap();
        assert!(unread.starts_with("1 place(s) could not be read, first 001.json: "), "{unread}");
        assert_ne!(identity(&slot), live_file);
        let tombstone = (identity(&slot), identity(&guard));
        assert!(store.forget(&live.id).unwrap().ids.is_empty());
        assert_eq!((identity(&slot), identity(&guard)), tombstone, "forgotten twice, written once");
        assert!(std::fs::read_to_string(&slot).unwrap().contains("tombstone"));
        assert!(is_tombstone_of(store, &guard, &digest_of("legacy live")));
        // Found, and forgotten: plainly "not found", with nothing to add.
        let error = store.fetch(&live.id, 0, 1).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(error.to_string(), not_found().to_string());
        let refused = store.seal(&text("legacy live"), "").unwrap_err();
        assert!(refused.to_string().contains("was forgotten"));
        assert_eq!(others(), before);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), entries);
        assert!(temporary_files(&dir).is_empty());
    }

    #[test]
    fn a_legacy_slot_that_is_not_a_valid_record_hides_only_itself() {
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
        let invalid = io::ErrorKind::InvalidData;
        for (kind, bytes) in cases {
            std::fs::write(&slot, &bytes).unwrap();
            assert_slot_1_is_passed_over(&tmp, &store, &live, kind, invalid);
            assert_eq!(std::fs::read(&slot).unwrap(), bytes, "{kind}");
        }
        // Over the size of a segment: refused by its length, never read.
        let large = MAX_SEGMENT_BYTES as u64 + 1;
        std::fs::File::create(&slot).unwrap().set_len(large).unwrap();
        assert_slot_1_is_passed_over(&tmp, &store, &live, "over the size bound", invalid);
        assert_eq!(std::fs::metadata(&slot).unwrap().len(), large);
        // Five seals, five new files beside the tombstone that the forget
        // of slot 2 left on the shelf, and no other change to the directory.
        assert_eq!(shelf_files(&dir).len(), 6);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 5, "lock, 3 slots, 1 shelf");
        assert_forgets_beside_slot_1(&tmp, &store, &live);
        assert_eq!(std::fs::metadata(&slot).unwrap().len(), large);
    }

    #[test]
    fn a_legacy_slot_that_is_not_a_private_file_hides_only_itself() {
        use std::os::unix::fs::FileTypeExt;
        let (tmp, store, live) = legacy_store();
        let slot = directory(&tmp, &store).join("001.json");
        let kind = || std::fs::symlink_metadata(&slot).unwrap().file_type();
        let victim = tmp.path().join("synthetic-victim");
        std::fs::write(&victim, "do not change").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
        // Nothing was read into, written through or made private by way of
        // the slot.
        let passed_over = |words: &str, error: io::ErrorKind| {
            assert_slot_1_is_passed_over(&tmp, &store, &live, words, error);
            assert_eq!(std::fs::read_to_string(&victim).unwrap(), "do not change");
            assert_eq!(mode(&victim), 0o644, "{words}");
        };
        let denied = io::ErrorKind::PermissionDenied;
        symlink(&victim, &slot).unwrap();
        // The kind of the error that refuses a link has no stable name.
        let looped = store.fs.read_with("001.json", MAX_SEGMENT_BYTES, true).unwrap_err().kind();
        passed_over("past a symlink", looped);
        assert!(kind().is_symlink());
        std::fs::remove_file(&slot).unwrap();
        std::fs::hard_link(&victim, &slot).unwrap();
        passed_over("past a hard link", denied);
        std::fs::remove_file(&slot).unwrap();
        let fifo = std::ffi::CString::new(slot.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        passed_over("past a FIFO", denied);
        assert!(kind().is_fifo());
        std::fs::remove_file(&slot).unwrap();
        std::fs::create_dir(&slot).unwrap();
        passed_over("past a directory", denied);
        assert!(kind().is_dir());
        assert_forgets_beside_slot_1(&tmp, &store, &live);
        assert!(kind().is_dir());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "do not change");
    }

    /// A slot that was forgotten by a build from before a forget left a
    /// tombstone on a shelf has only itself to say so.
    #[test]
    fn a_legacy_tombstone_that_cannot_be_read_no_longer_forbids_its_projection() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let slot = dir.join("000.json");
        let gone = seal_legacy_slot(&store, 0, &text("forgotten words"), "");
        store.forget(&gone.id).unwrap();
        let refused = store.seal(&text("forgotten words"), "").unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        // With the tombstone torn, the one on the shelf still says that
        // this projection was forgotten.
        let tombstone = std::fs::read(&slot).unwrap();
        let torn = &tombstone[..tombstone.len() / 2];
        std::fs::write(&slot, torn).unwrap();
        let refused = store.seal(&text("forgotten words"), "").unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        // Without that one, the price of passing over what cannot be read:
        // nothing says so any more.
        std::fs::remove_file(only_archive_file(&dir)).unwrap();
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
            store.forget(&reference.id).map(drop),
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
        assert_busy(store.forget(&reference.id).map(drop));
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
        assert_busy(store.forget(&reference.id).map(drop));
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
        assert_busy(reader.forget(&reference.id).map(drop));
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
                            let seal = |round| {
                                let window = window(format!("{name} window {round};"));
                                store.store(project(&window, &note), Written::now())
                            };
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

    /// Seals from several threads, each through its own store, the gate and
    /// the lock, into shelves that fill up while they run. Whoever crosses
    /// from one shelf to the next, each shelf is filled exactly and every
    /// window is stored once.
    #[test]
    fn concurrent_seals_across_shelf_boundaries_fill_each_shelf_exactly() {
        const WRITERS: usize = 4;
        const EACH: usize = 80;
        let (tmp, store) = setup();
        let window = |writer: usize, round: usize| {
            [msg("user", json!(format!("writer {writer} window {round};")))]
        };
        let start = std::sync::Barrier::new(WRITERS);
        let sealed: Vec<Vec<String>> = std::thread::scope(|scope| {
            let writers: Vec<_> = (0..WRITERS)
                .map(|writer| {
                    let (tmp, start) = (&tmp, &start);
                    scope.spawn(move || {
                        let name = format!("writer-{writer}");
                        let store = ArchiveStore::new(tmp.path(), "synthetic-project", &name);
                        let store = store.unwrap();
                        start.wait();
                        let seal = |round| store.seal(&window(writer, round), "note").unwrap().id;
                        (0..EACH).map(seal).collect()
                    })
                })
                .collect();
            writers.into_iter().map(|writer| writer.join().unwrap()).collect()
        });
        assert_eq!(store.shelves().unwrap(), ["s000003", "s000002", "s000001"]);
        let project = tmp.path().join("context-archives").join(&store.scope);
        let held = |shelf: &str| std::fs::read_dir(project.join(shelf)).unwrap().count();
        let rest = WRITERS * EACH - 2 * SHELF_FILES;
        assert_eq!(
            (held("s000001"), held("s000002"), held("s000003")),
            (SHELF_FILES, SHELF_FILES, rest)
        );
        for (writer, ids) in sealed.iter().enumerate() {
            for (round, id) in ids.iter().enumerate() {
                assert_eq!(store.fetch(id, 0, 2).unwrap(), window(writer, round));
            }
        }
        let mut names = shelved(&tmp, &store);
        names.sort();
        names.dedup();
        assert_eq!(names.len(), WRITERS * EACH);
        assert!(names.iter().all(|name| archive_id(name).is_some()));
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
        // Together they seal more than a shelf holds (200), so they cross
        // from the first shelf to the second. Whichever of them did, the
        // first is exactly full, the rest is in the second, and neither left
        // a temporary file.
        let held = |shelf: &str| std::fs::read_dir(project.join(shelf)).unwrap().count();
        assert_eq!(
            (held("s000001"), held("s000002")),
            (SHELF_FILES, 2 * CHILD_SEALS - SHELF_FILES)
        );
        assert_eq!(store.shelves().unwrap().len(), 2);
        // Every window of both, under the id its content gives it.
        for child in &children {
            let name = format!("child-{}", child.id());
            for round in 0..CHILD_SEALS {
                let window = [msg("user", json!(format!("{name} round {round};")))];
                let projection = project_archive(&name, &window, "", None).unwrap();
                let found = store.fetch(&projection.digest[..32], 0, 2);
                assert_eq!(found.unwrap(), window, "{name} round {round}");
            }
            for round in [0, CHILD_SEALS - 1] {
                let words = format!("{name} round {round};");
                assert_eq!(store.search(&words, 2).unwrap().len(), 1, "{words}");
            }
        }
    }
}
