//! The bash tool under a session's sandbox policy. Every tree is a scratch
//! dir outside the baseline directories (`test_scratch_parent`: a fake home,
//! base dir and work dirs). `bash_sandbox_local_*`
//! runs commands under a real Landlock domain: LOCAL kernel only (not
//! g1pro/WSL), opt in with SYNAPS_TEST_LANDLOCK=1. The rest needs no kernel
//! support: no domain is built.

use super::*;
use crate::sandbox::landlock::{abi_version, Unavailable, MIN_ABI};
use crate::sandbox::policy::{test_scratch_parent, KillSwitch, PolicyEnv};
use crate::sandbox::spawn::SUDO_REFUSED;
use crate::sandbox::SandboxBinding;
use crate::tools::test_helpers::create_tool_context;
use crate::tools::Tool;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    env: PolicyEnv,
}

fn fx() -> Fx {
    let tmp = tempfile::Builder::new().prefix("sbx-bash-").tempdir_in(test_scratch_parent()).expect("tempdir");
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
    fn write_policy(&self, text: &str) {
        fs::write(self.env.policy_path("s1").expect("valid id"), text).expect("write policy");
    }
    fn set_kill_switch(&self, on: bool) {
        if on {
            fs::write(self.env.kill_switch(), "").expect("create OFF");
        } else {
            fs::remove_file(self.env.kill_switch()).expect("remove OFF");
        }
    }
    /// Session `s1`'s binding, with a test-owned kill switch.
    fn binding(&self, abi: Option<std::result::Result<u32, Unavailable>>) -> Arc<SandboxBinding> {
        SandboxBinding::with_env("s1", self.env.clone(), Box::leak(Box::new(KillSwitch::new())), abi)
    }
}

async fn run(sandbox: Option<&Arc<SandboxBinding>>, command: &str) -> Result<String> {
    let mut ctx = create_tool_context();
    ctx.capabilities.sandbox = sandbox.cloned();
    BashTool.execute(json!({ "command": command, "timeout": 30 }), ctx).await
}

const TOO_OLD: Unavailable = Unavailable::AbiTooOld(2);

#[tokio::test]
async fn bash_sandbox_off_is_byte_identical() {
    let fx = fx();
    // stdout only: stdout/stderr interleaving is not deterministic. The
    // whole sudo function is printed, so a different prelude shows.
    let ok = "echo out; declare -f sudo; echo end";
    let fail = "echo x; exit 3";
    let plain = (run(None, ok).await.unwrap(), run(None, fail).await.unwrap_err().to_string());
    let lines: Vec<&str> = plain.0.lines().collect();
    assert_eq!((lines.first(), lines.last()), (Some(&"out"), Some(&"end")), "{}", plain.0);
    // This test binary does not dispatch the askpass helper: sudo never asks.
    assert!(plain.0.contains("command sudo -n"), "the unsandboxed prelude's sudo function:\n{}", plain.0);
    let no_policy = fx.binding(Some(Ok(7)));
    assert_eq!((run(Some(&no_policy), ok).await.unwrap(), run(Some(&no_policy), fail).await.unwrap_err().to_string()), plain);
    fx.write_policy("mode = off\n");
    let off = fx.binding(Some(Ok(7)));
    assert_eq!((run(Some(&off), ok).await.unwrap(), run(Some(&off), fail).await.unwrap_err().to_string()), plain);
    let unavailable = fx.binding(Some(Err(TOO_OLD)));
    fx.write_policy(&format!("mode = enforce\nwrite = {}\n", fx.r("lane")));
    assert_eq!(run(Some(&unavailable), ok).await.unwrap(), plain.0, "unavailable and not required: unsandboxed");
}

