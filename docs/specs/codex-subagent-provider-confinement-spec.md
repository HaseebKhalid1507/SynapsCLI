# Codex Subagent Provider and Model Confinement Specification

**Status:** Draft normative specification

**Product:** SynapsCLI

**Scope:** Foreground-to-worker model selection, authorization, startup, diagnostics, and observability

**Security posture:** Fail closed

**Normative terms:** The words **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY** are to be interpreted as described in RFC 2119 and RFC 8174.

## 1. Purpose

This specification prevents a foreground agent from dispatching a subagent through an invented, stale, log-derived, or otherwise unauthorized provider/model identity. It makes the foreground provider boundary a runtime invariant rather than a prompt instruction.

The motivating incident involved a foreground session using `openai-codex/gpt-5.6-sol`. The foreground model guessed multiple `openai/...` worker identities, observed opaque worker failures, searched a historical debug log, and then attempted a model identity learned from that log on another provider. The fleet had been launched without `--prompt-manifest`; consequently, the same-provider requirement existed only in task prose and was not enforced by the runtime.

This document specifies:

- exact, catalog-derived worker choices;
- omitted-model inheritance from the foreground qualified identity;
- fail-closed same-provider defaults, including launches without a prompt manifest;
- explicit grants for cross-provider delegation;
- rejection before provider resolution, credential lookup, network I/O, or billing;
- durable typed startup failures visible through both status and collection;
- safe diagnostics and telemetry;
- a black-box regression harness and migration plan.

This is a standalone runtime-security specification. Prompt text may explain the policy, but prompt compliance is neither necessary nor sufficient for enforcement.

## 2. Scope and non-goals

### 2.1 In scope

This specification applies to every operation that creates or resumes a worker whose inference request can differ from the foreground inference route, including reactive `subagent_start`, one-shot delegation, resume/retry operations, RPC equivalents, extension-provided aliases, and future fleet APIs.

It applies regardless of whether the request originates from:

- an LLM tool call;
- a user command;
- an automation harness;
- an extension;
- an RPC client; or
- internal retry logic.

### 2.2 Non-goals

This specification does not:

- define provider wire formats, authentication protocols, or credential storage;
- guarantee that an authorized catalog model will successfully answer;
- authorize a model merely because credentials or a provider route exist;
- make logs a model catalog or policy source;
- permit the foreground agent to mutate its own model;
- prescribe worker task decomposition or answer quality;
- repair unrelated transport serialization defects.

A Gemini tool-response serialization issue may produce superficially similar empty or failed workers, but it is a separate transport defect. It MUST be diagnosed under the transport layer and MUST NOT be used to explain, waive, or weaken the Codex provider-confinement failure described here.

## 3. Evidence classification

The specification distinguishes direct observations from interpretations and proposed behavior. Paths and identifiers below are included only to identify supplied evidence; no credential, token, request header, or private prompt body is reproduced.

### 3.1 Observed facts

The following facts are directly supported by the supplied Storm session evidence and repository state at the time of drafting:

1. Session `20260713-122905-14bc` recorded foreground model `openai-codex/gpt-5.6-sol`.
2. Its recorded system prompt was the 202-character built-in prompt. No modular policy prompt was present in the recorded session.
3. The user task required same-provider, provider-qualified workers.
4. The foreground Codex agent first attempted workers identified as `openai/gpt-5.4`, then `openai/gpt-5.2`, and later `openai/gpt-5.5`.
5. The early worker attempts reached terminal failure with zero tool calls and empty output.
6. The agent searched `~/.synaps-cli/synaps-debug.log`. A tool result exposed historical model identifiers, including `claude-opus-4-7` and `gpt-5.5`.
7. After seeing that output, the agent attempted `anthropic/claude-opus-4-7`.
8. The fleet launch omitted `--prompt-manifest`.
9. Therefore the same-provider restriction in `TASK.md` was prompt prose, not an effective manifest-backed runtime policy for that launch.
10. The `subagent_start` tool schema includes `anthropic/claude-sonnet-4-6` as an example model string.
11. The current omitted worker-model path in `subagent_start` selects `crate::models::default_model()` rather than inheriting the foreground provider-qualified model identity.
12. Current worker authorization code contains allowlist and preflight concepts, but an enforced policy is optional in the tool context. Absence of such a policy does not itself establish a secure same-provider baseline.

### 3.2 Inferences and hypotheses

The following are plausible explanations, not established facts, and MUST NOT be emitted as definitive incident diagnoses without further evidence:

