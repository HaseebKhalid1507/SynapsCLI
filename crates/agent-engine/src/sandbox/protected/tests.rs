//! The in-process 27c guard on its own: names, identities (a root moved
//! or reached under another name), and the path split the tools rely on.
//! Scratch trees only (`test_scratch_parent`), never a real `~/.synaps-cli`.

use super::*;
use crate::sandbox::policy::test_scratch_parent;
use std::fs;

struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    env: PolicyEnv,
}

fn fx() -> Fx {
    let tmp = tempfile::Builder::new().prefix("sbx-27c-guard-").tempdir_in(test_scratch_parent()).expect("tempdir");
    let root = fs::canonicalize(tmp.path()).expect("canonical tempdir");
    for d in ["home/.synaps-cli/memory/jev/access/answers", "home/.synaps-cli/plugins/jev", "base/sandbox/sessions", "lane", "other"] {
        fs::create_dir_all(root.join(d)).expect("mkdir");
    }
    for f in ["home/.synaps-cli/plugins/jev/config", "home/.synaps-cli/config", "base/config"] {
        fs::write(root.join(f), "x\n").expect("write");
    }
    let env = PolicyEnv { home: root.join("home"), base_dir: root.join("base") };
    Fx { _tmp: tmp, root, env }
}

fn refused(r: Result<Target, WriteError>) -> bool {
    matches!(r, Err(WriteError::Protected))
}

#[test]
fn the_guard_covers_each_root_and_what_is_under_it_only() {
    let fx = fx();
    let g = ToolGuard::new(&fx.env);
    for rel in [
        "home/.synaps-cli/memory/jev",
        "home/.synaps-cli/memory/jev/access/answers/a.json",
        "home/.synaps-cli/memory/jev/not-yet/x",
        "home/.synaps-cli/plugins/jev/config",
        "home/.synaps-cli/config",
        "base/config",
        "base/sandbox",
        "base/sandbox/OFF",
        "base/sandbox/sessions/s.conf",
    ] {
        assert!(g.covers(&fx.root.join(rel)), "{rel}");
    }
    for rel in ["home/.synaps-cli/memory", "home/.synaps-cli/plugins/jev/lib.js", "home/.synaps-cli/config.bak", "base/logs/x", "lane/config"] {
        assert!(!g.covers(&fx.root.join(rel)), "{rel}");
    }
}

#[test]
fn a_root_reached_under_another_name_is_refused_by_identity() {
    let fx = fx();
    // The guard knows the jev dir by name and identity; the dir is then
    // moved under an allowed name (a stand-in for a bind mount, which a
    // test can't make): only its identity, seen by walking `..` from the
    // target's parent, still says what it is.
    let g = ToolGuard::new(&fx.env);
    fs::rename(fx.root.join("home/.synaps-cli/memory/jev"), fx.root.join("other/moved")).expect("move");
    assert!(!g.covers(&fx.root.join("other/moved/access/answers/a.json")), "not by name");
    assert!(refused(g.open_target(&fx.root.join("other/moved/access/answers/a.json"), false)), "the dir above, by identity");
    assert!(refused(g.open_target(&fx.root.join("other/moved/new/deeper/a.json"), true)), "and before anything is made in it");
    assert!(!fx.root.join("other/moved/new").exists(), "nothing was made");
    assert!(refused(g.open_target(&fx.root.join("other/moved"), false)), "the root itself, as a target");
    // A protected file under another name: its identity.
    fs::rename(fx.root.join("base/config"), fx.root.join("lane/cfg")).expect("move");
    assert!(refused(g.open_target(&fx.root.join("lane/cfg"), false)));
    // Elsewhere: allowed.
    assert!(g.open_target(&fx.root.join("lane/new/a.txt"), true).is_ok());
    assert!(fx.root.join("lane/new").is_dir(), "a missing parent is made");
}

