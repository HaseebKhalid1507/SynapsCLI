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
    /// Redacted cmdline — see [`redact_cmdline`]. Never the raw argv: MCP
    /// servers take API keys as arguments and this struct is serialized by
    /// `synaps status --memory --json`.
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
    #[serde(default)]
    pub jemalloc_metadata_kb: u64,
    /// `AnonHugePages` from `smaps_rollup` — THP-backed anon (a 2 MiB huge
    /// page per touched thread stack / bss / arena chunk when THP=always).
    #[serde(default)]
    pub anon_huge_kb: u64,
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
        snap.anon_huge_kb = linux::anon_huge_kb(Path::new("/proc/self/smaps_rollup"));
    }
    #[cfg(all(unix, not(target_env = "musl")))]
    {
        use tikv_jemalloc_ctl::{epoch, stats};
        if epoch::advance().is_ok() {
            snap.jemalloc_allocated_kb = stats::allocated::read().unwrap_or(0) as u64 / 1024;
            snap.jemalloc_active_kb = stats::active::read().unwrap_or(0) as u64 / 1024;
            snap.jemalloc_resident_kb = stats::resident::read().unwrap_or(0) as u64 / 1024;
            snap.jemalloc_retained_kb = stats::retained::read().unwrap_or(0) as u64 / 1024;
            snap.jemalloc_metadata_kb = stats::metadata::read().unwrap_or(0) as u64 / 1024;
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

/// Outcome of one mallctl write, for the ladder line (never fatal).
pub type MallctlResult = std::result::Result<(), String>;

#[cfg(all(unix, not(target_env = "musl")))]
fn mallctl_err(name: &str, e: impl std::fmt::Display) -> String {
    format!("{name}: {e}")
}

/// `background_thread` on/off. Turning it off joins every `jemalloc_bg_thd`
/// (PLAN-phase4 §4.1) — call before spawning threads so none are created for
/// the arenas they touch. No-op `Ok` without jemalloc.
pub fn set_background_threads(on: bool) -> MallctlResult {
    #[cfg(all(unix, not(target_env = "musl")))]
    {
        tikv_jemalloc_ctl::background_thread::write(on)
            .map_err(|e| mallctl_err("background_thread", e))
    }
    #[cfg(not(all(unix, not(target_env = "musl"))))]
    {
        let _ = on;
        Ok(())
    }
}

/// Current `background_thread` setting (`None` without jemalloc).
pub fn background_threads_enabled() -> Option<bool> {
    #[cfg(all(unix, not(target_env = "musl")))]
    {
        tikv_jemalloc_ctl::background_thread::read().ok()
    }
    #[cfg(not(all(unix, not(target_env = "musl"))))]
    {
        None
    }
}

/// Decay times for **existing** arenas (`arena.<ALL>.*_decay_ms`) and the
/// default for future ones (`arenas.*_decay_ms`). `0` = purge a freed run
/// on the next decay tick; `-1` = never. §4.2.
pub fn set_decay_ms(dirty_ms: i64, muzzy_ms: i64) -> MallctlResult {
    #[cfg(all(unix, not(target_env = "musl")))]
    {
        use tikv_jemalloc_ctl::raw;
        // ssize_t on every supported target.
        let d = dirty_ms as libc::ssize_t;
        let m = muzzy_ms as libc::ssize_t;
        unsafe {
            raw::write(b"arenas.dirty_decay_ms\0", d)
                .map_err(|e| mallctl_err("arenas.dirty_decay_ms", e))?;
            raw::write(b"arenas.muzzy_decay_ms\0", m)
                .map_err(|e| mallctl_err("arenas.muzzy_decay_ms", e))?;
            // `arena.<i>.*_decay_ms` rejects MALLCTL_ARENAS_ALL (EFAULT);
            // walk the initialised arenas instead. Uninitialised ones take
            // the `arenas.*` default above when created.
            let n: u32 = raw::read(b"arenas.narenas\0")
                .map_err(|e| mallctl_err("arenas.narenas", e))?;
            for i in 0..n {
                let dk = format!("arena.{i}.dirty_decay_ms\0");
                let mk = format!("arena.{i}.muzzy_decay_ms\0");
                if raw::write(dk.as_bytes(), d).is_ok() {
                    let _ = raw::write(mk.as_bytes(), m);
                }
            }
        }
        Ok(())
    }
    #[cfg(not(all(unix, not(target_env = "musl"))))]
    {
        let _ = (dirty_ms, muzzy_ms);
        Ok(())
    }
}

/// `prctl(PR_GET_THP_DISABLE)` — is THP already off for this process
/// (inherited across `execve`, so a re-exec'd client sees `Some(true)`)?
pub fn thp_disabled() -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: plain prctl with integer arguments.
        let rc = unsafe { libc::prctl(libc::PR_GET_THP_DISABLE, 0u64, 0u64, 0u64, 0u64) };
        (rc >= 0).then_some(rc == 1)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// `prctl(PR_SET_THP_DISABLE)` — no transparent huge pages for this process
/// from now on. With `THP=always` every touched thread stack, the `.bss` and
/// each jemalloc chunk costs a 2 MiB page; the thin client wants 4 KiB
/// granularity. Inherited by threads created afterwards. Linux only.
pub fn disable_thp() -> MallctlResult {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: plain prctl with integer arguments.
        let rc = unsafe { libc::prctl(libc::PR_SET_THP_DISABLE, 1u64, 0u64, 0u64, 0u64) };
        if rc == 0 {
            Ok(())
        } else {
            Err(format!("prctl(PR_SET_THP_DISABLE): {}", std::io::Error::last_os_error()))
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(())
    }
}

/// `thread.tcache.enabled` for the **calling** thread (§4.3 fallback).
pub fn set_thread_tcache(on: bool) -> MallctlResult {
    #[cfg(all(unix, not(target_env = "musl")))]
    {
        unsafe { tikv_jemalloc_ctl::raw::write(b"thread.tcache.enabled\0", on) }
            .map_err(|e| mallctl_err("thread.tcache.enabled", e))
    }
    #[cfg(not(all(unix, not(target_env = "musl"))))]
    {
        let _ = on;
        Ok(())
    }
}

/// `SYNAPS_MEM_TRACE=1` turns on the per-turn memory trace and the broker
/// install log line. Read once; one atomic load per call afterwards.
pub fn mem_trace_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SYNAPS_MEM_TRACE").is_ok_and(|v| v == "1"))
}

/// One `agent_core::memstat` info line ("turn memory") — called at
/// `SessionEvent::Done` so `synaps.log` carries a greppable per-turn memory
/// trace. No-op (one atomic load) unless `SYNAPS_MEM_TRACE=1`.
pub fn log_turn_memory() {
    if !mem_trace_enabled() {
        return;
    }
    let s = self_snapshot();
    tracing::info!(
        target: "agent_core::memstat",
        rss_anon_kb = s.rss_anon_kb,
        jemalloc_allocated_kb = s.jemalloc_allocated_kb,
        jemalloc_resident_kb = s.jemalloc_resident_kb,
        threads = s.threads,
        broker_installs = crate::auth::global_broker_install_count(),
        "turn memory"
    );
}

/// Process-start anchor for [`ladder`]'s `t_ms`. Pinned by the first
/// [`ladder`] call (the `main` stage in the attach client), so call it first.
static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// The boot-ladder sink: `SYNAPS_MEM_TRACE_FILE` (default
/// `${XDG_RUNTIME_DIR:-/tmp}/synaps-memtrace-<pid>.log`), opened once, 0600.
/// `None` when the file could not be opened (the ladder then drops lines).
fn ladder_sink() -> Option<&'static std::sync::Mutex<std::fs::File>> {
    static SINK: std::sync::OnceLock<Option<std::sync::Mutex<std::fs::File>>> =
        std::sync::OnceLock::new();
    SINK.get_or_init(|| {
        let path = std::env::var_os("SYNAPS_MEM_TRACE_FILE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                let dir = std::env::var_os("XDG_RUNTIME_DIR")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
                dir.join(format!("synaps-memtrace-{}.log", std::process::id()))
            });
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(path).ok().map(std::sync::Mutex::new)
    })
    .as_ref()
}

