use super::*;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

#[test]
fn landlock_abi_clipping_table() {
    for abi in 0..MIN_ABI {
        assert_eq!(handled_for_abi(abi), Err(Unavailable::AbiTooOld(abi)), "abi {abi} must be unavailable");
    }
    for abi in MIN_ABI..=9 {
        assert_eq!(handled_for_abi(abi), Ok(WRITE_RIGHTS), "abi {abi}");
    }
    assert_eq!(WRITE_RIGHTS.count_ones(), 12);
    let unhandled = ACCESS_FS_EXECUTE | ACCESS_FS_READ_FILE | ACCESS_FS_READ_DIR | ACCESS_FS_IOCTL_DEV;
    assert_eq!(WRITE_RIGHTS & unhandled, 0, "read/exec/ioctl must stay unhandled");
    assert_eq!(Unavailable::AbiTooOld(2).reason(), "abi-too-old");
}

#[test]
fn landlock_restrict_flags_audit_after_exec_from_abi_7() {
    // 2 is the UAPI value of LANDLOCK_RESTRICT_SELF_LOG_NEW_EXEC_ON (1U << 1).
    assert_eq!(restrict_flags_for_abi(3), 0);
    assert_eq!(restrict_flags_for_abi(6), 0);
    assert_eq!(restrict_flags_for_abi(7), 2);
    assert_eq!(restrict_flags_for_abi(8), 2);
}

#[test]
fn landlock_restrict_flags_ok_only_for_the_exec_audit_bit() {
    assert!(restrict_flags_ok(0));
    for bit in 0..32 {
        // bit 1 = LOG_NEW_EXEC_ON; bit 3 = TSYNC must be refused.
        assert_eq!(restrict_flags_ok(1 << bit), bit == 1, "flag bit {bit}");
    }
    assert!(!restrict_flags_ok(2 | 8), "TSYNC with an allowed bit is still refused");
}

#[test]
fn landlock_rule_access_files_get_only_file_rights() {
    assert_eq!(rule_access(WRITE_RIGHTS, WRITE_RIGHTS, true), WRITE_RIGHTS);
    assert_eq!(rule_access(WRITE_RIGHTS, WRITE_RIGHTS, false), FILE_WRITE_RIGHTS);
    assert_eq!(rule_access(WRITE_RIGHTS, ACCESS_FS_EXECUTE, true), 0, "unhandled rights are dropped");
    assert_eq!(rule_access(ACCESS_FS_WRITE_FILE, WRITE_RIGHTS, false), ACCESS_FS_WRITE_FILE);
}

#[test]
fn landlock_tags_are_unique_and_fit_comm() {
    let a = Tag::next();
    let b = Tag::next();
    assert_ne!(a, b);
    for t in [&a, &b] {
        assert!(t.as_str().starts_with("sbx-") && t.as_str().len() <= 15, "{t}");
        assert_eq!(t.as_cstr().to_bytes(), t.as_str().as_bytes());
    }
    assert_eq!(tag_for(0).as_str(), "sbx-0");
    assert_eq!(tag_for(35).as_str(), "sbx-z");
    assert_eq!(tag_for(36).as_str(), "sbx-10");
    assert!(tag_for(u64::MAX).as_str().len() <= 15, "comm holds 15 bytes: {}", tag_for(u64::MAX));
}

#[test]
fn landlock_each_seed_value_owns_its_own_block_of_tags() {
    const BLOCK: u64 = 36u64.pow(6);
    assert_eq!(tag_for(tag_seed(1)).as_str(), "sbx-1000000");
    assert_eq!(tag_seed(0), 0);
    for t in [1u64, 1_792_485, 36u64.pow(5) - 2] {
        assert_eq!(tag_seed(t + 1) - tag_seed(t), BLOCK, "{t}");
    }
    assert_eq!(tag_seed(36u64.pow(5)), tag_seed(0), "a seed is taken mod 36^5");
    // The largest tag a seed can reach still fits comm and is valid.
    let max = tag_for(tag_seed(36u64.pow(5) - 1) + BLOCK - 1);
    assert_eq!(max.as_str(), "sbx-zzzzzzzzzzz");
    assert!(valid_tag(max.as_str()) && max.as_str().len() == 15, "{max}");
    // Two different seeds never share a tag.
    let block = |seed: u64| (0..1000u64).map(|i| tag_for(tag_seed(seed) + i).to_string()).collect::<std::collections::HashSet<_>>();
    assert!(block(1_792_485).is_disjoint(&block(1_792_486)));
}

