//! Tests for one spawn's decision, hints and restriction. Every tree is a
//! scratch dir outside the baseline directories (`test_scratch_parent`:
//! fake home, base dir and work dirs). `spawn_local_*`
//! applies a Landlock domain, only to test children, and only with
//! SYNAPS_TEST_LANDLOCK=1 (LOCAL kernel, not g1pro/WSL); the rest needs no
//! kernel support.

use super::*;
use crate::sandbox::landlock::{abi_version, MIN_ABI};
use crate::sandbox::policy::{test_scratch_parent, KillSwitch, PolicyEnv, DEFAULT_CONTACT};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::Arc;

struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    env: PolicyEnv,
}

/// A fake tree: home/ (fake $HOME) holding the base dir one level down
/// (home/x/base), a work dir `lane` and a dir `other` outside every rule.
fn fx() -> Fx {
    let tmp = tempfile::Builder::new().prefix("sbx-spawn-").tempdir_in(test_scratch_parent()).expect("tempdir");
    let root = fs::canonicalize(tmp.path()).expect("canonical tempdir");
    for d in ["home/x/base/sandbox/sessions", "lane", "other"] {
        fs::create_dir_all(root.join(d)).expect("mkdir");
    }
    let env = PolicyEnv { home: root.join("home"), base_dir: root.join("home/x/base") };
    Fx { _tmp: tmp, root, env }
}

impl Fx {
    fn r(&self, rel: &str) -> String {
        self.root.join(rel).display().to_string()
    }
    fn write_policy(&self, id: &str, text: &str) -> PathBuf {
        let p = self.env.policy_path(id).expect("valid id");
        fs::write(&p, text).expect("write policy");
        p
    }
    fn set_kill_switch(&self, on: bool) {
        if on {
            fs::write(self.env.kill_switch(), "").expect("create OFF");
        } else {
            fs::remove_file(self.env.kill_switch()).expect("remove OFF");
        }
    }
    /// A hint context as `prepare` would build it for a policy with one
    /// work dir (`lane`) and one work file (`other/granted.txt`).
    fn ctx(&self, contact: &str) -> SpawnCtx {
        SpawnCtx {
            tag: Tag::new("sbx-t1").unwrap(),
            session: "s1".into(),
            tool: "bash",
            contact: contact.into(),
            rules: vec![
                (PathBuf::from("/tmp"), true),
                (self.root.join("lane"), true),
                (self.root.join("other/granted.txt"), false),
            ],
            allowed: vec![self.root.join("lane"), self.root.join("other/granted.txt")],
            work_dir: Some(self.root.join("lane")),
            git_common: vec![self.root.join("home/repo/.git")],
            home: self.root.join("home"),
            base: self.root.join("home/x/base"),
            audit: None,
        }
    }
}

/// A kill switch owned by one test (the daemon's is process-wide).
fn switch() -> &'static KillSwitch {
    Box::leak(Box::new(KillSwitch::new()))
}

fn binding(fx: &Fx, id: &str, abi: Option<Result<u32, Unavailable>>) -> Arc<SandboxBinding> {
    SandboxBinding::with_env(id, fx.env.clone(), switch(), abi)
}

/// `prepare` for bash, collecting its log lines.
fn prepare_into(b: &SandboxBinding, log: &mut Vec<String>) -> SpawnSandbox {
    prepare(b, "bash", &mut |line| log.push(line))
}

fn decide(b: &SandboxBinding) -> (String, Vec<String>) {
    let mut log = Vec::new();
    let kind = match prepare_into(b, &mut log) {
        SpawnSandbox::None => "none".to_string(),
        SpawnSandbox::Apply(_) => "apply".to_string(),
        SpawnSandbox::Refuse(msg) => format!("refuse: {msg}"),
    };
    (kind, log)
}

const TOO_OLD: Unavailable = Unavailable::AbiTooOld(2);

// ── Decisions (no kernel support needed: no domain is built) ─────────────

#[test]
fn spawn_no_policy_runs_unsandboxed_and_logs_nothing() {
    let fx = fx();
    let b = binding(&fx, "s1", Some(Ok(7)));
    assert_eq!(decide(&b), ("none".to_string(), vec![]));
    assert_eq!(decide(&b), ("none".to_string(), vec![]));
}

#[test]
fn spawn_mode_off_runs_unsandboxed_and_logs_once() {
    let fx = fx();
    fx.write_policy("s1", "mode = off\n");
    let b = binding(&fx, "s1", Some(Ok(7)));
    let (kind, log) = decide(&b);
    assert_eq!(kind, "none");
    assert_eq!(log.len(), 2, "{log:?}");
    assert!(log[0].starts_with("sandbox.policy session=s1 mode=off subagents=enforce abi=7 "), "{}", log[0]);
    assert_eq!(log[1], "sandbox.off session=s1 reason=explicit subagents=enforce");
    assert_eq!(decide(&b), ("none".to_string(), vec![]), "logged once, not per spawn");
}

#[test]
fn spawn_invalid_policy_refuses_with_its_location_and_contact_and_logs_once() {
    let fx = fx();
    let file = fx.write_policy("s1", "mode = enforce\ncontact = Ask your foreman.\nbogus = 1\n");
    let b = binding(&fx, "s1", Some(Ok(7)));
    let (kind, log) = decide(&b);
    let want = format!("refuse: sandbox policy invalid for this session ({}:3: ", file.display());
    assert!(kind.starts_with(&want), "{kind}");
    assert!(kind.ends_with("commands can't run until it's fixed. Ask your foreman."), "{kind}");
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].starts_with(&format!("sandbox.invalid session=s1 file={} line=3 reason=", file.display())), "{}", log[0]);
    let (again, log) = decide(&b);
    assert_eq!((again, log), (kind, vec![]), "still refused; logged once");
}

#[test]
fn spawn_required_but_unavailable_refuses_and_logs_once() {
    let fx = fx();
    fx.write_policy("s1", &format!("mode = enforce\nrequired = true\nwrite = {}\ncontact = Ask your foreman.\n", fx.r("lane")));
    let b = binding(&fx, "s1", Some(Err(TOO_OLD)));
    let (kind, log) = decide(&b);
    assert!(kind.starts_with("refuse: sandbox required for this session but unavailable on this host ("), "{kind}");
    assert!(kind.ends_with("); commands can't run. Ask your foreman."), "{kind}");
    assert_eq!(log.len(), 2, "{log:?}");
    assert!(log[0].starts_with("sandbox.policy session=s1 mode=enforce subagents=enforce abi=unavailable:abi-too-old "));
    assert_eq!(log[1], "sandbox.refused session=s1 reason=unavailable:abi-too-old");
    assert_eq!(decide(&b), (kind, vec![]));
}

#[test]
fn spawn_unavailable_and_not_required_runs_unsandboxed_and_logs_once() {
    let fx = fx();
    fx.write_policy("s1", &format!("mode = enforce\nwrite = {}\n", fx.r("lane")));
    let b = binding(&fx, "s1", Some(Err(TOO_OLD)));
    let (kind, log) = decide(&b);
    assert_eq!(kind, "none");
    // The kernel reports an old ABI as Ok(2): the same decision.
    let old = binding(&fx, "s1", Some(Ok(2)));
    let (old_kind, old_log) = decide(&old);
    assert_eq!((old_kind.as_str(), old_log.get(1).map(String::as_str)), ("none", Some("sandbox.unavailable session=s1 reason=abi-too-old")));
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[1], "sandbox.unavailable session=s1 reason=abi-too-old");
    assert_eq!(decide(&b), ("none".to_string(), vec![]));
}

