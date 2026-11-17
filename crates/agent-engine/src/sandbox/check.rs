//! `synaps sandbox check`: what a session started here would be confined to,
//! without starting one.
//!
//! The report is read-only. It reads the config, asks the kernel which
//! Landlock ABI it has, loads the configured policy provider (and only that
//! plugin), asks it the same question a session start asks (with
//! `session_id = "check"` unless told otherwise) and prints the outcome.
//! Nothing is written.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_core::config::{SandboxConfine, SandboxSettings};
use serde_json::{json, Value};

use super::confinement::{Confinement, Plan, SplitImpossible};
use super::policy::{Dropped, Protected, Resolution};
use super::provider;
use crate::extensions::hooks::HookBus;
use crate::extensions::manager::ExtensionManager;

/// The one line `check` prints for the read list: v1 confines writes only.
pub const READ_NOT_ENFORCED: &str = "not enforced (v1)";

/// Why a path inside a write grant is not granted.
pub const LEFT_OUT: &str = "left out of its write grant";

/// Why a directory on a split path is not granted itself.
pub const NO_NEW_ENTRIES: &str =
    "on a split path: a shell command can't create new entries directly in it (the write tools can)";

/// The plain report's one-line statement of the split's cost.
pub const SPLIT_NOTE: &str =
    "a directory on a split path can't get new entries created directly in it by a shell command; its existing entries stay writable";

/// The outcome of one check.
#[derive(Debug, Clone)]
pub struct CheckReport {
    pub confine: SandboxConfine,
    pub resolution: Resolution,
    /// Directories on a split path (see [`NO_NEW_ENTRIES`]).
    pub split_dirs: Vec<PathBuf>,
    /// Why the provider plugin could not be loaded, when it could not.
    pub load_error: Option<String>,
}

impl CheckReport {
    /// Whether a session started now would be confined (or confinement is
    /// off): `false` means it would be refused.
    pub fn usable(&self) -> bool {
        self.resolution.refusal.is_none()
    }

    /// The process exit code: 0 when usable or off, 1 when refusing.
    pub fn exit_code(&self) -> i32 {
        if self.usable() {
            0
        } else {
            1
        }
    }

    fn confine_word(&self) -> &'static str {
        match self.confine {
            SandboxConfine::Off => "off",
            SandboxConfine::Enforce => "enforce",
        }
    }

    /// The single JSON object `--json` prints: exactly these keys.
    pub fn to_json(&self) -> Value {
        let r = &self.resolution;
        let paths = |list: &[PathBuf]| -> Vec<String> { list.iter().map(|p| p.to_string_lossy().into_owned()).collect() };
        let dropped: Vec<Value> = r
            .dropped
            .iter()
            .map(|d: &Dropped| json!({ "path": d.path.to_string_lossy(), "reason": d.reason }))
            .collect();
        json!({
            "confine": self.confine_word(),
            "abi": r.abi,
            "provider": r.provider,
            "provider_state": r.provider_state.word(),
            "refusal": r.refusal.as_ref().map(|f| f.cause.word()),
            "write": paths(&r.write),
            "read": paths(&r.read),
            "dropped": dropped,
        })
    }

    /// The `key: value` lines of the plain report.
    pub fn lines(&self) -> Vec<String> {
        let r = &self.resolution;
        let mut out = vec![
            format!("confine: {}", self.confine_word()),
            format!("abi: {}", r.abi.map(|a| a.to_string()).unwrap_or_else(|| "none".to_string())),
            format!("provider: {}", r.provider.as_deref().unwrap_or("none")),
            format!("provider_state: {}", r.provider_state.word()),
            format!("refusal: {}", r.refusal.as_ref().map(|f| f.check_text()).unwrap_or_else(|| "none".to_string())),
        ];
        if let Some(refusal) = &r.refusal {
            out.push(format!("session_cause: {}", refusal.session_cause));
        }
        if let Some(why) = &self.load_error {
            out.push(format!("provider_error: {why}"));
        }
        if r.write.is_empty() {
            out.push("write: none".to_string());
        }
        for path in &r.write {
            out.push(format!("write: {}", path.display()));
        }
        out.push(format!("read: {READ_NOT_ENFORCED}"));
        for path in &r.read {
            out.push(format!("read_listed: {}", path.display()));
        }
        if r.dropped.is_empty() {
            out.push("dropped: none".to_string());
        }
        for d in &r.dropped {
            out.push(format!("dropped: {} ({})", d.path.display(), d.reason));
        }
        if !self.split_dirs.is_empty() {
            out.push(format!("note: {SPLIT_NOTE}"));
        }
        out
    }
}

