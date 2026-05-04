//! Background install progress: shared state for the in-flight `git clone`
//! and the parser that extracts phase / percentage from `git clone --progress`
//! stderr output.
//!
//! Git emits progress on stderr using carriage returns (`\r`) to overwrite
//! the same line, e.g.:
//!
//! ```text
//! remote: Counting objects:  37% (124/333)        \r
//! Receiving objects:  78% (260/333)\r
//! Receiving objects: 100% (333/333), 364.81 KiB | 2.05 MiB/s, done.\n
//! Resolving deltas:  56% (9/16)\r
//! ```
//!
//! The reader chunks stderr on either `\r` or `\n`, hands each chunk to
//! [`parse_progress_line`], and pushes any extracted [`CloneProgress`]
//! into the shared [`InstallProgress`].

use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Coarse-grained phase of a `git clone --depth=1`. Maps 1:1 to the prefixes
/// git emits before its `N% (a/b)` counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClonePhase {
    /// Before any progress line has been seen — DNS, TLS, connect.
    Connecting,
    /// `remote: Counting objects: N% (a/b)`
    Counting,
    /// `remote: Compressing objects: N% (a/b)`
    Compressing,
    /// `Receiving objects: N% (a/b)` — the actual network download.
    Receiving,
    /// `Resolving deltas: N% (a/b)` — local CPU work after download completes.
    Resolving,
    /// Clone finished cleanly.
    Done,
    /// Post-clone setup script is running.
    SetupRunning,
    /// Failed before completion.
    Failed,
}

impl ClonePhase {
    pub fn label(&self) -> &'static str {
        match self {
            ClonePhase::Connecting => "Connecting…",
            ClonePhase::Counting => "Counting objects…",
            ClonePhase::Compressing => "Compressing objects…",
            ClonePhase::Receiving => "Receiving objects…",
            ClonePhase::Resolving => "Resolving deltas…",
            ClonePhase::Done => "Clone complete",
            ClonePhase::SetupRunning => "Running setup script…",
            ClonePhase::Failed => "Failed",
        }
    }
}

/// One snapshot of clone progress, parsed out of a single git progress line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneProgress {
    pub phase: ClonePhase,
    /// 0–100. `None` when the phase is Connecting/Done/Failed/SetupRunning.
    pub percent: Option<u8>,
    /// `(current, total)` in objects/deltas — `None` for non-counting phases.
    pub counts: Option<(u64, u64)>,
    /// Raw throughput string from git (e.g. "2.05 MiB/s") if present.
    pub throughput: Option<String>,
}

impl CloneProgress {
    pub fn new(phase: ClonePhase) -> Self {
        Self {
            phase,
            percent: None,
            counts: None,
            throughput: None,
        }
    }
}

/// Shared mutable state for the install background task. The action layer
/// hands an `Arc<Mutex<InstallProgress>>` to both the spawned clone task
/// (writer) and the modal state (reader). The UI render path reads it
/// through a short-lived lock on every frame; updates happen at most
/// once per git progress line.
#[derive(Debug)]
pub struct InstallProgress {
    pub plugin_name: String,
    pub started_at: Instant,
    pub phase: ClonePhase,
    pub percent: Option<u8>,
    pub counts: Option<(u64, u64)>,
    pub throughput: Option<String>,
    /// Animated spinner frame, advanced by the UI tick (not the clone thread).
    pub spinner_frame: u8,
    /// Last line of git output we couldn't parse — useful for diagnostics
    /// when something goes wrong; rendered subtly under the bar.
    pub last_raw_line: Option<String>,
}

impl InstallProgress {
    pub fn new(plugin_name: impl Into<String>) -> Self {
        Self {
            plugin_name: plugin_name.into(),
            started_at: Instant::now(),
            phase: ClonePhase::Connecting,
            percent: None,
            counts: None,
            throughput: None,
            spinner_frame: 0,
            last_raw_line: None,
        }
    }

    /// Apply a parsed progress snapshot to this state. Phases only advance
    /// monotonically (Connecting → Counting → Compressing → Receiving →
    /// Resolving → Done) so a stray late line from a prior phase doesn't
    /// regress the bar.
    pub fn apply(&mut self, p: CloneProgress) {
        if phase_order(p.phase) < phase_order(self.phase) {
            // Ignore regressions but keep updating ancillary fields if same phase.
            return;
        }
        self.phase = p.phase;
        if p.percent.is_some() {
            self.percent = p.percent;
        }
        if p.counts.is_some() {
            self.counts = p.counts;
        }
        if p.throughput.is_some() {
            self.throughput = p.throughput;
        }
    }

