//! `synaps retention` — headless unified retention operations (T34, spec
//! §9.7) over sessions, compaction parents, memory, memory indexes, traces,
//! and logs.
//!
//! - `inspect` prints per-domain file counts and byte totals (JSON);
//! - `sweep` enforces max-age / disk budgets (chain heads are never
//!   deleted — a sweep cannot leave a named chain dangling);
//! - `export <DIR>` copies every retained artifact for offline inspection;
//! - `forget <DOMAIN> <ID>` deletes one artifact (session ids, trace/log
//!   file names, index namespaces, or `namespace:mem-id` memory records);
//!   session chain heads fail closed.

use anyhow::{bail, Result};
use synaps_cli::core::retention::{self, RetentionDomain, RetentionPolicy, RetentionRoots};

#[derive(clap::Subcommand, Debug)]
pub enum RetentionAction {
    /// Report per-domain artifact counts and bytes as JSON.
    Inspect,
    /// Apply the retention policy (age and/or disk budget).
    Sweep {
        /// Delete artifacts older than this many days.
        #[arg(long)]
        max_age_days: Option<u32>,
        /// Delete oldest artifacts until total disk use fits this budget.
        #[arg(long)]
        max_disk_bytes: Option<u64>,
    },
    /// Copy sessions, memory, traces, and logs into a directory.
    Export {
        /// Destination directory (created if missing).
        dest: std::path::PathBuf,
    },
    /// Delete one artifact by domain and id.
    Forget {
        /// sessions | memory | memory-index | traces | logs
        domain: String,
        /// Session id, trace/log file name, index namespace, or
        /// `namespace:mem-id` for memory records.
        id: String,
    },
}

pub fn run(action: RetentionAction) -> Result<()> {
    let roots = RetentionRoots::resolve();
    match action {
        RetentionAction::Inspect => {
            let report = retention::inspect(&roots)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        RetentionAction::Sweep {
            max_age_days,
            max_disk_bytes,
        } => {
            if max_age_days.is_none() && max_disk_bytes.is_none() {
                bail!("sweep needs --max-age-days and/or --max-disk-bytes");
            }
            let outcome = retention::sweep(
                &roots,
                &RetentionPolicy {
                    max_age_days,
                    max_disk_bytes,
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
        }
        RetentionAction::Export { dest } => {
            let summary = retention::export(&roots, &dest)?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        RetentionAction::Forget { domain, id } => {
            let domain = match domain.as_str() {
                "sessions" => RetentionDomain::Sessions,
                "memory" => RetentionDomain::Memory,
                "memory-index" => RetentionDomain::MemoryIndex,
                "traces" => RetentionDomain::Traces,
                "logs" => RetentionDomain::Logs,
                other => bail!(
                    "unknown domain {other:?} — expected sessions|memory|memory-index|traces|logs"
                ),
            };
            retention::forget(&roots, domain, &id)?;
            println!("forgot {domain:?} artifact {id}");
        }
    }
    Ok(())
}
