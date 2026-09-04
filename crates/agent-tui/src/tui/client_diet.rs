//! Client diet (PLAN-phase4 §4): the allocator knobs and purge points that
//! make the thin client's RssAnon equal its live heap instead of its
//! high-water mark.
//!
//! * `tune_allocator()` — called from `main()` on the attach path **before**
//!   the runtime or any thread exists: jemalloc background threads off (so
//!   none are spawned for the arenas the render/crossterm threads touch;
//!   8 → 4 threads), `dirty_decay_ms=0`/`muzzy_decay_ms=0` (a freed run goes
//!   back to the OS on the next decay tick instead of ≤ 10 s later).
//! * `purge_arenas(stage)` — explicit `arena.<all>.purge` + a ladder line.
//! * `IdlePurge` — the `run_loop` arm: purge N s after the client goes idle
//!   (turn/compaction finished), plus the syntect idle-eviction hook.
//!
//! * `PR_SET_THP_DISABLE` first: with `THP=always` (bella, most desktops)
//!   every touched thread stack, the `.bss` and each arena chunk is a 2 MiB
//!   page — the A1 ladder showed +4 MB at `render_thread` for 128 KB of
//!   allocations. `SYNAPS_CLIENT_THP=1` keeps huge pages.
//!
//! Flags: `SYNAPS_CLIENT_MALLOC=off` / `SYNAPS_CLIENT_ALLOC=default` skip the
//! mallctls; `SYNAPS_CLIENT_TCACHE=0` disables the main thread's tcache;
//! `SYNAPS_CLIENT_PURGE_SECS` / `SYNAPS_CLIENT_PURGE_IDLE_SECS` (default 10,
//! 0 = never); `SYNAPS_MEMPROF_PURGE=1` purges on every idle immediately.

use std::time::{Duration, Instant};

use agent_core::core::memstat;

/// Re-purge cadence while the client stays idle (cheap: nothing to purge
/// after the first one; keeps the syntect eviction check ticking).
const IDLE_TICK: Duration = Duration::from_secs(30);

fn env_is(name: &str, value: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == value)
}

/// Mallctls skipped? (`SYNAPS_CLIENT_MALLOC=off` or `SYNAPS_CLIENT_ALLOC=default`).
pub fn allocator_tuning_disabled() -> bool {
    env_is("SYNAPS_CLIENT_MALLOC", "off") || env_is("SYNAPS_CLIENT_ALLOC", "default")
}

/// §4.1–4.3 in order, each result recorded on the `alloc` ladder line; never
/// fatal (risk #9). Must run before any thread is spawned.
pub fn tune_allocator() {
    if allocator_tuning_disabled() {
        memstat::ladder("alloc", &"skipped=1");
        return;
    }
    let thp = if env_is("SYNAPS_CLIENT_THP", "1") {
        Err("kept".to_string())
    } else {
        memstat::disable_thp()
    };
    let bg = memstat::set_background_threads(false);
    let decay = memstat::set_decay_ms(0, 0);
    let tcache = if env_is("SYNAPS_CLIENT_TCACHE", "0") {
        Some(memstat::set_thread_tcache(false))
    } else {
        None
    };
    memstat::ladder(
        "alloc",
        &format_args!(
            "thp_off={} bg_off={} decay0={} tcache_off={} bg_now={:?}",
            res(&thp),
            res(&bg),
            res(&decay),
            tcache.as_ref().map(res).unwrap_or("n/a"),
            memstat::background_threads_enabled()
        ),
    );
}

fn res(r: &memstat::MallctlResult) -> &str {
    match r {
        Ok(()) => "ok",
        Err(e) => e.as_str(),
    }
}

/// `arena.<all>.purge` and one ladder line at `stage`.
pub fn purge_arenas(stage: &'static str) {
    memstat::purge_arenas();
    memstat::ladder(stage, &"");
}

