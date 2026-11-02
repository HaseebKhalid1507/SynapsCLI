//! Last words: a panic reaches `synaps.log` before the process can die.
//!
//! The default panic hook writes to stderr only, and a daemon's stderr may
//! be gone; in a release build (`panic = "abort"`) the process then dies
//! with no trace at all. [`install`] puts, ahead of the default hook, ONE
//! `ERROR` line through tracing — target [`TARGET`], message `PANIC`, with
//! the thread, the location, the payload and a backtrace of at most
//! [`MAX_FRAMES`] frames (captured even without `RUST_BACKTRACE`) on a
//! single line — then, when the panic is going to abort, flushes the log
//! writer, then runs the default hook (stderr, as before).
//!
//! Under `panic = "unwind"` a panic usually ends one thread or one tokio
//! task, and the process lives on, so the hook must not shut the log writer
//! down: the line drains on the writer's own thread, and `main` flushes on
//! its way out when a panic does end the process.
//!
//! The hook never waits for a lock: the flush only `try_lock`s the writer
//! guard, and a panic raised by the logging stack itself (the writer
//! thread, or a location inside a `tracing*` crate) gets no tracing line —
//! re-entering a logger that just panicked could panic again (an abort) or
//! deadlock. stderr still gets the default report.

use std::any::Any;
use std::panic::Location;

/// Tracing target of the panic line (kept by the log file's `synaps_cli`
/// directive, and by any filter at ERROR).
pub const TARGET: &str = "synaps_cli::panic";

/// Backtrace frames kept on the line.
pub const MAX_FRAMES: usize = 40;

/// Payload bytes kept on the line.
pub const MAX_PAYLOAD_BYTES: usize = 4096;

/// tracing-appender's writer thread (its default name).
const APPENDER_THREAD: &str = "tracing-appender";

/// Install the hook in front of the current one (the default hook).
pub fn install() {
    let next = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        last_words(info.location(), info.payload());
        if cfg!(panic = "abort") {
            // The process ends right after the default hook: drain the
            // writer now or lose the line. Never blocks (see module doc).
            agent_engine::EngineHost::try_flush_installed_logs();
        }
        next(info);
    }));
}

/// Emit the panic line for a panic at `location` with `payload`, unless the
/// logging stack itself panicked. Takes the parts, not the hook's info type,
/// whose name (`PanicHookInfo`) is newer than the crate's MSRV.
pub fn last_words(location: Option<&Location<'_>>, payload: &(dyn Any + Send)) {
    let thread = std::thread::current();
    let report = Report::new(
        thread.name().unwrap_or("<unnamed>"),
        location.map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())),
        payload,
        &std::backtrace::Backtrace::force_capture().to_string(),
    );
    if !report.from_logging_stack() {
        report.emit();
    }
}

/// Everything the panic line carries, each field on one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub thread: String,
    pub location: String,
    pub payload: String,
    pub backtrace: String,
}

impl Report {
    pub fn new(thread: &str, location: Option<String>, payload: &(dyn Any + Send), backtrace: &str) -> Self {
        Self {
            thread: thread.to_string(),
            location: location.unwrap_or_else(|| "<unknown>".to_string()),
            payload: cap(&payload_text(payload), MAX_PAYLOAD_BYTES),
            backtrace: compact_backtrace(backtrace, MAX_FRAMES),
        }
    }

    /// The panic came from the logging machinery: don't log through it.
    pub fn from_logging_stack(&self) -> bool {
        self.thread == APPENDER_THREAD || in_tracing_crate(&self.location)
    }

    /// The ERROR line. Payload and backtrace are `Debug`-quoted, so a
    /// newline in either stays on the one line.
    pub fn emit(&self) {
        tracing::error!(
            target: TARGET,
            thread = ?self.thread,
            location = %self.location,
            payload = ?self.payload,
            backtrace = ?self.backtrace,
            "PANIC"
        );
    }
}

fn payload_text(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "Box<dyn Any>".to_string()
    }
}

/// At most `max` bytes of `s`, cut on a char boundary, with a marker.
fn cap(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes)", &s[..end], s.len())
}

/// Whether `location` (a `file:line:col`) is inside a `tracing*` crate's
/// source (`…/tracing-subscriber-0.3.x/src/…`, `…/tracing-0.1.x/…`).
fn in_tracing_crate(location: &str) -> bool {
    location
        .split(['/', '\\'])
        .any(|c| c.starts_with("tracing-") && c.len() > "tracing-".len())
}

/// Frames of the panic machinery itself, dropped from the top.
fn is_machinery(frame: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "std::backtrace",
        "std::sys::backtrace",
        "std::panicking",
        "std::panic::",
        "core::panicking",
        "rust_begin_unwind",
        "__rustc::rust_begin_unwind",
        "synaps_cli::panic_hook",
        "<alloc::boxed::Box<F,A> as core::ops::function::Fn<Args>>::call",
    ];
    PREFIXES.iter().any(|p| frame.starts_with(p))
}

