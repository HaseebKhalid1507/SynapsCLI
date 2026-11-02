//! Per-session write sandbox for agent bash/PTY children.
//!
//! Linux: a Landlock domain that handles every write right; reads and
//! execution stay unrestricted. Off unless a session has a policy.
//! `docs/sandbox.md` documents the policy file and the log lines.
//!
//! Off Linux there is no Landlock, and the sandbox fails closed: the policy
//! file, the kill switch, the spawn decision, `tools = confined` and the
//! 27c write checks are the same code as on Linux, but a spawn under an
//! enforcing policy is refused (as one that is `required` on a Linux host
//! without Landlock), never run unsandboxed. Without a policy file nothing
//! changes.
//!
//! Off Unix (Windows) a policy file is refused unread (its session's
//! spawns are refused) and 27c-checked writes fail closed; the Unix-only
//! steps are compiled out there (`nonunix` stands in for the file facts).
// Off Unix most of this tree is reached only from Unix code: it stays one
// code path, so its unused items are allowed there rather than gated singly.
#![cfg_attr(not(unix), allow(dead_code))]
pub mod carry;
pub mod confine;
mod decide;
pub mod landlock;
#[cfg(not(unix))]
mod nonunix;
pub mod policy;
pub mod protected;
#[cfg(target_os = "linux")]
pub mod audit;
#[cfg(target_os = "linux")]
pub mod spawn;
#[cfg(all(test, target_os = "linux"))]
pub(crate) mod log_tests;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// The tracing target of every `sandbox.*` log line. It is under
/// `agent_engine`, so the daemon log's filter (`agent_core::logging`,
/// `agent_engine=debug`) keeps the lines even with `RUST_LOG` unset; a
/// target outside Synaps's crates would be dropped below ERROR.
pub const LOG_TARGET: &str = "agent_engine::sandbox";

/// Log one fixed-shape sandbox line (WARN, [`LOG_TARGET`]): the one path
/// every `sandbox.*` line takes to the daemon log.
pub fn log_line(line: &str) {
    tracing::warn!(target: LOG_TARGET, "{}", line);
}

/// Log one fixed-shape sandbox line at INFO ([`LOG_TARGET`]): the audit
/// reader's counts (`sandbox.audit unattributed …`, `sandbox.audit dropped …`),
/// which are bookkeeping, not denials.
pub fn log_info(line: &str) {
    tracing::info!(target: LOG_TARGET, "{}", line);
}

/// Make this process the one with a kernel-audit reader (the daemon, at its
/// start, before any session). Sessions bound after this register their
/// enforcing spawns with it; in any other process there is no reader.
pub fn enable_audit() {
    #[cfg(target_os = "linux")]
    audit::enable_daemon();
}

/// Stop the kernel-audit reader's thread, if it started (daemon shutdown).
/// Never waits for it.
pub fn stop_audit() {
    #[cfg(target_os = "linux")]
    audit::stop_global();
}

/// [`SandboxBinding::gate_tool`] for a dispatch site's binding: every
/// production tool dispatch calls it after the execution gate has resolved
/// the call's exact runtime name, before any hook or the tool runs. A
/// context without a session binding confines nothing.
pub fn gate_tool(binding: Option<&SandboxBinding>, tool: &str) -> Result<(), String> {
    binding.map_or(Ok(()), |b| b.gate_tool(tool))
}

/// What the daemon's `sandbox_status` query reports for one session: the
/// state its next spawn and tool call would see. No path, contact or text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// `none`, `off`, `enforce` or `invalid`; `unavailable` for an
    /// enforcing policy on an OS without Landlock (every spawn refused).
    pub state: &'static str,
    pub kill_switch: bool,
    /// `open` or `confined`.
    pub tools: &'static str,
    pub policy_hash: Option<String>,
    pub rules: usize,
    pub skipped: usize,
    /// The usable Landlock ABI; `None` when unavailable.
    pub abi: Option<u32>,
}

/// [`Status`] for `session` in this process: its `$HOME` and Synaps base dir
/// (the daemon's own) and this kernel's Landlock ABI. Reads only (the
/// policy file and the kill switch, as a spawn does); logs nothing and
/// leaves the kill switch's logged state alone. Blocking.
pub fn status(session: &str) -> Status {
    status_with(&policy::PolicyEnv::from_process(), session, landlock::abi_version())
}

