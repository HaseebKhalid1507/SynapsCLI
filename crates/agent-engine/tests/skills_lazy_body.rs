//! Task 21 — lazy skill bodies (spec §7.6).
//!
//! Boot discovery reads ONLY bounded metadata: frontmatter (name,
//! description) plus provenance, source path, an immutable fingerprint,
//! and sizes. Body bytes are NEVER read, substituted, validated, or
//! inserted into context until one exact skill is SELECTED; selection
//! re-reads the file boundedly and verifies the recorded fingerprint and
//! frontmatter digest before any substitution.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_engine::skills::loader::{
    load_all, DISCOVERY_MAX_SKILLS, SKILL_FILE_MAX_BYTES, SKILL_FRONTMATTER_MAX_BYTES,
};
use agent_engine::skills::registry::CommandRegistry;
use agent_engine::skills::tool::{LoadSkillTool, SearchSkillsTool};
use agent_engine::tools::{ToolCapabilities, ToolChannels, ToolContext, ToolLimits};
use agent_engine::Tool;

// ── plumbing ────────────────────────────────────────────────────────────────

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("synaps-lazy-skill-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A body containing a SENTINEL that must be absent from every boot-time
/// artifact, plus a `{baseDir}` reference proving substitution still works
/// at selection time.
const BODY_SENTINEL: &str = "LAZY_BODY_SENTINEL_71c2";

fn write_skill(dir: &Path, name: &str, body: &str) -> PathBuf {
    let skill_dir = dir.join(name);
    std::fs::create_dir_all(&skill_dir).unwrap();
    let path = skill_dir.join("SKILL.md");
    std::fs::write(
        &path,
        format!("---\nname: {name}\ndescription: desc {name}\n---\n{body}"),
    )
    .unwrap();
    path
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
            delegation_parent: None,
            secret_prompt: None,
            orchestration: None,
            tool_activation: None,
            mcp_leases: None,
            extension_leases: None,
        },
        limits: ToolLimits {
            max_tool_output: 64 * 1024,
            max_tool_buffer: 256 * 1024,
            bash_timeout: 30,
            bash_max_timeout: 300,
            subagent_timeout: 300,
        },
    }
}

// ── tests ───────────────────────────────────────────────────────────────────

/// Boot discovery must not read one body byte: after `load_all`, no
/// discovered metadata (Debug rendering included) contains the body
/// sentinel, and the first-request surfaces (schema + search) stay free
/// of it too.
#[tokio::test]
async fn boot_reads_no_skill_bodies_and_first_request_carries_none() {
    let root = tmp_dir("no-body-boot");
    write_skill(
        &root.join("skills"),
        "alpha",
        &format!("{BODY_SENTINEL} run {{baseDir}}/x.sh"),
    );

    let (_plugins, skills) = load_all(std::slice::from_ref(&root));
    assert_eq!(skills.len(), 1);

    // No body bytes in any metadata rendering.
    let rendered = format!("{skills:?}");
    assert!(
        !rendered.contains(BODY_SENTINEL),
        "boot metadata must not contain body bytes"
    );

    // Bounded metadata is present: name, description, source path, size.
    assert_eq!(skills[0].name, "alpha");
    assert_eq!(skills[0].description, "desc alpha");
    assert!(skills[0].source_path.ends_with("SKILL.md"));

    // First-request surfaces: constant load_skill schema + search results
    // never contain body bytes.
    let registry = Arc::new(CommandRegistry::new(&[], skills));
    let load = LoadSkillTool::new(registry.clone());
    let schema = serde_json::to_string(&load.parameters()).unwrap();
    assert!(!schema.contains(BODY_SENTINEL));
    let search = SearchSkillsTool::new(registry);
    let out = search
        .execute(serde_json::json!({"query": "alpha"}), ctx())
        .await
        .unwrap();
    assert!(out.contains("alpha"));
    assert!(!out.contains(BODY_SENTINEL));
    let _ = std::fs::remove_dir_all(&root);
}

