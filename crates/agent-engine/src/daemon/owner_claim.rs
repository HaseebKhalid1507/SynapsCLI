//! The owner claim: which daemon link is the owner's own app.
//!
//! The owner's app (a desktop chat app the owner installs) holds a 32-byte
//! key and sends it on each link as `claim_owner` after `Welcome`. The
//! daemon keeps only the key's SHA-256
//! ([`OwnerKey::digest_and_wipe`](crate::session::wire::OwnerKey)) and
//! answers each claim with `owner_claim {owner_bound, reason?}`. A bound
//! link's turns can be marked as the owner's in any session, owned or not
//! (`from_sender` in runtime/turn_origin.rs).
//!
//! - **Image check, at each claim.** The claimant is the process that wrote
//!   the claim frame (its tgid from `SCM_CREDENTIALS`, `frame_creds`). Its
//!   `/proc/<pid>/exe` must be the very file (dev, ino) the owner app path
//!   names now: env [`APP_EXE_ENV`] when set and non-empty, else config
//!   `daemon.owner_app_exe` ([`app_exe`]); the path is resolved at each
//!   claim, so an install that repoints a stable `current` link needs no
//!   daemon restart. The env is read once, at the daemon's start, and
//!   removed from its own environment ([`take_app_exe_env`]), so no child
//!   the daemon starts inherits it. A claimant whose exe can't be read or
//!   stat'd (a non-dumpable process: its `/proc` entries are root's) fails
//!   closed (`not_the_app`), as does one started with a loader or webview
//!   remote-control env ([`REFUSED_ENV`]). Neither path set (or not Linux):
//!   `unsupported`.
//! - **States.** Vacant → the first claim that passes registers {digest,
//!   pid + start ticks, image dev/ino} → Settling for G ([`SETTLE_ENV`],
//!   default 15 s, timed from that registration, not from the daemon's
//!   start) → Bound, pushed `owner_bound:true` to every link that proved
//!   the key. Contested means nobody: a second key while Settling or Bound
//!   unbinds every link and pushes `contested` to every link that claimed;
//!   it holds until a cold start or [`OwnerClaims::reset`]. The right key
//!   from another process is `not_the_registrant`, told only to that link
//!   (the bound links stay bound). Reset is denial only: Vacant, `reset`
//!   pushed, nothing binds by itself. The registrant's exit (its pidfd turns
//!   readable) → Vacant at once, every link unproved and free to claim again
//!   (nothing is pushed: `reason` is absent only when bound, and the
//!   registrant's own links are gone with it); while it lives the
//!   registration holds, links or none.
//! - **Per frame.** A Submit/Steer counts as the owner's only when Bound
//!   ([`OwnerClaims::is_owner`]), its link proved the key, its bytes came
//!   from the registrant's tgid (a child on an inherited fd goes plain),
//!   that process still runs the registered image (its `/proc/<pid>/exe`
//!   is stat'd per frame: one that execs another image loses its marks),
//!   and the frame starts at or after the link's mark. The mark is taken
//!   when the claim passes the image check: the bytes read so far plus
//!   those still queued in the socket (`frame_creds::CredRead::horizon`).
//!   A frame written before the check (by a script that claimed, queued
//!   frames, then exec'd the app at the same pid) starts before it.
//! - **Pushes are not dropped.** A push or reply that finds the link's
//!   queue full is held (the latest one: each states the link's whole
//!   claim) and sent as soon as there is room, before any later one.
//! - **Only claimed links hear of it.** `owner_claim` frames (replies and
//!   pushes) go only to links that sent `claim_owner`, plus the reply to
//!   `owner_reset`: an older client's `DaemonFrame` is closed and would drop
//!   the link on an unknown type. A claim's reply is queued under the same
//!   lock as the state change it reports, so a later push never overtakes it.
//! - **Reload.** [`OwnerCarry`] rides `ReloadState` (state, digest hex,
//!   pid + start, dev/ino); the new image runs the image check again on the
//!   carried registrant against the app path as it is now (gone, another
//!   start time, another image or a refused env → Vacant) and links
//!   re-prove the key with no new registration.
//!
//! Logs: one line per claim (pid, exe, verdict) and per origin-carrying
//! command (`from_sender` in runtime/turn_origin.rs); never the key, its
//! digest or any text.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::session::wire::{DaemonFrame, OwnerClaimReason as Reason};

