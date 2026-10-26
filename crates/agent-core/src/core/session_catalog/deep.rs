//! Deep search: full-text scan over conversation bodies.
//!
//! Searched by default: user prompts (display projection, as in previews)
//! and assistant `text` blocks. NOT searched by default: tool results,
//! tool-call inputs and thinking. Tool results are most of the bytes on
//! disk (file dumps, command output, fetched pages): they are what the
//! model *read*, not what was *said*, and a common word would fill the
//! 200-hit cap from one `cat` of a big file. They can be opted in via
//! [`DeepSearchOptions::include_tool_results`], which also opts in
//! host-injected user-role text (event deliveries, abort context) as
//! [`DeepRole::Host`]. Thinking is private reasoning and stays out.
//!
//! The query is one literal phrase, case-insensitive. Files are first
//! checked with a raw byte pre-filter (ASCII phrases only), so only files
//! that can contain the phrase get JSON-parsed.

use super::body::{
    is_context_note, is_host_injected, read_snapshot_bytes, walk_session, walk_snapshot,
};
use super::CatalogEntry;
use crate::core::session::user_content_for_display;
use serde_json::Value;
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

/// Hard stop: the scan ends once this many hits were delivered.
pub const DEEP_HIT_CAP: usize = 200;

#[derive(Debug, Clone)]
pub struct DeepSearchOptions {
    pub include_tool_results: bool,
    /// Total hit cap (≤ [`DEEP_HIT_CAP`] recommended).
    pub max_hits: usize,
    /// At most this many hits from one session, so one huge session cannot
    /// eat the whole cap.
    pub max_hits_per_session: usize,
    /// Context characters on each side of the match.
    pub context_chars: usize,
    /// Worker threads (0 = auto, up to 4).
    pub threads: usize,
}

