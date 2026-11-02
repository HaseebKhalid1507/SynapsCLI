//! The policy carry to a compaction successor. Scratch trees only
//! (`test_scratch_parent`: a fake home and base dir); no kernel support
//! needed.

use super::*;
use crate::sandbox::policy::test_scratch_parent;
use std::fs;
use std::os::unix::fs::MetadataExt;

struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    env: PolicyEnv,
}

fn fx() -> Fx {
    let tmp = tempfile::Builder::new().prefix("sbx-carry-").tempdir_in(test_scratch_parent()).expect("tempdir");
    let root = fs::canonicalize(tmp.path()).expect("canonical tempdir");
    for d in ["home/x/base/sandbox/sessions", "lane"] {
        fs::create_dir_all(root.join(d)).expect("mkdir");
    }
    let env = PolicyEnv { home: root.join("home"), base_dir: root.join("home/x/base") };
    Fx { _tmp: tmp, root, env }
}

impl Fx {
    fn conf(&self, id: &str) -> PathBuf {
        self.env.sessions_dir().join(format!("{id}.conf"))
    }
    fn valid(&self) -> String {
        format!("mode = enforce\nwrite = {}\ncontact = Ask your foreman.\n", self.root.join("lane").display())
    }
    fn write(&self, id: &str, text: &str, mode: u32) {
        fs::write(self.conf(id), text).expect("write policy");
        fs::set_permissions(self.conf(id), fs::Permissions::from_mode(mode)).expect("chmod");
    }
    fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(self.env.sessions_dir())
            .expect("read sessions dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }
}

fn run(fx: &Fx, prev: &str, new: &str) -> (Result<Carried, String>, Vec<String>) {
    let mut log = Vec::new();
    let r = carry(&fx.env, prev, new, &mut |l| log.push(l));
    (r, log)
}

#[test]
fn carry_copies_the_bytes_and_mode_to_one_new_name() {
    let fx = fx();
    fx.write("s1", &fx.valid(), 0o660);
    let (r, log) = run(&fx, "s1", "s2");
    assert!(matches!(r, Ok(Carried::Copied(_))), "{r:?}");
    assert_eq!(log, ["sandbox.carry session=s2 from=s1 result=copied"]);
    assert_eq!(fs::read_to_string(fx.conf("s2")).unwrap(), fx.valid());
    let m = fs::symlink_metadata(fx.conf("s2")).unwrap();
    assert!(m.file_type().is_file());
    assert_eq!(m.mode() & 0o777, 0o660, "the predecessor's mode, whatever the umask");
    assert_eq!(m.nlink(), 1);
    assert_eq!(fs::read_to_string(fx.conf("s1")).unwrap(), fx.valid(), "the predecessor is unchanged");
    assert_eq!(fx.names(), ["s1.conf", "s2.conf"], "no temp file is left");
}

#[test]
fn carry_without_a_predecessor_file_does_nothing_and_logs_nothing() {
    let fx = fx();
    let (r, log) = run(&fx, "s1", "s2");
    assert!(matches!(r, Ok(Carried::Nothing)), "{r:?}");
    assert!(log.is_empty(), "{log:?}");
    assert!(fx.names().is_empty());
    // No sandbox dir at all: nothing is created.
    let bare = PolicyEnv { home: fx.root.join("home"), base_dir: fx.root.join("bare-base") };
    fs::create_dir_all(&bare.base_dir).unwrap();
    let mut log = Vec::new();
    assert!(matches!(carry(&bare, "s1", "s2", &mut |l| log.push(l)), Ok(Carried::Nothing)));
    assert!(log.is_empty());
    assert!(!bare.base_dir.join("sandbox").exists(), "the carry creates no directory");
}

#[test]
fn carry_keeps_an_existing_successor_file() {
    let fx = fx();
    fx.write("s1", &fx.valid(), 0o644);
    fx.write("s2", "mode = off\n", 0o644);
    let (r, log) = run(&fx, "s1", "s2");
    assert!(matches!(r, Ok(Carried::Kept)), "{r:?}");
    assert_eq!(log, ["sandbox.carry session=s2 from=s1 result=kept"]);
    assert_eq!(fs::read_to_string(fx.conf("s2")).unwrap(), "mode = off\n", "kept as it was");
}

#[test]
fn carry_keeps_a_file_that_appears_just_before_the_rename() {
    let fx = fx();
    fx.write("s1", &fx.valid(), 0o644);
    let racer = fx.conf("s2");
    let mut log = Vec::new();
    let r = carry_inner(&fx.env, "s1", "s2", &mut |l| log.push(l), &mut || {
        fs::write(&racer, "mode = off\n").expect("racing write");
    }, &mut || {});
    assert!(matches!(r, Ok(Carried::Kept)), "{r:?}");
    assert_eq!(log, ["sandbox.carry session=s2 from=s1 result=kept"]);
    assert_eq!(fs::read_to_string(fx.conf("s2")).unwrap(), "mode = off\n", "never replaced (RENAME_NOREPLACE)");
    assert_eq!(fx.names(), ["s1.conf", "s2.conf"], "the temp file is removed");
}

#[test]
fn carry_refuses_an_invalid_predecessor_policy_and_creates_nothing() {
    let fx = fx();
    fx.write("s1", "mode = enforce\nbogus = 1\n", 0o644);
    let (r, log) = run(&fx, "s1", "s2");
    let e = r.expect_err("an invalid predecessor policy fails the carry");
    assert!(e.starts_with("the predecessor's policy is invalid: "), "{e}");
    assert!(e.contains(&format!("{}:2", fx.conf("s1").display())), "{e}");
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].starts_with("sandbox.carry session=s2 from=s1 result=failed reason=the predecessor's policy is invalid: "), "{log:?}");
    assert_eq!(fx.names(), ["s1.conf"], "no successor file, no temp file");
}

