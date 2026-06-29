//! Integration test: skill_manage end-to-end (A1–A4).
//!
//! Binds HOME to a TempDir. No Axel involved — indexing is passive.

use std::sync::Arc;
use serial_test::serial;
use tempfile::TempDir;

use agent_engine::{
    skills::{
        loader::load_skill_file,
        registry::CommandRegistry,
        sidecar::SkillMeta,
    },
    SynapsConfig, Tool, ToolContext,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_ctx() -> ToolContext {
    ToolContext {
        channels: agent_engine::tools::ToolChannels {
            tx_delta: None,
            tx_events: None,
        },
        capabilities: agent_engine::tools::ToolCapabilities {
            watcher_exit_path: None,
            tool_register_tx: None,
            session_manager: None,
            subagent_registry: None,
            event_queue: None,
            secret_prompt: None,
        },
        limits: agent_engine::tools::ToolLimits {
            max_tool_output: 30000,
            max_tool_buffer: 256 * 1024,
            bash_timeout: 30,
            bash_max_timeout: 300,
            subagent_timeout: 300,
        },
    }
}

fn make_tool(
    registry: Arc<CommandRegistry>,
    config: Arc<SynapsConfig>,
) -> agent_engine::skills::manage_tool::SkillManageTool {
    agent_engine::skills::manage_tool::SkillManageTool::new(registry, config)
}

// ---------------------------------------------------------------------------
// A1 — create → load_skill reads it back byte-identical
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn a1_create_and_load_back() {
    let tmp = TempDir::new().unwrap();
    unsafe { std::env::set_var("HOME", tmp.path()) };

    let registry = Arc::new(CommandRegistry::new(&[], vec![]));
    let config = Arc::new(SynapsConfig::default());
    let tool = make_tool(registry, config);

    let body = "# My Skill\n\nDo things properly.";
    let out = tool
        .execute(
            serde_json::json!({
                "action": "create",
                "name": "my-skill",
                "description": "Does things",
                "body": body
            }),
            test_ctx(),
        )
        .await
        .unwrap();

    assert!(out.contains("created"), "output: {out}");

    // Verify SKILL.md is on disk
    let skill_md = tmp
        .path()
        .join(".synaps-cli/skills/my-skill/SKILL.md");
    assert!(skill_md.exists(), "SKILL.md not created");

    // Use load_skill_file directly — A1 round-trip
    let loaded = load_skill_file(&skill_md, None, None).unwrap();
    assert_eq!(loaded.name, "my-skill");
    assert_eq!(loaded.description, "Does things");
    assert_eq!(loaded.body, body, "body mismatch");

    // On-disk bytes match expected content
    let on_disk = std::fs::read_to_string(&skill_md).unwrap();
    assert!(on_disk.starts_with("---\nname: my-skill\n"), "frontmatter: {on_disk}");
    assert!(on_disk.contains("description: Does things"));
    assert!(on_disk.contains(body));

    // Sidecar exists
    assert!(SkillMeta::path("my-skill").exists(), "sidecar missing");
    let meta = SkillMeta::read("my-skill").unwrap();
    assert_eq!(meta.schema_version, 1);
}

// ---------------------------------------------------------------------------
// A2 — update → body diff persists; sidecar last_updated changes; load works
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn a2_update_persists_diff_and_bumps_sidecar() {
    let tmp = TempDir::new().unwrap();
    unsafe { std::env::set_var("HOME", tmp.path()) };

    let registry = Arc::new(CommandRegistry::new(&[], vec![]));
    let config = Arc::new(SynapsConfig::default());
    let tool = make_tool(registry, config);

    // Create
    tool.execute(
        serde_json::json!({
            "action": "create",
            "name": "upd-skill",
            "description": "Original desc",
            "body": "# Original\nOriginal body."
        }),
        test_ctx(),
    )
    .await
    .unwrap();

    let meta_before = SkillMeta::read("upd-skill").unwrap();

    // Small delay so timestamps differ
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Update
    let new_body = "# Updated\nNew body content.";
    let out = tool
        .execute(
            serde_json::json!({
                "action": "update",
                "name": "upd-skill",
                "body": new_body
            }),
            test_ctx(),
        )
        .await
        .unwrap();

    assert!(out.contains("updated"), "output: {out}");

    // Body changed on disk
    let skill_md = tmp
        .path()
        .join(".synaps-cli/skills/upd-skill/SKILL.md");
    let loaded = load_skill_file(&skill_md, None, None).unwrap();
    assert_eq!(loaded.body, new_body, "body should be updated");

    // Frontmatter preserved (description unchanged since we didn't pass one)
    assert_eq!(loaded.description, "Original desc", "description should be preserved");

    // Sidecar last_updated > created
    let meta_after = SkillMeta::read("upd-skill").unwrap();
    assert_eq!(meta_after.created, meta_before.created, "created should be unchanged");
    assert_ne!(
        meta_after.last_updated, meta_before.last_updated,
        "last_updated should have changed"
    );
    assert!(
        meta_after.last_updated > meta_before.last_updated,
        "last_updated should be later"
    );
}

// ---------------------------------------------------------------------------
// A3 — static contract: file shape satisfies Axel source-root requirements
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn a3_static_indexing_contract() {
    let tmp = TempDir::new().unwrap();
    unsafe { std::env::set_var("HOME", tmp.path()) };

    let registry = Arc::new(CommandRegistry::new(&[], vec![]));
    let config = Arc::new(SynapsConfig::default());
    let tool = make_tool(registry, config);

    // Create
    tool.execute(
        serde_json::json!({
            "action": "create",
            "name": "idx-skill",
            "description": "Indexable skill",
            "body": "# Idx\nContent for indexing."
        }),
        test_ctx(),
    )
    .await
    .unwrap();

    let skills_root = tmp.path().join(".synaps-cli/skills");
    let skill_md = skills_root.join("idx-skill/SKILL.md");

    // File has .md extension (Axel reindex ext filter: "md" | "txt")
    assert_eq!(skill_md.extension().and_then(|e| e.to_str()), Some("md"));

    // File is non-empty
    let content = std::fs::read_to_string(&skill_md).unwrap();
    assert!(!content.is_empty());

    // File lives directly under the registered source root
    assert_eq!(
        skill_md.parent().unwrap().parent().unwrap(),
        skills_root,
        "SKILL.md should be under <skills_root>/<name>/"
    );

    // Delete → no .md file remains under live skills root for this skill
    tool.execute(
        serde_json::json!({"action": "delete", "name": "idx-skill"}),
        test_ctx(),
    )
    .await
    .unwrap();

    // Live path gone
    assert!(!skill_md.exists(), "SKILL.md should be gone after delete");

    // Archive renamed to .archived (Axel won't pick it up)
    let archive_base = skills_root.join(".archive");
    let mut has_archived_md = false;
    if let Ok(entries) = std::fs::read_dir(&archive_base) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let archived = p.join("SKILL.md.archived");
                if archived.exists() {
                    has_archived_md = true;
                }
                // Assert NO .md file exists under the archive entry
                let md_in_archive = p.join("SKILL.md");
                assert!(!md_in_archive.exists(), "SKILL.md should not remain in archive");
            }
        }
    }
    assert!(has_archived_md, "SKILL.md.archived should exist in archive dir");
}

