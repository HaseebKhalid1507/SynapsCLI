//! The kill guard ([`super`]). SAFETY: every signal a test sends to pid -1
//! or process group 1 is signal 0, which sends nothing, so a run without
//! the shim (red) is harmless. A real signal goes only to a pid the test's
//! own shell started, checked > 1 first.

use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::{Command, Stdio};

type Env = Vec<(OsString, OsString)>;

/// A guard installed in a fresh `<tmp>/lib`, and the env a tool child gets
/// through it.
fn guarded() -> (tempfile::TempDir, KillGuard, Env) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let guard = KillGuard::install_in(&tmp.path().join("lib"));
    let mut env = crate::process_scope::inherited_env();
    guard.apply_env(&mut env);
    (tmp, guard, env)
}

/// This process's env without any preload: the control, an unguarded child.
fn bare_env() -> Env {
    let mut env = crate::process_scope::inherited_env();
    env.retain(|(k, _)| k != "LD_PRELOAD");
    env
}

fn value(env: &[(OsString, OsString)], key: &str) -> Option<String> {
    env.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.to_string_lossy().into_owned())
}

fn pairs(list: &[(&str, &str)]) -> Env {
    list.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect()
}

/// The embedded shim's file name.
fn shim_file_name() -> String {
    file_name(SHIM)
}

#[derive(Debug)]
struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run(program: &str, args: &[&str], env: &[(OsString, OsString)]) -> Ran {
    let out = Command::new(program)
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("run {program}: {e}"));
    Ran {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// (a) kill(-1, 0) from a guarded child fails with EPERM; without the guard
/// the same command succeeds.
#[test]
fn a_guarded_child_cannot_signal_pid_minus_one() {
    let (_tmp, guard, env) = guarded();
    assert!(guard.shim_path().is_some(), "installed");
    let script = "kill -0 -1; echo rc=$?";
    let bare = run("bash", &["-c", script], &bare_env());
    assert!(bare.stdout.contains("rc=0"), "control, no guard: {bare:?}");
    let ran = run("bash", &["-c", script], &env);
    assert!(ran.stdout.contains("rc=1"), "{ran:?}");
    assert!(ran.stderr.contains("Operation not permitted"), "{ran:?}");
}

/// (a) Python's `os.killpg(1, 0)` (glibc's killpg(1) is kill(-1) inside
/// libc) raises PermissionError in a guarded child; without it, it succeeds.
#[test]
fn a_guarded_python_gets_permission_error_from_killpg_1() {
    let python = Command::new("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !python {
        eprintln!("skipped: python3 is not on this host");
        return;
    }
    let (_tmp, _guard, env) = guarded();
    let script = "import os\ntry:\n    os.killpg(1, 0)\n    print('killpg=ok')\nexcept PermissionError:\n    print('killpg=EPERM')\n";
    let bare = run("python3", &["-c", script], &bare_env());
    assert!(bare.stdout.contains("killpg=ok"), "control, no guard: {bare:?}");
    let ran = run("python3", &["-c", script], &env);
    assert!(ran.stdout.contains("killpg=EPERM"), "{ran:?}");
}

/// (b) A guarded child still signals itself and what it started itself.
#[test]
fn b_a_guarded_child_can_still_kill_its_own_child() {
    let (_tmp, _guard, env) = guarded();
    let script = "kill -0 -1 2>/dev/null; echo minus1=$?\n\
                  kill -0 $$ && echo self=ok\n\
                  sleep 30 & p=$!\n\
                  if [ \"$p\" -gt 1 ]; then kill \"$p\"; wait \"$p\"; echo status=$?; else echo no-child; fi\n";
    let ran = run("bash", &["-c", script], &env);
    assert!(ran.stdout.contains("minus1=1"), "the child ran guarded: {ran:?}");
    assert!(ran.stdout.contains("self=ok"), "{ran:?}");
    assert!(ran.stdout.contains("status=143"), "{ran:?}");
}

/// Setuid programs (`--version`/`--help` only) that may be on a host.
const SETUID_CANDIDATES: &[(&str, &str)] = &[
    ("/usr/bin/mount", "--version"),
    ("/bin/mount", "--version"),
    ("/usr/bin/umount", "--version"),
    ("/usr/bin/su", "--help"),
    ("/bin/su", "--help"),
    ("/usr/bin/passwd", "--help"),
    ("/usr/bin/chsh", "--help"),
    ("/usr/bin/chfn", "--help"),
];

/// (c) A setuid program runs with a guarded env and ld.so prints nothing
/// (glibc ignores a preload with a slash in secure-exec mode). A snap app
/// too, where one is installed.
#[test]
fn c_a_setuid_program_runs_with_the_guarded_env() {
    let (_tmp, guard, env) = guarded();
    assert!(guard.shim_path().is_some(), "installed");
    let quiet_loader = |ran: &Ran| !ran.stderr.contains("ld.so") && !ran.stderr.contains("LD_PRELOAD");
    let setuid = SETUID_CANDIDATES
        .iter()
        .find(|(p, _)| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.mode() & 0o4000 != 0));
    match setuid {
        None => eprintln!("setuid check skipped: none of {SETUID_CANDIDATES:?} is setuid on this host"),
        Some((program, arg)) => {
            let ran = run(program, &[arg], &env);
            assert_eq!(ran.code, Some(0), "{program} {arg}: {ran:?}");
            assert!(quiet_loader(&ran), "{program} {arg}: {ran:?}");
            assert!(!format!("{}{}", ran.stdout, ran.stderr).trim().is_empty(), "{ran:?}");
        }
    }
    let gcloud = "/snap/bin/gcloud";
    if Path::new(gcloud).exists() {
        let ran = run(gcloud, &["--version"], &env);
        assert_eq!(ran.code, Some(0), "{gcloud}: {ran:?}");
        assert!(quiet_loader(&ran), "{gcloud}: {ran:?}");
    } else {
        eprintln!("snap check skipped: {gcloud} is not on this host");
    }
}