/// The load_skill parameter schema is CONSTANT: it does not grow with the
/// catalog and never enumerates skill names/descriptions (bounded
/// discovery routes through search_skills instead).
#[test]
fn load_skill_schema_is_constant_and_catalog_size_independent() {
    let root = tmp_dir("const-schema");
    let many = root.join("skills");
    for i in 0..40 {
        write_skill(&many, &format!("skill-{i:02}"), "body");
    }
    let (_p, skills) = load_all(std::slice::from_ref(&root));
    assert_eq!(skills.len(), 40);

    let empty = LoadSkillTool::new(Arc::new(CommandRegistry::new(&[], vec![])));
    let full = LoadSkillTool::new(Arc::new(CommandRegistry::new(&[], skills)));
    let empty_schema = serde_json::to_string(&empty.parameters()).unwrap();
    let full_schema = serde_json::to_string(&full.parameters()).unwrap();
    assert_eq!(
        empty_schema, full_schema,
        "schema must not grow with the catalog"
    );
    assert!(!full_schema.contains("skill-07"), "no name enumeration");
    let _ = std::fs::remove_dir_all(&root);
}

/// Selecting one skill loads exactly that body — verified against the
/// boot-recorded fingerprint — with {baseDir} substitution applied at
/// selection time; sibling bodies are never read into the result.
#[tokio::test]
async fn selection_loads_exactly_one_verified_body_with_substitution() {
    let root = tmp_dir("exact-select");
    let dir = root.join("skills");
    write_skill(
        &dir,
        "wanted",
        &format!("{BODY_SENTINEL} use {{baseDir}}/tool.sh"),
    );
    write_skill(&dir, "sibling", "SIBLING_BODY_MARKER_x9");

    let (_p, skills) = load_all(std::slice::from_ref(&root));
    assert_eq!(skills.len(), 2);
    let registry = Arc::new(CommandRegistry::new(&[], skills));
    let tool = LoadSkillTool::new(registry);

    let out = tool
        .execute(serde_json::json!({"skill": "wanted"}), ctx())
        .await
        .unwrap();
    assert!(out.contains("# Skill: wanted"));
    assert!(out.contains(BODY_SENTINEL));
    // {baseDir} substituted to the absolute skill dir at SELECTION time.
    let base = dir.join("wanted").canonicalize().unwrap();
    assert!(out.contains(&format!("{}/tool.sh", base.display())));
    assert!(
        !out.contains("SIBLING_BODY_MARKER_x9"),
        "sibling bodies must not be read"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A body mutated AFTER boot fails the recorded fingerprint check closed:
/// the tampered body is never returned, and the error reveals neither
/// paths nor hashes.
#[tokio::test]
async fn post_boot_tamper_fails_closed_without_leaking_paths() {
    let root = tmp_dir("tamper");
    let dir = root.join("skills");
    let path = write_skill(&dir, "victim", "ORIGINAL_BODY");

    let (_p, skills) = load_all(std::slice::from_ref(&root));
    let registry = Arc::new(CommandRegistry::new(&[], skills));
    let tool = LoadSkillTool::new(registry);

    // Mutate the file after discovery recorded its fingerprint.
    std::fs::write(
        &path,
        "---\nname: victim\ndescription: desc victim\n---\nTAMPERED_BODY_INJECTED",
    )
    .unwrap();

    let err = tool
        .execute(serde_json::json!({"skill": "victim"}), ctx())
        .await
        .expect_err("tampered skill must fail closed");
    let msg = format!("{err}");
    assert!(
        !msg.contains("TAMPERED_BODY_INJECTED") && !msg.contains("ORIGINAL_BODY"),
        "no body content in errors: {msg}"
    );
    assert!(
        !msg.contains(root.to_str().unwrap()),
        "no source paths in errors: {msg}"
    );
    assert!(
        msg.to_lowercase().contains("changed") || msg.to_lowercase().contains("verif"),
        "typed static reason expected: {msg}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Deleting the file after boot also fails closed, path-free.
#[tokio::test]
async fn deleted_source_fails_closed() {
    let root = tmp_dir("deleted");
    let dir = root.join("skills");
    let path = write_skill(&dir, "gone", "BODY");
    let (_p, skills) = load_all(std::slice::from_ref(&root));
    let registry = Arc::new(CommandRegistry::new(&[], skills));
    let tool = LoadSkillTool::new(registry);
    std::fs::remove_file(&path).unwrap();
    let err = tool
        .execute(serde_json::json!({"skill": "gone"}), ctx())
        .await
        .expect_err("deleted skill must fail closed");
    let msg = format!("{err}");
    assert!(!msg.contains(root.to_str().unwrap()), "{msg}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Oversized inputs fail closed at DISCOVERY: frontmatter beyond the 8 KiB
/// scan bound and regular files beyond the 1 MiB cap are skipped without
/// reading bodies; sane siblings still load.
#[test]
fn oversized_frontmatter_and_files_are_skipped_bounded() {
    assert_eq!(SKILL_FRONTMATTER_MAX_BYTES, 8 * 1024);
    assert_eq!(SKILL_FILE_MAX_BYTES, 1024 * 1024);
    let root = tmp_dir("oversize");
    let dir = root.join("skills");

    // Frontmatter larger than the scan bound (never closes within 8 KiB).
    let huge_fm_dir = dir.join("huge-fm");
    std::fs::create_dir_all(&huge_fm_dir).unwrap();
    std::fs::write(
        huge_fm_dir.join("SKILL.md"),
        format!("---\nname: huge-fm\n{}\n---\nbody", "x: y\n".repeat(4000)),
    )
    .unwrap();

    // Regular file over the 1 MiB cap (frontmatter fine, body huge).
    let huge_file_dir = dir.join("huge-file");
    std::fs::create_dir_all(&huge_file_dir).unwrap();
    std::fs::write(
        huge_file_dir.join("SKILL.md"),
        format!(
            "---\nname: huge-file\ndescription: d\n---\n{}",
            "B".repeat(SKILL_FILE_MAX_BYTES + 1)
        ),
    )
    .unwrap();

    write_skill(&dir, "sane", "body");

    let (_p, skills) = load_all(std::slice::from_ref(&root));
    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["sane"]);
    let _ = std::fs::remove_dir_all(&root);
}

/// Discovery caps are deterministic: beyond DISCOVERY_MAX_SKILLS the
/// remaining candidates are skipped (first-wins) instead of growing
/// without bound.
#[test]
fn discovery_skill_count_cap_is_deterministic() {
    assert_eq!(DISCOVERY_MAX_SKILLS, 2048);
    let root = tmp_dir("cap");
    let dir = root.join("skills");
    for i in 0..2050 {
        write_skill(&dir, &format!("s{i:04}"), "b");
    }
    let (_p, skills) = load_all(std::slice::from_ref(&root));
    assert_eq!(skills.len(), DISCOVERY_MAX_SKILLS);
    // Deterministic retained MEMBERSHIP: sorted directory order means the
    // first 2048 names win and exactly s2048/s2049 are skipped — on every
    // run, regardless of filesystem read_dir order.
    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names[0], "s0000");
    assert_eq!(names[DISCOVERY_MAX_SKILLS - 1], "s2047");
    assert!(!names.contains(&"s2048") && !names.contains(&"s2049"));
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(
        names, sorted,
        "retained order is the deterministic sorted order"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The metadata BYTE cap is enforced too: skills whose retained metadata
/// would exceed the byte budget are skipped deterministically even when
/// the count cap is far away.
#[test]
fn discovery_metadata_byte_cap_is_enforced() {
    use agent_engine::skills::loader::DISCOVERY_MAX_METADATA_BYTES;
    let root = tmp_dir("byte-cap");
    let dir = root.join("skills");
    // ~1000-byte descriptions × names: each retained skill costs >1 KiB of
    // metadata, so ~4 MiB admits well under the 2048 count cap.
    let big_desc = "d".repeat(1000);
    let per_skill_floor = 1000; // description alone
    let overshoot = DISCOVERY_MAX_METADATA_BYTES / per_skill_floor + 50;
    assert!(overshoot < 2048 * 3); // sanity: still a bounded test
    for i in 0..overshoot.min(4600) {
        let name = format!("s{i:04}");
        let skill_dir = dir.join(&name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {big_desc}\n---\nb"),
        )
        .unwrap();
    }
    let (_p, skills) = load_all(std::slice::from_ref(&root));
    assert!(
        skills.len() < overshoot.min(4600),
        "byte cap must skip later candidates ({} admitted)",
        skills.len()
    );
    // Deterministic first-wins membership under the byte cap as well.
    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
    assert_eq!(names[0], "s0000");
    let _ = std::fs::remove_dir_all(&root);
}

/// Selection re-verifies and works repeatedly (no consumed state), and
/// stable-id selection loads the same verified body.
#[tokio::test]
async fn selection_is_repeatable_and_stable_id_resolves() {
    let root = tmp_dir("repeat");
    write_skill(&root.join("skills"), "stable", BODY_SENTINEL);
    let (_p, skills) = load_all(std::slice::from_ref(&root));
    let id = agent_engine::skills::stable_skill_id(&skills[0]);
    let registry = Arc::new(CommandRegistry::new(&[], skills));
    let tool = LoadSkillTool::new(registry);
    for _ in 0..2 {
        let out = tool
            .execute(serde_json::json!({"skill": "stable"}), ctx())
            .await
            .unwrap();
        assert!(out.contains(BODY_SENTINEL));
    }
    let out = tool
        .execute(serde_json::json!({"skill": id}), ctx())
        .await
        .unwrap();
    assert!(out.contains(BODY_SENTINEL));
    let _ = std::fs::remove_dir_all(&root);
}

/// A skill whose BODY is invalid UTF-8 still discovers at boot (bodies
/// are never read), and selection fails closed typed without echoing
/// bytes or paths.
#[tokio::test]
async fn invalid_utf8_body_discovers_at_boot_and_fails_closed_on_selection() {
    let root = tmp_dir("bad-utf8");
    let dir = root.join("skills").join("binary");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("SKILL.md");
    let mut bytes = b"---\nname: binary\ndescription: d\n---\n".to_vec();
    bytes.extend_from_slice(&[0xFF, 0xFE, 0x00, 0xC3, 0x28]); // invalid UTF-8 body
    std::fs::write(&path, &bytes).unwrap();

    let (_p, skills) = load_all(std::slice::from_ref(&root));
    assert_eq!(skills.len(), 1, "boot never reads the body, so it loads");
    assert_eq!(skills[0].name, "binary");

    let registry = Arc::new(CommandRegistry::new(&[], skills));
    let tool = LoadSkillTool::new(registry);
    let err = tool
        .execute(serde_json::json!({"skill": "binary"}), ctx())
        .await
        .expect_err("invalid UTF-8 body must fail closed at selection");
    let msg = format!("{err}");
    assert!(!msg.contains(root.to_str().unwrap()), "{msg}");
    assert!(
        msg.to_lowercase().contains("utf-8") || msg.to_lowercase().contains("verif"),
        "{msg}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Unix: a SKILL.md that became a symlink after boot is refused at
/// selection (O_NOFOLLOW), path-free.
#[cfg(unix)]
#[tokio::test]
async fn symlinked_source_is_refused_at_selection() {
    let root = tmp_dir("symlink");
    let dir = root.join("skills");
    let path = write_skill(&dir, "linked", "REAL_BODY");
    let (_p, skills) = load_all(std::slice::from_ref(&root));
    let registry = Arc::new(CommandRegistry::new(&[], skills));
    let tool = LoadSkillTool::new(registry);

    // Swap the regular file for a symlink pointing at attacker content.
    let target = root.join("attacker.md");
    std::fs::write(
        &target,
        "---\nname: linked\ndescription: desc linked\n---\nATTACKER_BODY",
    )
    .unwrap();
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();

    let err = tool
        .execute(serde_json::json!({"skill": "linked"}), ctx())
        .await
        .expect_err("symlinked source must be refused");
    let msg = format!("{err}");
    assert!(!msg.contains("ATTACKER_BODY"), "{msg}");
    assert!(!msg.contains(root.to_str().unwrap()), "{msg}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Unix: a SKILL.md replaced by a FIFO after boot must fail closed
/// promptly (O_NONBLOCK — no open/read hang) instead of blocking the
/// selection path forever.
#[cfg(unix)]
#[tokio::test]
async fn fifo_source_fails_closed_without_hanging() {
    let root = tmp_dir("fifo");
    let dir = root.join("skills");
    let path = write_skill(&dir, "pipe", "BODY");
    let (_p, skills) = load_all(std::slice::from_ref(&root));
    let registry = Arc::new(CommandRegistry::new(&[], skills));
    let tool = LoadSkillTool::new(registry);

    std::fs::remove_file(&path).unwrap();
    let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

    let attempt = tool.execute(serde_json::json!({"skill": "pipe"}), ctx());
    let err = tokio::time::timeout(std::time::Duration::from_secs(5), attempt)
        .await
        .expect("selection must not hang on a FIFO")
        .expect_err("FIFO source must fail closed");
    let msg = format!("{err}");
    assert!(!msg.contains(root.to_str().unwrap()), "{msg}");
    let _ = std::fs::remove_dir_all(&root);
}
