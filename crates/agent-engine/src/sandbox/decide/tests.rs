//! The decision on an OS without Landlock (`Unavailable::UnsupportedOs`,
//! what `abi_version` answers in every non-Linux build): an enforcing
//! policy is refused whatever `required` says, through the Linux
//! "unavailable" path (its text and its `sandbox.*` lines, once per state).
//! Without an enforcing policy everything decides as it does on Linux. The
//! ABI is the binding's test seam, so this runs on Linux too; no kernel
//! support is needed and no domain is built.

use super::*;
use crate::sandbox::policy::{check_report, test_scratch_parent, KillSwitch, PolicyEnv};
use crate::sandbox::{status_with, SandboxBinding};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

const NO_OS: Unavailable = Unavailable::UnsupportedOs;
const CONTACT: &str = "Ask your foreman.";

struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    env: PolicyEnv,
}

/// A fake `$HOME` with the base dir one level down and a work dir `lane`.
fn fx() -> Fx {
    let tmp = tempfile::Builder::new().prefix("sbx-decide-").tempdir_in(test_scratch_parent()).expect("tempdir");
    let root = fs::canonicalize(tmp.path()).expect("canonical tempdir");
    for d in ["home/x/base/sandbox/sessions", "lane"] {
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
    fn enforcing(&self, extra: &str) -> String {
        format!("mode = enforce\n{extra}write = {}\ncontact = {CONTACT}\n", self.r("lane"))
    }
    /// A binding with its own kill-switch state and `abi` as the answer.
    fn binding(&self, id: &str, abi: Result<u32, Unavailable>) -> Arc<SandboxBinding> {
        SandboxBinding::with_env(id, self.env.clone(), Box::leak(Box::new(KillSwitch::new())), Some(abi))
    }
}

fn decide(b: &SandboxBinding) -> (String, Vec<String>) {
    let mut log = Vec::new();
    let kind = match prepare(b, "bash", &mut |line| log.push(line)) {
        SpawnSandbox::None => "none".to_string(),
        SpawnSandbox::Refuse(msg) => format!("refuse: {msg}"),
        #[cfg(target_os = "linux")]
        SpawnSandbox::Apply(_) => "apply".to_string(),
    };
    (kind, log)
}

#[test]
fn an_enforcing_policy_is_refused_without_os_support_whatever_required_says() {
    for extra in ["", "required = false\n", "required = true\n"] {
        let fx = fx();
        fx.write_policy("s1", &fx.enforcing(extra));
        let b = fx.binding("s1", Err(NO_OS));
        let (kind, log) = decide(&b);
        assert_eq!(
            kind,
            format!(
                "refuse: sandbox required for this session but unavailable on this host (unsupported-os (no Landlock on {})); commands can't run. {CONTACT}",
                std::env::consts::OS
            ),
            "{extra:?}"
        );
        assert_eq!(log.len(), 2, "{extra:?}: {log:?}");
        assert!(log[0].starts_with("sandbox.policy session=s1 mode=enforce "), "{}", log[0]);
        assert!(log[0].contains(" abi=unavailable:unsupported-os "), "{}", log[0]);
        assert_eq!(log[1], "sandbox.refused session=s1 reason=unavailable:unsupported-os");
        assert_eq!(decide(&b), (kind, vec![]), "{extra:?}: still refused, logged once");
    }
}

#[test]
fn a_linux_host_without_landlock_still_decides_as_before() {
    // The same files on a Linux kernel without Landlock: `required` decides.
    let fx = fx();
    fx.write_policy("s1", &fx.enforcing(""));
    let (kind, log) = decide(&fx.binding("s1", Err(Unavailable::NoLandlock)));
    assert_eq!(kind, "none");
    assert_eq!(log.get(1).map(String::as_str), Some("sandbox.unavailable session=s1 reason=no-landlock"));
    fx.write_policy("s2", &fx.enforcing("required = true\n"));
    let (kind, log) = decide(&fx.binding("s2", Err(Unavailable::NoLandlock)));
    assert!(kind.starts_with("refuse: sandbox required for this session but unavailable on this host (no-landlock"), "{kind}");
    assert_eq!(log.get(1).map(String::as_str), Some("sandbox.refused session=s2 reason=unavailable:no-landlock"));
}

#[test]
fn without_an_enforcing_policy_nothing_changes() {
    let fx = fx();
    // No policy file: nothing refused, nothing logged.
    assert_eq!(decide(&fx.binding("none1", Err(NO_OS))), ("none".to_string(), vec![]));
    // An invalid file: refused exactly as on Linux (the ABI plays no part).
    fx.write_policy("bad", "mode = enforce\nwrite = relative/path\n");
    let (kind, log) = decide(&fx.binding("bad", Err(NO_OS)));
    assert!(kind.starts_with("refuse: "), "{kind}");
    assert_eq!((kind, log.clone()), decide(&fx.binding("bad", Err(Unavailable::NoLandlock))));
    assert_eq!(log.len(), 1, "{log:?}");
    // `mode = off`: runs, with the policy line and the explicit-off line.
    fx.write_policy("off1", &format!("mode = off\nwrite = {}\n", fx.r("lane")));
    let (kind, log) = decide(&fx.binding("off1", Err(NO_OS)));
    assert_eq!(kind, "none");
    assert_eq!(log.len(), 2, "{log:?}");
    assert!(log[0].starts_with("sandbox.policy session=off1 mode=off "), "{}", log[0]);
}

#[test]
fn the_kill_switch_works_as_on_linux() {
    let fx = fx();
    fx.write_policy("s1", &fx.enforcing("required = true\n"));
    fs::write(fx.env.kill_switch(), "").expect("create OFF");
    let mac = fx.binding("s1", Err(NO_OS));
    let linux = fx.binding("s1", Err(Unavailable::NoLandlock));
    let (kind, log) = decide(&mac);
    assert_eq!(kind, "none", "the switch turns every sandbox off");
    assert_eq!((kind, log), decide(&linux), "the same decision and lines");
    // Removed: the policy is read again and refused.
    fs::remove_file(fx.env.kill_switch()).expect("remove OFF");
    assert!(decide(&mac).0.starts_with("refuse: "));
}

#[test]
fn status_says_unavailable_without_os_support() {
    let fx = fx();
    let none = status_with(&fx.env, "s1", Err(NO_OS));
    assert_eq!((none.state, none.tools, none.abi), ("none", "open", None));
    fx.write_policy("s1", &fx.enforcing("tools = confined\n"));
    let s = status_with(&fx.env, "s1", Err(NO_OS));
    assert_eq!((s.state, s.tools, s.abi, s.rules > 0), ("unavailable", "confined", None, true));
    assert!(s.policy_hash.is_some());
    // A Linux host without Landlock keeps reporting the file's mode.
    assert_eq!(status_with(&fx.env, "s1", Err(Unavailable::NoLandlock)).state, "enforce");
    fx.write_policy("off1", &format!("mode = off\nwrite = {}\n", fx.r("lane")));
    assert_eq!(status_with(&fx.env, "off1", Err(NO_OS)).state, "off");
    fx.write_policy("bad", "mode = enforce\ntools = confined\nwrite = relative/path\n");
    let bad = status_with(&fx.env, "bad", Err(NO_OS));
    assert_eq!((bad.state, bad.tools), ("invalid", "confined"));
}

#[test]
fn check_report_says_every_spawn_is_refused_without_os_support() {
    let fx = fx();
    let file = fx.write_policy("s1", &fx.enforcing(""));
    let (ok, text) = check_report(&file, &fx.env, Err(NO_OS));
    assert!(ok, "{text}");
    assert!(text.contains("on this OS every spawn of this session is REFUSED"), "{text}");
    let (_, linux) = check_report(&file, &fx.env, Err(Unavailable::NoLandlock));
    assert!(!linux.contains("REFUSED"), "{linux}");
}