/// (d) Fail-open: a dir that can't be written, or a path LD_PRELOAD can't
/// carry, leaves the env as it was, with one WARN however often it's applied.
#[test]
fn d_an_unusable_dir_leaves_the_env_unchanged_with_one_warn() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let file = tmp.path().join("base-is-a-file");
    std::fs::write(&file, "").expect("write");
    let spaced = tmp.path().join("with space");
    let colon = tmp.path().join("with:colon");
    let cases = [
        (file.join("lib"), "unwritable"),
        (spaced.join("lib"), "space"),
        (colon.join("lib"), "colon"),
        (PathBuf::from("killguard-relative-test/lib"), "relative"),
    ];
    for (dir, why) in cases {
        let (log, _capture) = crate::tools::test_helpers::DaemonLog::capture();
        let guard = KillGuard::install_in(&dir);
        assert!(guard.shim_path().is_none(), "{why}");
        let before = pairs(&[("PATH", "/usr/bin:/bin"), ("LD_PRELOAD", "/x/libmine.so"), ("ASAN_OPTIONS", "a=1")]);
        for _ in 0..3 {
            let mut env = before.clone();
            guard.apply_env(&mut env);
            assert_eq!(env, before, "{why}");
        }
        let warns = log.find("agent_engine::killguard", tracing::Level::WARN, UNAVAILABLE);
        assert_eq!(warns.len(), 1, "{why}: {:?}", log.lines());
        assert!(warns[0].fields.contains_key("reason"), "{why}: {:?}", warns[0]);
    }
    // Nothing was written where it couldn't be used.
    assert!(!spaced.exists() && !colon.exists());
    assert!(!Path::new("killguard-relative-test").exists());
}

