//! `synaps skills status|reload [--json]`: the running daemon's skills
//! (skills v1). `status` reports the daemon's last discovery pass (boot or
//! reload) without walking the disk; `reload` walks the skill roots again
//! with the skill keys read fresh from the config file and swaps the new
//! set in. Extensions, hooks and MCP still need `synaps daemon reload`.
//!
//! It asks the running daemon on a control connection and never starts one.
//! Exit codes: 0 report, 1 no daemon or an I/O error (or no answer in
//! time), 3 refused (the daemon answered an error, or predates skills v1).

use std::path::Path;
use std::time::Duration;

use agent_engine::session::socket_transport::{QueryError, SkillsReply, SocketTransport};
use agent_engine::skills::status::SkillsReport;

const EXIT_OK: i32 = 0;
const EXIT_IO: i32 = 1;
const EXIT_REFUSED: i32 = 3;

/// The whole query's deadline (connect, handshake and reply). A reload
/// waits for any reload before it, then walks the roots.
const STATUS_WAIT: Duration = Duration::from_secs(10);
const RELOAD_WAIT: Duration = Duration::from_secs(60);

#[derive(clap::Subcommand, Debug)]
pub enum SkillsAction {
    /// The daemon's last skill discovery pass: roots (configured and
    /// resolved paths), counts, caps, shadowed skills.
    ///
    /// Exit codes: 0 report, 1 no daemon or I/O error, 3 refused.
    Status {
        /// Print the report as one JSON object (on failure:
        /// {"error": reason, "message": text}).
        #[arg(long)]
        json: bool,
    },
    /// Walk the skill roots again with skills.extra_root, disabled_plugins
    /// and disabled_skills read fresh from the config file, and swap the
    /// new set in. Prints the new pass's report.
    ///
    /// Exit codes: 0 report, 1 no daemon or I/O error, 3 refused.
    Reload {
        /// Print the report as one JSON object (on failure:
        /// {"error": reason, "message": text}).
        #[arg(long)]
        json: bool,
    },
}

/// Runs the action against `profile`'s daemon; returns the exit code.
pub async fn run(profile: Option<String>, action: SkillsAction) -> i32 {
    let (reload, json, wait) = match action {
        SkillsAction::Status { json } => (false, json, STATUS_WAIT),
        SkillsAction::Reload { json } => (true, json, RELOAD_WAIT),
    };
    let paths = agent_engine::daemon::registry::daemon_paths(profile.as_deref());
    let sock = agent_engine::daemon::registry::is_alive(&paths).then(|| paths.sock.clone());
    let outcome = query(sock.as_deref(), reload, wait).await;
    emit(json, &outcome)
}

/// What the query came to.
#[derive(Debug)]
enum Outcome {
    Report(SkillsReport),
    /// (reason, message, exit code).
    Failed(&'static str, String, i32),
}

async fn query(sock: Option<&Path>, reload: bool, wait: Duration) -> Outcome {
    let Some(sock) = sock else {
        return Outcome::Failed("no_daemon", "no daemon is running".into(), EXIT_IO);
    };
    match SocketTransport::skills(sock, reload, tokio::time::Instant::now() + wait).await {
        Ok(SkillsReply::Report(report)) => Outcome::Report(report),
        Ok(SkillsReply::Refused(message)) => Outcome::Failed("refused", message, EXIT_REFUSED),
        Ok(SkillsReply::DaemonPredates {
            daemon_version,
            daemon_build,
        }) => {
            let build = if daemon_build.is_empty() {
                String::new()
            } else {
                format!(", build {daemon_build}")
            };
            Outcome::Failed(
                "daemon_predates",
                format!("the daemon predates `synaps skills` (daemon {daemon_version}{build}): restart or reload it"),
                EXIT_REFUSED,
            )
        }
        Err(QueryError::Transport(e)) => {
            Outcome::Failed("io", format!("daemon at {}: {e}", sock.display()), EXIT_IO)
        }
        Err(QueryError::TimedOut) => Outcome::Failed(
            "timeout",
            format!("the daemon did not answer within {} s", wait.as_secs()),
            EXIT_IO,
        ),
    }
}

fn emit(json: bool, outcome: &Outcome) -> i32 {
    match outcome {
        Outcome::Report(report) => {
            if json {
                println!("{}", serde_json::to_string(report).unwrap_or_default());
            } else {
                print!("{}", text(report));
            }
            EXIT_OK
        }
        Outcome::Failed(reason, message, code) => {
            eprintln!("synaps skills: {message}");
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "error": reason, "message": message })
                );
            }
            *code
        }
    }
}

/// One line per root, then the totals.
fn text(report: &SkillsReport) -> String {
    let mut out = format!(
        "generation {} (at {} ms)\n",
        report.generation, report.at_unix_ms
    );
    for root in &report.roots {
        let kind = serde_json::to_value(root.kind)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        out.push_str(&format!(
            "{kind}\t{}\t-> {}\tplugins {}, skills {}\n",
            root.configured.as_deref().unwrap_or("(not set)"),
            root.resolved.as_deref().unwrap_or("(absent)"),
            root.plugins,
            root.skills,
        ));
    }
    out.push_str(&format!(
        "skills {}/{}, metadata {}/{} bytes, skipped {}, disabled {}, shadowed {}\n",
        report.skills_total,
        report.skills_cap,
        report.metadata_bytes,
        report.metadata_cap,
        report.skipped,
        report.disabled,
        report.shadowed_total,
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_daemon_is_an_io_failure() {
        let outcome = query(None, true, STATUS_WAIT).await;
        assert!(
            matches!(outcome, Outcome::Failed("no_daemon", _, EXIT_IO)),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_dead_socket_is_an_io_failure() {
        let dir = tempfile::TempDir::new().unwrap();
        let outcome = query(Some(&dir.path().join("none.sock")), false, STATUS_WAIT).await;
        assert!(
            matches!(outcome, Outcome::Failed("io", _, EXIT_IO)),
            "{outcome:?}"
        );
    }
}
