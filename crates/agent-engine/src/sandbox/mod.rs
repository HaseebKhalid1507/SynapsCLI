//! The per-session write sandbox (Linux).
//!
//! With `sandbox.confine = enforce` (set by hand in the config file) a
//! session's shell commands and file-writing tools can write only to the
//! session's workspace, the directories the configured policy provider adds
//! (a plugin answering the `sandbox_policy` hook) and a small baseline
//! (`/tmp`, `/dev/null`, …). The engine's own data, credential stores and
//! autostart locations are protected whatever the provider says.
//!
//! Two layers, one policy:
//!
//! * **Shell commands** run under a Landlock domain applied in the child
//!   before `exec` ([`landlock`]), so every process a command starts inherits
//!   it, whichever spawn path started it ([`SessionSandbox::for_spawn`]).
//! * **In-process writes** (`write`, `edit`, `multi_edit`) run in the engine,
//!   which is not confined, so they check the same policy on the opened
//!   directory object ([`guard`]).
//!
//! A session that cannot be confined (no usable kernel support, no valid
//! policy) never runs unconfined: it is refused at start, and a runtime whose
//! policy is unresolved refuses every command and write. v1 confines writes
//! only: reads and the network are not restricted. Process control is in
//! part: on Landlock ABI 6 and later a sandboxed process can't signal out of
//! its domain or reach an abstract unix socket made outside it, and the
//! daemon refuses a client from inside a sandbox ([`crate::daemon::peer`]).

pub mod check;
pub mod confinement;
pub mod policy;
pub mod provider;

#[cfg(target_os = "linux")]
pub mod landlock;

#[cfg(test)]
pub(crate) mod testkit;

#[cfg(unix)]
pub mod guard;

#[cfg(not(unix))]
pub mod guard {
    //! Placeholder on platforms without Landlock or `openat`: a confined
    //! session cannot exist there, so none of these is ever reached.
    use std::io;
    use std::path::Path;

    use super::confinement::Confinement;

    #[derive(Debug)]
    pub enum WriteError {
        Refused(String),
        Io(io::Error),
    }

    impl std::fmt::Display for WriteError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                WriteError::Refused(why) => f.write_str(why),
                WriteError::Io(e) => write!(f, "{e}"),
            }
        }
    }

    #[derive(Debug)]
    pub struct GuardedTarget(());

    impl GuardedTarget {
        pub fn real_path(&self) -> &Path {
            Path::new("")
        }
        pub fn exists(&self) -> bool {
            false
        }
        pub fn mode(&self) -> Option<u32> {
            None
        }
        pub fn read_to_string(&self) -> io::Result<String> {
            Err(io::ErrorKind::Unsupported.into())
        }
        pub fn replace(&self, _tmp: &str, _bytes: &[u8], _mode: Option<u32>) -> io::Result<()> {
            Err(io::ErrorKind::Unsupported.into())
        }
        pub fn remove(&self) -> io::Result<()> {
            Err(io::ErrorKind::Unsupported.into())
        }
    }

    pub fn open_target(_c: &Confinement, _p: &Path, _create: bool) -> Result<GuardedTarget, WriteError> {
        Err(WriteError::Refused("sandbox: not available on this platform".to_string()))
    }
}

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use confinement::Confinement;
pub use guard::{GuardedTarget, WriteError};

/// `Welcome.features` entry: this daemon can confine sessions through a
/// `sandbox_policy` provider (Linux builds only).
pub const FEATURE_SANDBOX_POLICY_V1: &str = "sandbox_policy_v1";

/// The kernel's usable Landlock ABI, `Err(())` when there is none (not
/// Linux, no Landlock, or older than the write-confinement minimum).
pub(crate) fn kernel_abi() -> Result<u32, ()> {
    #[cfg(target_os = "linux")]
    {
        landlock::usable_abi().map_err(|_| ())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(())
    }
}