#[test]
fn landlock_tag_seeds_come_from_the_os_rng() {
    // Read twice in a row (well inside one second): a clock would repeat.
    let (a, b) = (fresh_seed(), fresh_seed());
    assert!(a < 36u64.pow(5) && b < 36u64.pow(5), "a seed is below 36^5: {a} {b}");
    assert_ne!(a, b, "two reads of the OS RNG (they collide 1 time in 36^5)");
    let block = |seed: u64| (0..1000u64).map(|i| tag_for(tag_seed(seed) + i).to_string()).collect::<std::collections::HashSet<_>>();
    assert!(block(a).is_disjoint(&block(b)));
}

#[test]
fn landlock_this_process_tags_start_at_its_seed() {
    let seed = tag_seed_base();
    assert!(seed < 36u64.pow(5), "the seed is a random value below 36^5, not a clock reading: {seed}");
    let t = Tag::next();
    let n = u64::from_str_radix(&t.as_str()["sbx-".len()..], 36).unwrap();
    assert!(n >= tag_seed(seed) && n < tag_seed(seed) + 36u64.pow(6), "{t} is in the block of {seed}");
}

#[test]
fn landlock_tags_are_validated_in_the_parent() {
    for ok in ["sbx-0", "sbx-z", "sbx-zzzzzzzzzzz", "sbx-0123456789a"] {
        assert!(valid_tag(ok), "{ok:?} must be valid");
        assert_eq!(Tag::new(ok).unwrap().as_str(), ok);
    }
    for bad in [
        "", "sbx-", "sbx", "SBX-1", "foo-1", "sbx-A", "sbx-1/2", "sbx-1 2", "sbx-1\0", "sbx-0123456789ab",
        "sbx-é", "xsbx-1",
    ] {
        assert!(!valid_tag(bad), "{bad:?} must be invalid");
        let err = Tag::new(bad).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
    }
}

// ── Real-kernel tests: LOCAL kernel only (not g1pro/WSL). Opt in with
// SYNAPS_TEST_LANDLOCK=1: they then FAIL (never skip) when Landlock is
// unusable here. Unset, they return early (upstream CI without Landlock). The
// domain is applied only to test children or to a dedicated test thread, and
// each operation under test is done directly (std::fs / libc), asserting its
// errno and the filesystem afterwards. No shell redirection to a path outside
// the rules: it would fail by itself and hide the operation under test.

/// Opt-in gate. Unset: skip. Set to 1 on a host without a usable Landlock:
/// FAIL, so an opted-in run can't be hollow-green.
fn local_kernel() -> bool {
    if std::env::var("SYNAPS_TEST_LANDLOCK").as_deref() != Ok("1") {
        return false;
    }
    match abi_version() {
        Ok(abi) if abi >= MIN_ABI => true,
        other => panic!("SYNAPS_TEST_LANDLOCK=1 but Landlock is unusable here: {other:?}"),
    }
}

