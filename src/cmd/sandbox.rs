//! `synaps sandbox check <file>` (hidden; for operators): validate a sandbox
//! policy file with the same read, parse and validation code a bash/PTY
//! spawn uses, and print every rule as it would be applied (canonical path,
//! rights, skipped paths). Exit 0 = valid, 1 = invalid (always 1 off Linux:
//! there is no Landlock to enforce it, so its spawns would be refused).
//! Writes nothing, so it is safe to run on a `<id>.conf.new` before moving
//! it into place.

use std::path::{Path, PathBuf};

#[derive(clap::Subcommand, Debug)]
pub enum SandboxAction {
    /// Validate a policy file and print the rules it would apply.
    Check {
        /// The policy file (e.g. <base>/sandbox/sessions/<session-id>.conf.new).
        file: PathBuf,
    },
}

/// Runs the action; returns the process exit code.
pub fn run(action: SandboxAction) -> i32 {
    match action {
        SandboxAction::Check { file } => check(&file),
    }
}

fn check(file: &Path) -> i32 {
    use agent_engine::sandbox::{landlock, policy};
    let env = policy::PolicyEnv::from_process();
    let abi = landlock::abi_version();
    // No Landlock on this OS (never so on Linux): the file is still checked
    // and reported, but nothing it allows can be enforced, so never exit 0.
    let no_os_support = matches!(abi, Err(u) if u.always_refuses());
    if no_os_support {
        eprintln!(
            "synaps sandbox check {}: the write sandbox is unavailable on this OS (no Landlock): every bash/PTY spawn of a session with an enforcing policy is refused",
            file.display()
        );
    }
    let (ok, text) = policy::check_report(file, &env, abi);
    print!("{text}");
    if ok && !no_os_support {
        0
    } else {
        1
    }
}