/// A session's confinement state.
#[derive(Clone, Default)]
pub enum SessionSandbox {
    /// Confinement is off: today's behaviour.
    #[default]
    Off,
    /// Confinement is on but no policy has been resolved for this runtime
    /// (a host that never wired one). Every command and write is refused.
    Unresolved,
    /// Confined by a resolved policy.
    Confined(Arc<Confinement>),
}

impl std::fmt::Debug for SessionSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionSandbox::Off => f.write_str("Off"),
            SessionSandbox::Unresolved => f.write_str("Unresolved"),
            SessionSandbox::Confined(_) => f.write_str("Confined"),
        }
    }
}

const UNRESOLVED: &str = "sandbox: confinement is on but this session has no resolved policy, so nothing may run or write";

/// What a process spawn must do about the sandbox.
pub enum SpawnSandbox {
    /// Confinement is off: spawn as before.
    None,
    /// Spawn under this domain.
    Apply(SpawnPlan),
    /// Don't spawn; this is why.
    Refuse(String),
}

/// A Landlock ruleset ready to apply to one child.
pub struct SpawnPlan {
    #[cfg(target_os = "linux")]
    ruleset: landlock::Ruleset,
    /// The session's own temporary folder: the child's `TMPDIR`.
    tmp_dir: PathBuf,
}

impl SpawnPlan {
    /// Make `cmd`'s child restrict itself before exec. Add this after any
    /// other `pre_exec` hooks (`setsid`, …). If the restrict fails the spawn
    /// fails; the child never runs unconfined.
    /// The child's `TMPDIR` is set to the session's own temporary folder.
    #[cfg(target_os = "linux")]
    pub fn install(&self, cmd: &mut std::process::Command) -> std::io::Result<()> {
        cmd.env("TMPDIR", &self.tmp_dir);
        landlock::confine_command(cmd, &self.ruleset)
    }

    /// The session's own temporary folder, for a spawn that sets the child's
    /// environment itself (a PTY's command): set it as `TMPDIR`.
    pub fn tmp_dir(&self) -> &Path {
        &self.tmp_dir
    }

    #[cfg(not(target_os = "linux"))]
    pub fn install(&self, _cmd: &mut std::process::Command) -> std::io::Result<()> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "sandbox: not available on this platform"))
    }

    /// Run `f` (which spawns a child) on a dedicated thread that is under
    /// the domain; the child inherits it, no other thread is restricted.
    #[cfg(target_os = "linux")]
    pub fn run<T, F>(&self, f: F) -> std::io::Result<T>
    where
        T: Send,
        F: FnOnce() -> T + Send,
    {
        landlock::run_confined(&self.ruleset, f)
    }

    #[cfg(not(target_os = "linux"))]
    pub fn run<T, F>(&self, _f: F) -> std::io::Result<T>
    where
        T: Send,
        F: FnOnce() -> T + Send,
    {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "sandbox: not available on this platform"))
    }
}

impl SessionSandbox {
    /// The state a fresh runtime starts in: confined-pending when the
    /// config has confinement on (or unreadable), else off. A session start
    /// replaces it by the resolved policy; one that never does stays
    /// refusing.
    pub fn from_settings() -> Self {
        match agent_core::config::read_sandbox_settings().confine {
            agent_core::config::SandboxConfine::Off => SessionSandbox::Off,
            agent_core::config::SandboxConfine::Enforce => SessionSandbox::Unresolved,
        }
    }

    pub fn is_off(&self) -> bool {
        matches!(self, SessionSandbox::Off)
    }

    /// The number per-session process caches key this sandbox by: 0 when
    /// off, the confinement's own number when confined (never 0), and
    /// `u64::MAX` when unresolved. A process started under one sandbox is
    /// never handed to a caller under another.
    pub fn lease_tag(&self) -> u64 {
        match self {
            SessionSandbox::Off => 0,
            SessionSandbox::Unresolved => u64::MAX,
            SessionSandbox::Confined(c) => c.id(),
        }
    }

