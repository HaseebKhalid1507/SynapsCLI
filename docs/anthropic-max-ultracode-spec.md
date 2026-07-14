# Anthropic Max and Ultracode specification

**Status:** implementation-ready specification
**Base:** `b1646af`
**Scope:** native `anthropic/<model-id>` requests only, except for explicit
non-regression requirements for Codex Max/Ultra.

## Objective and assumptions

Add genuine Anthropic effort `max` and a distinct Synaps logical mode named
`ultracode`. `ultracode` lowers to Anthropic effort `xhigh` and enables a
foreground standing orchestration workflow; it is never provider wire data.
Selections, capability decisions, execution plans, and request roles are typed.
Invalid combinations fail before credentials or network. Existing numeric
thinking budgets and Codex Max/Ultra semantics remain unchanged.

Assumptions:

- The native Anthropic Messages API remains the route; authentication remains
  exclusively through the credential broker.
- “Standing workflow” means model-directed use of the existing bounded
  subagent lifecycle, not eager worker spawning and not a new Anthropic field.
- A source-controlled, typed exact-model manifest is the only static authority
  for model-specific Max/Ultracode positives. Live metadata may narrow support,
  but cannot synthesize these flags unless its schema explicitly represents
  them and is separately specified and tested.
- The local Claude Code pane is read-only semantic evidence. It must not edit,
  generate, or apply implementation code.

## Evidence and exact positive citation

The exact production positive is `anthropic/claude-fable-5`. The local authoritative Claude Code 2.1.207 binary at `/home/jr/.local/share/claude/versions/2.1.207` (SHA-256 `85e7e988a392d859f90802ca21fb26e89d3c9ab527f5ed0b08df3955e34d5c83`) and matching settings schema at `/home/jr/.vscode/extensions/anthropic.claude-code-2.1.207-linux-x64/claude-code-settings.schema.json` prove that `max` is supported effort, UltraCode is xhigh plus workflows, and Fable 5 advertises `max_effort` plus `xhigh_effort`; the live Fable picker displays both Max and UltraCode. These paths and digest are citations only; no binary, credentials, or secrets are source-controlled. No neighboring or family model inherits this evidence.

## Evidence and explicit evidence gap

The local Claude Code behavior establishes the requested **semantic mapping**:
Anthropic Max emits `output_config.effort = "max"`; logical Ultracode emits
`"xhigh"` and activates standing orchestration. It does **not** establish which
exact model IDs support either positive capability.

Checked-in evidence inspected at this base:

- `crates/agent-core/src/core/models.rs:4-8` lists exactly
  `claude-sonnet-4-6`, `claude-fable-5`, `claude-opus-4-7`,
  `claude-opus-4-6`, and `claude-haiku-4-5-20251001` in `KNOWN_MODELS`.
- `crates/agent-core/src/core/models.rs:230-260` (including its tests) provides
  adaptive-thinking evidence for exact IDs, notably `claude-opus-4-7` and
  `claude-fable-5`.
- `crates/agent-engine/src/runtime/openai/catalog/anthropic.rs:101-114`
  classifies exact static IDs as adaptive named effort or fixed budget.
- `docs/anthropic-xai-reasoning-modes-spec.md:28-40` records only
  low/medium/high/xhigh for adaptive Anthropic models and no named effort for
  fixed-budget models.

**Resolved gap:** the authoritative 2.1.207 evidence cited above positively
associates exact `anthropic/claude-fable-5` with Anthropic `max` and Synaps
`ultracode`. Name recency, family, adaptive support, context size,
`KNOWN_MODELS` membership, and support for `xhigh` remain insufficient evidence
for any other model. Every future positive requires a reviewable adjacent
citation. Production must not enable Max/Ultracode for `claude-opus-4-7`, a
Fable near-match, or any other ID by inference.

## Exact semantics

### Typed domain

Use distinct closed types (names illustrative):

- `LogicalReasoningSelection`: existing levels plus provider-neutral `Max`,
  existing Codex `Ultra`, and new `Ultracode` (never alias the last two).
- `RequestRole`: `Foreground | Worker | Internal`.
- `AnthropicCapabilities`: exact model ID, named effort set, `max_supported`,
  `ultracode_supported`, provenance, and manifest schema version.
