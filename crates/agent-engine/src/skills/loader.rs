//! SKILL.md parsing, {baseDir}/${CLAUDE_PLUGIN_ROOT} substitution, and discovery.
//!
//! Task 21 (spec §7.6) — LAZY skill bodies: boot discovery reads ONLY the
//! bounded frontmatter (never one body byte), records an immutable source
//! fingerprint, and defers body read/substitution/validation to exact
//! selection, which re-verifies the fingerprint before returning content.

use crate::skills::{LoadedSkill, SkillSource};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Hard bound on the boot-time frontmatter scan: the scan reads through
/// the closing `---` delimiter and never past this many bytes.
pub const SKILL_FRONTMATTER_MAX_BYTES: usize = 8 * 1024;
/// Hard cap on a SKILL.md regular file; larger files are skipped at
/// discovery (stat only) and refused at selection.
pub const SKILL_FILE_MAX_BYTES: usize = 1024 * 1024;
/// Bounded retained skill name.
pub const SKILL_NAME_MAX_BYTES: usize = 128;
/// Bounded retained skill description: the retained copy is a BOUNDED
/// PROJECTION (UTF-8-safe truncation via `BoundedText`) used only for
/// display/search; selection verification always uses the exact
/// frontmatter byte digest, never this projection.
pub const SKILL_DESCRIPTION_MAX_BYTES: usize = 1024;
/// Deterministic cap on discovered skills (first-wins across roots).
pub const DISCOVERY_MAX_SKILLS: usize = 2048;
/// Deterministic cap on total retained metadata bytes (names +
/// descriptions + paths) across the whole discovery pass.
pub const DISCOVERY_MAX_METADATA_BYTES: usize = 4 * 1024 * 1024;

/// Immutable identity of the exact regular file the frontmatter was
/// scanned from, captured via fstat on the OPEN handle (no TOCTOU window
/// against the scanned bytes). Selection re-opens, re-stats, re-scans,
/// and requires every recorded value to match before any body byte is
/// trusted. Never rendered into errors or logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillFingerprint {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) len: u64,
    pub(crate) mtime: (i64, i64),
    pub(crate) ctime: (i64, i64),
    /// SHA-256 of the exact frontmatter bytes (through the closing
    /// delimiter line, i.e. bytes `[0, body_start)`).
    pub(crate) frontmatter_sha256: [u8; 32],
    /// Byte offset where the body begins (== bytes read by the scan).
    pub(crate) body_start: u64,
}

fn fingerprint_of(
    meta: &std::fs::Metadata,
    frontmatter: &[u8],
    body_start: u64,
) -> SkillFingerprint {
    use sha2::{Digest as _, Sha256};
    #[cfg(unix)]
    let (dev, ino, mtime, ctime) = {
        use std::os::unix::fs::MetadataExt as _;
        (
            meta.dev(),
            meta.ino(),
            (meta.mtime(), meta.mtime_nsec()),
            (meta.ctime(), meta.ctime_nsec()),
        )
    };
    #[cfg(not(unix))]
    let (dev, ino, mtime, ctime) = (0u64, 0u64, (0i64, 0i64), (0i64, 0i64));
    let digest = Sha256::digest(frontmatter);
    let mut frontmatter_sha256 = [0u8; 32];
    frontmatter_sha256.copy_from_slice(&digest);
    SkillFingerprint {
        dev,
        ino,
        len: meta.len(),
        mtime,
        ctime,
        frontmatter_sha256,
        body_start,
    }
}

/// Open a SKILL.md for bounded reading: regular files only, symlinks
/// refused (O_NOFOLLOW), non-blocking so a FIFO swapped in place can
/// never hang the caller (O_NONBLOCK; regular-file reads ignore it).
fn open_skill_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    if meta.len() > SKILL_FILE_MAX_BYTES as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file exceeds the skill size cap",
        ));
    }
    Ok(file)
}

/// Bytewise bounded scan of the frontmatter block on an OPEN file:
/// reads exactly through the closing `---` delimiter line (never past it,
/// never more than [`SKILL_FRONTMATTER_MAX_BYTES`]) and returns the raw
/// frontmatter bytes (including both delimiter lines). Zero body bytes
/// are consumed from the handle.
fn scan_frontmatter_bytes(file: &mut std::fs::File) -> std::io::Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    let mut line_start = 0usize;
    let mut delimiters = 0usize;
    loop {
        if buf.len() >= SKILL_FRONTMATTER_MAX_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "frontmatter exceeds the scan bound",
            ));
        }
        let n = file.read(&mut byte)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "frontmatter not closed",
            ));
        }
        buf.push(byte[0]);
        if byte[0] == b'\n' {
            let line = &buf[line_start..buf.len() - 1];
            let line = if line.last() == Some(&b'\r') {
                &line[..line.len() - 1]
            } else {
                line
            };
            if line == b"---" {
                delimiters += 1;
                if delimiters == 1 && line_start != 0 {
                    // Opening delimiter must be the very first line.
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "missing opening frontmatter delimiter",
                    ));
                }
                if delimiters == 2 {
                    return Ok(buf);
                }
            } else if delimiters == 0 {
                // First line was not `---`: no frontmatter.
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "missing opening frontmatter delimiter",
                ));
            }
            line_start = buf.len();
        }
    }
}

