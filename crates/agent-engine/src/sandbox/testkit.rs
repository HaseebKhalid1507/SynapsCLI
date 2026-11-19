//! Fixtures for tests that need a resolved confinement without a provider
//! plugin or a kernel: a scratch tree with a home, an engine base dir and a
//! workspace, and a confinement resolved around the workspace by the real
//! [`resolve`] (so the tests exercise the product's policy, not a copy).
//!
//! The tree lives under `/var/tmp`, not `/tmp`: the baseline grants `/tmp`
//! whole, so a "not allowed" path under it would be allowed.

use std::path::PathBuf;
use std::sync::Arc;

use super::policy::{canon_lossy, resolve, Answer, PolicyEnv, Protected, ProviderOutcome, ResolveInput};
use super::{Confinement, SessionSandbox};

/// A scratch home / base dir / workspace.
pub(crate) struct Tree {
    _dir: tempfile::TempDir,
    pub root: PathBuf,
    pub home: PathBuf,
    pub base: PathBuf,
    pub ws: PathBuf,
}

impl Tree {
    pub fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("sandbox-pin-")
            .tempdir_in("/var/tmp")
            .expect("a scratch dir under /var/tmp");
        let root = canon_lossy(dir.path());
        let home = root.join("home");
        let base = home.join(".synaps-cli");
        let ws = home.join("proj");
        std::fs::create_dir_all(&base).expect("base dir");
        std::fs::create_dir_all(ws.join("src")).expect("workspace");
        Tree { _dir: dir, root, home, base, ws }
    }

    /// The environment the policy is computed from, with `path_dirs` on
    /// `$PATH`.
    pub fn env(&self, path_dirs: Vec<PathBuf>) -> PolicyEnv {
        let mut env = PolicyEnv::fixed(&self.home, &self.base);
        env.path_dirs = path_dirs;
        env
    }

    /// The confinement around the workspace, with `path_dirs` on `$PATH` and
    /// `extra_write` as the provider's `write` answer.
    pub fn confinement(&self, path_dirs: Vec<PathBuf>, extra_write: Vec<String>) -> Confinement {
        let env = self.env(path_dirs);
        let res = resolve(ResolveInput {
            confine_on: true,
            provider: Some("example-guard"),
            abi: Ok(7),
            outcome: ProviderOutcome::Answer(Answer { write: extra_write, read: vec![] }),
            workspace: &self.ws,
            env: &env,
        });
        assert!(res.refusal.is_none(), "the fixture must resolve: {:?}", res.refusal);
        Confinement::new(res, Protected::new(&env), &self.ws, Some(&self.ws))
    }

    /// [`Self::confinement`] as a session's sandbox.
    pub fn session(&self, path_dirs: Vec<PathBuf>, extra_write: Vec<String>) -> SessionSandbox {
        SessionSandbox::Confined(Arc::new(self.confinement(path_dirs, extra_write)))
    }

    /// A confined session whose ruleset can no longer be built: its
    /// workspace holds a protected `$PATH` folder and is then removed, so it
    /// cannot be split around that folder. Every spawn for it must be
    /// refused; none may run loose. The fixture checks that it does refuse.
    /// (Needs no Landlock kernel: the refusal comes before any kernel call.)
    pub fn session_that_cannot_spawn(&self) -> SessionSandbox {
        let bin = self.ws.join("bin");
        std::fs::create_dir_all(&bin).expect("a $PATH folder in the workspace");
        let session = self.session(vec![bin], vec![]);
        std::fs::remove_dir_all(&self.ws).expect("remove the workspace");
        assert!(
            matches!(session.for_spawn(), super::SpawnSandbox::Refuse(_)),
            "the fixture must refuse every spawn"
        );
        session
    }

    /// A script file beside the workspace (outside every grant, readable),
    /// for a spawn site that only takes a program and arguments.
    pub fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.root.join(name);
        std::fs::write(&path, body).expect("write the script");
        path
    }
}

/// Wait up to five seconds for `path` to appear.
pub(crate) async fn appears(path: &std::path::Path) -> bool {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < end {
        if path.exists() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    path.exists()
}

/// Whether this kernel can confine a child (Landlock ABI at or above the
/// minimum). Tests that start a real confined process skip, loudly, when it
/// can't.
pub(crate) fn kernel_ok(test: &str) -> bool {
    kernel_ok_with(test, super::kernel_abi(), require_landlock())
}

/// [`kernel_ok`] with the kernel's answer and the switch given.
fn kernel_ok_with(test: &str, abi: Result<u32, ()>, required: bool) -> bool {
    match abi {
        Ok(abi) => {
            eprintln!("{test}: Landlock ABI {abi}");
            true
        }
        Err(()) => {
            if required {
                panic!("{test}: SYNAPS_REQUIRE_LANDLOCK=1, but this kernel has no usable Landlock");
            }
            eprintln!("SKIPPED {test}: this kernel has no usable Landlock");
            false
        }
    }
}

/// `SYNAPS_REQUIRE_LANDLOCK=1`: a real-kernel test fails instead of skipping
/// when the kernel has no usable Landlock (CI sets it).
pub(crate) fn require_landlock() -> bool {
    require_landlock_in(std::env::var_os("SYNAPS_REQUIRE_LANDLOCK"))
}

fn require_landlock_in(value: Option<std::ffi::OsString>) -> bool {
    value.is_some_and(|v| v == "1")
}

#[test]
fn a_kernel_without_landlock_fails_a_real_kernel_test_when_landlock_is_required() {
    assert!(kernel_ok_with("t", Ok(6), true) && kernel_ok_with("t", Ok(6), false));
    assert!(!kernel_ok_with("t", Err(()), false), "not required: the test skips");
    let required = std::panic::catch_unwind(|| kernel_ok_with("t", Err(()), true));
    assert!(required.is_err(), "required: the test fails instead of skipping");
    assert!(require_landlock_in(Some("1".into())));
    assert!(!require_landlock_in(Some("0".into())) && !require_landlock_in(Some("".into())) && !require_landlock_in(None));
}