- `AnthropicEffort`: `Low | Medium | High | XHigh | Max`; no `Ultracode` variant.
- `AnthropicExecutionPlan`: qualified model, logical selection, role, thinking
  shape, optional wire effort, and `NoStandingWorkflow | StandingWorkflow`.

The serializer accepts a validated plan, never a raw logical selection or
string. Parsing is exact and case-sensitive; aliases are not accepted.

### Validation and lowering matrix

| Logical selection | Exact capability required | Wire | Workflow |
|---|---|---|---|
| Off/Adaptive/Low/Medium/High/XHigh | existing rules | existing exact behavior | none |
| Max | exact Anthropic Max positive | `output_config:{"effort":"max"}` with adaptive thinking | none |
| Ultracode, foreground | exact Ultracode positive, xhigh support, valid orchestration manifest, actionable lifecycle tools | `output_config:{"effort":"xhigh"}` with adaptive thinking | standing |
| Ultracode, worker/internal | irrelevant | deny before auth/network | forbidden |
| Codex Max/Ultra | existing Codex capability rules | unchanged (`max`; Ultra remains Codex logical mode) | unchanged |

`"ultracode"` must occur in persisted/UI logical state and diagnostics only. It
must never occur in JSON sent to Anthropic, headers, URLs, or broker payloads.
Max is genuine wire effort and must not silently lower to xhigh. Ultracode must
not silently become Max. Unknown provider, unknown exact model, malformed or
unknown manifest version/field value, absent row, missing positive flag, live
contradiction, unqualified identity, and provider mismatch all deny.

### Foreground-only standing workflow

A foreground Ultracode plan installs exactly one turn-scoped, typed standing
workflow policy using the existing orchestration layer. Its intent is:

> Keep bounded orchestration available throughout the foreground turn. Delegate
> independent or specialist work when it materially improves correctness or
> latency; retain synthesis and final responsibility in the foreground; await
> and collect required workers before final output.

This is policy/context plus existing tools, not an API effort value, eager pool,
background daemon, or permission bypass. Preflight requires a valid typed
orchestration manifest, provider-qualified worker authorization, configured
limits, and actionable start/status/steer/collect/resume tools. Existing
completion gates and limits apply.

Workers and internal requests are marked centrally and may never construct an
Ultracode plan, receive standing policy, or expose recursive subagent-start
capability. Restoring Ultracode into such a role is a stable fail-closed denial,
not a downgrade. Foreground-only means orchestration lives only for the active
foreground request/session lifecycle; no detached work survives completion,
cancellation, or process exit.

### Manifest and authority

Create a versioned typed manifest/table whose rows use canonical
`anthropic/<exact-id>` and explicit booleans/sets. Every production positive row
must carry an adjacent source-controlled evidence citation. Duplicate IDs,
unknown schema versions, unknown enum values, contradictory flags (for example
Ultracode without xhigh), malformed IDs, or live/static conflicts produce a
sanitized denial; they never fall back to broad family logic. Live rows take
precedence and may narrow a static row. No substring, prefix, date, display-name,
price, context-window, or neighboring-model inference is allowed.

### Preflight, broker, and diagnostics

Both mutation-time validation and request-boundary defense-in-depth construct
the same plan. Request preflight completes before OAuth refresh, broker
credential lookup, broker proxy dispatch, DNS, socket creation, or provider I/O.
Native Anthropic auth remains broker-only; engine/runtime code receives no raw
refresh/access token and adds no direct-key fallback.

Errors and traces may contain only qualified model ID, logical selection, role,
manifest version/provenance, sanitized capability booleans, decision, stable
code, prerequisite booleans, and `credentials_attempted=false` /
`network_attempted=false`. Never include prompts, generated policy text, body,
headers, tokens, account IDs, broker response, tool arguments/results, or raw
malformed manifest values.

### Persistence, resume, commands, and TUI

Persist logical `max`, `ultracode`, and Codex `ultra` distinctly. Never persist a
derived effort or workflow plan. Session save/load, compaction, continue, and
resume round-trip exact logical values with explicit-selection provenance;
model changes revalidate rather than remap. Legacy numeric `thinking_budget`
and `/thinking N` preserve exact budget values and existing mappings and never
map to Max or Ultracode.

