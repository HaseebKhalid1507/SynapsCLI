//! A headless `synaps agent` leaves shared-inbox files alone. Nobody is at a
//! headless agent, and each inbox file goes to exactly one taker: an agent
//! that took them could win an alert meant for the session someone is at.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Kill(Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for(what: &str, timeout: Duration, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn has_card(run: &Path) -> bool {
    std::fs::read_dir(run).is_ok_and(|rd| {
        rd.flatten().any(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("inbox-probe-") && name.ends_with(".json")
        })
    })
}

#[test]
fn a_headless_agent_never_takes_shared_inbox_files() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let base = home.join(".synaps-cli");
    let run = home.join("run");
    let inbox = base.join("inbox");
    let agent_dir = home.join("agent");
    for dir in [&run, &inbox, &agent_dir] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(
        agent_dir.join("config.toml"),
        "[agent]\nname = \"inbox-probe\"\n",
    )
    .unwrap();
    std::fs::write(agent_dir.join("soul.md"), "You are a test agent.\n").unwrap();

    // A model endpoint that accepts and never answers keeps the agent in its
    // first turn, so its watchers run for as long as the test needs.
    let model = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", model.local_addr().unwrap());

    let event = synaps_cli::events::types::Event::simple(
        "axon",
        "for the person at the keyboard",
        Some(synaps_cli::events::types::Severity::High),
    );
    let file = inbox.join("1-alert.json");
    std::fs::write(&file, serde_json::to_string(&event).unwrap()).unwrap();

    let _agent = Kill(
        Command::new(env!("CARGO_BIN_EXE_synaps"))
            .arg("agent")
            .arg("--config")
            .arg(agent_dir.join("config.toml"))
            .env("HOME", home)
            .env("SYNAPS_HOME", home)
            .env("SYNAPS_BASE_DIR", &base)
            .env("SYNAPS_RUNTIME_DIR", &run)
            .env("SYNAPS_DAEMON", "0")
            .env("SYNAPS_ANTHROPIC_BASE_URL", &url)
            .env("ANTHROPIC_API_KEY", "test-key")
            .env_remove("SYNAPS_PROFILE")
            .env_remove("SYNAPS_CLIENT_REEXECED")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );

    // The registry card is written just before the inbox watcher would start.
    wait_for("the agent's registry card", Duration::from_secs(30), || {
        has_card(&run)
    });
    // Past the watcher's start-up scan and one 2 s rescan.
    std::thread::sleep(Duration::from_millis(3000));
    let left: Vec<_> = std::fs::read_dir(&inbox)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(
        file.exists(),
        "the agent took the inbox file; inbox now holds {left:?}"
    );
}
