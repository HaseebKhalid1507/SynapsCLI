//! Operator-only retention over the plain files under the base directory:
//! sessions, notes, the derived note index, traces and logs.
//! No operator export (which can contain secret bodies) is exposed as a model tool.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use synaps_cli::core::retention::{self, RetentionDomain, RetentionPolicy, RetentionRoots};

#[derive(clap::Subcommand, Debug)]
pub enum RetentionAction {
    /// Report artifact counts/bytes.
    Inspect,
    /// Apply age/disk policy.
    Sweep {
        /// Delete sessions, traces and logs older than this many days. Saved
        /// notes are NOT deleted by age unless --include-notes is given; a
        /// note saved with its own expiry still expires by that.
        #[arg(long)]
        max_age_days: Option<u32>,
        /// Requested disk budget: deletes derived indexes, then the oldest
        /// sessions, traces and logs (never protected chain heads). It does
        /// NOT delete notes unless --include-notes is given; without it an
        /// unmet budget is an error and every note is kept.
        #[arg(long)]
        max_disk_bytes: Option<u64>,
        /// Let this sweep delete saved notes of any class. With
        /// --max-age-days, every note older than that goes; with
        /// --max-disk-bytes, notes are evicted oldest first until the budget
        /// is met or no note is left. Without this flag only notes past their
        /// own expiry and forgotten notes are removed.
        #[arg(long)]
        include_notes: bool,
    },
    /// Export privately, secrets included. Stop writers first.
    Export {
        /// Destination directory; export files/index must not already exist.
        dest: PathBuf,
    },
    /// Delete one artifact by domain and exact id.
    Forget {
        /// sessions | memory | memory-index | traces | logs
        domain: String,
        /// Memory: namespace:record-id (the id as it is in the file).
        id: String,
    },
}

pub fn run(action: RetentionAction) -> Result<()> {
    run_legacy(action, &RetentionRoots::resolve())
}

fn print_json(value: &Value) -> Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, value)?;
    writeln!(stdout)?;
    Ok(())
}

fn require_policy(age: Option<u32>, bytes: Option<u64>) -> Result<()> {
    if age.is_none() && bytes.is_none() {
        bail!("sweep needs --max-age-days and/or --max-disk-bytes");
    }
    Ok(())
}
fn parse_domain(domain: &str) -> Result<RetentionDomain> {
    Ok(match domain {
        "sessions" => RetentionDomain::Sessions,
        "memory" => RetentionDomain::Memory,
        "memory-index" => RetentionDomain::MemoryIndex,
        "traces" => RetentionDomain::Traces,
        "logs" => RetentionDomain::Logs,
        _ => bail!("unknown domain — expected sessions|memory|memory-index|traces|logs"),
    })
}
fn run_legacy(action: RetentionAction, roots: &RetentionRoots) -> Result<()> {
    match action {
        RetentionAction::Inspect => print_json(&serde_json::to_value(retention::inspect(roots)?)?)?,
        RetentionAction::Sweep {
            max_age_days,
            max_disk_bytes,
            include_notes,
        } => {
            require_policy(max_age_days, max_disk_bytes)?;
            print_json(&serde_json::to_value(retention::sweep(
                roots,
                &RetentionPolicy {
                    max_age_days,
                    max_disk_bytes,
                    include_notes,
                },
            )?)?)?;
        }
        RetentionAction::Export { dest } => {
            print_json(&serde_json::to_value(retention::export(roots, &dest)?)?)?
        }
        RetentionAction::Forget { domain, id } => {
            let domain = parse_domain(&domain)?;
            retention::forget(roots, domain, &id)?;
            print_json(&json!({"domain":domain,"forgotten":true}))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        action: RetentionAction,
    }
    #[test]
    fn the_removed_migration_actions_are_unknown_subcommands() {
        use clap::Parser;
        for removed in ["memory-scope", "upgrade-memory", "migrate-memory"] {
            let error = Cli::try_parse_from(["retention", removed]).err().expect(removed);
            assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand, "{removed}");
        }
    }
    /// `history` was a domain of the Axel backend only.
    #[test]
    fn history_is_no_domain() {
        let refused = parse_domain("history").unwrap_err().to_string();
        assert_eq!(
            refused,
            "unknown domain — expected sessions|memory|memory-index|traces|logs"
        );
        for domain in ["sessions", "memory", "memory-index", "traces", "logs"] {
            parse_domain(domain).unwrap();
        }
    }
    #[test]
    fn include_notes_parses_with_the_disk_budget_alone() {
        use clap::Parser;
        let parsed = Cli::try_parse_from([
            "retention",
            "sweep",
            "--max-disk-bytes",
            "1000",
            "--include-notes",
        ])
        .unwrap();
        assert!(matches!(
            parsed.action,
            RetentionAction::Sweep {
                max_age_days: None,
                max_disk_bytes: Some(1000),
                include_notes: true,
            }
        ));
    }
}
