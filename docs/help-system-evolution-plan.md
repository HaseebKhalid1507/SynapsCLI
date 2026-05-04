# SynapsCLI Help System Evolution Plan

## Purpose

This plan folds research from AWS CLI, GitHub CLI, Git, Docker, VS Code, Raycast, Textual, kbar, fzf, and TUI tools into the SynapsCLI help system.

The current enhanced help branch already adds JSON-backed help entries, `/help <topic>`, plugin-contributed help entries, protected namespace enforcement, and a searchable `/help find` lightbox. The next step is to make that system feel mature under a large, monolithic command surface while staying brief in chat.

## Design Principles

1. **Brief by default, deep on demand**
   - `/help` should remain short and task-oriented.
   - `/help <topic>` should explain one branch or command.
   - `/help find` should be the searchable full index.

2. **Structured metadata, rendered multiple ways**
   - Keep `assets/help.json` as the source of truth for built-in help.
   - Render the same entries as inline chat text, modal search rows, and detail views.
   - Avoid hardcoded help strings in command handlers.

3. **Two parallel help namespaces**
   - **Commands** answer “how do I run this?”
   - **Topics/guides** answer “what is this and how does it fit together?”
   - This mirrors AWS `help topics`, GitHub CLI `help <topic>`, and Git conceptual docs.

4. **Task-stage grouping beats alphabetical dumping**
   - Group help by user intent: Start, Conversation, Models, Settings, Plugins, Diagnostics, Sessions, Advanced.
   - Alphabetical order is useful inside a group, not as the primary information architecture.

5. **Plugins are first-class but cannot hijack core help**
   - Plugin help entries appear in the same search index.
   - Protected command, alias, and topic namespaces remain enforced.
   - Plugin entries should clearly show source/plugin ownership.

6. **TUI-native discovery**
   - `/help find` should behave like a lightweight command palette.
   - It should support type-to-filter, grouped results, keyboard navigation, Enter for detail, and Esc to close.

## Research Patterns to Adopt

### AWS CLI

Adopt:
- Deep hierarchical help: root → service/topic → command detail.
- Concept topics separate from command reference.
- Copy-pasteable examples for complex commands.
- “See also” links between related entries.

Avoid:
- Huge paged manpage dumps inside chat history.
- Requiring users to know the exact namespace before discovery.

### GitHub CLI

Adopt:
- Clear categories: core commands, extension commands, help topics.
- A reference-style index that can list all commands/topics.
- Extension command discoverability.

Avoid:
- Treating extension help as separate from normal help discovery.

### Git

Adopt:
- Workflow/task-stage grouping.
- Conceptual guide links from command groups.
- Short root help with pointers to deeper help.

Avoid:
- Plumbing-level noise in the default help view.

### Docker / kubectl-style CLIs

Adopt:
- Common commands first.
- Management namespaces for larger areas like plugins/extensions/models.
- Detail help for subcommand families.

Avoid:
- Overloading root help with every possible subcommand.

### VS Code / Raycast / Textual / kbar / fzf

Adopt for `/help find`:
- Command-palette style search.
- Sectioned results.
- Hidden keywords and aliases.
- Search over command, title, summary, aliases, keywords, and details.
- Ranking that favors exact/prefix matches, then fuzzy matches, then keywords/details.
- Optional future MRU ranking.

Avoid:
- Blind cycling through matches with no visible result list.
- Unstructured string matching over pre-rendered prose.

## Target Information Architecture

### Root Help: `/help`

Goal: answer “where should I go next?” in under ~15 lines.

Proposed shape:

```text
SynapsCLI Help

Start here. Pick a path or search everything.

Common paths
  /help find      Search every help topic
  /model          Choose or inspect models
  /settings       Configure providers and preferences
  /plugins        Manage extensions
  /doctor         Diagnose local setup

Guides
  /help models    Models, providers, and routing
  /help plugins   Plugins, skills, commands, and trust
  /help sessions  Sessions, resume, compact, and chains

Tip: type /help find and start typing.
```

### Branch Help: `/help <topic>`

Goal: describe one area and its common commands.

Examples:
- `/help models`
- `/help settings`
- `/help plugins`
- `/help doctor`
- `/help login`
- `/help sessions`
- `/help extensions`

Shape:

```text
Models

Choose the model and provider used for the conversation.

Common commands
  /model       Open model picker
  /models      Alias for /model
  /ping        Check provider health
  /settings    Configure provider keys and defaults

Examples
  /model
  /ping

Related: /help settings, /help login, /help find
```

### Command Help: `/help <command>`

Goal: exact usage for one executable command.

Examples:
- `/help compact`
- `/help chain`
- `/help extensions audit`
- `/help plugins install`

