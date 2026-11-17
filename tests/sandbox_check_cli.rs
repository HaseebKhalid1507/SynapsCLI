//! `synaps sandbox check` as the health check and the extension see it: the
//! real binary, a real provider plugin (a process answering `hook.handle`),
//! a private base dir. Linux only (the seam is).
//!
//! What this pins (each case is its own undo of the others):
//! - the provider is called through the hook envelope with `kind =
//!   "sandbox_policy"`, `session_id = "check"`, the workspace and
//!   `parent_session_id = null`;
//! - only a plugin that declares the hook AND holds `sandbox.policy` is a
//!   provider (without the permission it counts as not loaded: refusal);
//! - `--json` is exactly one JSON object with exactly the frozen keys, on a
//!   usable session and on every refusal, and the exit code is 0 / 1;
//! - protected entries, and entries above one, are dropped and reported;
//!   `.git/hooks`, `.git/config` and the plugin folder are listed as left
//!   out of the workspace grant;
//! - the check writes nothing.
#![cfg(target_os = "linux")]

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const FROZEN_KEYS: [&str; 8] = ["abi", "confine", "dropped", "provider", "provider_state", "read", "refusal", "write"];

/// Whether this kernel can confine a child at all. Without it every
/// enforcing check refuses with `abi-too-old` (also a pinned answer).
fn kernel_usable() -> bool {
    agent_engine::sandbox::landlock::usable_abi().is_ok()
}

struct Rig {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    base: PathBuf,
    ws: PathBuf,
    data: PathBuf,
    seen: PathBuf,
}

impl Rig {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let home = root.join("home");
        let base = home.join(".synaps-cli");
        let ws = root.join("ws");
        let data = root.join("data");
        for dir in [&base, &ws, &data] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let seen = root.join("seen.json");
        Rig { _tmp: tmp, root, home, base, ws, data, seen }
    }

    fn config(&self, text: &str) {
        std::fs::write(self.base.join("config"), text).unwrap();
    }

    /// Install a provider plugin `name` that holds `permissions` and answers
    /// as `mode` says.
    fn provider(&self, name: &str, permissions: &[&str], mode: Value) {
        let dir = self.base.join("plugins").join(name);
        std::fs::create_dir_all(dir.join(".synaps-plugin")).unwrap();
        std::fs::create_dir_all(dir.join("extensions")).unwrap();
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sandbox_policy_extension.py"),
            dir.join("extensions/sandbox_policy_extension.py"),
        )
        .unwrap();
        let mut mode = mode;
        mode["seen"] = json!(self.seen);
        std::fs::write(dir.join("mode.json"), mode.to_string()).unwrap();
        let manifest = json!({
            "name": name,
            "version": "0.1.0",
            "extension": {
                "protocol_version": 1,
                "runtime": "process",
                "command": "python3",
                "args": ["extensions/sandbox_policy_extension.py"],
                "permissions": permissions,
                "hooks": [{"hook": "sandbox_policy"}]
            }
        });
        std::fs::write(dir.join(".synaps-plugin/plugin.json"), manifest.to_string()).unwrap();
    }

    fn check(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_synaps"))
            .env_clear()
            .env("HOME", &self.home)
            .env("SYNAPS_BASE_DIR", &self.base)
            .env("PATH", std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()))
            .env("TERM", "dumb")
            .current_dir(&self.ws)
            .stdin(Stdio::null())
            .args(["sandbox", "check"])
            .args(args)
            .output()
            .unwrap()
    }

    /// Everything under the root but the provider's own `seen` file: name,
    /// kind, length, mtime.
    fn snapshot(&self) -> BTreeMap<PathBuf, (bool, u64, std::time::SystemTime)> {
        fn walk(dir: &Path, skip: &Path, out: &mut BTreeMap<PathBuf, (bool, u64, std::time::SystemTime)>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path == skip {
                    continue;
                }
                let meta = std::fs::symlink_metadata(&path).unwrap();
                out.insert(path.clone(), (meta.is_dir(), meta.len(), meta.modified().unwrap()));
                if meta.is_dir() {
                    walk(&path, skip, out);
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(&self.root, &self.seen, &mut out);
        out
    }

    fn make_main_checkout(&self) {
        for d in ["objects/ab", "refs/heads", "logs", "hooks", "info", "worktrees"] {
            std::fs::create_dir_all(self.ws.join(".git").join(d)).unwrap();
        }
        std::fs::write(self.ws.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(self.ws.join(".git/config"), "[core]\n").unwrap();
    }
}

/// The one JSON object a `--json` check printed: standard output is exactly
/// that object and a newline, with the frozen keys and no others.
fn one_object(out: &Output) -> Value {
    let text = String::from_utf8(out.stdout.clone()).unwrap();
    assert!(text.ends_with('\n') && text.trim_end().lines().count() == 1, "not exactly one line: {text:?}");
    let value: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("not one JSON value ({e}): {text:?}"));
    let mut keys: Vec<&str> = value.as_object().expect("an object").keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, FROZEN_KEYS, "{text}");
    value
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
}

