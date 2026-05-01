## Platform Phase Continuation Plan

Convergence: none (per prior decision).

### Dependency graph
1. H1 hardening/polish: extension tool spec validation, process cleanup, builder dev support.
2. H2 commands: extension-backed and skill-prompt-backed slash commands.
3. H3 spec: provider registration design before code.
4. E2/E3/F3: config, trust inspection, marketplace metadata.
5. Phase I: memory/session intelligence foundations.
6. Later platform groundwork: richer marketplace, config UI/CLI, examples/templates, provider implementation after H3.

### Immediate vertical slices

#### Task H1.5: Harden registered extension tool specs
Acceptance:
- Reject empty/duplicate tool names and empty descriptions.
- Reject non-object input_schema values.
- Failed post-initialize loads shut down child process.
- Tests cover invalid specs and cleanup-relevant failure path.
Verification: `cargo test --test extensions_e2e extension_tool` and process tests.
Files: `src/extensions/runtime/process.rs`, `src/extensions/manager.rs`, fixtures/tests. Scope M.

#### Task H1.6: plugin-builder dev support for tool.call
Acceptance:
- `plugin dev extension --tool-call NAME --input JSON [PATH]` initializes extension, calls `tool.call`, prints response.
- Smoke test covers tool-registering generated/example extension.
Verification: `bash plugin-builder-plugin/scripts/test.sh`. Scope M.

#### Task H2.1: Define command backend model
Acceptance:
- Manifest supports shell, extension tool, and skill prompt command backends.
- Existing shell-backed commands remain compatible.
- Validation driven by JSON contract where applicable.
Verification: targeted command tests. Scope M.

#### Task H2.2: Runtime command dispatch
Acceptance:
- `/plugin:cmd` can execute extension-backed commands via namespaced tools.
- `/plugin:cmd` can inject skill prompt content.
Verification: command integration tests. Scope M.

#### Task H3: Provider registration design spec
Acceptance:
- Spec documents protocol, permissions, lifecycle, trust/security, config, failure semantics, and phased rollout.
Verification: docs review and link from execution plan. Scope S.

#### Task E2: Extension config story
Acceptance:
- Manifest declares non-secret config keys and env-secret references.
- Initialize receives resolved config.
- CLI/docs explain local-first storage and secret handling.
Verification: Rust tests + builder validation smoke. Scope M.

#### Task E3: Trust/permission inspection
Acceptance:
- `plugin inspect` shows permissions/capabilities before enabling executable extensions.
- Install/enable path has confirmation affordance where present.
Verification: builder smoke/CLI tests. Scope M.

#### Task F3: Marketplace metadata
Acceptance:
- Recommended index fields finalized and validated.
- Builder templates include metadata.
Verification: builder smoke. Scope M.

#### Phase I: Memory/session intelligence foundation
Acceptance:
- Local-first memory/session spec and minimal API surfaces.
- No external service dependency.
Verification: docs/tests appropriate to chosen slice. Scope M.

Checkpoint after H1.5/H1.6: run Rust extension tests and plugin-builder smoke before proceeding.
