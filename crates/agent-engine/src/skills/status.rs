//! `skills.status` and `skills.reload` (skills v1): the report of the last
//! skill discovery pass, and the reload that walks again and replaces it.
//!
//! The boot pass is generation 1 and each reload adds one. A reload holds
//! one lock from its config read to its publish, so an older walk can never
//! publish after a newer one. `status` reads the last report only: it never
//! walks the disk and never waits for a reload.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::loader::{self, RootKind, DISCOVERY_MAX_METADATA_BYTES, DISCOVERY_MAX_SKILLS};
use super::registry::CommandRegistry;
use super::Discovered;

/// What one discovery pass (boot or `skills.reload`) loaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillsReport {
    /// 1 for the boot pass; each reload adds one.
    pub generation: u64,
    /// When the pass was published (Unix time, ms).
    pub at_unix_ms: u64,
    /// The five roots, in walk order (first wins).
    pub roots: Vec<SkillsRootReport>,
    /// Skills the walk kept within the caps, before the disable filters
    /// (the sum of the roots' `skills`). The registry holds
    /// `skills_total - disabled`.
    pub skills_total: usize,
    pub skills_cap: usize,
    /// Retained metadata bytes, counted against `metadata_cap`.
    pub metadata_bytes: usize,
    pub metadata_cap: usize,
    /// Skills skipped at a cap.
    pub skipped: usize,
    /// Skills the disable lists removed.
    pub disabled: usize,
    /// The first [`loader::SHADOWED_REPORT_MAX`] shadowed skills, in walk
    /// order.
    pub shadowed: Vec<ShadowedReport>,
    pub shadowed_total: usize,
    /// Non-skill parts of `skills.extra_root` plugins that were ignored
    /// (that root gives skills only), one per (plugin, part).
    #[serde(default)]
    pub ignored_non_skill: usize,
    /// The first [`loader::IGNORED_NON_SKILL_REPORT_MAX`] of them, as
    /// `<plugin>:<part>`; parts are `commands`, `keybinds`, `extension`,
    /// `hooks`, `sidecar`, `help`, `settings` and `mcp`.
    #[serde(default)]
    pub ignored_non_skill_names: Vec<String>,
}

/// One root of a [`SkillsReport`]. Paths are UTF-8 (lossy).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillsRootReport {
    pub kind: RootKind,
    /// The configured path, made absolute. `None`: `skills.extra_root` is
    /// not set.
    pub configured: Option<String>,
    /// The path the walk read (realpath, once per pass). `None`: absent or
    /// not a directory. A versioned store's version is this path.
    pub resolved: Option<String>,
    /// Plugins and skills this root added, before the disable filters.
    pub plugins: usize,
    pub skills: usize,
}

/// A skill dropped because an earlier root (or an earlier place in the
/// same root) already had its (plugin, name).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowedReport {
    pub plugin: Option<String>,
    pub name: String,
    pub kept_root_kind: RootKind,
    pub dropped_root_kind: RootKind,
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl SkillsReport {
    /// The report of `found`, published now as `generation`.
    pub fn of(generation: u64, found: &Discovered) -> Self {
        let pass = &found.pass;
        let roots = pass
            .roots
            .iter()
            .map(|root| SkillsRootReport {
                kind: root.kind,
                configured: root.configured.as_deref().map(path_text),
                resolved: root.resolved.as_deref().map(path_text),
                plugins: root.plugins,
                skills: root.skills,
            })
            .collect::<Vec<_>>();
        let shadowed = pass
            .shadowed
            .iter()
            .map(|s| ShadowedReport {
                plugin: s.plugin.clone(),
                name: s.name.clone(),
                kept_root_kind: s.kept,
                dropped_root_kind: s.dropped,
            })
            .collect();
        Self {
            generation,
            at_unix_ms: now_unix_ms(),
            skills_total: roots.iter().map(|r| r.skills).sum(),
            roots,
            skills_cap: DISCOVERY_MAX_SKILLS,
            metadata_bytes: pass.metadata_bytes,
            metadata_cap: DISCOVERY_MAX_METADATA_BYTES,
            skipped: pass.skipped,
            disabled: found.disabled,
            shadowed,
            shadowed_total: pass.shadowed_total,
            ignored_non_skill: pass.ignored_non_skill,
            ignored_non_skill_names: pass.ignored_non_skill_names.clone(),
        }
    }
}

/// The last pass's report, and the lock that orders reloads.
pub struct SkillsState {
    /// Held across a whole reload: config read, walk, filter and publish.
    reload: tokio::sync::Mutex<()>,
    last: std::sync::Mutex<SkillsReport>,
}

