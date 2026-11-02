//! Subprocess tests for the accounts wire contract v1 (pria.synaps.accounts/1):
//! `auth capabilities`, the additive `auth list --json` keys, `auth order`,
//! and `--json`/`--expect` on `auth remove` and `auth use`. Every pinned reply
//! is compared byte-for-byte with tests/fixtures/accounts_v1/*.json. Each
//! process gets an empty environment and a private synthetic store; no
//! browser, network, daemon or real credential is involved.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/accounts_v1");
const CONFIG: &str = "auth.account.anthropic = auto\n";

struct Sandbox {
    dir: tempfile::TempDir,
}

fn credential(seat: &str, identity: &str, added_at: u64) -> Value {
    json!({"type":"oauth", "access":format!("synthetic-access-{seat}"),
        "refresh":format!("synthetic-refresh-{seat}"), "expires":1800003600000_u64,
        "accountId":seat, "identity":identity, "addedAt":added_at})
}

impl Sandbox {
    fn new() -> Self {
        let data = json!({
            "anthropic": credential("a1000001-synthetic-seat", "fixture@example.invalid", 1760000000),
            "anthropic@claude2": credential("b2000002-synthetic-seat", "second@example.invalid", 1760000100),
            "anthropic@claude3": credential("c3000003-synthetic-seat", "third@example.invalid", 1760000200),
            "openai-codex": credential("d4000004-synthetic-seat", "codex@example.invalid", 1760000300),
            "openrouter": {"type":"api_key", "key":"synthetic-static-secret"},
            "unrelated": {"metadata":"must survive"}
        });
        Self::with_store(&serde_json::to_vec(&data).unwrap())
    }

    fn with_store(auth_json: &[u8]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("auth.json"), auth_json).unwrap();
        std::fs::write(dir.path().join("config"), CONFIG).unwrap();
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

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn auth_bytes(&self) -> Vec<u8> {
        std::fs::read(self.path("auth.json")).unwrap()
    }

    fn auth(&self) -> Value {
        serde_json::from_slice(&self.auth_bytes()).unwrap()
    }

    fn config(&self) -> String {
        std::fs::read_to_string(self.path("config")).unwrap()
    }
}

