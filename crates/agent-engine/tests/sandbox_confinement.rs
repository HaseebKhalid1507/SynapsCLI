//! Real-kernel pins for the per-session write sandbox: a confined shell
//! command is really denied by Landlock, and so is the in-process write
//! guard. Every test builds the session's policy exactly as a session start
//! does (`resolve` + `Confinement`), spawns `/bin/sh` through
//! `SessionSandbox::confine_std`, and checks what the kernel allowed.
//!
//! Each pin has its undo as a control in the same test: the very same
//! script run with the whole tree granted (no split) *succeeds*, so the
//! denial asserted above it is the split's doing, not an accident of the
//! fixture. Removing the split from `Confinement::plan` turns the pins red.
//!
//! A host without usable Landlock (older than ABI 3, or disabled) prints
//! `SKIPPED` and passes nothing it didn't check; it cannot fake a result.
//!
//! The fixtures live under `/var/tmp`, not `/tmp`: the baseline grants all of
//! `/tmp` to every session, so a fixture there would be writable whatever
//! the policy says.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use agent_engine::sandbox::confinement::Confinement;
use agent_engine::sandbox::landlock;
use agent_engine::sandbox::policy::{
    canon_lossy, resolve, Answer, PolicyEnv, Protected, ProviderOutcome, ResolveInput, REASON_SYMLINK,
};
use agent_engine::sandbox::SessionSandbox;

/// `true` when this kernel can run the tests; says so loudly when not.
fn kernel_ok(test: &str) -> bool {
    match landlock::usable_abi() {
        Ok(abi) => {
            eprintln!("{test}: landlock abi {abi}");
            true
        }
        Err(why) => {
            eprintln!("SKIPPED {test}: no usable Landlock on this host ({why:?})");
            false
        }
    }
}

struct Tree {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    base: PathBuf,
    ws: PathBuf,
}

fn tree() -> Tree {
    let dir = tempfile::tempdir_in("/var/tmp").expect("a scratch dir under /var/tmp");
    let root = canon_lossy(dir.path());
    let home = root.join("home");
    let base = home.join(".synaps-cli");
    let ws = home.join("proj");
    std::fs::create_dir_all(base.join("sandbox")).unwrap();
    // `src` exists up front: the workspace is always split (the project's
    // plugin folder is left out of it), so a shell can add files in `src`
    // but not directly in the workspace.
    std::fs::create_dir_all(ws.join("src")).unwrap();
    Tree { _dir: dir, root, home, base, ws }
}

fn env_for(t: &Tree, path_dirs: &[PathBuf]) -> PolicyEnv {
    let mut env = PolicyEnv::fixed(&t.home, &t.base);
    env.path_dirs = path_dirs.to_vec();
    env
}

/// The session's confinement around `workspace`, the provider having
/// answered `provider_write`.
fn confine(t: &Tree, workspace: &Path, path_dirs: &[PathBuf], provider_write: &[&Path]) -> Arc<Confinement> {
    let env = env_for(t, path_dirs);
    let resolution = resolve(ResolveInput {
        confine_on: true,
        provider: Some("example-guard"),
        abi: Ok(landlock::usable_abi().unwrap_or(7)),
        outcome: ProviderOutcome::Answer(Answer {
            write: provider_write.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
            read: vec![],
        }),
        workspace,
        env: &env,
    });
    assert!(resolution.refusal.is_none(), "{:?}", resolution.refusal);
    Arc::new(Confinement::new(resolution, Protected::new(&env), workspace, Some(workspace)))
}

