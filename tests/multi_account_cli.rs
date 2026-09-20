//! Subprocess contract tests for multi-account OAuth management. Every process
//! gets an empty environment and a private synthetic credential directory. No
//! browser, identity provider, real refresh token or inference is involved.
use serde_json::{json, Value};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let credential = |access: &str, id: &str| {
            json!({"type":"oauth", "access":access, "refresh":format!("refresh-{id}"),
                "expires":4102444800000_u64, "accountId":id})
        };
        let data = json!({
            "openai-codex": credential("synthetic-access-default", "synthetic-default-id"),
            "openai-codex@second": credential("synthetic-access-second", "synthetic-second-id"),
            "kimi-code@work": credential("synthetic-access-kimi", "synthetic-kimi-id"),
            "openrouter": {"type":"api_key", "key":"synthetic-static-secret"},
            "unrelated": {"metadata":"must survive"}
        });
        std::fs::write(
            dir.path().join("auth.json"),
            serde_json::to_vec(&data).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.path().join("config"), "").unwrap();
        Self { dir }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_synaps"));
        cmd.env_clear()
            .env("HOME", self.dir.path())
            .env("SYNAPS_BASE_DIR", self.dir.path())
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "dumb")
            .current_dir(self.dir.path())
            .stdin(Stdio::null());
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn auth(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.dir.path().join("auth.json")).unwrap()).unwrap()
    }
}

fn assert_ok(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn account_listing_is_json_and_never_prints_credentials() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["auth", "list", "--json"]);
    assert_ok(&output);
    let text = String::from_utf8(output.stdout).unwrap();
    let _: Value = serde_json::from_str(&text).unwrap();
    assert!(text.contains("openai-codex") && text.contains("second"));
    for secret in [
        "synthetic-access",
        "refresh-synthetic",
        "synthetic-static-secret",
    ] {
        assert!(!text.contains(secret));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    }
}

#[test]
fn removing_one_slot_preserves_default_other_providers_and_metadata() {
    let sandbox = Sandbox::new();
    let before = sandbox.auth();
    let output = sandbox.run(&[
        "auth",
        "remove",
        "--provider",
        "openai-codex",
        "--account",
        "second",
        "--yes",
    ]);
    assert_ok(&output);
    let mut expected = before;
    expected
        .as_object_mut()
        .unwrap()
        .remove("openai-codex@second");
    assert_eq!(sandbox.auth(), expected);
}

#[test]
fn malformed_account_and_unknown_selection_do_not_mutate_storage() {
    let sandbox = Sandbox::new();
    let before = sandbox.auth();
    for label in ["../escape", "missing", ""] {
        let output = sandbox.run(&[
            "auth",
            "use",
            "--provider",
            "openai-codex",
            "--account",
            label,
        ]);
        assert!(!output.status.success(), "unexpected success for {label:?}");
        assert_eq!(sandbox.auth(), before);
    }
}

#[test]
fn default_removal_requires_confirmation_and_selection_does_not_copy_tokens() {
    let sandbox = Sandbox::new();
    let before = sandbox.auth();
    let refused = sandbox.run(&[
        "auth",
        "remove",
        "--provider",
        "openai-codex",
        "--account",
        "default",
    ]);
    assert!(!refused.status.success());
    assert_eq!(sandbox.auth(), before);
    let selected = sandbox.run(&[
        "auth",
        "use",
        "--provider",
        "openai-codex",
        "--account",
        "second",
    ]);
    assert_ok(&selected);
    assert_eq!(sandbox.auth(), before);
    let config = std::fs::read_to_string(sandbox.dir.path().join("config")).unwrap();
    assert!(config.contains("auth.account.openai-codex") && config.contains("second"));
    assert!(!config.contains("synthetic-access") && !config.contains("refresh-synthetic"));
}

struct BrokerProcess(Child);
impl Drop for BrokerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn codex_only_broker_serves_distinct_accounts_and_authenticates_metadata() {
    let sandbox = Sandbox::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let child = sandbox
        .command()
        .env("SYNAPS_BROKER_TOKEN", "synthetic-machine-auth")
        .args(["auth-broker", "--bind", &addr.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut process = BrokerProcess(child);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let base = format!("http://{addr}");
    let mut ready = false;
    for _ in 0..100 {
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "broker exited before ready"
        );
        if let Ok(r) = client.get(format!("{base}/healthz")).send().await {
            if r.status().is_success() {
                ready = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready, "Codex/Kimi-only broker did not become healthy");
    let unauth = client
        .get(format!("{base}/capabilities"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status().as_u16(), 401);
    for (label, expected) in [
        ("default", "synthetic-access-default"),
        ("second", "synthetic-access-second"),
    ] {
        let response = client
            .get(format!("{base}/token"))
            .query(&[("provider", "openai-codex"), ("account", label)])
            .bearer_auth("synthetic-machine-auth")
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let data: Value = response.json().await.unwrap();
        assert_eq!(data["access_token"], expected);
        assert!(data.get("refresh").is_none() && data.get("refresh_token").is_none());
    }
    for (label, expected) in [("missing", 404), ("../bad", 400), ("", 400)] {
        let response = client
            .get(format!("{base}/token"))
            .query(&[("provider", "openai-codex"), ("account", label)])
            .bearer_auth("synthetic-machine-auth")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected, "label {label:?}");
        let text = response.text().await.unwrap();
        assert!(!text.contains("synthetic-access") && !text.contains("refresh-synthetic"));
    }
    let capabilities = client
        .get(format!("{base}/capabilities"))
        .bearer_auth("synthetic-machine-auth")
        .send()
        .await
        .unwrap();
    assert!(capabilities.status().is_success());
    let text = capabilities.text().await.unwrap();
    assert!(text.contains("second"));
    assert!(
        !text.contains("synthetic-access")
            && !text.contains("refresh-synthetic")
            && !text.contains("synthetic-static-secret")
    );
}
