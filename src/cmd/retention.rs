//! Operator-only retention.
//!
//! Legacy mode retains the original all-domain behavior. In Axel mode inventory,
//! sweep and export cover ONLY selected project memory/history; each response
//! lists omitted legacy domains. Explicit session/trace/log forget remains legacy.
//! No operator export (which can contain secret bodies) is exposed as a model tool.

use agent_engine::memory_backend::MemoryBinding;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use synaps_cli::config::{MemoryBackendConfig, MemoryBackendKind};
use synaps_cli::core::retention::{self, RetentionDomain, RetentionPolicy, RetentionRoots};

#[derive(clap::Subcommand, Debug)]
pub enum RetentionAction {
    /// Report artifact counts/bytes. Axel: selected project memory/history only.
    Inspect,
    /// Apply age/disk policy. Axel: notes only, preserving history/tombstones.
    Sweep {
        /// Delete sessions, traces and logs older than this many days. Plain-file
        /// memory: saved notes are NOT deleted by age unless --include-notes is
        /// given; a note saved with its own expiry still expires by that.
        /// Axel: the age applies to notes, which is all its sweep can delete,
        /// so it is refused without --include-notes.
        #[arg(long)]
        max_age_days: Option<u32>,
        /// Requested disk budget: deletes derived indexes, then the oldest
        /// sessions, traces and logs (never protected chain heads). Plain-file
        /// memory: it does NOT delete notes unless --include-notes is given;
        /// without it an unmet budget is an error and every note is kept.
        /// Axel: the budget deletes notes, oldest first, and nothing else, so
        /// it is refused without --include-notes.
        #[arg(long)]
        max_disk_bytes: Option<u64>,
        /// Plain-file memory: let this sweep delete saved notes of any class.
        /// With --max-age-days, every note older than that goes; with
        /// --max-disk-bytes, notes are evicted oldest first until the budget
        /// is met or no note is left. Without this flag only notes past their
        /// own expiry and forgotten notes are removed. Axel: required, because
        /// notes are all its sweep can delete; without it the sweep is refused
        /// and nothing is deleted.
        #[arg(long)]
        include_notes: bool,
    },
    /// Export privately. Axel: every linked original scope, including secrets.
    /// Stop writers first; per-scope files plus a final index are not a DB snapshot.
    Export {
        /// Destination directory; export files/index must not already exist.
        dest: PathBuf,
    },
    /// Delete one artifact by domain and exact id.
    Forget {
        /// sessions | memory | history (Axel) | memory-index | traces | logs
        domain: String,
        /// Axel memory/history: exact ID (optional exact project namespace prefix
        /// for notes). Legacy memory: namespace:record-id (the id as it is in the file).
        id: String,
    },
}

pub fn run(action: RetentionAction) -> Result<()> {
    let config = synaps_cli::config::load_config().memory_backend;
    if config.kind == MemoryBackendKind::Unavailable {
        bail!("selected memory backend is unavailable; no legacy retention fallback");
    }
    if config.kind == MemoryBackendKind::Legacy {
        return run_legacy(action, &RetentionRoots::resolve());
    }
    // main already owns a Tokio runtime. Do not nest block_on in that runtime;
    // a scoped operator worker owns its own current-thread runtime and secrets.
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(run_memory_action(action, &config))
            })
            .join()
            .map_err(|_| anyhow::anyhow!("retention operator worker failed"))?
    })
}

