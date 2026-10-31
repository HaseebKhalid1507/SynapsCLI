//! Tool output virtualisation.
//!
//! Once a tool result is in model history it rides every later request. A
//! text result bigger than `tool_output_spill_bytes` is therefore stored
//! whole in a private spill file (`<base>/spill/<session>/<tool_use_id>.txt`,
//! layout and retention in [`agent_core::core::tool_spill`]), and history
//! gets a short preview instead: the first `tool_output_spill_head_bytes`
//! and the last `tool_output_spill_tail_bytes` of the result, cut at line
//! boundaries where possible, and a note naming the file and the omitted
//! line range so the model can `read` it with offset/limit or `grep` it.
//! The legacy cut (`max_tool_output`, head only) loses the tail, which is
//! usually where the errors are; this loses nothing.
//!
//! Rules:
//!
//! - **Off is legacy.** A `0` threshold ([`ToolSpillPolicy::OFF`]) makes
//!   [`ToolSpill::virtualise`] return `None` before touching anything, so
//!   every caller falls through to the unchanged legacy path.
//! - **Deterministic.** The preview is a pure function of the result, the
//!   two budgets and the file path (session dir + tool_use_id): no
//!   timestamps, no random ids. History bytes, and the prompt cache over
//!   them, stay stable.
//! - **Fail-soft.** A spill that cannot be written (unwritable base dir,
//!   session cap reached) returns `None`: the caller uses the legacy cut,
//!   never a pointer to a file that does not exist.
//! - **`read` is never copied.** The model can already read that file by
//!   range, so its preview points back at the file itself with the
//!   offset/limit of the omitted lines. A `read` with an explicit `limit`
//!   is a deliberate range request and is left alone.
//! - **Rich results are untouched.** Callers only pass plain-text content
//!   (no image/document blocks).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::Value;

/// A spill file keeps at most this many bytes of a result (then a note).
pub const SPILL_FILE_CAP_BYTES: usize = 16 * 1024 * 1024;

/// One session (runtime) writes at most this many spill bytes; past it,
/// results fall back to the legacy cut. A safety net against a loop that
/// re-runs a huge command, not a budget anyone should meet.
pub const SPILL_SESSION_CAP_BYTES: u64 = 256 * 1024 * 1024;

/// Builtin tools whose results are output dumps (command output, search
/// hits, listings, worker reports) and may be spilled. Everything else
/// builtin (`edit`, `write`, `load_skill`, memory and forum tools, …)
/// returns content the model asked for in full, and keeps the legacy cut.
const SPILL_BUILTINS: &[&str] = &[
    "bash",
    "powershell",
    "shell_start",
    "shell_send",
    "shell_end",
    "grep",
    "find",
    "ls",
    "subagent",
    "subagent_collect",
    "subagent_resume",
    "subagent_status",
];

/// Thresholds from `tool_output_spill_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSpillPolicy {
    /// Results strictly bigger than this are virtualised. `0` = off.
    pub threshold_bytes: usize,
    /// Bytes of the result's start kept in the preview.
    pub head_bytes: usize,
    /// Bytes of the result's end kept in the preview.
    pub tail_bytes: usize,
}

impl ToolSpillPolicy {
    /// Virtualisation off: every result takes the legacy path.
    pub const OFF: Self = Self {
        threshold_bytes: 0,
        head_bytes: agent_core::config::DEFAULT_TOOL_OUTPUT_SPILL_HEAD_BYTES,
        tail_bytes: agent_core::config::DEFAULT_TOOL_OUTPUT_SPILL_TAIL_BYTES,
    };

    pub fn from_config(config: &agent_core::config::SynapsConfig) -> Self {
        Self {
            threshold_bytes: config.tool_output_spill_bytes,
            head_bytes: config.tool_output_spill_head_bytes,
            tail_bytes: config.tool_output_spill_tail_bytes,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.threshold_bytes > 0
    }
}

impl Default for ToolSpillPolicy {
    fn default() -> Self {
        Self::OFF
    }
}

/// Session-cumulative counters, shared by a runtime and its clones.
/// Metadata only.
#[derive(Debug, Default)]
pub struct ToolSpillStats {
    virtualised: AtomicU64,
    original_bytes: AtomicU64,
    preview_bytes: AtomicU64,
    file_bytes: AtomicU64,
    failure_warned: AtomicBool,
}

/// Point-in-time copy of [`ToolSpillStats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolSpillTotals {
    /// Results whose history entry is a preview.
    pub virtualised: u64,
    /// Their full size.
    pub original_bytes: u64,
    /// Their preview size in history.
    pub preview_bytes: u64,
    /// Bytes written to spill files (`read` previews write none).
    pub file_bytes: u64,
}