/// Run the check. `workspace` is the directory a session would be confined
/// around (the caller's choice, else the process's working directory).
pub async fn run_check(settings: &SandboxSettings, session_id: &str, workspace: &Path) -> CheckReport {
    let bus = Arc::new(HookBus::new());
    let mut manager = ExtensionManager::new(Arc::clone(&bus));
    let mut load_error = None;
    if settings.confine == SandboxConfine::Enforce {
        if let Some(name) = settings.policy_provider.as_deref() {
            if let Err(why) = manager.load_named(name).await {
                load_error = Some(why);
            }
        }
    }
    let (mut resolution, env) = provider::evaluate(&bus, settings, session_id, None, Some(workspace)).await;
    manager.shutdown_all().await;
    let mut split_dirs = Vec::new();
    if resolution.confine_on && resolution.refusal.is_none() {
        let confinement = Confinement::new(resolution.clone(), Protected::new(&env), workspace, Some(workspace));
        split_dirs = apply_plan(&mut resolution, confinement.plan());
    }
    CheckReport { confine: settings.confine, resolution, split_dirs, load_error }
}

/// Fold what the grants turn into into the report: each inner path left out
/// and each directory that can't get new entries is listed as dropped; a root
/// that can't be split is a refusal. Returns the split directories.
pub fn apply_plan(resolution: &mut Resolution, plan: Result<Plan, SplitImpossible>) -> Vec<PathBuf> {
    match plan {
        Ok(plan) => {
            for hole in &plan.left_out {
                resolution.dropped.push(Dropped { path: hole.path.clone(), reason: format!("{LEFT_OUT}: {}", hole.why) });
            }
            for dir in &plan.no_new_entries {
                resolution.dropped.push(Dropped { path: dir.clone(), reason: NO_NEW_ENTRIES.to_string() });
            }
            plan.no_new_entries
        }
        Err(split) => {
            resolution.dropped.push(Dropped {
                path: split.path.clone(),
                reason: "inside a write grant that can't be split around it".to_string(),
            });
            resolution.refusal = Some(split.refusal());
            resolution.write.clear();
            resolution.read.clear();
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::policy::{resolve, Answer, PolicyEnv, ProviderOutcome, ResolveInput};

    fn env() -> PolicyEnv {
        PolicyEnv::fixed(Path::new("/home/u"), Path::new("/home/u/.synaps-cli"))
    }

    fn report(confine_on: bool, outcome: ProviderOutcome, abi: Result<u32, ()>) -> CheckReport {
        let env = env();
        let resolution = resolve(ResolveInput {
            confine_on,
            provider: Some("guard"),
            abi,
            outcome,
            workspace: Path::new("/work/app"),
            env: &env,
        });
        CheckReport {
            confine: if confine_on { SandboxConfine::Enforce } else { SandboxConfine::Off },
            resolution,
            split_dirs: Vec::new(),
            load_error: None,
        }
    }

    #[test]
    fn the_json_has_exactly_the_frozen_keys() {
        let ok = report(
            true,
            ProviderOutcome::Answer(Answer {
                write: vec!["/data/cache".into(), "/home/u".into()],
                read: vec!["/etc".into()],
            }),
            Ok(7),
        );
        let json = ok.to_json();
        let mut keys: Vec<&str> = json.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["abi", "confine", "dropped", "provider", "provider_state", "read", "refusal", "write"]
        );
        assert_eq!(json["confine"], "enforce");
        assert_eq!(json["abi"], 7);
        assert_eq!(json["provider"], "guard");
        assert_eq!(json["provider_state"], "loaded");
        assert!(json["refusal"].is_null());
        assert_eq!(json["read"], json!(["/etc"]));
        let write: Vec<&str> = json["write"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert!(write.contains(&"/work/app") && write.contains(&"/data/cache"));
        assert!(!write.contains(&"/home/u"));
        let dropped = json["dropped"].as_array().unwrap();
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0]["path"], "/home/u");
        assert!(dropped[0]["reason"].as_str().unwrap().contains("home"));
        let mut dropped_keys: Vec<&str> = dropped[0].as_object().unwrap().keys().map(String::as_str).collect();
        dropped_keys.sort_unstable();
        assert_eq!(dropped_keys, ["path", "reason"]);
        assert!(ok.usable());
        assert_eq!(ok.exit_code(), 0);
    }

    #[test]
    fn a_refusal_exits_one_and_names_a_cause_word() {
        let cases = [
            (ProviderOutcome::Missing, Ok(7), "missing", "provider-missing"),
            (ProviderOutcome::Timeout, Ok(7), "timeout", "provider-timeout"),
            (ProviderOutcome::Error, Ok(7), "error", "provider-error"),
            (ProviderOutcome::BadAnswer, Ok(7), "bad-answer", "provider-bad-answer"),
            (ProviderOutcome::Refused("no".into()), Ok(7), "refused", "provider-refused"),
            (ProviderOutcome::NotConfigured, Ok(7), "missing", "no-policy"),
            (ProviderOutcome::Answer(Answer::default()), Err(()), "loaded", "abi-too-old"),
        ];
        for (outcome, abi, state, cause) in cases {
            let r = report(true, outcome, abi);
            let json = r.to_json();
            assert_eq!(json["provider_state"], state, "{cause}");
            assert_eq!(json["refusal"], cause);
            assert_eq!(json["write"], json!([]), "a refused session resolves nothing ({cause})");
            assert_eq!(r.exit_code(), 1, "{cause}");
            assert!(!r.usable());
        }
        assert!(report(true, ProviderOutcome::Missing, Err(())).to_json()["abi"].is_null());
    }

    #[test]
    fn off_is_usable_and_empty() {
        let r = report(false, ProviderOutcome::NotConfigured, Ok(7));
        let json = r.to_json();
        assert_eq!(json["confine"], "off");
        assert_eq!(json["provider_state"], "off");
        assert!(json["refusal"].is_null());
        assert_eq!(json["write"], json!([]));
        assert_eq!(r.exit_code(), 0);
    }

    #[test]
    fn the_plain_report_says_reads_are_not_enforced() {
        let r = report(
            true,
            ProviderOutcome::Answer(Answer { write: vec![], read: vec!["/etc".into()] }),
            Ok(3),
        );
        let lines = r.lines();
        assert!(lines.contains(&"read: not enforced (v1)".to_string()), "{lines:?}");
        assert!(lines.contains(&"write: /work/app".to_string()));
        assert!(lines.iter().all(|l| l.contains(": ")), "{lines:?}");
    }

    #[test]
    fn the_state_words_are_the_frozen_set() {
        use crate::sandbox::policy::ProviderState as S;
        let all = [S::Off, S::Loaded, S::Missing, S::Timeout, S::Error, S::BadAnswer, S::Refused];
        let words: Vec<&str> = all.iter().map(|s| s.word()).collect();
        assert_eq!(words, ["off", "loaded", "missing", "timeout", "error", "bad-answer", "refused"]);
    }

    /// The plain report names every inner path a grant leaves out, every
    /// directory a split makes unable to take new entries, and says what
    /// that costs; the JSON carries the same paths in `dropped`.
    #[test]
    fn the_report_lists_each_inner_path_left_out_and_the_cost_of_the_split() {
        let t = crate::sandbox::testkit::Tree::new();
        for d in ["bin", "src", "node_modules/.bin"] {
            std::fs::create_dir_all(t.ws.join(d)).unwrap();
        }
        let env = t.env(vec![t.ws.join("bin"), t.ws.join("node_modules/.bin")]);
        let mut resolution = resolve(ResolveInput {
            confine_on: true,
            provider: Some("guard"),
            abi: Ok(7),
            outcome: ProviderOutcome::Answer(Answer::default()),
            workspace: &t.ws,
            env: &env,
        });
        let confinement = Confinement::new(resolution.clone(), Protected::new(&env), &t.ws, Some(&t.ws));
        let split_dirs = apply_plan(&mut resolution, confinement.plan());
        assert!(split_dirs.contains(&t.ws) && split_dirs.contains(&t.ws.join("node_modules")));
        let report = CheckReport { confine: SandboxConfine::Enforce, resolution, split_dirs, load_error: None };
        assert_eq!(report.exit_code(), 0, "a split is a usable policy");

        let lines = report.lines();
        for inner in [t.ws.join("bin"), t.ws.join("node_modules/.bin"), t.ws.join(".synaps/plugins")] {
            let prefix = format!("dropped: {} (", inner.display());
            assert!(
                lines.iter().any(|l| l.starts_with(&prefix) && l.contains(LEFT_OUT)),
                "{} must be listed as left out: {lines:?}",
                inner.display()
            );
        }
        let split_prefix = format!("dropped: {} (", t.ws.display());
        assert!(
            lines.iter().any(|l| l.starts_with(&split_prefix) && l.contains("can't create new entries")),
            "the split directory is listed: {lines:?}"
        );
        assert!(lines.contains(&format!("note: {SPLIT_NOTE}")), "{lines:?}");
        assert!(SPLIT_NOTE.contains("can't get new entries created directly in it"));

        let json = report.to_json();
        let listed: Vec<&str> = json["dropped"].as_array().unwrap().iter().map(|d| d["path"].as_str().unwrap()).collect();
        for inner in [t.ws.join("bin"), t.ws.join("node_modules/.bin"), t.ws.join(".synaps/plugins")] {
            assert!(listed.contains(&inner.to_str().unwrap()), "{} in dropped: {listed:?}", inner.display());
        }
        assert!(json["refusal"].is_null());
    }

    /// A grant that can't be split around a protected path inside it refuses
    /// the session, naming the path.
    #[test]
    fn a_grant_that_cannot_be_split_refuses_in_the_report() {
        let mut resolution = report(true, ProviderOutcome::Answer(Answer::default()), Ok(7)).resolution;
        assert!(!resolution.write.is_empty());
        let path = PathBuf::from("/work/app/bin");
        let folded = apply_plan(&mut resolution, Err(SplitImpossible { path: path.clone() }));
        assert!(folded.is_empty());
        let report = CheckReport {
            confine: SandboxConfine::Enforce,
            resolution,
            split_dirs: Vec::new(),
            load_error: None,
        };
        assert_eq!(report.exit_code(), 1);
        assert!(report.to_json()["write"].as_array().unwrap().is_empty(), "a refused session resolves nothing");
        let lines = report.lines();
        assert!(lines.contains(&"refusal: protected-inside-grant: /work/app/bin".to_string()), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("dropped: /work/app/bin (")), "{lines:?}");
    }

    #[tokio::test]
    async fn a_check_with_confinement_off_asks_nobody() {
        let settings = SandboxSettings::default();
        let r = run_check(&settings, "check", Path::new("/tmp")).await;
        assert_eq!(r.exit_code(), 0);
        assert_eq!(r.to_json()["provider_state"], "off");
    }

    #[tokio::test]
    async fn a_missing_provider_is_a_refusal() {
        let settings = SandboxSettings { confine: SandboxConfine::Enforce, policy_provider: Some("no-such-plugin-here".into()) };
        let r = run_check(&settings, "check", Path::new("/tmp")).await;
        assert_eq!(r.exit_code(), 1);
        assert!(r.load_error.is_some());
        let cause = r.to_json()["refusal"].as_str().unwrap().to_string();
        assert!(cause == "provider-missing" || cause == "abi-too-old", "{cause}");
    }
}
