//! Tests of the kernel-audit reader. Every audit file here is a scratch file
//! (`test_scratch_parent`) holding records the tests write, with made-up
//! paths, pids, exes and tags; no test but the last reads the host's audit
//! log. `audit_local_*` run only with SYNAPS_TEST_LANDLOCK=1 (LOCAL kernel).
//! Three of them build a prepared spawn's ruleset and check its registration
//! and the reader's open, but restrict nothing (no child, no confined
//! thread). Only `audit_local_the_real_audit_log_reports_a_scratch_denial`
//! restricts a thread (a refused create on a scratch dir) and reads the
//! kernel's records of it; it passes WITHOUT checking anything when the
//! audit log can't be read here, and prints why it skipped.

use super::*;
use crate::sandbox::landlock::{abi_version, tag_for, Ruleset, MIN_ABI, WRITE_RIGHTS};
use crate::sandbox::policy::{test_scratch_parent, KillSwitch, PolicyEnv, DEFAULT_AUDIT_LOG};
use crate::sandbox::{spawn, SandboxBinding, SpawnSandbox};
use std::fs;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// The daemon's exe and pid as the tail under test knows them.
const EXE: &str = "/opt/synaps/bin/synaps";
const DAEMON_PID: u32 = 1000;
/// The tests' clock (unix ms) when a rig starts.
const T0: u64 = 1_792_485_000_000;

fn scratch() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::Builder::new().prefix("sbx-audit-").tempdir_in(test_scratch_parent()).expect("tempdir");
    let root = fs::canonicalize(tmp.path()).expect("canonical tempdir");
    (tmp, root)
}

fn append(path: &Path, text: &str) {
    let mut f = fs::OpenOptions::new().create(true).append(true).open(path).expect("open scratch log");
    f.write_all(text.as_bytes()).expect("append");
}

/// A clock the test sets.
#[derive(Clone)]
struct FakeClock(Arc<AtomicU64>);

impl FakeClock {
    fn new(ms: u64) -> Self {
        FakeClock(Arc::new(AtomicU64::new(ms)))
    }
    fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst)
    }
    fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
    fn clock(&self) -> Clock {
        let c = Arc::clone(&self.0);
        Arc::new(move || c.load(Ordering::SeqCst))
    }
}

/// A rig's two clocks: unix time (record stamps, spawn times: the join) and
/// the monotonic one (every elapsed-time gate). `set` moves both.
#[derive(Clone)]
struct Clocks {
    unix: FakeClock,
    mono: FakeClock,
}

impl Clocks {
    fn new(ms: u64) -> Self {
        Clocks { unix: FakeClock::new(ms), mono: FakeClock::new(ms) }
    }
    fn set(&self, ms: u64) {
        self.unix.set(ms);
        self.mono.set(ms);
    }
    fn now(&self) -> u64 {
        self.unix.now()
    }
}

fn ts(ms: u64) -> String {
    format!("{}.{:03}", ms / 1000, ms % 1000)
}

/// A 1423 record: `domain` was refused `blockers` on `path` at `ms`.
fn access(ms: u64, domain: u64, blockers: &str, path: &str) -> String {
    format!(
        "type=UNKNOWN[1423] msg=audit({}:{}): domain={domain:x} blockers={blockers} path=\"{path}\" dev=\"dm-1\" ino=42\n",
        ts(ms),
        ms % 100_000
    )
}

/// A 1423 ptrace record (a /proc read of a process outside the domain).
fn ptrace(ms: u64, domain: u64) -> String {
    format!("type=UNKNOWN[1423] msg=audit({}:{}): domain={domain:x} blockers=ptrace opid=3226 ocomm=\"chrome\"\n", ts(ms), ms % 100_000)
}

/// The 1424 record that comes with a domain's first denial (ENRICHED: the
/// interpreted fields after \x1d).
fn allocated(ms: u64, domain: u64, pid: u32, exe: &str, comm: &str) -> String {
    format!(
        "type=UNKNOWN[1424] msg=audit({}:{}): domain={domain:x} status=allocated mode=enforcing pid={pid} uid=1000 exe=\"{exe}\" comm=\"{comm}\"\x1dUID=\"jr\"\n",
        ts(ms),
        ms % 100_000
    )
}

fn dealloc(ms: u64, domain: u64, denials: u64) -> String {
    format!("type=UNKNOWN[1424] msg=audit({}:{}): domain={domain:x} status=deallocated denials={denials}\n", ts(ms), ms % 100_000)
}

fn info(session: &str, quiet: bool, home: &Path) -> SpawnInfo {
    SpawnInfo {
        session: session.into(),
        tool: "bash",
        quiet,
        home: home.to_path_buf(),
        base: home.join("x/base"),
        git_common: vec![home.join("repo/.git")],
        cargo_home: vec![home.join(".cargo")],
    }
}

fn denial(session: &str, tag: &str, op: &str, class: &str, path: &str, n: u64) -> (Level, String) {
    (Level::Warn, format!("sandbox.denial session={session} tool=bash tag={tag} op={op} class={class} path={path} n={n} src=audit"))
}

/// A tail on a scratch log, driven by hand: no thread.
struct Rig {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    log: PathBuf,
    clock: Clocks,
    reg: Arc<Registry>,
    tail: Tail,
    f: Follower,
}

fn rig() -> Rig {
    rig_with(EXE, "")
}

/// A tail for a daemon whose exe is `exe`, on a log that already holds
/// `before` when it is opened.
fn rig_with(exe: &str, before: &str) -> Rig {
    rig_tail(Some(exe), before)
}

/// [`rig_with`], for a daemon whose exe may be unknown (`None`).
fn rig_tail(exe: Option<&str>, before: &str) -> Rig {
    let (tmp, root) = scratch();
    let log = root.join("audit.log");
    append(&log, before);
    let clock = Clocks::new(T0);
    let reg = Registry::with_clocks(clock.unix.clock(), clock.mono.clock());
    let f = Follower::open_at_end(&log).expect("open scratch log");
    Rig { _tmp: tmp, root, log, clock, reg, tail: Tail::new(exe.map(|e| e.as_bytes().to_vec()), DAEMON_PID), f }
}

impl Rig {
    fn home(&self) -> PathBuf {
        self.root.join("home")
    }
    fn h(&self, rel: &str) -> String {
        self.home().join(rel).display().to_string()
    }
    fn spawn(&self, tag: &str, session: &str, quiet: bool) -> Registration {
        self.reg.register(&Tag::new(tag).expect("tag"), info(session, quiet, &self.home()))
    }
    fn poll(&mut self) -> Vec<(Level, String)> {
        self.poll_until(&|| false)
    }
    /// One poll, asked to stop by `stop`.
    fn poll_until(&mut self, stop: &dyn Fn() -> bool) -> Vec<(Level, String)> {
        let mut out = Vec::new();
        let now = self.clock.mono.now();
        self.tail.poll(&mut self.f, &self.reg, now, stop, &mut |l, s| out.push((l, s)));
        out
    }
    fn feed(&mut self, text: &str) -> Vec<(Level, String)> {
        append(&self.log, text);
        self.poll()
    }
    fn at(&mut self, ms: u64) -> Vec<(Level, String)> {
        self.clock.set(ms);
        self.poll()
    }
}

// ── Records ──────────────────────────────────────────────────────────────

#[test]
fn audit_parse_reads_both_spellings_and_only_the_raw_fields() {
    let a = b"type=UNKNOWN[1423] msg=audit(1792485022.753:124381): domain=140319882 blockers=fs.write_file path=\"/home/jr/x\" dev=\"dm-1\" ino=1\x1dUID=\"jr\"";
    let want = Record {
        time_ms: 1_792_485_022_753,
        domain: 0x1_4031_9882,
        body: Body::Access { blockers: vec!["fs.write_file".into()], path: Some(b"/home/jr/x".to_vec()) },
    };
    assert_eq!(parse(a), Parsed::Record(want));
    // auditd writes a value with a space, quote, control or non-ASCII byte
    // as bare hex: "/tmp/a b".
    let b = b"type=LANDLOCK_ACCESS msg=audit(1792485022.007:9): domain=1a blockers=fs.make_reg,fs.refer path=2F746D702F612062";
    let want = Record {
        time_ms: 1_792_485_022_007,
        domain: 0x1a,
        body: Body::Access { blockers: vec!["fs.make_reg".into(), "fs.refer".into()], path: Some(b"/tmp/a b".to_vec()) },
    };
    assert_eq!(parse(b), Parsed::Record(want));
    let c = b"type=UNKNOWN[1424] msg=audit(1.002:3): domain=ff status=allocated mode=enforcing pid=7 uid=1000 exe=2F612062 comm=7362782D31";
    let want = Record {
        time_ms: 1002,
        domain: 0xff,
        body: Body::Allocated { pid: 7, exe: Some(b"/a b".to_vec()), comm: Some(b"sbx-1".to_vec()) },
    };
    assert_eq!(parse(c), Parsed::Record(want));
    let d = b"type=LANDLOCK_DOMAIN msg=audit(1.000:4): domain=ff status=deallocated denials=36";
    assert_eq!(parse(d), Parsed::Record(Record { time_ms: 1000, domain: 0xff, body: Body::Deallocated { denials: 36 } }));
    // The interpreted part after \x1d is never read: a field there doesn't count.
    assert_eq!(parse(b"type=UNKNOWN[1424] msg=audit(1.000:4): domain=ff status=deallocated\x1ddenials=36"), Parsed::Malformed);
    // Other records aren't read at all, and a 1424 of another status is skipped.
    let others: [&[u8]; 7] = [
        b"type=SYSCALL msg=audit(1.000:1): arch=c000003e syscall=257 success=no exit=-13",
        b"type=PROCTITLE msg=audit(1.000:1): proctitle=736C656570",
        b"type=PATH msg=audit(1.000:1): item=0 name=\"/x\" nametype=PARENT",
        b"type=UNKNOWN[1424] msg=audit(1.000:4): domain=ff status=something-new",
        b"",
        b"not an audit line",
        b"type=UNKNOWN[14230] msg=audit(1.000:1): domain=1 blockers=fs.make_reg",
    ];
    for other in others {
        assert_eq!(parse(other), Parsed::Other, "{:?}", String::from_utf8_lossy(other));
    }
}

