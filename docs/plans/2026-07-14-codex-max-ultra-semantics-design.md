# Codex Max/Ultra Execution Semantics Design

**Status:** Approved by the authoritative correction in the 2026-07-14 task
**Complexity:** Large
**Oracle:** OpenAI Codex `rust-v0.144.3` (`78ad6e6bfd1d3b6a209acd3ef82172a96b25179c`)

## Goal

Make Synaps preserve Max and Ultra as distinct logical, persisted Codex modes while emitting the exact Codex 0.144.3 network and multi-agent behavior. Max remains single-agent unless delegation is explicitly authorized. Ultra uses the same maximum wire reasoning effort and proactively delegates when parallel work materially helps.

## Authoritative behavior

The checked-out oracle is `/tmp/openai-codex-01443`.

- `codex-rs/core/src/client.rs::reasoning_effort_for_request` maps only `Ultra` to `Max`; Max remains Max.
- `codex-rs/core/src/client_tests.rs::ultra_reasoning_uses_max_for_requests` locks that lowering.
- `codex-rs/core/tests/suite/client.rs::includes_configured_max_effort_in_request` locks Max as wire `"max"`.
- `codex-rs/core/tests/suite/multi_agent_mode.rs::ultra_reasoning_uses_max_and_proactive_mode` locks Ultra as wire `"max"` plus one proactive developer item.
- `codex-rs/core/src/session/multi_agents.rs` derives `Proactive` only from logical Ultra when the exact model uses multi-agent v2; other foreground v2 turns are `ExplicitRequestOnly`.
- `codex-rs/core/src/context/multi_agent_mode_instructions.rs` owns the exact tagged texts.

The resulting contract is:

| Logical level | Wire effort | Foreground v2 mode |
|---|---|---|
| Off / Adaptive | omitted | explicit-request-only |
| Low / Medium / High | unchanged | explicit-request-only |
| XHigh | `xhigh` | explicit-request-only |
| Max | `max` | explicit-request-only |
| Ultra | `max` | proactive |

Models without multi-agent v2 receive no v2 mode developer item. In particular, Luna supports Max with v1 and does not support Ultra. Sol and Terra support Ultra with v2.

## Current defect

`crates/agent-engine/src/runtime/openai/stream.rs::build_codex_body` accepts a raw `ReasoningLevel` and serializes `level.as_str()`. That conflates the persisted selection vocabulary with the provider wire vocabulary and emits the invalid logical token `"ultra"`.

Synaps also discards `multi_agent_version` while parsing the Codex model catalog, so it cannot prove that an Ultra-capable exact model has the v2 protocol required for proactive mode.

## Design

### 1. Exact-model capability and typed execution plan

Extend the authoritative Codex capability object with a closed `CodexMultiAgentVersion` value. Preserve `v1`, `v2`, absent, and sanitized unknown states without retaining arbitrary server text. Live metadata remains authoritative over static fallback; a present live row missing or contradicting v2 must never borrow v2 from the static table.

Introduce a pure `CodexExecutionPlan` derived from:

- an exact provider-qualified identity (`openai-codex/<slug>`),
- the logical `ReasoningLevel`,
- the request role (`Foreground`, `Worker`, or `Internal`), and
- one authoritative live/static capability record.

The plan contains the logical mode, a closed wire-effort enum, capability provenance, multi-agent version, and optional mode protocol. The serializer accepts the plan rather than `ReasoningLevel`, making another direct logical-to-wire leak structurally difficult.

Ultra authorization requires the conjunction of exact Ultra support and multi-agent v2. Max requires exact Max support but does not require v2. Unknown models, wrong providers, and live rows lacking required evidence fail closed.

### 2. Request protocol composition

Do not invent a Responses API field. Following Codex 0.144.3, render the exact `<multi_agent_mode>…</multi_agent_mode>` fragment as one developer-role input item. Insert it before the current turn's final user message so it is turn-scoped and appears exactly once per request.

Keep the existing Synaps autonomous harness policy in `instructions`; it is unrelated to multi-agent mode. XHigh continues to serialize as `"xhigh"`. Max and Ultra both serialize as `"max"`.

### 3. Automatic delegation and authorization

Ultra is model-driven orchestration: the proactive developer item tells the model when to delegate, while the existing `subagent_start/status/steer/collect/resume` tools perform the work. Synaps does not pre-spawn a fixed worker pool.

A foreground Ultra request must have:

- installed session orchestration policy,
- actionable subagent start/status/collect tools, and
- an exact Ultra+v2 model plan.

Missing prerequisites fail before credential acquisition or network access. Existing provider-qualified worker selection, catalog authorization, concurrency limits, completion gates, and `network_attempted=false` denials remain unchanged.

### 4. Worker recursion boundary

Official multi-agent v2 children inherit Ultra and can recursively expose collaboration tools; v2 ignores the v1 `agents.max_depth` gate and is bounded by session slots. Synaps intentionally retains a stricter invariant: worker runtimes have an explicit worker request role and `ToolRegistry::without_subagent()`. A worker plan never emits proactive mode, even if logical Ultra is restored or assigned later. This is a documented Synaps safety difference, not claimed upstream parity.

### 5. Persistence and preflight

Persist only the logical selection already stored as `Session::thinking_level`; derive wire and mode behavior at each request. Boot and TUI resume restore the logical value as an explicit choice so model switches do not silently overwrite Max/Ultra.

Run shared reasoning/mode preflight before OAuth refresh, broker token access, or provider network work. The provider request path repeats exact plan construction before broker access as defense in depth.

### 6. Sanitized diagnostics

Emit structured mode-plan tracing with only:

- provider-qualified model,
- requested logical level and execution mode,
- wire effort,
- capability source and sanitized multi-agent version,
- runtime role and derived mode,
- prerequisite booleans,
- allow/deny decision and stable deny code,
- `network_attempted=false` on preflight denial.

Never log request bodies, prompts, user tasks, tool arguments/results, tokens, JWTs, account IDs, headers, or unknown raw catalog strings.

## Test strategy

Strict RED-before-GREEN applies. The first regression changes the existing Ultra request expectation from `"ultra"` to `"max"`; it must fail against the current serializer and its failure output is retained in the implementation report. Additional initially failing tests cover typed planning, v2 gating, exact developer fragments, single insertion, XHigh preservation, foreground prerequisites, worker suppression, pre-credential rejection, and explicit restore provenance.

Focused tests precede broader core, engine, TUI, workspace, release, formatting, ratchet, and diff checks. Only one heavy Cargo command runs at a time with `CARGO_BUILD_JOBS=8`.

## Non-goals

- No guessed request fields or backend headers.
- No fixed eager worker count.
- No relaxation of provider-qualified authorization.
- No persistence of derived wire state.
- No recursive worker tool exposure.
- No global install, push, or unrelated refactor.
