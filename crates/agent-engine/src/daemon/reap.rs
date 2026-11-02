//! Reap the children this image inherited across a reload exec.
//!
//! `daemon reload` execs the new binary in place (same pid), so every child
//! process the old image had at exec becomes a child of the new image, which
//! holds no handle on it: a tool command killed during the drain whose exit
//! the old image had not collected yet, a PTY shell that did not die within
//! its drop's short reap bound, an MCP server whose pipes closed at exec.
//! Without a reaper each stays a zombie for the daemon's lifetime, and every
//! reload adds more.
//!
//! [`snapshot`] lists this process's children before the image starts any
//! process of its own (the top of `run_foreground`); a freshly started
//! process has none, so the list is non-empty only after an exec in place.
//! [`spawn_reaper`] then waits for exactly those pids, `waitpid(pid,
//! WNOHANG)` one by one: never `waitpid(-1)` or any wait on pids this image
//! spawned itself, so `tokio::process` and `std::process` (which wait only
//! on the pids of the `Child` handles they own) keep every exit status. A
//! pid leaves the set when it is reaped, or on `ECHILD` (no longer ours). A
//! child's pid cannot be reused while it is alive or a zombie, and a reaped
//! pid is dropped at once, so the set never names someone else's process.
//! The reaper wakes on SIGCHLD with a 60 s fallback tick and ends when the
//! set is empty.
//!
//! This relies on two invariants of the daemon process:
//! - It is NOT a child subreaper (`PR_SET_CHILD_SUBREAPER` is never set):
//!   orphaned grandchildren (the detached jobs of tool commands) go to the
//!   user's service manager, so the direct children are all there is to
//!   reap. If the daemon ever becomes a subreaper, this module must be
//!   revisited: grandchildren reparented to it after the snapshot would be
//!   missed.
//! - No other code in it waits on pids it does not own: no `waitpid(-1)`,
//!   `wait()` or `waitid(P_ALL, ..)`, and SIGCHLD is never set to `SIG_IGN`
//!   (which makes the kernel reap every child at once). Such a waiter could
//!   reap an inherited pid before this reaper does; the kernel may then give
//!   that pid to a new child of this image, and the reaper's
//!   `waitpid(pid, WNOHANG)` would take that child's exit status from the
//!   `Child` handle that owns it. (It would also take exit statuses from
//!   `tokio::process` and `std::process` directly.) An audit of the daemon
//!   and its dependencies (tokio, std, portable-pty, the MCP and extension
//!   runtimes, the signal handlers) found no such waiter; anything that adds
//!   one must revisit this module.
//!
//! Linux only (the snapshot reads `/proc`); elsewhere the snapshot is empty.

#[cfg(target_os = "linux")]
use std::time::Duration;

/// Children of this process when the image started (pids, ascending).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Inherited {
    pids: Vec<i32>,
}

impl Inherited {
    pub fn pids(&self) -> &[i32] {
        &self.pids
    }

    pub fn is_empty(&self) -> bool {
        self.pids.is_empty()
    }
}

/// Fallback wake-up between SIGCHLDs (a missed or coalesced signal is
/// caught here).
#[cfg(target_os = "linux")]
const TICK: Duration = Duration::from_secs(60);

/// This process's children now. Call it before the image starts any process
/// of its own: every pid listed is waited for by [`spawn_reaper`].
pub fn snapshot() -> Inherited {
    #[cfg(target_os = "linux")]
    {
        let mut pids = linux::children_from_task_files().unwrap_or_else(linux::children_from_ppid_scan);
        pids.sort_unstable();
        pids.dedup();
        Inherited { pids }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Inherited::default()
    }
}

/// Reap the inherited children that have already exited, log one INFO line
/// when there were any, and leave a task waiting for the rest. Needs a tokio
/// runtime when some are still running.
pub fn spawn_reaper(inherited: Inherited) {
    #[cfg(target_os = "linux")]
    {
        if inherited.is_empty() {
            return;
        }
        let total = inherited.pids.len();
        // Registered before the first sweep: a child that exits after the
        // sweep raises a SIGCHLD this stream sees.
        let sigchld = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child()).ok();
        let mut running = inherited.pids;
        let exited = linux::reap_exited(&mut running);
        tracing::info!(
            inherited = total,
            exited,
            "daemon: inherited {total} children, {exited} already exited and reaped"
        );
        if running.is_empty() {
            return;
        }
        tokio::spawn(wait_for_the_rest(running, sigchld));
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = inherited;
    }
}

