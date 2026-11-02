//! Process-wide table of Anthropic/OAuth usage reads, one entry per account
//! slot, shared by every [`LocalBroker`](super::LocalBroker) in the process
//! (a daemon can hold several brokers, one per runtime `TokenCache`).
//!
//! * **Single-flight.** One network read per slot at a time; a reader that
//!   waited for a read in flight takes that read's result (success or
//!   failure) and sends nothing. The per-slot gate is never removed, so
//!   invalidating a cached reading can never open a second flight.
//! * **Negative cache.** A transient failure (usage-endpoint 429, 5xx,
//!   timeout, transport) holds the slot: reads inside the hold return the
//!   same failure with no request. Holds back off 2, 4, 8, then 10 minutes;
//!   a longer `Retry-After` wins, capped at 10 minutes. A success clears the
//!   hold. Credential rejections are never held (their path is unchanged).
//! * **Last good.** The slot's last successful snapshot and the seat it was
//!   read from, which automatic selection may use in place of a read that
//!   failed transiently (see [`USAGE_READ_GRACE`],
//!   [`UsageReads::last_good_for`] and `quota_policy::SelectionRequest::grace`).
//! * **A drop wins over a read in flight.** Every drop of a slot's reading
//!   (a reported limit, a refused login) moves the slot's epoch
//!   ([`UsageReads::epoch`]). A good reading whose read was admitted at an
//!   older epoch is discarded when it lands ([`ReadError::Superseded`]): not
//!   used, kept, saved or cached, and a reader that joined the read gets the
//!   same. A broker's cached copy of a reading carries the epoch it was
//!   cached at and is not reused once the epoch moved, whichever broker of
//!   the process dropped it; nor is a selection's own copy (a reading it
//!   took or reused, or a last good one it offered to grace): the selection
//!   checks the epoch right before it reuses a copy or takes a re-read
//!   queued for it, after its reads, and before it returns, and an account
//!   whose epoch moved counts as failed for the rest of that selection: a
//!   re-read still queued for it when the move is seen is dropped unsent,
//!   and one already going out can complete but can't make the account
//!   eligible in that selection.
//!   Removing an account is not one of these drops: its entry is pruned at
//!   the writer's next selection (see **Limits** below).
//! * **One pause for every account.** A usage 429 on one slot pauses every
//!   read in its [`ReadScope::pause`] (one provider's usage endpoint for one
//!   credential store) until the 429's hold ends: the other accounts are not
//!   read one after another into the same limit. Reads inside the pause
//!   return the 429 with no request, and so does a read that passed the
//!   check but is about to go out after its token refresh
//!   ([`Fetched::Paused`]). Anthropic only.
//! * **Saved readings.** Anthropic only ([`ReadScope::persisted`]). Each
//!   slot's last good reading and each active pause are saved, best effort,
//!   to [`SAVED_READINGS_FILE`] beside the credential store, and loaded once
//!   per process on the first use of that store: a restart keeps them.
//!   - **One writer per file.** A table writes a saved readings file only
//!     while it holds that file's lease: an exclusive lock on
//!     [`SAVED_READINGS_LOCK`] beside it, taken without waiting by
//!     [`save_readings_in_this_process`] (the daemon, at its start) before
//!     the table loads the file, and held under the table's save lock until
//!     [`stop_saving_readings`] (the daemon's shutdown; a save in flight
//!     ends first) or the process ends or execs (the lock is closed on
//!     exec; a child forked without exec shares it until it exits or
//!     execs). The writer writes its own table: the slots it holds a reading
//!     for, never a merge with the file. So a reading it dropped (a reported
//!     limit, a refused login) is gone from the file and can't come back
//!     through another process, and no other process erases a pause it
//!     saved. Every other process (another daemon profile on the same store,
//!     `synaps status`, a TUI, tests by default) only loads the file. A
//!     pause therefore flows from the writer to clients that load after it;
//!     a client's own 429 does not pause a running daemon.
//!   - **Loaded before use.** The first load holds the table's load lock
//!     through the read and the install: a read that starts meanwhile waits
//!     for it, so no read of a reload's first turns goes out on a slot whose
//!     saved reading or pause was still loading.
//!   - **What loads.** A loaded reading keeps its original `observed_at`; it
//!     stands for a read (the caller's reuse age and the grace count from
//!     that time) and never for eligibility. Only a reading bound to an
//!     account fingerprint is saved or loaded (a login without an account id
//!     gets no saved reading). A file over [`SAVED_READINGS_MAX_BYTES`] or
//!     of another version, a reading observed after the load time or
//!     malformed, or a pause longer than [`NEGATIVE_BACKOFF_CAP`], is
//!     ignored.
//!   - **What is written.** Numbers, times, the slot keys and the account
//!     fingerprint a reading is bound to: no token, account id or credential
//!     digest. Written (0600, a new file renamed over the old) only on a new
//!     good reading, a new pause or a dropped reading, never on a cache hit,
//!     synchronously on the task that made the change, with no lock held
//!     across an await (ordering matters more than the few kilobytes).
//!   - **Limits.** Pauses and drops reach the file only from the lease
//!     owner. A daemon that starts while another process holds the lease
//!     (another profile's daemon on the same store) stays a reader until
//!     its next start, and another profile's daemon that starts inside a
//!     reload's exec gap can take the lease. After a rollback
//!     to a binary without this file and an upgrade within the grace window,
//!     a reading the old binary dropped can be reused once (bounded by the
//!     grace; a message-path 429 still cools the account). Removing an
//!     account does not drop its reading: the entry is pruned at the
//!     writer's next selection (which loads the file first). Until that
//!     selection the reading can be written back to the file, by a direct
//!     usage read at any time or by a first read in flight across a prune (a
//!     result that lands after a prune that already ran stays in the file
//!     until a later selection of the writer), and a re-add of the same
//!     account before then can be served it within the normal age and grace
//!     limits. It stays bound to that account's fingerprint (another account
//!     in the slot never gets it) and to its own `observed_at`, and it never
//!     brings back a refused or limited reading or erases a pause: those
//!     drops still win as above. The loaded guard keys on the file's path
//!     as given: a process enables and loads one file through one spelling
//!     of its path (every caller takes it from the credential store's
//!     resolver).
//!
//! Keys are the stable slot identity (credential file + storage key + usage
//! endpoint), never the seat digest, which changes on token rotation. Last
//! good is bound to the account it was read from: when the credential now in
//! the slot carries an account id, the reading counts only for that same id
//! (a refresh rotation keeps it; a re-login to another account drops it).
//! Credentials without an account id (an Anthropic login whose profile
//! lookup failed, or one made before ids were recorded) cannot tell a
//! rotation from a re-login, so their reading counts only for the exact
//! credential it was read with: a rotation loses grace until the next good
//! read, and a re-login to another account is never graced on the previous
//! occupant's reading.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::account::SeatIdentity;
use super::broker::BrokerError;
use super::quota_policy::ObservationFailure;
use super::usage::{UsageError, UsageSnapshot};

/// First hold after a transient usage-read failure.
pub const NEGATIVE_BACKOFF_START: Duration = Duration::from_secs(2 * 60);
/// Longest hold, including one asked for by `Retry-After`.
pub const NEGATIVE_BACKOFF_CAP: Duration = Duration::from_secs(10 * 60);
/// A last successful reading younger than this may stand in for a read that
/// failed transiently. Past it, only a reading that shows the account's paid
/// extra usage off may, and only for a usage 429 (see
/// `quota_policy::SelectionRequest::grace`). Never for an auth rejection or
/// an active cooldown; a window it shows used up until a reset still ahead
/// keeps the account out until then.
pub const USAGE_READ_GRACE: Duration = Duration::from_secs(30 * 60);
/// Tracing target of the per-read INFO line (passes the daemon log filter).
pub const USAGE_READ_TARGET: &str = "agent_core::auth::usage_read";
/// File name of the saved readings, in the credential store's directory.
pub const SAVED_READINGS_FILE: &str = "usage-last-good.json";
/// Version of the saved readings' format; a file with another one is
/// ignored (and replaced at the next save).
pub const SAVED_READINGS_VERSION: u32 = 1;
/// A saved readings file larger than this is not loaded (a size bound on
/// the load; seven accounts take about 4 KiB).
pub const SAVED_READINGS_MAX_BYTES: u64 = 64 * 1024;
/// File name of the saved readings' lease, beside [`SAVED_READINGS_FILE`]:
/// the one table that holds an exclusive lock on it writes that file. Never
/// deleted.
pub const SAVED_READINGS_LOCK: &str = "usage-last-good.lock";

/// A usage read that failed for a reason that says nothing about the
/// account: rate limit, server error, timeout or transport. Secret-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransientReadFailure {
    /// The usage error kind (`rate_limited`, `upstream_status`, `timeout`,
    /// `transport`).
    pub kind: &'static str,
    pub status: Option<u16>,
    pub retry_after_s: Option<u64>,
    /// The failed read itself, so a held read reports exactly what the
    /// network read did (broker error text and typed cause).
    error: UsageError,
}

impl TransientReadFailure {
    /// `Some` only for the failures the negative cache holds.
    pub fn from_usage_error(e: &UsageError) -> Option<Self> {
        let (status, retry_after_s) = match e {
            UsageError::RateLimited { retry_after_s } => (Some(429), *retry_after_s),
            UsageError::UpstreamStatus {
                status,
                retry_after_s,
            } if *status >= 500 => (Some(*status), *retry_after_s),
            UsageError::Timeout | UsageError::Transport { .. } => (None, None),
            _ => return None,
        };
        Some(Self {
            kind: e.kind(),
            status,
            retry_after_s,
            error: e.clone(),
        })
    }

    pub fn is_rate_limited(&self) -> bool {
        self.status == Some(429)
    }

    /// The broker error callers saw for this failure before the table.
    pub fn to_broker_error(&self) -> BrokerError {
        self.error.clone().into_broker_error()
    }

    /// The typed cause the selection report keeps for this failure.
    pub fn cause(&self) -> ObservationFailure {
        self.error.failure()
    }
}

/// What one network read produced, before the table classifies it.
// One value per network read, moved straight into the table: boxing the
// snapshot would buy nothing.
#[allow(clippy::large_enum_variant)]
pub enum Fetched {
    Ok(UsageSnapshot, SeatIdentity),
    /// The usage request itself failed.
    Usage(UsageError),
    /// The read failed before a usage request (slot check, token refresh),
    /// with its typed cause.
    Failed {
        error: BrokerError,
        cause: ObservationFailure,
    },
    /// No request: the read's scope was paused (another slot's usage 429)
    /// when the read was about to go out, after its token refresh. Not a
    /// read of the slot: nothing is recorded.
    Paused {
        failure: TransientReadFailure,
        retry_at_ms: u64,
    },
}

