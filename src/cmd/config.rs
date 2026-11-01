//! `synaps config set` — change config keys from scripts and installers.
//!
//! Works on the config file alone: it never starts or contacts the daemon
//! and loads no provider or plugin. A written value applies from the next
//! daemon start. The checks and the single atomic write are
//! [`synaps_cli::config::write_config_values_checked`]; this module parses
//! arguments and maps its outcome to output and an exit code.

use std::io::Write as _;
use std::path::PathBuf;
use synaps_cli::config::{
    write_config_values_checked, ConfigKeyStatus, ConfigSetError, ConfigSetMode,
    ConfigSetOutcome, ConfigSetResult, IF_UNSET_ONLY_CONFIG_KEYS, SETTABLE_CONFIG_KEYS,
};

/// Written, or every value was already set.
const EXIT_OK: i32 = 0;
/// The config file could not be read or written.
const EXIT_IO: i32 = 1;
/// Malformed or rejected input.
const EXIT_USAGE: i32 = 2;
/// Refused: a key is not settable or a check failed (`--if-unset`,
/// `--expect`, `--expect-path`); nothing written. Always `"result":"refused"`.
const EXIT_REFUSED: i32 = 3;

/// `schema` of every `--json` line.
const SCHEMA: &str = "synaps.config-set/1";
/// When a written value takes effect.
const APPLIES: &str = "next daemon start";

#[derive(clap::Subcommand, Debug)]
pub enum ConfigAction {
    /// Set config keys in one atomic write, or write nothing.
    ///
    /// Settable keys: memory.backend, memory.axel.executable. With
    /// --if-unset only: context_management.mode, skills.extra_root and
    /// auth.account.<provider>=auto. A key with more than one line in the
    /// file is always refused. Exit codes: 0 written or unchanged,
    /// 3 refused, 2 usage or invalid value, 1 I/O error. Applies from the
    /// next daemon start.
    Set(SetArgs),
}

#[derive(clap::Args, Debug)]
pub struct SetArgs {
    /// Print one JSON line (schema synaps.config-set/1); never prints a
    /// current value.
    #[arg(long)]
    json: bool,
    /// Write only unset keys; a key set to another value, or to an empty
    /// one (`key =`), refuses the call.
    #[arg(long, conflicts_with = "expect")]
    if_unset: bool,
    /// Compare-and-set: KEY must currently be VALUE (empty VALUE: unset).
    /// Repeatable.
    #[arg(long, value_name = "KEY=VALUE")]
    expect: Vec<String>,
    /// Refuse unless the config file's canonical path is exactly this one.
    #[arg(long, value_name = "ABS_PATH")]
    expect_path: Option<PathBuf>,
    /// Keys and values to set.
    #[arg(value_name = "KEY=VALUE", required = true)]
    pairs: Vec<String>,
}

/// Run `synaps config <action>`; returns the process exit code.
pub fn run(action: ConfigAction) -> i32 {
    match action {
        ConfigAction::Set(args) => {
            let result = request(&args).and_then(|(pairs, mode)| {
                write_config_values_checked(&pairs, &mode, args.expect_path.as_deref())
            });
            let report = report(args.json, &result);
            report.emit();
            report.code
        }
    }
}

/// The pairs and mode `args` ask for.
fn request(args: &SetArgs) -> Result<(Vec<(String, String)>, ConfigSetMode), ConfigSetError> {
    let pairs = parse_pairs("argument", &args.pairs)?;
    let mode = if args.if_unset {
        ConfigSetMode::IfUnset
    } else if args.expect.is_empty() {
        ConfigSetMode::Overwrite
    } else {
        ConfigSetMode::Expect(parse_pairs("--expect value", &args.expect)?)
    };
    Ok((pairs, mode))
}

/// Split `KEY=VALUE` at its first `=`; the value may contain `=` itself.
fn parse_pair(arg: &str) -> Option<(String, String)> {
    let (key, value) = arg.split_once('=')?;
    Some((key.to_string(), value.to_string()))
}

fn parse_pairs(what: &str, args: &[String]) -> Result<Vec<(String, String)>, ConfigSetError> {
    args.iter()
        .enumerate()
        .map(|(index, arg)| {
            // Not echoed: a malformed argument may be a misplaced secret.
            parse_pair(arg).ok_or_else(|| {
                ConfigSetError::Invalid(format!("{what} {} is not KEY=VALUE", index + 1))
            })
        })
        .collect()
}