/// The owner app's path, set in the daemon's environment by the owner app's
/// service unit. Wins over `daemon.owner_app_exe` when set and non-empty.
/// Read once at the daemon's start ([`take_app_exe_env`]); no child of the
/// daemon inherits it.
pub const APP_EXE_ENV: &str = "SYNAPS_DAEMON_OWNER_APP_EXE";
/// The settle window G in seconds (read at daemon start).
pub const SETTLE_ENV: &str = "SYNAPS_DAEMON_OWNER_SETTLE_SECS";
pub const DEFAULT_SETTLE: Duration = Duration::from_secs(15);
/// The answer to a second `claim_owner` on a link whose claim is live.
pub const DUPLICATE_CLAIM: &str = "owner claim already made on this connection";

/// A claimant started with one of these set (non-empty) is refused
/// (`not_the_app`): each loads other code into the app (the loader's
/// preload, audit and search path) or lets another process drive its
/// webview (WebKit's remote inspector, Tauri's WebDriver automation). The
/// app takes no argument that changes what it runs, so its args are not
/// read. The list is best effort, not a sandbox.
pub const REFUSED_ENV: [&str; 6] = [
    "LD_PRELOAD",
    "LD_AUDIT",
    "LD_LIBRARY_PATH",
    "WEBKIT_INSPECTOR_SERVER",
    "WEBKIT_INSPECTOR_HTTP_SERVER",
    "TAURI_WEBVIEW_AUTOMATION",
];

/// The owner app's path: `env` when set and non-empty, else `config`
/// (ignored whenever env is set).
pub fn app_exe(env: Option<&str>, config: Option<&str>) -> Option<PathBuf> {
    let set = |v: Option<&str>| v.map(str::trim).filter(|v| !v.is_empty()).map(PathBuf::from);
    set(env).or_else(|| set(config))
}

/// [`APP_EXE_ENV`]'s value (`None` when unset or not Unicode), removed from
/// this process's environment, so every child the daemon starts after this
/// (a tool's shell or PTY, an extension, an MCP server and the `${VAR}`
/// expansion of its config, a hook's or a skill's command, a subagent's
/// tools) lacks it. The daemon calls it first thing at start, before the
/// host boot starts any process; a reload hands the value to the next image
/// explicitly ([`OwnerClaims::env_exe`]). When `main` already moved it
/// aside ([`stash_app_exe_env`]) this takes it from there.
pub fn take_app_exe_env() -> Option<String> {
    if let Some(stash) = STASH.get() {
        return stash.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
    let value = std::env::var_os(APP_EXE_ENV)?;
    std::env::remove_var(APP_EXE_ENV);
    value.into_string().ok()
}

/// [`APP_EXE_ENV`] as `main` moved it out of the environment.
static STASH: std::sync::OnceLock<Mutex<Option<String>>> = std::sync::OnceLock::new();

/// Called by `main` before it builds the tokio runtime, while the process
/// has one thread, so `remove_var` races no `getenv`: moves
/// [`APP_EXE_ENV`] out of the environment into a stash that
/// [`take_app_exe_env`] (the daemon's start) takes and
/// [`stashed_app_exe_env`] (a client starting a daemon) reads. Once only.
pub fn stash_app_exe_env() {
    let value = std::env::var_os(APP_EXE_ENV);
    if value.is_some() {
        std::env::remove_var(APP_EXE_ENV);
    }
    let _ = STASH.set(Mutex::new(value.and_then(|v| v.into_string().ok())));
}

/// The stashed [`APP_EXE_ENV`] (left in place), for a client that starts a
/// daemon to hand on; `None` when nothing was stashed or it was taken.
pub fn stashed_app_exe_env() -> Option<String> {
    STASH.get().and_then(|stash| stash.lock().unwrap_or_else(|e| e.into_inner()).clone())
}

/// G from [`SETTLE_ENV`], else [`DEFAULT_SETTLE`].
pub fn settle_from_env() -> Duration {
    std::env::var(SETTLE_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(DEFAULT_SETTLE, Duration::from_secs)
}

pub type LinkId = u64;

/// What became of a frame [`OwnerClaims`] queued on a link.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Delivery {
    /// Queued on the link's writer.
    Queued,
    /// The writer is gone (the connection is ending).
    Closed,
    /// The queue was full: the caller sends it (awaiting room).
    Pending(DaemonFrame),
}

/// The daemon's owner claim state (one per daemon image).
pub struct OwnerClaims {
    inner: Mutex<Inner>,
    settle: Duration,
    env_exe: Option<String>,
    config_exe: Option<String>,
    me: Weak<OwnerClaims>,
}

#[derive(Default)]
struct Inner {
    state: State,
    /// Bumped whenever a registration begins or ends: a settle timer or a
    /// lapse watcher of an older one finds it changed and does nothing.
    epoch: u64,
    links: HashMap<LinkId, Link>,
    next_link: LinkId,
}

#[derive(Default)]
enum State {
    #[default]
    Vacant,
    Settling(Registration),
    Bound(Registration),
    Contested,
}

impl State {
    fn name(&self) -> &'static str {
        match self {
            State::Vacant => "vacant",
            State::Settling(_) => "settling",
            State::Bound(_) => "bound",
            State::Contested => "contested",
        }
    }
}