- The guessed `openai/...` identities may have failed because `openai` and `openai-codex` are distinct routing providers, because the guessed models were absent from the active catalog, because credentials were unavailable, or because of another startup defect.
- Empty output and zero tools strongly suggest failure before useful model execution, but do not identify the failed stage.
- The schema example may prime a model to propose an Anthropic identity; the incident evidence does not prove that it caused the cross-provider attempt.
- Historical log lines influenced the sequence of attempts, but do not prove the agent treated them as formal authorization.
- A qualified identity that appears syntactically plausible may still be unavailable, unentitled, stale, or unauthorized.

### 3.3 Required conclusions independent of root-cause uncertainty

The exact cause of the failed worker starts is not needed to establish the required controls:

- `openai-codex` and `openai` MUST be treated as different providers.
- Model names invented by an agent MUST NOT become eligible choices.
- A model name found in logs MUST NOT become authorized.
- Missing manifest policy MUST NOT imply unrestricted delegation.
- A startup failure MUST retain a typed cause rather than collapse to empty output.

## 4. Security and trust model

### 4.1 Assets

The protected assets are:

- provider and tenant boundaries;
- credential-selection boundaries;
- user cost and quota;
- data-routing intent;
- foreground model identity;
- catalog integrity and freshness;
- worker lifecycle integrity;
- diagnostic confidentiality; and
- reproducible session policy.

### 4.2 Trust boundaries

The runtime policy compiler, catalog loader, authorization decision point, and provider router are trusted to enforce this specification. The following are untrusted inputs:

- foreground model output and tool arguments;
- task files and repository content;
- system and application logs;
- worker output;
- copied examples in tool descriptions;
- user-controlled prompt modules;
- extension text and tool results;
- environment text visible to a model; and
- historical session records.

Authentication state is trusted only as evidence that a route may authenticate. It is not authorization to delegate through that route.

### 4.3 Threat: context contamination from logs

Logs frequently contain provider names, historical model identifiers, endpoints, errors, and traces from unrelated sessions. When log content enters an LLM context, the model may mistake descriptive historical data for current capability or authorization. This is a confused-deputy problem: the untrusted agent proposes a route and the runtime, if permissive, uses credentials outside the intended boundary.

The runtime MUST apply these rules:

1. **No authority by mention.** No identity gains catalog membership or authorization because it appears in a prompt, task, log, tool result, schema example, session transcript, error message, or model output.
2. **No dynamic allowlist mutation from context.** Only trusted launch configuration and explicit trusted control-plane actions may change grants.
3. **No credential probing as discovery.** The runtime MUST NOT try credentials or contact providers to test an unlisted agent-proposed identity.
4. **Safe logs.** Logs MUST avoid secrets. Model-visible diagnostics SHOULD avoid unrelated historical model identities. Access control and redaction are defense in depth; authorization MUST remain correct even when a model reads arbitrary logs.
5. **Tainted discovery results.** A model identity extracted from a general filesystem or shell result has provenance `untrusted_context` and is never directly dispatchable. Eligibility is determined independently from the trusted catalog and effective grants.

### 4.4 Attacker capabilities

Conformance assumes an adversarial or mistaken foreground model can:

- submit any syntactically valid tool JSON;
- omit the model field;
- repeatedly guess model and provider names;
- copy names from schema descriptions or logs;
- request a provider for which credentials happen to exist;
- race multiple starts; and
- inspect all returned error text.

The safety properties MUST hold under those capabilities.

## 5. Typed domain model

The following reference types are normative in meaning, not in Rust spelling:

```rust
struct ProviderId(String);          // canonical, exact, non-empty
struct ModelId(String);             // provider-local canonical identifier
struct QualifiedModelId {
    provider: ProviderId,
    model: ModelId,
}

struct CatalogSnapshot {
    id: CatalogSnapshotId,
    generated_at: Timestamp,
    entries: BTreeMap<QualifiedModelId, CatalogEntry>,
    digest_sha256: Digest,
}

struct CatalogEntry {
    identity: QualifiedModelId,
    availability: Availability,
    worker_eligible: bool,
    source: CatalogSource,
}

enum WorkerModelRequest {
    InheritForeground,
    Explicit(QualifiedModelId),
}

struct CrossProviderGrant {
    source: TrustedGrantSource,
    from_provider: ProviderId,
    to_provider: ProviderId,
    allowed_models: BTreeSet<QualifiedModelId>,
    expires_at: Option<Timestamp>,
    grant_id: GrantId,
}

struct EffectiveDelegationPolicy {
    foreground: QualifiedModelId,
    same_provider_models: BTreeSet<QualifiedModelId>,
    cross_provider_grants: Vec<CrossProviderGrant>,
    catalog_snapshot_id: CatalogSnapshotId,
    limits: WorkerLimits,
    policy_digest_sha256: Digest,
}
```

