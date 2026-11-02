//! Open-file watch: the daemon counts its open file descriptors against the
//! soft `RLIMIT_NOFILE` every [`SAMPLE_EVERY`] and says so ONCE before it
//! runs out. At the limit every `open` fails with EMFILE (a session save,
//! a journal lock, an accepted socket), so the WARN comes first, at
//! [`WARN_PERCENT`] of the soft limit, with the counts in the line. It is
//! not repeated until the count has fallen below [`RECOVER_PERCENT`] (one
//! info line says so): a daemon that stays near its limit warns once, not
//! every sample.
//!
//! Linux and macOS; elsewhere the watch does nothing. A sample lists
//! [`FD_DIR`], the directory that names each open descriptor of this
//! process: `/proc/self/fd` on Linux (as before: Linux's `/dev/fd` is only a
//! link to it, and is missing where `/dev` is minimal) and `/dev/fd` on
//! macOS (devfs lists every open descriptor there, not only 0 to 2). The
//! listing reads through a directory handle of its own, and that handle is
//! open while it reads, so on both the count includes it: one more than the
//! descriptors open before the sample, the same on both systems, from the
//! same [`count_entries`]. The soft and hard limits come from
//! `getrlimit(RLIMIT_NOFILE)` on both; `RLIM_INFINITY` (all ones on Linux,
//! `i64::MAX` on macOS) reads as [`NO_LIMIT`].
//!
//! The decision is [`FdWatch::observe`] (pure, no I/O); [`run`] samples and
//! logs. A sample that fails (the count itself can fail with EMFILE) is a
//! debug line, never a WARN.
//!
//! At start on macOS, the Synaps daemon raises its own open-files soft limit
//! from 256 to min(hard, 10240); it never touches the hard limit or other
//! processes. launchd starts the daemon with a soft limit of 256, which a
//! busy daemon outgrows. `run_foreground` calls `raise_at_start` (macOS
//! only) before the host boot opens anything: one `getrlimit`, then one
//! `setrlimit` with the pair [`raised_rlimit`] builds, whose `rlim_max` is
//! the one read; nothing is set when it gives none. The decision is
//! [`raised_soft_limit`] (pure): the target is min(hard, [`RAISE_SOFT_TO`]),
//! and a soft limit already at the target or above is left as it is (never
//! lowered; a reload's new image inherits the raised one). The cap is 10240
//! because macOS's `setrlimit` rejects (EINVAL) a soft limit above
//! `kern.maxfilesperproc`, `RLIM_INFINITY` included, and setrlimit(2) says
//! to ask for min(OPEN_MAX, rlim_max), `OPEN_MAX` being 10240.
//! The outcome is logged once logging is up ([`Raise::emit`]): one info
//! line from -> to, or a WARN when a call failed; the start goes on either
//! way. Processes the daemon starts afterwards inherit the raised soft
//! limit, as they inherit every limit; no running process is changed.
//! Linux makes no call.

use std::path::Path;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// How often the daemon counts its open files.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(15);
/// The WARN fires when the open count reaches this share of the soft limit.
pub const WARN_PERCENT: u64 = 80;
/// After a WARN, the count must fall below this share before the next one.
pub const RECOVER_PERCENT: u64 = 70;
/// The WARN's message (stable: log rules match it).
pub const WARN_MESSAGE: &str = "daemon open files near the soft limit";
/// The recovery line's message.
pub const RECOVERED_MESSAGE: &str = "daemon open files recovered below the warn level";
/// A limit of this value means none ([`limit_value`] maps `RLIM_INFINITY`,
/// whatever its value on the platform, to it).
pub const NO_LIMIT: u64 = u64::MAX;
/// The directory listing this process's open descriptors, one entry each.
#[cfg(target_os = "linux")]
pub const FD_DIR: &str = "/proc/self/fd";
/// The directory listing this process's open descriptors, one entry each.
#[cfg(target_os = "macos")]
pub const FD_DIR: &str = "/dev/fd";

/// One sample: open descriptors and the `RLIMIT_NOFILE` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub open: u64,
    pub soft: u64,
    pub hard: u64,
}

/// What a sample says, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line {
    /// The count crossed [`WARN_PERCENT`] of the soft limit.
    Warn { sample: Sample, percent: u64 },
    /// After a WARN, the count fell below [`RECOVER_PERCENT`].
    Recovered { sample: Sample, percent: u64 },
}

