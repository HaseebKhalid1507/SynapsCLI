# Phase 2 — Local-first capability platform

Status: planning doc

## Objective

Phase 1 established plugins as the parent object for skills and extensions, hardened hook execution, introduced standalone extension tools, and made extension-provided model providers installable, visible, and routable through `provider.complete`.

Phase 2 turns those foundations into a broader local-first capability platform. The goal is for plugins to safely provide model backends, tools, hooks, memory, voice, and agentic workflows while Synaps remains the policy-enforcing mediator.

Phase 2 should stay incremental. Each phase slice must ship a user-visible capability, tests, and contract documentation. Avoid large framework rewrites unless required by an immediate feature.

## Guiding principles

- Plugins are the parent object; skills and extensions are children.
- Extensions remain standalone; no forced companion skill.
- Capabilities are driven by `docs/extensions/contract.json` and `contracts/extensions.json`, not hardcoded hook or permission names.
- Synaps remains the security boundary and mediator for tools, providers, hooks, and future capabilities.
- Provider models are high-impact because they receive conversation content.
- Capability names and model IDs remain namespaced by plugin ID where needed.
- Local-first by default: prefer filesystem, localhost, and user-controlled config over hosted services.
- YAGNI: each slice should be minimal, testable, and reversible.

## Phase 2 candidate slices

### P — Provider tool-use support

Goal: make extension-provided models first-class agentic model backends by allowing them to request tool calls through Synaps.

Current state:

- Extension providers can register models with `providers.register`.
- Synaps routes `plugin-id:provider-id:model-id` through `provider.complete`.
- `provider.stream` is reserved and unsupported.
- Provider tool-use was deferred from Phase O.

Deliverables:

- Specify provider tool-call semantics in `docs/specs/extension-providers.md`.
- Extend provider completion result/request types to support an iterative tool-use loop.
- Expose available Synaps tool schemas to provider completions only when appropriate.
- Allow provider-requested tool calls only through Synaps mediation.
- Apply existing tool permission and hook flow:
  - `before_tool_call`
  - tool execution
  - `after_tool_call`
- Add max iteration/depth limits to prevent infinite tool loops.
- Add fixture provider tests for:
  - successful tool request
  - blocked tool request
  - unknown tool request
  - malformed tool request
  - max-iteration failure
- Update install/status/package UX if a provider declares tool-use support.

Security boundaries:

- Providers cannot directly invoke tools.
- Providers cannot bypass hooks or permission checks.
- Provider-requested tool names must resolve exactly to registered namespaced tools.
- Blocked tool calls stop or fail the provider loop; they must not be silently retried through another path.
- Tool results returned to providers should match the same visibility rules used for normal model tool-use.

Open questions:

- Should tool-use be declared as provider metadata, e.g. `tool_use: true`, or inferred from runtime requests?
- Should built-in tools and extension tools be exposed together, or should provider access be separately scoped?
- Should provider tool-use be disabled by default until users explicitly trust the provider?

### Q — Provider streaming protocol

Goal: activate `provider.stream` with explicit JSON-RPC streaming semantics.

Deliverables:

- Define notification/event semantics for provider streaming.
- Support text deltas, final message, usage metadata, provider errors, and cancellation.
- Route stream chunks into existing `LlmStreamEvent` handling.
- Add a fixture provider that emits delayed chunks.
- Add tests for happy path, provider error, timeout, cancellation, and malformed events.
- Keep `provider.complete` as the non-streaming fallback for providers that do not implement streaming.

Design constraints:

- No implicit fallback from an extension model to built-in providers after provider failure.
- Stream cancellation must terminate or detach the extension request safely.
- Notification handling must be precise before marking `provider.stream` active in the contract.

Open questions:

- Should streaming be request/response plus notifications, or a long-running JSON-RPC method with framed events?
- How should partial provider usage metadata be represented?
- What heartbeat or idle timeout is appropriate for local providers?

### R — Provider configuration and trust UX

Goal: make provider installation, configuration, and diagnosis clear for users.

Deliverables:

- `/extensions status` shows missing required provider config without leaking secret values.
- Add config inspection UX for extension config schema:
  - required keys
  - defaults
  - env override names
  - secret env hints
- Improve install/update confirmation for providers that need API keys, network access, or local endpoints.
- Add redaction helpers for provider config display.
- Add docs for configuring provider examples.

Design constraints:

- Do not persist secrets outside existing config/env mechanisms.
- Redact secrets aggressively in logs, status, and errors.
- Preserve `SYNAPS_BASE_DIR` override behavior.

Open questions:

- Should Synaps provide an interactive config editor, or only status/docs guidance?
- Should plugins declare network access as distribution metadata?

### S — Provider and extension trust hardening

Goal: improve auditability and user control for high-impact capabilities.

Deliverables:

- Per-provider enable/disable controls.
- Optional provider allowlist in local config/state.
- Audit log entries for provider invocation:
  - timestamp
  - plugin ID
  - provider ID
  - model ID
  - whether tools were exposed/requested
- Stronger warnings for providers that receive conversation content and can request tools.
- Security review checklist for `providers.register` and provider tool-use.

Design constraints:

- Trust decisions should be local and user-owned.
- Disablement must prevent routing before provider IPC starts.
- Audit logs must not store full prompts or tool payloads by default.

Open questions:

- Where should capability audit logs live under `SYNAPS_BASE_DIR`?
- Should trust decisions be per plugin, per provider, or per model?

