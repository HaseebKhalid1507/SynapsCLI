//! A session's entry in the daemon's crash record (`daemon::crash_record`).
//!
//! The actor publishes what a restore after a crash needs into its handle
//! ([`LiveSlot`]): the same [`SessionReloadRecord`] a reload checkpoint
//! replies with, whether a turn is in flight, and the workers it saw
//! running. It publishes at create, at every turn start and end, on a
//! lifecycle change (park/unpark), after a settings change and when the set
//! of running workers changes, then marks the record dirty ([`mark_dirty`]).
//! The daemon's writer reads every handle's slot without asking the actor
//! (an actor deep in a tool call still has a current entry) and rewrites the
//! record: at once after a turn start (`urgent`), else at most once a
//! second.
//!
//! [`process_key`] names this process instance (`pid:start_ticks:boot_id`).
//! The crash record names its writer by it, and a turn draft carries it
//! (`TurnDraft::writer`), so a restore can tell the draft of the turn the
//! crash cut from one a later process left.

use std::sync::{Arc, OnceLock};

use tokio::sync::watch;

use super::types::SessionReloadRecord;
use crate::proc_identity::{BootKey, ProcessIdentity};

/// What the actor last published.
#[derive(Clone, Debug)]
pub struct LiveEntry {
    pub record: SessionReloadRecord,
    /// A turn was running when the actor published this entry.
    pub turn_in_flight: bool,
    /// Handle ids (`sa_N`) of the workers that were running.
    pub running_workers: Vec<String>,
    /// When a restore (after a crash or a clean stop) last resumed this
    /// session's cut turn in this process; a crash record restore that
    /// finds it recent suspects a crash loop and doesn't resume again.
    pub resumed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Wall-clock time the session parked (`None` while Live), so a restore
    /// keeps its eviction timer instead of starting it over.
    pub parked_since: Option<chrono::DateTime<chrono::Utc>>,
    /// The actor's publish counter (only increases): what the record's
    /// writer acknowledges ([`LiveSlot::durable`]).
    pub gen: u64,
}

/// The handle's cell, shared with the actor. Empty until the actor
/// publishes (echo actors and lock-held placeholders never do, so they are
/// never recorded). Beside the entry: how far this daemon's crash record
/// writer has durably written it ([`Durable`]).
#[derive(Clone)]
pub struct LiveSlot {
    entry: Arc<arc_swap::ArcSwapOption<LiveEntry>>,
    durable: Arc<watch::Sender<Durable>>,
}

impl Default for LiveSlot {
    fn default() -> Self {
        Self {
            entry: Arc::default(),
            durable: Arc::new(watch::channel(Durable::default()).0),
        }
    }
}

/// A successful crash-record write that held this slot's entry: the change
/// count ([`changes_now`]) read before its snapshot, and the entry's
/// [`LiveEntry::gen`]. Only this daemon's writer stamps it, and only after
/// the write succeeded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Durable {
    pub changes: u64,
    pub gen: u64,
}

impl LiveSlot {
    pub fn store(&self, entry: Option<Arc<LiveEntry>>) {
        self.entry.store(entry);
    }

    pub fn load(&self) -> arc_swap::Guard<Option<Arc<LiveEntry>>> {
        self.entry.load()
    }

    pub fn load_full(&self) -> Option<Arc<LiveEntry>> {
        self.entry.load_full()
    }

    /// The writer wrote this slot's entry `gen` in a write whose snapshot
    /// started at change count `changes`. The writer stamps under the same
    /// gate as the write, so stamps arrive in the order the records were
    /// written and the mark is the pair of the record on disk. A snapshot
    /// that started before the last written one is never written, so a
    /// lower count never arrives after a higher one; two snapshots that
    /// started at the same count may land in either order (the count alone
    /// doesn't order them), and the later write's stamp replaces the mark.
    /// What makes an acknowledgement safe is [`Self::await_durable`]'s
    /// `after`, read after the actor's own publish.
    pub fn stamp_durable(&self, changes: u64, gen: u64) {
        self.durable.send_if_modified(|d| {
            let next = Durable { changes, gen };
            if changes < d.changes || next == *d {
                return false;
            }
            *d = next;
            true
        });
    }

    /// The last acknowledged write.
    pub fn durable(&self) -> Durable {
        *self.durable.borrow()
    }

    /// Wait, at most `within`, for a write that started at or after change
    /// count `after` (read after the actor's own publish) and holds `gen` or
    /// a later entry: the acknowledgement that the entry is on disk and no
    /// older snapshot can replace it. `false`: none came in time.
    pub async fn await_durable(&self, after: u64, gen: u64, within: std::time::Duration) -> bool {
        let mut rx = self.durable.subscribe();
        let acked = |d: &Durable| d.changes >= after && d.gen >= gen;
        tokio::time::timeout(within, rx.wait_for(acked))
            .await
            .is_ok_and(|r| r.is_ok())
    }
}

