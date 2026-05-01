## Platform Phase Continuation Plan

Convergence: none (per prior decision).

### Dependency graph
1. H1 hardening/polish: extension tool spec validation, process cleanup, builder dev support.
2. H2 commands: extension-backed and skill-prompt-backed slash commands.
3. H3 spec: provider registration design before code.
4. E2/E3/F3: config, trust inspection, marketplace metadata.
5. Phase I: memory/session intelligence foundations.
6. Phase J: marketplace/distribution foundations.
7. Later platform groundwork: richer marketplace, config UI/CLI, examples/templates, provider implementation after H3.

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


#### Phase I checkpoint: completed
Verification run:
- `cargo test --test contracts_sync`
- `cargo test --test extensions_contract`
- `cargo test --lib extensions`
- `cargo test --test extensions_e2e`
- `cargo test --test extensions_process`
- `cargo test --bin synaps chatui::commands::tests::plugin`
- `cargo test --bin synaps chatui::plugins`
- `bash plugin-builder-plugin/scripts/test.sh`

Results: all listed tests passed in the Phase I checkpoint run.

#### Phase J: Marketplace and distribution foundations
Acceptance:
- Plugin index schema spec exists at `docs/specs/plugin-index.md`.
- Checksums/signing design spec exists at `docs/specs/plugin-signing.md`.
- `plugin package --dry-run PATH` validates plugin metadata and prints files, permissions, hooks, config keys, skills, and commands without creating an archive.
Verification: `bash plugin-builder-plugin/scripts/test.sh`; spec files present. Scope M.

#### Phase K: Local marketplace/index consumption — completed
Acceptance:
- Rust plugin index model and validation exist.
- `plugin index validate|list|inspect` provide local index UX without remote fetch.
- Index-backed install entries route through pending install/trust flow.
- Manifest update diff helper reports capability changes.
Verification: targeted Rust tests and plugin-builder smoke passed during Phase K.

#### Phase L: Real marketplace/index UX + safe update flow — completed
Acceptance:
- Marketplace fetch accepts both legacy marketplace JSON and v1 plugin indexes.
- `/plugins` detail pane shows index metadata.
- Index-backed cached entries install through index flow.
- Updates preview manifest capability diffs and apply via backup/restore.
- `plugin index generate --dry-run PATH` emits v1 index JSON.
Verification: `cargo test --lib skills`, `cargo test --bin synaps chatui::plugins`, and `bash plugin-builder-plugin/scripts/test.sh` passed during Phase L.

#### Phase M: Distribution/update hardening — completed
Acceptance:
- Index-backed install/update candidates verify deterministic sha256 plugin-tree checksums before final install/update.
- `plugin index generate --dry-run` emits checksums using the same algorithm runtime verifies.
- Update preview tests cover real changed manifests and refreshed index checksums.
- Index-backed installed plugins do not show false update markers when remote HEAD is unknown but checksum metadata exists.
- Plugin index docs, checksum/signing docs, and plugin-builder README describe checksum generation and verification semantics.
- Index validation requires 64-character lowercase sha256 digests.
Verification:
- `cargo test --lib skills::install`
- `cargo test --lib skills::plugin_index`
- `cargo test --bin synaps chatui::plugins`
- `bash plugin-builder-plugin/scripts/test.sh`
Scope M.

#### Phase N: Provider/runtime capability expansion — planned next
Acceptance to define before code:
- Provider routing design refresh: provider IDs, model IDs, config, JSON-RPC methods, streaming, failures/timeouts/cancellation, trust/security.
- Runtime provider protocol methods.
- Model registry integration.
- Chat routing to extension-backed providers.
- Provider config UX.
- Example local provider plugin.
Verification: design spec before implementation, then targeted Rust/plugin-builder/example tests per slice.