impl ToolSpillTotals {
    /// Bytes kept out of history: `original - preview`.
    pub fn kept_out_bytes(&self) -> u64 {
        self.original_bytes.saturating_sub(self.preview_bytes)
    }
}

impl ToolSpillStats {
    pub fn totals(&self) -> ToolSpillTotals {
        ToolSpillTotals {
            virtualised: self.virtualised.load(Ordering::Relaxed),
            original_bytes: self.original_bytes.load(Ordering::Relaxed),
            preview_bytes: self.preview_bytes.load(Ordering::Relaxed),
            file_bytes: self.file_bytes.load(Ordering::Relaxed),
        }
    }
}

/// How one call's result may be virtualised, decided from the tool and its
/// (post-`before_tool_call`) input before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpillTarget {
    /// Keep the legacy cut.
    Never,
    /// Store the whole result in a spill file.
    Spill,
    /// `read` without an explicit `limit`: point back at this file.
    ReadFile(PathBuf),
}

impl SpillTarget {
    /// `runtime_name` is the tool's runtime (not API-safe) name. Extension,
    /// MCP and plugin tools spill; builtins only from [`SPILL_BUILTINS`];
    /// tools of unknown origin keep the legacy cut.
    pub fn classify(
        runtime_name: &str,
        origin: &crate::tools::ToolOrigin,
        input: &Value,
        cwd: Option<&Path>,
    ) -> Self {
        use crate::tools::ToolOrigin;
        match origin {
            ToolOrigin::Builtin if runtime_name == "read" => {
                let ranged = input.get("limit").is_some_and(|limit| !limit.is_null());
                match input.get("path").and_then(Value::as_str) {
                    Some(path) if !ranged => {
                        SpillTarget::ReadFile(crate::tools::resolve_path_in(path, cwd))
                    }
                    _ => SpillTarget::Never,
                }
            }
            ToolOrigin::Builtin if SPILL_BUILTINS.contains(&runtime_name) => SpillTarget::Spill,
            ToolOrigin::Builtin | ToolOrigin::Unknown => SpillTarget::Never,
            ToolOrigin::Extension { .. } | ToolOrigin::Mcp { .. } | ToolOrigin::Plugin { .. } => {
                SpillTarget::Spill
            }
        }
    }
}

/// A virtualised result: what history carries instead of the full text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Virtualised {
    pub preview: String,
    pub original_bytes: usize,
    /// The spill file, `None` for a `read` preview (it points at the file).
    pub spill_path: Option<PathBuf>,
}

/// One session's virtualiser: policy, spill dir, shared counters. Cheap to
/// clone; built per stream from the runtime's bound spill root.
#[derive(Debug, Clone)]
pub struct ToolSpill {
    policy: ToolSpillPolicy,
    /// `<root>/<session>`; `None` when the session key is not a safe path
    /// component (spills then fall back; `read` previews still work).
    dir: Option<PathBuf>,
    stats: Arc<ToolSpillStats>,
    file_cap: usize,
    session_cap: u64,
}

impl Default for ToolSpill {
    fn default() -> Self {
        Self::off()
    }
}

impl ToolSpill {
    /// Virtualisation off.
    pub fn off() -> Self {
        Self {
            policy: ToolSpillPolicy::OFF,
            dir: None,
            stats: Arc::default(),
            file_cap: SPILL_FILE_CAP_BYTES,
            session_cap: SPILL_SESSION_CAP_BYTES,
        }
    }

