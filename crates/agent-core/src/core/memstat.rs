//! Process memory instrumentation (§3.7): the numbers `mem.sh` reads from
//! `/proc`, available in-process and via `synaps status --memory` — no root,
//! no `smem`.
//!
//! Linux only. Every entry point degrades to `Unsupported` / zeroed fields on
//! other platforms so callers never need their own `cfg`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// What a process in a session tree is, classified from its cmdline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcRole {
    Engine,
    ExtensionSidecar { name: String },
    McpServer { name: String },
    Shell,
    Other,
}

/// One process' memory, straight from `smaps_rollup` + `status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcMem {
    pub pid: u32,
    pub ppid: u32,
    pub cmd: String,
    pub role: ProcRole,
    pub rss_kb: u64,
    pub pss_kb: u64,
    /// `Private_Clean + Private_Dirty` — the smem "USS".
    pub uss_kb: u64,
    /// `RssAnon` from `status`.
    pub anon_kb: u64,
    pub threads: u32,
}

/// In-process snapshot: kernel view + jemalloc's own accounting (zeros when
/// the build does not link jemalloc-ctl).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct SelfMem {
    pub rss_kb: u64,
    pub rss_anon_kb: u64,
    pub threads: u32,
    pub jemalloc_allocated_kb: u64,
    pub jemalloc_active_kb: u64,
    pub jemalloc_resident_kb: u64,
    pub jemalloc_retained_kb: u64,
}

/// Totals over a set of [`ProcMem`] rows.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct MemTotals {
    pub procs: usize,
    pub rss_kb: u64,
    pub pss_kb: u64,
    pub uss_kb: u64,
    pub anon_kb: u64,
    pub threads: u32,
}

impl MemTotals {
    pub fn of(rows: &[ProcMem]) -> Self {
        rows.iter().fold(Self::default(), |mut t, r| {
            t.procs += 1;
            t.rss_kb += r.rss_kb;
            t.pss_kb += r.pss_kb;
            t.uss_kb += r.uss_kb;
            t.anon_kb += r.anon_kb;
            t.threads += r.threads;
            t
        })
    }
}

#[cfg_attr(target_os = "linux", allow(dead_code))]
fn unsupported() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "memstat: /proc walking is Linux-only",
    )
}

/// Walk `pid` and every descendant (via `/proc/*/stat` ppid), reading
/// `smaps_rollup` and `status` for each. Root first, then children in
/// discovery order. Processes that vanish mid-walk are skipped.
pub fn tree(pid: u32) -> std::io::Result<Vec<ProcMem>> {
    #[cfg(target_os = "linux")]
    {
        linux::tree(Path::new("/proc"), pid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        Err(unsupported())
    }
}

/// Memory of exactly one process (no descendants).
pub fn process(pid: u32) -> std::io::Result<ProcMem> {
    #[cfg(target_os = "linux")]
    {
        linux::read_proc(Path::new("/proc"), pid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        Err(unsupported())
    }
}

/// Kernel + allocator view of the calling process. Never fails; fields are
/// zero where unavailable.
pub fn self_snapshot() -> SelfMem {
    let mut snap = SelfMem::default();
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = linux::read_status(Path::new("/proc/self/status")) {
            snap.rss_kb = s.rss_kb;
            snap.rss_anon_kb = s.anon_kb;
            snap.threads = s.threads;
        }
    }
    #[cfg(all(unix, not(target_env = "musl")))]
    {
        use tikv_jemalloc_ctl::{epoch, stats};
        if epoch::advance().is_ok() {
            snap.jemalloc_allocated_kb = stats::allocated::read().unwrap_or(0) as u64 / 1024;
            snap.jemalloc_active_kb = stats::active::read().unwrap_or(0) as u64 / 1024;
            snap.jemalloc_resident_kb = stats::resident::read().unwrap_or(0) as u64 / 1024;
            snap.jemalloc_retained_kb = stats::retained::read().unwrap_or(0) as u64 / 1024;
        }
    }
    snap
}

/// Ask jemalloc to purge every arena's dirty/muzzy pages back to the OS —
/// the manual `malloc_trim` equivalent. No-op on builds without jemalloc.
pub fn purge_arenas() {
    #[cfg(all(unix, not(target_env = "musl")))]
    {
        // MALLCTL_ARENAS_ALL = 4096; `arena.<i>.purge` is a void mallctl
        // (jemalloc rejects a non-null newp), so issue it as a zero-length read.
        let _ = unsafe { tikv_jemalloc_ctl::raw::read::<()>(b"arena.4096.purge\0") };
    }
}

/// One `synaps::mem` info line — call at turn end so `synaps.log` carries a
/// greppable per-turn memory trace.
pub fn log_turn_memory() {
    let s = self_snapshot();
    tracing::info!(
        target: "synaps::mem",
        rss_anon_kb = s.rss_anon_kb,
        jemalloc_allocated_kb = s.jemalloc_allocated_kb,
        jemalloc_resident_kb = s.jemalloc_resident_kb,
        threads = s.threads,
        "turn memory"
    );
}

/// Classify a process from its cmdline (argv joined by spaces) and, when
/// known, the engine binary's own name.
pub fn classify(cmd: &str) -> ProcRole {
    let argv: Vec<&str> = cmd.split(' ').filter(|s| !s.is_empty()).collect();
    let Some(&argv0) = argv.first() else {
        return ProcRole::Other;
    };
    let base = Path::new(argv0)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if base == "synaps" || base.starts_with("synaps-cli") {
        return ProcRole::Engine;
    }
    if base == "bash" || base == "sh" || base == "zsh" || base == "fish" || base == "pwsh" {
        return ProcRole::Shell;
    }
    // Extension sidecars are launched from a plugin directory:
    // `.../plugins/<name>/<entry>` or `.../extensions/<name>/...`.
    for arg in &argv {
        if let Some(name) = plugin_name_from_path(arg) {
            return ProcRole::ExtensionSidecar { name };
        }
    }
    if argv.iter().any(|a| a.contains("mcp")) {
        let name = Path::new(argv0)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| argv0.to_string());
        return ProcRole::McpServer { name };
    }
    ProcRole::Other
}

