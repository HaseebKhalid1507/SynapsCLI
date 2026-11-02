//! `wait`: one tool call that blocks, with no model turns,
//! until its conditions hold, a new event arrives on the session's queue,
//! or its timeout fires.
//!
//! - Conditions, `any` or `all`, up to [`MAX_CONDITIONS`]:
//!   - `file`: the path exists; with `contains`, it holds that literal text.
//!     `contains` has append-log semantics: the file is scanned forward from
//!     where the last check stopped (at most [`SCAN_PER_TICK`] per check),
//!     and once seen the text stays seen; a shrink or a new inode (rotation,
//!     rewrite) rescans from the start. `tail_lines` adds the file's last
//!     lines to the result, at most [`MAX_TAIL_BYTES`] of UTF-8 text across
//!     all tails (a line that alone exceeds a tail's share is cut on a char
//!     boundary, keeping its end; `truncated` then says so). Both read
//!     regular files only (FIFOs and devices are refused) and are a read
//!     path, like `read`.
//!   - `gone`: no such path (ENOENT only; permission denied is an error).
//!   - `pid`: that process instance exited (gone, a zombie, or the pid now
//!     names another process). `start_ticks` (field 22 of /proc stat; on
//!     macOS the kernel's unique id) names the instance; when omitted it is
//!     captured at entry and echoed.
//!   - `subagent`: the handle is terminal (live status, or its archive).
//!   - `event`: a new arrival on this session's queue matching `type`,
//!     `source` and `min_severity` (headers only: no text or data match).
//! - Checked every second, on every arrival and on a pidfd exit; blocking
//!   file and /proc I/O runs on the blocking pool, raced against the
//!   deadline and the quiesce flag (plus a short grace): a check stuck in
//!   the kernel (a hung mount) ends the wait without its result
//!   (`check_unfinished`), and a tail not read in time is reported
//!   `unavailable`. A started syscall cannot be cancelled, so such a check
//!   runs on after the wait and holds one of its runtime's two check slots
//!   until it returns; with both held, new waits there are refused.
//! - A new event at or above `interrupt` (default medium) that matches no
//!   condition also ends the wait (outcome `event`). The result carries its
//!   header only: the event itself arrives through the normal steer path at
//!   the next round, and the result promises nothing about that delivery.
//!   Arrivals that never reach the model (display-only) are ignored.
//! - At most one wait in flight per runtime (keyed by its event queue;
//!   contexts without one share a key): a second concurrent call is
//!   refused (combine conditions with any/all). Sequential waits are
//!   allowed, each bounded by its timeout. There is no detach in v1.
//! - Esc drops the call: the admission slot, the pidfds and the watch go
//!   with it. A file or /proc check already in the kernel finishes on the
//!   blocking pool (see above).
//! - A daemon reload or a host checkpoint (the session's quiesce flag, set
//!   from outside the actor) ends the wait at once with outcome
//!   `interrupted` and its `reason`. Nothing will wake the session for that
//!   wait. When several hold on one pass, the order is error, met,
//!   interrupted, event, timeout.

use super::{resolve_path_in, Tool, ToolContext};
use crate::events::queue::{Arrival, Quiesce};
use crate::events::Severity;
use crate::proc_identity::{ProcessIdentity, StartKey};
use crate::{Result, RuntimeError};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Conditions one wait may hold.
pub const MAX_CONDITIONS: usize = 8;
/// `timeout_secs` when the caller gives none (capped by the config max).
pub const DEFAULT_TIMEOUT_SECS: u64 = 600;
/// How often the conditions are re-checked without any other wake.
const TICK: Duration = Duration::from_secs(1);
/// Bounds on `tail_lines`, and on the tail text one result carries: the
/// UTF-8 bytes of every returned line, all tails together (each tail gets
/// an equal share), counted before JSON escaping. Escaping can make the
/// JSON text longer (a control character takes up to six bytes there).
const MAX_TAIL_LINES: usize = 200;
const MAX_TAIL_BYTES: usize = 16 * 1024;
/// The longest `contains` literal, and the most bytes scanned per check.
const MAX_CONTAINS_BYTES: usize = 1024;
const SCAN_PER_TICK: u64 = 4 * 1024 * 1024;
/// The longest event filter string (arrival header fields are cut there).
const MAX_FILTER_BYTES: usize = 128;
/// Event headers one result reports.
const MAX_EVENTS: usize = 8;

const KINDS: [&str; 5] = ["file", "gone", "pid", "subagent", "event"];

/// The tracing target of the one line each wait logs ("wait ended"). It is
/// under `agent_engine`, so the daemon log's filter
/// (`agent_core::logging::with_crate_directives`, `agent_engine=debug`)
/// keeps it with `RUST_LOG` unset; a target outside Synaps's crates is
/// dropped below ERROR. A `RUST_LOG` that named the earlier `synaps::wait`
/// target must name this one.
pub const LOG_TARGET: &str = "agent_engine::wait";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Any,
    All,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct EventFilter {
    content_type: Option<String>,
    source: Option<String>,
    min_severity: Option<Severity>,
}

