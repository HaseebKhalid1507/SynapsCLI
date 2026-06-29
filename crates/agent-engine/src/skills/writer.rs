//! Atomic write primitives for loose skill files.
//!
//! All writes go through `write_skill_md_atomic`: tmp file → fsync → rename.
//! Never truncates SKILL.md directly — crash leaves no partial state.
//!
//! Companion to `loader.rs` (read path, UNTOUCHED).

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use fs4::fs_std::FileExt;

use crate::RuntimeError;

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// `~/.synaps-cli/skills/` — canonical write root for loose skills.
pub fn skills_root() -> PathBuf {
    crate::config::base_dir().join("skills")
}

/// Directory for a specific skill: `~/.synaps-cli/skills/<name>/`.
pub fn skill_dir(name: &str) -> PathBuf {
    skills_root().join(name)
}

/// Archive root: `~/.synaps-cli/skills/.archive/`.
pub fn archive_root() -> PathBuf {
    skills_root().join(".archive")
}

// ---------------------------------------------------------------------------
// Name validation
// ---------------------------------------------------------------------------

/// Reserved names (Windows compat + defensive Linux).
const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul",
    "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9",
    "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Validate a skill name: `^[a-z0-9][a-z0-9-]{0,63}$`.
/// Also rejects path separators, `.`, `..`, and Windows reserved names.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name must not be empty".into());
    }
    if name.len() > 64 {
        return Err(format!("name too long ({} chars, max 64)", name.len()));
    }
    if name.contains('/') || name.contains('\\') {
        return Err("name must not contain path separators".into());
    }
    if name == "." || name == ".." {
        return Err("name must not be '.' or '..'".into());
    }
    // Defensive: belt-and-braces against frontmatter injection. The
    // [a-z0-9-] charset rejects these anyway, but be explicit at the
    // boundary so refactors of the charset can't reintroduce the hole.
    if name.contains('\n') || name.contains('\r') || name.contains("---") {
        return Err("name must not contain newlines or '---'".into());
    }
    // Must start with [a-z0-9]
    let mut chars = name.chars();
    let first = chars.next().unwrap(); // non-empty checked above
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(format!("name must start with [a-z0-9], got '{first}'"));
    }
    // Remaining chars: [a-z0-9-]
    for ch in chars {
        if !ch.is_ascii_lowercase() && !ch.is_ascii_digit() && ch != '-' {
            return Err(format!("name contains invalid char '{ch}' (only [a-z0-9-] allowed)"));
        }
    }
    // Reserved names
    if RESERVED.contains(&name) {
        return Err(format!("'{name}' is a reserved name"));
    }
    Ok(())
}

