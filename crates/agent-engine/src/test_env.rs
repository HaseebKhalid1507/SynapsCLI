//! ONE architecture for process-environment mutation in engine tests
//! (final-Judge fix1 I2).
//!
//! `SYNAPS_BASE_DIR`, `HOME`, and `SYNAPS_ANTHROPIC_BASE_URL` are process
//! globals. Under `cargo test`'s default parallelism, any two tests that
//! read or write them race unless they hold the SAME `serial_test` lock —
//! an unkeyed `#[serial]` and a keyed `#[serial(synaps_base_dir)]` do NOT
//! exclude each other, and that exact split caused real workspace-parallel
//! failures (Copilot catalog credential leak, capture-sweep root race).
//!
//! Policy, enforced by review + the guards below living in one place:
//!
//! 1. every test that mutates one of these variables uses these RAII
//!    guards — never ad-hoc `set_var` pairs;
//! 2. every such test carries `#[serial_test::serial(synaps_base_dir)]`
//!    (ONE key for the whole family, since `HOME` and base-dir resolution
//!    feed the same config paths);
//! 3. production code must bind env-derived roots ONCE at construction
//!    (see `Runtime::capture_dir`) so post-construction behavior is immune
//!    to ambient churn even outside the lock.

use std::path::Path;
use std::sync::PoisonError;

/// Serializes every engine lib test that writes a script and then executes
/// it (28b review finding 7; was commit 2's mutex in `memory_backend/process.rs`).
/// Writing holds the script open for writing; a child that another test forks
/// meanwhile inherits that descriptor until its own exec closes it, and
/// executing the script in that window fails with ETXTBSY ("Axel service
/// could not start", 28b sa_1879 B1). Every participant holds this from
/// before its first script write until its last exec, so no participant
/// forks inside another's write window. Reentrant on one thread (one test may
/// build several fakes); blocking, so a test on a multi-thread runtime takes
/// it once, up front. Take it after every other guard (innermost lock).
/// Residual: a fork by a test outside this lock can still copy the descriptor.
pub(crate) static SCRIPT_EXEC_LOCK: ScriptExecLock = ScriptExecLock::new();

/// See [`SCRIPT_EXEC_LOCK`]: the holding thread and its depth.
pub(crate) struct ScriptExecLock {
    owner: std::sync::Mutex<Option<(std::thread::ThreadId, usize)>>,
    free: std::sync::Condvar,
}

impl ScriptExecLock {
    const fn new() -> Self {
        Self {
            owner: std::sync::Mutex::new(None),
            free: std::sync::Condvar::new(),
        }
    }

    /// Blocks until the calling thread holds the lock (at once if it already does).
    pub(crate) fn lock(&'static self) -> ScriptExecGuard {
        let me = std::thread::current().id();
        let owner = self.owner.lock().unwrap_or_else(PoisonError::into_inner);
        let mut owner = self
            .free
            .wait_while(owner, |o| matches!(o, Some((id, _)) if *id != me))
            .unwrap_or_else(PoisonError::into_inner);
        owner.get_or_insert((me, 0)).1 += 1;
        ScriptExecGuard { lock: self }
    }

    /// Whether the calling thread holds the lock (script-writing helpers assert it).
    pub(crate) fn held(&self) -> bool {
        let me = std::thread::current().id();
        let owner = self.owner.lock().unwrap_or_else(PoisonError::into_inner);
        matches!(*owner, Some((id, _)) if id == me)
    }
}

/// One hold of [`SCRIPT_EXEC_LOCK`]; the lock is free once every hold drops.
#[must_use = "the script lock is released when the guard drops"]
pub(crate) struct ScriptExecGuard {
    lock: &'static ScriptExecLock,
}

impl Drop for ScriptExecGuard {
    fn drop(&mut self) {
        let mut owner = self.lock.owner.lock().unwrap_or_else(PoisonError::into_inner);
        let released = match owner.as_mut() {
            Some((_, depth)) => {
                *depth -= 1;
                *depth == 0
            }
            None => false,
        };
        if released {
            *owner = None;
            self.lock.free.notify_all();
        }
    }
}

/// RAII guard: point `SYNAPS_BASE_DIR` at a fresh private temp dir and
/// restore the previous value on drop (panic-safe), byte for byte.
pub(crate) struct BaseDirGuard {
    old: Option<std::ffi::OsString>,
    _dir: tempfile::TempDir,
}

impl BaseDirGuard {
    pub(crate) fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let old = std::env::var_os("SYNAPS_BASE_DIR");
        agent_core::config::set_base_dir_for_tests(dir.path().to_path_buf());
        Self { old, _dir: dir }
    }

    pub(crate) fn path(&self) -> &Path {
        self._dir.path()
    }
}

impl Drop for BaseDirGuard {
    fn drop(&mut self) {
        match self.old.take() {
            Some(v) => std::env::set_var("SYNAPS_BASE_DIR", v),
            None => std::env::remove_var("SYNAPS_BASE_DIR"),
        }
    }
}

