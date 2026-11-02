//! What one spawn does under its session's policy ([`prepare`]): the policy
//! lookup, the kill switch, the Landlock answer and the state lines. Built
//! on every platform. Off Linux there is no Landlock, so an enforcing
//! policy always ends in [`unavailable`] with
//! [`Unavailable::UnsupportedOs`], which refuses whatever `required` says:
//! nothing runs unsandboxed under an enforcing policy there.

use super::landlock::{self, Unavailable};
use super::policy::{self, Lookup, Mode, Policy};
use super::{SandboxBinding, SpawnSandbox};

#[cfg(test)]
mod tests;

/// Decide what the next spawn of `tool` does for `b`'s session: the policy
/// file and the kill switch are read again for every spawn. Log lines for
/// state changes go to `log`, each state once, when it is first seen.
///
/// One session's spawns decide one at a time: the binding's lock is held
/// from before the lookup (kill switch, policy file read and validation)
/// until the state is logged, so the states a session logs are the states
/// its spawns observed, in the order they observed them, each once. (Lock
/// order: the binding, then the kill switch inside the lookup.)
pub(crate) fn prepare(b: &SandboxBinding, tool: &'static str, log: &mut dyn FnMut(String)) -> SpawnSandbox {
    prepare_hooked(b, tool, log, &mut || {})
}

/// [`prepare`] with `after_lookup` run between the lookup and the decision,
/// under the binding's lock (tests: a spawn that pauses there).
pub(crate) fn prepare_hooked(
    b: &SandboxBinding,
    tool: &'static str,
    log: &mut dyn FnMut(String),
    after_lookup: &mut dyn FnMut(),
) -> SpawnSandbox {
    let mut guard = b.logged.lock().unwrap_or_else(|p| p.into_inner());
    let last = &mut *guard;
    let id = b.session_id.as_str();
    let found = policy::lookup_with(&b.env, id, b.switch, &mut |line| log(line.to_string()));
    after_lookup();
    match found {
        Lookup::None => {
            // A policy seen before (found, off, invalid, …) has been removed.
            note(last, log, "none".into(), |previous| match previous {
                Some(prev) if prev != "global-off" => vec![format!("sandbox.off session={id} reason=removed")],
                _ => Vec::new(),
            });
            SpawnSandbox::None
        }
        Lookup::GlobalOff => {
            note(last, log, "global-off".into(), |_| Vec::new());
            SpawnSandbox::None
        }
        Lookup::Invalid(e) => {
            let line = policy::invalid_line(id, &e);
            note(last, log, line.clone(), |_| vec![line]);
            SpawnSandbox::Refuse(e.refusal())
        }
        Lookup::Found(p) => {
            let abi = policy::usable_abi(b.abi.unwrap_or_else(landlock::abi_version));
            if p.mode() == Mode::Off {
                note(last, log, format!("off {}", p.hash()), |_| {
                    vec![policy::policy_line(id, &p, abi), policy::explicit_off_line(id, &p)]
                });
                return SpawnSandbox::None;
            }
            match abi {
                Err(u) => unavailable(b, last, log, &p, u),
                Ok(_) => apply(b, last, log, &p, tool, abi),
            }
        }
    }
}

/// An enforcing policy with a usable Landlock ABI: build the domain.
#[cfg(target_os = "linux")]
fn apply(
    b: &SandboxBinding,
    last: &mut Option<String>,
    log: &mut dyn FnMut(String),
    p: &Policy,
    tool: &'static str,
    abi: Result<u32, Unavailable>,
) -> SpawnSandbox {
    use super::policy::BuildError;
    let id = b.session_id.as_str();
    match super::spawn::build(b, p) {
        Ok(ruleset) => {
            note(last, log, format!("enforce {}", p.hash()), |_| {
                let mut lines = vec![policy::policy_line(id, p, abi)];
                lines.extend(policy::skip_lines(id, p));
                lines
            });
            // The audit log is open (at its end) before this child
            // can make a domain; a failed open doesn't stop it.
            if let Some(r) = &b.audit {
                r.before_spawn(log);
            }
            SpawnSandbox::Apply(Box::new(super::spawn::Prepared::new(ruleset, id, tool, p, b)))
        }
        Err(BuildError::Unavailable(u)) => unavailable(b, last, log, p, u),
        Err(e) => {
            let line = format!("sandbox.refused session={id} reason=build {e}");
            note(last, log, format!("build-failed {} {e}", p.hash()), |_| vec![line]);
            SpawnSandbox::Refuse(format!(
                "sandbox policy could not be applied for this session ({e}); commands can't run until it's fixed. {}",
                p.contact()
            ))
        }
    }
}

/// Off Linux an ABI can only come from a test binding: there is still no
/// Landlock to apply it with, so the spawn is refused as unavailable.
#[cfg(not(target_os = "linux"))]
fn apply(
    b: &SandboxBinding,
    last: &mut Option<String>,
    log: &mut dyn FnMut(String),
    p: &Policy,
    tool: &'static str,
    abi: Result<u32, Unavailable>,
) -> SpawnSandbox {
    let _ = (tool, abi);
    unavailable(b, last, log, p, Unavailable::UnsupportedOs)
}

/// No usable Landlock. A version query or ruleset that failed with an error
/// (a resource or syscall failure, not an unsupported kernel) refuses the
/// spawn like a failed build. A kernel without usable Landlock (no syscalls,
/// disabled at boot, too old) refuses when the policy requires it, else runs
/// unsandboxed. An OS without Landlock at all
/// ([`Unavailable::always_refuses`]) refuses as if the policy required it.
/// Logged once per state.
fn unavailable(
    b: &SandboxBinding,
    last: &mut Option<String>,
    log: &mut dyn FnMut(String),
    p: &Policy,
    u: Unavailable,
) -> SpawnSandbox {
    let id = b.session_id.as_str();
    if let Unavailable::Error(errno) = u {
        note(last, log, format!("error {} {errno}", p.hash()), |_| {
            vec![policy::policy_line(id, p, Err(u)), format!("sandbox.refused session={id} reason=landlock-error errno={errno}")]
        });
        return SpawnSandbox::Refuse(format!(
            "sandbox policy could not be applied for this session (landlock {u}); commands can't run until it's fixed. {}",
            p.contact()
        ));
    }
    if p.required() || u.always_refuses() {
        note(last, log, format!("refused {} {}", p.hash(), u.reason()), |_| {
            vec![
                policy::policy_line(id, p, Err(u)),
                format!("sandbox.refused session={id} reason=unavailable:{}", u.reason()),
            ]
        });
        return SpawnSandbox::Refuse(format!(
            "sandbox required for this session but unavailable on this host ({u}); commands can't run. {}",
            p.contact()
        ));
    }
    note(last, log, format!("unavailable {} {}", p.hash(), u.reason()), |_| {
        vec![
            policy::policy_line(id, p, Err(u)),
            format!("sandbox.unavailable session={id} reason={}", u.reason()),
        ]
    });
    SpawnSandbox::None
}

/// Log `lines` (given the previous state's fingerprint) when the state's
/// fingerprint differs from the last one logged. `last` is the binding's
/// state, borrowed from under its lock (held by [`prepare_hooked`] for the
/// whole decision).
fn note(
    last: &mut Option<String>,
    log: &mut dyn FnMut(String),
    fingerprint: String,
    lines: impl FnOnce(Option<&str>) -> Vec<String>,
) {
    if last.as_deref() != Some(fingerprint.as_str()) {
        for line in lines(last.as_deref()) {
            log(line);
        }
        *last = Some(fingerprint);
    }
}