    /// Spill into `<root>/<session_key>/`.
    pub fn new(
        policy: ToolSpillPolicy,
        root: &Path,
        session_key: &str,
        stats: Arc<ToolSpillStats>,
    ) -> Self {
        let dir = agent_core::core::tool_spill::session_component(session_key)
            .map(|component| root.join(component));
        Self {
            policy,
            dir,
            stats,
            file_cap: SPILL_FILE_CAP_BYTES,
            session_cap: SPILL_SESSION_CAP_BYTES,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_caps(mut self, file_cap: usize, session_cap: u64) -> Self {
        self.file_cap = file_cap;
        self.session_cap = session_cap;
        self
    }

    pub fn is_enabled(&self) -> bool {
        self.policy.is_enabled()
    }

    pub fn policy(&self) -> ToolSpillPolicy {
        self.policy
    }

    /// This session's spill dir, when it has a valid one.
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    pub fn totals(&self) -> ToolSpillTotals {
        self.stats.totals()
    }

    /// The history text for a finished call's plain-text result, or `None`
    /// to keep the legacy path: off, not a virtualised tool, not bigger than
    /// the threshold, nothing to omit, or the spill could not be written.
    pub fn virtualise(
        &self,
        tool_use_id: &str,
        target: &SpillTarget,
        text: &str,
    ) -> Option<Virtualised> {
        if !self.policy.is_enabled() || text.len() <= self.policy.threshold_bytes {
            return None;
        }
        let cut = Cut::new(text, self.policy.head_bytes, self.policy.tail_bytes)?;
        let virtualised = match target {
            SpillTarget::Never => return None,
            SpillTarget::ReadFile(path) => Virtualised {
                preview: read_preview(text, &cut, path),
                original_bytes: text.len(),
                spill_path: None,
            },
            SpillTarget::Spill => {
                let (path, capped) = match self.write_spill(tool_use_id, text) {
                    Ok(written) => written,
                    Err(error) => {
                        if !self.stats.failure_warned.swap(true, Ordering::Relaxed) {
                            // Metadata only: our own dir and the error.
                            tracing::warn!(
                                dir = ?self.dir,
                                %error,
                                "tool output spill failed; big results keep the legacy cut",
                            );
                        }
                        return None;
                    }
                };
                Virtualised {
                    preview: spill_preview(text, &cut, &path, capped.then_some(self.file_cap)),
                    original_bytes: text.len(),
                    spill_path: Some(path),
                }
            }
        };
        self.stats.virtualised.fetch_add(1, Ordering::Relaxed);
        self.stats
            .original_bytes
            .fetch_add(text.len() as u64, Ordering::Relaxed);
        self.stats
            .preview_bytes
            .fetch_add(virtualised.preview.len() as u64, Ordering::Relaxed);
        tracing::debug!(
            tool_use_id,
            original_bytes = text.len(),
            preview_bytes = virtualised.preview.len(),
            spilled = virtualised.spill_path.is_some(),
            "tool output virtualised",
        );
        Some(virtualised)
    }

    /// Write `text` (capped) to a new private file in the session dir.
    /// Returns its path and whether the cap cut it.
    fn write_spill(&self, tool_use_id: &str, text: &str) -> std::io::Result<(PathBuf, bool)> {
        let dir = self.dir.as_deref().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "no valid spill session dir")
        })?;
        let capped = text.len() > self.file_cap;
        let body = if capped {
            agent_core::truncate_str(text, self.file_cap)
        } else {
            text
        };
        let note = capped.then(|| {
            format!(
                "\n[spill file cut at {} of {} bytes]\n",
                body.len(),
                text.len()
            )
        });
        let size = (body.len() + note.as_ref().map_or(0, String::len)) as u64;
        let before = self.stats.file_bytes.fetch_add(size, Ordering::Relaxed);
        if before.saturating_add(size) > self.session_cap {
            self.stats.file_bytes.fetch_sub(size, Ordering::Relaxed);
            return Err(std::io::Error::other(format!(
                "session spill cap of {} bytes reached",
                self.session_cap
            )));
        }
        let written = (|| {
            if let Some(root) = dir.parent() {
                agent_core::core::private_fs::ensure_private_dir(root)?;
            }
            agent_core::core::private_fs::ensure_private_dir(dir)?;
            let (path, mut file) = create_spill_file(dir, &file_stem(tool_use_id))?;
            file.write_all(body.as_bytes())?;
            if let Some(note) = &note {
                file.write_all(note.as_bytes())?;
            }
            Ok::<_, std::io::Error>(path)
        })();
        match written {
            Ok(path) => Ok((path, capped)),
            Err(error) => {
                self.stats.file_bytes.fetch_sub(size, Ordering::Relaxed);
                Err(error)
            }
        }
    }
}

/// A tool_use_id as a file stem: `[A-Za-z0-9_-]` kept, anything else `_`,
/// at most 96 bytes, never empty.
fn file_stem(tool_use_id: &str) -> String {
    let stem: String = tool_use_id
        .chars()
        .take(96)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if stem.is_empty() {
        "tool".to_string()
    } else {
        stem
    }
}