/// [`status`] against `env`, with `abi` as the kernel's answer.
pub fn status_with(env: &policy::PolicyEnv, session: &str, abi: Result<u32, landlock::Unavailable>) -> Status {
    // The switch is only looked at (no observation is recorded, so its
    // logged state, and the line a spawn logs for a change, stay as they
    // are); the file is read even while it is present.
    let kill_switch = policy::kill_switch_present(env);
    // No Landlock on this OS: an enforcing policy's spawns are all refused.
    let no_os_support = matches!(abi, Err(u) if u.always_refuses());
    let abi = policy::usable_abi(abi).ok();
    let none = Status { state: "none", kill_switch, tools: "open", policy_hash: None, rules: 0, skipped: 0, abi };
    let Some(file) = env.policy_path(session) else {
        return none;
    };
    match policy::load(&file, env) {
        Ok(None) => none,
        Ok(Some(p)) => Status {
            state: if no_os_support && p.mode() == policy::Mode::Enforce { "unavailable" } else { p.mode().as_str() },
            kill_switch,
            tools: p.tools().as_str(),
            policy_hash: Some(p.hash().to_string()),
            rules: p.rules().len(),
            skipped: p.skipped().len(),
            abi,
        },
        Err(e) => Status {
            state: "invalid",
            kill_switch,
            tools: if e.confined { policy::Tools::Confined.as_str() } else { policy::Tools::Open.as_str() },
            policy_hash: None,
            rules: 0,
            skipped: 0,
            abi,
        },
    }
}

/// What one spawn does about the sandbox.
pub enum SpawnSandbox {
    /// Spawn exactly as without a sandbox: no policy for the session, the
    /// kill switch is on, the policy says `mode = off`, or Landlock is
    /// unavailable on a Linux host and the policy doesn't require it.
    None,
    /// Restrict the child with this domain (boxed: it is far larger than
    /// the other variants).
    #[cfg(target_os = "linux")]
    Apply(Box<spawn::Prepared>),
    /// Refuse the spawn: nothing runs, and this text goes back to the agent.
    Refuse(String),
}

impl SpawnSandbox {
    /// The output check of a spawn this decision restricts (`Apply`), for a
    /// child whose output comes in parts after the spawn (the PTY shell);
    /// `None` for an unrestricted or refused spawn.
    pub fn output_check(&self) -> Option<OutputCheck> {
        #[cfg(target_os = "linux")]
        if let SpawnSandbox::Apply(p) = self {
            return Some(OutputCheck { ctx: p.ctx().clone() });
        }
        None
    }
}

/// Checks a restricted child's output as the `bash` tool checks a
/// command's: the same annotation, hint and `sandbox.*` lines, with the
/// spawn's session, tool and tag.
#[derive(Clone, Debug)]
pub struct OutputCheck {
    #[cfg(target_os = "linux")]
    ctx: spawn::SpawnCtx,
}

impl OutputCheck {
    /// `\n` + the hint for `output`, the output of `input` (the shell's
    /// command, or the text sent to it), if it shows a refused sudo or a
    /// refused write outside the rules, logging each line found
    /// ([`log_line`]); empty otherwise.
    pub fn hint(&self, input: &str, output: &str) -> String {
        #[cfg(target_os = "linux")]
        if let Some(a) = self.ctx.annotate(input, output) {
            for line in &a.lines {
                log_line(line);
            }
            if !a.hint.is_empty() {
                return format!("\n{}", a.hint);
            }
        }
        let _ = (input, output);
        String::new()
    }
}

/// A session's handle on its sandbox policy, carried in
/// `ToolCapabilities.sandbox`. It holds no policy: the policy file is read at
/// every spawn, so a change takes effect at the next one. It remembers what
/// it last logged, so the policy and state lines appear once per change, not
/// once per spawn.
pub struct SandboxBinding {
    session_id: String,
    env: policy::PolicyEnv,
    switch: &'static policy::KillSwitch,
    /// Test seam: the Landlock ABI to assume (`None` = ask the kernel).
    abi: Option<Result<u32, landlock::Unavailable>>,
    /// Fingerprint of the state last logged.
    logged: Mutex<Option<String>>,
    /// Test seam: the parent-side rule build fails (`BuildError::Rule`), as
    /// when the kernel refuses a rule, which no scratch tree can make it do.
    #[cfg(all(test, target_os = "linux"))]
    build_fails: bool,
    /// The kernel-audit reader its enforcing spawns register with (the
    /// daemon's one; `None` in unit tests and for scratch bindings, so no
    /// test opens the real audit log by accident).
    #[cfg(target_os = "linux")]
    audit: Option<Arc<audit::Reader>>,
}

impl std::fmt::Debug for SandboxBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxBinding").field("session_id", &self.session_id).finish_non_exhaustive()
    }
}

fn registry() -> &'static Mutex<HashMap<String, Arc<SandboxBinding>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<SandboxBinding>>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