#[test]
fn audit_parse_drops_a_malformed_landlock_record_and_never_panics() {
    for bad in [
        "type=UNKNOWN[1423] domain=1 blockers=fs.make_reg path=\"/x\"",
        "type=UNKNOWN[1423] msg=audit(x.y:z): domain=1 blockers=fs.make_reg path=\"/x\"",
        "type=UNKNOWN[1423] msg=audit(1.000:1): blockers=fs.make_reg path=\"/x\"",
        "type=UNKNOWN[1423] msg=audit(1.000:1): domain=zz blockers=fs.make_reg path=\"/x\"",
        "type=UNKNOWN[1423] msg=audit(1.000:1): domain=1 path=\"/x\"",
        "type=UNKNOWN[1423] msg=audit(1.000:1): domain=1 blockers= path=\"/x\"",
        "type=UNKNOWN[1423] msg=audit(1.000:1): domain=1 blockers=fs.MAKE path=\"/x\"",
        "type=UNKNOWN[1423] msg=audit(1.000:1): domain=1 blockers=fs.make_reg path=2F7",
        "type=UNKNOWN[1423] msg=audit(1.000:1): domain=1 blockers=fs.make_reg path=GG",
        "type=UNKNOWN[1423] msg=audit(1.000:1): domain=1 blockers=fs.make_reg path=\"/x",
        "type=UNKNOWN[1424] msg=audit(1.000:1): domain=1 status=allocated exe=\"/x\" comm=\"sbx-1\"",
        "type=UNKNOWN[1424] msg=audit(1.000:1): domain=1 status=allocated pid=x exe=\"/x\" comm=\"sbx-1\"",
        "type=UNKNOWN[1424] msg=audit(1.000:1): domain=1 status=deallocated",
        "type=UNKNOWN[1424] msg=audit(1.000:1): domain=1 status=deallocated denials=-1",
    ] {
        assert_eq!(parse(bad.as_bytes()), Parsed::Malformed, "{bad}");
    }
    let full = "type=UNKNOWN[1424] msg=audit(1792485022.753:124381): domain=140319882 status=allocated mode=enforcing pid=752387 uid=1000 exe=\"/x/synaps\" comm=736278";
    for i in 0..=full.len() {
        let _ = parse(&full.as_bytes()[..i]);
    }
    let bytes: Vec<u8> = (0..=255u8).collect();
    for w in bytes.windows(7) {
        let mut line = b"type=UNKNOWN[1423] msg=audit(1.000:1): domain=1 blockers=fs.make_reg path=".to_vec();
        line.extend_from_slice(w);
        let _ = parse(&line);
    }
}

#[test]
fn audit_op_names_what_the_kernel_refused() {
    let all = "fs.write_file,fs.remove_dir,fs.remove_file,fs.make_char,fs.make_dir,fs.make_reg,fs.make_sock,fs.make_fifo,fs.make_block,fs.make_sym,fs.refer,fs.truncate";
    for (blockers, want) in [
        ("fs.write_file", "write"),
        ("fs.truncate", "truncate"),
        ("fs.write_file,fs.truncate", "write+truncate"),
        ("fs.make_reg", "create"),
        ("fs.make_dir", "mkdir"),
        ("fs.make_sym", "symlink"),
        ("fs.make_char", "mknod"),
        ("fs.make_block", "mknod"),
        ("fs.make_fifo", "mknod"),
        ("fs.make_sock", "mknod"),
        ("fs.remove_file", "remove"),
        ("fs.remove_dir", "remove"),
        ("fs.remove_file,fs.remove_dir", "remove"),
        ("fs.refer", "rename/link"),
        ("fs.make_reg,fs.refer", "rename/link"),
        (all, "rename/link"),
        ("fs.change_topology", "mount"),
        ("fs.ioctl_dev", "ioctl_dev"),
    ] {
        let b: Vec<String> = blockers.split(',').map(String::from).collect();
        assert_eq!(op_of(&b), want, "{blockers}");
    }
}

#[test]
fn audit_paths_print_plain_or_quoted() {
    assert_eq!(render_path(b"/home/jr/.cargo"), "/home/jr/.cargo");
    // Anything else is quoted, every byte that isn't visible ASCII (the
    // space too) as \xNN, and `"` and `\` escaped: one field, no controls,
    // and the bytes can be read back.
    assert_eq!(render_path(b"/tmp/a b"), "\"/tmp/a\\x20b\"");
    assert_eq!(render_path(b"/tmp/a\nb"), "\"/tmp/a\\x0Ab\"");
    assert_eq!(render_path(b"/tmp/a\x1b[0m"), "\"/tmp/a\\x1B[0m\"");
    assert_eq!(render_path(b"/tmp/a=\"b\""), "\"/tmp/a=\\\"b\\\"\"");
    assert_eq!(render_path(b"/tmp/a\\b"), "\"/tmp/a\\\\b\"");
    assert_eq!(render_path(b"/tmp/\xff\x7f"), "\"/tmp/\\xFF\\x7F\"");
    assert_eq!(render_path("/tmp/é".as_bytes()), "\"/tmp/\\xC3\\xA9\"");
    assert_eq!(render_path(b""), "\"\"");
}

#[test]
fn audit_class_marks_by_design_refusals_and_classes_the_rest() {
    let i = info("s1", false, Path::new("/h"));
    let mut bad = Vec::new();
    for (op, path, want) in [
        // The common dir's root stays read-only by design. A create or
        // remove names the parent dir; a write names the file.
        ("create", "/h/repo/.git", "by_design:git_common"),
        ("remove", "/h/repo/.git", "by_design:git_common"),
        ("rename/link", "/h/repo/.git", "by_design:git_common"),
        ("write", "/h/repo/.git/config", "by_design:git_common"),
        ("write+truncate", "/h/repo/.git/index", "by_design:git_common"),
        ("mkdir", "/h/repo/.git/rr-cache", "home_other"),
        ("write", "/h/repo/.git/hooks/pre-commit", "home_other"),
        ("write", "/h/repo/.git", "home_other"),
        // A create in cargo's home's root names only that dir: it may be
        // cargo's own file, or not. Logged, and not exempt.
        ("create", "/h/.cargo", "uncertain:cargo_home"),
        ("mkdir", "/h/.cargo", "home_dot:.cargo"),
        ("write", "/h/.cargo/config.toml", "home_dot:.cargo"),
        ("create", "/h/.cargo/bin", "home_dot:.cargo"),
        // The session registry's lock, at its one path (the registry reads
        // it then): a write or truncate names it.
        ("write", "/h/x/base/run/.session-registry.lock", "by_design:session_registry"),
        ("truncate", "/h/x/base/run/.session-registry.lock", "by_design:session_registry"),
        ("write", "/h/x/base/sessions/.session-registry.lock", "synaps_state"),
        ("create", "/h/x/base/run", "synaps_state"),
        ("write", "/elsewhere/.session-registry.lock", "other"),
        ("write", "/h/x/base/sessions/a.json", "synaps_state"),
        // less's history (through git's pager): a write names the file. A
        // create names only ~/.local/state, so it may be any program's file:
        // logged, and not exempt.
        ("write", "/h/.local/state/lesshst", "by_design:less_history"),
        ("truncate", "/h/.local/state/lesshst", "by_design:less_history"),
        ("create", "/h/.local/state", "uncertain:local_state"),
        ("mkdir", "/h/.local/state", "home_dot:.local"),
        ("write", "/h/.local/state/other", "home_dot:.local"),
        ("create", "/h/.local/share", "home_dot:.local"),
        // A class is only ever [a-z0-9_:.-]: a path part outside that
        // becomes a fixed placeholder (a control could forge a line).
        ("write", "/h/.a\nb session=x/f", "home_dot:unprintable"),
        ("write", "/h/.a\x1b[31m/f", "home_dot:unprintable"),
        ("write", "/h/.Xauthority", "home_dot:unprintable"),
        ("write", "/h/.ok-name_1.d/f", "home_dot:.ok-name_1.d"),
        // Everything else as the output matcher classes it.
        ("create", "/h/repo/.worktrees/x", "worktree_new"),
        ("create", "/var/tmp/pria-release/heavy1-logs", "system"),
        ("write", "/dev/dri/renderD128", "dev_proc"),
        ("create", "/h/work", "home_other"),
        ("create", "/data/x", "other"),
    ] {
        let got = class_of(&i, op, Path::new(path));
        if got != want {
            bad.push(format!("{op} {path}: got {got:?}, want {want:?}"));
        }
    }
    assert!(bad.is_empty(), "{} case(s) wrong:\n{}", bad.len(), bad.join("\n"));
}

// ── Following the file ───────────────────────────────────────────────────

fn read(f: &mut Follower) -> Vec<String> {
    let mut lines = Vec::new();
    f.poll(&mut |l| lines.push(String::from_utf8_lossy(l).into_owned())).expect("poll");
    lines
}

/// [`read`] in at most 16 steps: a follower that never reaches the end of
/// a small file (reopening forever) fails instead of hanging.
fn read_bounded(f: &mut Follower) -> Vec<String> {
    let mut lines = Vec::new();
    for _ in 0..16 {
        if f.step(&mut |l| lines.push(String::from_utf8_lossy(l).into_owned())).expect("step") == Step::End {
            return lines;
        }
    }
    panic!("no end in 16 steps (it reads again and again): {lines:?}");
}

#[test]
fn audit_follower_reads_from_the_end_and_keeps_a_partial_line_until_its_newline() {
    let (_tmp, root) = scratch();
    let log = root.join("audit.log");
    append(&log, "old 1\nold 2\n");
    let mut f = Follower::open_at_end(&log).expect("open");
    assert!(read(&mut f).is_empty(), "nothing from before the open");
    append(&log, "new 1\npart");
    assert_eq!(read(&mut f), vec!["new 1"]);
    append(&log, "ial\n");
    assert_eq!(read(&mut f), vec!["partial"]);
}

#[test]
fn audit_follower_skips_the_rest_of_a_line_being_written_at_open() {
    let (_tmp, root) = scratch();
    let log = root.join("audit.log");
    append(&log, "whole\nhalf a reco");
    let mut f = Follower::open_at_end(&log).expect("open");
    append(&log, "rd\nnext\n");
    assert_eq!(read(&mut f), vec!["next"]);
    assert_eq!(f.take_long(), 0, "not an over-long line");
}

#[test]
fn audit_follower_skips_and_counts_an_overlong_line() {
    let (_tmp, root) = scratch();
    let log = root.join("audit.log");
    append(&log, "");
    let mut f = Follower::open_at_end(&log).expect("open");
    append(&log, &format!("{}\nnext\n{}\n", "a".repeat(MAX_LINE + 1), "b".repeat(MAX_LINE)));
    let lines = read(&mut f);
    assert_eq!(lines.len(), 2, "the over-long line is skipped, a line of exactly the cap is kept");
    assert_eq!(lines[0], "next");
    assert_eq!(lines[1].len(), MAX_LINE);
    assert_eq!(f.take_long(), 1);
    // Over the cap across two polls.
    append(&log, &"c".repeat(MAX_LINE / 2 + 1));
    assert!(read(&mut f).is_empty());
    append(&log, &format!("{}\nok\n", "c".repeat(MAX_LINE / 2 + 1)));
    assert_eq!(read(&mut f), vec!["ok"]);
    assert_eq!(f.take_long(), 1);
    assert_eq!(f.take_long(), 0, "taken");
}

#[test]
fn audit_follower_restarts_at_zero_after_a_truncate() {
    let (_tmp, root) = scratch();
    let log = root.join("audit.log");
    append(&log, &"x\n".repeat(10));
    let mut f = Follower::open_at_end(&log).expect("open");
    append(&log, "y\n");
    assert_eq!(read(&mut f), vec!["y"]);
    fs::OpenOptions::new().write(true).open(&log).unwrap().set_len(0).unwrap();
    append(&log, "z\n");
    assert_eq!(read(&mut f), vec!["z"]);
}

