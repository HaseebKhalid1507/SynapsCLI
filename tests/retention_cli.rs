//! `synaps retention` on plain files, through the built binary, with
//! `SYNAPS_BASE_DIR` and `HOME` in a temp folder of the test. The cutover
//! rehearsal runs `synaps retention sweep` on a plain-file base after the
//! files-to-Axel migration was removed (cut spec, B-5); these hold it to
//! that. Nothing here reads or writes `~/.synaps-cli`.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use agent_core::context_archive::{archive_file, ArchiveStore};
use agent_core::memory::store::{
    project_notes_file, store_record_in, MemoryProvenance, MemoryRetention, MemorySensitivity,
    NewMemoryRecord, ProjectScope,
};
use serde_json::{json, Value};

fn synaps(tmp: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_synaps"))
        .args(args)
        .env("SYNAPS_BASE_DIR", tmp.join("base"))
        .env("HOME", tmp.join("home"))
        .env("SYNAPS_DAEMON_AUTOSPAWN", "0")
        .env_remove("SYNAPS_PROFILE")
        .env_remove("SYNAPS_PROJECT_ROOT")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn session(dir: &Path, id: &str, updated_at: &str) -> PathBuf {
    let path = dir.join(format!("{id}.json"));
    std::fs::write(&path, json!({"id": id, "updated_at": updated_at}).to_string()).unwrap();
    path
}

#[test]
fn a_sweep_works_on_plain_files_and_keeps_notes_and_archives() {
    let tmp = tempfile::tempdir().unwrap();
    let (base, project) = (tmp.path().join("base"), tmp.path().join("project"));
    for dir in [base.join("sessions"), tmp.path().join("home"), project.clone()] {
        std::fs::create_dir_all(dir).unwrap();
    }
    // The config of a cutover: the memory lines are commented out.
    std::fs::write(
        base.join("config"),
        "# memory.backend = axel\n# memory.axel.executable = /nowhere/axel\n# memory.axel.brain = /nowhere/brain.r8\n",
    )
    .unwrap();
    let old = session(&base.join("sessions"), "old-session", "2020-01-01T00:00:00Z");
    let new = session(&base.join("sessions"), "new-session", &chrono::Utc::now().to_rfc3339());
    let scope = ProjectScope::for_root(&project).unwrap();
    let note = store_record_in(
        &base,
        &scope,
        NewMemoryRecord {
            content: "a saved note".into(),
            tags: vec![],
            provenance: MemoryProvenance { source: "user".into(), session: None },
            sensitivity: MemorySensitivity::Normal,
            retention: MemoryRetention::Standard,
        },
    )
    .unwrap();
    let notes = project_notes_file(&base, scope.key());
    let notes_before = std::fs::read(&notes).unwrap();
    let store = ArchiveStore::new(&base, "synthetic-project", "synthetic-chat").unwrap();
    let message = Arc::new(json!({"role": "user", "content": "an archived window"}));
    let sealed = store.seal(&[message], "").unwrap();
    let archive = archive_file(&base, "synthetic-project", &sealed.id).unwrap().unwrap();
    let archive_before = std::fs::read(&archive).unwrap();

    let run = synaps(tmp.path(), &["retention", "inspect"]);
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    let report: Value = serde_json::from_slice(&run.stdout).unwrap();
    assert!(report["domains"].is_array(), "{report}");

    let run = synaps(tmp.path(), &["retention", "sweep", "--max-age-days", "30"]);
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    let outcome: Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(outcome["deleted_files"], 1, "{outcome}");
    assert!(!old.exists());
    assert!(new.exists());
    // Notes are not deleted by age without --include-notes, even when every
    // one is older than the age; archives are not the sweep's.
    let run = synaps(tmp.path(), &["retention", "sweep", "--max-age-days", "0"]);
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    assert!(!new.exists());
    assert_eq!(std::fs::read(&notes).unwrap(), notes_before);
    assert!(note.id.is_some());
    assert_eq!(std::fs::read(&archive).unwrap(), archive_before);
}

#[test]
fn the_removed_migration_actions_are_unknown_to_retention() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("home")).unwrap();
    for removed in ["memory-scope", "upgrade-memory", "migrate-memory"] {
        let run = synaps(tmp.path(), &["retention", removed]);
        assert_eq!(run.status.code(), Some(2), "{removed}");
        assert!(run.stdout.is_empty(), "{removed}");
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(
            stderr.starts_with(&format!("error: unrecognized subcommand '{removed}'")),
            "{removed}: {stderr}"
        );
        assert!(!tmp.path().join("base").exists(), "{removed}: nothing was created");
    }
}