impl SandboxBinding {
    /// The process-wide binding for a session id (`None` for a context
    /// without one). Policy lookups use this process's `$HOME` and Synaps
    /// base dir and the daemon's kill-switch state. A binding's id is fixed
    /// when it is made. An id that can't name a policy file
    /// (`policy::valid_session_id`) never has a policy, so its binding never
    /// enforces; nothing maps one id onto another session's binding.
    pub fn for_session(session_id: Option<&str>) -> Option<Arc<SandboxBinding>> {
        let id = session_id?;
        let mut map = registry().lock().unwrap_or_else(|p| p.into_inner());
        Some(Arc::clone(map.entry(id.to_string()).or_insert_with(|| Arc::new(SandboxBinding::new(id)))))
    }

    fn new(session_id: &str) -> Self {
        SandboxBinding {
            session_id: session_id.to_string(),
            env: policy::PolicyEnv::from_process(),
            switch: &policy::GLOBAL_KILL_SWITCH,
            abi: None,
            logged: Mutex::new(None),
            #[cfg(all(test, target_os = "linux"))]
            build_fails: false,
            #[cfg(target_os = "linux")]
            audit: audit::daemon_reader(),
        }
    }

    /// A binding with its own policy dir, kill-switch state and (optionally)
    /// an assumed ABI, outside the registry. For tests and tools that check a
    /// policy against a scratch base dir.
    pub fn with_env(
        session_id: &str,
        env: policy::PolicyEnv,
        switch: &'static policy::KillSwitch,
        abi: Option<Result<u32, landlock::Unavailable>>,
    ) -> Arc<SandboxBinding> {
        Arc::new(SandboxBinding {
            session_id: session_id.to_string(),
            env,
            switch,
            abi,
            logged: Mutex::new(None),
            #[cfg(all(test, target_os = "linux"))]
            build_fails: false,
            #[cfg(target_os = "linux")]
            audit: None,
        })
    }

    /// [`SandboxBinding::with_env`] whose enforcing spawns register with
    /// `reader` (a test's, on a scratch log).
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn with_audit(
        session_id: &str,
        env: policy::PolicyEnv,
        switch: &'static policy::KillSwitch,
        abi: Option<Result<u32, landlock::Unavailable>>,
        reader: Arc<audit::Reader>,
    ) -> Arc<SandboxBinding> {
        Arc::new(SandboxBinding {
            session_id: session_id.to_string(),
            env,
            switch,
            abi,
            logged: Mutex::new(None),
            build_fails: false,
            audit: Some(reader),
        })
    }

