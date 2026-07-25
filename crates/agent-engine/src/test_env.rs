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

/// RAII guard: point `SYNAPS_BASE_DIR` at a fresh private temp dir and
/// restore the previous value on drop (panic-safe).
pub(crate) struct BaseDirGuard {
    old: Option<String>,
    _dir: tempfile::TempDir,
}

impl BaseDirGuard {
    pub(crate) fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let old = std::env::var("SYNAPS_BASE_DIR").ok();
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

/// RAII guard for any other process env var (set → restore on drop).
pub(crate) struct EnvVarGuard {
    key: &'static str,
    old: Option<String>,
}

impl EnvVarGuard {
    pub(crate) fn set(key: &'static str, value: &str) -> Self {
        let old = std::env::var(key).ok();
        std::env::set_var(key, value);
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