`QualifiedModelId` parsing MUST split at the first `/`, preserve the complete provider-local remainder, reject missing components, and compare canonical identities exactly. It MUST NOT infer provider equivalence from shared substrings, SDK families, API hosts, corporate ownership, or compatible wire formats. Thus `openai-codex/gpt-5.6-sol` and any `openai/...` identity are cross-provider relative to one another.

## 6. Normative runtime invariants

For foreground identity `F`, trusted catalog snapshot `C`, effective policy `P`, requested worker identity `W`, and network-attempt count `N`, the runtime MUST maintain all of the following.

### INV-1: Foreground identity immutability

```text
session.foreground_identity == F
```

Worker selection, retry, denial, startup, and collection MUST NOT mutate `F`.

### INV-2: Catalog membership

```text
Authorized(W) => C.entries.contains_exact(W)
                 && C[W].worker_eligible
                 && C[W].availability permits launch
```

A syntactically valid identity is insufficient.

### INV-3: Same-provider baseline

Absent an applicable explicit cross-provider grant:

```text
Authorized(W) => W.provider == F.provider
```

This invariant applies even when no prompt manifest is supplied.

### INV-4: Exact-model allowlist

```text
Authorized(W) => W in P.same_provider_models
              || W in ApplicableCrossProviderGrant(P, F)
```

Provider equality alone does not authorize an arbitrary model. Choices are exact catalog-derived identities.

### INV-5: Omitted-model inheritance

```text
Resolve(InheritForeground, F) == F
```

The omitted model MUST resolve to the complete foreground qualified identity, not a process-wide default, provider-local default, last-used model, cheapest model, or inferred family peer.

If `F` is not present and worker-eligible in the pinned catalog snapshot, omitted-model dispatch MUST fail with a typed local error. It MUST NOT silently substitute another model.

### INV-6: Explicit cross-provider grant

```text
W.provider != F.provider =>
  exists G in P.cross_provider_grants:
      G.from_provider == F.provider
      && G.to_provider == W.provider
      && W in G.allowed_models
      && !Expired(G)
      && G.source is trusted
```

Credentials, catalog exposure, task prose, model request, or prior successful use cannot satisfy this invariant.

### INV-7: Pre-network denial

For every denied request:

```text
N_after == N_before
```

The decision MUST occur before provider client creation that performs discovery, credential acquisition or refresh, DNS, socket creation, HTTP request construction with credentials, network I/O, or billable inference. Pure local parsing and local trusted-catalog lookup are permitted.

### INV-8: Handle integrity and durable terminal cause

Once a worker handle is returned:

```text
Terminal(handle) => status(handle).terminal_cause != None
Terminal(handle) => collect(handle).terminal_cause == status(handle).terminal_cause
```

A failed worker MUST NOT be represented only as empty output, zero tools, or generic `failed`. Terminal records survive until normal session retention expires and remain collectable exactly as completed workers do.

### INV-9: Authorization provenance isolation

```text
AuthorizationInputs subset_of {
  trusted catalog snapshot,
  compiled launch policy,
  trusted explicit control-plane grants,
  immutable session identity,
  runtime limits
}
```

Logs, prompts, task files, schema examples, worker text, and arbitrary tool results MUST NOT be authorization inputs.

### INV-10: Race-free authorization

Catalog snapshot, effective policy, limits reservation, and selected identity MUST be checked atomically for dispatch. Concurrent requests MUST NOT exceed limits or use a grant after revocation/expiry. The authorized identity bound to a handle MUST be the identity passed to routing.

### INV-11: Equivalent entry points

All delegation entry points MUST call the same authorization decision point. No extension, RPC method, retry path, or resume path may bypass or weaken these invariants.

## 7. Catalog and model-choice exposure

### 7.1 Trusted catalog snapshot

At session startup the runtime MUST resolve the foreground qualified identity and load or construct a trusted catalog snapshot. The snapshot MUST have an identifier and digest retained in session metadata. A session uses a pinned snapshot unless a trusted catalog-refresh operation atomically recompiles policy and records the change.

Catalog sources MUST be runtime-controlled. General log scanning and model-generated lists are prohibited catalog sources. Remote provider discovery, if supported, occurs through a trusted catalog subsystem before a choice is presented; it is not performed in response to an unauthorized guessed dispatch.

### 7.2 Exact worker-choice set

The effective worker choice set is:

```text
Choices(F, C, P) =
  {m in C | m.worker_eligible && m.provider == F.provider && m in P.same_provider_models}
  union
  {m in C | m is covered by an applicable explicit cross-provider grant}
```

Every exposed choice MUST be a complete `QualifiedModelId`. Aliases such as `same`, `fast`, or `default` MAY exist only as UI conveniences if the runtime resolves them to a displayed exact identity before authorization and records that identity. Agent-facing dispatch should use `model: null`/omission for inheritance or an exact choice; free-form aliases SHOULD NOT be accepted.