    /// The resolved confinement, when there is one.
    pub fn confinement(&self) -> Option<&Arc<Confinement>> {
        match self {
            SessionSandbox::Confined(c) => Some(c),
            _ => None,
        }
    }

    /// What a spawn must do. Called once per spawn: the ruleset is built
    /// fresh, so an exploded root picks up new top-level entries.
    pub fn for_spawn(&self) -> SpawnSandbox {
        match self {
            SessionSandbox::Off => SpawnSandbox::None,
            SessionSandbox::Unresolved => SpawnSandbox::Refuse(UNRESOLVED.to_string()),
            SessionSandbox::Confined(c) => {
                #[cfg(target_os = "linux")]
                {
                    match c.build_ruleset().and_then(|ruleset| Ok((ruleset, c.tmp_dir()?))) {
                        Ok((ruleset, tmp_dir)) => SpawnSandbox::Apply(SpawnPlan { ruleset, tmp_dir }),
                        Err(why) => SpawnSandbox::Refuse(why),
                    }
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = c;
                    SpawnSandbox::Refuse("sandbox: not available on this platform".to_string())
                }
            }
        }
    }

    /// Make `cmd`'s child start confined as this sandbox says (nothing to do
    /// when it is off). `Err` is the reason the spawn must not happen: the
    /// child never runs unconfined while confinement is on. Call it after
    /// any other `pre_exec` hook has been added.
    pub fn confine_std(&self, cmd: &mut std::process::Command) -> Result<(), String> {
        match self.for_spawn() {
            SpawnSandbox::None => Ok(()),
            SpawnSandbox::Refuse(why) => Err(why),
            SpawnSandbox::Apply(plan) => plan
                .install(cmd)
                .map_err(|e| format!("sandbox: cannot confine this process: {e}")),
        }
    }

    /// [`Self::confine_std`] for a tokio command.
    pub fn confine_tokio(&self, cmd: &mut tokio::process::Command) -> Result<(), String> {
        self.confine_std(cmd.as_std_mut())
    }

    /// Check and open `path` for an in-process write. `Ok(None)` when
    /// confinement is off (the tool writes as before); `Ok(Some(target))`
    /// when confined and allowed; the error says why not.
    pub fn guard_write(&self, path: &Path, create_parents: bool) -> Result<Option<GuardedTarget>, WriteError> {
        match self {
            SessionSandbox::Off => Ok(None),
            SessionSandbox::Unresolved => Err(WriteError::Refused(UNRESOLVED.to_string())),
            SessionSandbox::Confined(c) => guard::open_target(c, path, create_parents).map(Some),
        }
    }

    /// Whether the policy lets an in-process write land on `target`, a
    /// canonical path (symlinks already resolved). The caller must still
    /// open the file's parent by a no-follow walk and compare it with
    /// [`Self::check_opened_parent`].
    pub fn check_write_target(&self, target: &Path) -> Result<(), String> {
        match self {
            SessionSandbox::Off => Ok(()),
            SessionSandbox::Unresolved => Err(UNRESOLVED.to_string()),
            SessionSandbox::Confined(c) => c.allows_write(target),
        }
    }

    /// After a parent directory was opened for a checked `target`: the
    /// directory the kernel reports must be exactly the checked parent, so a
    /// component renamed in between can't redirect the write. Fails closed
    /// when the kernel can't say.
    pub fn check_opened_parent(&self, opened: std::io::Result<PathBuf>, target: &Path) -> Result<(), String> {
        if self.is_off() {
            return Ok(());
        }
        let expected = target.parent().unwrap_or(Path::new("/"));
        match opened {
            Ok(seen) if seen == expected => Ok(()),
            Ok(_) => Err("sandbox: write refused: the target's directory changed while it was being opened".to_string()),
            Err(e) => Err(format!("sandbox: write refused: cannot verify the target's directory ({e})")),
        }
    }