impl EventFilter {
    /// Header match only. Display-only arrivals are dropped before this
    /// (in [`EventWatch::absorb`]), so they neither match nor interrupt.
    fn matches(&self, arrival: &Arrival) -> bool {
        self.content_type
            .as_deref()
            .is_none_or(|t| t == arrival.content_type)
            && self.source.as_deref().is_none_or(|s| s == arrival.source)
            && self
                .min_severity
                .as_ref()
                .is_none_or(|min| severity_of(arrival) >= *min)
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Cond {
    File {
        raw: String,
        path: PathBuf,
        contains: Option<String>,
        tail_lines: Option<usize>,
    },
    Gone {
        raw: String,
        path: PathBuf,
    },
    Pid {
        pid: u32,
        start_ticks: Option<u64>,
    },
    Subagent {
        handle: String,
    },
    Event(EventFilter),
}

impl Cond {
    fn kind(&self) -> &'static str {
        match self {
            Cond::File { .. } => "file",
            Cond::Gone { .. } => "gone",
            Cond::Pid { .. } => "pid",
            Cond::Subagent { .. } => "subagent",
            Cond::Event(_) => "event",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Spec {
    mode: Mode,
    conds: Vec<Cond>,
    timeout: Duration,
    /// `None`: no arrival ends the wait unless it matches a condition.
    interrupt: Option<Severity>,
}

fn severity_of(arrival: &Arrival) -> Severity {
    // The event tag shows a missing severity as medium.
    arrival.severity.clone().unwrap_or(Severity::Medium)
}

fn parse_severity(value: &str) -> Option<Severity> {
    match value {
        "low" => Some(Severity::Low),
        "medium" => Some(Severity::Medium),
        "high" => Some(Severity::High),
        "critical" => Some(Severity::Critical),
        _ => None,
    }
}

fn path_field(obj: &Map<String, Value>, key: &str) -> std::result::Result<String, String> {
    match obj.get(key) {
        Some(Value::String(raw)) if !raw.trim().is_empty() => Ok(raw.clone()),
        _ => Err(format!("{key} must be a non-empty path")),
    }
}

fn parse_cond(value: &Value, cwd: Option<&Path>) -> std::result::Result<Cond, String> {
    let obj = value.as_object().ok_or("a condition is an object")?;
    if obj.contains_key("job") {
        return Err("job conditions are not available in this version".into());
    }
    let kinds: Vec<&str> = KINDS
        .iter()
        .copied()
        .filter(|kind| obj.contains_key(*kind))
        .collect();
    let [kind] = kinds[..] else {
        return Err(format!("give exactly one of {}", KINDS.join(", ")));
    };
    let extra: &[&str] = match kind {
        "file" => &["contains", "tail_lines"],
        "pid" => &["start_ticks"],
        _ => &[],
    };
    if let Some(other) = obj
        .keys()
        .find(|key| key.as_str() != kind && !extra.contains(&key.as_str()))
    {
        return Err(format!("'{other}' does not belong in a {kind} condition"));
    }
    match kind {
        "file" => {
            let raw = path_field(obj, "file")?;
            let contains = match obj.get("contains") {
                None => None,
                Some(Value::String(text))
                    if !text.is_empty() && text.len() <= MAX_CONTAINS_BYTES =>
                {
                    Some(text.clone())
                }
                Some(_) => {
                    return Err(format!(
                        "contains must be a non-empty string of at most {MAX_CONTAINS_BYTES} bytes"
                    ))
                }
            };
            let tail_lines = match obj.get("tail_lines") {
                None => None,
                Some(value) => Some(
                    value
                        .as_u64()
                        .filter(|n| (1..=MAX_TAIL_LINES as u64).contains(n))
                        .ok_or_else(|| format!("tail_lines must be 1 to {MAX_TAIL_LINES}"))?
                        as usize,
                ),
            };
            Ok(Cond::File {
                path: resolve_path_in(&raw, cwd),
                raw,
                contains,
                tail_lines,
            })
        }
        "gone" => {
            let raw = path_field(obj, "gone")?;
            Ok(Cond::Gone {
                path: resolve_path_in(&raw, cwd),
                raw,
            })
        }
        "pid" => {
            let pid = obj["pid"]
                .as_u64()
                .filter(|pid| (1..=u64::from(u32::MAX)).contains(pid))
                .ok_or("pid must be a positive integer")? as u32;
            let start_ticks = match obj.get("start_ticks") {
                None => None,
                Some(value) => Some(
                    value
                        .as_u64()
                        .ok_or("start_ticks must be a non-negative integer")?,
                ),
            };
            Ok(Cond::Pid { pid, start_ticks })
        }
        "subagent" => {
            let handle = obj["subagent"]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= MAX_FILTER_BYTES)
                .ok_or("subagent must be a handle id")?
                .to_string();
            Ok(Cond::Subagent { handle })
        }
        _ => {
            let filter = obj["event"]
                .as_object()
                .ok_or("event must be an object: {type?, source?, min_severity?}")?;
            if let Some(other) = filter
                .keys()
                .find(|key| !["type", "source", "min_severity"].contains(&key.as_str()))
            {
                return Err(format!(
                    "an event filter takes only type, source and min_severity, not '{other}'"
                ));
            }
            let text = |key: &str| -> std::result::Result<Option<String>, String> {
                match filter.get(key) {
                    None => Ok(None),
                    // Arrival headers render `"` as `'` (as the event tag does).
                    Some(Value::String(s)) if !s.is_empty() && s.len() <= MAX_FILTER_BYTES => {
                        Ok(Some(s.replace('"', "'")))
                    }
                    Some(_) => Err(format!(
                        "event {key} must be a non-empty string of at most {MAX_FILTER_BYTES} bytes"
                    )),
                }
            };
            let min_severity = match filter.get("min_severity") {
                None => None,
                Some(Value::String(s)) => {
                    Some(parse_severity(s).ok_or_else(|| format!("unknown min_severity '{s}'"))?)
                }
                Some(_) => return Err("min_severity must be a string".into()),
            };
            Ok(Cond::Event(EventFilter {
                content_type: text("type")?,
                source: text("source")?,
                min_severity,
            }))
        }
    }
}

fn parse(params: &Value, cwd: Option<&Path>, max_secs: u64) -> std::result::Result<Spec, String> {
    let obj = params.as_object().ok_or("wait takes an object")?;
    for key in obj.keys() {
        match key.as_str() {
            "any" | "all" | "timeout_secs" | "interrupt" => {}
            "detach" => {
                return Err(
                    "detach is not supported: a wait is inline only, bounded by timeout_secs"
                        .into(),
                )
            }
            other => return Err(format!("unknown field '{other}'")),
        }
    }
    let (mode, list) = match (obj.get("any"), obj.get("all")) {
        (Some(list), None) => (Mode::Any, list),
        (None, Some(list)) => (Mode::All, list),
        (Some(_), Some(_)) => return Err("give either any or all, not both".into()),
        (None, None) => return Err("give the conditions as any: [...] or all: [...]".into()),
    };
    let list = list
        .as_array()
        .ok_or("any/all must be an array of conditions")?;
    if list.is_empty() || list.len() > MAX_CONDITIONS {
        return Err(format!("give 1 to {MAX_CONDITIONS} conditions"));
    }
    let conds = list
        .iter()
        .enumerate()
        .map(|(i, cond)| parse_cond(cond, cwd).map_err(|e| format!("condition {i}: {e}")))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let max_secs = max_secs.max(1);
    let timeout_secs = match obj.get("timeout_secs") {
        None => DEFAULT_TIMEOUT_SECS.min(max_secs),
        Some(value) => value
            .as_u64()
            .filter(|secs| (1..=max_secs).contains(secs))
            .ok_or_else(|| format!("timeout_secs must be an integer from 1 to {max_secs}"))?,
    };
    let interrupt = match obj.get("interrupt") {
        None => Some(Severity::Medium),
        Some(Value::String(s)) if s == "none" => None,
        Some(Value::String(s)) => Some(parse_severity(s).ok_or_else(|| {
            format!("interrupt must be low, medium, high, critical or none, not '{s}'")
        })?),
        Some(_) => return Err("interrupt must be a string".into()),
    };
    Ok(Spec {
        mode,
        conds,
        timeout: Duration::from_secs(timeout_secs),
        interrupt,
    })
}

// ── Evaluation ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum State {
    Waiting(Value),
    Met(Value),
    Error(String),
}

/// Forward scan state of one `contains` condition.
#[derive(Debug, Clone, Default)]
struct FileScan {
    identity: Option<(u64, u64)>,
    offset: u64,
    carry: Vec<u8>,
    seen: bool,
}

/// Open a regular file for reading without ever blocking on a FIFO, and
/// check the opened file itself (no check-then-open race).
fn open_regular(path: &Path) -> io::Result<(std::fs::File, std::fs::Metadata)> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file; contains and tail_lines read regular files only",
        ));
    }
    Ok((file, meta))
}