#[test]
fn audit_follower_drains_the_old_file_then_reads_the_new_one_after_a_rotation() {
    let (_tmp, root) = scratch();
    let log = root.join("audit.log");
    append(&log, "a\n");
    let mut f = Follower::open_at_end(&log).expect("open");
    append(&log, "b\n");
    fs::rename(&log, root.join("audit.log.1")).unwrap();
    // The name is gone for a moment: keep the old file.
    append(&root.join("audit.log.1"), "c\n");
    assert_eq!(read(&mut f), vec!["b", "c"]);
    append(&root.join("audit.log.1"), "d\n");
    append(&log, "e\n");
    assert_eq!(read(&mut f), vec!["d", "e"], "the old file's last lines, then the new file from its start");
    append(&log, "f\n");
    assert_eq!(read(&mut f), vec!["f"]);
}

#[test]
fn audit_follower_open_fails_for_a_missing_file_or_a_directory() {
    let (_tmp, root) = scratch();
    let e = Follower::open_at_end(&root.join("missing")).err().expect("missing file");
    assert_eq!(reason(&e), "ENOENT");
    let e = Follower::open_at_end(&root).err().expect("a directory");
    assert_eq!(reason(&e), "not-a-file");
}

// ── The join and the lines ───────────────────────────────────────────────

#[test]
fn audit_a_spawns_denial_logs_one_line_with_its_session_tool_tag_op_and_class() {
    let mut r = rig();
    let _s = r.spawn("sbx-a1", "s1", false);
    let (t, work) = (T0 + 100, r.h("work"));
    let out = r.feed(&(access(t, 0xd1, "fs.make_reg", &work) + &allocated(t, 0xd1, DAEMON_PID, EXE, "sbx-a1")));
    assert_eq!(out, vec![denial("s1", "sbx-a1", "create", "home_other", &work, 1)]);
}

#[test]
fn audit_the_new_spellings_and_hex_fields_join_the_same_way() {
    let mut r = rig();
    let _s = r.spawn("sbx-a2", "s1", false);
    let hex = |s: &str| s.bytes().map(|b| format!("{b:02X}")).collect::<String>();
    let spaced = r.h("a b");
    let text = format!(
        "type=LANDLOCK_ACCESS msg=audit({}:5): domain=d2 blockers=fs.write_file path={} dev=\"dm-1\" ino=7\x1dX=\"y\"\n\
         type=LANDLOCK_DOMAIN msg=audit({}:5): domain=d2 status=allocated mode=enforcing pid={DAEMON_PID} uid=1000 exe={} comm={}\n",
        ts(T0 + 1),
        hex(&spaced),
        ts(T0 + 1),
        hex(EXE),
        hex("sbx-a2")
    );
    let out = r.feed(&text);
    assert_eq!(out, vec![denial("s1", "sbx-a2", "write", "home_other", &format!("\"{}\"", spaced.replace(' ', "\\x20")), 1)]);
}

#[test]
fn audit_only_the_daemons_own_domains_are_read() {
    let mut r = rig();
    let _s = r.spawn("sbx-b1", "s1", false);
    let t = T0 + 10;
    // A test binary's domain, even with a tag this daemon gave out: skipped,
    // and not counted as unattributed.
    let test_exe = "/var/tmp/l/target/debug/deps/agent_engine-0123";
    let mut text = access(t, 0xe1, "fs.make_reg", "/var/tmp/x") + &allocated(t, 0xe1, DAEMON_PID, test_exe, "sbx-b1");
    text += &access(t, 0xe1, "fs.make_reg", "/var/tmp/x");
    text += &dealloc(t, 0xe1, 2);
    assert!(r.feed(&text).is_empty());
    assert!(r.at(T0 + PENDING_MS + REPORT_MS).is_empty(), "a foreign domain is not unattributed");
    // The installed binary was replaced since the daemon started. The kernel
    // writes a value with a space as hex, so " (deleted)" comes hex-encoded.
    let mut r = rig();
    let _s = r.spawn("sbx-b2", "s1", false);
    let deleted: String = format!("{EXE} (deleted)").bytes().map(|b| format!("{b:02X}")).collect();
    let rec = allocated(t, 0xe2, DAEMON_PID, EXE, "sbx-b2").replace(&format!("exe=\"{EXE}\""), &format!("exe={deleted}"));
    assert!(rec.contains(&format!(" exe={deleted} ")), "{rec}");
    let out = r.feed(&(access(t, 0xe2, "fs.make_reg", "/var/tmp/x") + &rec));
    assert_eq!(out, vec![denial("s1", "sbx-b2", "create", "system", "/var/tmp/x", 1)]);
    let mut r = rig_with(&format!("{EXE} (deleted)"), "");
    let _s = r.spawn("sbx-b3", "s1", false);
    let out = r.feed(&(access(t, 0xe3, "fs.make_reg", "/var/tmp/x") + &allocated(t, 0xe3, DAEMON_PID, EXE, "sbx-b3")));
    assert_eq!(out, vec![denial("s1", "sbx-b3", "create", "system", "/var/tmp/x", 1)]);
}

#[test]
fn audit_a_domain_joins_its_spawn_by_tag_pid_and_time() {
    let mut r = rig();
    let child = r.spawn("sbx-c1", "s1", false);
    child.set_pid(4242);
    let pty = r.spawn("sbx-c2", "s2", false);
    let other = r.spawn("sbx-c3", "s3", false);
    other.set_pid(5000);
    let t = T0 + 50;
    let pair = |d: u64, pid: u32, comm: &str| access(t, d, "fs.make_reg", "/var/tmp/x") + &allocated(t, d, pid, EXE, comm);
    // A bash child (its own pid) and a PTY spawn (the daemon's pid).
    let out = r.feed(&(pair(0xf1, 4242, "sbx-c1") + &pair(0xf2, DAEMON_PID, "sbx-c2")));
    assert_eq!(
        out,
        vec![denial("s1", "sbx-c1", "create", "system", "/var/tmp/x", 1), denial("s2", "sbx-c2", "create", "system", "/var/tmp/x", 1)]
    );
    // Neither the spawn's pid nor the daemon's; a comm that isn't a tag; a
    // tag this daemon never gave out.
    let out = r.feed(&(pair(0xf3, 6000, "sbx-c3") + &pair(0xf4, DAEMON_PID, "bash") + &pair(0xf5, DAEMON_PID, "sbx-zz")));
    assert_eq!(out, vec![(Level::Info, "sandbox.audit unattributed domains=3 records=6".to_string())]);
    // A domain made before the spawn was: a previous image's tag.
    r.clock.set(T0 + 10_000);
    let late = r.spawn("sbx-c4", "s4", false);
    let out = r.feed(&(access(T0 + 9_000, 0xf6, "fs.make_reg", "/var/tmp/x") + &allocated(T0 + 9_000, 0xf6, DAEMON_PID, EXE, "sbx-c4")));
    assert!(out.is_empty(), "counted, reported at most every 10 min: {out:?}");
    let out = r.at(T0 + REPORT_MS + 1);
    assert_eq!(out, vec![(Level::Info, "sandbox.audit unattributed domains=1 records=2".to_string())]);
    drop((pty, late));
}

#[test]
fn audit_a_domain_stamped_up_to_the_clock_slack_before_its_spawn_joins() {
    let mut r = rig();
    let at = T0 + 1_000;
    r.clock.set(at);
    let _a = r.spawn("sbx-k1", "s1", false);
    let _b = r.spawn("sbx-k2", "s2", false);
    // The kernel stamps its records from its coarse clock, up to a tick
    // behind the clock a spawn's time is read from.
    let t = at - CLOCK_SLACK_MS;
    let out = r.feed(&(access(t, 0x61, "fs.make_reg", "/var/tmp/x") + &allocated(t, 0x61, DAEMON_PID, EXE, "sbx-k1")));
    assert_eq!(out, vec![denial("s1", "sbx-k1", "create", "system", "/var/tmp/x", 1)]);
    // Older than that: another image's spawn with this tag.
    let t = at - CLOCK_SLACK_MS - 1;
    let out = r.feed(&(access(t, 0x62, "fs.make_reg", "/var/tmp/x") + &allocated(t, 0x62, DAEMON_PID, EXE, "sbx-k2")));
    assert_eq!(out, vec![(Level::Info, "sandbox.audit unattributed domains=1 records=2".to_string())]);
}

#[test]
fn audit_a_bash_childs_domain_waits_for_its_pid() {
    let mut r = rig();
    let child = r.spawn("sbx-w1", "s1", false);
    let t = T0 + 5;
    assert!(r.feed(&(access(t, 0x71, "fs.make_reg", "/var/tmp/x") + &allocated(t, 0x71, 4242, EXE, "sbx-w1"))).is_empty());
    child.set_pid(4242);
    assert_eq!(r.poll(), vec![denial("s1", "sbx-w1", "create", "system", "/var/tmp/x", 1)]);
    // One that never gets its pid is given up after a while.
    let _never = r.spawn("sbx-w2", "s1", false);
    assert!(r.feed(&(access(t, 0x72, "fs.make_reg", "/var/tmp/y") + &allocated(t, 0x72, 4343, EXE, "sbx-w2"))).is_empty());
    assert!(r.at(T0 + PENDING_MS - 1).is_empty());
    assert_eq!(r.at(T0 + PENDING_MS), vec![(Level::Info, "sandbox.audit unattributed domains=1 records=2".to_string())]);
}

#[test]
fn audit_a_denial_read_before_its_domain_record_waits_for_it() {
    let mut r = rig();
    let _s = r.spawn("sbx-p1", "s1", false);
    let t = T0 + 5;
    assert!(r.feed(&access(t, 0x81, "fs.make_reg", "/var/tmp/x")).is_empty());
    assert_eq!(r.feed(&allocated(t, 0x81, DAEMON_PID, EXE, "sbx-p1")), vec![denial("s1", "sbx-p1", "create", "system", "/var/tmp/x", 1)]);
    // A domain whose first denial came before the reader started never
    // brings its record: given up, counted.
    assert!(r.feed(&(access(t, 0x82, "fs.make_reg", "/var/tmp/y") + &ptrace(t, 0x82))).is_empty());
    assert!(r.at(T0 + PENDING_MS - 1).is_empty());
    assert_eq!(r.at(T0 + PENDING_MS), vec![(Level::Info, "sandbox.audit unattributed domains=1 records=2".to_string())]);
    // Its later records and its end are counted too, reported later.
    assert!(r.feed(&(access(t, 0x82, "fs.make_reg", "/var/tmp/y") + &dealloc(t, 0x82, 3))).is_empty());
    assert_eq!(r.at(T0 + PENDING_MS + REPORT_MS), vec![(Level::Info, "sandbox.audit unattributed domains=0 records=2".to_string())]);
    // The end of a domain never seen at all.
    assert!(r.feed(&dealloc(t, 0x83, 1)).is_empty());
    assert_eq!(r.at(T0 + PENDING_MS + 2 * REPORT_MS), vec![(Level::Info, "sandbox.audit unattributed domains=1 records=1".to_string())]);
}

