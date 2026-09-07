//! Operator-only retention and explicit, reversible memory migration.
//!
//! Legacy mode retains the original all-domain behavior. In Axel mode inventory,
//! sweep and export cover ONLY selected project memory/history; each response
//! lists omitted legacy domains. Explicit session/trace/log forget remains legacy.
//! No operator export (which can contain secret bodies) is exposed as a model tool.

use agent_engine::memory_backend::{migration, repository_migration, MemoryBinding};
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
        /// Delete artifacts older than this many days.
        #[arg(long)]
        max_age_days: Option<u32>,
        /// Requested disk budget; protected content can leave this unmet.
        #[arg(long)]
        max_disk_bytes: Option<u64>,
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
        /// for notes). Legacy memory: namespace:mem-id.
        id: String,
    },
    /// Report verified repository/worktree identity metadata without opening memory.
    MemoryScope {
        /// Query service membership too (opens selected brain); default is identity only.
        #[arg(long, conflicts_with_all = ["link", "apply", "dry_run"])]
        service: bool,
        /// Explicit original verified scope key to link; repeat per source. Preview only by default.
        #[arg(long, action = clap::ArgAction::Append)]
        link: Vec<String>,
        #[arg(long, conflicts_with = "apply")]
        dry_run: bool,
        #[arg(long, requires_all = ["link", "manifest_digest", "project"])]
        apply: bool,
        #[arg(long, requires = "apply")]
        manifest_digest: Option<String>,
        /// Exact canonical repository key from identity report.
        #[arg(long)]
        project: Option<String>,
    },
    /// Preview explicit schema-marker upgrade of the existing configured pinned .r8.
    /// Stop/checkpoint writers first. Never imports data, links aliases or changes config.
    UpgradeMemory {
        #[arg(long, conflicts_with = "apply")]
        dry_run: bool,
        #[arg(long, requires = "manifest_digest")]
        apply: bool,
        #[arg(long, requires = "apply")]
        manifest_digest: Option<String>,
        /// Exact original pinned project key (must be a verified repository scope).
        #[arg(long)]
        project: String,
    },
    /// Preview legacy -> shared Axel migration; apply needs the exact preview digest.
    /// Never switches config, deletes sources or enables dual writes. Stop legacy
    /// writers before preview/apply. Sources and hidden archive notes stay private.
    MigrateMemory {
        /// Explicit preview (also the default); no destination is opened.
        #[arg(long, conflicts_with = "apply")]
        dry_run: bool,
        /// Apply separate bounded source transactions, then explicit alias links; may partially complete.
        #[arg(long, requires_all = ["manifest_digest", "project"])]
        apply: bool,
        /// Exact lowercase SHA-256 printed by the current preview.
        #[arg(long, requires = "apply")]
        manifest_digest: Option<String>,
        /// Exact canonical repository key, never a proj_ prefix conversion.
        #[arg(long)]
        project: Option<String>,
        /// Explicit absolute legacy plugin brain, opened read-only by the service.
        #[arg(long, conflicts_with = "source_export", requires_all = ["source_project", "target_project", "project"])]
        source_brain: Option<PathBuf>,
        /// Private full JSON inventory from an operator-selected read-only exporter.
        #[arg(long, conflicts_with = "source_brain", requires_all = ["source_project", "target_project", "project"])]
        source_export: Option<PathBuf>,
        /// Exact authoritative source project alias; required for plugin import.
        #[arg(long)]
        source_project: Option<String>,
        /// Explicit verified original host scope receiving this source (not canonical rewriting).
        /// Native host exports: must equal --source-project; plugin aliases map per source.
        #[arg(long)]
        target_project: Option<String>,
    },
}

pub fn run(action: RetentionAction) -> Result<()> {
    let config = synaps_cli::config::load_config().memory_backend;
    if config.kind == MemoryBackendKind::Unavailable {
        bail!("selected memory backend is unavailable; no legacy retention fallback");
    }
    if !matches!(
        action,
        RetentionAction::MigrateMemory { .. }
            | RetentionAction::MemoryScope { .. }
            | RetentionAction::UpgradeMemory { .. }
    ) && config.kind == MemoryBackendKind::Legacy
    {
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
                    .block_on(run_selected(action, &config))
            })
            .join()
            .map_err(|_| anyhow::anyhow!("retention operator worker failed"))?
    })
}