/// Parse `key: value` fields from raw frontmatter bytes (both delimiter
/// lines included). UTF-8 required for the frontmatter itself.
fn parse_frontmatter_fields(frontmatter: &[u8]) -> Option<Vec<(String, String)>> {
    let text = std::str::from_utf8(frontmatter).ok()?;
    let mut lines: Vec<&str> = text.lines().collect();
    // Drop the delimiter lines.
    if lines.first().map(|l| l.trim()) != Some("---") {
        return None;
    }
    lines.remove(0);
    while let Some(last) = lines.last() {
        if last.trim() == "---" {
            lines.pop();
            break;
        }
        lines.pop();
    }
    Some(
        lines
            .into_iter()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() {
                    return None;
                }
                let (k, v) = line.split_once(':')?;
                Some((k.trim().to_string(), v.trim().trim_matches('"').to_string()))
            })
            .collect(),
    )
}

/// Discover a SKILL.md into a LAZY `LoadedSkill` (Task 21): bounded
/// frontmatter scan ONLY — zero body bytes read, no substitution. The
/// open-handle fstat fingerprint (device/inode/size/mtime/ctime +
/// frontmatter digest + body offset) is recorded so selection can verify
/// the exact same content before trusting one body byte.
///
/// Returns None if the file is not a safe regular file within bounds, the
/// frontmatter is missing/unclosed/malformed, name/description are
/// absent, or there is no body byte after the closing delimiter.
pub fn load_skill_file(
    skill_md: &Path,
    plugin: Option<&str>,
    plugin_root: Option<&Path>,
) -> Option<LoadedSkill> {
    let mut file = open_skill_file(skill_md).ok()?;
    let frontmatter = scan_frontmatter_bytes(&mut file).ok()?;
    let body_start = frontmatter.len() as u64;
    let meta = file.metadata().ok()?;
    if meta.len() <= body_start {
        return None; // empty body
    }
    let fields = parse_frontmatter_fields(&frontmatter)?;
    let name = fields
        .iter()
        .find(|(k, _)| k == "name")
        .map(|(_, v)| v.clone())?;
    let description = fields
        .iter()
        .find(|(k, _)| k == "description")
        .map(|(_, v)| v.clone())?;
    // Bounded identity metadata (Task 21): non-empty, control-free name up
    // to SKILL_NAME_MAX_BYTES; non-empty description bounded to
    // SKILL_DESCRIPTION_MAX_BYTES (retained truncated; verification uses
    // the exact frontmatter digest, not this bounded copy).
    if name.is_empty()
        || name.len() > SKILL_NAME_MAX_BYTES
        || name.chars().any(char::is_control)
        || description.is_empty()
        || description.chars().any(char::is_control)
    {
        return None;
    }
    let description = agent_core::BoundedText::new(&description, SKILL_DESCRIPTION_MAX_BYTES).text;
    let base_dir = skill_md.parent()?.canonicalize().ok()?;
    let plugin_root = match plugin_root {
        Some(root) => Some(root.canonicalize().ok()?),
        None => None,
    };
    Some(LoadedSkill {
        name,
        description,
        source: SkillSource::Lazy {
            fingerprint: fingerprint_of(&meta, &frontmatter, body_start),
            plugin_root,
        },
        plugin: plugin.map(str::to_string),
        base_dir,
        source_path: skill_md.canonicalize().ok()?,
    })
}

/// Static selection-time failures. Deliberately free of paths, hashes,
/// and file content.
const ERR_SOURCE_CHANGED: &str =
    "skill source changed since discovery (fingerprint verification failed); reload skills and retry";
const ERR_SOURCE_UNREADABLE: &str =
    "skill source is no longer readable as a bounded regular file; reload skills and retry";
const ERR_BODY_NOT_UTF8: &str = "skill body is not valid UTF-8";

/// Selection result: the verified, substituted body plus the SHA-256 of
/// the exact verified on-disk snapshot. No Debug — the digest and body
/// are never rendered by accident.
pub(crate) struct VerifiedSkillBody {
    pub(crate) body: String,
    /// Typed local evidence only: consumed by verification unit tests;
    /// production callers deliberately never render or persist it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) snapshot_sha256: [u8; 32],
}