fn identity(meta: &std::fs::Metadata) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        (0, 0)
    }
}

/// Scan forward for `needle`. Append-log semantics (module doc).
fn scan_for(path: &Path, needle: &str, scan: &mut FileScan) -> io::Result<bool> {
    let (file, meta) = open_regular(path)?;
    let id = identity(&meta);
    if scan.identity != Some(id) || meta.len() < scan.offset {
        *scan = FileScan {
            identity: Some(id),
            ..FileScan::default()
        };
    }
    if scan.seen {
        return Ok(true);
    }
    let mut file = file;
    file.seek(SeekFrom::Start(scan.offset))?;
    let want = (meta.len() - scan.offset).min(SCAN_PER_TICK);
    let mut buf = std::mem::take(&mut scan.carry);
    let carried = buf.len();
    file.take(want).read_to_end(&mut buf)?;
    scan.offset += (buf.len() - carried) as u64;
    let needle = needle.as_bytes();
    if memchr::memmem::find(&buf, needle).is_some() {
        scan.seen = true;
        return Ok(true);
    }
    let keep = needle.len().saturating_sub(1).min(buf.len());
    scan.carry = buf[buf.len() - keep..].to_vec();
    Ok(false)
}

fn check_file(path: &Path, cond: &Cond, scan: &mut FileScan) -> State {
    #[cfg(test)]
    hold::pause(path, hold::Site::Probe);
    let Cond::File {
        contains,
        tail_lines,
        ..
    } = cond
    else {
        unreachable!("check_file takes a file condition");
    };
    match std::fs::metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => State::Waiting(json!({"exists": false})),
        Err(e) => State::Error(format!("{}: {e}", path.display())),
        Ok(_) if contains.is_none() && tail_lines.is_none() => State::Met(json!({"exists": true})),
        Ok(meta) if !meta.is_file() => State::Error(format!(
            "{} is not a regular file; contains and tail_lines read regular files only",
            path.display()
        )),
        Ok(_) => match contains {
            None => State::Met(json!({"exists": true})),
            Some(needle) => match scan_for(path, needle, scan) {
                Ok(true) => State::Met(json!({"exists": true, "contains": true})),
                Ok(false) => State::Waiting(json!({"exists": true, "contains": false})),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    State::Waiting(json!({"exists": false}))
                }
                Err(e) => State::Error(format!("{}: {e}", path.display())),
            },
        },
    }
}

fn check_gone(path: &Path) -> State {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => State::Met(json!({"gone": true})),
        Ok(_) => State::Waiting(json!({"gone": false})),
        Err(e) => State::Error(format!("{}: {e}", path.display())),
    }
}

/// The instance a pid condition names: the given `start_ticks` (a
/// [`StartKey::number`]), else the one captured at entry.
fn wanted_start(given: Option<u64>, captured: Option<StartKey>) -> Option<StartKey> {
    given.map(StartKey::from_number).or(captured)
}

fn check_pid(pid: u32, given: Option<u64>, captured: &mut Option<StartKey>) -> State {
    let mut want = wanted_start(given, *captured);
    if want.is_none() {
        if let Ok(now) = ProcessIdentity::identify(pid) {
            want = Some(now.start);
            *captured = want;
        }
    }
    // Echoed as the number it was given as (on Linux the start ticks).
    let start_ticks = want.map(StartKey::number);
    match ProcessIdentity::exit_of(pid, want) {
        Ok(Some(exit)) => {
            State::Met(json!({"pid": pid, "start_ticks": start_ticks, "exit": exit.as_str()}))
        }
        Ok(None) => State::Waiting(json!({"pid": pid, "start_ticks": start_ticks})),
        Err(e) if e.kind() == io::ErrorKind::Unsupported => {
            State::Error("pid conditions need Linux or macOS".into())
        }
        Err(e) => State::Error(format!("pid {pid}: {e}")),
    }
}

/// Filesystem and /proc state for one evaluation, on the blocking pool.
struct Probe {
    conds: Arc<Vec<Cond>>,
    scans: Vec<FileScan>,
    /// Per condition, the start a pid condition captured at entry.
    ticks: Vec<Option<StartKey>>,
}

impl Probe {
    /// One pass: `Some(state)` for file, gone and pid conditions.
    fn run(&mut self) -> Vec<Option<State>> {
        let conds = Arc::clone(&self.conds);
        conds
            .iter()
            .enumerate()
            .map(|(i, cond)| match cond {
                Cond::File { path, .. } => Some(check_file(path, cond, &mut self.scans[i])),
                Cond::Gone { path, .. } => Some(check_gone(path)),
                Cond::Pid { pid, start_ticks } => {
                    Some(check_pid(*pid, *start_ticks, &mut self.ticks[i]))
                }
                _ => None,
            })
            .collect()
    }
}

