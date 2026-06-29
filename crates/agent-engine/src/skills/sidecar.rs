//! Sidecar metadata for loose skills: `.skill-meta.json`.
//!
//! Carries authorship provenance and timestamps. Forward-compatible:
//! unknown fields are silently ignored on read.
//!
//! Missing sidecar is legal — hand-authored skills don't have one.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;

use crate::RuntimeError;
use crate::skills::writer::skill_dir;

// ---------------------------------------------------------------------------
// Schema types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    #[default]
    User,
    Learn,
    BackgroundReview,
}

impl Provenance {
    /// Parse from optional string; defaults to `BackgroundReview` (used by the
    /// tool boundary where the caller may explicitly intend background-review).
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("user") => Self::User,
            Some("learn") => Self::Learn,
            _ => Self::BackgroundReview,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillMeta {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub provenance: Provenance,
    #[serde(default)]
    pub created: String,     // RFC3339
    #[serde(default)]
    pub last_updated: String, // RFC3339
}

fn default_schema_version() -> u32 { 1 }

impl SkillMeta {
    /// Create a new sidecar for a freshly-created skill.
    pub fn new(provenance: Provenance) -> Self {
        let now = Utc::now().to_rfc3339();
        Self {
            schema_version: 1,
            provenance,
            created: now.clone(),
            last_updated: now,
        }
    }

    /// Path to the sidecar file for a given skill name.
    pub fn path(name: &str) -> std::path::PathBuf {
        skill_dir(name).join(".skill-meta.json")
    }

    /// Write the sidecar atomically: tmp → fsync → rename.
    pub fn write_atomic(&self, name: &str) -> Result<(), RuntimeError> {
        let dir = skill_dir(name);
        fs::create_dir_all(&dir).map_err(|e| {
            RuntimeError::Tool(format!("cannot create skill dir for sidecar: {e}"))
        })?;

        let content = serde_json::to_string_pretty(self).map_err(|e| {
            RuntimeError::Tool(format!("sidecar serialization failed: {e}"))
        })?;

        let tmp_path = dir.join(".skill-meta.json.tmp");
        let final_path = Self::path(name);

        let mut f = File::create(&tmp_path).map_err(|e| {
            RuntimeError::Tool(format!("cannot create sidecar tmp: {e}"))
        })?;
        if let Err(e) = f.write_all(content.as_bytes()) {
            let _ = fs::remove_file(&tmp_path);
            return Err(RuntimeError::Tool(format!("cannot write sidecar tmp: {e}")));
        }
        if let Err(e) = f.sync_all() {
            let _ = fs::remove_file(&tmp_path);
            return Err(RuntimeError::Tool(format!("sidecar fsync failed: {e}")));
        }
        drop(f);

        if let Err(e) = fs::rename(&tmp_path, &final_path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(RuntimeError::Tool(format!("sidecar rename failed: {e}")));
        }

        Ok(())
    }

    /// Read an existing sidecar. Returns None if missing (back-compat).
    pub fn read(name: &str) -> Option<Self> {
        let path = Self::path(name);
        let content = fs::read_to_string(path).ok()?;
        // Use a permissive Value then re-deserialize so unknown fields are silently dropped
        let val: serde_json::Value = serde_json::from_str(&content).ok()?;
        serde_json::from_value(val).ok()
    }

    /// Create a new sidecar and write it (used by skill_manage create).
    pub fn create(name: &str, provenance: Provenance) -> Result<Self, RuntimeError> {
        let meta = Self::new(provenance);
        meta.write_atomic(name)?;
        Ok(meta)
    }

    /// Bump `last_updated` and write. Lazily creates the sidecar if missing
    /// (back-compat for hand-authored skills). Lazy-create defaults to
    /// `Provenance::User` — these are hand-authored skills, not learn/review
    /// artifacts.
    pub fn touch(name: &str) -> Result<(), RuntimeError> {
        let now = Utc::now().to_rfc3339();
        let mut meta = Self::read(name).unwrap_or_else(|| Self {
            schema_version: 1,
            provenance: Provenance::User,
            created: now.clone(),
            last_updated: now.clone(),
        });
        meta.last_updated = now;
        meta.write_atomic(name)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use serial_test::serial;
    use tempfile::TempDir;

    fn set_home(dir: &Path) -> String {
        let old = std::env::var("HOME").unwrap_or_default();
        unsafe { std::env::set_var("HOME", dir) };
        old
    }

    fn restore_home(old: &str) {
        if old.is_empty() {
            unsafe { std::env::remove_var("HOME") };
        } else {
            unsafe { std::env::set_var("HOME", old) };
        }
    }

    #[test]
    fn roundtrip_serialize_deserialize() {
        let meta = SkillMeta::new(Provenance::User);
        let json = serde_json::to_string_pretty(&meta).unwrap();
        let back: SkillMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schema_version, 1);
        assert_eq!(back.provenance, Provenance::User);
        assert_eq!(back.created, meta.created);
        assert_eq!(back.last_updated, meta.last_updated);
    }

