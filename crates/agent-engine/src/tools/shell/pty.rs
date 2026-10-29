//! PTY abstraction — spawn processes on a pseudo-terminal, async read/write.
//!
//! Wraps `portable-pty` to provide an async-friendly handle with:
//! - Spawning commands on a PTY (with cwd, env, size)
//! - Non-blocking reads via a background reader thread + mpsc channel
//! - Synchronous writes to the PTY master
//! - Resize support
//! - Alive-check and graceful cleanup on Drop

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::{Result, RuntimeError};

const PTY_OUTPUT_CHANNEL_CAPACITY: usize = 64;
const PTY_OUTPUT_MAX_CHUNK_BYTES: usize = 4096;
static PTY_PRODUCED_BYTES: AtomicU64 = AtomicU64::new(0);
static PTY_ACCEPTED_BYTES: AtomicU64 = AtomicU64::new(0);
static PTY_CONSUMED_BYTES: AtomicU64 = AtomicU64::new(0);
static PTY_DROPPED_BYTES: AtomicU64 = AtomicU64::new(0);
static PTY_ACTIVE_READERS: AtomicU64 = AtomicU64::new(0);

/// Test-only preemption seam (fix1 I2c): milliseconds the reader thread
/// sleeps between a successful chunk handoff and the accounting update
/// that follows — deterministically reproducing the scheduler preemption
/// that let a Drop run in that gap and leak the chunk's accounting.
#[cfg(test)]
static TEST_PAUSE_AFTER_SEND_MS: AtomicU64 = AtomicU64::new(0);

/// Per-handle output accounting with EXACT-ONCE final release (fix1 I2c).
///
/// Two teardown parties share this state: the blocking reader thread and
/// the `PtyHandle`. Each calls [`PtyAccounting::finish_party`] exactly once
/// when it is done mutating its counters (the reader after its final loop
/// iteration — panic-safe via a Drop guard; the handle at the top of its
/// own Drop, after which `consumed` can never grow because draining needs
/// `&mut self`). Whichever party finishes LAST observes final counters and
/// releases the remainder (`produced − consumed − dropped`) into the
/// global dropped gauge exactly once — so no interleaving of a Drop with
/// an in-flight handoff can leak retained bytes.
struct PtyAccounting {
    produced: AtomicU64,
    consumed: AtomicU64,
    dropped: AtomicU64,
    /// Countdown of unfinished parties (reader thread + handle).
    parties: AtomicU64,
}

