# Synaps Skills & Plugins — Design

**Date:** 2026-04-18
**Status:** Approved, ready for implementation
**Scope:** Replace the current flat-`.md` skills loader with a plugin-based system that discovers, registers, and dispatches skills as dynamic slash commands.

---

## Goals

1. Extend Synaps's existing skills subsystem to load plugin-packaged skills from disk.
2. Expose each discovered skill as a dynamic slash command in the chat UI (typed `/skill-name <args>`).
3. Unify the user-initiated (slash) and model-initiated (`load_skill` tool) code paths through a single loading mechanism.
4. Support the `pi-skills` repository layout with a folder rename (`.claude-plugin` → `.synaps-plugin`).

## Non-goals (v1)

- `/plugins install <git-url>` command. Users install by `git clone`ing into the plugins directory.
- Marketplace trust prompts, signature verification, version management.
- Hot-reload on SKILL.md changes (restart required).
- `enabled_skills` allowlist (auto-enable all; use `disabled_skills` to block).
- Per-skill tool scoping or permission model.

---

## Key Decisions

| # | Decision |
|---|----------|
| 1 | Read `.synaps-plugin/` only. Never read `.claude-plugin/`. Folder name differs; JSON schema mirrors Claude's. |
| 2 | `/skill-name <args>` = load SKILL.md body into context via synthesized `load_skill` tool-result, then append `<args>` as a user message. |
| 3 | Two-tier layout with marketplace support (plugins contain skills; a marketplace.json can register multiple plugins in one clone). |
| 4 | Migrate away from flat `.md` skill files. Old layout no longer loaded. |
| 5 | Built-in command collisions: built-in wins, warning logged, skill reachable via qualified `/plugin:skill` form. |
| 6 | Cross-plugin collisions: unqualified dispatch requires uniqueness; ambiguous names must be qualified. |
| 7 | `{baseDir}` token in SKILL.md replaced with the skill's absolute path at load time (one substitution, used by both dispatch paths). |
| 8 | Auto-enable all discovered skills. Disable via `disabled_skills` / `disabled_plugins` in config. Local + global merge is additive union. |
| 9 | Dynamic command registry replaces the static `ALL_COMMANDS` constant for autocomplete and `/help`. |
| 10 | Manual install for v1 (user `git clone`s into `~/.synaps-cli/plugins/<name>/`). |

---

## 1. Architecture & Discovery

**Discovery roots** (scanned at chat startup, in priority order):

```
.synaps-cli/plugins/                   # project-local plugins
.synaps-cli/skills/                    # project-local loose skills
~/.synaps-cli/plugins/                 # global plugins
~/.synaps-cli/skills/                  # global loose skills
```

**Discovery logic per root:**

1. **Marketplace pass** — if `<root>/.synaps-plugin/marketplace.json` exists, parse it; each entry's `source` field points to a sibling directory treated as a plugin root.
2. **Plugin pass** — each direct subdirectory containing `.synaps-plugin/plugin.json` is a plugin. Iterate its `skills/` subdirectory; each child directory with `SKILL.md` is a skill.
3. **Loose-skill pass** — under `<root>/skills/`, any subdirectory with `SKILL.md` is a loose skill (`plugin_name = None`).

**Deduplication:** `(plugin_name, skill_name)` pairs are deduped; project-local wins over global. Collision rules (Section 4) apply after dedup.

**New module layout:**

```
src/skills/
  mod.rs            # public API: load_all(), Skill, Plugin structs
  manifest.rs       # marketplace.json + plugin.json parsing
  loader.rs         # filesystem walking, SKILL.md parsing, baseDir substitution
  config.rs         # disabled_skills / disabled_plugins resolution
  registry.rs       # name resolution, collision handling, qualified lookups
  tool.rs           # LoadSkillTool (moved from current skills.rs)
```

The current `src/skills.rs` is replaced by this module tree.

---

## 2. Manifest Schemas

### `.synaps-plugin/marketplace.json` (optional, at discovery root)

```json
{
  "name": "pi-skills",
  "version": "1.0.0",
  "description": "...",
  "owner": { "name": "JR Morton", "url": "..." },
  "plugins": [
    {
      "name": "web-tools",
      "source": "./web-tools-plugin",
      "version": "1.0.0",
      "description": "..."
    }
  ]
}
```

Required: `name`, `plugins[].source`. `source` is relative to the marketplace.json's directory. Unknown fields ignored (forward-compat).

### `.synaps-plugin/plugin.json` (required per plugin)

```json
{
  "name": "web-tools",
  "version": "1.0.0",
  "description": "...",
  "author": { "name": "...", "url": "..." },
  "repository": "...",
  "license": "MIT"
}
```