impl LoadedSkill {
    /// Read, VERIFY, substitute, and return the body of this exact skill.
    /// Called only at selection (`load_skill` tool or slash command) —
    /// never during boot, search, or diagnostics.
    ///
    /// Verification: reopen (regular file only, O_NOFOLLOW|O_NONBLOCK),
    /// fstat must equal the recorded fingerprint, the re-scanned
    /// frontmatter bytes must digest to the recorded SHA-256, and a second
    /// fstat AFTER the body read must still match — any drift fails
    /// closed with a static, path-free reason.
    pub fn load_body(&self) -> Result<String, &'static str> {
        self.load_body_with_evidence().map(|verified| verified.body)
    }

    /// As [`Self::load_body`], plus the typed SHA-256 of the exact
    /// verified snapshot (frontmatter + body bytes) as local evidence.
    /// Crate-internal; the digest is never logged or exposed and never
    /// substitutes for the immutable fingerprint authority.
    pub(crate) fn load_body_with_evidence(&self) -> Result<VerifiedSkillBody, &'static str> {
        let (fingerprint, plugin_root) = match &self.source {
            SkillSource::Inline(body) => {
                use sha2::{Digest as _, Sha256};
                return Ok(VerifiedSkillBody {
                    snapshot_sha256: Sha256::digest(body.as_bytes()).into(),
                    body: body.clone(),
                });
            }
            SkillSource::Lazy {
                fingerprint,
                plugin_root,
            } => (fingerprint, plugin_root.as_deref()),
        };
        let mut file = open_skill_file(&self.source_path).map_err(|_| ERR_SOURCE_UNREADABLE)?;
        let meta = file.metadata().map_err(|_| ERR_SOURCE_UNREADABLE)?;
        let frontmatter = scan_frontmatter_bytes(&mut file).map_err(|_| ERR_SOURCE_UNREADABLE)?;
        if fingerprint_of(&meta, &frontmatter, frontmatter.len() as u64) != *fingerprint {
            return Err(ERR_SOURCE_CHANGED);
        }
        // Bounded body read: the pre-verified length is under the file
        // cap; read exactly the remaining bytes.
        let remaining = (fingerprint.len - fingerprint.body_start) as usize;
        let mut body_bytes = vec![0u8; remaining];
        std::io::Read::read_exact(&mut file, &mut body_bytes).map_err(|_| ERR_SOURCE_CHANGED)?;
        // No trailing bytes may exist beyond the recorded length.
        let mut probe = [0u8; 1];
        match std::io::Read::read(&mut file, &mut probe) {
            Ok(0) => {}
            _ => return Err(ERR_SOURCE_CHANGED),
        }
        // Post-read fstat: the inode must not have changed underneath us.
        let post = file.metadata().map_err(|_| ERR_SOURCE_UNREADABLE)?;
        if fingerprint_of(&post, &frontmatter, fingerprint.body_start) != *fingerprint {
            return Err(ERR_SOURCE_CHANGED);
        }
        // Honest evidence: SHA-256 over the exact verified snapshot
        // (frontmatter + body bytes). Typed local value only — never
        // logged, never exposed, never a substitute for the immutable
        // fingerprint authority above.
        let snapshot_sha256: [u8; 32] = {
            use sha2::{Digest as _, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(&frontmatter);
            hasher.update(&body_bytes);
            hasher.finalize().into()
        };
        let body = String::from_utf8(body_bytes).map_err(|_| ERR_BODY_NOT_UTF8)?;
        let body = body.trim().to_string();
        if body.is_empty() {
            return Err(ERR_SOURCE_CHANGED);
        }
        // Substitutions happen HERE, at selection time only.
        let base = self.base_dir.to_str().ok_or(ERR_SOURCE_UNREADABLE)?;
        let mut body = body.replace("{baseDir}", base);
        if let Some(root) = plugin_root {
            let root = root.to_str().ok_or(ERR_SOURCE_UNREADABLE)?;
            body = body.replace("${CLAUDE_PLUGIN_ROOT}", root);
            body = body.replace("$CLAUDE_PLUGIN_ROOT", root);
        }
        Ok(VerifiedSkillBody {
            body,
            snapshot_sha256,
        })
    }
}

use crate::skills::{
    manifest::{MarketplaceManifest, PluginManifest},
    Plugin,
};

/// The four default discovery roots, in priority order (local first, global second).
pub fn default_roots() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from(".synaps-cli/plugins"),
        PathBuf::from(".synaps-cli/skills"),
    ];
    let home_plugins = crate::config::resolve_read_path_extended("plugins");
    let home_skills = crate::config::resolve_read_path_extended("skills");
    roots.push(home_plugins);
    roots.push(home_skills);
    roots
}

/// Walk the given roots and discover all plugins and skills.
/// Deduplicates on (plugin_name, skill_name); first occurrence wins.
///
/// Task 21: discovery is deterministically CAPPED — at most
/// [`DISCOVERY_MAX_SKILLS`] skills and [`DISCOVERY_MAX_METADATA_BYTES`]
/// of retained metadata across the whole pass. Beyond a cap, later
/// candidates are skipped (first-wins order preserved) and only COUNTS
/// are reported.
pub fn load_all(roots: &[PathBuf]) -> (Vec<Plugin>, Vec<LoadedSkill>) {
    let mut plugins: Vec<Plugin> = Vec::new();
    let mut skills: Vec<LoadedSkill> = Vec::new();
    let mut seen: std::collections::HashSet<(Option<String>, String)> =
        std::collections::HashSet::new();
    let mut budget = DiscoveryBudget::default();

    for root in roots {
        walk_root(root, &mut plugins, &mut skills, &mut seen, &mut budget);
    }
    if budget.skipped > 0 {
        tracing::warn!(
            skipped = budget.skipped,
            "skill discovery caps reached; later candidates skipped (count only)"
        );
    }
    (plugins, skills)
}

/// Running discovery caps. Metadata cost counts the bounded retained
/// strings (name, description, paths).
#[derive(Default)]
struct DiscoveryBudget {
    metadata_bytes: usize,
    skipped: usize,
}

