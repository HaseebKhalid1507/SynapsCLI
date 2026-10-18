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