#[test]
fn spawn_a_landlock_error_refuses_even_when_not_required() {
    // A resource or syscall failure (here EMFILE from the version query)
    // never falls back to an unsandboxed spawn; only an unsupported kernel
    // may, and only with required = false.
    let fx = fx();
    fx.write_policy("s1", &format!("mode = enforce\nwrite = {}\ncontact = Ask your foreman.\n", fx.r("lane")));
    let b = binding(&fx, "s1", Some(Err(Unavailable::Error(libc::EMFILE))));
    let (kind, log) = decide(&b);
    assert_eq!(
        kind,
        format!(
            "refuse: sandbox policy could not be applied for this session (landlock error (errno {})); commands can't run until it's fixed. Ask your foreman.",
            libc::EMFILE
        )
    );
    assert_eq!(log.len(), 2, "{log:?}");
    assert!(log[0].starts_with("sandbox.policy session=s1 mode=enforce subagents=enforce abi=unavailable:error "), "{}", log[0]);
    assert_eq!(log[1], format!("sandbox.refused session=s1 reason=landlock-error errno={}", libc::EMFILE));
    assert_eq!(decide(&b), (kind, vec![]), "still refused; logged once");
    // The same policy on a kernel without Landlock runs unsandboxed.
    let b = binding(&fx, "s1", Some(Err(Unavailable::NoLandlock)));
    assert_eq!(decide(&b).0, "none");
}

#[test]
fn spawn_a_refused_rule_refuses_and_logs_once() {
    let fx = fx();
    fx.write_policy("s1", &format!("mode = enforce\nwrite = {}\ncontact = Ask your foreman.\n", fx.r("lane")));
    let b = SandboxBinding::with_failing_build("s1", fx.env.clone(), switch(), Some(Ok(7)));
    let (kind, log) = decide(&b);
    let want = format!(
        "refuse: sandbox policy could not be applied for this session (rule for `{}` refused: {}); \
         commands can't run until it's fixed. Ask your foreman.",
        fx.r("lane"),
        io::Error::from_raw_os_error(libc::EINVAL)
    );
    assert_eq!(kind, want);
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].starts_with(&format!("sandbox.refused session=s1 reason=build rule for `{}` refused: ", fx.r("lane"))), "{}", log[0]);
    assert_eq!(decide(&b), (kind, vec![]), "still refused; logged once");
}

#[test]
fn spawn_kill_switch_on_off_on_between_calls_of_one_session() {
    let fx = fx();
    fx.write_policy("s1", &format!("mode = enforce\nrequired = true\nwrite = {}\ncontact = Ask your foreman.\n", fx.r("lane")));
    let b = binding(&fx, "s1", Some(Err(TOO_OLD)));
    let (first, log) = decide(&b);
    assert!(first.starts_with("refuse: sandbox required"), "{first}");
    assert_eq!(log.len(), 2);

    fx.set_kill_switch(true);
    assert_eq!(decide(&b), ("none".to_string(), vec!["sandbox.off reason=global".to_string()]), "the switch overrides required");
    assert_eq!(decide(&b), ("none".to_string(), vec![]), "the transition is logged once");

    fx.set_kill_switch(false);
    let (kind, log) = decide(&b);
    assert_eq!(kind, first, "the session's own policy applies again");
    assert_eq!(log.len(), 3, "{log:?}");
    assert_eq!(log[0], "sandbox.on reason=global-cleared");
    assert!(log[1].starts_with("sandbox.policy session=s1 "));
    assert_eq!(log[2], "sandbox.refused session=s1 reason=unavailable:abi-too-old");
    assert_eq!(decide(&b), (first, vec![]));
}

#[test]
fn spawn_a_removed_or_changed_policy_is_logged_at_the_next_spawn() {
    let fx = fx();
    let file = fx.write_policy("s1", "mode = off\n");
    let b = binding(&fx, "s1", Some(Ok(7)));
    assert_eq!(decide(&b).1.len(), 2);

    fx.write_policy("s1", "mode = off\nlane = demo\n");
    let (_, log) = decide(&b);
    assert_eq!(log.len(), 2, "a changed file is a new state: {log:?}");

    fs::remove_file(&file).unwrap();
    assert_eq!(decide(&b), ("none".to_string(), vec!["sandbox.off session=s1 reason=removed".to_string()]));
    assert_eq!(decide(&b), ("none".to_string(), vec![]));

    // No policy before or after the global switch: only the switch's lines.
    fx.set_kill_switch(true);
    assert_eq!(decide(&b).1, vec!["sandbox.off reason=global".to_string()]);
    fx.set_kill_switch(false);
    assert_eq!(decide(&b).1, vec!["sandbox.on reason=global-cleared".to_string()]);
}

