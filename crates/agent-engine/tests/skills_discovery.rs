//! Task 17 — `search_skills` and stable-ID `load_skill` (spec §7.2, §7.6
//! boundary: T17 adds IDs and bounded search WITHOUT touching skill bodies;
//! lazy body reads are Task 21).
//!
//! Covers:
//! - stable skill IDs: deterministic, alias-safe (injective per
//!   (plugin, name)), bounded, and free of source paths;
//! - `search_skills`: bounded deterministic output of stable IDs + compact
//!   descriptions only — no bodies, no paths, no process/network;
//! - `load_skill`: exact stable ID loads only the selected skill; existing
//!   exact qualified/bare inputs keep working where unambiguous; bare-name
//!   alias ambiguity still fails typed.

use std::path::PathBuf;
use std::sync::Arc;

use agent_engine::skills::registry::CommandRegistry;
use agent_engine::skills::tool::{LoadSkillTool, SearchSkillsTool};
use agent_engine::skills::{stable_skill_id, LoadedSkill, BUILTIN_COMMANDS};
use agent_engine::tools::{Tool, ToolCapabilities, ToolChannels, ToolContext, ToolLimits};
use serde_json::json;

const BODY_MARKER: &str = "SECRET-SKILL-BODY-MARKER";
const PATH_MARKER: &str = "/secret/skill/source/path";

fn skill(name: &str, plugin: Option<&str>, description: &str) -> LoadedSkill {
    LoadedSkill {
        name: name.to_string(),
        description: description.to_string(),
        body: format!("{BODY_MARKER} body of {name}"),
        plugin: plugin.map(str::to_string),
        base_dir: PathBuf::from(PATH_MARKER),
        source_path: PathBuf::from(format!("{PATH_MARKER}/{name}/SKILL.md")),
    }
}

fn registry() -> Arc<CommandRegistry> {
    Arc::new(CommandRegistry::new(
        BUILTIN_COMMANDS,
        vec![
            skill("review", None, "loose review checklist"),
            skill("dup", Some("p1"), "first duplicate"),
            skill("dup", Some("p2"), "second duplicate"),
            skill("Wei rd:Name", Some("Pl Ug"), "weird identity skill"),
        ],
    ))
}

fn ctx() -> ToolContext {
    ToolContext {
        channels: ToolChannels {
            tx_delta: None,
            tx_events: None,
        },
        capabilities: ToolCapabilities {
            watcher_exit_path: None,
            tool_register_tx: None,
            session_manager: None,
            subagent_registry: None,
            event_queue: None,
            secret_prompt: None,
            orchestration: None,
            tool_activation: None,
            mcp_leases: None,
        },
        limits: ToolLimits {
            max_tool_output: 30000,
            max_tool_buffer: 256 * 1024,
            bash_timeout: 30,
            bash_max_timeout: 300,
            subagent_timeout: 300,
        },
    }
}

// ── Stable skill IDs ────────────────────────────────────────────────────────

#[test]
fn stable_skill_ids_are_deterministic_alias_safe_and_bounded() {
    let loose = skill("review", None, "d");
    let p1 = skill("dup", Some("p1"), "d");
    let p2 = skill("dup", Some("p2"), "d");
    let weird = skill("Wei rd:Name", Some("Pl Ug"), "d");

    // Deterministic.
    assert_eq!(stable_skill_id(&loose), stable_skill_id(&loose));
    assert_eq!(stable_skill_id(&weird), stable_skill_id(&weird));
    // Alias-safe: distinct (plugin, name) pairs never collapse.
    assert_ne!(stable_skill_id(&p1), stable_skill_id(&p2));
    assert_ne!(stable_skill_id(&loose), stable_skill_id(&p1));
    // A loose skill and a plugin skill spelling the same raw text differ.
    let loose_qualified = skill("p1:dup", None, "d");
    assert_ne!(stable_skill_id(&loose_qualified), stable_skill_id(&p1));
    // Bounded and path-free even for hostile raw identities.
    let long = skill(&"x".repeat(4096), Some(&"y".repeat(4096)), "d");
    let id = stable_skill_id(&long);
    assert!(id.len() <= 256, "id must stay bounded, got {}", id.len());
    for s in [
        stable_skill_id(&loose),
        stable_skill_id(&p1),
        stable_skill_id(&weird),
        id,
    ] {
        assert!(!s.contains(PATH_MARKER), "no source path in id: {s}");
        assert!(!s.contains(' '), "ids are canonical: {s}");
    }
}