#[test]
fn carry_refuses_a_hard_linked_or_symlinked_predecessor() {
    let fx = fx();
    fx.write("s1", &fx.valid(), 0o644);
    fs::hard_link(fx.conf("s1"), fx.root.join("alias")).unwrap();
    let (r, log) = run(&fx, "s1", "s2");
    assert!(r.expect_err("hard-linked").contains("hard links"), "{log:?}");
    assert!(!fx.conf("s2").exists());
    fs::write(fx.root.join("real"), fx.valid()).unwrap();
    std::os::unix::fs::symlink(fx.root.join("real"), fx.conf("s3")).unwrap();
    let (r, log) = run(&fx, "s3", "s4");
    assert!(r.expect_err("symlinked").contains("symlink"), "{log:?}");
    assert!(!fx.conf("s4").exists());
}

#[test]
fn carry_handles_the_same_id_and_ids_that_cant_name_a_file() {
    let fx = fx();
    fx.write("s1", &fx.valid(), 0o644);
    let (r, log) = run(&fx, "s1", "s1");
    assert!(matches!(r, Ok(Carried::Nothing)) && log.is_empty(), "in place: {r:?} {log:?}");
    let (r, log) = run(&fx, ".hidden", "s2");
    assert!(matches!(r, Ok(Carried::Nothing)) && log.is_empty(), "no file can exist: {r:?} {log:?}");
    let (r, log) = run(&fx, "s1", "bad/id");
    assert_eq!(r.expect_err("unnameable successor"), "the new session id can't name a policy file");
    assert_eq!(log, ["sandbox.carry session=<invalid> from=s1 result=failed reason=the new session id can't name a policy file"]);
    assert_eq!(fx.names(), ["s1.conf"]);
}

#[test]
fn rollback_removes_the_file_this_carry_created() {
    let fx = fx();
    fx.write("s1", &fx.valid(), 0o644);
    let (r, _) = run(&fx, "s1", "s2");
    let Ok(Carried::Copied(created)) = r else { panic!("expected a copy: {r:?}") };
    let mut log = Vec::new();
    created.rollback(&mut |l| log.push(l));
    assert_eq!(log, ["sandbox.carry session=s2 from=s1 result=rolled-back"]);
    assert_eq!(fx.names(), ["s1.conf"], "only the carried file is removed");
}