/// Boot-ladder stage (PLAN-phase4 §7.1): appends one line to the trace file
/// — **not** `tracing` (the attach client has no subscriber) and **not**
/// stderr (it is the TUI). No-op (one atomic load) unless `SYNAPS_MEM_TRACE=1`.
///
/// Line format: `t_ms=<since first call> stage=<stage> rss_anon_kb=…
/// jemalloc_allocated_kb=… active_kb=… resident_kb=… retained_kb=…
/// metadata_kb=… threads=… <extra>`.
pub fn ladder(stage: &'static str, extra: &dyn std::fmt::Display) {
    if !mem_trace_enabled() {
        return;
    }
    let start = *START.get_or_init(std::time::Instant::now);
    let line = ladder_line(start.elapsed().as_millis(), stage, &self_snapshot(), extra);
    if let Some(sink) = ladder_sink() {
        if let Ok(mut f) = sink.lock() {
            use std::io::Write;
            let _ = f.write_all(line.as_bytes());
        }
    }
}

fn ladder_line(t_ms: u128, stage: &str, s: &SelfMem, extra: &dyn std::fmt::Display) -> String {
    let extra = extra.to_string();
    let sep = if extra.is_empty() { "" } else { " " };
    format!(
        "t_ms={t_ms} stage={stage} rss_anon_kb={} jemalloc_allocated_kb={} active_kb={} resident_kb={} retained_kb={} metadata_kb={} threads={} anon_huge_kb={}{sep}{extra}\n",
        s.rss_anon_kb,
        s.jemalloc_allocated_kb,
        s.jemalloc_active_kb,
        s.jemalloc_resident_kb,
        s.jemalloc_retained_kb,
        s.jemalloc_metadata_kb,
        s.threads,
        s.anon_huge_kb,
    )
}

