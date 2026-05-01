# Phase O — Provider UX, trust, streaming contract, and distribution metadata

Status: implementation spec

## Objective

Build on Phase N's functional extension provider routing by making provider capabilities visible, trustable, and distribution-ready. Phase O keeps streaming/tool-use execution conservative: the contract names `provider.stream`, but the implementation must fail with an explicit unsupported error until notification semantics are implemented.

Success criteria:

- Install/update/detail UX flags `providers.register` as high-impact because selected provider models receive conversation content.
- `/extensions status` shows registered provider IDs, model IDs, and extension health.
- `/models` extension provider entries include provenance metadata.
- `provider.stream` is contractually reserved and runtime calls return a clear unsupported error rather than silently falling back.
- Plugin index/package metadata includes provider summaries from manifest-declared provider metadata.
- Builder validation checks manifest-declared provider metadata shape when present.
- The echo provider example remains valid and appears in generated index capabilities.

## Commands

```bash
cargo test --lib extensions
cargo test --lib skills::plugin_index
cargo test --bin synaps chatui::models
cargo test --bin synaps chatui::plugins
bash plugin-builder-plugin/scripts/test.sh
```

## Project structure

SynapsCLI:

- `docs/specs/extension-providers.md` — update with Phase O UX/distribution notes.
- `src/extensions/manager.rs` / `providers.rs` — provider status summaries.
- `src/extensions/runtime/*` — explicit `provider.stream` unsupported API.
- `src/chatui/models/mod.rs` — provenance metadata.
- `src/chatui/plugins/draw.rs` — trust/detail warnings.
- `src/skills/plugin_index.rs` and state conversion — provider metadata in indexes.

synaps-skills:

- `plugin-builder-plugin/lib/validate.sh` — validate `extension.providers` metadata.
- `plugin-builder-plugin/lib/package.sh` — print provider summary.
- `plugin-builder-plugin/lib/index.sh` — emit/inspect provider capability metadata.
- `plugin-builder-plugin/scripts/test.sh` — smoke coverage.

## Code style

Keep provider summaries as plain data:

```rust
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PluginIndexProviderCapability {
    pub id: String,
    pub models: Vec<String>,
}
```

## Testing strategy

- Rust unit tests for index provider capability validation.
- ChatUI model/plugin tests for warning/provenance output where practical.
- Builder smoke tests for package/index provider metadata.

## Boundaries

Always do:

- Fail closed on malformed provider metadata.
- Preserve provider-only hookless extensions.
- Keep streaming unsupported explicit until implemented.

Ask first:

- Implementing notification-based streaming.
- Allowing provider tool-use requests.
- Adding new dependencies.

Never do:

- Fall back to built-in providers for an extension model after provider failure.
- Hide `providers.register` impact in install/update/detail UX.
- Persist provider secrets outside existing config/env handling.