### T — Unified capability registry and lifecycle

Goal: make tools, hooks, providers, and future extension capabilities visible through one lifecycle model without over-abstracting prematurely.

Deliverables:

- Unified status surface for extension capabilities:
  - hooks
  - tools
  - providers
  - future memory/indexer/voice capabilities
- Health model for extension processes:
  - loaded
  - failed validation
  - failed initialize
  - running
  - degraded
- Restart/backoff policy for extension processes where needed.
- Contract-driven capability validation helpers shared across tools/providers/hooks.

Design constraints:

- Avoid rewriting stable registries unless a current feature needs it.
- Keep capability-specific runtime behavior explicit.
- Preserve existing namespacing rules.

Open questions:

- Is a formal `CapabilityRegistry` needed now, or can status aggregation stay lightweight?
- Which lifecycle events should be sent to extensions, if any?

### U — Local memory and indexing capabilities

Goal: let plugins contribute local-first memory and indexing behavior safely.

Possible deliverables:

- Extension capability for local memory append/query.
- Extension capability for local indexers over user-approved paths.
- Contract and permissions for filesystem/index access.
- Session memory reference plugin using `$SYNAPS_BASE_DIR/memory/session-notes.jsonl`.
- Status and trust UX for memory/indexing capabilities.

Design constraints:

- User-approved paths only.
- No silent indexing of arbitrary home directory content.
- Clear retention/deletion story.
- Local-first storage under `SYNAPS_BASE_DIR` unless user config says otherwise.

Open questions:

- Should memory be a core Synaps API first, with extension hooks later?
- What minimum query interface is useful without building a full vector database layer?

### V — Voice sidecar integration

Goal: integrate voice capabilities as extension-provided local services without coupling them to provider routing.

Possible deliverables:

- Voice extension capability metadata.
- Local sidecar lifecycle/status integration.
- Voice command toggle integration.
- Trust/config UX for microphone and audio output access.

Design constraints:

- Microphone access must be explicit and visible.
- Voice capabilities should be independently installable from skills/providers.
- Do not block provider/tool work on voice abstractions.

Open questions:

- Should voice be modeled as tools, hooks, provider-like streams, or a separate capability class?
- What contract is needed for push-to-talk vs continuous listening?

### W — Subagent and workflow integration

Goal: allow plugin capabilities to participate in multi-step local workflows and subagent execution.

Possible deliverables:

- Use extension provider models as subagent model backends.
- Permit plugin commands to launch constrained workflows.
- Define workflow trust and audit semantics.
- Add examples that combine:
  - skill prompt
  - extension tools
  - extension provider model
  - local memory

Design constraints:

- Workflows must not bypass the central permission/hook system.
- Long-running workflows need cancellation and status.
- Keep command dispatch form `/plugin-name:cmd`.

Open questions:

- Are workflows core Synaps behavior or just plugin commands with better UX?
- What minimum workflow metadata should distribution indexes expose?

## Recommended Phase 2 ordering

1. **P — Provider tool-use support**
   - Completes the first-class provider story and picks up Phase O's deferred item.
2. **Q — Provider streaming protocol**
   - Improves UX once providers can behave agentically.
3. **R/S — Provider config and trust hardening**
   - Polish and harden the provider surface after tool-use/streaming increase impact.
4. **T — Unified capability registry and lifecycle**
   - Consolidate once tools/hooks/providers have enough shared behavior to justify it.
5. **U/V/W — Memory, voice, and workflows**
   - Expand the platform into broader local-first capabilities.

This ordering is not mandatory. If users are blocked by configuration friction, move R earlier. If streaming UX is the highest pain point, swap P and Q. If security concerns rise after provider tool-use, split S immediately after P.

## Phase 2 success criteria

By the end of Phase 2, Synaps should support:

- Extension-provided providers that can complete, stream, and request tools through Synaps mediation.
- Clear trust/config/status UX for high-impact provider capabilities.
- A documented and tested lifecycle for extension capabilities.
- A path for local-first memory, voice, and workflow capabilities without requiring hosted services.
- Distribution metadata that accurately describes plugin capabilities before install.

## Non-goals

- Remote marketplace hosting.
- Mandatory cloud accounts or hosted services.
- Arbitrary extension network sandboxing beyond explicit metadata/trust UX.
- Full plugin workflow engine before concrete workflow examples exist.
- Provider streaming over undocumented notification semantics.
- Any provider or tool path that bypasses Synaps hooks and permissions.

## Verification targets

Each slice should include its own specific verification commands. Phase-wide expected coverage should include:

```bash
cargo test --lib extensions
cargo test --lib skills::plugin_index
cargo test --bin synaps chatui::models
cargo test --bin synaps chatui::plugins
bash plugin-builder-plugin/scripts/test.sh
```

Additional tests should be added per capability, especially E2E fixture extensions for provider tool-use and streaming.

## Slice progress (running tally)

- Slice P (Provider tool-use) — landed.
- Slice Q (Provider streaming) — landed.
- Slice R (Provider config UX) — landed.
- Slice S (Provider trust + audit) — landed.
- Slice T (Unified capability surface) — landed: expanded health states, restart backoff, capability snapshot, shared validation helpers.
- Slice U (Local memory) — partial: append/query store and protocol landed; indexer + shared namespaces deferred.
- Slice V (Voice sidecar integration) — pending (voice plugin already lives in `synaps-skills`).
- Slice W (Subagent/workflow integration) — pending.
