# Modular System-Prompt Injection and Foreman Orchestration Specification

**Status:** Draft  
**Schema target:** `synaps-prompt/1`  
**Product:** SynapsCLI  
**Primary objective:** Replace monolithic, manually concatenated system prompts with typed, modular, observable prompt composition backed by runtime-enforced orchestration policy.

## 1. Motivation

Current prompt experiments use a universal foreman kernel plus manually composed provider/model-family adapters. This has produced useful behavior, but it exposes several structural limitations:

- A single `--system <file>` string has no typed module boundaries or provenance.
- Model-family instructions are manually concatenated into complete prompt files.
- Important delegation rules depend on model compliance rather than runtime enforcement.
- A model may under-delegate, as observed when Sonnet completed a non-trivial project without workers.
- A model may misclassify a long-running worker as stalled and duplicate its assignment, as observed with Kimi.
- Exact prompt versions and hashes are not automatically preserved in session metadata.
- There is no first-class way to inspect why a module was selected or which policy won a conflict.
- Prompt quality cannot be measured objectively without structured orchestration telemetry.
- Provider wire defects and prompt-behavior failures are difficult to distinguish without safe structural diagnostics.

The next Synaps prompt architecture must keep the universal prompt lightweight while moving authorization, lifecycle invariants, provenance, and reproducibility into typed runtime machinery.

## 2. Goals

1. Compose system prompts from typed, versioned modules.
2. Select adapters using exact provider-qualified model identity.
3. Separate immutable runtime policy from mutable prompt-research guidance.
4. Enforce delegation authorization and foreground-model immutability at runtime.
5. Track the complete worker lifecycle and prevent premature completion.
6. Make effective prompt composition inspectable and reproducible.
7. Record non-secret orchestration telemetry for prompt evaluation.
8. Support model-family adapters without maintaining complete duplicated prompts.
9. Detect contradictory modules and resolve conflicts deterministically.
10. Provide zero-network conformance tests for foreman behavior.

## 3. Non-goals

- The prompt system does not replace provider wire protocols or credential handling.
- Prompt adapters must not work around runtime serialization or transport defects.
- The system does not authorize cross-provider delegation merely because credentials exist.
- The runtime must not silently change the foreground model.
- The system will not run uncontrolled infinite worker or cost loops.
- Initial implementation need not provide a general-purpose templating language.
- Prompt inspection must not expose secrets, credentials, private tool results, or hidden provider identity blocks.

## 4. Design principles

### 4.1 Runtime policy outranks prompt prose

Security, authorization, credential boundaries, foreground-model immutability, and worker allowlists are runtime invariants. Prompt text may explain these rules but cannot weaken them.

### 4.2 Provider-qualified identity is canonical

Provider and model identity must remain exact and qualified throughout adapter selection and worker authorization. No substring inference or prefix stripping is permitted.

Examples:

- `anthropic/claude-sonnet-5`
- `openai-codex/gpt-5.6-sol`
- `xai-auth/grok-4.5-latest`
- `google-gemini/gemini-3.1-pro-preview`
- `openrouter/z-ai/glm-5.2`
- `openrouter/moonshotai/kimi-k2.7-code`

### 4.3 Universal kernel plus small adapters

Synaps should maintain one universal foreman kernel and small provider, family, and model adapters. It should not maintain a separate full system prompt for every route.

### 4.4 External evidence outranks self-report

Worker reports and foreground claims are not proof. Completion requires inspected artifacts and fresh verification evidence.

### 4.5 Fresh contexts remain the experimental default

Hot reload may improve interactive iteration, but controlled prompt comparisons should use fresh Synaps processes, fresh contexts, fixed tasks, and external evaluation.

## 5. Typed prompt stack

The effective system prompt must be compiled from ordered layers:

```text
built-in safety/runtime contract
→ universal foreman kernel
→ provider adapter
→ model-family adapter
→ exact-model adapter
→ capability policy insert
→ session/task policy insert
→ user-supplied system module
```

A reference type model:

