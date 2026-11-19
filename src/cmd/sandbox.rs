//! `synaps sandbox check`: what a session started here would be confined to,
//! without starting one.
//!
//! Read-only: it reads the config, asks the kernel for its Landlock ABI,
//! loads the configured policy provider (and no other plugin), asks it the
//! same question a session start asks and prints the outcome. Nothing is
//! written. The report itself is built by `agent_engine::sandbox::check`.
//!
//! Plain output is `key: value` lines. With `--json` standard output is
//! exactly one JSON object (the keys are frozen: `confine`, `abi`,
//! `provider`, `provider_state`, `refusal`, `write`, `read`, `dropped`).
//!
//! Exit codes: 0 usable (or confinement off), 1 refusing, 2 the command line
//! or the folder could not be used.

use std::io::Write;
use std::path::PathBuf;

use agent_engine::sandbox::{check, provider};

/// Usable, or confinement is off.
const EXIT_OK: i32 = 0;
/// A session started now would be refused.
const EXIT_REFUSING: i32 = 1;
/// The command could not run.
const EXIT_STOPPED: i32 = 2;

#[derive(clap::Subcommand, Debug)]
pub enum SandboxAction {
    /// Print the write confinement a session started here would get: the
    /// provider's answer screened against the protected set. Read-only.
    /// Exit code 0: usable or off; 1: a session would be refused.
    Check(CheckArgs),
}

#[derive(clap::Args, Debug)]
pub struct CheckArgs {
    /// The session id the provider is asked about.
    #[arg(long, default_value = provider::CHECK_SESSION_ID)]
    session: String,
    /// The folder the session would be confined around (default: the
    /// current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Print one JSON object instead of `key: value` lines.
    #[arg(long)]
    json: bool,
}

pub(crate) async fn run(action: SandboxAction) -> i32 {
    match action {
        SandboxAction::Check(args) => run_check(args).await,
    }
}

async fn run_check(args: CheckArgs) -> i32 {
    let workspace = match &args.workspace {
        Some(dir) if dir.is_absolute() => dir.clone(),
        Some(dir) => match std::env::current_dir() {
            Ok(cwd) => cwd.join(dir),
            Err(e) => return stopped(&format!("cannot read the current directory: {e}")),
        },
        None => match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(e) => return stopped(&format!("cannot read the current directory: {e}")),
        },
    };
    let workspace = provider::workspace_for(Some(&workspace));
    let settings = agent_core::config::read_sandbox_settings();
    let report = check::run_check(&settings, &args.session, &workspace).await;
    let mut out = String::new();
    if args.json {
        out.push_str(&report.to_json().to_string());
        out.push('\n');
    } else {
        for line in report.lines() {
            out.push_str(&line);
            out.push('\n');
        }
    }
    let mut stdout = std::io::stdout().lock();
    if stdout.write_all(out.as_bytes()).and_then(|()| stdout.flush()).is_err() {
        return EXIT_STOPPED;
    }
    match report.exit_code() {
        0 => EXIT_OK,
        _ => EXIT_REFUSING,
    }
}

fn stopped(why: &str) -> i32 {
    eprintln!("synaps sandbox check: {why}");
    EXIT_STOPPED
}