/// Idle purge delay: `SYNAPS_CLIENT_PURGE_SECS` (alias
/// `SYNAPS_CLIENT_PURGE_IDLE_SECS`), default 10; `None` = disabled (0).
/// `SYNAPS_MEMPROF_PURGE=1` → immediate.
pub fn purge_delay() -> Option<Duration> {
    if env_is("SYNAPS_MEMPROF_PURGE", "1") {
        return Some(Duration::ZERO);
    }
    let secs = std::env::var("SYNAPS_CLIENT_PURGE_SECS")
        .or_else(|_| std::env::var("SYNAPS_CLIENT_PURGE_IDLE_SECS"))
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(10);
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// syntect idle window for `highlight::evict_if_idle` (C's
/// `SYNAPS_TUI_SYNTECT_IDLE_SECS`, default 120; 0 = never).
fn syntect_idle() -> Option<Duration> {
    let secs = std::env::var("SYNAPS_TUI_SYNTECT_IDLE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120);
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// The `run_loop` idle arm's state machine. Busy = streaming or compacting.
/// On the busy→idle edge the first fire is scheduled `delay` later (ladder
/// `idle+N` → purge → `purged`); afterwards every [`IDLE_TICK`] while still
/// idle (syntect eviction + a silent purge). Purging is Socket-only; the
/// eviction check runs in both modes (same code, same render output).
pub struct IdlePurge {
    purge: bool,
    delay: Option<Duration>,
    idle_since: Option<Instant>,
    next: Option<Instant>,
    fired_since_idle: bool,
}

impl IdlePurge {
    pub fn new(socket: bool) -> Self {
        Self {
            purge: socket,
            delay: purge_delay(),
            idle_since: None,
            next: None,
            fired_since_idle: false,
        }
    }

    /// Track the busy/idle edge; call once per loop iteration.
    pub fn observe(&mut self, busy: bool) {
        if busy {
            self.idle_since = None;
            self.next = None;
            self.fired_since_idle = false;
        } else if self.idle_since.is_none() {
            let now = Instant::now();
            self.idle_since = Some(now);
            self.next = self.delay.map(|d| now + d);
        }
    }

    /// Resolves at the next scheduled fire; pending when nothing is scheduled.
    pub async fn wait(&self) {
        match self.next {
            Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
            None => std::future::pending().await,
        }
    }

    /// The arm body.
    pub fn fire(&mut self) {
        let first = !self.fired_since_idle;
        self.fired_since_idle = true;
        self.next = Some(Instant::now() + IDLE_TICK);
        if self.purge {
            if first {
                let n = self.delay.map(|d| d.as_secs()).unwrap_or(0);
                memstat::ladder("idle+N", &format_args!("n={n}"));
                purge_arenas("purged");
            } else {
                memstat::purge_arenas();
            }
        }
        if let (Some(window), Some(since)) = (syntect_idle(), self.idle_since) {
            if since.elapsed() >= window {
                super::highlight::evict_if_idle(window);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Other tests in this binary spawn threads concurrently, so the count
    /// is not ours to assert; the *kind* is — no `jemalloc_bg_thd` after.
    #[test]
    fn tune_allocator_spawns_nothing_and_purge_returns() {
        tune_allocator();
        purge_arenas("test");
        if !allocator_tuning_disabled() {
            assert_ne!(memstat::background_threads_enabled(), Some(true));
        }
        #[cfg(target_os = "linux")]
        {
            let bg = std::fs::read_dir("/proc/self/task")
                .unwrap()
                .filter_map(|e| std::fs::read_to_string(e.ok()?.path().join("comm")).ok())
                .filter(|c| c.starts_with("jemalloc_bg"))
                .count();
            assert_eq!(bg, 0, "jemalloc background threads still alive");
        }
    }

    #[test]
    fn idle_purge_arms_on_idle_edge_only() {
        let mut p = IdlePurge {
            purge: true,
            delay: Some(Duration::from_secs(10)),
            idle_since: None,
            next: None,
            fired_since_idle: false,
        };
        assert!(p.next.is_none());
        p.observe(false);
        assert!(p.next.is_some());
        p.observe(true);
        assert!(p.next.is_none());
        p.observe(false);
        p.fire();
        assert!(p.fired_since_idle);
        assert!(p.next.unwrap() > Instant::now() + Duration::from_secs(20));
    }

    #[test]
    fn disabled_delay_never_schedules() {
        let mut p = IdlePurge {
            purge: true,
            delay: None,
            idle_since: None,
            next: None,
            fired_since_idle: false,
        };
        p.observe(false);
        assert!(p.next.is_none());
    }
}
