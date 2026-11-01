//! `synaps config set` end to end: the real binary on a private base dir that
//! does not exist yet, as on a machine where Synaps never ran. No daemon is
//! started or contacted and no provider is involved.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_synaps"));
        command.args(["config", "set"]);
        self.output(command, &self.base(), args)
    }

    /// `synaps config set <args>` under umask 022 (a common default), with
    /// `base` as the base dir.
    #[cfg(unix)]
    fn run_umask_022(&self, base: &Path, args: &[&str]) -> Output {
        let mut command = Command::new("/bin/sh");
        let script = "umask 022 && exec \"$0\" config set \"$@\"";
        command.args(["-c", script, env!("CARGO_BIN_EXE_synaps")]);
        self.output(command, base, args)
    }

    fn output(&self, mut command: Command, base: &Path, args: &[&str]) -> Output {
        command
            .env_clear()
            .env("HOME", self.dir.path())
            .env("USERPROFILE", self.dir.path())
            .env("SYNAPS_BASE_DIR", base)
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "dumb")
            .current_dir(self.dir.path())
            .stdin(Stdio::null())
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

/// The setup doctor's three keys take `--if-unset` only: a plain set or an
/// `--expect` of each is refused (3), as is anything under auth.account.*
/// but `auto` for a known provider, and a refusal writes nothing. A root path
/// with spaces is written whole, and reads back equal.
#[test]
#[cfg(unix)]
fn if_unset_only_keys_refuse_every_other_write_and_keep_a_spaced_path_whole() {
    let sandbox = Sandbox::new();
    let extra = "/Users/me/Library/Application Support/pria.desktop.dev/skills/current";
    let extra_pair = format!("skills.extra_root={extra}");
    let three = [
        "context_management.mode=auto",
        extra_pair.as_str(),
        "auth.account.anthropic=auto",
    ];
    let mut refused: Vec<Vec<String>> = Vec::new();
    for pair in three {
        let key = pair.split_once('=').unwrap().0;
        refused.push(vec![pair.into()]);
        refused.push(vec!["--expect".into(), format!("{key}="), pair.into()]);
    }
    for pair in [
        "auth.account.anthropic=work",
        "auth.account.anthropic=",
        "auth.account.openai=auto",
    ] {
        refused.push(vec!["--if-unset".into(), pair.into()]);
    }
    for args in &refused {
        let key = args.last().unwrap().split_once('=').unwrap().0;
        let mut argv = vec!["--json"];
        argv.extend(args.iter().map(String::as_str));
        let output = sandbox.run(&argv);
        assert_eq!(output.status.code(), Some(3), "{argv:?}");
        let report = json_line(&output);
        assert_eq!(report["result"], "refused", "{argv:?}");
        assert_eq!(report["keys"], json!({ key: "not_settable" }), "{argv:?}");
        assert!(!sandbox.config().exists(), "{argv:?}");
    }

    let mut argv = vec!["--json", "--if-unset"];
    argv.extend(three);
    let output = sandbox.run(&argv);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(json_line(&output)["result"], "written");
    let written = std::fs::read_to_string(sandbox.config()).unwrap();
    let want = format!(
        "context_management.mode = auto\nskills.extra_root = {extra}\nauth.account.anthropic = auto\n"
    );
    assert_eq!(written, want);
    let again = sandbox.run(&argv);
    assert_eq!(again.status.code(), Some(0));
    assert_eq!(
        json_line(&again)["result"],
        "unchanged",
        "the spaced path reads back equal"
    );
}

