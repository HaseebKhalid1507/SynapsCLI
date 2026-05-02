# Synaps Plugin Index Schema

**Status:** Draft v1 for local-first distribution foundations  
**Scope:** Index metadata only; package creation and cryptographic signing are separate phases.

## Goals

- Define a small, machine-readable index format for discovering Synaps plugins.
- Let users and tooling inspect compatibility, permissions, hooks, commands, and source before install.
- Keep the format static-host friendly: a single JSON document can be served from a file, Git repo, or HTTPS URL.
- Make trust boundaries explicit without requiring a central marketplace.

## Non-goals

- No centralized registry requirement.
- No binary/package archive format in this spec.
- No mandatory signatures in v1; checksum/signing details are handled by `plugin-signing.md`.
- No remote execution during indexing or inspection.

## Top-level document

```json
{
  "schema_version": 1,
  "generated_at": "2026-05-01T12:00:00Z",
  "plugins": []
}
```

Fields:

- `schema_version` (number, required): currently `1`.
- `generated_at` (string, optional): RFC3339 timestamp for the generated index.
- `plugins` (array, required): plugin entries.

## Plugin entry schema

Each entry describes one installable plugin version:

```json
{
  "id": "session-memory",
  "name": "session-memory",
  "version": "0.1.0",
  "description": "Extracts local session notes from lifecycle transcripts.",
  "repository": "https://github.com/example/synaps-skills.git",
  "subdir": "session-memory-plugin",
  "license": "MIT",
  "categories": ["memory"],
  "keywords": ["local-first", "session"],
  "checksum": {
    "algorithm": "sha256",
    "value": "64-character-hex-plugin-tree-digest"
  },
  "compatibility": {
    "synaps": ">=0.1.0",
    "extension_protocol": "1"
  },
  "capabilities": {
    "skills": ["session-memory"],
    "has_extension": true,
    "permissions": ["session.lifecycle"],
    "hooks": ["on_session_end"],
    "commands": []
  },
  "trust": {
    "publisher": "Maha Media",
    "homepage": "https://example.com"
  }
}
```

Required fields:

- `id`: Stable lower-kebab-case plugin identifier. Prefer the plugin manifest `name`.
- `name`: Human-visible plugin name; may match `id`.
- `version`: Semver plugin version from `.synaps-plugin/plugin.json`.
- `description`: Short human-readable description.
- `repository`: Source repository URL or local file URL.
- `checksum`: Digest metadata for the installable plugin tree. v1 supports `sha256` only; `value` is the deterministic plugin tree digest described below. It must be a 64-character lowercase hex digest.
- `compatibility`: Minimum Synaps/runtime compatibility metadata.
- `capabilities`: Static capability summary.

Optional fields:

- `subdir`: Plugin directory within a repository.
- `license`: SPDX license identifier when known.
- `categories`, `keywords`: Marketplace/browse metadata.
- `trust`: Publisher metadata copied from plugin marketplace metadata when present.

## Checksum semantics

v1 index checksums are deterministic plugin tree digests, not git commit IDs.
For the effective plugin root (the fetched repository root, or `subdir` when an
entry points at a plugin inside a repository), tooling computes:

1. Recursively enumerate regular files under the plugin root.
2. Exclude any `.git` directory and its contents.
3. Ignore symlinks and non-regular files.
4. Sort relative file paths lexically.
5. Feed sha256 with, for each file: relative path bytes, a NUL byte, file bytes,
   and another NUL byte.

`plugin index generate --dry-run PATH` emits this checksum. Runtime install and
update flows fetch to a pending directory, recompute the same digest, and abort
before final install/update if it does not match the index metadata.

For index-backed updates, Synaps prefers the latest refreshed index checksum for
the plugin entry. If no refreshed index metadata is available, it falls back to
the checksum recorded when the plugin was installed.

## Capability summary

`capabilities` is derived from the plugin manifest and file layout:

- `skills`: skill directory names under `skills/`.
- `has_extension`: true when `.synaps-plugin/plugin.json` has an `extension` block.
- `permissions`: extension permissions, if any.
- `hooks`: extension hook names, if any.
- `commands`: plugin command names, if any.

Indexers should not execute extension code to populate these fields.

## Trust and security expectations

- Index consumers must treat repositories and plugin code as untrusted until the user approves install/enable.
- Executable extensions require a permission/trust confirmation before enablement.
- Index metadata is advisory. Installers must re-read the plugin manifest from fetched content and compare the permission summary shown to the user.
- Checksums are verified for index-backed install and update candidates before content is moved into the final plugin directory.
- Plugins must not receive secrets during indexing, inspection, or dry-run packaging.

## Install lifecycle

1. Fetch index JSON from a configured local path, Git source, or HTTPS URL.
2. Display entry metadata and capability summary.
3. Fetch selected plugin content to a pending-install location.
4. Recompute and verify the v1 plugin tree checksum from the pending content.
5. Inspect the fetched manifest locally.
6. Show permission/config/trust summary for executable extensions.
7. On user confirmation, move from pending install to the final plugin directory.
8. Extension loading uses normal Synaps manifest validation.

## Update lifecycle

1. Compare installed plugin version with index entry version.
2. Fetch candidate update to a pending-update location.
3. Verify checksum and inspect manifest.
4. Highlight new or changed permissions, hooks, commands, config keys, and executable command path.
5. Apply only after user confirmation.
6. Failed update leaves the existing installed plugin intact.

## Compatibility rules

- `compatibility.synaps` should be a simple semver/range string.
- `compatibility.extension_protocol` must match the active extension protocol for executable extensions.
- Installers may hide or warn on incompatible entries, but should explain why.

## Example minimal local index

```json
{
  "schema_version": 1,
  "plugins": [
    {
      "id": "policy-bundle",
      "name": "policy-bundle",
      "version": "0.1.0",
      "description": "Local safe tool policy bundle.",
      "repository": "file:///home/me/synaps-skills",
      "subdir": "policy-bundle-plugin",
      "checksum": {
        "algorithm": "sha256",
        "value": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
      },
      "compatibility": {"synaps": ">=0.1.0", "extension_protocol": "1"},
      "capabilities": {
        "skills": ["safe-tool-policy"],
        "has_extension": true,
        "permissions": ["tools.intercept"],
        "hooks": ["before_tool_call"],
        "commands": []
      }
    }
  ]
}
```
