//! The kill guard's Seatbelt stand-in ([`super`]). Runs on every target;
//! the real sandbox-exec only on macOS. SAFETY: no test signals pid -1 or a
//! process group; signal 0 sends nothing, and the one real signal goes to a
//! `sleep` the test's own shell started.

use super::*;

fn os(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

/// The profile: everything allowed, signals only inside the sandbox. It
/// ends in a newline, so a later rule set can append to it.
#[test]
fn the_profile_denies_every_signal_but_to_its_own_sandbox() {
    let rules: Vec<&str> = PROFILE.lines().collect();
    assert_eq!(
        rules,
        [
            "(version 1)",
            "(allow default)",
            "(deny signal)",
            "(allow signal (target same-sandbox))"
        ]
    );
    assert!(PROFILE.ends_with('\n'));
}

/// `sandbox-exec -p <PROFILE> program args…`, arguments verbatim (an
/// argument that looks like an option stays the command's).
#[test]
fn seatbelt_wrap_puts_sandbox_exec_and_the_profile_before_the_command() {
    let (program, args) = seatbelt_wrap("/bin/bash", os(&["-c", "kill -0 $$; echo ${HOME}", "-p"]));
    assert_eq!(program, OsString::from(SANDBOX_EXEC));
    assert_eq!(
        args,
        os(&[
            "-p",
            PROFILE,
            "/bin/bash",
            "-c",
            "kill -0 $$; echo ${HOME}",
            "-p"
        ])
    );
}

/// `Direct` changes nothing, even for a program that doesn't exist (its
/// spawn fails as before). `Seatbelt` resolves the program by the child's
/// PATH first; one that can't be found is an error.
#[cfg(unix)]
#[test]
fn launch_apply_wraps_only_for_seatbelt_and_resolves_the_program_first() {
    let env = vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))];
    let args = os(&["-c", "echo ${HOME}"]);
    let direct = Launch::Direct.apply(
        "synaps-test-no-such-program".into(),
        args.clone(),
        &env,
        None,
    );
    assert_eq!(
        direct.expect("direct"),
        ("synaps-test-no-such-program".into(), args.clone())
    );

    let (program, wrapped) = Launch::Seatbelt
        .apply("sh".into(), args.clone(), &env, None)
        .expect("sh resolves");
    assert_eq!(program, OsString::from(SANDBOX_EXEC));
    assert_eq!(wrapped[..2], os(&["-p", PROFILE])[..]);
    let resolved = Path::new(&wrapped[2]);
    assert!(
        resolved.is_absolute() && resolved.ends_with("sh"),
        "{resolved:?}"
    );
    assert_eq!(wrapped[3..], args[..]);

    let missing = Launch::Seatbelt.apply("synaps-test-no-such-program".into(), args, &env, None);
    assert_eq!(
        missing.expect_err("not found").kind(),
        std::io::ErrorKind::NotFound
    );
}

/// A passed probe wraps every child and logs nothing.
#[test]
fn an_available_guard_wraps_every_child_without_a_warn() {
    let (log, _capture) = crate::tools::test_helpers::DaemonLog::capture();
    let guard = Guard::from_probe(Ok(()));
    for _ in 0..3 {
        assert_eq!(guard.launch(), Launch::Seatbelt);
    }
    assert!(
        log.find("agent_engine::seatbelt", tracing::Level::WARN, UNAVAILABLE)
            .is_empty(),
        "{:?}",
        log.lines()
    );
}

/// Fail-open: a failed probe starts every child unwrapped, with one WARN
/// (and its reason) however many children start.
#[test]
fn an_unavailable_guard_starts_children_unwrapped_with_one_warn() {
    let (log, _capture) = crate::tools::test_helpers::DaemonLog::capture();
    let guard = Guard::from_probe(Err("probe: test reason".into()));
    for _ in 0..3 {
        assert_eq!(guard.launch(), Launch::Direct);
    }
    let warns = log.find("agent_engine::seatbelt", tracing::Level::WARN, UNAVAILABLE);
    assert_eq!(warns.len(), 1, "{:?}", log.lines());
    let reason = warns[0].fields.get("reason").cloned().unwrap_or_default();
    assert!(reason.contains("probe: test reason"), "{reason}");
}