fn check_subagent(
    registry: Option<&Arc<std::sync::Mutex<crate::runtime::subagent::SubagentRegistry>>>,
    handle: &str,
) -> State {
    let Some(registry) = registry else {
        return State::Error("subagent conditions need a session with a subagent registry".into());
    };
    let reg = registry.lock().unwrap_or_else(|p| p.into_inner());
    // A worker the daemon resumed after a reload: its id waits on the run
    // that continues it (never the old run's stale end).
    match reg.resumed_as(handle) {
        Some(None) => return State::Waiting(json!({"status": "resuming", "resumed_by": "daemon"})),
        Some(Some(new)) => {
            return match subagent_state(&reg, &new) {
                State::Met(mut v) | State::Waiting(mut v) if v.is_object() => {
                    v["resumed_as"] = json!(new);
                    if v["status"] == "running" {
                        State::Waiting(v)
                    } else {
                        State::Met(v)
                    }
                }
                other => other,
            }
        }
        None => {}
    }
    subagent_state(&reg, handle)
}

/// [`check_subagent`] for one id, aliases aside.
fn subagent_state(reg: &crate::runtime::subagent::SubagentRegistry, handle: &str) -> State {
    if let Some(live) = reg.get(handle) {
        let status = live.status();
        return match status {
            crate::runtime::subagent::SubagentStatus::Running => {
                State::Waiting(json!({"status": "running"}))
            }
            other => State::Met(json!({"status": other.as_str()})),
        };
    }
    if reg.archive_path(handle).is_some() {
        return State::Met(json!({
            "status": "archived",
            "stopped_by_reload": reg.was_stopped_by_reload(handle),
        }));
    }
    State::Error(format!("no subagent with handle_id '{handle}'"))
}

fn header(arrival: &Arrival) -> Value {
    json!({
        "id": arrival.id,
        "type": arrival.content_type,
        "source": arrival.source,
        "severity": severity_of(arrival).as_str(),
    })
}

/// What the arrivals since the last look did to the wait.
#[derive(Debug, Default)]
struct EventWatch {
    cursor: u64,
    /// Per condition: `Some(header)` once an arrival matched it (latched).
    matched: Vec<Option<Value>>,
    /// Arrivals at or above `interrupt` that matched no condition.
    interrupts: Vec<Value>,
    overflow: bool,
}

impl EventWatch {
    fn absorb(&mut self, spec: &Spec, since: crate::events::queue::ArrivalsSince) {
        self.cursor = since.last_seq;
        self.overflow |= since.overflow;
        for arrival in since.arrivals.iter().filter(|a| a.reaches_model) {
            let mut sought = false;
            for (i, cond) in spec.conds.iter().enumerate() {
                if let Cond::Event(filter) = cond {
                    if filter.matches(arrival) {
                        sought = true;
                        if self.matched[i].is_none() {
                            self.matched[i] = Some(header(arrival));
                        }
                    }
                }
            }
            if !sought
                && spec
                    .interrupt
                    .as_ref()
                    .is_some_and(|min| severity_of(arrival) >= *min)
                && self.interrupts.len() < MAX_EVENTS
            {
                self.interrupts.push(header(arrival));
            }
        }
    }
}

/// The file's last `lines` lines, at most `budget` bytes of returned UTF-8
/// in all (invalid bytes become U+FFFD, three bytes each, so the bound is
/// applied to the text returned, not to the bytes read). `true` when
/// anything was left out or cut.
fn read_tail(path: &Path, lines: usize, budget: usize) -> io::Result<(Vec<String>, bool)> {
    #[cfg(test)]
    hold::pause(path, hold::Site::Tail);
    let (file, meta) = open_regular(path)?;
    let len = meta.len();
    let budget = budget.max(1);
    let read = budget as u64;
    let start = len.saturating_sub(read);
    let mut file = file;
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.take(read).read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let mut all: Vec<&str> = text.lines().collect();
    let mut truncated = false;
    if start > 0 && !all.is_empty() {
        // Starts mid-line: the first piece is partial.
        all.remove(0);
        truncated = true;
    }
    if all.len() > lines {
        all.drain(..all.len() - lines);
        truncated = true;
    }
    let (kept, cut) = fit_newest(&all, budget);
    Ok((kept, truncated || cut))
}

/// The newest whole lines whose UTF-8 bytes fit `budget`. When not even
/// the newest fits, its last bytes up to `budget`, cut on a char boundary.
/// `true` when anything was left out or cut.
fn fit_newest(lines: &[&str], budget: usize) -> (Vec<String>, bool) {
    let mut used = 0;
    let mut kept: Vec<String> = Vec::new();
    for line in lines.iter().rev() {
        if used + line.len() > budget {
            break;
        }
        used += line.len();
        kept.push((*line).to_string());
    }
    let cut = kept.len() < lines.len();
    if kept.is_empty() {
        if let Some(newest) = lines.last() {
            let mut from = newest.len() - budget.min(newest.len());
            while !newest.is_char_boundary(from) {
                from += 1;
            }
            kept.push(newest[from..].to_string());
        }
    }
    kept.reverse();
    (kept, cut)
}

// ── Admission ────────────────────────────────────────────────────────────────

/// Which runtime a wait belongs to: its event queue (one per session or
/// worker runtime). Contexts without a queue (an extension provider's tool
/// loop, a plugin command) share one key, so admission and the blocking
/// bound below hold for them too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RuntimeKey {
    Queue(usize),
    Unqueued,
}

impl RuntimeKey {
    fn of(ctx: &ToolContext) -> Self {
        ctx.capabilities
            .event_queue
            .as_ref()
            .map_or(Self::Unqueued, |q| Self::Queue(Arc::as_ptr(q) as usize))
    }
}

/// One wait in flight per runtime. Held for the call; dropped with it.
struct Admitted<'a> {
    running: &'a Mutex<HashSet<RuntimeKey>>,
    key: RuntimeKey,
}

impl Drop for Admitted<'_> {
    fn drop(&mut self) {
        self.running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.key);
    }
}