// ---------------------------------------------------------------------------
// A4 — no regression: existing skill round-trip still works
// ---------------------------------------------------------------------------

#[test]
fn a4_load_skill_file_unchanged() {
    // Directly test loader (no HOME needed) — exercises the read path
    // that must remain byte-identical.
    let tmp = TempDir::new().unwrap();
    let skill_dir = tmp.path().join("code-review");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: code-review\ndescription: Review code carefully\n---\n# Code Review\n\nCheck everything.",
    )
    .unwrap();

    let path = skill_dir.join("SKILL.md");
    let skill = load_skill_file(&path, None, None).unwrap();

    assert_eq!(skill.name, "code-review");
    assert_eq!(skill.description, "Review code carefully");
    assert_eq!(skill.body, "# Code Review\n\nCheck everything.");
    assert!(skill.plugin.is_none());
    assert!(skill.base_dir.is_absolute());
    assert!(skill.source_path.is_absolute());
}

// ---------------------------------------------------------------------------
// Security: frontmatter injection refused end-to-end (B1 from sec review)
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn security_frontmatter_injection_refused() {
    let tmp = TempDir::new().unwrap();
    unsafe { std::env::set_var("HOME", tmp.path()) };

    let registry = Arc::new(CommandRegistry::new(&[], vec![]));
    let config = Arc::new(SynapsConfig::default());
    let tool = make_tool(registry, config);

    let err = tool
        .execute(
            serde_json::json!({
                "action": "create",
                "name": "inject",
                "description": "evil\n---\nbody-pwn",
                "body": "# body"
            }),
            test_ctx(),
        )
        .await
        .unwrap_err();

    assert!(format!("{err}").to_lowercase().contains("description"), "got: {err}");

    let path = tmp.path().join(".synaps-cli/skills/inject/SKILL.md");
    assert!(!path.exists(), "no SKILL.md should be on disk after refusal");
}

// ---------------------------------------------------------------------------
// Security: archive timestamp collisions (B2 from sec review)
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn security_archive_no_collision_on_rapid_recreate() {
    let tmp = TempDir::new().unwrap();
    unsafe { std::env::set_var("HOME", tmp.path()) };

    let registry = Arc::new(CommandRegistry::new(&[], vec![]));
    let config = Arc::new(SynapsConfig::default());
    let tool = make_tool(registry, config);

    for _ in 0..2 {
        tool.execute(
            serde_json::json!({"action":"create","name":"rapid","description":"d","body":"# b"}),
            test_ctx(),
        ).await.unwrap();
        tool.execute(
            serde_json::json!({"action":"delete","name":"rapid"}),
            test_ctx(),
        ).await.unwrap();
    }

    let archive = tmp.path().join(".synaps-cli/skills/.archive");
    let count = std::fs::read_dir(&archive).unwrap().count();
    assert_eq!(count, 2, "rapid create/delete must produce two distinct archive dirs");
}