#[derive(Debug, Clone)]
pub enum ReadError {
    /// Held: no request for this slot before `retry_at_ms` (epoch ms).
    Transient {
        failure: TransientReadFailure,
        retry_at_ms: u64,
    },
    Other {
        error: BrokerError,
        cause: ObservationFailure,
    },
    /// A good reading that came back after the slot's reading was dropped
    /// (a reported limit, a refused login) while it was in flight: not
    /// used, kept or saved. Not a usage 429 (no pause), held by nothing (no
    /// hold, no cooldown); its cause is [`AccountFailureKind::QuotaUnknown`]
    /// (no reading), which is not transient, so it is never graced either.
    ///
    /// [`AccountFailureKind::QuotaUnknown`]: super::quota_policy::AccountFailureKind::QuotaUnknown
    Superseded,
}

/// The text of a [`ReadError::Superseded`] read.
pub const SUPERSEDED_READ: &str =
    "usage reading superseded: the account's reading was dropped while it was read";

impl ReadError {
    pub fn to_broker_error(&self) -> BrokerError {
        match self {
            Self::Transient { failure, .. } => failure.to_broker_error(),
            Self::Other { error, .. } => error.clone(),
            Self::Superseded => BrokerError::Denied(SUPERSEDED_READ.to_string()),
        }
    }

    /// The typed cause the selection report keeps for this failure.
    pub fn cause(&self) -> ObservationFailure {
        match self {
            Self::Transient { failure, .. } => failure.cause(),
            Self::Other { cause, .. } => cause.clone(),
            Self::Superseded => ObservationFailure::new(
                super::quota_policy::AccountFailureKind::QuotaUnknown,
            ),
        }
    }
}

pub type ReadResult = Result<(UsageSnapshot, SeatIdentity), ReadError>;

/// How a read was answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadPath {
    Network,
    Joined,
    NegativeCacheHit,
    /// Answered by the pause after another slot's usage 429: no request.
    Paused,
}

#[derive(Debug, Clone)]
struct Hold {
    failure: TransientReadFailure,
    until_ms: u64,
    backoff_ms: u64,
}

/// Every read of one [`ReadScope::pause`] waits until `until_ms`.
#[derive(Debug, Clone)]
struct Pause {
    failure: TransientReadFailure,
    until_ms: u64,
}

#[derive(Default)]
struct Entry {
    /// Bumped by every recorded network read.
    generation: u64,
    last: Option<ReadResult>,
    hold: Option<Hold>,
    /// Last successful snapshot and what it is bound to: the account
    /// fingerprint of the seat whose token read it, or that exact seat when
    /// its login carried no account id ([`same_account`]).
    last_good: Option<(UsageSnapshot, ReadFrom)>,
    /// `last_good` was loaded from the saved readings, and this process
    /// has not read the slot since.
    saved: bool,
    /// The slot's scope has saved readings ([`ReadScope::persisted`]): the
    /// writer saves its reading.
    persist: bool,
    /// When this process dropped the slot's last good reading: a saved one
    /// is not installed over the drop.
    dropped_at_ms: Option<u64>,
    /// Moved by every drop of the slot's reading: a good reading of a read
    /// admitted before the drop is discarded when it lands.
    epoch: u64,
}

/// What a last good reading is bound to: the account fingerprint of the
/// seat whose token read it (the only binding saved readings carry), or,
/// for a login without an account id, that exact seat.
#[derive(Clone)]
enum ReadFrom {
    Account(String),
    Seat(SeatIdentity),
}

impl ReadFrom {
    fn of(seat: &SeatIdentity) -> Self {
        match seat.seat_fingerprint() {
            Some(fp) => Self::Account(fp.to_string()),
            None => Self::Seat(seat.clone()),
        }
    }
}

/// Whether a reading taken from `read_from` may stand for the credential
/// now stored in the slot (`now`): the same account id when either carries
/// one; without ids on both sides, only the exact same credential (see the
/// module docs).
fn same_account(read_from: &ReadFrom, now: &SeatIdentity) -> bool {
    match (read_from, now.seat_fingerprint()) {
        (ReadFrom::Account(a), Some(b)) => a == b,
        (ReadFrom::Seat(seat), None) => seat == now,
        _ => false,
    }
}

/// Where a slot's readings are saved, and which reads one usage 429 pauses.
#[derive(Debug, Clone)]
pub struct ReadScope {
    /// The saved readings of the slot's credential store
    /// ([`SAVED_READINGS_FILE`] in its directory).
    pub saved: PathBuf,
    /// The prefix every key of that store's slots starts with.
    pub store: String,
    /// The reads one usage 429 pauses together (one provider's usage
    /// endpoint for this store), or `None` for a provider without the pause.
    pub pause: Option<String>,
    /// Whether the store's slots of this provider have saved readings.
    pub persisted: bool,
}

/// The saved readings file ([`SAVED_READINGS_FILE`]).
#[derive(Serialize, Deserialize)]
struct SavedReadings {
    version: u32,
    #[serde(default)]
    readings: BTreeMap<String, SavedReading>,
    #[serde(default)]
    pauses: BTreeMap<String, SavedPause>,
}

/// One slot's last good reading and the account it is bound to.
#[derive(Serialize, Deserialize, Clone)]
struct SavedReading {
    /// Account fingerprint of the seat it was read from. Required: a reading
    /// of a login without an account id is never saved, and an entry
    /// without one (null or missing) is not loaded.
    account: String,
    snapshot: UsageSnapshot,
}

/// An active pause: until when, and the 429's Retry-After.
#[derive(Serialize, Deserialize, Clone, Copy)]
struct SavedPause {
    until_ms: u64,
    retry_after_s: Option<u64>,
}