impl SkillsState {
    /// The state after the boot pass `boot` (generation 1).
    pub fn new(boot: SkillsReport) -> Self {
        Self {
            reload: tokio::sync::Mutex::new(()),
            last: std::sync::Mutex::new(boot),
        }
    }

    /// `skills.status`: the last published pass.
    pub fn status(&self) -> SkillsReport {
        self.last.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// `skills.reload`: read the skill keys fresh from the config file (the
    /// caller's config snapshot is untouched), walk the roots on a blocking
    /// thread, filter, swap the new set into `registry` and publish its
    /// report. Extensions, hooks, MCP and keybinds are not touched.
    pub async fn reload(&self, registry: &CommandRegistry) -> Result<SkillsReport, String> {
        self.reload_with(registry, || {
            super::discover(&agent_core::config::read_skills_keys())
        })
        .await
    }

    /// [`Self::reload`] with `pass` as the read and walk.
    async fn reload_with(
        &self,
        registry: &CommandRegistry,
        pass: impl FnOnce() -> Discovered + Send + 'static,
    ) -> Result<SkillsReport, String> {
        let _one = self.reload.lock().await;
        let found = tokio::task::spawn_blocking(pass)
            .await
            .map_err(|e| format!("skills reload failed: {e}"))?;
        let report = SkillsReport::of(self.status().generation + 1, &found);
        let loader::DiscoveryPass {
            plugins, skills, ..
        } = found.pass;
        registry.rebuild_with_plugins(skills, plugins);
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = report.clone();
        tracing::info!(
            generation = report.generation,
            skills = report.skills_total - report.disabled,
            "reloaded skills"
        );
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::loader::DiscoveryRoot;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn write_skill(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir.join(name)).unwrap();
        std::fs::write(
            dir.join(name).join("SKILL.md"),
            format!("---\nname: {name}\ndescription: d\n---\n{body}"),
        )
        .unwrap();
    }

    /// One pass over `dir` as the extra root, with no disable lists.
    fn pass_of(dir: &Path) -> Discovered {
        let roots = [DiscoveryRoot {
            kind: RootKind::Extra,
            path: Some(dir.to_path_buf()),
        }];
        Discovered {
            pass: loader::load_all_reported(&roots),
            disabled: 0,
        }
    }

    fn names(registry: &CommandRegistry) -> Vec<String> {
        let mut names: Vec<String> = registry
            .all_skills()
            .iter()
            .map(|s| s.name.clone())
            .collect();
        names.sort();
        names
    }

    /// Pin 4 (skills v1): `skills.reload` reads the skill keys from the
    /// config file now, so a `skills.extra_root` written after boot is
    /// walked.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn reload_walks_a_root_key_written_after_boot() {
        let base = crate::test_env::BaseDirGuard::new();
        std::fs::write(base.path().join("config"), "# boot\n").unwrap();
        let boot = super::super::discover(&agent_core::config::read_skills_keys());
        assert_eq!(boot.pass.roots[2].configured, None);
        let registry = CommandRegistry::new(&[], boot.pass.skills.clone());
        let state = SkillsState::new(SkillsReport::of(1, &boot));
        assert!(!names(&registry).contains(&"late".to_string()));

        let extra = tempfile::TempDir::new().unwrap();
        write_skill(extra.path(), "late", "Body");
        std::fs::write(
            base.path().join("config"),
            format!("# boot\nskills.extra_root = {}\n", extra.path().display()),
        )
        .unwrap();
        let report = state.reload(&registry).await.unwrap();
        assert_eq!(report.generation, 2);
        let root = &report.roots[2];
        assert_eq!(root.kind, RootKind::Extra);
        assert_eq!(root.configured, Some(path_text(extra.path())));
        assert_eq!(
            root.resolved,
            Some(path_text(&extra.path().canonicalize().unwrap()))
        );
        assert_eq!((root.plugins, root.skills), (0, 1));
        assert!(names(&registry).contains(&"late".to_string()));
        assert_eq!(state.status(), report, "status reports the last pass");
    }

    /// Pin 5 (skills v1): a reload that starts while another runs waits for
    /// its whole step, so the two publish in order and the newer walk's set
    /// is the one left in the registry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_reloads_publish_in_order() {
        let old = tempfile::TempDir::new().unwrap();
        let new = tempfile::TempDir::new().unwrap();
        write_skill(old.path(), "old", "Body");
        write_skill(new.path(), "new", "Body");
        let empty = tempfile::TempDir::new().unwrap();
        let boot = pass_of(empty.path());
        let registry = Arc::new(CommandRegistry::new(&[], vec![]));
        let state = Arc::new(SkillsState::new(SkillsReport::of(1, &boot)));

