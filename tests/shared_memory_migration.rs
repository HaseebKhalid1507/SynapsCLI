//! Synthetic-only repository migration/identity tests. Never read user config,
//! HOME, real repositories or private memory. Service tests use explicit fixtures.

use agent_core::memory::store::ProjectScope;
use agent_engine::memory_backend::repository_migration::{identity_report, preview_builtin_in};
use serde_json::json;
use std::fs;

fn scopes() -> (tempfile::TempDir, ProjectScope, ProjectScope) {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    let worktree = tmp.path().join("worktree");
    fs::create_dir(&main).unwrap();
    fs::create_dir(&worktree).unwrap();
    (
        tmp,
        ProjectScope::for_root(&main).unwrap(),
        ProjectScope::for_root(&worktree).unwrap(),
    )
}

#[test]
fn metadata_mapping_is_order_independent_and_binds_exact_members() {
    let (_tmp, main, worktree) = scopes();
    let first = identity_report(&main, &main, &[main.clone(), worktree.clone()]).unwrap();
    let second = identity_report(&main, &worktree, &[worktree.clone(), main.clone()]).unwrap();
    assert_eq!(first.mapping_digest, second.mapping_digest);
    assert_eq!(first.members, second.members);
    assert_ne!(first.legacy_project, second.legacy_project);
    assert_eq!(first.members.len(), 2);
    assert_eq!(first.canonical_project, main.key());
    assert_ne!(
        first.mapping_digest,
        identity_report(&main, &main, &[]).unwrap().mapping_digest
    );
    assert!(identity_report(&main, &worktree, &[]).is_err());
}

#[cfg(unix)]
#[test]
fn builtin_preview_preserves_original_scopes_and_excludes_unverified_sources() {
    let (tmp, main, worktree) = scopes();
    let base = tmp.path().join("synthetic-home");
    fs::create_dir_all(base.join("memory")).unwrap();
    let outside = tmp.path().join("unrelated");
    fs::create_dir(&outside).unwrap();
    let outside = ProjectScope::for_root(&outside).unwrap();
    for scope in [&main, &worktree, &outside] {
        let record = json!({
            "namespace":scope.namespace(),"timestamp_ms":1000,
            "content":"synthetic-private-body","tags":["synthetic-private-tag"],
            "id":"mem-synthetic","project":scope.key(),
            "provenance":{"source":"user"},"sensitivity":"secret","retention":"standard"
        });
        fs::write(
            base.join("memory")
                .join(format!("{}.jsonl", scope.namespace())),
            format!("{record}\n"),
        )
        .unwrap();
    }
    let verified = [main.clone(), worktree.clone()];
    let preview = preview_builtin_in(&base, &main, &worktree, &verified).unwrap();
    assert_eq!(preview.sources.len(), 2);
    assert!(preview
        .sources
        .iter()
        .all(|s| s.records == 1 && s.secret_records == 1));
    assert!(preview.sources.iter().any(|s| s.project == worktree.key()));
    assert!(!preview.sources.iter().any(|s| s.project == outside.key()));
    let metadata = serde_json::to_string(&preview).unwrap();
    assert!(!metadata.contains("synthetic-private"));
    assert!(!metadata.contains("mem-synthetic"));
    assert!(preview.atomicity.contains("partial progress"));
    let again = preview_builtin_in(&base, &main, &main, &verified).unwrap();
    assert_eq!(again.manifest_digest, preview.manifest_digest);
    fs::write(
        base.join("memory")
            .join(format!("{}.jsonl", worktree.namespace())),
        "{\"tombstone\":\"mem-synthetic\",\"timestamp_ms\":2}\n",
    )
    .unwrap();
    assert_ne!(
        preview.manifest_digest,
        preview_builtin_in(&base, &main, &main, &verified)
            .unwrap()
            .manifest_digest
    );
    assert!(!base.join("context-archives").exists());
}