#[test]
fn audit_repeats_fold_into_one_line_with_their_count() {
    let mut r = rig();
    let _s = r.spawn("sbx-d1", "s1", false);
    let t = T0 + 5;
    let x = r.h(".config/x");
    let first = r.feed(&(access(t, 0x91, "fs.make_reg", &x) + &allocated(t, 0x91, DAEMON_PID, EXE, "sbx-d1")));
    assert_eq!(first, vec![denial("s1", "sbx-d1", "create", "home_dot:.config", &x, 1)]);
    r.clock.set(T0 + 1_000);
    assert!(r.feed(&access(t, 0x91, "fs.make_reg", &x).repeat(4)).is_empty(), "folded");
    let other = r.feed(&access(t, 0x91, "fs.write_file", &x));
    assert_eq!(other, vec![denial("s1", "sbx-d1", "write", "home_dot:.config", &x, 1)], "another op is another line");
    assert!(r.at(T0 + DEDUP_MS - 1).is_empty());
    assert_eq!(r.at(T0 + DEDUP_MS), vec![denial("s1", "sbx-d1", "create", "home_dot:.config", &x, 4)]);
    assert!(r.at(T0 + 1_000 + DEDUP_MS).is_empty(), "the write had no repeats");
    let again = r.feed(&access(t, 0x91, "fs.make_reg", &x));
    assert_eq!(again, vec![denial("s1", "sbx-d1", "create", "home_dot:.config", &x, 1)], "a new window");
}

#[test]
fn audit_domain_end_carries_the_kernels_count_and_the_ptrace_count() {
    let mut r = rig();
    let _s = r.spawn("sbx-e1", "s1", false);
    let t = T0 + 5;
    let x = r.h("work");
    // The first denial is a ptrace one (a /proc walk): it brings the domain.
    let mut text = ptrace(t, 0xa1) + &allocated(t, 0xa1, DAEMON_PID, EXE, "sbx-e1");
    text += &access(t, 0xa1, "fs.make_reg", &x);
    text += &access(t, 0xa1, "fs.make_reg", &x);
    text += &access(t, 0xa1, "scope.signal", "/");
    assert_eq!(r.feed(&text), vec![denial("s1", "sbx-e1", "create", "home_other", &x, 1)]);
    let end = r.feed(&dealloc(t, 0xa1, 4));
    assert_eq!(
        end,
        vec![
            denial("s1", "sbx-e1", "create", "home_other", &x, 1),
            (Level::Warn, "sandbox.domain_end session=s1 tag=sbx-e1 denials=4 ptrace=2".to_string()),
        ],
        "the folded repeat first, then the end"
    );
    assert!(r.at(T0 + DEDUP_MS + 1).is_empty(), "nothing left to fold");
    // Only ptrace denials: no denial line, just the end. (A domain no older
    // than its spawn: the clock moved on above.)
    let _p = r.spawn("sbx-e2", "s2", false);
    let t = r.clock.now();
    let mut text = ptrace(t, 0xa2) + &allocated(t, 0xa2, DAEMON_PID, EXE, "sbx-e2");
    text += &ptrace(t, 0xa2);
    assert!(r.feed(&text).is_empty());
    assert_eq!(r.feed(&dealloc(t, 0xa2, 2)), vec![(Level::Warn, "sandbox.domain_end session=s2 tag=sbx-e2 denials=2 ptrace=2".to_string())]);
}

#[test]
fn audit_a_session_with_audit_log_off_gets_no_audit_lines() {
    let mut r = rig();
    let _q = r.spawn("sbx-q1", "s1", true);
    let t = T0 + 5;
    let mut text = access(t, 0xb1, "fs.make_reg", "/var/tmp/x") + &allocated(t, 0xb1, DAEMON_PID, EXE, "sbx-q1");
    text += &ptrace(t, 0xb1);
    text += &dealloc(t, 0xb1, 2);
    assert!(r.feed(&text).is_empty());
    assert!(r.at(T0 + DEDUP_MS + PENDING_MS + REPORT_MS).is_empty(), "nor counted as unattributed");
}

#[test]
fn audit_a_by_design_refusal_is_logged_with_its_class() {
    let mut r = rig();
    let _s = r.spawn("sbx-g1", "s1", false);
    let t = T0 + 5;
    let git = r.h("repo/.git");
    let out = r.feed(&(access(t, 0xc1, "fs.make_reg", &git) + &allocated(t, 0xc1, DAEMON_PID, EXE, "sbx-g1")));
    assert_eq!(out, vec![denial("s1", "sbx-g1", "create", "by_design:git_common", &git, 1)]);
}

#[test]
fn audit_reads_no_syscall_path_cwd_or_proctitle_record_and_no_enriched_field() {
    const SECRET: &str = "FAKE-SECRET-7f3a91";
    let hex: String = SECRET.bytes().map(|b| format!("{b:02X}")).collect();
    let mut r = rig();
    let _s = r.spawn("sbx-s1", "s1", false);
    let t = T0 + 5;
    let mut text = access(t, 0xd1, "fs.make_reg", "/var/tmp/x");
    text += &format!(
        "type=UNKNOWN[1424] msg=audit({}:1): domain=d1 status=allocated mode=enforcing pid={DAEMON_PID} uid=1000 exe=\"{EXE}\" comm=\"sbx-s1\"\x1dAUID=\"{SECRET}\" UID=\"{SECRET}\"\n",
        ts(t)
    );
    text += &format!("type=SYSCALL msg=audit({}:1): arch=c000003e syscall=257 success=no exit=-13 key=\"{SECRET}\" comm=\"sbx-s1\" exe=\"{EXE}\"\n", ts(t));
    text += &format!("type=CWD msg=audit({}:1): cwd=\"/var/tmp/{SECRET}\"\n", ts(t));
    text += &format!("type=PATH msg=audit({}:1): item=0 name=\"/var/tmp/{SECRET}\" nametype=PARENT\n", ts(t));
    text += &format!("type=PROCTITLE msg=audit({}:1): proctitle={hex}\n", ts(t));
    text += &dealloc(t, 0xd1, 1);
    let out = r.feed(&text);
    assert_eq!(out.len(), 2, "the denial and the end: {out:?}");
    for (_, line) in &out {
        assert!(!line.contains(SECRET) && !line.contains(&hex), "{line}");
    }
}

#[test]
fn audit_malformed_and_overlong_records_are_dropped_and_counted() {
    let mut r = rig();
    let _s = r.spawn("sbx-m1", "s1", false);
    let t = T0 + 5;
    let mut text = "type=UNKNOWN[1423] msg=audit(1.000:1): domain=zz blockers=fs.make_reg\n".to_string();
    text += "type=UNKNOWN[1424] msg=audit(1.000:1): domain=1 status=deallocated denials=x\n";
    text += &format!("type=UNKNOWN[1423] msg=audit(1.000:1): domain=1 blockers=fs.make_reg path=\"/{}\"\n", "a".repeat(MAX_LINE));
    text += &(access(t, 0xe1, "fs.make_reg", "/var/tmp/x") + &allocated(t, 0xe1, DAEMON_PID, EXE, "sbx-m1"));
    let out = r.feed(&text);
    assert_eq!(
        out,
        vec![
            denial("s1", "sbx-m1", "create", "system", "/var/tmp/x", 1),
            (Level::Info, "sandbox.audit dropped malformed=2 long=1".to_string()),
        ]
    );
    assert!(r.feed("type=UNKNOWN[1423] msg=audit(1.000:1): domain=zz\n").is_empty(), "reported at most every 10 min");
    assert_eq!(r.at(T0 + REPORT_MS), vec![(Level::Info, "sandbox.audit dropped malformed=1 long=0".to_string())]);
}

#[test]
fn audit_at_the_fold_cap_the_oldest_window_closes_first() {
    let mut r = rig();
    // The line budget would stop the lines long before the cap.
    r.tail.set_budget(1_000_000, 1_000_000);
    let _s = r.spawn("sbx-f1", "s1", false);
    let t = T0 + 5;
    let p = |i: usize| format!("/var/tmp/f{i}");
    let mut text = access(t, 0x41, "fs.make_reg", &p(0)) + &allocated(t, 0x41, DAEMON_PID, EXE, "sbx-f1");
    text += &access(t, 0x41, "fs.make_reg", &p(0));
    for i in 1..DEDUP_CAP {
        text += &access(t, 0x41, "fs.make_reg", &p(i));
    }
    assert_eq!(r.feed(&text).len(), DEDUP_CAP, "one line per window, the repeat folded");
    // One window more: the oldest (with its repeat) closes to make room.
    let out = r.feed(&access(t, 0x41, "fs.make_reg", &p(DEDUP_CAP)));
    assert_eq!(
        out,
        vec![denial("s1", "sbx-f1", "create", "system", &p(0), 1), denial("s1", "sbx-f1", "create", "system", &p(DEDUP_CAP), 1)]
    );
}

#[test]
fn audit_at_the_pending_cap_the_oldest_waiting_domain_is_given_up() {
    let mut r = rig();
    let _s = r.spawn("sbx-n1", "s1", false);
    let t = T0 + 5;
    // Records whose domain record hasn't come yet: one of 0x31, then 0x32's.
    let mut text = access(t, 0x31, "fs.make_reg", "/var/tmp/x");
    text += &access(t, 0x32, "fs.make_reg", "/var/tmp/y").repeat(PENDING_CAP - 1);
    assert!(r.feed(&text).is_empty(), "held, up to the cap");
    let out = r.feed(&access(t, 0x33, "fs.make_reg", "/var/tmp/z"));
    assert_eq!(out, vec![(Level::Info, "sandbox.audit unattributed domains=1 records=1".to_string())], "the oldest domain makes room");
    let out = r.feed(&allocated(t, 0x32, DAEMON_PID, EXE, "sbx-n1"));
    assert_eq!(out, vec![denial("s1", "sbx-n1", "create", "system", "/var/tmp/y", 1)], "the others still wait for theirs");
}

#[test]
fn audit_at_the_domain_cap_the_oldest_domain_is_forgotten() {
    let mut r = rig();
    let t = T0 + 5;
    // Another program's domains: remembered (and skipped), up to the cap.
    let mut text = String::new();
    for d in 0..=DOMAIN_CAP as u64 {
        text += &allocated(t, 0x1000 + d, 7, "/usr/bin/other", "bash");
    }
    assert!(r.feed(&text).is_empty());
    // The oldest is forgotten: its next record waits, then is counted. The
    // newest is still known and skipped.
    let newest = 0x1000 + DOMAIN_CAP as u64;
    assert!(r.feed(&(access(t, 0x1000, "fs.make_reg", "/var/tmp/x") + &access(t, newest, "fs.make_reg", "/var/tmp/x"))).is_empty());
    assert_eq!(r.at(T0 + PENDING_MS), vec![(Level::Info, "sandbox.audit unattributed domains=1 records=1".to_string())]);
}