impl Line {
    /// Log the line: one WARN, or one info.
    pub fn emit(&self) {
        match *self {
            Line::Warn { sample: Sample { open, soft, hard }, percent } => {
                tracing::warn!(open, soft, hard, percent, "{WARN_MESSAGE}")
            }
            Line::Recovered { sample: Sample { open, soft, hard }, percent } => {
                tracing::info!(open, soft, hard, percent, "{RECOVERED_MESSAGE}")
            }
        }
    }
}

/// The watch's state between samples: whether it has warned and not seen
/// the count recover since.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FdWatch {
    warned: bool,
}

impl FdWatch {
    /// The line one sample gives, if any. A failed sample gives none and
    /// changes nothing.
    pub fn observe(&mut self, sample: std::io::Result<Sample>) -> Option<Line> {
        let sample = sample.ok()?;
        // No limit to be near (unlimited, or a zero that would only divide).
        if sample.soft == 0 || sample.soft == NO_LIMIT {
            return None;
        }
        let scaled = sample.open.saturating_mul(100);
        let percent = scaled / sample.soft;
        if !self.warned && scaled >= sample.soft.saturating_mul(WARN_PERCENT) {
            self.warned = true;
            return Some(Line::Warn { sample, percent });
        }
        if self.warned && scaled < sample.soft.saturating_mul(RECOVER_PERCENT) {
            self.warned = false;
            return Some(Line::Recovered { sample, percent });
        }
        None
    }
}

/// The number of entries a directory listing gives (every entry the listing
/// yields counts, an unreadable one too). On [`FD_DIR`] that is the open
/// descriptors, the listing's own handle among them.
pub fn count_entries(dir: &Path) -> std::io::Result<u64> {
    Ok(std::fs::read_dir(dir)?.count() as u64)
}

/// A `getrlimit` value as a count: `RLIM_INFINITY` is [`NO_LIMIT`].
// rlim_t is u64 on 64-bit Linux and on macOS, not on every target libc
// covers.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
pub fn limit_value(v: libc::rlim_t) -> u64 {
    if v == libc::RLIM_INFINITY {
        NO_LIMIT
    } else {
        v as u64
    }
}

/// This process's `RLIMIT_NOFILE` pair, as `getrlimit` gives it.
#[cfg(unix)]
pub fn nofile_rlimit() -> std::io::Result<libc::rlimit> {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: getrlimit writes the pair into `lim`, which outlives the call.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(lim)
}

/// Sample this process: count [`FD_DIR`] (the directory handle the count
/// holds is one of them) and read `RLIMIT_NOFILE`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn sample() -> std::io::Result<Sample> {
    let open = count_entries(Path::new(FD_DIR))?;
    let lim = nofile_rlimit()?;
    Ok(Sample { open, soft: limit_value(lim.rlim_cur), hard: limit_value(lim.rlim_max) })
}

/// The daemon's watch task: samples every [`SAMPLE_EVERY`] (the first at
/// once) until `shutdown`. Started by every `Daemon::start`, so a reload's
/// new image (the same pid after the exec) runs its own from a fresh state.
/// Elsewhere than Linux and macOS it returns at once.
pub async fn run(shutdown: CancellationToken) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let mut watch = FdWatch::default();
        let mut tick = tokio::time::interval(SAMPLE_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tick.tick() => {}
            }
            let sample = sample();
            if let Err(e) = &sample {
                tracing::debug!(error = %e, "daemon: open-file sample failed");
            }
            if let Some(line) = watch.observe(sample) {
                line.emit();
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = shutdown;
}

/// The highest soft limit the daemon raises itself to: macOS's `OPEN_MAX`
/// (`<sys/syslimits.h>`). macOS's `setrlimit` rejects (EINVAL) a soft
/// `RLIMIT_NOFILE` above `kern.maxfilesperproc`, `RLIM_INFINITY` included,
/// and setrlimit(2) says to ask for min(OPEN_MAX, rlim_max).
pub const RAISE_SOFT_TO: u64 = 10240;
/// The info line's message when the start raised the soft limit.
pub const RAISED_MESSAGE: &str = "daemon: raised the open-files soft limit";
/// The WARN's message when a call failed (the limit stays as it was).
pub const RAISE_FAILED_MESSAGE: &str = "daemon: could not raise the open-files soft limit; it stays as it was";