    /// Whether a command tool may start at all: `Err` carries the refusal.
    /// A confined session may run (under [`Self::for_spawn`]).
    pub fn check_can_spawn(&self) -> Result<(), String> {
        match self {
            SessionSandbox::Unresolved => Err(UNRESOLVED.to_string()),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_state_is_off_and_spawns_freely() {
        let s = SessionSandbox::default();
        assert!(s.is_off());
        assert!(matches!(s.for_spawn(), SpawnSandbox::None));
        assert!(s.guard_write(Path::new("/tmp/x"), false).unwrap().is_none());
    }

    #[test]
    fn an_unresolved_session_refuses_everything() {
        let s = SessionSandbox::Unresolved;
        assert!(matches!(s.for_spawn(), SpawnSandbox::Refuse(_)));
        assert!(s.check_can_spawn().is_err());
        assert!(matches!(s.guard_write(Path::new("/tmp/x"), false), Err(WriteError::Refused(_))));
    }

    #[test]
    fn the_checks_of_the_in_process_write_tools_fail_closed() {
        use crate::sandbox::testkit::Tree;
        let t = Tree::new();
        let off = SessionSandbox::Off;
        let unresolved = SessionSandbox::Unresolved;
        let confined = t.session(vec![], vec![]);
        let inside = t.ws.join("src/a.txt");
        let outside = t.home.join("out.txt");

        // Which target a write may land on.
        assert!(off.check_write_target(&outside).is_ok());
        assert!(unresolved.check_write_target(&inside).is_err());
        assert!(confined.check_write_target(&inside).is_ok());
        assert!(confined.check_write_target(&outside).is_err());
        assert!(confined.check_write_target(&t.base.join("config")).is_err());

        // The directory the kernel opened must be the one that was checked.
        let parent = inside.parent().unwrap().to_path_buf();
        assert!(off.check_opened_parent(Ok(t.home.clone()), &inside).is_ok(), "off checks nothing");
        assert!(confined.check_opened_parent(Ok(parent), &inside).is_ok());
        let why = confined.check_opened_parent(Ok(t.home.clone()), &inside).unwrap_err();
        assert!(why.contains("changed while it was being opened"), "{why}");
        let gone = std::io::Error::from(std::io::ErrorKind::NotFound);
        let why = confined.check_opened_parent(Err(gone), &inside).unwrap_err();
        assert!(why.contains("cannot verify"), "{why}");

        // Whether a command may start at all.
        assert!(off.check_can_spawn().is_ok());
        assert!(confined.check_can_spawn().is_ok());
        assert!(unresolved.check_can_spawn().is_err());
    }

    /// The state a fresh runtime starts in is decided by the owner's config
    /// alone, and every doubt means "refuse until a policy is installed".
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_fresh_runtime_is_off_without_the_switch_and_refusing_with_it_or_when_it_cannot_be_read() {
        let base = crate::test_env::BaseDirGuard::new();
        let config = base.path().join("config");

        assert!(SessionSandbox::from_settings().is_off(), "no config file: off");
        assert!(crate::Runtime::new().await.unwrap().sandbox().is_off());

        std::fs::write(&config, "sandbox.confine = enforce\n").unwrap();
        assert!(matches!(SessionSandbox::from_settings(), SessionSandbox::Unresolved));
        let rt = crate::Runtime::new().await.unwrap();
        assert!(matches!(rt.sandbox(), SessionSandbox::Unresolved), "a runtime nobody configured refuses everything");
        assert!(rt.sandbox().check_can_spawn().is_err());

        std::fs::write(&config, "sandbox.confine = off\n").unwrap();
        assert!(SessionSandbox::from_settings().is_off());

        // A config that exists but cannot be read as a file: the switch is
        // unknown, so confinement stays on.
        std::fs::remove_file(&config).unwrap();
        std::fs::create_dir(&config).unwrap();
        assert!(matches!(SessionSandbox::from_settings(), SessionSandbox::Unresolved));
    }
}