#[tokio::test]
async fn bash_sandbox_invalid_policy_refuses_and_runs_nothing() {
    let fx = fx();
    fx.write_policy("mode = enforce\ncontact = Ask your foreman.\nbogus = 1\n");
    let marker = fx.root.join("lane/ran");
    let err = run(Some(&fx.binding(Some(Ok(7)))), &format!("touch {}", marker.display())).await.unwrap_err().to_string();
    assert!(err.contains("sandbox policy invalid for this session ("), "{err}");
    assert!(err.contains("commands can't run until it's fixed. Ask your foreman."), "{err}");
    assert!(!marker.exists(), "nothing ran");
    // A refusal is the tool's error, returned before any child exists, so the
    // child-output prompt detection never sees it, even text that looks like
    // a sudo password prompt.
    fx.write_policy("mode = enforce\nsudo password: = 1\n");
    let err = run(Some(&fx.binding(Some(Ok(7)))), &format!("touch {}", marker.display())).await.unwrap_err().to_string();
    assert!(err.contains(":2: unknown key `sudo password:`); commands can't run until it's fixed."), "{err}");
    assert!(!marker.exists(), "nothing ran");
}

#[tokio::test]
async fn bash_sandbox_a_landlock_error_refuses_and_runs_nothing() {
    let fx = fx();
    fx.write_policy(&format!("mode = enforce\nwrite = {}\ncontact = Ask your foreman.\n", fx.r("lane")));
    let marker = fx.root.join("lane/ran");
    let b = fx.binding(Some(Err(Unavailable::Error(libc::ENOMEM))));
    let err = run(Some(&b), &format!("touch {}", marker.display())).await.unwrap_err().to_string();
    assert!(err.contains("sandbox policy could not be applied for this session (landlock error (errno "), "{err}");
    assert!(!marker.exists(), "nothing ran: an error is never a reason to run unsandboxed");
}

#[tokio::test]
async fn bash_sandbox_a_refused_rule_refuses_and_runs_nothing() {
    let fx = fx();
    fx.write_policy(&format!("mode = enforce\nwrite = {}\ncontact = Ask your foreman.\n", fx.r("lane")));
    let marker = fx.root.join("lane/ran");
    let b = SandboxBinding::with_failing_build("s1", fx.env.clone(), Box::leak(Box::new(KillSwitch::new())), Some(Ok(7)));
    let err = run(Some(&b), &format!("touch {}", marker.display())).await.unwrap_err().to_string();
    let want = format!("sandbox policy could not be applied for this session (rule for `{}` refused: ", fx.r("lane"));
    assert!(err.contains(&want), "{err}");
    assert!(err.contains("commands can't run until it's fixed. Ask your foreman."), "{err}");
    assert!(!marker.exists(), "nothing ran: the rules are built before any child exists");
}

#[tokio::test]
async fn bash_sandbox_required_but_unavailable_refuses_and_runs_nothing() {
    let fx = fx();
    fx.write_policy(&format!("mode = enforce\nrequired = true\nwrite = {}\ncontact = Ask your foreman.\n", fx.r("lane")));
    let marker = fx.root.join("lane/ran");
    let err = run(Some(&fx.binding(Some(Err(TOO_OLD)))), &format!("touch {}", marker.display())).await.unwrap_err().to_string();
    assert!(err.contains("sandbox required for this session but unavailable on this host ("), "{err}");
    assert!(err.contains("Ask your foreman."), "{err}");
    assert!(!marker.exists(), "nothing ran");
}

#[tokio::test]
async fn bash_sandbox_kill_switch_on_off_on_between_calls() {
    let fx = fx();
    fx.write_policy(&format!("mode = enforce\nrequired = true\nwrite = {}\ncontact = Ask your foreman.\n", fx.r("lane")));
    let b = fx.binding(Some(Err(TOO_OLD)));
    let touch = |n: &str| format!("touch {}", fx.r(&format!("lane/{n}")));
    assert!(run(Some(&b), &touch("one")).await.is_err(), "refused: required, unavailable");
    fx.set_kill_switch(true);
    run(Some(&b), &touch("two")).await.expect("the global switch turns the sandbox off");
    fx.set_kill_switch(false);
    assert!(run(Some(&b), &touch("three")).await.is_err(), "refused again");
    assert_eq!(
        ["one", "two", "three"].map(|n| fx.root.join("lane").join(n).exists()),
        [false, true, false],
        "only the call under the switch ran"
    );
}

// ── Real-kernel tests: LOCAL kernel only (not g1pro/WSL). ────────────────

fn local_kernel() -> bool {
    if std::env::var("SYNAPS_TEST_LANDLOCK").as_deref() != Ok("1") {
        return false;
    }
    match abi_version() {
        Ok(abi) if abi >= MIN_ABI => true,
        other => panic!("SYNAPS_TEST_LANDLOCK=1 but Landlock is unusable here: {other:?}"),
    }
}