impl PtyAccounting {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            produced: AtomicU64::new(0),
            consumed: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            parties: AtomicU64::new(2),
        })
    }

    fn add_produced(&self, n: u64) {
        self.produced.fetch_add(n, Ordering::Relaxed);
        PTY_PRODUCED_BYTES.fetch_add(n, Ordering::Relaxed);
    }

    fn add_consumed(&self, n: u64) {
        self.consumed.fetch_add(n, Ordering::Relaxed);
        PTY_CONSUMED_BYTES.fetch_add(n, Ordering::Relaxed);
    }

    fn add_dropped(&self, n: u64) {
        self.dropped.fetch_add(n, Ordering::Relaxed);
        PTY_DROPPED_BYTES.fetch_add(n, Ordering::Relaxed);
    }

    /// Both parties (reader thread and handle) have finished and the books
    /// balance: every byte this handle produced was consumed or released as
    /// dropped. Per-handle, so it cannot be skewed by other live PTYs.
    #[cfg(test)]
    fn released(&self) -> bool {
        self.parties.load(Ordering::Acquire) == 0
            && self.produced.load(Ordering::Acquire)
                == self.consumed.load(Ordering::Acquire) + self.dropped.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn retained(&self) -> u64 {
        let produced = self.produced.load(Ordering::Acquire);
        let consumed = self.consumed.load(Ordering::Acquire);
        let dropped = self.dropped.load(Ordering::Acquire);
        produced.saturating_sub(consumed).saturating_sub(dropped)
    }

    /// Mark one party finished. The LAST party (AcqRel countdown makes the
    /// other party's counter updates visible) releases the unaccounted
    /// remainder exactly once.
    fn finish_party(&self) {
        if self.parties.fetch_sub(1, Ordering::AcqRel) == 1 {
            let produced = self.produced.load(Ordering::Acquire);
            let consumed = self.consumed.load(Ordering::Acquire);
            let dropped = self.dropped.load(Ordering::Acquire);
            let remainder = produced.saturating_sub(consumed).saturating_sub(dropped);
            if remainder > 0 {
                self.add_dropped(remainder);
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PtyOutputSnapshot {
    pub produced_bytes: u64,
    /// Chunks/bytes accepted into the bounded handoff. This is not consumer
    /// delivery; queued bytes remain retained until `try_read_output` drains.
    pub accepted_bytes: u64,
    pub dropped_bytes: u64,
    pub retained_bytes: u64,
    pub active_readers: u64,
}

pub fn pty_output_snapshot() -> PtyOutputSnapshot {
    let produced = PTY_PRODUCED_BYTES.load(Ordering::Relaxed);
    let accepted = PTY_ACCEPTED_BYTES.load(Ordering::Relaxed);
    let consumed = PTY_CONSUMED_BYTES.load(Ordering::Relaxed);
    let dropped = PTY_DROPPED_BYTES.load(Ordering::Relaxed);
    PtyOutputSnapshot {
        produced_bytes: produced,
        accepted_bytes: accepted,
        dropped_bytes: dropped,
        retained_bytes: produced.saturating_sub(consumed).saturating_sub(dropped),
        active_readers: PTY_ACTIVE_READERS.load(Ordering::SeqCst),
    }
}

/// Async-friendly wrapper around a PTY master/child pair.
///
/// The reader runs on a blocking Tokio thread and pushes raw byte chunks
/// into an unbounded mpsc channel. Consumers drain the channel via
/// `try_read_output()`.
pub struct PtyHandle {
    /// Master PTY — retained for resize operations.
    master: Box<dyn MasterPty + Send>,
    /// Writer end of the PTY master (bytes written here reach the child's stdin).
    writer: Box<dyn Write + Send>,
    /// Handle to the blocking reader task (for cleanup tracking).
    _reader_task: JoinHandle<()>,
    /// Receiving end of the output channel fed by the reader task.
    output_rx: mpsc::Receiver<Vec<u8>>,
    /// Per-handle accounting; the remainder is released EXACTLY ONCE by
    /// the last of {reader thread, handle drop} (see [`PtyAccounting`]).
    accounting: Arc<PtyAccounting>,
    /// Child process handle — used for try_wait / kill.
    child: Box<dyn Child + Send + Sync>,
    /// Cached alive flag — once the child exits, this stays false.
    alive: Arc<AtomicBool>,
    /// Separate killer handle so Drop can kill even if child is borrowed.
    killer: Box<dyn ChildKiller + Send + Sync>,
}

type PreparedScope = (
    Vec<(std::ffi::OsString, std::ffi::OsString)>,
    Option<std::path::PathBuf>,
    Option<crate::process_scope::ScopeLauncher>,
);

impl PtyHandle {
    /// Spawn a command on a new PTY.
    ///
    /// # Arguments
    /// * `command` — the program (and optional arguments) to run, e.g. `"bash"` or `"ssh user@host"`.
    /// * `working_dir` — optional working directory for the child process.
    /// * `env` — additional environment variables (merged on top of inherited env).
    /// * `rows` / `cols` — initial terminal dimensions.
    pub fn spawn(
        command: &str,
        working_dir: Option<&str>,
        env: HashMap<String, String>,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        let prepared = Self::prepare_scope(working_dir, &env);
        Self::spawn_prepared(command, working_dir, rows, cols, prepared)
    }

    pub(crate) fn prepare_scope(working_dir: Option<&str>, env: &HashMap<String, String>) -> PreparedScope {
        // Snapshot after TERM and user overrides; probe and child see exactly
        // the same environment. This synchronous API may probe for up to 3 s.
        let mut effective_env = crate::process_scope::inherited_env();
        crate::process_scope::set_env(&mut effective_env, "TERM", "xterm-256color");
        for (k, v) in env { crate::process_scope::set_env(&mut effective_env, k, v); }
        let scope_cwd = working_dir.map(std::path::PathBuf::from)
            .filter(|p| p.is_dir())
            .or_else(|| effective_env.iter().find(|(k, _)| k == "HOME")
                .map(|(_, v)| std::path::PathBuf::from(v)));
        let launcher = scope_cwd.as_deref().and_then(|cwd|
            crate::process_scope::launcher_for_effective_env(&effective_env, Some(cwd)));
        (effective_env, scope_cwd, launcher)
    }

    pub(crate) fn spawn_prepared(
        command: &str, working_dir: Option<&str>, rows: u16, cols: u16,
        prepared: PreparedScope,
    ) -> Result<Self> {
        // 1. Open a PTY pair with the requested size.
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| RuntimeError::Tool(format!("Failed to open PTY: {e}")))?;

        // 2. Build the command.
        //    We split on whitespace for simple cases ("bash -l", "ssh user@host").
        let parts: Vec<&str> = command.split_whitespace().collect();
        let program = parts
            .first()
            .ok_or_else(|| RuntimeError::Tool("Empty command string".to_string()))?;
        let (effective_env, scope_cwd, launcher) = prepared;
        let (program, args): (std::ffi::OsString, Vec<std::ffi::OsString>) = {
            let program: std::ffi::OsString = (*program).into();
            let args = parts
                .iter()
                .skip(1)
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>();
            match launcher {
                Some(launcher) => launcher.wrap("synaps tool: shell", crate::process_scope::resolve_program(program, &effective_env, scope_cwd.as_deref()).map_err(|e| RuntimeError::Tool(e.to_string()))?, args),
                None => (program, args),
            }
        };
        let mut cmd = CommandBuilder::new(program);
        for arg in &args {
            cmd.arg(arg);
        }

        // Set working directory if provided.
        if let Some(dir) = working_dir {
            cmd.cwd(dir);
        }

        cmd.env_clear();
        for (k, v) in &effective_env { cmd.env(k, v); }

        // 3. Spawn the child on the slave side.
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| RuntimeError::Tool(format!("Failed to spawn command: {e}")))?;

        // Drop the slave — the child process owns its end now.
        drop(pair.slave);

        // 4. Obtain writer and reader from the master.
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| RuntimeError::Tool(format!("Failed to take PTY writer: {e}")))?;

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| RuntimeError::Tool(format!("Failed to clone PTY reader: {e}")))?;

        // 5. Spawn a blocking reader task that pushes chunks into the channel.
        let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>(PTY_OUTPUT_CHANNEL_CAPACITY);
        let accounting = PtyAccounting::new();
        let reader_accounting = Arc::clone(&accounting);
        let alive = Arc::new(AtomicBool::new(true));
        let reader_alive = alive.clone();

        PTY_ACTIVE_READERS.fetch_add(1, Ordering::SeqCst);
        let reader_task = tokio::task::spawn_blocking(move || {
            /// Runs on EVERY reader exit path (return or panic): finishes
            /// the reader's accounting party AFTER its final counter
            /// updates, then drops the active-reader gauge.
            struct ReaderGauge(Arc<PtyAccounting>);
            impl Drop for ReaderGauge {
                fn drop(&mut self) {
                    self.0.finish_party();
                    PTY_ACTIVE_READERS.fetch_sub(1, Ordering::SeqCst);
                }
            }
            let _gauge = ReaderGauge(Arc::clone(&reader_accounting));
            let mut buf = [0u8; PTY_OUTPUT_MAX_CHUNK_BYTES];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        // EOF — child closed its side.
                        break;
                    }
                    Ok(n) => {
                        reader_accounting.add_produced(n as u64);
                        match output_tx.blocking_send(buf[..n].to_vec()) {
                            Ok(()) => {
                                #[cfg(test)]
                                {
                                    let ms = TEST_PAUSE_AFTER_SEND_MS.load(Ordering::Relaxed);
                                    if ms > 0 {
                                        std::thread::sleep(Duration::from_millis(ms));
                                    }
                                }
                                PTY_ACCEPTED_BYTES.fetch_add(n as u64, Ordering::Relaxed);
                            }
                            Err(_) => {
                                reader_accounting.add_dropped(n as u64);
                                break;
                            }
                        }
                    }
                    Err(_) => {
                        // Read error (child exited, fd closed, etc.) — exit cleanly.
                        break;
                    }
                }
            }
            reader_alive.store(false, Ordering::SeqCst);
        });

        // 6. Clone a killer for Drop usage.
        let killer = child.clone_killer();

        Ok(PtyHandle {
            master: pair.master,
            writer,
            _reader_task: reader_task,
            output_rx,
            accounting,
            child,
            alive,
            killer,
        })
    }

    /// Write raw bytes to the PTY (reaches the child's stdin).
    pub fn write(&mut self, input: &[u8]) -> Result<()> {
        self.writer
            .write_all(input)
            .map_err(|e| RuntimeError::Tool(format!("PTY write failed: {e}")))?;
        self.writer
            .flush()
            .map_err(|e| RuntimeError::Tool(format!("PTY flush failed: {e}")))?;
        Ok(())
    }

    /// Read all available output from the PTY, waiting up to `timeout` for data.
    ///
    /// Behavior:
    /// 1. Drain everything currently in the channel (non-blocking).
    /// 2. If nothing was found, wait up to `timeout` for the first chunk.
    /// 3. After getting something (or timing out), drain any remaining buffered data.
    ///
    /// Returns an empty `Vec` if no data arrived within the timeout.
    pub async fn try_read_output(&mut self, timeout: Duration) -> Vec<u8> {
        let mut collected = Vec::new();

        // Phase 1: non-blocking drain of everything already queued.
        while let Ok(chunk) = self.output_rx.try_recv() {
            self.accounting.add_consumed(chunk.len() as u64);
            collected.extend_from_slice(&chunk);
        }

        // Phase 2: if we got nothing, wait up to `timeout` for at least one chunk.
        if collected.is_empty() {
            match tokio::time::timeout(timeout, self.output_rx.recv()).await {
                Ok(Some(chunk)) => {
                    self.accounting.add_consumed(chunk.len() as u64);
                    collected.extend_from_slice(&chunk);
                }
                Ok(None) | Err(_) => {
                    // Channel closed or timeout — return whatever we have (empty).
                    return collected;
                }
            }

            // Phase 3: drain any additional chunks that arrived while we waited.
            while let Ok(chunk) = self.output_rx.try_recv() {
                self.accounting.add_consumed(chunk.len() as u64);
                collected.extend_from_slice(&chunk);
            }
        }

        collected
    }

    /// Resize the PTY to new dimensions.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| RuntimeError::Tool(format!("PTY resize failed: {e}")))
    }

    /// Check whether the child process is still running.
    ///
    /// Once the child exits, subsequent calls return `false` without syscalls.
    pub fn is_alive(&mut self) -> bool {
        if !self.alive.load(Ordering::SeqCst) {
            return false;
        }
        match self.child.try_wait() {
            Ok(Some(_status)) => {
                // Child exited.
                self.alive.store(false, Ordering::SeqCst);
                false
            }
            Ok(None) => true,
            Err(_) => {
                // If we can't query, assume dead.
                self.alive.store(false, Ordering::SeqCst);
                false
            }
        }
    }
}

