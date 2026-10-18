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

#[test]
fn empty_inventory_status_and_keeper_are_safe_without_login() {
    let sandbox = Sandbox::new();
    std::fs::write(sandbox.dir.path().join("auth.json"), "{}").unwrap();
    let status = sandbox.run(&["status", "--all", "--json"]);
    assert_ok(&status);
    let report: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(report["accounts"], json!([]));
    let keeper = sandbox.run(&["quota-keeper", "--once", "--json"]);
    assert!(!keeper.status.success());
    assert!(String::from_utf8_lossy(&keeper.stderr).contains("no connected accounts"));
    assert!(!sandbox.dir.path().join("quota-keeper/state.json").exists());
}

#[test]
fn keeper_requires_explicit_model_and_canonical_local_ledger_before_any_network() {
    let sandbox = Sandbox::new();
    let before = sandbox.auth();
    let invalid: &[&[&str]] = &[
        &[
            "quota-keeper",
            "--once",
            "--activate",
            "openai-codex@second",
        ],
        &[
            "quota-keeper",
            "--once",
            "--activate",
            "openai-codex@second",
            "--model",
            "gpt-5.4-mini",
            "--state-dir",
            "/unused-test-ledger",
        ],
        &[
            "quota-keeper",
            "--once",
            "--activate",
            "openai-codex@second",
            "--model",
            "gpt-imaginary-model",
        ],
        &["quota-keeper", "--once", "--rearm", "openai-codex@second"],
    ];
    for args in invalid {
        let output = sandbox.run(args);
        assert!(!output.status.success(), "unexpected success for {args:?}");
        assert_eq!(sandbox.auth(), before);
        assert!(!sandbox.dir.path().join("quota-keeper/state.json").exists());
    }
    // The unreachable endpoint would fail differently if activation tried to
    // list or vend remotely. It must reject the mode before touching network.
    let remote = sandbox
        .command()
        .env("SYNAPS_AUTH_ENDPOINT", "http://127.0.0.1:1")
        .env("SYNAPS_MACHINE_TOKEN", "synthetic-machine-auth")
        .args([
            "quota-keeper",
            "--once",
            "--activate",
            "openai-codex@second",
            "--model",
            "gpt-5.4-mini",
        ])
        .output()
        .unwrap();
    assert!(!remote.status.success());
    assert!(String::from_utf8_lossy(&remote.stderr)
        .contains("activation is only permitted on the broker host"));
    assert_eq!(sandbox.auth(), before);
}

#[test]
fn account_policy_env_overrides_selection_without_moving_credentials() {
    let sandbox = Sandbox::new();
    let before = sandbox.auth();
    assert_ok(&sandbox.run(&[
        "auth",
        "use",
        "--provider",
        "openai-codex",
        "--account",
        "auto",
    ]));
    let auto = sandbox.run(&["auth", "list", "--provider", "openai-codex", "--json"]);
    assert_ok(&auto);
    let report: Value = serde_json::from_slice(&auto.stdout).unwrap();
    assert_eq!(report["selectors"]["openai-codex"]["kind"], "auto");
    let selected = sandbox
        .command()
        .env("SYNAPS_ACCOUNT_OPENAI_CODEX", "second")
        .args(["auth", "list", "--provider", "openai-codex", "--json"])
        .output()
        .unwrap();
    assert_ok(&selected);
    let report: Value = serde_json::from_slice(&selected.stdout).unwrap();
    assert_eq!(report["selectors"]["openai-codex"]["account"], "second");
    assert_eq!(sandbox.auth(), before);
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

/// #147 C1 end to end: a loopback broker started with NO token configured
/// generates `<base_dir>/auth-broker.token` (0600) and requires it; a local
/// `synaps` pointed at the loopback endpoint with no token configured reads
/// that file and works with zero config. A DNS-rebinding Host is refused.
#[tokio::test]
async fn zero_config_loopback_broker_requires_its_generated_token() {
    let sandbox = Sandbox::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let child = sandbox
        .command()
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
    assert!(ready, "zero-config loopback broker did not become healthy");

    let token_path = sandbox.dir.path().join("auth-broker.token");
    let token = std::fs::read_to_string(&token_path)
        .expect("token file generated next to auth.json")
        .trim()
        .to_string();
    assert_eq!(token.len(), 64);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&token_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // No bearer: refused. Was open before (#147 C1).
    let unauth = client
        .get(format!("{base}/token"))
        .query(&[("provider", "openai-codex")])
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status().as_u16(), 401);
    let rebinding = client
        .get(format!("{base}/capabilities"))
        .header(reqwest::header::HOST, format!("attacker.example:{}", addr.port()))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(rebinding.status().as_u16(), 403);
    let authed = client
        .get(format!("{base}/capabilities"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(authed.status().is_success());

    // A local client with only the endpoint configured picks up the token.
    let listed = sandbox
        .command()
        .env("SYNAPS_AUTH_ENDPOINT", &base)
        .args(["auth", "list", "--provider", "openai-codex", "--json"])
        .output()
        .unwrap();
    assert_ok(&listed);
    let text = String::from_utf8_lossy(&listed.stdout);
    assert!(text.contains("second"), "{text}");
    assert!(!text.contains(&token), "machine token never printed");
}
