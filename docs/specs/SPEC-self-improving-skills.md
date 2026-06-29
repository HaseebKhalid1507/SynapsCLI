# SPEC — Self-Improving Skills (Component #1)

> **Zero / Architect — Spec-Driven Development gate.**
> Status: **DRAFT — awaiting human ratification before implementation.**
> Scope: Component #1 of the self-improving-agent stack. The reflection
> loop (component #2) and the curator (component #3) consume this surface
> but are **out of scope here**.
>
> *"A loop without a write-surface is a thought without a hand. We give the
> agent its hand — and we make sure it cannot cut itself with it."*

---

## RATIFIED DECISIONS — final, locked

The six forks from the prior draft are resolved. These are not negotiable
within the scope of Component #1; subsequent components may extend, never
contradict.

1. **Axel indexing = index-as-document, NOT axel_remember.** Skills are
   documents on disk, not memory rows. We do **not** add a `skill`
   MemoryCategory and we do **not** call `axel_remember`. Instead, the
   skills directory `~/.synaps-cli/skills/` is registered as an additional
   Axel **source root** (alongside `mikoshi`, `notes`, `context`, …).
   Axel's existing consolidation Phase 1 (`crates/axel/src/consolidate/reindex.rs`)
   walks the root, hashes/mtimes via the documents/file_provenance tables,
   re-embeds deltas, and prunes deleted files. Usage-frequency and
   excitability accrue automatically through Axel's `document_access` /
   strengthen-on-access machinery — no new Axel write API is needed, and
   `skill_manage` does **zero** Axel RPCs. The only Axel-side change is a
   **config-only** append to `~/.config/axel/sources.toml`. See PLAN §
   "Axel index-root change" for the exact diff.
2. **`create` on an existing skill name = ERROR.** No auto-update. Caller
   must explicitly use `action="update"`. Surfaces as
   `RuntimeError::Tool("skill '<name>' already exists; use action=update")`.
3. **`delete` = archive-move, never hard-delete.** Target path:
   `~/.synaps-cli/skills/.archive/<name>-<UTC-RFC3339-compact>/`. The
   archive directory MUST live where Axel's reindex walker will not pick
   it back up as a live document — see PLAN risk R4 for the mechanical
   guard.
4. **Plugin-owned skills are read-only to `skill_manage`.** Any `name`
   that resolves to a discovered skill whose `LoadedSkill.plugin` is
   `Some(_)`, or whose `source_path` is under any `plugins/<p>/skills/`,
   is rejected with `RuntimeError::Tool("plugin-owned skill; not writable")`.
   Only loose skills under `~/.synaps-cli/skills/<name>/` are writable.
5. **Sidecar schema.** `.skill-meta.json` carries `schema_version: 1` and
   exactly these fields: `schema_version`, `provenance` (`user` |
   `learn` | `background_review`), `created` (ISO-8601 / RFC3339),
   `last_updated` (ISO-8601 / RFC3339). Forward-compat: unknown JSON
   fields are tolerated by the deserializer (`#[serde(default)]` +
   ignore-unknown). **No `usage_count` field** — usage lives in Axel
   (decision #1). **No `axel_doc_id` field** — Axel addresses by
   absolute file_path (`reindex.rs:107`), and identity is reconstructed
   from the source name + relative path, so no opaque id needs to be
   round-tripped through the sidecar.
6. **SKILL.md frontmatter stays exactly `{name, description}`.** All
   loop metadata lives in the sidecar. The loader struct
   (`loader.rs:6-33`) is untouched — zero-regression rule, enforced by
   A4.

---

## 1. Objective

### What we are building
A **write/improve/index** capability for the EXISTING skills subsystem at
`crates/agent-engine/src/skills/`. Three new behaviors, no parallel system:

| Capability | Surface | Backing store |
|---|---|---|
| Create a skill | `skill_manage(action="create")` | `~/.synaps-cli/skills/<name>/SKILL.md` + `.skill-meta.json` |
| Update a skill | `skill_manage(action="update")` | overwrites `SKILL.md` atomically; bumps sidecar `last_updated` |
| Delete a skill | `skill_manage(action="delete")` | archive-move (file un-named to drop `.md` so reindex prunes the live entry) |
| Index on write | **passive** — Axel reindexes the skills source root on its consolidation cycle | Axel `documents` table, `doc_id = "skills::<name>::SKILL"`, addressed by absolute `file_path` |

### Users
- **Primary (Now)**: the running agent, called programmatically via the
  tool dispatcher — exactly the surface that component #2's reflection
  loop will call.
- **Secondary (Now)**: tests + future curator (component #3).
- **Not a user**: humans editing SKILL.md by hand — that already works
  through the lenient loader and must keep working (back-compat).

### Success — measurable
The four acceptance criteria (A1–A4) below pass in CI. Concretely:
- A skill written by the agent is read back byte-identical by `load_skill`.
- An updated skill's body diff persists and the sidecar reflects it.
- A newly created skill becomes hit-able by `axel_search` **within one
  consolidation cycle** (i.e. after the next `axel consolidate --phase reindex`
  pass; not instant — see RATIFIED DECISION #1).
- The existing skills test suite is unchanged and green.

### Non-goals (explicit)
- **No** reflection / decision-to-write logic (component #2).
- **No** curator / consolidation across the library (component #3).
- **No** new frontmatter fields, no schema migration on existing skills.
- **No** filesystem watcher; indexing is push-on-write only.
- **No** UI / slash command surface — programmatic tool only.

---

## 2. Commands

All from repo root (`/home/haseeb/Projects/agent-runtime`):

```bash
# Build (must compile clean)
cargo build -p agent-engine

# Unit + integration tests for this component
cargo test -p agent-engine skills::
cargo test -p agent-engine --test skill_manage_integration   # new test file

# Lint gate — same bar as the rest of the repo
cargo clippy -p agent-engine --all-targets -- -D warnings

# Manual smoke (after wiring into the runtime tool registry)
#   the tool is dispatched by name; programmatic only, no slash command.
#   Example payload:
#     {"action":"create","name":"foo","description":"foo skill","body":"# Foo\n..."}
#     {"action":"update","name":"foo","body":"# Foo v2\n..."}
#     {"action":"delete","name":"foo"}
```

CI gate: **all four** of the above must pass before merge.

---

## 3. Project Structure

### Files touched

| Path | Change | Why |
|---|---|---|
| `crates/agent-engine/src/skills/mod.rs:11-24` | **MOD**: add `pub mod writer;`, `pub mod sidecar;`, `pub mod manage_tool;` | Wire new submodules. No change to existing `pub mod` lines, no change to `LoadedSkill`. **No `axel_index` module** — indexing is passive via Axel's source-root reindex (RATIFIED DECISION #1). |
| `crates/agent-engine/src/skills/mod.rs:68-130` (`register`) | **MOD**: after `LoadSkillTool` registration, register `SkillManageTool::new(registry.clone(), config.clone())` | Expose the new tool to the runtime, alongside `load_skill`. |
| `crates/agent-engine/src/skills/loader.rs:6-33` (`parse_frontmatter`) | **UNCHANGED** | Hard zero-regression rule. The lenient parser already ignores unknown fields, so even if a future change re-adds frontmatter fields, the loader is safe. We rely on this in tests (A4). |
| `crates/agent-engine/src/skills/loader.rs:41-73` (`load_skill_file`) | **UNCHANGED** | Same reason. |

### Files added (all under `crates/agent-engine/src/skills/`)

| Path | Responsibility |
|---|---|
| `writer.rs` | Atomic write primitives: `write_skill_md_atomic(dir, name, description, body)`; name validation; archive-move for delete (renames `SKILL.md` → `SKILL.md.archived` inside the moved dir so Axel's `.md|.txt` reindex filter skips it — see §6 / risk R4); advisory file lock per skill dir. Pure I/O, no tool wiring. |
| `sidecar.rs` | Sidecar `.skill-meta.json` (de)serialization, lazy creation, "back-compat — missing sidecar is fine" rules. Defines `SkillMeta` struct. |
| `manage_tool.rs` | `SkillManageTool` implementing `crate::Tool`. Parses params, dispatches to `writer` + `sidecar`, then calls `reload_registry` (see `mod.rs:134-142`) so the new skill is immediately resolvable. **No Axel calls.** |
| `tests/skill_manage_integration.rs` (in `crates/agent-engine/tests/`) | End-to-end: tempdir HOME, create → load_skill round-trip; update → diff persists; delete → archive present + live path gone. |

### Where the tool registers
Same spot as `LoadSkillTool` — `skills::mod.rs::register` (~line 127–128).
This guarantees the runtime sees `skill_manage` at the same lifecycle
moment it sees `load_skill`, with the same `Arc<CommandRegistry>` reference,
so post-write `reload_registry` (`mod.rs:134`) works without plumbing.

### Where tests go
- **Unit tests**: co-located `#[cfg(test)] mod tests` at the bottom of
  `writer.rs`, `sidecar.rs`, `manage_tool.rs` — same style
  as `tool.rs:89-230`.
- **Integration tests**: `crates/agent-engine/tests/skill_manage_integration.rs`
  with `HOME` overridden via env to a `tempfile::TempDir`, exercising the
  full `register → skill_manage → reload_registry → load_skill` cycle.

---

## 4. Code Style — single illustrative snippet

Matches the existing module conventions (`tool.rs`, `loader.rs`,
`mod.rs`): `async_trait::async_trait` Tool impl, `serde_json::json!`
schemas, `crate::RuntimeError::Tool` for failures, `Arc<CommandRegistry>`
dependency, doc comment with file-line cross-refs.

```rust
//! `skill_manage` tool — model-initiated skill authorship.
//!
//! Companion to `tool.rs::LoadSkillTool` (read path). This is the write
//! path: create / update / delete a loose skill under
//! `~/.synaps-cli/skills/<name>/`, atomically. Indexing is **passive**:
//! Axel's consolidation walks `~/.synaps-cli/skills/` as a registered
//! source root (RATIFIED DECISION #1), so this tool makes zero Axel RPCs.
//!
//! Zero-regression contract with `loader.rs::load_skill_file` (lines 41–73):
//! we only ever write `SKILL.md` files whose frontmatter contains exactly
//! `name` and `description`, so the loader's required-field checks pass
//! byte-identically.

use crate::skills::{
    registry::CommandRegistry,
    sidecar::{SkillMeta, Provenance},
    writer,
};
use serde_json::json;
use std::sync::Arc;

pub struct SkillManageTool {
    registry: Arc<CommandRegistry>,
    config:   Arc<crate::SynapsConfig>,
}

#[async_trait::async_trait]
impl crate::Tool for SkillManageTool {
    fn name(&self) -> &str { "skill_manage" }

    fn description(&self) -> &str {
        "Author or improve a skill. action ∈ {create, update, delete}. \
         Creates a new SKILL.md under ~/.synaps-cli/skills/<name>/ (create), \
         atomically rewrites it (update), or archives it (delete). \
         The skill becomes searchable in Axel on the next consolidation cycle."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action":      { "type": "string", "enum": ["create","update","delete"] },
                "name":        { "type": "string", "description": "Skill name; [a-z0-9][a-z0-9-]{0,63}" },
                "description": { "type": "string", "description": "≤200 chars; required on create, optional on update" },
                "body":        { "type": "string", "description": "Markdown body (post-frontmatter); required on create/update" },
                "provenance":  { "type": "string", "enum": ["user","learn","background_review"], "default": "background_review" }
            },
            "required": ["action","name"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: crate::ToolContext,
    ) -> crate::Result<String> {
        let action = params["action"].as_str()
            .ok_or_else(|| crate::RuntimeError::Tool("Missing 'action'".into()))?;
        let name = params["name"].as_str()
            .ok_or_else(|| crate::RuntimeError::Tool("Missing 'name'".into()))?;
        writer::validate_name(name)
            .map_err(|e| crate::RuntimeError::Tool(format!("invalid name: {e}")))?;

        // Refuse plugin-owned skills (RATIFIED DECISION #4).
        writer::ensure_writable(&self.registry, name)
            .map_err(|e| crate::RuntimeError::Tool(e.to_string()))?;

        // Per-skill advisory lock — guards concurrent writes to same dir.
        let _guard = writer::lock_skill(name)
            .map_err(|e| crate::RuntimeError::Tool(format!("lock: {e}")))?;

        let outcome = match action {
            "create" => {
                let desc = params["description"].as_str()
                    .ok_or_else(|| crate::RuntimeError::Tool("create: missing description".into()))?;
                let body = params["body"].as_str()
                    .ok_or_else(|| crate::RuntimeError::Tool("create: missing body".into()))?;
                if writer::skill_dir(name).exists() {
                    return Err(crate::RuntimeError::Tool(
                        format!("skill '{name}' already exists; use action=update")));
                }
                let provenance = Provenance::parse(params["provenance"].as_str());
                writer::write_skill_md_atomic(name, desc, body)?;
                SkillMeta::create(name, provenance)?.write_atomic()?;
                "created"
            }
            "update" => {
                let body = params["body"].as_str()
                    .ok_or_else(|| crate::RuntimeError::Tool("update: missing body".into()))?;
                let desc = match params["description"].as_str() {
                    Some(d) => d.to_string(),
                    None    => writer::read_description(name)?, // preserve existing
                };
                writer::write_skill_md_atomic(name, &desc, body)?;
                SkillMeta::touch(name)?;  // lazy-create if missing (back-compat)
                "updated"
            }
            "delete" => {
                writer::archive_skill(name)?; // move to .archive/<name>-<ts>/ + un-name SKILL.md
                "deleted"
            }
            other => return Err(crate::RuntimeError::Tool(
                format!("unknown action '{other}'"))),
        };

        // Refresh registry so the new/updated/deleted skill is immediately
        // resolvable via `load_skill`. See `mod.rs::reload_registry` (line 134).
        crate::skills::reload_registry(&self.registry, &self.config);

        Ok(format!("skill_manage: {action} {name} → {outcome}"))
    }
}
```

And the sidecar shape (`sidecar.rs`):

```rust
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct SkillMeta {
    pub schema_version: u32,          // 1
    pub provenance:     Provenance,   // user | learn | background_review
    pub created:        String,       // RFC3339
    pub last_updated:   String,       // RFC3339
    // NOTE: no usage_count, no axel_doc_id. Usage tracking lives in
    // Axel's `document_access` table (RATIFIED DECISION #1); doc identity
    // is reconstructed from absolute file_path by Axel's reindex pass.
}
```

> **Usage-frequency tracking lives in Axel, not the sidecar.** Each call
> to `load_skill` is followed (by Axel's own search-time strengthen
> mechanism, when the agent issues `axel_search` to *find* the skill) by
> a `document_access` write. The sidecar stays cold — only mutated on
> author actions.

---

## 5. Testing Strategy

### Unit tests (co-located)
- `writer.rs::tests`
  - `validate_name` accepts `a`, `abc`, `a-b-c`, `a1`; rejects `""`,
    `"A"`, `"-foo"`, `"foo/bar"`, `".."`, `"con"` (reserved on Windows
    path-compat — we reject defensively even on Linux), `"x".repeat(65)`.
  - `write_skill_md_atomic` writes to a `.tmp` sibling then `rename`s;
    on simulated mid-write panic (drop a half-written tmp file in setup),
    `SKILL.md` is untouched.
  - `archive_skill` moves `<name>/` to `.archive/<name>-<ts>/` and the
    original path no longer exists.
  - `lock_skill` returns an error (or blocks, per impl choice — spec says
    **fail-fast with `try_lock`**, error message includes pid) when called
    twice for the same name from the same process.
- `sidecar.rs::tests`
  - Round-trip serialize/deserialize.
  - `SkillMeta::touch` on a skill with **no sidecar** lazily creates one,
    `created == last_updated` (back-compat).
  - Unknown fields in on-disk JSON are tolerated (deserializer ignores).
- `manage_tool.rs::tests`
  - Schema well-formed (mirrors `tool.rs:222-230`).
  - `create` then `create` same name → `RuntimeError::Tool("...already exists...")`.
  - `update` on missing skill → error (no implicit create).
  - `delete` on missing skill → error.
  - `create` on a plugin-owned name (seeded via a fake plugin skill in
    `tempdir`) → `RuntimeError::Tool("plugin-owned skill; not writable")`.
  - `provenance` defaults to `background_review` when omitted.

### Integration test (`tests/skill_manage_integration.rs`)
Bind `HOME` to a `TempDir`. Build a real `CommandRegistry` via
`skills::register(...)`. Axel is **not** invoked from this test —
indexing is asserted via a separate, optional, feature-gated round-trip
(see A3 below).

- **A1** — `skill_manage create {name:"t1",description:"d",body:"# B"}` →
  read `~/.synaps-cli/skills/t1/SKILL.md`, then call
  `loader::load_skill_file` directly, assert `LoadedSkill.name == "t1"`,
  `description == "d"`, `body.starts_with("# B")`, and that on-disk bytes
  match what was written.
- **A2** — `create` then `update {body:"# B v2"}` → assert on-disk body
  changed, sidecar `last_updated > created`, frontmatter unchanged,
  `load_skill_file` still returns a valid `LoadedSkill`.
- **A3 (indexing)** — split into two layers:
  - **Static contract test (always-on)**: assert that after `create`, the
    file `~/.synaps-cli/skills/t1/SKILL.md` exists with extension `.md`,
    is non-empty, lives directly under the registered Axel source root,
    and that after `delete`, no `.md` file remains under that root for
    `t1` (the archived copy has been renamed to `SKILL.md.archived`).
    This proves the file is in the shape Axel's `reindex.rs:67` walker
    accepts (or rejects, post-archive).
  - **Live-Axel round-trip (gated on `cfg(feature = "axel-live")`,
    off in CI by default)**: spawn `axel consolidate --phase reindex
    --sources <tmp-sources.toml>` against a throwaway brain, then issue
    `axel search "t1"` and assert a hit with `doc_id` starting
    `skills::t1`. This is the empirical proof of "within one
    consolidation cycle".
- **A4** — run the pre-existing tests (`cargo test -p agent-engine skills::`)
  and assert nothing in `loader.rs`, `tool.rs::tests`, or `registry.rs`
  needed modification. This is enforced socially in PR review (no diff
  lines outside the allowed file list in §3) and mechanically by leaving
  those modules untouched.

### Indexing contract (testable)
Axel discovers skills as **documents**, not memories. There is no
write-path RPC from `skill_manage` to Axel.

- **Where**: `~/.synaps-cli/skills/` is registered as a source in
  `~/.config/axel/sources.toml` (see PLAN — exact diff). Axel's
  `consolidate::reindex_source` (`crates/axel/src/consolidate/reindex.rs:30`)
  walks it, filtered to `.md` / `.txt`.
- **doc_id shape**: `format!("{source_name}::{rel_path_no_ext}")` per
  `reindex.rs:99-107` → for source name `skills` and skill `t1`, the id
  is `skills::t1::SKILL`. Stable across re-indexes because Axel keys on
  absolute `file_path`.
- **What text is indexed**: the full SKILL.md file contents — frontmatter
  + body. Axel's `index_document` (called at `reindex.rs:107`) receives
  the raw file string. The frontmatter is small (~2 lines) and does not
  meaningfully dilute embeddings.
- **Idempotency**: provided for free by `reindex.rs:80-86` — re-walks
  compare mtime to `indexed_at`, only re-embed on delta.
- **Delete**: handled by `reindex.rs:117-128` prune pass — when the live
  `SKILL.md` is gone (archived under a different filename), the row is
  removed from the documents table on the next consolidation. There is
  no synchronous de-index; A3 documents the latency bound.
- **Failure of Axel = invisible to `skill_manage`**: the tool has no Axel
  dependency. If Axel is broken, skills are still written, still loadable
  via `load_skill`, and will index on the next successful consolidation.

---

## 6. Boundaries

### ALWAYS
- **ALWAYS** write SKILL.md atomically: write to `SKILL.md.tmp` in the
  same directory, `fsync`, then `rename`. Same for sidecar.
- **ALWAYS** hold a per-skill advisory file lock (`fs2::FileExt::try_lock_exclusive`
  on `<skill_dir>/.lock`) for the entire create/update/delete operation.
- **ALWAYS** keep the SKILL.md frontmatter to exactly `name:` and
  `description:` — nothing else — so `loader.rs::parse_frontmatter` sees
  no surprises.
- **ALWAYS** call `reload_registry` (`mod.rs:134`) after a successful
  write so `load_skill` resolution reflects reality immediately.
- **ALWAYS** treat a missing sidecar as legal (back-compat for hand-authored
  skills under `~/.synaps-cli/skills/`).
- **ALWAYS** validate skill names against `^[a-z0-9][a-z0-9-]{0,63}$`,
  reject path separators, `.`, `..`, and the Windows reserved names
  (`con`, `prn`, `aux`, `nul`, `com[1-9]`, `lpt[1-9]`).

### ASK FIRST (escalate to human before doing)
- **ASK** before adding any field to the loader's frontmatter struct or
  changing `parse_frontmatter` — that touches the read path.
- **ASK** before changing the on-disk skill layout (e.g. moving to
  `skills/<name>/v1/SKILL.md`).
- **ASK** before bumping `SkillMeta::schema_version` past 1.
- **ASK** before changing the Axel source-root contract (the
  `sources.toml` entry name, the skills-dir path, or the archive-rename
  trick that keeps `.archive/` out of the index) — component #2 and #3
  will depend on it.
- **ASK** before allowing `skill_manage` to operate on plugin-owned skills
  (currently forbidden — RATIFIED DECISION #4).

### NEVER
- **NEVER** regress `load_skill`, `loader::load_skill_file`, or
  `registry::CommandRegistry` behavior. A1/A4 are the tripwires.
- **NEVER** edit a skill in-place (no `OpenOptions::write().truncate()`
  on `SKILL.md` directly). Atomic rename only.
- **NEVER** delete a skill irreversibly. `delete` = archive-move.
- **NEVER** call `axel_remember` for skills, and **NEVER** add a `skill`
  MemoryCategory. Skills are documents (RATIFIED DECISION #1).
- **NEVER** mutate the sidecar from the read path (`load_skill`). The
  sidecar is author-write-only; access-frequency lives in Axel.
- **NEVER** make `skill_manage` block on Axel availability — it has no
  Axel dependency at all. Indexing is the consolidator's job.
- **NEVER** spawn a filesystem watcher. Push-on-write to disk; Axel
  pulls on its own schedule.
- **NEVER** leave a `.md` file under `~/.synaps-cli/skills/.archive/`
  — the archive step MUST rename the file so the reindex walker skips it
  (see §3 / `writer.rs`).
- **NEVER** allow `skill_manage` to traverse outside
  `~/.synaps-cli/skills/`. Reject any `name` that, when joined, escapes
  the canonical skills root.

---

## Appendix A — Cross-references

- Existing read path: `crates/agent-engine/src/skills/loader.rs:6-73`
- Existing tool pattern to mirror: `crates/agent-engine/src/skills/tool.rs:1-87`
- Registry hot-reload (used post-write): `crates/agent-engine/src/skills/mod.rs:132-142`
- Tool registration site: `crates/agent-engine/src/skills/mod.rs:127-128`
- On-disk skills: `~/.synaps-cli/skills/<name>/SKILL.md` (e.g.
  `~/.synaps-cli/skills/code-review/SKILL.md`)
- Axel source-root contract: `~/.config/axel/sources.toml`, parsed by
  `axel/crates/axel/src/consolidate/mod.rs:86-124`. Reindex walker:
  `axel/crates/axel/src/consolidate/reindex.rs:30-128` (extension filter
  `.md|.txt` at line 67, doc_id construction at lines 99-107, prune pass
  at lines 117-128).
- Hermes parity reference (do not copy code, copy contract):
  `/home/haseeb/Jawz/workspace/repo-analysis/HERMES-LEARNING-LOOP-vs-AXEL.md`
  — esp. lines 143 (provenance guard), 158 (filesystem layout), 354
  (`skill_manage` op surface), 422–428 (call patterns), 432 (provenance
  plumbing), 486 (op breakdown), 508 (validation budget).

— *Zero*