impl Drop for PtyHandle {
    fn drop(&mut self) {
        // Finish the HANDLE's accounting party (fix1 I2c). `consumed` can
        // never grow after this point (draining needs `&mut self`), so if
        // the reader has already finished, the release happens here with
        // final counters; if the reader is still running — even mid-handoff
        // — IT becomes the last party and releases after its own final
        // updates. Either way: exactly once, nothing leaked.
        self.accounting.finish_party();
        if self.alive.load(Ordering::SeqCst) {
            let _ = self.killer.kill();
            // Reap the child after kill — without a wait(), the SIGKILL'd
            // child lingers as a zombie until the parent process exits.
            // SIGKILL takes effect near-instantly; bound the reap attempts
            // so Drop can never hang on a pathological child.
            for _ in 0..5 {
                match self.child.try_wait() {
                    Ok(Some(_)) | Err(_) => break, // reaped (or unreapable)
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Keyed, not bare: a PTY spawned without a working dir starts in the
    // child's `$HOME`, so these tests read HOME and must exclude the tests
    // that repoint it (see `crate::test_env`). A bare `#[serial]` does not,
    // and a spawn racing a HOME swap failed with ENOENT.
    use serial_test::serial;
    use std::collections::HashMap;

    /// `try_read_output` returns after the first chunk, and a loaded machine
    /// can split even a short echo across chunks; read until `needle` shows
    /// up or three seconds pass.
    async fn read_until(handle: &mut PtyHandle, needle: &str) -> String {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut out = Vec::new();
        while !String::from_utf8_lossy(&out).contains(needle)
            && tokio::time::Instant::now() < deadline
        {
            out.extend(handle.try_read_output(Duration::from_millis(200)).await);
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Wait for one handle's reader to exit and its accounting to be
    /// released exactly once. The process-wide gauges in
    /// `pty_output_snapshot` also count every PTY other tests have open
    /// (the shell session tests take no lock), so before/after deltas of
    /// them flake under load; a handle's own books do not.
    async fn wait_released(accounting: &PtyAccounting, what: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !accounting.released() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{what}: reader still running or bytes leaked \
                 (parties {}, retained {})",
                accounting.parties.load(Ordering::Acquire),
                accounting.retained()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A fast PTY producer with a deliberately stalled consumer is bounded
    /// at the reader-thread handoff, and dropping the handle releases it.
    /// A PTY shell gets its own systemd scope when the machine supports it
    /// (S348): a long-lived shell running a memory-hungry build is then the
    /// separate cgroup (not a memory cap or protection from ancestor OOM). It still has a working terminal.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[serial(synaps_base_dir)]
    async fn pty_shell_gets_its_own_scope_when_available() {
        if std::env::var("SYNAPS_TEST_REAL_SCOPES").as_deref() != Ok("1") { return; }
        let own = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        // Typed into an interactive shell, as the tool is used (the spawn
        // command line is whitespace-split, so no quoted -c script here).
        let mut handle = PtyHandle::spawn("bash --norc --noprofile", None, HashMap::new(), 24, 80)
            .expect("spawn");
        handle
            .write(b"cat /proc/$$/cgroup; test -t 0 && echo tty=yes; exit\n")
            .expect("write");
        let mut out = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline
            && !String::from_utf8_lossy(&out).contains("tty=yes\r")
        {
            out.extend(handle.try_read_output(Duration::from_millis(200)).await);
        }
        let accounting = Arc::clone(&handle.accounting);
        drop(handle);
        wait_released(&accounting, "scoped shell").await;
        let text = String::from_utf8_lossy(&out).replace('\r', "");
        assert!(
            text.contains("tty=yes"),
            "the shell must still have a terminal: {text}"
        );
        // A real terminal prefixes lines with control sequences (bracketed
        // paste etc.); take the cgroup path wherever it starts on its line.
        let child = text
            .lines()
            .filter(|l| !l.contains("cat /proc/")) // the echoed command line
            .find_map(|l| l.find("0::/").map(|i| l[i..].trim().to_string()))
            .unwrap_or_default();
        assert!(child.starts_with("0::/"), "no cgroup line in: {text:?}");
        if crate::process_scope::launcher_for_env(None).is_some() {
            assert_ne!(child, own.trim(), "{text}");
            assert!(child.ends_with(".scope"), "{child}");
        } else {
            assert_eq!(child, own.trim(), "unwrapped fallback");
        }
    }

    #[tokio::test]
    #[serial(synaps_base_dir)]
    async fn pty_slow_consumer_retention_is_bounded_and_drop_releases_reader() {
        let handle = PtyHandle::spawn(
            "bash -c 'yes x | head -c 10485760'",
            None,
            HashMap::new(),
            24,
            80,
        )
        .expect("spawn producer");
        let accounting = Arc::clone(&handle.accounting);
        // Nothing drains the handle, so the reader fills the channel and
        // blocks. Poll for that rather than assume a fixed sleep is enough
        // for the spawn under load.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while accounting.retained() == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "stalled consumer must retain queued PTY bytes"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Let the producer run on into the full channel, then check the bound.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let retained = accounting.retained();
        assert!(
            retained <= ((PTY_OUTPUT_CHANNEL_CAPACITY + 1) * PTY_OUTPUT_MAX_CHUNK_BYTES) as u64,
            "retained {retained}"
        );
        drop(handle);
        wait_released(&accounting, "drop must release the reader and its queued bytes").await;
    }

    /// fix1 I2c: dropping a handle at ANY point relative to the reader
    /// thread must conserve accounting exactly once — bytes accepted after
    /// Drop's snapshot were previously never released, leaking retained
    /// bytes. Repeated immediate drops of a flooding producer make that
    /// window land reliably.
    #[tokio::test]
    #[serial(synaps_base_dir)]
    async fn pty_drop_races_reader_and_always_conserves_accounting() {
        let mut released = Vec::new();
        for _ in 0..5 {
            let mut handle = PtyHandle::spawn(
                "bash -c 'yes x | head -c 33554432'",
                None,
                HashMap::new(),
                24,
                80,
            )
            .expect("spawn producer");
            // Drain some output so the channel has free capacity, then drop
            // while the producer is still mid-stream: the reader keeps
            // accepting chunks during Drop's kill/reap window — every one
            // of them must still be released exactly once.
            let _ = handle.try_read_output(Duration::from_millis(50)).await;
            released.push(Arc::clone(&handle.accounting));
            drop(handle);
        }
        // Every reader must finish and every handle's books must balance
        // EXACTLY: no leaked retained bytes from any drop/reader race.
        for accounting in &released {
            wait_released(accounting, "drop/reader race must conserve accounting").await;
        }
    }

    /// fix1 I2c (deterministic): the reader is paused IN the gap between
    /// a successful handoff and its accounting update while the handle is
    /// dropped — exactly the preemption the parallel suite produced. The
    /// chunk must still be released exactly once after the reader resumes.
    #[tokio::test]
    #[serial(synaps_base_dir)]
    async fn pty_preempted_accounting_across_drop_is_released_exactly_once() {
        TEST_PAUSE_AFTER_SEND_MS.store(150, Ordering::Relaxed);
        let handle = PtyHandle::spawn("bash -c 'echo leak-window'", None, HashMap::new(), 24, 80)
            .expect("spawn producer");
        let accounting = Arc::clone(&handle.accounting);
        // Let the reader reach the pause with one chunk handed off.
        tokio::time::sleep(Duration::from_millis(40)).await;
        drop(handle); // runs entirely inside the reader's paused gap
        TEST_PAUSE_AFTER_SEND_MS.store(0, Ordering::Relaxed);

        wait_released(&accounting, "a drop inside the accounting gap must not leak").await;
    }

    /// fix1 I2c: concurrent handles (spawned and dropped from parallel
    /// tasks at varied points) each release their remainder exactly once —
    /// per-handle scope, no cross-handle interference, exact global
    /// conservation after all readers exit.
    #[tokio::test]
    #[serial(synaps_base_dir)]
    async fn pty_concurrent_handles_release_exactly_once_per_handle() {
        let mut tasks = Vec::new();
        for i in 0..8 {
            tasks.push(tokio::spawn(async move {
                let mut handle = PtyHandle::spawn(
                    "bash -c 'yes x | head -c 262144'",
                    None,
                    HashMap::new(),
                    24,
                    80,
                )
                .expect("spawn producer");
                match i % 3 {
                    0 => {} // immediate drop
                    1 => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    _ => {
                        // Consume a little first, then drop mid-stream.
                        let _ = handle.try_read_output(Duration::from_millis(10)).await;
                    }
                }
                let accounting = Arc::clone(&handle.accounting);
                drop(handle);
                accounting
            }));
        }
        for t in tasks {
            let accounting = t.await.unwrap();
            wait_released(&accounting, "concurrent handles must each release exactly once").await;
        }
    }

    #[tokio::test]
    #[serial(synaps_base_dir)]
    async fn test_spawn_echo_hello() {
        let mut handle = PtyHandle::spawn("echo hello", None, HashMap::new(), 24, 80)
            .expect("failed to spawn echo");

        let text = read_until(&mut handle, "hello").await;
        assert!(
            text.contains("hello"),
            "expected 'hello' in output, got: {text:?}"
        );
    }

    #[tokio::test]
    #[serial(synaps_base_dir)]
    async fn test_cat_echo_back() {
        let mut handle =
            PtyHandle::spawn("cat", None, HashMap::new(), 24, 80).expect("failed to spawn cat");

        // Write input — cat will echo it back via the PTY.
        handle.write(b"test\n").expect("write failed");

        let text = read_until(&mut handle, "test").await;
        assert!(
            text.contains("test"),
            "expected 'test' in output, got: {text:?}"
        );
    }

    #[tokio::test]
    #[serial(synaps_base_dir)]
    async fn test_exit_code_detection() {
        let mut handle = PtyHandle::spawn("bash -c exit 42", None, HashMap::new(), 24, 80)
            .expect("failed to spawn bash exit");

        // Wait for the process to finish — read until EOF / timeout.
        let _ = handle.try_read_output(Duration::from_secs(3)).await;

        // Small additional delay to let try_wait catch up.
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(!handle.is_alive(), "expected process to have exited");
    }
}
