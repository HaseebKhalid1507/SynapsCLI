//! Unit tests for the policy file: parse, validation, lookup, kill switch,
//! validator report, log lines. Filesystem work in a scratch tree that is
//! never under /tmp (see `test_scratch_parent`). Only `policy_local_*`
//! applies a Landlock domain, to a dedicated thread, and only with
//! SYNAPS_TEST_LANDLOCK=1 (LOCAL kernel, not g1pro/WSL).

use super::*;
use crate::sandbox::landlock::{abi_version, run_confined, Tag, MIN_ABI};
use std::fs;
use std::io::Read as _;
use std::os::unix::fs::symlink;

struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    env: PolicyEnv,
}

/// A fake tree: home/ (fake $HOME) holding the base dir one level down
/// (home/x/base), a lane dir, a repo with a .git common dir, a cargo home,
/// and h2/me + b2/: a disjoint $HOME and base dir for `env2`.
fn fx() -> Fx {
    let tmp = tempfile::Builder::new().prefix("sbx-policy-").tempdir_in(test_scratch_parent()).expect("tempdir");
    let root = fs::canonicalize(tmp.path()).expect("canonical tempdir");
    for d in [
        "home/x/base/sandbox/sessions",
        "home/x/base/logs",
        "lane/sub",
        "other",
        "repo/.git/objects",
        "repo/.git/refs",
        "repo/.git/logs",
        "repo/.git/worktrees",
        "repo/.git/hooks",
        "cargo/registry",
        "cargo/git",
        "cargo/bin",
        "h2/me",
        "b2/logs",
    ] {
        fs::create_dir_all(root.join(d)).expect("mkdir");
    }
    for f in ["repo/.git/HEAD", "repo/.git/config", "cargo/.package-cache", "cargo/.global-cache", "lane/file.txt"] {
        fs::write(root.join(f), b"x").expect("write");
    }
    let env = PolicyEnv { home: root.join("home"), base_dir: root.join("home/x/base") };
    Fx { _tmp: tmp, root, env }
}

impl Fx {
    fn r(&self, rel: &str) -> String {
        self.root.join(rel).display().to_string()
    }
    fn parse(&self, text: &str) -> Result<Policy, PolicyError> {
        parse(text, Path::new("/test/s.conf"), &self.env)
    }
    /// $HOME (h2/me) and the base dir (b2) in separate trees, neither holding
    /// the other, so each name guard can be reached on its own.
    fn env2(&self) -> PolicyEnv {
        PolicyEnv { home: self.root.join("h2/me"), base_dir: self.root.join("b2") }
    }
    fn write_policy(&self, id: &str, text: &str) -> PathBuf {
        let p = self.env.policy_path(id).expect("valid id");
        fs::write(&p, text).expect("write policy");
        p
    }
    fn id(&self, rel: &str) -> (u64, u64) {
        let m = fs::metadata(self.root.join(rel)).expect("stat");
        (m.dev(), m.ino())
    }
}

fn mkfifo(p: &Path) {
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).expect("path");
    // SAFETY: a valid NUL-terminated path; mkfifo has no other preconditions.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo {}", p.display());
}

fn lookup_quiet(env: &PolicyEnv, id: &str, sw: &KillSwitch) -> Lookup {
    lookup_with(env, id, sw, &mut |_| {})
}

fn file_rules(p: &Policy, root: &Path) -> Vec<(&'static str, String, u64, bool, RuleClass, usize)> {
    p.rules
        .iter()
        .filter(|r| r.class != RuleClass::Baseline)
        .map(|r| {
            let rel = r.path.strip_prefix(root).expect("rule under the fixture").display().to_string();
            (r.key, rel, r.access, r.dir, r.class, r.line)
        })
        .collect()
}

