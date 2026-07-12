# Cloud OAuth Providers — Implementation and Deployment Runbook

This runbook operationalizes [`cloud-oauth-providers-spec.md`](cloud-oauth-providers-spec.md). Execute in order. It is a pre-deployment plan, not evidence that Azure or Vertex registration exists.

## 0. Stop/go matrix

| Provider | Build may start | Preview deploy may start | General release gate |
|---|---|---|---|
| Azure OpenAI | Yes, with fixture/config seams | Only after a Synaps-owned or explicitly broker-configured Microsoft public client ID exists | Registration + consent + sandbox deployment + deferred live test pass |
| AWS Bedrock | Yes | Yes in sandbox | Required live AWS test pass |
| Google Vertex | Yes, with fixture/config seams | Only after a Synaps-owned or explicitly broker-configured Google Desktop client ID exists | Registration + consent + sandbox project + deferred live test pass |

**Hard stop:** never substitute copied first-party client IDs. Never state “OAuth ready” when either external registration is missing. AWS OIDC tokens are used to obtain temporary role credentials; Bedrock inference is broker-side **SigV4**, never bearer OAuth. Azure v1 means Azure OpenAI only; Foundry is deferred.

## 1. Preflight

1. Confirm clean branch/worktree and read the spec and existing broker decisions.
2. Assign one owner per task/file group in §8; parallel workers must not edit the same file.
3. Record external prerequisite status without secrets:
   - `MICROSOFT_PUBLIC_CLIENT_REGISTERED=yes/no`, application owner, tenant policy owner.
   - `GOOGLE_DESKTOP_CLIENT_REGISTERED=yes/no`, consent-screen owner, test/publish status.
   - AWS sandbox owner, SSO start URL owner, SSO/Bedrock regions, account/role, budget and cleanup owner.
4. Security owner approves host/suffix/path allowlists and cloud permissions before implementation.
5. Use `CARGO_BUILD_JOBS=8`; run one Cargo command at a time. Default tests must have no external network.

Checkpoint **C0 / feasibility** passes when blockers and release labels are recorded, configurable public-client IDs are broker-owned, and no secret/client ID is committed accidentally. Missing Microsoft/Google registrations blocks deployment claims, not fixture-driven implementation.

## 2. Implementation sequence

### Slice A — typed identities, configuration, and storage

Add canonical IDs `azure-openai`, `aws-bedrock`, `google-vertex` through provider parsing, descriptor registry, auth storage, broker strategy, and RPC capability surfaces. Aliases normalize only at CLI parsing. Define non-secret provider context (tenant/subscription/resource; SSO region/account/role/Bedrock region; project/location) separately from secret credential state. Preserve unknown `auth.json` fields and atomically merge with restrictive permissions.

**Accept:** malformed tenant/project/region/resource values fail before network; all secret structs have redacted `Debug`; runtime/TUI cannot deserialize refresh/AWS secret fields; missing public client registration yields typed `registration_required`.

### Slice B — login state machines

Implement behind injected HTTP/clock/browser/callback/storage seams:

- Azure device code against tenant-specific v2 endpoints, ARM `.default`, then inference `cognitiveservices.azure.com/.default`; honor poll interval/slow-down/expiry/cancel and refresh rotation.
- AWS `RegisterClient` → `StartDeviceAuthorization` → `CreateToken` → `ListAccounts` → `ListAccountRoles` → explicit choice → `GetRoleCredentials`; honor dynamic registration expiry and all polling errors.
- Vertex installed-app loopback + state + S256 PKCE, cloud-platform scope, offline access, exact redirect, one-shot callback, refresh-token preservation.

**Accept:** successful login validates a minimal catalog call before commit; denial leaves old valid credentials untouched; redirects are disabled; no device code, auth code, token, client secret, AWS key, or authorization header appears in output.

Checkpoint **C1 / auth boundary** requires focused auth tests, storage fault tests, redaction snapshots, and security review. Continue autonomously after recording results; a failure blocks dependent slices until fixed.

### Slice C — broker catalogs and credential refresh

Implement bounded dynamic discovery:

- Azure ARM deployment enumeration with ARM token; inference token is separate.
- AWS `ListFoundationModels` through signed control request.
- Vertex regional `publishers/google/models` pagination.

Normalize to canonical namespaced IDs and capabilities. Cache last-known-good with TTL/source/fetched-at and mark stale. Empty or unauthorized responses are not replaced with guessed models.