Shape:

```text
/compact

Summarize and compact the current conversation.

Usage
  /compact [focus]

Examples
  /compact
  /compact keep the auth debugging context

Related: /help sessions, /chain
```

### Topic Guides: `/help <guide>` or `/help about <topic>`

Goal: explain cross-cutting concepts.

Candidate guides:
- `sessions` — sessions, names, resume, compact, chains
- `plugins` — plugins vs skills vs commands vs extensions
- `models` — providers, routing, API keys, health checks
- `trust` — extension/plugin trust and protected namespaces
- `keybinds` — registered keybinds and plugin keybinds
- `hooks` — extension lifecycle and hook behavior

The initial implementation can use the same `HelpEntry` format with `topic: "Branch"` or a future `topic: "Guide"` variant.

## JSON Schema Evolution

Current `HelpEntry` fields are a good base:

```json
{
  "id": "models",
  "command": "/help models",
  "title": "Models",
  "summary": "Choose providers, models, and routing defaults.",
  "category": "Models",
  "topic": "Branch",
  "protected": true,
  "common": true,
  "aliases": ["/models help"],
  "keywords": ["model", "provider", "router"],
  "lines": [],
  "related": ["/help settings", "/help login"]
}
```

Recommended additions, in order:

1. `usage: Option<String>`
   - For command/detail help.

2. `examples: Vec<HelpExample>`
   - Copy-pasteable examples.

```json
"examples": [
  {
    "command": "/compact keep auth debugging context",
    "description": "Compact while preserving an important focus."
  }
]
```

3. `guide: Option<String>` or `topic: "Guide"`
   - Distinguish conceptual docs from executable commands and branch pages.

4. `priority: Option<u16>`
   - Stable ordering inside categories.
   - Lets common entries outrank obscure entries without hardcoding in renderer.

5. `source_name: Option<String>`
   - Display plugin ownership clearly in the modal.

## `/help find` Target Behavior

### Current Behavior to Preserve

- Opens a lightbox.
- Type to filter.
- Up/down navigation.
- Enter opens detail.
- Esc closes or returns from detail to browse.
- Includes plugin-contributed help entries after namespace validation.

### Improvements

1. **Sectioned browse mode**
   - Empty query groups results by category.
   - Show common/core entries first.

2. **Better ranking**
   - Exact command match.
   - Prefix command/title match.
   - Alias match.
   - Keyword match.
   - Summary/body match.
   - Alphabetical or priority tiebreak.

3. **Detail view enrichment**
   - Render usage and examples when available.
   - Show related links.
   - Show source for plugin entries.

4. **Empty state**
   - If no matches: “No help entries match ‘x’. Try models, plugins, settings, sessions, doctor.”

5. **Future: MRU**
   - Track recently opened help entries or executed commands.
   - Use MRU only as a subtle ranking boost.

## Plugin Help Policy

### Plugin Manifest Help Entries

Plugins may contribute help entries through manifest `help_entries`.

Required or strongly recommended fields:
- `id`
- `command`
- `title`
- `summary`
- `category`
- `keywords`
- `lines` or future `usage/examples`

### Namespace Protection

Continue enforcing protected namespace checks across:
- `command`
- `id`
- `aliases`

Reject plugin entries that attempt to shadow:
- built-in slash commands
- built-in help topics
- protected aliases
- protected IDs

Diagnostics:
- Keep `tracing::warn!` on rejection.
- Consider surfacing rejected entries in `/help plugins` or `/plugins` diagnostics later.

### Plugin Display

In `/help find`, plugin entries should show source, for example:

```text
Plugin: acme-tools
  /acme:sync    Sync Acme workspace state
```

In detail view:

```text
Source: plugin acme-tools
```

## Implementation Phases

### Phase 1 — Polish Existing Help Content

Scope:
- Improve copy in `assets/help.json`.
- Add missing branch pages for important namespaces.
- Keep `/help` brief.

Tasks:
1. Rewrite root `/help` copy around “Common paths” and “Guides”.
2. Add or improve branch entries:
   - `sessions`
   - `extensions`
   - `trust`
   - `keybinds`
   - `compact` / `chain` if treated as command help.
3. Ensure each branch has:
   - concise summary
   - common commands
   - related links
   - search keywords

Tests:
- Root help remains under line budget.
- Root help includes `/help find` and core branches.
- Branch help for each new topic renders.
- Search finds new topics by keywords.

### Phase 2 — Add Usage and Examples to HelpEntry

Scope:
- Extend schema and renderer without changing command dispatch.

Tasks:
1. Add optional `usage` and `examples` fields to `HelpEntry`.
2. Update `render_entry()` to render:
   - title
   - summary
   - body lines
   - usage
   - examples
   - related