async fn run_memory_action(action: RetentionAction, config: &MemoryBackendConfig) -> Result<()> {
    let binding = MemoryBinding::from_config(config);
    match action {
        RetentionAction::Inspect => {
            let stats = binding.rpc("stats", json!({})).await?;
            print_json(&selected_report(&binding, stats)?)?;
        }
        RetentionAction::Sweep { max_age_days, max_disk_bytes, include_notes } => {
            require_policy(max_age_days, max_disk_bytes)?;
            // Before the service is asked for anything.
            require_include_notes_for_axel(max_age_days, max_disk_bytes, include_notes)?;
            let stats = binding.rpc("sweep", json!({"max_age_days":max_age_days,"max_disk_bytes":max_disk_bytes})).await?;
            let budget_met = max_disk_bytes.map(|limit| stats["bytes"].as_u64().is_some_and(|bytes| bytes <= limit));
            let mut report = selected_report(&binding, stats)?;
            report["disk_budget_met"] = json!(budget_met);
            report["sweep_scope"] = json!("notes only; history and permanent tombstones protected");
            print_json(&report)?;
            if budget_met == Some(false) {
                bail!("Axel memory disk budget unmet; protected history/tombstones remain (no legacy domains swept)");
            }
        }
        RetentionAction::Export { dest } => {
            // Do not print full RPC output. Full exports can contain secret note
            // bodies, captures and hidden archive notes: local 0600 artifact only.
            let metadata = export_members(&binding, &dest).await?;
            print_json(&selected_report(&binding, metadata)?)?;
        }
        RetentionAction::Forget { domain, id } => match domain.as_str() {
            "memory" => {
                let exact = selected_memory_id(&binding.scope()?.namespace(), &id)?;
                binding.forget(exact).await?;
                print_json(&json!({"backend":"axel","domain":"memory","forgotten":true}))?;
            }
            "history" => {
                if binding.rpc("history_forget", json!({"id":id})).await? != true {
                    bail!("history artifact not found in selected project");
                }
                print_json(&json!({"backend":"axel","domain":"history","forgotten":true}))?;
            }
            "memory-index" => bail!("Axel derived indexes are service-owned; legacy memory-index forget is not a selected-memory operation"),
            _ => {
                let domain = parse_domain(&domain)?;
                retention::forget(&RetentionRoots::resolve(), domain, &id)?;
                print_json(&json!({"backend":"legacy-files","domain":domain,"forgotten":true}))?;
            }
        },
    }
    Ok(())
}

fn selected_report(binding: &MemoryBinding, result: Value) -> Result<Value> {
    Ok(json!({
        "backend":"axel", "project":binding.scope()?.key(), "selected_memory":result,
        "omitted_domains":["legacy-memory","legacy-memory-index","sessions","traces","logs"],
        "scope":"selected project memory/history only; not an all-domain retention pass",
    }))
}
fn selected_memory_id<'a>(namespace: &str, id: &'a str) -> Result<&'a str> {
    let id = match id.split_once(':') {
        Some((prefix, id)) if prefix == namespace => id,
        Some(_) => bail!("memory namespace does not exactly match the selected host project"),
        None => id,
    };
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        bail!("memory forget requires a bounded exact ID");
    }
    Ok(id)
}
fn print_json(value: &Value) -> Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, value)?;
    writeln!(stdout)?;
    Ok(())
}

/// A service export is EXACT ORIGINAL SCOPE, not an alias-group inventory.
/// Enumerate service-authorized membership (not merely currently registered Git
/// roots), retain each envelope unchanged, and publish the index LAST. Stop all
/// writers: membership is checked twice, but these reads are not a global snapshot.
#[cfg(unix)]
async fn export_members(binding: &MemoryBinding, dest: &Path) -> Result<Value> {
    use agent_core::{core::private_fs::ConfinedDir, memory::store::ProjectScope};
    let info = scope_info(binding).await?;
    let mut members = info.members.clone();
    members.sort();
    let names: Vec<_> = members
        .iter()
        .map(|project| export_filename(project, members.len()))
        .collect();
    let absolute = if dest.is_absolute() {
        dest.to_path_buf()
    } else {
        std::env::current_dir()?.join(dest)
    };
    let root = ConfinedDir::create_absolute_no_symlinks_durable(&absolute)?;
    // Refuse previous backups before writing any member, including symlinks.
    for entry in root.entries()? {
        if entry.name == EXPORT_INDEX || names.contains(&entry.name) {
            bail!("export file/index already exists; use a fresh private destination");
        }
    }
    let mut files = Vec::new();
    for (project, name) in members.iter().zip(&names) {
        // scope_info is trusted operator membership evidence. Do not derive or
        // canonicalize historical paths, or omit service members absent from Git.
        let scope = ProjectScope::from_key(binding.scope()?.root(), project)?;
        let inventory = binding.with_scope(scope).rpc("export", json!({"full":true})).await
            .context("scope export failed; any files written are an incomplete backup (no final index); retry in a fresh directory")?;
        validate_exact_export(&inventory, project)?;
        let file = write_export_file(&root, name, &inventory)?;
        files.push(json!({"project":project,"file":name,"bytes":file.0,"sha256":file.1}));
    }
    let after = scope_info(binding).await?;
    let mut after_members = after.members;
    after_members.sort();
    if after_members != members
        || after.canonical_project != info.canonical_project
        || after.mode != info.mode
    {
        bail!("scope membership changed; incomplete backup has no final index; stop writers and retry in a fresh directory");
    }
    let manifest = json!({
        "format":"synaps-axel-group-export/1", "complete":true,
        "canonical_project":info.canonical_project, "selected_project":binding.scope()?.key(),
        "members":members, "files":files,
        "consistency":"separate exact-owner exports, not an atomic group snapshot; stop writers before export",
        "restore":"import each file separately with its original source/target project, then explicitly link aliases",
    });
    let index = write_export_file(&root, EXPORT_INDEX, &manifest)?;
    let bytes: u64 = files
        .iter()
        .map(|file| file["bytes"].as_u64().expect("byte count"))
        .sum::<u64>()
        + index.0 as u64;
    Ok(
        json!({"files":files.len()+1,"scope_files":files.len(),"bytes":bytes,"index":EXPORT_INDEX,
        "members":members,"complete":true,"consistency":manifest["consistency"]}),
    )
}
#[cfg(not(unix))]
async fn export_members(_: &MemoryBinding, _: &Path) -> Result<Value> {
    bail!("private Axel export requires Unix nofollow filesystem support")
}
// Only the Unix exporter writes files; elsewhere `export_members` refuses.
#[cfg(unix)]
const EXPORT_INDEX: &str = "axel-memory-index.json";
/// The most one export file may hold: the service's one-transaction bound.
#[cfg(unix)]
const EXPORT_FILE_MAX_BYTES: usize = 24 * 1024 * 1024;