impl DiscoveryBudget {
    /// Admit one discovered skill if both caps hold; otherwise count it
    /// as skipped. `current` is the number of skills already retained.
    fn admit(&mut self, current: usize, skill: &LoadedSkill) -> bool {
        let cost = skill.name.len()
            + skill.description.len()
            + skill.plugin.as_ref().map(String::len).unwrap_or(0)
            + skill.base_dir.as_os_str().len()
            + skill.source_path.as_os_str().len();
        if current >= DISCOVERY_MAX_SKILLS
            || self.metadata_bytes.saturating_add(cost) > DISCOVERY_MAX_METADATA_BYTES
        {
            self.skipped += 1;
            return false;
        }
        self.metadata_bytes += cost;
        true
    }
}

/// Return the first existing path from a list of candidates, or None.
/// Used to accept both `.synaps-plugin/` (native) and `.claude-plugin/`
/// (Claude Code compat) manifest directories.
fn first_existing(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|p| p.exists()).cloned()
}

fn marketplace_json_for(root: &Path) -> Option<PathBuf> {
    first_existing(&[
        root.join(".synaps-plugin").join("marketplace.json"),
        root.join(".claude-plugin").join("marketplace.json"),
    ])
}

fn plugin_json_for(plugin_root: &Path) -> Option<PathBuf> {
    first_existing(&[
        plugin_root.join(".synaps-plugin").join("plugin.json"),
        plugin_root.join(".claude-plugin").join("plugin.json"),
    ])
}

/// Deterministic directory listing: `read_dir` order is filesystem-
/// dependent, so every first-wins/cap decision sorts entries by file
/// name first (Task 21 determinism).
fn sorted_dir_entries(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    paths
}

fn walk_root(
    root: &Path,
    plugins: &mut Vec<Plugin>,
    skills: &mut Vec<LoadedSkill>,
    seen: &mut std::collections::HashSet<(Option<String>, String)>,
    budget: &mut DiscoveryBudget,
) {
    if !root.exists() {
        return;
    }

    // 1. Marketplace pass
    let marketplace_name = if let Some(marketplace_json) = marketplace_json_for(root) {
        match std::fs::read_to_string(&marketplace_json)
            .ok()
            .and_then(|c| serde_json::from_str::<MarketplaceManifest>(&c).ok())
        {
            Some(m) => {
                for entry in &m.plugins {
                    let Some(source) = entry.source.as_ref() else {
                        continue;
                    };
                    let plugin_root = root.join(source);
                    load_plugin(&plugin_root, Some(&m.name), plugins, skills, seen, budget);
                }
                Some(m.name)
            }
            None => {
                tracing::warn!("failed to parse {}", marketplace_json.display());
                None
            }
        }
    } else {
        None
    };

    // 2. Plugin pass (subdirs with .synaps-plugin/plugin.json or .claude-plugin/plugin.json)
    //    Additionally, if a subdir contains a marketplace.json, treat it as a
    //    nested discovery root and recurse once. This supports the common
    //    "clone marketplace repo into plugins/" install pattern.
    for path in sorted_dir_entries(root) {
        if !path.is_dir() {
            continue;
        }
        if marketplace_json_for(&path).is_some() {
            walk_root(&path, plugins, skills, seen, budget);
        } else if plugin_json_for(&path).is_some() {
            load_plugin(
                &path,
                marketplace_name.as_deref(),
                plugins,
                skills,
                seen,
                budget,
            );
        }
    }

    // 3. Loose-skill pass — scan both root/ and root/skills/ for <name>/SKILL.md
    for loose_dir in [root.to_path_buf(), root.join("skills")] {
        if !loose_dir.is_dir() {
            continue;
        }
        for path in sorted_dir_entries(&loose_dir) {
            if !path.is_dir() {
                continue;
            }
            let skill_md = path.join("SKILL.md");
            if skill_md.exists() {
                if let Some(s) = load_skill_file(&skill_md, None, None) {
                    let key = (None, s.name.clone());
                    if seen.contains(&key) || !budget.admit(skills.len(), &s) {
                        continue;
                    }
                    seen.insert(key);
                    skills.push(s);
                }
            }
        }
    }
}