/// Off macOS nothing is probed and every child starts directly; on macOS
/// the answer is the process's probe. Here (Linux) the real probe finds no
/// sandbox-exec: no wrap.
#[test]
fn tool_child_is_direct_off_macos() {
    prepare();
    if cfg!(target_os = "macos") {
        assert_eq!(tool_child(), global().launch());
        return;
    }
    assert_eq!(tool_child(), Launch::Direct);
    if !Path::new(SANDBOX_EXEC).exists() {
        assert_eq!(global().launch(), Launch::Direct);
    }
}

/// A fake sandbox-exec in `dir`: a shell script with `body`. The caller
/// holds `test_env::SCRIPT_EXEC_LOCK` from before the write through its
/// last exec.
#[cfg(unix)]
fn fake_sandbox_exec(dir: &Path, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    assert!(
        crate::test_env::SCRIPT_EXEC_LOCK.held(),
        "take SCRIPT_EXEC_LOCK first"
    );
    let path = dir.join("sandbox-exec");
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).expect("write fake");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("chmod fake");
    path
}

/// The probe passes when sandbox-exec runs `true` cleanly, and runs it in
/// exactly the wrapped form a tool child gets.
#[cfg(unix)]
#[test]
fn the_probe_passes_when_sandbox_exec_runs_true_cleanly() {
    let _exec = crate::test_env::SCRIPT_EXEC_LOCK.lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let argv = dir.path().join("argv");
    let fake = fake_sandbox_exec(
        dir.path(),
        &format!(
            "printf '%s\\0' \"$@\" > '{}'\n[ \"$1\" = -p ] || exit 64\nshift 2\nexec \"$@\"\n",
            argv.display()
        ),
    );
    assert_eq!(probe(&fake, PROBE_TIMEOUT), Ok(()));
    let recorded = std::fs::read(&argv).expect("argv");
    let recorded: Vec<&[u8]> = recorded
        .split(|b| *b == 0)
        .filter(|a| !a.is_empty())
        .collect();
    assert_eq!(
        recorded,
        [
            b"-p".as_slice(),
            PROFILE.as_bytes(),
            PROBE_PROGRAM.as_bytes()
        ]
    );
}

/// Every way the probe fails says why: sandbox-exec missing, refused (a
/// nested sandbox: "Operation not permitted"), talking on stderr (it would
/// in every tool child), or hanging (no signal is sent; it exits on its own).
#[cfg(unix)]
#[test]
fn a_failed_probe_says_why() {
    let missing = probe(
        Path::new("/nonexistent/synaps-test/sandbox-exec"),
        PROBE_TIMEOUT,
    );
    assert!(
        missing.as_ref().is_err_and(|r| r.contains("cannot run it")),
        "{missing:?}"
    );

    let _exec = crate::test_env::SCRIPT_EXEC_LOCK.lock();
    let cases = [
        (
            "echo 'sandbox-exec: sandbox_apply: Operation not permitted' >&2\nexit 71\n",
            PROBE_TIMEOUT,
            vec!["exit code 71", "Operation not permitted"],
        ),
        (
            "printf 'x%.0s' $(seq 1 300) >&2\nshift 2\nexec \"$@\"\n",
            PROBE_TIMEOUT,
            vec!["exit code 0", "xxx…"],
        ),
        (
            "exec sleep 1\n",
            Duration::from_millis(100),
            vec!["no exit within 100ms"],
        ),
    ];
    for (body, timeout, says) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = fake_sandbox_exec(dir.path(), body);
        let reason = probe(&fake, timeout).expect_err(body);
        assert!(
            reason.starts_with(&format!(
                "probe: {} -p <profile> /usr/bin/true: ",
                fake.display()
            )),
            "{reason}"
        );
        for said in says {
            assert!(reason.contains(said), "{body}: {reason}");
        }
        assert!(reason.chars().count() < 400, "the stderr is cut: {reason}");
    }
}

