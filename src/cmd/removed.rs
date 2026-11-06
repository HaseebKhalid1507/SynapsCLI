//! One-release stubs for removed subcommands. Each still parses, with any
//! arguments, so a script or unit that runs it gets a pointer instead of a
//! clap usage error. Removing another subcommand is one row in [`REMOVED`]
//! and one variant in [`RemovedCommand`]. Delete this module,
//! `Command::Removed` and the `without_stubs` call in `synaps completions`
//! in the release after [`REMOVED_IN`].

use agent_core::REMOVED_IN;

/// A removed subcommand and what to run instead.
pub(crate) struct Removed {
    pub(crate) name: &'static str,
    pub(crate) instead: &'static str,
}

const PIN_A_SESSION: &str = "pin a daemon session (synaps --attach --keep-warm, or synaps daemon \
     pin) and drive it with synaps send --session <name>. See \
     https://github.com/HaseebKhalid1507/SynapsCLI/blob/dev/docs/daemon-mode.md#pinning-a-session-pin";

/// Every removed subcommand. Each row has a hidden variant in
/// [`RemovedCommand`]; a unit test keeps the two in step.
pub(crate) const REMOVED: &[Removed] = &[
    Removed { name: "agent", instead: PIN_A_SESSION },
    Removed { name: "watcher", instead: PIN_A_SESSION },
];

/// The hidden stubs, flattened into the top-level `Command`.
#[derive(clap::Subcommand)]
pub(crate) enum RemovedCommand {
    #[command(hide = true, disable_help_flag = true, about = about("agent"))]
    Agent {
        #[arg(hide = true, trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(hide = true, disable_help_flag = true, about = about("watcher"))]
    Watcher {
        #[arg(hide = true, trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

impl RemovedCommand {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Agent { .. } => "agent",
            Self::Watcher { .. } => "watcher",
        }
    }
}

/// Exit status of a removed subcommand (a usage error, like clap's).
const EXIT_REMOVED: i32 = 2;

fn row(name: &str) -> &'static Removed {
    REMOVED
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| unreachable!("`{name}` is not in cmd::removed::REMOVED"))
}

fn about(name: &str) -> String {
    format!("Removed in {REMOVED_IN}: {}", row(name).instead)
}

fn message(name: &str) -> String {
    format!("`synaps {name}` was removed in {REMOVED_IN}: {}", row(name).instead)
}

/// Print the removal notice for `name` to stderr and exit 2.
pub(crate) fn exit(name: &str) -> ! {
    eprintln!("{}", message(name));
    std::process::exit(EXIT_REMOVED);
}

/// `cmd` without the removed subcommands, for `synaps completions`:
/// clap_complete lists hidden subcommands, and a removed command must not
/// be offered. Clap cannot remove a subcommand, so this rebuilds the top
/// level. It copies only the name, version, about, long_about, arguments
/// and the other subcommands: a top-level `#[command(...)]` setting added
/// later is missing from the completions until it is copied here too.
pub(crate) fn without_stubs(cmd: clap::Command) -> clap::Command {
    clap::Command::new("synaps")
        .version(env!("CARGO_PKG_VERSION"))
        .about(cmd.get_about().cloned().unwrap_or_default())
        .long_about(cmd.get_long_about().cloned().unwrap_or_default())
        .args(cmd.get_arguments().cloned())
        .subcommands(
            cmd.get_subcommands()
                .filter(|s| REMOVED.iter().all(|r| r.name != s.get_name()))
                .cloned(),
        )
}

#[cfg(test)]
mod tests {
    use super::{without_stubs, REMOVED};
    use crate::{Cli, Command};
    use clap::{CommandFactory, Parser};
    use std::collections::BTreeSet;

    fn table() -> BTreeSet<&'static str> {
        REMOVED.iter().map(|r| r.name).collect()
    }

    #[test]
    fn the_hidden_subcommands_are_the_removed_table() {
        assert_eq!(table().len(), REMOVED.len(), "duplicate row in REMOVED");
        let cmd = Cli::command();
        let hidden: BTreeSet<&str> =
            cmd.get_subcommands().filter(|s| s.is_hide_set()).map(|s| s.get_name()).collect();
        assert_eq!(hidden, table());
    }

    #[test]
    fn each_row_parses_to_its_stub() {
        for row in REMOVED {
            let cli = Cli::try_parse_from(["synaps", row.name, "--x", "y"]).unwrap();
            match cli.command {
                Some(Command::Removed(stub)) => assert_eq!(stub.name(), row.name),
                _ => panic!("`synaps {}` did not parse to its stub", row.name),
            }
        }
    }

    #[test]
    fn completions_offer_every_subcommand_but_the_removed_ones() {
        let names = |cmd: &clap::Command| -> Vec<String> {
            cmd.get_subcommands().map(|s| s.get_name().to_string()).collect()
        };
        let expected: Vec<String> =
            names(&Cli::command()).into_iter().filter(|n| !table().contains(n.as_str())).collect();
        let mut cmd = without_stubs(Cli::command());
        assert_eq!(names(&cmd), expected);
        use clap_complete::Shell;
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::Elvish, Shell::PowerShell] {
            let mut script = Vec::new();
            clap_complete::generate(shell, &mut cmd, "synaps", &mut script);
            assert!(!script.is_empty(), "{shell}");
        }
    }
}