pub const SECOND_WAIT_REFUSED: &str = "wait refused: another wait is already running in this \
    session. Combine the conditions in one wait (any/all, up to 8) instead of parallel waits.";

pub const CHECKS_STILL_BLOCKED: &str = "wait refused: file or /proc checks that earlier waits \
    in this session started have not returned (a hung mount or device?). They hold this \
    session's check slots until the kernel answers; try again later.";

#[derive(Default)]
pub struct WaitTool {
    running: Mutex<HashSet<RuntimeKey>>,
    blocking: Arc<Blocking>,
}

impl WaitTool {
    fn admit(&self, key: RuntimeKey) -> std::result::Result<Admitted<'_>, String> {
        let mut running = self.running.lock().unwrap_or_else(|p| p.into_inner());
        if !running.insert(key) {
            return Err(SECOND_WAIT_REFUSED.to_string());
        }
        Ok(Admitted {
            running: &self.running,
            key,
        })
    }
}

// ── Blocking checks ──────────────────────────────────────────────────────────

/// Blocking-pool checks (file and /proc reads, the config read) of one
/// runtime that have not returned yet, whether or not the wait that started
/// them still runs. A started syscall cannot be cancelled: on a hung mount
/// it outlives the wait that gave up on it, holding one slot until the
/// kernel answers. With every slot held, new waits in that runtime are
/// refused ([`CHECKS_STILL_BLOCKED`]). Two: the running wait's check, plus
/// one an earlier wait left behind.
const MAX_BLOCKING_PER_RUNTIME: usize = 2;
/// How long a check may run past the wait's deadline, or once the quiesce
/// flag is seen set.
const CHECK_GRACE: Duration = Duration::from_millis(250);
/// How long reading the tails may take once the outcome is known.
const TAIL_BOUND: Duration = Duration::from_secs(2);
/// How long reading the configured wait cap may take.
const CONFIG_BOUND: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Blocking {
    outstanding: Mutex<HashMap<RuntimeKey, usize>>,
}

impl Blocking {
    fn take(self: &Arc<Self>, key: RuntimeKey) -> Option<BlockingSlot> {
        let mut outstanding = self.outstanding.lock().unwrap_or_else(|p| p.into_inner());
        let n = outstanding.entry(key).or_insert(0);
        if *n >= MAX_BLOCKING_PER_RUNTIME {
            return None;
        }
        *n += 1;
        Some(BlockingSlot {
            blocking: Arc::clone(self),
            key,
        })
    }

    #[cfg(test)]
    fn outstanding(&self, key: RuntimeKey) -> usize {
        let outstanding = self.outstanding.lock().unwrap_or_else(|p| p.into_inner());
        outstanding.get(&key).copied().unwrap_or(0)
    }
}

/// A started check's slot: moved into the blocking job, so it is freed when
/// the job returns, not when its wait ends.
struct BlockingSlot {
    blocking: Arc<Blocking>,
    key: RuntimeKey,
}

impl Drop for BlockingSlot {
    fn drop(&mut self) {
        let mut outstanding = self
            .blocking
            .outstanding
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(n) = outstanding.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                outstanding.remove(&self.key);
            }
        }
    }
}

/// Why a check gave no result.
#[derive(Debug)]
enum Stopped {
    /// Its time ran out (the deadline, the tail bound, or the quiesce
    /// grace); the job runs on and holds its slot until it returns.
    GaveUp,
    /// Every slot of the runtime is held by checks that have not returned.
    NoSlot,
    /// The job panicked.
    Failed(String),
}

/// The session's quiesce flag as one wait sees it.
struct QuiesceWatch {
    rx: Option<tokio::sync::watch::Receiver<Option<Quiesce>>>,
    open: bool,
}

impl QuiesceWatch {
    fn new(rx: Option<tokio::sync::watch::Receiver<Option<Quiesce>>>) -> Self {
        let open = rx.is_some();
        Self { rx, open }
    }

    /// The flag now, without marking it seen.
    fn current(&self) -> Option<Quiesce> {
        self.rx.as_ref().and_then(|rx| *rx.borrow())
    }

    /// The flag now, marked seen: a later change wakes [`Self::changed`].
    fn read(&mut self) -> Option<Quiesce> {
        self.rx.as_mut().and_then(|rx| *rx.borrow_and_update())
    }

    /// The next change; never, once the sender is gone.
    async fn changed(&mut self) {
        match self.rx.as_mut() {
            Some(rx) if self.open => {
                if rx.changed().await.is_err() {
                    self.open = false;
                }
            }
            _ => std::future::pending().await,
        }
    }
}

/// A wait's handle on its runtime's check slots.
struct Checks {
    blocking: Arc<Blocking>,
    key: RuntimeKey,
}

impl Checks {
    /// The latest a check started now may run: [`CHECK_GRACE`] past the
    /// wait's deadline (or past now, once the deadline is gone), so a check
    /// started at the deadline still gets its grace.
    fn until(deadline: tokio::time::Instant) -> tokio::time::Instant {
        deadline.max(tokio::time::Instant::now()) + CHECK_GRACE
    }

    /// Run `job` on the blocking pool under one of the runtime's slots,
    /// until `until`, or until [`CHECK_GRACE`] after the quiesce flag is
    /// seen set, whichever comes first. A job given up on runs on and holds
    /// its slot until it returns.
    async fn run<T: Send + 'static>(
        &self,
        until: tokio::time::Instant,
        quiesce: &mut QuiesceWatch,
        job: impl FnOnce() -> T + Send + 'static,
    ) -> std::result::Result<T, Stopped> {
        let slot = self.blocking.take(self.key).ok_or(Stopped::NoSlot)?;
        let mut task = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            job()
        });
        let mut cut = until;
        let mut graced = false;
        loop {
            if !graced && quiesce.current().is_some() {
                graced = true;
                cut = cut.min(tokio::time::Instant::now() + CHECK_GRACE);
            }
            tokio::select! {
                biased;
                done = &mut task => return done.map_err(|e| Stopped::Failed(e.to_string())),
                _ = tokio::time::sleep_until(cut) => return Err(Stopped::GaveUp),
                _ = quiesce.changed() => {}
            }
        }
    }
}

fn max_inline_secs() -> u64 {
    crate::config::load_config().wait.max_inline_secs
}

