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

/// A config that still selects the removed Axel backend, all four keys
/// active. The helper and the brain it names do not exist.
fn old_axel_config(tmp: &Path) -> String {
    format!(
        "memory.backend = axel\nmemory.user_scope = true\nmemory.axel.executable = {}\n\
         memory.axel.brain = {}\ncontext_management.mode = auto\n",
        tmp.join("no-such-helper").display(),
        tmp.join("brain.r8").display(),
    )
}

/// Ruling 4, T-B8. With that config `synaps retention inspect | sweep |
/// export | forget` do what they do with no config: they act on the plain
/// files, print the same reports and never fail for the config. Before B-7
/// each of them asked the helper, and failed without it.
#[test]
fn retention_acts_on_plain_files_under_a_config_that_still_selects_axel() {
    let seeded = || {
        let tmp = tempfile::tempdir().unwrap();
        let (base, project) = (tmp.path().join("base"), tmp.path().join("project"));
        for dir in [base.join("sessions"), tmp.path().join("home"), project.clone()] {
            std::fs::create_dir_all(dir).unwrap();
        }
        session(&base.join("sessions"), "old-session", "2020-01-01T00:00:00Z");
        session(&base.join("sessions"), "kept-session", &chrono::Utc::now().to_rfc3339());
        tmp
    };
    let report = |tmp: &Path, args: &[&str]| -> Value {
        let run = synaps(tmp, args);
        assert!(run.status.success(), "{args:?}: {}", String::from_utf8_lossy(&run.stderr));
        serde_json::from_slice(&run.stdout).unwrap()
    };

    let old = seeded();
    let config = old_axel_config(old.path());
    std::fs::write(old.path().join("base/config"), &config).unwrap();
    let plain = seeded();

    // A note in each base, forgotten below by its plain-file id.
    let note_in = |tmp: &Path| {
        let scope = ProjectScope::for_root(&tmp.join("project")).unwrap();
        let note = store_record_in(
            &tmp.join("base"),
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
        (project_notes_file(&tmp.join("base"), scope.key()), format!("{}:{}", scope.namespace(), note.id.unwrap()))
    };

    let mut reports = Vec::new();
    for tmp in [old.path(), plain.path()] {
        let (notes, note_id) = note_in(tmp);
        let inspect = report(tmp, &["retention", "inspect"]);
        assert!(inspect["domains"].is_array(), "{inspect}");
        let dest = tmp.join("export");
        let export = report(tmp, &["retention", "export", dest.to_str().unwrap()]);
        assert!(export["files"].as_u64().unwrap() >= 3, "two sessions and the notes: {export}");
        // The plain-file export, not the helper's: no Axel index.
        assert!(!dest.join("axel-memory-index.json").exists());
        let sweep = report(tmp, &["retention", "sweep", "--max-age-days", "30"]);
        assert_eq!(sweep["deleted_files"], 1, "{sweep}");
        assert!(!tmp.join("base/sessions/old-session.json").exists());
        assert!(tmp.join("base/sessions/kept-session.json").exists());
        let before = std::fs::read(&notes).unwrap();
        let forget = report(tmp, &["retention", "forget", "memory", &note_id]);
        assert_eq!(forget, json!({"domain": "memory", "forgotten": true}));
        assert_ne!(std::fs::read(&notes).unwrap(), before, "the notes file was rewritten");
        // `history` was the Axel backend's domain; it is unknown now.
        let history = synaps(tmp, &["retention", "forget", "history", "ctx-0"]);
        assert!(!history.status.success());
        assert!(String::from_utf8_lossy(&history.stderr).contains("unknown domain"));
        // No report names a backend.
        for value in [&inspect, &export, &sweep] {
            assert!(value.get("backend").is_none() && value.get("omitted_domains").is_none(), "{value}");
        }
        let domains: Vec<Value> = inspect["domains"]
            .as_array()
            .unwrap()
            .iter()
            .map(|domain| json!([domain["domain"], domain["files"]]))
            .collect();
        reports.push((domains, export["files"].clone(), sweep["deleted_files"].clone()));
    }
    // The same with the old config as with none.
    assert_eq!(reports[0], reports[1]);
    // The config is as it was, and nothing of Axel's was made.
    assert_eq!(std::fs::read_to_string(old.path().join("base/config")).unwrap(), config);
    assert!(!old.path().join("brain.r8").exists());
    assert!(!old.path().join("no-such-helper").exists());
}

/// Ruling 4. A headless host (`synaps rpc`) under that config starts,
/// says the one warning once on stderr, and stops cleanly.
#[test]
fn a_headless_host_starts_under_a_config_that_still_selects_axel() {
    use std::io::Write;
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    for dir in [base.clone(), tmp.path().join("home")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(base.join("config"), old_axel_config(tmp.path())).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_synaps"))
        .arg("rpc")
        .current_dir(tmp.path())
        .env("SYNAPS_BASE_DIR", &base)
        .env("HOME", tmp.path().join("home"))
        .env("SYNAPS_DAEMON_AUTOSPAWN", "0")
        .env_remove("SYNAPS_PROFILE")
        .env_remove("SYNAPS_PROJECT_ROOT")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"{\"type\":\"shutdown\"}\n").unwrap();
    let run = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(run.status.success(), "{stderr}");
    let warnings: Vec<&str> =
        stderr.lines().filter(|line| line.starts_with("synaps: config warning: ")).collect();
    assert_eq!(warnings.len(), 1, "{stderr}");
    let version = agent_core::AXEL_BACKEND_REMOVED_IN;
    assert_eq!(
        warnings[0],
        format!(
            "synaps: config warning: memory.backend = axel is no longer supported: since \
             {version} Synaps keeps context archives and notes in plain files under its base \
             directory, always, and Axel is an extension. This session uses plain files. \
             Archives and notes saved to the Axel brain are not read any more: move them once \
             with \"axel synaps-export\" and \"synaps memory import-archives\" / \"import-notes\", \
             install the Axel extension for recall and the forum, then remove the \
             memory.backend, memory.user_scope and memory.axel.* lines. See \
             docs/upgrading/{version}.md"
        )
    );
    assert!(!stderr.contains("unavailable"), "{stderr}");
    assert!(!tmp.path().join("brain.r8").exists());
}