/// Run `script` under the session's sandbox; `true` when it exited 0.
fn confined_ok(c: &Arc<Confinement>, cwd: &Path, script: &str) -> bool {
    let sandbox = SessionSandbox::Confined(Arc::clone(c));
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(script).current_dir(cwd);
    sandbox.confine_std(&mut cmd).expect("the spawn is confined, never refused here");
    let out = cmd.output().expect("spawn /bin/sh");
    if !out.status.success() {
        eprintln!("denied: {script}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    out.status.success()
}

/// The undo: run `script` with `whole` (a directory) granted entire and
/// nothing split, so the denial a pin asserts is lifted.
fn whole_tree_ok(whole: &Path, cwd: &Path, script: &str) -> bool {
    let mut ruleset = landlock::Ruleset::new().expect("ruleset");
    assert!(ruleset.allow(whole).expect("allow"));
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(script).current_dir(cwd);
    landlock::confine_command(&mut cmd, &ruleset).expect("confine");
    cmd.output().expect("spawn /bin/sh").status.success()
}

fn write_script(path: &Path) -> String {
    format!("printf x > '{}'", path.display())
}

fn make_main_checkout(dir: &Path) {
    for d in ["objects/ab", "refs/heads", "logs", "hooks", "info", "worktrees"] {
        std::fs::create_dir_all(dir.join(".git").join(d)).unwrap();
    }
    std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(dir.join(".git/config"), "[core]\n").unwrap();
}

/// A main repository at `<root>/repo` with a linked worktree `<root>/repo-wt`.
fn make_linked(root: &Path) -> (PathBuf, PathBuf) {
    let repo = root.join("repo");
    let wt = root.join("repo-wt");
    make_main_checkout(&repo);
    let admin = repo.join(".git/worktrees/wt");
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::create_dir_all(wt.join("src")).unwrap();
    std::fs::write(admin.join("commondir"), "../..\n").unwrap();
    std::fs::write(admin.join("gitdir"), format!("{}\n", wt.join(".git").display())).unwrap();
    std::fs::write(admin.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
    (repo, wt)
}

#[test]
fn reports_the_kernels_landlock_abi() {
    match landlock::abi_version() {
        Ok(abi) => eprintln!("landlock-abi: {abi} (usable: {})", landlock::usable_abi().is_ok()),
        Err(why) => eprintln!("landlock-abi: none ({why:?})"),
    }
}

#[test]
fn a_main_checkouts_git_hooks_and_config_cannot_be_written() {
    if !kernel_ok("main checkout") {
        return;
    }
    let t = tree();
    make_main_checkout(&t.ws);
    std::fs::create_dir_all(t.ws.join("src")).unwrap();
    let c = confine(&t, &t.ws, &[], &[]);

    // The attack: plant a hook / config line the owner's next git runs.
    assert!(!confined_ok(&c, &t.ws, &write_script(&t.ws.join(".git/hooks/pre-commit"))));
    assert!(!confined_ok(&c, &t.ws, &format!("printf x >> '{}'", t.ws.join(".git/config").display())));
    assert!(!confined_ok(&c, &t.ws, "mkdir .git/hooks2"));
    assert!(!confined_ok(&c, &t.ws, "mv .git/hooks .git/hooks-old"));
    assert!(!confined_ok(&c, &t.ws, &write_script(&t.ws.join(".git/info/exclude"))));
    assert!(!t.ws.join(".git/hooks/pre-commit").exists());
    assert_eq!(std::fs::read_to_string(t.ws.join(".git/config")).unwrap(), "[core]\n");
    // Inherited by everything the command starts.
    assert!(!confined_ok(&c, &t.ws, &format!("sh -c \"{}\"", write_script(&t.ws.join(".git/hooks/post-commit")))));
    assert!(!t.ws.join(".git/hooks/post-commit").exists());
    // The in-process write tools stop at the same line.
    let sandbox = SessionSandbox::Confined(Arc::clone(&c));
    assert!(sandbox.guard_write(&t.ws.join(".git/hooks/pre-commit"), true).is_err());
    assert!(sandbox.guard_write(&t.ws.join(".git/config"), false).is_err());

    // What git needs to commit still works.
    assert!(confined_ok(&c, &t.ws, &write_script(&t.ws.join("src/lib.rs"))));
    assert!(confined_ok(&c, &t.ws, &write_script(&t.ws.join(".git/refs/heads/topic"))));
    assert!(confined_ok(&c, &t.ws, &write_script(&t.ws.join(".git/objects/ab/cdef"))));
    assert!(confined_ok(&c, &t.ws, &write_script(&t.ws.join(".git/logs/HEAD"))));
    assert!(sandbox.guard_write(&t.ws.join("src/lib.rs"), false).is_ok());

    // Undo: the whole workspace granted lets the very same writes through.
    assert!(whole_tree_ok(&t.ws, &t.ws, &write_script(&t.ws.join(".git/hooks/pre-commit"))));
    assert!(whole_tree_ok(&t.ws, &t.ws, &format!("printf x >> '{}'", t.ws.join(".git/config").display())));
}

#[test]
fn a_linked_worktrees_common_dir_hooks_and_config_cannot_be_written() {
    if !kernel_ok("linked worktree") {
        return;
    }
    let t = tree();
    let (repo, wt) = make_linked(&t.root);
    let c = confine(&t, &wt, &[], &[]);

    // The hooks and config live in the main repository, which is outside
    // the worktree: still not writable, and the worktree's own `.git` file
    // (which names the git dir) can't be pointed somewhere else.
    assert!(!confined_ok(&c, &wt, &write_script(&repo.join(".git/hooks/pre-commit"))));
    assert!(!confined_ok(&c, &wt, &format!("printf x >> '{}'", repo.join(".git/config").display())));
    assert!(!confined_ok(&c, &wt, &write_script(&wt.join(".git"))));
    assert!(!repo.join(".git/hooks/pre-commit").exists());
    assert_eq!(std::fs::read_to_string(repo.join(".git/config")).unwrap(), "[core]\n");
    let sandbox = SessionSandbox::Confined(Arc::clone(&c));
    assert!(sandbox.guard_write(&repo.join(".git/hooks/pre-commit"), true).is_err());
    assert!(sandbox.guard_write(&repo.join(".git/config"), false).is_err());

    // The worktree's own work and git's bookkeeping for it still work.
    assert!(confined_ok(&c, &wt, &write_script(&wt.join("src/lib.rs"))));
    assert!(confined_ok(&c, &wt, &write_script(&repo.join(".git/refs/heads/wt"))));
    assert!(confined_ok(&c, &wt, &write_script(&repo.join(".git/worktrees/wt/index"))));
    assert!(confined_ok(&c, &wt, &write_script(&repo.join(".git/objects/ab/cdef"))));

    // Undo: grant the common dir whole and the hook can be planted.
    assert!(whole_tree_ok(&repo.join(".git"), &wt, &write_script(&repo.join(".git/hooks/pre-commit"))));
    assert!(whole_tree_ok(&repo.join(".git"), &wt, &format!("printf x >> '{}'", repo.join(".git/config").display())));
}

#[test]
fn the_engines_sandbox_dir_and_whole_base_dir_cannot_be_written() {
    if !kernel_ok("engine base dir") {
        return;
    }
    let t = tree();
    std::fs::write(t.base.join("config"), "x=1\n").unwrap();
    // A provider that asks for the sandbox dir, the base dir and the home
    // (which holds both) gets none of them.
    let c = confine(&t, &t.ws, &[], &[&t.base.join("sandbox"), &t.base, &t.home]);
    let dropped: Vec<&Path> = c.resolution.dropped.iter().map(|d| d.path.as_path()).collect();
    for asked in [t.base.join("sandbox"), t.base.clone(), t.home.clone()] {
        assert!(dropped.contains(&asked.as_path()), "{asked:?} not reported in {dropped:?}");
    }
    assert!(!confined_ok(&c, &t.ws, &write_script(&t.base.join("sandbox/policy.json"))));
    assert!(!confined_ok(&c, &t.ws, &write_script(&t.base.join("config"))));
    assert!(!confined_ok(&c, &t.ws, &write_script(&t.base.join("plugins-new"))));
    // A guard plugin's own folder: its manifest can't be rewritten and a
    // sibling folder can't be added (the guard list lives in the config file
    // above and the folders here).
    std::fs::create_dir_all(t.base.join("plugins/example-guard/.synaps-plugin")).unwrap();
    let manifest = t.base.join("plugins/example-guard/.synaps-plugin/plugin.json");
    std::fs::write(&manifest, "{}\n").unwrap();
    assert!(!confined_ok(&c, &t.ws, &write_script(&manifest)));
    assert!(!confined_ok(&c, &t.ws, &format!("mkdir '{}'", t.base.join("plugins/other").display())));
    assert!(!t.base.join("plugins/other").exists());
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), "{}\n");
    assert!(!t.base.join("sandbox/policy.json").exists());
    let sandbox = SessionSandbox::Confined(Arc::clone(&c));
    assert!(sandbox.guard_write(&t.base.join("sandbox/policy.json"), true).is_err());
    assert!(confined_ok(&c, &t.ws, &write_script(&t.ws.join("src/ok.txt"))));

    // Undo: granted whole, the sandbox dir and a guard's manifest are writable.
    assert!(whole_tree_ok(&t.base.join("plugins"), &t.ws, &write_script(&manifest)));
    std::fs::write(&manifest, "{}\n").unwrap();
    assert!(whole_tree_ok(&t.base.join("sandbox"), &t.ws, &write_script(&t.base.join("sandbox/policy.json"))));
}

#[test]
fn a_path_entry_inside_the_workspace_cannot_be_written_or_swapped() {
    if !kernel_ok("PATH entry") {
        return;
    }
    let t = tree();
    for d in ["bin", "src"] {
        std::fs::create_dir_all(t.ws.join(d)).unwrap();
    }
    std::fs::write(t.ws.join("README.md"), "r\n").unwrap();
    let c = confine(&t, &t.ws, &[t.ws.join("bin")], &[]);

    assert!(!confined_ok(&c, &t.ws, &write_script(&t.ws.join("bin/ls"))));
    assert!(!confined_ok(&c, &t.ws, "mv bin bin-old"));
    assert!(!confined_ok(&c, &t.ws, "rm -rf bin"));
    assert!(!confined_ok(&c, &t.ws, "mv src bin2"));
    let sandbox = SessionSandbox::Confined(Arc::clone(&c));
    assert!(sandbox.guard_write(&t.ws.join("bin/ls"), true).is_err());
    assert!(!t.ws.join("bin/ls").exists());

    // Everything else in the workspace is still writable, new files below
    // the first level included.
    assert!(confined_ok(&c, &t.ws, &write_script(&t.ws.join("src/main.rs"))));
    assert!(confined_ok(&c, &t.ws, "mkdir src/deep && printf x > src/deep/new.rs"));
    assert!(confined_ok(&c, &t.ws, &write_script(&t.ws.join("README.md"))));

    // Undo.
    assert!(whole_tree_ok(&t.ws, &t.ws, &write_script(&t.ws.join("bin/ls"))));
    assert!(whole_tree_ok(&t.ws, &t.ws, "mv bin bin-old"));
}

#[test]
fn node_modules_bin_on_the_path_cannot_be_written() {
    if !kernel_ok("node_modules/.bin") {
        return;
    }
    let t = tree();
    for d in ["node_modules/.bin", "node_modules/left-pad", "src"] {
        std::fs::create_dir_all(t.ws.join(d)).unwrap();
    }
    let c = confine(&t, &t.ws, &[t.ws.join("node_modules/.bin")], &[]);

    assert!(!confined_ok(&c, &t.ws, &write_script(&t.ws.join("node_modules/.bin/tsc"))));
    assert!(!confined_ok(&c, &t.ws, "mv node_modules/.bin node_modules/.bin-old"));
    assert!(!t.ws.join("node_modules/.bin/tsc").exists());
    // A package's own files stay writable.
    assert!(confined_ok(&c, &t.ws, &write_script(&t.ws.join("node_modules/left-pad/index.js"))));
    assert!(confined_ok(&c, &t.ws, &write_script(&t.ws.join("src/a.ts"))));

    // Undo.
    assert!(whole_tree_ok(&t.ws, &t.ws, &write_script(&t.ws.join("node_modules/.bin/tsc"))));
}

#[test]
fn a_project_plugin_folder_cannot_be_written_even_before_it_exists() {
    if !kernel_ok("project plugins") {
        return;
    }
    let t = tree();
    std::fs::create_dir_all(t.ws.join(".synaps/notes")).unwrap();
    std::fs::create_dir_all(t.ws.join(".synaps/plugins/p")).unwrap();
    std::fs::create_dir_all(t.ws.join("src")).unwrap();
    let c = confine(&t, &t.ws, &[], &[]);
    assert!(!confined_ok(&c, &t.ws, &write_script(&t.ws.join(".synaps/plugins/p/run.sh"))));
    assert!(!confined_ok(&c, &t.ws, "mkdir .synaps/plugins/q"));
    assert!(confined_ok(&c, &t.ws, &write_script(&t.ws.join(".synaps/notes/a.md"))));

    // A project with no plugin folder yet can't grow one either.
    let bare = tree();
    std::fs::create_dir_all(bare.ws.join("src")).unwrap();
    let c = confine(&bare, &bare.ws, &[], &[]);
    assert!(!confined_ok(&c, &bare.ws, "mkdir -p .synaps/plugins/p && printf x > .synaps/plugins/p/run.sh"));
    assert!(!bare.ws.join(".synaps").exists());
    assert!(confined_ok(&c, &bare.ws, &write_script(&bare.ws.join("src/a.rs"))));

    // Undo.
    assert!(whole_tree_ok(&bare.ws, &bare.ws, "mkdir -p .synaps/plugins/p && printf x > .synaps/plugins/p/run.sh"));
}

/// The consequence of granting per child, stated in the docs' section on
/// splitting: a directory on a split path can't get new entries created
/// directly in it by a shell command, its existing entries stay writable.
#[test]
fn a_split_directory_cannot_get_new_entries_but_keeps_its_existing_ones() {
    if !kernel_ok("no new entries") {
        return;
    }
    let t = tree();
    for d in ["bin", "src"] {
        std::fs::create_dir_all(t.ws.join(d)).unwrap();
    }
    std::fs::write(t.ws.join("README.md"), "r\n").unwrap();
    let c = confine(&t, &t.ws, &[t.ws.join("bin")], &[]);
    // The workspace itself is on the split path (bin is inside it).
    assert!(!confined_ok(&c, &t.ws, "touch new.txt"));
    assert!(!confined_ok(&c, &t.ws, "mkdir newdir"));
    assert!(!confined_ok(&c, &t.ws, "rm README.md"));
    assert!(confined_ok(&c, &t.ws, &write_script(&t.ws.join("README.md"))));
    assert!(confined_ok(&c, &t.ws, "touch src/new.txt && mkdir src/newdir"));
    // The write tools can create there: they check the policy, not the
    // directory's rights.
    let sandbox = SessionSandbox::Confined(Arc::clone(&c));
    assert!(sandbox.guard_write(&t.ws.join("new.txt"), false).is_ok());
    let plan = c.plan().expect("splittable");
    assert!(plan.no_new_entries.contains(&t.ws), "{:?}", plan.no_new_entries);
    // Undo.
    assert!(whole_tree_ok(&t.ws, &t.ws, "touch new.txt"));
}

#[test]
fn a_provider_root_swapped_for_a_link_between_validation_and_use_is_dropped_and_not_writable() {
    if !kernel_ok("swapped root") {
        return;
    }
    let t = tree();
    let cache = t.root.join("data/cache");
    let elsewhere = t.root.join("data/elsewhere");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    let c = confine(&t, &t.ws, &[], &[&cache]);

    // Control: unswapped, the provider's root is writable.
    assert!(confined_ok(&c, &t.ws, &write_script(&cache.join("a"))));

    // The swap happens after the policy was validated and before the rules
    // are built: the path now leads through a link to an unprotected dir.
    let moved = t.root.join("data/cache-real");
    let (cache2, elsewhere2, moved2) = (cache.clone(), elsewhere.clone(), moved.clone());
    let (ruleset, _plan, dropped) = c
        .build_ruleset_with(&mut || {
            std::fs::rename(&cache2, &moved2).unwrap();
            std::os::unix::fs::symlink(&elsewhere2, &cache2).unwrap();
        })
        .expect("the build goes on without the swapped entry");
    assert!(
        dropped.iter().any(|d| d.path == cache && d.reason == REASON_SYMLINK),
        "swapped entry not reported: {dropped:?}"
    );
    let run = |script: String| {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(script).current_dir(&t.ws);
        landlock::confine_command(&mut cmd, &ruleset).unwrap();
        cmd.output().unwrap().status.success()
    };
    assert!(!run(write_script(&cache.join("b"))), "written through the swapped link");
    assert!(!run(write_script(&elsewhere.join("b"))));
    assert!(!elsewhere.join("b").exists());
    assert!(run(write_script(&t.ws.join("src/fine.txt"))));
}