/// RAII guard for any other process env var (set or unset → restore on
/// drop). The previous value is kept as an `OsString`, so a value that is
/// not UTF-8 (a valid Unix path can be) is restored byte for byte.
pub(crate) struct EnvVarGuard {
    key: &'static str,
    old: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    pub(crate) fn set(key: &'static str, value: &str) -> Self {
        let old = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, old }
    }

    /// Unset `key` for the guard's lifetime.
    pub(crate) fn remove(key: &'static str) -> Self {
        let old = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, old }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.old.take() {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

/// RAII: no provider credential or endpoint can come from the environment.
/// Clears `ANTHROPIC_API_KEY` and the remote-broker pair
/// (`SYNAPS_AUTH_ENDPOINT` + `SYNAPS_MACHINE_TOKEN`), and points the Anthropic
/// base URL at the discard port, so a test that runs a real worker can't
/// reach a live provider or broker from a developer shell. Pair it with
/// [`BaseDirGuard`] (no `auth.json`) and the same
/// `#[serial_test::serial(synaps_base_dir)]` key.
pub(crate) struct OfflineProviderEnv {
    _guards: [EnvVarGuard; 4],
}

impl OfflineProviderEnv {
    pub(crate) fn new() -> Self {
        Self {
            _guards: [
                EnvVarGuard::remove("ANTHROPIC_API_KEY"),
                EnvVarGuard::remove("SYNAPS_AUTH_ENDPOINT"),
                EnvVarGuard::remove("SYNAPS_MACHINE_TOKEN"),
                EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", "http://127.0.0.1:9"),
            ],
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::EnvVarGuard;
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::OsStrExt;

    const KEY: &str = "SYNAPS_TEST_ENV_GUARD_NON_UTF8";

    /// A valid Unix path need not be UTF-8 (0xff here).
    fn non_utf8() -> OsString {
        OsStr::from_bytes(b"/var/run/syn\xffaps").to_os_string()
    }

    /// Removing a variable whose value is not UTF-8 restores the exact bytes.
    #[test]
    #[serial_test::serial(synaps_base_dir)]
    fn remove_restores_a_non_utf8_value() {
        std::env::set_var(KEY, non_utf8());
        {
            let _guard = EnvVarGuard::remove(KEY);
            assert_eq!(std::env::var_os(KEY), None);
        }
        assert_eq!(std::env::var_os(KEY), Some(non_utf8()));
        std::env::remove_var(KEY);
    }

    /// A base dir that is not UTF-8 is restored exactly when the guard drops.
    #[test]
    #[serial_test::serial(synaps_base_dir)]
    fn the_base_dir_guard_restores_a_non_utf8_value() {
        let outer = std::env::var_os("SYNAPS_BASE_DIR");
        std::env::set_var("SYNAPS_BASE_DIR", non_utf8());
        {
            let guard = super::BaseDirGuard::new();
            assert_eq!(
                std::env::var_os("SYNAPS_BASE_DIR").as_deref(),
                Some(guard.path().as_os_str())
            );
        }
        let restored = std::env::var_os("SYNAPS_BASE_DIR");
        match outer {
            Some(v) => std::env::set_var("SYNAPS_BASE_DIR", v),
            None => std::env::remove_var("SYNAPS_BASE_DIR"),
        }
        assert_eq!(restored, Some(non_utf8()));
    }

    /// The script lock nests on one thread (several fakes in one test) and
    /// keeps every other thread out until the last hold drops.
    #[test]
    fn the_script_lock_nests_on_one_thread_and_excludes_others() {
        use super::SCRIPT_EXEC_LOCK;
        let outer = SCRIPT_EXEC_LOCK.lock();
        let inner = SCRIPT_EXEC_LOCK.lock();
        assert!(SCRIPT_EXEC_LOCK.held());
        let (tx, rx) = std::sync::mpsc::channel();
        let other = std::thread::spawn(move || {
            assert!(!SCRIPT_EXEC_LOCK.held());
            let _held = SCRIPT_EXEC_LOCK.lock();
            tx.send(SCRIPT_EXEC_LOCK.held()).unwrap();
        });
        drop(inner);
        assert!(SCRIPT_EXEC_LOCK.held());
        let waited = rx.recv_timeout(std::time::Duration::from_millis(300));
        assert!(waited.is_err(), "another thread took a held lock");
        drop(outer);
        assert!(!SCRIPT_EXEC_LOCK.held());
        let took = rx.recv_timeout(std::time::Duration::from_secs(120));
        assert_eq!(took, Ok(true));
        other.join().unwrap();
    }

    /// Setting over a value that is not UTF-8 restores the exact bytes.
    #[test]
    #[serial_test::serial(synaps_base_dir)]
    fn set_restores_a_non_utf8_value() {
        std::env::set_var(KEY, non_utf8());
        {
            let _guard = EnvVarGuard::set(KEY, "plain");
            assert_eq!(std::env::var_os(KEY), Some(OsString::from("plain")));
        }
        assert_eq!(std::env::var_os(KEY), Some(non_utf8()));
        std::env::remove_var(KEY);
    }
}