/// Validate a skill description for inclusion in YAML frontmatter.
///
/// Rejects: empty, > 200 chars, embedded newlines/carriage returns,
/// leading/trailing whitespace, and any occurrence of `---` (which would
/// terminate the frontmatter block and let body content masquerade as
/// metadata — frontmatter injection).
pub fn validate_description(desc: &str) -> Result<(), String> {
    if desc.is_empty() {
        return Err("description must not be empty".into());
    }
    let char_len = desc.chars().count();
    if char_len > 200 {
        return Err(format!("description too long ({char_len} chars, max 200)"));
    }
    if desc.contains('\n') || desc.contains('\r') {
        return Err("description must not contain newlines".into());
    }
    if desc.trim() != desc {
        return Err("description must not have leading/trailing whitespace".into());
    }
    if desc.contains("---") {
        return Err("description must not contain '---' (frontmatter delimiter)".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Plugin-ownership guard
// ---------------------------------------------------------------------------

/// Canonicalize a path, tolerating non-existent leaves by canonicalizing
/// the nearest existing ancestor and re-appending the tail.
fn canonicalize_lenient(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    let mut cur = p.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while let Some(parent) = cur.parent().map(Path::to_path_buf) {
        if let Some(name) = cur.file_name() {
            tail.push(name.to_os_string());
        }
        cur = parent;
        if let Ok(c) = cur.canonicalize() {
            let mut out = c;
            for seg in tail.iter().rev() {
                out.push(seg);
            }
            return out;
        }
        if cur.as_os_str().is_empty() {
            break;
        }
    }
    p.to_path_buf()
}

/// Reject writes to plugin-owned skills.
///
/// Robust check (no substring matching on path strings):
/// 1. Any skill discovered with `plugin: Some(_)` is plugin-owned.
/// 2. For loose skills with the same name already in the registry,
///    canonicalize their `source_path` and the loose skills root; refuse
///    if the source lives outside the loose root (i.e. came from a
///    plugin discovery root).
pub fn ensure_writable(
    registry: &crate::skills::registry::CommandRegistry,
    name: &str,
) -> Result<(), RuntimeError> {
    let loose_root = canonicalize_lenient(&skills_root());

    for skill in registry.all_skills() {
        if skill.name != name {
            continue;
        }
        if skill.plugin.is_some() {
            return Err(RuntimeError::Tool(
                "plugin-owned skill; not writable".into(),
            ));
        }
        let src = canonicalize_lenient(&skill.source_path);
        if !src.starts_with(&loose_root) {
            return Err(RuntimeError::Tool(
                "plugin-owned skill; not writable".into(),
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Advisory file lock
// ---------------------------------------------------------------------------

/// An RAII guard that holds an exclusive lock on a skill-specific lock file.
/// Dropped when it goes out of scope; the OS releases the lock fd.
pub struct SkillLockGuard {
    _file: File,
}

/// Lock directory: `~/.synaps-cli/skills/.locks/`.
fn locks_dir() -> PathBuf {
    skills_root().join(".locks")
}

/// Acquire a fail-fast exclusive lock for the named skill.
/// Uses `~/.synaps-cli/skills/.locks/<name>.lock` — separate from the
/// skill directory so the lock does NOT create the skill dir as a side effect.
pub fn lock_skill(name: &str) -> Result<SkillLockGuard, RuntimeError> {
    let dir = locks_dir();
    fs::create_dir_all(&dir).map_err(|e| {
        RuntimeError::Tool(format!("cannot create locks dir: {e}"))
    })?;

    let lock_path = dir.join(format!("{name}.lock"));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| RuntimeError::Tool(format!("cannot open lock file: {e}")))?;

    let acquired = file.try_lock_exclusive().map_err(|e| {
        RuntimeError::Tool(format!(
            "lock held by another skill_manage in flight ({}): {e}",
            lock_path.display()
        ))
    })?;

    if !acquired {
        return Err(RuntimeError::Tool(format!(
            "lock held by another skill_manage in flight ({})",
            lock_path.display()
        )));
    }

    Ok(SkillLockGuard { _file: file })
}

// ---------------------------------------------------------------------------
// Symlink refusal
// ---------------------------------------------------------------------------

/// Refuse to operate on a skill whose directory (or parent) is a symlink.
/// Prevents write-through to attacker-controlled targets.
fn refuse_symlink(path: &Path) -> Result<(), RuntimeError> {
    match fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => Err(RuntimeError::Tool(format!(
            "refusing to write through symlink: {}",
            path.display()
        ))),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Atomic SKILL.md write
// ---------------------------------------------------------------------------

/// Build SKILL.md content from name, description, and body.
/// Callers must have validated `name` (via `validate_name`) and
/// `description` (via `validate_description`) — otherwise the
/// generated frontmatter could be malformed or injected.
pub fn skill_md_content(name: &str, description: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n{body}")
}

/// Write `SKILL.md` atomically: tmp file → fsync → rename.
/// Creates the skill directory if it doesn't exist.
pub fn write_skill_md_atomic(
    name: &str,
    description: &str,
    body: &str,
) -> Result<(), RuntimeError> {
    // Belt-and-braces — refuse to write garbage even if caller skipped validation.
    validate_name(name).map_err(|e| RuntimeError::Tool(format!("invalid name: {e}")))?;
    validate_description(description)
        .map_err(|e| RuntimeError::Tool(format!("invalid description: {e}")))?;

    let dir = skill_dir(name);

    // Symlink refusal: parent (skills root) and skill dir itself.
    if let Some(parent) = dir.parent() {
        if parent.exists() {
            refuse_symlink(parent)?;
        }
    }
    if dir.exists() {
        refuse_symlink(&dir)?;
    }

    fs::create_dir_all(&dir).map_err(|e| {
        RuntimeError::Tool(format!("cannot create skill dir: {e}"))
    })?;

    let content = skill_md_content(name, description, body);
    let skill_md = dir.join("SKILL.md");
    let tmp_path = dir.join("SKILL.md.tmp");

    // Write to tmp
    let mut f = File::create(&tmp_path).map_err(|e| {
        RuntimeError::Tool(format!("cannot create tmp file: {e}"))
    })?;
    if let Err(e) = f.write_all(content.as_bytes()) {
        let _ = fs::remove_file(&tmp_path);
        return Err(RuntimeError::Tool(format!("cannot write tmp file: {e}")));
    }
    if let Err(e) = f.sync_all() {
        let _ = fs::remove_file(&tmp_path);
        return Err(RuntimeError::Tool(format!("fsync failed: {e}")));
    }
    drop(f);

    // Atomic rename — on failure, clean up the tmp file.
    if let Err(e) = fs::rename(&tmp_path, &skill_md) {
        let _ = fs::remove_file(&tmp_path);
        return Err(RuntimeError::Tool(format!("atomic rename failed: {e}")));
    }

    // Best-effort fsync on parent directory (Linux POSIX). Log on failure
    // — durability isn't guaranteed without it, but it's not fatal.
    if let Err(e) = fsync_dir(&dir) {
        tracing::warn!(target: "skills.writer", "fsync(parent) failed for {}: {e}", dir.display());
    }

    Ok(())
}

/// Read the description from an existing SKILL.md (to preserve it on update).
/// Reuses the loader's frontmatter parser for parity with the read path.
pub fn read_description(name: &str) -> Result<String, RuntimeError> {
    let path = skill_dir(name).join("SKILL.md");
    let content = fs::read_to_string(&path).map_err(|e| {
        RuntimeError::Tool(format!("skill '{}' not found: {e}", name))
    })?;
    let (fields, _) = crate::skills::loader::parse_frontmatter(&content);
    for (k, v) in &fields {
        if k == "description" {
            return Ok(v.clone());
        }
    }
    Err(RuntimeError::Tool(format!(
        "skill '{name}': no 'description' field in frontmatter"
    )))
}

/// Check that a skill exists (SKILL.md present).
pub fn skill_exists(name: &str) -> bool {
    skill_dir(name).join("SKILL.md").exists()
}

// ---------------------------------------------------------------------------
// Archive (delete = move, never hard-delete)
// ---------------------------------------------------------------------------

/// Move `~/.synaps-cli/skills/<name>/` to `.archive/<name>-<timestamp>/`
/// and rename the inner `SKILL.md` → `SKILL.md.archived` so Axel's .md
/// filter skips it. Sub-second timestamp + collision-suffix guard handles
/// fast create→delete→create→delete cycles.
pub fn archive_skill(name: &str) -> Result<(), RuntimeError> {
    let src = skill_dir(name);
    if !src.exists() {
        return Err(RuntimeError::Tool(format!("skill '{name}' not found")));
    }

    fs::create_dir_all(archive_root()).map_err(|e| {
        RuntimeError::Tool(format!("cannot create archive dir: {e}"))
    })?;

    // Sub-second precision avoids same-second collisions.
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ").to_string();
    let base = format!("{name}-{ts}");
    let mut archive_dir = archive_root().join(&base);
    let mut suffix = 1u32;
    while archive_dir.exists() {
        archive_dir = archive_root().join(format!("{base}-{suffix}"));
        suffix += 1;
        if suffix > 1_000 {
            return Err(RuntimeError::Tool(
                "archive target collision: exhausted suffixes".into(),
            ));
        }
    }

    fs::rename(&src, &archive_dir).map_err(|e| {
        RuntimeError::Tool(format!("cannot move skill to archive: {e}"))
    })?;

    // Rename SKILL.md → SKILL.md.archived so Axel's ext filter skips it
    let md = archive_dir.join("SKILL.md");
    let md_archived = archive_dir.join("SKILL.md.archived");
    if md.exists() {
        fs::rename(&md, &md_archived).map_err(|e| {
            RuntimeError::Tool(format!("cannot rename archived skill file: {e}"))
        })?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    let f = File::open(dir)?;
    f.sync_all()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use serial_test::serial;
    use tempfile::TempDir;

    fn set_home(dir: &Path) -> String {
        let old = std::env::var("HOME").unwrap_or_default();
        // SAFETY: tests are single-threaded where env mutation matters
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
    fn validate_name_accepts_valid() {
        for name in &["a", "abc", "a-b-c", "a1", "z9", "foo-bar-baz"] {
            validate_name(name).unwrap_or_else(|e| panic!("rejected '{name}': {e}"));
        }
    }

    #[test]
    fn validate_name_rejects_empty() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn validate_name_rejects_uppercase() {
        assert!(validate_name("Abc").is_err());
        assert!(validate_name("ABC").is_err());
    }

    #[test]
    fn validate_name_rejects_leading_hyphen() {
        assert!(validate_name("-foo").is_err());
    }

    #[test]
    fn validate_name_rejects_path_sep() {
        assert!(validate_name("foo/bar").is_err());
        assert!(validate_name("foo\\bar").is_err());
    }

    #[test]
    fn validate_name_rejects_dots() {
        assert!(validate_name(".").is_err());
        assert!(validate_name("..").is_err());
    }

    #[test]
    fn validate_name_rejects_reserved() {
        assert!(validate_name("con").is_err());
        assert!(validate_name("com1").is_err());
        assert!(validate_name("lpt9").is_err());
        assert!(validate_name("nul").is_err());
    }

    #[test]
    fn validate_name_rejects_too_long() {
        let long = "a".repeat(65);
        assert!(validate_name(&long).is_err());
    }

    #[test]
    fn validate_name_accepts_64_chars() {
        let name = format!("a{}", "b".repeat(63));
        assert_eq!(name.len(), 64);
        validate_name(&name).unwrap();
    }

    #[test]
    fn validate_name_rejects_newlines() {
        assert!(validate_name("a\nb").is_err());
        assert!(validate_name("a\rb").is_err());
    }

    // --- validate_description ------------------------------------------------

    #[test]
    fn validate_description_accepts_normal() {
        validate_description("A nice short description.").unwrap();
    }

    #[test]
    fn validate_description_rejects_empty() {
        assert!(validate_description("").is_err());
    }

    #[test]
    fn validate_description_rejects_over_200_chars() {
        let long: String = "x".repeat(201);
        assert!(validate_description(&long).is_err());
    }

    #[test]
    fn validate_description_counts_chars_not_bytes() {
        // 200 multibyte chars — bytes > 200, chars == 200. Must be ACCEPTED.
        let s: String = "★".repeat(200);
        assert!(s.len() > 200, "byte len should exceed 200");
        validate_description(&s).unwrap();
    }

    #[test]
    fn validate_description_rejects_newlines() {
        assert!(validate_description("hello\nworld").is_err());
        assert!(validate_description("hello\rworld").is_err());
    }

    #[test]
    fn validate_description_rejects_whitespace_edges() {
        assert!(validate_description(" leading").is_err());
        assert!(validate_description("trailing ").is_err());
    }

    #[test]
    fn validate_description_rejects_frontmatter_delimiter() {
        assert!(validate_description("evil---thing").is_err());
        assert!(validate_description("---").is_err());
    }

    // --- frontmatter injection (B1) ------------------------------------------

    #[test]
    #[serial]
    fn write_rejects_injected_description() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());
        let result = write_skill_md_atomic(
            "fm-inject",
            "evil\n---\nbody-pwn",
            "# body",
        );
        let no_dir = !tmp.path().join(".synaps-cli/skills/fm-inject").exists();
        restore_home(&old);
        assert!(result.is_err(), "injection must be refused");
        assert!(no_dir, "no skill dir should have been created");
    }

    #[test]
    #[serial]
    fn write_roundtrips_valid_description() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());
        write_skill_md_atomic("rt", "A clean description.", "# body").unwrap();
        let desc = read_description("rt").unwrap();
        restore_home(&old);
        assert_eq!(desc, "A clean description.");
    }

    // --- archive collision (B2) ---------------------------------------------

    #[test]
    #[serial]
    fn archive_skill_creates_distinct_dirs_in_tight_loop() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());

        for _ in 0..2 {
            write_skill_md_atomic("collide", "desc", "body").unwrap();
            archive_skill("collide").unwrap();
        }

        let archive_base = tmp.path().join(".synaps-cli/skills/.archive");
        let entries: Vec<_> = fs::read_dir(&archive_base).unwrap().flatten().collect();
        restore_home(&old);

        assert_eq!(entries.len(), 2, "expected two distinct archive dirs");
        let names: Vec<String> = entries
            .iter()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_ne!(names[0], names[1], "archive dirs must be distinct");
    }

    // --- plugin guard (H1) ---------------------------------------------------

    #[test]
    #[serial]
    fn ensure_writable_allows_home_with_plugins_substring() {
        // HOME contains the substring "plugins" but the skill lives under
        // ~/.synaps-cli/skills/ — must NOT be refused by a substring check.
        let parent = TempDir::new().unwrap();
        let home = parent.path().join("home-with-plugins-in-name");
        std::fs::create_dir_all(&home).unwrap();
        let old = set_home(&home);

        // Pre-create the loose skill so it can be canonicalized.
        write_skill_md_atomic("looseone", "d", "body").unwrap();

        // Build a registry entry pointing at this loose skill.
        use crate::skills::LoadedSkill;
        let src = home
            .join(".synaps-cli/skills/looseone/SKILL.md")
            .canonicalize()
            .unwrap();
        let base = src.parent().unwrap().to_path_buf();
        let registry = crate::skills::registry::CommandRegistry::new(
            &[],
            vec![LoadedSkill {
                name: "looseone".into(),
                description: "d".into(),
                body: "b".into(),
                plugin: None,
                base_dir: base,
                source_path: src,
            }],
        );

        let res = ensure_writable(&registry, "looseone");
        restore_home(&old);
        res.unwrap();
    }

    #[test]
    #[serial]
    fn ensure_writable_refuses_path_under_real_plugin_root() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());

        // Create a plugin-style path under HOME that is OUTSIDE skills_root.
        let plug_src_dir = tmp.path().join(".synaps-cli/plugins/p1/skills/foo");
        std::fs::create_dir_all(&plug_src_dir).unwrap();
        std::fs::write(plug_src_dir.join("SKILL.md"), "---\nname: foo\ndescription: d\n---\nb").unwrap();
        let plug_src = plug_src_dir.join("SKILL.md").canonicalize().unwrap();

        use crate::skills::LoadedSkill;
        let registry = crate::skills::registry::CommandRegistry::new(
            &[],
            vec![LoadedSkill {
                name: "foo".into(),
                description: "d".into(),
                body: "b".into(),
                plugin: None, // even without plugin marker, path-based guard catches it
                base_dir: plug_src.parent().unwrap().to_path_buf(),
                source_path: plug_src,
            }],
        );

        let res = ensure_writable(&registry, "foo");
        restore_home(&old);
        assert!(res.is_err(), "skill outside loose root must be refused");
    }

    // --- tmp cleanup (H2) ----------------------------------------------------

    #[test]
    #[serial]
    fn rename_failure_cleans_tmp_file() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());

        // Pre-create a DIRECTORY at the SKILL.md target path so fs::rename
        // (renaming a regular file onto an existing non-empty dir) fails.
        let dir = skill_dir("rmtmp");
        std::fs::create_dir_all(dir.join("SKILL.md")).unwrap();
        // Put a sentinel file inside the target dir so rename can't succeed.
        std::fs::write(dir.join("SKILL.md").join("sentinel"), b"x").unwrap();

        let result = write_skill_md_atomic("rmtmp", "d", "body");
        let tmp_path = dir.join("SKILL.md.tmp");
        let has_tmp = tmp_path.exists();
        restore_home(&old);

        assert!(result.is_err(), "rename onto a non-empty dir must fail");
        assert!(!has_tmp, ".tmp file must be cleaned up on rename failure");
    }

    // --- symlink refusal (H3) ------------------------------------------------

    #[cfg(unix)]
    #[test]
    #[serial]
    fn refuse_symlink_skill_dir() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());

        let skills = tmp.path().join(".synaps-cli/skills");
        std::fs::create_dir_all(&skills).unwrap();
        let target = tmp.path().join("evil-target");
        std::fs::create_dir_all(&target).unwrap();
        symlink(&target, skills.join("sneaky")).unwrap();

        let result = write_skill_md_atomic("sneaky", "d", "body");
        let wrote_through =
            target.join("SKILL.md").exists() || target.join("SKILL.md.tmp").exists();
        restore_home(&old);

        assert!(result.is_err(), "must refuse symlinked skill dir");
        assert!(!wrote_through, "must NOT write through symlink");
    }

    // --- pre-existing tests --------------------------------------------------

    #[test]
    #[serial]
    fn write_skill_md_atomic_creates_file() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());
        let result = write_skill_md_atomic("test-skill", "A test skill", "# Body\nContent here");
        restore_home(&old);
        result.unwrap();

        let expected = tmp
            .path()
            .join(".synaps-cli/skills/test-skill/SKILL.md");
        assert!(expected.exists(), "SKILL.md not created");
        let content = fs::read_to_string(&expected).unwrap();
        assert!(content.contains("name: test-skill"));
        assert!(content.contains("description: A test skill"));
        assert!(content.contains("# Body"));
    }

    #[test]
    #[serial]
    fn write_skill_md_atomic_no_tmp_left_behind() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());
        write_skill_md_atomic("no-tmp", "desc", "body").unwrap();
        restore_home(&old);

        let tmp_path = tmp
            .path()
            .join(".synaps-cli/skills/no-tmp/SKILL.md.tmp");
        assert!(!tmp_path.exists(), ".tmp should not remain after successful write");
    }

    #[test]
    #[serial]
    fn archive_skill_moves_dir_and_renames_md() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());

        write_skill_md_atomic("to-archive", "desc", "body").unwrap();
        let original = tmp.path().join(".synaps-cli/skills/to-archive");
        assert!(original.exists());

        archive_skill("to-archive").unwrap();
        restore_home(&old);

        assert!(!original.exists(), "skill dir should be gone after archive");

        let archive_base = tmp.path().join(".synaps-cli/skills/.archive");
        let entries: Vec<_> = fs::read_dir(&archive_base)
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one archive entry");
        let archived_dir = &entries[0].path();

        assert!(archived_dir.join("SKILL.md.archived").exists());
        assert!(!archived_dir.join("SKILL.md").exists());

        let md_files: Vec<_> = walkdir_md(&archive_base);
        assert!(md_files.is_empty(), "no .md files under .archive: {md_files:?}");
    }

    #[test]
    #[serial]
    fn archive_skill_missing_returns_error() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());
        let result = archive_skill("nonexistent");
        restore_home(&old);
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn lock_skill_fail_fast_on_double_lock() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());

        let _guard = lock_skill("double-lock").unwrap();
        let result = lock_skill("double-lock");
        restore_home(&old);

        assert!(result.is_err(), "second lock on same skill should fail fast");
    }

    fn walkdir_md(dir: &Path) -> Vec<PathBuf> {
        let mut out = vec![];
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    out.extend(walkdir_md(&p));
                } else if p.extension().and_then(|e| e.to_str()) == Some("md") {
                    out.push(p);
                }
            }
        }
        out
    }
}