/// Capacity-resume admission against this daemon's crash record: present
/// on the host only while a crash record is kept (`daemon.crash_restore`
/// on and a nameable process); without it no capacity resume is armed,
/// held or dispatched. A stop or a reload hand-off SEALS it before it takes
/// its snapshot, and a resume's final admission ([`Self::admit`]) is
/// linearized against that seal under one mutex: either the admission came
/// first (and the snapshot, taken after the seal, sees the consumed entry
/// the actor published before it), or the resume is not dispatched.
#[derive(Debug, Default)]
pub struct RecordLink {
    seal: std::sync::Mutex<Seal>,
}

#[derive(Debug, Default)]
struct Seal {
    epoch: u64,
    /// Every seal still in force, by epoch: a stop's and a reload's may
    /// overlap in either order, and reopening one leaves the other.
    active: Vec<u64>,
}

impl RecordLink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Close admission before a stop's or a hand-off's snapshot. The epoch
    /// reopens it ([`Self::unseal`]) if that hand-off is abandoned.
    pub fn seal(&self) -> u64 {
        let mut s = self.seal.lock().unwrap_or_else(|p| p.into_inner());
        s.epoch += 1;
        let epoch = s.epoch;
        s.active.push(epoch);
        epoch
    }

    /// Lift the seal `epoch` took (a reload whose exec failed). Any other
    /// seal stays, whichever came first: a stop never unseals, so a stop's
    /// seal holds for the rest of the process.
    pub fn unseal(&self, epoch: u64) {
        let mut s = self.seal.lock().unwrap_or_else(|p| p.into_inner());
        s.active.retain(|e| *e != epoch);
    }

    /// A capacity resume's final admission: `true` while nothing sealed.
    /// The caller published its consumed entry BEFORE this call; a seal
    /// taken after it (under the same mutex) is followed by a snapshot that
    /// sees that entry.
    pub fn admit(&self) -> bool {
        self.seal.lock().unwrap_or_else(|p| p.into_inner()).active.is_empty()
    }
}

/// The change count now ([`mark_dirty`] raises it on every publish).
pub fn changes_now() -> u64 {
    changes().borrow().any
}

/// Change counters: `any` on every publish, `urgent` on a turn start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub any: u64,
    pub urgent: u64,
}

fn changes() -> &'static watch::Sender<Changes> {
    static CHANGES: OnceLock<watch::Sender<Changes>> = OnceLock::new();
    CHANGES.get_or_init(|| watch::channel(Changes::default()).0)
}

/// Something a crash record holds changed (a publish, a session added or
/// removed). `urgent`: a turn started, so the writer must not wait out its
/// debounce. A no-op cost when no daemon writer listens.
pub fn mark_dirty(urgent: bool) {
    changes().send_modify(|c| {
        c.any = c.any.wrapping_add(1);
        if urgent {
            c.urgent = c.urgent.wrapping_add(1);
        }
    });
}

/// The daemon writer's view of [`mark_dirty`].
pub fn subscribe() -> watch::Receiver<Changes> {
    changes().subscribe()
}

/// This process instance: `pid:start:boot`, on Linux
/// `pid:start_ticks:boot_id` from `/proc`, on macOS the kernel's unique id
/// and the boot session. `None` where processes can't be identified: then
/// no crash record is kept and no draft is attributed.
pub fn process_key() -> Option<&'static str> {
    static KEY: OnceLock<Option<String>> = OnceLock::new();
    KEY.get_or_init(|| {
        let pid = std::process::id();
        let start = ProcessIdentity::identify(pid).ok()?.start.number();
        Some(key_of(pid, start, ProcessIdentity::boot_key().map(BootKey::as_str)))
    })
    .as_deref()
}