#[async_trait::async_trait]
impl Tool for WaitTool {
    fn effect(&self) -> crate::tools::catalog::ToolEffect {
        crate::tools::catalog::ToolEffect::ReadOnly
    }

    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }

    fn name(&self) -> &str {
        "wait"
    }

    fn description(&self) -> &str {
        "Block in ONE call, with no model turns, until conditions hold, a new event arrives, or the timeout fires. Use it instead of sleep/poll loops or a polling subagent. Conditions (any or all, up to 8): {file: path, contains?: text, tail_lines?: n} (exists, or holds the text; tail_lines returns the last lines); {gone: path}; {pid: n, start_ticks?: n} (that process exited); {subagent: handle_id} (finished); {event: {type?, source?, min_severity?}} (a matching new event on this session's queue). A new event at or above `interrupt` (default medium) also ends the wait; it is shown at the next round. One wait at a time per session: combine conditions instead of parallel waits. Returns JSON with outcome met|timeout|event|interrupted|error; a daemon reload or host checkpoint ends it as interrupted, and nothing then wakes the session for it."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "any": {
                    "type": "array",
                    "items": {"type": "object"},
                    "maxItems": MAX_CONDITIONS,
                    "description": "Conditions; the wait ends when one holds."
                },
                "all": {
                    "type": "array",
                    "items": {"type": "object"},
                    "maxItems": MAX_CONDITIONS,
                    "description": "Conditions; the wait ends when all hold."
                },
                "timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Default 600; at most wait.max_inline_secs (3600 by default)."
                },
                "interrupt": {
                    "type": "string",
                    "enum": ["low", "medium", "high", "critical", "none"],
                    "description": "Lowest severity of a new event that ends the wait early (default medium)."
                }
            }
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let key = RuntimeKey::of(&ctx);
        // First: a refused second wait takes no check slot from the first.
        let _admitted = self.admit(key).map_err(RuntimeError::Tool)?;
        let checks = Checks {
            blocking: Arc::clone(&self.blocking),
            key,
        };
        let cap = checks
            .run(
                tokio::time::Instant::now() + CONFIG_BOUND,
                &mut QuiesceWatch::new(None),
                max_inline_secs,
            )
            .await;
        let max_secs = match cap {
            Ok(secs) => secs,
            Err(Stopped::NoSlot) => return Err(RuntimeError::Tool(CHECKS_STILL_BLOCKED.into())),
            Err(Stopped::GaveUp) => {
                return Err(RuntimeError::Tool(format!(
                    "wait: the configuration could not be read within {} s",
                    CONFIG_BOUND.as_secs()
                )))
            }
            Err(Stopped::Failed(_)) => agent_core::config::DEFAULT_WAIT_MAX_INLINE_SECS,
        };
        let spec = parse(&params, ctx.capabilities.cwd.as_deref(), max_secs)
            .map_err(|e| RuntimeError::Tool(format!("wait: {e}")))?;
        let delegated = ctx.capabilities.delegation_parent.is_some();
        // A worker's events go to its parent; its own queue carries none.
        let queue = ctx.capabilities.event_queue.clone().filter(|_| !delegated);
        if spec.conds.iter().any(|c| matches!(c, Cond::Event(_))) && queue.is_none() {
            return Err(RuntimeError::Tool(
                "wait: event conditions need a foreground session (a subagent's events go to its parent)"
                    .into(),
            ));
        }
        // The quiesce flag comes from the unfiltered queue: a worker's own
        // queue has no handle, so nothing ever sets it there.
        let quiesce = ctx
            .capabilities
            .event_queue
            .as_ref()
            .map(|q| q.watch_quiesce().0);
        Ok(run(
            spec,
            queue,
            QuiesceWatch::new(quiesce),
            checks,
            ctx.capabilities.subagent_registry.clone(),
        )
        .await)
    }
}