```rust
struct PromptStack {
    builtin: PromptModule,
    kernel: PromptModule,
    provider_adapter: Option<PromptModule>,
    family_adapter: Option<PromptModule>,
    model_adapter: Option<PromptModule>,
    capability_policy: CapabilityPolicy,
    delegation_policy: DelegationPolicy,
    task_policy: Option<TaskPolicy>,
    user_module: Option<PromptModule>,
}

struct PromptModule {
    id: PromptModuleId,
    version: Version,
    source: PromptModuleSource,
    priority: u16,
    selectors: PromptSelectors,
    requirements: PromptRequirements,
    content: String,
    sha256: String,
}
```

Every module must have:

- Stable ID.
- Semantic or monotonic version.
- Source and safe display path.
- Priority.
- Provider/model/capability selectors.
- Compatibility requirements.
- Content digest.
- Explicit mutability classification.

## 6. Prompt manifest

Synaps must support a declarative manifest:

```yaml
schema: synaps-prompt/1
kernel: foreman@0.2
adapters:
  - provider/openrouter@0.1
  - family/kimi@0.2
policies:
  delegation:
    mode: enforced
    allowed_providers:
      - openrouter
    allowed_models:
      - openrouter/moonshotai/kimi-k2.7-code
      - openrouter/deepseek/deepseek-v4-pro
      - openrouter/z-ai/glm-5.2
    allow_foreground_model_change: false
    max_concurrent_workers: 3
```

Launch syntax:

```bash
synaps --prompt-manifest path/to/kimi.yaml
```

The manifest compiler must validate all references and policies before the session starts.

### 6.1 Backward compatibility

`--system <PROMPT_OR_FILE>` remains supported. It should be represented internally as a final user module in the typed stack.

The current behavior must not silently change for users who do not opt into a manifest. Synaps may initially run modular policy in `advisory` mode by default.

## 7. Module selectors and adapter registry

Adapters must be registered and selected through typed selectors:

```rust
struct PromptAdapterSelector {
    provider: Option<ProviderId>,
    model_family: Option<ModelFamilyId>,
    exact_model: Option<QualifiedModelId>,
    required_capabilities: BTreeSet<CapabilityId>,
    priority: u16,
}
```

Selection order:

1. Built-in module.
2. Universal kernel.
3. Exact provider adapter.
4. Matching family adapter.
5. Exact-model adapter.
6. Capability and session policy modules.
7. User module.

Ambiguous adapters at the same specificity and priority must fail validation. Synaps must not guess.

## 8. Immutable and mutable modules

### 8.1 Immutable runtime policy

The following rules cannot be overridden by prompt modules:

- Credential and secret non-egress.
- Broker ownership of refresh tokens, static keys, and cloud credentials.
- Provider and model authorization.
- Cross-provider delegation authorization.
- Foreground-model immutability.
- Filesystem/network restrictions imposed by session policy.
- Worker concurrency and budget ceilings.
- Secret-safe logging and telemetry.

### 8.2 Mutable research guidance

The following may be changed by versioned adapters:

- Preferred decomposition style.
- Polling cadence and backoff guidance.
- How aggressively to delegate.
- When to steer a worker.
- Model-family communication style.
- Preferred verification sequencing.
- Stop conditions after green evidence.

Mutable modules cannot override immutable policy even if their prose conflicts.

## 9. Delegation policy

Delegation must be runtime-authorized using a typed policy:

```rust
struct DelegationPolicy {
    mode: EnforcementMode,
    allowed_providers: BTreeSet<ProviderId>,
    allowed_models: BTreeSet<QualifiedModelId>,
    allow_cross_provider: bool,
    allow_foreground_model_change: bool,
    max_concurrent_workers: usize,
    max_total_workers: usize,
    worker_timeout: Duration,
    session_budget: Option<Budget>,
}

enum EnforcementMode {
    Off,
    Advisory,
    Enforced,
}
```

### 9.1 Default authorization

- Same-provider worker tier changes may be allowed by default when catalog and cost policy permit.
- Cross-provider delegation requires explicit user authorization.
- Authentication or model availability is not authorization.
- Worker selection never mutates the foreground model.

### 9.2 OpenRouter example

A session may authorize only:

- Kimi: smart.
- DeepSeek V4 Pro: smarter than Kimi but below GLM.
- GLM: genius.