/// A fresh tree: `in`, `in2` (granted by [`Tree::ruleset`]) and `out` (not
/// granted) with `out/victim`, `out/empty/` and `out/src`.
struct Tree {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Tree {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for d in ["in", "in2", "out", "out/empty"] {
            std::fs::create_dir(root.join(d)).unwrap();
        }
        for f in ["out/victim", "out/src", "in/a", "in/b", "in/c"] {
            std::fs::write(root.join(f), "keep\n").unwrap();
        }
        Tree { _dir: dir, root }
    }
    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
    fn c(&self, rel: &str) -> CString {
        CString::new(self.p(rel).as_os_str().as_bytes()).unwrap()
    }
    fn ruleset(&self) -> Ruleset {
        let mut rs = Ruleset::new().unwrap();
        assert!(rs.allow(&self.p("in"), WRITE_RIGHTS).unwrap());
        assert!(rs.allow(&self.p("in2"), WRITE_RIGHTS).unwrap());
        rs
    }
    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.p(rel)).unwrap()
    }
    fn exists(&self, rel: &str) -> bool {
        std::fs::symlink_metadata(self.p(rel)).is_ok()
    }
}

/// 0 for success, else the errno (-1 if there is none).
fn errno<T>(r: io::Result<T>) -> i32 {
    match r {
        Ok(_) => 0,
        Err(e) => e.raw_os_error().unwrap_or(-1),
    }
}

/// 0 for a libc call returning 0, else errno.
fn sys(r: libc::c_int) -> i32 {
    if r == 0 {
        0
    } else {
        io::Error::last_os_error().raw_os_error().unwrap_or(-1)
    }
}

fn open_append(p: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new().append(true).open(p)
}

/// The calling thread's "NoNewPrivs:" value from /proc/thread-self/status.
fn thread_nnp() -> String {
    let status = std::fs::read_to_string("/proc/thread-self/status").unwrap();
    status.lines().find_map(|l| l.strip_prefix("NoNewPrivs:")).expect("NoNewPrivs line").trim().to_string()
}

fn thread_comm() -> String {
    std::fs::read_to_string("/proc/thread-self/comm").unwrap().trim().to_string()
}

const EACCES: i32 = libc::EACCES;
const EXDEV: i32 = libc::EXDEV;

#[test]
fn landlock_local_direct_ops_inside_succeed_and_outside_are_refused() {
    if !local_kernel() {
        return;
    }
    let t = Tree::new();
    let rs = t.ruleset();
    let (victim, fifo) = (t.c("out/victim"), t.c("out/f"));
    let got = run_confined(&rs, &Tag::next(), || {
        vec![
            ("create in/ok", errno(std::fs::write(t.p("in/ok"), "a"))),
            ("mkdir -p in/t/u", errno(std::fs::create_dir_all(t.p("in/t/u")))),
            ("rm -r in/t", errno(std::fs::remove_dir_all(t.p("in/t")))),
            ("unlink in/a", errno(std::fs::remove_file(t.p("in/a")))),
            ("create out/new", errno(std::fs::OpenOptions::new().write(true).create_new(true).open(t.p("out/new")))),
            ("append out/victim", errno(open_append(&t.p("out/victim")))),
            ("open O_TRUNC out/victim", errno(std::fs::OpenOptions::new().write(true).truncate(true).open(t.p("out/victim")))),
            // SAFETY: a valid NUL-terminated path.
            ("truncate(2) out/victim", sys(unsafe { libc::truncate(victim.as_ptr(), 0) })),
            ("unlink out/victim", errno(std::fs::remove_file(t.p("out/victim")))),
            ("rmdir out/empty", errno(std::fs::remove_dir(t.p("out/empty")))),
            ("mkdir out/d", errno(std::fs::create_dir(t.p("out/d")))),
            ("symlink out/s", errno(std::os::unix::fs::symlink("victim", t.p("out/s")))),
            // SAFETY: a valid NUL-terminated path.
            ("mkfifo out/f", sys(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) })),
        ]
    })
    .unwrap();
    let want = vec![
        ("create in/ok", 0),
        ("mkdir -p in/t/u", 0),
        ("rm -r in/t", 0),
        ("unlink in/a", 0),
        ("create out/new", EACCES),
        ("append out/victim", EACCES),
        ("open O_TRUNC out/victim", EACCES),
        ("truncate(2) out/victim", EACCES),
        ("unlink out/victim", EACCES),
        ("rmdir out/empty", EACCES),
        ("mkdir out/d", EACCES),
        ("symlink out/s", EACCES),
        ("mkfifo out/f", EACCES),
    ];
    assert_eq!(got, want, "(operation, errno) on a confined thread");
    assert_eq!(t.read("in/ok"), "a");
    assert!(!t.exists("in/t") && !t.exists("in/a"));
    assert_eq!(t.read("out/victim"), "keep\n", "outside file intact");
    assert!(t.p("out/empty").is_dir(), "outside dir intact");
    for gone in ["out/new", "out/d", "out/s", "out/f"] {
        assert!(!t.exists(gone), "{gone} must not exist");
    }
}