/// The service's membership of the selected scope, as metadata only: never
/// forward arbitrary service output to stdout.
#[cfg(unix)]
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeInfo {
    canonical_project: String,
    members: Vec<String>,
    mode: String,
    user_scope: bool,
}
#[cfg(unix)]
async fn scope_info(binding: &MemoryBinding) -> Result<ScopeInfo> {
    parse_scope_info(binding.rpc("scope_info", json!({})).await?, binding.scope()?.key())
}
#[cfg(unix)]
fn parse_scope_info(value: Value, requested: &str) -> Result<ScopeInfo> {
    let info: ScopeInfo = serde_json::from_value(value)
        .map_err(|_| anyhow::anyhow!("invalid service scope metadata"))?;
    if !matches!(info.mode.as_str(), "multi_project" | "pinned")
        || info.user_scope
        || info.members.len() > 4096
        || !info.members.iter().all(|p| valid_project(p) && p != "p0000000000000000")
        || info.members.iter().collect::<std::collections::BTreeSet<_>>().len()
            != info.members.len()
        || !info.members.iter().any(|p| p == requested)
        || !info.members.contains(&info.canonical_project)
    {
        bail!("invalid service scope membership metadata");
    }
    Ok(info)
}
#[cfg(unix)]
fn valid_project(s: &str) -> bool {
    s.len() == 17
        && s.starts_with('p')
        && s[1..].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
#[cfg(unix)]
fn export_filename(project: &str, members: usize) -> String {
    if members == 1 {
        "axel-memory.json".into()
    } else {
        format!("axel-memory-{project}.json")
    }
}
#[cfg(unix)]
fn validate_exact_export(value: &Value, project: &str) -> Result<()> {
    if value["format"] != "synaps-axel-export/1"
        || value["source_project"] != project
        || value["target_project"] != project
        || ![
            "records",
            "tombstones",
            "histories",
            "captures",
            "fingerprints",
        ]
        .iter()
        .all(|field| value[field].is_array())
        || !value["records"]
            .as_array()
            .is_some_and(|records| records.iter().all(|record| record["project"] == project))
    {
        bail!(
            "service export is not a complete exact-owner inventory; no final backup index written"
        );
    }
    Ok(())
}
#[cfg(unix)]
fn write_export_file(
    root: &agent_core::core::private_fs::ConfinedDir,
    name: &str,
    value: &Value,
) -> Result<(usize, String)> {
    use sha2::{Digest, Sha256};
    use std::io::Write;
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    if bytes.len() > EXPORT_FILE_MAX_BYTES {
        bail!("Axel export file exceeds the 24 MiB bound; no final backup index written");
    }
    let mut file = root.create_file(name)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    root.sync_all()?;
    Ok((bytes.len(), format!("{:x}", Sha256::digest(&bytes))))
}

fn require_policy(age: Option<u32>, bytes: Option<u64>) -> Result<()> {
    if age.is_none() && bytes.is_none() {
        bail!("sweep needs --max-age-days and/or --max-disk-bytes");
    }
    Ok(())
}
/// The Axel service's sweep deletes saved notes and nothing else: every note
/// older than `max_age_days`, then notes oldest first until `max_disk_bytes`
/// is met (`sidecars/axel-memory-service/src/retention.rs`, `sweep`). It has
/// no flag of its own for that, so an age or a budget reaches it only when
/// the command said `--include-notes`.
fn require_include_notes_for_axel(
    age: Option<u32>,
    bytes: Option<u64>,
    include_notes: bool,
) -> Result<()> {
    if include_notes {
        return Ok(());
    }
    let asked = match (age, bytes) {
        (Some(_), Some(_)) => "--max-age-days and --max-disk-bytes",
        (Some(_), None) => "--max-age-days",
        (None, Some(_)) => "--max-disk-bytes",
        (None, None) => return Ok(()),
    };
    bail!(
        "{asked} refused: with the Axel memory backend saved notes are all a sweep can \
         delete (history and tombstones are kept), so an age or a budget deletes notes. \
         Give --include-notes to allow that. Nothing was deleted."
    )
}
fn parse_domain(domain: &str) -> Result<RetentionDomain> {
    Ok(match domain {
        "sessions" => RetentionDomain::Sessions,
        "memory" => RetentionDomain::Memory,
        "memory-index" => RetentionDomain::MemoryIndex,
        "traces" => RetentionDomain::Traces,
        "logs" => RetentionDomain::Logs,
        _ => bail!("unknown domain — expected sessions|memory|memory-index|traces|logs (history also available with Axel)"),
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
    #[test]
    fn exact_scope_prefix_never_converted() {
        assert_eq!(
            selected_memory_id("project-p123", "project-p123:mem-a").unwrap(),
            "mem-a"
        );
        assert!(selected_memory_id("project-p123", "project-proj_123:mem-a").is_err());
        assert!(selected_memory_id("project-p123", "other:mem-a").is_err());
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
    /// Axel's sweep deletes notes and nothing else, so an age or a budget
    /// without `--include-notes` is refused before the service is asked: the
    /// service here does not exist, and only the sweep that is let through
    /// gets as far as finding that out.
    #[tokio::test]
    async fn the_axel_sweep_refuses_an_age_or_a_budget_without_include_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let config = MemoryBackendConfig {
            kind: MemoryBackendKind::Axel,
            executable: Some(tmp.path().join("missing-service")),
            brain: Some(tmp.path().join("brain.r8")),
            user_scope: false,
        };
        let sweep = |max_age_days, max_disk_bytes, include_notes| {
            let config = &config;
            async move {
                let action = RetentionAction::Sweep {
                    max_age_days,
                    max_disk_bytes,
                    include_notes,
                };
                run_memory_action(action, config).await.unwrap_err().to_string()
            }
        };
        for (age, bytes, named) in [
            (Some(30), None, "--max-age-days refused"),
            (None, Some(1_000), "--max-disk-bytes refused"),
            (Some(30), Some(1_000), "--max-age-days and --max-disk-bytes refused"),
        ] {
            let refused = sweep(age, bytes, false).await;
            assert!(refused.starts_with(named), "{refused}");
            assert!(refused.contains("notes are all a sweep can delete"), "{refused}");
            assert!(refused.contains("--include-notes"), "{refused}");
            // With the flag the sweep is not refused here: it goes on to
            // the service, which is not there.
            let let_through = sweep(age, bytes, true).await;
            assert!(!let_through.contains("--include-notes"), "{let_through}");
        }
        assert!(!tmp.path().join("brain.r8").exists(), "nothing was provisioned");
        // No age and no budget is still the first refusal.
        assert!(sweep(None, None, false).await.contains("sweep needs"));
    }
    #[cfg(unix)]
    fn write_axel_export(dest: &Path, value: &Value) -> Result<()> {
        let root =
            agent_core::core::private_fs::ConfinedDir::create_absolute_no_symlinks_durable(dest)?;
        write_export_file(&root, "axel-memory.json", value)?;
        Ok(())
    }
    #[cfg(unix)]
    #[test]
    fn export_is_private_nofollow_and_never_overwrites() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("export");
        let secret = json!({"records":[{"content":"synthetic-private-body"}]});
        write_axel_export(&dest, &secret).unwrap();
        let path = dest.join("axel-memory.json");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let before = std::fs::read(&path).unwrap();
        assert!(write_axel_export(&dest, &json!({})).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let link = tmp.path().join("link");
        symlink(&dest, &link).unwrap();
        assert!(write_axel_export(&link.join("escape"), &secret).is_err());
        assert!(!dest.join("escape").exists());
        std::fs::remove_file(&path).unwrap();
        symlink(tmp.path().join("victim"), &path).unwrap();
        assert!(write_axel_export(&dest, &secret).is_err());
        assert!(!tmp.path().join("victim").exists());
    }
}