**Accept:** page/entry/body limits, pagination-loop detection, deduplication, cancellation, single-flight refresh, scope isolation, AWS role expiry, and endpoint validation all have tests.

### Slice D — runtime adapters

Implement typed broker invocation only. Azure uses selected deployment and inference token; AWS signs `Converse`/`ConverseStream` with temporary role credentials; Vertex invokes public regional `streamGenerateContent` SSE. The broker constructs every host/path/header and rejects caller URL/auth/signing inputs.

**Accept:** fixture/golden tests cover request translation, tool calls, stream fragmentation, usage, provider errors, cancellation, one terminal event, redirect rejection, stale credentials, and unknown IDs. SigV4 has deterministic AWS-compatible golden vectors and signs session token/body.

Checkpoint **C2 / vertical slice** requires each provider’s start-login-to-stream flow to pass entirely against local fakes while an external-network-deny connector is active.

### Slice E — TUI `/models`

Wire catalog state into `/models`: grouped providers, explicit context, filter/navigation, loading/error/empty/stale/retry and registration-required states. Selection resolves to exactly the broker route represented by the entry. Refresh must not reorder the focused item unexpectedly.

**Accept:** TUI credential-boundary test proves no `auth.json` or cloud HTTP access; snapshots/harness cover all states and duplicate/unknown entries.

## 3. Zero-network unattended test procedure

1. Start local fake authorities on random loopback ports via explicit test-only endpoint injection.
2. Install a connector/DNS seam that panics on every non-loopback destination; clear cloud proxy variables in the child environment.
3. Use deterministic clock/RNG where protocol safety permits; assert polling sleeps rather than busy-loops.
4. Run focused suites sequentially (names may be adapted to final package layout):

```sh
CARGO_BUILD_JOBS=8 cargo test -p synaps-core cloud_oauth -- --test-threads=1
CARGO_BUILD_JOBS=8 cargo test -p synaps-engine cloud_provider -- --test-threads=1
CARGO_BUILD_JOBS=8 cargo test -p synaps-tui models -- --test-threads=1
CARGO_BUILD_JOBS=8 cargo test --test azure_openai_oauth_e2e -- --test-threads=1
CARGO_BUILD_JOBS=8 cargo test --test aws_bedrock_sso_e2e -- --test-threads=1
CARGO_BUILD_JOBS=8 cargo test --test google_vertex_oauth_e2e -- --test-threads=1
CARGO_BUILD_JOBS=8 cargo test --test oauth_provider_e2e -- --test-threads=1
```

5. Search captured stdout/stderr, telemetry, snapshots, and fake-server logs for fixture canaries representing every secret class. Assert zero hits outside fake-server request assertions.
6. Fault-inject storage write/rename, token rotation, 401/403/429/5xx, malformed/oversized pages/events, replay, redirect, DNS/host substitution, pagination loops, cancellation, and concurrent refresh.

Checkpoint **C3 / unattended holdout** passes only with zero non-loopback attempts and all adversarial assertions green.

## 4. Required live AWS test

Use a dedicated least-privilege account/role and low-cost enabled Bedrock model. Do not screen-record user/device codes or capture HTTP authorization headers.

1. Record commit, tester, UTC time, SSO region, Bedrock region, account alias/ID (redacted as policy requires), role name, model ID, budget cap, CloudTrail query owner.
2. Run `synaps login --provider aws-bedrock`; verify device URI/code display and successful account/role selection. If multiple choices exist, verify no default-first behavior.
3. Open `/models`; confirm results correspond to `ListFoundationModels` in the selected Bedrock region and context is correctly labeled.
4. Send one harmless non-sensitive prompt through `Converse`; verify a response and expected CloudTrail principal/action.
5. Send one short streaming prompt through `ConverseStream`, then cancel; verify bounded termination and no credential/error leakage.
6. Exercise role credential refresh (short session or controlled clock/expiry where practical) without reusing expired credentials. Confirm requests are SigV4 and contain a session token; no bearer inference header.
7. Logout; verify broker credentials/catalog cache are cleared and subsequent invocation is login-required. Review logs for canaries/secrets and CloudTrail for only expected actions.
8. Store a secret-free evidence record: commit, regions, model ID, actions, timestamps, result, issue links. Remove test conversations and perform owner-approved cleanup.