/// (e) ASAN_OPTIONS gets `verify_asan_link_order=0` once; an existing
/// LD_PRELOAD is kept and ours added once.
#[test]
fn e_asan_options_and_an_existing_preload() {
    let (_tmp, guard, _) = guarded();
    let shim = guard.shim_path().expect("installed").display().to_string();
    let apply = |list: &[(&str, &str)]| {
        let mut env = pairs(list);
        guard.apply_env(&mut env);
        env
    };
    let asan = |list: &[(&str, &str)]| value(&apply(list), "ASAN_OPTIONS");
    assert_eq!(asan(&[]).as_deref(), Some(ASAN_LINK_ORDER_OFF));
    assert_eq!(asan(&[("ASAN_OPTIONS", "")]).as_deref(), Some(ASAN_LINK_ORDER_OFF));
    assert_eq!(asan(&[("ASAN_OPTIONS", "a=1")]).as_deref(), Some("a=1:verify_asan_link_order=0"));
    for kept in ["a=1:verify_asan_link_order=0", "verify_asan_link_order=0", "verify_asan_link_order=1,verify_asan_link_order=0"] {
        assert_eq!(asan(&[("ASAN_OPTIONS", kept)]).as_deref(), Some(kept));
    }
    // ASan takes a flag's last value: ours goes after a different one.
    assert_eq!(
        asan(&[("ASAN_OPTIONS", "verify_asan_link_order=1,a=1")]).as_deref(),
        Some("verify_asan_link_order=1,a=1:verify_asan_link_order=0")
    );
    assert_eq!(asan(&[("ASAN_OPTIONS", "a=1:")]).as_deref(), Some("a=1:verify_asan_link_order=0"));
    let preload = |list: &[(&str, &str)]| value(&apply(list), "LD_PRELOAD");
    assert_eq!(preload(&[]), Some(shim.clone()));
    assert_eq!(preload(&[("LD_PRELOAD", "/x/libmine.so")]), Some(format!("/x/libmine.so:{shim}")));
    assert_eq!(preload(&[("LD_PRELOAD", "/x/a.so /x/b.so")]), Some(format!("/x/a.so /x/b.so:{shim}")));
    let ours_first = format!("{shim} /x/a.so");
    assert_eq!(preload(&[("LD_PRELOAD", ours_first.as_str())]), Some(ours_first.clone()));
    // Applied twice: still once, and one entry per variable.
    let mut env = pairs(&[("LD_PRELOAD", "/x/libmine.so"), ("ASAN_OPTIONS", "a=1")]);
    guard.apply_env(&mut env);
    guard.apply_env(&mut env);
    assert_eq!(value(&env, "LD_PRELOAD"), Some(format!("/x/libmine.so:{shim}")));
    assert_eq!(value(&env, "ASAN_OPTIONS").as_deref(), Some("a=1:verify_asan_link_order=0"));
    assert_eq!(env.iter().filter(|(k, _)| k == "LD_PRELOAD").count(), 1);
    assert_eq!(env.iter().filter(|(k, _)| k == "ASAN_OPTIONS").count(), 1);
}

/// The shim is written content-addressed (dir 0700, file 0644), reused when
/// its hash matches, rewritten when it doesn't, and an older version beside
/// it is never deleted.
#[test]
fn the_shim_is_content_addressed_reused_and_rewritten() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("base").join("lib");
    let guard = KillGuard::install_in(&dir);
    let path = guard.shim_path().expect("installed").to_path_buf();
    let name = shim_file_name();
    assert_eq!(path, dir.join(&name));
    let hex = name.strip_prefix("synaps-killguard-").and_then(|n| n.strip_suffix(".so")).expect("name");
    assert!(hex.len() == 12 && hex.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()), "{name}");
    assert_eq!(std::fs::read(&path).expect("read"), SHIM);
    assert_eq!(std::fs::metadata(&dir).expect("dir").permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(&path).expect("file").permissions().mode() & 0o777, 0o644);

    let inode = std::fs::metadata(&path).expect("file").ino();
    let again = KillGuard::install_in(&dir);
    assert_eq!(again.shim_path(), Some(path.as_path()));
    assert_eq!(std::fs::metadata(&path).expect("file").ino(), inode, "reused, not rewritten");

    let older = dir.join("synaps-killguard-000000000000.so");
    std::fs::write(&older, b"an older build's shim").expect("write");
    std::fs::write(&path, b"not the shim").expect("write");
    let fixed = KillGuard::install_in(&dir);
    assert_eq!(fixed.shim_path(), Some(path.as_path()));
    assert_eq!(std::fs::read(&path).expect("read"), SHIM, "rewritten");
    assert_eq!(std::fs::metadata(&path).expect("file").permissions().mode() & 0o777, 0o644);
    assert_eq!(std::fs::read(&older).expect("older kept"), b"an older build's shim");
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .expect("list")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["synaps-killguard-000000000000.so".to_string(), name], "no temp file left");
}