#[cfg(target_os = "linux")]
async fn wait_for_the_rest(mut running: Vec<i32>, mut sigchld: Option<tokio::signal::unix::Signal>) {
    loop {
        let signals_ended = tokio::select! {
            ended = async {
                match sigchld.as_mut() {
                    Some(s) => s.recv().await.is_none(),
                    None => std::future::pending::<bool>().await,
                }
            } => ended,
            _ = tokio::time::sleep(TICK) => false,
        };
        if signals_ended {
            // The SIGCHLD stream returns `None` once it can deliver no more
            // signals, and would then return at once on every turn: wake on
            // the tick alone from here on instead of spinning.
            sigchld = None;
        }
        linux::reap_exited(&mut running);
        if running.is_empty() {
            tracing::debug!("daemon: every inherited child reaped");
            return;
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::BTreeSet;

    /// `/proc/self/task/*/children` (needs CONFIG_PROC_CHILDREN). `None` when
    /// any of it can't be read, so the caller falls back to the ppid scan.
    pub(super) fn children_from_task_files() -> Option<Vec<i32>> {
        let mut out = BTreeSet::new();
        for task in std::fs::read_dir("/proc/self/task").ok()? {
            let text = std::fs::read_to_string(task.ok()?.path().join("children")).ok()?;
            for pid in text.split_whitespace() {
                out.insert(pid.parse().ok()?);
            }
        }
        Some(out.into_iter().collect())
    }

    /// Every `/proc/<pid>/stat` whose parent pid is ours.
    pub(super) fn children_from_ppid_scan() -> Vec<i32> {
        let me = std::process::id() as i32;
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        dir.flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
            .filter(|&pid| pid != me)
            .filter(|&pid| {
                std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .ok()
                    .and_then(|stat| parent_of(&stat))
                    == Some(me)
            })
            .collect()
    }

    /// The ppid field of a `/proc/<pid>/stat` line (the 4th field; the 2nd,
    /// `(comm)`, may contain spaces and parentheses, so count from the last
    /// `)`).
    pub(super) fn parent_of(stat: &str) -> Option<i32> {
        let rest = &stat[stat.rfind(')')? + 1..];
        rest.split_whitespace().nth(1)?.parse().ok()
    }

    /// `waitpid(pid, WNOHANG)` for each pid in `pids`; drops the reaped ones
    /// and those that are no longer our children (`ECHILD`). Returns how
    /// many were reaped.
    pub(super) fn reap_exited(pids: &mut Vec<i32>) -> usize {
        let mut reaped = 0;
        pids.retain(|&pid| {
            let mut status = 0;
            // SAFETY: waitpid on one specific pid, a valid out-pointer.
            let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if r == pid {
                tracing::debug!(pid, status, "daemon: reaped an inherited child");
                reaped += 1;
                false
            } else if r == 0 {
                true
            } else {
                // ECHILD: not (or no longer) our child; EINTR: try again at
                // the next wake-up.
                std::io::Error::last_os_error().raw_os_error() != Some(libc::ECHILD)
            }
        });
        reaped
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn the_ppid_field_is_read_past_a_comm_with_spaces_and_parens() {
        assert_eq!(linux::parent_of("123 (npm exec mcp-re) Z 456 123 123 0"), Some(456));
        assert_eq!(linux::parent_of("7 (a) b) (c) S 1 7 7"), Some(1));
        assert_eq!(linux::parent_of("garbage"), None);
    }

    #[test]
    fn both_listings_name_a_child_of_this_process() {
        let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        let pid = child.id() as i32;
        assert!(linux::children_from_ppid_scan().contains(&pid));
        if let Some(pids) = linux::children_from_task_files() {
            assert!(pids.contains(&pid));
        }
        assert!(snapshot().pids().contains(&pid));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn reaping_takes_exited_pids_and_drops_foreign_ones() {
        let child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        // Leave the std handle unwaited: a zombie only this reaper collects.
        std::mem::forget(child);
        let t0 = std::time::Instant::now();
        while !std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap().contains(") Z ") {
            assert!(t0.elapsed() < Duration::from_secs(5), "`true` never exited");
            std::thread::sleep(Duration::from_millis(10));
        }
        // pid 1 is not our child: dropped (ECHILD), not counted.
        let mut pids = vec![1, pid];
        assert_eq!(linux::reap_exited(&mut pids), 1);
        assert!(pids.is_empty());
        assert!(std::fs::metadata(format!("/proc/{pid}")).is_err(), "reaped");
    }
}