Required: `name`. Skills discovered by walking `skills/` — no manifest enumeration needed.

### `skills/<skill-name>/SKILL.md` (required per skill)

Unchanged from Claude/pi-skills format:

```markdown
---
name: exa-search
description: Web search via Exa API. Use for documentation, news, research papers.
---

# Exa Search

Run `{baseDir}/search.js "query"` to search...
```

Required frontmatter: `name`, `description`. Body = everything after the closing `---`.

### Validation policy

Soft validation. Missing optional fields → defaults. Missing required fields → skill/plugin skipped with `tracing::warn!`. Malformed JSON → plugin skipped with warning. No hard failures at startup.

---

## 3. Loading Pipeline & `{baseDir}` Substitution

### Data structures

```rust
pub struct Plugin {
    pub name: String,
    pub root: PathBuf,                // absolute
    pub marketplace: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
}

pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,                 // post-substitution markdown
    pub plugin: Option<String>,
    pub base_dir: PathBuf,            // absolute
    pub source_path: PathBuf,         // absolute path to SKILL.md
}
```

### Load sequence — `loader::load_all() -> (Vec<Plugin>, Vec<Skill>)`

1. Walk each discovery root.
2. For each found skill, read `SKILL.md` and parse frontmatter.
3. Compute `base_dir = SKILL.md.parent()`, canonicalize to absolute path.
4. **`{baseDir}` substitution:** `body = body.replace("{baseDir}", base_dir.to_str())`.
5. Apply disable filter (Section 5) and collision resolution (Section 4).
6. Return `(plugins, skills)`.

### Why substitute at load time

The same body feeds both the slash-command path and the `load_skill` tool-result path. Substituting once in the loader guarantees both paths emit identical content (important for prompt cache hit rate and predictability) and eliminates any chance of a literal `{baseDir}` token leaking into a Bash command.

### Edge cases

- Paths with spaces: preserved; model is expected to quote.
- Symlinks: followed via `canonicalize()`.
- No `{baseDir}` references: substitution is a no-op.

---

## 4. Slash Command Registration & Dispatch

### Registry

```rust
pub struct CommandRegistry {
    builtins: &'static [&'static str],        // "clear", "model", …
    skills: HashMap<String, Vec<SkillRef>>,   // unqualified → all matches
    qualified: HashMap<String, SkillRef>,     // "plugin:skill" → single match
}
```

Each skill is inserted under both its unqualified name (`exa-search`) and qualified name (`web-tools:exa-search`). Loose skills get only the unqualified entry.

### Collision resolution at registration

- Skill unqualified name matches a built-in → `tracing::warn!`; skill reachable via qualified form only.
- Two skills share unqualified name → both remain; unqualified dispatch returns ambiguous error forcing qualified form.

### Dispatch logic (replaces `match cmd` in `commands.rs:52`)

```
resolve(cmd):
  1. cmd contains ':'    → look up in `qualified`.
  2. cmd in builtins     → route to existing built-in handler.
  3. cmd in skills, 1 match → that skill.
  4. cmd in skills, >1   → error: "ambiguous; try /plugin:skill".
  5. no match            → "unknown command".
```

### Fire path

On skill resolve, handler synthesizes:

```rust
app.api_messages.push(assistant_with_tool_use("load_skill", { "skill": name }));
app.api_messages.push(tool_result(skill.body));   // already baseDir-substituted
app.api_messages.push(user_message(arg));
```

Then starts a normal stream. The model sees the skill as if it had invoked `load_skill` itself — identical context shape, identical cache behavior. Display: one `ChatMessage::System("loaded skill: exa-search")` in scrollback.

### Autocomplete and `/help`

The static `commands.rs:10` `ALL_COMMANDS: &[&str]` becomes `fn all_commands(&registry) -> Vec<&str>` — builtins + every registered skill (unqualified names, deduped, sorted). `/help` prints built-ins first, then a `## Skills` section listing each skill with its description.

### Streaming-time policy

Skills are **not** dispatchable mid-stream (they inject context, which would race the model). Attempting `/skill-name` while streaming shows `"cannot load skill while streaming"`. The `STREAMING_COMMANDS` set is unchanged.

---

## 5. Config & Disable Mechanism

Extends Synaps's existing config system (`src/config.rs`).

### Config file locations (existing resolution order)

```
.synaps-cli/config               # project-local (highest priority)
~/.synaps-cli/config             # global
~/.synaps-cli/<profile>/config   # profile (if set)
```

### New config keys

```toml
# Disable whole plugins by plugin.json name
disabled_plugins = ["black-box-engineering"]

# Disable individual skills; bare name disables everywhere,
# qualified name disables only that plugin's copy
disabled_skills = ["gamba-helper", "web-tools:transcribe"]
```

### Resolution rules

