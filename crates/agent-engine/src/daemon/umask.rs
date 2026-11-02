//! `daemon.umask`: the daemon's file-creation mask, set once at start.
//!
//! The mask is process-wide and inherited by every child, so the daemon sets
//! it in its own start path (`run_foreground`), right after the host has
//! booted and loaded the config, and before it starts any process:
//! extensions, MCP servers, sessions and their tool commands and PTY shells
//! all come later and inherit it. The TUI and one-shot commands never call
//! this; they keep the mask they inherited. Files written through
//! `private_fs` keep their explicit modes either way.
//!
//! Threads exist at that point (the allocator's background thread, the async
//! runtime's workers, the log writer). That is safe because nothing here
//! swaps or edits process state in steps: the current mask is read from
//! `/proc/self/status` (Linux), and the new one is set with a single
//! `umask(2)` call, only when it differs. A file another thread creates at
//! that moment gets either the old or the new mask. Where `/proc` cannot be
//! read, the mask is read by setting `0777` and putting the old value back
//! (two calls, then the change call if any); a file another thread creates
//! in that window gets no permission bits at all, since `0777` never grants
//! a bit that any prior mask forbids. The process environment is never
//! changed. What can predate the call: the base/log directory, if this
//! start creates it, and the first log file (the boot creates and opens
//! them); later daily log files get the set mask.
//!
//! A reload execs the new image in place, which inherits the mask the old
//! image set. So the old image records the mask the daemon was first
//! started with in the reload state ([`super::reload::ReloadState::base_umask`]),
//! and `inherit` after a reload means that mask: removing the config line
//! and reloading is a full rollback, as long as every reload since the start
//! went through images that carry it (an older image records none; after a
//! round trip through one only a full restart restores the original). A
//! changed value takes effect at the next reload (or restart); there is no
//! re-apply while the daemon runs.

use std::fmt;
use std::sync::OnceLock;

use crate::config::DaemonUmask;

/// Where the applied mask came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UmaskSource {
    /// `daemon.umask` set an octal mode.
    Config,
    /// No key, `inherit`, or a malformed value: the started-with mask.
    Inherit,
}

impl UmaskSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Inherit => "inherit",
        }
    }
}

/// What the daemon applied at start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UmaskOutcome {
    /// The mask now in effect.
    pub applied: u32,
    pub source: UmaskSource,
    /// The mask the daemon was first started with (carried across reloads).
    pub base: u32,
    /// The config parser's warning when the value was malformed (then the
    /// daemon inherits).
    pub warning: Option<String>,
    /// Whether the mask was changed (one `umask(2)` call); `false` when the
    /// mask in place already was the one to apply.
    pub changed: bool,
}

/// A mode shown as four octal digits (`0022`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Octal(pub u32);

impl fmt::Display for Octal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04o}", self.0)
    }
}

static OUTCOME: OnceLock<UmaskOutcome> = OnceLock::new();

/// The mask to apply for `setting`, given the started-with mask `base`.
/// Pure.
pub fn resolve(setting: DaemonUmask, base: u32, warning: Option<String>) -> UmaskOutcome {
    let base = base & 0o777;
    let (applied, source) = match setting {
        DaemonUmask::Mode(mode) => (mode & 0o777, UmaskSource::Config),
        DaemonUmask::Inherit => (base, UmaskSource::Inherit),
    };
    UmaskOutcome { applied, source, base, warning, changed: false }
}

/// The `Umask:` line of a `/proc/<pid>/status` text (Linux ≥ 4.7): up to four
/// octal digits, at most `0777`. Pure.
pub fn parse_proc_status_umask(status: &str) -> Option<u32> {
    let value = status.lines().find_map(|l| l.strip_prefix("Umask:"))?.trim();
    if value.is_empty() || value.len() > 4 || !value.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return None;
    }
    u32::from_str_radix(value, 8).ok().filter(|mode| *mode <= 0o777)
}

/// The base for this start: the started-with mask a reload state carried
/// (when it is a mode), else the mask this process has now. Pure.
pub fn start_base(carried: Option<u32>, current: u32) -> u32 {
    carried.filter(|mode| *mode <= 0o777).unwrap_or(current & 0o777)
}

/// This process's mask, read without changing it.
#[cfg(target_os = "linux")]
fn read_current() -> Option<u32> {
    parse_proc_status_umask(&std::fs::read_to_string("/proc/self/status").ok()?)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn read_current() -> Option<u32> {
    None
}

/// The mask [`read_by_swap`] holds while it reads the current one: `0777`
/// grants nothing that any prior mask forbids, so a file another thread
/// creates in that window gets no permission bits (`0077` would give `0600`
/// under an inherited `0777`).
pub const READ_SWAP_MASK: u32 = 0o777;

/// The current mask without `/proc`: set [`READ_SWAP_MASK`], then put the
/// returned old mask back. `umask` performs one `umask(2)` call (the real one
/// in [`apply_at_start`]; a recording fake in tests).
pub fn read_by_swap(mut umask: impl FnMut(u32) -> u32) -> u32 {
    let old = umask(READ_SWAP_MASK);
    umask(old);
    old & 0o777
}

/// Sets `daemon.umask` for this process and records what it did. Call it
/// once, in the daemon's start path, before the daemon starts any process.
/// `carried_base` is the started-with mask from the reload state (`None` on
/// a fresh start or after an image that recorded none). Where
/// `/proc/self/status` can be read (Linux): at most one `umask(2)` call, and
/// none when the mask in place is already the one to apply. Without it, two
/// calls read the mask ([`read_by_swap`]) before that possible change call.
/// The environment is not touched. A second call changes nothing and returns
/// the first outcome.
#[cfg(unix)]
pub fn apply_at_start(
    setting: DaemonUmask,
    warning: Option<String>,
    carried_base: Option<u32>,
) -> Option<&'static UmaskOutcome> {
    if let Some(done) = OUTCOME.get() {
        return Some(done);
    }
    let current = match read_current() {
        Some(mask) => mask,
        // No /proc: read it by a swap through READ_SWAP_MASK (0777).
        // SAFETY: umask(2) cannot fail; it only swaps this process's mask.
        None => read_by_swap(|mask| unsafe { libc::umask(mask as libc::mode_t) as u32 }),
    };
    let mut outcome = resolve(setting, start_base(carried_base, current), warning);
    if outcome.applied != current {
        // SAFETY: as above; one call sets the new mask.
        unsafe {
            libc::umask(outcome.applied as libc::mode_t);
        }
        outcome.changed = true;
    }
    Some(OUTCOME.get_or_init(|| outcome))
}

