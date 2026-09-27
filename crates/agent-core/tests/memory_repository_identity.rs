//! All discovery and persistence here is confined to real, synthetic temp repos.
#![cfg(unix)]

use agent_core::memory::repository::RepositoryIdentity;
use agent_core::memory::store::ProjectScope;
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

const MARKER: &str = "synaps-memory-identity.json";
const LOCK: &str = "synaps-memory-identity.lock";

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let mut command = Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name.as_encoded_bytes().starts_with(b"GIT_") {
            command.env_remove(name);
        }
    }
    let output = command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("HOME", root)
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgsign=false",
        ])
        .args([
            "-c",
            "user.name=Synthetic",
            "-c",
            "user.email=test@example.invalid",
        ])
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn repo(temp: &TempDir, name: &str) -> PathBuf {
    let root = temp.path().join(name);
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["commit", "-q", "--allow-empty", "-m", "synthetic"]);
    root.canonicalize().unwrap()
}

fn worktree(main: &Path, root: &Path, branch: &str) -> PathBuf {
    git(
        main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            branch,
            root.to_str().unwrap(),
        ],
    );
    root.canonicalize().unwrap()
}

fn resolve(root: &Path) -> RepositoryIdentity {
    ProjectScope::discover_repository_with_override(root, None).unwrap()
}

fn private_write(path: &Path, data: &[u8]) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(data).unwrap();
}

#[test]
fn worktree_first_converges_on_random_key_and_enumerates_legacy_aliases() {
    let tmp = TempDir::new().unwrap();
    let main = repo(&tmp, "main");
    let a = worktree(&main, &tmp.path().join("worker a"), "a");
    let b = worktree(&main, &tmp.path().join("worker\n\"b"), "b");
    fs::create_dir_all(b.join("src/deep")).unwrap();
    let first = resolve(&b.join("src/deep"));
    assert_ne!(
        first.scope.key(),
        ProjectScope::for_root(&main).unwrap().key()
    );
    assert_eq!(first.legacy_scope, ProjectScope::for_root(&b).unwrap());
    assert_eq!(first.repository_root, main);
    assert_eq!(first.worktree_root, b);
    assert_eq!(first.common_dir, Some(main.join(".git")));
    for root in [&main, &a, &b] {
        assert_eq!(resolve(root).scope.key(), first.scope.key());
        assert!(first
            .aliases
            .contains(&ProjectScope::for_root(root).unwrap()));
    }
    assert_eq!(first.aliases.len(), 4);
    for name in [MARKER, LOCK] {
        let meta = fs::symlink_metadata(main.join(".git").join(name)).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o600);
        assert_eq!(meta.nlink(), 1);
        assert_eq!(meta.uid(), unsafe { libc::geteuid() });
    }
}

#[test]
fn unrelated_repositories_with_same_remote_stay_isolated() {
    let tmp = TempDir::new().unwrap();
    let a = repo(&tmp, "a");
    let b = repo(&tmp, "b");
    for root in [&a, &b] {
        git(
            root,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/same.git",
            ],
        );
    }
    let a = resolve(&a);
    let b = resolve(&b);
    assert_ne!(a.scope.key(), b.scope.key());
    assert!(!a.aliases.iter().any(|s| s.key() == b.scope.key()));
}

#[test]
fn moved_main_and_worktree_retain_key_and_historical_aliases() {
    let tmp = TempDir::new().unwrap();
    let main = repo(&tmp, "old-main");
    let worker = worktree(&main, &tmp.path().join("old-worker"), "worker");
    let before = resolve(&worker);
    let old_worker_scope = ProjectScope::for_root(&worker).unwrap();
    let moved_worker = tmp.path().join("new-worker");
    git(
        &main,
        &[
            "worktree",
            "move",
            worker.to_str().unwrap(),
            moved_worker.to_str().unwrap(),
        ],
    );
    resolve(&moved_worker);
    let moved_main = tmp.path().join("new-main");
    fs::rename(&main, &moved_main).unwrap();
    git(
        &moved_main,
        &["worktree", "repair", moved_worker.to_str().unwrap()],
    );
    let after = resolve(&moved_worker);
    assert_eq!(after.scope.key(), before.scope.key());
    assert_eq!(after.scope.root(), moved_main);
    assert_ne!(
        after.scope.key(),
        ProjectScope::for_root(&moved_main).unwrap().key()
    );
    assert!(after.aliases.contains(&old_worker_scope));
    assert!(after
        .aliases
        .contains(&ProjectScope::for_root(&moved_main).unwrap()));
    assert!(after
        .aliases
        .contains(&ProjectScope::for_root(&moved_worker).unwrap()));
    assert_eq!(resolve(&moved_main).scope, after.scope);
    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(moved_main.join(".git").join(MARKER)).unwrap()).unwrap();
    assert_eq!(value["version"], 2);
    assert_eq!(value["initial_root"], main.to_str().unwrap());
    assert_eq!(value["key"], before.scope.key());
}

