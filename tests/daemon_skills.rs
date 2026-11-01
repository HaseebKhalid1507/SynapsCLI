//! Skills v1 end to end against a REAL in-process daemon and the `synaps`
//! binary: `skills_status` reports the boot pass, `synaps config set
//! --if-unset` writes a spaced `skills.extra_root` after boot, `synaps
//! skills reload --json` walks it (the host's boot config snapshot is kept)
//! and `synaps skills status --json` reports it whole; `skills_reload` is
//! refused on an attached connection, `skills_status` is answered there.
//!
//! Starts a daemon: run it on the remote test host only.

#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::{self, Daemon, DaemonOpts};
use agent_engine::session::wire::{encode_line, Attach, ClientFrame, Hello};
use agent_engine::session::{AttachMode, ClientKind, SessionConfig};
use agent_engine::{EngineHost, HostOpts};
use phase2::HomeGuard;
use serde_json::{json, Value};
use serial_test::serial;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

struct Raw {
    lines: tokio::io::Lines<BufReader<OwnedReadHalf>>,
    w: OwnedWriteHalf,
}

impl Raw {
    /// Connect and say hello; returns the connection and the `Welcome`.
    async fn connect(sock: &Path) -> (Self, Value) {
        let stream = UnixStream::connect(sock).await.expect("connect");
        let (r, w) = stream.into_split();
        let mut raw = Raw {
            lines: BufReader::new(r).lines(),
            w,
        };
        let hello = encode_line(&ClientFrame::Hello(Hello::new(ClientKind::Attach))).unwrap();
        raw.send_line(hello.trim_end()).await;
        let welcome = raw.next_reply().await;
        assert_eq!(welcome["type"], "welcome", "{welcome}");
        (raw, welcome)
    }

    async fn send_line(&mut self, line: &str) {
        self.w.write_all(line.as_bytes()).await.unwrap();
        self.w.write_all(b"\n").await.unwrap();
    }

    /// The next frame whose type is not `event` (an attached connection
    /// also carries the session's events).
    async fn next_reply(&mut self) -> Value {
        loop {
            let line = tokio::time::timeout(Duration::from_secs(15), self.lines.next_line())
                .await
                .expect("a frame in time")
                .expect("read")
                .expect("connection open");
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame["type"] != "event" {
                return frame;
            }
        }
    }
}

/// Runs the `synaps` binary against `base` (config) and `run` (daemon
/// socket); returns (exit code, stdout, stderr).
async fn synaps(
    home: &Path,
    base: &Path,
    run: &Path,
    args: &[&str],
) -> (Option<i32>, String, String) {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_synaps"));
    cmd.args(args)
        .env("HOME", home)
        .env("SYNAPS_BASE_DIR", base)
        .env("SYNAPS_RUNTIME_DIR", run)
        .env("SYNAPS_DAEMON_AUTOSPAWN", "0")
        .env("TERM", "dumb")
        .env_remove("SYNAPS_PROFILE")
        .stdin(std::process::Stdio::null());
    let out = tokio::task::spawn_blocking(move || cmd.output())
        .await
        .unwrap()
        .expect("run synaps");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn json_of(out: &str) -> Value {
    serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("not JSON ({e}): {out}"))
}

/// A store laid out like P's: `versions/<v>/<plugin>/...` and a `current`
/// symlink to the version. The plugin also carries a command, an extension
/// with a hook and an MCP server, each of which would touch `<home>/ran`;
/// the extra root takes its skills only. Returns (`current`, the version
/// dir).
fn store(home: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let skills = home.join("Library/Application Support/pria.desktop.dev/skills");
    let version = skills.join("versions/1.0.0-abcd1234");
    let plugin = version.join("pria-skills");
    std::fs::create_dir_all(plugin.join(".synaps-plugin")).unwrap();
    let touch = json!({"command": "touch", "args": [home.join("ran")]});
    let manifest = json!({
        "name": "pria-skills",
        "commands": [{"name": "hi", "command": "touch", "args": [home.join("ran")]}],
        "extension": {
            "protocol_version": 1,
            "runtime": "process",
            "command": "touch",
            "args": [home.join("ran")],
            "permissions": ["tools.intercept"],
            "hooks": [{"hook": "before_tool_call"}]
        },
        "mcpServers": {"m": touch},
    });
    std::fs::write(
        plugin.join(".synaps-plugin/plugin.json"),
        manifest.to_string(),
    )
    .unwrap();
    let skill = plugin.join("skills/doctor-check");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: doctor-check\ndescription: d\n---\nBody",
    )
    .unwrap();
    let current = skills.join("current");
    std::os::unix::fs::symlink(&version, &current).unwrap();
    (current, version)
}