struct Link {
    /// Weak: a link never keeps its connection's writer open.
    tx: mpsc::WeakSender<DaemonFrame>,
    /// It sent `claim_owner` at least once: only such a link hears of it.
    claimed: bool,
    /// A claim was answered and nothing has unbound it since.
    live: bool,
    /// It proved the key from the registrant's process.
    proved: bool,
    /// Where this link's proof starts to count: the socket offset
    /// (`CredRead::horizon`) when its claim passed the image check. A frame
    /// that starts before it was written before that check.
    mark: u64,
    /// The latest `owner_claim` frame that found the queue full, sent by
    /// [`flush`] once there is room (a later one replaces it: each states
    /// the link's whole claim).
    owed: Option<DaemonFrame>,
    /// A [`flush`] task is sending `owed`: later frames queue behind it.
    flushing: bool,
}

impl Link {
    fn deliver(&self, frame: DaemonFrame) -> Delivery {
        let Some(tx) = self.tx.upgrade() else {
            return Delivery::Closed;
        };
        match tx.try_send(frame) {
            Ok(()) => Delivery::Queued,
            Err(mpsc::error::TrySendError::Full(frame)) => Delivery::Pending(frame),
            Err(mpsc::error::TrySendError::Closed(_)) => Delivery::Closed,
        }
    }

    /// An `owner_claim` frame for this link (`id`), in order and never
    /// dropped while the link lives: queued now, or held and sent by a
    /// [`flush`] task as soon as there is room.
    fn push(&mut self, claims: &Weak<OwnerClaims>, id: LinkId, frame: DaemonFrame) -> Delivery {
        if self.flushing {
            self.owed = Some(frame);
            return Delivery::Queued;
        }
        match self.deliver(frame) {
            Delivery::Pending(frame) => {
                tracing::info!(link = id, "owner claim: the link's queue is full; the frame goes out once there is room");
                self.owed = Some(frame);
                self.flushing = true;
                flush(claims.clone(), id, self.tx.clone());
                Delivery::Queued
            }
            delivery => delivery,
        }
    }
}

/// Send link `id`'s owed frame (and any that replaces it meanwhile) as room
/// frees up; stops when nothing is owed, or the link or its writer is gone.
fn flush(claims: Weak<OwnerClaims>, id: LinkId, tx: mpsc::WeakSender<DaemonFrame>) {
    tokio::spawn(async move {
        loop {
            let frame = {
                let Some(claims) = claims.upgrade() else { return };
                let mut inner = claims.lock();
                let Some(link) = inner.links.get_mut(&id) else { return };
                match link.owed.take() {
                    Some(frame) => frame,
                    None => {
                        link.flushing = false;
                        return;
                    }
                }
            };
            let Some(tx) = tx.upgrade() else { return };
            if tx.send(frame).await.is_err() {
                return;
            }
        }
    });
}

struct Registration {
    digest: [u8; 32],
    pid: u32,
    start_ticks: u64,
    dev: u64,
    ino: u64,
    alive: Arc<Liveness>,
}

/// The registrant's pidfd (readable once it exits).
struct Liveness {
    #[cfg(target_os = "linux")]
    fd: std::os::fd::OwnedFd,
}