        // A walks the old set and is held mid-walk.
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let old_dir: PathBuf = old.path().to_path_buf();
        let a = tokio::spawn({
            let (state, registry) = (state.clone(), registry.clone());
            async move {
                state
                    .reload_with(&registry, move || {
                        entered_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        pass_of(&old_dir)
                    })
                    .await
            }
        });
        tokio::task::spawn_blocking(move || entered_rx.recv().unwrap())
            .await
            .unwrap();

        // B starts after A and walks the new set.
        let b_walked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let new_dir: PathBuf = new.path().to_path_buf();
        let b = tokio::spawn({
            let (state, registry, b_walked) = (state.clone(), registry.clone(), b_walked.clone());
            async move {
                state
                    .reload_with(&registry, move || {
                        b_walked.store(true, std::sync::atomic::Ordering::SeqCst);
                        pass_of(&new_dir)
                    })
                    .await
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !b_walked.load(std::sync::atomic::Ordering::SeqCst),
            "B waits for A's whole step"
        );
        assert_eq!(state.status().generation, 1, "nothing published yet");

        release_tx.send(()).unwrap();
        let a = a.await.unwrap().unwrap();
        let b = b.await.unwrap().unwrap();
        assert_eq!((a.generation, b.generation), (2, 3));
        assert_eq!(state.status(), b);
        assert_eq!(
            names(&registry),
            ["new"],
            "the newer walk is published last"
        );
    }

    /// Pin 7 (skills v1): after the store swaps `current` and deletes the
    /// version the daemon loaded (GC), loading one of its skills fails
    /// closed with "reload skills and retry", and a reload recovers.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_load_after_the_version_is_deleted_asks_to_reload_and_retry() {
        let store = tempfile::TempDir::new().unwrap();
        let (v1, v2) = (store.path().join("v1"), store.path().join("v2"));
        write_skill(&v1, "pick", "first version");
        write_skill(&v2, "pick", "second version");
        let current = store.path().join("current");
        std::os::unix::fs::symlink(&v1, &current).unwrap();
        let boot = pass_of(&current);
        let registry = CommandRegistry::new(&[], boot.pass.skills.clone());
        let state = SkillsState::new(SkillsReport::of(1, &boot));

        std::fs::remove_file(&current).unwrap();
        std::os::unix::fs::symlink(&v2, &current).unwrap();
        std::fs::remove_dir_all(&v1).unwrap();
        let skill = registry.all_skills().pop().unwrap();
        let err = crate::skills::tool::LoadSkillTool::format_body(&skill).unwrap_err();
        assert!(err.contains("reload skills and retry"), "{err}");

        let report = state
            .reload_with(&registry, move || pass_of(&current))
            .await
            .unwrap();
        assert_eq!(
            report.roots[0].resolved,
            Some(path_text(&v2.canonicalize().unwrap()))
        );
        let skill = registry.all_skills().pop().unwrap();
        assert!(skill.load_body().unwrap().contains("second version"));
    }

    /// Pin 8 (skills v1), the count cap: 2050 skills report 2048 kept and
    /// 2 skipped.
    #[test]
    fn report_counts_at_the_skill_cap() {
        let root = tempfile::TempDir::new().unwrap();
        for i in 0..2050 {
            write_skill(root.path(), &format!("s{i:04}"), "b");
        }
        let report = SkillsReport::of(1, &pass_of(root.path()));
        assert_eq!((report.skills_total, report.skills_cap), (2048, 2048));
        assert_eq!(report.skipped, 2);
        assert_eq!(report.roots[0].skills, 2048);
        assert!(report.metadata_bytes <= report.metadata_cap);
        assert_eq!(report.metadata_cap, 4 * 1024 * 1024);
    }

    /// Pin 8 (skills v1), the metadata cap: skills whose retained metadata
    /// passes 4 MiB are skipped and counted before the count cap.
    #[test]
    fn report_counts_at_the_metadata_cap() {
        let root = tempfile::TempDir::new().unwrap();
        // Long paths: each skill retains > 2 KiB (name, description, its
        // directory and SKILL.md path), so 4 MiB holds fewer than 2048.
        let deep = ["a", "b", "c"]
            .iter()
            .fold(root.path().to_path_buf(), |p, c| p.join(c.repeat(200)));
        let description = "d".repeat(1000);
        let written = 1700;
        for i in 0..written {
            let name = format!("{i:04}{}", "n".repeat(120));
            std::fs::create_dir_all(deep.join(&name)).unwrap();
            std::fs::write(
                deep.join(&name).join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {description}\n---\nb"),
            )
            .unwrap();
        }
        let report = SkillsReport::of(1, &pass_of(&deep));
        assert!(report.skills_total < report.skills_cap, "{report:?}");
        assert!(report.skipped > 0);
        assert_eq!(report.skills_total + report.skipped, written);
        assert!(report.metadata_bytes <= report.metadata_cap);
        assert!(report.metadata_bytes > report.metadata_cap - 4096);
    }