/// Two spawns of one session, ordered by channels, with no sleeps. Each
/// lookup (kill switch, policy read and validation: `policy::lookup_with`)
/// reports through the `policy::ON_LOOKUP` test hook whether the binding's
/// lock is held at that moment; for every spawn it must be. (`try_lock` on
/// the thread that holds the lock reports WouldBlock; it never blocks.)
/// A looks up policy P and pauses before deciding; Q is installed; B
/// starts; A is released. B can look up only after A has decided and logged
/// P, so B reads Q and the lines say P then Q, each once. A spawn that looks
/// up before it takes the lock, or takes it only after the lookup, fails the
/// held check on every run, whatever the scheduling.
///
/// Inside the scope the facts are only recorded and A is always released;
/// the asserts come after the joins. An assert that failed while A was
/// paused would otherwise leave the scope waiting for A forever (the sender
/// that releases A outlives the scope), so a broken lock order would hang
/// the test run instead of failing it.
#[test]
fn spawn_one_sessions_decisions_and_lines_are_serialised() {
    use std::sync::mpsc;
    use std::sync::{Mutex, TryLockError};
    use std::time::Duration;
    type Events = Arc<Mutex<Vec<String>>>;
    /// Clears this thread's lookup hook when dropped.
    struct Unwatch;
    impl Drop for Unwatch {
        fn drop(&mut self) {
            let _ = policy::ON_LOOKUP.try_with(|hook| hook.borrow_mut().take());
        }
    }
    /// Until the guard drops, each lookup on this thread adds
    /// "`name` looks up, held=<the binding's lock is held>" to `events`.
    fn watch_lookups(b: &Arc<SandboxBinding>, events: &Events, name: &'static str) -> Unwatch {
        let (b, events) = (Arc::clone(b), Arc::clone(events));
        policy::ON_LOOKUP.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let held = matches!(b.logged.try_lock(), Err(TryLockError::WouldBlock));
                events.lock().unwrap_or_else(|p| p.into_inner()).push(format!("{name} looks up, held={held}"));
            }));
        });
        Unwatch
    }
    let fx = fx();
    let (p_text, q_text) = ("mode = off\ncontact = P.\n", "mode = off\ncontact = Q.\n");
    let file = fx.write_policy("s1", p_text);
    let hash = |t: &str| policy::parse(t, &file, &fx.env).expect("valid").hash().to_string();
    let (hp, hq) = (hash(p_text), hash(q_text));
    assert_ne!(hp, hq);
    let b = binding(&fx, "s1", Some(Ok(7)));
    let events: Events = Arc::default();
    let (bref, evref) = (&b, &events);
    let push = move |l: String| evref.lock().unwrap_or_else(|p| p.into_inner()).push(l);
    let (a_read_tx, a_read_rx) = mpsc::channel::<()>();
    let (a_go_tx, a_go_rx) = mpsc::channel::<()>();
    let (b_started_tx, b_started_rx) = mpsc::channel::<()>();
    let (a_read, b_started, a_none, b_none) = std::thread::scope(|s| {
        // Owned here, so an unexpected panic in this closure drops it too.
        let a_go_tx = a_go_tx;
        let a = s.spawn(move || {
            let _watch = watch_lookups(bref, evref, "A");
            let mut push = push;
            let r = prepare_hooked(bref, "bash", &mut push, &mut || {
                let _ = a_read_tx.send(());
                let _ = a_go_rx.recv();
            });
            matches!(r, SpawnSandbox::None)
        });
        // A has looked up P and is paused before its decision.
        let a_read = a_read_rx.recv_timeout(Duration::from_secs(30)).is_ok();
        fx.write_policy("s1", q_text);
        let second = s.spawn(move || {
            let _watch = watch_lookups(bref, evref, "B");
            let _ = b_started_tx.send(());
            let mut push = push;
            matches!(prepare(bref, "bash", &mut push), SpawnSandbox::None)
        });
        let b_started = b_started_rx.recv_timeout(Duration::from_secs(30)).is_ok();
        let _ = a_go_tx.send(());
        (a_read, b_started, a.join(), second.join())
    });
    assert!(a_read, "A looked up the policy and paused");
    assert!(b_started, "B started");
    assert!(a_none.expect("A"), "mode = off: unsandboxed");
    assert!(b_none.expect("B"), "mode = off: unsandboxed");
    let third = {
        let _watch = watch_lookups(&b, &events, "C");
        let mut third = Vec::new();
        assert!(matches!(prepare_into(&b, &mut third), SpawnSandbox::None));
        third
    };
    assert!(third.is_empty(), "Q was logged once, by B: {third:?}");
    let events = events.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let seen: Vec<String> = events
        .iter()
        .map(|e| match e.rsplit_once(" hash=") {
            Some((_, h)) if e.starts_with("sandbox.policy ") && h == hp => "policy P".to_string(),
            Some((_, h)) if e.starts_with("sandbox.policy ") && h == hq => "policy Q".to_string(),
            _ if e.starts_with("sandbox.off session=s1 reason=explicit ") => "off".to_string(),
            _ => e.clone(),
        })
        .collect();
    assert_eq!(
        seen,
        ["A looks up, held=true", "policy P", "off", "B looks up, held=true", "policy Q", "off", "C looks up, held=true"],
        "every lookup under the binding's lock; P then Q, each once: {events:?}"
    );
}

// ── Hints (no kernel support needed) ─────────────────────────────────────

#[test]
fn spawn_abs_paths_finds_quoted_and_bare_paths() {
    let cases: &[(&str, &[&str])] = &[
        ("bash: line 1: /x/y: Permission denied", &["/x/y"]),
        ("mkdir: cannot create directory ‘/q/r’: Permission denied", &["/q/r"]),
        ("touch: cannot touch \"/z\": Permission denied", &["/z"]),
        ("mv: cannot move `/m' to `/n': Invalid cross-device link", &["/m", "/n"]),
        ("error: failed to write /t/u.lock: Permission denied (os error 13)", &["/t/u.lock"]),
        ("mkdir: Permission denied", &[]),
        ("cd / && ls", &[]),
    ];
    for (line, want) in cases {
        let got: Vec<PathBuf> = abs_paths(line);
        let want: Vec<PathBuf> = want.iter().map(PathBuf::from).collect();
        assert_eq!(got, want, "{line:?}");
    }
}

#[test]
fn spawn_classify_puts_each_path_in_its_class() {
    let (home, base) = (Path::new("/h"), Path::new("/h/.synaps-cli"));
    let cases = [
        ("/h/Projects/.worktrees/x", "", "worktree_new"),
        ("/v/x/.git", "fatal: could not create leading directories of '/v/x/.git': Permission denied", "worktree_new"),
        ("/h/.synaps-cli/logs/a", "", "synaps_state"),
        ("/h/.ssh/id", "", "home_dot:.ssh"),
        ("/h/notes", "", "home_other"),
        ("/dev/sda", "", "dev_proc"),
        ("/proc/1/x", "", "dev_proc"),
        ("/etc/x", "", "system"),
        ("/usr/lib/x", "", "system"),
        ("/srvx/a", "", "other"),
        ("/mnt/x", "", "other"),
    ];
    for (path, line, want) in cases {
        assert_eq!(classify(Path::new(path), line, home, base), want, "{path}");
    }
}

// Finding 12: a captured path ends at whitespace, a backtick or a quote, so
// a quoted span of prose that starts with `/` (a printed brief) gives its
// first word, never the whole span.
#[test]
fn spawn_abs_paths_ends_a_path_at_whitespace_a_backtick_or_a_quote() {
    let brief = format!("cat: '/v/brief.md {}': Permission denied", "and then the whole printed brief, many words long. ".repeat(20));
    let cases: Vec<(String, Vec<&str>)> = vec![
        ("rm: cannot remove '/a/b c': Permission denied".into(), vec!["/a/b"]),
        (brief, vec!["/v/brief.md"]),
        ("echo '/n/o\nnext line words': Permission denied".into(), vec!["/n/o"]),
        ("open /p/q`id`: permission denied".into(), vec!["/p/q"]),
        ("open /r/s\"t: permission denied".into(), vec!["/r/s"]),
        ("touch '/u/v\"w': Permission denied".into(), vec!["/u/v"]),
        ("mkdir: cannot create directory ‘/q/r’s x’: Permission denied".into(), vec!["/q/r"]),
        ("mv: cannot move `/m x' to `/n': Invalid cross-device link".into(), vec!["/m", "/n"]),
        ("cp: cannot create \"/e/f\tg\": Permission denied".into(), vec!["/e/f"]),
    ];
    for (line, want) in &cases {
        let got: Vec<PathBuf> = abs_paths(line);
        let want: Vec<PathBuf> = want.iter().map(PathBuf::from).collect();
        assert_eq!(got, want, "{line:?}");
    }
}

// Finding 12: a path as a log field: render_path's escaping, at most
// PATH_FIELD_MAX bytes; a longer one is cut at a whole escape, quoted, and
// marked by `...` after its closing quote (an uncut field never ends so).
#[test]
fn spawn_log_path_escapes_and_caps_a_path() {
    use std::os::unix::ffi::OsStrExt as _;
    let fits = format!("/{}", "a".repeat(PATH_FIELD_MAX - 1));
    assert_eq!(log_path(Path::new(&fits)), fits, "a plain path that fits is as is");
    let over = format!("/{}", "a".repeat(PATH_FIELD_MAX));
    let cut = log_path(Path::new(&over));
    assert_eq!(cut, format!("\"{}\"...", &over[..PATH_FIELD_MAX - 5]), "cut, quoted and marked");
    assert_eq!(cut.len(), PATH_FIELD_MAX);
    assert_eq!(log_path(Path::new("/a b\x1b\"\\")), "\"/a\\x20b\\x1B\\\"\\\\\"", "escaped as in the audit lines");
    let odd = std::ffi::OsStr::from_bytes(b"/x\xff");
    assert_eq!(log_path(Path::new(odd)), "\"/x\\xFF\"", "lossless");
    let escapes = format!("/{}", "\x1b".repeat(PATH_FIELD_MAX));
    let cut = log_path(Path::new(&escapes));
    assert!(cut.len() <= PATH_FIELD_MAX, "{} bytes", cut.len());
    let inner = cut.strip_prefix("\"/").and_then(|c| c.strip_suffix("\"...")).expect("quoted and marked");
    assert_eq!(inner, "\\x1B".repeat(inner.len() / 4), "cut at a whole escape");
    assert_eq!(inner.len(), (PATH_FIELD_MAX - 6) / 4 * 4, "as many escapes as fit");
}