#[test]
fn malformed_reserved_and_inconsistent_markers_fail_without_repair() {
    let tmp = TempDir::new().unwrap();
    let main = repo(&tmp, "main");
    let scope = ProjectScope::for_root(&main).unwrap();
    resolve(&main);
    let marker = main.join(".git").join(MARKER);
    let valid = fs::read(&marker).unwrap();
    let mut cases = vec![
        b"".to_vec(),
        b"not-json".to_vec(),
        b"{}".to_vec(),
        vec![b'x'; 1024 * 1024 + 1],
    ];
    for (field, value) in [
        ("key", serde_json::json!("p0000000000000000")),
        ("key", serde_json::json!("pABCDEF0123456789")),
        ("key", serde_json::json!("p1111")),
        ("version", serde_json::json!(1)),
        ("version", serde_json::json!(3)),
        ("extra", serde_json::json!(true)),
        ("roots", serde_json::json!([])),
        ("roots", serde_json::json!(["../elsewhere"])),
        ("initial_root", serde_json::json!("/changed")),
    ] {
        let mut value_json: serde_json::Value = serde_json::from_slice(&valid).unwrap();
        value_json[field] = value;
        cases.push(serde_json::to_vec(&value_json).unwrap());
    }
    for bytes in cases {
        private_write(&marker, &bytes);
        assert!(ProjectScope::discover_repository_with_override(&main, None).is_err());
        assert_eq!(fs::read(&marker).unwrap(), bytes);
        assert_eq!(
            ProjectScope::discover_with_override(&main, None).unwrap(),
            scope
        );
    }
}

#[test]
fn marker_and_lock_symlinks_hardlinks_modes_and_special_files_fail() {
    let tmp = TempDir::new().unwrap();
    for name in [MARKER, LOCK] {
        let main = repo(&tmp, name);
        resolve(&main);
        let target = main.join(".git").join(name);
        let original = fs::read(&target).unwrap();
        let outside = tmp.path().join(format!("outside-{name}"));
        private_write(&outside, &original);
        fs::remove_file(&target).unwrap();
        symlink(&outside, &target).unwrap();
        assert!(ProjectScope::discover_repository_with_override(&main, None).is_err());
        assert_eq!(fs::read(&outside).unwrap(), original);
        fs::remove_file(&target).unwrap();
        fs::hard_link(&outside, &target).unwrap();
        assert!(ProjectScope::discover_repository_with_override(&main, None).is_err());
        fs::remove_file(&target).unwrap();
        private_write(&target, &original);
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(ProjectScope::discover_repository_with_override(&main, None).is_err());
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o644);
        fs::remove_file(&target).unwrap();
        fs::create_dir(&target).unwrap();
        assert!(ProjectScope::discover_repository_with_override(&main, None).is_err());
        fs::remove_dir(&target).unwrap();
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(target.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(ProjectScope::discover_repository_with_override(&main, None).is_err());
    }
}