fn plugin_name_from_path(arg: &str) -> Option<String> {
    let p = Path::new(arg);
    let comps: Vec<String> = p
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    for marker in ["plugins", "extensions"] {
        if let Some(i) = comps.iter().position(|c| c == marker) {
            if let Some(name) = comps.get(i + 1) {
                if i + 2 < comps.len() {
                    return Some(name.clone());
                }
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    #[derive(Default)]
    pub(super) struct Status {
        pub ppid: u32,
        pub rss_kb: u64,
        pub anon_kb: u64,
        pub threads: u32,
    }

    fn kb(line: &str) -> u64 {
        line.split_whitespace()
            .nth(1)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    pub(super) fn read_status(path: &Path) -> std::io::Result<Status> {
        let text = std::fs::read_to_string(path)?;
        let mut s = Status::default();
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("PPid:") {
                s.ppid = v.trim().parse().unwrap_or(0);
            } else if line.starts_with("VmRSS:") {
                s.rss_kb = kb(line);
            } else if line.starts_with("RssAnon:") {
                s.anon_kb = kb(line);
            } else if let Some(v) = line.strip_prefix("Threads:") {
                s.threads = v.trim().parse().unwrap_or(0);
            }
        }
        Ok(s)
    }

    struct Rollup {
        rss_kb: u64,
        pss_kb: u64,
        uss_kb: u64,
    }

    fn read_rollup(path: &Path) -> std::io::Result<Rollup> {
        let text = std::fs::read_to_string(path)?;
        let (mut rss, mut pss, mut pc, mut pd) = (0, 0, 0, 0);
        for line in text.lines() {
            if line.starts_with("Rss:") {
                rss = kb(line);
            } else if line.starts_with("Pss:") {
                pss = kb(line);
            } else if line.starts_with("Private_Clean:") {
                pc = kb(line);
            } else if line.starts_with("Private_Dirty:") {
                pd = kb(line);
            }
        }
        Ok(Rollup {
            rss_kb: rss,
            pss_kb: pss,
            uss_kb: pc + pd,
        })
    }

    fn read_cmdline(path: &Path) -> String {
        std::fs::read(path)
            .map(|b| {
                b.split(|c| *c == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
    }

    pub(super) fn read_proc(proc_root: &Path, pid: u32) -> std::io::Result<ProcMem> {
        let dir = proc_root.join(pid.to_string());
        let status = read_status(&dir.join("status"))?;
        let rollup = read_rollup(&dir.join("smaps_rollup")).unwrap_or(Rollup {
            rss_kb: status.rss_kb,
            pss_kb: 0,
            uss_kb: 0,
        });
        let cmd = read_cmdline(&dir.join("cmdline"));
        let role = classify(&cmd);
        Ok(ProcMem {
            pid,
            ppid: status.ppid,
            cmd,
            role,
            rss_kb: rollup.rss_kb,
            pss_kb: rollup.pss_kb,
            uss_kb: rollup.uss_kb,
            anon_kb: status.anon_kb,
            threads: status.threads,
        })
    }

    /// `ppid -> [pid]` for every process visible under `proc_root`.
    fn children_map(proc_root: &Path) -> HashMap<u32, Vec<u32>> {
        let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
        let Ok(entries) = std::fs::read_dir(proc_root) else {
            return map;
        };
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            // `pid (comm) state ppid ...` — comm may contain spaces/parens.
            let Some(after) = stat.rfind(')') else {
                continue;
            };
            let ppid = stat[after + 1..]
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0);
            map.entry(ppid).or_default().push(pid);
        }
        for v in map.values_mut() {
            v.sort_unstable();
        }
        map
    }

    pub(super) fn tree(proc_root: &Path, pid: u32) -> std::io::Result<Vec<ProcMem>> {
        let root = read_proc(proc_root, pid)?;
        let children = children_map(proc_root);
        let mut out = vec![root];
        let mut queue = std::collections::VecDeque::from([pid]);
        while let Some(p) = queue.pop_front() {
            if let Some(kids) = children.get(&p) {
                for &k in kids {
                    if let Ok(pm) = read_proc(proc_root, k) {
                        out.push(pm);
                        queue.push_back(k);
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_roles_from_cmdline() {
        assert_eq!(classify("/usr/local/bin/synaps"), ProcRole::Engine);
        assert_eq!(classify("synaps server --port 1"), ProcRole::Engine);
        assert_eq!(classify("bash -c ls"), ProcRole::Shell);
        assert_eq!(
            classify("node /home/u/.synaps-cli/plugins/munder-hive-god/bridge.cjs"),
            ProcRole::ExtensionSidecar {
                name: "munder-hive-god".into()
            }
        );
        assert_eq!(
            classify("python3 /home/u/.synaps-cli/plugins/synaps-chronos/main.py"),
            ProcRole::ExtensionSidecar {
                name: "synaps-chronos".into()
            }
        );
        assert_eq!(
            classify("npx -y mcp-server-filesystem /tmp"),
            ProcRole::McpServer { name: "npx".into() }
        );
        assert_eq!(classify(""), ProcRole::Other);
    }

    #[test]
    fn totals_sum_rows() {
        let row = |pss| ProcMem {
            pid: 1,
            ppid: 0,
            cmd: String::new(),
            role: ProcRole::Other,
            rss_kb: 10,
            pss_kb: pss,
            uss_kb: 1,
            anon_kb: 2,
            threads: 3,
        };
        let t = MemTotals::of(&[row(5), row(7)]);
        assert_eq!(
            (t.procs, t.rss_kb, t.pss_kb, t.uss_kb, t.anon_kb, t.threads),
            (2, 20, 12, 2, 4, 6)
        );
    }

    #[cfg_attr(not(target_os = "linux"), ignore)]
    #[test]
    fn tree_of_self_starts_with_self() {
        let me = std::process::id();
        let rows = tree(me).unwrap();
        assert_eq!(rows[0].pid, me);
        assert!(rows[0].rss_kb > 0, "rss must be non-zero");
        assert!(rows[0].pss_kb > 0, "pss must be non-zero (smaps_rollup)");
        assert!(rows[0].threads >= 1);
        // Non-root rows are descendants: their ppid is in the set.
        let pids: std::collections::HashSet<u32> = rows.iter().map(|r| r.pid).collect();
        for r in &rows[1..] {
            assert!(pids.contains(&r.ppid), "{} not a descendant of {me}", r.pid);
        }
    }

    #[cfg_attr(not(target_os = "linux"), ignore)]
    #[test]
    fn self_snapshot_has_kernel_numbers() {
        let s = self_snapshot();
        assert!(s.rss_kb > 0);
        assert!(s.rss_anon_kb > 0);
        assert!(s.threads >= 1);
        // The value must not panic; jemalloc fields are zero on non-jemalloc builds.
        purge_arenas();
    }
}