// Finding 12: the log line and the hint for a printed brief, a long path
// and odd bytes: one bounded field, controls escaped, a fixed-alphabet class.
#[test]
fn spawn_annotate_logs_a_long_or_odd_path_bounded_and_escaped() {
    let fx = fx();
    let ctx = fx.ctx("Ask your foreman.");
    let who = "sandbox.denial_suspected session=s1 tool=bash tag=sbx-t1";
    let home = fx.r("home");
    let mut all = Vec::new();
    let brief = format!("cat: '{home}/brief.md {}': Permission denied\n", "and then the whole printed brief, many words long. ".repeat(20));
    let a = ctx.annotate("", &brief).expect("a hint");
    assert_eq!(a.lines, vec![format!("{who} class=home_other path={home}/brief.md src=output")], "the brief's first word only");
    all.push(a);
    let long = format!("{home}/{}", vec!["a".repeat(200); 15].join("/"));
    let a = ctx.annotate("", &format!("open {long}: permission denied\n")).expect("a hint");
    let field = format!("\"{}\"...", &long[..PATH_FIELD_MAX - 5]);
    assert_eq!(a.lines, vec![format!("{who} class=home_other path={field} src=output")], "cut");
    assert!(a.hint.contains(&format!("refused: {field}. Allowed: ")), "{}", a.hint);
    all.push(a);
    let odd = format!("{home}/a\x1b[31mb\x7fc");
    let a = ctx.annotate("", &format!("open {odd}: permission denied\n")).expect("a hint");
    let field = format!("\"{home}/a\\x1B[31mb\\x7Fc\"");
    assert_eq!(a.lines, vec![format!("{who} class=home_other path={field} src=output")], "escaped");
    assert!(a.hint.contains(&format!("refused: {field}. Allowed: ")), "{}", a.hint);
    all.push(a);
    let dot = format!("{home}/.Bad\x1bX/f");
    let a = ctx.annotate("", &format!("open {dot}: permission denied\n")).expect("a hint");
    let field = format!("\"{home}/.Bad\\x1BX/f\"");
    assert_eq!(a.lines, vec![format!("{who} class=home_dot:unprintable path={field} src=output")], "a fixed-alphabet class");
    all.push(a);
    for a in &all {
        assert!(a.lines.iter().all(|l| !l.chars().any(char::is_control)), "no control in a line: {:?}", a.lines);
        assert!(a.hint.chars().all(|c| c == '\n' || !c.is_control()), "no control in the hint: {:?}", a.hint);
    }
}

#[test]
fn spawn_annotate_names_a_refused_write_outside_the_rules() {
    let fx = fx();
    let ctx = fx.ctx("Ask your foreman.");
    let a = ctx.annotate("", &format!("rm: cannot remove '{}': Permission denied\n", fx.r("home/notes"))).expect("a hint");
    assert_eq!(
        a.lines,
        vec![format!("sandbox.denial_suspected session=s1 tool=bash tag=sbx-t1 class=home_other path={} src=output", fx.r("home/notes"))]
    );
    assert_eq!(
        a.hint,
        format!(
            "[sandbox] writes outside this session's write set are refused: {}. Allowed: {}, {}, /tmp. Ask your foreman. Do not work around it.",
            fx.r("home/notes"),
            fx.r("lane"),
            fx.r("other/granted.txt")
        )
    );
}

#[test]
fn spawn_annotate_skips_covered_paths_and_ones_the_daemon_could_not_write_either() {
    let fx = fx();
    let ctx = fx.ctx("Ask your foreman.");
    assert_eq!(ctx.annotate("", &format!("{}: Permission denied", fx.r("lane/sub/x"))), None, "inside a dir rule");
    assert_eq!(ctx.annotate("", &format!("{}: Permission denied", fx.r("other/granted.txt"))), None, "the file rule");
    assert_eq!(ctx.annotate("", "all good\n"), None, "no denial");
    assert_eq!(ctx.annotate("", "cat: notes: Permission denied"), None, "a relative path from a reader");
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        fs::create_dir(fx.root.join("ro")).unwrap();
        fs::set_permissions(fx.root.join("ro"), fs::Permissions::from_mode(0o555)).unwrap();
        assert_eq!(ctx.annotate("", &format!("touch: cannot touch '{}': Permission denied", fx.r("ro/x"))), None, "DAC, not the sandbox");
    }
}

#[test]
fn spawn_annotate_gives_new_worktrees_their_class_and_hint() {
    let fx = fx();
    let ctx = fx.ctx("Ask your foreman.");
    let target = fx.r("home/Projects/.worktrees/wt1/.git");
    let a = ctx.annotate("", &format!("fatal: could not create leading directories of '{target}': Permission denied")).expect("a hint");
    assert_eq!(a.lines, vec![format!("sandbox.denial_suspected session=s1 tool=bash tag=sbx-t1 class=worktree_new path={target} src=output")]);
    assert_eq!(
        a.hint,
        format!(
            "[sandbox] new worktrees go under a directory this session may write, e.g. `git worktree add {}/wt-<name> <base>`; \
             another path must first be added to this session's policy. Ask your foreman.",
            fx.r("lane")
        )
    );
}

#[test]
fn spawn_annotate_gives_git_packed_refs_lock_its_class_and_hint() {
    let fx = fx();
    fs::create_dir_all(fx.root.join("home/repo/.git")).unwrap();
    let ctx = fx.ctx("Ask your foreman.");
    let lock = fx.r("home/repo/.git/packed-refs.lock");
    // What every commit in a linked worktree prints (it still exits 0).
    let out = format!("error: Unable to create '{lock}': Permission denied\n[w 1a2b3c4] trial commit\n");
    let a = ctx.annotate("", &out).expect("a hint");
    assert_eq!(
        a.lines,
        vec![format!("sandbox.denial_suspected session=s1 tool=bash tag=sbx-t1 class=git_packed_refs path={lock} src=output")]
    );
    assert_eq!(
        a.hint,
        format!(
            "[sandbox] git may not create {lock} (by design: the repository's common dir stays read-only). \
             The exit code tells: a commit or `git worktree add` that prints this still succeeds; \
             deleting a ref (`branch -D`, `tag -d`, `update-ref -d`, `fetch --prune`) fails. Ask your foreman. Do not work around it."
        )
    );
    // With another refused write: each path gets its own hint.
    let notes = fx.r("home/notes");
    let a = ctx.annotate("", &format!("{out}rm: cannot remove '{notes}': Permission denied\n")).expect("hints");
    assert_eq!(a.lines.len(), 2, "{:?}", a.lines);
    assert!(a.lines[1].contains(&format!("class=home_other path={notes} ")), "{:?}", a.lines);
    assert!(a.hint.contains(&format!("refused: {notes}. Allowed: ")), "{}", a.hint);
    assert!(!a.hint.contains(&format!("refused: {lock}")), "{}", a.hint);
    // Another repository's packed-refs.lock is an ordinary refused write.
    let foreign = fx.r("home/elsewhere/.git/packed-refs.lock");
    let a = ctx.annotate("", &format!("error: Unable to create '{foreign}': Permission denied")).expect("a hint");
    assert_eq!(
        a.lines,
        vec![format!("sandbox.denial_suspected session=s1 tool=bash tag=sbx-t1 class=home_other path={foreign} src=output")]
    );
    assert!(a.hint.starts_with(&format!("[sandbox] writes outside this session's write set are refused: {foreign}. ")), "{}", a.hint);
}