fn load_plugin(
    plugin_root: &Path,
    marketplace: Option<&str>,
    plugins: &mut Vec<Plugin>,
    skills: &mut Vec<LoadedSkill>,
    seen: &mut std::collections::HashSet<(Option<String>, String)>,
    budget: &mut DiscoveryBudget,
) {
    let Some(manifest_path) = plugin_json_for(plugin_root) else {
        tracing::warn!("no plugin.json under {}", plugin_root.display());
        return;
    };
    let Ok(content) = std::fs::read_to_string(&manifest_path) else {
        tracing::warn!("failed to read {}", manifest_path.display());
        return;
    };
    let Ok(m): Result<PluginManifest, _> = serde_json::from_str(&content) else {
        tracing::warn!("failed to parse {}", manifest_path.display());
        return;
    };

    let Ok(root_abs) = plugin_root.canonicalize() else {
        return;
    };
    if plugins.iter().any(|p| p.root == root_abs) {
        return;
    }
    plugins.push(Plugin {
        name: m.name.clone(),
        root: root_abs,
        marketplace: marketplace.map(str::to_string),
        version: m.version.clone(),
        description: m.description.clone(),
        extension: m.extension.clone(),
        manifest: Some(m.clone()),
    });

    let skills_dir = plugin_root.join("skills");
    if !skills_dir.is_dir() {
        return;
    }
    for path in sorted_dir_entries(&skills_dir) {
        if !path.is_dir() {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }
        if let Some(s) = load_skill_file(&skill_md, Some(&m.name), Some(plugin_root)) {
            let key = (Some(m.name.clone()), s.name.clone());
            if seen.contains(&key) || !budget.admit(skills.len(), &s) {
                continue;
            }
            seen.insert(key);
            skills.push(s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_skill(dir: &Path, content: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join("SKILL.md");
        fs::write(&path, content).unwrap();
        path
    }

    /// The bounded scan consumes EXACTLY through the closing delimiter
    /// line: the stream position equals the frontmatter byte length, so
    /// zero body bytes were pulled from the handle.
    #[test]
    fn frontmatter_scan_stops_exactly_at_body_offset() {
        let tmp = tempdir();
        let path = write_skill(
            &tmp.join("scan"),
            "---\nname: s\ndescription: d\n---\nBODY_AFTER",
        );
        let mut file = open_skill_file(&path).unwrap();
        let frontmatter = scan_frontmatter_bytes(&mut file).unwrap();
        let pos = std::io::Seek::stream_position(&mut file).unwrap();
        assert_eq!(frontmatter.len() as u64, pos, "no body byte consumed");
        let full = fs::read(&path).unwrap();
        assert_eq!(&full[..frontmatter.len()], &frontmatter[..]);
        let body_pos = full.windows(10).position(|w| w == b"BODY_AFTER").unwrap();
        assert_eq!(frontmatter.len(), body_pos, "scan ends where body begins");
    }

    /// CRLF line endings scan and parse identically to LF.
    #[test]
    fn frontmatter_scan_accepts_crlf() {
        let tmp = tempdir();
        let path = write_skill(
            &tmp.join("crlf"),
            "---\r\nname: s\r\ndescription: d\r\n---\r\nBody",
        );
        let s = load_skill_file(&path, None, None).unwrap();
        assert_eq!(s.name, "s");
        assert_eq!(s.load_body().unwrap(), "Body");
    }

    /// A closing delimiter line must be exactly `---`: a `----` (or any
    /// prefixed spelling) does NOT close the frontmatter.
    #[test]
    fn frontmatter_rejects_dash_prefix_as_delimiter() {
        let tmp = tempdir();
        let path = write_skill(
            &tmp.join("dashes"),
            "---\nname: s\ndescription: d\n----\nBody",
        );
        assert!(
            load_skill_file(&path, None, None).is_none(),
            "'----' must not close frontmatter"
        );
    }

    /// A file that ends right at the closing delimiter (with the trailing
    /// newline, empty body) is skipped; without the trailing newline the
    /// frontmatter never closes and is also skipped.
    #[test]
    fn frontmatter_trailing_newline_variants_fail_closed_on_empty_body() {
        let tmp = tempdir();
        let with_nl = write_skill(&tmp.join("with-nl"), "---\nname: s\ndescription: d\n---\n");
        assert!(
            load_skill_file(&with_nl, None, None).is_none(),
            "empty body"
        );
        let without_nl = write_skill(&tmp.join("no-nl"), "---\nname: s\ndescription: d\n---");
        assert!(
            load_skill_file(&without_nl, None, None).is_none(),
            "unterminated closing delimiter line never closes"
        );
    }

    /// Bounded identity metadata: oversized/control names and empty or
    /// control descriptions are refused; long descriptions are retained
    /// TRUNCATED while the exact frontmatter digest stays authoritative
    /// (the skill still loads and verifies).
    #[test]
    fn discovery_bounds_name_and_description() {
        let tmp = tempdir();
        let long_name = "n".repeat(SKILL_NAME_MAX_BYTES + 1);
        let over = write_skill(
            &tmp.join("over-name"),
            &format!("---\nname: {long_name}\ndescription: d\n---\nBody"),
        );
        assert!(load_skill_file(&over, None, None).is_none());
        let ctrl = write_skill(
            &tmp.join("ctrl-name"),
            "---\nname: bad\u{7}name\ndescription: d\n---\nBody",
        );
        assert!(load_skill_file(&ctrl, None, None).is_none());
        let no_desc = write_skill(
            &tmp.join("empty-desc"),
            "---\nname: x\ndescription:\n---\nBody",
        );
        assert!(load_skill_file(&no_desc, None, None).is_none());
        let ctrl_desc = write_skill(
            &tmp.join("ctrl-desc"),
            "---\nname: x\ndescription: bad\u{7}desc\n---\nBody",
        );
        assert!(
            load_skill_file(&ctrl_desc, None, None).is_none(),
            "control characters in description are refused"
        );

        let long_desc = "d".repeat(SKILL_DESCRIPTION_MAX_BYTES + 100);
        let truncated = write_skill(
            &tmp.join("long-desc"),
            &format!("---\nname: x\ndescription: {long_desc}\n---\nBody"),
        );
        let s = load_skill_file(&truncated, None, None).unwrap();
        assert!(s.description.len() <= SKILL_DESCRIPTION_MAX_BYTES);
        // Exact digest verification is unaffected by the truncated copy.
        assert_eq!(s.load_body().unwrap(), "Body");
    }

    /// Selection evidence: the typed snapshot digest equals the SHA-256
    /// of the exact on-disk bytes, and the struct exposes no Debug.
    #[test]
    fn verified_body_carries_exact_snapshot_digest() {
        use sha2::{Digest as _, Sha256};
        let tmp = tempdir();
        let path = write_skill(&tmp.join("ev"), "---\nname: e\ndescription: d\n---\nBody");
        let s = load_skill_file(&path, None, None).unwrap();
        let verified = s.load_body_with_evidence().unwrap();
        let expected: [u8; 32] = Sha256::digest(fs::read(&path).unwrap()).into();
        assert_eq!(verified.snapshot_sha256, expected);
        assert_eq!(verified.body, "Body");
    }

    /// Malformed openings fail closed: content before the first `---`,
    /// or no frontmatter at all.
    #[test]
    fn frontmatter_requires_opening_delimiter_first_line() {
        let tmp = tempdir();
        let late = write_skill(&tmp.join("late"), "intro\n---\nname: s\n---\nBody");
        assert!(load_skill_file(&late, None, None).is_none());
        let none = write_skill(&tmp.join("none"), "just a body");
        assert!(load_skill_file(&none, None, None).is_none());
    }

    #[test]
    fn load_skill_basic() {
        let tmp = tempdir();
        let skill_dir = tmp.join("my-skill");
        let path = write_skill(
            &skill_dir,
            "---\nname: my-skill\ndescription: desc\n---\nBody",
        );
        let s = load_skill_file(&path, Some("plugin-x"), None).unwrap();
        assert_eq!(s.name, "my-skill");
        assert_eq!(s.description, "desc");
        assert_eq!(s.load_body().unwrap(), "Body");
        assert_eq!(s.plugin.as_deref(), Some("plugin-x"));
        assert!(s.base_dir.is_absolute());
    }

    #[test]
    fn load_skill_basedir_substitution() {
        let tmp = tempdir();
        let skill_dir = tmp.join("skill");
        let path = write_skill(
            &skill_dir,
            "---\nname: s\ndescription: d\n---\nRun {baseDir}/x.js",
        );
        let s = load_skill_file(&path, None, None).unwrap();
        let expected = format!("Run {}/x.js", s.base_dir.to_str().unwrap());
        assert_eq!(s.load_body().unwrap(), expected);
    }

    #[test]
    fn load_skill_missing_frontmatter_returns_none() {
        let tmp = tempdir();
        let skill_dir = tmp.join("bad");
        let path = write_skill(&skill_dir, "no frontmatter here");
        assert!(load_skill_file(&path, None, None).is_none());
    }

    #[test]
    fn load_skill_missing_description_returns_none() {
        let tmp = tempdir();
        let skill_dir = tmp.join("bad2");
        let path = write_skill(&skill_dir, "---\nname: x\n---\nbody");
        assert!(load_skill_file(&path, None, None).is_none());
    }

    #[test]
    fn load_skill_missing_name_returns_none() {
        let tmp = tempdir();
        let skill_dir = tmp.join("bad3");
        let path = write_skill(&skill_dir, "---\ndescription: d\n---\nbody");
        assert!(load_skill_file(&path, None, None).is_none());
    }

    #[test]
    fn load_skill_empty_body_returns_none() {
        let tmp = tempdir();
        let skill_dir = tmp.join("empty-body");
        let path = write_skill(&skill_dir, "---\nname: x\ndescription: d\n---\n");
        assert!(load_skill_file(&path, None, None).is_none());
    }

    #[test]
    fn load_skill_unclosed_frontmatter_returns_none() {
        let tmp = tempdir();
        let skill_dir = tmp.join("unclosed");
        // No closing `---`; parse_frontmatter returns ([], full_text) so name/description missing → None.
        let path = write_skill(
            &skill_dir,
            "---\nname: x\ndescription: d\nbody without closing fence",
        );
        assert!(load_skill_file(&path, None, None).is_none());
    }

    #[test]
    fn load_skill_basedir_multiple_occurrences() {
        let tmp = tempdir();
        let skill_dir = tmp.join("multi");
        let path = write_skill(
            &skill_dir,
            "---\nname: m\ndescription: d\n---\n{baseDir}/a and {baseDir}/b",
        );
        let s = load_skill_file(&path, None, None).unwrap();
        let bd = s.base_dir.to_str().unwrap();
        assert_eq!(s.load_body().unwrap(), format!("{}/a and {}/b", bd, bd));
    }

    #[test]
    fn load_skill_substitutes_claude_plugin_root_braced_and_plain() {
        // Regression: Claude-Code-style skills reference ${CLAUDE_PLUGIN_ROOT}
        // (and the bare $CLAUDE_PLUGIN_ROOT form) which must be substituted
        // to the plugin's canonical root before the body is handed to the model.
        let tmp = tempdir();
        let plugin_root = tmp.join("my-plugin");
        fs::create_dir_all(&plugin_root).unwrap();
        let skill_dir = plugin_root.join("skills").join("exa");
        let path = write_skill(
            &skill_dir,
            "---\nname: exa\ndescription: d\n---\nbash ${CLAUDE_PLUGIN_ROOT}/scripts/a.js then $CLAUDE_PLUGIN_ROOT/b.js",
        );
        let s = load_skill_file(&path, Some("my-plugin"), Some(&plugin_root)).unwrap();
        let root_abs = plugin_root.canonicalize().unwrap();
        let r = root_abs.to_str().unwrap();
        assert_eq!(
            s.load_body().unwrap(),
            format!("bash {}/scripts/a.js then {}/b.js", r, r)
        );
    }

    #[test]
    fn load_skill_leaves_claude_plugin_root_alone_when_not_in_plugin() {
        // Loose skills (plugin_root = None) should not receive the substitution.
        let tmp = tempdir();
        let skill_dir = tmp.join("loose");
        let path = write_skill(
            &skill_dir,
            "---\nname: loose\ndescription: d\n---\n${CLAUDE_PLUGIN_ROOT}/x",
        );
        let s = load_skill_file(&path, None, None).unwrap();
        assert_eq!(s.load_body().unwrap(), "${CLAUDE_PLUGIN_ROOT}/x");
    }

    #[test]
    fn load_all_loose_skill() {
        let tmp = tempdir();
        let skill_dir = tmp.join("skills").join("loose");
        write_skill(&skill_dir, "---\nname: loose\ndescription: d\n---\nBody");

        let (plugins, skills) = load_all(std::slice::from_ref(&tmp));
        assert!(plugins.is_empty());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "loose");
        assert_eq!(skills[0].plugin, None);
    }

    #[test]
    fn load_all_plugin_skill() {
        let tmp = tempdir();
        let plugin_dir = tmp.join("my-plugin");
        fs::create_dir_all(plugin_dir.join(".synaps-plugin")).unwrap();
        fs::write(
            plugin_dir.join(".synaps-plugin").join("plugin.json"),
            r#"{"name":"my-plugin"}"#,
        )
        .unwrap();
        write_skill(
            &plugin_dir.join("skills").join("s1"),
            "---\nname: s1\ndescription: d\n---\nBody",
        );

        let (plugins, skills) = load_all(std::slice::from_ref(&tmp));
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "my-plugin");
        assert!(plugins[0].manifest.as_ref().unwrap().commands.is_empty());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].plugin.as_deref(), Some("my-plugin"));
    }

    #[test]
    fn load_all_plugin_commands_are_carried_in_manifest() {
        let tmp = tempdir();
        let plugin_dir = tmp.join("cmd-plugin");
        fs::create_dir_all(plugin_dir.join(".synaps-plugin")).unwrap();
        fs::write(
            plugin_dir.join(".synaps-plugin").join("plugin.json"),
            r#"{
                "name": "cmd-plugin",
                "commands": [
                    {"name":"hello","description":"Say hello","command":"printf","args":["hello"]}
                ]
            }"#,
        )
        .unwrap();

        let (plugins, skills) = load_all(std::slice::from_ref(&tmp));

        assert_eq!(plugins.len(), 1);
        assert!(skills.is_empty());
        let commands = &plugins[0].manifest.as_ref().unwrap().commands;
        assert_eq!(commands.len(), 1);
        match &commands[0] {
            crate::skills::manifest::ManifestCommand::Shell(cmd) => {
                assert_eq!(cmd.name, "hello");
                assert_eq!(cmd.command, "printf");
            }
            other => panic!("expected shell command, got {other:?}"),
        }
    }

    #[test]
    fn load_all_marketplace() {
        let tmp = tempdir();
        // marketplace.json at root
        fs::create_dir_all(tmp.join(".synaps-plugin")).unwrap();
        fs::write(
            tmp.join(".synaps-plugin").join("marketplace.json"),
            r#"{"name":"pi-skills","plugins":[{"name":"web","source":"./web"}]}"#,
        )
        .unwrap();
        // plugin at ./web
        let plugin_dir = tmp.join("web");
        fs::create_dir_all(plugin_dir.join(".synaps-plugin")).unwrap();
        fs::write(
            plugin_dir.join(".synaps-plugin").join("plugin.json"),
            r#"{"name":"web"}"#,
        )
        .unwrap();
        write_skill(
            &plugin_dir.join("skills").join("search"),
            "---\nname: search\ndescription: d\n---\nBody",
        );

        let (plugins, skills) = load_all(std::slice::from_ref(&tmp));
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].marketplace.as_deref(), Some("pi-skills"));
        assert_eq!(skills.len(), 1);
    }

    #[test]
    fn load_all_dedup_priority() {
        let tmp_local = tempdir();
        let tmp_global = tempdir();
        // same skill name in both
        write_skill(
            &tmp_local.join("skills").join("dup"),
            "---\nname: dup\ndescription: local\n---\nBody",
        );
        write_skill(
            &tmp_global.join("skills").join("dup"),
            "---\nname: dup\ndescription: global\n---\nBody",
        );

        let (_p, skills) = load_all(&[tmp_local, tmp_global]);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "local"); // local wins
    }

    #[test]
    fn test_load_all_plugin_dedup_via_marketplace_and_subdir() {
        // Regression: when a plugin is discovered both through marketplace.json
        // and through the plugin-subdir walk, load_plugin's root-based dedup guard
        // must prevent a duplicate Plugin entry and duplicate skill registration.
        let root = tempdir();

        // marketplace.json at root pointing to ./web
        fs::create_dir_all(root.join(".synaps-plugin")).unwrap();
        fs::write(
            root.join(".synaps-plugin").join("marketplace.json"),
            r#"{"name":"mp","plugins":[{"name":"web","source":"./web"}]}"#,
        )
        .unwrap();

        // Plugin at ./web — also discoverable via the plugin-subdir pass
        let plugin_dir = root.join("web");
        fs::create_dir_all(plugin_dir.join(".synaps-plugin")).unwrap();
        fs::write(
            plugin_dir.join(".synaps-plugin").join("plugin.json"),
            r#"{"name":"web"}"#,
        )
        .unwrap();
        write_skill(
            &plugin_dir.join("skills").join("demo"),
            "---\nname: demo\ndescription: d\n---\nBody",
        );

        let (plugins, skills) = load_all(std::slice::from_ref(&root));

        // Exactly one plugin registered, not two.
        assert_eq!(plugins.len(), 1, "plugin should be deduplicated");
        assert_eq!(plugins[0].name, "web");
        assert_eq!(plugins[0].root, plugin_dir.canonicalize().unwrap());

        // Skill registered exactly once.
        assert_eq!(skills.len(), 1, "skill should be registered exactly once");
        assert_eq!(skills[0].name, "demo");
        assert_eq!(skills[0].plugin.as_deref(), Some("web"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_all_accepts_claude_plugin_marketplace_layout() {
        // Claude-Code-style: marketplace.json under .claude-plugin/, plugin.json
        // also under .claude-plugin/.
        let tmp = tempdir();
        fs::create_dir_all(tmp.join(".claude-plugin")).unwrap();
        fs::write(
            tmp.join(".claude-plugin").join("marketplace.json"),
            r#"{"name":"cc-mp","plugins":[{"name":"web","source":"./web"}]}"#,
        )
        .unwrap();
        let plugin_dir = tmp.join("web");
        fs::create_dir_all(plugin_dir.join(".claude-plugin")).unwrap();
        fs::write(
            plugin_dir.join(".claude-plugin").join("plugin.json"),
            r#"{"name":"web"}"#,
        )
        .unwrap();
        write_skill(
            &plugin_dir.join("skills").join("search"),
            "---\nname: search\ndescription: d\n---\nBody",
        );

        let (plugins, skills) = load_all(std::slice::from_ref(&tmp));
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].marketplace.as_deref(), Some("cc-mp"));
        assert_eq!(plugins[0].name, "web");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "search");
    }

    #[test]
    fn load_all_prefers_synaps_plugin_over_claude_plugin() {
        // When both layouts are present, .synaps-plugin/ wins.
        let tmp = tempdir();
        let plugin_dir = tmp.join("dual");
        fs::create_dir_all(plugin_dir.join(".synaps-plugin")).unwrap();
        fs::create_dir_all(plugin_dir.join(".claude-plugin")).unwrap();
        fs::write(
            plugin_dir.join(".synaps-plugin").join("plugin.json"),
            r#"{"name":"native"}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join(".claude-plugin").join("plugin.json"),
            r#"{"name":"claude"}"#,
        )
        .unwrap();
        write_skill(
            &plugin_dir.join("skills").join("s"),
            "---\nname: s\ndescription: d\n---\nBody",
        );

        let (plugins, _skills) = load_all(std::slice::from_ref(&tmp));
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "native", "synaps-plugin layout must win");
    }

    #[test]
    fn test_load_all_malformed_plugin_json_continues_walk() {
        // Regression: a malformed plugin.json should be skipped with a warning,
        // and the walk must continue so other valid plugins still register.
        let root = tempdir();

        // Broken plugin: invalid JSON in plugin.json
        let broken_dir = root.join("broken");
        fs::create_dir_all(broken_dir.join(".synaps-plugin")).unwrap();
        fs::write(
            broken_dir.join(".synaps-plugin").join("plugin.json"),
            "{ this is not valid json",
        )
        .unwrap();

        // Good plugin alongside it
        let good_dir = root.join("good");
        fs::create_dir_all(good_dir.join(".synaps-plugin")).unwrap();
        fs::write(
            good_dir.join(".synaps-plugin").join("plugin.json"),
            r#"{"name":"good"}"#,
        )
        .unwrap();
        write_skill(
            &good_dir.join("skills").join("hello"),
            "---\nname: hello\ndescription: d\n---\nBody",
        );

        let (plugins, skills) = load_all(std::slice::from_ref(&root));

        // Only the good plugin registered.
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "good");

        // Its skill is present.
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "hello");
        assert_eq!(skills[0].plugin.as_deref(), Some("good"));

        let _ = fs::remove_dir_all(&root);
    }

    /// Create a unique tempdir under /tmp for tests.
    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("synaps-skills-test-{}", std::process::id()));
        let unique = base.join(format!("{}-{}", crate::epoch_millis(), n));
        std::fs::create_dir_all(&unique).unwrap();
        unique
    }
}