/// The soft limit to raise to: `min(hard, RAISE_SOFT_TO)` when that is
/// above `soft`; `None` when `soft` is already there or above (a limit is
/// never lowered). Never above `hard`; an unlimited `hard` ([`NO_LIMIT`])
/// gives [`RAISE_SOFT_TO`]. Pure.
pub fn raised_soft_limit(soft: u64, hard: u64) -> Option<u64> {
    let target = hard.min(RAISE_SOFT_TO);
    (target > soft).then_some(target)
}

/// The pair to set: `current` with the soft limit from
/// [`raised_soft_limit`] and `rlim_max` passed through as read, bit for bit
/// (`RLIM_INFINITY` too). `None` when nothing is raised. Pure.
#[cfg(unix)]
pub fn raised_rlimit(current: libc::rlimit) -> Option<libc::rlimit> {
    let to = raised_soft_limit(limit_value(current.rlim_cur), limit_value(current.rlim_max))?;
    // At most RAISE_SOFT_TO, so it fits any rlim_t (u64 on 64-bit Linux
    // and on macOS, not on every target libc covers).
    #[allow(clippy::unnecessary_cast)]
    let rlim_cur = to as libc::rlim_t;
    Some(libc::rlimit { rlim_cur, rlim_max: current.rlim_max })
}

/// What the start did with the open-files soft limit.
#[derive(Debug)]
pub enum Raise {
    /// Raised from `from` to `to`; the hard limit passed through as read.
    Raised { from: u64, to: u64, hard: u64 },
    /// Already at the target or above: nothing set.
    Kept { soft: u64, hard: u64 },
    /// `getrlimit` failed: nothing set.
    ReadFailed { error: std::io::Error },
    /// `setrlimit` failed: the soft limit stays `from`.
    SetFailed { from: u64, to: u64, hard: u64, error: std::io::Error },
}

/// A limit as logged: its number, or `unlimited` for [`NO_LIMIT`].
fn shown(limit: u64) -> String {
    if limit == NO_LIMIT {
        "unlimited".to_string()
    } else {
        limit.to_string()
    }
}

impl Raise {
    /// Log the outcome (once logging is up): one info line when raised, one
    /// WARN when a call failed, a debug line when nothing was to be raised.
    pub fn emit(&self) {
        match self {
            Raise::Raised { from, to, hard } => {
                tracing::info!(from = *from, to = *to, hard = %shown(*hard), "{RAISED_MESSAGE}")
            }
            Raise::Kept { soft, hard } => tracing::debug!(
                soft = %shown(*soft),
                hard = %shown(*hard),
                "daemon: open-files soft limit already at the raise target or above"
            ),
            Raise::ReadFailed { error } => {
                tracing::warn!(error = %error, step = "getrlimit", "{RAISE_FAILED_MESSAGE}")
            }
            Raise::SetFailed { from, to, hard, error } => tracing::warn!(
                from = *from,
                to = *to,
                hard = %shown(*hard),
                error = %error,
                step = "setrlimit",
                "{RAISE_FAILED_MESSAGE}"
            ),
        }
    }
}

/// Read the pair with `get` and, when [`raised_rlimit`] gives one, set it
/// with `set`: at most one call each, and `set` gets the hard limit `get`
/// gave. Never fails: an error is the outcome.
#[cfg(unix)]
pub fn raise_with(
    get: impl FnOnce() -> std::io::Result<libc::rlimit>,
    set: impl FnOnce(&libc::rlimit) -> std::io::Result<()>,
) -> Raise {
    let current = match get() {
        Ok(lim) => lim,
        Err(error) => return Raise::ReadFailed { error },
    };
    let (from, hard) = (limit_value(current.rlim_cur), limit_value(current.rlim_max));
    let Some(new) = raised_rlimit(current) else {
        return Raise::Kept { soft: from, hard };
    };
    let to = limit_value(new.rlim_cur);
    match set(&new) {
        Ok(()) => Raise::Raised { from, to, hard },
        Err(error) => Raise::SetFailed { from, to, hard, error },
    }
}