#[test]
fn rollback_keeps_a_file_replaced_since_the_carry() {
    let fx = fx();
    fx.write("s1", &fx.valid(), 0o644);
    let (r, _) = run(&fx, "s1", "s2");
    let Ok(Carried::Copied(created)) = r else { panic!("expected a copy: {r:?}") };
    // Replace it by rename, so the new file can't reuse the old inode.
    let other = fx.env.sessions_dir().join("replacement");
    fs::write(&other, "mode = off\n").unwrap();
    fs::rename(&other, fx.conf("s2")).unwrap();
    let mut log = Vec::new();
    created.rollback(&mut |l| log.push(l));
    assert_eq!(log, ["sandbox.carry session=s2 from=s1 result=rollback-kept reason=replaced since the carry"]);
    assert_eq!(fs::read_to_string(fx.conf("s2")).unwrap(), "mode = off\n", "someone else's file survives");
}

#[test]
fn a_dropped_carry_rolls_back_only_a_copy() {
    let fx = fx();
    fx.write("s1", &fx.valid(), 0o644);
    let (r, _) = run(&fx, "s1", "s2");
    drop(crate::sandbox::CarriedPolicy::from_carried(r.expect("copied")));
    assert_eq!(fx.names(), ["s1.conf"], "dropped without keep: the copy is rolled back");
    let (r, _) = run(&fx, "s1", "s2");
    crate::sandbox::CarriedPolicy::from_carried(r.expect("copied")).keep();
    assert_eq!(fx.names(), ["s1.conf", "s2.conf"], "kept: the copy stays");
    // A kept file is someone else's: dropping its carry must leave it (the
    // file check comes first, so it is the one a wrongly rolled-back keep hits).
    let (r, _) = run(&fx, "s1", "s2");
    let carried = r.expect("kept");
    let was_kept = matches!(carried, Carried::Kept);
    drop(crate::sandbox::CarriedPolicy::from_carried(carried));
    assert_eq!(fx.names(), ["s1.conf", "s2.conf"], "a kept file is never rolled back");
    assert!(was_kept, "the second carry keeps the file the first one left");
}

/// One injected [`Fault`] on this thread, cleared on drop.
struct Armed;

impl Armed {
    fn new(fault: Fault) -> Self {
        inject(Some(fault));
        Armed
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        inject(None);
    }
}

#[test]
fn carry_a_failed_stat_before_the_rename_creates_nothing() {
    let fx = fx();
    fx.write("s1", &fx.valid(), 0o644);
    let _armed = Armed::new(Fault::TempStat);
    let (r, log) = run(&fx, "s1", "s2");
    let e = r.expect_err("the stat fails");
    assert!(e.starts_with("cannot stat the copy in "), "{e}");
    assert_eq!(log, [carry_line("s2", "s1", "failed", Some(&e))]);
    assert_eq!(fx.names(), ["s1.conf"], "no successor file, no temp file");
}

#[test]
fn carry_a_failed_dir_sync_after_the_rename_removes_the_copy() {
    let fx = fx();
    fx.write("s1", &fx.valid(), 0o644);
    let armed = Armed::new(Fault::DirSync);
    let (r, log) = run(&fx, "s1", "s2");
    drop(armed);
    let e = r.expect_err("the dir fsync fails");
    assert!(e.starts_with("cannot sync ") && e.ends_with("; the copy was removed"), "{e}");
    assert_eq!(log, [carry_line("s2", "s1", "failed", Some(&e))]);
    assert_eq!(fx.names(), ["s1.conf"], "the file this carry created is gone, and only it");
    assert_eq!(fs::read_to_string(fx.conf("s1")).unwrap(), fx.valid());

    // A destination that was there first is kept: the fault is never reached.
    let racer = fx.conf("s2");
    let _armed = Armed::new(Fault::DirSync);
    let mut log = Vec::new();
    let r = carry_inner(&fx.env, "s1", "s2", &mut |l| log.push(l), &mut || {
        fs::write(&racer, "mode = off\n").expect("racing write");
    }, &mut || {});
    assert!(matches!(r, Ok(Carried::Kept)), "{r:?}");
    assert_eq!(fs::read_to_string(fx.conf("s2")).unwrap(), "mode = off\n", "the kept file is untouched");
    assert_eq!(fx.names(), ["s1.conf", "s2.conf"]);
}