#[test]
fn spawn_annotate_a_denial_without_a_path() {
    let fx = fx();
    let ctx = fx.ctx("Ask your foreman.");
    let a = ctx.annotate("", "mkdir: Permission denied\nln: Permission denied\n").expect("a hint");
    assert_eq!(a.lines, vec!["sandbox.denial_suspected session=s1 tool=bash tag=sbx-t1 class=unknown path=- src=output".to_string()]);
    assert!(a.hint.starts_with(&format!("[sandbox] this session may write only: {}, {}, /tmp. ", fx.r("lane"), fx.r("other/granted.txt"))), "{}", a.hint);
}

#[test]
fn spawn_is_denial_line_folds_the_case_of_the_words() {
    for line in [
        "rm: cannot remove '/a': Permission denied",
        "open /a/b.yml123: permission denied",
        "PERMISSION DENIED",
        "rename /a /b: invalid cross-device link",
        "mv: cannot move '/m' to '/n': Invalid cross-device link",
        "write failed: EACCES",
        "link: EXDEV",
    ] {
        assert!(is_denial_line(line), "{line:?}");
    }
    for line in ["permission granted", "access denied", "permission", "denied", "cross-device", "eacces", "exdev"] {
        assert!(!is_denial_line(line), "{line:?}");
    }
}

#[test]
fn spawn_annotate_names_a_lowercase_denial_from_a_go_program() {
    let fx = fx();
    let ctx = fx.ctx("Ask your foreman.");
    // Go's os errors are lowercase: `open <path>: permission denied`.
    let p = fx.r("home/.config/tool-x/.config.yml6370644719247780078");
    let a = ctx.annotate("", &format!("open {p}: permission denied\n")).expect("a hint");
    assert_eq!(
        a.lines,
        vec![format!("sandbox.denial_suspected session=s1 tool=bash tag=sbx-t1 class=home_dot:.config path={p} src=output")]
    );
    assert!(a.hint.starts_with(&format!("[sandbox] writes outside this session's write set are refused: {p}. Allowed: ")), "{}", a.hint);
}

#[test]
fn spawn_annotate_joins_a_denial_wrapped_over_two_lines() {
    let fx = fx();
    let ctx = fx.ctx("Ask your foreman.");
    let p = fx.r("home/.config/tool-x/.config.yml6370644719247780078");
    let want = vec![format!("sandbox.denial_suspected session=s1 tool=bash tag=sbx-t1 class=home_dot:.config path={p} src=output")];
    // A Go program's error soft-wrapped with a hanging indent: the words
    // straddle the break.
    let out = format!("  Failed to refresh credentials for \"example.test\": open {p}: permission\n  denied.\n");
    let a = ctx.annotate("", &out).expect("a hint");
    assert_eq!(a.lines, want);
    assert!(a.hint.starts_with(&format!("[sandbox] writes outside this session's write set are refused: {p}. Allowed: ")), "{}", a.hint);
    // Capitalised and wrapped with no indent, after an unrelated line.
    let out = format!("Compiling x\nerror: failed to write {p}: Permission\ndenied (os error 13)\n");
    assert_eq!(ctx.annotate("", &out).map(|a| a.lines), Some(want.clone()));
    // A PTY's CRLF lines.
    let out = format!("open {p}: permission\r\n  denied.\r\n");
    assert_eq!(ctx.annotate("", &out).map(|a| a.lines), Some(want));
}

#[test]
fn spawn_annotate_a_wrapped_denial_counts_only_when_its_words_straddle_the_break() {
    let fx = fx();
    let ctx = fx.ctx("Ask your foreman.");
    let notes = fx.r("home/notes");
    assert_eq!(ctx.annotate("", &format!("open {}: permission\n  denied.\n", fx.r("lane/x.yml1"))), None, "inside a dir rule");
    assert_eq!(ctx.annotate("", &format!("open {notes}: permission\n  granted\n")), None, "not a denial");
    assert_eq!(ctx.annotate("", &format!("open {notes}: permission\n\n  denied.\n")), None, "a blank line between");
    // A path on one line, a whole denial (naming no path) on the next: the
    // path isn't the denial's.
    assert_eq!(ctx.annotate("", &format!("wrote {notes}\ncat: notes: Permission denied\n")), None, "no straddle");
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        fs::create_dir(fx.root.join("ro")).unwrap();
        fs::set_permissions(fx.root.join("ro"), fs::Permissions::from_mode(0o555)).unwrap();
        assert_eq!(ctx.annotate("", &format!("open {}: permission\n  denied.\n", fx.r("ro/x"))), None, "DAC, not the sandbox");
    }
}

#[test]
fn spawn_annotate_joins_a_path_line_to_the_denial_that_starts_the_next() {
    let fx = fx();
    let ctx = fx.ctx("Ask your foreman.");
    let p = fx.r("home/.config/tool-x/.config.yml6370644719247780078");
    let want = vec![format!("sandbox.denial_suspected session=s1 tool=bash tag=sbx-t1 class=home_dot:.config path={p} src=output")];
    // The same hanging-indent wrapper at another path length: the line
    // breaks after the path's colon, the whole denial starts the next line.
    let out = format!("  Failed to refresh credentials for \"example.test\": open {p}:\n  permission denied.\n");
    let a = ctx.annotate("", &out).expect("a hint");
    assert_eq!(a.lines, want);
    assert!(a.hint.starts_with(&format!("[sandbox] writes outside this session's write set are refused: {p}. Allowed: ")), "{}", a.hint);
    // Capitalised, no indent, CRLF.
    let out = format!("error: failed to write {p}:\r\nPermission denied (os error 13)\r\n");
    assert_eq!(ctx.annotate("", &out).map(|a| a.lines), Some(want));
}

#[test]
fn spawn_annotate_a_path_line_joins_only_a_denial_that_starts_the_next_line() {
    let fx = fx();
    let ctx = fx.ctx("Ask your foreman.");
    let notes = fx.r("home/notes");
    assert_eq!(ctx.annotate("", "error reading config:\n  permission denied.\n"), None, "a word, not a path, before the colon");
    assert_eq!(ctx.annotate("", "open notes.yml:\n  permission denied.\n"), None, "a relative path");
    assert_eq!(ctx.annotate("", &format!("open {notes}\n  permission denied.\n")), None, "no colon after the path");
    assert_eq!(ctx.annotate("", &format!("open {notes}:\n\n  permission denied.\n")), None, "a blank line between");
    assert_eq!(ctx.annotate("", &format!("open {}:\n  permission denied.\n", fx.r("lane/x.yml1"))), None, "inside a dir rule");
    // The denial must start the next line: one that only contains it is
    // another message.
    assert_eq!(ctx.annotate("", &format!("listing {notes}:\ncat: notes: Permission denied\n")), None, "not at the start");
}