The runtime must reject every other OpenRouter worker model for that session.

### 9.3 Dispatch validation

Before network traffic or billing occurs, Synaps must validate:

- Provider is allowed.
- Qualified model ID is allowed.
- Worker concurrency and total limits are satisfied.
- Cross-provider permission exists where required.
- Foreground model remains unchanged.
- Requested worker role and write policy are valid.

Denials must be typed, concise, and secret-safe.

## 10. Task-class delegation requirements

The task policy may require delegation for specific classes of work:

```yaml
delegation:
  mode: enforced
  required_when:
    - task_class: project_build
      minimum_workers: 1
    - task_class: security_change
      required_roles: [independent_reviewer]
    - changed_files_gte: 4
      minimum_workers: 1
```

Initial task classes:

- `trivial_action`
- `bug_fix`
- `project_build`
- `security_change`
- `architecture_change`
- `independent_review`
- `research`

Classification may be explicit in harnesses and advisory in ordinary interactive sessions. A false-positive classifier must not silently incur cost in enforced mode without user/session authorization.

## 11. First-class worker roles

Worker dispatch must carry a typed role:

```rust
enum WorkerRole {
    Planner,
    Implementer,
    Tester,
    Reviewer,
    Researcher,
    Debugger,
}

enum WorkerWritePolicy {
    ReadOnly,
    IsolatedWorktree,
    NonOverlappingPaths(Vec<PathSpec>),
}
```

Example dispatch:

```json
{
  "role": "reviewer",
  "model": "anthropic/claude-sonnet-5",
  "scope": ["index.html", "game.js"],
  "write_policy": "read_only",
  "acceptance_checks": [
    "node verify.js",
    "headless browser smoke test"
  ]
}
```

The runtime should warn or deny concurrent writers whose declared paths overlap.

## 12. Worker lifecycle state machine

Synaps must track workers explicitly:

```text
Dispatched
→ Running
→ Polled
→ Steered (zero or more times)
→ Terminal
→ Collected
→ Reconciled
```

A worker may enter a typed failure or timeout terminal state, but terminal workers must still be collected and reconciled.

### 12.1 Required invariants

- Every dispatch returns and records a stable handle.
- Active workers are polled fairly.
- A long-running tool call alone is not evidence of a stall.
- Stall diagnosis requires repeated unchanged polls and, where useful, steering.
- Completed workers are surfaced prominently for collection.
- Foreground completion is blocked or warned while required workers remain running or uncollected.
- Worker reports are inspected; they are not accepted as proof.
- Reconciliation is explicit before final completion.

### 12.2 Foreground overlap detection

When a worker owns a write scope, Synaps should detect foreground edits to overlapping files and:

- Warn in advisory mode.
- Require explicit reconciliation in enforced mode.
- Deny unsafe concurrent writes when policy requires isolation.

This directly guards against a foreman assuming a worker is stuck and silently duplicating its assignment.

## 13. Model-family orchestration expectations

### 13.1 Anthropic/Sonnet

For non-trivial project builds:

- Dispatch at least one same-provider worker before substantial implementation.
- Prefer independent implementation/review and adversarial-verification roles when scope permits.
- Keep the foreground focused on decomposition, tracking, integration, and verification.

### 13.2 Kimi

- Do not infer a stall from elapsed time or a long tool call.
- Poll repeatedly and compare progress.
- Steer before replacing or duplicating an active worker.
- Perform only non-overlapping foreground work while waiting.
- Collect and reconcile every terminal worker promptly.

These adapters are behavioral guidance. Runtime lifecycle enforcement remains authoritative.

## 14. Prompt conflict resolution

The prompt compiler must detect contradictions such as:

- “Always delegate” versus “Never use workers.”
- Cross-provider delegation requested while session policy denies it.
- “Conclude promptly” while required workers remain active.
- Two adapters defining incompatible polling or completion requirements.

Precedence:

1. Immutable safety and credential policy.
2. Explicit session authorization.
3. Task policy.
4. Exact-model adapter.
5. Family adapter.
6. Provider adapter.
7. Universal kernel.
8. User prose.

Policy conflicts must be reported before launch. Prose conflicts should produce a warning with module IDs and source locations.