/// Scrub credentials out of a space-joined argv before it leaves the
/// process. Redacted: the value of any `--flag value` / `--flag=value` whose
/// flag name mentions key/token/secret/password/bearer/auth/credential, any
/// `KEY=value` env-style arg with such a key, and any bare arg that looks
/// like a key (`sk-…`, `ghp_…`, `xox…`, `Bearer …`, or ≥ 32 chars of
/// base64/hex with no path separator).
pub fn redact_cmdline(cmd: &str) -> String {
    const SENSITIVE: &[&str] = &[
        "key",
        "token",
        "secret",
        "password",
        "passwd",
        "bearer",
        "auth",
        "credential",
    ];
    fn sensitive(name: &str) -> bool {
        let n = name.to_ascii_lowercase();
        SENSITIVE.iter().any(|s| n.contains(s))
    }
    fn looks_like_key(arg: &str) -> bool {
        let a = arg.trim_matches(|c| c == '"' || c == '\'');
        if a.starts_with("sk-")
            || a.starts_with("ghp_")
            || a.starts_with("github_pat_")
            || a.starts_with("xox")
            || a.starts_with("AKIA")
            || a.eq_ignore_ascii_case("bearer")
        {
            return true;
        }
        a.len() >= 32
            && !a.contains('/')
            && !a.contains('.')
            && a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '=')
    }
    let mut out = Vec::new();
    let mut redact_next = false;
    for arg in cmd.split(' ').filter(|s| !s.is_empty()) {
        if redact_next {
            out.push("***".to_string());
            redact_next = false;
            continue;
        }
        if let Some(flag) = arg.strip_prefix('-') {
            let flag = flag.trim_start_matches('-');
            match flag.split_once('=') {
                Some((name, _)) if sensitive(name) => {
                    out.push(format!("{}=***", &arg[..arg.len() - flag.len() + name.len()]));
                }
                Some(_) => out.push(arg.to_string()),
                None => {
                    if sensitive(flag) {
                        redact_next = true;
                    }
                    out.push(arg.to_string());
                }
            }
            continue;
        }
        if let Some((name, _)) = arg.split_once('=') {
            if sensitive(name) {
                out.push(format!("{name}=***"));
                continue;
            }
        }
        if looks_like_key(arg) {
            out.push("***".to_string());
            continue;
        }
        out.push(arg.to_string());
    }
    out.join(" ")
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
    if base.starts_with("synaps") {
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

    /// `AnonHugePages:` line of a `smaps_rollup` (0 when unreadable).
    pub(super) fn anon_huge_kb(path: &Path) -> u64 {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| t.lines().find(|l| l.starts_with("AnonHugePages:")).map(kb))
            .unwrap_or(0)
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
        let raw = read_cmdline(&dir.join("cmdline"));
        let role = classify(&raw);
        let cmd = redact_cmdline(&raw);
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
    fn ladder_line_format_and_noop_when_disabled() {
        let s = SelfMem {
            rss_anon_kb: 1,
            jemalloc_allocated_kb: 2,
            jemalloc_active_kb: 3,
            jemalloc_resident_kb: 4,
            jemalloc_retained_kb: 5,
            jemalloc_metadata_kb: 6,
            threads: 7,
            ..SelfMem::default()
        };
        assert_eq!(
            ladder_line(42, "http", &s, &"build_ms=3"),
            "t_ms=42 stage=http rss_anon_kb=1 jemalloc_allocated_kb=2 active_kb=3 resident_kb=4 retained_kb=5 metadata_kb=6 threads=7 anon_huge_kb=0 build_ms=3\n"
        );
        assert_eq!(
            ladder_line(0, "main", &s, &""),
            "t_ms=0 stage=main rss_anon_kb=1 jemalloc_allocated_kb=2 active_kb=3 resident_kb=4 retained_kb=5 metadata_kb=6 threads=7 anon_huge_kb=0\n"
        );
        // `mem_trace_enabled()` is cached per process and the test binary
        // does not set SYNAPS_MEM_TRACE: `ladder` must be a no-op and must
        // not create the default trace file.
        if mem_trace_enabled() {
            return; // the harness itself is tracing; nothing to assert
        }
        let dir = std::env::temp_dir().join(format!("synaps-ladder-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("trace.log");
        std::env::set_var("SYNAPS_MEM_TRACE_FILE", &file);
        ladder("main", &"");
        assert!(!file.exists(), "ladder wrote while disabled");
        std::env::remove_var("SYNAPS_MEM_TRACE_FILE");
        let _ = std::fs::remove_dir_all(&dir);
    }

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
    fn redact_cmdline_scrubs_keys_and_flag_values() {
        let raw = "npx -y some-mcp --api-key sk-abc123 --token=tok_zzz --port 8080 \
                   OPENAI_API_KEY=sk-live-1 /home/u/.synaps-cli/plugins/x/main.py";
        let red = redact_cmdline(raw);
        assert!(!red.contains("sk-abc123"), "{red}");
        assert!(!red.contains("tok_zzz"), "{red}");
        assert!(!red.contains("sk-live-1"), "{red}");
        assert!(red.contains("--api-key ***"), "{red}");
        assert!(red.contains("--token=***"), "{red}");
        assert!(red.contains("OPENAI_API_KEY=***"), "{red}");
        assert!(red.contains("--port 8080"), "{red}");
        assert!(red.contains("/home/u/.synaps-cli/plugins/x/main.py"), "{red}");
        // Bare long opaque tokens go too; ordinary args survive.
        let red = redact_cmdline("node bridge.cjs ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcd");
        assert_eq!(red, "node bridge.cjs ***");
        assert_eq!(redact_cmdline("bash -c ls"), "bash -c ls");
        // Classification still sees the raw argv; the row carries the scrubbed one.
        assert_eq!(classify(raw), ProcRole::ExtensionSidecar { name: "x".into() });
    }

    /// Fake `/proc/<pid>` with an MCP child carrying `--api-key sk-abc`:
    /// the serialized row must not contain the key.
    #[cfg_attr(not(target_os = "linux"), ignore)]
    #[test]
    fn proc_row_json_never_carries_api_key() {
        let root = std::env::temp_dir().join(format!("memstat-fakeproc-{}", std::process::id()));
        let dir = root.join("4242");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cmdline"),
            b"npx\0-y\0my-mcp-server\0--api-key\0sk-abc\0--verbose\0",
        )
        .unwrap();
        std::fs::write(
            dir.join("status"),
            "Name:\tnpx\nPPid:\t1\nVmRSS:\t100 kB\nRssAnon:\t50 kB\nThreads:\t2\n",
        )
        .unwrap();
        let row = linux::read_proc(&root, 4242).unwrap();
        let json = serde_json::to_string(&row).unwrap();
        assert!(!json.contains("sk-abc"), "{json}");
        assert!(json.contains("--api-key ***"), "{json}");
        assert!(json.contains("--verbose"), "{json}");
        assert_eq!(row.role, ProcRole::McpServer { name: "npx".into() });
        let _ = std::fs::remove_dir_all(&root);
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