#[test]
fn spawn_annotate_a_join_needs_the_words_to_end_at_a_boundary() {
    let fx = fx();
    let ctx = fx.ctx("Ask your foreman.");
    let notes = fx.r("home/notes");
    let lane_a = fx.r("lane/a");
    let want = Some(vec![format!("sandbox.denial_suspected session=s1 tool=bash tag=sbx-t1 class=home_other path={notes} src=output")]);
    // The whole words, then `.`, ` (`, the end of the line or of the text:
    // both joins.
    for out in [
        format!("open {notes}: permission\n  denied.\n"),
        format!("open {notes}: permission\n  denied (os error 13)\n"),
        format!("open {notes}: permission\n  denied\n"),
        format!("open {notes}: permission\n  denied"),
        format!("rename {lane_a} {notes}: invalid\n  cross-device link\n"),
        format!("open {notes}:\n  permission denied.\n"),
        format!("open {notes}:\n  permission denied (os error 13)\n"),
        format!("open {notes}:\n  permission denied\n"),
        format!("open {notes}:\n  permission denied"),
        format!("rename {lane_a} {notes}:\n  invalid cross-device link\n"),
    ] {
        assert_eq!(ctx.annotate("", &out).map(|a| a.lines), want, "{out:?}");
    }
    // A longer word after them is another word: no join, for either rule.
    for out in [
        format!("open {notes}: permission\n  deniedness is a fixture label\n"),
        format!("open {notes}: permission\n  denied_x\n"),
        format!("rename {lane_a} {notes}: invalid cross-device\n  linkage\n"),
        format!("listing {notes}:\npermission deniedness is a fixture label\n"),
        format!("listing {notes}:\n  permission denied2\n"),
        format!("listing {notes}:\ninvalid cross-device linkage\n"),
    ] {
        assert_eq!(ctx.annotate("", &out), None, "{out:?}");
    }
}

#[test]
fn spawn_annotate_never_blames_a_proc_magic_link() {
    let fx = fx();
    let ctx = fx.ctx("Ask your foreman.");
    // A child whose cwd and stdout are writable scratch paths outside the
    // rules: following its /proc links would reach something the daemon
    // could write, though nothing tried to write there.
    let work = fx.root.join("home/work");
    fs::create_dir_all(&work).unwrap();
    let out = fs::File::create(work.join("out.txt")).unwrap();
    let mut child = Command::new("sleep")
        .arg("30")
        .current_dir(&work)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .spawn()
        .expect("spawn sleep");
    let pid = child.id();
    let links = [
        format!("readlink /proc/{pid}/cwd: permission denied"),
        format!("ls: cannot read symbolic link '/proc/{pid}/cwd': Permission denied"),
        format!("open /proc/{pid}/fd/1: permission denied"),
        format!("readlink /proc/{pid}/task/{pid}/cwd: permission denied"),
    ];
    let got: Vec<Option<Annotation>> = links.iter().map(|l| ctx.annotate("", l)).collect();
    // Another /proc file still counts: the child's comm, which the daemon
    // (same user) could write.
    let comm = ctx.annotate("", &format!("bash: /proc/{pid}/comm: Permission denied")).map(|a| a.lines);
    let _ = child.kill();
    let _ = child.wait();
    for (line, a) in links.iter().zip(&got) {
        assert_eq!(a, &None, "{line}");
    }
    assert_eq!(
        comm,
        Some(vec![format!("sandbox.denial_suspected session=s1 tool=bash tag=sbx-t1 class=dev_proc path=/proc/{pid}/comm src=output")])
    );
}

#[test]
fn spawn_annotate_never_follows_a_proc_path_with_dotdot() {
    let fx = fx();
    let ctx = fx.ctx("Ask your foreman.");
    // As above: a child whose /proc links lead to writable scratch paths
    // outside the rules. A `..` spelling of one is the same link to the
    // kernel, so it must not be followed either. The cwd is three levels
    // below home/, so `cwd/../../..` leads back to a writable scratch dir.
    let work = fx.root.join("home/work/a/b");
    fs::create_dir_all(&work).unwrap();
    let out = fs::File::create(work.join("out.txt")).unwrap();
    let mut child = Command::new("sleep")
        .arg("30")
        .current_dir(&work)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .spawn()
        .expect("spawn sleep");
    let pid = child.id();
    let spelled = [
        format!("readlink /proc/{pid}/../{pid}/cwd: permission denied"),
        format!("open /proc/{pid}/task/../fd/1: permission denied"),
        format!("ls: cannot read symbolic link '/proc/../proc/{pid}/cwd': Permission denied"),
        format!("readlink /../proc/{pid}/cwd: permission denied"),
        // `..` after a link climbs from where the link points: this names
        // home/new.txt, though dropping each `..` with the part before it
        // would read /new.txt.
        format!("touch: cannot touch '/proc/{pid}/cwd/../../../new.txt': Permission denied"),
    ];
    let got: Vec<Option<Annotation>> = spelled.iter().map(|l| ctx.annotate("", l)).collect();
    let _ = child.kill();
    let _ = child.wait();
    for (line, a) in spelled.iter().zip(&got) {
        assert_eq!(a, &None, "{line}");
    }
    // A `..` outside /proc still counts.
    let p = format!("{}/other/../other/new.txt", fx.root.display());
    let a = ctx.annotate("", &format!("touch: cannot touch '{p}': Permission denied")).expect("a hint");
    assert_eq!(a.lines.len(), 1, "{:?}", a.lines);
    assert!(a.lines[0].starts_with("sandbox.denial_suspected session=s1 tool=bash tag=sbx-t1 class="), "{}", a.lines[0]);
    assert!(a.lines[0].ends_with(&format!(" path={p} src=output")), "{}", a.lines[0]);
}

#[test]
fn spawn_proc_magic_links_are_known_by_their_text() {
    for p in [
        "/proc/1/cwd",
        "/proc/self/exe",
        "/proc/thread-self/root",
        "/proc/42/fd/3",
        "/proc/self/map_files/7f00-7f01",
        "/proc/42/task/43/cwd",
        "/proc/self/task/7/fd/0",
        "/proc/42/task/43/map_files/a-b",
        "/proc//42/./cwd/",
    ] {
        assert!(is_proc_magic_link(Path::new(p)), "{p}");
    }
    for p in [
        "/proc/42/cwd/x",
        "/proc/42/comm",
        "/proc/self/attr/current",
        "/proc/sys/vm/drop_caches",
        "/proc/abc/cwd",
        "/proc/42/fd",
        "/proc/42/fd/x",
        "/proc/42/fdinfo/3",
        "/proc/42/map_files",
        "/proc/42/task/cwd",
        "/proc/42/task/x/cwd",
        "/proc/42/task/43/task/44/cwd",
        "/proc/thread-self/task/1/cwd",
        "/proc/cwd",
        "/procx/1/cwd",
        "/x/proc/1/cwd",
        "proc/1/cwd",
    ] {
        assert!(!is_proc_magic_link(Path::new(p)), "{p}");
    }
}