## 15. Effective-prompt inspection

Required CLI and TUI interfaces:

```bash
synaps prompt validate manifest.yaml
synaps prompt inspect
synaps prompt inspect --json
synaps prompt explain adapter.kimi.foreman
```

TUI command:

```text
/prompt
```

Inspection must show:

- Ordered module IDs and versions.
- Safe source paths.
- SHA-256 digests.
- Applied selectors.
- Provider and qualified model identity.
- Capability/session policy inserts.
- Conflict resolution and overridden fields.
- Final byte and token estimates.
- Whether each important rule is prose-only, advisory, or runtime-enforced.

Inspection must not expose credentials, private tool results, or hidden identity material.

## 16. Session provenance

Each session must record safe prompt provenance:

```json
{
  "prompt_schema": "synaps-prompt/1",
  "prompt_stack": [
    {
      "id": "kernel.foreman",
      "version": "0.2.0",
      "sha256": "..."
    },
    {
      "id": "adapter.anthropic",
      "version": "0.2.0",
      "sha256": "..."
    }
  ],
  "delegation_policy_digest": "...",
  "foreground_model": "anthropic/claude-sonnet-5"
}
```

The session must record prompt changes as events. Secret-bearing dynamic content should be represented by safe metadata or a redacted digest, not raw content.

## 17. Hot reload

Optional commands:

```text
/prompt reload
/prompt apply adapter.kimi@0.3
```

Requirements:

- Show the module/policy diff before application.
- Require explicit confirmation where policy changes affect cost or provider scope.
- Record the change in session provenance.
- Never silently alter the foreground model.
- Never weaken immutable policy.
- Indicate that existing history was generated under a previous stack.

Fresh-context respawn remains the required mode for controlled prompt experiments.

## 18. Orchestration telemetry

Synaps should emit structured, non-secret events:

```text
prompt.stack_compiled
prompt.module_selected
prompt.conflict_detected
worker.dispatch_requested
worker.dispatch_denied
worker.dispatched
worker.polled
worker.steered
worker.terminal
worker.collected
worker.reconciled
foreground.scope_overlap
completion.blocked_by_workers
verification.started
verification.finished
```

Useful evaluation metrics:

- Time from task start to first dispatch.
- Dispatches per task class.
- Time to first worker poll.
- Poll count per worker.
- Steering count and timing.
- Workers abandoned or never collected.
- Foreground/worker path overlap.
- Percentage of implementation performed before delegation.
- Completion attempts while workers remain active.
- Verification commands and result state.
- Foreground versus worker tokens, latency, and cost.
- Median, minimum, and completion reliability across model families.

Telemetry must not contain prompts, credentials, raw tool output, or sensitive file contents by default.

## 19. Provider diagnostics

Provider errors should include bounded, redacted structural diagnostics sufficient to distinguish prompt failures from wire failures.

Example Gemini diagnostic:

```text
provider: google-gemini
model: gemini-3.1-pro-preview
turn_roles: [user, model, user]
part_kinds: [text, functionCall, functionResponse]
function_response_shape: object
stream_terminal_seen: false
```

Never log:

- Access or refresh tokens.
- API keys.
- AWS credentials.
- Prompt text.
- Tool output.
- Function argument contents.
- User content.

## 20. Conformance harness

A zero-network harness must test runtime enforcement and prompt behavior separately.

### 20.1 Sonnet under-delegation case

Given a non-trivial project build:

- Require at least one same-provider worker.
- Assert dispatch occurs before substantial writes.
- Assert the worker reaches terminal and collected states.

### 20.2 Kimi premature-takeover case

Simulate a worker that remains active through several polls:

- Assert the foreground polls repeatedly.
- Assert elapsed time alone does not classify it as stalled.
- Assert steering occurs before replacement when progress is unchanged.
- Assert the foreground does not edit the worker-owned files concurrently.
- Assert the worker is collected before completion.

### 20.3 Provider authorization case

Offer an attractive cross-provider worker without permission:

- Assert dispatch is denied before network activity.
- Assert the foreground model remains unchanged.

### 20.4 Completion-gating case

Leave one required worker active or terminal-but-uncollected:

- Assert completion is blocked in enforced mode.
- Assert a clear warning is emitted in advisory mode.

### 20.5 Prompt conflict case

Provide contradictory modules:

- Assert immutable/session policy wins.
- Assert the conflict identifies both module IDs.
- Assert ambiguous equal-priority adapters fail closed.

### 20.6 Provenance case

Compile a known manifest:

- Assert deterministic module ordering.
- Assert stable content hashes.
- Assert session metadata contains the exact stack and foreground model.

## 21. Security requirements

- Prompt modules are untrusted configuration until validated.
- Module paths must be confined according to the same path policies used for other configuration artifacts.
- Remote prompt modules are out of scope initially; if introduced, they require pinning and signature/digest validation.
- Prompt inspection and telemetry must be secret-safe.
- Cross-provider delegation must be explicitly authorized because it may change billing, retention, privacy, and policy boundaries.
- Credential broker invariants remain unchanged.
- Worker dispatch must not vend provider credentials to remote clients.
- Runtime enforcement must fail closed on malformed manifests, unknown provider IDs, ambiguous adapters, or invalid model IDs.

## 22. Performance and limits

The compiler must enforce configurable limits:

- Maximum module count.
- Maximum individual module size.
- Maximum composed prompt size.
- Maximum conflict count reported.
- Maximum worker count and concurrency.
- Bounded telemetry fields and event sizes.

Prompt compilation should be deterministic and inexpensive relative to provider startup. Content hashes may be cached by path, modification time, and size, while final correctness must not rely solely on metadata.

## 23. Migration plan

### Phase 1: Typed composition and inspection

- Introduce `PromptModule`, `PromptStack`, and manifest parsing.
- Preserve `--system` as a user module.
- Add exact provider/model selectors.
- Add `prompt validate` and `prompt inspect`.
- Record prompt provenance in sessions.

### Phase 2: Delegation authorization

- Add typed `DelegationPolicy`.
- Enforce provider/model allowlists and foreground immutability.
- Add advisory and enforced modes.
- Emit dispatch telemetry.

### Phase 3: Worker lifecycle

- Add lifecycle state tracking.
- Add completion gating.
- Add polling/collection reminders.
- Add declared worker roles and write scopes.
- Add overlap detection.

### Phase 4: Experimental harness

- Add zero-network foreman simulations.
- Add fixed task suites and objective metrics.
- Add deterministic experiment manifests and result records.

### Phase 5: Hot reload and advanced policy

- Add safe prompt reload/apply commands.
- Add task-class requirements and cost policy integration.
- Add richer structural provider diagnostics.

## 24. Acceptance criteria for the next build

The first production-worthy increment is complete when:

1. Synaps can compile a `synaps-prompt/1` manifest into a deterministic prompt stack.
2. `--system` remains backward compatible.
3. Exact provider-qualified IDs select the correct adapters without substring inference.
4. `prompt inspect --json` reports ordered modules, versions, hashes, selectors, and enforcement state.
5. Session metadata records the exact prompt stack and delegation-policy digest.
6. Runtime delegation policy rejects unauthorized providers/models before network activity.
7. Worker model selection cannot mutate the foreground model.
8. Required active or uncollected workers block completion in enforced mode.
9. Zero-network tests reproduce and prevent Sonnet under-delegation and Kimi premature takeover.
10. Prompt, provider, and orchestration diagnostics remain credential- and content-safe.
11. Existing credential-broker and provider-qualified model identity tests remain green.
12. A fresh-context six-model experiment can be reproduced from manifests without manually composed prompt files.

## 25. Recommended initial implementation scope

Prioritize the following in the next build:

1. `synaps-prompt/1` manifest and typed prompt stack.
2. Exact provider/model adapter registry.
3. Runtime worker provider/model allowlists.
4. Foreground-model immutability enforcement.
5. Worker lifecycle tracking and completion gating.
6. `/prompt` and `synaps prompt inspect`.
7. Session provenance and structured orchestration telemetry.
8. Zero-network foreman conformance tests.

This scope preserves a lightweight system prompt while moving critical authorization, lifecycle, and reproducibility guarantees into typed, testable runtime components.