async fn run(
    spec: Spec,
    queue: Option<Arc<crate::events::EventQueue>>,
    mut quiesce: QuiesceWatch,
    checks: Checks,
    registry: Option<Arc<std::sync::Mutex<crate::runtime::subagent::SubagentRegistry>>>,
) -> String {
    let started = tokio::time::Instant::now();
    let deadline = started + spec.timeout;
    let conds = Arc::new(spec.conds.clone());
    let n = conds.len();
    let mut probe = Some(Probe {
        conds: Arc::clone(&conds),
        scans: vec![FileScan::default(); n],
        ticks: vec![None; n],
    });
    // Subscribe, then read the position: no arrival after it is missed.
    let (mut arrivals, start_seq) = match &queue {
        Some(queue) => {
            let (rx, seq) = queue.watch_arrivals();
            (Some(rx), seq)
        }
        None => (None, 0),
    };
    let evictions_before = queue.as_ref().map_or(0, |q| q.evictions());
    let mut events = EventWatch {
        cursor: start_seq,
        matched: vec![None; n],
        ..EventWatch::default()
    };
    let mut exits = tokio::task::JoinSet::new();
    let mut pidfds_opened = false;
    let mut reason = None;
    // The last finished pass: what a wait that gives up on a check reports.
    let mut last: Vec<State> = vec![State::Waiting(Value::Null); n];
    let mut unfinished = false;
    let (outcome, states) = loop {
        // Filesystem and /proc, off the executor, raced against the
        // deadline and the quiesce flag: a check stuck in the kernel ends
        // the wait without its result (`check_unfinished`).
        let mut taken = probe.take().expect("the probe is back after each pass");
        let pass = checks
            .run(Checks::until(deadline), &mut quiesce, move || {
                let states = taken.run();
                (taken, states)
            })
            .await;
        let (taken, blocking) = match pass {
            Ok(done) => done,
            Err(Stopped::GaveUp) => {
                unfinished = true;
                match quiesce.read() {
                    Some(why) => {
                        reason = Some(why);
                        break ("interrupted", last);
                    }
                    None => break ("timeout", last),
                }
            }
            Err(Stopped::NoSlot) => {
                break ("error", vec![State::Error(CHECKS_STILL_BLOCKED.into())])
            }
            Err(Stopped::Failed(e)) => {
                break (
                    "error",
                    vec![State::Error(format!("wait: check failed: {e}"))],
                )
            }
        };
        let ticks = taken.ticks.clone();
        probe = Some(taken);
        if let Some(queue) = &queue {
            // Mark the notification seen BEFORE reading: an arrival after the
            // read marks the receiver changed again, so none is lost, and one
            // already read here causes no extra pass.
            if let Some(rx) = arrivals.as_mut() {
                rx.borrow_and_update();
            }
            events.absorb(&spec, queue.arrivals_since(events.cursor));
        }
        // Read and mark seen: a change after this read wakes the select.
        let quiesced = quiesce.read();
        let states: Vec<State> = conds
            .iter()
            .enumerate()
            .map(|(i, cond)| match (cond, &blocking[i]) {
                (_, Some(state)) => state.clone(),
                (Cond::Subagent { handle }, None) => check_subagent(registry.as_ref(), handle),
                (Cond::Event(_), None) => match &events.matched[i] {
                    Some(header) => State::Met(header.clone()),
                    None => State::Waiting(Value::Null),
                },
                _ => State::Error("unchecked condition".into()),
            })
            .collect();
        let met = |s: &State| matches!(s, State::Met(_));
        if states.iter().any(|s| matches!(s, State::Error(_))) {
            break ("error", states);
        }
        let done = match spec.mode {
            Mode::Any => states.iter().any(met),
            Mode::All => states.iter().all(met),
        };
        if done {
            break ("met", states);
        }
        if let Some(why) = quiesced {
            reason = Some(why);
            break ("interrupted", states);
        }
        if !events.interrupts.is_empty() || events.overflow {
            break ("event", states);
        }
        if tokio::time::Instant::now() >= deadline {
            break ("timeout", states);
        }
        last.clone_from(&states);
        if !pidfds_opened {
            pidfds_opened = true;
            let gave_up = open_exit_watches(
                &conds,
                &ticks,
                &states,
                &mut exits,
                &checks,
                deadline,
                &mut quiesce,
            )
            .await;
            if gave_up {
                // The deadline or the flag came first: check again at once.
                continue;
            }
        }
        let wake = (tokio::time::Instant::now() + TICK).min(deadline);
        let mut closed = false;
        tokio::select! {
            _ = tokio::time::sleep_until(wake) => {}
            changed = async {
                match arrivals.as_mut() {
                    Some(rx) => rx.changed().await,
                    None => std::future::pending().await,
                }
            } => closed = changed.is_err(),
            _ = quiesce.changed() => {}
            _ = exits.join_next(), if !exits.is_empty() => {}
        }
        if closed {
            arrivals = None;
        }
    };
    drop(exits);
    let elapsed = started.elapsed();
    let tails = if outcome == "error" {
        Vec::new()
    } else {
        collect_tails(&conds, &checks, &mut quiesce).await
    };
    let evicted = queue
        .as_ref()
        .map_or(0, |q| q.evictions().saturating_sub(evictions_before));
    let kinds: Vec<&str> = conds.iter().map(Cond::kind).collect();
    let mode = if spec.mode == Mode::Any { "any" } else { "all" };
    tracing::info!(
        target: LOG_TARGET,
        outcome,
        elapsed_ms = elapsed.as_millis() as u64,
        conditions = n,
        kinds = %kinds.join(","),
        mode,
        reason = reason.map_or("none", Quiesce::as_str),
        overflow = events.overflow,
        evicted,
        check_unfinished = unfinished,
        "wait ended"
    );
    render(
        &spec, outcome, reason, elapsed, &states, &events, tails, unfinished,
    )
}

/// On Linux, a pidfd per waiting pid condition: readable once that
/// instance exits, so the wait wakes at once instead of at the next tick.
/// Any failure leaves the condition to the tick. `true` when a pidfd open
/// was given up on (the deadline or the quiesce flag came first).
#[allow(unused_variables)]
async fn open_exit_watches(
    conds: &[Cond],
    ticks: &[Option<StartKey>],
    states: &[State],
    exits: &mut tokio::task::JoinSet<()>,
    checks: &Checks,
    deadline: tokio::time::Instant,
    quiesce: &mut QuiesceWatch,
) -> bool {
    #[cfg(target_os = "linux")]
    for (i, cond) in conds.iter().enumerate() {
        let Cond::Pid { pid, start_ticks } = cond else {
            continue;
        };
        if !matches!(states[i], State::Waiting(_)) {
            continue;
        }
        let (pid, start) = (*pid, wanted_start(*start_ticks, ticks[i]));
        let opened = checks
            .run(Checks::until(deadline), quiesce, move || {
                ProcessIdentity::pidfd(pid, start)
            })
            .await;
        match opened {
            Ok(Ok(Ok(fd))) => {
                exits.spawn(async move {
                    match tokio::io::unix::AsyncFd::with_interest(
                        fd,
                        tokio::io::Interest::READABLE,
                    ) {
                        Ok(fd) => {
                            let _ = fd.readable().await;
                        }
                        Err(_) => std::future::pending::<()>().await,
                    }
                });
            }
            Err(Stopped::GaveUp) => return true,
            _ => {}
        }
    }
    false
}

