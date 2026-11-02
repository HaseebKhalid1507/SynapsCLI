//! The sandbox's log lines reach the daemon log: the daemon's own filter
//! (`agent_core::logging::default_env_filter` with `RUST_LOG` unset, exactly
//! what `init_logging` builds) keeps them. No kernel support
//! needed. [`daemon_log`] is shared with the bash tool's sandbox tests.

use std::io;
use std::sync::{Arc, Mutex};

/// A writer for the subscriber that keeps everything written to it.
#[derive(Clone, Default)]
pub(crate) struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    /// Everything written so far.
    pub(crate) fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()).expect("utf-8 log")
    }
}

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// A subscriber with the daemon log's filter, writing to `out`.
fn daemon_subscriber(out: Capture) -> impl tracing::Subscriber + Send + Sync {
    // `RUST_LOG` unset: the filter `init_logging` builds then.
    tracing_subscriber::fmt()
        .with_env_filter(agent_core::logging::default_env_filter(None))
        .with_writer(out)
        .with_ansi(false)
        .finish()
}

/// What the daemon log would keep of the lines logged on THIS thread until
/// the guard drops. A test on a current-thread runtime logs on its own
/// thread, so it sees the lines its tool calls log. (The bash tool's and
/// the PTY shell's tests use it.)
pub(crate) fn daemon_log() -> (Capture, tracing::subscriber::DefaultGuard) {
    let out = Capture::default();
    let guard = tracing::subscriber::set_default(daemon_subscriber(out.clone()));
    (out, guard)
}

#[test]
fn the_daemon_log_filter_keeps_sandbox_lines() {
    let out = Capture::default();
    tracing::subscriber::with_default(daemon_subscriber(out.clone()), || {
        super::log_line("sandbox.test session=s1 result=kept");
        tracing::warn!(target: "unrelated_crate::x", "a warning outside the crate directives");
    });
    let text = out.text();
    assert!(text.contains("sandbox.test session=s1 result=kept"), "the sandbox line reaches the log: {text:?}");
    assert!(text.contains("WARN"), "at WARN: {text:?}");
    // The control: the filter really drops what it should, so the first
    // assertion isn't passing on a filter that keeps everything.
    assert!(!text.contains("a warning outside the crate directives"), "{text:?}");
}

#[test]
fn the_daemon_log_filter_keeps_sandbox_info_lines() {
    let out = Capture::default();
    tracing::subscriber::with_default(daemon_subscriber(out.clone()), || {
        super::log_info("sandbox.audit unattributed domains=1 records=2");
        tracing::info!(target: "unrelated_crate::x", "an info line outside the crate directives");
    });
    let text = out.text();
    assert!(text.contains(" INFO ") && text.contains("sandbox.audit unattributed domains=1 records=2"), "{text:?}");
    assert!(!text.contains("an info line outside the crate directives"), "{text:?}");
}

/// The audit reader's daemon sink: its WARN lines and INFO lines both reach
/// the daemon log.
#[test]
fn the_audit_readers_daemon_sink_reaches_the_daemon_log() {
    let out = Capture::default();
    let sink = super::audit::Config::daemon().sink;
    tracing::subscriber::with_default(daemon_subscriber(out.clone()), || {
        sink(super::audit::Level::Warn, "sandbox.denial session=s1 tool=bash tag=sbx-1 op=create class=other path=/x n=1 src=audit");
        sink(super::audit::Level::Info, "sandbox.audit dropped malformed=1 long=0");
    });
    let text = out.text();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.iter().any(|l| l.contains(" WARN ") && l.contains("sandbox.denial session=s1")), "{text:?}");
    assert!(lines.iter().any(|l| l.contains(" INFO ") && l.contains("sandbox.audit dropped malformed=1 long=0")), "{text:?}");
}