/// `Backtrace`'s multi-line text as one line: `N: function at file:line`
/// frames joined by ` | `, the leading panic-machinery frames dropped, at
/// most `max` frames, then how many were cut. Text that is not a frame list
/// ("disabled backtrace", "unsupported backtrace") comes back trimmed.
pub fn compact_backtrace(text: &str, max: usize) -> String {
    let mut frames: Vec<String> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(at) = t.strip_prefix("at ") {
            if let Some(last) = frames.last_mut() {
                last.push_str(" at ");
                last.push_str(at.trim());
            }
            continue;
        }
        if let Some((n, rest)) = t.split_once(": ") {
            if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
                frames.push(rest.trim().to_string());
            }
        }
    }
    if frames.is_empty() {
        return text.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    let first = frames.iter().position(|f| !is_machinery(f)).unwrap_or(0);
    let kept: Vec<&String> = frames[first..].iter().take(max).collect();
    let cut = frames.len() - first - kept.len();
    let mut out = kept
        .iter()
        .enumerate()
        .map(|(i, f)| format!("{i}: {f}"))
        .collect::<Vec<_>>()
        .join(" | ");
    if cut > 0 {
        out.push_str(&format!(" | … {cut} more frames"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "   0: std::backtrace_rs::backtrace::libunwind::trace
             at /rustc/x/library/std/src/../../backtrace/src/backtrace/libunwind.rs:117:9
   1: std::backtrace::Backtrace::force_capture
             at /rustc/x/library/std/src/backtrace.rs:312:9
   2: synaps_cli::panic_hook::last_words
             at ./src/panic_hook.rs:55:9
   3: <alloc::boxed::Box<F,A> as core::ops::function::Fn<Args>>::call
             at /rustc/x/library/alloc/src/boxed.rs:1999:9
   4: std::panicking::panic_with_hook
             at /rustc/x/library/std/src/panicking.rs:842:13
   5: core::panicking::panic_fmt
             at /rustc/x/library/core/src/panicking.rs:75:14
   6: agent_engine::session::actor::SessionActor::submit
             at ./crates/agent-engine/src/session/actor.rs:900:13
   7: tokio::runtime::task::core::Core<T,S>::poll
   8: std::panicking::try
";

    #[test]
    fn the_backtrace_is_one_line_starting_at_the_panicking_frame() {
        let line = compact_backtrace(SAMPLE, MAX_FRAMES);
        assert!(!line.contains('\n'), "{line}");
        assert_eq!(
            line,
            "0: agent_engine::session::actor::SessionActor::submit at \
             ./crates/agent-engine/src/session/actor.rs:900:13 | \
             1: tokio::runtime::task::core::Core<T,S>::poll | 2: std::panicking::try"
        );
    }

    #[test]
    fn the_backtrace_is_capped_and_says_how_much_was_cut() {
        let mut text = String::new();
        for i in 0..100 {
            text.push_str(&format!("  {i}: f{i}\n             at ./src/f.rs:{i}:1\n"));
        }
        let line = compact_backtrace(&text, MAX_FRAMES);
        assert_eq!(line.matches(" | ").count(), MAX_FRAMES, "{line}");
        assert!(line.starts_with("0: f0 at ./src/f.rs:0:1 | 1: f1"), "{line}");
        assert!(line.contains("39: f39 at ./src/f.rs:39:1 | … 60 more frames"), "{line}");
        assert!(!line.contains("f40"), "{line}");
    }

    #[test]
    fn a_stripped_or_disabled_backtrace_still_fits_one_line() {
        assert_eq!(compact_backtrace("disabled backtrace\n", MAX_FRAMES), "disabled backtrace");
        let stripped = "   0: <unknown>\n   1: <unknown>\n";
        assert_eq!(compact_backtrace(stripped, MAX_FRAMES), "0: <unknown> | 1: <unknown>");
    }

    #[test]
    fn payloads_of_every_kind_become_capped_text() {
        let r = Report::new("t", None, &"static str", "");
        assert_eq!(r.payload, "static str");
        assert_eq!(r.location, "<unknown>");
        let r = Report::new("t", Some("a.rs:1:2".into()), &String::from("owned\nline"), "");
        assert_eq!(r.payload, "owned\nline");
        let r = Report::new("t", None, &42u8, "");
        assert_eq!(r.payload, "Box<dyn Any>");
        let big = "é".repeat(MAX_PAYLOAD_BYTES);
        let r = Report::new("t", None, &big, "");
        assert!(r.payload.len() < MAX_PAYLOAD_BYTES + 32, "{}", r.payload.len());
        assert!(r.payload.ends_with(&format!("… ({} bytes)", big.len())), "{}", &r.payload[r.payload.len() - 30..]);
    }

    #[test]
    fn panics_from_the_logging_stack_are_recognised() {
        let at = |loc: &str, thread: &str| Report::new(thread, Some(loc.into()), &"x", "").from_logging_stack();
        assert!(at(
            "/home/u/.cargo/registry/src/index.crates.io-1/tracing-subscriber-0.3.23/src/fmt/mod.rs:1:1",
            "synaps-rt"
        ));
        assert!(at("/r/tracing-appender-0.2.5/src/worker.rs:1:1", "main"));
        assert!(at("src/main.rs:1:1", "tracing-appender"), "the writer thread itself");
        assert!(!at("crates/agent-engine/src/session/actor.rs:1:1", "synaps-rt"));
        assert!(!at("crates/agent-engine/src/tracing/mod.rs:1:1", "synaps-rt"), "a module named tracing is ours");
    }
}