#[test]
fn landlock_local_rename_and_link_across_the_boundary() {
    if !local_kernel() {
        return;
    }
    let t = Tree::new();
    let rs = t.ruleset();
    let rename = |a: &str, b: &str| {
        let (a, b) = (t.c(a), t.c(b));
        // SAFETY: valid NUL-terminated paths. rename(2) itself: no copy fallback.
        sys(unsafe { libc::rename(a.as_ptr(), b.as_ptr()) })
    };
    let link = |a: &str, b: &str| {
        let (a, b) = (t.c(a), t.c(b));
        // SAFETY: valid NUL-terminated paths.
        sys(unsafe { libc::link(a.as_ptr(), b.as_ptr()) })
    };
    let got = run_confined(&rs, &Tag::next(), || {
        vec![
            ("rename in/a -> in2/a", rename("in/a", "in2/a")),
            ("rename in/b -> out/b", rename("in/b", "out/b")),
            ("rename out/src -> in/src", rename("out/src", "in/src")),
            ("link out/src -> in/hl", link("out/src", "in/hl")),
            ("link in/c -> out/hl", link("in/c", "out/hl")),
        ]
    })
    .unwrap();
    // A rename or link out of the set, or into it from outside, is refused:
    // EACCES when a right is missing on either side, EXDEV when only the
    // REFER (reparenting) check fails.
    let want = vec![
        ("rename in/a -> in2/a", 0),
        ("rename in/b -> out/b", EACCES),
        ("rename out/src -> in/src", EACCES),
        ("link out/src -> in/hl", EXDEV),
        ("link in/c -> out/hl", EACCES),
    ];
    assert_eq!(got, want, "(operation, errno) on a confined thread");
    assert!(t.exists("in2/a") && !t.exists("in/a"));
    assert!(t.exists("in/b") && !t.exists("out/b"));
    assert!(t.exists("out/src") && !t.exists("in/src"));
    assert!(!t.exists("in/hl") && !t.exists("out/hl"));
}

#[test]
fn landlock_local_symlink_inside_pointing_outside_is_refused() {
    if !local_kernel() {
        return;
    }
    let t = Tree::new();
    std::os::unix::fs::symlink(t.p("out/victim"), t.p("in/esc-file")).unwrap();
    std::os::unix::fs::symlink(t.p("out"), t.p("in/esc-dir")).unwrap();
    let rs = t.ruleset();
    let got = run_confined(&rs, &Tag::next(), || {
        vec![
            ("append in/esc-file", errno(open_append(&t.p("in/esc-file")))),
            ("create in/esc-dir/new", errno(std::fs::write(t.p("in/esc-dir/new"), "x"))),
            ("unlink in/esc-dir/victim", errno(std::fs::remove_file(t.p("in/esc-dir/victim")))),
            ("symlink in/s2 -> out/victim", errno(std::os::unix::fs::symlink(t.p("out/victim"), t.p("in/s2")))),
            ("append in/s2", errno(open_append(&t.p("in/s2")))),
            ("unlink in/esc-file", errno(std::fs::remove_file(t.p("in/esc-file")))),
        ]
    })
    .unwrap();
    let want = vec![
        ("append in/esc-file", EACCES),
        ("create in/esc-dir/new", EACCES),
        ("unlink in/esc-dir/victim", EACCES),
        // Making a symlink inside is allowed; writing through it is not.
        ("symlink in/s2 -> out/victim", 0),
        ("append in/s2", EACCES),
        // Removing the link itself (inside) is allowed; its target stays.
        ("unlink in/esc-file", 0),
    ];
    assert_eq!(got, want, "(operation, errno) on a confined thread");
    assert_eq!(t.read("out/victim"), "keep\n");
    assert!(!t.exists("out/new"));
    // The two allowed steps left their own traces: the new link, the old one gone.
    assert_eq!(std::fs::read_link(t.p("in/s2")).unwrap(), t.p("out/victim"), "in/s2 was made, pointing outside");
    assert!(!t.exists("in/esc-file"), "the link in/esc-file was removed");
    assert!(t.p("in/esc-dir").is_symlink(), "in/esc-dir untouched");
}