#[test]
fn spawn_proc_paths_with_a_dotdot_are_skipped_whole() {
    for p in [
        "/proc/42/../42/cwd",
        "/proc/../proc/42/fd/1",
        "/proc/42/task/../cwd",
        "/proc/self/..",
        "/proc/./42/../42/exe",
        "/proc/42/cwd/../x",
        "/proc/42/cwd/../../../x",
        "/proc/42/../../etc/x",
        "/../proc/42/cwd",
        "/tmp/../proc/42/root",
        "/a/b/../../proc/self/fd/3",
    ] {
        assert!(is_proc_dotdot(Path::new(p)), "{p}");
    }
    for p in [
        "/proc/42/cwd",
        "/proc/42/..x/cwd",
        "/proc/42/x../cwd",
        "/procx/../1/cwd",
        "/x/proc/../1/cwd",
        "/a/../b/proc/1/cwd",
        "/tmp/../tmp/x",
        "/tmp/a/../b",
        "proc/../proc/1/cwd",
        "../proc/1/cwd",
    ] {
        assert!(!is_proc_dotdot(Path::new(p)), "{p}");
    }
}

#[test]
fn spawn_annotate_a_refused_sudo() {
    let fx = fx();
    let ctx = fx.ctx("Ask your foreman.");
    let real = "sudo: The \"no new privileges\" flag is set, which prevents sudo from running as root.\n";
    let a = ctx.annotate("sudo -n true", real).expect("a hint");
    assert_eq!(a.lines, vec!["sandbox.sudo_refused session=s1 tool=bash tag=sbx-t1".to_string()]);
    assert_eq!(a.hint, format!("{SUDO_REFUSED} Ask your foreman."));
    let a = ctx
        .annotate("cd x && sudo -n true", "sudo: sudo must be owned by uid 0 and have the setuid bit set\n")
        .expect("sudo-rs self-check");
    assert_eq!(a.lines, vec!["sandbox.sudo_refused session=s1 tool=bash tag=sbx-t1".to_string()]);
    // The sandboxed sudo function already told the agent: log, no second hint.
    let a = ctx.annotate("sudo -n true", &format!("{SUDO_REFUSED} Ask your foreman.\n")).expect("logged");
    assert_eq!((a.lines.len(), a.hint.as_str()), (1, ""));
}

#[test]
fn spawn_annotate_output_that_quotes_a_refusal_is_not_a_refused_sudo() {
    let fx = fx();
    let ctx = fx.ctx("Ask your foreman.");
    // Reading a doc, this crate's source or a log that quotes the texts.
    let real = "sudo: The \"no new privileges\" flag is set, which prevents sudo from running as root.\n";
    let own = format!("{SUDO_REFUSED} Ask your foreman.\n");
    for command in ["sed -n 1,30p docs/sandbox.md", "grep -rn NNP_SUDO_TEXTS crates", "cat pseudo.txt visudo.log sudoers.md", ""] {
        assert_eq!(ctx.annotate(command, real), None, "{command}");
        assert_eq!(ctx.annotate(command, &own), None, "{command}");
    }
    // A refused write in the same output still gets its line and hint.
    let notes = fx.r("home/notes");
    let out = format!("{real}rm: cannot remove '{notes}': Permission denied\n");
    let a = ctx.annotate("cat docs/sandbox.md; rm notes", &out).expect("the refused write");
    assert_eq!(a.lines, vec![format!("sandbox.denial_suspected session=s1 tool=bash tag=sbx-t1 class=home_other path={notes} src=output")]);
    assert!(!a.hint.contains(SUDO_REFUSED), "{}", a.hint);
}

#[test]
fn spawn_invokes_sudo_is_the_word_sudo() {
    // A command that names sudo as a word counts even when it only reads
    // (`grep sudo …`): the documented limit, kept narrow on purpose.
    for yes in [
        "sudo -n true",
        "cd d && sudo x",
        "x;sudo y",
        "command sudo -n true",
        "/usr/bin/sudo x",
        "sudo-rs -n true",
        "echo $(sudo id)",
        "sudo",
        "grep -n sudo notes.md",
    ] {
        assert!(invokes_sudo(yes), "{yes}");
    }
    for no in ["", "pseudo x", "visudo -c", "cat /etc/sudoers", "sudo_x", "x_sudo", "cat sudoku", "grep -n SUDO_REFUSED x"] {
        assert!(!invokes_sudo(no), "{no}");
    }
}

#[test]
fn spawn_command_in_strips_the_sudo_prelude() {
    let fx = fx();
    let ctx = fx.ctx("Ask your foreman.");
    assert_eq!(ctx.command_in(&ctx.bash_script("cat notes.md")), "cat notes.md");
    assert_eq!(ctx.command_in(&ctx.bash_script("")), "");
    // Not a script it made: the whole of it, so a sudo in it still counts.
    assert_eq!(ctx.command_in("sudo -n true"), "sudo -n true");
    assert!(invokes_sudo(ctx.command_in(&ctx.bash_script("cd d && sudo -n true"))));
    assert!(!invokes_sudo(ctx.command_in(&ctx.bash_script("cat notes.md"))));
}

#[test]
fn spawn_no_text_says_password() {
    // bash.rs treats output with "sudo" and "password" as a password prompt.
    let fx = fx();
    let ctx = fx.ctx(DEFAULT_CONTACT);
    let mut texts = vec![SUDO_REFUSED.to_string(), DEFAULT_CONTACT.to_string(), ctx.bash_script("true")];
    for out in [
        format!("rm: cannot remove '{}': Permission denied", fx.r("home/notes")),
        format!("fatal: could not create leading directories of '{}': Permission denied", fx.r("home/.worktrees/a")),
        "mkdir: Permission denied".to_string(),
        "sudo: The \"no new privileges\" flag is set, which prevents sudo from running as root.".to_string(),
    ] {
        texts.push(ctx.annotate("sudo -n true", &out).expect("a hint").hint);
    }
    for t in texts {
        assert!(!t.to_lowercase().contains("password"), "{t}");
    }
}

#[test]
fn spawn_bash_script_sudo_function_refuses_without_running_sudo() {
    let fx = fx();
    let ctx = fx.ctx("Ask JR's foreman.");
    // `sudo -n true` with an empty PATH: if the function were missing, no
    // real sudo could be found (exit 127), so nothing ever runs as root here.
    let script = ctx.bash_script("sudo -n true; echo after=$?");
    let o = Command::new("/bin/bash").arg("-c").arg(&script).env("LC_ALL", "C").env("PATH", "/nonexistent").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&o.stdout), "after=1\n");
    assert_eq!(String::from_utf8_lossy(&o.stderr), format!("{SUDO_REFUSED} Ask JR's foreman.\n"));
}

#[test]
fn spawn_bindings_are_one_per_session() {
    let a = SandboxBinding::for_session(Some("sbx-test-registry-a")).expect("a binding");
    let again = SandboxBinding::for_session(Some("sbx-test-registry-a")).expect("a binding");
    let b = SandboxBinding::for_session(Some("sbx-test-registry-b")).expect("a binding");
    assert!(Arc::ptr_eq(&a, &again), "one binding per session id: its logged state is shared by every call");
    assert!(!Arc::ptr_eq(&a, &b));
    assert_eq!((a.session_id(), b.session_id()), ("sbx-test-registry-a", "sbx-test-registry-b"));
    assert!(SandboxBinding::for_session(None).is_none(), "no conversation, no sandbox");
}