fn describe(output: &Output) -> String {
    format!(
        "status={:?} stdout={} stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(output.status.code(), Some(code), "{}", describe(output));
}

/// The one JSON document on stdout (exactly one line ending in LF for the
/// new verbs; `compact` is false for the pretty-printed existing reads).
fn reply(output: &Output, compact: bool) -> Value {
    let text = String::from_utf8(output.stdout.clone()).unwrap();
    assert!(text.ends_with('\n'), "{}", describe(output));
    if compact {
        assert_eq!(text.matches('\n').count(), 1, "{}", describe(output));
    }
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {}", describe(output)))
}

fn revision_of(value: &Value) -> String {
    let rev = value["revision"].as_str().expect("revision").to_string();
    assert!(
        rev.len() == 16 && rev.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "{rev}"
    );
    rev
}

/// Golden replies. Mismatches are collected so one run shows every actual
/// reply (the flow itself continues on the real values).
#[derive(Default)]
struct Golden {
    mismatches: Vec<String>,
}

impl Golden {
    fn check(&mut self, name: &str, stdout: &[u8]) {
        let path = Path::new(FIXTURES).join(format!("{name}.json"));
        let expected = std::fs::read(&path).unwrap_or_default();
        if expected != stdout {
            self.mismatches.push(format!(
                "--- {} differs; actual stdout:\n{}",
                path.display(),
                String::from_utf8_lossy(stdout)
            ));
        }
    }

    fn finish(self) {
        assert!(self.mismatches.is_empty(), "\n{}", self.mismatches.join("\n"));
    }
}

#[test]
fn golden_replies_and_the_compare_and_set_flow() {
    let sandbox = Sandbox::new();
    let mut golden = Golden::default();

    let output = sandbox.run(&["auth", "capabilities", "--json"]);
    assert_exit(&output, 0);
    golden.check("capabilities", &output.stdout);
    assert_eq!(reply(&output, true)["accounts_api"], 1);

    let output = sandbox.run(&["auth", "list", "--json"]);
    assert_exit(&output, 0);
    golden.check("list", &output.stdout);
    let list = reply(&output, false);
    let r0 = revision_of(&list);
    assert_eq!(list["schema_version"], 1);
    assert_eq!(
        list["order"],
        json!({"anthropic":["default","claude2","claude3"], "openai-codex":["default"]})
    );
    assert_eq!(
        list["strategy"],
        json!({"anthropic":"soonest_reset", "openai-codex":"soonest_reset"})
    );
    for secret in ["synthetic-access", "synthetic-refresh", "synthetic-static-secret"] {
        assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    }
    // A provider filter narrows the rows, never the store-wide revision.
    let filtered = reply(&sandbox.run(&["auth", "list", "--json", "--provider", "openai-codex"]), false);
    assert_eq!(filtered["order"], json!({"openai-codex":["default"]}));
    assert_eq!(revision_of(&filtered), r0);

    let output = sandbox.run(&["auth", "order", "--provider", "anthropic", "--json"]);
    assert_exit(&output, 0);
    golden.check("order_read", &output.stdout);
    let read = reply(&output, true);
    assert_eq!(read["order"], json!(["default", "claude2", "claude3"]));
    assert_eq!(read["strategy"], "soonest_reset");
    assert_eq!(revision_of(&read), r0);

    // A stale --expect writes nothing and answers the current revision.
    let store = sandbox.auth_bytes();
    let set = ["auth", "order", "--provider", "anthropic", "--json", "--set"];
    let stale = [&set[..], &["claude3,default,claude2", "--strict", "--expect", "0000000000000000"][..]].concat();
    let output = sandbox.run(&stale);
    assert_exit(&output, 1);
    golden.check("order_conflict", &output.stdout);
    let conflict = reply(&output, true);
    assert_eq!(conflict["error"], "conflict");
    assert_eq!(revision_of(&conflict), r0);
    assert_eq!(sandbox.config(), CONFIG);

    // --set must name exactly the provider's labels.
    let partial = [&set[..], &["claude3,default", "--expect", r0.as_str()][..]].concat();
    let output = sandbox.run(&partial);
    assert_exit(&output, 1);
    assert_eq!(output.stdout, b"{\"v\":1,\"ok\":false,\"error\":\"order_mismatch\"}\n");
    assert_eq!(sandbox.config(), CONFIG);

    let write = [&set[..], &["claude3,default,claude2", "--strict", "--expect", r0.as_str()][..]].concat();
    let output = sandbox.run(&write);
    assert_exit(&output, 0);
    let written = reply(&output, true);
    assert_eq!(written["order"], json!(["claude3", "default", "claude2"]));
    assert_eq!(written["strategy"], "preference_order");
    let r1 = revision_of(&written);
    assert_ne!(r1, r0);
    assert_eq!(
        sandbox.config(),
        "auth.account.anthropic = auto\nauth.order.anthropic = claude3,default,claude2\n\
         auth.auto.strategy.anthropic = preference_order\n"
    );
    assert_eq!(sandbox.auth_bytes(), store, "order never writes auth.json");

    // remove: the revision check and the removal share the auth.json lock.
    let remove = ["auth", "remove", "--provider", "anthropic", "--account", "claude2", "-y", "--json"];
    let output = sandbox.run(&[&remove[..], &["--expect-rev", r0.as_str()][..]].concat());
    assert_exit(&output, 1);
    golden.check("remove_conflict", &output.stdout);
    assert_eq!(revision_of(&reply(&output, true)), r1);
    assert_eq!(sandbox.auth_bytes(), store, "a refused remove writes nothing");

    let before = sandbox.auth();
    let output = sandbox.run(&[&remove[..], &["--expect-rev", r1.as_str()][..]].concat());
    assert_exit(&output, 0);
    golden.check("remove_ok", &output.stdout);
    let removed = reply(&output, true);
    assert_eq!(removed["removed"], json!({"provider":"anthropic", "label":"claude2"}));
    let r2 = revision_of(&removed);
    assert_ne!(r2, r1);
    let after = sandbox.auth();
    assert!(after.get("anthropic@claude2").is_none());
    for key in ["anthropic", "anthropic@claude3", "openai-codex", "openrouter", "unrelated"] {
        assert_eq!(after[key], before[key], "{key} must survive");
    }

    let output = sandbox.run(&[
        "auth", "use", "--provider", "anthropic", "--account", "claude3", "--json", "--expect", r2.as_str(),
    ]);
    assert_exit(&output, 0);
    golden.check("use_ok", &output.stdout);
    let used = reply(&output, true);
    assert_eq!(used["selector"], json!({"kind":"account", "account":"claude3"}));
    let r3 = revision_of(&used);
    assert_ne!(r3, r2);
    assert_eq!(
        sandbox.config(),
        "auth.account.anthropic = claude3\nauth.order.anthropic = claude3,default,claude2\n\
         auth.auto.strategy.anthropic = preference_order\n"
    );

    // The plain list sees the same state and the same revision.
    let list = reply(&sandbox.run(&["auth", "list", "--json"]), false);
    assert_eq!(revision_of(&list), r3);
    assert_eq!(list["order"]["anthropic"], json!(["claude3", "default"]));
    assert_eq!(list["strategy"]["anthropic"], "preference_order");
    assert_eq!(list["selectors"]["anthropic"], json!({"kind":"account", "account":"claude3"}));

    // --tie-break alone flips the strategy back and keeps the order.
    let output = sandbox.run(&[
        "auth", "order", "--provider", "claude", "--json", "--tie-break", "--expect", r3.as_str(),
    ]);
    assert_exit(&output, 0);
    let flipped = reply(&output, true);
    assert_eq!(flipped["provider"], "anthropic");
    assert_eq!(flipped["order"], json!(["claude3", "default"]));
    assert_eq!(flipped["strategy"], "soonest_reset");
    assert_ne!(revision_of(&flipped), r3);

    golden.finish();
}

#[test]
fn refusals_print_one_closed_code_and_write_nothing() {
    let sandbox = Sandbox::new();
    let store = sandbox.auth_bytes();
    let rev = revision_of(&reply(&sandbox.run(&["auth", "list", "--json"]), false));
    let refused = |args: &[&str], code: &str| {
        let output = sandbox.run(args);
        assert_exit(&output, 1);
        let expected = format!("{{\"v\":1,\"ok\":false,\"error\":\"{code}\"}}\n");
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected, "{args:?}");
    };
    refused(&["auth", "order", "--provider", "nope", "--json"], "unknown_provider");
    refused(&["auth", "order", "--provider", "anthropic", "--json", "--expect", "XYZ"], "bad_request");
    refused(
        &["auth", "order", "--provider", "anthropic", "--json", "--set", "default,,claude2", "--expect", rev.as_str()],
        "bad_request",
    );
    refused(
        &["auth", "order", "--provider", "anthropic", "--json", "--set", "default,claude2,claude3,ghost"],
        "order_mismatch",
    );
    refused(&["auth", "use", "--provider", "anthropic", "--account", "ghost", "--json"], "not_found");
    refused(&["auth", "use", "--provider", "anthropic", "--account", "Bad Label", "--json"], "bad_request");
    refused(&["auth", "use", "--provider", "nope", "--account", "default", "--json"], "unknown_provider");
    refused(&["auth", "remove", "--provider", "anthropic", "--account", "ghost", "-y", "--json"], "not_found");
    refused(&["auth", "remove", "--provider", "anthropic", "--account", "claude2", "--json"], "bad_request");
    refused(&["auth", "remove", "--provider", "anthropic", "--account", "auto", "-y", "--json"], "bad_request");
    refused(
        &["auth", "remove", "--provider", "openai-codex", "--account", "default", "-y", "--json", "--expect-rev", "x"],
        "bad_request",
    );

    // use --expect: a mismatch answers conflict with the current revision.
    let output = sandbox.run(&[
        "auth", "use", "--provider", "anthropic", "--account", "claude2", "--json", "--expect",
        "0000000000000000",
    ]);
    assert_exit(&output, 1);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("{{\"v\":1,\"ok\":false,\"error\":\"conflict\",\"revision\":\"{rev}\"}}\n")
    );

    // A profile without its own store must not edit the inherited one.
    let output = sandbox
        .command()
        .env("SYNAPS_PROFILE", "work")
        .args(["auth", "remove", "--provider", "anthropic", "--account", "claude2", "-y", "--json"])
        .output()
        .unwrap();
    assert_exit(&output, 1);
    assert_eq!(output.stdout, b"{\"v\":1,\"ok\":false,\"error\":\"refused_profile\"}\n");

    assert_eq!(sandbox.auth_bytes(), store);
    assert_eq!(sandbox.config(), CONFIG);
    let list = reply(&sandbox.run(&["auth", "list", "--json"]), false);
    assert_eq!(revision_of(&list), rev);
}

