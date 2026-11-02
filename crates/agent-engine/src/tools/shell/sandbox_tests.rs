//! The PTY shell (`shell_start`, then `shell_send`) under a session's sandbox
//! policy. Every tree is a scratch dir outside the baseline directories
//! (`test_scratch_parent`: a fake home, base dir and work dirs).
//! `shell_sandbox_local_*` runs shells under a real Landlock domain: LOCAL
//! kernel only (not g1pro/WSL), opt in with SYNAPS_TEST_LANDLOCK=1. The rest
//! needs no kernel support: no domain is built.
//!
//! PTY commands are split on whitespace (no quoting), so the kernel tests run
//! a script file from the scratch tree.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use serial_test::serial;

use super::pty::{pty_output_snapshot, PtyHandle, PtyOutputSnapshot};
use super::{SessionManager, ShellConfig, ShellStartTool};
use crate::sandbox::landlock::{abi_version, Unavailable, MIN_ABI};
use crate::sandbox::policy::{test_scratch_parent, KillSwitch, PolicyEnv};
use crate::sandbox::spawn::SUDO_REFUSED;
use crate::sandbox::{SandboxBinding, SpawnSandbox};
use crate::tools::test_helpers::create_tool_context;
use crate::tools::Tool;
use crate::Result;

struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    env: PolicyEnv,
}

fn fx() -> Fx {
    let tmp = tempfile::Builder::new().prefix("sbx-shell-").tempdir_in(test_scratch_parent()).expect("tempdir");
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
    /// Session `s1`'s binding, with a test-owned kill switch.
    fn binding(&self, abi: Option<std::result::Result<u32, Unavailable>>) -> Arc<SandboxBinding> {
        SandboxBinding::with_env("s1", self.env.clone(), Box::leak(Box::new(KillSwitch::new())), abi)
    }
    /// A script in the work dir that `bash` runs (a PTY command has no quoting).
    fn script(&self, name: &str, text: &str) -> String {
        let path = self.root.join("lane").join(name);
        fs::write(&path, text).expect("write script");
        format!("bash {}", path.display())
    }
}

fn manager() -> Arc<SessionManager> {
    SessionManager::new(ShellConfig::default())
}

/// Every test here runs `#[serial]` with the PTY accounting tests in
/// `pty.rs`, which compare the process-wide PTY reader and byte counts
/// before and after: a shell of ours alive across their snapshots would
/// fail them. Made first in a test, so dropped last (after every shell and
/// manager): waits, bounded, until our shells' readers have ended and their
/// bytes are released, as those tests do themselves.
struct SettlePty(PtyOutputSnapshot);

impl SettlePty {
    fn new() -> Self {
        SettlePty(pty_output_snapshot())
    }
}

