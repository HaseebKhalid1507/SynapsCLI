//! `tools = confined`: the tool lists, the parse of the `tools`/`tool` keys,
//! the gate on scratch bindings, its log line, and the daemon's
//! `sandbox_status` answer. Every tree is a scratch dir outside the baseline
//! directories (`test_scratch_parent`) with a fake `$HOME` and base dir;
//! nothing here touches a real `~/.synaps-cli`, and nothing needs kernel
//! support (the gate doesn't).

use super::*;
use crate::sandbox::landlock::Unavailable;
use crate::sandbox::policy::{self, test_scratch_parent, KillSwitch, PolicyEnv, Tools};
use crate::sandbox::SandboxBinding;
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const CONTACT: &str = "Ask the Master's desk.";
const CONFINED: &str = "Refused by this session's sandbox (tools = confined)";

struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    env: PolicyEnv,
}

fn fx() -> Fx {
    let tmp = tempfile::Builder::new().prefix("sbx-confine-").tempdir_in(test_scratch_parent()).expect("tempdir");
    let root = fs::canonicalize(tmp.path()).expect("canonical tempdir");
    for d in ["home/x/base/sandbox/sessions", "lane/sub", "other", "repo/.git/objects", "repo/.git/refs", "repo/.git/logs", "repo/.git/worktrees"] {
        fs::create_dir_all(root.join(d)).expect("mkdir");
    }
    for f in ["repo/.git/HEAD", "lane/file.txt"] {
        fs::write(root.join(f), b"x").expect("write");
    }
    let env = PolicyEnv { home: root.join("home"), base_dir: root.join("home/x/base") };
    Fx { _tmp: tmp, root, env }
}

impl Fx {
    fn r(&self, rel: &str) -> String {
        self.root.join(rel).display().to_string()
    }
    fn policy(&self, id: &str, text: &str) -> PathBuf {
        let p = self.env.policy_path(id).expect("valid id");
        fs::write(&p, text).expect("write policy");
        p
    }
    fn parse(&self, text: &str) -> Result<policy::Policy, policy::PolicyError> {
        policy::parse(text, Path::new("/test/s.conf"), &self.env)
    }
    /// A confined file: the lane, a named extension tool, the contact.
    fn confined(&self) -> String {
        format!("mode = enforce\ntools = confined\nwrite = {}\ntool = ext__ok__tool\ncontact = {CONTACT}\n", self.r("lane"))
    }
    /// Session `id`'s binding, with a test-owned kill switch and `abi`.
    fn binding_abi(&self, id: &str, abi: Result<u32, Unavailable>) -> Arc<SandboxBinding> {
        SandboxBinding::with_env(id, self.env.clone(), Box::leak(Box::new(KillSwitch::new())), Some(abi))
    }
    fn binding(&self, id: &str) -> Arc<SandboxBinding> {
        self.binding_abi(id, Ok(7))
    }
}

// ---- the lists and the names ----

#[test]
fn the_builtin_and_worker_lists_are_exact_and_disjoint() {
    assert_eq!(
        BUILTIN_TOOLS,
        [
            "read",
            "grep",
            "find",
            "ls",
            "bash",
            "shell_start",
            "shell_send",
            "shell_end",
            "write",
            "edit",
            "multi_edit",
            "wait",
            "context_checkpoint",
            "memory_search",
            "memory_fetch",
            "forum_read"
        ]
    );
    assert_eq!(WORKER_TOOLS, ["subagent", "subagent_start", "subagent_resume"]);
    assert!(BUILTIN_TOOLS.iter().all(|t| !WORKER_TOOLS.contains(t)));
    assert!(BUILTIN_TOOLS.iter().chain(WORKER_TOOLS).all(|t| valid_tool_name(t)));
}