fn dropped_paths(value: &Value) -> Vec<(String, String)> {
    value["dropped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            let mut keys: Vec<&str> = d.as_object().unwrap().keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(keys, ["path", "reason"]);
            (d["path"].as_str().unwrap().to_string(), d["reason"].as_str().unwrap().to_string())
        })
        .collect()
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("exited by a signal")
}

#[test]
fn off_is_exit_zero_with_the_frozen_keys_and_writes_nothing() {
    let rig = Rig::new();
    let before = rig.snapshot();
    let out = rig.check(&["--json"]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let v = one_object(&out);
    assert_eq!(v["confine"], "off");
    assert_eq!(v["provider_state"], "off");
    assert!(v["refusal"].is_null() && v["provider"].is_null());
    assert_eq!(v["write"], json!([]));
    assert_eq!(v["read"], json!([]));
    assert_eq!(v["dropped"], json!([]));
    assert_eq!(rig.snapshot(), before, "the check wrote something");
}

#[test]
fn enforce_with_no_provider_refuses_with_exit_one() {
    let rig = Rig::new();
    rig.config("sandbox.confine = enforce\n");
    let out = rig.check(&["--json"]);
    assert_eq!(code(&out), 1);
    let v = one_object(&out);
    assert_eq!(v["confine"], "enforce");
    assert_eq!(v["refusal"], if kernel_usable() { "no-policy" } else { "abi-too-old" });
    assert_eq!(v["write"], json!([]), "a refused session resolves nothing");
    // The plain report is `key: value` lines with the read line spelled out.
    let plain = rig.check(&[]);
    assert_eq!(code(&plain), 1);
    let text = String::from_utf8(plain.stdout).unwrap();
    assert!(text.lines().all(|l| l.contains(": ")), "{text}");
    assert!(text.contains("read: not enforced (v1)\n"), "{text}");
}

#[test]
fn a_provider_that_is_not_installed_or_lacks_the_permission_is_not_loaded() {
    let rig = Rig::new();
    rig.config("sandbox.confine = enforce\nsandbox.policy_provider = ghost\n");
    let out = rig.check(&["--json"]);
    assert_eq!(code(&out), 1);
    let v = one_object(&out);
    assert_eq!(v["provider"], "ghost");
    assert_eq!(v["provider_state"], "missing");
    assert_eq!(v["refusal"], if kernel_usable() { "provider-missing" } else { "abi-too-old" });

    // Installed and declaring the hook, but without `sandbox.policy`: still
    // not a provider, and it is never asked.
    rig.provider("ghost", &[], json!({"mode": "ok", "write": []}));
    let out = rig.check(&["--json"]);
    assert_eq!(code(&out), 1);
    let v = one_object(&out);
    assert_eq!(v["provider_state"], "missing");
    assert!(!rig.seen.exists(), "a plugin without the permission was asked");

    // Undo: the same plugin with the permission is asked and answers.
    rig.provider("ghost", &["sandbox.policy"], json!({"mode": "ok", "write": []}));
    let out = rig.check(&["--json"]);
    let v = one_object(&out);
    if kernel_usable() {
        assert_eq!(code(&out), 0, "{v}");
        assert_eq!(v["provider_state"], "loaded");
        assert!(rig.seen.exists(), "the provider with the permission was not asked");
    }
}

#[test]
fn a_usable_answer_is_screened_and_the_inner_paths_are_listed() {
    if !kernel_usable() {
        eprintln!("SKIPPED a_usable_answer_is_screened_and_the_inner_paths_are_listed: no usable Landlock");
        return;
    }
    let rig = Rig::new();
    rig.make_main_checkout();
    rig.config("sandbox.confine = enforce\nsandbox.policy_provider = guard\n");
    let under_base = rig.base.join("sandbox/extra");
    rig.provider(
        "guard",
        &["sandbox.policy"],
        json!({"mode": "ok",
               "write": [rig.data, under_base, rig.home, rig.base],
               "read": ["/etc"]}),
    );
    let before = rig.snapshot();
    let ws = rig.ws.to_str().unwrap();
    let out = rig.check(&["--json", "--workspace", ws]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let v = one_object(&out);
    assert_eq!(v["confine"], "enforce");
    assert_eq!(v["provider"], "guard");
    assert_eq!(v["provider_state"], "loaded");
    assert!(v["refusal"].is_null());
    assert!(v["abi"].as_u64().unwrap() >= 3);
    assert_eq!(v["read"], json!(["/etc"]));

    let write = strings(&v["write"]);
    let has = |p: &Path| write.iter().any(|w| Path::new(w) == p);
    assert!(has(&rig.ws) && has(&rig.data), "{write:?}");
    for denied in [&under_base, &rig.home, &rig.base] {
        assert!(!has(denied), "{} must not be writable: {write:?}", denied.display());
    }

    let dropped = dropped_paths(&v);
    let reason_of = |p: &Path| dropped.iter().find(|(d, _)| Path::new(d) == p).map(|(_, r)| r.clone());
    for p in [&under_base, &rig.home, &rig.base] {
        assert!(reason_of(p).is_some_and(|r| !r.is_empty()), "{} is not reported: {dropped:?}", p.display());
    }
    // Each inner path a grant leaves out is listed, with a reason, and so is
    // the directory that can't take new entries.
    for inner in [rig.ws.join(".git/hooks"), rig.ws.join(".git/config"), rig.ws.join(".synaps/plugins")] {
        let why = reason_of(&inner).unwrap_or_else(|| panic!("{} is not listed: {dropped:?}", inner.display()));
        assert!(why.starts_with("left out of its write grant"), "{why}");
    }
    let why = reason_of(&rig.ws).expect("the split directory is listed");
    assert!(why.contains("new entries"), "{why}");
    assert!(
        !write.iter().any(|w| Path::new(w).ends_with(".git/hooks") || Path::new(w).ends_with(".git/config")),
        "{write:?}"
    );

    // The provider was asked through the hook envelope.
    let seen: Value = serde_json::from_str(&std::fs::read_to_string(&rig.seen).unwrap()).unwrap();
    assert_eq!(seen["kind"], "sandbox_policy");
    assert_eq!(seen["session_id"], "check");
    assert_eq!(Path::new(seen["workspace"].as_str().unwrap()), rig.ws);
    assert!(seen["parent_session_id"].is_null());

    // `--session` changes only the id it is asked about.
    let out = rig.check(&["--json", "--session", "s-42"]);
    assert_eq!(code(&out), 0);
    let seen: Value = serde_json::from_str(&std::fs::read_to_string(&rig.seen).unwrap()).unwrap();
    assert_eq!(seen["session_id"], "s-42");
    assert_eq!(Path::new(seen["workspace"].as_str().unwrap()), rig.ws, "the working directory is the default");

    // The plain report lists the same drops and the cost of the split.
    let plain = rig.check(&[]);
    let text = String::from_utf8(plain.stdout).unwrap();
    assert!(text.contains(&format!("dropped: {}", rig.ws.join(".git/hooks").display())), "{text}");
    assert!(text.contains("read: not enforced (v1)"), "{text}");
    assert!(text.contains("note: a directory on a split path"), "{text}");

    assert_eq!(rig.snapshot(), before, "the check wrote something");
}

#[test]
fn every_provider_failure_refuses_with_its_own_cause() {
    let rig = Rig::new();
    rig.config("sandbox.confine = enforce\nsandbox.policy_provider = guard\n");
    let usable = kernel_usable();
    let cases: [(Value, &str, &str); 3] = [
        (json!({"mode": "refuse", "reason": "no\u{202e}pe\u{7}"}), "refused", "provider-refused"),
        (json!({"mode": "garbage"}), "bad-answer", "provider-bad-answer"),
        (json!({"mode": "error"}), "error", "provider-error"),
    ];
    for (mode, state, cause) in cases {
        rig.provider("guard", &["sandbox.policy"], mode);
        let out = rig.check(&["--json"]);
        assert_eq!(code(&out), 1, "{cause}");
        let v = one_object(&out);
        if usable {
            assert_eq!(v["provider_state"], state);
            assert_eq!(v["refusal"], cause);
        } else {
            assert_eq!(v["refusal"], "abi-too-old");
        }
        assert_eq!(v["write"], json!([]));
    }
    if usable {
        // The refusal text a session reports has the control and bidi
        // characters stripped.
        rig.provider("guard", &["sandbox.policy"], json!({"mode": "refuse", "reason": "no\u{202e}pe\u{7}"}));
        let plain = rig.check(&[]);
        let text = String::from_utf8(plain.stdout).unwrap();
        assert!(text.contains("session_cause: refused: nope\n"), "{text}");
    }
}

#[test]
fn a_provider_that_never_answers_times_out() {
    if !kernel_usable() {
        eprintln!("SKIPPED a_provider_that_never_answers_times_out: no usable Landlock");
        return;
    }
    let rig = Rig::new();
    rig.config("sandbox.confine = enforce\nsandbox.policy_provider = guard\n");
    rig.provider("guard", &["sandbox.policy"], json!({"mode": "hang"}));
    let started = std::time::Instant::now();
    let out = rig.check(&["--json"]);
    assert!(started.elapsed() < std::time::Duration::from_secs(20), "the check did not give up");
    assert_eq!(code(&out), 1);
    let v = one_object(&out);
    assert_eq!(v["provider_state"], "timeout");
    assert_eq!(v["refusal"], "provider-timeout");
}