3. Update `/help find` detail rendering to include usage/examples.
4. Add examples for complex commands:
   - `/compact`
   - `/chain`
   - `/extensions trust`
   - `/extensions audit`
   - `/plugins`
   - `/model`

Tests:
- JSON with omitted `usage/examples` still loads.
- Entries with usage/examples render expected sections.
- `/help find` detail includes examples.

### Phase 3 — Strengthen `/help find` Search and Layout

Scope:
- Improve ranking and browse layout.

Tasks:
1. Add scoring function over:
   - command
   - title
   - aliases
   - keywords
   - summary
   - lines/details
2. Use priority/common/category for empty-query ordering.
3. Group browse results by category.
4. Add better no-results state.
5. Consider match highlighting if cheap in ratatui spans.

Tests:
- Exact command outranks keyword-only match.
- Prefix match outranks body match.
- Alias match returns the canonical entry.
- Empty browse mode puts common/core entries first.
- No-results state is stable.

### Phase 4 — Command Help Drill-Down

Scope:
- Make `/help <command>` work for executable commands, not only branches.

Tasks:
1. Normalize `/help model`, `/help /model`, and `/help models` lookups.
2. Support multi-token commands like `/help extensions audit`.
3. Prefer exact command match before branch fallback.
4. Unknown topic fallback should suggest `/help find` and maybe closest matches.

Tests:
- `/help model` renders `/model` command help.
- `/help /model` renders same entry.
- `/help extensions audit` renders exact command/subcommand help.
- Unknown command suggests search.

### Phase 5 — Plugin Help Parity

Scope:
- Ensure plugins can provide rich help and appear correctly.

Tasks:
1. Document `help_entries` in plugin manifest docs.
2. Add optional usage/examples fields to plugin help entries.
3. Show plugin source in `/help find` detail view.
4. Add diagnostics for rejected plugin help entries if practical.

Tests:
- Plugin help entry with usage/examples loads.
- Plugin entry appears in search.
- Plugin source is forced to plugin/source name.
- Protected namespace rejection still covers command/id/aliases.

### Phase 6 — Optional Advanced Discovery

Scope:
- Make discovery feel like a mature command palette.

Potential tasks:
1. MRU ranking for opened help entries or executed commands.
2. Tab on ambiguous slash command opens `/help find` prefiltered.
3. Ghost text shows match count for ambiguous prefixes.
4. `/help reference` renders a full markdown-style command index.
5. `/help topics` lists conceptual guides only.

These should be deferred until core help content and search quality are solid.

## Suggested Near-Term Content Additions

### `/help sessions`

Cover:
- `/sessions`
- `/resume`
- `/saveas`
- `/compact`
- `/chain`

Keywords:
- session, history, resume, compact, chain, save, conversation

### `/help extensions`

Cover:
- `/extensions status`
- `/extensions config`
- `/extensions trust`
- `/extensions audit`
- `/extensions memory`

Keywords:
- extension, hook, trust, audit, memory, provider

### `/help trust`

Cover:
- protected namespaces
- plugin help entry rejection
- extension trust model
- diagnostics

Keywords:
- trust, security, plugin, extension, protected, namespace, audit

### `/help compact`

Cover:
- why compacting matters
- optional focus argument
- relation to chains/sessions

Examples:
- `/compact`
- `/compact preserve the OAuth debugging trail`

### `/help chain`

Cover:
- naming chains
- listing chains
- un-naming chains
- how compaction advances named chains

Examples:
- `/chain`
- `/chain name release-prep`
- `/chain list`
- `/chain unname release-prep`

## Acceptance Criteria

A future iteration should satisfy:

- `/help` is brief, polished, and task-oriented.
- `/help find` is the authoritative full index.
- `/help <topic>` works for branch/topic pages.
- `/help <command>` works for important executable commands.
- Help content comes from structured JSON/plugin metadata.
- Complex commands include usage and examples.
- Plugin help entries appear in search and cannot shadow protected namespaces.
- Search results rank exact/prefix/alias matches above body text matches.
- The system remains fast and requires no runtime file I/O for built-in help.

## Non-Goals for Now

- Replacing all documentation with in-app help.
- Building a full manpage renderer.
- Adding network-backed help search.
- Letting plugins override core help.
- Making `/help` dump every command into chat history.

## Recommended Next Slice

Implement **Phase 1** first:

1. Improve root `/help` copy around Common paths and Guides.
2. Add `/help sessions`, `/help extensions`, `/help trust`, `/help compact`, and `/help chain` entries to `assets/help.json`.
3. Add tests that those topics render and are searchable.

This gives users better content immediately while preserving the current architecture and avoiding premature search/ranking complexity.