/// Not applied off Unix (no process umask to set).
#[cfg(not(unix))]
pub fn apply_at_start(
    _setting: DaemonUmask,
    _warning: Option<String>,
    _carried_base: Option<u32>,
) -> Option<&'static UmaskOutcome> {
    None
}

/// What [`apply_at_start`] did in this process, if it ran.
pub fn outcome() -> Option<&'static UmaskOutcome> {
    OUTCOME.get()
}

/// One log line for the daemon's start (call it once logging is up): the
/// mask in effect and its source; a warning with the reason when the
/// configured value was malformed. Nothing when the mask was not applied.
pub fn log_start() {
    let Some(o) = outcome() else { return };
    match &o.warning {
        Some(reason) => tracing::warn!(
            umask = %Octal(o.applied),
            source = %o.source.as_str(),
            base = %Octal(o.base),
            reason = %reason,
            "daemon: umask"
        ),
        None => tracing::info!(
            umask = %Octal(o.applied),
            source = %o.source.as_str(),
            base = %Octal(o.base),
            "daemon: umask"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fallback swap holds 0777 and puts the inherited mask back: with a
    /// fake process mask, for inherited masks from open to closed.
    #[test]
    fn the_fallback_read_swaps_through_0777_and_restores_the_inherited_mask() {
        for inherited in [0o000, 0o002, 0o022, 0o077, 0o777] {
            let mut mask = inherited;
            let mut calls = Vec::new();
            let read = read_by_swap(|new| {
                calls.push(new);
                std::mem::replace(&mut mask, new)
            });
            assert_eq!(read, inherited, "reads the inherited mask");
            assert_eq!(calls, vec![0o777, inherited], "two calls, the first holding 0777");
            assert_eq!(mask, inherited, "the inherited mask is back");
        }
    }

    #[test]
    fn a_configured_mode_is_applied_and_inherit_keeps_the_base() {
        let o = resolve(DaemonUmask::Mode(0o022), 0o002, None);
        assert_eq!((o.applied, o.source, o.base), (0o022, UmaskSource::Config, 0o002));
        let o = resolve(DaemonUmask::Inherit, 0o002, None);
        assert_eq!((o.applied, o.source, o.base), (0o002, UmaskSource::Inherit, 0o002));
        let o = resolve(DaemonUmask::Inherit, 0o002, Some("daemon.umask — bad".into()));
        assert_eq!((o.applied, o.source), (0o002, UmaskSource::Inherit));
        assert_eq!(o.warning.as_deref(), Some("daemon.umask — bad"));
        // Only permission bits.
        assert_eq!(resolve(DaemonUmask::Mode(0o7777), 0o4002, None).applied, 0o777);
        assert_eq!(resolve(DaemonUmask::Inherit, 0o4002, None).applied, 0o002);
        assert!(!resolve(DaemonUmask::Mode(0o022), 0o002, None).changed, "resolve only decides");
    }

    #[test]
    fn the_carried_base_wins_only_when_it_is_a_mode() {
        assert_eq!(start_base(Some(0o002), 0o022), 0o002);
        assert_eq!(start_base(None, 0o022), 0o022, "a fresh start, or an older image recorded none");
        assert_eq!(start_base(Some(0o1777), 0o027), 0o027, "not a permission mode");
        assert_eq!(start_base(None, 0o4022), 0o022);
    }

    #[test]
    fn the_current_mask_is_read_from_the_status_umask_line() {
        let status = "Name:\tsynaps\nUmask:\t0002\nState:\tS (sleeping)\n";
        assert_eq!(parse_proc_status_umask(status), Some(0o002));
        assert_eq!(parse_proc_status_umask("Umask: 0777\n"), Some(0o777));
        assert_eq!(parse_proc_status_umask("Umask:\t022\n"), Some(0o022));
        for bad in ["Name:\tx\n", "Umask:\t\n", "Umask:\t0778\n", "Umask:\t01777\n", "Umask:\t1777\n", "Umask:\tabc\n"] {
            assert_eq!(parse_proc_status_umask(bad), None, "{bad:?}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_mask_reads_from_proc() {
        // No swap here: the test process is multi-threaded.
        assert!(read_current().is_some_and(|mask| mask <= 0o777));
    }

    #[test]
    fn modes_show_as_four_octal_digits() {
        assert_eq!(Octal(0o22).to_string(), "0022");
        assert_eq!(Octal(0).to_string(), "0000");
        assert_eq!(Octal(0o777).to_string(), "0777");
    }
}