impl Drop for SettlePty {
    fn drop(&mut self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            let now = pty_output_snapshot();
            if now.active_readers <= self.0.active_readers && now.retained_bytes == self.0.retained_bytes {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// `shell_start` of `command` on `mgr`, under `sandbox`'s session binding.
async fn start(mgr: &Arc<SessionManager>, sandbox: Option<&Arc<SandboxBinding>>, command: &str) -> Result<String> {
    let mut ctx = create_tool_context();
    ctx.capabilities.session_manager = Some(Arc::clone(mgr));
    ctx.capabilities.sandbox = sandbox.cloned();
    // An interactive bash saves its history to $HOME/.bash_history on exit:
    // the developer's real one, unless the test says otherwise.
    let params = json!({
        "command": command,
        "env": { "LC_ALL": "C", "HISTFILE": "/dev/null" },
        "readiness_timeout_ms": 1000,
    });
    ShellStartTool.execute(params, ctx).await
}

/// The shell session id in a `shell_start` result (`[Session <id> | …]`).
fn session_id(result: &str) -> String {
    result.strip_prefix("[Session ").and_then(|r| r.split(' ').next()).expect("a session header").to_string()
}

/// A `shell_start` result without its first line (the session header).
fn body(result: &str) -> &str {
    result.split_once('\n').map(|(_, b)| b).unwrap_or("")
}

fn has_line(out: &str, want: &str) -> bool {
    out.lines().any(|l| l.trim() == want)
}

const TOO_OLD: Unavailable = Unavailable::AbiTooOld(2);

#[tokio::test]
#[serial]
async fn shell_sandbox_invalid_policy_refuses_and_runs_nothing() {
    let _settle = SettlePty::new();
    let fx = fx();
    fx.write_policy("mode = enforce\ncontact = Ask your foreman.\nbogus = 1\n");
    let marker = fx.root.join("lane/ran");
    let mgr = manager();
    let result = start(&mgr, Some(&fx.binding(Some(Ok(7)))), &format!("touch {}", marker.display())).await;
    let err = result.expect_err("an invalid policy refuses the shell").to_string();
    assert!(err.contains("sandbox policy invalid for this session ("), "{err}");
    assert!(err.contains("commands can't run until it's fixed. Ask your foreman."), "{err}");
    assert!(!marker.exists(), "nothing ran");
    assert_eq!(mgr.active_count(), 0, "no shell session was created");
}

#[tokio::test]
#[serial]
async fn shell_sandbox_a_refused_rule_refuses_and_runs_nothing() {
    let _settle = SettlePty::new();
    let fx = fx();
    fx.write_policy(&format!("mode = enforce\nwrite = {}\ncontact = Ask your foreman.\n", fx.r("lane")));
    let marker = fx.root.join("lane/ran");
    let mgr = manager();
    let b = SandboxBinding::with_failing_build("s1", fx.env.clone(), Box::leak(Box::new(KillSwitch::new())), Some(Ok(7)));
    let result = start(&mgr, Some(&b), &format!("touch {}", marker.display())).await;
    let err = result.expect_err("a refused rule refuses the shell").to_string();
    let want = format!("sandbox policy could not be applied for this session (rule for `{}` refused: ", fx.r("lane"));
    assert!(err.contains(&want), "{err}");
    assert!(!marker.exists(), "nothing ran: the rules are built before any child exists");
    assert_eq!(mgr.active_count(), 0, "no shell session was created");
}

/// No binding, no policy file, `mode = off`, and an enforcing policy on a
/// host without usable Landlock (not required): the shell runs as before.
#[tokio::test]
#[serial]
async fn shell_sandbox_off_is_unchanged() {
    let _settle = SettlePty::new();
    let fx = fx();
    let marker = |n: &str| fx.root.join("other").join(n);
    let run = |b: Option<Arc<SandboxBinding>>, n: &'static str| {
        let mgr = manager();
        let cmd = fx.script(&format!("{n}.sh"), &format!("touch {}\necho out=$?\n", marker(n).display()));
        async move { start(&mgr, b.as_ref(), &cmd).await.expect("the shell runs") }
    };
    let plain = run(None, "plain").await;
    assert!(has_line(body(&plain), "out=0"), "{plain}");
    let no_policy = run(Some(fx.binding(Some(Ok(7)))), "no-policy").await;
    fx.write_policy("mode = off\n");
    let off = run(Some(fx.binding(Some(Ok(7)))), "off").await;
    fx.write_policy(&format!("mode = enforce\nwrite = {}\n", fx.r("lane")));
    let unavailable = run(Some(fx.binding(Some(Err(TOO_OLD)))), "unavailable").await;
    for (what, out) in [("no policy", &no_policy), ("mode = off", &off), ("unavailable, not required", &unavailable)] {
        assert_eq!(body(out), body(&plain), "{what}: the same output");
    }
    for n in ["plain", "no-policy", "off", "unavailable"] {
        assert!(marker(n).exists(), "{n}: the write outside ran");
    }
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

/// This thread's `NoNewPrivs` (1 once a domain was applied to it).
fn this_thread_no_new_privs() -> String {
    let status = fs::read_to_string("/proc/thread-self/status").expect("thread status");
    status.lines().find_map(|l| l.strip_prefix("NoNewPrivs:")).map(|v| v.trim().to_string()).expect("NoNewPrivs line")
}

#[tokio::test]
#[serial]
async fn shell_sandbox_local_inside_works_outside_is_refused() {
    let _settle = SettlePty::new();
    if !local_kernel() {
        return;
    }
    let fx = fx();
    let victim = fx.r("other/victim");
    fs::write(&victim, "keep\n").unwrap();
    let nnp_before = this_thread_no_new_privs();
    let b = enforcing(&fx);
    let mgr = manager();
    let cmd = fx.script(
        "x.sh",
        &format!(
            "touch {lane}/in; echo in=$?\n\
             touch {other}/out; echo out=$?\n\
             rm -f {victim}; echo rm=$?\n\
             grep '^NoNewPrivs:' /proc/self/status\n",
            lane = fx.r("lane"),
            other = fx.r("other"),
        ),
    );
    let out = start(&mgr, Some(&b), &cmd).await.expect("a sandboxed shell runs");
    for want in ["in=0", "out=1", "rm=1"] {
        assert!(has_line(&out, want), "missing {want:?}:\n{out}");
    }
    assert!(out.lines().any(|l| l.split_whitespace().collect::<Vec<_>>() == ["NoNewPrivs:", "1"]), "{out}");
    assert!(fx.root.join("lane/in").exists(), "the write inside landed");
    assert!(!fx.root.join("other/out").exists(), "the write outside was refused");
    assert_eq!(fs::read_to_string(&victim).unwrap(), "keep\n");
    // Only the shell is confined: the thread that started it is not. Its
    // no_new_privs is unchanged, not "0": a test run from a sandboxed
    // session starts with 1 (as in landlock_local_confined_thread_leaves_the_caller_unrestricted).
    assert_eq!(this_thread_no_new_privs(), nnp_before, "the caller's thread is unrestricted");
    fs::write(fx.root.join("other/after"), "x").expect("the caller still writes outside the set");
}

/// The decision is made at `shell_start`: commands sent to the shell later
/// run inside the same domain.
#[tokio::test]
#[serial]
async fn shell_sandbox_local_commands_sent_later_run_inside_the_shell() {
    let _settle = SettlePty::new();
    if !local_kernel() {
        return;
    }
    let fx = fx();
    let b = enforcing(&fx);
    let mgr = manager();
    let started = start(&mgr, Some(&b), "bash --norc --noprofile").await.expect("a sandboxed shell starts");
    let id = session_id(&started);
    let input = format!(
        "touch {}/late; echo late_in=$?; touch {}/late; echo late_out=$?\n",
        fx.r("lane"),
        fx.r("other")
    );
    let sent = mgr.send_input(&id, &input, Some(1000), None, 64 * 1024).await.expect("send");
    let _ = mgr.close_session(&id).await;
    for want in ["late_in=0", "late_out=1"] {
        assert!(has_line(&sent.output, want), "missing {want:?}:\n{}", sent.output);
    }
    assert!(fx.root.join("lane/late").exists());
    assert!(!fx.root.join("other/late").exists(), "the later command ran inside the domain");
}

/// The tag of a `sandbox.spawn…` line.
fn tag_of(line: &str) -> &str {
    line.split_whitespace().find_map(|w| w.strip_prefix("tag=")).unwrap_or("-")
}

/// A sandboxed shell's spawn line, through the daemon log's own filter
/// (`with_crate_directives` over an ERROR base): one `sandbox.spawn` with
/// tool=shell_start, the session, a tag and the pid; a line outside the
/// crate directives is dropped (the control).
#[tokio::test]
#[serial]
async fn shell_sandbox_local_spawn_line_reaches_the_daemon_log() {
    let _settle = SettlePty::new();
    if !local_kernel() {
        return;
    }
    let fx = fx();
    let b = enforcing(&fx);
    let mgr = manager();
    let cmd = fx.script("ok.sh", &format!("echo a > {}/ok\n", fx.r("lane")));
    // This test's runtime is current-thread: the tool decides and spawns on
    // this thread, where the capture is the default subscriber.
    let (out, guard) = crate::sandbox::log_tests::daemon_log();
    start(&mgr, Some(&b), &cmd).await.expect("a sandboxed shell runs");
    tracing::warn!(target: "unrelated_crate::x", "a control line outside the crate directives");
    drop(guard);
    let text = out.text();
    let spawned: Vec<&str> = text.lines().filter(|l| l.contains("sandbox.spawn session=")).collect();
    assert_eq!(spawned.len(), 1, "one line per shell:\n{text}");
    assert!(spawned[0].contains(" WARN ") && spawned[0].contains("sandbox.spawn session=s1 tool=shell_start tag=sbx-"), "{text}");
    assert!(tag_of(spawned[0]).len() > "sbx-".len(), "{text}");
    let pid = spawned[0].rsplit_once(" pid=").map(|(_, p)| p.trim()).unwrap_or("-");
    assert!(!pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit()), "a pid: {text}");
    assert!(!text.contains("sandbox.spawn_failed"), "{text}");
    assert!(text.contains("sandbox.policy session=s1 mode=enforce "), "the policy line is kept too: {text}");
    assert!(!text.contains("a control line outside the crate directives"), "the filter drops other targets: {text}");
    assert!(fx.root.join("lane/ok").exists());
}

/// What a sandboxed shell prints is checked as bash's output is: a refused
/// write outside the rules gets the hint, and a `sandbox.denial_suspected`
/// line with tool=shell_start and that shell's own tag reaches the daemon
/// log through its own filter. Checked: shell_start's first output (shell 1,
/// a script), each shell_send (shell 2, interactive) and the output read at
/// shell_end (a background job's write, refused after its send returned).
/// Clean output gets no hint; a line outside the crate directives is
/// dropped (the control). The class isn't asserted: it depends on where the
/// scratch tree is.
#[tokio::test]
#[serial]
async fn shell_sandbox_local_denials_get_the_hint_and_reach_the_daemon_log() {
    let _settle = SettlePty::new();
    if !local_kernel() {
        return;
    }
    let fx = fx();
    let b = enforcing(&fx);
    let mgr = manager();
    let hint = |path: &str| {
        format!(
            "[sandbox] writes outside this session's write set are refused: {path}. Allowed: {}, /tmp. \
             Ask your foreman. Do not work around it.",
            fx.r("lane")
        )
    };
    let (first, second, third) = (fx.r("other/first"), fx.r("other/second"), fx.r("other/third"));
    let done = fx.root.join("lane/done");
    // This test's runtime is current-thread: the tool decides, spawns and
    // checks the output on this thread, where the capture is the default
    // subscriber.
    let (out, guard) = crate::sandbox::log_tests::daemon_log();
    let cmd = fx.script("deny.sh", &format!("touch {first}\necho first=$?\n"));
    let started = start(&mgr, Some(&b), &cmd).await.expect("a sandboxed shell runs");
    let _ = mgr.close_session(&session_id(&started)).await;
    let shell = start(&mgr, Some(&b), "bash --norc --noprofile").await.expect("a sandboxed shell starts");
    let id = session_id(&shell);
    let clean_in = format!("touch {}/fine; echo clean=$?\n", fx.r("lane"));
    let clean = mgr.send_input(&id, &clean_in, Some(1000), None, 64 * 1024).await.expect("send").output;
    let denied_in = format!("touch {second}; echo second=$?\n");
    let denied = mgr.send_input(&id, &denied_in, Some(1000), None, 64 * 1024).await.expect("send").output;
    // The job writes 3 s later, long after this send's 300 ms of silence.
    let late_in = format!("(sleep 3; touch {third}; touch {}) &\n", done.display());
    let late = mgr.send_input(&id, &late_in, Some(300), None, 64 * 1024).await.expect("send").output;
    // The refused write's message is printed before `done` exists.
    for _ in 0..300 {
        if done.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let closed = mgr.close_session(&id).await.expect("close");
    tracing::warn!(target: "unrelated_crate::x", "a control line outside the crate directives");
    drop(guard);

    assert!(has_line(body(&started), "first=1"), "{started}");
    assert!(started.contains(&hint(&first)), "shell_start's output carries the hint:\n{started}");
    assert!(!shell.contains("[sandbox]"), "no hint on clean output:\n{shell}");
    assert!(has_line(&clean, "clean=0") && !clean.contains("[sandbox]"), "no hint on clean output:\n{clean}");
    assert!(has_line(&denied, "second=1"), "{denied}");
    assert!(denied.contains(&hint(&second)), "shell_send's output carries the hint:\n{denied}");
    assert!(!late.contains("[sandbox]"), "the job's write comes after this send:\n{late}");
    assert!(done.exists(), "the job ran");
    assert!(closed.contains(&hint(&third)), "shell_end's output carries the hint:\n{closed}");
    for n in ["first", "second", "third"] {
        assert!(!fx.root.join("other").join(n).exists(), "{n}: the write outside was refused");
    }

    let text = out.text();
    let spawned: Vec<&str> = text.lines().filter(|l| l.contains("sandbox.spawn session=")).collect();
    let denials: Vec<&str> = text.lines().filter(|l| l.contains("sandbox.denial_suspected ")).collect();
    assert_eq!((spawned.len(), denials.len()), (2, 3), "one spawn per shell, one line per refused write:\n{text}");
    let (tag1, tag2) = (tag_of(spawned[0]), tag_of(spawned[1]));
    assert_ne!(tag1, tag2, "each shell has its own tag:\n{text}");
    for (line, (tag, path)) in denials.iter().zip([(tag1, &first), (tag2, &second), (tag2, &third)]) {
        assert!(line.contains(" WARN "), "{text}");
        let who = format!("sandbox.denial_suspected session=s1 tool=shell_start tag={tag} class=");
        assert!(line.contains(&who) && line.contains(&format!(" path={path} src=output")), "{line}\n{text}");
    }
    assert!(!text.contains("a control line outside the crate directives"), "the filter drops other targets: {text}");
}

/// The streaming runtime takes a streamed call's model history from its
/// delta lane, not from the tool's return value: the hint is in the lane
/// too, for `shell_start` and `shell_send`.
#[tokio::test]
#[serial]
async fn shell_sandbox_local_the_hint_reaches_the_model_history() {
    let _settle = SettlePty::new();
    if !local_kernel() {
        return;
    }
    let fx = fx();
    let b = enforcing(&fx);
    let mgr = manager();
    let (first, second) = (fx.r("other/first"), fx.r("other/second"));
    let hint = |path: &str| format!("[sandbox] writes outside this session's write set are refused: {path}. ");
    let start_lane = crate::tools::output::delta_channel();
    let start_history = start_lane.output_handle();
    let _start_rx = start_lane.receiver;
    let mut ctx = create_tool_context();
    ctx.capabilities.session_manager = Some(Arc::clone(&mgr));
    ctx.capabilities.sandbox = Some(Arc::clone(&b));
    ctx.channels.tx_delta = Some(start_lane.sender);
    let cmd = fx.script("deny-lane.sh", &format!("touch {first}\necho first=$?\nexec bash --norc --noprofile\n"));
    let params = json!({
        "command": cmd,
        "env": { "LC_ALL": "C", "HISTFILE": "/dev/null" },
        "readiness_timeout_ms": 1000,
    });
    let started = ShellStartTool.execute(params, ctx).await.expect("a sandboxed shell starts");
    let id = session_id(&started);
    let send_lane = crate::tools::output::delta_channel();
    let send_history = send_lane.output_handle();
    let _send_rx = send_lane.receiver;
    let sent = mgr
        .send_input(&id, &format!("touch {second}; echo second=$?\n"), Some(1000), Some(&send_lane.sender), 64 * 1024)
        .await
        .expect("send")
        .output;
    let _ = mgr.close_session(&id).await;

    assert!(started.contains(&hint(&first)), "{started}");
    let text = start_history.model_history().text;
    assert!(text.contains("first=1") && text.contains(&hint(&first)), "shell_start's lane carries the hint:\n{text}");
    assert!(sent.contains(&hint(&second)), "{sent}");
    let text = send_history.model_history().text;
    assert!(text.contains("second=1") && text.contains(&hint(&second)), "shell_send's lane carries the hint:\n{text}");
    for n in ["first", "second"] {
        assert!(!fx.root.join("other").join(n).exists(), "{n}: the write outside was refused");
    }
}

/// The PTY form of bash's check: a `shell_send` that only reads text
/// quoting a sudo refusal (sudo's no_new_privs messages, or the sandboxed
/// sudo function's, which a PTY shell doesn't have) gets no sudo hint and
/// no `sandbox.sudo_refused` line; one that runs sudo (`cd d && sudo …`)
/// gets both.
#[tokio::test]
#[serial]
async fn shell_sandbox_local_reading_a_sudo_refusal_is_not_a_refused_sudo() {
    let _settle = SettlePty::new();
    if !local_kernel() {
        return;
    }
    let fx = fx();
    let b = enforcing(&fx);
    let mgr = manager();
    let refused = format!("{SUDO_REFUSED} Ask your foreman.");
    let quoted = fx.root.join("lane/quoted.txt");
    let quoted_text = "sudo: The \"no new privileges\" flag is set, which prevents sudo from running as root.\n\
                       sudo: sudo must be owned by uid 0 and have the setuid bit set\n";
    fs::write(&quoted, quoted_text).expect("write the quoted refusals");
    let own = fx.root.join("lane/own.txt");
    fs::write(&own, format!("{refused}\n")).expect("write the function's refusal");
    let gate = "grep -q '^NoNewPrivs:[[:space:]]*1' /proc/self/status";
    // This test's runtime is current-thread: the tool decides, spawns and
    // checks the output on this thread, where the capture is the default
    // subscriber.
    let (out, guard) = crate::sandbox::log_tests::daemon_log();
    let shell = start(&mgr, Some(&b), "bash --norc --noprofile").await.expect("a sandboxed shell starts");
    let id = session_id(&shell);
    let send = |input: String| {
        let (mgr, id) = (Arc::clone(&mgr), id.clone());
        async move { mgr.send_input(&id, &input, Some(1000), None, 64 * 1024).await.expect("send").output }
    };
    let read = send(format!("cat {}; echo read=$?\n", quoted.display())).await;
    let read_own = send(format!("cat {}; echo read_own=$?\n", own.display())).await;
    let ran = send(format!("if {gate}; then cd {} && sudo -n true; echo rc=$?; else echo nnp=0; fi\n", fx.r("lane"))).await;
    let _ = mgr.close_session(&id).await;
    drop(guard);

    assert!(has_line(&read, "read=0") && read.contains("\"no new privileges\" flag is set"), "{read}");
    assert!(!read.contains(SUDO_REFUSED) && !read.contains("[sandbox]"), "a read gets no sudo hint:\n{read}");
    assert!(has_line(&read_own, "read_own=0"), "{read_own}");
    assert!(has_line(&ran, "rc=1") && ran.contains(&refused), "a sudo run's refusal gets the hint:\n{ran}");
    let text = out.text();
    let sudo: Vec<&str> = text.lines().filter(|l| l.contains("sandbox.sudo_refused ")).collect();
    assert_eq!(sudo.len(), 1, "one line, for the sudo run only:\n{text}");
    assert!(sudo[0].contains(" WARN ") && sudo[0].contains("sandbox.sudo_refused session=s1 tool=shell_start tag=sbx-"), "{text}");
}

/// `shell_send` turns literal escapes (`\n`, `\t`) into real bytes before
/// the shell gets them; the sudo check reads those bytes. A sudo right after
/// a literal escape is a sudo run (the unprocessed text reads `nsudo`,
/// `tsudo`). `--noediting`: no readline, so a tab is a tab, not completion.
#[tokio::test]
#[serial]
async fn shell_sandbox_local_sudo_after_a_literal_escape_is_a_refused_sudo() {
    let _settle = SettlePty::new();
    if !local_kernel() {
        return;
    }
    let fx = fx();
    let b = enforcing(&fx);
    let mgr = manager();
    let refused = format!("{SUDO_REFUSED} Ask your foreman.");
    let gate = "grep -q '^NoNewPrivs:[[:space:]]*1' /proc/self/status";
    let (out, guard) = crate::sandbox::log_tests::daemon_log();
    let shell = start(&mgr, Some(&b), "bash --noediting --norc --noprofile").await.expect("a sandboxed shell starts");
    let id = session_id(&shell);
    let send = |input: String| {
        let (mgr, id) = (Arc::clone(&mgr), id.clone());
        async move { mgr.send_input(&id, &input, Some(1000), None, 64 * 1024).await.expect("send").output }
    };
    // Without no_new_privs the shell exits here, so no later send (and no
    // sudo) runs: send_input fails on the exited session.
    let gated = send(format!("{gate} || exit 9; cd {} && echo gated=$?\n", fx.r("lane"))).await;
    // Literal backslash escapes, as a model writes them.
    let newline = send("echo ready\\nsudo -n true; echo nl=$?\\n".to_string()).await;
    let tab = send("true;\\tsudo -n true; echo tab=$?\\n".to_string()).await;
    let _ = mgr.close_session(&id).await;
    drop(guard);

    assert!(has_line(&gated, "gated=0"), "{gated}");
    assert!(has_line(&newline, "nl=1") && newline.contains(&refused), "a sudo after a literal \\n gets the hint:\n{newline}");
    assert!(has_line(&tab, "tab=1") && tab.contains(&refused), "a sudo after a literal \\t gets the hint:\n{tab}");
    let text = out.text();
    let sudo = text.lines().filter(|l| l.contains("sandbox.sudo_refused ")).count();
    assert_eq!(sudo, 2, "one line per sudo run:\n{text}");
}

/// A sandboxed spawn that fails inside the confined thread (the program
/// doesn't exist) logs one `sandbox.spawn_failed` line and no
/// `sandbox.spawn`, and nothing runs. Called below the tool without a scope
/// launcher, so the failure is portable-pty's own (with a launcher, the
/// program is resolved before the spawn and nothing reaches the thread).
#[tokio::test]
#[serial]
async fn shell_sandbox_local_a_failed_spawn_is_logged() {
    let _settle = SettlePty::new();
    if !local_kernel() {
        return;
    }
    let fx = fx();
    let b = enforcing(&fx);
    let (out, guard) = crate::sandbox::log_tests::daemon_log();
    let sandbox = b.prepare("shell_start");
    assert!(matches!(sandbox, SpawnSandbox::Apply(_)), "an enforcing policy applies");
    let prepared = (crate::process_scope::inherited_env(), None, None);
    let result = PtyHandle::spawn_prepared("synaps-test-no-such-program", None, 24, 80, prepared, &sandbox);
    drop(guard);
    let err = match result {
        Ok(_) => panic!("a missing program can't be spawned"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("Failed to spawn command"), "{err}");
    let text = out.text();
    let failed: Vec<&str> = text.lines().filter(|l| l.contains("sandbox.spawn_failed session=")).collect();
    assert_eq!(failed.len(), 1, "one line per failed spawn:\n{text}");
    assert!(failed[0].contains("sandbox.spawn_failed session=s1 tool=shell_start tag=sbx-") && failed[0].contains(" error="), "{text}");
    assert!(!text.contains("sandbox.spawn session="), "no spawn line: {text}");
}