#[test]
fn audit_registry_keeps_an_exited_spawn_15_minutes_and_holds_at_most_its_cap() {
    let clock = FakeClock::new(T0);
    let reg = Registry::new(clock.clock());
    let home = PathBuf::from("/h");
    let a = reg.register(&Tag::new("sbx-r1").unwrap(), info("s1", false, &home));
    let a2 = a.clone();
    drop(a);
    assert_eq!(reg.entry("sbx-r1").map(|e| e.exited), Some(false), "a clone still holds it");
    drop(a2);
    assert_eq!(reg.entry("sbx-r1").map(|e| e.exited), Some(true));
    clock.set(T0 + KEEP_AFTER_EXIT_MS - 1);
    let _b = reg.register(&Tag::new("sbx-r2").unwrap(), info("s1", false, &home));
    assert!(reg.entry("sbx-r1").is_some(), "kept for 15 min after its exit");
    clock.set(T0 + KEEP_AFTER_EXIT_MS);
    let _c = reg.register(&Tag::new("sbx-r3").unwrap(), info("s1", false, &home));
    assert!(reg.entry("sbx-r1").is_none(), "then dropped");
    assert_eq!(reg.len(), 2);
    // At the cap the oldest exited spawn goes first, then the oldest live one.
    let mut held = Vec::new();
    for n in 0..(REGISTRY_CAP as u64 - 2) {
        clock.set(T0 + KEEP_AFTER_EXIT_MS + 1 + n);
        held.push(reg.register(&tag_for(1_000_000 + n), info("s1", false, &home)));
    }
    assert_eq!(reg.len(), REGISTRY_CAP);
    drop(held.remove(10));
    let gone = tag_for(1_000_010).to_string();
    let _d = reg.register(&Tag::new("sbx-r4").unwrap(), info("s1", false, &home));
    assert_eq!(reg.len(), REGISTRY_CAP);
    assert!(reg.entry(&gone).is_none(), "the exited one made room");
    assert!(reg.entry("sbx-r2").is_some());
    let _e = reg.register(&Tag::new("sbx-r5").unwrap(), info("s1", false, &home));
    assert_eq!(reg.len(), REGISTRY_CAP);
    assert!(reg.entry("sbx-r2").is_none(), "then the oldest live one");
}

// ── Lines: escaping, classes ─────────────────────────────────────────────

/// `bytes` as auditd's bare hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

/// [`access`] with the path in auditd's hex form (any bytes).
fn access_hex(ms: u64, domain: u64, blockers: &str, path: &[u8]) -> String {
    format!(
        "type=UNKNOWN[1423] msg=audit({}:{}): domain={domain:x} blockers={blockers} path={} dev=\"dm-1\" ino=42\n",
        ts(ms),
        ms % 100_000,
        hex(path)
    )
}

/// A line's field keys, in order, after its event name.
fn keys(line: &str) -> Vec<String> {
    line.split(' ').skip(1).map(|kv| kv.split('=').next().unwrap_or("").to_string()).collect()
}

/// Whether `line` holds only visible ASCII and spaces.
fn printable(line: &str) -> bool {
    line.bytes().all(|b| (0x20..0x7f).contains(&b))
}

#[test]
fn audit_a_hostile_path_or_session_gives_one_line_with_no_controls_and_no_extra_fields() {
    let mut r = rig();
    let _s = r.spawn("sbx-i1", "s1", false);
    let t = T0 + 5;
    let home = r.home().display().to_string();
    // auditd writes such a path as hex; its bytes reach the line only escaped.
    let hostile = format!("{home}/.a\nb\x1b[31m session=x tag=sbx-forged/f");
    let out = r.feed(&(access_hex(t, 0x11, "fs.write_file", hostile.as_bytes()) + &allocated(t, 0x11, DAEMON_PID, EXE, "sbx-i1")));
    let want = format!("\"{home}/.a\\x0Ab\\x1B[31m\\x20session=x\\x20tag=sbx-forged/f\"");
    assert_eq!(out, vec![denial("s1", "sbx-i1", "write", "home_dot:unprintable", &want, 1)]);
    assert!(printable(&out[0].1), "{:?}", out[0].1);
    assert_eq!(keys(&out[0].1), ["session", "tool", "tag", "op", "class", "path", "n", "src"], "{}", out[0].1);
    // A session id is escaped the same way, on the denial line and the end line.
    let _h = r.spawn("sbx-i2", "s\n2 tag=x", false);
    let text = access(t, 0x12, "fs.make_reg", "/var/tmp/x") + &allocated(t, 0x12, DAEMON_PID, EXE, "sbx-i2") + &dealloc(t, 0x12, 1);
    let out = r.feed(&text);
    let session = "\"s\\x0A2\\x20tag=x\"";
    assert_eq!(
        out,
        vec![
            denial(session, "sbx-i2", "create", "system", "/var/tmp/x", 1),
            (Level::Warn, format!("sandbox.domain_end session={session} tag=sbx-i2 denials=1 ptrace=0")),
        ]
    );
    for (_, line) in &out {
        assert!(printable(line), "{line:?}");
    }
    assert_eq!(keys(&out[1].1), ["session", "tag", "denials", "ptrace"]);
}

#[test]
fn audit_a_create_in_local_state_is_uncertain_not_by_design() {
    let mut r = rig();
    let _s = r.spawn("sbx-l1", "s1", false);
    let t = T0 + 5;
    let (state, lesshst) = (r.h(".local/state"), r.h(".local/state/lesshst"));
    // A create names only the parent: it may be any program's file.
    let mut text = access(t, 0x13, "fs.make_reg", &state) + &allocated(t, 0x13, DAEMON_PID, EXE, "sbx-l1");
    text += &access(t, 0x13, "fs.write_file", &lesshst);
    assert_eq!(
        r.feed(&text),
        vec![
            denial("s1", "sbx-l1", "create", "uncertain:local_state", &state, 1),
            denial("s1", "sbx-l1", "write", "by_design:less_history", &lesshst, 1),
        ]
    );
}

// ── Records: the strict tokenizer, the caps ──────────────────────────────

#[test]
fn audit_parse_refuses_a_bad_token_or_a_repeated_key_as_a_whole() {
    let head = "type=LANDLOCK_ACCESS msg=audit(1.000:1):";
    for rest in [
        // The review's two: a quoted value that doesn't end can't promote the
        // fields inside it, and a key may appear once.
        "junk=\"x domain=1 blockers=fs.write_file path=\"/victim\"",
        "domain=1 domain=2 blockers=fs.write_file path=\"/x\"",
        "domain=1 blockers=fs.write_file path=\"/x\" path=\"/y\"",
        "domain=1 blockers=fs.write_file blockers=fs.make_reg path=\"/x\"",
        "domain=1 blockers=fs.write_file path=\"/x\" msg=audit(2.000:2):",
        // A quoted value holds visible ASCII other than `"` (auditd hex-encodes the rest).
        "domain=1 blockers=fs.write_file path=\"/a\"b\"",
        "domain=1 blockers=fs.write_file path=\"/a\0b\"",
        "domain=1 blockers=fs.write_file path=\"/x\" dev=\"\x7f\"",
        "domain=1 blockers=fs.write_file path=\"/x\" dev=\"",
        // A bare value has no quote; every token is key=value with a key of [a-z0-9_].
        "domain=1 blockers=fs.write_file path=\"/x\" dev=d\"m",
        "domain=1 blockers=fs.write_file path=\"/x\" ino",
        "domain=1 blockers=fs.write_file path=\"/x\" =1",
        "domain=1 blockers=fs.write_file path=\"/x\" Dev=\"dm-1\"",
        "domain=1 blockers=fs.write_file path=\"/x\" \"dev\"=1",
    ] {
        let line = format!("{head} {rest}");
        assert_eq!(parse(line.as_bytes()), Parsed::Malformed, "{line:?}");
    }
    // Well formed, with a doubled space: read.
    let ok = format!("{head} domain=1  blockers=fs.write_file path=\"/x\" dev=\"dm-1\" ino=42");
    let want = Record { time_ms: 1000, domain: 1, body: Body::Access { blockers: vec!["fs.write_file".into()], path: Some(b"/x".to_vec()) } };
    assert_eq!(parse(ok.as_bytes()), Parsed::Record(want));
    let alloc = "type=UNKNOWN[1424] msg=audit(1.000:1): domain=1 status=allocated mode=enforcing pid=7 uid=1000";
    for rest in ["exe=\"/x\" comm=\"sbx-1\" comm=\"sbx-2\"", "exe=\"/x comm=\"sbx-1\"", "exe=\"/x\" pid=8 comm=\"sbx-1\""] {
        let line = format!("{alloc} {rest}");
        assert_eq!(parse(line.as_bytes()), Parsed::Malformed, "{line:?}");
    }
    // At most MAX_FIELDS fields after the stamp.
    let fill = |n: usize| (0..n).map(|i| format!(" f{i}=1")).collect::<String>();
    let at_cap = format!("{head} domain=1 blockers=fs.write_file path=\"/x\"{}", fill(MAX_FIELDS - 3));
    assert!(matches!(parse(at_cap.as_bytes()), Parsed::Record(_)), "{at_cap}");
    let over = format!("{head} domain=1 blockers=fs.write_file path=\"/x\"{}", fill(MAX_FIELDS - 2));
    assert_eq!(parse(over.as_bytes()), Parsed::Malformed);
}

#[test]
fn audit_parse_caps_the_blockers_and_reads_a_non_fs_refusal_without_its_fields() {
    let head = "type=UNKNOWN[1423] msg=audit(1.000:1): domain=1";
    let list = |n: usize| (0..n).map(|i| format!("fs.x{i}")).collect::<Vec<_>>();
    let ok = format!("{head} blockers={} path=\"/x\"", list(MAX_BLOCKERS).join(","));
    match parse(ok.as_bytes()) {
        Parsed::Record(Record { body: Body::Access { blockers, .. }, .. }) => assert_eq!(blockers, list(MAX_BLOCKERS)),
        other => panic!("{other:?}"),
    }
    let over = format!("{head} blockers={} path=\"/x\"", list(MAX_BLOCKERS + 1).join(","));
    assert_eq!(parse(over.as_bytes()), Parsed::Malformed);
    // A ptrace or scope refusal is only counted: none of its fields is kept.
    for rest in ["blockers=ptrace opid=3226 ocomm=\"chrome\"", "blockers=scope.signal opid=1 ocomm=\"x\"", "blockers=scope.abstract_unix_socket path=\"/x\""] {
        let line = format!("{head} {rest}");
        assert_eq!(parse(line.as_bytes()), Parsed::Record(Record { time_ms: 1000, domain: 1, body: Body::NonFs }), "{line}");
    }
    // Repeated blockers name each op once, in order.
    let b: Vec<String> = "fs.x,fs.write_file,fs.x,fs.write_file,fs.truncate".split(',').map(String::from).collect();
    assert_eq!(op_of(&b), "x+write+truncate");
}