1. Load global → profile → project-local.
2. Merge is **additive union** for both lists. Local adds to global; cannot un-disable something global disabled.
3. After discovery, filter the skill set:
   - Skip if `skill.plugin in disabled_plugins`.
   - Skip if `skill.name in disabled_skills` (unqualified match).
   - Skip if `"{plugin}:{skill}" in disabled_skills` (qualified match).
4. Disabled skills emit `tracing::debug!`, not warnings.

### Rationale

Additive-only merge is a simpler mental model ("local adds more blocks") and avoids the footgun where project config silently re-enables a skill an admin globally blocked.

No allowlist is supported. Auto-enable is the default; supporting both lists invites confusion about precedence.

### Single source of truth

The `load_skill` tool's skill list is filtered by the same disable config. A disabled skill is unreachable via both slash command and model-initiated tool call.

---

## 6. Migration from `src/skills.rs`

### Removed

- `scan_skills_dir` (`skills.rs:63`) — flat-`.md` loading.
- `parse_skills_config` (`skills.rs:145`) and any callers reading a `skills = "a, b"` allowlist.
- `format_skills_for_prompt` (`skills.rs:124`) — startup system-prompt injection of skills is gone.
- Single-file `src/skills.rs` — replaced by `src/skills/` module tree.

### Preserved (moved, not rewritten)

- `parse_frontmatter` (`skills.rs:13`) → `src/skills/loader.rs`. Same logic, same tests.
- `Skill` struct → `src/skills/mod.rs` with added fields (`plugin`, `base_dir`, `source_path`).
- `LoadSkillTool` (`skills.rs:159`) → `src/skills/tool.rs`. Unchanged semantics.
- `setup_skill_tool` (`skills.rs:209`) → renamed `register` in `src/skills/mod.rs`; returns `CommandRegistry` alongside the tool registration so chat UI can wire autocomplete.

### Breaking changes (CHANGELOG entries required)

1. Flat `.md` skill files in `.synaps-cli/skills/` no longer load. Users must convert to `skills/<name>/SKILL.md` folder form.
2. `skills = "a, b"` config key (allowlist) removed. Replace with `disabled_skills` using inverted logic, or delete the line to auto-enable all.
3. Skills are no longer force-injected into the system prompt at startup. Every skill load is now an explicit event in conversation history.

### Callers to update

Grep for `synaps_cli::skills::` and `crate::skills::` — update imports, adjust `setup_skill_tool` → `register` signature, remove any `format_skills_for_prompt` invocations.

---

## 7. Testing Plan

### Unit tests (colocated with each module)

- `loader.rs`: `{baseDir}` substitution with/without token; frontmatter parsing for missing/malformed fields (existing cases reused); absolute-path canonicalization; SKILL.md without frontmatter → skipped.
- `manifest.rs`: well-formed marketplace.json → plugins enumerated; missing `source` → plugin skipped with warning; unknown fields ignored; plugin.json with only `name` accepted.
- `config.rs`: additive merge of `disabled_plugins` across local + global; qualified vs unqualified `disabled_skills` matching; empty config → nothing disabled.
- `registry.rs`: built-in collision emits warning, skill still reachable via qualified name; two plugins with same skill name → ambiguous unqualified dispatch, each reachable qualified; unknown command returns error; prefix resolution still works for built-ins.

### Integration test — `tests/skills_plugin.rs`

Build a temp directory tree with: a marketplace.json, one plugin containing two skills (one colliding with a built-in name, one unique), and a loose skill. Assert:

- All three skills discovered.
- Built-in collision warning logged.
- Qualified + unqualified dispatch both resolve correctly.
- Disable config filters work end-to-end.

### Manual verification checklist

1. `git clone` pi-skills into `~/.synaps-cli/plugins/pi-skills/` with `.claude-plugin` dirs renamed to `.synaps-plugin`. Every skill appears in `/help` and autocomplete.
2. `/exa-search "rust async patterns"` — scrollback shows `loaded skill: exa-search`; model calls Bash with the substituted absolute `search.js` path.
3. Add `disabled_skills = ["exa-search"]` to project config → `/exa-search` returns unknown command; `/help` omits it.
4. Two plugins each defining `search` → `/search` returns ambiguity error; `/plugin-a:search` and `/plugin-b:search` both work.
5. Startup with malformed `plugin.json` → warning in log, chat starts normally, other plugins still load.

---

## Open items for future versions

- `/plugins install <git-url>` command and marketplace browsing UX.
- Trust / signing / version pinning for marketplace entries.
- Hot-reload via the existing `src/watcher/` module.
- Per-skill tool scoping (e.g., a skill that disables Bash).
- Skill-invoked-by-skill mechanics beyond what the current `load_skill` tool provides.
