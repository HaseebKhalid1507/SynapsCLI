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
//! only: reads, the network and process control are not restricted.

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
}

impl SpawnPlan {
    /// Make `cmd`'s child restrict itself before exec. Add this after any
    /// other `pre_exec` hooks (`setsid`, …). If the restrict fails the spawn
    /// fails; the child never runs unconfined.
    #[cfg(target_os = "linux")]
    pub fn install(&self, cmd: &mut std::process::Command) -> std::io::Result<()> {
        landlock::confine_command(cmd, &self.ruleset)
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
                    match c.build_ruleset() {
                        Ok(ruleset) => SpawnSandbox::Apply(SpawnPlan { ruleset }),
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
}