#[test]
fn class_of_runs_builtins_and_named_tools_and_refuses_the_rest() {
    let named = vec!["ext__ok__tool".to_string(), "mcp:srv.tool-1".to_string(), "subagent_start".to_string()];
    let cases: &[(&str, Option<Class>)] = &[
        ("read", None),
        ("bash", None),
        ("forum_read", None),
        ("ext__ok__tool", None),
        ("mcp:srv.tool-1", None),
        // A worker tool stays refused even if a list names it.
        ("subagent", Some(Class::Worker)),
        ("subagent_start", Some(Class::Worker)),
        ("subagent_resume", Some(Class::Worker)),
        // Allowlist: anything else, a near miss included, is refused.
        ("subagent_steer", Some(Class::NotAllowed)),
        ("memory_store", Some(Class::NotAllowed)),
        ("forum_post", Some(Class::NotAllowed)),
        ("ext__other__tool", Some(Class::NotAllowed)),
        ("Read", Some(Class::NotAllowed)),
        ("read ", Some(Class::NotAllowed)),
        ("ext__ok__tool2", Some(Class::NotAllowed)),
        ("", Some(Class::NotAllowed)),
    ];
    let mut bad = Vec::new();
    for (tool, want) in cases {
        let got = class_of(tool, &named);
        if got != *want {
            bad.push(format!("{tool:?}: got {got:?}, want {want:?}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
    assert_eq!(class_of("ext__ok__tool", &[]), Some(Class::NotAllowed), "named only by the list it is given");
    assert_eq!([Class::Path, Class::Worker, Class::NotAllowed].map(|c| c.as_str()), ["path", "worker", "not_allowed"]);
}

#[test]
fn valid_tool_names_are_short_and_plain() {
    let long = "x".repeat(MAX_TOOL_NAME);
    for ok in ["a", "ext__figma__get_screenshot", "mcp:srv.tool-1", "A_b.c:d-e9", long.as_str()] {
        assert!(valid_tool_name(ok), "{ok:?}");
    }
    let too_long = "x".repeat(MAX_TOOL_NAME + 1);
    for bad in ["", too_long.as_str(), "a b", "a/b", "a=b", "é", "a\nb", "a\tb", "a\u{1b}b", "\"a\""] {
        assert!(!valid_tool_name(bad), "{bad:?}");
    }
}

#[test]
fn the_refused_line_has_a_fixed_shape_and_escapes_the_name() {
    assert_eq!(refused_line("s1", "ext__x", Class::NotAllowed), "sandbox.tool_refused session=s1 tool=ext__x class=not_allowed");
    assert_eq!(refused_line("s1", "subagent", Class::Worker), "sandbox.tool_refused session=s1 tool=subagent class=worker");
    assert_eq!(refused_line("s1", "write", Class::Path), "sandbox.tool_refused session=s1 tool=write class=path");
    // A forged name can't add a field or a line.
    let forged = refused_line("s1", "a\nsandbox.x session=evil class=path \u{1b}[0m", Class::NotAllowed);
    assert!(!forged.chars().any(|c| c.is_control()), "{forged:?}");
    assert_eq!(forged.matches(' ').count(), 3, "four fields, no more: {forged:?}");
    assert_eq!(forged.matches("session=").count(), 1, "{forged:?}");
    assert!(forged.starts_with("sandbox.tool_refused session=s1 tool=a\\x0asandbox.x\\x20session\\x3devil"), "{forged:?}");
    assert!(forged.ends_with(" class=not_allowed"), "{forged:?}");
    let long = refused_line("s1", &"y".repeat(500), Class::NotAllowed);
    assert!(long.contains(&format!("tool={}... class=", "y".repeat(MAX_TOOL_NAME))), "{long}");
    assert!(refused_line("s1", "", Class::NotAllowed).contains(" tool=\\x00 "), "an empty name still fills the field");
}

#[test]
fn refusal_texts_carry_the_contact_and_name_only_the_tool() {
    for (class, says) in [
        (Class::Worker, "starts or resumes a worker"),
        (Class::NotAllowed, "is not on its tool list"),
        (Class::Path, "may write only inside the directories its policy grants"),
    ] {
        let t = refusal("some_tool", class, CONTACT);
        assert!(t.starts_with(CONFINED), "{t}");
        assert!(t.contains("`some_tool`") && t.contains(says) && t.ends_with(CONTACT), "{t}");
    }
    let p = path_refusal("write", Path::new("/x/y.txt"), CONTACT);
    assert!(p.starts_with(CONFINED) && p.contains("`/x/y.txt`") && p.contains("`write`") && p.ends_with(CONTACT), "{p}");
}

// ---- the policy file ----

#[test]
fn policy_parses_the_tools_and_tool_keys() {
    let fx = fx();
    let p = fx.parse(&fx.confined()).expect("valid");
    assert_eq!(p.tools(), Tools::Confined);
    assert!(p.confines_tools());
    assert_eq!(p.named_tools(), ["ext__ok__tool".to_string()]);
    // A `tool` line may come before the `tools` line; each keeps file order.
    let p = fx.parse("mode = enforce\ntool = mcp:b\ntools = confined\ntool = ext__a\n").expect("valid");
    assert_eq!(p.named_tools(), ["mcp:b".to_string(), "ext__a".to_string()]);
    assert!(p.confines_tools());
    // Without the key, and with `tools = open`: open, nothing named.
    for text in ["mode = enforce\n", "mode = enforce\ntools = open\n", "mode = off\n", "mode = off\ntools = open\n"] {
        let p = fx.parse(text).expect("valid");
        assert_eq!((p.tools(), p.named_tools().len(), p.confines_tools()), (Tools::Open, 0, false), "{text:?}");
    }
    assert_eq!([Tools::Open.as_str(), Tools::Confined.as_str()], ["open", "confined"]);
    // 64 named tools are fine.
    let many: String = (0..64).map(|i| format!("tool = ext__t{i}\n")).collect();
    let p = fx.parse(&format!("mode = enforce\ntools = confined\n{many}")).expect("64 tool lines");
    assert_eq!(p.named_tools().len(), 64);
}

#[test]
fn policy_refuses_each_bad_tools_or_tool_line() {
    let fx = fx();
    let too_many: String = (0..65).map(|i| format!("tool = ext__t{i}\n")).collect();
    let long = "x".repeat(MAX_TOOL_NAME + 1);
    // (text, line, reason, the error says the file confines tools)
    let cases: Vec<(String, usize, &str, bool)> = vec![
        ("mode = enforce\ntools = closed\n".into(), 2, "`tools` must be `open` or `confined`, got `closed`", false),
        ("mode = enforce\ntools = confined\ntools = confined\n".into(), 3, "`tools` set twice (first on line 2)", true),
        ("mode = enforce\ntools = open\ntools = confined\n".into(), 3, "`tools` set twice (first on line 2)", true),
        ("mode = off\ntools = confined\n".into(), 2, "`tools = confined` needs `mode = enforce`", true),
        ("mode = enforce\ntool = ext__a\n".into(), 2, "`tool` needs `tools = confined`", false),
        ("mode = enforce\ntools = open\ntool = ext__a\n".into(), 3, "`tool` needs `tools = confined`", false),
        ("mode = enforce\ntools = confined\ntool = a b\n".into(), 3, "`tool` must be 1-128 of A-Z a-z 0-9 _ . : -", true),
        (format!("mode = enforce\ntools = confined\ntool = {long}\n"), 3, "`tool` must be 1-128 of", true),
        ("mode = enforce\ntools = confined\ntool = subagent_start\n".into(), 3, "starts or resumes a worker", true),
        ("mode = enforce\ntools = confined\ntool = subagent\n".into(), 3, "starts or resumes a worker", true),
        ("mode = enforce\ntools = confined\ntool = x\ntool = x\n".into(), 4, "`tool = x` set twice (first on line 3)", true),
        (format!("mode = enforce\ntools = confined\n{too_many}"), 67, "more than 64 `tool =` lines", true),
        // Any other broken line in a file that says `tools = confined`.
        ("mode = enforce\ntools = confined\nwrite = /\n".into(), 3, "refuses `/`", true),
        ("mode = enforce\nwrite = /\n".into(), 2, "refuses `/`", false),
        ("mode = enforce\nbogus = 1\n# tools = confined\n".into(), 2, "unknown key `bogus`", false),
        ("mode = enforce\nbogus = 1\n  tools   =   confined  \n".into(), 2, "unknown key `bogus`", true),
    ];
    let mut bad = Vec::new();
    for (text, line, reason, confined) in cases {
        match fx.parse(&text) {
            Ok(_) => bad.push(format!("ACCEPTED {text:?}")),
            Err(e) if e.line != line || !e.reason.contains(reason) || e.confined != confined => bad.push(format!(
                "{text:?}: got line {} {:?} confined={}, want line {line} {reason:?} confined={confined}",
                e.line, e.reason, e.confined
            )),
            Err(_) => {}
        }
    }
    assert!(bad.is_empty(), "{} case(s) wrong:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn policy_a_file_that_cant_be_read_says_confined_only_if_its_text_does() {
    let fx = fx();
    // Not UTF-8: the scan reads it leniently.
    let p = fx.env.policy_path("bin").expect("id");
    fs::write(&p, b"mode = enforce\ntools = confined\nwrite = \xff\n").expect("write");
    let e = policy::load(&p, &fx.env).expect_err("not UTF-8");
    assert!(e.reason.contains("not valid UTF-8") && e.confined, "{e:?}");
    fs::write(&p, b"mode = enforce\nwrite = \xff\n").expect("write");
    assert!(!policy::load(&p, &fx.env).expect_err("not UTF-8").confined);
    // Not read at all (a symlink, a second name): nothing to scan, so open
    // as today (the file is invalid either way: every spawn is refused).
    let real = fx.root.join("other/real.conf");
    fs::write(&real, fx.confined()).expect("write");
    let link = fx.env.policy_path("link").expect("id");
    symlink(&real, &link).expect("symlink");
    assert!(!policy::load(&link, &fx.env).expect_err("symlink").confined);
    let two = fx.policy("two", &fx.confined());
    fs::hard_link(&two, fx.root.join("other/two.conf")).expect("hard link");
    assert!(!policy::load(&two, &fx.env).expect_err("two names").confined);
}

#[test]
fn policy_line_is_byte_identical_without_the_key() {
    let fx = fx();
    let plain = format!("mode = enforce\nwrite = {}\n", fx.r("lane"));
    let p = fx.parse(&plain).expect("valid");
    let want = format!(
        "sandbox.policy session=S1 mode=enforce subagents=enforce abi=8 rules={} skipped={} hash={}",
        p.rules().len(),
        p.skipped().len(),
        p.hash()
    );
    assert_eq!(policy::policy_line("S1", &p, Ok(8)), want);
    // `tools = open` is today's behavior, said out loud: no suffix either.
    let open = fx.parse(&format!("{plain}tools = open\n")).expect("valid");
    assert!(policy::policy_line("S1", &open, Ok(8)).ends_with(&format!(" hash={}", open.hash())));
    for (named, text) in [(0, format!("{plain}tools = confined\n")), (2, format!("{plain}tools = confined\ntool = a\ntool = b\n"))] {
        let c = fx.parse(&text).expect("valid");
        assert_eq!(
            policy::policy_line("S1", &c, Ok(8)),
            format!(
                "sandbox.policy session=S1 mode=enforce subagents=enforce abi=8 rules={} skipped={} hash={} tools=confined named={named}",
                c.rules().len(),
                c.skipped().len(),
                c.hash()
            )
        );
    }
}

#[test]
fn check_report_is_unchanged_without_the_key_and_names_the_tools_with_it() {
    let fx = fx();
    let plain = format!("mode = enforce\nwrite = {}\ncontact = {CONTACT}\n", fx.r("lane"));
    let a = fx.policy("plain", &plain);
    let b = fx.policy("conf", &format!("{plain}tools = confined\ntool = ext__ok__tool\ntool = mcp:x\n"));
    let (ok_a, text_a) = policy::check_report(&a, &fx.env, Ok(8));
    let (ok_b, text_b) = policy::check_report(&b, &fx.env, Ok(8));
    assert!(ok_a && ok_b, "{text_a}\n{text_b}");
    assert!(!text_a.lines().any(|l| l.starts_with("tools") || l.starts_with("tool ")), "nothing new without the key:\n{text_a}");
    let added: Vec<&str> = text_b.lines().filter(|l| l.starts_with("tools = ")).collect();
    assert_eq!(
        added,
        ["tools = confined: write, edit and multi_edit write only in or below the [all-write] dirs below; worker tools are refused; other tools: the built-in list and 2 named (ext__ok__tool, mcp:x)"],
        "{text_b}"
    );
    // Everything else is the same report (the appended lines don't move
    // a rule's line number; the hash and file name differ).
    let hash = |t: &str| t.lines().next().and_then(|l| l.rsplit("(hash ").next()).map(|h| h.trim_end_matches(')').to_string()).expect("hash");
    let rest: Vec<String> = text_b
        .lines()
        .filter(|l| !l.starts_with("tools = "))
        .map(|l| l.replace(&b.display().to_string(), &a.display().to_string()).replace(&hash(&text_b), &hash(&text_a)))
        .collect();
    assert_eq!(rest, text_a.lines().map(str::to_string).collect::<Vec<_>>());
    let (_, none) = policy::check_report(&fx.policy("none", &format!("{plain}tools = confined\n")), &fx.env, Ok(8));
    assert!(none.contains("other tools: the built-in list and 0 named (-)"), "{none}");
}

#[test]
fn write_dirs_are_the_all_write_directory_rules() {
    let fx = fx();
    fs::write(fx.root.join("other/f.txt"), b"x").expect("write");
    let text = format!(
        "mode = enforce\ntools = confined\nwrite = {}\nwrite_file = {}\ninfra = {}\ngit_common = {}\n",
        fx.r("lane"),
        fx.r("other/f.txt"),
        fx.r("other"),
        fx.r("repo/.git")
    );
    let p = fx.parse(&text).expect("valid");
    let id = |p: &str| fs::metadata(p).map(|m| (m.dev(), m.ino())).expect("stat");
    let dirs = write_dirs(&p);
    for yes in [fx.r("lane"), fx.r("other"), fx.r("repo/.git/objects"), fx.r("repo/.git/refs"), fx.r("repo/.git/logs"), fx.r("repo/.git/worktrees")] {
        assert!(dirs.contains(&id(&yes)), "{yes}");
    }
    // The baseline's write dirs (read-only stat of the real paths).
    for yes in ["/tmp", "/dev/shm"] {
        if Path::new(yes).is_dir() {
            assert!(dirs.contains(&id(yes)), "{yes}");
        }
    }
    // A file rule, the git common dir itself, /proc and /dev/pts: never.
    for no in [fx.r("other/f.txt"), fx.r("repo/.git"), "/proc".into(), "/dev/pts".into()] {
        if Path::new(&no).exists() {
            assert!(!dirs.contains(&id(&no)), "{no}");
        }
    }
}

// ---- the gate ----

#[test]
fn the_gate_refuses_workers_and_unlisted_tools_in_a_confined_session() {
    let fx = fx();
    fx.policy("c1", &fx.confined());
    let b = fx.binding("c1");
    for tool in BUILTIN_TOOLS.iter().copied().chain(["ext__ok__tool"]) {
        b.gate_tool(tool).unwrap_or_else(|e| panic!("{tool} runs: {e}"));
    }
    for (tool, says) in [
        ("subagent", "starts or resumes a worker"),
        ("subagent_start", "starts or resumes a worker"),
        ("subagent_resume", "starts or resumes a worker"),
        ("subagent_steer", "is not on its tool list"),
        ("memory_store", "is not on its tool list"),
        ("ext__other__tool", "is not on its tool list"),
    ] {
        let e = b.gate_tool(tool).expect_err(tool);
        assert!(e.starts_with(CONFINED) && e.contains(says) && e.ends_with(CONTACT), "{tool}: {e}");
    }
    // The free function the dispatch sites call: no binding, no gate.
    assert!(crate::sandbox::gate_tool(None, "subagent_start").is_ok());
    assert!(crate::sandbox::gate_tool(Some(&b), "subagent_start").is_err());
}

#[test]
fn the_gate_lets_every_tool_run_in_a_session_without_the_key() {
    let fx = fx();
    let tools = ["subagent_start", "ext__other__tool", "memory_store", "read"];
    fx.policy("enf", &format!("mode = enforce\nwrite = {}\n", fx.r("lane")));
    fx.policy("open", &format!("mode = enforce\ntools = open\nwrite = {}\n", fx.r("lane")));
    fx.policy("off", "mode = off\n");
    fx.policy("bad", "mode = enforce\nwrite = /\n");
    for id in ["none", "enf", "open", "off", "bad"] {
        let b = fx.binding(id);
        for tool in tools {
            assert!(b.gate_tool(tool).is_ok(), "{id}: {tool}");
        }
    }
    // An id that can't name a file never has a policy.
    assert!(fx.binding("../c1").gate_tool("subagent_start").is_ok());
}

#[test]
fn the_kill_switch_lifts_the_gate_and_the_file_is_read_at_every_call() {
    let fx = fx();
    let file = fx.policy("c1", &fx.confined());
    let b = fx.binding("c1");
    assert!(b.gate_tool("subagent_start").is_err());
    fs::write(fx.env.kill_switch(), b"").expect("touch OFF");
    assert!(b.gate_tool("subagent_start").is_ok(), "the kill switch lifts it");
    fs::remove_file(fx.env.kill_switch()).expect("rm OFF");
    assert!(b.gate_tool("subagent_start").is_err(), "back on with the switch gone");
    fs::write(&file, format!("mode = enforce\nwrite = {}\n", fx.r("lane"))).expect("rewrite");
    assert!(b.gate_tool("subagent_start").is_ok(), "a file without the key, from the next call");
}

#[test]
fn an_invalid_confined_file_fails_closed() {
    let fx = fx();
    for (id, text) in [
        ("inv1", format!("mode = enforce\ntools = confined\ntool = ext__ok__tool\nwrite = /\ncontact = {CONTACT}\n")),
        ("inv2", format!("mode = off\ntools = confined\ncontact = {CONTACT}\n")),
        ("inv3", format!("contact = {CONTACT}\ntools = confined\n")),
    ] {
        fx.policy(id, &text);
        let b = fx.binding(id);
        for tool in ["subagent_start", "ext__ok__tool", "ext__other__tool"] {
            let e = b.gate_tool(tool).expect_err(tool);
            assert!(e.starts_with(CONFINED) && e.ends_with(CONTACT), "{id} {tool}: {e}");
        }
        assert!(b.gate_tool("read").is_ok(), "{id}: the built-ins still run");
    }
}

#[test]
fn the_gate_holds_whatever_the_landlock_abi() {
    let fx = fx();
    fx.policy("c1", &fx.confined());
    for abi in [Err(Unavailable::NoLandlock), Err(Unavailable::Disabled), Err(Unavailable::AbiTooOld(2)), Ok(2), Err(Unavailable::Error(libc::EMFILE))] {
        let shown = format!("{abi:?}");
        let b = fx.binding_abi("c1", abi);
        assert!(b.gate_tool("subagent_start").is_err(), "{shown}");
        assert!(b.gate_tool("ext__other__tool").is_err(), "{shown}");
        assert!(b.gate_tool("ext__ok__tool").is_ok(), "{shown}");
    }
}

#[test]
fn a_refused_call_logs_one_line_without_its_input() {
    let fx = fx();
    fx.policy("c1", &fx.confined());
    fx.policy("plain", &format!("mode = enforce\nwrite = {}\n", fx.r("lane")));
    let b = fx.binding("c1");
    let plain = fx.binding("plain");
    let (out, guard) = crate::sandbox::log_tests::daemon_log();
    let r1 = b.gate_tool("subagent_start");
    let r2 = b.gate_tool("ext__other__tool");
    let r3 = b.gate_tool("read");
    // The control: the same calls in a session without the key log nothing.
    let r4 = plain.gate_tool("subagent_start");
    drop(guard);
    assert!(r1.is_err() && r2.is_err() && r3.is_ok() && r4.is_ok());
    let text = out.text();
    let lines: Vec<&str> = text.lines().filter(|l| l.contains("sandbox.")).collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[0].contains(" WARN ") && lines[0].ends_with("sandbox.tool_refused session=c1 tool=subagent_start class=worker"), "{text}");
    assert!(lines[1].contains(" WARN ") && lines[1].ends_with("sandbox.tool_refused session=c1 tool=ext__other__tool class=not_allowed"), "{text}");
    assert!(!text.contains(CONTACT) && !text.contains(&fx.r("lane")), "no contact or path in the log:\n{text}");
}

// ---- sandbox_status ----

fn status(fx: &Fx, id: &str) -> crate::sandbox::Status {
    crate::sandbox::status_with(&fx.env, id, Ok(7))
}

#[test]
fn status_reports_each_state_from_the_spawns_read() {
    let fx = fx();
    let none = status(&fx, "none");
    assert_eq!(
        none,
        crate::sandbox::Status { state: "none", kill_switch: false, tools: "open", policy_hash: None, rules: 0, skipped: 0, abi: Some(7) }
    );
    let text = format!("{}write = {}\n", fx.confined(), fx.r("missing"));
    let file = fx.policy("conf", &text);
    let p = policy::load(&file, &fx.env).expect("valid").expect("exists");
    let s = status(&fx, "conf");
    assert_eq!(
        s,
        crate::sandbox::Status {
            state: "enforce",
            kill_switch: false,
            tools: "confined",
            policy_hash: Some(policy::short_hash(text.as_bytes())),
            rules: p.rules().len(),
            skipped: p.skipped().len(),
            abi: Some(7)
        }
    );
    assert_eq!(s.skipped, 1);
    fx.policy("enf", &format!("mode = enforce\nwrite = {}\n", fx.r("lane")));
    assert_eq!((status(&fx, "enf").state, status(&fx, "enf").tools), ("enforce", "open"));
    fx.policy("off", "mode = off\n");
    let off = status(&fx, "off");
    assert_eq!((off.state, off.tools, off.policy_hash.is_some()), ("off", "open", true));
    for (id, text, tools) in [("inv", "mode = enforce\nwrite = /\n", "open"), ("invc", "mode = enforce\ntools = confined\nwrite = /\n", "confined")] {
        fx.policy(id, text);
        assert_eq!(
            status(&fx, id),
            crate::sandbox::Status { state: "invalid", kill_switch: false, tools, policy_hash: None, rules: 0, skipped: 0, abi: Some(7) },
            "{id}"
        );
    }
    // An id that can't name a file has none.
    assert_eq!(status(&fx, "../conf").state, "none");
    // The ABI as a spawn sees it: below the minimum or unavailable = none.
    assert_eq!(crate::sandbox::status_with(&fx.env, "conf", Ok(2)).abi, None);
    assert_eq!(crate::sandbox::status_with(&fx.env, "conf", Err(Unavailable::NoLandlock)).abi, None);
}

#[test]
fn status_reads_the_file_under_the_kill_switch_and_logs_nothing() {
    let fx = fx();
    fx.policy("conf", &fx.confined());
    fs::write(fx.env.kill_switch(), b"").expect("touch OFF");
    let (out, guard) = crate::sandbox::log_tests::daemon_log();
    let s = status(&fx, "conf");
    drop(guard);
    assert_eq!((s.state, s.kill_switch, s.tools), ("enforce", true, "confined"));
    assert_eq!(out.text(), "", "the query logs nothing (no kill-switch line either)");
    fs::remove_file(fx.env.kill_switch()).expect("rm OFF");
    assert!(!status(&fx, "conf").kill_switch);
}

#[test]
fn status_answers_for_a_session_that_never_spawned() {
    let fx = fx();
    let id = format!("never-spawned-{}", std::process::id());
    fx.policy(&id, &fx.confined());
    let known = || crate::sandbox::registry().lock().unwrap_or_else(|p| p.into_inner()).contains_key(&id);
    assert!(!known());
    assert_eq!((status(&fx, &id).state, status(&fx, &id).tools), ("enforce", "confined"));
    assert!(!known(), "asking binds nothing");
}

#[test]
fn the_status_reply_carries_no_path_contact_or_text() {
    let fx = fx();
    let text = format!("{}write = {}\n", fx.confined(), fx.r("missing"));
    fx.policy("conf", &text);
    fx.policy("invc", &format!("mode = enforce\ntools = confined\nwrite = {}\ncontact = {CONTACT}\n", fx.r("home/x/base")));
    for id in ["conf", "invc", "none"] {
        let frame = crate::daemon::control_queries::sandbox_status_reply(Some(9), id.to_string(), status(&fx, id));
        let json = serde_json::to_string(&frame).expect("encode");
        assert!(json.starts_with("{\"type\":\"sandbox_status_result\",\"id\":9,"), "{json}");
        for leak in [fx.root.display().to_string(), CONTACT.to_string(), "Master".into(), "ext__ok__tool".into(), "/".into()] {
            assert!(!json.contains(&leak), "{id}: {leak:?} in {json}");
        }
    }
    assert!(crate::daemon::control_queries::daemon_features().contains(&"sandbox_status".to_string()));
}