/// Create `<stem>.txt` (or `<stem>-N.txt` when taken: providers that reuse
/// ids like `call_0`, or two ids that sanitize alike) as a NEW `0600`
/// file, never following a symlink. An existing file is never reused, so
/// an earlier preview's pointer keeps pointing at its own output.
fn create_spill_file(dir: &Path, stem: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    let mut last = None;
    for n in 0..16 {
        let name = if n == 0 {
            format!("{stem}.txt")
        } else {
            format!("{stem}-{n}.txt")
        };
        let path = dir.join(name);
        match create_new_private(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no free spill file name")))
}

fn create_new_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(agent_core::core::private_fs::FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options.open(path)
}

/// Where a result is cut: `text[..head_end]` and `text[tail_start..]` are
/// kept, with line numbers for the note. Both cuts are on UTF-8 char
/// boundaries; each moves to a line boundary when that keeps at least half
/// its budget (one huge line is cut mid-line instead of dropped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cut {
    head_end: usize,
    tail_start: usize,
    /// Lines in the whole text (a final line without `\n` counts).
    total_lines: usize,
    /// First and last line (1-based) touched by the omitted bytes.
    omitted_first: usize,
    omitted_last: usize,
}

impl Cut {
    /// `None` when head + tail already cover the text (nothing to omit).
    fn new(text: &str, head: usize, tail: usize) -> Option<Self> {
        let len = text.len();
        if head.saturating_add(tail) >= len {
            return None;
        }
        let bytes = text.as_bytes();

        let mut head_end = floor_char_boundary(text, head);
        if let Some(nl) = bytes[..head_end].iter().rposition(|&b| b == b'\n') {
            if 2 * (nl + 1) >= head_end {
                head_end = nl + 1;
            }
        }

        let mut tail_start = ceil_char_boundary(text, len - tail);
        if tail_start < len && bytes[tail_start - 1] != b'\n' {
            if let Some(off) = bytes[tail_start..].iter().position(|&b| b == b'\n') {
                let line_start = tail_start + off + 1;
                if 2 * (len - line_start) >= len - tail_start {
                    tail_start = line_start;
                }
            }
        }
        debug_assert!(head_end < tail_start, "head < len - tail <= tail_start");

        let newlines = |range: std::ops::Range<usize>| {
            bytes[range].iter().filter(|&&b| b == b'\n').count()
        };
        let before_omitted = newlines(0..head_end);
        let within_omitted = newlines(head_end..tail_start - 1);
        let after = newlines(tail_start - 1..len);
        let total_lines =
            before_omitted + within_omitted + after + usize::from(!text.ends_with('\n'));
        Some(Self {
            head_end,
            tail_start,
            total_lines,
            omitted_first: before_omitted + 1,
            omitted_last: before_omitted + within_omitted + 1,
        })
    }

    fn head<'a>(&self, text: &'a str) -> &'a str {
        &text[..self.head_end]
    }

    fn tail<'a>(&self, text: &'a str) -> &'a str {
        &text[self.tail_start..]
    }

    fn omitted_lines(&self) -> usize {
        self.omitted_last - self.omitted_first + 1
    }
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// `head`, the gap marker, `tail`, then the note.
fn assemble(text: &str, cut: &Cut, marker: &str, note: &str) -> String {
    let (head, tail) = (cut.head(text), cut.tail(text));
    let mut out = String::with_capacity(head.len() + tail.len() + marker.len() + note.len() + 8);
    out.push_str(head);
    if !head.is_empty() && !head.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(marker);
    out.push('\n');
    out.push_str(tail);
    if !tail.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(note);
    out
}

fn spill_preview(text: &str, cut: &Cut, path: &Path, capped_at: Option<usize>) -> String {
    let (first, last) = (cut.omitted_first, cut.omitted_last);
    let marker = format!("[... lines {first}-{last} omitted ...]");
    let capped = capped_at
        .map(|cap| format!(" The file keeps the first {cap} bytes."))
        .unwrap_or_default();
    let note = format!(
        "[Output virtualised: {} bytes, {} lines; lines {first}-{last} are omitted above. \
         Full output: {} (read it with offset={} limit={}, or grep it).{capped}]",
        text.len(),
        cut.total_lines,
        path.display(),
        first - 1,
        cut.omitted_lines(),
    );
    assemble(text, cut, &marker, &note)
}