#[test]
fn audit_no_other_record_type_or_field_reaches_a_line() {
    const SECRET: &str = "FAKE-SECRET-41c2d8";
    let sx = hex(SECRET.as_bytes());
    let mut r = rig();
    let _s = r.spawn("sbx-s2", "s1", false);
    let t = T0 + 5;
    let (x, y) = ("/var/tmp/x", "/var/tmp/y");
    let mut text = access(t, 0xe1, "fs.write_file", x) + &allocated(t, 0xe1, DAEMON_PID, EXE, "sbx-s2");
    // Other record types: a command line, sudo's records.
    text += &format!("type=EXECVE msg=audit({}:2): argc=2 a0=\"cat\" a1=\"{SECRET}\"\n", ts(t));
    for ty in ["USER_CMD", "USER_AUTH", "USER_START"] {
        text += &format!(
            "type={ty} msg=audit({}:3): pid=1 uid=1000 auid=1000 ses=1 msg='op=x acct=\"{SECRET}\" cmd={sx} exe=\"/usr/bin/sudo\" res=success'\n",
            ts(t)
        );
    }
    // Malformed Landlock records of a known domain: dropped whole.
    text += &format!("type=UNKNOWN[1423] msg=audit({}:4): junk=\"x domain=e1 blockers=fs.write_file path=\"/var/tmp/{SECRET}\"\n", ts(t));
    text += &format!("type=UNKNOWN[1423] msg=audit({}:5): domain=e1 blockers=fs.write_file path=\"/var/tmp/{SECRET}\" path=\"{y}\"\n", ts(t));
    // Raw fields other than the path, and an access record's interpreted part.
    text += &format!(
        "type=UNKNOWN[1423] msg=audit({}:6): domain=e1 blockers=fs.write_file path=\"{y}\" dev=\"{SECRET}\" ino={sx}\x1dPATH=\"/var/tmp/{SECRET}\" OCOMM=\"{SECRET}\"\n",
        ts(t)
    );
    text += &format!("type=UNKNOWN[1423] msg=audit({}:7): domain=e1 blockers=ptrace opid=1 ocomm=\"{SECRET}\"\n", ts(t));
    text += &format!("type=UNKNOWN[1423] msg=audit({}:8): domain=e1 blockers=scope.abstract_unix_socket path=\"/var/tmp/{SECRET}\"\n", ts(t));
    text += &dealloc(t, 0xe1, 5);
    let out = r.feed(&text);
    for (_, line) in &out {
        assert!(!line.contains(SECRET) && !line.contains(&sx), "{line}");
    }
    assert_eq!(
        out,
        vec![
            denial("s1", "sbx-s2", "write", "system", x, 1),
            denial("s1", "sbx-s2", "write", "system", y, 1),
            (Level::Warn, "sandbox.domain_end session=s1 tag=sbx-s2 denials=5 ptrace=2".to_string()),
            (Level::Info, "sandbox.audit dropped malformed=2 long=0".to_string()),
        ]
    );
}

// ── Following the file: the reopen, the per-poll bound, the name ─────────

#[test]
fn audit_follower_keeps_its_place_when_the_reopen_finds_the_same_file() {
    let (_tmp, root) = scratch();
    let (log, old) = (root.join("audit.log"), root.join("audit.log.1"));
    append(&log, "a\n");
    // The seam opens whichever file `target` names: the name changed again
    // between the follower's look at it and its open.
    let target = Arc::new(Mutex::new(log.clone()));
    let t = Arc::clone(&target);
    let open: Opener = Arc::new(move |_: &Path| {
        let p = lock(&t).clone();
        open_file(&p)
    });
    let mut f = Follower::open_at_end_with(&log, open).expect("open");
    append(&log, "b\n");
    assert_eq!(read_bounded(&mut f), vec!["b"]);
    fs::rename(&log, &old).unwrap();
    append(&log, "new\n");
    append(&old, "c\n");
    *lock(&target) = old.clone();
    assert_eq!(read_bounded(&mut f), vec!["c"], "the reopen found the file it holds: its place is kept, nothing read twice");
    append(&old, "d\n");
    *lock(&target) = log.clone();
    assert_eq!(read_bounded(&mut f), vec!["d", "new"]);
}

#[test]
fn audit_one_poll_reads_at_most_its_budget_and_stops_between_reads() {
    let mut r = rig();
    let kib = format!("{}\n", "x".repeat(1023));
    append(&r.log, &kib.repeat(2 * POLL_BYTES / 1024));
    assert!(r.poll().is_empty());
    assert_eq!(r.f.offset(), POLL_BYTES as u64, "one poll reads at most POLL_BYTES");
    assert!(r.poll_until(&|| true).is_empty());
    assert_eq!(r.f.offset(), (POLL_BYTES + CHUNK) as u64, "asked to stop, it stops after its current read");
    // Short lines: it stops after the read that passes POLL_LINES.
    const _: () = assert!(CHUNK / 10 < POLL_LINES && 2 * CHUNK / 10 >= POLL_LINES);
    let mut r = rig();
    append(&r.log, &"xxxxxxxxx\n".repeat(4 * CHUNK / 10));
    assert!(r.poll().is_empty());
    assert_eq!(r.f.offset(), 2 * CHUNK as u64);
}

#[test]
fn audit_a_missing_log_name_is_said_after_a_grace_and_the_old_file_still_drains() {
    let mut r = rig();
    let _s = r.spawn("sbx-y1", "s1", false);
    let t = T0 + 5;
    let old = r.root.join("audit.log.1");
    fs::rename(&r.log, &old).unwrap();
    append(&old, &(access(t, 0x77, "fs.make_reg", "/var/tmp/x") + &allocated(t, 0x77, DAEMON_PID, EXE, "sbx-y1")));
    assert_eq!(r.poll(), vec![denial("s1", "sbx-y1", "create", "system", "/var/tmp/x", 1)], "the old file still drains");
    assert!(r.at(T0 + NAME_GRACE_MS - 1).is_empty(), "a rotation's moment says nothing");
    assert_eq!(r.at(T0 + NAME_GRACE_MS), vec![(Level::Warn, "sandbox.audit unavailable reason=ENOENT".to_string())]);
    assert!(r.at(T0 + NAME_GRACE_MS + 1_000).is_empty(), "said once");
    let log = r.log.clone();
    append(&log, &(access(t, 0x78, "fs.make_reg", "/var/tmp/y") + &allocated(t, 0x78, DAEMON_PID, EXE, "sbx-y1")));
    assert_eq!(
        r.poll(),
        vec![denial("s1", "sbx-y1", "create", "system", "/var/tmp/y", 1), (Level::Warn, "sandbox.audit available".to_string())]
    );
}

#[test]
fn audit_a_symlink_or_a_directory_at_the_log_name_is_not_a_file() {
    let (_tmp, root) = scratch();
    let real = root.join("real.log");
    append(&real, "a\n");
    let link = root.join("link.log");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let e = Follower::open_at_end(&link).err().expect("a symlink is refused");
    assert_eq!(reason(&e), "not-a-file");
    // A symlink put in the log's place is not followed.
    let mut r = rig();
    let _s = r.spawn("sbx-y2", "s1", false);
    let t = T0 + 5;
    let target = r.root.join("elsewhere.log");
    append(&target, &(access(t, 0x79, "fs.make_reg", "/var/tmp/target") + &allocated(t, 0x79, DAEMON_PID, EXE, "sbx-y2")));
    fs::rename(&r.log, r.root.join("audit.log.1")).unwrap();
    std::os::unix::fs::symlink(&target, &r.log).unwrap();
    assert!(r.poll().is_empty(), "its target is not read");
    assert_eq!(r.at(T0 + NAME_GRACE_MS), vec![(Level::Warn, "sandbox.audit unavailable reason=not-a-file".to_string())]);
    // Then a directory there: still down, said once.
    fs::remove_file(&r.log).unwrap();
    fs::create_dir(&r.log).unwrap();
    assert!(r.at(T0 + NAME_GRACE_MS + 1_000).is_empty());
}

// ── The join: the pending cap, the clocks, the keep window, the exe ──────

#[test]
fn audit_allocation_records_waiting_for_their_pid_count_against_the_pending_cap() {
    let mut r = rig();
    let child = r.spawn("sbx-n3", "s1", false);
    let t = T0 + 5;
    // PENDING_CAP + 1 domains of one spawn whose child's pid isn't known yet.
    let mut text = String::new();
    for i in 0..=PENDING_CAP as u64 {
        text += &allocated(t, 0x2000 + i, 7000 + i as u32, EXE, "sbx-n3");
    }
    let out = r.feed(&text);
    assert_eq!(out, vec![(Level::Info, "sandbox.audit unattributed domains=1 records=1".to_string())], "the oldest makes room");
    // The newest still waits: its pid comes and its denial joins; the
    // others are given up.
    child.set_pid(7000 + PENDING_CAP as u32);
    assert!(r.poll().is_empty());
    let out = r.feed(&access(t, 0x2000 + PENDING_CAP as u64, "fs.make_reg", "/var/tmp/x"));
    assert_eq!(out, vec![denial("s1", "sbx-n3", "create", "system", "/var/tmp/x", 1)]);
    let gone = PENDING_CAP - 1;
    assert_eq!(r.at(T0 + REPORT_MS), vec![(Level::Info, format!("sandbox.audit unattributed domains={gone} records={gone}"))]);
}

#[test]
fn audit_a_full_queue_then_a_waiting_domain_record_gives_up_the_oldest() {
    let mut r = rig();
    let _s = r.spawn("sbx-n4", "s1", false);
    let t = T0 + 5;
    assert!(r.feed(&access(t, 0x31, "fs.make_reg", "/var/tmp/x").repeat(PENDING_CAP)).is_empty(), "held, up to the cap");
    let out = r.feed(&allocated(t, 0x40, 7000, EXE, "sbx-n4"));
    assert_eq!(out, vec![(Level::Info, format!("sandbox.audit unattributed domains=1 records={PENDING_CAP}"))]);
}

#[test]
fn audit_the_keep_window_runs_on_the_monotonic_clock() {
    let mut r = rig();
    drop(r.spawn("sbx-k3", "s1", false));
    // The wall clock jumps a day ahead; a minute passes.
    let day = 24 * 3_600_000;
    r.clock.unix.set(T0 + day);
    r.clock.mono.set(T0 + 60_000);
    let _other = r.spawn("sbx-k4", "s2", false);
    let t = T0 + day;
    let out = r.feed(&(access(t, 0x73, "fs.make_reg", "/var/tmp/x") + &allocated(t, 0x73, DAEMON_PID, EXE, "sbx-k3")));
    assert_eq!(out, vec![denial("s1", "sbx-k3", "create", "system", "/var/tmp/x", 1)], "a minute after its exit, it still joins");
}

#[test]
fn audit_an_exited_spawn_stays_joinable_15_minutes_with_no_registration_since() {
    let mut r = rig();
    drop(r.spawn("sbx-k5", "s1", false));
    let t = T0 + KEEP_AFTER_EXIT_MS - 1;
    r.clock.set(t);
    let out = r.feed(&(access(t, 0x74, "fs.make_reg", "/var/tmp/x") + &allocated(t, 0x74, DAEMON_PID, EXE, "sbx-k5")));
    assert_eq!(out, vec![denial("s1", "sbx-k5", "create", "system", "/var/tmp/x", 1)]);
    let t = T0 + KEEP_AFTER_EXIT_MS;
    r.clock.set(t);
    let out = r.feed(&(access(t, 0x75, "fs.make_reg", "/var/tmp/y") + &allocated(t, 0x75, DAEMON_PID, EXE, "sbx-k5")));
    assert_eq!(
        out,
        vec![(Level::Info, "sandbox.audit unattributed domains=1 records=2".to_string())],
        "15 min after its exit it is gone, with no registration since to drop it"
    );
}

#[test]
fn audit_with_no_exe_to_filter_by_no_domain_is_attributed() {
    let mut r = rig_tail(None, "");
    let _s = r.spawn("sbx-x1", "s1", false);
    let t = T0 + 5;
    let out = r.feed(&(access(t, 0x76, "fs.make_reg", "/var/tmp/x") + &allocated(t, 0x76, DAEMON_PID, EXE, "sbx-x1")));
    assert_eq!(out, vec![(Level::Info, "sandbox.audit unattributed domains=1 records=2".to_string())]);
}

#[test]
fn audit_only_the_daemon_process_has_a_reader() {
    // No test calls enable_daemon: this binary is not the daemon.
    assert!(daemon_reader().is_none(), "a CLI or test process has no audit reader");
    let b = SandboxBinding::for_session(Some("audit-no-reader-1")).expect("a binding");
    assert!(!b.has_audit_reader(), "its enforcing spawns open no audit log and start no thread");
}

