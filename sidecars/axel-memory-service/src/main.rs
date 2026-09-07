use std::{io, path::PathBuf};
use synaps_axel_memory_service::{contract::valid_project, protocol};
fn main() {
    // Single-threaded startup, before SQLite can create a file/WAL/SHM.
    unsafe {
        libc::umask(0o077);
    }
    let mut args = std::env::args_os().skip(1);
    let mut brain = None;
    let mut project = None;
    let mut operator = false;
    let mut user_scope = false;
    while let Some(arg) = args.next() {
        if arg == "--brain" && brain.is_none() {
            brain = args.next().map(PathBuf::from);
        } else if arg == "--project" && project.is_none() {
            project = args.next().and_then(|s| s.into_string().ok());
        } else if arg == "--operator" && !operator {
            operator = true;
        } else if arg == "--user-scope" && !user_scope {
            user_scope = true;
        } else {
            invalid();
        }
    }
    let (Some(brain), Some(project)) = (brain, project) else {
        invalid()
    };
    if !brain.is_absolute()
        || brain.extension().and_then(|s| s.to_str()) != Some("r8")
        || !valid_project(&project)
    {
        invalid();
    }
    if protocol::run_with_options(
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
        &brain,
        &project,
        operator,
        user_scope,
    )
    .is_err()
    {
        std::process::exit(1);
    }
}
fn invalid() -> ! {
    eprintln!(
        "usage: synaps-axel-memory-service --brain ABSOLUTE.r8 --project p<16 lowercase hex> [--operator] [--user-scope]"
    );
    std::process::exit(2)
}