### 7.3 Tool schema

The worker tool contract MUST stop teaching a single unrelated provider/model as its generic example. The schema SHOULD expose one of:

- a per-session `enum` containing exactly `Choices(F, C, P)`; or
- a constrained string plus a sibling read-only `subagent_models` operation returning exactly that set when dynamic schemas are unavailable.

The description MUST state: “Omit to inherit `<foreground-qualified-id>`. Explicit values must be one of the session’s listed exact choices.” A static example MUST be derived from the active session or omitted entirely.

Schema exposure is usability guidance, not the enforcement point. A forged value outside the enum MUST still be denied by runtime authorization.

### 7.4 Empty choice set

The foreground identity SHOULD ordinarily be worker-eligible, making inheritance available. If policy or catalog state yields no choices, worker tools MUST remain honest: they may be unavailable or return a typed local error explaining that no worker model is authorized. They MUST NOT expose unrelated fallback examples.

## 8. Model default semantics

The term “default model” is ambiguous and MUST NOT be used internally for worker dispatch without a type indicating its scope.

The normative resolution algorithm is:

```text
resolve_worker_model(request, session):
  if request.model is omitted:
      candidate = session.foreground_qualified_model
      selection_source = foreground_inheritance
  else:
      candidate = parse_exact_qualified_model(request.model)
      selection_source = explicit_request

  require candidate in session.catalog_snapshot
  require candidate worker_eligible
  authorize(candidate, session.effective_policy)
  reserve_limits_atomically()
  bind candidate and selection_source to worker record
  return candidate
```

`crate::models::default_model()` or an equivalent process-wide fallback MUST NOT participate in this algorithm. Configuration MAY determine the foreground model before session creation; after creation, the immutable session foreground identity is the inheritance source.

Examples for foreground `openai-codex/gpt-5.6-sol`:

| Worker request | Required result |
|---|---|
| model omitted | Resolve exactly to `openai-codex/gpt-5.6-sol`, then authorize |
| `openai-codex/gpt-5.6-sol` | Authorize if present and allowed |
| `openai/gpt-5.5` | Deny unless an explicit exact cross-provider grant covers it |
| `anthropic/claude-opus-4-7` | Deny unless an explicit exact cross-provider grant covers it |
| catalog-absent guessed Codex model | Deny as catalog-unknown even if provider matches |
| unqualified `gpt-5.6-sol` | Deny as invalid qualified identity; do not infer provider |

## 9. Manifest and launch requirements

### 9.1 Secure baseline without a manifest

`--prompt-manifest` is optional for ordinary launch compatibility, but runtime confinement is not optional. When it is omitted, the runtime MUST synthesize an immutable baseline policy equivalent to:

```yaml
# Illustrative effective policy; generated by runtime.
delegation:
  enforcement: enforced
  foreground: <resolved exact foreground identity>
  model_omission: inherit_foreground
  allowed_providers:
    - <foreground provider>
  allowed_models:
    - <catalog-confirmed foreground identity>
  cross_provider_grants: []
```

The runtime MAY safely include additional same-provider models only when they are exact entries from the trusted catalog and a trusted user/configuration policy explicitly allows that expansion. Agent prose alone cannot expand it.

This synthesized baseline fixes the incident class in which fleet launch omitted `--prompt-manifest`.

### 9.2 Manifest requirements

When supplied, the prompt manifest MUST compile before the first foreground request. Delegation configuration MUST identify exact qualified models. Wildcard provider grants, unrestricted `allow_cross_provider: true`, and bare model IDs MUST be rejected for enforced launches unless a separately versioned enterprise policy explicitly defines and accepts those semantics; they are outside this specification.

A cross-provider grant MUST include:

- source foreground provider;
- destination provider;
- exact destination model set;
- trusted grant provenance;
- optional expiry;
- stable non-secret grant ID; and
- catalog membership validation.

Illustrative manifest fragment:

```yaml
schema: synaps-prompt/1
policies:
  delegation:
    enforcement: enforced
    same_provider_models:
      - openai-codex/gpt-5.6-sol
    cross_provider_grants:
      - id: review-grant-01
        from_provider: openai-codex
        to_provider: anthropic
        allowed_models:
          - anthropic/claude-opus-4-7
        expires_at: 2026-07-14T00:00:00Z
```

This example describes shape only; it does not claim that either model is currently catalog-available or entitled.

### 9.3 Startup validation

Session startup MUST fail before foreground network activity when any of these apply:

- foreground identity is malformed or unresolved;
- foreground identity is absent from the trusted catalog;
- manifest schema or signature/provenance requirements fail;
- an allowed model is absent from the catalog;
- a grant’s model provider does not match its destination provider;
- grant source does not apply to the foreground provider;
- policy compilation is ambiguous;
- limits are invalid; or
- secure baseline policy cannot be constructed.