fn enforcing(fx: &Fx) -> Arc<SandboxBinding> {
    fx.write_policy(&format!("mode = enforce\nwrite = {}\ncontact = Ask your foreman.\n", fx.r("lane")));
    fx.binding(None)
}

#[tokio::test]
async fn bash_sandbox_local_inside_works_outside_is_refused_with_a_hint() {
    if !local_kernel() {
        return;
    }
    let fx = fx();
    let victim = fx.r("home/victim");
    fs::write(&victim, "keep\n").unwrap();
    let b = enforcing(&fx);
    let out = run(Some(&b), &format!("echo a > {0}/ok && cat {0}/ok", fx.r("lane"))).await.unwrap();
    assert_eq!(out.trim(), "a", "a write inside works");
    assert!(!out.contains("[sandbox]"), "no hint without a denial: {out}");

    let err = run(Some(&b), &format!("export LC_ALL=C; rm -f {victim}")).await.unwrap_err().to_string();
    assert!(err.contains("Command failed (exit 1)"), "{err}");
    assert!(err.contains(&format!("rm: cannot remove '{victim}': Permission denied")), "{err}");
    let hint = format!("[sandbox] writes outside this session's write set are refused: {victim}. Allowed: {}, /tmp.", fx.r("lane"));
    assert!(err.contains(&hint), "the failed command carries the hint:\n{err}");

    // A command that succeeds anyway still gets the hint.
    let out = run(Some(&b), &format!("export LC_ALL=C; rm -f {victim}; echo done")).await.unwrap();
    assert!(out.contains("done") && out.contains(&hint), "{out}");
    assert_eq!(fs::read_to_string(&victim).unwrap(), "keep\n");
}

#[tokio::test]
async fn bash_sandbox_local_a_successful_commands_hint_reaches_the_model_history() {
    if !local_kernel() {
        return;
    }
    let fx = fx();
    let victim = fx.r("home/victim");
    let b = enforcing(&fx);
    // The streaming runtime takes a successful command's history text from
    // the delta lane, not from the tool's return value.
    let channel = crate::tools::output::delta_channel();
    let history = channel.output_handle();
    let _rx = channel.receiver;
    let mut ctx = create_tool_context();
    ctx.capabilities.sandbox = Some(b);
    ctx.channels.tx_delta = Some(channel.sender);
    let out = BashTool
        .execute(json!({ "command": format!("export LC_ALL=C; touch {victim}; echo done"), "timeout": 30 }), ctx)
        .await
        .unwrap();
    let hint = format!("[sandbox] writes outside this session's write set are refused: {victim}. ");
    assert!(out.contains("done") && out.contains(&hint), "{out}");
    let text = history.model_history().text;
    assert!(text.contains("done") && text.contains(&hint), "the delta lane carries the hint:\n{text}");
    assert!(!fx.root.join("home/victim").exists());
}

#[tokio::test]
async fn bash_sandbox_local_sudo_is_refused_with_the_message() {
    if !local_kernel() {
        return;
    }
    let fx = fx();
    let b = enforcing(&fx);
    let refused = format!("{SUDO_REFUSED} Ask your foreman.");
    // Only under no_new_privs (the domain's own precondition): without the
    // sandbox (a broken build), no real sudo may run unconfined, so the
    // command prints nnp=0 and the test fails instead.
    let gate = "grep -q '^NoNewPrivs:[[:space:]]*1' /proc/self/status";
    let out = run(Some(&b), &format!("if {gate}; then sudo -n true; echo rc=$?; else echo nnp=0; fi")).await.unwrap();
    assert!(out.contains(&refused) && out.contains("rc=1"), "the sudo function refuses:\n{out}");
    assert!(!out.contains("no new privileges"), "the function refused before any real sudo ran:\n{out}");
    // The real binary, past the function: no_new_privs stops it; the hint says why.
    let out = run(Some(&b), &format!("export LC_ALL=C; if {gate}; then command sudo -n true; echo rc=$?; else echo nnp=0; fi"))
        .await
        .unwrap();
    assert!(out.contains("rc=1"), "{out}");
    assert!(
        out.contains("\"no new privileges\" flag is set") || out.contains("sudo must be owned by uid 0"),
        "sudo's own no_new_privs refusal:\n{out}"
    );
    assert!(out.contains(&refused), "the hint:\n{out}");
}