#[test]
fn landlock_local_file_rule_grants_that_file_only() {
    if !local_kernel() {
        return;
    }
    let t = Tree::new();
    std::fs::write(t.p("out/granted"), "g\n").unwrap();
    let mut rs = t.ruleset();
    // Asking for every write right on a file must still build a file rule.
    assert!(rs.allow(&t.p("out/granted"), WRITE_RIGHTS).unwrap(), "file rule must be added");
    assert_eq!(rs.rules(), 3);
    let granted = t.c("out/granted");
    let (got, after_append) = run_confined(&rs, &Tag::next(), || {
        let append = errno(open_append(&t.p("out/granted")).and_then(|mut f| {
            use std::io::Write;
            f.write_all(b"more\n")
        }));
        // Reading is not handled: observe the append before the truncate.
        let after_append = std::fs::read_to_string(t.p("out/granted")).unwrap_or_default();
        let ops = vec![
            ("append out/granted", append),
            // SAFETY: a valid NUL-terminated path.
            ("truncate(2) out/granted", sys(unsafe { libc::truncate(granted.as_ptr(), 2) })),
            ("unlink out/granted", errno(std::fs::remove_file(t.p("out/granted")))),
            ("append out/victim", errno(open_append(&t.p("out/victim")))),
            ("create out/new", errno(std::fs::write(t.p("out/new"), "x"))),
        ];
        (ops, after_append)
    })
    .unwrap();
    let want = vec![
        ("append out/granted", 0),
        ("truncate(2) out/granted", 0),
        ("unlink out/granted", EACCES),
        ("append out/victim", EACCES),
        ("create out/new", EACCES),
    ];
    assert_eq!(got, want, "(operation, errno) on a confined thread");
    assert_eq!(after_append, "g\nmore\n", "the append wrote");
    assert_eq!(t.read("out/granted"), "g\n", "then truncated to 2 bytes");
    assert_eq!(t.read("out/victim"), "keep\n");
    assert!(!t.exists("out/new"));
}

// rule-6 exception (foreman 10-18): /dev/null, open only, non-destructive; a device rule can't be tested on a scratch node
#[test]
fn landlock_local_dev_null_needs_its_rule() {
    if !local_kernel() {
        return;
    }
    let t = Tree::new();
    let open_null = || errno(std::fs::OpenOptions::new().write(true).open("/dev/null"));
    let rs = t.ruleset();
    assert_eq!(run_confined(&rs, &Tag::next(), open_null).unwrap(), EACCES, "without its rule /dev/null is refused");
    let mut rs = t.ruleset();
    assert!(rs.allow(Path::new("/dev/null"), FILE_WRITE_RIGHTS).unwrap());
    assert_eq!(run_confined(&rs, &Tag::next(), open_null).unwrap(), 0, "with its rule /dev/null opens for write");
}