impl Liveness {
    fn running(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            running(&self.fd)
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }
}

/// The registrant, carried across a reload in `ReloadState` (its digest as
/// hex: the only place the digest is ever written).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerCarry {
    pub state: CarriedState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registrant: Option<CarriedRegistrant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CarriedState {
    Settling,
    Bound,
    Contested,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CarriedRegistrant {
    pub sha256: String,
    pub pid: u32,
    pub start_ticks: u64,
    pub dev: u64,
    pub ino: u64,
}

/// A link's place in [`OwnerClaims`]; dropping it removes the link (its
/// registration, if any, holds while the registrant lives).
pub struct LinkGuard {
    claims: Arc<OwnerClaims>,
    pub id: LinkId,
}

impl Drop for LinkGuard {
    fn drop(&mut self) {
        self.claims.lock().links.remove(&self.id);
    }
}

fn frame(owner_bound: bool, reason: Option<Reason>) -> DaemonFrame {
    DaemonFrame::OwnerClaim { owner_bound, reason }
}

/// Constant-time digest compare.
fn same_digest(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    std::hint::black_box(diff) == 0
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<[u8; 32]> {
    let raw = text.as_bytes();
    if raw.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in raw.chunks_exact(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

/// The exe a pid runs, for a log line only (never proof).
pub fn exe_of(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok().map(|p| p.display().to_string())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// A claimant that passed the image check.
struct Claimant {
    pid: u32,
    start_ticks: u64,
    dev: u64,
    ino: u64,
    alive: Liveness,
}

impl OwnerClaims {
    /// `env_exe`: [`APP_EXE_ENV`] as [`take_app_exe_env`] read it at start;
    /// `config_exe`: `daemon.owner_app_exe`.
    pub fn new(settle: Duration, env_exe: Option<String>, config_exe: Option<String>) -> Arc<Self> {
        Arc::new_cyclic(|me| Self { inner: Mutex::new(Inner::default()), settle, env_exe, config_exe, me: me.clone() })
    }

    /// [`APP_EXE_ENV`] as read at start, for a reload to hand to the next
    /// image (which reads and removes it in turn).
    pub fn env_exe(&self) -> Option<&str> {
        self.env_exe.as_deref()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The owner app path: [`APP_EXE_ENV`] (as read at start) wins over the
    /// config value. The caller resolves it to a file at each claim.
    pub fn app_path(&self) -> Option<PathBuf> {
        app_exe(self.env_exe.as_deref(), self.config_exe.as_deref())
    }

    /// Add a link (one per connection); `tx` is its frame queue.
    pub fn link(self: &Arc<Self>, tx: &mpsc::Sender<DaemonFrame>) -> LinkGuard {
        let mut inner = self.lock();
        inner.next_link += 1;
        let id = inner.next_link;
        inner.links.insert(
            id,
            Link { tx: tx.downgrade(), claimed: false, live: false, proved: false, mark: u64::MAX, owed: None, flushing: false },
        );
        LinkGuard { claims: Arc::clone(self), id }
    }

    /// The state's name (`vacant`, `settling`, `bound`, `contested`).
    pub fn state_name(&self) -> &'static str {
        self.lock().state.name()
    }

    /// `link`'s claim with `digest` (the key already hashed and wiped),
    /// written by `claimant` (the frame's tgid; `None` when its bytes came
    /// from more than one process or could not be told). `horizon` is the
    /// link's socket offset that no byte written so far ends past
    /// (`CredRead::horizon`); it is read once the image check has passed
    /// and becomes the link's mark (`None`: fail closed, `not_the_app`).
    /// The reply is queued on the link under the lock that decided it;
    /// `Err` (a second claim while this link's is live) is the caller's to
    /// send.
    pub fn claim(
        &self,
        link: LinkId,
        digest: [u8; 32],
        claimant: Option<u32>,
        horizon: impl FnOnce() -> Option<u64>,
    ) -> Result<Delivery, &'static str> {
        {
            let mut inner = self.lock();
            let Some(l) = inner.links.get_mut(&link) else {
                return Err("owner claim on an unknown link");
            };
            if l.live {
                return Err(DUPLICATE_CLAIM);
            }
            l.claimed = true;
            l.live = true;
        }
        // The lock is not held over the image check (it reads /proc). A
        // reset or a lapse in this gap finds this link live and unproved:
        // it sets `live = false` (and a reset pushes `reset` here), then
        // `decide` below runs on the state as it is then and its reply goes
        // out after that push. Benign: the client's last frame is the state
        // now, and the link may claim again sooner.
        let exe = claimant.and_then(exe_of);
        // The mark is read after the check passed: from then on the pid
        // runs the app image, so a byte it writes later is past the mark.
        let checked = self.check(claimant).and_then(|claimant| match horizon() {
            Some(mark) => Ok((claimant, mark)),
            None => Err((Reason::NotTheApp, "the link's queued bytes can't be measured (fail closed)")),
        });
        let mut inner = self.lock();
        let (reply, why) = match checked {
            Err((reason, why)) => (frame(false, Some(reason)), why),
            Ok((claimant, mark)) => self.decide(&mut inner, link, &digest, claimant, mark),
        };
        let verdict = match &reply {
            DaemonFrame::OwnerClaim { owner_bound: true, .. } => "bound",
            DaemonFrame::OwnerClaim { reason: Some(reason), .. } => reason.as_str(),
            _ => "refused",
        };
        tracing::info!(link, pid = claimant, exe = exe.as_deref().unwrap_or("-"), verdict, why, "owner claim");
        Ok(match inner.links.get_mut(&link) {
            Some(l) => l.push(&self.me, link, reply),
            None => Delivery::Closed,
        })
    }

    /// The checks that need no state: a path is set, the platform can
    /// check, the writer is known and passes the image check.
    fn check(&self, claimant: Option<u32>) -> Result<Claimant, (Reason, &'static str)> {
        let Some(path) = self.app_path() else {
            return Err((Reason::Unsupported, "no owner app path is set"));
        };
        if !cfg!(target_os = "linux") {
            return Err((Reason::Unsupported, "no image check on this platform"));
        }
        let Some(pid) = claimant else {
            return Err((Reason::NotTheApp, "the claim's writer could not be told"));
        };
        check_image(pid, &path).map_err(|why| (Reason::NotTheApp, why))
    }

    /// `mark`: the link's socket offset when the image check passed; a
    /// frame on it counts only from there on ([`Self::is_owner`]).
    fn decide(&self, inner: &mut Inner, link: LinkId, digest: &[u8; 32], claimant: Claimant, mark: u64) -> (DaemonFrame, &'static str) {
        match &inner.state {
            State::Contested => (frame(false, Some(Reason::Contested)), "contested"),
            State::Vacant => {
                inner.epoch += 1;
                let reg = Registration {
                    digest: *digest,
                    pid: claimant.pid,
                    start_ticks: claimant.start_ticks,
                    dev: claimant.dev,
                    ino: claimant.ino,
                    alive: Arc::new(claimant.alive),
                };
                self.watch(inner.epoch, &reg);
                if let Some(l) = inner.links.get_mut(&link) {
                    l.proved = true;
                    l.mark = mark;
                }
                if self.settle.is_zero() {
                    inner.state = State::Bound(reg);
                    (frame(true, None), "registered (no settle window)")
                } else {
                    inner.state = State::Settling(reg);
                    self.settle_after(inner.epoch);
                    (frame(false, Some(Reason::Settling)), "registered")
                }
            }
            State::Settling(reg) | State::Bound(reg) => {
                let bound = matches!(inner.state, State::Bound(_));
                if !same_digest(digest, &reg.digest) {
                    self.contest(inner, link);
                    return (frame(false, Some(Reason::Contested)), "a second key");
                }
                if claimant.pid != reg.pid || claimant.start_ticks != reg.start_ticks {
                    return (frame(false, Some(Reason::NotTheRegistrant)), "the key from another process");
                }
                if let Some(l) = inner.links.get_mut(&link) {
                    l.proved = true;
                    l.mark = mark;
                }
                if bound {
                    (frame(true, None), "proved")
                } else {
                    (frame(false, Some(Reason::Settling)), "proved")
                }
            }
        }
    }

    /// Contested: nobody is bound; every other link that claimed is told
    /// (and may claim again, to be told the same).
    fn contest(&self, inner: &mut Inner, claimant: LinkId) {
        inner.state = State::Contested;
        inner.epoch += 1;
        for (id, l) in inner.links.iter_mut() {
            l.proved = false;
            if *id != claimant && l.claimed {
                l.live = false;
                l.push(&self.me, *id, frame(false, Some(Reason::Contested)));
            }
        }
        tracing::warn!("owner claim contested: a second key; nobody is bound until a cold start or `synaps daemon owner reset`");
    }

    /// `synaps daemon owner reset` from `resetter`'s link: Vacant, `reset`
    /// pushed to every other link that claimed; the reply is the caller's.
    /// Nothing binds by itself afterwards.
    pub fn reset(&self, resetter: LinkId) -> DaemonFrame {
        let mut inner = self.lock();
        let was = inner.state.name();
        inner.state = State::Vacant;
        inner.epoch += 1;
        for (id, l) in inner.links.iter_mut() {
            l.proved = false;
            if l.claimed {
                l.live = false;
                if *id != resetter {
                    l.push(&self.me, *id, frame(false, Some(Reason::Reset)));
                }
            }
        }
        tracing::warn!(was, "owner claim reset: vacant");
        frame(false, Some(Reason::Reset))
    }

    fn settle_after(&self, epoch: u64) {
        let me = self.me.clone();
        // G from the registration itself, not from whenever the task first runs.
        let due = tokio::time::Instant::now() + self.settle;
        tokio::spawn(async move {
            tokio::time::sleep_until(due).await;
            if let Some(me) = me.upgrade() {
                me.settled(epoch);
            }
        });
    }

    fn settled(&self, epoch: u64) {
        let mut inner = self.lock();
        if inner.epoch != epoch || !matches!(inner.state, State::Settling(_)) {
            return;
        }
        let State::Settling(reg) = std::mem::take(&mut inner.state) else {
            return;
        };
        let pid = reg.pid;
        inner.state = State::Bound(reg);
        for (id, l) in inner.links.iter_mut().filter(|(_, l)| l.proved) {
            l.push(&self.me, *id, frame(true, None));
        }
        tracing::info!(pid, "owner claim bound: the settle window passed");
    }

    #[cfg(target_os = "linux")]
    fn watch(&self, epoch: u64, reg: &Registration) {
        let me = self.me.clone();
        let Ok(fd) = reg.alive.fd.try_clone() else {
            tracing::warn!(pid = reg.pid, "owner claim: the lapse watch polls (pidfd dup failed)");
            let alive = Arc::clone(&reg.alive);
            tokio::spawn(async move {
                while alive.running() {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                if let Some(me) = me.upgrade() {
                    me.lapsed(epoch);
                }
            });
            return;
        };
        tokio::spawn(async move {
            if let Ok(fd) = tokio::io::unix::AsyncFd::with_interest(fd, tokio::io::Interest::READABLE) {
                let _ = fd.readable().await;
            }
            if let Some(me) = me.upgrade() {
                me.lapsed(epoch);
            }
        });
    }

    #[cfg(not(target_os = "linux"))]
    fn watch(&self, _epoch: u64, _reg: &Registration) {}

    /// The registrant exited: Vacant at once; every link unproved and free
    /// to claim again.
    fn lapsed(&self, epoch: u64) {
        let mut inner = self.lock();
        if inner.epoch != epoch || !matches!(inner.state, State::Settling(_) | State::Bound(_)) {
            return;
        }
        inner.state = State::Vacant;
        inner.epoch += 1;
        for l in inner.links.values_mut() {
            l.proved = false;
            l.live = false;
        }
        tracing::info!("owner claim lapsed: the registrant exited; vacant");
    }

    /// Whether a frame on `link` whose bytes came from `frame_pid`, starting
    /// at socket offset `frame_start`, is the owner's: Bound, the link proved
    /// the key, the frame starts at or past the link's mark (it was written
    /// after the claim's image check passed), the frame's writer is the
    /// registrant, it still runs (so its pid is not reused) and it still
    /// runs the image that was checked (stat'd per frame).
    pub fn is_owner(&self, link: LinkId, frame_pid: Option<u32>, frame_start: u64) -> bool {
        let inner = self.lock();
        let State::Bound(reg) = &inner.state else {
            return false;
        };
        inner.links.get(&link).is_some_and(|l| l.proved && frame_start >= l.mark)
            && frame_pid == Some(reg.pid)
            && reg.alive.running()
            && runs_image(reg.pid, reg.dev, reg.ino)
    }

    /// `build`'s frame queued on `link`, given `Attached.owner_bound` for it
    /// (`None` unless it sent `claim_owner`, then whether it is bound now),
    /// under the lock so no push overtakes it.
    pub fn send_bound(&self, link: LinkId, build: impl FnOnce(Option<bool>) -> DaemonFrame) -> Delivery {
        let inner = self.lock();
        let Some(l) = inner.links.get(&link) else {
            return Delivery::Pending(build(None));
        };
        let bound = l.claimed.then(|| l.proved && matches!(inner.state, State::Bound(_)));
        l.deliver(build(bound))
    }

    /// What a reload carries (`None`: Vacant).
    pub fn carry(&self) -> Option<OwnerCarry> {
        let inner = self.lock();
        let registrant = |reg: &Registration| CarriedRegistrant {
            sha256: hex(&reg.digest),
            pid: reg.pid,
            start_ticks: reg.start_ticks,
            dev: reg.dev,
            ino: reg.ino,
        };
        match &inner.state {
            State::Vacant => None,
            State::Contested => Some(OwnerCarry { state: CarriedState::Contested, registrant: None }),
            State::Settling(reg) => Some(OwnerCarry { state: CarriedState::Settling, registrant: Some(registrant(reg)) }),
            State::Bound(reg) => Some(OwnerCarry { state: CarriedState::Bound, registrant: Some(registrant(reg)) }),
        }
    }

    /// The new image's side of a reload: take the carried state back. The
    /// carried registrant must pass the image check again (the app path as
    /// this image reads it, its dev/ino and the start environ), as the same
    /// instance (start time); a registrant that is gone, runs another image
    /// now or can't be watched leaves it Vacant. A carried Settling starts
    /// a full settle window again.
    pub fn restore(&self, carry: Option<&OwnerCarry>) {
        let Some(carry) = carry else { return };
        // The check reads /proc: done before the lock, as at a claim.
        let checked = carry.registrant.as_ref().map(|r| self.check(Some(r.pid)));
        let mut inner = self.lock();
        inner.epoch += 1;
        if carry.state == CarriedState::Contested {
            inner.state = State::Contested;
            tracing::warn!("owner claim: contested, carried over the reload");
            return;
        }
        let (Some(r), Some(checked)) = (carry.registrant.as_ref(), checked) else { return };
        let Some(digest) = unhex(&r.sha256) else { return };
        let claimant = match checked {
            Ok(claimant) if claimant.start_ticks == r.start_ticks => claimant,
            Ok(_) => {
                tracing::info!(pid = r.pid, "owner claim: the registrant's pid is another process after the reload; vacant");
                return;
            }
            Err((_, why)) => {
                tracing::info!(pid = r.pid, why, "owner claim: the registrant fails the image check after the reload; vacant");
                return;
            }
        };
        let reg = Registration {
            digest,
            pid: r.pid,
            start_ticks: r.start_ticks,
            dev: claimant.dev,
            ino: claimant.ino,
            alive: Arc::new(claimant.alive),
        };
        self.watch(inner.epoch, &reg);
        if carry.state == CarriedState::Bound || self.settle.is_zero() {
            inner.state = State::Bound(reg);
        } else {
            inner.state = State::Settling(reg);
            self.settle_after(inner.epoch);
        }
        tracing::info!(pid = r.pid, state = inner.state.name(), "owner claim carried over the reload");
    }
}

#[cfg(target_os = "linux")]
fn running(fd: &std::os::fd::OwnedFd) -> bool {
    use std::os::fd::AsRawFd;
    let mut poll = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd, zero timeout.
    let n = unsafe { libc::poll(&mut poll, 1, 0) };
    n == 0
}

/// `pid`'s start time in clock ticks since boot (`/proc/<pid>/stat` field
/// 22) and whether it already exited (a zombie: state `Z` or `X`); `None`
/// when there is no such pid or its stat line can't be read.
#[cfg(target_os = "linux")]
fn stat_of(pid: u32) -> Option<(u64, bool)> {
    parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// One `/proc/<pid>/stat` line. The command name (field 2) is in
/// parentheses and may itself hold spaces and `)`, so the fields are split
/// after the LAST `)`: field 3 is the first one there.
#[cfg(target_os = "linux")]
fn parse_stat(line: &str) -> Option<(u64, bool)> {
    let rest = line.get(line.rfind(')')? + 1..)?;
    let mut fields = rest.split_ascii_whitespace();
    let state = fields.next()?.chars().next()?; // field 3
    let start_ticks = fields.nth(18)?.parse().ok()?; // field 22
    Some((start_ticks, matches!(state, 'Z' | 'X')))
}

/// A pidfd for the instance `(pid, start_ticks)`, readable once it exits;
/// `None` when it already exited (a zombie included) or can't be opened.
/// The instance is checked before AND after the open, so the fd never
/// refers to a process that reused the pid in between.
#[cfg(target_os = "linux")]
fn pidfd(pid: u32, start_ticks: u64) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd, OwnedFd};
    let same = || stat_of(pid).is_some_and(|(start, exited)| start == start_ticks && !exited);
    if !same() {
        return None;
    }
    let pid_t = libc::pid_t::try_from(pid).ok()?;
    // SAFETY: pidfd_open takes a pid and flags and returns a new fd or -1.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid_t, 0) };
    if raw < 0 {
        return None;
    }
    // SAFETY: a non-negative return is a new fd this process owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
    same().then_some(fd)
}

/// Whether `pid` runs the image (dev, ino) now: `/proc/<pid>/exe` follows
/// an execve, so a registrant that exec'd another image fails it.
#[cfg(target_os = "linux")]
fn runs_image(pid: u32, dev: u64, ino: u64) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(format!("/proc/{pid}/exe")).is_ok_and(|exe| (exe.dev(), exe.ino()) == (dev, ino))
}

#[cfg(not(target_os = "linux"))]
fn runs_image(_pid: u32, _dev: u64, _ino: u64) -> bool {
    false
}

/// The image check: `pid`'s exe is the file `want` names now (dev, ino),
/// read while a pidfd pins the instance; it was not started with a
/// [`REFUSED_ENV`] var. Anything unreadable fails closed.
#[cfg(target_os = "linux")]
fn check_image(pid: u32, want: &Path) -> Result<Claimant, &'static str> {
    use std::os::unix::fs::MetadataExt;
    let Ok(want) = std::fs::metadata(want) else {
        return Err("the owner app path does not resolve");
    };
    let Some((start_ticks, _)) = stat_of(pid) else {
        return Err("the claimant can't be identified");
    };
    let Some(fd) = pidfd(pid, start_ticks) else {
        return Err("the claimant is gone");
    };
    let Ok(image) = std::fs::metadata(format!("/proc/{pid}/exe")) else {
        return Err("the claimant's exe can't be read (fail closed)");
    };
    if (image.dev(), image.ino()) != (want.dev(), want.ino()) {
        return Err("the claimant's image is not the owner app's");
    }
    refused_env(pid)?;
    if !running(&fd) {
        return Err("the claimant is gone");
    }
    Ok(Claimant { pid, start_ticks, dev: image.dev(), ino: image.ino(), alive: Liveness { fd } })
}

#[cfg(not(target_os = "linux"))]
fn check_image(_pid: u32, _want: &Path) -> Result<Claimant, &'static str> {
    Err("no image check on this platform")
}

/// `Err` when `pid`'s start env sets a [`REFUSED_ENV`] var (or can't be read).
#[cfg(target_os = "linux")]
fn refused_env(pid: u32) -> Result<(), &'static str> {
    let Ok(environ) = std::fs::read(format!("/proc/{pid}/environ")) else {
        return Err("the claimant's environ can't be read (fail closed)");
    };
    if environ_refused(&environ) {
        return Err("the claimant was started with a loader or webview remote-control env");
    }
    Ok(())
}

/// Whether a NUL-separated environ block sets a [`REFUSED_ENV`] var to a
/// non-empty value.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn environ_refused(environ: &[u8]) -> bool {
    environ.split(|b| *b == 0).any(|entry| {
        REFUSED_ENV
            .iter()
            .any(|name| entry.len() > name.len() + 1 && entry.starts_with(name.as_bytes()) && entry[name.len()] == b'=')
    })
}

#[cfg(all(test, target_os = "linux"))]
#[path = "owner_claim_tests.rs"]
mod tests;