A launch MUST NOT downgrade from enforced to advisory or off because a manifest is missing, malformed, or incompatible.

### 9.4 Fleet launch

Fleet supervisors MUST pass either the intended manifest reference or an already compiled, digest-bound effective policy to every child. Every child status record MUST identify the policy digest and catalog snapshot ID. A child that cannot prove it received or synthesized an enforced baseline MUST fail startup rather than run unconstrained.

## 10. Dispatch decision and startup lifecycle

### 10.1 Decision order

The runtime MUST perform dispatch in this order:

1. Parse the request without side effects.
2. Resolve omission to foreground inheritance.
3. Validate exact qualified syntax.
4. Look up the candidate in the pinned trusted catalog.
5. Evaluate same-provider or exact cross-provider grant.
6. Atomically check and reserve worker limits.
7. Create and persist a worker record and handle.
8. Resolve the authorized route and local credential reference.
9. Create the worker runtime.
10. Start network inference.

Steps 3–6 are pre-network. Failure in those steps is a dispatch denial and SHOULD return no worker handle. Failure after a handle is returned is a worker startup failure and MUST be retained in lifecycle state.

If an implementation creates a handle before authorization for architectural reasons, the handle MUST immediately become a durable terminal `denied` record and still satisfy status/collect consistency. It MUST never enter `running` and MUST cause no network attempt.

### 10.2 Typed states

```rust
enum WorkerState {
    Authorized,
    Starting,
    Running,
    Terminal(WorkerTerminal),
    Collected,
    Reconciled,
}

enum WorkerTerminal {
    Completed,
    Denied(DispatchFailure),
    StartupFailed(StartupFailure),
    ExecutionFailed(ExecutionFailure),
    TimedOut(TimeoutFailure),
    Cancelled(CancelFailure),
}
```

`tool_count == 0` and `output == ""` are metrics, not error types.

### 10.3 Failure types

At minimum:

```rust
enum DispatchFailureCode {
    InvalidQualifiedModel,
    CatalogModelUnknown,
    CatalogModelUnavailable,
    ModelNotWorkerEligible,
    ProviderNotAllowed,
    ModelNotAllowed,
    CrossProviderGrantRequired,
    CrossProviderGrantExpired,
    ConcurrencyLimit,
    TotalWorkerLimit,
    PolicyUnavailable,
}

enum StartupFailureCode {
    RouteResolutionFailed,
    CredentialUnavailable,
    CredentialRefreshFailed,
    RuntimeConstructionFailed,
    ProviderClientConstructionFailed,
    TransportSetupFailed,
}
```

Implementations MAY add finer codes. They MUST NOT map a known startup cause to an undifferentiated `failed` solely to simplify display.

## 11. Diagnostics, status, and collection

### 11.1 Safe dispatch denial

A denial response MUST contain:

- stable machine-readable code;
- requested qualified identity, when safe and syntactically valid;
- immutable foreground provider or identity;
- whether the request was explicit or inherited;
- policy digest or short correlation ID;
- safe remediation, such as “omit model to inherit foreground” or “select from `subagent_models`”; and
- `network_attempted: false`.

Example:

```json
{
  "kind": "dispatch_denied",
  "code": "cross_provider_grant_required",
  "requested_model": "anthropic/claude-opus-4-7",
  "foreground_model": "openai-codex/gpt-5.6-sol",
  "selection_source": "explicit_request",
  "network_attempted": false,
  "remediation": "Omit model to inherit the foreground identity or use an explicitly granted catalog choice."
}
```

The error MUST NOT list installed credentials, tokens, endpoints with sensitive query data, unrelated model history, raw provider bodies, full manifests, prompt content, or log excerpts.

### 11.2 Status retention

For every returned handle, `subagent_status` MUST include:

- handle ID;
- authorized exact model identity;
- selection source (`foreground_inheritance` or `explicit_request`);
- state;
- terminal category and code, if terminal;
- safe stage (`authorize`, `route`, `credential`, `runtime`, `transport`, `inference`);
- elapsed time;
- tool count and output length as separate metrics;
- `network_attempted` boolean; and
- a safe correlation ID.

A failed status MUST not erase its cause after polling, process-local retries, or transition to collected.

### 11.3 Collection retention

`subagent_collect` on a terminal failure MUST return the same terminal category, code, model identity, stage, and correlation ID as status. It MAY additionally include a safe message. Empty model output is valid as an output field but MUST be accompanied by the typed failure.

Collection MUST be idempotent for diagnostic data. A second collection may report `already_collected`, but it MUST still make the retained terminal result inspectable under normal session-retention rules.