/// Everything `config set` prints, and its exit code.
#[derive(Debug, PartialEq, Eq)]
struct Report {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Report {
    fn on_stdout(code: i32, text: String) -> Self {
        Self {
            code,
            stdout: text,
            stderr: String::new(),
        }
    }

    fn on_stderr(code: i32, text: String) -> Self {
        Self {
            code,
            stdout: String::new(),
            stderr: text,
        }
    }

    fn emit(&self) {
        // The caller may `process::exit` next: flush explicitly.
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(self.stdout.as_bytes());
        let _ = stdout.flush();
        let _ = std::io::stderr().write_all(self.stderr.as_bytes());
    }
}

fn report(json: bool, result: &Result<ConfigSetOutcome, ConfigSetError>) -> Report {
    match result {
        Ok(outcome) => outcome_report(json, outcome),
        Err(err) => error_report(json, err),
    }
}

fn outcome_code(outcome: &ConfigSetOutcome) -> i32 {
    match outcome.result {
        ConfigSetResult::Written | ConfigSetResult::Unchanged => EXIT_OK,
        // One code per result: a not-settable key is a refusal too.
        ConfigSetResult::Refused => EXIT_REFUSED,
    }
}

fn has_not_settable(outcome: &ConfigSetOutcome) -> bool {
    outcome.keys.iter().any(|(_, status)| *status == ConfigKeyStatus::NotSettable)
}

fn outcome_report(json: bool, outcome: &ConfigSetOutcome) -> Report {
    let code = outcome_code(outcome);
    if json {
        let line = json_line(&JsonOutcome {
            schema: SCHEMA,
            result: outcome.result.as_str(),
            path: outcome.path.to_string_lossy().into_owned(),
            keys: JsonKeys(&outcome.keys),
            applies: APPLIES,
        });
        return Report::on_stdout(code, line);
    }
    let mut text: String = outcome
        .keys
        .iter()
        .map(|(key, status)| format!("{key}: {}\n", status.as_str()))
        .collect();
    if has_not_settable(outcome) {
        text.push_str(&format!("settable keys: {}\n", SETTABLE_CONFIG_KEYS.join(", ")));
        text.push_str(&format!(
            "--if-unset only: {}, auth.account.<provider>=auto\n",
            IF_UNSET_ONLY_CONFIG_KEYS.join(", ")
        ));
    }
    let path = outcome.path.display();
    text.push_str(&match outcome.result {
        ConfigSetResult::Written => format!("wrote {path} (applies on {APPLIES})\n"),
        ConfigSetResult::Unchanged => format!("{path} already has these values\n"),
        ConfigSetResult::Refused => format!("refused: nothing written to {path}\n"),
    });
    if outcome.result == ConfigSetResult::Refused {
        Report::on_stderr(code, text)
    } else {
        Report::on_stdout(code, text)
    }
}

fn error_report(json: bool, err: &ConfigSetError) -> Report {
    let (code, kind) = match err {
        ConfigSetError::Invalid(_) => (EXIT_USAGE, "usage"),
        ConfigSetError::Io(_) => (EXIT_IO, "io"),
    };
    let message = err.to_string();
    if json {
        let line = json_line(&JsonError {
            schema: SCHEMA,
            result: "error",
            error: kind,
            message,
        });
        Report::on_stdout(code, line)
    } else {
        Report::on_stderr(code, format!("synaps config set: {message}\n"))
    }
}

/// `value` as one line of compact JSON.
fn json_line(value: &impl serde::Serialize) -> String {
    let mut line = serde_json::to_string(value).expect("strings always serialize");
    line.push('\n');
    line
}

/// The `--json` line for an outcome, fields in schema order.
#[derive(serde::Serialize)]
struct JsonOutcome<'a> {
    schema: &'static str,
    result: &'static str,
    path: String,
    keys: JsonKeys<'a>,
    applies: &'static str,
}

/// Key statuses as a JSON object, in the order the call named the keys.
struct JsonKeys<'a>(&'a [(String, ConfigKeyStatus)]);

impl serde::Serialize for JsonKeys<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.0.iter().map(|(key, status)| (key, status.as_str())))
    }
}