/// Set this process's `RLIMIT_NOFILE` pair.
#[cfg(unix)]
pub fn set_nofile_rlimit(lim: &libc::rlimit) -> std::io::Result<()> {
    // SAFETY: setrlimit only reads the pair from `lim`, which outlives the
    // call.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, lim) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// macOS, daemon start (`run_foreground`, before the host boot): raise this
/// process's open-files soft limit to `min(hard, 10240)` when that is
/// higher, the hard limit passed through as read. Log the outcome with
/// [`Raise::emit`] once logging is up.
#[cfg(target_os = "macos")]
pub fn raise_at_start() -> Raise {
    raise_with(nofile_rlimit, set_nofile_rlimit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(open: u64, soft: u64) -> std::io::Result<Sample> {
        Ok(Sample { open, soft, hard: 4096 })
    }

    fn warn(open: u64, soft: u64, percent: u64) -> Option<Line> {
        Some(Line::Warn { sample: Sample { open, soft, hard: 4096 }, percent })
    }

    fn recovered(open: u64, soft: u64, percent: u64) -> Option<Line> {
        Some(Line::Recovered { sample: Sample { open, soft, hard: 4096 }, percent })
    }

    /// Below 80% nothing is said, however long; a fresh watch never says
    /// "recovered" (it never warned).
    #[test]
    fn below_the_warn_level_is_quiet() {
        let mut w = FdWatch::default();
        for open in [0, 10, 500, 700, 716, 800, 819] {
            assert_eq!(w.observe(at(open, 1024)), None, "{open}/1024");
        }
    }

    /// The crossing warns once, with the counts; staying above (up to and
    /// past the limit) stays quiet.
    #[test]
    fn the_crossing_warns_once_and_staying_above_is_quiet() {
        let mut w = FdWatch::default();
        assert_eq!(w.observe(at(819, 1024)), None);
        assert_eq!(w.observe(at(820, 1024)), warn(820, 1024, 80), "820/1024 = 80.07%");
        for open in [820, 900, 992, 1024, 1500, 820] {
            assert_eq!(w.observe(at(open, 1024)), None, "{open}/1024: already warned");
        }
        // Straight from quiet to the limit: one WARN too.
        let mut w = FdWatch::default();
        assert_eq!(w.observe(at(992, 1024)), warn(992, 1024, 96));
        assert_eq!(w.observe(at(992, 1024)), None);
    }

    /// Hysteresis: after a WARN, 79%..70% stays quiet; below 70% gives ONE
    /// recovered line; a second crossing warns again.
    #[test]
    fn it_recovers_below_seventy_percent_once_and_a_second_crossing_warns_again() {
        let mut w = FdWatch::default();
        assert_eq!(w.observe(at(800, 1000)), warn(800, 1000, 80));
        for open in [799, 750, 700] {
            assert_eq!(w.observe(at(open, 1000)), None, "{open}/1000: not recovered yet");
        }
        assert_eq!(w.observe(at(799, 1000)), None, "back up without recovering: no repeat");
        assert_eq!(w.observe(at(699, 1000)), recovered(699, 1000, 69));
        for open in [600, 10, 699, 799] {
            assert_eq!(w.observe(at(open, 1000)), None, "{open}/1000: recovered once");
        }
        assert_eq!(w.observe(at(850, 1000)), warn(850, 1000, 85), "a second crossing warns again");
        assert_eq!(w.observe(at(100, 1000)), recovered(100, 1000, 10));
    }

    /// No soft limit (`RLIM_INFINITY`) or a zero one: never a line.
    #[test]
    fn an_unlimited_or_zero_soft_limit_never_warns() {
        for soft in [NO_LIMIT, 0] {
            let mut w = FdWatch::default();
            for open in [0, 1, 1000, u64::MAX / 2, u64::MAX] {
                assert_eq!(w.observe(at(open, soft)), None, "{open}/{soft}");
            }
        }
    }

    /// A failed sample says nothing and changes nothing: no WARN loop, and
    /// a watch that warned neither recovers nor warns again on errors.
    #[test]
    fn a_failed_sample_is_quiet_and_keeps_the_state() {
        // Any error (not `libc::EMFILE`: this test also builds off Unix).
        let err = || Err(std::io::Error::other("too many open files"));
        let mut w = FdWatch::default();
        for _ in 0..5 {
            assert_eq!(w.observe(err()), None);
        }
        assert_eq!(w.observe(at(900, 1000)), warn(900, 1000, 90));
        for _ in 0..5 {
            assert_eq!(w.observe(err()), None);
        }
        assert_eq!(w.observe(at(900, 1000)), None, "still warned");
        assert_eq!(w.observe(at(10, 1000)), recovered(10, 1000, 1));
    }

    /// Everything `f` logs (DEBUG and up), as the fmt layer writes it.
    fn captured(f: impl FnOnce()) -> String {
        use std::sync::{Arc, Mutex};
        #[derive(Clone, Default)]
        struct Capture(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Capture {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap_or_else(|p| p.into_inner()).extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
            type Writer = Capture;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }
        let out = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(out.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = out.0.lock().unwrap_or_else(|p| p.into_inner()).clone();
        String::from_utf8(bytes).unwrap()
    }

    /// The lines as logged: one WARN with the stable message and the
    /// counts, one INFO for the recovery.
    #[test]
    fn the_lines_carry_the_counts() {
        let text = captured(|| {
            let mut w = FdWatch::default();
            for open in [100, 992, 1000, 992, 500] {
                if let Some(line) = w.observe(at(open, 1024)) {
                    line.emit();
                }
            }
        });
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].contains("WARN"), "{text}");
        assert!(lines[0].contains(WARN_MESSAGE), "{text}");
        assert!(lines[0].contains("open=992 soft=1024 hard=4096 percent=96"), "{text}");
        assert!(lines[1].contains("INFO"), "{text}");
        assert!(lines[1].contains(RECOVERED_MESSAGE), "{text}");
        assert!(lines[1].contains("open=500 soft=1024 hard=4096 percent=48"), "{text}");
        assert_eq!(text.matches("WARN").count(), 1, "{text}");
    }

    /// The count is the listing's entries, files and directories alike
    /// (std's listing never yields `.` or `..`); a directory that cannot be
    /// listed is an error (a failed sample), never a count of zero.
    #[test]
    fn counting_a_listing_counts_its_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(count_entries(dir.path()).expect("empty"), 0);
        for name in ["0", "1", "2"] {
            std::fs::write(dir.path().join(name), b"").expect("file");
        }
        std::fs::create_dir(dir.path().join("3")).expect("subdir");
        assert_eq!(count_entries(dir.path()).expect("four"), 4);
        let err = count_entries(&dir.path().join("missing")).expect_err("missing");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{err}");
    }

    /// `RLIM_INFINITY` (all ones on Linux, `i64::MAX` on macOS) reads as
    /// NO_LIMIT, so the watch never divides by it; every other value is
    /// itself.
    #[cfg(unix)]
    #[test]
    fn rlim_infinity_reads_as_no_limit() {
        assert_eq!(limit_value(libc::RLIM_INFINITY), NO_LIMIT);
        for (v, want) in [(0, 0u64), (1, 1), (256, 256), (4096, 4096), (10240, 10240), (1 << 20, 1 << 20)] {
            assert_eq!(limit_value(v), want, "{want}");
        }
    }

    /// The descriptor directory names this process's descriptors, not only
    /// 0 to 2: a file this test holds open is listed by its number. Not a
    /// count, so other tests opening or closing files meanwhile change
    /// nothing (the file stays open for the whole listing).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn the_fd_dir_lists_a_file_held_open() {
        use std::os::fd::AsRawFd;
        let held = tempfile::tempfile().expect("tempfile");
        let fd = held.as_raw_fd().to_string();
        let names: Vec<String> = std::fs::read_dir(FD_DIR)
            .expect("list")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&fd), "fd {fd} not in {FD_DIR}: {names:?}");
        drop(held);
    }

    /// A real sample of this process (Linux, macOS): some files open, a
    /// soft limit no higher than the hard one.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_real_sample_counts_this_process() {
        let s = sample().expect("sample");
        assert!(s.open >= 3, "{s:?}");
        assert!(s.soft > 0 && s.soft <= s.hard, "{s:?}");
    }

    /// The approved cases: soft 256 / hard unlimited -> 10240; soft 256 /
    /// hard 4096 -> 4096; a soft limit at the target -> nothing to set; one
    /// above 10240 (or unlimited) is left alone, never lowered.
    #[test]
    fn the_raise_target_is_min_of_hard_and_10240_and_never_lower() {
        assert_eq!(RAISE_SOFT_TO, 10240);
        assert_eq!(raised_soft_limit(256, NO_LIMIT), Some(10240));
        assert_eq!(raised_soft_limit(256, 4096), Some(4096));
        assert_eq!(raised_soft_limit(256, 10240), Some(10240));
        assert_eq!(raised_soft_limit(256, 65536), Some(10240));
        assert_eq!(raised_soft_limit(10240, NO_LIMIT), None, "soft == target");
        assert_eq!(raised_soft_limit(4096, 4096), None, "soft == target == hard");
        assert_eq!(raised_soft_limit(256, 256), None, "soft == hard below 10240");
        for soft in [10241, 24576, 65536, 1 << 20, NO_LIMIT] {
            assert_eq!(raised_soft_limit(soft, NO_LIMIT), None, "{soft} / unlimited");
            assert_eq!(raised_soft_limit(soft, soft), None, "{soft} / {soft}");
        }
    }

    /// Over a grid of pairs (soft <= hard, as the kernel keeps them): a
    /// raise is above the soft limit, never above the hard one nor 10240;
    /// no raise means the soft limit is already at min(hard, 10240).
    #[test]
    fn the_raise_never_exceeds_hard_and_never_lowers_soft() {
        let values = [0, 1, 255, 256, 257, 1024, 4096, 10239, 10240, 10241, 65536, NO_LIMIT];
        for soft in values {
            for hard in values.into_iter().filter(|hard| *hard >= soft) {
                let target = hard.min(RAISE_SOFT_TO);
                match raised_soft_limit(soft, hard) {
                    Some(to) => {
                        assert_eq!(to, target, "{soft} / {hard}");
                        assert!(to > soft && to <= hard && to <= RAISE_SOFT_TO, "{soft} / {hard} -> {to}");
                    }
                    None => assert!(soft >= target, "{soft} / {hard}: {target} was above"),
                }
            }
        }
    }

    /// The pair to set changes rlim_cur only: rlim_max is the one read, bit
    /// for bit (RLIM_INFINITY too). Nothing when nothing is raised.
    #[cfg(unix)]
    #[test]
    fn the_new_rlimit_passes_the_hard_limit_through() {
        let raised = |rlim_cur, rlim_max| {
            raised_rlimit(libc::rlimit { rlim_cur, rlim_max }).map(|r| (r.rlim_cur, r.rlim_max))
        };
        let inf = libc::RLIM_INFINITY;
        assert_eq!(raised(256, inf), Some((10240, inf)));
        assert_eq!(raised(256, 4096), Some((4096, 4096)));
        assert_eq!(raised(256, 65536), Some((10240, 65536)));
        assert_eq!(raised(0, 10241), Some((10240, 10241)));
        assert_eq!(raised(10240, inf), None);
        assert_eq!(raised(4096, 4096), None);
        assert_eq!(raised(20000, 20000), None);
        assert_eq!(raised(inf, inf), None);
    }

    /// One read, then [`raise_with`] as the start runs it with the given
    /// read and set results: the outcome and every pair `set` was given.
    #[cfg(unix)]
    fn run_raise(
        read: std::io::Result<libc::rlimit>,
        result: std::io::Result<()>,
    ) -> (Raise, Vec<(libc::rlim_t, libc::rlim_t)>) {
        let mut calls = Vec::new();
        let out = raise_with(
            || read,
            |lim| {
                calls.push((lim.rlim_cur, lim.rlim_max));
                result
            },
        );
        (out, calls)
    }

    /// The wiring: when raising, one set, with the raised soft limit and
    /// the hard limit as read; no set when nothing is to be raised or the
    /// read failed; a failed set is the outcome (the start goes on).
    #[cfg(unix)]
    #[test]
    fn the_raise_sets_once_with_the_hard_limit_as_read() {
        let inf = libc::RLIM_INFINITY;
        let pair = |rlim_cur, rlim_max| Ok(libc::rlimit { rlim_cur, rlim_max });

        let (out, calls) = run_raise(pair(256, inf), Ok(()));
        assert!(matches!(out, Raise::Raised { from: 256, to: 10240, hard: NO_LIMIT }), "{out:?}");
        assert_eq!(calls, [(10240, inf)]);

        let (out, calls) = run_raise(pair(256, 4096), Ok(()));
        assert!(matches!(out, Raise::Raised { from: 256, to: 4096, hard: 4096 }), "{out:?}");
        assert_eq!(calls, [(4096, 4096)]);

        for (soft, hard) in [(10240, inf), (4096, 4096), (65536, inf), (inf, inf)] {
            let (out, calls) = run_raise(pair(soft, hard), Ok(()));
            assert!(matches!(out, Raise::Kept { .. }), "{out:?}");
            assert!(calls.is_empty(), "{calls:?}");
        }

        let (out, calls) = run_raise(Err(std::io::Error::from_raw_os_error(libc::EPERM)), Ok(()));
        assert!(matches!(out, Raise::ReadFailed { .. }), "{out:?}");
        assert!(calls.is_empty(), "{calls:?}");

        let (out, calls) = run_raise(pair(256, inf), Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
        match &out {
            Raise::SetFailed { from: 256, to: 10240, hard: NO_LIMIT, error } => {
                assert_eq!(error.raw_os_error(), Some(libc::EINVAL), "{out:?}")
            }
            _ => panic!("{out:?}"),
        }
        assert_eq!(calls, [(10240, inf)]);
    }

    /// The start's lines: one INFO from -> to (an unlimited hard limit shown
    /// as such), one WARN for a failed call, a debug line only when nothing
    /// was raised.
    #[test]
    fn the_raise_lines() {
        let err = || std::io::Error::from(std::io::ErrorKind::InvalidInput);

        let text = captured(|| Raise::Raised { from: 256, to: 10240, hard: NO_LIMIT }.emit());
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains("INFO") && !text.contains("WARN"), "{text}");
        assert!(text.contains(RAISED_MESSAGE), "{text}");
        assert!(text.contains("from=256 to=10240 hard=unlimited"), "{text}");

        let text = captured(|| Raise::SetFailed { from: 256, to: 4096, hard: 4096, error: err() }.emit());
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains("WARN"), "{text}");
        assert!(text.contains(RAISE_FAILED_MESSAGE), "{text}");
        assert!(text.contains("from=256 to=4096 hard=4096"), "{text}");
        assert!(text.contains("setrlimit"), "{text}");

        let text = captured(|| Raise::ReadFailed { error: err() }.emit());
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains("WARN") && text.contains(RAISE_FAILED_MESSAGE), "{text}");
        assert!(text.contains("getrlimit"), "{text}");

        let text = captured(|| Raise::Kept { soft: 10240, hard: NO_LIMIT }.emit());
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains("DEBUG"), "{text}");
        assert!(!text.contains("INFO") && !text.contains("WARN"), "{text}");
        assert!(text.contains("soft=10240 hard=unlimited"), "{text}");
    }

    /// macOS, live: the start's raise is accepted by this kernel (no EINVAL
    /// at min(hard, 10240)) and leaves the hard limit as it was. It raises
    /// this test process's own soft limit, as the daemon raises its own.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_raise_is_accepted_on_macos_and_keeps_the_hard_limit() {
        let before = nofile_rlimit().expect("getrlimit");
        let out = raise_at_start();
        assert!(matches!(out, Raise::Raised { .. } | Raise::Kept { .. }), "{out:?}");
        let after = nofile_rlimit().expect("getrlimit");
        let pairs = (before.rlim_cur, before.rlim_max, after.rlim_cur, after.rlim_max);
        assert_eq!(after.rlim_max, before.rlim_max, "(cur, max) before, after: {pairs:?}");
        let target = limit_value(before.rlim_max).min(RAISE_SOFT_TO);
        assert!(limit_value(after.rlim_cur) >= target, "(cur, max) before, after: {pairs:?}");
    }

    /// The task ends with the daemon's shutdown.
    #[tokio::test]
    async fn the_watch_stops_with_the_daemon() {
        let token = CancellationToken::new();
        let task = tokio::spawn(run(token.clone()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
        tokio::time::timeout(Duration::from_secs(5), task).await.expect("stopped").expect("no panic");
    }
}