#[test]
fn split_keeps_the_last_component_as_written() {
    let cases = [
        ("/a/b/c.txt", "/a/b", "c.txt"),
        ("/a/b/", "/a", "b/"),
        ("/a/b//", "/a", "b//"),
        ("/a/sub/..", "/a/sub", ".."),
        ("/a/./c", "/a", "./c"),
        ("c.txt", ".", "c.txt"),
        ("/c.txt", "/", "c.txt"),
        ("/", "/", "."),
        ("", ".", "."),
    ];
    for (path, dir, tail) in cases {
        let (d, t) = split(Path::new(path));
        assert_eq!((d.as_path(), t.as_os_str()), (Path::new(dir), OsStr::new(tail)), "{path:?}");
    }
}

#[test]
fn a_root_that_does_not_exist_yet_is_known_by_the_name_it_would_get() {
    let fx = fx();
    symlink_dir(&fx.root.join("other"), &fx.root.join("home/alias"));
    // Through a symlinked ancestor: the resolved name.
    assert_eq!(resolved_name(&fx.root.join("home/alias/not/yet")), fx.root.join("other/not/yet"));
    let env = PolicyEnv { home: fx.root.join("home/alias"), base_dir: fx.root.join("base") };
    let g = ToolGuard::new(&env);
    assert!(g.covers(&fx.root.join("other/.synaps-cli/memory/jev/x")), "by its resolved name");
    assert!(g.covers(&fx.root.join("home/alias/.synaps-cli/memory/jev/x")), "and as written");
}

fn symlink_dir(to: &Path, link: &Path) {
    std::os::unix::fs::symlink(to, link).expect("symlink");
}

#[test]
fn grant_roots_name_each_root_and_identify_the_directories_above() {
    let fx = fx();
    let roots = grant_roots(&fx.root.join("home")).expect("roots");
    for rel in [".synaps-cli/memory/jev", ".synaps-cli/plugins/jev/config", ".synaps-cli/config"] {
        assert!(roots.names.contains(&fx.root.join("home").join(rel)), "{rel}");
    }
    let id = |rel: &str| {
        let m = fs::metadata(fx.root.join(rel)).expect("stat");
        (m.dev(), m.ino())
    };
    for rel in ["home/.synaps-cli/memory/jev", "home/.synaps-cli/memory", "home/.synaps-cli/plugins/jev", "home/.synaps-cli/config", "home"] {
        assert!(roots.ids.contains(&id(rel)), "{rel}");
    }
    assert!(!roots.ids.contains(&id("home/.synaps-cli/memory/jev/access")), "not the dirs below a root");
    assert!(!roots.ids.contains(&id("lane")));
}

#[test]
fn a_live_directory_named_like_an_unlinked_one_keeps_its_name() {
    // The kernel marks an unlinked object's name with " (deleted)", but a
    // live name may end so too: that one is used, checked by identity.
    let fx = fx();
    let live = fx.root.join("lane/notes (deleted)");
    fs::create_dir(&live).expect("mkdir");
    let fd = open_dir(&live).expect("open");
    assert_eq!(fd_name(fd.as_fd()).expect("a live name"), live);
    let g = ToolGuard::new(&fx.env);
    assert!(g.open_target(&live.join("a.txt"), false).is_ok());
    assert!(g.open_target(&live.join("new/a.txt"), true).is_ok(), "and a missing parent under it");
    assert!(live.join("new").is_dir());
}

#[test]
fn an_unlinked_directory_fails_closed_even_when_its_marked_name_is_live() {
    let fx = fx();
    let g = ToolGuard::new(&fx.env);
    for (name, live_at_mark) in [("gone", true), ("gone2", false)] {
        let gone = fx.root.join("lane").join(name);
        fs::create_dir(&gone).expect("mkdir");
        let fd = open_dir(&gone).expect("open");
        fs::remove_dir(&gone).expect("remove own scratch dir");
        if live_at_mark {
            // Another, live directory now has the name the kernel gives the
            // unlinked one: it isn't this one.
            fs::create_dir(fx.root.join("lane").join(format!("{name} (deleted)"))).expect("mkdir");
        }
        assert!(fd_name(fd.as_fd()).is_err(), "{name}: no name for an unlinked directory");
        assert!(matches!(g.check_dir(fd.as_fd()), Err(WriteError::Check(_))), "{name}: the check fails closed");
    }
}
