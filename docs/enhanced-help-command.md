# Enhanced `/help` Command

## Objective

Make SynapsCLI help beautiful, brief, discoverable, JSON-driven, and extensible without allowing plugins/extensions to hijack internal help namespaces.

## User-facing behavior

- `/help` renders a concise inline overview with common commands and pointers to deeper help.
- `/help settings`, `/help plugins`, `/help doctor`, `/help models`, `/help login`, etc. render concise inline branch help.
- `/help find` opens a searchable lightbox of all known help topics.
- `/help find <query>` opens the lightbox with the filter pre-populated.
- The lightbox filters as the user types, supports up/down scrolling, Enter selection, and Esc close.

## Content source

Built-in help content is generated from JSON lists. Code owns rendering, validation, protected namespace enforcement, and dispatch behavior; JSON owns copy, sections, commands, aliases, keywords, and branch membership.

Plugin manifests may include a `help_entries` array with the same shape as built-in `HelpEntry` JSON. Rich plugin help supports optional `usage` and `examples` fields:

```json
{
  "name": "acme-tools",
  "help_entries": [
    {
      "id": "acme-sync",
      "command": "/acme:sync",
      "title": "Acme Sync",
      "summary": "Sync Acme workspace state.",
      "category": "Plugin",
      "topic": "Command",
      "protected": false,
      "common": false,
      "aliases": ["/acme:pull"],
      "keywords": ["acme", "sync", "workspace"],
      "lines": ["Keeps the local Acme cache up to date."],
      "usage": "/acme:sync [workspace]",
      "examples": [
        {
          "command": "/acme:sync docs",
          "description": "Sync the docs workspace."
        }
      ],
      "related": ["/help plugins"]
    }
  ]
}
```

Plugin help sources are assigned from the manifest name and shown in `/help find` detail as `Source: plugin <name>` (or `Source: plugin` when no name is available).

## Extension behavior

Plugins/extensions may contribute help entries for their own commands or topics. They may not replace, shadow, or claim protected internal help entries. Protected internal paths include `/help`, `/help find`, `/settings`, `/plugins`, `/models`, `/login`, `/status`, `/ping`, `/extensions`, and all built-in slash command names.

Invalid or conflicting contributed help should be ignored with diagnostics/logging and must not break core `/help`.

## Success criteria

- `/help` is under ~20 lines, beautiful, and includes common commands.
- Branch help exists for settings, plugins, doctor, models, login, commands, extensions, sessions, and tools.
- `/help find` is interactive and searchable in memory.
- Help is generated from JSON.
- Protected namespace behavior is tested.
- Core command tests/build pass.

## Verification commands

```bash
cargo test
cargo build
cargo clippy --all-targets --all-features
```