/// A command that only reads text quoting a sudo refusal (a doc, a source
/// file: sudo's own no_new_privs messages, or the sandboxed sudo function's)
/// ran no sudo: no sudo hint and no `sandbox.sudo_refused` line. A command
/// that runs sudo, the word anywhere in it (`cd d && sudo …`, `command sudo
/// …`), still gets both: one line each, with that command's tag.
#[tokio::test]
async fn bash_sandbox_local_reading_a_sudo_refusal_is_not_a_refused_sudo() {
    if !local_kernel() {
        return;
    }
    let fx = fx();
    let b = enforcing(&fx);
    let refused = format!("{SUDO_REFUSED} Ask your foreman.");
    let quoted = fx.root.join("lane/quoted.txt");
    let quoted_text = "sudo: The \"no new privileges\" flag is set, which prevents sudo from running as root.\n\
                       sudo: sudo must be owned by uid 0 and have the setuid bit set\n";
    fs::write(&quoted, quoted_text).expect("write the quoted refusals");
    let own = fx.root.join("lane/own.txt");
    fs::write(&own, format!("{refused}\n")).expect("write the function's refusal");
    let gate = "grep -q '^NoNewPrivs:[[:space:]]*1' /proc/self/status";
    let lane = fx.r("lane");
    // This test's runtime is current-thread: the tool decides, spawns and
    // checks the output on this thread, where the capture is the default
    // subscriber.
    let (out, guard) = crate::sandbox::log_tests::daemon_log();
    let read = run(Some(&b), &format!("cat {}", quoted.display())).await.unwrap();
    let read_own = run(Some(&b), &format!("cat {}", own.display())).await.unwrap();
    let function = run(Some(&b), &format!("if {gate}; then cd {lane} && sudo -n true; echo rc=$?; else echo nnp=0; fi"))
        .await
        .unwrap();
    let binary = run(
        Some(&b),
        &format!("export LC_ALL=C; if {gate}; then cd {lane} && command sudo -n true; echo rc=$?; else echo nnp=0; fi"),
    )
    .await
    .unwrap();
    drop(guard);

    assert!(read.contains("\"no new privileges\" flag is set"), "{read}");
    assert!(!read.contains(SUDO_REFUSED) && !read.contains("[sandbox]"), "a read gets no sudo hint:\n{read}");
    assert_eq!(read_own.matches(SUDO_REFUSED).count(), 1, "the file's own line only:\n{read_own}");
    assert!(function.contains("rc=1") && function.contains(&refused), "the sudo function refuses:\n{function}");
    assert!(binary.contains("rc=1") && binary.contains(&refused), "the real sudo's refusal gets the hint:\n{binary}");
    let text = out.text();
    let spawned: Vec<&str> = text.lines().filter(|l| l.contains("sandbox.spawn session=")).collect();
    let sudo: Vec<&str> = text.lines().filter(|l| l.contains("sandbox.sudo_refused ")).collect();
    assert_eq!(spawned.len(), 4, "one spawn per command:\n{text}");
    let tags: Vec<&str> = sudo.iter().map(|l| tag_of(l)).collect();
    assert_eq!(tags, vec![tag_of(spawned[2]), tag_of(spawned[3])], "lines for the two sudo runs only:\n{text}");
    assert!(sudo.iter().all(|l| l.contains(" WARN ") && l.contains("sandbox.sudo_refused session=s1 tool=bash tag=")), "{text}");
}