### 11.4 Startup failure versus policy denial

Diagnostics MUST make clear whether:

- policy rejected the request before network (`denied`);
- an authorized worker failed before inference (`startup_failed`);
- provider transport failed after an attempt (`execution_failed` or a transport subtype); or
- inference completed with empty content (`completed`, empty output).

This distinction prevents transport defects, model availability issues, and policy violations from being conflated.

## 12. Telemetry and audit

### 12.1 Required events

Emit bounded structured events for:

- `worker.model_resolution_requested`;
- `worker.model_inherited` or `worker.model_explicit`;
- `worker.dispatch_allowed`;
- `worker.dispatch_denied`;
- `worker.starting`;
- `worker.startup_failed`;
- `worker.running`;
- `worker.terminal`;
- `worker.collected`; and
- `policy.catalog_refreshed` when applicable.

### 12.2 Required safe fields

Events SHOULD include:

- session correlation ID;
- worker correlation/handle ID where one exists;
- foreground provider and exact model identity;
- requested/resolved model identity;
- selection source;
- same-provider boolean;
- grant ID for cross-provider allowance, but not grant contents;
- catalog snapshot ID;
- policy digest;
- reason code;
- decision stage;
- network-attempted boolean;
- latency bucket; and
- lifecycle counters.

### 12.3 Prohibited fields

Telemetry and logs MUST NOT include:

- access tokens, refresh tokens, API keys, cookies, authorization headers, or credential file contents;
- full prompts, tasks, worker output, or arbitrary tool results by default;
- raw manifest text;
- provider response bodies unless independently redacted and explicitly enabled;
- unrelated catalog entries in denial events; or
- secret-bearing URLs.

Exact model IDs are generally operational metadata, but exports MUST follow the deployment’s metadata-classification policy. Aggregate metrics SHOULD use provider/model dimensions only when approved.

### 12.4 Metrics

Recommended counters and histograms:

```text
worker_dispatch_total{decision,reason,selection_source,same_provider}
worker_startup_failure_total{stage,reason}
worker_dispatch_pre_network_denial_total{reason}
worker_model_inheritance_total{provider}
worker_cross_provider_grant_use_total{grant_id}
worker_status_collect_mismatch_total
worker_dispatch_latency_seconds{decision_stage}
```

A pre-network denial with `network_attempted=true`, or a status/collect mismatch, is a security invariant violation and SHOULD trigger high-severity operational alerting.

## 13. Black-box regression harness

The conformance harness MUST exercise the built CLI/runtime boundary, not only policy unit types. It MUST run with isolated temporary configuration, a fake catalog, fake credential broker, and instrumented fake provider transports. It MUST not use real credentials or public networks.

### 13.1 Harness fixtures

Provide:

- foreground identity `openai-codex/gpt-5.6-sol`;
- a catalog containing that identity, at least one other exact same-provider model, `openai/gpt-5.5`, and `anthropic/claude-opus-4-7`;
- a provider transport spy counting construction, credential, DNS/network, and request attempts;
- optional startup failures injected at route, credential, runtime, and transport stages;
- a log fixture containing tempting historical model IDs;
- launches both with and without `--prompt-manifest`;
- dynamic tool-schema capture; and
- status/collect polling through the same public interfaces used by an agent.

The listed identities are test fixtures, not assertions of real-world availability.

### 13.2 Required conformance cases

| ID | Scenario | Expected result |
|---|---|---|
| C01 | No manifest; model omitted | Exact foreground identity inherited and dispatched |
| C02 | No manifest; explicit foreground identity | Allowed if catalog eligible |
| C03 | No manifest; `openai/gpt-5.5` | Denied pre-network as cross-provider/no grant |
| C04 | No manifest; Anthropic identity | Denied pre-network as cross-provider/no grant |
| C05 | Same-provider but catalog-unknown guessed model | Denied pre-network as catalog unknown |
| C06 | Unqualified model string | Denied pre-network as invalid qualified identity |
| C07 | Log fixture read before cross-provider request | Same denial as C04; log contents do not alter choices or policy digest |
| C08 | Tool schema captured | Contains only exact effective choices; no unrelated static Anthropic example |
| C09 | Valid exact cross-provider manifest grant | Granted model allowed and grant ID recorded |
| C10 | Grant names provider but not exact requested model | Denied pre-network as model not allowed |
| C11 | Expired grant | Denied pre-network as grant expired |
| C12 | Credentials exist for ungranted provider | Still denied; credential broker invocation count remains zero |
| C13 | Authorized route startup failure after handle | Status returns typed startup code and stage |
| C14 | Collect C13 | Same code, stage, model, and correlation ID as status |
| C15 | Repeated collect | Diagnostic remains inspectable and consistent |
| C16 | Process-wide default differs from foreground | Omitted worker still inherits foreground exactly |
| C17 | Concurrent starts at limit boundary | Atomic limit enforcement; denied request makes no network attempt |
| C18 | RPC/extension/one-shot aliases | Same decisions and reason codes as `subagent_start` |
| C19 | Manifest malformed or catalog reference unknown | Session fails before foreground network request; no advisory downgrade |
| C20 | Fleet child lacks passed policy | Child synthesizes secure baseline or fails startup, never runs open |
| C21 | Catalog refresh removes a granted model | New dispatch denied under new digest; existing binding does not silently reroute |
| C22 | Foreground identity absent from catalog | Startup or omitted dispatch fails typed; no fallback model used |
| C23 | Authorized inference returns empty output | Distinguished from startup failure with terminal `completed` |
| C24 | Inject separate Gemini transport serialization failure | Reported as transport failure, not provider-policy denial |