async fn run_selected(action: RetentionAction, config: &MemoryBackendConfig) -> Result<()> {
    // Operator operations may prepare cutover while selection remains legacy.
    // This temporary binding never persists config or changes runtime selection.
    match action {
        RetentionAction::MemoryScope {
            service,
            link,
            apply,
            manifest_digest,
            project,
            ..
        } => {
            let binding = operator_binding(config);
            confirm_project(&binding, project.as_deref(), apply)?;
            if service {
                let info = repository_migration::scope_info(&binding).await?;
                return print_json(
                    &json!({"identity":repository_migration::report(&binding)?, "service":info}),
                );
            }
            if link.is_empty() {
                return print_json(&serde_json::to_value(repository_migration::report(
                    &binding,
                )?)?);
            }
            if apply {
                return print_apply(
                    &repository_migration::apply_link(
                        &binding,
                        &link,
                        manifest_digest
                            .as_deref()
                            .context("apply requires preview digest")?,
                    )
                    .await?,
                );
            }
            print_json(&serde_json::to_value(repository_migration::preview_link(
                &binding, &link,
            )?)?)
        }
        RetentionAction::UpgradeMemory {
            apply,
            manifest_digest,
            project,
            ..
        } => {
            let binding = operator_binding(config);
            if apply {
                let info = repository_migration::apply_upgrade(
                    &binding,
                    &project,
                    manifest_digest
                        .as_deref()
                        .context("apply requires preview digest")?,
                )
                .await?;
                return print_json(
                    &json!({"upgraded":true,"service":info,"config_unchanged":true,
                    "identities_unchanged":true,"aliases_linked":false,"data_imported":false}),
                );
            }
            print_json(&serde_json::to_value(
                repository_migration::preview_upgrade(&binding, &project)?,
            )?)
        }
        RetentionAction::MigrateMemory {
            apply,
            manifest_digest,
            project,
            source_brain,
            source_export,
            source_project,
            target_project,
            ..
        } => {
            let binding = operator_binding(config);
            confirm_project(&binding, project.as_deref(), apply)?;
            let source = match (source_brain, source_export) {
                (Some(path), None) => migration::MigrationSource::Brain {
                    path,
                    project: source_project.context("--source-project required")?,
                },
                (None, Some(path)) => migration::MigrationSource::Export {
                    path,
                    project: source_project.context("--source-project required")?,
                },
                (None, None) => {
                    if source_project.is_some() || target_project.is_some() {
                        bail!("source/target mapping requires an explicit --source-brain or --source-export");
                    }
                    migration::MigrationSource::Builtin
                }
                _ => bail!("choose exactly one external source"),
            };
            if matches!(source, migration::MigrationSource::Builtin) {
                if apply {
                    return print_apply(
                        &repository_migration::apply_migration(
                            &binding,
                            manifest_digest
                                .as_deref()
                                .context("apply requires preview digest")?,
                        )
                        .await?,
                    );
                }
                return print_json(&serde_json::to_value(
                    repository_migration::preview_migration(&binding)?,
                )?);
            }
            if project.is_none() {
                bail!("external import requires explicit canonical --project");
            }
            let target = target_project
                .context("external import requires per-source --target-project mapping")?;
            if apply {
                return print_apply(
                    &repository_migration::apply_external(
                        &binding,
                        &source,
                        &target,
                        manifest_digest
                            .as_deref()
                            .context("apply requires preview digest")?,
                    )
                    .await?,
                );
            }
            return print_json(&serde_json::to_value(
                repository_migration::preview_external(&binding, &source, &target).await?,
            )?);
        }
        action => return run_memory_action(action, config).await,
    }
}

fn operator_binding(config: &MemoryBackendConfig) -> MemoryBinding {
    let mut destination = config.clone();
    destination.kind = MemoryBackendKind::Axel;
    MemoryBinding::from_config(&destination)
}
fn confirm_project(binding: &MemoryBinding, project: Option<&str>, required: bool) -> Result<()> {
    if (required && project.is_none())
        || project.is_some_and(|p| p != binding.scope().map(|s| s.key()).unwrap_or(""))
    {
        bail!(
            "--project must exactly match the canonical repository key {}",
            binding.scope()?.key()
        );
    }
    Ok(())
}
fn print_apply(report: &repository_migration::ApplyReport) -> Result<()> {
    print_json(&serde_json::to_value(report)?)?;
    if !report.complete {
        bail!("operation incomplete; partial progress/unknown commit possible; reconcile the metadata report before retry");
    }
    Ok(())
}

async fn run_memory_action(action: RetentionAction, config: &MemoryBackendConfig) -> Result<()> {
    let binding = MemoryBinding::from_config(config);
    match action {
        RetentionAction::Inspect => {
            let stats = binding.rpc("stats", json!({})).await?;
            print_json(&selected_report(&binding, stats)?)?;
        }
        RetentionAction::Sweep { max_age_days, max_disk_bytes } => {
            require_policy(max_age_days, max_disk_bytes)?;
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
        RetentionAction::MigrateMemory { .. } | RetentionAction::MemoryScope { .. } | RetentionAction::UpgradeMemory { .. } => unreachable!(),
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
    let info = repository_migration::scope_info(binding).await?;
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
    let after = repository_migration::scope_info(binding).await?;
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
const EXPORT_INDEX: &str = "axel-memory-index.json";
fn export_filename(project: &str, members: usize) -> String {
    if members == 1 {
        "axel-memory.json".into()
    } else {
        format!("axel-memory-{project}.json")
    }
}
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
    if bytes.len() > migration::MAX_APPLY_BYTES {
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
        } => {
            require_policy(max_age_days, max_disk_bytes)?;
            print_json(&serde_json::to_value(retention::sweep(
                roots,
                &RetentionPolicy {
                    max_age_days,
                    max_disk_bytes,
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
        RetentionAction::MigrateMemory { .. }
        | RetentionAction::MemoryScope { .. }
        | RetentionAction::UpgradeMemory { .. } => unreachable!(),
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
    fn migration_defaults_to_preview_and_apply_requires_digest_and_project() {
        use clap::Parser;
        assert!(matches!(
            Cli::try_parse_from(["retention", "migrate-memory"])
                .unwrap()
                .action,
            RetentionAction::MigrateMemory { apply: false, .. }
        ));
        assert!(Cli::try_parse_from(["retention", "migrate-memory", "--apply"]).is_err());
        assert!(
            Cli::try_parse_from(["retention", "migrate-memory", "--dry-run", "--apply"]).is_err()
        );
        assert!(Cli::try_parse_from([
            "retention",
            "migrate-memory",
            "--source-brain",
            "/tmp/source.r8"
        ])
        .is_err());
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