#[tokio::test]
async fn bash_sandbox_local_children_inherit_the_domain() {
    if !local_kernel() {
        return;
    }
    let fx = fx();
    let b = enforcing(&fx);
    let (lane, other) = (fx.r("lane"), fx.r("other"));
    let script = format!(
        "export LC_ALL=C\n\
         python3 -c \"open('{lane}/py', 'w').write('x')\"; echo py_in=$?\n\
         python3 -c \"open('{other}/py', 'w')\"; echo py_out=$?\n\
         git init -q {lane}/repo; echo git_in=$?\n\
         git init -q {other}/repo; echo git_out=$?\n\
         bash -c 'echo n > {other}/nested'; echo nested=$?\n\
         if [ \"${{SYNAPS_TEST_REAL_SCOPES:-}}\" = 1 ]; then\n\
           systemd-run --user --scope --quiet touch {lane}/sd; echo sd_in=$?\n\
           systemd-run --user --scope --quiet touch {other}/sd; echo sd_out=$?\n\
         fi\n\
         true"
    );
    let out = run(Some(&b), &script).await.unwrap();
    for want in ["py_in=0", "py_out=1", "git_in=0", "nested=1"] {
        assert!(out.lines().any(|l| l == want), "missing {want:?}:\n{out}");
    }
    assert!(out.lines().any(|l| l.starts_with("git_out=") && l != "git_out=0"), "{out}");
    if std::env::var("SYNAPS_TEST_REAL_SCOPES").as_deref() == Ok("1") {
        for want in ["sd_in=0", "sd_out=1"] {
            assert!(out.lines().any(|l| l == want), "systemd-run --scope: missing {want:?}:\n{out}");
        }
        assert!(fx.root.join("lane/sd").exists());
    }
    assert!(fx.root.join("lane/py").exists() && fx.root.join("lane/repo/.git/HEAD").exists());
    assert_eq!(fs::read_dir(fx.root.join("other")).unwrap().count(), 0, "nothing was written outside:\n{out}");
}

/// The tag of a `sandbox.spawn…` line.
fn tag_of(line: &str) -> &str {
    line.split_whitespace().find_map(|w| w.strip_prefix("tag=")).unwrap_or("-")
}

/// The spawn lines of a sandboxed bash run, through the daemon log's own
/// filter (`with_crate_directives` over an ERROR base): one
/// `sandbox.spawn` per child, one `sandbox.spawn_failed` per failed spawn,
/// each with its session, tool and own tag; a line outside the crate
/// directives is dropped (the control).
#[tokio::test]
async fn bash_sandbox_local_spawn_lines_reach_the_daemon_log() {
    if !local_kernel() {
        return;
    }
    let fx = fx();
    let b = enforcing(&fx);
    // This test's runtime is current-thread: the tool decides and spawns on
    // this thread, where the capture is the default subscriber.
    let (out, guard) = crate::sandbox::log_tests::daemon_log();
    run(Some(&b), &format!("echo a > {}/ok", fx.r("lane"))).await.expect("a sandboxed command runs");
    // A spawn that fails: its cwd doesn't exist, so the child's chdir fails
    // before exec and nothing runs.
    let mut ctx = create_tool_context();
    ctx.capabilities.sandbox = Some(Arc::clone(&b));
    ctx.capabilities.cwd = Some(fx.root.join("no-such-cwd"));
    BashTool.execute(json!({ "command": "true", "timeout": 30 }), ctx).await.expect_err("the spawn fails");
    tracing::warn!(target: "unrelated_crate::x", "a control line outside the crate directives");
    drop(guard);
    let text = out.text();
    let spawned: Vec<&str> = text.lines().filter(|l| l.contains("sandbox.spawn session=")).collect();
    let failed: Vec<&str> = text.lines().filter(|l| l.contains("sandbox.spawn_failed session=")).collect();
    assert_eq!((spawned.len(), failed.len()), (1, 1), "one line per spawn:\n{text}");
    assert!(spawned[0].contains(" WARN ") && spawned[0].contains("sandbox.spawn session=s1 tool=bash tag=sbx-"), "{text}");
    let pid = spawned[0].rsplit_once(" pid=").map(|(_, p)| p.trim()).unwrap_or("-");
    assert!(!pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit()), "a pid: {text}");
    assert!(failed[0].contains("sandbox.spawn_failed session=s1 tool=bash tag=sbx-") && failed[0].contains(" error="), "{text}");
    assert_ne!(tag_of(spawned[0]), tag_of(failed[0]), "each spawn has its own tag: {text}");
    assert!(text.contains("sandbox.policy session=s1 mode=enforce "), "the policy line is kept too: {text}");
    assert!(!text.contains("a control line outside the crate directives"), "the filter drops other targets: {text}");
}