    pub fn fail(&mut self, msg: impl Into<String>) {
        self.phase = ClonePhase::Failed;
        self.last_raw_line = Some(msg.into());
    }

    pub fn finish_clone(&mut self) {
        self.phase = ClonePhase::Done;
        self.percent = Some(100);
    }

    pub fn set_setup_running(&mut self) {
        self.phase = ClonePhase::SetupRunning;
        self.percent = None;
    }

    pub fn tick_spinner(&mut self) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
    }
}

fn phase_order(p: ClonePhase) -> u8 {
    match p {
        ClonePhase::Connecting => 0,
        ClonePhase::Counting => 1,
        ClonePhase::Compressing => 2,
        ClonePhase::Receiving => 3,
        ClonePhase::Resolving => 4,
        ClonePhase::Done => 5,
        ClonePhase::SetupRunning => 6,
        ClonePhase::Failed => 7,
    }
}

/// Convenience: shared, lockable handle to install progress.
pub type InstallProgressHandle = Arc<Mutex<InstallProgress>>;

/// Parse one git progress chunk (between `\r` or `\n` boundaries).
/// Returns `Some(CloneProgress)` only for known phase lines; everything
/// else (cloning into…, hostname banner, etc.) returns `None`.
///
/// Recognised forms:
/// - `remote: Counting objects:  37% (124/333)`
/// - `remote: Compressing objects: 100% (258/258), done.`
/// - `Receiving objects:  78% (260/333)`
/// - `Receiving objects: 100% (333/333), 364.81 KiB | 2.05 MiB/s, done.`
/// - `Resolving deltas:  56% (9/16)`
pub fn parse_progress_line(raw: &str) -> Option<CloneProgress> {
    let line = raw.trim();
    if line.is_empty() {
        return None;
    }
    // Strip optional "remote: " prefix
    let line = line.strip_prefix("remote: ").unwrap_or(line);

    let (phase, rest) = if let Some(r) = line.strip_prefix("Counting objects:") {
        (ClonePhase::Counting, r)
    } else if let Some(r) = line.strip_prefix("Compressing objects:") {
        (ClonePhase::Compressing, r)
    } else if let Some(r) = line.strip_prefix("Receiving objects:") {
        (ClonePhase::Receiving, r)
    } else if let Some(r) = line.strip_prefix("Resolving deltas:") {
        (ClonePhase::Resolving, r)
    } else {
        return None;
    };

    let rest = rest.trim_start();
    // Expected: "<pct>% (<a>/<b>)[, <throughput>][, done.]"
    // Parse percent
    let (pct_str, rest) = rest.split_once('%')?;
    let percent: u8 = pct_str.trim().parse().ok()?;

    // Parse "(a/b)"
    let counts = rest
        .trim_start()
        .strip_prefix('(')
        .and_then(|r| r.split_once(')'))
        .and_then(|(inner, _)| {
            let (a, b) = inner.split_once('/')?;
            Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
        });

    // Parse throughput — only present on Receiving lines like
    // ", 364.81 KiB | 2.05 MiB/s, done." — extract the "X.YY UNIT/s" token.
    let throughput = extract_throughput(rest);

    Some(CloneProgress {
        phase,
        percent: Some(percent.min(100)),
        counts,
        throughput,
    })
}

fn extract_throughput(rest: &str) -> Option<String> {
    let idx = rest.find('|')?;
    let after = rest[idx + 1..].trim();
    // Take up to the next comma or end.
    let end = after.find(',').unwrap_or(after.len());
    let candidate = after[..end].trim();
    if candidate.ends_with("/s") {
        Some(candidate.to_string())
    } else {
        None
    }
}