Any unexpected host/action, token/key in output, unsigned/bearer request, wrong account/region, or uncontrolled spend is a release blocker and security incident candidate.

## 5. Deferred live tests

### Azure OpenAI (blocked until Microsoft registration/resource readiness)

Confirm public-client registration and admin consent; run tenant device login; verify ARM token audience/scope and deployment discovery; verify separate Cognitive Services audience token; stream one harmless request to a selected deployment; test refresh rotation/logout; review Entra sign-in and Azure activity logs. Record no response content. Foundry endpoints are prohibited.

### Vertex (blocked until Google registration/project readiness)

Confirm Desktop client/consent/test-user status and API/billing; run loopback PKCE login; verify cloud-platform scope and refresh retention; list `publishers/google/models` for explicit project/location; stream one harmless public `streamGenerateContent` SSE request; test refresh/logout and audit logs.

These are **deferred, not waived**. Do not mark either provider generally available based solely on fake services.

## 6. Security review checklist

- [ ] Durable and temporary credentials are broker-only; typed RPC has no generic token endpoint for these providers.
- [ ] Azure ARM and inference audiences/caches cannot be interchanged.
- [ ] AWS OIDC client secret and role credentials are redacted; SigV4 host, service, region, path, headers and payload are broker-derived.
- [ ] Google exact state, S256 PKCE, callback one-shot and refresh preservation are enforced.
- [ ] HTTPS/host suffix/path/method/API-version allowlists and redirect-off behavior are tested.
- [ ] Caller cannot inject host, project, region, deployment path, auth headers, signing headers, query auth, or absolute URL.
- [ ] Time/body/page/event/catalog limits; cancellation; single-flight; clock skew; atomic storage; file modes are tested.
- [ ] Logs/errors/telemetry/TUI contain no secret; unknown models and unsupported blocks fail closed.
- [ ] Least-privilege permissions and audit trails are documented; logout/cache clearing is verified.

## 7. Final verification, convergence, and deployment

Run in order:

```sh
CARGO_BUILD_JOBS=8 cargo fmt --all -- --check
CARGO_BUILD_JOBS=8 cargo test --workspace -- --test-threads=1
CARGO_BUILD_JOBS=8 cargo check --workspace
CARGO_BUILD_JOBS=8 cargo build --release
```

Convergence holdout threshold is **0.90**. Weighted review axes: correctness **.30**, security **.30**, architecture **.20**, tests **.15**, docs **.05**. Maximum fix iterations: **2**; maximum total evaluator calls: **10**. A security-axis failure or unmet hard feasibility/live gate vetoes the aggregate score. Freeze the candidate before holdout evaluation; record per-axis evidence and score. Fix only diagnosed failures, rerun affected gates plus full final verification, and stop after two fix iterations for lead disposition rather than weakening tests.

Checkpoint **C4 / release candidate:** holdout ≥0.90, all axes acceptable, AWS live evidence attached, clean worktree, and deployment labels correctly block Azure/Vertex. Deploy AWS behind a provider flag/canary, observe auth failures, signing failures, latency, cancellation, and spend with secret-safe metrics, then expand. Azure/Vertex flags remain off except named sandbox tenants/projects until their live gates pass. Rollback disables provider routing/catalog/login and invalidates broker cache; it never exports credentials.

## 8. Task and file ownership plan

Assign people before coding; these are ownership boundaries, not permission to edit all files in one task.

| Owner role | Task | Expected files (final names may vary) | Dependency |
|---|---|---|---|
| Auth owner | Typed IDs/config/storage and three login state machines | `crates/agent-core/src/core/auth/provider.rs`, `providers.rs`, `storage.rs`, `broker.rs`, new `azure_openai.rs`, `aws_bedrock.rs`, `google_vertex.rs`; `src/cmd/login.rs`, `src/cmd/auth_broker.rs` | C0 |
| Runtime owner | Catalog contracts, Azure/Vertex adapters, AWS signer/Converse adapters | `crates/agent-engine/src/runtime/**`, provider catalog modules | C1 |
| TUI owner | `/models` async catalog states and selection | `crates/agent-tui/src/tui/models/**`, command/view-model wiring only as needed | Slice C contract |
| Harness owner | Local fakes, external-network deny, E2E and adversarial tests | `tests/*cloud*`, `tests/azure_openai_*`, `tests/aws_bedrock_*`, `tests/google_vertex_*`, secret-free fixtures | Interfaces frozen at C1 |
| Security reviewer | Threat/allowlist/signing review; no builder test authorship before freeze | Review records only; changes returned to owning role | C1–C4 |
| Release owner | Registrations, sandbox permissions, AWS live evidence, flags/rollback | deployment configuration/evidence in approved operational system; no credentials in repo | C3 |
| Docs owner | Keep spec/runbook/engplan truthful and synchronized | `docs/cloud-oauth-providers-{spec,runbook}.md`, `.plans/cloud-oauth-providers.plan.html` | All checkpoints |