#[test]
fn usage_errors_exit_2_and_a_bad_store_is_store_unreadable() {
    let sandbox = Sandbox::new();
    for args in [
        &["auth", "use", "--provider", "anthropic", "--account", "default", "--expect", "0000000000000000"][..],
        &["auth", "remove", "--provider", "anthropic", "--account", "claude2", "-y", "--expect-rev", "0000000000000000"][..],
        &["auth", "order", "--provider", "anthropic", "--json", "--strict", "--tie-break"][..],
        &["auth", "order", "--json"][..],
    ] {
        let output = sandbox.run(args);
        assert_exit(&output, 2);
        assert!(output.stdout.is_empty(), "{}", describe(&output));
    }
    // The app's too-old discriminator is clap's own text for a missing verb.
    let output = sandbox.run(&["auth", "no-such-verb", "--json"]);
    assert_exit(&output, 2);
    assert!(String::from_utf8_lossy(&output.stderr).contains("unrecognized subcommand 'no-such-verb'"));

    let broken = Sandbox::with_store(b"{not json");
    for args in [&["auth", "list", "--json"][..], &["auth", "list", "--json", "--provider", "anthropic"][..]] {
        let output = broken.run(args);
        assert_exit(&output, 1);
        assert_eq!(output.stdout, b"{\"v\":1,\"ok\":false,\"error\":\"store_unreadable\"}\n");
    }
    assert_eq!(broken.auth_bytes(), b"{not json");
}

#[test]
fn text_modes_stay_human_and_plain_verbs_are_unchanged() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["auth", "capabilities"]);
    assert_exit(&output, 0);
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("accounts api 1;"));

    let output = sandbox.run(&["auth", "order", "--provider", "anthropic"]);
    assert_exit(&output, 0);
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        text.starts_with("anthropic order: default, claude2, claude3 (strategy soonest_reset, revision "),
        "{text}"
    );

    // The pre-existing plain forms keep working without --json.
    let output = sandbox.run(&["auth", "use", "--provider", "anthropic", "--account", "claude2"]);
    assert_exit(&output, 0);
    assert!(sandbox.config().contains("auth.account.anthropic = claude2"));
    let output = sandbox.run(&["auth", "remove", "--provider", "anthropic", "--account", "claude3", "-y"]);
    assert_exit(&output, 0);
    assert!(sandbox.auth().get("anthropic@claude3").is_none());
}