/// Split a buffer on either CR or LF into chunks. Used by the stderr reader
/// since git uses `\r` to overwrite the same line.
pub fn split_progress_chunks(buf: &str) -> Vec<&str> {
    buf.split(|c: char| c == '\r' || c == '\n')
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_counting_with_remote_prefix() {
        let p = parse_progress_line("remote: Counting objects:  37% (124/333)        ").unwrap();
        assert_eq!(p.phase, ClonePhase::Counting);
        assert_eq!(p.percent, Some(37));
        assert_eq!(p.counts, Some((124, 333)));
        assert_eq!(p.throughput, None);
    }

    #[test]
    fn parses_compressing_done_line() {
        let p =
            parse_progress_line("remote: Compressing objects: 100% (258/258), done.").unwrap();
        assert_eq!(p.phase, ClonePhase::Compressing);
        assert_eq!(p.percent, Some(100));
        assert_eq!(p.counts, Some((258, 258)));
    }

    #[test]
    fn parses_receiving_without_throughput() {
        let p = parse_progress_line("Receiving objects:  78% (260/333)").unwrap();
        assert_eq!(p.phase, ClonePhase::Receiving);
        assert_eq!(p.percent, Some(78));
        assert_eq!(p.counts, Some((260, 333)));
        assert_eq!(p.throughput, None);
    }

    #[test]
    fn parses_receiving_with_throughput() {
        let p = parse_progress_line(
            "Receiving objects: 100% (333/333), 364.81 KiB | 2.05 MiB/s, done.",
        )
        .unwrap();
        assert_eq!(p.phase, ClonePhase::Receiving);
        assert_eq!(p.percent, Some(100));
        assert_eq!(p.counts, Some((333, 333)));
        assert_eq!(p.throughput.as_deref(), Some("2.05 MiB/s"));
    }

    #[test]
    fn parses_resolving_deltas() {
        let p = parse_progress_line("Resolving deltas:  56% (9/16)").unwrap();
        assert_eq!(p.phase, ClonePhase::Resolving);
        assert_eq!(p.percent, Some(56));
        assert_eq!(p.counts, Some((9, 16)));
    }

    #[test]
    fn ignores_cloning_into_banner() {
        assert!(parse_progress_line("Cloning into '/tmp/x'...").is_none());
    }

    #[test]
    fn ignores_total_summary() {
        assert!(
            parse_progress_line("remote: Total 333 (delta 16), reused 266 (delta 13)").is_none()
        );
    }

    #[test]
    fn ignores_blank_line() {
        assert!(parse_progress_line("").is_none());
        assert!(parse_progress_line("   ").is_none());
    }

    #[test]
    fn split_chunks_handles_carriage_returns() {
        let buf = "Receiving objects:   0% (1/333)\rReceiving objects:   1% (4/333)\rReceiving objects:   2% (7/333)\n";
        let chunks = split_progress_chunks(buf);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0], "Receiving objects:   0% (1/333)");
        assert_eq!(chunks[2], "Receiving objects:   2% (7/333)");
    }

    #[test]
    fn split_chunks_skips_empty_pieces() {
        let buf = "\r\n\rfoo\rbar\n";
        let chunks = split_progress_chunks(buf);
        assert_eq!(chunks, vec!["foo", "bar"]);
    }

    #[test]
    fn install_progress_apply_advances_phase() {
        let mut s = InstallProgress::new("test");
        assert_eq!(s.phase, ClonePhase::Connecting);

        s.apply(parse_progress_line("remote: Counting objects: 50% (5/10)").unwrap());
        assert_eq!(s.phase, ClonePhase::Counting);
        assert_eq!(s.percent, Some(50));

        s.apply(parse_progress_line("Receiving objects: 25% (10/40)").unwrap());
        assert_eq!(s.phase, ClonePhase::Receiving);
        assert_eq!(s.percent, Some(25));
        assert_eq!(s.counts, Some((10, 40)));
    }

    #[test]
    fn install_progress_apply_ignores_phase_regression() {
        // A late "Counting" line arriving after we've moved to Receiving must
        // not drag us back — git can interleave server-side counting with the
        // start of receiving on fast clones.
        let mut s = InstallProgress::new("test");
        s.apply(parse_progress_line("Receiving objects: 30% (10/33)").unwrap());
        assert_eq!(s.phase, ClonePhase::Receiving);
        assert_eq!(s.percent, Some(30));

        s.apply(parse_progress_line("remote: Counting objects: 99% (32/33)").unwrap());
        assert_eq!(s.phase, ClonePhase::Receiving, "phase must not regress");
        assert_eq!(s.percent, Some(30), "percent must not regress");
    }

    #[test]
    fn install_progress_finish_clone_jumps_to_100() {
        let mut s = InstallProgress::new("test");
        s.apply(parse_progress_line("Receiving objects: 60% (20/33)").unwrap());
        s.finish_clone();
        assert_eq!(s.phase, ClonePhase::Done);
        assert_eq!(s.percent, Some(100));
    }

    #[test]
    fn install_progress_fail_records_message() {
        let mut s = InstallProgress::new("test");
        s.fail("git: connection refused");
        assert_eq!(s.phase, ClonePhase::Failed);
        assert_eq!(s.last_raw_line.as_deref(), Some("git: connection refused"));
    }

    #[test]
    fn tick_spinner_wraps() {
        let mut s = InstallProgress::new("test");
        for _ in 0..256 {
            s.tick_spinner();
        }
        // No panic = wrap behaved.
        assert_eq!(s.spinner_frame, 0);
    }
}