#[test]
fn owner_owned_group_writable_repository_layout_is_supported_without_chmod() {
    let tmp = TempDir::new().unwrap();
    let main = repo(&tmp, "main");
    let worker = worktree(&main, &tmp.path().join("worker"), "worker");
    for path in [
        tmp.path(),
        main.as_path(),
        main.join(".git").as_path(),
        worker.as_path(),
    ] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o775)).unwrap();
    }
    let identity = resolve(&worker);
    assert_eq!(identity.scope, resolve(&main).scope);
    assert_ne!(
        identity.scope.key(),
        ProjectScope::for_root(&main).unwrap().key()
    );
    for path in [
        tmp.path(),
        main.as_path(),
        main.join(".git").as_path(),
        worker.as_path(),
    ] {
        assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o775);
    }
    for name in [MARKER, LOCK] {
        assert_eq!(
            fs::metadata(main.join(".git").join(name)).unwrap().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn untrusted_common_directory_is_not_chmod_repaired() {
    let tmp = TempDir::new().unwrap();
    let main = repo(&tmp, "main");
    fs::set_permissions(main.join(".git"), fs::Permissions::from_mode(0o777)).unwrap();
    assert!(ProjectScope::discover_repository_with_override(&main, None).is_err());
    assert!(!main.join(".git").join(MARKER).exists());
    assert_eq!(
        fs::metadata(main.join(".git")).unwrap().mode() & 0o777,
        0o777
    );
}

#[test]
fn separate_gitdir_worktree_first_shares_random_key_and_common_directory_root() {
    let tmp = TempDir::new().unwrap();
    let main = tmp.path().join("main");
    let common = tmp.path().join("separate.git");
    fs::create_dir(&main).unwrap();
    git(
        &main,
        &["init", "-q", "--separate-git-dir", common.to_str().unwrap()],
    );
    git(&main, &["commit", "-q", "--allow-empty", "-m", "synthetic"]);
    let worker = worktree(&main, &tmp.path().join("worker"), "worker");
    let first = resolve(&worker);
    let second = resolve(&main);
    assert_eq!(first.repository_root, common);
    assert_ne!(
        first.scope.key(),
        ProjectScope::for_root(&common).unwrap().key()
    );
    assert_eq!(first.scope, second.scope);
    assert!(second
        .aliases
        .contains(&ProjectScope::for_root(&main).unwrap()));
    assert!(second
        .aliases
        .contains(&ProjectScope::for_root(&worker).unwrap()));
}

#[test]
fn repository_and_worktree_git_entry_symlinks_fail_closed() {
    let tmp = TempDir::new().unwrap();
    let main = repo(&tmp, "main");
    let worker = worktree(&main, &tmp.path().join("worker"), "worker");
    let saved = tmp.path().join("worker-gitfile");
    fs::rename(worker.join(".git"), &saved).unwrap();
    symlink(&saved, worker.join(".git")).unwrap();
    assert!(ProjectScope::discover_repository_with_override(&worker, None).is_err());
    fs::remove_file(worker.join(".git")).unwrap();
    fs::rename(&saved, worker.join(".git")).unwrap();
    let saved_git = tmp.path().join("saved-git");
    fs::rename(main.join(".git"), &saved_git).unwrap();
    symlink(&saved_git, main.join(".git")).unwrap();
    assert!(ProjectScope::discover_repository_with_override(&main, None).is_err());
    assert!(!saved_git.join(MARKER).exists());
}

#[test]
fn explicit_override_and_non_git_are_legacy_compatible_and_do_not_write() {
    let tmp = TempDir::new().unwrap();
    let main = repo(&tmp, "main");
    let worker = worktree(&main, &tmp.path().join("worker"), "worker");
    let sub = worker.join("sub");
    fs::create_dir(&sub).unwrap();
    for root in [&main, &worker, &sub] {
        let identity = ProjectScope::discover_repository_with_override(
            Path::new("/missing-start"),
            Some(root),
        )
        .unwrap();
        let legacy = ProjectScope::for_root(root).unwrap();
        assert_eq!(identity.scope, legacy);
        assert_eq!(identity.legacy_scope, legacy);
        assert_eq!(identity.aliases, vec![legacy]);
        assert_eq!(identity.common_dir, None);
        assert!(!main.join(".git").join(MARKER).exists());
    }
    let plain = tmp.path().join("plain");
    fs::create_dir_all(plain.join("sub")).unwrap();
    assert_eq!(
        resolve(&plain.join("sub")).scope,
        ProjectScope::for_root(&plain.join("sub")).unwrap()
    );
    fs::write(plain.join(".synaps-project"), b"").unwrap();
    assert_eq!(
        resolve(&plain.join("sub")).scope,
        ProjectScope::for_root(&plain).unwrap()
    );
    // Old discovery still accepts fake .git entries and does no Git parsing.
    fs::write(plain.join(".git"), b"not a git file").unwrap();
    assert!(ProjectScope::discover_with_override(&plain, None).is_ok());
    assert!(ProjectScope::discover_repository_with_override(&plain, None).is_err());
}

#[test]
fn stale_and_one_way_worktree_registrations_never_become_aliases() {
    let tmp = TempDir::new().unwrap();
    let main = repo(&tmp, "main");
    let stale = worktree(&main, &tmp.path().join("stale"), "stale");
    fs::remove_dir_all(&stale).unwrap();
    fs::create_dir(&stale).unwrap();
    git(&stale, &["init", "-q"]);
    let worker = worktree(&main, &tmp.path().join("worker"), "worker");
    let forged = tmp.path().join("forged");
    fs::create_dir(&forged).unwrap();
    fs::copy(worker.join(".git"), forged.join(".git")).unwrap();
    let identity = resolve(&main);
    assert!(!identity
        .aliases
        .iter()
        .any(|s| s.root() == stale || s.root() == forged));
    assert!(ProjectScope::discover_repository_with_override(&forged, None).is_err());
}

#[test]
fn user_scope_is_explicit_and_host_key_constructor_is_strict() {
    let tmp = TempDir::new().unwrap();
    let scope = ProjectScope::user_scope(tmp.path()).unwrap();
    assert_eq!(scope.key(), "p0000000000000000");
    assert_eq!(scope.root(), tmp.path().canonicalize().unwrap());
    for key in [
        "",
        "p0000000000000000",
        "pABCDEF0123456789",
        "p0123456789abcdef/",
        "p0123456789abcde",
        "q0123456789abcdef",
        "../project",
        "p0123456789abcdeg",
    ] {
        assert!(ProjectScope::from_key(tmp.path(), key).is_err(), "{key}");
    }
    assert_eq!(
        ProjectScope::from_key(tmp.path(), "p0123456789abcdef")
            .unwrap()
            .key(),
        "p0123456789abcdef"
    );
    assert!(ProjectScope::user_scope(&tmp.path().join("missing")).is_err());
    let file = tmp.path().join("not-a-directory");
    fs::write(&file, b"").unwrap();
    assert!(ProjectScope::user_scope(&file).is_err());
    assert!(ProjectScope::from_key(&file, "p0123456789abcdef").is_err());
    assert!(ProjectScope::for_root(&file).is_ok()); // legacy contract unchanged
}

// A separate process verifies the real advisory lock (not a thread-only mutex),
// and gives environment override/inherited Git injection tests an isolated seam.
#[test]
fn repository_identity_child() {
    let Some(root) = std::env::var_os("SYNTHETIC_REPOSITORY_CHILD") else {
        return;
    };
    let root = PathBuf::from(root);
    let result = ProjectScope::discover_repository(&root);
    if let Ok(expected) = std::env::var("SYNTHETIC_EXPECTED_ERROR") {
        assert!(result.unwrap_err().to_string().contains(&expected));
        return;
    }
    let identity = result.unwrap();
    let expected = std::env::var("SYNTHETIC_EXPECTED_KEY").unwrap();
    if expected.is_empty() {
        // Concurrent first resolution cannot know the random key in advance.
        let marker: serde_json::Value =
            serde_json::from_slice(&fs::read(identity.common_dir.unwrap().join(MARKER)).unwrap())
                .unwrap();
        assert_eq!(marker["key"], identity.scope.key());
    } else {
        assert_eq!(identity.scope.key(), expected);
    }
}

fn child(root: &Path, expected: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "repository_identity_child", "--nocapture"])
        .env("SYNTHETIC_REPOSITORY_CHILD", root)
        .env("SYNTHETIC_EXPECTED_KEY", expected)
        .env_remove("SYNAPS_PROJECT_ROOT")
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    command
}

