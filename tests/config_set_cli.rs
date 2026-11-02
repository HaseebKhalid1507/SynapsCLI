//! `synaps config set` end to end: the real binary on a private base dir that
//! does not exist yet, as on a machine where Synaps never ran. No daemon is
//! started or contacted and no provider is involved.
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn base(&self) -> PathBuf {
        self.dir.path().join(".synaps-cli")
    }

    fn config(&self) -> PathBuf {
        self.base().join("config")
    }

    /// An absolute path, as `memory.axel.executable` requires.
    fn executable(&self) -> String {
        let path = self.dir.path().join("synaps-axel-memory-service");
        path.to_str().unwrap().to_string()
    }

    /// `synaps config set <args>`.
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_synaps"))
            .env_clear()
            .env("HOME", self.dir.path())
            .env("USERPROFILE", self.dir.path())
            .env("SYNAPS_BASE_DIR", self.base())
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "dumb")
            .current_dir(self.dir.path())
            .stdin(Stdio::null())
            .args(["config", "set"])
            .args(args)
            .output()
            .unwrap()
    }
}

/// The single JSON line on stdout.
fn json_line(output: &Output) -> Value {
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.ends_with('\n'), "stdout={stdout:?} stderr={stderr}");
    assert_eq!(stdout.lines().count(), 1, "stdout={stdout:?} stderr={stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// The desktop installer's call: written once, then unchanged, and never
/// over a value the user set.
#[test]
fn if_unset_two_keys_writes_once_then_unchanged_then_refuses_a_user_value() {
    let sandbox = Sandbox::new();
    let exe = sandbox.executable();
    let executable = format!("memory.axel.executable={exe}");
    let args = ["--json", "--if-unset", "memory.backend=axel", executable.as_str()];

    let first = sandbox.run(&args);
    assert_eq!(first.status.code(), Some(0), "{}", String::from_utf8_lossy(&first.stderr));
    let canonical = std::fs::canonicalize(sandbox.config()).unwrap();
    assert_eq!(
        json_line(&first),
        json!({
            "schema": "synaps.config-set/1",
            "result": "written",
            "path": canonical.to_str().unwrap(),
            "keys": {"memory.backend": "written", "memory.axel.executable": "written"},
            "applies": "next daemon start",
        })
    );
    let written = std::fs::read_to_string(sandbox.config()).unwrap();
    assert_eq!(written, format!("memory.backend = axel\nmemory.axel.executable = {exe}\n"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(sandbox.config()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // Again: nothing to do, and the file is not rewritten.
    let second = sandbox.run(&args);
    assert_eq!(second.status.code(), Some(0));
    let report = json_line(&second);
    assert_eq!(report["result"], "unchanged");
    let unchanged = json!({"memory.backend": "unchanged", "memory.axel.executable": "unchanged"});
    assert_eq!(report["keys"], unchanged);
    assert_eq!(std::fs::read_to_string(sandbox.config()).unwrap(), written);

    // The user picked the legacy backend: the whole call is refused.
    let user = "# mine\nmemory.backend = legacy\n";
    std::fs::write(sandbox.config(), user).unwrap();
    let third = sandbox.run(&args);
    assert_eq!(third.status.code(), Some(3));
    let report = json_line(&third);
    assert_eq!(report["result"], "refused");
    let conflict = json!({"memory.backend": "conflict", "memory.axel.executable": "unchanged"});
    assert_eq!(report["keys"], conflict);
    assert_eq!(std::fs::read_to_string(sandbox.config()).unwrap(), user);
}

#[test]
fn output_never_echoes_current_values() {
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.base()).unwrap();
    let original = "provider.anthropic = sk-ant-SECRET-other\nmemory.backend = SECRET-hand-set\n";
    std::fs::write(sandbox.config(), original).unwrap();
    let executable = format!("memory.axel.executable={}", sandbox.executable());
    let executable = executable.as_str();
    let runs: [(&[&str], i32); 4] = [
        (&["--json", "--if-unset", "memory.backend=axel", executable], 3),
        (&["--if-unset", "memory.backend=axel", executable], 3),
        (&["--json", "--expect", "memory.backend=legacy", "memory.backend=axel"], 3),
        (&["--json", "--expect", "provider.anthropic=", "memory.backend=axel"], 3),
    ];
    for (args, code) in runs {
        let output = sandbox.run(args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(code), "{args:?}: {stdout} {stderr}");
        assert!(!stdout.contains("SECRET"), "{args:?}: {stdout}");
        assert!(!stderr.contains("SECRET"), "{args:?}: {stderr}");
    }
    assert_eq!(std::fs::read_to_string(sandbox.config()).unwrap(), original);
}

#[test]
fn keys_outside_the_allowlist_refuse_and_bad_input_exits_2_and_nothing_is_written() {
    let sandbox = Sandbox::new();
    // A real config key, but not a settable one: refused (exit 3).
    let output = sandbox.run(&["--json", "model=claude-opus-4-6", "memory.backend=axel"]);
    assert_eq!(output.status.code(), Some(3));
    let report = json_line(&output);
    assert_eq!(report["result"], "refused");
    let keys = json!({"model": "not_settable", "memory.backend": "unchanged"});
    assert_eq!(report["keys"], keys);

    let bad: [&[&str]; 4] = [
        &["--json", "memory.backend=bogus"],
        &["--json", "memory.backend"],
        &["--json", "memory.backend=axel", "memory.backend=legacy"],
        &["--json", "memory.backend=ax\u{1b}el"],
    ];
    for args in bad {
        let output = sandbox.run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        let report = json_line(&output);
        assert_eq!(report["schema"], "synaps.config-set/1");
        assert_eq!(report["result"], "error");
        assert_eq!(report["error"], "usage");
        assert!(report["message"].is_string());
    }
    // clap's own usage error.
    let both = ["--if-unset", "--expect", "memory.backend=", "memory.backend=axel"];
    assert_eq!(sandbox.run(&both).status.code(), Some(2));
    assert!(!sandbox.config().exists());
}

#[test]
fn expect_path_must_be_the_canonical_config_path() {
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.base()).unwrap();
    let canonical = std::fs::canonicalize(sandbox.base()).unwrap().join("config");
    let canonical = canonical.to_str().unwrap();
    let other = sandbox.dir.path().join("elsewhere").join("config");

    let args = ["--json", "--expect-path", other.to_str().unwrap(), "memory.backend=axel"];
    let output = sandbox.run(&args);
    assert_eq!(output.status.code(), Some(3));
    let report = json_line(&output);
    assert_eq!(report["result"], "refused");
    assert_eq!(report["path"], canonical);
    assert_eq!(report["keys"], json!({"memory.backend": "path_mismatch"}));
    assert!(!sandbox.config().exists());

    let output = sandbox.run(&["--json", "--expect-path", canonical, "memory.backend=axel"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json_line(&output)["result"], "written");
    assert_eq!(std::fs::read_to_string(sandbox.config()).unwrap(), "memory.backend = axel\n");
}

/// `--profile` selects the profile's own config file, which must exist:
/// otherwise the checks would read the base config but write another file.
#[test]
fn profile_config_is_written_only_when_it_exists() {
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.base()).unwrap();
    std::fs::write(sandbox.config(), "memory.backend = legacy\n").unwrap();
    let args = ["--profile", "work", "--json", "memory.backend=axel"];

    let output = sandbox.run(&args);
    assert_eq!(output.status.code(), Some(2));
    let report = json_line(&output);
    assert_eq!(report["result"], "error");
    assert_eq!(report["error"], "usage");
    assert!(!sandbox.base().join("work").exists());

    let profile_config = sandbox.base().join("work").join("config");
    std::fs::create_dir_all(sandbox.base().join("work")).unwrap();
    std::fs::write(&profile_config, "").unwrap();
    let output = sandbox.run(&args);
    assert_eq!(output.status.code(), Some(0));
    let report = json_line(&output);
    let canonical = std::fs::canonicalize(&profile_config).unwrap();
    assert_eq!(report["path"], canonical.to_str().unwrap());
    assert_eq!(std::fs::read_to_string(&profile_config).unwrap(), "memory.backend = axel\n");
    let base = std::fs::read_to_string(sandbox.config()).unwrap();
    assert_eq!(base, "memory.backend = legacy\n");
}
