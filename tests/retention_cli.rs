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

/// `synaps retention export` with `memory.backend = axel`, against a real
/// helper named by `SYNAPS_AXEL_TEST_BIN` (synthetic temp state only).
mod axel_export {
    use serde_json::Value;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    struct Fixture {
        tmp: tempfile::TempDir,
        main: PathBuf,
        base: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let main = tmp.path().join("repository");
            let worktree = tmp.path().join("registered-worktree");
            let base = tmp.path().join("synthetic-base");
            fs::create_dir(&main).unwrap();
            fs::create_dir(&base).unwrap();
            fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
            let fixture = Self { tmp, main, base };
            fixture.git(&["init", "-q"]);
            fixture.git(&[
                "-c",
                "user.name=Synthetic",
                "-c",
                "user.email=synthetic@example.invalid",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "synthetic",
            ]);
            fixture.git(&[
                "worktree",
                "add",
                "-q",
                "-b",
                "synthetic-worktree",
                worktree.to_str().unwrap(),
            ]);
            fixture
        }
        fn git(&self, args: &[&str]) {
            let output = Command::new("/usr/bin/git")
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", self.tmp.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .arg("-c")
                .arg("core.hooksPath=/dev/null")
                .arg("-C")
                .arg(&self.main)
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        }
        fn brain(&self) -> PathBuf {
            self.base.join("shared.r8")
        }
        fn run(&self, cwd: &Path, args: &[&str]) -> Output {
            Command::new(env!("CARGO_BIN_EXE_synaps"))
                .current_dir(cwd)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", self.tmp.path())
                .env("SYNAPS_BASE_DIR", &self.base)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("XDG_CONFIG_HOME", self.tmp.path().join("xdg"))
                .env("XDG_CACHE_HOME", self.tmp.path().join("cache"))
                .arg("retention")
                .args(args)
                .output()
                .unwrap()
        }
    }

    fn real_executable() -> PathBuf {
        PathBuf::from(
            std::env::var_os("SYNAPS_AXEL_TEST_BIN").expect("explicit synthetic service binary"),
        )
    }

    fn select(f: &Fixture, exe: &Path, brain: &Path) {
        fs::write(
            f.base.join("config"),
            format!(
                "memory.backend = axel\nmemory.axel.executable = {}\nmemory.axel.brain = {}\n",
                exe.display(),
                brain.display()
            ),
        )
        .unwrap();
        fs::set_permissions(f.base.join("config"), fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn checked_backup(f: &Fixture, dest: &Path, expected: usize) -> Value {
        use sha2::{Digest, Sha256};
        let out = f.run(&f.main, &["export", dest.to_str().unwrap()]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(!String::from_utf8_lossy(&out.stdout).contains("synthetic-private"));
        let index: Value =
            serde_json::from_slice(&fs::read(dest.join("axel-memory-index.json")).unwrap())
                .unwrap();
        assert_eq!(index["complete"], true);
        assert_eq!(index["files"].as_array().unwrap().len(), expected);
        for file in index["files"].as_array().unwrap() {
            let path = dest.join(file["file"].as_str().unwrap());
            let bytes = fs::read(&path).unwrap();
            assert_eq!(file["sha256"], format!("{:x}", Sha256::digest(&bytes)));
            assert_eq!(file["bytes"], bytes.len());
            let inventory: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(inventory["source_project"], file["project"]);
            assert_eq!(inventory["target_project"], file["project"]);
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(fs::metadata(dest).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(dest.join("axel-memory-index.json")).unwrap().permissions().mode()
                & 0o777,
            0o600
        );
        assert!(!f.run(&f.main, &["export", dest.to_str().unwrap()]).status.success());
        index
    }

    #[test]
    #[ignore = "requires explicitly selected service binary; synthetic temp state only"]
    fn real_cli_single_member_backup_keeps_compatible_filename() {
        let f = Fixture::new();
        let exe = real_executable();
        select(&f, &exe, &f.brain());
        let dest = f.tmp.path().join("single-backup");
        let index = checked_backup(&f, &dest, 1);
        assert_eq!(index["files"][0]["file"], "axel-memory.json");
    }
}