#[test]
fn spawn_quoting_and_lists() {
    assert_eq!(sh_single_quote("a'b"), "'a'\\''b'");
    assert_eq!(sh_single_quote(""), "''");
    let paths: Vec<PathBuf> = (0..8).map(|i| PathBuf::from(format!("/p{i}"))).collect();
    let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    assert_eq!(list(&refs), "/p0, /p1, /p2, /p3, /p4, /p5 (+2 more)");
    assert_eq!(list(&refs[..2]), "/p0, /p1");
}

// ── Real-kernel tests: LOCAL kernel only (not g1pro/WSL). ────────────────

/// Opt-in gate, as in the landlock tests: unset, skip; SYNAPS_TEST_LANDLOCK=1
/// on a host without a usable Landlock, FAIL.
fn local_kernel() -> bool {
    if std::env::var("SYNAPS_TEST_LANDLOCK").as_deref() != Ok("1") {
        return false;
    }
    match abi_version() {
        Ok(abi) if abi >= MIN_ABI => true,
        other => panic!("SYNAPS_TEST_LANDLOCK=1 but Landlock is unusable here: {other:?}"),
    }
}

fn enforce(fx: &Fx) -> Prepared {
    fx.write_policy("s1", &format!("mode = enforce\nwrite = {}\ncontact = Ask your foreman.\n", fx.r("lane")));
    let mut log = Vec::new();
    match prepare_into(&binding(fx, "s1", None), &mut log) {
        SpawnSandbox::Apply(p) => *p,
        SpawnSandbox::None => panic!("expected a domain, got none: {log:?}"),
        SpawnSandbox::Refuse(m) => panic!("expected a domain, got a refusal: {m}"),
    }
}

#[test]
fn spawn_local_enforce_applies_the_policy_to_a_command() {
    if !local_kernel() {
        return;
    }
    let fx = fx();
    fs::write(fx.root.join("home/victim"), "keep\n").unwrap();
    fx.write_policy("s1", &format!("mode = enforce\nwrite = {}\ncontact = Ask your foreman.\n", fx.r("lane")));
    let b = binding(&fx, "s1", None);
    let mut log = Vec::new();
    let SpawnSandbox::Apply(p) = prepare_into(&b, &mut log) else { panic!("expected a domain: {log:?}") };
    assert!(log[0].starts_with("sandbox.policy session=s1 mode=enforce subagents=enforce abi="), "{log:?}");
    let line = p.spawned_line(Some(42));
    assert_eq!(line, format!("sandbox.spawn session=s1 tool=bash tag={} pid=42", p.tag()));

    let mut cmd = Command::new("/bin/sh");
    cmd.env("LC_ALL", "C").arg("-c").arg(format!(
        "echo a > {}; echo in=$?; rm -f {}; echo rm=$?",
        fx.r("lane/ok"),
        fx.r("home/victim")
    ));
    p.install(&mut cmd).unwrap();
    let o = cmd.output().unwrap();
    let (out, err) = (String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    assert_eq!(out, "in=0\nrm=1\n", "stderr:\n{err}");
    assert_eq!(fs::read_to_string(fx.root.join("lane/ok")).unwrap(), "a\n");
    assert_eq!(fs::read_to_string(fx.root.join("home/victim")).unwrap(), "keep\n");

    let a = p.annotate("", &err).expect("the refused rm is annotated");
    assert_eq!(
        a.lines,
        vec![format!(
            "sandbox.denial_suspected session=s1 tool=bash tag={} class=home_other path={} src=output",
            p.tag(),
            fx.r("home/victim")
        )]
    );
    let mut again = Vec::new();
    let SpawnSandbox::Apply(p2) = prepare_into(&b, &mut again) else { panic!("expected a domain") };
    assert!(again.is_empty(), "the policy is logged once: {again:?}");
    assert_ne!(p2.tag(), p.tag(), "every spawn gets its own tag");
    assert_eq!(p.spawn_failed_line(&"EBADFD"), format!("sandbox.spawn_failed session=s1 tool=bash tag={} error=EBADFD", p.tag()));
}

#[test]
fn spawn_local_git_common_packed_refs_lock_is_refused_and_classed() {
    if !local_kernel() {
        return;
    }
    let fx = fx();
    let git = fx.root.join("home/repo/.git");
    for d in ["objects", "refs"] {
        fs::create_dir_all(git.join(d)).unwrap();
    }
    fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fx.write_policy(
        "s1",
        &format!("mode = enforce\nwrite = {}\ngit_common = {}\ncontact = Ask your foreman.\n", fx.r("lane"), git.display()),
    );
    let mut log = Vec::new();
    let SpawnSandbox::Apply(p) = prepare_into(&binding(&fx, "s1", None), &mut log) else { panic!("expected a domain: {log:?}") };
    assert_eq!(p.ctx().git_common, vec![git.clone()], "one entry per git_common line");
    let lock = git.join("packed-refs.lock");
    let mut cmd = Command::new("/bin/sh");
    cmd.env("LC_ALL", "C").arg("-c").arg(format!("touch {}; touch {}/refs/ok; echo rc=$?", lock.display(), git.display()));
    p.install(&mut cmd).unwrap();
    let o = cmd.output().unwrap();
    let (out, err) = (String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    assert_eq!(out, "rc=0\n", "refs/ is writable; stderr:\n{err}");
    assert!(!lock.exists(), "the common dir's root is not");
    let a = p.annotate("", &err).expect("the refused lock is annotated");
    assert_eq!(
        a.lines,
        vec![format!(
            "sandbox.denial_suspected session=s1 tool=bash tag={} class=git_packed_refs path={} src=output",
            p.tag(),
            lock.display()
        )]
    );
    assert!(a.hint.starts_with(&format!("[sandbox] git may not create {} (by design", lock.display())), "{}", a.hint);
}

#[test]
fn spawn_local_one_command_spawned_twice_is_restricted_both_times() {
    if !local_kernel() {
        return;
    }
    let fx = fx();
    let p = enforce(&fx);
    let mut cmd = Command::new("/bin/sh");
    cmd.env("LC_ALL", "C").arg("-c").arg(format!(
        "touch {}/in-$$; echo in=$?; touch {}/out-$$; echo out=$?",
        fx.r("lane"),
        fx.r("other")
    ));
    p.install(&mut cmd).unwrap();
    let first = cmd.output().unwrap();
    drop(p); // the hook owns its own fd
    let second = cmd.output().unwrap();
    for o in [first, second] {
        assert_eq!(String::from_utf8_lossy(&o.stdout), "in=0\nout=1\n", "{}", String::from_utf8_lossy(&o.stderr));
    }
    assert_eq!(fs::read_dir(fx.root.join("lane")).unwrap().count(), 2, "two writes inside");
    assert_eq!(fs::read_dir(fx.root.join("other")).unwrap().count(), 0, "nothing outside");
}

#[test]
fn spawn_local_dropping_an_unspawned_confined_command_closes_its_fd() {
    if !local_kernel() {
        return;
    }
    let fx = fx();
    let p = enforce(&fx);
    let ruleset_fds = || {
        fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| fs::read_link(e.path()).map(|l| l.to_string_lossy().contains("landlock-ruleset")).unwrap_or(false))
            .count()
    };
    let before = ruleset_fds();
    for _ in 0..64 {
        let mut cmd = Command::new("/bin/true");
        p.install(&mut cmd).unwrap();
        drop(cmd);
    }
    let after = ruleset_fds();
    // Other tests may hold a few ruleset fds at any moment; a leak adds 64.
    assert!(after < before + 16, "ruleset fds before {before}, after {after}: a dropped Command must close its duplicate");
}