/// Under `--if-unset` a key whose one line is `key =` is `empty` (in the JSON,
/// and as `<key>: empty` on stderr), and a key with two lines is `duplicate`
/// in every mode and in either order. Both refuse (3) and write nothing.
#[test]
#[cfg(unix)]
fn an_empty_line_is_empty_and_two_lines_are_duplicate_and_nothing_is_written() {
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.base()).unwrap();
    let extra = "/Users/me/Library/Application Support/pria.desktop.dev/skills/current";
    let extra_pair = format!("skills.extra_root={extra}");
    for (key, pair) in [
        ("skills.extra_root", extra_pair.as_str()),
        ("auth.account.anthropic", "auth.account.anthropic=auto"),
    ] {
        let original = format!("{key} =\n");
        std::fs::write(sandbox.config(), &original).unwrap();
        let output = sandbox.run(&["--json", "--if-unset", pair]);
        assert_eq!(output.status.code(), Some(3));
        let report = json_line(&output);
        assert_eq!(report["result"], "refused");
        assert_eq!(report["keys"], json!({ key: "empty" }));

        let output = sandbox.run(&["--if-unset", pair]);
        assert_eq!(output.status.code(), Some(3));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.starts_with(&format!("{key}: empty\n")), "{stderr}");
        assert_eq!(std::fs::read_to_string(sandbox.config()).unwrap(), original);
    }

    for (key, pair, value) in [
        ("skills.extra_root", extra_pair.as_str(), extra),
        ("memory.backend", "memory.backend=axel", "axel"),
    ] {
        let mut runs = vec![vec!["--json", "--if-unset", pair]];
        if key == "memory.backend" {
            runs.push(vec!["--json", pair]);
            runs.push(vec!["--json", "--expect", "memory.backend=axel", pair]);
        }
        for original in [
            format!("{key} = {value}\n{key} =\n"),
            format!("{key} =\n{key} = {value}\n"),
        ] {
            std::fs::write(sandbox.config(), &original).unwrap();
            for argv in &runs {
                let output = sandbox.run(argv);
                assert_eq!(output.status.code(), Some(3), "{argv:?}");
                let report = json_line(&output);
                assert_eq!(report["result"], "refused", "{argv:?}");
                assert_eq!(report["keys"], json!({ key: "duplicate" }), "{argv:?}");
                assert_eq!(std::fs::read_to_string(sandbox.config()).unwrap(), original);
            }
        }
    }
}

/// A fresh install (no base dir, no config file, no profile): the doctor's
/// `--if-unset ... --expect-path P` call creates `<base>/config`, owner-only.
/// P is the canonical path of `<base>`'s existing parent joined with the rest,
/// which is what the reply's "path" says.
#[test]
#[cfg(unix)]
fn a_fresh_install_is_filled_by_if_unset_with_expect_path() {
    let sandbox = Sandbox::new();
    assert!(!sandbox.base().exists());
    let parent = std::fs::canonicalize(sandbox.dir.path()).unwrap();
    let expected = parent.join(".synaps-cli").join("config");
    let expected = expected.to_str().unwrap();
    let args = [
        "--json",
        "--if-unset",
        "--expect-path",
        expected,
        "context_management.mode=auto",
    ];
    let output = sandbox.run(&args);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert_eq!(
        json_line(&output),
        json!({
            "schema": "synaps.config-set/1",
            "result": "written",
            "path": expected,
            "keys": {"context_management.mode": "written"},
            "applies": "next daemon start",
        })
    );
    let written = std::fs::read_to_string(sandbox.config()).unwrap();
    assert_eq!(written, "context_management.mode = auto\n");
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(sandbox.config()).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
}

/// Under umask 022, the base dir the doctor's fill makes is 0700, and only
/// that one: its missing parents get the umask's 0755, and an existing 0755
/// base dir stays 0755.
#[test]
#[cfg(unix)]
fn a_base_dir_config_set_makes_is_0700_and_an_existing_one_keeps_its_mode() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let sandbox = Sandbox::new();
    let root = std::fs::canonicalize(sandbox.dir.path()).unwrap();
    let fill = |base: &Path| {
        let expected = base.join("config");
        let args = [
            "--json",
            "--if-unset",
            "--expect-path",
            expected.to_str().unwrap(),
            "context_management.mode=auto",
        ];
        let output = sandbox.run_umask_022(base, &args);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert_eq!(output.status.code(), Some(0), "{stderr}");
        assert_eq!(json_line(&output)["result"], "written");
        assert_eq!(mode(&expected), 0o600);
    };

    // A fresh install: no base dir.
    let base = root.join(".synaps-cli");
    assert!(!base.exists());
    fill(&base);
    assert_eq!(mode(&base), 0o700);

    // A base dir under two missing parents.
    let parents = root.join("a").join("b");
    fill(&parents.join(".synaps-cli"));
    assert_eq!(mode(&parents.join(".synaps-cli")), 0o700);
    assert_eq!(mode(&root.join("a")), 0o755);
    assert_eq!(mode(&parents), 0o755);

    // An existing 0755 base dir.
    let base = root.join("existing");
    std::fs::create_dir(&base).unwrap();
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
    fill(&base);
    assert_eq!(mode(&base), 0o755);
}