#[test]
fn carry_a_failed_dir_sync_keeps_a_file_replaced_after_the_rename() {
    let fx = fx();
    fx.write("s1", &fx.valid(), 0o644);
    let (dest, other) = (fx.conf("s2"), fx.env.sessions_dir().join("replacement"));
    let armed = Armed::new(Fault::DirSync);
    let mut log = Vec::new();
    let r = carry_inner(&fx.env, "s1", "s2", &mut |l| log.push(l), &mut || {}, &mut || {
        // Replace it by rename, so the new file can't reuse the old inode.
        fs::write(&other, "mode = off\n").expect("write the replacement");
        fs::rename(&other, &dest).expect("replace the copy");
    });
    drop(armed);
    let e = r.expect_err("the dir fsync fails");
    assert!(e.ends_with("; the copy was not removed: replaced since the carry"), "{e}");
    assert_eq!(fs::read_to_string(&dest).unwrap(), "mode = off\n", "someone else's file survives");
}

#[test]
fn carry_keeps_tools_confined_for_the_successor() {
    let fx = fx();
    let text = format!("{}tools = confined\ntool = ext__ok__tool\n", fx.valid());
    fx.write("s1", &text, 0o600);
    let (r, log) = run(&fx, "s1", "s2");
    assert!(matches!(r, Ok(Carried::Copied(_))), "{r:?} {log:?}");
    assert_eq!(fs::read_to_string(fx.conf("s2")).unwrap(), text);
    let p = policy::load(&fx.conf("s2"), &fx.env).expect("valid").expect("exists");
    assert!(p.confines_tools(), "the successor's tools stay confined");
    assert_eq!(p.named_tools(), ["ext__ok__tool".to_string()]);
}

#[test]
fn carry_never_quotes_the_policy_text() {
    let fx = fx();
    // A malformed line and a bad value, each quoted in the full reason.
    for (i, (text, line)) in [("mode = enforce\nSENTINEL-9d2e without an equals sign\n", 2), ("mode = SENTINEL-9d2e\n", 1)]
        .into_iter()
        .enumerate()
    {
        let (prev, new) = (format!("p{i}"), format!("n{i}"));
        fx.write(&prev, text, 0o644);
        let (r, log) = run(&fx, &prev, &new);
        let e = r.expect_err("an invalid predecessor policy");
        let file = fx.conf(&prev);
        assert_eq!(
            e,
            format!(
                "the predecessor's policy is invalid: {}:{line}: invalid line (`synaps sandbox check {}` gives the reason)",
                file.display(),
                file.display()
            )
        );
        assert!(log.iter().all(|l| !l.contains("SENTINEL")), "{log:?}");
        assert_eq!(log.len(), 1, "{log:?}");
        let (_, report) = policy::check_report(&file, &fx.env, Ok(7));
        assert!(report.contains("SENTINEL-9d2e"), "the validator gives the full reason: {report}");
        assert!(!fx.conf(&new).exists());
    }
}

#[test]
fn the_link_fallback_never_replaces_and_drops_the_temp_name() {
    // The off-Linux stand-in for `renameat2(RENAME_NOREPLACE)`.
    let tmp = tempfile::Builder::new().prefix("sbx-carry-link-").tempdir_in(test_scratch_parent()).expect("tempdir");
    let dir = fs::File::open(tmp.path()).expect("open dir");
    let name = |s: &str| CString::new(s).expect("name");
    fs::write(tmp.path().join(".a"), "A").expect("write");
    link_noreplace(dir.as_raw_fd(), &name(".a"), &name("b")).expect("link");
    assert_eq!(fs::read_to_string(tmp.path().join("b")).expect("b"), "A");
    assert!(!tmp.path().join(".a").exists(), "the temp name is gone");
    fs::write(tmp.path().join(".c"), "C").expect("write");
    let e = link_noreplace(dir.as_raw_fd(), &name(".c"), &name("b")).expect_err("b exists");
    assert_eq!(e.raw_os_error(), Some(libc::EEXIST));
    assert_eq!(fs::read_to_string(tmp.path().join("b")).expect("b"), "A", "kept, never replaced");
    assert!(tmp.path().join(".c").exists(), "the caller removes its temp file");
}
