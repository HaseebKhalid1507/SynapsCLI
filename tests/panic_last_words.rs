//! Last words (no daemon): the panic hook puts ONE ERROR line through the
//! daemon's real log filter (`default_env_filter(None)`, i.e. `RUST_LOG`
//! unset) before the default hook, for a plain thread and for a tokio task
//! — and a task's panic ends only that task (`panic = "unwind"`, the test
//! profile). A panic raised by the logging stack itself gets no tracing
//! line, so the hook cannot re-enter a broken logger, and returns.
//!
//! One global subscriber (writing into memory) for the whole binary; every
//! test looks only for its own unique payload.

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Install (once) the hook and a subscriber shaped like the log file's:
/// the daemon filter, no target, thread ids, no ANSI.
fn captured() -> &'static Captured {
    static CAPTURED: OnceLock<Captured> = OnceLock::new();
    CAPTURED.get_or_init(|| {
        let out = Captured::default();
        let writer = out.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(synaps_cli::logging::default_env_filter(None))
            .with_writer(move || writer.clone())
            .with_target(false)
            .with_thread_ids(true)
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one subscriber per binary");
        synaps_cli::panic_hook::install();
        out
    })
}

/// The captured lines that mention `needle`.
fn lines_with(needle: &str) -> Vec<String> {
    let bytes = captured().0.lock().unwrap_or_else(|e| e.into_inner()).clone();
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|l| l.contains(needle))
        .map(str::to_string)
        .collect()
}

#[test]
fn a_thread_panic_leaves_one_error_line_with_its_last_words() {
    captured();
    let payload = "last-words-thread-7f3a\nsecond line of the message";
    let joined = std::thread::Builder::new()
        .name("lw-worker".into())
        .spawn(move || panic!("{payload}"))
        .unwrap()
        .join();
    assert!(joined.is_err(), "the thread did not panic");
    let lines = lines_with("last-words-thread-7f3a");
    assert_eq!(lines.len(), 1, "want exactly one line: {lines:#?}");
    let line = &lines[0];
    assert!(line.contains(" ERROR "), "{line}");
    assert!(line.contains("PANIC"), "{line}");
    assert!(line.contains(r#"thread="lw-worker""#), "{line}");
    assert!(line.contains("location=tests/panic_last_words.rs:"), "{line}");
    assert!(
        line.contains(r#"payload="last-words-thread-7f3a\nsecond line of the message""#),
        "the payload is quoted, its newline escaped: {line}"
    );
    let backtrace = line.split("backtrace=").nth(1).expect("a backtrace field");
    assert!(backtrace.starts_with("\"0: "), "frames, even without RUST_BACKTRACE: {line}");
    assert!(backtrace.matches(" | ").count() <= synaps_cli::panic_hook::MAX_FRAMES, "{line}");
    assert!(!backtrace.contains("std::panicking::begin_panic"), "machinery frames lead: {line}");
}

#[test]
fn a_task_panic_is_logged_and_ends_only_that_task() {
    captured();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("synaps-rt")
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let doomed = tokio::spawn(async { panic!("last-words-task-91c2") });
        let err = doomed.await.expect_err("the task panics");
        assert!(err.is_panic());
        // The runtime and its other tasks carry on.
        let alive = tokio::spawn(async { 41 + 1 }).await.expect("another task still runs");
        assert_eq!(alive, 42);
    });
    let lines = lines_with("last-words-task-91c2");
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert!(lines[0].contains(r#"thread="synaps-rt""#), "{}", lines[0]);
}

#[test]
fn a_panic_from_the_log_writer_thread_gets_no_line_and_the_hook_returns() {
    captured();
    // tracing-appender's writer thread is named this: re-entering the
    // logger from its own panic could panic again (abort) or deadlock.
    let joined = std::thread::Builder::new()
        .name("tracing-appender".into())
        .spawn(|| panic!("last-words-writer-5d0e"))
        .unwrap()
        .join();
    assert!(joined.is_err());
    assert_eq!(lines_with("last-words-writer-5d0e"), Vec::<String>::new());
}
