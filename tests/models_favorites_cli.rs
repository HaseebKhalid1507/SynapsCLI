//! W24 — `synaps models --favorites --json` with no daemon running: it
//! answers from this process (no extensions, no network) with
//! `source: "local"`, and it never starts a daemon.
//!
//! Run it on the remote test host only: if it did autospawn, the daemon
//! would start wherever the test runs.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

/// Under cargo's per-target scratch dir (never /tmp). `run` is a short
/// runtime dir, so a socket path would fit.
fn fx() -> Fx {
    let tmp = tempfile::Builder::new().prefix("mf-cli-").tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("tempdir");
    let root = std::fs::canonicalize(tmp.path()).expect("canonical");
    for d in ["home/base", "run"] {
        std::fs::create_dir_all(root.join(d)).expect("mkdir");
    }
    std::fs::write(
        root.join("home/base/config"),
        "favorite_models = groq/llama-3.3-70b-versatile, local/qwen3:8b, plug:prov:m\nprovider.local.models = llama3:8b, qwen3:8b\n",
    )
    .expect("config");
    Fx { _tmp: tmp, root }
}

/// The binary with a cleared environment: no provider keys, no profile.
fn synaps(root: &Path, args: &[&str]) -> Output {
    let child = Command::new(env!("CARGO_BIN_EXE_synaps"))
        .args(args)
        .env_clear()
        .env("HOME", root.join("home"))
        .env("SYNAPS_BASE_DIR", root.join("home/base"))
        .env("SYNAPS_RUNTIME_DIR", root.join("run"))
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn synaps");
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(out) = rx.try_recv() {
            return out.expect("wait synaps");
        }
        if Instant::now() > deadline {
            let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
            panic!("synaps {args:?} did not exit in 30 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The `daemon*` files in `dir` (a daemon's lock, socket, json, pid).
fn daemon_files(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("daemon"))
        .collect();
    names.sort();
    names
}

#[test]
fn favorites_without_a_daemon_answer_locally_and_start_none() {
    let fx = fx();
    let out = synaps(&fx.root, &["models", "--favorites", "--json"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "exit {:?}\nstdout: {stdout}\nstderr: {stderr}", out.status);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("JSON ({e}): {stdout}"));
    assert_eq!(v["source"], "local", "{v}");
    assert_eq!(v["reason"], "no_daemon", "{v}");
    assert_eq!(v["plugins_loaded"], false, "{v}");
    assert_eq!(v["live_providers"], serde_json::json!([]), "{v}");
    assert_eq!(
        v["models"],
        serde_json::json!([{"id": "local/qwen3:8b", "label": "Qwen3", "provider_key": "local", "provider_name": "Local"}]),
        "groq has no key and the plugin is not loaded locally: {v}"
    );

    // Nothing was started: no daemon files anywhere a daemon would put
    // them, and a second run still finds no daemon.
    assert_eq!(daemon_files(&fx.root.join("run")), Vec::<String>::new());
    assert_eq!(daemon_files(&fx.root.join("home/base/run")), Vec::<String>::new());
    std::thread::sleep(Duration::from_millis(300));
    let again = synaps(&fx.root, &["models", "--favorites", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&again.stdout).expect("JSON");
    assert_eq!((v["source"].as_str(), v["reason"].as_str()), (Some("local"), Some("no_daemon")), "{v}");
    assert_eq!(daemon_files(&fx.root.join("run")), Vec::<String>::new());
}