#[test]
fn concurrent_first_resolution_from_multiple_processes_and_workers() {
    let tmp = TempDir::new().unwrap();
    let main = repo(&tmp, "main");
    let a = worktree(&main, &tmp.path().join("a"), "a");
    let b = worktree(&main, &tmp.path().join("b"), "b");
    let mut children = Vec::new();
    for i in 0..16 {
        children.push(child([&a, &b, &main][i % 3], "").spawn().unwrap());
    }
    for mut process in children {
        assert!(process.wait().unwrap().success());
    }
    let identity = resolve(&main);
    assert_eq!(identity.aliases.len(), 4);
    assert_eq!(resolve(&a).scope.key(), identity.scope.key());
    assert_eq!(resolve(&b).scope.key(), identity.scope.key());
    let entries = fs::read_dir(main.join(".git")).unwrap();
    assert!(!entries
        .map(|e| e.unwrap().file_name())
        .any(|name| name.to_string_lossy().ends_with(".tmp")));
}

#[test]
#[cfg(target_os = "linux")]
fn lock_replacement_while_waiting_fails_closed() {
    use std::time::{Duration, Instant};
    let tmp = TempDir::new().unwrap();
    let main = repo(&tmp, "main");
    let lock_path = main.join(".git").join(LOCK);
    private_write(&lock_path, b"");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    fs4::fs_std::FileExt::lock_exclusive(&lock).unwrap();
    let scope = ProjectScope::for_root(&main).unwrap();
    let mut process = child(&main, scope.key())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    // Wait until the child actually opened our locked inode. This makes the
    // replacement race deterministic, rather than hoping a sleep is sufficient.
    let proc_fds = PathBuf::from(format!("/proc/{}/fd", process.id()));
    let inode = lock.metadata().unwrap().ino();
    loop {
        let opened = fs::read_dir(&proc_fds).unwrap().any(|entry| {
            entry
                .ok()
                .and_then(|e| fs::metadata(e.path()).ok())
                .is_some_and(|m| m.ino() == inode && m.dev() == lock.metadata().unwrap().dev())
        });
        if opened {
            break;
        }
        if Instant::now() >= deadline {
            let _ = process.kill();
            let _ = process.wait();
            panic!("child never reached advisory lock");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    fs::remove_file(&lock_path).unwrap();
    private_write(&lock_path, b"");
    fs4::fs_std::FileExt::unlock(&lock).unwrap();
    assert!(!process.wait().unwrap().success());
    assert!(!main.join(".git").join(MARKER).exists());
    // No corruption or stale process lock prevents a subsequent safe retry.
    assert!(resolve(&main).aliases.contains(&scope));
}

#[test]
fn env_override_is_explicit_and_inherited_git_routing_is_ignored() {
    let tmp = TempDir::new().unwrap();
    let main = repo(&tmp, "main");
    let other = repo(&tmp, "other");
    let sub = main.join("sub");
    fs::create_dir(&sub).unwrap();
    let sub_key = ProjectScope::for_root(&sub).unwrap();
    assert!(child(&other, sub_key.key())
        .env("SYNAPS_PROJECT_ROOT", &sub)
        .status()
        .unwrap()
        .success());
    assert!(!main.join(".git").join(MARKER).exists());
    assert!(!other.join(".git").join(MARKER).exists());
    let main_key = resolve(&main).scope;
    assert!(child(&main, main_key.key())
        .env("GIT_DIR", other.join(".git"))
        .env("GIT_WORK_TREE", &other)
        .env("GIT_COMMON_DIR", other.join(".git"))
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.bare")
        .env("GIT_CONFIG_VALUE_0", "true")
        .status()
        .unwrap()
        .success());
    assert!(!other.join(".git").join(MARKER).exists());
}

#[test]
fn unrelated_repository_reusing_moved_path_gets_distinct_canonical_key() {
    let tmp = TempDir::new().unwrap();
    let old_path = repo(&tmp, "original");
    let before = resolve(&old_path);
    let legacy = ProjectScope::for_root(&old_path).unwrap();
    let moved = tmp.path().join("moved");
    fs::rename(&old_path, &moved).unwrap();
    let unrelated = repo(&tmp, "original");
    let fresh = resolve(&unrelated);
    let after = resolve(&moved);
    assert_eq!(after.scope.key(), before.scope.key());
    assert_ne!(fresh.scope.key(), before.scope.key());
    assert_ne!(fresh.scope.key(), legacy.key());
    assert_ne!(before.scope.key(), legacy.key());
    assert!(after.aliases.contains(&legacy));
    assert!(fresh.aliases.contains(&legacy)); // never an automatic merge
    assert!(!fresh.aliases.iter().any(|s| s.key() == after.scope.key()));
    assert!(!after.aliases.iter().any(|s| s.key() == fresh.scope.key()));
}

#[test]
fn unreleased_version_one_path_key_is_rejected_without_rewriting() {
    let tmp = TempDir::new().unwrap();
    let main = repo(&tmp, "main");
    let marker = main.join(".git").join(MARKER);
    let bytes = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "key": ProjectScope::for_root(&main).unwrap().key(),
        "initial_root": main,
        "roots": [main],
    }))
    .unwrap();
    private_write(&marker, &bytes);
    assert!(ProjectScope::discover_repository_with_override(&main, None).is_err());
    assert_eq!(fs::read(marker).unwrap(), bytes);
}