    /// [`SandboxBinding::with_env`] whose rule build always fails.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn with_failing_build(
        session_id: &str,
        env: policy::PolicyEnv,
        switch: &'static policy::KillSwitch,
        abi: Option<Result<u32, landlock::Unavailable>>,
    ) -> Arc<SandboxBinding> {
        Arc::new(SandboxBinding {
            session_id: session_id.to_string(),
            env,
            switch,
            abi,
            logged: Mutex::new(None),
            build_fails: true,
            audit: None,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Owner rule 27c's in-process check for this session's `write`, `edit`
    /// and `multi_edit` calls: `Some` when the session's policy enforces, or
    /// when its policy file exists but can't be used (fail closed, as its
    /// spawns are refused); `None` with no policy, with the kill switch, or
    /// with `mode = off`, and then those tools run exactly as before. Read
    /// at every call, like the spawn path; the Landlock ABI doesn't matter
    /// (the check needs no kernel support).
    ///
    /// With `tools = confined` the check also holds the target to the
    /// policy's write set ([`confine`]); a broken file that says `tools =
    /// confined` gets an empty write set (fail closed).
    pub fn tool_guard(&self) -> Option<protected::ToolGuard> {
        match policy::lookup_with(&self.env, &self.session_id, self.switch, &mut |line| log_line(line)) {
            policy::Lookup::Found(p) if p.mode() == policy::Mode::Enforce => {
                let guard = protected::ToolGuard::new(&self.env);
                Some(if p.confines_tools() { guard.confined(confine::write_dirs(&p), p.contact()) } else { guard })
            }
            policy::Lookup::Invalid(e) => {
                let guard = protected::ToolGuard::new(&self.env);
                Some(if e.confined { guard.confined(Vec::new(), &e.contact) } else { guard })
            }
            policy::Lookup::Found(_) | policy::Lookup::None | policy::Lookup::GlobalOff => None,
        }
    }

    /// `tools = confined`, for one call of `tool` (its exact runtime name),
    /// before any hook or the tool itself runs: `Err` with the text for the
    /// agent (the call is then refused, and one `sandbox.tool_refused` line
    /// logged) when the session's policy confines its tools and doesn't allow
    /// this one ([`confine::class_of`]). Read at every call, like the spawn
    /// path; the Landlock ABI doesn't matter. `Ok` for every session whose
    /// file doesn't say `tools = confined` (and with the kill switch).
    pub fn gate_tool(&self, tool: &str) -> Result<(), String> {
        let Some((named, contact)) = self.confinement() else {
            return Ok(());
        };
        match confine::class_of(tool, &named) {
            None => Ok(()),
            Some(class) => {
                log_line(&confine::refused_line(&self.session_id, tool, class));
                Err(confine::refusal(tool, class, &contact))
            }
        }
    }

    /// The session's tool list and contact sentence when its policy confines
    /// its tools: a valid `mode = enforce` + `tools = confined` file (its
    /// `tool =` names), or a file that can't be used but says `tools =
    /// confined` (no names: fail closed). `None` otherwise, and while the
    /// kill switch is present. The switch is only looked at, not observed:
    /// its transition lines stay the spawn path's, exactly as before.
    fn confinement(&self) -> Option<(Vec<String>, String)> {
        if policy::kill_switch_present(&self.env) {
            return None;
        }
        let file = self.env.policy_path(&self.session_id)?;
        match policy::load(&file, &self.env) {
            Ok(Some(p)) if p.confines_tools() => Some((p.named_tools().to_vec(), p.contact().to_string())),
            Err(e) if e.confined => Some((Vec::new(), e.contact)),
            Ok(_) | Err(_) => None,
        }
    }

    /// A refused file-tool target (`tools = confined`): log the content-free
    /// line and return the text for the agent.
    pub fn refuse_confined(&self, tool: &str, path: &std::path::Path, contact: &str) -> String {
        self.log_confined(tool);
        confine::path_refusal(tool, path, contact)
    }

    /// The content-free line of one refused file-tool call (WARN, [`LOG_TARGET`]).
    pub fn log_confined(&self, tool: &str) {
        log_line(&confine::refused_line(&self.session_id, tool, confine::Class::Path));
    }

    /// Tests: make `binding` the process-wide binding of its session id, so a
    /// dispatch path that looks the session up ([`Self::for_session`]) finds
    /// a scratch policy dir. The id must be unique to the test.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn register_for_test(binding: Arc<SandboxBinding>) {
        let mut map = registry().lock().unwrap_or_else(|p| p.into_inner());
        map.insert(binding.session_id.clone(), binding);
    }

    /// A refused 27c target: log the content-free line once for this call
    /// and return the text for the agent.
    pub fn refuse_protected(&self, tool: &str, path: &std::path::Path) -> String {
        self.log_protected(tool);
        protected::refusal(path)
    }

    /// The content-free line of one refused 27c call (WARN, [`LOG_TARGET`]).
    pub fn log_protected(&self, tool: &str) {
        log_line(&protected::protected_line(&self.session_id, tool));
    }

    /// Whether its enforcing spawns register with an audit reader.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn has_audit_reader(&self) -> bool {
        self.audit.is_some()
    }

    /// Decide what the next spawn of `tool` does, logging each state change
    /// once ([`log_line`]), as it is decided.
    pub fn prepare(&self, tool: &'static str) -> SpawnSandbox {
        decide::prepare(self, tool, &mut |line| log_line(&line))
    }
}

/// A session's policy file carried to its linked compaction successor
/// ([`carry_to_successor`]). Dropping it without [`CarriedPolicy::keep`]
/// removes the file this carry created, if it is still that file: a
/// compaction that fails after the carry leaves no successor policy behind.
#[must_use = "dropping it removes the carried policy file"]
#[derive(Debug)]
pub struct CarriedPolicy {
    created: Option<carry::Created>,
}

impl CarriedPolicy {
    /// Only a file the carry created can be rolled back; a kept one never is.
    pub fn from_carried(carried: carry::Carried) -> Self {
        let created = match carried {
            carry::Carried::Copied(created) => Some(created),
            carry::Carried::Nothing | carry::Carried::Kept => None,
        };
        CarriedPolicy { created }
    }

    /// The transition is persisted: the carried file stays.
    pub fn keep(self) {
        let mut this = self;
        this.created = None;
    }
}

impl Drop for CarriedPolicy {
    fn drop(&mut self) {
        if let Some(created) = self.created.take() {
            created.rollback(&mut |line| log_line(&line));
        }
    }
}

/// Carry session `prev`'s policy file (if any) to its linked successor
/// `new`, in this process's base dir, logging the result. `Err` = the copy
/// couldn't be made and nothing was created: the caller must not create the
/// successor (it would run without its predecessor's policy).
pub fn carry_to_successor(prev: &str, new: &str) -> Result<CarriedPolicy, String> {
    let env = policy::PolicyEnv::from_process();
    carry::carry(&env, prev, new, &mut |line| log_line(&line)).map(CarriedPolicy::from_carried)
}