/// The key format [`process_key`] and the crash record's writer share.
pub fn key_of(pid: u32, start_ticks: u64, boot_id: Option<&str>) -> String {
    format!("{pid}:{start_ticks}:{}", boot_id.unwrap_or("-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_process_key_names_this_process_and_is_stable() {
        if !ProcessIdentity::supported() {
            assert!(process_key().is_none());
            return;
        }
        let key = process_key().expect("a key where processes are identified");
        assert_eq!(Some(key), process_key());
        let pid = std::process::id().to_string();
        let mut parts = key.split(':');
        assert_eq!(parts.next(), Some(pid.as_str()));
        let number: u64 = parts.next().unwrap().parse().unwrap();
        assert_eq!(number, ProcessIdentity::identify(std::process::id()).unwrap().start.number());
        assert_eq!(parts.next(), ProcessIdentity::boot_key().map(BootKey::as_str));
        if !cfg!(target_os = "linux") {
            return;
        }
        // As the kernel prints it: field 22 of the stat line and the boot id.
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        let field22 = stat[stat.rfind(')').unwrap() + 1..].split_whitespace().nth(19).unwrap().to_string();
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
        assert_eq!(key, format!("{pid}:{field22}:{}", boot.trim()));
        assert_eq!(key_of(4242, 77, None), "4242:77:-");
    }

    /// A write that started earlier never replaces the mark.
    #[test]
    fn the_durable_mark_only_moves_up() {
        let slot = LiveSlot::default();
        assert_eq!(slot.durable(), Durable::default());
        slot.stamp_durable(5, 2);
        assert_eq!(slot.durable(), Durable { changes: 5, gen: 2 });
        slot.stamp_durable(3, 1);
        assert_eq!(slot.durable(), Durable { changes: 5, gen: 2 }, "an older write never lowers it");
        slot.stamp_durable(9, 4);
        assert_eq!(slot.durable(), Durable { changes: 9, gen: 4 });
    }

    /// The acknowledgement needs a write that started after the publish AND
    /// holds its entry; anything less, or nothing in time, is no ACK.
    #[tokio::test(start_paused = true)]
    async fn the_ack_needs_a_later_write_that_holds_the_entry() {
        let wait = std::time::Duration::from_secs(5);
        let slot = LiveSlot::default();
        assert!(!slot.await_durable(10, 3, wait).await, "no write at all");
        slot.stamp_durable(9, 3);
        assert!(!slot.await_durable(10, 3, wait).await, "a write that started before the publish");
        slot.stamp_durable(12, 2);
        assert!(!slot.await_durable(10, 3, wait).await, "a later write of an older entry");
        let s = slot.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            s.stamp_durable(12, 3);
        });
        let t0 = tokio::time::Instant::now();
        assert!(slot.await_durable(10, 3, wait).await);
        assert_eq!(t0.elapsed(), std::time::Duration::from_secs(1), "answered at the stamp");
        assert!(slot.await_durable(10, 3, wait).await, "already acknowledged");
    }

    /// A seal closes admission; only its own epoch reopens it, so a
    /// failed reload's reopen never lifts a later stop's seal.
    #[test]
    fn a_seal_closes_admission_and_only_its_own_epoch_reopens_it() {
        let link = RecordLink::new();
        assert!(link.admit());
        let reload = link.seal();
        assert!(!link.admit());
        link.unseal(reload);
        assert!(link.admit(), "the failed reload reopens its own seal");
        let reload = link.seal();
        let stop = link.seal();
        assert_ne!(reload, stop);
        link.unseal(reload);
        assert!(!link.admit(), "the stop's seal stays");
        link.unseal(stop);
        assert!(link.admit());
    }

    /// The opposite order: a stop seals, a reload already preparing on
    /// another connection seals after it, and its exec fails. Reopening
    /// the reload's seal leaves the stop's in place.
    #[test]
    fn a_stop_seal_survives_a_later_reload_seal_being_reopened() {
        let link = RecordLink::new();
        let stop = link.seal();
        let reload = link.seal();
        link.unseal(reload);
        assert!(!link.admit(), "the stop's seal stays");
        link.unseal(reload);
        assert!(!link.admit(), "reopening the reload's seal again changes nothing");
        link.unseal(stop);
        assert!(link.admit());
    }

    /// Both orders of a resume's admission and a stop's snapshot, raced: a
    /// resume that was admitted (dispatched) is never in the stop's
    /// snapshot as still armed, because the actor published the consumed
    /// entry before its admission and the stop reads after its seal.
    #[test]
    fn an_admitted_resume_is_never_snapshotted_armed_after_a_seal() {
        let (mut admitted, mut refused) = (0, 0);
        for _ in 0..2000 {
            let link = Arc::new(RecordLink::new());
            let armed = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let go = Arc::new(std::sync::Barrier::new(2));
            let actor = {
                let (link, armed, go) = (Arc::clone(&link), Arc::clone(&armed), Arc::clone(&go));
                std::thread::spawn(move || {
                    go.wait();
                    armed.store(false, std::sync::atomic::Ordering::Relaxed);
                    link.admit()
                })
            };
            go.wait();
            link.seal();
            let snapshot_armed = armed.load(std::sync::atomic::Ordering::Relaxed);
            let dispatched = actor.join().unwrap();
            assert!(!(dispatched && snapshot_armed), "a dispatched resume recorded as still armed");
            if dispatched {
                admitted += 1;
            } else {
                refused += 1;
            }
        }
        assert!(admitted + refused == 2000);
    }

    #[test]
    fn a_turn_start_is_urgent_and_every_publish_counts() {
        let mut rx = subscribe();
        let before = *rx.borrow_and_update();
        mark_dirty(false);
        let after = *rx.borrow_and_update();
        assert!(after.any != before.any);
        mark_dirty(true);
        let urgent = *rx.borrow_and_update();
        assert!(urgent.urgent != after.urgent && urgent.any != after.any);
    }
}