For every denied case, the harness MUST assert all network spies and billable-request counters remain zero. It MUST also assert that credentials and raw log fixture text do not appear in CLI output, status, collection, session metadata, or telemetry.

### 13.3 Storm incident replay

A dedicated replay SHOULD present the original behavioral sequence to a deterministic scripted foreground:

1. launch foreground `openai-codex/gpt-5.6-sol` without a prompt manifest;
2. request `openai/gpt-5.4`, `openai/gpt-5.2`, and `openai/gpt-5.5`;
3. read a log fixture exposing Anthropic and Codex historical names;
4. request `anthropic/claude-opus-4-7`;
5. omit the model on a final request.

Expected outcomes:

- all explicit ungranted cross-provider guesses are denied locally with typed, useful errors;
- the log read causes no catalog or policy mutation;
- no denied request creates provider traffic;
- the omitted final request resolves exactly to `openai-codex/gpt-5.6-sol`;
- the session remains on the original foreground identity; and
- all returned worker handles have status/collect-consistent terminal records.

## 14. Migration and backward compatibility

### 14.1 Compatibility guarantees

- Existing launches without `--prompt-manifest` continue to start when the foreground identity and trusted catalog can be resolved.
- Existing callers that omit worker `model` retain the ability to omit it, but semantics change from process-default fallback to exact foreground inheritance. This is an intentional security correction.
- Existing explicit same-provider qualified IDs continue to work only if present in the trusted catalog and effective exact allowlist.
- Existing explicit cross-provider dispatches that relied on ambient credentials or permissive absence of policy will be denied until an explicit grant is configured.
- Existing status consumers may continue reading legacy fields, but new typed terminal fields are authoritative.

### 14.2 No unsafe compatibility mode

There MUST NOT be an automatic fallback to the old unrestricted behavior. A temporary operator compatibility escape hatch, if product governance requires one, MUST:

- be explicit at trusted launch/control-plane level, never prompt-controlled;
- display a high-visibility warning;
- emit an audit event;
- have bounded scope and expiry;
- remain catalog-bound;
- preserve pre-network validation and typed failures; and
- never restore process-wide omitted-model fallback.

Such an escape hatch is not conformant for production acceptance under this specification.

### 14.3 Rollout phases

1. **Observe:** Add typed model resolution, catalog snapshots, network spies in tests, and safe telemetry while retaining behavior only in development builds.
2. **Correct inheritance:** Replace worker process-default fallback with foreground qualified identity; add compatibility diagnostics.
3. **Enforce baseline:** Synthesize fail-closed same-provider policy for manifestless launches.
4. **Grant support:** Enable exact explicit cross-provider grants with audit provenance.
5. **Schema narrowing:** Expose exact per-session choices and remove unrelated static examples.
6. **Fleet enforcement:** Require policy digest/catalog snapshot propagation or secure child synthesis.
7. **Remove escape hatch:** After telemetry confirms migration, eliminate any temporary permissive option.

Each phase MUST preserve or strengthen the invariants already enabled. Enforcement MUST not be postponed merely because prompt adapters are incomplete.

## 15. Implementation slices

These slices describe separable deliverables and do not prescribe module names.

### Slice A: Identity and catalog foundation

- Introduce canonical `ProviderId`, `ModelId`, and `QualifiedModelId` use across foreground and worker paths.
- Build immutable session `CatalogSnapshot` with ID and digest.
- Eliminate provider substring/family equivalence in authorization.
- Add exact effective-choice computation.

**Exit condition:** Catalog and identity unit/property tests pass, including first-slash parsing and exact provider inequality.

### Slice B: Session policy compilation

- Synthesize enforced same-provider baseline when no manifest exists.
- Compile exact manifest allowlists and grants.
- Bind foreground identity, catalog snapshot, and policy digest to session and fleet child metadata.
- Fail startup on invalid policy instead of downgrading.