/// The tails, read within [`TAIL_BOUND`] (or [`CHECK_GRACE`] once the
/// quiesce flag is set). A tail not read by then is reported
/// `unavailable` with the reason; its read runs on and holds its slot.
async fn collect_tails(
    conds: &Arc<Vec<Cond>>,
    checks: &Checks,
    quiesce: &mut QuiesceWatch,
) -> Vec<Value> {
    let wanted: Vec<(usize, String)> = conds
        .iter()
        .enumerate()
        .filter_map(|(i, c)| match c {
            Cond::File {
                raw,
                tail_lines: Some(_),
                ..
            } => Some((i, raw.clone())),
            _ => None,
        })
        .collect();
    if wanted.is_empty() {
        return Vec::new();
    }
    let budget = MAX_TAIL_BYTES / wanted.len();
    let read = {
        let conds = Arc::clone(conds);
        move || -> Vec<Value> {
            conds
                .iter()
                .enumerate()
                .filter_map(|(i, cond)| {
                    let Cond::File {
                        raw,
                        path,
                        tail_lines: Some(lines),
                        ..
                    } = cond
                    else {
                        return None;
                    };
                    Some(match read_tail(path, *lines, budget) {
                        Ok((lines, truncated)) => json!({
                            "condition": i, "path": raw, "lines": lines, "truncated": truncated,
                        }),
                        Err(e) => json!({"condition": i, "path": raw, "error": e.to_string()}),
                    })
                })
                .collect()
        }
    };
    let why = match checks
        .run(tokio::time::Instant::now() + TAIL_BOUND, quiesce, read)
        .await
    {
        Ok(tails) => return tails,
        Err(Stopped::GaveUp) if quiesce.current().is_some() => "interrupted",
        Err(Stopped::GaveUp) => "timeout",
        Err(Stopped::NoSlot) => "checks_blocked",
        Err(Stopped::Failed(_)) => "failed",
    };
    wanted
        .into_iter()
        .map(|(i, raw)| json!({"condition": i, "path": raw, "unavailable": why}))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn render(
    spec: &Spec,
    outcome: &str,
    reason: Option<Quiesce>,
    elapsed: Duration,
    states: &[State],
    events: &EventWatch,
    tails: Vec<Value>,
    unfinished: bool,
) -> String {
    let secs = elapsed.as_secs_f64();
    let conditions: Vec<Value> = spec
        .conds
        .iter()
        .zip(
            states
                .iter()
                .chain(std::iter::repeat(&State::Waiting(Value::Null))),
        )
        .map(|(cond, state)| {
            let (state, detail) = match state {
                State::Waiting(detail) => ("waiting", detail.clone()),
                State::Met(detail) => ("met", detail.clone()),
                State::Error(error) => ("error", json!(error)),
            };
            json!({"kind": cond.kind(), "state": state, "detail": detail})
        })
        .collect();
    let met: Vec<usize> = states
        .iter()
        .enumerate()
        .filter(|(_, s)| matches!(s, State::Met(_)))
        .map(|(i, _)| i)
        .collect();
    let mut result = json!({
        "outcome": outcome,
        "mode": if spec.mode == Mode::Any { "any" } else { "all" },
        "elapsed_ms": elapsed.as_millis() as u64,
        "met": met,
        "conditions": conditions,
    });
    let mut shown: Vec<Value> = events.matched.iter().flatten().cloned().collect();
    shown.extend(events.interrupts.iter().cloned());
    shown.truncate(MAX_EVENTS);
    if !shown.is_empty() {
        result["events"] = json!(shown);
    }
    if let Some(reason) = reason {
        result["reason"] = json!(reason.as_str());
    }
    if events.overflow {
        result["overflow"] = json!(true);
    }
    if !tails.is_empty() {
        result["tail"] = json!(tails);
    }
    if unfinished {
        result["check_unfinished"] = json!(true);
    }
    let line = match outcome {
        "met" => format!("wait: met after {secs:.1} s."),
        "timeout" => {
            format!("wait: timed out after {secs:.1} s; nothing changed enough to end it.")
        }
        "interrupted" => {
            let why = match reason {
                Some(Quiesce::Reload) => "the daemon is reloading",
                _ => "the host is checkpointing this session",
            };
            format!(
                "wait: interrupted after {secs:.1} s: {why}. Nothing will wake this session \
                 for this wait: check its conditions again afterwards, or wait again."
            )
        }
        "event" if events.overflow => format!(
            "wait: ended after {secs:.1} s; more events arrived than the wait could follow \
             (arrival information unavailable). New events are shown at the next round."
        ),
        "event" => format!(
            "wait: ended after {secs:.1} s by a new event (header below). The event itself is \
             shown at the next round."
        ),
        _ => {
            let first = states
                .iter()
                .enumerate()
                .find_map(|(i, s)| match s {
                    State::Error(e) => Some(format!("condition {i}: {e}")),
                    _ => None,
                })
                .unwrap_or_default();
            format!("wait: error after {secs:.1} s: {first}")
        }
    };
    let line = if unfinished {
        format!(
            "{line} A file or /proc check had not returned (a hung mount or device?); the \
             conditions show the last check that did."
        )
    } else {
        line
    };
    format!("{line}\n{result}")
}

/// Test seam: a check of a held path blocks, as a read on a hung mount
/// would, until the test releases it. Tests only, on their own scratch
/// paths; nothing real is ever stalled.
#[cfg(test)]
mod hold {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex, OnceLock};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Site {
        Probe,
        Tail,
    }

    #[derive(Default)]
    pub(super) struct Hold {
        released: Mutex<bool>,
        cv: Condvar,
        /// Checks that reached the hold so far.
        pub(super) entered: AtomicUsize,
    }

    type Holds = Mutex<Vec<(PathBuf, Site, Arc<Hold>)>>;

    fn holds() -> &'static Holds {
        static HOLDS: OnceLock<Holds> = OnceLock::new();
        HOLDS.get_or_init(Holds::default)
    }

    /// Releases the hold when dropped, a failed assertion included, so no
    /// blocking thread outlives the test.
    pub(super) struct Guard {
        path: PathBuf,
        site: Site,
        pub(super) hold: Arc<Hold>,
    }

    pub(super) fn hold(path: &Path, site: Site) -> Guard {
        let hold = Arc::new(Hold::default());
        holds()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((path.to_path_buf(), site, Arc::clone(&hold)));
        Guard {
            path: path.to_path_buf(),
            site,
            hold,
        }
    }

    impl Guard {
        pub(super) fn entered(&self) -> usize {
            self.hold.entered.load(Ordering::SeqCst)
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            holds()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retain(|(p, s, _)| !(p == &self.path && *s == self.site));
            *self.hold.released.lock().unwrap_or_else(|p| p.into_inner()) = true;
            self.hold.cv.notify_all();
        }
    }

    pub(super) fn pause(path: &Path, site: Site) {
        let found = holds()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|(p, s, _)| p == path && *s == site)
            .map(|(_, _, h)| Arc::clone(h));
        let Some(hold) = found else {
            return;
        };
        hold.entered.fetch_add(1, Ordering::SeqCst);
        let mut released = hold.released.lock().unwrap_or_else(|p| p.into_inner());
        while !*released {
            released = hold.cv.wait(released).unwrap_or_else(|p| p.into_inner());
        }
    }
}

#[cfg(test)]
#[path = "wait_tests.rs"]
mod tests;