/// Pins 4, 6 and 12 (skills v1), end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn skills_reload_walks_a_root_set_after_boot_and_is_refused_when_attached() {
    let guard = HomeGuard::new();
    let base = guard.base_dir();
    let run = base.join("run");
    let host: Arc<EngineHost> = EngineHost::boot_and_install(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .expect("host boot");
    let d = Daemon::start(
        host.clone(),
        DaemonOpts {
            runtime_dir: Some(run.clone()),
            factory: Some(daemon::echo_factory()),
            ..Default::default()
        },
    )
    .await
    .expect("daemon start");
    let sock = d.paths.sock.clone();

    // The boot pass: generation 1, five roots, no extra root.
    let (mut c, welcome) = Raw::connect(&sock).await;
    let features = welcome["features"].as_array().cloned().unwrap_or_default();
    assert!(features.contains(&json!("skills_v1")), "{welcome}");
    c.send_line(&json!({"type": "skills_status", "id": 5}).to_string())
        .await;
    let boot = c.next_reply().await;
    assert_eq!(
        (&boot["type"], &boot["id"]),
        (&json!("skills_result"), &json!(5)),
        "{boot}"
    );
    let report = &boot["report"];
    assert_eq!(report["generation"], 1, "{boot}");
    let kinds: Vec<&str> = report["roots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "project_plugins",
            "project_skills",
            "extra",
            "home_plugins",
            "home_skills"
        ]
    );
    assert!(report["roots"][2]["configured"].is_null(), "{boot}");
    assert_eq!(
        (
            report["skills_cap"].as_u64(),
            report["metadata_cap"].as_u64()
        ),
        (Some(2048), Some(4194304))
    );

    // The doctor writes the root after boot; the CLI reloads.
    let (current, version) = store(guard.home.path());
    let current_text = current.to_str().unwrap();
    assert!(current_text.contains("/Application Support/"));
    let pair = format!("skills.extra_root={current_text}");
    let (code, out, err) = synaps(
        guard.home.path(),
        &base,
        &run,
        &["config", "set", "--json", "--if-unset", &pair],
    )
    .await;
    assert_eq!(code, Some(0), "{out}{err}");
    assert_eq!(json_of(&out)["result"], "written", "{out}");

    let (code, out, err) = synaps(
        guard.home.path(),
        &base,
        &run,
        &["skills", "reload", "--json"],
    )
    .await;
    assert_eq!(code, Some(0), "{out}{err}");
    let reloaded = json_of(&out);
    assert_eq!(reloaded["generation"], 2, "{reloaded}");
    let extra = &reloaded["roots"][2];
    assert_eq!(extra["kind"], "extra", "{reloaded}");
    assert_eq!(
        extra["configured"], current_text,
        "the spaced path, whole: {reloaded}"
    );
    assert_eq!(
        extra["resolved"],
        version.canonicalize().unwrap().to_str().unwrap(),
        "the version is the resolved path: {reloaded}"
    );
    assert_eq!(
        (extra["plugins"].as_u64(), extra["skills"].as_u64()),
        (Some(1), Some(1)),
        "{reloaded}"
    );
    let loaded: Vec<(Option<String>, String)> = host
        .command_registry()
        .all_skills()
        .iter()
        .map(|s| (s.plugin.clone(), s.name.clone()))
        .collect();
    assert!(
        loaded.contains(&(Some("pria-skills".into()), "doctor-check".into())),
        "{loaded:?}"
    );
    // The extra root gives skills only (790d flag 1).
    assert_eq!(
        (
            &reloaded["ignored_non_skill"],
            &reloaded["ignored_non_skill_names"]
        ),
        (
            &json!(4),
            &json!([
                "pria-skills:commands",
                "pria-skills:extension",
                "pria-skills:hooks",
                "pria-skills:mcp"
            ])
        ),
        "{reloaded}"
    );
    assert!(matches!(
        host.command_registry().resolve("pria-skills:hi"),
        agent_engine::skills::registry::Resolution::Unknown
    ));
    assert!(host.command_registry().sidecars().is_empty());
    assert!(!host.command_registry().sidecar_has_extension("pria-skills"));
    assert!(!guard.home.path().join("ran").exists(), "nothing ran");
    assert_eq!(
        host.config().skills_extra_root,
        None,
        "the boot config snapshot is kept"
    );

    let (code, out, err) = synaps(
        guard.home.path(),
        &base,
        &run,
        &["skills", "status", "--json"],
    )
    .await;
    assert_eq!(code, Some(0), "{out}{err}");
    assert_eq!(json_of(&out), reloaded, "status reports the last pass");

    // Attached: reload is refused, status is answered, nothing changes.
    let (mut a, _) = Raw::connect(&sock).await;
    let create = encode_line(&ClientFrame::Attach(Attach::Create {
        config: SessionConfig {
            cwd: Some(guard.home.path().to_path_buf()),
            persist: false,
            ..Default::default()
        },
        mode: AttachMode::Mirror,
    }))
    .unwrap();
    a.send_line(create.trim_end()).await;
    let attached = a.next_reply().await;
    assert_eq!(attached["type"], "attached", "{attached}");
    a.send_line(&json!({"type": "skills_reload", "id": 6}).to_string())
        .await;
    let refused = a.next_reply().await;
    assert_eq!(refused["type"], "error", "{refused}");
    assert_eq!(
        refused["message"], "control frames are not accepted on an attached connection",
        "{refused}"
    );
    a.send_line(&json!({"type": "skills_status", "id": 7}).to_string())
        .await;
    let status = a.next_reply().await;
    assert_eq!(
        (&status["type"], &status["id"]),
        (&json!("skills_result"), &json!(7)),
        "{status}"
    );
    assert_eq!(
        status["report"], reloaded,
        "the refused reload changed nothing"
    );

    drop(a);
    drop(c);
    d.shutdown_token().cancel();
    d.wait().await;
}

/// `synaps skills status|reload` without a daemon: exit 1, an error object
/// with `--json`, and no daemon started.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn skills_cli_without_a_daemon_exits_1() {
    let home = tempfile::tempdir().unwrap();
    let (base, run) = (home.path().join("base"), home.path().join("run"));
    std::fs::create_dir_all(&base).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(base.join("config"), "").unwrap();
    for verb in ["status", "reload"] {
        let (code, out, err) = synaps(home.path(), &base, &run, &["skills", verb, "--json"]).await;
        assert_eq!(code, Some(1), "{out}{err}");
        assert_eq!(json_of(&out)["error"], "no_daemon", "{out}");
        assert!(err.contains("no daemon is running"), "{err}");
    }
    assert!(!run.join("daemon.sock").exists(), "no daemon was started");
}