**Exit condition:** Manifestless and manifest launches produce inspectable enforced policies.

### Slice C: Worker model resolution

- Replace `crate::models::default_model()` worker fallback with foreground inheritance.
- Resolve and authorize before route/client/network work.
- Atomically reserve limits and bind the authorized identity to the handle.
- Route all delegation entry points through one decision point.

**Exit condition:** C01–C07, C12, C16–C18, and C22 pass with zero denied network attempts.

### Slice D: Catalog exposure and tool contract

- Generate per-session exact model choices or add a trusted `subagent_models` query.
- Remove the static unrelated-provider schema example.
- Explain omission/inheritance in the agent-facing schema.
- Keep forged-value runtime checks.

**Exit condition:** C08 passes and schema snapshots contain no unauthorized model.

### Slice E: Durable typed lifecycle failures

- Add dispatch/startup/execution terminal categories and stable reason codes.
- Persist authorized model, stage, correlation ID, and network-attempted flag.
- Make status and collect consistent and collection idempotently inspectable.
- Ensure zero output/zero tools remain metrics only.

**Exit condition:** C13–C15 and C23 pass across process and RPC boundaries.

### Slice F: Telemetry and secret hygiene

- Emit required bounded events and metrics.
- Add redaction and prohibited-field tests.
- Alert on pre-network invariant violations and status/collect mismatches.
- Ensure logs cannot mutate catalog or policy.

**Exit condition:** telemetry tests show required fields, no fixture secrets/raw context, and stable reason codes.

### Slice G: Black-box fleet regression

- Add fake providers, credential broker, catalog, and transport spies.
- Implement the incident replay and launch permutations.
- Keep Gemini transport coverage separate and labeled.
- Run the harness in CI without public network access.

**Exit condition:** C01–C24 pass in a network-isolated CI job.

## 16. Acceptance criteria

The implementation is accepted only when all criteria below are met:

1. A manifestless `openai-codex/gpt-5.6-sol` foreground session enforces a same-provider runtime baseline.
2. Omitting worker `model` resolves exactly to the foreground qualified identity and never calls a process-wide model default.
3. Agent-invented same-provider model IDs are rejected unless exact trusted-catalog entries and policy choices.
4. `openai` and `openai-codex` are treated as distinct providers.
5. Every cross-provider dispatch requires a trusted, applicable grant naming the exact destination model.
6. Credentials, task prose, logs, schema examples, and historical success do not confer authorization.
7. Every policy or catalog denial occurs before credential access, network activity, and billing, proven by black-box spies.
8. The worker tool exposes only exact catalog-derived effective choices and no static unrelated-provider example.
9. Every returned failed-worker handle retains a typed terminal cause in status and collection; zero tools/empty output never stand alone as the diagnosis.
10. Status and collection agree on terminal category, reason, stage, identity, and correlation ID.
11. Fleet children either inherit a digest-bound policy/catalog snapshot, synthesize the secure baseline, or fail startup.
12. Diagnostics are actionable but contain no secrets, unrelated model history, raw prompts, or raw logs.
13. Telemetry distinguishes policy denial, startup failure, transport failure, and completed empty output.
14. The Storm incident replay passes exactly as specified.
15. Gemini transport regression coverage, if included, remains separate and cannot satisfy Codex confinement cases.
16. All public delegation paths share the same enforcement point and pass equivalent conformance cases.
17. Repository documentation and implementation tests define stable reason codes suitable for automation.
18. The complete black-box suite passes with public networking disabled and without real credentials.

## 17. Security review checklist

Before release, reviewers MUST verify:

- no worker fallback remains connected to a process-wide default;
- no authorization path accepts bare model IDs or provider aliases;
- no provider client, credential broker, or refresh path runs before authorization;
- catalog data cannot be populated from application logs or model context;
- dynamic schema generation and runtime authorization use the same pinned snapshot;
- grants cannot be created or expanded by LLM tool calls unless a separately authenticated trusted control plane is explicitly designed for that purpose;
- revocation, expiry, and concurrent dispatch are race-safe;
- status and collection retain errors through terminal lifecycle transitions;
- fleet child launch cannot omit policy silently;
- telemetry redaction tests include adversarial prompt/log strings resembling credentials; and
- provider transport bugs are classified independently from authorization decisions.

## 18. Summary of required behavior

The foreground model may propose work, but it does not choose its authorization boundary. Synaps derives a finite exact worker set from a trusted catalog and an enforced session policy. With no manifest, the secure default is the foreground qualified model on the foreground provider. Cross-provider use requires a trusted exact grant. Guesses and names copied from logs remain untrusted text. Denials happen locally before credentials or network activity. Once a handle exists, every startup outcome is typed, durable, and consistently visible through status and collection.
