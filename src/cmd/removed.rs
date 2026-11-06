//! One-release stubs for removed subcommands. `synaps agent` and
//! `synaps watcher` still parse, with any arguments, so a
//! script or unit that runs them gets a pointer instead of a clap usage
//! error. Delete this module, the hidden `Command` variants and the
//! `without_stubs` call in `synaps completions` in the release after
//! `REMOVED_IN`.

/// The release that removed `synaps agent` and `synaps watcher`.
const REMOVED_IN: &str = "0.12.0";

/// Exit status of a removed subcommand (a usage error, like clap's).
const EXIT_REMOVED: i32 = 2;

fn message(command: &str) -> String {
    format!(
        "`synaps {command}` was removed in {REMOVED_IN}: run long-lived headless work as a \
         pinned daemon session (synaps daemon, synaps --attach --keep-warm). See CHANGELOG."
    )
}

/// Print the removal notice for `command` to stderr and exit 2.
pub(crate) fn exit(command: &str) -> ! {
    eprintln!("{}", message(command));
    std::process::exit(EXIT_REMOVED);
}

/// `cmd` without its hidden subcommands, for `synaps completions`:
/// clap_complete lists hidden subcommands, and a removed command must not
/// be offered. Clap cannot remove a subcommand, so this rebuilds the top
/// level from the same arguments and the visible subcommands.
pub(crate) fn without_stubs(cmd: clap::Command) -> clap::Command {
    clap::Command::new("synaps")
        .version(env!("CARGO_PKG_VERSION"))
        .about(cmd.get_about().cloned().unwrap_or_default())
        .long_about(cmd.get_long_about().cloned().unwrap_or_default())
        .args(cmd.get_arguments().cloned())
        .subcommands(cmd.get_subcommands().filter(|s| !s.is_hide_set()).cloned())
}