#[test]
fn landlock_local_allow_fd_binds_the_held_object_not_its_name() {
    if !local_kernel() {
        return;
    }
    let t = Tree::new();
    let open_dir = |rel: &str| {
        std::fs::OpenOptions::new().read(true).custom_flags(libc::O_PATH | libc::O_CLOEXEC).open(t.p(rel)).unwrap()
    };
    // `held`: removed while its fd is held, then a new directory takes the
    // name. `moved`: renamed away while held, a new directory takes the name.
    std::fs::create_dir(t.p("held")).unwrap();
    std::fs::create_dir(t.p("moved")).unwrap();
    let held = open_dir("held");
    let moved = open_dir("moved");
    std::fs::remove_dir(t.p("held")).unwrap();
    std::fs::create_dir(t.p("held")).unwrap();
    std::fs::rename(t.p("moved"), t.p("moved-away")).unwrap();
    std::fs::create_dir(t.p("moved")).unwrap();
    let mut rs = Ruleset::new().unwrap();
    assert!(rs.allow_fd(held.as_fd(), WRITE_RIGHTS).unwrap(), "a rule on a held, removed directory");
    assert!(rs.allow_fd(moved.as_fd(), WRITE_RIGHTS).unwrap(), "a rule on a held, renamed directory");
    let got = run_confined(&rs, &Tag::next(), || {
        vec![
            ("create held/new (the new object)", errno(std::fs::write(t.p("held/new"), "x"))),
            ("create moved/new (the new object)", errno(std::fs::write(t.p("moved/new"), "x"))),
            ("create moved-away/new (the held object)", errno(std::fs::write(t.p("moved-away/new"), "x"))),
        ]
    })
    .unwrap();
    let want = vec![
        ("create held/new (the new object)", EACCES),
        ("create moved/new (the new object)", EACCES),
        ("create moved-away/new (the held object)", 0),
    ];
    assert_eq!(got, want, "the rules cover the objects the fds hold, not what their names point to now");
    assert!(!t.exists("held/new") && !t.exists("moved/new"));
    assert_eq!(t.read("moved-away/new"), "x");
}

#[test]
fn landlock_local_confined_child_inherits_no_ruleset_fd() {
    if !local_kernel() {
        return;
    }
    let t = Tree::new();
    let rs = t.ruleset();
    let mut cmd = Command::new("/bin/ls");
    cmd.env("LC_ALL", "C").args(["-l", "/proc/self/fd"]);
    confine_command(&mut cmd, &rs, Tag::next()).unwrap();
    let o = cmd.output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success(), "ls failed: {}", String::from_utf8_lossy(&o.stderr));
    assert!(out.matches(" -> ").count() >= 3, "expected the child's fd links:\n{out}");
    // The hook's duplicate is O_CLOEXEC: it closes at exec.
    assert!(!out.contains("landlock-ruleset"), "the exec'd child holds a ruleset fd:\n{out}");
}

#[test]
fn landlock_local_missing_path_and_bad_ruleset_fd_are_errors() {
    if !local_kernel() {
        return;
    }
    let t = Tree::new();
    let mut rs = t.ruleset();
    let err = rs.allow(&t.p("missing"), WRITE_RIGHTS).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert_eq!(rs.rules(), 2, "a failed allow adds no rule");

    // An fd that is not a Landlock ruleset: the restrict fails with EBADFD,
    // the closure never runs, and a confined Command fails to spawn (nothing
    // is exec'd, no unrestricted fallback).
    let not_a_ruleset = OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
    let bogus = Ruleset { fd: not_a_ruleset, abi: rs.abi(), handled: rs.handled(), flags: rs.flags(), rules: 0 };
    let ran = AtomicBool::new(false);
    let err = run_confined(&bogus, &Tag::next(), || ran.store(true, Ordering::SeqCst)).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EBADFD), "{err}");
    assert!(!ran.load(Ordering::SeqCst), "the closure must not run when the restrict fails");

    let marker = t.p("in/child-ran");
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(format!("echo x > {}", marker.display()));
    confine_command(&mut cmd, &bogus, Tag::next()).unwrap();
    let err = cmd.status().unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EBADFD), "{err}");
    assert!(!marker.exists(), "the child must not have run");
}