    #[test]
    fn unknown_fields_tolerated() {
        let json = r#"{
            "schema_version": 1,
            "provenance": "user",
            "created": "2024-01-01T00:00:00Z",
            "last_updated": "2024-01-01T00:00:00Z",
            "unknown_future_field": 42,
            "another_field": "hello"
        }"#;
        let meta: SkillMeta = serde_json::from_str(json).unwrap();
        assert_eq!(meta.schema_version, 1);
        assert_eq!(meta.provenance, Provenance::User);
    }

    #[test]
    #[serial]
    fn touch_lazily_creates_sidecar() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());

        // Create the skill directory and SKILL.md (no sidecar)
        let skill_name = "lazy-create-test";
        let skill_dir = tmp.path().join(".synaps-cli/skills").join(skill_name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: lazy-create-test\ndescription: d\n---\nBody",
        )
        .unwrap();

        // No sidecar yet
        assert!(!SkillMeta::path(skill_name).exists());

        SkillMeta::touch(skill_name).unwrap();

        // Sidecar should now exist — read while HOME is still set
        let meta = SkillMeta::read(skill_name).unwrap();
        restore_home(&old);

        assert_eq!(meta.schema_version, 1);
        // On lazy create, created == last_updated
        assert_eq!(meta.created, meta.last_updated);
    }

    #[test]
    #[serial]
    fn touch_bumps_last_updated() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());

        let skill_name = "touch-bump";
        let skill_dir = tmp.path().join(".synaps-cli/skills").join(skill_name);
        std::fs::create_dir_all(&skill_dir).unwrap();

        // Create with a fixed early time
        let early = SkillMeta {
            schema_version: 1,
            provenance: Provenance::Learn,
            created: "2020-01-01T00:00:00+00:00".to_string(),
            last_updated: "2020-01-01T00:00:00+00:00".to_string(),
        };
        early.write_atomic(skill_name).unwrap();

        // Touch should bump last_updated
        std::thread::sleep(std::time::Duration::from_millis(10));
        SkillMeta::touch(skill_name).unwrap();

        let meta = SkillMeta::read(skill_name).unwrap();
        restore_home(&old);

        assert_eq!(meta.created, "2020-01-01T00:00:00+00:00", "created unchanged");
        assert_ne!(meta.last_updated, "2020-01-01T00:00:00+00:00", "last_updated should change");
    }

    #[test]
    #[serial]
    fn read_missing_sidecar_returns_none() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());
        let result = SkillMeta::read("no-such-skill");
        restore_home(&old);
        assert!(result.is_none());
    }

    #[test]
    fn provenance_parse_defaults_to_background_review() {
        assert_eq!(Provenance::parse(None), Provenance::BackgroundReview);
        assert_eq!(Provenance::parse(Some("unknown")), Provenance::BackgroundReview);
        assert_eq!(Provenance::parse(Some("user")), Provenance::User);
        assert_eq!(Provenance::parse(Some("learn")), Provenance::Learn);
    }

    // --- M1: missing fields tolerated ----------------------------------------

    #[test]
    fn deserialize_missing_schema_version_uses_default() {
        let json = r#"{
            "provenance": "user",
            "created": "2024-01-01T00:00:00Z",
            "last_updated": "2024-01-01T00:00:00Z"
        }"#;
        let meta: SkillMeta = serde_json::from_str(json).unwrap();
        assert_eq!(meta.schema_version, 1);
        assert_eq!(meta.provenance, Provenance::User);
    }

    #[test]
    fn deserialize_empty_object_uses_defaults() {
        let meta: SkillMeta = serde_json::from_str("{}").unwrap();
        assert_eq!(meta.schema_version, 1);
        assert_eq!(meta.provenance, Provenance::default());
    }

    // --- M2: touch lazy-create defaults to User ------------------------------

    #[test]
    #[serial]
    fn touch_lazy_create_defaults_to_user() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());

        let name = "lazy-user";
        let skill_dir = tmp.path().join(".synaps-cli/skills").join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "---\nname: lazy-user\ndescription: d\n---\nB").unwrap();

        SkillMeta::touch(name).unwrap();
        let meta = SkillMeta::read(name).unwrap();
        restore_home(&old);

        assert_eq!(meta.provenance, Provenance::User, "lazy-create must default to User");
    }
}