#[cfg(unix)]
#[test]
fn empty_sources_are_bound_without_creating_storage() {
    let (tmp, main, worktree) = scopes();
    let base = tmp.path().join("absent-home");
    let preview = preview_builtin_in(&base, &main, &main, &[worktree]).unwrap();
    assert_eq!(preview.sources.len(), 2);
    assert!(preview
        .sources
        .iter()
        .all(|s| !s.source_jsonl_present && s.records == 0));
    assert!(!base.exists());
}

#[cfg(unix)]
mod cli {
    use super::*;
    use serde_json::Value;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    struct Fixture {
        tmp: tempfile::TempDir,
        main: PathBuf,
        worktree: PathBuf,
        base: PathBuf,
        executable: PathBuf,
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
            let fixture = Self {
                executable: tmp.path().join("mock-service"),
                tmp,
                main,
                worktree,
                base,
            };
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
                fixture.worktree.to_str().unwrap(),
            ]);
            fixture.configure(&fixture.executable);
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
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        fn configure(&self, executable: &Path) {
            // Explicit synthetic config only; normal backend selection remains
            // legacy throughout operator migration and never gets rewritten.
            fs::write(
                self.base.join("config"),
                format!(
                "memory.backend = legacy\nmemory.axel.executable = {}\nmemory.axel.brain = {}\n",
                executable.display(), self.brain().display()),
            )
            .unwrap();
            fs::set_permissions(self.base.join("config"), fs::Permissions::from_mode(0o600))
                .unwrap();
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
        fn json(&self, cwd: &Path, args: &[&str]) -> Value {
            let out = self.run(cwd, args);
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            serde_json::from_slice(&out.stdout).unwrap()
        }
        fn identity(&self) -> Value {
            self.json(&self.main, &["memory-scope"])
        }
        fn legacy(&self, root: &Path) -> ProjectScope {
            ProjectScope::for_root(root).unwrap()
        }
        fn source(&self, root: &Path, id: &str) -> PathBuf {
            let scope = self.legacy(root);
            fs::create_dir_all(self.base.join("memory")).unwrap();
            let path = self
                .base
                .join("memory")
                .join(format!("{}.jsonl", scope.namespace()));
            let r = json!({"namespace":scope.namespace(),"timestamp_ms":1000,"content":"synthetic-private-body",
                "tags":[],"id":id,"project":scope.key(),"provenance":{"source":"user"},"sensitivity":"normal","retention":"standard"});
            fs::write(&path, format!("{r}\n")).unwrap();
            path
        }
        fn mock(&self, fail_project: Option<&str>) {
            // The mock logs synthetic payloads so tests can assert that host
            // migration never rewrites source identity and sets --operator only
            // for explicit alias/upgrade operations. Not a storage semantics test.
            let program = format!(
                r##"#!/usr/bin/python3
import json, sys, os
log = {log:?}
fail = {fail:?}
a = sys.argv
project = a[a.index('--project') + 1]
hello = json.loads(sys.stdin.readline())
def reply(result, ok=True):
    v = dict(schema='synaps-axel/2', project=project, ok=ok)
    v['result' if ok else 'error'] = result
    print(json.dumps(v), flush=True)
reply(dict(backend='axel', revision='edbdea401d66feedb87fcad28c879ece54e3ccd2', contract='synaps-axel/2'))
r = json.loads(sys.stdin.readline())
with open(log, 'a') as f:
    f.write(json.dumps(dict(request=r, operator='--operator' in a)) + '\n')
op = r['operation']
if op == 'migration_apply' and project == fail:
    reply('synthetic_failure', False)
elif op == 'scope_info':
    reply(dict(canonical_project=project, members=[project], mode='multi_project', user_scope=False))
elif op == 'scope_alias':
    reply(dict(canonical_project=project, members=[project, r['payload']['alias_project']], mode='multi_project', user_scope=False))
elif op == 'scope_upgrade':
    reply(dict(canonical_project=project, members=[project], mode='multi_project', user_scope=False))
else:
    reply(dict(committed=True))
"##,
                log = self.tmp.path().join("calls.jsonl").to_str().unwrap(),
                fail = fail_project.unwrap_or("")
            );
            fs::write(&self.executable, program).unwrap();
            fs::set_permissions(&self.executable, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fn calls(&self) -> Vec<Value> {
            fs::read_to_string(self.tmp.path().join("calls.jsonl"))
                .unwrap_or_default()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }
    }

    #[test]
    fn identity_and_previews_never_open_memory_and_require_exact_mapping_consent() {
        let f = Fixture::new();
        let identity = f.identity();
        let worker = f.json(&f.worktree, &["memory-scope"]);
        assert_eq!(identity["canonical_project"], worker["canonical_project"]);
        assert_eq!(identity["mapping_digest"], worker["mapping_digest"]);
        assert_eq!(identity["members"].as_array().unwrap().len(), 3);
        assert_ne!(identity["canonical_project"], f.legacy(&f.main).key());
        let marker: Value = serde_json::from_slice(
            &fs::read(f.main.join(".git/synaps-memory-identity.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(marker["version"], 2);
        let project = identity["canonical_project"].as_str().unwrap();
        let alias = f.legacy(&f.worktree);
        let link = f.json(&f.main, &["memory-scope", "--link", alias.key()]);
        assert!(!f.brain().exists());
        assert!(f.calls().is_empty());
        let stale = f.run(
            &f.main,
            &[
                "memory-scope",
                "--link",
                alias.key(),
                "--apply",
                "--project",
                project,
                "--manifest-digest",
                &"0".repeat(64),
            ],
        );
        assert!(!stale.status.success());
        assert!(f.calls().is_empty());
        f.mock(None);
        let applied = f.json(
            &f.main,
            &[
                "memory-scope",
                "--link",
                alias.key(),
                "--apply",
                "--project",
                project,
                "--manifest-digest",
                link["manifest_digest"].as_str().unwrap(),
            ],
        );
        assert_eq!(applied["complete"], true);
        let calls = f.calls();
        let alias_call = calls
            .iter()
            .find(|c| c["request"]["operation"] == "scope_alias")
            .unwrap();
        assert_eq!(alias_call["operator"], true);
        assert_eq!(
            alias_call["request"]["payload"]["alias_project"],
            alias.key()
        );
        assert!(!f
            .run(&f.main, &["memory-scope", "--apply"])
            .status
            .success());
        assert!(!f
            .run(
                &f.main,
                &["upgrade-memory", "--project", project, "--apply"]
            )
            .status
            .success());
    }

    #[test]
    fn repository_batch_preserves_sources_and_reports_partial_progress_without_global_claim() {
        let f = Fixture::new();
        let main_file = f.source(&f.main, "mem-main");
        let worker_file = f.source(&f.worktree, "mem-worker");
        let before = (
            fs::read(&main_file).unwrap(),
            fs::read(&worker_file).unwrap(),
            fs::read(f.base.join("config")).unwrap(),
        );
        let preview = f.json(&f.main, &["migrate-memory"]);
        let sources = preview["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 3);
        let fail_project = sources[1]["project"].as_str().unwrap();
        f.mock(Some(fail_project));
        let canonical = preview["linking"]["identity"]["canonical_project"]
            .as_str()
            .unwrap();
        let out = f.run(
            &f.worktree,
            &[
                "migrate-memory",
                "--apply",
                "--project",
                canonical,
                "--manifest-digest",
                preview["manifest_digest"].as_str().unwrap(),
            ],
        );
        assert!(!out.status.success());
        let report: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["complete"], false);
        assert_eq!(report["imported_projects"].as_array().unwrap().len(), 1);
        assert_eq!(report["failed_project"], fail_project);
        assert_eq!(report["failed_operation"], "migration_apply");
        assert_eq!(report["linked_projects"], json!([]));
        assert_eq!(report["outcome_unconfirmed"], true);
        for call in f.calls() {
            let r = &call["request"];
            assert_eq!(r["operation"], "migration_apply");
            for record in r["payload"]["records"].as_array().unwrap() {
                assert_eq!(record["project"], r["project"]);
            }
        }
        assert_eq!(fs::read(main_file).unwrap(), before.0);
        assert_eq!(fs::read(worker_file).unwrap(), before.1);
        assert_eq!(fs::read(f.base.join("config")).unwrap(), before.2);
    }

    #[test]
    fn changed_source_or_mapping_refuses_all_imports_before_service() {
        let f = Fixture::new();
        f.mock(None);
        f.source(&f.main, "mem-main");
        let preview = f.json(&f.main, &["migrate-memory"]);
        let canonical = preview["linking"]["identity"]["canonical_project"]
            .as_str()
            .unwrap();
        f.source(&f.worktree, "mem-late");
        let out = f.run(
            &f.main,
            &[
                "migrate-memory",
                "--apply",
                "--project",
                canonical,
                "--manifest-digest",
                preview["manifest_digest"].as_str().unwrap(),
            ],
        );
        assert!(!out.status.success());
        assert!(f.calls().is_empty());
        let preview = f.json(&f.main, &["migrate-memory"]);
        let another = f.tmp.path().join("new-worktree");
        f.git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "another",
            another.to_str().unwrap(),
        ]);
        let out = f.run(
            &f.main,
            &[
                "migrate-memory",
                "--apply",
                "--project",
                canonical,
                "--manifest-digest",
                preview["manifest_digest"].as_str().unwrap(),
            ],
        );
        assert!(!out.status.success());
        assert!(f.calls().is_empty());
    }

    #[test]
    fn upgrade_preview_hashes_file_and_checks_wal_without_service_or_mutation() {
        let f = Fixture::new();
        f.mock(None);
        let project = f.legacy(&f.main);
        fs::write(f.brain(), b"synthetic-not-a-real-db").unwrap();
        fs::set_permissions(f.brain(), fs::Permissions::from_mode(0o600)).unwrap();
        let preview = f.json(&f.main, &["upgrade-memory", "--project", project.key()]);
        assert!(f.calls().is_empty());
        fs::write(f.brain(), b"synthetic-changed-db").unwrap();
        let out = f.run(
            &f.main,
            &[
                "upgrade-memory",
                "--project",
                project.key(),
                "--apply",
                "--manifest-digest",
                preview["manifest_digest"].as_str().unwrap(),
            ],
        );
        assert!(!out.status.success());
        assert!(f.calls().is_empty());
        let wal = f.base.join("shared.r8-wal");
        fs::write(&wal, b"synthetic-dirty-wal").unwrap();
        fs::set_permissions(&wal, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!f
            .run(&f.main, &["upgrade-memory", "--project", project.key()])
            .status
            .success());
        fs::remove_file(&wal).unwrap();
        std::os::unix::fs::symlink(f.brain(), &wal).unwrap();
        assert!(!f
            .run(&f.main, &["upgrade-memory", "--project", project.key()])
            .status
            .success());
        fs::remove_file(wal).unwrap();
        let preview = f.json(&f.main, &["upgrade-memory", "--project", project.key()]);
        let applied = f.json(
            &f.main,
            &[
                "upgrade-memory",
                "--project",
                project.key(),
                "--apply",
                "--manifest-digest",
                preview["manifest_digest"].as_str().unwrap(),
            ],
        );
        assert_eq!(applied["upgraded"], true);
        assert_eq!(f.calls()[0]["request"]["operation"], "scope_upgrade");
        assert_eq!(f.calls()[0]["operator"], true);
    }

    #[test]
    fn moved_repository_retains_historical_sources_without_recanonicalizing_them() {
        let f = Fixture::new();
        let identity = f.identity();
        f.source(&f.main, "mem-before-move");
        // Remove worker registration first so a rename is a valid synthetic move.
        f.git(&["worktree", "remove", f.worktree.to_str().unwrap()]);
        let moved = f.tmp.path().join("moved-repository");
        fs::rename(&f.main, &moved).unwrap();
        let after = f.json(&moved, &["memory-scope"]);
        assert_eq!(identity["canonical_project"], after["canonical_project"]);
        assert_ne!(identity["mapping_digest"], after["mapping_digest"]);
        let preview = f.json(&moved, &["migrate-memory"]);
        assert!(preview["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["project"] == identity["legacy_project"] && s["records"] == 1));
        assert!(!f.brain().exists());
    }
    fn real_executable() -> PathBuf {
        PathBuf::from(
            std::env::var_os("SYNAPS_AXEL_TEST_BIN").expect("explicit synthetic service binary"),
        )
    }
    fn rpc(exe: &Path, brain: &Path, project: &str, op: &str, payload: Value) -> Value {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = Command::new(exe)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .args(["--brain", brain.to_str().unwrap(), "--project", project])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        for (operation, payload) in [("hello", json!({})), (op, payload)] {
            writeln!(stdin, "{}", json!({"schema":"synaps-axel/2","project":project,"operation":operation,"payload":payload})).unwrap();
        }
        drop(stdin);
        let out = child.wait_with_output().unwrap();
        let reply: Value = serde_json::from_str(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .last()
                .expect("reply"),
        )
        .unwrap();
        assert_eq!(reply["ok"], true, "{op}: {reply}");
        reply["result"].clone()
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
    fn import(f: &Fixture, canonical: &str, owner: &str, path: &Path, brain: bool) -> Value {
        let mut args = vec![
            "migrate-memory",
            "--project",
            canonical,
            if brain {
                "--source-brain"
            } else {
                "--source-export"
            },
            path.to_str().unwrap(),
            "--source-project",
            owner,
            "--target-project",
            owner,
        ];
        let preview = f.json(&f.main, &args);
        let text = serde_json::to_string(&preview).unwrap();
        assert!(!text.contains("synthetic-private"));
        args.extend([
            "--apply",
            "--manifest-digest",
            preview["manifest_digest"].as_str().unwrap(),
        ]);
        let result = f.json(&f.main, &args);
        assert_eq!(result["complete"], true);
        result
    }
    fn checked_backup(f: &Fixture, dest: &Path, expected: usize) -> Value {
        use sha2::{Digest, Sha256};
        let out = f.run(&f.main, &["export", dest.to_str().unwrap()]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
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
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            fs::metadata(dest).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(dest.join("axel-memory-index.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(!f
            .run(&f.main, &["export", dest.to_str().unwrap()])
            .status
            .success());
        index
    }

    #[test]
    #[ignore = "requires explicitly selected service binary; synthetic temp state only"]
    fn real_cli_group_export_restore_original_lineage_bodyless_and_retry() {
        use agent_core::context_archive::ArchiveStore;
        let f = Fixture::new();
        let exe = real_executable();
        f.configure(&exe);
        let identity = f.identity();
        let canonical = identity["canonical_project"].as_str().unwrap();
        let main = f.legacy(&f.main);
        let worker = f.legacy(&f.worktree);
        assert_ne!(canonical, main.key());
        let main_file = f.source(&f.main, "mem-main");
        let worker_file = f.source(&f.worktree, "mem-worker");
        let archive = ArchiveStore::new(&f.base, main.key(), "synthetic-logical").unwrap();
        archive
            .seal(
                &[std::sync::Arc::new(
                    json!({"role":"user","content":"synthetic-private-history"}),
                )],
                "synthetic-private-hidden-note",
            )
            .unwrap();
        let source_bytes = fs::read(&main_file).unwrap();
        let worker_bytes = fs::read(&worker_file).unwrap();
        let config_bytes = fs::read(f.base.join("config")).unwrap();
        let preview = f.json(&f.main, &["migrate-memory"]);
        let args = [
            "migrate-memory",
            "--apply",
            "--project",
            canonical,
            "--manifest-digest",
            preview["manifest_digest"].as_str().unwrap(),
        ];
        assert_eq!(f.json(&f.worktree, &args)["complete"], true);
        assert_eq!(f.json(&f.main, &args)["complete"], true);
        assert_eq!(fs::read(&main_file).unwrap(), source_bytes);
        assert_eq!(fs::read(&worker_file).unwrap(), worker_bytes);
        assert_eq!(fs::read(f.base.join("config")).unwrap(), config_bytes);
        let capture_id = "a".repeat(64);
        rpc(
            &exe,
            &f.brain(),
            worker.key(),
            "capture",
            json!({"schema":"chat_turn_capture/1",
            "capture_id":capture_id,"project_id":worker.key(),"session_id":"synthetic","turn_id":"one","turn_ordinal":1,
            "source_digest":"b".repeat(64),"user":"synthetic-private-question","assistant":"synthetic-private-answer","tools":[]}),
        );
        select(&f, &exe, &f.brain());
        let backup = f.tmp.path().join("backup");
        let index = checked_backup(&f, &backup, 3);
        let inventories: Vec<_> = index["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| {
                let path = backup.join(file["file"].as_str().unwrap());
                let inventory: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                (
                    file["project"].as_str().unwrap().to_owned(),
                    path,
                    inventory,
                )
            })
            .collect();
        let main_inventory = &inventories
            .iter()
            .find(|(p, _, _)| p == main.key())
            .unwrap()
            .2;
        let worker_inventory = &inventories
            .iter()
            .find(|(p, _, _)| p == worker.key())
            .unwrap()
            .2;
        assert_eq!(main_inventory["histories"].as_array().unwrap().len(), 1);
        assert_eq!(main_inventory["records"].as_array().unwrap().len(), 1);
        assert_eq!(
            worker_inventory["captures"][0]["evidence"]["project_id"],
            worker.key()
        );
        assert_eq!(worker_inventory["histories"], json!([]));
        // A real exact-owner read-only native source must remain byte-identical.
        let source_brain_bytes = fs::read(f.brain()).unwrap();
        let restored = f.base.join("restored.r8");
        select(&f, &exe, &restored);
        import(&f, canonical, main.key(), &f.brain(), true);
        assert_eq!(fs::read(f.brain()).unwrap(), source_brain_bytes);
        for (owner, path, _) in &inventories {
            import(&f, canonical, owner, path, false);
        }
        for (owner, _, inventory) in &inventories {
            assert_eq!(
                rpc(&exe, &restored, owner, "export", json!({"full":true})),
                *inventory
            );
        }
        assert_eq!(
            rpc(
                &exe,
                &restored,
                canonical,
                "capture_query",
                json!({"capture_id":capture_id})
            )["committed"],
            true
        );
        // Forget through canonical group, then replay ALL source files/receipts.
        f.json(&f.main, &["forget", "memory", "mem-main"]);
        let cap_note = format!("mem-cap-{capture_id}");
        f.json(&f.main, &["forget", "memory", &cap_note]);
        let history_id = main_inventory["histories"][0]["id"].as_str().unwrap();
        f.json(&f.main, &["forget", "history", history_id]);
        for (owner, path, _) in &inventories {
            import(&f, canonical, owner, path, false);
        }
        let deleted = rpc(&exe, &restored, main.key(), "export", json!({"full":true}));
        assert_eq!(deleted["records"], json!([]));
        assert!(deleted["tombstones"]
            .as_array()
            .unwrap()
            .contains(&json!("mem-main")));
        assert_eq!(deleted["histories"][0]["tombstone"], true);
        assert_eq!(
            rpc(
                &exe,
                &restored,
                canonical,
                "capture_query",
                json!({"capture_id":capture_id})
            )["tombstoned"],
            true
        );
        let tomb_backup = f.tmp.path().join("tombstone-backup");
        let tomb_index = checked_backup(&f, &tomb_backup, 3);
        let final_brain = f.base.join("bodyless-restored.r8");
        select(&f, &exe, &final_brain);
        for file in tomb_index["files"].as_array().unwrap() {
            import(
                &f,
                canonical,
                file["project"].as_str().unwrap(),
                &tomb_backup.join(file["file"].as_str().unwrap()),
                false,
            );
        }
        // Different migration manifests cannot resurrect source lineage either.
        for (owner, path, _) in &inventories {
            import(&f, canonical, owner, path, false);
        }
        assert_eq!(
            rpc(
                &exe,
                &final_brain,
                main.key(),
                "export",
                json!({"full":true})
            ),
            deleted
        );
        assert_eq!(
            rpc(
                &exe,
                &final_brain,
                canonical,
                "capture_query",
                json!({"capture_id":capture_id})
            )["tombstoned"],
            true
        );
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