#[test]
fn landlock_local_unknown_flags_are_refused_before_any_change() {
    if !local_kernel() {
        return;
    }
    let t = Tree::new();
    let rs = t.ruleset();
    let tag = Tag::next();
    // A fresh thread that ends right after. Bit 31 is not a Landlock flag (a
    // refused TSYNC is covered by the flag table: never send it to a kernel).
    let (result, before, after) = std::thread::scope(|s| {
        s.spawn(|| {
            let before = (thread_nnp(), thread_comm());
            // SAFETY: rs is alive; this is a dedicated thread that ends next.
            let r = unsafe { restrict_current(rs.fd.as_raw_fd(), 1 << 31, &tag) };
            let after = (thread_nnp(), thread_comm());
            (r.map_err(|e| e.raw_os_error()), before, after)
        })
        .join()
        .unwrap()
    });
    assert_eq!(result, Err(Some(libc::EINVAL)));
    assert_eq!(before, after, "no_new_privs and comm must be unchanged after a refused flag");
}

#[test]
fn landlock_local_confined_command_child_is_restricted_after_the_ruleset_is_dropped() {
    if !local_kernel() {
        return;
    }
    let t = Tree::new();
    let r = t.root.display();
    let mut cmd = Command::new("/bin/sh");
    // Each operation runs and reports its own status; stderr is captured (a
    // pipe), never redirected to a path.
    cmd.env("LC_ALL", "C").arg("-c").arg(format!(
        "echo a > {r}/in/ok; echo in=$?; rm -f {r}/out/victim; echo rm=$?; mkdir {r}/out/d; echo mkdir=$?; \
         ln -s x {r}/out/l; echo symlink=$?; mv {r}/in/a {r}/in2/a; echo mv_in=$?; \
         grep NoNewPrivs /proc/self/status"
    ));
    {
        let rs = t.ruleset();
        confine_command(&mut cmd, &rs, Tag::next()).unwrap();
    } // the ruleset is dropped: the hook holds its own fd.
    let o = cmd.output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    let err = String::from_utf8_lossy(&o.stderr);
    for want in ["in=0", "rm=1", "mkdir=1", "symlink=1", "mv_in=0", "NoNewPrivs:\t1"] {
        assert!(out.lines().any(|l| l == want), "missing line {want:?}; stdout:\n{out}\nstderr:\n{err}");
    }
    assert_eq!(err.matches("Permission denied").count(), 3, "rm, mkdir, ln each report EACCES:\n{err}");
    assert_eq!(t.read("out/victim"), "keep\n");
    assert!(!t.exists("out/d") && !t.exists("out/l") && t.exists("in2/a") && t.exists("in/ok"));
}

#[test]
fn landlock_local_confined_thread_leaves_the_caller_unrestricted() {
    if !local_kernel() {
        return;
    }
    let t = Tree::new();
    let rs = t.ruleset();
    let nnp_before = thread_nnp();
    let r = t.root.display().to_string();
    let (direct, child, nnp_inside) = run_confined(&rs, &Tag::next(), || {
        let direct = errno(std::fs::write(t.p("out/new"), "x"));
        let child = Command::new("/bin/sh").arg("-c").arg(format!("echo b > {r}/out/new2")).output().unwrap();
        (direct, child.status.success(), thread_nnp())
    })
    .unwrap();
    assert_eq!(direct, EACCES, "the confined thread is restricted");
    assert!(!child, "its child inherits the domain");
    assert_eq!(nnp_inside, "1");
    assert!(!t.exists("out/new") && !t.exists("out/new2"));
    // The calling thread and its own children are untouched.
    assert_eq!(thread_nnp(), nnp_before, "the caller's no_new_privs must not change");
    std::fs::write(t.p("out/caller"), "x").expect("caller thread must still write outside");
    let o = Command::new("/bin/sh").arg("-c").arg(format!("echo c > {r}/out/caller-child")).status().unwrap();
    assert!(o.success() && t.exists("out/caller-child"), "caller's own child unrestricted");
}