/// On the Mac, the real thing: a wrapped shell can't signal this test (its
/// parent, outside the sandbox), but can signal itself and its own child.
/// Signal 0 sends nothing; the one real signal goes to the shell's own
/// `sleep`. When Seatbelt can't nest here (the test itself runs in a
/// sandbox), the probe says so and the test stops.
#[cfg(unix)]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "macOS only: needs /usr/bin/sandbox-exec"
)]
#[test]
fn on_the_mac_a_wrapped_shell_cannot_signal_outside_its_sandbox() {
    if let Err(reason) = probe(Path::new(SANDBOX_EXEC), PROBE_TIMEOUT) {
        assert!(
            reason.contains("not permitted"),
            "the probe failed: {reason}"
        );
        eprintln!("skipped, Seatbelt can't nest here: {reason}");
        return;
    }
    let script = "kill -0 $PPID; echo parent_rc=$?; kill -0 $$ && echo self=ok; \
                  sleep 30 >/dev/null 2>&1 & kill $! && echo child=ok";
    let (program, args) = seatbelt_wrap("/bin/sh", os(&["-c", script]));
    let out = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run sandbox-exec");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("parent_rc=1"), "{stdout}{stderr}");
    assert!(
        stderr.to_lowercase().contains("operation not permitted"),
        "{stdout}{stderr}"
    );
    assert!(
        stdout.contains("self=ok") && stdout.contains("child=ok"),
        "{stdout}{stderr}"
    );
}

/// The Seatbelt wrap goes exactly where the shim goes: every source that
/// adds the shim to a tool child's env (`killguard::apply_env`) also asks
/// [`tool_child`], and no other does.
#[test]
fn the_wrap_goes_exactly_where_the_shim_goes() {
    fn sources(dir: &Path, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).expect("read src") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).expect("read source");
                out.push((path.display().to_string(), text));
            }
        }
    }
    let mut all = Vec::new();
    sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut all);
    // This file names both calls.
    all.retain(|(path, _)| !path.ends_with("seatbelt/tests.rs"));
    let calling = |needle: &str| -> Vec<String> {
        let mut files: Vec<String> = all
            .iter()
            .filter(|(_, text)| text.contains(needle))
            .map(|(path, _)| path.clone())
            .collect();
        files.sort();
        files
    };
    let shim = calling("crate::killguard::apply_env(");
    let wrap = calling("crate::seatbelt::tool_child()");
    assert_eq!(shim.len(), 2, "bash and PTY: {shim:?}");
    assert_eq!(shim, wrap);
    assert!(
        wrap.iter().any(|p| p.ends_with("tools/bash.rs"))
            && wrap.iter().any(|p| p.ends_with("shell/pty.rs"))
    );
}

/// `scripts/seatbelt-proof.sh` (run on the Mac) tests the same profile,
/// byte for byte, with the same sandbox-exec, and never signals everything.
#[test]
fn the_proof_script_carries_the_same_profile() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/seatbelt-proof.sh");
    let script =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(script.matches("\nPROFILE='").count(), 1, "one PROFILE");
    let start = script.find("\nPROFILE='").expect("PROFILE") + "\nPROFILE='".len();
    let len = script[start..].find('\'').expect("closing quote");
    assert_eq!(&script[start..start + len], PROFILE);
    assert!(script.contains(&format!("\nSANDBOX_EXEC={SANDBOX_EXEC}\n")));
    for banned in [
        "kill -1",
        "kill -9 -1",
        "kill -KILL -1",
        "kill -s KILL -1",
        "kill -- -1",
        "pkill",
        "killall",
    ] {
        assert!(!script.contains(banned), "{banned}");
    }
}