`/thinking`, `/effort`, settings, headless/config, and resume use one validator.
For an exact Anthropic model, options are in this exact order:

`off, adaptive, low, medium, high, xhigh, max, ultracode`

filtered only by explicit exact capabilities and existing thinking shape. Thus
`max` appears only with Max positive; `ultracode` only with Ultracode+xhigh
positive. Codex option spelling/order and logical `ultra` are untouched.
Unknown models expose only the existing conservative options, never the two new
ones. A rejected apply leaves runtime and persistence unchanged.

Opening captures qualified model plus capability generation. Applying rechecks
streaming state, current model, capability generation, role, and prerequisites
atomically. While streaming, apply is denied. Model/catalog changes, stream
start, close/Escape, or stale modal generation cannot apply an old selection.
Only the event-loop apply path mutates and persists; duplicate key events and
stream-completion races are idempotent.

### Body shape, prompt cache, and numeric compatibility

Preserve the canonical body member order documented in
`runtime/request.rs`: `max_tokens`, `messages`, `model`, optional
`output_config`, optional `stream`, optional `system`, `thinking`, `tools`.
Adding Max/Ultracode must not reorder body members, message/content blocks,
system blocks, tools, or `cache_control` markers. Cache markers are attached
only after stable body content/policy composition through existing helpers, so
standing policy is included deterministically before cache marking and no marker
moves. Unchanged requests remain byte-identical under golden tests.

Fixed-budget Anthropic models retain `thinking:{type:"enabled",
budget_tokens:N,...}` and no `output_config`. Numeric budgets retain exact
integer value, validation, persistence, and cache shape. They do not gain Max or
Ultracode absent explicit compatible manifest support; the initial manifest has
none.

## Project structure and style

Expected implementation areas (final file placement may follow existing module
boundaries):

```text
crates/agent-core/src/reasoning.rs                 # logical Ultracode type/parse
crates/agent-engine/src/runtime/openai/catalog/anthropic.rs # typed manifest
crates/agent-engine/src/runtime/openai/catalog/validation.rs # shared validation
crates/agent-engine/src/runtime/request.rs         # plan-only Anthropic body
crates/agent-engine/src/runtime/{mod,api,api_sync}.rs # role/preflight/broker order
crates/agent-engine/src/tools/subagent/            # foreground/recursion boundary
crates/agent-tui/src/tui/{effort,settings,commands}.rs # exact options/race guards
```

Style example:

```rust
match (role, selection, caps.ultracode_supported) {
    (RequestRole::Foreground, LogicalSelection::Ultracode, true) =>
        Ok(AnthropicExecutionPlan::standing(AnthropicEffort::XHigh)),
    (_, LogicalSelection::Ultracode, _) => Err(PlanError::UltracodeDenied),
    _ => plan_existing_selection(selection, caps),
}
```

No wildcard may convert unknown capability/selection values into an allow.

## Testing strategy and acceptance

Strict RED-first: capture a failing assertion before production changes for each
slice. Unit/property tests cover parsing, exact-ID manifest validation, full
matrix, plan typing, and `ultracode` wire absence. Golden body tests cover Max,
Ultracode, unchanged levels, key/block/cache order, and numeric budgets.
Integration tests use fake broker and recording transport counters to prove
rejections precede auth/network. TUI tests cover exact options, stale generation,
stream-start/completion races, duplicate events, and unchanged persistence on
failure. Persistence tests distinguish Max/Ultracode/Codex Ultra across
save/compact/resume. Worker tests cover every spawn/resume/internal route and
recursive tool denial. Codex Max/Ultra regression tests are mandatory.

Success requires no inferred positive production ID, no literal `ultracode` in
captured wire, exact Max/xhigh wire, all zero-attempt assertions, byte-stable
unaffected bodies, and clean workspace checks.

## Boundaries

No live credentials/network tests, direct Anthropic auth, eager workers,
detached/background orchestration, recursive workers, undocumented API fields,
model-family inference, Codex semantic changes, numeric-budget migration,
unrelated refactor, global install, or push. Claude Code is read-only evidence,
never a coding agent for this increment.