/// (g) The guard exists only for Linux with glibc: build.rs returns before
/// the compiler runs unless the target is linux + gnu, and every cfg in this
/// module names that pair, so a musl (or any other non-gnu) build compiles
/// to the no-op `apply_env` and leaves the env alone. Read from the sources:
/// no musl build runs here.
#[test]
fn g_the_guard_is_built_for_linux_gnu_only() {
    let build = include_str!("../../build.rs");
    let os = build.find(r#"target("CARGO_CFG_TARGET_OS") != "linux""#).expect("build.rs checks the target os");
    let gnu = build.find(r#"target("CARGO_CFG_TARGET_ENV") != "gnu""#).expect("build.rs checks the target env");
    let compile = build.find("try_get_compiler").expect("build.rs finds the compiler");
    assert!(os < compile && gnu < compile, "both checks come first");
    assert!(build[os.max(gnu)..compile].contains("return;"), "and return");
    assert!(!build.contains("-ldl"), "no libdl");

    let module = include_str!("../killguard.rs");
    let linux = module.matches(r#"target_os = "linux""#).count();
    let linux_gnu = module.matches(r#"target_os = "linux", target_env = "gnu""#).count();
    assert!(linux > 0 && linux == linux_gnu, "{linux} linux cfgs, {linux_gnu} of them gnu");
    assert!(
        module.contains("#[cfg(not(all(target_os = \"linux\", target_env = \"gnu\")))]\n    let _ = env;"),
        "apply_env is a no-op off linux-gnu"
    );
}

/// Every `GLIBC_2.…` version the bytes name, as numbers.
fn glibc_versions(bytes: &[u8]) -> Vec<Vec<u32>> {
    let tag = b"GLIBC_2.";
    let mut found = Vec::new();
    for (at, window) in bytes.windows(tag.len()).enumerate() {
        if window != tag {
            continue;
        }
        let rest = &bytes[at + "GLIBC_".len()..];
        let end = rest.iter().position(|b| !(b.is_ascii_digit() || *b == b'.')).unwrap_or(rest.len());
        let text = std::str::from_utf8(&rest[..end]).expect("ascii").trim_end_matches('.');
        found.push(text.split('.').map(|n| n.parse().expect("a number")).collect());
    }
    found
}

fn dotted(version: &[u32]) -> String {
    version.iter().map(u32::to_string).collect::<Vec<_>>().join(".")
}

/// The shim needs only libc.so.6 at its oldest symbol versions, so it loads
/// into any glibc process: no dlsym, no libdl, no `GLIBC_2.…` newer than the
/// arch's floor (syscall and __errno_location: 2.2.5 on x86_64, 2.17 on
/// aarch64). Read from the embedded bytes.
#[test]
fn the_shim_needs_only_the_oldest_glibc() {
    let contains = |needle: &[u8]| SHIM.windows(needle.len()).any(|w| w == needle);
    assert!(contains(b"libc.so.6"), "DT_NEEDED libc.so.6");
    for absent in [&b"dlsym"[..], b"libdl.so", b"GLIBC_PRIVATE"] {
        assert!(!contains(absent), "the shim names {}", String::from_utf8_lossy(absent));
    }
    let versions = glibc_versions(SHIM);
    let named: Vec<String> = versions.iter().map(|v| format!("GLIBC_{}", dotted(v))).collect();
    eprintln!("shim ({} bytes) names {named:?}", SHIM.len());
    assert!(!versions.is_empty(), "the shim names its glibc versions");
    let floor: &[u32] = if cfg!(target_arch = "x86_64") {
        &[2, 2, 5]
    } else if cfg!(target_arch = "aarch64") {
        &[2, 17]
    } else {
        eprintln!("floor check skipped: no floor recorded for this arch");
        return;
    };
    for v in &versions {
        assert!(v.as_slice() <= floor, "GLIBC_{} is newer than the floor GLIBC_{}", dotted(v), dotted(floor));
    }
}

/// `bytes` with every `from` replaced by `to` (the same length).
fn patched(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    assert_eq!(from.len(), to.len());
    let mut out = bytes.to_vec();
    let mut at = 0;
    let mut hits = 0;
    while let Some(i) = out[at..].windows(from.len()).position(|w| w == from) {
        out[at + i..at + i + from.len()].copy_from_slice(to);
        at += i + from.len();
        hits += 1;
    }
    assert!(hits > 0, "{} not found", String::from_utf8_lossy(from));
    out
}

/// (h) Self-check at install: `true` must run with the shim preloaded, exit
/// 0 and print nothing. A file that isn't ELF (ld.so: "cannot be preloaded
/// … ignored", exit 0) and a shim whose DT_NEEDED names a missing library
/// (ld.so fails every exec, exit 127) fail it: the env stays as it was, one
/// WARN with reason=self_check… and the exit code, never the whole stderr.
/// The real shim passes without a WARN.
#[test]
fn h_a_shim_the_loader_rejects_fails_the_self_check() {
    let missing_needed = patched(SHIM, b"libc.so.6", b"libnone.x");
    let cases: [(&[u8], &str, &str); 2] = [
        (b"not an ELF object", "not ELF", "exit code 0,"),
        (&missing_needed, "missing DT_NEEDED", "exit code 127,"),
    ];
    for (bytes, why, code) in cases {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (log, _capture) = crate::tools::test_helpers::DaemonLog::capture();
        let guard = KillGuard::install_shim_in(&tmp.path().join("lib"), bytes);
        assert!(guard.shim_path().is_none(), "{why}: the shim is not used");
        let before = pairs(&[("PATH", "/usr/bin:/bin"), ("LD_PRELOAD", "/x/libmine.so"), ("ASAN_OPTIONS", "a=1")]);
        for _ in 0..3 {
            let mut env = before.clone();
            guard.apply_env(&mut env);
            assert_eq!(env, before, "{why}");
        }
        let warns = log.find("agent_engine::killguard", tracing::Level::WARN, UNAVAILABLE);
        assert_eq!(warns.len(), 1, "{why}: {:?}", log.lines());
        let reason = warns[0].fields.get("reason").cloned().unwrap_or_default();
        eprintln!("{why}: reason={reason}");
        assert!(reason.starts_with("self_check"), "{why}: {reason}");
        assert!(reason.contains(code), "{why}: {reason}");
        assert!(reason.chars().count() <= 220, "{why}: stderr cut to ~120 chars: {reason}");
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let (log, _capture) = crate::tools::test_helpers::DaemonLog::capture();
    let guard = KillGuard::install_in(&tmp.path().join("lib"));
    assert!(guard.shim_path().is_some(), "the real shim passes");
    let mut env = Env::new();
    guard.apply_env(&mut env);
    assert!(value(&env, "LD_PRELOAD").is_some_and(|p| p.ends_with(&shim_file_name())), "{env:?}");
    assert!(log.find("agent_engine::killguard", tracing::Level::WARN, UNAVAILABLE).is_empty(), "{:?}", log.lines());
}