// ── The output budget ────────────────────────────────────────────────────

/// The value of `key=` in `line`, as a number.
fn num(line: &str, key: &str) -> u64 {
    let pre = format!("{key}=");
    line.split(' ').find_map(|f| f.strip_prefix(pre.as_str())).and_then(|v| v.parse().ok()).unwrap_or_else(|| panic!("no {key} in {line}"))
}

/// The refusals `out` accounts for: the sum of its denial lines' n, and its
/// suppressed count.
fn tally(out: &[(Level, String)]) -> (u64, u64) {
    let n = out.iter().filter(|(_, l)| l.starts_with("sandbox.denial ")).map(|(_, l)| num(l, "n")).sum();
    let s = out.iter().filter(|(_, l)| l.starts_with("sandbox.audit suppressed ")).map(|(_, l)| num(l, "lines")).sum();
    (n, s)
}

#[test]
fn audit_a_storm_of_distinct_paths_is_held_to_the_session_budget() {
    let mut r = rig();
    let _s = r.spawn("sbx-u1", "s1", false);
    let t = T0 + 5;
    let p = |i: usize| format!("/v/u{i}");
    // Twice the fold cap and one: folding can't hold these.
    let total = 2 * (DEDUP_CAP + 1);
    let mut out = r.feed(&(access(t, 0x51, "fs.make_reg", &p(0)) + &allocated(t, 0x51, DAEMON_PID, EXE, "sbx-u1")));
    let mut text = String::new();
    for i in 1..total {
        text += &access(t, 0x51, "fs.make_reg", &p(i));
        if i == DEDUP_CAP {
            out.extend(r.feed(&text));
            text.clear();
        }
    }
    out.extend(r.feed(&text));
    let lines = out.iter().filter(|(_, l)| l.starts_with("sandbox.denial ")).count();
    assert_eq!(lines, SESSION_LINES_PER_MIN as usize, "a minute's budget of lines");
    out.extend(r.at(T0 + REPORT_MS));
    assert!(out.iter().any(|(l, s)| *l == Level::Info && s.starts_with("sandbox.audit suppressed lines=")), "{:?}", &out[lines..]);
    assert_eq!(tally(&out), (SESSION_LINES_PER_MIN, total as u64 - SESSION_LINES_PER_MIN), "every refusal is in a line's n or the suppressed count");
}

#[test]
fn audit_all_sessions_together_are_held_to_the_host_budget() {
    let mut r = rig();
    let t = T0 + 5;
    let sessions = (HOST_LINES_PER_MIN / SESSION_LINES_PER_MIN) as usize + 2;
    let per = 100;
    let mut held = Vec::new();
    let mut text = String::new();
    for s in 0..sessions {
        let tag = format!("sbx-v{s}");
        held.push(r.spawn(&tag, &format!("s{s}"), false));
        let d = 0x60 + s as u64;
        text += &(access(t, d, "fs.make_reg", "/v/w0") + &allocated(t, d, DAEMON_PID, EXE, &tag));
        for i in 1..per {
            text += &access(t, d, "fs.make_reg", &format!("/v/w{i}"));
        }
    }
    let out = r.feed(&text);
    let lines = out.iter().filter(|(_, l)| l.starts_with("sandbox.denial ")).count();
    assert_eq!(lines, HOST_LINES_PER_MIN as usize);
    assert_eq!(tally(&out), (HOST_LINES_PER_MIN, (sessions * per) as u64 - HOST_LINES_PER_MIN));
}

// ── The reader: its one synchronous step, its thread ─────────────────────

/// A sink the reader thread writes to.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<(Level, String)>>>);