/// A saved account fingerprint: hex SHA-256, as `seat_fingerprint` makes it.
fn is_fingerprint(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The reading as it is saved: identity fields cleared (the binding is the
/// separate account fingerprint).
fn saved_snapshot(snapshot: &UsageSnapshot) -> UsageSnapshot {
    UsageSnapshot {
        identity_prefix: None,
        seat_fingerprint: None,
        ..snapshot.clone()
    }
}

/// The lease of one saved readings file: an exclusive lock on
/// [`SAVED_READINGS_LOCK`] beside it, held while the table writes the file.
struct Lease {
    /// The saved readings file it covers.
    saved: PathBuf,
    /// The open, locked lock file. Closing it frees the lease: a drop of
    /// the lease, the end of the process, or an exec (it is opened
    /// close-on-exec, as std opens every file on Unix).
    _lock: std::fs::File,
}

/// Take the lease of the saved readings file `saved` without waiting:
/// `Some` when this call holds it now, `None` when another open lock file
/// holds it (another process, or another table of this one). The lock file
/// is created 0600 if missing and never deleted.
fn take_lease(saved: &Path) -> std::io::Result<Option<Lease>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(saved.with_file_name(SAVED_READINGS_LOCK))?;
    if !fs4::fs_std::FileExt::try_lock_exclusive(&lock)? {
        return Ok(None);
    }
    Ok(Some(Lease {
        saved: saved.to_path_buf(),
        _lock: lock,
    }))
}

/// See the module docs. [`process_table`] is the one production instance.
#[derive(Default)]
pub struct UsageReads {
    entries: Mutex<HashMap<String, Entry>>,
    gates: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    pauses: Mutex<HashMap<String, Pause>>,
    /// Saved readings files already loaded by this table.
    loaded: Mutex<HashSet<PathBuf>>,
    /// The lease of the saved readings file this table writes, if any (see
    /// [`write_saved_readings`](Self::write_saved_readings)). Its lock also
    /// serialises the saves of this table (snapshot, write, rename), so a
    /// save checks the lease and writes under it, and a stop waits for it.
    saving: Mutex<Option<Lease>>,
    /// Test seam: run once inside the first load of a file, after it is
    /// read and before its readings are installed.
    #[cfg(test)]
    load_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Test seam: run once inside the next save that writes, under the
    /// save lock, before the file is written.
    #[cfg(test)]
    save_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

/// The table shared by every broker in this process.
pub fn process_table() -> Arc<UsageReads> {
    static TABLE: OnceLock<Arc<UsageReads>> = OnceLock::new();
    TABLE.get_or_init(|| Arc::new(UsageReads::default())).clone()
}

/// Ask for this process to write the saved readings of its credential store
/// (their lease, see the module docs): the daemon calls it once, at its
/// start, after it holds its lock and before any turn of it can read usage.
/// A process that doesn't get the lease, and every other process, only
/// loads them. The path comes from the credential store's resolver, the
/// same one every load of the process uses: the loaded guard of
/// [`UsageReads::write_saved_readings`] keys on the path as given.
pub fn save_readings_in_this_process() {
    let saved = super::storage::auth_file_path().with_file_name(SAVED_READINGS_FILE);
    process_table().write_saved_readings(&saved);
}

/// End this process's writing of the saved readings and free their lease:
/// the daemon calls it at its shutdown, after its sessions ended and before
/// it releases its lock. A save in flight finishes first; nothing is saved
/// after.
pub fn stop_saving_readings() {
    process_table().stop_writing();
}

fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

impl UsageReads {
    fn entries(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lease(&self) -> MutexGuard<'_, Option<Lease>> {
        self.saving.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Make this table the writer of the saved readings file `saved` (see
    /// [`save_readings_in_this_process`]) by taking its lease, without
    /// waiting; a table is read-only until then. True when this table
    /// writes `saved` now. Decided before the table loads `saved`: a table
    /// that has loaded it stays a reader (one WARN), and so does one whose
    /// lease can't be opened or locked (one WARN); a lease held elsewhere
    /// leaves it a reader too (one INFO). One lease per table. The log lines
    /// name no path and no account. The "has loaded it" check compares
    /// `saved` with the loaded paths as given, not as resolved on disk: a
    /// process enables and loads one file through one spelling of its path
    /// (every production caller takes it from the credential store's
    /// resolver); another spelling of the same file would not be seen as
    /// loaded.
    pub fn write_saved_readings(&self, saved: &Path) -> bool {
        let loaded = self.loaded.lock().unwrap_or_else(|e| e.into_inner());
        let mut lease = self.lease();
        if let Some(held) = lease.as_ref() {
            return held.saved == saved;
        }
        if loaded.contains(saved) {
            tracing::warn!(
                target: USAGE_READ_TARGET,
                writer = "none",
                "usage readings: this process only loads the saved readings: it loaded them before it asked to write them"
            );
            return false;
        }
        match take_lease(saved) {
            Ok(Some(taken)) => {
                *lease = Some(taken);
                tracing::info!(
                    target: USAGE_READ_TARGET,
                    writer = "this_process",
                    "usage readings: this process writes the saved readings"
                );
                true
            }
            Ok(None) => {
                tracing::info!(
                    target: USAGE_READ_TARGET,
                    writer = "other_process",
                    "usage readings: this process only loads the saved readings: another writes them"
                );
                false
            }
            Err(error) => {
                tracing::warn!(
                    target: USAGE_READ_TARGET,
                    writer = "none",
                    error_kind = ?error.kind(),
                    "usage readings: this process only loads the saved readings: their lease could not be taken"
                );
                false
            }
        }
    }

    /// Stop writing the saved readings and free their lease. A save in
    /// flight holds the lease's lock, so it finishes first; nothing is
    /// saved after. A table that has loaded the file stays a reader for
    /// good (see [`write_saved_readings`](Self::write_saved_readings)).
    pub fn stop_writing(&self) {
        *self.lease() = None;
    }

    /// Whether this table writes saved readings (holds a lease).
    pub fn saves_readings(&self) -> bool {
        self.lease().is_some()
    }

    /// Whether this table writes the saved readings file `saved`.
    fn writes(&self, saved: &Path) -> bool {
        self.lease().as_ref().is_some_and(|l| l.saved == saved)
    }

    /// Test seam: the descriptor of the lease this table holds, if any.
    #[cfg(all(test, unix))]
    fn lease_fd(&self) -> Option<i32> {
        use std::os::unix::io::AsRawFd;
        self.lease().as_ref().map(|l| l._lock.as_raw_fd())
    }

    /// Test seam: `hook` runs once, inside this table's first load of a
    /// saved readings file, after the file is read and before its readings
    /// are installed.
    #[cfg(test)]
    fn on_load(&self, hook: impl FnOnce() + Send + 'static) {
        *self.load_hook.lock().unwrap() = Some(Box::new(hook));
    }

    /// Test seam: `hook` runs once, inside this table's next save that
    /// writes, under the save lock, before the file is written.
    #[cfg(test)]
    fn on_save(&self, hook: impl FnOnce() + Send + 'static) {
        *self.save_hook.lock().unwrap() = Some(Box::new(hook));
    }

    /// The active hold for `key`: its failure and when it ends.
    pub fn held_at(&self, key: &str, now_ms: u64) -> Option<(TransientReadFailure, u64)> {
        self.entries()
            .get(key)
            .and_then(|e| e.hold.as_ref())
            .filter(|h| h.until_ms > now_ms)
            .map(|h| (h.failure.clone(), h.until_ms))
    }

    /// Record one network read for `key` (a slot of a store with saved
    /// readings) at `now_ms` and classify it.
    // ReadError carries the broker's own error (as `usage()` returns it);
    // one per read, so its size is no cost worth a box.
    #[allow(clippy::result_large_err)]
    pub fn record_at(&self, key: &str, fetched: Fetched, now_ms: u64) -> ReadResult {
        self.record(key, fetched, now_ms, true).0
    }

    /// [`record_at`](Self::record_at), and whether the slot's saved reading
    /// changes with it (a new good reading, or a dropped one). `persist`:
    /// the slot's scope has saved readings. A [`Fetched::Paused`] read sent
    /// nothing and records nothing.
    #[allow(clippy::result_large_err)]
    fn record(&self, key: &str, fetched: Fetched, now_ms: u64, persist: bool) -> (ReadResult, bool) {
        self.record_flight(key, fetched, now_ms, persist, None)
    }

    /// [`record`](Self::record) for a read admitted when the slot's epoch
    /// was `admitted` (`None`: not checked). A good reading whose slot's
    /// reading was dropped since (the epoch moved) is discarded: the read,
    /// and a reader that joined it, get [`ReadError::Superseded`]; nothing
    /// is installed or saved, and no hold starts. A failed read is recorded
    /// as usual (it says how the endpoint answered, not what the account
    /// has).
    #[allow(clippy::result_large_err)]
    fn record_flight(
        &self,
        key: &str,
        fetched: Fetched,
        now_ms: u64,
        persist: bool,
        admitted: Option<u64>,
    ) -> (ReadResult, bool) {
        let mut entries = self.entries();
        let entry = entries.entry(key.to_string()).or_default();
        let mut changed = false;
        let result = match fetched {
            Fetched::Paused {
                failure,
                retry_at_ms,
            } => {
                return (
                    Err(ReadError::Transient {
                        failure,
                        retry_at_ms,
                    }),
                    false,
                )
            }
            Fetched::Ok(..) if admitted.is_some_and(|epoch| epoch != entry.epoch) => {
                entry.generation = entry.generation.wrapping_add(1);
                entry.last = Some(Err(ReadError::Superseded));
                return (Err(ReadError::Superseded), false);
            }
            Fetched::Ok(snapshot, seat) => {
                entry.hold = None;
                entry.last_good = Some((snapshot.clone(), ReadFrom::of(&seat)));
                entry.saved = false;
                entry.dropped_at_ms = None;
                changed = true;
                Ok((snapshot, seat))
            }
            Fetched::Usage(e) => match TransientReadFailure::from_usage_error(&e) {
                Some(failure) => {
                    let cap = ms(NEGATIVE_BACKOFF_CAP);
                    let backoff_ms = match &entry.hold {
                        Some(h) => h.backoff_ms.saturating_mul(2).min(cap),
                        None => ms(NEGATIVE_BACKOFF_START),
                    };
                    // The longer of the backoff and the Retry-After: the
                    // Anthropic usage endpoint's 429s have said "retry after
                    // 0s", so the Retry-After alone would hold nothing.
                    let asked = failure.retry_after_s.map_or(0, |s| s.saturating_mul(1000));
                    let until_ms = now_ms.saturating_add(backoff_ms.max(asked).min(cap));
                    entry.hold = Some(Hold {
                        failure: failure.clone(),
                        until_ms,
                        backoff_ms,
                    });
                    Err(ReadError::Transient {
                        failure,
                        retry_at_ms: until_ms,
                    })
                }
                None => {
                    if e.is_auth_failure() {
                        changed = entry.drop_last_good(now_ms);
                    }
                    entry.hold = None;
                    Err(ReadError::Other {
                        cause: e.failure(),
                        error: e.into_broker_error(),
                    })
                }
            },
            Fetched::Failed { error, cause } => {
                // A refused or missing login ends any grace for the slot.
                if cause.kind.needs_login() {
                    changed = entry.drop_last_good(now_ms);
                }
                entry.hold = None;
                Err(ReadError::Other { error, cause })
            }
        };
        entry.persist = persist;
        entry.generation = entry.generation.wrapping_add(1);
        entry.last = Some(result.clone());
        (result, changed)
    }

    /// The slot's last successful snapshot, if it was read from the account
    /// now stored in the slot (`current`): never another account's reading.
    pub fn last_good_for(&self, key: &str, current: &SeatIdentity) -> Option<UsageSnapshot> {
        self.entries()
            .get(key)
            .and_then(|e| e.last_good.as_ref())
            .filter(|(_, read_from)| same_account(read_from, current))
            .map(|(snapshot, _)| snapshot.clone())
    }

    /// The slot's reading loaded from the saved readings, while this table
    /// has not read the slot since, if it was read from the account now
    /// stored in the slot (`current`). It keeps its original `observed_at`.
    pub fn saved_reading(&self, key: &str, current: &SeatIdentity) -> Option<UsageSnapshot> {
        self.entries()
            .get(key)
            .filter(|e| e.saved)
            .and_then(|e| e.last_good.as_ref())
            .filter(|(_, read_from)| same_account(read_from, current))
            .map(|(snapshot, _)| snapshot.clone())
    }

    /// Drop the slot's last good reading at `now_ms` (e.g. after a reported
    /// limit); true when there was one, so the saved readings change. The
    /// slot's epoch moves either way, so a read of it in flight now is
    /// discarded when it lands.
    pub fn forget_last_good(&self, key: &str, now_ms: u64) -> bool {
        self.entries()
            .entry(key.to_string())
            .or_default()
            .drop_last_good(now_ms)
    }

    /// The slot's epoch: how many times its reading was dropped. A reading
    /// taken, or a copy of one cached or held by a selection, while it stays
    /// the same may be used; one that lands after it moved is not.
    pub fn epoch(&self, key: &str) -> u64 {
        self.entries().get(key).map_or(0, |e| e.epoch)
    }

    fn pauses(&self) -> MutexGuard<'_, HashMap<String, Pause>> {
        self.pauses.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The active pause of `scope`'s reads: the 429 and when it ends.
    pub fn paused_at(
        &self,
        scope: &ReadScope,
        now_ms: u64,
    ) -> Option<(TransientReadFailure, u64)> {
        let pause = scope.pause.as_deref()?;
        self.pauses()
            .get(pause)
            .filter(|p| p.until_ms > now_ms)
            .map(|p| (p.failure.clone(), p.until_ms))
    }

    /// Pause `scope`'s reads until `until_ms` after `failure` (a usage 429);
    /// true when that moves the pause's end.
    fn start_pause(&self, scope: &ReadScope, failure: &TransientReadFailure, until_ms: u64) -> bool {
        let Some(pause) = scope.pause.as_deref() else {
            return false;
        };
        let mut pauses = self.pauses();
        if pauses.get(pause).is_some_and(|p| p.until_ms >= until_ms) {
            return false;
        }
        pauses.insert(
            pause.to_string(),
            Pause {
                failure: failure.clone(),
                until_ms,
            },
        );
        true
    }

    /// Load `scope`'s saved readings, once per file for this table: each
    /// valid reading of a slot this table holds no reading or drop for, and
    /// each valid pause. A missing, unreadable or invalid file loads nothing,
    /// and so does a scope without saved readings. The load lock is held
    /// through the read and the install: a read that starts meanwhile waits
    /// here until the readings are in (the barrier of the module docs).
    pub fn load_saved(&self, scope: &ReadScope, now_ms: u64) {
        if !scope.persisted {
            return;
        }
        let mut loaded = self.loaded.lock().unwrap_or_else(|e| e.into_inner());
        if loaded.contains(&scope.saved) {
            return;
        }
        if let Some((readings, pauses)) = read_saved(scope, now_ms) {
            #[cfg(test)]
            {
                let hook = self.load_hook.lock().unwrap().take();
                if let Some(hook) = hook {
                    hook();
                }
            }
            self.install(readings, pauses);
        }
        loaded.insert(scope.saved.clone());
    }

    /// Install loaded readings and pauses: a reading only where this table
    /// holds no reading or drop of its own, a pause only where it ends later.
    fn install(
        &self,
        readings: BTreeMap<String, SavedReading>,
        pauses: BTreeMap<String, SavedPause>,
    ) {
        {
            let mut entries = self.entries();
            for (key, reading) in readings {
                let entry = entries.entry(key).or_default();
                if entry.last_good.is_none() && entry.dropped_at_ms.is_none() {
                    entry.last_good =
                        Some((reading.snapshot, ReadFrom::Account(reading.account)));
                    entry.saved = true;
                    entry.persist = true;
                }
            }
        }
        let mut held = self.pauses();
        for (key, pause) in pauses {
            let Some(failure) = TransientReadFailure::from_usage_error(&UsageError::RateLimited {
                retry_after_s: pause.retry_after_s,
            }) else {
                continue;
            };
            if held.get(&key).is_some_and(|p| p.until_ms >= pause.until_ms) {
                continue;
            }
            held.insert(
                key,
                Pause {
                    failure,
                    until_ms: pause.until_ms,
                },
            );
        }
    }

    /// Save `scope`'s store, if this table holds the lease of its file (the
    /// writer) and the store has saved readings: this table's own readings
    /// of the store's slots that are bound to an account fingerprint, and
    /// its active pauses (never a merge with the file: a reading this table
    /// dropped is not written). The lease is held through the write.
    /// The file's readings are loaded first, so the table holds them. The
    /// file is written to a new file in the same directory (mode 0600) and
    /// renamed over it; one over [`SAVED_READINGS_MAX_BYTES`] is not
    /// written. Best effort: a failure is logged at debug and changes
    /// nothing else.
    pub fn save(&self, scope: &ReadScope, now_ms: u64) {
        if !scope.persisted || !self.writes(&scope.saved) {
            return;
        }
        self.load_saved(scope, now_ms);
        // Checked again, and held through the write: a stop (the daemon's
        // shutdown) waits for this save, and none starts after it.
        let lease = self.lease();
        if lease.as_ref().is_none_or(|l| l.saved != scope.saved) {
            return;
        }
        #[cfg(test)]
        {
            let hook = self.save_hook.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        let readings = self
            .entries()
            .iter()
            .filter(|(key, entry)| entry.persist && key.starts_with(&scope.store))
            .filter_map(|(key, entry)| match &entry.last_good {
                Some((snapshot, ReadFrom::Account(account))) => Some((
                    key.clone(),
                    SavedReading {
                        account: account.clone(),
                        snapshot: saved_snapshot(snapshot),
                    },
                )),
                _ => None,
            })
            .collect();
        let pauses = self
            .pauses()
            .iter()
            .filter(|(key, pause)| key.starts_with(&scope.store) && pause.until_ms > now_ms)
            .map(|(key, pause)| {
                (
                    key.clone(),
                    SavedPause {
                        until_ms: pause.until_ms,
                        retry_after_s: pause.failure.retry_after_s,
                    },
                )
            })
            .collect();
        let file = SavedReadings {
            version: SAVED_READINGS_VERSION,
            readings,
            pauses,
        };
        let written = serde_json::to_vec(&file)
            .map_err(std::io::Error::from)
            .and_then(|bytes| {
                if bytes.len() as u64 > SAVED_READINGS_MAX_BYTES {
                    return Err(std::io::Error::other("the saved readings are over the size bound"));
                }
                write_saved(&scope.saved, &bytes)
            });
        drop(lease);
        if let Err(error) = written {
            tracing::debug!(%error, "usage readings not saved");
        }
    }

    /// Prune the reading of each slot with saved readings that `covers` (one
    /// provider's slots of `scope`'s store) and `listed` (the store's
    /// listing now) does not hold: an account removed from the store, so
    /// the writer's next saves leave it out. This is file hygiene, not one
    /// of the drops of the module docs (a reported limit, a refused login).
    /// The store's saved readings are loaded first, so a saved reading of a
    /// slot already removed is pruned too; a reading the slot gets after
    /// this call (a direct usage read, or a first read that was in flight)
    /// stays until a later call. Nothing is saved here (the next save
    /// leaves them out).
    pub fn keep_listed(
        &self,
        scope: &ReadScope,
        covers: impl Fn(&str) -> bool,
        listed: &HashSet<String>,
        now_ms: u64,
    ) {
        self.load_saved(scope, now_ms);
        for (key, entry) in self.entries().iter_mut() {
            if entry.persist && entry.last_good.is_some() && covers(key) && !listed.contains(key) {
                entry.drop_last_good(now_ms);
            }
        }
    }

    /// Drop the slot's hold and last good reading (`synaps auth state
    /// --clear`): the next read goes to the network. The gate stays, so a
    /// read in flight keeps its single flight; if one is in flight, its
    /// result is still recorded when it lands.
    pub fn forget(&self, key: &str) {
        if let Some(e) = self.entries().get_mut(key) {
            e.hold = None;
            e.last_good = None;
        }
    }

    fn generation(&self, key: &str) -> u64 {
        self.entries().get(key).map_or(0, |e| e.generation)
    }

    fn gate(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.gates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Read `key` (a slot of `scope`'s store) at the times `now` gives: the
    /// store's saved readings are loaded first (once); a held failure, or
    /// the pause of `scope` after another slot's usage 429, is returned
    /// without a request; otherwise one flight per key, and a reader that
    /// waited for a flight takes its result. A usage 429 starts the pause.
    /// A new good reading, a new pause or a dropped reading is saved.
    pub async fn read<N, F, Fut>(
        &self,
        key: &str,
        scope: &ReadScope,
        now: N,
        fetch: F,
    ) -> (ReadResult, ReadPath)
    where
        N: Fn() -> u64,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Fetched>,
    {
        self.load_saved(scope, now());
        if let Some((failure, retry_at_ms)) = self.held_at(key, now()) {
            return (
                Err(ReadError::Transient {
                    failure,
                    retry_at_ms,
                }),
                ReadPath::NegativeCacheHit,
            );
        }
        let paused = |reads: &Self| -> Option<ReadResult> {
            reads
                .paused_at(scope, now())
                .map(|(failure, retry_at_ms)| {
                    Err(ReadError::Transient {
                        failure,
                        retry_at_ms,
                    })
                })
        };
        if let Some(result) = paused(self) {
            return (result, ReadPath::Paused);
        }
        let seen = self.generation(key);
        let gate = self.gate(key);
        let _flight = gate.lock().await;
        let joined = self
            .entries()
            .get(key)
            .filter(|e| e.generation != seen)
            .and_then(|e| e.last.clone());
        if let Some(result) = joined {
            return (result, ReadPath::Joined);
        }
        // A pause that began while this read waited for its slot's flight.
        if let Some(result) = paused(self) {
            return (result, ReadPath::Paused);
        }
        // The read is admitted at the slot's epoch now: a drop of its
        // reading while it is out discards the reading it brings back.
        let admitted = self.epoch(key);
        let fetched = fetch().await;
        let now_ms = now();
        // The pause began after the checks above, while the read refreshed
        // its token: it sent nothing, and nothing is recorded.
        if let Fetched::Paused {
            failure,
            retry_at_ms,
        } = fetched
        {
            return (
                Err(ReadError::Transient {
                    failure,
                    retry_at_ms,
                }),
                ReadPath::Paused,
            );
        }
        let (result, mut changed) =
            self.record_flight(key, fetched, now_ms, scope.persisted, Some(admitted));
        if let Err(ReadError::Transient {
            failure,
            retry_at_ms,
        }) = &result
        {
            if failure.is_rate_limited() {
                changed |= self.start_pause(scope, failure, *retry_at_ms);
            }
        }
        if changed {
            self.save(scope, now_ms);
        }
        (result, ReadPath::Network)
    }
}

impl Entry {
    /// Drop the last good reading at `now_ms`; true when there was one. The
    /// drop time is never earlier than the dropped reading's observation (a
    /// clock step back), so its saved copy is dropped with it. The epoch
    /// moves on every drop, with or without a reading.
    fn drop_last_good(&mut self, now_ms: u64) -> bool {
        self.saved = false;
        self.epoch = self.epoch.wrapping_add(1);
        let dropped = self.last_good.take();
        let observed = dropped.as_ref().map_or(0, |(s, _)| s.observed_at);
        let before = self.dropped_at_ms.unwrap_or(0);
        self.dropped_at_ms = Some(now_ms.max(observed).max(before));
        dropped.is_some()
    }
}

/// The valid readings and pauses of `scope`'s saved readings file at
/// `now_ms`, or `None` when it is missing, unreadable, over
/// [`SAVED_READINGS_MAX_BYTES`], not JSON or of another version. Dropped one
/// by one: an entry outside the store, a malformed entry, a reading of
/// another schema version, observed after `now_ms`, or not bound to an
/// account fingerprint, and a pause that has ended or ends more than
/// [`NEGATIVE_BACKOFF_CAP`] from now.
#[allow(clippy::type_complexity)]
fn read_saved(
    scope: &ReadScope,
    now_ms: u64,
) -> Option<(BTreeMap<String, SavedReading>, BTreeMap<String, SavedPause>)> {
    use std::io::Read;
    #[derive(Deserialize)]
    struct Raw {
        version: u32,
        #[serde(default)]
        readings: BTreeMap<String, serde_json::Value>,
        #[serde(default)]
        pauses: BTreeMap<String, serde_json::Value>,
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&scope.saved)
        .ok()?
        .take(SAVED_READINGS_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > SAVED_READINGS_MAX_BYTES {
        return None;
    }
    let raw: Raw = serde_json::from_slice(&bytes).ok()?;
    if raw.version != SAVED_READINGS_VERSION {
        return None;
    }
    let readings = raw
        .readings
        .into_iter()
        .filter(|(key, _)| key.starts_with(&scope.store))
        .filter_map(|(key, value)| Some((key, serde_json::from_value::<SavedReading>(value).ok()?)))
        .filter(|(_, r)| {
            r.snapshot.schema_version == super::usage::SCHEMA_VERSION
                && r.snapshot.observed_at <= now_ms
                && is_fingerprint(&r.account)
        })
        .collect();
    let longest = now_ms.saturating_add(ms(NEGATIVE_BACKOFF_CAP));
    let pauses = raw
        .pauses
        .into_iter()
        .filter(|(key, _)| key.starts_with(&scope.store))
        .filter_map(|(key, value)| Some((key, serde_json::from_value::<SavedPause>(value).ok()?)))
        .filter(|(_, p)| p.until_ms > now_ms && p.until_ms <= longest)
        .collect();
    Some((readings, pauses))
}

#[cfg(test)]
thread_local! {
    /// Test seam: the name of the next temporary file a save on this thread
    /// writes, in place of the generated one.
    static TEMP_NAME: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Write `bytes` to `path`: a new file beside it (mode 0600 on Unix), then
/// a rename over `path`. On a failure only the new file this call created
/// is removed.
fn write_saved(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("the saved readings have no directory"))?;
    let name = format!(
        ".{SAVED_READINGS_FILE}.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    #[cfg(test)]
    let name = TEMP_NAME.with(|n| n.borrow_mut().take()).unwrap_or(name);
    let tmp = dir.join(name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // Not created (it exists, or the directory refuses it): nothing to undo.
    let mut file = options.open(&tmp)?;
    let written = file.write_all(bytes);
    drop(file);
    let written = written.and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    const MIN: u64 = 60_000;

    fn limited(retry_after_s: Option<u64>) -> Fetched {
        Fetched::Usage(UsageError::RateLimited { retry_after_s })
    }

    fn held_for(t: &UsageReads, fetched: Fetched, now: u64) -> u64 {
        match t.record_at("k", fetched, now) {
            Err(ReadError::Transient { retry_at_ms, .. }) => {
                assert_eq!(
                    t.held_at("k", now + 1).map(|(_, until)| until),
                    Some(retry_at_ms),
                    "a read inside the hold is answered from the table"
                );
                assert!(t.held_at("k", retry_at_ms).is_none(), "the hold ends on time");
                retry_at_ms - now
            }
            other => panic!("expected a held failure, got {other:?}"),
        }
    }

    fn seat(refresh: &str, account_id: Option<&str>) -> SeatIdentity {
        let creds = super::super::OAuthCredentials {
            auth_type: "oauth".into(),
            refresh: refresh.into(),
            access: "a".into(),
            expires: 0,
            account_id: account_id.map(Into::into),
        };
        SeatIdentity::of(super::super::OAuthProviderId::Anthropic, &creds)
    }

    fn good_from(read_from: SeatIdentity) -> Fetched {
        let snapshot = super::super::usage::parse_anthropic_usage(
            r#"{"seven_day":{"utilization":10,"resets_at":"2099-01-01T00:00:00Z"}}"#,
            "a",
            1,
        )
        .unwrap();
        Fetched::Ok(snapshot, read_from)
    }

    fn good() -> Fetched {
        good_from(seat("r", None))
    }

    #[test]
    fn failed_reads_back_off_two_four_eight_ten_minutes_and_a_success_clears() {
        let t = UsageReads::default();
        let mut now = 1_000_000;
        let mut holds = Vec::new();
        for _ in 0..5 {
            let held = held_for(&t, limited(None), now);
            holds.push(held / MIN);
            now += held;
        }
        assert_eq!(holds, vec![2, 4, 8, 10, 10]);
        assert!(t.record_at("k", good(), now).is_ok());
        assert!(t.held_at("k", now).is_none(), "a success clears the hold");
        assert!(t.last_good_for("k", &seat("r", None)).is_some());
        assert_eq!(held_for(&t, limited(None), now), 2 * MIN, "and restarts the backoff");

        // A longer Retry-After wins, capped at 10 minutes.
        let t = UsageReads::default();
        assert_eq!(held_for(&t, limited(Some(300)), now), 5 * MIN);
        assert_eq!(held_for(&t, limited(Some(3600)), now + 5 * MIN), 10 * MIN);
        let t = UsageReads::default();
        assert_eq!(held_for(&t, limited(Some(30)), now), 2 * MIN, "a shorter one does not");

        // 5xx, timeouts and transport failures are held too; a 5xx keeps its
        // Retry-After like a 429.
        for e in [
            UsageError::UpstreamStatus {
                status: 503,
                retry_after_s: None,
            },
            UsageError::Timeout,
            UsageError::Transport { detail: "connect".into() },
        ] {
            let t = UsageReads::default();
            assert_eq!(held_for(&t, Fetched::Usage(e), now), 2 * MIN);
        }
        let t = UsageReads::default();
        let slow = UsageError::UpstreamStatus {
            status: 503,
            retry_after_s: Some(300),
        };
        assert_eq!(held_for(&t, Fetched::Usage(slow), now), 5 * MIN);
        // Credential rejections and other answers are not, and a refused
        // login drops the last good reading.
        let t = UsageReads::default();
        assert!(t.record_at("k", good(), now).is_ok());
        for e in [
            UsageError::Unauthorized { status: 401 },
            UsageError::UpstreamStatus {
                status: 404,
                retry_after_s: None,
            },
            UsageError::Malformed { detail: "x".into() },
        ] {
            assert!(matches!(
                t.record_at("k", Fetched::Usage(e), now),
                Err(ReadError::Other { .. })
            ));
            assert!(t.held_at("k", now).is_none());
        }
        assert!(
            t.last_good_for("k", &seat("r", None)).is_none(),
            "the 401 dropped the last good reading"
        );
    }

    /// Last good is bound to the account it was read from: the same account
    /// id after a token rotation keeps it, another account id (or a switch
    /// between id and no id) never sees it. Without ids only the exact
    /// credential the reading came from does (see the module docs).
    #[test]
    fn last_good_counts_only_for_the_account_it_was_read_from() {
        let t = UsageReads::default();
        assert!(t.record_at("k", good_from(seat("r1", Some("acct-1"))), 1).is_ok());
        assert!(t.last_good_for("k", &seat("r1", Some("acct-1"))).is_some());
        assert!(
            t.last_good_for("k", &seat("r2-rotated", Some("acct-1"))).is_some(),
            "a refresh rotation keeps the account"
        );
        assert!(
            t.last_good_for("k", &seat("r1", Some("acct-2"))).is_none(),
            "another account in the slot is never graced on this reading"
        );
        assert!(t.last_good_for("k", &seat("r1", None)).is_none());
        assert!(t.last_good_for("other", &seat("r1", Some("acct-1"))).is_none());

        let t = UsageReads::default();
        assert!(t.record_at("k", good_from(seat("r1", None)), 1).is_ok());
        assert!(t.last_good_for("k", &seat("r1", None)).is_some());
        assert!(
            t.last_good_for("k", &seat("r2", None)).is_none(),
            "without ids, other refresh material may be another account"
        );
        assert!(t.last_good_for("k", &seat("r1", Some("acct-1"))).is_none());
    }

    // ── Saved readings and the pause ────────────────────────────────────

    use serde_json::json;

    /// A scope over a scratch directory: the store `s|`, its pause
    /// `s|anthropic`.
    fn scope_in(dir: &Path) -> ReadScope {
        ReadScope {
            saved: dir.join(SAVED_READINGS_FILE),
            store: "s|".into(),
            pause: Some("s|anthropic".into()),
            persisted: true,
        }
    }

    /// A table that writes `scope`'s saved readings (the daemon's): it holds
    /// their lease.
    fn writer(scope: &ReadScope) -> UsageReads {
        let t = UsageReads::default();
        assert!(t.write_saved_readings(&scope.saved), "the lease was not free");
        t
    }

    /// [`good_from`] observed at `observed_at`.
    fn good_at(observed_at: u64, read_from: SeatIdentity) -> Fetched {
        match good_from(read_from) {
            Fetched::Ok(mut snapshot, seat) => {
                snapshot.observed_at = observed_at;
                Fetched::Ok(snapshot, seat)
            }
            _ => unreachable!(),
        }
    }

    fn rate_limited(retry_after_s: Option<u64>) -> TransientReadFailure {
        TransientReadFailure::from_usage_error(&UsageError::RateLimited { retry_after_s }).unwrap()
    }

    /// The slots of `keys` that `t` holds a reading for, bound or not.
    fn with_reading(t: &UsageReads, keys: &[&'static str]) -> Vec<&'static str> {
        let entries = t.entries();
        keys.iter()
            .copied()
            .filter(|k| entries.get(*k).is_some_and(|e| e.last_good.is_some()))
            .collect()
    }

    fn pause_keys(t: &UsageReads) -> Vec<String> {
        let mut keys: Vec<_> = t.pauses().keys().cloned().collect();
        keys.sort();
        keys
    }

    /// A saved reading loads into a new table (a reload) with its original
    /// observation time, for the account it was read from only, and stops
    /// being a saved reading once the slot is read again.
    #[test]
    fn a_saved_reading_loads_with_its_original_time_for_its_account_only() {
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_in(dir.path());
        let now = 100 * MIN;
        let one = writer(&scope);
        let (read, changed) =
            one.record("s|a", good_at(now - MIN, seat("r1", Some("acct-1"))), now, true);
        assert!(read.is_ok() && changed, "a good reading changes the saved readings");
        one.save(&scope, now);
        let temps: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(temps.is_empty(), "a temporary file is left: {temps:?}");
        assert!(scope.saved.exists());

        let two = UsageReads::default();
        two.load_saved(&scope, now + MIN);
        let loaded = two
            .saved_reading("s|a", &seat("r2-rotated", Some("acct-1")))
            .expect("the same account after a rotation");
        assert_eq!(loaded.observed_at, now - MIN, "a load is not an observation");
        assert!(loaded.seat_fingerprint.is_none() && loaded.identity_prefix.is_none());
        assert!(
            two.saved_reading("s|a", &seat("r1", Some("acct-2"))).is_none(),
            "another account"
        );
        assert!(
            two.saved_reading("s|a", &seat("r1", None)).is_none(),
            "a login without an id"
        );
        assert!(two
            .record_at("s|a", good_at(now + MIN, seat("r1", Some("acct-1"))), now + MIN)
            .is_ok());
        assert!(
            two.saved_reading("s|a", &seat("r1", Some("acct-1"))).is_none(),
            "read again: no longer a saved reading"
        );
    }

    /// A load drops each invalid entry on its own: a reading observed after
    /// the load time, of another schema version, bound to no account or a
    /// malformed one, malformed, or of another store; a pause that has
    /// ended, ends past the longest hold, is malformed or of another store.
    /// A file of another version, not JSON, or over
    /// [`SAVED_READINGS_MAX_BYTES`], loads nothing, and the next save
    /// replaces it.
    #[test]
    fn a_load_drops_invalid_entries_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_in(dir.path());
        let now = 100 * MIN;
        let fingerprint = seat("r", Some("acct-1"))
            .seat_fingerprint()
            .unwrap()
            .to_string();
        let current = super::super::usage::SCHEMA_VERSION;
        let snapshot = |observed_at: u64, schema_version: u32| match good_at(
            observed_at,
            seat("r", None),
        ) {
            Fetched::Ok(mut s, _) => {
                s.schema_version = schema_version;
                serde_json::to_value(saved_snapshot(&s)).unwrap()
            }
            _ => unreachable!(),
        };
        let reading = |observed_at: u64, schema_version: u32| {
            json!({"account": fingerprint, "snapshot": snapshot(observed_at, schema_version)})
        };
        let file = |version: u32| {
            json!({
                "version": version,
                "readings": {
                    "s|ok": reading(now - MIN, current),
                    "s|edge": reading(now, current),
                    "s|future": reading(now + 1, current),
                    "s|schema": reading(now - MIN, current + 1),
                    "s|unbound": {"account": "acct-1", "snapshot": snapshot(now - MIN, current)},
                    "s|no-account": {"account": null, "snapshot": snapshot(now - MIN, current)},
                    "s|missing-account": {"snapshot": snapshot(now - MIN, current)},
                    "s|malformed": {"account": fingerprint, "snapshot": "x"},
                    "t|ok": reading(now - MIN, current),
                },
                "pauses": {
                    "s|ok": {"until_ms": now + MIN, "retry_after_s": 60},
                    "s|ended": {"until_ms": now, "retry_after_s": 60},
                    "s|too-long": {"until_ms": now + 10 * MIN + 1, "retry_after_s": null},
                    "s|malformed": {"until_ms": "soon"},
                    "t|ok": {"until_ms": now + MIN, "retry_after_s": 60},
                }
            })
        };
        let keys = [
            "s|ok",
            "s|edge",
            "s|future",
            "s|schema",
            "s|unbound",
            "s|no-account",
            "s|missing-account",
            "s|malformed",
            "t|ok",
        ];
        let write = |bytes: Vec<u8>| std::fs::write(&scope.saved, bytes).unwrap();

        write(file(SAVED_READINGS_VERSION).to_string().into_bytes());
        let t = UsageReads::default();
        t.load_saved(&scope, now);
        assert_eq!(with_reading(&t, &keys), ["s|ok", "s|edge"]);
        assert_eq!(pause_keys(&t), ["s|ok"]);

        // The same valid file, padded with whitespace past the size bound.
        let mut oversized = file(SAVED_READINGS_VERSION).to_string().into_bytes();
        oversized.resize(SAVED_READINGS_MAX_BYTES as usize + 1, b' ');
        for bad in [
            file(SAVED_READINGS_VERSION + 1).to_string().into_bytes(),
            json!(["not", "an", "object"]).to_string().into_bytes(),
            b"{not json".to_vec(),
            oversized,
        ] {
            write(bad);
            let t = UsageReads::default();
            t.load_saved(&scope, now);
            assert!(with_reading(&t, &keys).is_empty(), "nothing loads");
            assert!(pause_keys(&t).is_empty(), "nothing loads");
        }
        let t = writer(&scope);
        t.load_saved(&scope, now);
        assert!(t
            .record_at("s|ok", good_at(now, seat("r", Some("acct-1"))), now)
            .is_ok());
        t.save(&scope, now);
        let again = UsageReads::default();
        again.load_saved(&scope, now);
        assert_eq!(with_reading(&again, &keys), ["s|ok"], "the save replaced it");
    }

    /// A pause is saved with its end and Retry-After, and a new table (a
    /// reload, or another process on the store) keeps it until it ends. It
    /// covers its own scope only, and an ended pause is not saved again.
    #[test]
    fn a_saved_pause_holds_in_a_new_table_until_it_ends() {
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_in(dir.path());
        let now = 100 * MIN;
        let one = writer(&scope);
        assert!(one.start_pause(&scope, &rate_limited(Some(120)), now + 2 * MIN));
        assert!(
            !one.start_pause(&scope, &rate_limited(Some(60)), now + MIN),
            "a shorter pause changes nothing"
        );
        one.save(&scope, now);
        one.stop_writing();

        let two = writer(&scope);
        two.load_saved(&scope, now + MIN);
        let (failure, until) = two.paused_at(&scope, now + MIN).expect("the saved pause");
        assert_eq!((failure.retry_after_s, until), (Some(120), now + 2 * MIN));
        assert!(failure.is_rate_limited());
        assert!(two.paused_at(&scope, now + 2 * MIN).is_none(), "it ends on time");
        let unpaused = ReadScope {
            pause: None,
            ..scope.clone()
        };
        assert!(two.paused_at(&unpaused, now + MIN).is_none(), "no pause for it");
        let other = ReadScope {
            pause: Some("s|other".into()),
            ..scope.clone()
        };
        assert!(two.paused_at(&other, now + MIN).is_none(), "another scope");

        two.save(&scope, now + 2 * MIN);
        let three = UsageReads::default();
        three.load_saved(&scope, now + 2 * MIN - 1);
        assert!(pause_keys(&three).is_empty(), "the ended pause was not saved");
    }

    /// The keys of the readings in `scope`'s saved readings file.
    fn saved_keys(scope: &ReadScope) -> Vec<String> {
        let file: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&scope.saved).unwrap()).unwrap();
        file["readings"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }

    /// The writer saves its own table: what it holds, and nothing it dropped
    /// (a reported limit, a refused login), so the file holds exactly the
    /// readings it still has.
    #[test]
    fn the_writer_saves_its_own_readings_and_drops_forgotten_ones() {
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_in(dir.path());
        let now = 100 * MIN;
        let account = || seat("r", Some("acct-1"));
        let one = writer(&scope);
        for key in ["s|a", "s|b", "s|c"] {
            assert!(one.record_at(key, good_at(now - 2 * MIN, account()), now).is_ok());
        }
        one.save(&scope, now);
        assert_eq!(saved_keys(&scope), ["s|a", "s|b", "s|c"]);
        assert!(one.forget_last_good("s|a", now + MIN));
        let refused = Fetched::Usage(UsageError::Unauthorized { status: 401 });
        assert!(one.record("s|b", refused, now + MIN, true).1, "a refused login drops it");
        one.save(&scope, now + MIN);
        assert_eq!(saved_keys(&scope), ["s|c"]);

        let two = UsageReads::default();
        two.load_saved(&scope, now + MIN);
        assert!(two.saved_reading("s|a", &account()).is_none(), "the limit dropped it");
        assert!(two.saved_reading("s|b", &account()).is_none(), "the 401 dropped it");
        assert_eq!(
            two.saved_reading("s|c", &account()).map(|s| s.observed_at),
            Some(now - 2 * MIN)
        );
    }

    /// p1-r1 B1: only the writer saves. A table that is not the writer (a
    /// `synaps status`, a TUI) never writes the file, whatever it reads.
    #[tokio::test]
    async fn a_table_that_is_not_the_writer_never_saves() {
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_in(dir.path());
        let now = 100 * MIN;
        let t = UsageReads::default();
        assert!(!t.saves_readings());
        let (good, path) = t
            .read("s|a", &scope, || now, || async move {
                good_at(now, seat("r", Some("acct-1")))
            })
            .await;
        assert!(good.is_ok() && path == ReadPath::Network);
        let (limited_read, _) = t
            .read("s|b", &scope, || now, || async { limited(Some(60)) })
            .await;
        assert!(limited_read.is_err());
        assert!(t.paused_at(&scope, now).is_some(), "the 429 paused the reads");
        assert!(t.forget_last_good("s|a", now));
        t.save(&scope, now);
        let files: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(files.is_empty(), "a read-only table wrote {files:?}");
    }

    /// p1-r1 B1: the reviewer's sequence. The daemon (the writer) and a
    /// status process load the same readings; the daemon drops `a` (a
    /// reported limit) and saves; status then reads `b`. After a reload `a`
    /// is gone: status never writes its old copy of `a` back.
    #[tokio::test]
    async fn a_reading_the_writer_dropped_never_comes_back_through_another_table() {
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_in(dir.path());
        let now = 100 * MIN;
        let account = || seat("r", Some("acct-1"));
        let before = writer(&scope);
        for key in ["s|a", "s|b"] {
            assert!(before.record_at(key, good_at(now - 2 * MIN, account()), now).is_ok());
        }
        before.save(&scope, now);
        before.stop_writing();

        let daemon = writer(&scope);
        daemon.load_saved(&scope, now);
        let status = UsageReads::default();
        status.load_saved(&scope, now);
        assert!(daemon.forget_last_good("s|a", now + MIN));
        daemon.save(&scope, now + MIN);
        let (read, _) = status
            .read("s|b", &scope, || now + MIN, || async move {
                good_at(now + MIN, seat("r", Some("acct-1")))
            })
            .await;
        assert!(read.is_ok());
        status.save(&scope, now + MIN);

        let reload = UsageReads::default();
        reload.load_saved(&scope, now + 2 * MIN);
        assert!(reload.saved_reading("s|a", &account()).is_none(), "a came back");
        assert_eq!(
            reload.saved_reading("s|b", &account()).map(|s| s.observed_at),
            Some(now - 2 * MIN),
            "b as the writer saved it"
        );
    }

    /// p1-r1 B3, p1-r2 SF3: the first load of the saved readings completes
    /// before anything else on the table goes on. Reads, a save and a drop
    /// that reach the table while the load is installing (a reload's
    /// restored turns, all at once) wait for it: a saved reading is there
    /// for the reads and a saved pause holds them, so no request goes out;
    /// the save writes the loaded readings, not an empty table; the install
    /// does not undo the drop. The hook proves every racer has started
    /// (a barrier) before the install goes on, and no racer may finish
    /// while it is held (the bounded wait only gives one that does not wait
    /// time to finish and fail the test).
    #[test]
    fn reads_a_save_and_a_drop_during_the_first_load_wait_for_it() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::{mpsc, Barrier};
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_in(dir.path());
        let now = 100 * MIN;
        let account = || seat("r", Some("acct-1"));
        let before = writer(&scope);
        for key in ["s|a", "s|b", "s|e"] {
            assert!(before.record_at(key, good_at(now - MIN, account()), now).is_ok());
        }
        assert!(before.start_pause(&scope, &rate_limited(Some(120)), now + 2 * MIN));
        before.save(&scope, now);
        before.stop_writing();

        let t = Arc::new(writer(&scope));
        let fetched = Arc::new(AtomicUsize::new(0));
        let racers = Arc::new(Mutex::new(Vec::new()));
        const RACERS: usize = 6;
        let started = Arc::new(Barrier::new(RACERS + 1));
        let (done, finished) = mpsc::channel::<()>();
        let (table, at, count, joins) = (t.clone(), scope.clone(), fetched.clone(), racers.clone());
        t.on_load(move || {
            let spawn = |race: Box<dyn FnOnce() -> bool + Send>| {
                let (started, done) = (started.clone(), done.clone());
                joins.lock().unwrap().push(std::thread::spawn(move || {
                    started.wait();
                    let ok = race();
                    let _ = done.send(());
                    ok
                }));
            };
            for key in ["s|a", "s|b"] {
                let (t, scope) = (table.clone(), at.clone());
                spawn(Box::new(move || {
                    t.load_saved(&scope, now);
                    t.saved_reading(key, &seat("r", Some("acct-1"))).is_some()
                }));
            }
            for key in ["s|c", "s|d"] {
                let (t, scope, count) = (table.clone(), at.clone(), count.clone());
                spawn(Box::new(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    let (read, path) = runtime.block_on(t.read(key, &scope, || now, || async {
                        count.fetch_add(1, Ordering::SeqCst);
                        limited(None)
                    }));
                    read.is_err() && path == ReadPath::Paused
                }));
            }
            let (t, scope) = (table.clone(), at.clone());
            spawn(Box::new(move || {
                t.save(&scope, now);
                true
            }));
            let (t, scope) = (table.clone(), at.clone());
            spawn(Box::new(move || {
                // As a refusal drops a reading (`ReadingDrop`).
                t.load_saved(&scope, now);
                t.forget_last_good("s|e", now);
                t.save(&scope, now);
                true
            }));
            started.wait();
            assert!(
                finished.recv_timeout(Duration::from_millis(200)).is_err(),
                "a racer finished while the load was installing"
            );
        });
        t.load_saved(&scope, now);
        let joins: Vec<_> = racers.lock().unwrap().drain(..).collect();
        assert_eq!(joins.len(), RACERS, "the load ran the hook");
        let honoured: Vec<bool> = joins.into_iter().map(|j| j.join().unwrap()).collect();
        assert_eq!(honoured, [true; RACERS], "every racer saw the loaded table");
        assert_eq!(fetched.load(Ordering::SeqCst), 0, "no request went out");
        assert!(t.saved_reading("s|a", &account()).is_some());
        assert!(t.saved_reading("s|b", &account()).is_some());
        assert!(t.saved_reading("s|e", &account()).is_none(), "the install undid the drop");
        assert_eq!(saved_keys(&scope), ["s|a", "s|b"], "the saves lost the loaded readings");
        let reload = UsageReads::default();
        reload.load_saved(&scope, now);
        assert!(reload.paused_at(&scope, now).is_some(), "the saves lost the pause");
    }

    /// LOW2: a scope without saved readings (Codex, Gemini) neither loads
    /// nor saves them, and its load does not count as the file's load.
    #[tokio::test]
    async fn a_scope_without_saved_readings_neither_loads_nor_saves_them() {
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_in(dir.path());
        let unsaved = ReadScope {
            pause: None,
            persisted: false,
            ..scope.clone()
        };
        let now = 100 * MIN;
        let account = || seat("r", Some("acct-1"));
        let before = writer(&scope);
        assert!(before.record_at("s|a", good_at(now - MIN, account()), now).is_ok());
        before.save(&scope, now);
        before.stop_writing();
        let written = std::fs::read(&scope.saved).unwrap();

        let t = writer(&scope);
        t.load_saved(&unsaved, now);
        assert!(with_reading(&t, &["s|a"]).is_empty(), "loaded for an unsaved scope");
        let (read, _) = t
            .read("s|x", &unsaved, || now, || async move { good_at(now, account()) })
            .await;
        assert!(read.is_ok());
        t.save(&unsaved, now);
        assert_eq!(std::fs::read(&scope.saved).unwrap(), written, "the file changed");
        t.load_saved(&scope, now);
        assert_eq!(with_reading(&t, &["s|a"]), ["s|a"], "the saved scope still loads");
    }

    /// p1-r1 SF4: the writer saves only the slots of its store's listing. A
    /// slot that left the listing loses its reading (and so leaves the
    /// file); slots outside `covers`, or without saved readings, are kept.
    #[test]
    fn the_writer_saves_only_listed_slots() {
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_in(dir.path());
        let now = 100 * MIN;
        let account = || seat("r", Some("acct-1"));
        let t = writer(&scope);
        for key in ["s|a", "s|b", "s|other"] {
            assert!(t.record_at(key, good_at(now - MIN, account()), now).is_ok());
        }
        let _ = t.record("s|unsaved", good_at(now - MIN, account()), now, false);
        t.save(&scope, now);
        assert_eq!(saved_keys(&scope), ["s|a", "s|b", "s|other"]);

        let listed: HashSet<String> = ["s|a".to_string()].into();
        let covers = |key: &str| key != "s|other";
        t.keep_listed(&scope, covers, &listed, now + MIN);
        assert_eq!(
            with_reading(&t, &["s|a", "s|b", "s|other", "s|unsaved"]),
            ["s|a", "s|other", "s|unsaved"]
        );
        t.save(&scope, now + MIN);
        assert_eq!(saved_keys(&scope), ["s|a", "s|other"]);
    }

    /// What `f` logs (every level, this thread only), as text.
    fn logged<R>(f: impl FnOnce() -> R) -> (R, String) {
        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let sink = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || sink.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();
        let out = tracing::subscriber::with_default(subscriber, f);
        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        (out, text)
    }

    /// `scope`'s file holding readings of `s|a` and `s|b`, observed 2 min
    /// before `now`, written by a writer that then stopped.
    fn seed_a_and_b(scope: &ReadScope, now: u64) {
        let seed = writer(scope);
        for key in ["s|a", "s|b"] {
            assert!(seed
                .record_at(key, good_at(now - 2 * MIN, seat("r", Some("acct-1"))), now)
                .is_ok());
        }
        seed.save(scope, now);
        seed.stop_writing();
    }

    // ── One writer per file (p1-r2 B3, SF2) ─────────────────────────────

    /// p1-r2 B3, the deletion sequence of two daemons on one file: both ask
    /// to write it, and only X gets the lease (Y logs one INFO). Both load
    /// `a`; X drops it and saves; Y reads `b` and saves. A fresh load has no
    /// `a`: Y, a reader, never wrote its old copy back.
    #[tokio::test]
    async fn one_table_writes_a_file_so_a_dropped_reading_stays_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_in(dir.path());
        let now = 100 * MIN;
        let account = || seat("r", Some("acct-1"));
        seed_a_and_b(&scope, now);

        let (x, y) = (UsageReads::default(), UsageReads::default());
        assert!(x.write_saved_readings(&scope.saved), "X did not take the free lease");
        let (took, log) = logged(|| y.write_saved_readings(&scope.saved));
        assert!(!took, "Y took a held lease");
        assert!(log.contains("INFO") && log.contains("other_process"), "{log}");
        assert!(!log.contains("acct") && !log.contains("s|"), "{log}");
        assert!(x.saves_readings() && !y.saves_readings());
        x.load_saved(&scope, now);
        y.load_saved(&scope, now);
        assert!(x.forget_last_good("s|a", now + MIN));
        x.save(&scope, now + MIN);
        let (read, _) = y
            .read("s|b", &scope, || now + MIN, || async move {
                good_at(now + MIN, account())
            })
            .await;
        assert!(read.is_ok());
        y.save(&scope, now + MIN);

        let fresh = UsageReads::default();
        fresh.load_saved(&scope, now + 2 * MIN);
        assert!(fresh.saved_reading("s|a", &account()).is_none(), "a came back");
        assert_eq!(
            fresh.saved_reading("s|b", &account()).map(|s| s.observed_at),
            Some(now - 2 * MIN),
            "b as X saved it"
        );
    }

    /// p1-r2 B3, the pause: X (the writer) starts a pause and saves it; Y,
    /// which loaded the file before that, saves its own table. A fresh load
    /// still has the pause.
    #[test]
    fn a_table_that_is_not_the_writer_never_erases_a_saved_pause() {
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_in(dir.path());
        let now = 100 * MIN;
        seed_a_and_b(&scope, now);
        let x = writer(&scope);
        let y = UsageReads::default();
        assert!(!y.write_saved_readings(&scope.saved), "Y took a held lease");
        x.load_saved(&scope, now);
        y.load_saved(&scope, now);
        assert!(x.start_pause(&scope, &rate_limited(Some(120)), now + 2 * MIN));
        x.save(&scope, now);
        assert!(y
            .record_at("s|b", good_at(now, seat("r", Some("acct-1"))), now)
            .is_ok());
        y.save(&scope, now);

        let fresh = UsageReads::default();
        fresh.load_saved(&scope, now + MIN);
        assert!(fresh.paused_at(&scope, now + MIN).is_some(), "the pause was erased");
    }

    /// p1-r2 SF2: a writer that stops writes nothing more, and its lease is
    /// free: another table takes it and writes (loading the file first).
    /// The stopped table, which has loaded the file, cannot write again.
    #[test]
    fn a_stopped_writer_writes_nothing_and_its_lease_passes_on() {
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_in(dir.path());
        let now = 100 * MIN;
        let good = |at| good_at(at, seat("r", Some("acct-1")));
        let x = writer(&scope);
        assert!(x.record_at("s|a", good(now), now).is_ok());
        x.save(&scope, now);
        x.stop_writing();
        assert!(!x.saves_readings());
        let written = std::fs::read(&scope.saved).unwrap();
        assert!(x.record_at("s|b", good(now), now).is_ok());
        x.save(&scope, now);
        assert_eq!(std::fs::read(&scope.saved).unwrap(), written, "a stopped writer saved");

        let y = UsageReads::default();
        assert!(y.write_saved_readings(&scope.saved), "the lease was not passed on");
        assert!(y.record_at("s|c", good(now), now).is_ok());
        y.save(&scope, now);
        assert_eq!(saved_keys(&scope), ["s|a", "s|c"]);
        y.stop_writing();
        assert!(!x.write_saved_readings(&scope.saved), "a table that loaded the file wrote");
    }

    /// One lease per file: two stores' files have a writer each, a writer
    /// writes only its own file, and a third table gets neither.
    #[test]
    fn each_store_has_its_own_writer() {
        let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (first, second) = (scope_in(one.path()), scope_in(two.path()));
        let now = 100 * MIN;
        let x = writer(&first);
        let y = writer(&second);
        assert!(x.saves_readings() && y.saves_readings());
        let z = UsageReads::default();
        assert!(!z.write_saved_readings(&first.saved));
        assert!(!z.write_saved_readings(&second.saved));
        assert!(x
            .record_at("s|a", good_at(now, seat("r", Some("acct-1"))), now)
            .is_ok());
        x.save(&second, now);
        assert!(!second.saved.exists(), "a writer wrote another store's file");
        x.save(&first, now);
        assert!(first.saved.exists());
    }

    /// p1-r2 B3: who writes is decided before the table loads the file. A
    /// table that loaded it first stays a reader, with one WARN, even with
    /// the lease free; so does one whose lease can't be taken at all (here
    /// its directory is missing). Neither writes.
    #[test]
    fn a_table_that_loaded_the_file_first_stays_a_reader() {
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_in(dir.path());
        let now = 100 * MIN;
        let t = UsageReads::default();
        t.load_saved(&scope, now);
        let (took, log) = logged(|| t.write_saved_readings(&scope.saved));
        assert!(!took, "a table took the lease after it loaded the file");
        assert!(log.contains("WARN"), "{log}");
        assert!(!t.saves_readings());
        assert!(t
            .record_at("s|a", good_at(now, seat("r", Some("acct-1"))), now)
            .is_ok());
        t.save(&scope, now);
        assert!(!scope.saved.exists(), "a reader wrote");
        assert!(writer(&scope).saves_readings(), "the reader kept the lease");

        let missing = scope_in(&dir.path().join("missing"));
        let u = UsageReads::default();
        let (took, log) = logged(|| u.write_saved_readings(&missing.saved));
        assert!(!took && log.contains("WARN"), "{log}");
    }

    /// p1-r2 SF2: the lease's descriptor is closed on exec, so a process
    /// the daemon starts never holds it (and a reload's exec releases it).
    #[cfg(unix)]
    #[test]
    fn the_lease_is_closed_on_exec() {
        let dir = tempfile::tempdir().unwrap();
        let t = writer(&scope_in(dir.path()));
        let fd = t.lease_fd().expect("the writer holds no lease");
        // SAFETY: F_GETFD reads the descriptor flags of an open descriptor
        // this table owns; no memory is passed.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0, "fcntl failed");
        assert_ne!(flags & libc::FD_CLOEXEC, 0, "the lease survives an exec");
    }

    /// p1-r2 SF2 (R3-2): a stop waits for the save in flight. The writer's
    /// save is held inside its write; a stop on another thread, and the next
    /// writer's save after it, wait for that save to end, so the next
    /// writer's file is the last word. (The bounded wait only gives a stop
    /// that does not wait time to return and fail the test.)
    #[test]
    fn a_stop_waits_for_the_save_in_flight() {
        use std::sync::mpsc;
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_in(dir.path());
        let now = 100 * MIN;
        let good = move || good_at(now, seat("r", Some("acct-1")));
        let x = Arc::new(writer(&scope));
        assert!(x.record_at("s|x", good(), now).is_ok());
        let (entered, in_save) = mpsc::channel::<()>();
        let (go, gate) = mpsc::channel::<()>();
        x.on_save(move || {
            entered.send(()).unwrap();
            gate.recv().unwrap();
        });
        let saving = {
            let (x, scope) = (x.clone(), scope.clone());
            std::thread::spawn(move || x.save(&scope, now))
        };
        in_save.recv().unwrap();
        let (stopped, stop_done) = mpsc::channel::<bool>();
        let stopping = {
            let (x, scope) = (x.clone(), scope.clone());
            std::thread::spawn(move || {
                x.stop_writing();
                let y = UsageReads::default();
                let took = y.write_saved_readings(&scope.saved);
                assert!(y.record_at("s|y", good(), now).is_ok());
                y.save(&scope, now);
                stopped.send(took).unwrap();
            })
        };
        let early = stop_done.recv_timeout(Duration::from_millis(200)).ok();
        go.send(()).unwrap();
        saving.join().unwrap();
        let took = match early {
            Some(took) => took,
            None => stop_done.recv().unwrap(),
        };
        stopping.join().unwrap();
        assert!(early.is_none(), "the stop returned while the save was in flight");
        assert!(took, "the next writer did not get the lease");
        assert_eq!(saved_keys(&scope), ["s|x", "s|y"], "the stopped writer's save came last");
    }

    // ── A reading that lands after a drop (p1-r2 B1) ────────────────────

    /// p1-r2 B1 (R3-3): a good reading that lands after the slot's reading
    /// was dropped while it was in flight is discarded: the read (and a
    /// reader that joined it, who takes the slot's last result) gets
    /// [`ReadError::Superseded`]; nothing is installed or saved; it starts
    /// no hold and no pause, so the next read goes out, and counts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_good_reading_that_lands_after_a_drop_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_in(dir.path());
        let now = 100 * MIN;
        let account = || seat("r", Some("acct-1"));
        let t = Arc::new(writer(&scope));
        assert!(t.record_at("s|a", good_at(now - MIN, account()), now).is_ok());
        t.save(&scope, now);
        let (arrived, in_flight) = tokio::sync::oneshot::channel::<()>();
        let (land, landing) = tokio::sync::oneshot::channel::<()>();
        let flight = {
            let (t, scope) = (t.clone(), scope.clone());
            tokio::spawn(async move {
                t.read("s|a", &scope, || now, move || async move {
                    let _ = arrived.send(());
                    let _ = landing.await;
                    good_at(now, account())
                })
                .await
            })
        };
        in_flight.await.unwrap();
        // A reported limit (or a refused login) drops the reading meanwhile.
        assert!(t.forget_last_good("s|a", now));
        t.save(&scope, now);
        let written = std::fs::read(&scope.saved).unwrap();
        land.send(()).unwrap();
        let (read, path) = flight.await.unwrap();
        assert!(matches!(read, Err(ReadError::Superseded)), "{read:?}");
        assert_eq!(path, ReadPath::Network);
        assert!(
            matches!(
                t.entries().get("s|a").and_then(|e| e.last.clone()),
                Some(Err(ReadError::Superseded))
            ),
            "a reader that joined the flight would not get the discard"
        );
        assert!(t.last_good_for("s|a", &account()).is_none(), "the reading was installed");
        assert_eq!(std::fs::read(&scope.saved).unwrap(), written, "the reading was saved");
        assert!(t.held_at("s|a", now).is_none(), "the slot is held");
        assert!(t.paused_at(&scope, now).is_none(), "the reads are paused");
        let (read, path) = t
            .read("s|a", &scope, || now, || async move { good_at(now, account()) })
            .await;
        assert!(read.is_ok() && path == ReadPath::Network, "{read:?} {path:?}");
        assert!(t.last_good_for("s|a", &account()).is_some());
    }

    /// p1-r3 LOW2 (R4-4): a reader that joins a held flight gets its
    /// discard too. A second read of the slot starts while the first is out
    /// and waits to join it (it holds the slot's gate); a drop lands, then
    /// the first read's good reading: both reads get
    /// [`ReadError::Superseded`], and only the first sent a request.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reader_that_joins_a_discarded_flight_gets_the_discard() {
        use std::sync::atomic::AtomicUsize;
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_in(dir.path());
        let now = 100 * MIN;
        let account = || seat("r", Some("acct-1"));
        let t = Arc::new(writer(&scope));
        assert!(t.record_at("s|a", good_at(now - MIN, account()), now).is_ok());
        let fetched = Arc::new(AtomicUsize::new(0));
        let (arrived, in_flight) = tokio::sync::oneshot::channel::<()>();
        let (land, landing) = tokio::sync::oneshot::channel::<()>();
        let first = {
            let (t, scope, fetched) = (t.clone(), scope.clone(), fetched.clone());
            tokio::spawn(async move {
                t.read("s|a", &scope, || now, move || async move {
                    fetched.fetch_add(1, Ordering::SeqCst);
                    let _ = arrived.send(());
                    let _ = landing.await;
                    good_at(now, account())
                })
                .await
            })
        };
        in_flight.await.unwrap();
        // Who holds the slot's gate: the table and the first read (and this
        // probe, while it counts).
        let holders = || Arc::strong_count(&t.gate("s|a"));
        let alone = holders();
        let joiner = {
            let (t, scope, fetched) = (t.clone(), scope.clone(), fetched.clone());
            tokio::spawn(async move {
                t.read("s|a", &scope, || now, move || async move {
                    fetched.fetch_add(1, Ordering::SeqCst);
                    good_at(now, account())
                })
                .await
            })
        };
        // The second read takes the gate only after it has taken the
        // slot's generation: from then on it joins the flight.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while holders() == alone {
            assert!(
                std::time::Instant::now() < deadline,
                "the second read never waited for the flight"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(t.forget_last_good("s|a", now));
        land.send(()).unwrap();
        let (read, path) = first.await.unwrap();
        assert!(matches!(read, Err(ReadError::Superseded)), "{read:?}");
        assert_eq!(path, ReadPath::Network);
        let (read, path) = joiner.await.unwrap();
        assert!(matches!(read, Err(ReadError::Superseded)), "the joined read: {read:?}");
        assert_eq!(path, ReadPath::Joined);
        assert_eq!(fetched.load(Ordering::SeqCst), 1, "the joined read sent a request");
        assert!(t.last_good_for("s|a", &account()).is_none());
    }

    /// p1-r2 SF1 (R3-4): pruning loads the saved readings first. A file with
    /// `a` and `b` and a listing with only `a`: the table prunes before it
    /// does anything else, then reads `a`; the file it writes has no `b`.
    #[tokio::test]
    async fn pruning_loads_the_saved_readings_first() {
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_in(dir.path());
        let now = 100 * MIN;
        seed_a_and_b(&scope, now);
        let t = writer(&scope);
        let listed: HashSet<String> = ["s|a".to_string()].into();
        t.keep_listed(&scope, |_: &str| true, &listed, now);
        let (read, _) = t
            .read("s|a", &scope, || now, || async move {
                good_at(now, seat("r", Some("acct-1")))
            })
            .await;
        assert!(read.is_ok());
        assert_eq!(saved_keys(&scope), ["s|a"], "the removed slot was written back");
    }

    /// p1-r1 LOW1: a save whose temporary file already exists (not created
    /// by this call) fails, and leaves that file and its bytes alone.
    #[test]
    fn a_save_leaves_a_temporary_file_it_did_not_create_alone() {
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_in(dir.path());
        let now = 100 * MIN;
        let name = ".usage-last-good.json.taken.tmp";
        let taken = dir.path().join(name);
        std::fs::write(&taken, b"another writer's bytes").unwrap();
        let t = writer(&scope);
        assert!(t
            .record_at("s|a", good_at(now, seat("r", Some("acct-1"))), now)
            .is_ok());
        TEMP_NAME.with(|n| *n.borrow_mut() = Some(name.to_string()));
        t.save(&scope, now);
        assert_eq!(std::fs::read(&taken).unwrap(), b"another writer's bytes");
        assert!(!scope.saved.exists(), "the save wrote through a file it did not create");
    }
}