/// `read` output lines are `<file line>\t<content>`: map the omitted
/// output lines back to file lines so the hint is an exact offset/limit.
fn read_preview(text: &str, cut: &Cut, path: &Path) -> String {
    let file_line_at = |pos: usize| -> Option<usize> {
        let start = text[..pos].rfind('\n').map_or(0, |nl| nl + 1);
        let digits = text[start..].split('\t').next()?;
        digits.parse::<usize>().ok().filter(|n| *n > 0)
    };
    let file_range = file_line_at(cut.head_end)
        .zip(file_line_at(cut.tail_start - 1))
        .filter(|(first, last)| first <= last);
    match file_range {
        Some((first, last)) => {
            let marker = format!("[... file lines {first}-{last} omitted ...]");
            let note = format!(
                "[Output virtualised: {} bytes; file lines {first}-{last} are omitted above. \
                 Read {} with offset={} limit={} to see them.]",
                text.len(),
                path.display(),
                first - 1,
                last - first + 1,
            );
            assemble(text, cut, &marker, &note)
        }
        None => {
            let (first, last) = (cut.omitted_first, cut.omitted_last);
            let marker = format!("[... lines {first}-{last} omitted ...]");
            let note = format!(
                "[Output virtualised: {} bytes, {} lines; lines {first}-{last} of this output \
                 are omitted above. Read {} with offset/limit to see the rest.]",
                text.len(),
                cut.total_lines,
                path.display(),
            );
            assemble(text, cut, &marker, &note)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolOrigin;
    use serde_json::json;

    fn policy(threshold: usize, head: usize, tail: usize) -> ToolSpillPolicy {
        ToolSpillPolicy {
            threshold_bytes: threshold,
            head_bytes: head,
            tail_bytes: tail,
        }
    }

    fn spill_in(root: &Path, policy: ToolSpillPolicy) -> ToolSpill {
        ToolSpill::new(policy, root, "sess-1", Arc::default())
    }

    /// `n` numbered lines of `width` bytes each (including `\n`).
    fn lines(n: usize, width: usize) -> String {
        (1..=n)
            .map(|i| {
                let label = format!("line {i:05} ");
                format!("{label}{}\n", "x".repeat(width - label.len() - 1))
            })
            .collect()
    }

    #[test]
    fn threshold_is_exclusive_at_exactly_and_virtualises_one_over() {
        let tmp = tempfile::tempdir().unwrap();
        let spill = spill_in(tmp.path(), policy(1000, 100, 100));
        let exact = "a".repeat(1000);
        assert_eq!(spill.virtualise("t1", &SpillTarget::Spill, &exact), None);
        let over = "a".repeat(1001);
        let v = spill.virtualise("t2", &SpillTarget::Spill, &over).expect("one over spills");
        assert_eq!(v.original_bytes, 1001);
        assert_eq!(std::fs::read_to_string(v.spill_path.unwrap()).unwrap(), over);
    }

    #[test]
    fn nothing_to_omit_keeps_the_legacy_path() {
        let tmp = tempfile::tempdir().unwrap();
        // Over the threshold, but head + tail already cover all of it.
        let spill = spill_in(tmp.path(), policy(10, 600, 600));
        assert_eq!(spill.virtualise("t", &SpillTarget::Spill, &"z".repeat(1200)), None);
        assert!(!tmp.path().join("sess-1").exists(), "no file written");
    }

    #[test]
    fn off_and_never_touch_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let big = lines(500, 80);
        let off = ToolSpill::new(ToolSpillPolicy::OFF, tmp.path(), "sess-1", Arc::default());
        assert_eq!(off.virtualise("t", &SpillTarget::Spill, &big), None);
        assert_eq!(ToolSpill::off().virtualise("t", &SpillTarget::Spill, &big), None);
        let on = spill_in(tmp.path(), policy(1024, 256, 256));
        assert_eq!(on.virtualise("t", &SpillTarget::Never, &big), None);
        assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none(), "nothing written");
        assert_eq!(on.totals(), ToolSpillTotals::default());
    }

    #[test]
    fn cuts_land_on_line_boundaries_and_name_the_omitted_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let text = lines(200, 50); // 10_000 bytes, 200 lines
        let spill = spill_in(tmp.path(), policy(4096, 1030, 520));
        let v = spill.virtualise("toolu_x", &SpillTarget::Spill, &text).unwrap();
        // 1030 bytes of head = 20 whole lines (1000 bytes); 520 of tail =
        // 10 whole lines (500 bytes): the partial lines are not kept.
        let head = lines(20, 50);
        let tail: String = text[text.len() - 500..].to_string();
        let path = tmp.path().join("sess-1").join("toolu_x.txt");
        let expected = format!(
            "{head}[... lines 21-190 omitted ...]\n{tail}\n\
             [Output virtualised: 10000 bytes, 200 lines; lines 21-190 are omitted above. \
             Full output: {} (read it with offset=20 limit=170, or grep it).]",
            path.display()
        );
        assert_eq!(v.preview, expected);
        assert!(tail.starts_with("line 00191 "));
        // The hint is exact: the spill file's lines 21..=190 are the gap.
        let file = std::fs::read_to_string(&path).unwrap();
        let gap: Vec<&str> = file.lines().skip(20).take(170).collect();
        assert!(gap[0].starts_with("line 00021 ") && gap[169].starts_with("line 00190 "));
    }

    #[test]
    fn one_huge_line_is_cut_mid_line_on_char_boundaries() {
        let tmp = tempfile::tempdir().unwrap();
        // Multibyte everywhere: 'é' is 2 bytes, '🦀' 4; no newline at all.
        let text = "é🦀".repeat(5000); // 30_000 bytes
        let spill = spill_in(tmp.path(), policy(1000, 1001, 999));
        let v = spill.virtualise("t", &SpillTarget::Spill, &text).unwrap();
        let cut = Cut::new(&text, 1001, 999).unwrap();
        assert!(text.is_char_boundary(cut.head_end) && text.is_char_boundary(cut.tail_start));
        assert!(cut.head_end <= 1001 && cut.head_end > 990, "{}", cut.head_end);
        assert!(text.len() - cut.tail_start <= 999 && text.len() - cut.tail_start > 990);
        assert!(v.preview.starts_with(cut.head(&text)));
        assert!(v.preview.contains(&format!("\n[... lines 1-1 omitted ...]\n{}\n", cut.tail(&text))));
        assert_eq!(cut.total_lines, 1);
    }

    #[test]
    fn a_short_first_line_does_not_waste_the_head_budget() {
        // The only newline in the head budget is near its start: cutting
        // there would keep 4 of 1000 bytes, so the head is cut mid-line.
        let text = format!("abc\n{}\n{}", "y".repeat(5000), lines(10, 40));
        let cut = Cut::new(&text, 1000, 200).unwrap();
        assert_eq!(cut.head_end, 1000);
        assert_eq!(cut.omitted_first, 2, "line 2 is partly shown and partly omitted");
        assert!(cut.tail(&text).starts_with("line 00006 "), "tail cut moved to a line start");
    }

    #[test]
    fn preview_is_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let text = lines(400, 64);
        let spill = spill_in(tmp.path(), policy(12_288, 4096, 3072));
        let first = spill.virtualise("toolu_same", &SpillTarget::Spill, &text).unwrap();
        // Same output, same tool_use_id, same session dir: identical bytes.
        std::fs::remove_file(first.spill_path.as_ref().unwrap()).unwrap();
        let again = spill_in(tmp.path(), policy(12_288, 4096, 3072));
        let second = again.virtualise("toolu_same", &SpillTarget::Spill, &text).unwrap();
        assert_eq!(first.preview.as_bytes(), second.preview.as_bytes());
        let cut = Cut::new(&text, 4096, 3072).unwrap();
        let path = tmp.path().join("p.txt");
        assert_eq!(spill_preview(&text, &cut, &path, None), spill_preview(&text, &cut, &path, None));
    }

    #[test]
    fn default_budgets_shrink_a_25kb_result_to_about_7kb() {
        let tmp = tempfile::tempdir().unwrap();
        let config = agent_core::config::SynapsConfig::default();
        let spill = spill_in(tmp.path(), ToolSpillPolicy::from_config(&config));
        let text = lines(320, 80); // 25_600 bytes
        let v = spill.virtualise("toolu_big", &SpillTarget::Spill, &text).unwrap();
        assert!(v.preview.len() < 7 * 1024 + 400, "{}", v.preview.len());
        assert!(v.preview.len() > 6 * 1024, "{}", v.preview.len());
        let totals = spill.totals();
        assert_eq!(totals.virtualised, 1);
        assert_eq!(totals.original_bytes, 25_600);
        assert_eq!(totals.preview_bytes, v.preview.len() as u64);
        assert_eq!(totals.file_bytes, 25_600);
        assert_eq!(totals.kept_out_bytes(), 25_600 - v.preview.len() as u64);
    }

    #[cfg(unix)]
    #[test]
    fn spill_dirs_are_0700_and_files_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("spill");
        let spill = ToolSpill::new(policy(100, 10, 10), &root, "sess-1", Arc::default());
        let v = spill.virtualise("t", &SpillTarget::Spill, &"q".repeat(500)).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&root.join("sess-1")), 0o700);
        assert_eq!(mode(v.spill_path.as_ref().unwrap()), 0o600);
    }

    #[test]
    fn per_file_cap_cuts_the_file_with_a_note_and_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let spill = spill_in(tmp.path(), policy(100, 20, 60)).with_caps(1000, u64::MAX);
        let text = lines(100, 50); // 5_000 bytes
        let v = spill.virtualise("t", &SpillTarget::Spill, &text).unwrap();
        let file = std::fs::read_to_string(v.spill_path.as_ref().unwrap()).unwrap();
        assert!(file.starts_with(&text[..1000]));
        assert!(file.ends_with("\n[spill file cut at 1000 of 5000 bytes]\n"), "{file:?}");
        assert!(v.preview.ends_with(" The file keeps the first 1000 bytes.]"));
        // The preview still carries the real tail.
        assert!(v.preview.contains("line 00100 "));
    }

    #[test]
    fn session_cap_falls_back_to_the_legacy_cut() {
        let tmp = tempfile::tempdir().unwrap();
        let spill = spill_in(tmp.path(), policy(100, 20, 20)).with_caps(usize::MAX, 1500);
        let text = "w".repeat(1000);
        assert!(spill.virtualise("a", &SpillTarget::Spill, &text).is_some());
        assert_eq!(spill.virtualise("b", &SpillTarget::Spill, &text), None, "over the cap");
        assert_eq!(spill.totals().file_bytes, 1000);
        assert!(!tmp.path().join("sess-1/b.txt").exists());
    }

    #[test]
    fn unwritable_or_invalid_dirs_fall_back_instead_of_pointing_nowhere() {
        let tmp = tempfile::tempdir().unwrap();
        // The "root" is a regular file: no dir can be made under it.
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let spill = ToolSpill::new(policy(100, 10, 10), &blocker, "sess-1", Arc::default());
        assert_eq!(spill.virtualise("t", &SpillTarget::Spill, &"q".repeat(500)), None);
        assert_eq!(spill.totals(), ToolSpillTotals::default());
        // A session key that is not one path component never becomes a dir.
        let spill = ToolSpill::new(policy(100, 10, 10), tmp.path(), "../escape", Arc::default());
        assert_eq!(spill.dir(), None);
        assert_eq!(spill.virtualise("t", &SpillTarget::Spill, &"q".repeat(500)), None);
    }

    #[test]
    fn reused_tool_use_ids_never_overwrite_an_earlier_spill() {
        let tmp = tempfile::tempdir().unwrap();
        let spill = spill_in(tmp.path(), policy(100, 10, 10));
        let a = spill.virtualise("call_0", &SpillTarget::Spill, &"a".repeat(300)).unwrap();
        let b = spill.virtualise("call_0", &SpillTarget::Spill, &"b".repeat(300)).unwrap();
        let c = spill.virtualise("call/0", &SpillTarget::Spill, &"c".repeat(300)).unwrap();
        let name = |v: &Virtualised| v.spill_path.as_ref().unwrap().file_name().unwrap().to_owned();
        assert_eq!(name(&a), "call_0.txt");
        assert_eq!(name(&b), "call_0-1.txt");
        assert_eq!(name(&c), "call_0-2.txt", "sanitized alike: still a new file");
        assert_eq!(std::fs::read_to_string(a.spill_path.unwrap()).unwrap(), "a".repeat(300));
        assert_eq!(file_stem(""), "tool");
        assert_eq!(file_stem("toolu_01AbC-9"), "toolu_01AbC-9");
    }

    #[test]
    fn read_points_back_at_the_file_with_an_exact_offset_and_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("big.rs");
        let body: String = (1..=500).map(|i| format!("fn line_{i:03}() {{ /* {} */ }}\n", "r".repeat(40))).collect();
        std::fs::write(&file, &body).unwrap();
        // What `read` returns: `<n>\t<line>` (1-based).
        let text: String = body.lines().enumerate().map(|(i, l)| format!("{}\t{l}\n", i + 1)).collect();
        let spill = spill_in(tmp.path(), policy(12_288, 4096, 3072));
        let target = SpillTarget::ReadFile(file.clone());
        let v = spill.virtualise("toolu_r", &target, &text).unwrap();
        assert_eq!(v.spill_path, None, "read is never copied");
        assert!(!tmp.path().join("sess-1").exists(), "no spill dir either");
        let cut = Cut::new(&text, 4096, 3072).unwrap();
        let first: usize = text.lines().nth(cut.omitted_first - 1).unwrap().split('\t').next().unwrap().parse().unwrap();
        let last: usize = text.lines().nth(cut.omitted_last - 1).unwrap().split('\t').next().unwrap().parse().unwrap();
        assert!(v.preview.contains(&format!("[... file lines {first}-{last} omitted ...]")));
        assert!(v.preview.ends_with(&format!(
            "file lines {first}-{last} are omitted above. Read {} with offset={} limit={} to see them.]",
            file.display(),
            first - 1,
            last - first + 1
        )));
        // That offset/limit names exactly the omitted file lines.
        let omitted: Vec<&str> = body.lines().skip(first - 1).take(last - first + 1).collect();
        assert!(omitted[0].starts_with(&format!("fn line_{first:03}")));
        assert!(!v.preview.contains(omitted[0]));
        assert_eq!(spill.totals().file_bytes, 0);
    }

    #[test]
    fn read_output_without_line_numbers_still_points_at_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let spill = spill_in(tmp.path(), policy(1000, 100, 100));
        let v = spill
            .virtualise("t", &SpillTarget::ReadFile(PathBuf::from("/p/x.log")), &lines(100, 40))
            .unwrap();
        assert!(v.preview.ends_with(
            "of this output are omitted above. Read /p/x.log with offset/limit to see the rest.]"
        ));
    }

    #[test]
    fn classify_spills_output_dumps_and_extension_mcp_tools_only() {
        let builtin = ToolOrigin::Builtin;
        let none = json!({});
        for name in ["bash", "grep", "find", "ls", "shell_send", "subagent_collect"] {
            assert_eq!(SpillTarget::classify(name, &builtin, &none, None), SpillTarget::Spill, "{name}");
        }
        // Mutation results (multi_edit's included: at most 64 short lines,
        // each saying what changed) stay whole in history, never a preview.
        for name in ["edit", "write", "multi_edit", "load_skill", "memory_fetch", "respond"] {
            assert_eq!(SpillTarget::classify(name, &builtin, &none, None), SpillTarget::Never, "{name}");
        }
        let mcp = ToolOrigin::Mcp {
            server_id: "s".into(),
            server_tool_name: "t".into(),
        };
        let ext = ToolOrigin::Extension {
            extension_id: "e".into(),
        };
        let plugin = ToolOrigin::Plugin {
            plugin_id: "p".into(),
            tool_name: "t".into(),
        };
        for origin in [&mcp, &ext, &plugin] {
            assert_eq!(SpillTarget::classify("read", origin, &none, None), SpillTarget::Spill);
        }
        assert_eq!(SpillTarget::classify("bash", &ToolOrigin::Unknown, &none, None), SpillTarget::Never);
        // read: the original path (cwd-relative), unless a limit was asked for.
        let cwd = Path::new("/work");
        assert_eq!(
            SpillTarget::classify("read", &builtin, &json!({"path": "src/a.rs", "offset": 10}), Some(cwd)),
            SpillTarget::ReadFile(PathBuf::from("/work/src/a.rs"))
        );
        assert_eq!(
            SpillTarget::classify("read", &builtin, &json!({"path": "a.rs", "limit": 900}), Some(cwd)),
            SpillTarget::Never
        );
        assert_eq!(
            SpillTarget::classify("read", &builtin, &json!({"path": "a.rs", "limit": null}), Some(cwd)),
            SpillTarget::ReadFile(PathBuf::from("/work/a.rs"))
        );
        assert_eq!(SpillTarget::classify("read", &builtin, &none, None), SpillTarget::Never);
    }
}