// ── search_skills ───────────────────────────────────────────────────────────

#[tokio::test]
async fn search_skills_is_bounded_deterministic_ids_and_descriptions_only() {
    let tool = SearchSkillsTool::new(registry());

    let first = tool
        .execute(json!({"query": "dup"}), ctx())
        .await
        .expect("search succeeds");
    let second = tool
        .execute(json!({"query": "dup"}), ctx())
        .await
        .expect("search succeeds");
    assert_eq!(first, second, "deterministic across runs");

    // Both duplicates surface with distinct stable IDs, in sorted order.
    let p1 = skill("dup", Some("p1"), "first duplicate");
    let p2 = skill("dup", Some("p2"), "second duplicate");
    let i1 = first.find(&stable_skill_id(&p1)).expect("p1 id present");
    let i2 = first.find(&stable_skill_id(&p2)).expect("p2 id present");
    assert!(i1 < i2, "deterministic id ordering");
    assert!(first.contains("first duplicate"));

    // Compact descriptors only: no bodies, no source paths.
    assert!(!first.contains(BODY_MARKER), "no skill body: {first}");
    assert!(!first.contains(PATH_MARKER), "no source path: {first}");
    assert!(first.len() <= 10_000, "bounded output");
}

#[tokio::test]
async fn search_skills_fails_typed_on_empty_and_oversized_queries() {
    let tool = SearchSkillsTool::new(registry());

    let empty = tool
        .execute(json!({"query": ""}), ctx())
        .await
        .expect_err("empty query fails");
    assert!(empty.to_string().contains("empty"), "{empty}");

    let oversized = tool
        .execute(json!({"query": "q".repeat(4096)}), ctx())
        .await
        .expect_err("oversized query fails");
    assert!(oversized.to_string().contains("oversized"), "{oversized}");
    assert!(oversized.to_string().len() < 512, "bounded error");

    let missing = tool
        .execute(json!({}), ctx())
        .await
        .expect_err("missing query fails");
    assert!(missing.to_string().contains("query"), "{missing}");
}

// ── load_skill with stable IDs ──────────────────────────────────────────────

#[tokio::test]
async fn load_skill_exact_stable_id_loads_only_the_selected_skill() {
    let tool = LoadSkillTool::new(registry());
    let p1_id = stable_skill_id(&skill("dup", Some("p1"), "first duplicate"));

    let out = tool
        .execute(json!({"skill": p1_id}), ctx())
        .await
        .expect("stable id loads");
    assert!(out.contains("body of dup"));
    assert!(out.contains("first duplicate"), "selected p1: {out}");
    assert!(!out.contains("second duplicate"), "p2 not loaded: {out}");
}

#[tokio::test]
async fn load_skill_keeps_exact_qualified_and_unambiguous_bare_inputs() {
    let tool = LoadSkillTool::new(registry());

    // Exact qualified input still resolves.
    let qualified = tool
        .execute(json!({"skill": "p1:dup"}), ctx())
        .await
        .expect("qualified name loads");
    assert!(qualified.contains("first duplicate"));

    // Unambiguous bare input still resolves.
    let bare = tool
        .execute(json!({"skill": "review"}), ctx())
        .await
        .expect("unambiguous bare name loads");
    assert!(bare.contains("loose review checklist"));
}