impl Lines {
    fn sink(&self) -> Sink {
        let v = Arc::clone(&self.0);
        Arc::new(move |l: Level, s: &str| v.lock().unwrap_or_else(|p| p.into_inner()).push((l, s.to_string())))
    }
    fn all(&self) -> Vec<(Level, String)> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    /// Whether a line containing `needle` arrives within `secs`.
    fn wait_for(&self, needle: &str, secs: u64) -> bool {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if self.all().iter().any(|(_, s)| s.contains(needle)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }
}

/// A reader of the scratch log `path` (unix time from `clock`, gates on
/// `mono`).
fn reader(path: &Path, clock: Clock, mono: Clock, poll: Duration, lines: &Lines) -> Arc<Reader> {
    Reader::new(Config {
        path: path.to_path_buf(),
        exe: Some(EXE.as_bytes().to_vec()),
        pid: DAEMON_PID,
        poll,
        clock,
        mono,
        lines_per_min: (SESSION_LINES_PER_MIN, HOST_LINES_PER_MIN),
        sink: lines.sink(),
    })
}

#[test]
fn audit_an_unreadable_log_is_said_once_and_retried_at_most_every_ten_minutes() {
    let (_tmp, root) = scratch();
    let path = root.join("audit.log");
    let clock = FakeClock::new(T0);
    let lines = Lines::default();
    let r = reader(&path, clock.clock(), clock.clock(), Duration::from_millis(20), &lines);
    let mut log = Vec::new();
    r.before_spawn(&mut |l| log.push(l));
    r.before_spawn(&mut |l| log.push(l));
    let down = "sandbox.audit unavailable reason=ENOENT".to_string();
    assert_eq!(log, vec![down.clone()], "said once, not per spawn");
    clock.set(T0 + RETRY_MS);
    r.before_spawn(&mut |l| log.push(l));
    assert_eq!(log, vec![down.clone()], "a failed retry says nothing");
    append(&path, "");
    clock.set(T0 + 2 * RETRY_MS - 1);
    r.before_spawn(&mut |l| log.push(l));
    assert_eq!(log, vec![down.clone()], "no retry within 10 min of the last");
    clock.set(T0 + 2 * RETRY_MS);
    r.before_spawn(&mut |l| log.push(l));
    assert_eq!(log, vec![down, "sandbox.audit available".to_string()]);
    r.before_spawn(&mut |l| log.push(l));
    assert_eq!(log.len(), 2, "nothing more once it's up: {log:?}");
    // The healed reader follows the log.
    let _s = r.registry().register(&Tag::new("sbx-h1").unwrap(), info("s1", false, &root.join("home")));
    let t = clock.now();
    append(&path, &(access(t, 0xa1, "fs.make_reg", "/var/tmp/x") + &allocated(t, 0xa1, DAEMON_PID, EXE, "sbx-h1")));
    assert!(lines.wait_for("sandbox.denial session=s1 tool=bash tag=sbx-h1 op=create class=system path=/var/tmp/x n=1 src=audit", 10), "{:?}", lines.all());
    r.stop();
    r.join();
}

#[test]
fn audit_the_reader_thread_stops_within_one_poll_interval() {
    let (_tmp, root) = scratch();
    let path = root.join("audit.log");
    append(&path, "");
    let lines = Lines::default();
    let r = reader(&path, Arc::new(now_ms), Arc::new(mono_ms), Duration::from_secs(3), &lines);
    let _s = r.registry().register(&Tag::new("sbx-st1").unwrap(), info("s1", false, &root.join("home")));
    let mut log = Vec::new();
    r.before_spawn(&mut |l| log.push(l));
    assert!(log.is_empty(), "{log:?}");
    let t = now_ms();
    append(&path, &(access(t, 0xb1, "fs.make_reg", "/var/tmp/x") + &allocated(t, 0xb1, DAEMON_PID, EXE, "sbx-st1")));
    assert!(lines.wait_for("tag=sbx-st1", 10), "the thread runs: {:?}", lines.all());
    let started = Instant::now();
    r.stop();
    r.join();
    assert!(started.elapsed() < Duration::from_secs(2), "stopped in {:?}, within one 3 s poll", started.elapsed());
    let n = lines.all().len();
    append(&path, &dealloc(now_ms(), 0xb1, 1));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(lines.all().len(), n, "a stopped reader reads nothing more: {:?}", lines.all());
}

#[test]
fn audit_stop_during_a_continuous_writer_ends_within_two_polls() {
    let (_tmp, root) = scratch();
    let path = root.join("audit.log");
    append(&path, "");
    let poll = Duration::from_millis(250);
    let (reading, sunk) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicU64::new(0)));
    let (written, done) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)));
    // A slow sink (200 us a line, until the test has its answer) that says
    // when the thread is reading a backlog (its 100th line). The line budgets
    // are lifted, so every refusal reaches it: one 64 KiB read takes about a
    // tenth of a second, a whole poll's bound of lines seconds.
    let sink: Sink = {
        let (reading, sunk, done) = (Arc::clone(&reading), Arc::clone(&sunk), Arc::clone(&done));
        Arc::new(move |_: Level, _: &str| {
            if sunk.fetch_add(1, Ordering::SeqCst) + 1 >= 100 {
                reading.store(true, Ordering::SeqCst);
            }
            if !done.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_micros(200));
            }
        })
    };
    let r = Reader::new(Config {
        path: path.clone(),
        exe: Some(EXE.as_bytes().to_vec()),
        pid: DAEMON_PID,
        poll,
        clock: Arc::new(now_ms),
        mono: Arc::new(mono_ms),
        lines_per_min: (1 << 30, 1 << 30),
        sink,
    });
    let _s = r.registry().register(&Tag::new("sbx-st2").unwrap(), info("s1", false, &root.join("home")));
    let mut log = Vec::new();
    r.before_spawn(&mut |l| log.push(l));
    assert!(log.is_empty(), "{log:?}");
    // A writer that keeps the log growing faster than that sink lets it be
    // read: this spawn's refusals, each on its own path, 64 a millisecond
    // (16 MiB at most).
    let t = now_ms();
    let writer = {
        let (path, written, done) = (path.clone(), Arc::clone(&written), Arc::clone(&done));
        std::thread::spawn(move || {
            let mut f = fs::OpenOptions::new().append(true).open(&path).expect("open scratch log");
            let first = access(t, 0x98, "fs.make_reg", "/var/tmp/storm/0") + &allocated(t, 0x98, DAEMON_PID, EXE, "sbx-st2");
            f.write_all(first.as_bytes()).expect("append");
            let mut i = 1u64;
            while !done.load(Ordering::SeqCst) && written.load(Ordering::SeqCst) < 16 << 20 {
                let mut block = String::new();
                for _ in 0..64 {
                    block += &access(t, 0x98, "fs.make_reg", &format!("/var/tmp/storm/{i}"));
                    i += 1;
                }
                f.write_all(block.as_bytes()).expect("append");
                written.fetch_add(block.len() as u64, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    /// Ends the writer (and the sink's sleeps) however the test ends.
    struct Ends(Arc<AtomicBool>);
    impl Drop for Ends {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let ends = Ends(Arc::clone(&done));
    let end = Instant::now() + Duration::from_secs(10);
    while !reading.load(Ordering::SeqCst) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(reading.load(Ordering::SeqCst), "the thread never read");
    let (tx, rx) = std::sync::mpsc::channel();
    let started = Instant::now();
    r.stop();
    let joiner = {
        let r = Arc::clone(&r);
        std::thread::spawn(move || {
            r.join();
            let _ = tx.send(());
        })
    };
    let stopped = rx.recv_timeout(2 * poll);
    let took = started.elapsed();
    let out = sunk.load(Ordering::SeqCst);
    drop(ends);
    writer.join().expect("writer");
    joiner.join().expect("joiner");
    assert!(
        stopped.is_ok(),
        "asked to stop while reading, the reader took {took:?}, more than two {poll:?} polls ({out} lines out)"
    );
}

#[test]
fn audit_the_reader_takes_its_line_budgets_from_its_config() {
    let (_tmp, root) = scratch();
    let path = root.join("audit.log");
    append(&path, "");
    let lines = Lines::default();
    let budget = 2 * SESSION_LINES_PER_MIN;
    let r = Reader::new(Config {
        path: path.clone(),
        exe: Some(EXE.as_bytes().to_vec()),
        pid: DAEMON_PID,
        poll: Duration::from_millis(20),
        clock: Arc::new(now_ms),
        mono: Arc::new(mono_ms),
        lines_per_min: (budget, 2 * HOST_LINES_PER_MIN),
        sink: lines.sink(),
    });
    let _s = r.registry().register(&Tag::new("sbx-bg1").unwrap(), info("s1", false, &root.join("home")));
    let mut log = Vec::new();
    r.before_spawn(&mut |l| log.push(l));
    assert!(log.is_empty(), "{log:?}");
    // Three default budgets of distinct refusals, at once.
    let t = now_ms();
    let mut text = access(t, 0xb2, "fs.make_reg", "/var/tmp/b/0") + &allocated(t, 0xb2, DAEMON_PID, EXE, "sbx-bg1");
    for i in 1..3 * SESSION_LINES_PER_MIN {
        text += &access(t, 0xb2, "fs.make_reg", &format!("/var/tmp/b/{i}"));
    }
    append(&path, &text);
    let denials = || lines.all().iter().filter(|(_, l)| l.starts_with("sandbox.denial ")).count() as u64;
    let end = Instant::now() + Duration::from_secs(10);
    while denials() < budget && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(100));
    r.stop();
    r.join();
    // The budget refills 2 lines a second: one more line at most.
    let got = denials();
    assert!(got == budget || got == budget + 1, "its configured {budget} a minute, not the default: {got} lines");
}

#[test]
fn audit_a_wall_clock_step_back_brings_no_retry_or_report_early() {
    let (_tmp, root) = scratch();
    let path = root.join("audit.log");
    let (unix, mono) = (FakeClock::new(T0), FakeClock::new(T0));
    let lines = Lines::default();
    let r = reader(&path, unix.clock(), mono.clock(), Duration::from_millis(20), &lines);
    let mut log = Vec::new();
    r.before_spawn(&mut |l| log.push(l));
    let down = "sandbox.audit unavailable reason=ENOENT".to_string();
    assert_eq!(log, vec![down.clone()]);
    append(&path, "");
    // The wall clock steps back; the monotonic one doesn't move.
    unix.set(T0 - 1);
    r.before_spawn(&mut |l| log.push(l));
    assert_eq!(log, vec![down.clone()], "no retry within 10 min of the last, on the monotonic clock");
    mono.set(T0 + RETRY_MS);
    r.before_spawn(&mut |l| log.push(l));
    assert_eq!(log, vec![down, "sandbox.audit available".to_string()]);
    // A report; then the wall clock steps back an hour: the next still waits.
    let report = "sandbox.audit unattributed domains=1 records=1";
    append(&path, &dealloc(T0, 0x71, 1));
    assert!(lines.wait_for(report, 10), "{:?}", lines.all());
    unix.set(T0 - 3_600_000);
    append(&path, &dealloc(T0, 0x72, 1));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(lines.all().len(), 1, "no report early: {:?}", lines.all());
    mono.set(T0 + RETRY_MS + REPORT_MS);
    let end = Instant::now() + Duration::from_secs(10);
    while lines.all().len() < 2 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(lines.all().iter().filter(|(_, l)| l == report).count(), 2, "{:?}", lines.all());
    r.stop();
    r.join();
}

// ── Through prepare (LOCAL kernel) ───────────────────────────────────────

/// Opt-in gate, as in the spawn tests: unset, skip; SYNAPS_TEST_LANDLOCK=1
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

/// A fake home with the base dir in it, a work dir `lane`, and a scratch
/// audit log.
struct PolicyFx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    env: PolicyEnv,
    log: PathBuf,
}

fn policy_fx() -> PolicyFx {
    let (tmp, root) = scratch();
    for d in ["home/x/base/sandbox/sessions", "lane"] {
        fs::create_dir_all(root.join(d)).expect("mkdir");
    }
    let env = PolicyEnv { home: root.join("home"), base_dir: root.join("home/x/base") };
    let log = root.join("audit.log");
    append(&log, "");
    PolicyFx { _tmp: tmp, root, env, log }
}

impl PolicyFx {
    fn policy(&self, extra: &str) {
        let text = format!("mode = enforce\n{extra}write = {}\n", self.root.join("lane").display());
        fs::write(self.env.policy_path("s1").expect("valid id"), text).expect("write policy");
    }
    fn binding(&self, r: &Arc<Reader>) -> Arc<SandboxBinding> {
        SandboxBinding::with_audit("s1", self.env.clone(), Box::leak(Box::new(KillSwitch::new())), None, Arc::clone(r))
    }
}

fn apply(b: &SandboxBinding, tool: &'static str) -> (spawn::Prepared, Vec<String>) {
    let mut log = Vec::new();
    match spawn::prepare(b, tool, &mut |l| log.push(l)) {
        SpawnSandbox::Apply(p) => (*p, log),
        SpawnSandbox::None => panic!("expected a domain, got none: {log:?}"),
        SpawnSandbox::Refuse(m) => panic!("expected a domain, got a refusal: {m}"),
    }
}

#[test]
fn audit_local_an_enforcing_spawn_registers_its_session_tool_tag_and_pid() {
    if !local_kernel() {
        return;
    }
    let fx = policy_fx();
    let lines = Lines::default();
    let r = reader(&fx.log, Arc::new(now_ms), Arc::new(mono_ms), Duration::from_millis(50), &lines);
    fx.policy("");
    let (p, log) = apply(&fx.binding(&r), "bash");
    assert!(!log.iter().any(|l| l.starts_with("sandbox.audit")), "the log opened: {log:?}");
    let tag = p.tag().to_string();
    let entry = || r.registry().entry(&tag);
    assert_eq!(entry(), Some(EntryView { session: "s1".into(), tool: "bash", quiet: false, pid: None, exited: false }));
    p.log_spawned(Some(4242));
    assert_eq!(entry().map(|e| e.pid), Some(Some(4242)));
    let check = p.ctx().clone();
    drop(p);
    assert_eq!(entry().map(|e| e.exited), Some(false), "a PTY session's output check holds its spawn");
    drop(check);
    assert_eq!(entry().map(|e| e.exited), Some(true));
    r.stop();
    r.join();
}

#[test]
fn audit_local_audit_log_off_registers_the_spawn_quietly() {
    if !local_kernel() {
        return;
    }
    let fx = policy_fx();
    let lines = Lines::default();
    let r = reader(&fx.log, Arc::new(now_ms), Arc::new(mono_ms), Duration::from_millis(50), &lines);
    fx.policy("audit_log = off\n");
    let (p, _) = apply(&fx.binding(&r), "shell_start");
    let e = r.registry().entry(p.tag().as_str()).expect("registered");
    assert_eq!((e.tool, e.quiet), ("shell_start", true));
    r.stop();
    r.join();
}

#[test]
fn audit_local_the_first_enforcing_spawn_opens_the_log_at_its_end() {
    if !local_kernel() {
        return;
    }
    let fx = policy_fx();
    let lines = Lines::default();
    let r = reader(&fx.log, Arc::new(now_ms), Arc::new(mono_ms), Duration::from_millis(50), &lines);
    let _old = r.registry().register(&Tag::new("sbx-old1").unwrap(), info("s0", false, &fx.root.join("home")));
    let t = now_ms();
    append(&fx.log, &(access(t, 0xc0, "fs.make_reg", "/var/tmp/before") + &allocated(t, 0xc0, DAEMON_PID, EXE, "sbx-old1")));
    fx.policy("");
    let (p, _) = apply(&fx.binding(&r), "shell_start");
    let tag = p.tag().to_string();
    let t = now_ms();
    append(&fx.log, &(access(t, 0xc1, "fs.make_reg", "/var/tmp/after") + &allocated(t, 0xc1, DAEMON_PID, EXE, &tag)));
    let want = format!("sandbox.denial session=s1 tool=shell_start tag={tag} op=create class=system path=/var/tmp/after n=1 src=audit");
    assert!(lines.wait_for(&want, 10), "{:?}", lines.all());
    assert!(!lines.all().iter().any(|(_, l)| l.contains("/var/tmp/before")), "nothing from before the open: {:?}", lines.all());
    r.stop();
    r.join();
}

#[test]
fn audit_local_the_real_audit_log_reports_a_scratch_denial() {
    if !local_kernel() {
        return;
    }
    if let Err(e) = fs::File::open(DEFAULT_AUDIT_LOG) {
        eprintln!("skipped: {DEFAULT_AUDIT_LOG} can't be read here ({e})");
        return;
    }
    let (_tmp, root) = scratch();
    for d in ["in", "out"] {
        fs::create_dir(root.join(d)).expect("mkdir");
    }
    let lines = Lines::default();
    let r = Reader::new(Config {
        path: PathBuf::from(DEFAULT_AUDIT_LOG),
        exe: Config::daemon().exe,
        pid: std::process::id(),
        poll: Duration::from_millis(100),
        clock: Arc::new(now_ms),
        mono: Arc::new(mono_ms),
        lines_per_min: (SESSION_LINES_PER_MIN, HOST_LINES_PER_MIN),
        sink: lines.sink(),
    });
    let mut log = Vec::new();
    r.before_spawn(&mut |l| log.push(l));
    assert!(log.is_empty(), "{log:?}");
    let mut rs = Ruleset::new().expect("ruleset");
    assert!(rs.allow(&root.join("in"), WRITE_RIGHTS).expect("rule"));
    let tag = Tag::next();
    let home = root.join("home");
    let reg = r.registry().register(
        &tag,
        SpawnInfo {
            session: "s-real".into(),
            tool: "shell_start",
            quiet: false,
            home: home.clone(),
            base: home.join("x/base"),
            git_common: Vec::new(),
            cargo_home: Vec::new(),
        },
    );
    let out = root.join("out");
    let created = crate::sandbox::landlock::run_confined(&rs, &tag, || fs::File::create(out.join("new.txt")).map(drop)).expect("confined thread");
    assert_eq!(created.err().and_then(|e| e.raw_os_error()), Some(libc::EACCES), "the create is refused");
    let class = spawn::classify(&out, "", &home, &home.join("x/base"));
    let want = format!("sandbox.denial session=s-real tool=shell_start tag={tag} op=create class={class} path={} n=1 src=audit", out.display());
    assert!(lines.wait_for(&want, 15), "want {want:?} in {:?}", lines.all());
    let end = format!("sandbox.domain_end session=s-real tag={tag} denials=1 ptrace=0");
    assert!(lines.wait_for(&end, 15), "want {end:?} in {:?}", lines.all());
    drop(reg);
    r.stop();
    r.join();
}