/// Every case is checked and every mismatch reported, not just the first.
fn expect_refusals(cases: Vec<(String, usize, &str)>, parse: impl Fn(&str) -> Result<Policy, PolicyError>, root: &Path) {
    let mut bad = Vec::new();
    for (text, line, reason) in cases {
        match parse(text.as_str()) {
            Ok(p) => bad.push(format!("ACCEPTED {text:?}: rules {:?}", file_rules(&p, root))),
            Err(e) if e.line != line || !e.reason.contains(reason) => {
                bad.push(format!("{text:?}: got line {} {:?}, want line {line} {reason:?}", e.line, e.reason))
            }
            Err(e) if !e.refusal().contains("commands can't run until it's fixed") => bad.push(e.refusal()),
            Err(_) => {}
        }
    }
    assert!(bad.is_empty(), "{} case(s) wrong:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn policy_parses_a_full_file_into_baseline_and_rules() {
    let fx = fx();
    let text = format!(
        "# stage-1 worker\nmode = enforce\nlane = synapscli\nwrite = {}\nwrite_file = {}\ninfra = {}\ngit_common = {}\ncargo_home = {}\ncontact = Ask your foreman.\n",
        fx.r("lane"),
        fx.r("lane/file.txt"),
        fx.r("other"),
        fx.r("repo/.git"),
        fx.r("cargo")
    );
    let p = fx.parse(&text).expect("valid policy");
    assert_eq!(p.mode(), Mode::Enforce);
    assert!(!p.required());
    assert_eq!(p.lane(), Some("synapscli"));
    assert_eq!(p.subagents(), Subagents::Enforce);
    assert_eq!(p.audit_log(), Some(Path::new(DEFAULT_AUDIT_LOG)));
    assert_eq!(p.contact(), "Ask your foreman.");
    assert_eq!(p.hash().len(), 16);
    assert!(p.hash().bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(p.hash(), short_hash(text.as_bytes()));
    assert_eq!(p.file(), Path::new("/test/s.conf"));

    use RuleClass::{Infra, Work};
    let w = WRITE_RIGHTS;
    let f = FILE_WRITE_RIGHTS;
    let expected: Vec<(&'static str, String, u64, bool, RuleClass, usize)> = vec![
        ("write", "lane".into(), w, true, Work, 4),
        ("write_file", "lane/file.txt".into(), f, false, Work, 5),
        ("infra", "other".into(), w, true, Infra, 6),
        ("git_common", "repo/.git/objects".into(), w, true, Infra, 7),
        ("git_common", "repo/.git/refs".into(), w, true, Infra, 7),
        ("git_common", "repo/.git/logs".into(), w, true, Infra, 7),
        ("git_common", "repo/.git/worktrees".into(), w, true, Infra, 7),
        ("cargo_home", "cargo/registry".into(), w, true, Infra, 8),
        ("cargo_home", "cargo/git".into(), w, true, Infra, 8),
        ("cargo_home", "cargo/.package-cache".into(), f, false, Infra, 8),
        ("cargo_home", "cargo/.global-cache".into(), f, false, Infra, 8),
    ];
    assert_eq!(file_rules(&p, &fx.root), expected);
    // The accessors show the same rules.
    let via_api: Vec<_> = p.rules().iter().filter(|r| r.class() != RuleClass::Baseline).map(|r| (r.key(), r.path().to_path_buf(), r.access(), r.is_dir(), r.line())).collect();
    assert_eq!(via_api.len(), expected.len());
    assert_eq!(via_api[1], ("write_file", fx.root.join("lane/file.txt"), f, false, 5));
    let skipped: Vec<_> = p.skipped().iter().filter(|s| s.line != 0).cloned().collect();
    assert_eq!(skipped, vec![Skipped { line: 8, key: "cargo_home", path: fx.root.join("cargo/.package-cache-mutate") }]);

    // The common dir itself, hooks/, config, the cargo home itself and bin/ stay read-only.
    for denied in ["repo/.git", "repo/.git/hooks", "repo/.git/config", "cargo", "cargo/bin"] {
        assert!(!p.rules.iter().any(|r| r.path == fx.root.join(denied)), "{denied} must not be a rule");
    }
    let base = |path: &str| p.rules.iter().find(|r| r.class == RuleClass::Baseline && r.path == Path::new(path)).cloned();
    assert_eq!(base("/tmp").map(|r| (r.access, r.dir)), Some((WRITE_RIGHTS, true)));
    assert_eq!(base("/dev/null").map(|r| (r.access, r.dir)), Some((FILE_WRITE_RIGHTS, false)));
    assert_eq!(base("/dev/zero").map(|r| (r.access, r.dir)), Some((FILE_WRITE_RIGHTS, false)));
    assert_eq!(base("/proc").map(|r| (r.access, r.dir)), Some((FILE_WRITE_RIGHTS | ACCESS_FS_MAKE_REG, true)));
    assert!(p.rules.iter().take_while(|r| r.class == RuleClass::Baseline).count() >= 3, "baseline comes first");
}

#[test]
fn policy_refuses_each_invalid_line() {
    let fx = fx();
    symlink(fx.root.join("home"), fx.root.join("lane/to-home")).expect("symlink");
    symlink(fx.root.join("home/x/base/sandbox"), fx.root.join("lane/to-base")).expect("symlink");
    mkfifo(&fx.root.join("lane/fifo"));
    let cases: Vec<(String, usize, &str)> = vec![
        ("mode = maybe\n".into(), 1, "`mode` must be `enforce` or `off`"),
        (format!("write = {}\n", fx.r("lane")), 0, "missing `mode = enforce` or `mode = off`"),
        ("mode = enforce\nmode = off\n".into(), 2, "`mode` set twice (first on line 1)"),
        ("mode = enforce\nfrobnicate = 1\n".into(), 2, "unknown key `frobnicate`"),
        ("mode = enforce\nwrite\n".into(), 2, "expected `key = value`"),
        ("mode = enforce\nwrite =\n".into(), 2, "`write` has no value"),
        ("mode = enforce\nwrite = relative/dir\n".into(), 2, "is not absolute"),
        ("mode = enforce\nwrite = ~/x\n".into(), 2, "is not absolute"),
        ("mode = enforce\nwrite = /\n".into(), 2, "refuses `/`"),
        ("mode = enforce\nwrite = /var/tmp\n".into(), 2, "fewer than 3 path components"),
        (format!("mode = enforce\nwrite = {}\n", fx.r("home")), 2, "or an ancestor of it"),
        (format!("mode = enforce\nwrite = {}\n", fx.root.display()), 2, "or an ancestor of it"),
        (format!("mode = enforce\nwrite = {}\n", fx.r("home/x/base")), 2, "overlaps the Synaps base dir"),
        (format!("mode = enforce\nwrite = {}\n", fx.r("home/x/base/sandbox/sessions")), 2, "overlaps the Synaps base dir"),
        (format!("mode = enforce\nwrite = {}\n", fx.r("home/x")), 2, "overlaps the Synaps base dir"),
        (format!("mode = enforce\nwrite = {}\n", fx.r("lane/to-home")), 2, "or an ancestor of it"),
        (format!("mode = enforce\nwrite = {}\n", fx.r("lane/to-base")), 2, "overlaps the Synaps base dir"),
        (format!("mode = enforce\nwrite = {}\n", fx.r("lane/file.txt")), 2, "is not a directory"),
        (format!("mode = enforce\nwrite_file = {}\n", fx.r("lane")), 2, "is not a regular file"),
        // A FIFO has one link: only the type check refuses it.
        (format!("mode = enforce\nwrite_file = {}\n", fx.r("lane/fifo")), 2, "is not a regular file"),
        (format!("mode = enforce\ninfra_file = {}\n", fx.r("lane/sub")), 2, "is not a regular file"),
        (format!("mode = enforce\ngit_common = {}\n", fx.r("lane")), 2, "is not a git common dir"),
        (format!("mode = enforce\ncargo_home = {}\n", fx.r("lane/file.txt")), 2, "is not a directory"),
        ("mode = enforce\nsubagents = off\n".into(), 2, "`subagents = off` needs `mode = off`"),
        ("mode = off\nrequired = true\n".into(), 2, "`required = true` needs `mode = enforce`"),
        ("mode = enforce\nrequired = yes\n".into(), 2, "`required` must be `true` or `false`"),
        ("mode = enforce\nlane = a/b\n".into(), 2, "`lane` must be"),
        ("mode = enforce\naudit_log = audit.log\n".into(), 2, "`audit_log` must be `off` or `/var/log/audit/audit.log`"),
        // The daemon reads only the default log: another file can't be named.
        ("mode = enforce\naudit_log = /var/log/other.log\n".into(), 2, "`audit_log` must be `off` or `/var/log/audit/audit.log`"),
        ("mode = enforce\naudit_log = /var/log/audit/audit.log.1\n".into(), 2, "`audit_log` must be `off` or `/var/log/audit/audit.log`"),
        ("mode = enforce\naudit_log = /var/log/audit//audit.log\n".into(), 2, "`audit_log` must be `off` or `/var/log/audit/audit.log`"),
        (format!("mode = enforce\ncontact = {}\n", "x".repeat(201)), 2, "`contact` must be one line of at most 200 characters"),
        ("mode = enforce\ncontact = Ask R&R for the Password.\n".into(), 2, "without prompt words"),
    ];
    expect_refusals(cases, |t| fx.parse(t), &fx.root);
}

#[test]
fn policy_each_scope_guard_refuses_on_its_own() {
    // env2: $HOME = h2/me and base = b2 in separate trees, and an identity
    // nothing protects, so exactly one guard can refuse each case (h2 is an
    // ancestor of $HOME only, not of the base dir).
    let fx = fx();
    let g = Guard::new(&fx.env2()).expect("guard");
    let free = fx.id("other");
    let mut bad = Vec::new();
    for (c, want) in [
        (PathBuf::from("/opt/x"), "has fewer than 3 path components"),
        (fx.root.join("h2/me"), "is $HOME"),
        (fx.root.join("h2"), "is $HOME"),
        (fx.root.join("b2"), "overlaps the Synaps base dir"),
        (fx.root.join("b2/logs"), "overlaps the Synaps base dir"),
    ] {
        match g.check_scope(&c, free) {
            Ok(()) => bad.push(format!("ACCEPTED {}", c.display())),
            Err(e) if !e.contains(want) => bad.push(format!("{}: {e}, want {want:?}", c.display())),
            Err(_) => {}
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
    assert_eq!(g.check_scope(&fx.root.join("lane"), free), Ok(()));
    // Through a symlink whose own name passes every check: only naming the
    // opened object (its kernel name, b2/logs) refuses it.
    symlink(fx.root.join("b2/logs"), fx.root.join("lane/to-b2-logs")).expect("symlink");
    let e = parse(&format!("mode = enforce\nwrite = {}\n", fx.r("lane/to-b2-logs")), Path::new("/test/s.conf"), &fx.env2())
        .expect_err("named by the kernel: under the base dir");
    assert!(e.reason.contains(&format!("`{}` overlaps the Synaps base dir", fx.r("b2/logs"))), "{e}");
}

#[test]
fn policy_refuses_a_baseline_directory_that_holds_home_or_the_base_dir() {
    // Check-only, on scratch stand-ins for /tmp: nothing is granted or written.
    let fx = fx();
    symlink(fx.root.join("home"), fx.root.join("lane/to-home")).expect("symlink");
    let parse_b = |root: &str| parse_with("mode = enforce\n", Path::new("/test/s.conf"), &fx.env, &[(root, WRITE_RIGHTS, true)]);
    let mut bad = Vec::new();
    for root in [fx.r("home/x"), fx.r("home"), fx.root.display().to_string(), fx.r("lane/to-home"), fx.r("home/x/base")] {
        match parse_b(root.as_str()) {
            Ok(_) => bad.push(format!("ACCEPTED baseline {root}")),
            Err(e) if e.line != 0 || !e.reason.contains("must not be under /tmp, /dev/shm") => bad.push(format!("{root}: {e}")),
            Err(_) => {}
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
    let p = parse_b(fx.r("lane").as_str()).expect("a baseline dir beside them is fine");
    assert_eq!(p.rules.iter().map(|r| (r.path.clone(), r.class)).collect::<Vec<_>>(), vec![(fx.root.join("lane"), RuleClass::Baseline)]);

    // Each half alone: by name, and by identity (a bind mount of a protected dir).
    let g = Guard::new(&fx.env).expect("guard");
    assert!(g.check_baseline(&fx.root.join("home/x"), fx.id("other")).is_err(), "by name");
    assert!(g.check_baseline(&fx.root.join("other"), fx.id("home/x")).is_err(), "by identity");
    assert!(g.check_baseline(&fx.root.join("other"), fx.id("home/x/base/sandbox")).is_err(), "the sandbox dir's identity");
    assert_eq!(g.check_baseline(&fx.root.join("other"), fx.id("other")), Ok(()));
}

#[test]
fn policy_baseline_skips_only_a_missing_path() {
    let fx = fx();
    let missing = fx.r("lane/missing");
    let enotdir = format!("{}/x", fx.r("lane/file.txt"));
    let lane = fx.r("lane");
    let baseline: [(&str, u64, bool); 2] = [(missing.as_str(), WRITE_RIGHTS, true), (lane.as_str(), WRITE_RIGHTS, true)];
    let p = parse_with("mode = enforce\n", Path::new("/test/s.conf"), &fx.env, &baseline).expect("missing = skipped");
    assert_eq!(p.skipped, vec![Skipped { line: 0, key: "baseline", path: PathBuf::from(&missing) }]);
    assert_eq!(p.rules.len(), 1);
    let baseline: [(&str, u64, bool); 2] = [(enotdir.as_str(), WRITE_RIGHTS, true), (lane.as_str(), WRITE_RIGHTS, true)];
    let e = parse_with("mode = enforce\n", Path::new("/test/s.conf"), &fx.env, &baseline).expect_err("ENOTDIR is not missing");
    assert_eq!(e.line, 0);
    assert!(e.reason.contains(&format!("cannot open the baseline path `{enotdir}`")), "{e}");
}

#[test]
fn policy_refuses_a_file_with_more_than_one_name() {
    let fx = fx();
    // A protected file with a second name in an allowed dir: a grant would
    // cover the inode, so the protected name too.
    let victim = fx.env.sessions_dir().join("victim.conf");
    fs::write(&victim, "mode = off\n").expect("write");
    fs::hard_link(&victim, fx.root.join("lane/alias.conf")).expect("hard link");
    let mut cases: Vec<(String, usize, &str)> = ["write_file", "infra_file"]
        .iter()
        .map(|k| (format!("mode = enforce\n{k} = {}\n", fx.r("lane/alias.conf")), 2, "has 2 hard links"))
        .collect();
    fs::hard_link(fx.root.join("cargo/.global-cache"), fx.root.join("other/global-cache-2")).expect("hard link");
    cases.push((format!("mode = enforce\ncargo_home = {}\n", fx.r("cargo")), 2, "has 2 hard links"));
    expect_refusals(cases, |t| fx.parse(t), &fx.root);
}

#[test]
fn policy_file_must_be_one_regular_file() {
    let fx = fx();
    let sw = KillSwitch::new();
    let invalid = |id: &str, want: &str| match lookup_quiet(&fx.env, id, &sw) {
        Lookup::Invalid(e) if e.reason.contains(want) => {}
        other => panic!("{id}: want Invalid with {want:?}, got {other:?}"),
    };
    let linked = fx.write_policy("linked", "mode = enforce\n");
    assert!(matches!(lookup_quiet(&fx.env, "linked", &sw), Lookup::Found(_)));
    fs::hard_link(&linked, fx.root.join("other/linked.conf")).expect("hard link");
    invalid("linked", "has 2 hard links");
    // Opened without following a symlink.
    fs::write(fx.root.join("other/real.conf"), "mode = enforce\n").expect("write");
    symlink(fx.root.join("other/real.conf"), fx.env.sessions_dir().join("sym.conf")).expect("symlink");
    invalid("sym", "is a symlink");
    // A FIFO is refused at once, without waiting for a writer.
    mkfifo(&fx.env.sessions_dir().join("fifo.conf"));
    invalid("fifo", "not a regular file");
}

#[test]
fn policy_reads_and_parses_at_most_the_cap() {
    let cap = MAX_POLICY_BYTES;
    assert_eq!(read_capped(io::repeat(b'#').take(cap), cap).expect("read").map(|b| b.len()), Some(cap as usize));
    assert_eq!(read_capped(io::repeat(b'#').take(cap * 4), cap).expect("read"), None, "over the cap");
    let fx = fx();
    let big = format!("mode = enforce\n#{}\n", "x".repeat(cap as usize));
    let e = fx.parse(&big).expect_err("parse has its own cap");
    assert_eq!(e.line, 0);
    assert!(e.reason.contains(&format!("larger than {cap} bytes")), "{e}");
}

#[test]
fn policy_skips_missing_paths_and_keeps_the_rest() {
    let fx = fx();
    let text = format!("mode = enforce\nwrite = {}\nwrite = {}\n", fx.r("lane/missing"), fx.r("lane"));
    let p = fx.parse(&text).expect("a missing path is not an error");
    assert_eq!(p.skipped.iter().filter(|s| s.line != 0).cloned().collect::<Vec<_>>(), vec![Skipped {
        line: 2,
        key: "write",
        path: fx.root.join("lane/missing")
    }]);
    assert_eq!(file_rules(&p, &fx.root), vec![("write", "lane".to_string(), WRITE_RIGHTS, true, RuleClass::Work, 3)]);
}

#[test]
fn policy_refusal_text_names_the_location_and_the_contact() {
    let fx = fx();
    let e = fx
        .parse("mode = enforce\ncontact = Ask your foreman (R&R owns the policy files).\nwrite = relative\n")
        .expect_err("relative path");
    assert_eq!(
        e.refusal(),
        "sandbox policy invalid for this session (/test/s.conf:3: `relative` is not absolute (write the full path; `~` and variables are not expanded)); commands can't run until it's fixed. Ask your foreman (R&R owns the policy files)."
    );
    // The contact is found even when the error comes before it.
    let e = fx.parse("mode = sometimes\ncontact = Ask R&R.\n").expect_err("bad mode");
    assert!(e.refusal().ends_with(" Ask R&R."), "{}", e.refusal());
    let e = fx.parse("write = /var/tmp\n").expect_err("no mode");
    assert!(e.refusal().ends_with(DEFAULT_CONTACT), "{}", e.refusal());
    assert!(e.refusal().contains("(/test/s.conf: missing `mode"), "{}", e.refusal());
    // An invalid contact is refused, and its refusal falls back to the default.
    let e = fx.parse("mode = enforce\ncontact = Ask for the PASSWORD.\n").expect_err("prompt-word contact");
    assert!(e.refusal().ends_with(DEFAULT_CONTACT), "{}", e.refusal());
}

#[test]
fn policy_caps_the_rule_count() {
    let fx = fx();
    let baseline = fx.parse("mode = enforce\n").expect("valid").rules.len();
    assert!(baseline < MAX_RULES);
    let room = MAX_RULES - baseline;
    let lines = |k: usize| format!("mode = enforce\n{}", format!("write = {}\n", fx.r("lane")).repeat(k));
    let p = fx.parse(&lines(room)).expect("exactly MAX_RULES rules is fine");
    assert_eq!(p.rules.len(), MAX_RULES);
    let e = fx.parse(&lines(room + 1)).expect_err("one more is refused");
    assert_eq!(e.line, room + 2, "the line that goes over: {e}");
    assert!(e.reason.contains(&format!("more than {MAX_RULES} rules")), "{e}");
}

#[test]
fn policy_guard_refuses_the_identity_of_home_or_base_under_any_name() {
    let fx = fx();
    let g = Guard::new(&fx.env).expect("guard");
    // A path that passes every name check, opened as (say) a bind mount of a
    // protected directory: refused by identity.
    let alias = fx.root.join("lane/sub");
    assert_eq!(g.check_scope(&alias, fx.id("lane/sub")), Ok(()));
    for protected in ["home", "home/x", "home/x/base", "", "home/x/base/sandbox", "home/x/base/sandbox/sessions"] {
        let e = g.check_scope(&alias, fx.id(protected)).expect_err(protected);
        assert!(e.contains("is the same directory as $HOME, the Synaps base dir, one of their ancestors, or the base dir's sandbox/"), "{e}");
    }
    // Not protected by identity: a sibling, and other children of the base
    // dir (those only by name; deeper bind mounts are outside the trust
    // assumption, see the module doc).
    for fine in ["other", "home/x/base/logs"] {
        assert_eq!(g.check_scope(&alias, fx.id(fine)), Ok(()), "{fine}");
    }
}

#[test]
fn policy_refuses_when_home_or_the_base_dir_cannot_be_resolved() {
    let fx = fx();
    symlink(fx.root.join("lane/sub"), fx.root.join("other/alias")).expect("symlink");
    let home = fx.env.home.clone();
    let base = fx.env.base_dir.clone();
    let cases = [
        (PolicyEnv { home: home.clone(), base_dir: fx.root.join("home/x/not-yet") }, "cannot resolve the Synaps base dir"),
        // Through a symlink to a missing name: no lexical stand-in either.
        (PolicyEnv { home: home.clone(), base_dir: fx.root.join("other/alias/not-yet") }, "cannot resolve the Synaps base dir"),
        (PolicyEnv { home: fx.root.join("no-home"), base_dir: base.clone() }, "cannot resolve $HOME"),
        (PolicyEnv { home: PathBuf::from("/"), base_dir: base }, "$HOME is `/`"),
    ];
    let mut bad = Vec::new();
    for (env, want) in cases {
        match parse(&format!("mode = enforce\nwrite = {}\n", fx.r("lane")), Path::new("/test/s.conf"), &env) {
            Ok(_) => bad.push(format!("ACCEPTED with {env:?}")),
            Err(e) if e.line != 0 || !e.reason.contains(want) => bad.push(format!("{env:?}: {e}, want {want:?}")),
            Err(_) => {}
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn policy_an_opened_object_is_named_by_the_kernel_and_a_deleted_one_is_refused() {
    let fx = fx();
    // Through a symlink, the name is the target's.
    symlink(fx.root.join("lane/sub"), fx.root.join("other/link")).expect("symlink");
    let fd = open_path(&fx.root.join("other/link")).expect("open");
    assert_eq!(fd_path(&fd).expect("name"), fx.root.join("lane/sub"));
    // Removed after it was opened: no usable name.
    fs::create_dir(fx.root.join("lane/gone")).expect("mkdir");
    let fd = open_path(&fx.root.join("lane/gone")).expect("open");
    fs::remove_dir(fx.root.join("lane/gone")).expect("rmdir");
    let e = fd_path(&fd).expect_err("deleted");
    assert!(e.to_string().contains("(deleted)"), "{e}");
}

#[test]
fn policy_session_ids_that_can_name_a_file() {
    for ok in ["20261017-005143-4003", "a", "x_y.z", "550e8400-e29b-41d4-a716-446655440000"] {
        assert!(valid_session_id(ok), "{ok}");
    }
    let long = "x".repeat(129);
    for bad in ["", ".", "..", ".hidden", "../x", "a/b", "a b", "a\nb", long.as_str()] {
        assert!(!valid_session_id(bad), "{bad:?}");
    }
}

#[test]
fn policy_lookup_by_session_id() {
    let fx = fx();
    let sw = KillSwitch::new();
    let file = fx.write_policy("20261017-005143-4003", &format!("mode = enforce\nwrite = {}\n", fx.r("lane")));
    match lookup_quiet(&fx.env, "20261017-005143-4003", &sw) {
        Lookup::Found(p) => {
            assert_eq!(p.file(), file);
            assert_eq!(p.mode(), Mode::Enforce);
        }
        other => panic!("want Found, got {other:?}"),
    }
    for none in ["20261017-005143-4004", "../sessions/20261017-005143-4003", "", ".20261017-005143-4003"] {
        assert!(matches!(lookup_quiet(&fx.env, none, &sw), Lookup::None), "{none:?}");
    }
    fx.write_policy("bad", "mode = enforce\nwrite = relative\n");
    match lookup_quiet(&fx.env, "bad", &sw) {
        Lookup::Invalid(e) => assert_eq!((e.line, e.file), (2, fx.env.sessions_dir().join("bad.conf"))),
        other => panic!("want Invalid, got {other:?}"),
    }
    fx.write_policy("big", &format!("mode = enforce\n#{}\n", "x".repeat(MAX_POLICY_BYTES as usize)));
    match lookup_quiet(&fx.env, "big", &sw) {
        Lookup::Invalid(e) => assert!(e.reason.contains("larger than"), "{e}"),
        other => panic!("want Invalid, got {other:?}"),
    }
    fs::create_dir(fx.env.sessions_dir().join("dir.conf")).expect("mkdir");
    match lookup_quiet(&fx.env, "dir", &sw) {
        Lookup::Invalid(e) => assert!(e.reason.contains("not a regular file"), "{e}"),
        other => panic!("want Invalid, got {other:?}"),
    }
    fs::write(fx.env.sessions_dir().join("utf.conf"), b"mode = enforce\n\xff\n").expect("write");
    assert!(matches!(lookup_quiet(&fx.env, "utf", &sw), Lookup::Invalid(_)));
}

#[test]
fn policy_kill_switch_on_off_on_between_two_calls_of_one_session() {
    let fx = fx();
    let sw = KillSwitch::new();
    let id = "20261017-005143-4003";
    fx.write_policy(id, &format!("mode = enforce\nwrite = {}\n", fx.r("lane")));
    fx.write_policy("broken", "mode = nope\n");
    let state = |l: &Lookup| match l {
        Lookup::None => "none",
        Lookup::GlobalOff => "global-off",
        Lookup::Found(_) => "found",
        Lookup::Invalid(_) => "invalid",
    };
    let call = |id: &str| {
        let mut lines = Vec::new();
        let l = lookup_with(&fx.env, id, &sw, &mut |line| lines.push(line));
        (state(&l), lines)
    };
    assert_eq!(call(id), ("found", vec![]));
    fs::write(fx.env.kill_switch(), b"").expect("touch OFF");
    assert_eq!(call(id), ("global-off", vec![LINE_GLOBAL_OFF]));
    assert_eq!(call(id), ("global-off", vec![]), "logged once per transition, not per spawn");
    assert_eq!(call("broken"), ("global-off", vec![]), "the switch wins over a broken file");
    fs::remove_file(fx.env.kill_switch()).expect("rm OFF");
    assert_eq!(call(id), ("found", vec![LINE_GLOBAL_ON]));
    assert_eq!(call(id), ("found", vec![]));
    assert_eq!(call("broken"), ("invalid", vec![]));
    // Anything at the path counts, even a dangling symlink.
    symlink(fx.root.join("nowhere"), fx.env.kill_switch()).expect("symlink");
    assert_eq!(call(id), ("global-off", vec![LINE_GLOBAL_OFF]));
}

#[test]
fn policy_kill_switch_logs_each_transition_once() {
    let sw = KillSwitch::new();
    let seen: Vec<Option<&str>> = [false, false, true, true, false, false, true].iter().map(|p| sw.observe(*p)).collect();
    assert_eq!(seen, vec![None, None, Some(LINE_GLOBAL_OFF), None, Some(LINE_GLOBAL_ON), None, Some(LINE_GLOBAL_OFF)]);
    let sw = KillSwitch::new();
    assert_eq!(sw.observe(true), Some(LINE_GLOBAL_OFF), "present at the first spawn is logged");
    assert_eq!(LINE_GLOBAL_OFF, "sandbox.off reason=global");
    assert_eq!(LINE_GLOBAL_ON, "sandbox.on reason=global-cleared");
}

#[test]
fn policy_kill_switch_samples_and_logs_under_its_lock() {
    // Sampling, the state swap and the log line happen under one lock, so
    // concurrent spawns log each change once, in observation order. Asserts
    // come after the join, so a failure can't leave the observer blocked.
    let sw = KillSwitch::new();
    let sw = &sw;
    let (in_tx, in_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let (held_while_sampling, present, logged) = std::thread::scope(|s| {
        let h = s.spawn(move || {
            let mut logged = Vec::new();
            let present = sw.observe_with(
                || {
                    in_tx.send(()).expect("signal");
                    go_rx.recv().expect("go");
                    true
                },
                &mut |line| logged.push((line, sw.0.try_lock().is_err())),
            );
            (present, logged)
        });
        in_rx.recv().expect("sampling started");
        let held = sw.0.try_lock().is_err();
        go_tx.send(()).expect("go");
        let (present, logged) = h.join().expect("observer");
        (held, present, logged)
    });
    assert!(held_while_sampling, "the switch is sampled under its lock");
    assert!(present);
    assert_eq!(logged, vec![(LINE_GLOBAL_OFF, true)], "logged once, from under the lock");
}

#[test]
fn policy_mode_off_keeps_subagents_sandboxed_by_default() {
    let fx = fx();
    let p = fx.parse(&format!("mode = off\ninfra = {}\n", fx.r("other"))).expect("valid");
    assert_eq!((p.mode(), p.subagents()), (Mode::Off, Subagents::Enforce));
    assert_eq!(file_rules(&p, &fx.root).len(), 1, "a mode=off file's rules still feed its subagents");
    let p = fx.parse("mode = off\nsubagents = off\n").expect("valid");
    assert_eq!((p.mode(), p.subagents()), (Mode::Off, Subagents::Off));
    let p = fx.parse("mode = enforce\naudit_log = off\nrequired = true\n").expect("valid");
    assert_eq!((p.required(), p.audit_log()), (true, None));
    let p = fx.parse("mode = enforce\naudit_log = /var/log/audit/audit.log\n").expect("the default, spelled out");
    assert_eq!(p.audit_log(), Some(Path::new(DEFAULT_AUDIT_LOG)));
}

#[test]
fn policy_check_report_lists_rules_and_tells_valid_from_invalid() {
    let fx = fx();
    let good = fx.write_policy("good", &format!("mode = enforce\nrequired = true\nwrite = {}\ncargo_home = {}\n", fx.r("lane"), fx.r("cargo")));
    let (ok, text) = check_report(&good, &fx.env, Ok(8));
    assert!(ok, "{text}");
    assert!(text.starts_with(&format!("sandbox policy {}: OK (hash ", good.display())), "{text}");
    assert!(text.contains("landlock: abi 8"), "{text}");
    let row = |want: &[&str]| text.lines().any(|l| l.split_whitespace().collect::<Vec<_>>() == want);
    let lane = fx.r("lane");
    let gcache = fx.r("cargo/.global-cache");
    let mutate = fx.r("cargo/.package-cache-mutate");
    assert!(row(&["work", "dir", "write", "(line", "3)", lane.as_str(), "[all-write]"]), "{text}");
    assert!(row(&["infra", "file", "cargo_home", "(line", "4)", gcache.as_str(), "[write_file,truncate]"]), "{text}");
    assert!(row(&["cargo_home", "(line", "4)", mutate.as_str()]), "{text}");
    assert!(row(&["baseline", "dir", "baseline", "/tmp", "[all-write]"]), "{text}");
    assert!(text.contains("skipped, missing ("), "{text}");
    assert!(text.contains("kill switch") && text.contains(": absent"), "{text}");
    // The kernel's own answer on an old host is Ok(2): too old to use.
    let (ok, text) = check_report(&good, &fx.env, Ok(2));
    assert!(ok, "the file is still valid: {text}");
    assert!(!text.contains("landlock: abi 2"), "{text}");
    assert!(text.contains("landlock: unavailable (abi-too-old (abi 2, need 3))"), "{text}");
    assert!(text.contains("every spawn of this session is REFUSED (required = true)"), "{text}");
    // A Landlock error (not an unsupported kernel) refuses even without `required`.
    let plain = fx.write_policy("plain", &format!("mode = enforce\nwrite = {}\n", fx.r("lane")));
    let (ok, text) = check_report(&plain, &fx.env, Err(Unavailable::Error(libc::EMFILE)));
    assert!(ok, "{text}");
    assert!(
        text.contains(&format!("landlock: error (errno {}): every spawn of this session is REFUSED (an error", libc::EMFILE)),
        "{text}"
    );
    let (_, text) = check_report(&plain, &fx.env, Err(Unavailable::NoLandlock));
    assert!(text.contains("landlock: unavailable (no-landlock): on this host this session runs unsandboxed"), "{text}");

    let bad = fx.write_policy("bad2", "mode = enforce\nwrite = /var/tmp\n");
    let (ok, text) = check_report(&bad, &fx.env, Ok(8));
    assert!(!ok);
    assert!(text.contains(": INVALID\n"), "{text}");
    assert!(text.contains(&format!("{}:2: `/var/tmp` has fewer than 3 path components", bad.display())), "{text}");
    assert!(text.contains("agent sees: sandbox policy invalid for this session"), "{text}");
    let (ok, text) = check_report(&fx.root.join("nope.conf"), &fx.env, Ok(8));
    assert!(!ok && text.contains("no such file"), "{text}");
    assert!(text.contains("agent sees: nothing (a session without a policy file runs unsandboxed)"), "{text}");
}

#[test]
fn policy_check_report_puts_a_present_kill_switch_first() {
    let fx = fx();
    let good = fx.write_policy("good", &format!("mode = enforce\nwrite = {}\n", fx.r("lane")));
    let bad = fx.write_policy("bad", "mode = enforce\nbogus = 1\n");
    fs::write(fx.env.kill_switch(), b"").expect("touch OFF");
    let first = format!(
        "kill switch {}: PRESENT: every sandbox is off, so no policy file (this one included) is read or applied until it is removed",
        fx.env.kill_switch().display()
    );
    for (file, valid) in [(&good, true), (&bad, false)] {
        let (ok, text) = check_report(file, &fx.env, Ok(8));
        assert_eq!(ok, valid, "the file's own verdict is unchanged: {text}");
        assert_eq!(text.lines().next(), Some(first.as_str()), "{text}");
        assert_eq!(text.matches("kill switch").count(), 1, "said once: {text}");
    }
    fs::remove_file(fx.env.kill_switch()).expect("rm OFF");
    let (_, text) = check_report(&good, &fx.env, Ok(8));
    assert!(text.starts_with(&format!("sandbox policy {}: OK", good.display())), "{text}");
    assert!(text.contains(&format!("kill switch {}: absent", fx.env.kill_switch().display())), "{text}");
}

#[test]
fn policy_rights_names() {
    assert_eq!(rights_names(WRITE_RIGHTS), "all-write");
    assert_eq!(rights_names(FILE_WRITE_RIGHTS), "write_file,truncate");
    assert_eq!(rights_names(PROC_RIGHTS), "write_file,truncate,make_reg");
    assert_eq!(rights_names(WRITE_RIGHTS & !ACCESS_FS_REFER), "write_file,truncate,remove_file,remove_dir,make_reg,make_dir,make_sym,make_sock,make_fifo,make_char,make_block");
    assert_eq!(rights_names(0), "none");
}

#[test]
fn policy_log_lines_have_a_fixed_shape() {
    let fx = fx();
    let p = fx.parse(&format!("mode = enforce\nwrite = {}\n", fx.r("lane/missing"))).expect("valid");
    let n = p.rules().len();
    assert_eq!(
        policy_line("S1", &p, Ok(8)),
        format!("sandbox.policy session=S1 mode=enforce subagents=enforce abi=8 rules={n} skipped={} hash={}", p.skipped().len(), p.hash())
    );
    assert!(policy_line("S1", &p, Err(Unavailable::AbiTooOld(2))).contains(" abi=unavailable:abi-too-old "));
    assert!(policy_line("S1", &p, Ok(2)).contains(" abi=unavailable:abi-too-old "), "the kernel's Ok(2) is too old");
    assert!(skip_lines("S1", &p).contains(&format!("sandbox.skip session=S1 line=2 key=write path={} why=missing", fx.r("lane/missing"))));
    let e = fx.parse("mode = enforce\nwrite = /\n").expect_err("root");
    assert_eq!(invalid_line("S1", &e), "sandbox.invalid session=S1 file=/test/s.conf line=2 reason=refuses `/`");
    let off = fx.parse("mode = off\n").expect("valid");
    assert_eq!(explicit_off_line("S1", &off), "sandbox.off session=S1 reason=explicit subagents=enforce");
}

/// Opt-in gate for the real-kernel test, as in the landlock tests: unset =
/// skip; SYNAPS_TEST_LANDLOCK=1 on a host without usable Landlock = FAIL.
fn local_kernel() -> bool {
    if std::env::var("SYNAPS_TEST_LANDLOCK").as_deref() != Ok("1") {
        return false;
    }
    match abi_version() {
        Ok(abi) if abi >= MIN_ABI => true,
        other => panic!("SYNAPS_TEST_LANDLOCK=1 but Landlock is unusable here: {other:?}"),
    }
}

#[test]
fn policy_local_rules_bind_the_validated_object() {
    if !local_kernel() {
        return;
    }
    let fx = fx();
    fs::create_dir(fx.root.join("lane/work")).expect("mkdir");
    let p = fx.parse(&format!("mode = enforce\nwrite = {}\n", fx.r("lane/work"))).expect("valid");
    // After validation the checked dir moves away, and its old name becomes a
    // symlink to the base dir (which validation refuses).
    fs::rename(fx.root.join("lane/work"), fx.root.join("lane/work-old")).expect("rename");
    symlink(&fx.env.base_dir, fx.root.join("lane/work")).expect("symlink");
    let rs = p.to_ruleset().expect("ruleset");
    let errno = |r: io::Result<()>| r.err().map(|e| e.raw_os_error());
    let got = run_confined(&rs, &Tag::next(), || {
        (
            errno(fs::write(fx.root.join("lane/work-old/x"), "x")),
            errno(fs::write(fx.root.join("lane/work/x"), "x")),
        )
    })
    .expect("confined thread");
    assert_eq!(got, (None, Some(Some(libc::EACCES))), "the rule follows the checked object, not its old name");
    assert!(fx.root.join("lane/work-old/x").exists());
    assert!(!fx.env.base_dir.join("x").exists(), "the base dir stays unwritable");
}

/// `device` fixtures in /dev/shm (a baseline tmp dir under /dev), made the
/// dev root with [`DevRoot`] where a case says so: a regular file, a FIFO,
/// symlinks, and whiteouts: 0:0 char devices with no driver behind them,
/// which mknod makes without privilege (Linux 5.8+). `wh` is a direct child
/// of the root, `sub/wh` a nested one, `wh2` has a second name. No real
/// device; nothing is written.
struct DevFx {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    whiteouts: bool,
}

impl DevFx {
    fn p(&self, rel: &str) -> String {
        self.dir.join(rel).display().to_string()
    }
}

/// A whiteout at `p`; false if this kernel refuses one.
fn whiteout(p: &Path) -> bool {
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).expect("path");
    // SAFETY: a valid NUL-terminated path; mode and device are plain values.
    unsafe { libc::mknod(c.as_ptr(), libc::S_IFCHR | 0o600, 0) == 0 }
}

fn dev_fx(fx: &Fx) -> DevFx {
    let tmp = tempfile::Builder::new().prefix("sbx-dev-").tempdir_in("/dev/shm").expect("tempdir in /dev/shm");
    let dir = fs::canonicalize(tmp.path()).expect("canonical");
    fs::write(dir.join("file"), b"x").expect("write");
    mkfifo(&dir.join("fifo"));
    fs::create_dir(dir.join("sub")).expect("mkdir");
    // lane/wh: a char device outside the root, reached through a name in it.
    let whiteouts = ["wh", "wh2", "sub/wh"].iter().all(|n| whiteout(&dir.join(n))) && whiteout(&fx.root.join("lane/wh"));
    if whiteouts {
        fs::hard_link(dir.join("wh2"), dir.join("wh2-alias")).expect("hard link");
        symlink(dir.join("wh"), dir.join("to-wh")).expect("symlink");
        symlink(dir.join("sub/wh"), dir.join("to-nested")).expect("symlink");
        symlink(fx.root.join("lane/wh"), dir.join("to-scratch-wh")).expect("symlink");
    } else {
        eprintln!("this kernel refuses whiteouts: the whiteout cases are skipped");
    }
    DevFx { _tmp: tmp, dir, whiteouts }
}

/// Makes `root` the dev root on this thread until dropped.
struct DevRoot;

impl DevRoot {
    fn set(root: &Path) -> DevRoot {
        TEST_DEV_ROOT.with(|r| *r.borrow_mut() = Some(root.to_path_buf()));
        DevRoot
    }
}

impl Drop for DevRoot {
    fn drop(&mut self) {
        TEST_DEV_ROOT.with(|r| *r.borrow_mut() = None);
    }
}

/// A block device directly under /dev, if there is one.
fn a_block_device() -> Option<PathBuf> {
    fs::read_dir("/dev")
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_block_device()))
}

#[test]
fn policy_device_key_refuses_all_but_a_char_device_directly_under_dev() {
    let fx = fx();
    let d = dev_fx(&fx);
    let dev = |v: &str| format!("mode = enforce\ndevice = {v}\n");
    let text_err = "is not a device directly under /dev";
    let name_err = "is not a node directly under /dev";
    let mut cases: Vec<(String, &str)> = vec![
        // By its text, before anything is opened: one name directly under
        // /dev, split on `/` as written. Anything else is an error even for
        // a path that doesn't exist (not a skip).
        (dev("dev/null"), "is not absolute"),
        (dev("/dev"), text_err),
        (dev("/dev/"), text_err),
        (dev("/dev/."), text_err),
        (dev("/dev/.."), text_err),
        (dev("/dev/../tmp"), text_err),
        (dev("/dev/./null"), text_err),
        (dev("/dev//null"), text_err),
        (dev("/dev/null/"), text_err),
        (dev(&format!("/dev/synaps-test-no-such-dir-{}/node", std::process::id())), text_err),
        (dev(&fx.r("lane/file.txt")), text_err),
        (dev(&fx.r("lane/missing")), text_err),
        // By the kernel's name for the opened object, before its type.
        (dev("/dev/fd"), name_err),
        // By its type.
        (dev("/dev/shm"), "is not a character device"),
        // The 3-component exemption for /dev/<name> is this key's alone.
        ("mode = enforce\ninfra = /dev/pts\n".into(), "fewer than 3 path components"),
        ("mode = enforce\ninfra_file = /dev/null\n".into(), "fewer than 3 path components"),
        ("mode = enforce\nwrite_file = /dev/zero\n".into(), "fewer than 3 path components"),
    ];
    match a_block_device() {
        Some(b) => cases.push((dev(&b.display().to_string()), "is not a character device")),
        None => eprintln!("no block device under /dev: that case is skipped"),
    }
    // With the scratch dir as the dev root: nodes no real /dev has.
    let mut scratch: Vec<(String, &str)> = vec![
        (dev(&d.p("file")), "is not a character device"),
        (dev(&d.p("fifo")), "is not a character device"),
    ];
    if d.whiteouts {
        // A nested char device, by its own name and through a direct-child
        // symlink; one outside the root through a direct-child symlink.
        scratch.push((dev(&d.p("sub/wh")), text_err));
        scratch.push((dev(&d.p("to-nested")), name_err));
        scratch.push((dev(&d.p("to-scratch-wh")), name_err));
        scratch.push((dev(&d.p("wh2")), "has 2 hard links"));
    }
    let check = |cases: Vec<(String, &str)>| {
        let mut bad = Vec::new();
        for (text, reason) in cases {
            match fx.parse(&text) {
                Ok(p) => bad.push(format!(
                    "ACCEPTED {text:?}: {:?}",
                    p.rules.iter().filter(|r| r.class != RuleClass::Baseline).map(|r| r.path.display().to_string()).collect::<Vec<_>>()
                )),
                Err(e) if e.line != 2 || !e.reason.contains(reason) => {
                    bad.push(format!("{text:?}: got line {} {:?}, want {reason:?}", e.line, e.reason))
                }
                Err(_) => {}
            }
        }
        bad
    };
    let mut bad = check(cases);
    {
        let _root = DevRoot::set(&d.dir);
        bad.extend(check(scratch).into_iter().map(|b| format!("(scratch dev root) {b}")));
    }
    assert!(bad.is_empty(), "{} case(s) wrong:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn policy_device_node_must_be_a_char_device_with_exactly_one_name() {
    let c = Path::new("/dev/x");
    assert_eq!(device_node_ok(c, true, 1), Ok(()));
    // 0: unlinked between being named and being statted.
    for (char_device, nlink, want) in [(true, 0, "has 0 hard links"), (true, 2, "has 2 hard links"), (false, 1, "is not a character device")] {
        let e = device_node_ok(c, char_device, nlink).expect_err(want);
        assert!(e.contains(want), "{e}");
    }
}

#[test]
fn policy_device_key_grants_a_char_device_and_skips_a_missing_node() {
    let fx = fx();
    let d = dev_fx(&fx);
    let missing = format!("/dev/synaps-test-no-such-node-{}", std::process::id());
    let text = format!("mode = enforce\ndevice = /dev/null\ndevice = {missing}\ndevice = /dev/zero\n");
    let p = fx.parse(&text).expect("valid");
    let rules = |p: &Policy| -> Vec<_> {
        p.rules.iter().filter(|r| r.class != RuleClass::Baseline).map(|r| (r.key, r.path.clone(), r.access, r.dir, r.class, r.line)).collect()
    };
    let f = FILE_WRITE_RIGHTS;
    assert_eq!(
        rules(&p),
        vec![("device", PathBuf::from("/dev/null"), f, false, RuleClass::Infra, 2), ("device", PathBuf::from("/dev/zero"), f, false, RuleClass::Infra, 4)]
    );
    // With the scratch dir as the dev root: a direct-child char device, by
    // its own name and through a direct-child symlink (the rule is on the
    // kernel's name for the node).
    if d.whiteouts {
        let _root = DevRoot::set(&d.dir);
        let q = fx.parse(&format!("mode = enforce\ndevice = {}\ndevice = {}\n", d.p("wh"), d.p("to-wh"))).expect("valid");
        let wh = d.dir.join("wh");
        assert_eq!(rules(&q), vec![("device", wh.clone(), f, false, RuleClass::Infra, 2), ("device", wh, f, false, RuleClass::Infra, 3)]);
    }
    let skipped: Vec<_> = p.skipped.iter().filter(|s| s.line != 0).cloned().collect();
    assert_eq!(skipped, vec![Skipped { line: 3, key: "device", path: PathBuf::from(&missing) }]);
    assert!(skip_lines("S1", &p).contains(&format!("sandbox.skip session=S1 line=3 key=device path={missing} why=missing")));
    // The validator report's kind column says what the rule is on.
    let file = fx.write_policy("dev", &text);
    let (ok, report) = check_report(&file, &fx.env, Ok(8));
    assert!(ok, "{report}");
    let row = |want: &[&str]| report.lines().any(|l| l.split_whitespace().collect::<Vec<_>>() == want);
    assert!(row(&["infra", "dev", "device", "(line", "2)", "/dev/null", "[write_file,truncate]"]), "{report}");
    assert!(row(&["infra", "dev", "device", "(line", "4)", "/dev/zero", "[write_file,truncate]"]), "{report}");
}

#[test]
fn policy_local_device_rule_opens_its_node_only() {
    if !local_kernel() {
        return;
    }
    let fx = fx();
    // A rule on a devtmpfs node builds (validated and added, never opened).
    fx.parse("mode = enforce\ndevice = /dev/full\n").expect("valid").to_ruleset().expect("a rule on /dev/full");
    // Open-only, without a baseline, on two nodes every sandboxed command may
    // already write (so this part runs inside a sandboxed test run too):
    // /dev/null granted, /dev/tty not.
    // Landlock checks an open before the driver does, so the refused
    // /dev/tty is never opened; outside the domain it opens, or fails in its
    // driver (ENXIO without a controlling terminal), but is never refused.
    // Nothing is written. (Whiteouts in /dev/shm can't stand in: it is
    // mounted nodev, so opening any device node there is EACCES anyway.)
    let open_w = |p: &str| fs::OpenOptions::new().write(true).open(p).map(drop).err().and_then(|e| e.raw_os_error());
    let eacces = Some(libc::EACCES);
    assert_eq!(open_w("/dev/null"), None);
    assert_ne!(open_w("/dev/tty"), eacces, "outside the test's domain /dev/tty is not refused");
    let confined = |text: &str| {
        let p = parse_with(text, Path::new("/test/s.conf"), &fx.env, &[]).expect("valid");
        run_confined(&p.to_ruleset().expect("ruleset"), &Tag::next(), || (open_w("/dev/null"), open_w("/dev/tty")))
            .expect("confined thread")
    };
    assert_eq!(confined("mode = enforce\ndevice = /dev/null\n"), (None, eacces), "only the granted node opens");
    assert_eq!(confined("mode = enforce\n"), (eacces, eacces), "without the line neither does");

    // The baseline grants /dev/zero (a command may send output there). Only
    // checked where this process may write-open it itself: a sandboxed test
    // run's own domain refuses it until that domain's baseline has it.
    if open_w("/dev/zero").is_none() {
        let p = fx.parse("mode = enforce\n").expect("valid");
        let got = run_confined(&p.to_ruleset().expect("ruleset"), &Tag::next(), || open_w("/dev/zero")).expect("confined thread");
        assert_eq!(got, None, "the baseline lets /dev/zero open for writing");
    } else {
        eprintln!("skipped the /dev/zero open: this process may not write-open it (its own sandbox)");
    }

    // A `device` line on /dev/full, without a baseline: without the line a
    // write-open is refused, with it the node opens. Open-only (a write there
    // would only get ENOSPC). Only checked where this process may write-open
    // /dev/full itself: no sandboxed session's baseline has it.
    if open_w("/dev/full").is_none() {
        let full = |text: &str| {
            let p = parse_with(text, Path::new("/test/s.conf"), &fx.env, &[]).expect("valid");
            run_confined(&p.to_ruleset().expect("ruleset"), &Tag::next(), || open_w("/dev/full")).expect("confined thread")
        };
        assert_eq!(full("mode = enforce\ndevice = /dev/full\n"), None, "its device line lets /dev/full open");
        assert_eq!(full("mode = enforce\n"), eacces, "without the line /dev/full is refused");
    } else {
        eprintln!("skipped the /dev/full opens: this process may not write-open it (its own sandbox)");
    }
}

/// Owner rule 27c's `$HOME` roots in the fake home: the Jev state dir (its
/// access tree), the Jev plugin dir with its config, the Synaps config, and
/// siblings that stay grantable.
fn owner_tree(fx: &Fx) {
    for d in [
        "home/.synaps-cli/memory/jev/access/answers",
        "home/.synaps-cli/memory/other",
        "home/.synaps-cli/plugins/jev/lib",
        "home/.synaps-cli/plugins/other",
    ] {
        fs::create_dir_all(fx.root.join(d)).expect("mkdir");
    }
    for f in ["home/.synaps-cli/plugins/jev/config", "home/.synaps-cli/config", "home/.synaps-cli/memory/jev/sandbox-mode"] {
        fs::write(fx.root.join(f), b"x").expect("write");
    }
}

#[test]
fn policy_refuses_owner_rule_27c_roots_whatever_the_base_dir() {
    let fx = fx();
    owner_tree(&fx);
    symlink(fx.root.join("home/.synaps-cli/memory/jev"), fx.root.join("lane/to-jev")).expect("symlink");
    symlink(fx.root.join("home/.synaps-cli/config"), fx.root.join("lane/to-config")).expect("symlink");
    // The base dir inside $HOME but not at .synaps-cli (fx.env), and outside
    // $HOME (b2): either way only the 27c roots refuse these.
    let envs = [fx.env.clone(), PolicyEnv { home: fx.root.join("home"), base_dir: fx.root.join("b2") }];
    for env in envs {
        let cases: Vec<(String, usize, &str)> = [
            ("write", "home/.synaps-cli/memory/jev"),
            ("write", "home/.synaps-cli/memory/jev/access/answers"),
            ("write", "home/.synaps-cli/memory"),
            ("write", "home/.synaps-cli"),
            ("write", "home/.synaps-cli/plugins/jev"),
            ("infra", "home/.synaps-cli/plugins"),
            ("write_file", "home/.synaps-cli/plugins/jev/config"),
            ("infra_file", "home/.synaps-cli/config"),
            ("write_file", "home/.synaps-cli/memory/jev/sandbox-mode"),
            ("write", "lane/to-jev"),
            ("write_file", "lane/to-config"),
        ]
        .iter()
        .map(|(k, rel)| (format!("mode = enforce\n{k} = {}\n", fx.r(rel)), 2, "owner rule 27c"))
        .collect();
        expect_refusals(cases, |t| parse(t, Path::new("/test/s.conf"), &env), &fx.root);
        // Beside them: still grantable (the jev code dir's other files are
        // out of this rule's scope).
        for rel in ["home/.synaps-cli/memory/other", "home/.synaps-cli/plugins/jev/lib", "home/.synaps-cli/plugins/other"] {
            let text = format!("mode = enforce\nwrite = {}\n", fx.r(rel));
            assert!(parse(&text, Path::new("/test/s.conf"), &env).is_ok(), "{rel} with {env:?}");
        }
    }
}

#[test]
fn policy_refuses_the_ancestors_of_a_27c_root_that_does_not_exist_yet() {
    let fx = fx();
    // .synaps-cli with memory/ and plugins/, but no jev dir and no configs.
    for d in ["home/.synaps-cli/memory/other", "home/.synaps-cli/plugins/other"] {
        fs::create_dir_all(fx.root.join(d)).expect("mkdir");
    }
    let env = PolicyEnv { home: fx.root.join("home"), base_dir: fx.root.join("b2") };
    let cases: Vec<(String, usize, &str)> = ["home/.synaps-cli/memory", "home/.synaps-cli/plugins", "home/.synaps-cli"]
        .iter()
        .map(|rel| (format!("mode = enforce\nwrite = {}\n", fx.r(rel)), 2, "owner rule 27c"))
        .collect();
    expect_refusals(cases, |t| parse(t, Path::new("/test/s.conf"), &env), &fx.root);
    for rel in ["home/.synaps-cli/memory/other", "home/.synaps-cli/plugins/other", "lane"] {
        let text = format!("mode = enforce\nwrite = {}\n", fx.r(rel));
        assert!(parse(&text, Path::new("/test/s.conf"), &env).is_ok(), "{rel}");
    }
}

#[test]
fn policy_guard_refuses_the_identity_of_a_27c_root_under_any_name() {
    let fx = fx();
    owner_tree(&fx);
    let g = Guard::new(&fx.env).expect("guard");
    // A path that passes every name check, opened as (say) a bind mount of a
    // 27c root or of a directory above one: refused by identity.
    let alias = fx.root.join("lane/sub");
    assert_eq!(g.check_scope(&alias, fx.id("lane/sub")), Ok(()));
    for protected in [
        "home/.synaps-cli/memory/jev",
        "home/.synaps-cli/memory",
        "home/.synaps-cli",
        "home/.synaps-cli/plugins/jev",
        "home/.synaps-cli/plugins",
        "home/.synaps-cli/plugins/jev/config",
        "home/.synaps-cli/config",
    ] {
        let e = g.check_scope(&alias, fx.id(protected)).expect_err(protected);
        assert!(e.contains("owner rule 27c"), "{protected}: {e}");
    }
    for fine in ["home/.synaps-cli/memory/other", "home/.synaps-cli/plugins/jev/lib"] {
        assert_eq!(g.check_scope(&alias, fx.id(fine)), Ok(()), "{fine}");
    }
    // A baseline directory may not hold one either, by name or identity.
    assert!(g.check_baseline(&fx.root.join("other"), fx.id("home/.synaps-cli/memory/jev")).is_err(), "by identity");
}

#[test]
fn open_named_names_and_stats_the_object_without_opening_it() {
    // The off-Linux stand-in for an `O_PATH` handle: the canonical name
    // (symlinks followed) and that name's own metadata.
    let tmp = tempfile::Builder::new().prefix("sbx-named-").tempdir_in(test_scratch_parent()).expect("tempdir");
    let root = fs::canonicalize(tmp.path()).expect("canonical");
    fs::create_dir(root.join("d")).expect("mkdir");
    fs::write(root.join("d/f"), "x").expect("write");
    symlink(root.join("d"), root.join("l")).expect("symlink");
    let n = open_named(&root.join("l/f")).expect("named");
    assert_eq!(n.path, root.join("d/f"));
    let real = fs::metadata(root.join("d/f")).expect("stat");
    assert!(n.meta.is_file());
    assert_eq!((n.meta.dev(), n.meta.ino()), (real.dev(), real.ino()));
    // A device is statted by name: nothing is opened.
    let null = open_named(Path::new("/dev/null")).expect("named device");
    assert!(null.meta.file_type().is_char_device(), "{null:?}");
    assert_eq!(open_named(&root.join("missing")).expect_err("missing").kind(), io::ErrorKind::NotFound);
}