// Watchdog only for synthetic subprocess tests: a regression must not hang cargo.
fn bounded_wait(process: &mut std::process::Child) -> Option<std::process::ExitStatus> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(9);
    loop {
        if let Some(status) = process.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            let _ = process.kill();
            let _ = process.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn synthetic_git_failure(script: &str, expected: &str, timeout: bool) {
    use std::time::{Duration, Instant};
    let tmp = TempDir::new().unwrap();
    let main = repo(&tmp, "main");
    let executable = tmp.path().join("git");
    let pid_path = tmp.path().join("git.pid");
    let descendant_path = tmp.path().join("descendant.pid");
    fs::write(
        &executable,
        format!("#!/bin/sh\necho $$ > \"$SYNTHETIC_GIT_PID\"\n{script}\n"),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let started = Instant::now();
    let mut process = child(&main, "")
        .env("PATH", tmp.path())
        .env("SYNTHETIC_GIT_PID", &pid_path)
        .env("SYNTHETIC_DESCENDANT_PID", &descendant_path)
        .env("SYNTHETIC_EXPECTED_ERROR", expected)
        .spawn()
        .unwrap();
    let status = bounded_wait(&mut process);
    // Clean up the deliberate inherited-pipe holder even if discovery regressed.
    if let Ok(pid) = fs::read_to_string(&descendant_path) {
        unsafe {
            libc::kill(pid.trim().parse().unwrap(), libc::SIGKILL);
        }
    }
    let pid: libc::pid_t = fs::read_to_string(pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let reaped = unsafe { libc::kill(pid, 0) } == -1
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    if !reaped {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    assert!(status
        .expect("discovery exceeded watchdog deadline")
        .success());
    assert!(reaped, "direct Git child was not killed/reaped");
    if timeout {
        assert!(started.elapsed() >= Duration::from_millis(4500));
    }
    assert!(!main.join(".git").join(MARKER).exists());
}

#[test]
fn git_silent_hang_times_out_and_child_is_reaped() {
    synthetic_git_failure("exec /bin/sleep 30", "Git discovery timed out", true);
}

#[test]
fn git_closed_stdout_does_not_allow_unbounded_child_wait() {
    synthetic_git_failure(
        "exec 1>&-\nexec /bin/sleep 30",
        "Git discovery timed out",
        true,
    );
}

#[test]
fn git_exited_child_with_inherited_pipe_is_bounded() {
    synthetic_git_failure(
        "/bin/sleep 30 &\necho $! > \"$SYNTHETIC_DESCENDANT_PID\"\nexit 0",
        "Git discovery timed out",
        true,
    );
}

#[test]
fn git_excess_output_is_bounded_and_child_is_reaped() {
    synthetic_git_failure(
        "while :; do printf '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'; done",
        "Git discovery output too large",
        false,
    );
}

#[test]
fn held_marker_lock_times_out_without_publishing_and_allows_retry() {
    use std::time::{Duration, Instant};
    let tmp = TempDir::new().unwrap();
    let main = repo(&tmp, "main");
    let lock_path = main.join(".git").join(LOCK);
    private_write(&lock_path, b"");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    fs4::fs_std::FileExt::lock_exclusive(&lock).unwrap();
    let started = Instant::now();
    let mut process = child(&main, "")
        .env(
            "SYNTHETIC_EXPECTED_ERROR",
            "repository marker lock timed out",
        )
        .spawn()
        .unwrap();
    let status = bounded_wait(&mut process).expect("lock acquisition exceeded watchdog deadline");
    assert!(status.success());
    assert!(started.elapsed() >= Duration::from_millis(4500));
    assert!(!main.join(".git").join(MARKER).exists());
    assert_eq!(fs::read(&lock_path).unwrap(), b"");
    fs4::fs_std::FileExt::unlock(&lock).unwrap();
    let identity = resolve(&main);
    assert_eq!(identity.scope, resolve(&main).scope);
}
