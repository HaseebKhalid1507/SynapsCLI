# Plugin Agent Symlinks + Namespaced Resolution

**Goal:** Make plugin-provided agents (like BBE's sage, quinn, glitch, arbiter) discoverable via the global `~/.synaps-cli/agents/` directory and resolvable via `plugin:agent` syntax.

**Architecture:** Two changes: (A) After plugin install/update, symlink each `agents/*.md` file from the plugin into `~/.synaps-cli/agents/<frontmatter-name>.md`. On uninstall, remove those symlinks. (B) Extend `resolve_agent_prompt()` to support `plugin:agent` namespaced syntax (e.g. `dev-tools:sage`) that searches `~/.synaps-cli/plugins/<plugin>/skills/*/agents/<agent>.md`.

## Problem

BBE skill agents live at `~/.synaps-cli/plugins/dev-tools/skills/black-box-engineering/agents/{sage,quinn,glitch,arbiter,orchestrator}.md`. The `resolve_agent_prompt()` function only searches `~/.synaps-cli/agents/<name>.md` (by bare name) or reads an absolute path (if the name contains `/`).

The SKILL.md and orchestrator.md reference agents via `<skill_dir>/agents/sage.md` — full paths that work but are fragile and undiscoverable. You can't just say `agent: "bbe-sage"`.

## Design

### Part A: Symlinks on install/update/uninstall

**Discovery:** When a plugin is installed or updated, scan for `agents/` directories inside each `skills/*/` subdirectory. For each `.md` file found, parse its YAML frontmatter `name` field. If present, create a symlink:

```
~/.synaps-cli/agents/<frontmatter-name>.md → <plugin-dir>/skills/<skill>/agents/<file>.md
```

Example:
```
~/.synaps-cli/agents/bbe-sage.md → ~/.synaps-cli/plugins/dev-tools/skills/black-box-engineering/agents/sage.md
```

**New function:** `sync_plugin_agent_symlinks(plugin_dir: &Path)` in `src/skills/install.rs`:
1. `mkdir -p ~/.synaps-cli/agents/`
2. Walk `<plugin_dir>/skills/*/agents/*.md`
3. For each `.md`, parse frontmatter for `name`
4. If `name` exists, create symlink `~/.synaps-cli/agents/<name>.md → <absolute-path>`
5. If the symlink target already exists and is NOT a symlink, skip (don't clobber user files)
6. If it's an existing symlink, replace it (idempotent)

**New function:** `remove_plugin_agent_symlinks(plugin_dir: &Path)` in `src/skills/install.rs`:
1. Walk `<plugin_dir>/skills/*/agents/*.md`
2. For each `.md`, parse frontmatter for `name`
3. If `~/.synaps-cli/agents/<name>.md` is a symlink pointing into `plugin_dir`, remove it

**Call sites in `src/chatui/plugins/actions.rs`:**
- `run_install_flow()` — call `sync_plugin_agent_symlinks(&dest)` after successful install
- `apply_update()` — call `sync_plugin_agent_symlinks(&dir)` after successful update
- `apply_uninstall()` — call `remove_plugin_agent_symlinks(&dir)` BEFORE `uninstall_plugin()` (need dir to still exist)
- `apply_remove_marketplace()` — call `remove_plugin_agent_symlinks(&dir)` for each plugin BEFORE uninstall

### Part B: Namespaced `plugin:agent` resolution

**Extend `resolve_agent_prompt()`** in `src/tools/agent.rs`:

```
resolve_agent_prompt(name) →
  1. If name contains '/' → file path (existing)
  2. If name contains ':' → "plugin:agent" namespaced lookup (NEW)
     → search ~/.synaps-cli/plugins/<plugin>/skills/*/agents/<agent>.md
  3. Otherwise → ~/.synaps-cli/agents/<name>.md (existing)
```

For step 2: split on first `:`, left is plugin name, right is agent basename. Glob `~/.synaps-cli/plugins/<plugin>/skills/*/agents/<agent>.md`. If exactly one match, use it. If zero, error. If multiple (shouldn't happen), use first.

### Part C: Update BBE SKILL.md references

Update the BBE SKILL.md and orchestrator.md to reference agents by their frontmatter name rather than full paths. The instructions currently say:

```
agent: "<skill_dir>/agents/sage.md"
```

Change to:
```
agent: "bbe-sage"
```

This is cleaner and works because Part A ensures the symlinks exist.

## File Changes

| File | Change |
|------|--------|
| `src/skills/install.rs` | Add `sync_plugin_agent_symlinks()`, `remove_plugin_agent_symlinks()` |
| `src/tools/agent.rs` | Add `plugin:agent` namespaced branch to `resolve_agent_prompt()` |
| `src/chatui/plugins/actions.rs` | Call symlink functions at install/update/uninstall |
| `~/.synaps-cli/plugins/dev-tools/skills/black-box-engineering/SKILL.md` | Update agent refs |
| `~/.synaps-cli/plugins/dev-tools/skills/black-box-engineering/agents/orchestrator.md` | Update agent refs |

## Edge Cases

- **No frontmatter name:** Skip the file, don't symlink. Log a warning.
- **User file collision:** If `~/.synaps-cli/agents/<name>.md` exists and is a regular file (not symlink), don't clobber it. The user's file takes priority.
- **Cross-plugin name collision:** First installed plugin wins the symlink. The namespaced `plugin:agent` syntax always resolves unambiguously.
- **Broken symlinks:** On update, replace. On uninstall, only remove if the symlink points into the plugin being removed.
- **Windows:** `std::os::unix::fs::symlink` — this is Unix-only. On Windows, fall back to copying the file. (Match the existing Unix-only nature of the app.)