Conflicts are resolved by the listed file owner; cross-cutting interface changes require both owners. No worker edits another owner’s implementation files without handoff. Every checkpoint records commit, commands, result, blockers, owner, and UTC timestamp.

## 9. Integration evidence (2026-03-12)

Implemented commits through `bd24091` provide the typed cloud broker/RPC boundary, production AWS HTTPS/SigV4 and EventStream adapter, broker-owned cloud-state storage, runtime dispatch, and dynamic `/models` catalogs. `synaps login --provider aws-bedrock` now performs dynamic client registration, device polling, explicit account/role selection (or explicit unattended selectors), role credential acquisition, and atomic broker-state persistence. The opt-in live gate is `scripts/aws-bedrock-live-smoke.sh`; it remains unclaimed unless its prerequisite environment is present.

Azure and Vertex continue to report `registration_required` when their respective Synaps-owned public client IDs are absent. Provider-local zero-network contracts cover Azure device polling/two-audience tokens/ARM deployments and Vertex PKCE/refresh/catalog/SSE. Live readiness remains blocked by the registrations and cloud resources listed in §§0 and 5; fixture success does not waive those gates.

## 10. Configured-registration production wiring evidence

Azure and Vertex are no longer unconditional stubs. `synaps login` resolves `SYNAPS_AZURE_CLIENT_ID` and `SYNAPS_VERTEX_CLIENT_ID` (the legacy `SYNAPS_GOOGLE_VERTEX_CLIENT_ID` alias remains accepted), validates explicit provider context, and executes Azure device authorization/two-audience token acquisition or Vertex loopback PKCE/offline token exchange. Missing IDs still produce `registration_required`. Broker-owned state persists refresh/access material atomically; the production `LocalBroker` refreshes tokens, performs Azure ARM deployment and Vertex publisher-model discovery, and invokes pinned Azure Responses or Vertex public generate-content endpoints through typed catalog/invoke operations. Runtime and `/models` therefore use the existing provider-qualified broker seams.

Focused evidence: Azure provider contract E2E (6 passed), Vertex runtime contract tests (4 passed), and workspace check passed. Live validation remains deferred pending actual registrations/accounts; this is implementation evidence, not a live-gate claim.

## 11. Holdout hardening evidence (2026-03-12)

The follow-up release-hardening slice makes all three production response paths incremental and cancellation-safe: AWS EventStream is CRC-checked frame-by-frame with a 1 MiB frame bound; Azure and Vertex SSE are parsed across arbitrary HTTP chunk boundaries with a 1 MiB event bound; dropping any returned stream drops its upstream response. Remote broker NDJSON is likewise incremental and bounded rather than assuming one event per HTTP chunk. Malformed/truncated streams fail without synthesizing success.

Catalog entries now carry broker-generated opaque `ctx-…` route IDs and `fetched_at` epoch milliseconds; remote catalogs are bounded and validated before use. Provider IDs remain accepted only as bootstrap selectors, while returned/persisted routes are opaque. Tool input is rejected before catalog or invocation network activity until provider tool translation is implemented. Azure CLI device polling now enforces the advertised expiry and responds to cancellation. AWS SigV4 signs explicit `content-type` and `x-amz-content-sha256` headers (including the EventStream media type), in addition to host/date/session token. Credential-bearing local and remote clients disable redirects and use bounded connect/request budgets.

Verification evidence for this slice: `cargo test -p synaps-core --lib -- --test-threads=1` (320 passed), `cargo test -p synaps-core --test aws_bedrock_sso_e2e -- --test-threads=1` (3 passed), and `cargo check --workspace` passed with `CARGO_BUILD_JOBS=8`. AWS live execution remains unclaimed because the opt-in prerequisites were absent.