/// The `--json` line for an error.
#[derive(serde::Serialize)]
struct JsonError {
    schema: &'static str,
    result: &'static str,
    error: &'static str,
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use synaps_cli::config::ConfigKeyStatus::{
        Conflict, Duplicate, Empty, ExpectFailed, NotSettable, PathMismatch, Unchanged, Written,
    };

    #[derive(clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        action: ConfigAction,
    }

    /// `synaps config set <args>` as clap parses it.
    fn parse(args: &[&str]) -> Result<SetArgs, clap::Error> {
        use clap::Parser;
        let cli = Cli::try_parse_from(["config", "set"].iter().chain(args))?;
        let ConfigAction::Set(set) = cli.action;
        Ok(set)
    }

    fn outcome(result: ConfigSetResult, keys: &[(&str, ConfigKeyStatus)]) -> ConfigSetOutcome {
        ConfigSetOutcome {
            result,
            path: PathBuf::from("/home/u/.synaps-cli/config"),
            keys: keys.iter().map(|(key, status)| (key.to_string(), *status)).collect(),
        }
    }

    #[test]
    fn parse_pair_splits_on_the_first_equals() {
        let cases = [
            ("memory.backend=axel", Some(("memory.backend", "axel"))),
            ("memory.axel.executable=/a=b", Some(("memory.axel.executable", "/a=b"))),
            ("memory.backend=", Some(("memory.backend", ""))),
            ("memory.backend", None),
        ];
        for (arg, want) in cases {
            let want = want.map(|(key, value)| (key.to_string(), value.to_string()));
            assert_eq!(parse_pair(arg), want, "{arg}");
        }
        // The error names the argument by position, never by content.
        let args = ["memory.backend=axel".to_string(), "sk-secret".to_string()];
        let err = parse_pairs("argument", &args).unwrap_err().to_string();
        assert_eq!(err, "argument 2 is not KEY=VALUE");
    }

    #[test]
    fn cli_parses_every_mode_and_refuses_if_unset_with_expect() {
        let args = parse(&["--json", "--if-unset", "memory.backend=axel", "a.b=c"]).unwrap();
        assert!(args.json && args.if_unset);
        assert_eq!(args.pairs, ["memory.backend=axel", "a.b=c"]);
        assert_eq!(request(&args).unwrap().1, ConfigSetMode::IfUnset);

        let args = parse(&[
            "--expect",
            "memory.backend=",
            "--expect=memory.axel.executable=/x",
            "--expect-path",
            "/home/u/.synaps-cli/config",
            "memory.backend=axel",
        ])
        .unwrap();
        assert_eq!(args.expect_path, Some(PathBuf::from("/home/u/.synaps-cli/config")));
        let (pairs, mode) = request(&args).unwrap();
        assert_eq!(pairs, [("memory.backend".to_string(), "axel".to_string())]);
        let expects = vec![
            ("memory.backend".to_string(), String::new()),
            ("memory.axel.executable".to_string(), "/x".to_string()),
        ];
        assert_eq!(mode, ConfigSetMode::Expect(expects));

        let plain = parse(&["memory.backend=axel"]).unwrap();
        assert_eq!(request(&plain).unwrap().1, ConfigSetMode::Overwrite);

        assert!(parse(&["--if-unset", "--expect", "a=", "memory.backend=axel"]).is_err());
        assert!(parse(&["--json"]).is_err(), "at least one KEY=VALUE");
        let bad_expect = parse(&["--expect", "memory.backend", "memory.backend=axel"]).unwrap();
        assert!(matches!(request(&bad_expect), Err(ConfigSetError::Invalid(_))));
    }

    #[test]
    fn json_report_matches_the_schema() {
        let written = outcome(
            ConfigSetResult::Written,
            &[("memory.backend", Written), ("memory.axel.executable", Unchanged)],
        );
        let expected = concat!(
            r#"{"schema":"synaps.config-set/1","result":"written","#,
            r#""path":"/home/u/.synaps-cli/config","#,
            r#""keys":{"memory.backend":"written","memory.axel.executable":"unchanged"},"#,
            r#""applies":"next daemon start"}"#,
            "\n",
        );
        let report = report(true, &Ok(written));
        assert_eq!(report, Report::on_stdout(EXIT_OK, expected.to_string()));
    }

    #[test]
    fn json_errors_are_one_line_with_usage_or_io() {
        let invalid = ConfigSetError::Invalid("bad \"value\"\nfor memory.backend".into());
        let usage = report(true, &Err(invalid));
        let expected = concat!(
            r#"{"schema":"synaps.config-set/1","result":"error","error":"usage","#,
            r#""message":"bad \"value\"\nfor memory.backend"}"#,
            "\n",
        );
        assert_eq!(usage, Report::on_stdout(EXIT_USAGE, expected.to_string()));

        let denied = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let io = report(true, &Err(ConfigSetError::Io(denied)));
        let expected = concat!(
            r#"{"schema":"synaps.config-set/1","result":"error","error":"io","#,
            r#""message":"config file: denied"}"#,
            "\n",
        );
        assert_eq!(io, Report::on_stdout(EXIT_IO, expected.to_string()));
    }

    #[test]
    fn exit_codes_map_results_and_errors() {
        let cases = [
            (ConfigSetResult::Written, Written, EXIT_OK),
            (ConfigSetResult::Unchanged, Unchanged, EXIT_OK),
            (ConfigSetResult::Refused, Conflict, EXIT_REFUSED),
            (ConfigSetResult::Refused, Empty, EXIT_REFUSED),
            (ConfigSetResult::Refused, Duplicate, EXIT_REFUSED),
            (ConfigSetResult::Refused, ExpectFailed, EXIT_REFUSED),
            (ConfigSetResult::Refused, PathMismatch, EXIT_REFUSED),
            (ConfigSetResult::Refused, NotSettable, EXIT_REFUSED),
        ];
        for (result, status, code) in cases {
            let given = outcome(result, &[("memory.backend", status)]);
            for json in [true, false] {
                assert_eq!(report(json, &Ok(given.clone())).code, code, "{status:?}");
            }
            // A key that is not settable is still a refusal in the JSON.
            let line = report(true, &Ok(given)).stdout;
            let value: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(value["result"], result.as_str());
            assert_eq!(value["keys"]["memory.backend"], status.as_str());
        }
        let invalid = ConfigSetError::Invalid("x".into());
        assert_eq!(report(false, &Err(invalid)).code, EXIT_USAGE);
        let io = ConfigSetError::Io(std::io::Error::other("x"));
        assert_eq!(report(false, &Err(io)).code, EXIT_IO);
    }

    #[test]
    fn human_report_goes_to_stderr_when_refused() {
        let written = outcome(ConfigSetResult::Written, &[("memory.backend", Written)]);
        let text = "memory.backend: written\n\
                    wrote /home/u/.synaps-cli/config (applies on next daemon start)\n";
        assert_eq!(report(false, &Ok(written)), Report::on_stdout(EXIT_OK, text.into()));

        let refused = outcome(
            ConfigSetResult::Refused,
            &[("memory.backend", Conflict), ("memory.axel.executable", Unchanged)],
        );
        let text = "memory.backend: conflict\nmemory.axel.executable: unchanged\n\
                    refused: nothing written to /home/u/.synaps-cli/config\n";
        assert_eq!(report(false, &Ok(refused)), Report::on_stderr(EXIT_REFUSED, text.into()));

        let refused = outcome(
            ConfigSetResult::Refused,
            &[("skills.extra_root", Empty), ("memory.backend", Duplicate)],
        );
        let text = "skills.extra_root: empty\nmemory.backend: duplicate\n\
                    refused: nothing written to /home/u/.synaps-cli/config\n";
        assert_eq!(report(false, &Ok(refused)), Report::on_stderr(EXIT_REFUSED, text.into()));

        let not_settable = outcome(ConfigSetResult::Refused, &[("model", NotSettable)]);
        let stderr = report(false, &Ok(not_settable)).stderr;
        assert!(stderr.contains("settable keys: memory.backend, memory.axel.executable"));
        assert!(stderr.contains(
            "--if-unset only: context_management.mode, skills.extra_root, \
             auth.account.<provider>=auto\n"
        ));

        let invalid = ConfigSetError::Invalid("empty value for memory.backend".into());
        let text = "synaps config set: empty value for memory.backend\n";
        assert_eq!(report(false, &Err(invalid)), Report::on_stderr(EXIT_USAGE, text.into()));
    }
}