    /// Flag 5 (skills v1): only the report is lossy. The walk keeps the real
    /// `OsStr` path (the skill's source and base dir are under the exact
    /// non-UTF-8 version dir), and the report shows the U+FFFD form. The
    /// load never uses a lossy path either: it opens the exact `PathBuf`,
    /// and since the body names its base dir as text (`{baseDir}`), a base
    /// dir that is not UTF-8 is refused (loader.rs `load_body_with_evidence`,
    /// as before skills v1) rather than substituted lossily.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_non_utf8_root_is_walked_exactly_and_only_the_report_is_lossy() {
        use std::os::unix::ffi::OsStrExt;
        let store = tempfile::TempDir::new().unwrap();
        let version = store.path().join(std::ffi::OsStr::from_bytes(b"v-\xff"));
        write_skill(&version, "doctor-check", "raw path body");
        let current = store.path().join("current");
        std::os::unix::fs::symlink(&version, &current).unwrap();
        let found = pass_of(&current);
        let resolved = found.pass.roots[0].resolved.clone().unwrap();
        assert_eq!(resolved, version.canonicalize().unwrap());
        assert!(resolved.to_str().is_none(), "the walk keeps the raw bytes");

        let registry = CommandRegistry::new(&[], found.pass.skills.clone());
        let skill = registry.all_skills().pop().unwrap();
        assert_eq!(skill.name, "doctor-check");
        assert_eq!(skill.base_dir, resolved.join("doctor-check"));
        assert_eq!(
            skill.source_path,
            resolved.join("doctor-check").join("SKILL.md")
        );
        assert!(std::fs::read(&skill.source_path).is_ok());
        let err = crate::skills::tool::LoadSkillTool::format_body(&skill).unwrap_err();
        assert!(err.contains("no longer readable"), "{err}");
        assert!(!err.contains('\u{fffd}'), "{err}");

        let report = SkillsReport::of(1, &found);
        let shown = report.roots[0].resolved.clone().unwrap();
        assert!(shown.ends_with("/v-\u{fffd}"), "{shown}");
        assert_eq!(shown, resolved.to_string_lossy());
    }

    /// The report's JSON is the contract's: field names, snake_case root
    /// kinds, `null` for an unset or absent root, the shadowed skills.
    #[test]
    fn report_json_has_the_contract_fields() {
        let first = tempfile::TempDir::new().unwrap();
        let second = tempfile::TempDir::new().unwrap();
        write_skill(first.path(), "same", "Body");
        write_skill(second.path(), "same", "Body");
        let roots = [
            DiscoveryRoot {
                kind: RootKind::ProjectSkills,
                path: Some(first.path().to_path_buf()),
            },
            DiscoveryRoot {
                kind: RootKind::Extra,
                path: None,
            },
            DiscoveryRoot {
                kind: RootKind::HomeSkills,
                path: Some(second.path().join("absent")),
            },
            DiscoveryRoot {
                kind: RootKind::HomePlugins,
                path: Some(second.path().to_path_buf()),
            },
        ];
        let found = Discovered {
            pass: loader::load_all_reported(&roots),
            disabled: 0,
        };
        let json = serde_json::to_value(SkillsReport::of(4, &found)).unwrap();
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "at_unix_ms",
                "disabled",
                "generation",
                "ignored_non_skill",
                "ignored_non_skill_names",
                "metadata_bytes",
                "metadata_cap",
                "roots",
                "shadowed",
                "shadowed_total",
                "skills_cap",
                "skills_total",
                "skipped",
            ]
        );
        assert_eq!(json["generation"], 4);
        assert_eq!(json["skills_total"], 1);
        assert_eq!(json["metadata_cap"], 4194304);
        assert_eq!(json["roots"][0]["kind"], "project_skills");
        assert_eq!(json["roots"][1]["kind"], "extra");
        assert!(json["roots"][1]["configured"].is_null());
        assert!(json["roots"][1]["resolved"].is_null());
        assert!(json["roots"][2]["resolved"].is_null(), "absent: null");
        assert_eq!(json["roots"][2]["skills"], 0);
        assert_eq!(
            json["shadowed"],
            serde_json::json!([{
                "plugin": null,
                "name": "same",
                "kept_root_kind": "project_skills",
                "dropped_root_kind": "home_plugins",
            }])
        );
        assert_eq!(json["shadowed_total"], 1);
        assert_eq!(json["ignored_non_skill"], 0);
        assert_eq!(json["ignored_non_skill_names"], serde_json::json!([]));
        let back: SkillsReport = serde_json::from_value(json).unwrap();
        assert_eq!(back.generation, 4);
    }
}