#[tokio::test]
async fn load_skill_alias_ambiguity_and_unknown_ids_fail_typed() {
    let tool = LoadSkillTool::new(registry());

    // Bare-name alias ambiguity fails (never guesses a sibling).
    let ambiguous = tool
        .execute(json!({"skill": "dup"}), ctx())
        .await
        .expect_err("ambiguous bare name fails");
    assert!(ambiguous.to_string().contains("ambiguous"), "{ambiguous}");

    // Unknown stable ID fails typed.
    let unknown = tool
        .execute(json!({"skill": "skill:does-not-exist"}), ctx())
        .await
        .expect_err("unknown stable id fails");
    assert!(unknown.to_string().contains("unknown"), "{unknown}");
}

// ── Stable-namespace vs legacy plugin-qualified collisions ──────────────────

#[tokio::test]
async fn load_skill_stable_id_colliding_with_legacy_plugin_qualified_fails_closed() {
    // A loose skill `foo` (stable id `skill:foo`) AND a legacy plugin
    // literally named `skill` providing `foo` (legacy qualified `skill:foo`):
    // the input denotes both, so it must fail closed as ambiguous — never
    // silently resolve either interpretation.
    let reg = Arc::new(CommandRegistry::new(
        BUILTIN_COMMANDS,
        vec![
            skill("foo", None, "loose foo description"),
            skill("foo", Some("skill"), "plugin-qualified foo description"),
        ],
    ));
    let tool = LoadSkillTool::new(reg);
    let err = tool
        .execute(json!({"skill": "skill:foo"}), ctx())
        .await
        .expect_err("colliding spelling must fail closed");
    let msg = err.to_string();
    assert!(msg.contains("ambiguous"), "{msg}");
    // The denial points at the unambiguous stable id of the plugin skill.
    assert!(msg.contains("skill.skill:foo"), "{msg}");
    assert!(!msg.contains("loose foo description"), "{msg}");
}

#[tokio::test]
async fn load_skill_plugin_named_skill_keeps_qualified_access_without_collision() {
    // Only the legacy plugin-qualified skill exists: exact backward-compatible
    // behavior — `skill:foo` resolves to the plugin skill.
    let reg = Arc::new(CommandRegistry::new(
        BUILTIN_COMMANDS,
        vec![skill(
            "foo",
            Some("skill"),
            "plugin-qualified foo description",
        )],
    ));
    let tool = LoadSkillTool::new(Arc::clone(&reg));
    let out = tool
        .execute(json!({"skill": "skill:foo"}), ctx())
        .await
        .expect("plugin-only qualified access keeps resolving");
    assert!(out.contains("plugin-qualified foo description"), "{out}");

    // The unambiguous stable id of the same plugin skill also resolves.
    let plugin_id = stable_skill_id(&skill("foo", Some("skill"), "d"));
    let via_stable = tool
        .execute(json!({"skill": plugin_id}), ctx())
        .await
        .expect("stable plugin-skill id resolves");
    assert!(
        via_stable.contains("plugin-qualified foo description"),
        "{via_stable}"
    );
}

#[tokio::test]
async fn load_skill_stable_loose_id_keeps_resolving_without_collision() {
    // Only the loose skill exists: `skill:foo` resolves it by stable id.
    let reg = Arc::new(CommandRegistry::new(
        BUILTIN_COMMANDS,
        vec![skill("foo", None, "loose foo description")],
    ));
    let tool = LoadSkillTool::new(reg);
    let out = tool
        .execute(json!({"skill": "skill:foo"}), ctx())
        .await
        .expect("stable loose id resolves");
    assert!(out.contains("loose foo description"), "{out}");
}

#[test]
fn load_skill_schema_stays_compact_without_bodies() {
    let tool = LoadSkillTool::new(registry());
    let schema = serde_json::to_string(&tool.parameters()).expect("serializes");
    assert!(!schema.contains(BODY_MARKER), "no bodies in schema");
    assert!(!schema.contains(PATH_MARKER), "no paths in schema");
}