impl Default for DeepSearchOptions {
    fn default() -> Self {
        Self {
            include_tool_results: false,
            max_hits: DEEP_HIT_CAP,
            max_hits_per_session: 5,
            context_chars: 60,
            threads: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeepRole {
    User,
    Assistant,
    ToolResult,
    /// Host-injected user-role text (event deliveries, abort context).
    Host,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepHit {
    pub id: String,
    pub role: DeepRole,
    /// Index into the session's `api_messages`.
    pub message_index: usize,
    /// Whitespace-collapsed excerpt around the match (`…` at cut ends).
    pub snippet: String,
    /// Byte range of the match inside `snippet` (for highlighting).
    pub matched: Range<usize>,
}

#[derive(Debug, Clone)]
pub enum DeepEvent {
    Hit(DeepHit),
    /// Sent after every finished file.
    Progress {
        done: usize,
        total: usize,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeepSummary {
    pub files_total: usize,
    pub files_done: usize,
    /// Files rejected by the byte pre-filter without parsing.
    pub files_prefiltered: usize,
    pub files_failed: usize,
    pub hits: usize,
    /// Stopped because `max_hits` was reached.
    pub capped: bool,
    pub cancelled: bool,
    pub bytes_read: u64,
    pub elapsed_ms: u64,
}

/// Scan `targets` (in the given order — pass the current view order so
/// newest sessions report first) for `phrase`. Hits and progress stream
/// through `on_event` on the calling thread; set `cancel` from anywhere to
/// stop (checked per message). A blank phrase returns immediately. Blocking — run
/// it on `spawn_blocking` and forward events over a channel.
pub fn deep_search(
    dir: &Path,
    targets: &[&CatalogEntry],
    phrase: &str,
    opts: &DeepSearchOptions,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(DeepEvent),
) -> DeepSummary {
    let started = Instant::now();
    let mut sum = DeepSummary {
        files_total: targets.len(),
        ..DeepSummary::default()
    };
    let Some(m) = Matcher::new(phrase) else {
        return sum;
    };
    if targets.is_empty() || opts.max_hits == 0 {
        return sum;
    }
    let threads = match opts.threads {
        0 => std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .clamp(1, 4),
        n => n,
    }
    .min(targets.len());

    // Workers stop on the caller's cancel OR on our own cap.
    let stop = AtomicBool::new(false);
    let next = AtomicUsize::new(0);
    let (tx, rx) = std::sync::mpsc::sync_channel::<FileResult>(threads * 2);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            let tx = tx.clone();
            let (next, stop, m) = (&next, &stop, &m);
            scope.spawn(move || loop {
                if cancel.load(Ordering::Relaxed) || stop.load(Ordering::Relaxed) {
                    break;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(e) = targets.get(i) else { break };
                let r = scan_file(dir, e, m, opts, &[cancel, stop]);
                if tx.send(r).is_err() {
                    break;
                }
            });
        }
        drop(tx);
        for r in rx {
            sum.files_done += 1;
            sum.bytes_read += r.bytes;
            sum.files_prefiltered += usize::from(r.prefiltered);
            sum.files_failed += usize::from(r.failed);
            for hit in r.hits {
                if sum.hits >= opts.max_hits {
                    break;
                }
                sum.hits += 1;
                on_event(DeepEvent::Hit(hit));
            }
            on_event(DeepEvent::Progress {
                done: sum.files_done,
                total: sum.files_total,
            });
            if sum.hits >= opts.max_hits {
                sum.capped = true;
                stop.store(true, Ordering::Relaxed);
            }
            if cancel.load(Ordering::Relaxed) {
                stop.store(true, Ordering::Relaxed);
            }
        }
    });
    sum.cancelled =
        cancel.load(Ordering::Relaxed) && !sum.capped && sum.files_done < sum.files_total;
    sum.elapsed_ms = started.elapsed().as_millis() as u64;
    sum
}

struct FileResult {
    hits: Vec<DeepHit>,
    bytes: u64,
    prefiltered: bool,
    failed: bool,
}

fn scan_file(
    dir: &Path,
    e: &CatalogEntry,
    m: &Matcher,
    opts: &DeepSearchOptions,
    stop: &[&AtomicBool; 2],
) -> FileResult {
    let mut r = FileResult {
        hits: Vec::new(),
        bytes: 0,
        prefiltered: false,
        failed: false,
    };
    let visit = |hits: &mut Vec<DeepHit>, i: usize, msg: &Value| -> bool {
        if stop.iter().any(|s| s.load(Ordering::Relaxed)) {
            return false;
        }
        for (role, text) in searchable(msg, opts.include_tool_results) {
            if let Some(range) = m.find(&text) {
                hits.push(DeepHit {
                    id: e.id.clone(),
                    role,
                    message_index: i,
                    snippet: String::new(),
                    matched: 0..0,
                });
                let h = hits.last_mut().expect("just pushed");
                (h.snippet, h.matched) = snippet(&text, range, opts.context_chars);
                break; // one hit per message
            }
        }
        hits.len() < opts.max_hits_per_session
    };
    let mut idx = 0usize;
    let result = if e.has_journal {
        r.bytes = e.bytes;
        walk_session(dir, &e.id, true, &mut |msg| {
            let go = visit(&mut r.hits, idx, msg);
            idx += 1;
            go
        })
    } else {
        match read_snapshot_bytes(dir, &e.id) {
            Ok(Some(bytes)) => {
                r.bytes = bytes.len() as u64;
                if !m.may_contain_raw(&bytes) {
                    r.prefiltered = true;
                    return r;
                }
                walk_snapshot(&bytes, &mut |msg| {
                    let go = visit(&mut r.hits, idx, msg);
                    idx += 1;
                    go
                })
            }
            Ok(None) => Err(std::io::ErrorKind::NotFound.into()),
            Err(err) => Err(err),
        }
    };
    r.failed = result.is_err();
    r
}

/// The text pieces of one message that deep search looks at.
fn searchable(msg: &Value, tools: bool) -> Vec<(DeepRole, String)> {
    let mut out = Vec::new();
    match msg["role"].as_str() {
        Some("user") if !is_context_note(msg) => {
            if let Some(t) = user_content_for_display(&msg["content"]) {
                if !is_host_injected(&t) {
                    out.push((DeepRole::User, t));
                } else if tools {
                    out.push((DeepRole::Host, t));
                }
            }
            if tools {
                for b in msg["content"].as_array().into_iter().flatten() {
                    if b["type"] == "tool_result" {
                        let t = tool_result_text(&b["content"]);
                        if !t.is_empty() {
                            out.push((DeepRole::ToolResult, t));
                        }
                    }
                }
            }
        }
        Some("assistant") => match &msg["content"] {
            Value::String(s) => out.push((DeepRole::Assistant, s.clone())),
            Value::Array(blocks) => {
                let t: Vec<&str> = blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect();
                if !t.is_empty() {
                    out.push((DeepRole::Assistant, t.join("\n")));
                }
            }
            _ => {}
        },
        _ => {}
    }
    out
}

fn tool_result_text(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Case-insensitive literal phrase matcher.
struct Matcher {
    /// Lower-cased, whitespace-collapsed phrase.
    lower: String,
    ascii: Option<memchr::memmem::Finder<'static>>,
    /// Raw-byte pre-filter is sound: ASCII, and nothing JSON would escape.
    raw_ok: bool,
}

impl Matcher {
    fn new(phrase: &str) -> Option<Self> {
        let lower = phrase
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        if lower.is_empty() {
            return None;
        }
        let is_ascii = lower.is_ascii();
        let raw_ok = is_ascii && !lower.bytes().any(|b| b == b'"' || b == b'\\' || b < 0x20);
        let ascii = is_ascii.then(|| memchr::memmem::Finder::new(lower.as_bytes()).into_owned());
        Some(Self {
            lower,
            ascii,
            raw_ok,
        })
    }

    /// `false` only when the file certainly cannot contain the phrase.
    fn may_contain_raw(&self, bytes: &[u8]) -> bool {
        let Some(f) = self.ascii.as_ref().filter(|_| self.raw_ok) else {
            return true;
        };
        // A single space in the phrase may be any whitespace in the text;
        // only pre-filter on the longest space-free piece then.
        if self.lower.contains(' ') {
            let piece = self.lower.split(' ').max_by_key(|p| p.len()).unwrap_or("");
            return contains_ascii_ci(bytes, &memchr::memmem::Finder::new(piece.as_bytes()));
        }
        contains_ascii_ci(bytes, f)
    }

    /// Byte range of the first match in `text` (original offsets).
    /// Whitespace runs in `text` match a single space in the phrase.
    fn find(&self, text: &str) -> Option<Range<usize>> {
        let (norm, map) = fold(text, self.ascii.is_some());
        let at = match &self.ascii {
            Some(f) => f.find(norm.as_bytes())?,
            None => norm.find(&self.lower)?,
        };
        let end = at + self.lower.len();
        let start_orig = map[at];
        let last_orig = map[end - 1];
        let end_orig = last_orig + text[last_orig..].chars().next().map_or(1, char::len_utf8);
        Some(start_orig..end_orig)
    }
}

/// Lower-case (ASCII-only when `ascii`), collapse whitespace runs to one
/// space, and map every output byte back to its source byte offset.
fn fold(text: &str, ascii: bool) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(text.len());
    let mut map = Vec::with_capacity(text.len());
    let mut prev_space = false;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
                map.push(i);
            }
            prev_space = true;
            continue;
        }
        prev_space = false;
        if ascii {
            let l = c.to_ascii_lowercase();
            let before = out.len();
            out.push(l);
            map.extend(std::iter::repeat_n(i, out.len() - before));
        } else {
            for l in c.to_lowercase() {
                let before = out.len();
                out.push(l);
                map.extend(std::iter::repeat_n(i, out.len() - before));
            }
        }
    }
    (out, map)
}

/// ASCII case-insensitive containment in chunks (no whole-file copy).
fn contains_ascii_ci(bytes: &[u8], f: &memchr::memmem::Finder<'_>) -> bool {
    const CHUNK: usize = 1 << 20;
    let overlap = f.needle().len().saturating_sub(1);
    let mut buf = Vec::with_capacity(CHUNK + overlap);
    let mut pos = 0;
    while pos < bytes.len() {
        let start = pos.saturating_sub(overlap);
        let end = (pos + CHUNK).min(bytes.len());
        buf.clear();
        buf.extend_from_slice(&bytes[start..end]);
        buf.make_ascii_lowercase();
        if f.find(&buf).is_some() {
            return true;
        }
        pos = end;
    }
    false
}

/// Excerpt around `range` with up to `ctx` chars each side.
fn snippet(text: &str, range: Range<usize>, ctx: usize) -> (String, Range<usize>) {
    let collapse = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let before_full = &text[..range.start];
    let after_full = &text[range.end..];
    let before: String = {
        let chars: Vec<char> = before_full.chars().collect();
        let cut = chars.len() > ctx;
        let s: String = chars[chars.len().saturating_sub(ctx)..].iter().collect();
        let s = collapse(&s);
        let lead = if before_full.ends_with(char::is_whitespace) && !s.is_empty() {
            " "
        } else {
            ""
        };
        format!("{}{}{}", if cut { "…" } else { "" }, s, lead)
    };
    let matched = collapse(&text[range.clone()]);
    let after: String = {
        let cut = after_full.chars().count() > ctx;
        let s: String = after_full.chars().take(ctx).collect();
        let trail = if after_full.starts_with(char::is_whitespace) {
            " "
        } else {
            ""
        };
        let s = collapse(&s);
        if s.is_empty() {
            String::new()
        } else {
            format!("{}{}{}", trail, s, if cut { "…" } else { "" })
        }
    };
    let start = before.len();
    let snippet = format!("{before}{matched}{after}");
    (snippet, start..start + matched.len())
}
