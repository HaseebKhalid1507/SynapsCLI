# Cloud OAuth Providers — Implementation Specification

Status: **implementation-ready specification; deployment blocked on named feasibility gates**
Branch: `feat/cloud-oauth-providers`
Scope: Microsoft Azure OpenAI, AWS Bedrock, and Google Vertex AI. No product code is changed by this document.

## 1. Objective and non-goals

Add three typed cloud providers to SynapsCLI’s existing provider/credential-broker/runtime architecture:

| Canonical ID | Authentication | Runtime |
|---|---|---|
| `azure-openai` | Microsoft device authorization; OAuth access tokens | Azure OpenAI deployed models |
| `aws-bedrock` | IAM Identity Center OIDC device authorization; temporary IAM role credentials | Bedrock Runtime signed with SigV4 |
| `google-vertex` | Google installed-app authorization code + PKCE | Vertex AI public REST/SSE |

All durable credentials remain broker-owned. Catalogs are discovered dynamically and the TUI `/models` view consumes normalized broker catalog entries. Static model lists may exist only as an explicitly stale/error fallback, never as evidence of account availability.

**Non-goals:** Azure AI Foundry; Azure consumer/work-account provisioning; AWS bearer-token inference; AWS access-key import; Google Gemini CLI/Code Assist; service accounts/workload identity; arbitrary cloud hosts; cross-project Vertex discovery; automatic cloud resource creation. Azure AI Foundry is deferred because its resource/catalog/runtime contracts differ from Azure OpenAI.

## 2. Feasibility gates and assumptions

1. **Microsoft gate:** a Synaps-owned Microsoft Entra **public client/native application registration** with device-code/public-client flow enabled is an external prerequisite. Tenant administrators may also need to consent. Until its client ID and tenant policy are supplied and validated, Synaps MUST NOT claim Azure OAuth is deployable. Implementation may accept a broker-owned/configured public client ID for development or enterprise deployments; it must not borrow another product’s ID or embed a client secret.
2. **Google gate:** a Synaps-owned Google OAuth **Desktop app** client registration, consent screen, and required API enablement are external prerequisites. Until supplied and validated, Synaps MUST NOT claim Vertex OAuth is deployable. A broker-owned configurable desktop client ID is permitted. Desktop clients are public; no client secret is treated as confidential or required by Synaps.
3. AWS IAM Identity Center OIDC dynamic client registration (`RegisterClient`) supplies the client identity at runtime, so no Synaps OAuth registration is required. The user still needs an enabled IAM Identity Center instance, start URL/issuer region, assigned account/role, and Bedrock permissions/model access.
4. Users already own and administer the referenced cloud resources. Synaps performs no resource provisioning.
5. Exact API versions are configuration constants with reviewed defaults and fixture coverage; upgrades require contract tests.
6. Remote broker transport is authenticated and encrypted under the existing broker design. If that premise is false, remote cloud providers remain disabled.

**Release labels:** AWS may graduate after the live AWS gate. Azure and Vertex remain `preview / registration required` and their live gates are deferred until Synaps-owned registrations exist.

## 3. Prerequisites (exact operator checklist)

### Azure OpenAI
- Synaps-owned or broker-configured Entra public client ID; public-client/device-code flow enabled; **no secret**.
- Tenant selector: tenant UUID or approved `organizations`; never silently use `common`.
- Azure subscription ID, resource group, Azure OpenAI account/resource name, and endpoint resolved from ARM metadata.
- Azure OpenAI resource exists and contains at least one model deployment.
- User consent/admin consent for `https://management.azure.com/.default` (control plane) and `https://cognitiveservices.azure.com/.default` (inference); RBAC grants resource/deployment read plus inference invocation.
- Outbound HTTPS to `login.microsoftonline.com`, `management.azure.com`, and the allowlisted resource endpoint.

### AWS Bedrock
- IAM Identity Center enabled; `sso_start_url` (or issuer URL) and SSO region known.
- User assigned to at least one AWS account and role.
- Role permits `bedrock:ListFoundationModels`, `bedrock:Converse`, and `bedrock:ConverseStream` as applicable; Bedrock model access enabled in the selected runtime region.
- Explicit SSO region, account ID, role name, and Bedrock region. These are distinct fields and MUST NOT be inferred from one another.
- Outbound HTTPS only to region-pinned `oidc`, `portal.sso`, `bedrock`, and `bedrock-runtime` AWS hosts.

### Vertex AI
- Synaps-owned/configured Google Desktop OAuth client ID and consent screen; test users enrolled while app is in testing; Vertex AI API enabled.
- OAuth scope exactly includes `https://www.googleapis.com/auth/cloud-platform`; offline access requested for refresh.
- Explicit Google Cloud project ID and Vertex location; user has `aiplatform.models.list`/appropriate discovery permission and prediction/streaming invocation permissions (for example suitable Vertex AI User role).
- Billing enabled and publisher model available in the chosen location.
- Outbound HTTPS to Google authorization/token endpoints and validated regional Vertex host.

## 4. Provider setup and login flows

### 4.1 Azure device code

1. Collect and validate `tenant`, `subscription_id`, `resource_group`, and resource name. Resolve client ID from broker configuration; if absent, fail with `registration_required` and remediation text.
2. Request a device code from `https://login.microsoftonline.com/{tenant}/oauth2/v2.0/devicecode` for ARM scope `https://management.azure.com/.default`.
3. Display the server-supplied verification URI and user code; do not log either. Poll the matching token endpoint, honoring interval, expiry, `authorization_pending`, and `slow_down`; cancellation is immediate.
4. Store refresh material in the broker only. Use the ARM access token to resolve/validate the Azure OpenAI resource and enumerate **deployed models**. Never confuse base models with callable deployment names.
5. Obtain an inference token for `https://cognitiveservices.azure.com/.default` via refresh-token grant. Microsoft may rotate refresh tokens; commit access/refresh updates atomically while preserving unrelated auth metadata.
6. Pin the endpoint returned/derived for the selected Azure OpenAI resource; require HTTPS and an approved Azure Cognitive Services suffix. Persist opaque resource identifiers and selected deployment, not arbitrary caller URLs.

The broker caches ARM and inference access tokens separately by tenant/client/scope, refreshes with skew and single-flight, and never returns the refresh token remotely.

### 4.2 AWS IAM Identity Center OIDC + SigV4

AWS is **not bearer OAuth at inference**. OIDC authorizes acquisition of temporary AWS credentials; Bedrock requests use SigV4.

1. Validate SSO region and start/issuer URL. Call regional `RegisterClient` as a public native client; persist `clientId`, `clientSecret`, and registration expiry broker-side (the generated secret is sensitive despite dynamic registration).
2. Call `StartDeviceAuthorization`; display complete verification URI/user code; poll `CreateToken` with the device-code grant, respecting interval, `AuthorizationPendingException`, `SlowDownException`, denial, expiration, cancellation, and registration expiry.
3. With the SSO access token call `ListAccounts`, then `ListAccountRoles`; require explicit selection when multiple values exist. Do not choose the first silently.
4. Call `GetRoleCredentials(accountId, roleName)` and retain access key ID, secret access key, session token, and expiration only in broker memory/secure storage according to existing credential policy. Refresh by repeating role credential acquisition while the SSO token remains valid; otherwise repeat device authorization.
5. Use broker-side SigV4 (`service=bedrock` for control/catalog and `service=bedrock` for Bedrock Runtime per AWS endpoint signing metadata; region is the explicit Bedrock region). Sign canonical host/path/query/headers/body hash, include `x-amz-security-token`, and enforce clock-skew handling. No AWS key material or pre-signed arbitrary request crosses the broker boundary.

### 4.3 Vertex installed-app PKCE

1. Require configured public desktop client ID, explicit project, and location; absent ID yields `registration_required` rather than a misleading login URL.
2. Generate high-entropy state and PKCE verifier/challenge (S256). Bind a loopback listener on `127.0.0.1`/`::1` ephemeral port; redirect URI must exactly match. Open Google authorization with `response_type=code`, cloud-platform scope, `access_type=offline`, and appropriate consent behavior.
3. Validate callback peer, exact state, one-shot use, timeout, and OAuth error; exchange code at `https://oauth2.googleapis.com/token` using verifier. Never require or log a desktop client secret.
4. Atomically store refresh/access/expiry broker-side. If Google omits a refresh token on later consent, preserve the existing valid refresh token; never overwrite it with null.
5. Validate project/location syntax and pin `https://{location}-aiplatform.googleapis.com`. Discovery lists `publishers/google/models` in that project/location. Runtime invokes the selected publisher model using public `streamGenerateContent` with `alt=sse` (or documented SSE response contract), not Gemini Code Assist internal APIs.

Headless environments may print the URL, but unattended production login cannot bypass state/PKCE; tests inject callback/browser boundaries.

## 5. Runtime and catalog contracts

Canonical model IDs are opaque and namespaced:

- `azure-openai/<deployment-name>` — request path uses the selected deployment; metadata may show underlying model/version.
- `aws-bedrock/<foundation-model-id>` — region/account availability metadata retained; inference uses `Converse` or `ConverseStream`.
- `google-vertex/publishers/google/models/<model-id>` — project/location are provider context, not user-controlled URL fragments.

A normalized `CatalogEntry` must carry provider ID, stable runtime ID, display name, capabilities (text, tools, streaming, vision where verified), source=`dynamic`, and provider context/version. Refresh is bounded, cancellable, deduplicated, size-limited, and cacheable with fetched-at/TTL. Empty/403/404/malformed catalogs surface a provider-specific error and do not silently present guessed availability. Last-known-good entries may be shown as **stale** and invocation still revalidates/fails closed.

Discovery contracts:
- Azure: ARM using `management.azure.com/.default`; enumerate Azure OpenAI account **deployments** and map only supported inference deployments.
- AWS: `ListFoundationModels`; filter to models supporting the required inference type/capabilities and preserve model ARN/ID without accepting arbitrary endpoints.
- Vertex: regional Vertex API collection for `publishers/google/models`; paginate with loop/token bounds and expose only publisher Google models supported by `streamGenerateContent`.

Invocation contract: runtime sends a typed broker request (provider, canonical model ID, normalized messages/tools, stream flag, bounded options). Broker resolves stored provider context, refreshes credentials, constructs the fixed upstream URL, attaches Azure/Google bearer auth or AWS SigV4, disables redirects, streams normalized events, and redacts upstream bodies/headers from errors. Callers cannot supply authority, absolute URL, auth headers, scope, AWS signing inputs, Azure API version, project, or region.

Streaming adapters must support text deltas, tool-call argument accumulation, usage/final metadata, provider errors, cancellation, UTF-8/SSE fragmentation, and exactly one terminal event. Azure uses its deployed-model streaming response; AWS uses `ConverseStream` event stream (non-stream uses `Converse`); Vertex uses public `streamGenerateContent` SSE. Unsupported content blocks fail explicitly rather than being dropped.

## 6. Security boundaries

**Broker-only secrets:** Microsoft/Google refresh and access tokens; AWS OIDC client secret/access token; AWS access key, secret key, and session token. Runtime/TUI receives neither credentials nor authorization headers. Remote token vending is forbidden for these providers; expose typed catalog/invoke operations instead.

Always: TLS; exact scheme/host/suffix/path/method allowlists; redirects off on credential-bearing calls; DNS/proxy policy consistent with broker SSRF defenses; bounded body/event/page counts and timeouts; atomic `auth.json` merge with restrictive permissions; refresh single-flight and expiry skew; redacted `Debug`/errors/telemetry; zero credentials in model IDs.

Fail closed on tenant/project/region changes, unknown model IDs, endpoint metadata outside allowlists, clock/signature errors after one bounded correction, scope mismatch, refresh rotation failure, malformed pagination, and cross-provider credential use. Logout revokes where supported/best-effort, deletes all provider material atomically, and clears catalog caches.

## 7. TUI `/models`

`/models` requests broker catalogs concurrently with bounded concurrency and cancellation. Group as Azure OpenAI, AWS Bedrock, and Google Vertex; show account/resource/project and region/location in non-secret subtitles, plus loading, registration-required, login-required, permission-denied, empty, stale, and retry states. Selection stores only canonical model ID plus non-secret provider context reference. Keyboard navigation/filtering remains stable while refresh completes; duplicate IDs are rejected. The TUI must never read `auth.json`, initiate raw cloud HTTP, render tokens/user codes after login, or imply deferred providers passed live validation.

## 8. Verification and release gates

### Zero-network unattended harnesses

All default CI tests must install a deny-all external connector and use injected HTTP, clock, browser, callback, DNS, signer, and storage seams. Fixtures cover:

- Azure pending/slow-down/deny/expire/cancel, two-scope refresh and rotation, ARM pagination, deployment discovery, endpoint rejection, stream fragmentation.
- AWS registration lifecycle, device polling, account/role selection, role expiry, deterministic SigV4 golden vectors, `ListFoundationModels`, `Converse`, AWS event-stream `ConverseStream`, skew and credential non-egress.
- Vertex PKCE/state/callback replay, refresh preservation/rotation, project/location validation, pagination, publisher discovery, SSE text/tools/errors.
- Shared redirect/SSRF rejection, body/page/event limits, atomic storage fault injection, single-flight, cancellation, redaction snapshots, broker/RPC/TUI credential boundary, `/models` stale/error behavior, and existing-provider regression.

Suggested implementation test files: `tests/azure_openai_oauth_e2e.rs`, `tests/aws_bedrock_sso_e2e.rs`, `tests/google_vertex_oauth_e2e.rs`, and provider-focused unit modules. CI assertion: any attempted non-loopback socket fails the test.

### Live gates

**AWS required before AWS release:** in a dedicated least-privilege sandbox, register/login via device flow, list accounts/roles, obtain short-lived role credentials, list foundation models, send one harmless `Converse` prompt and one cancelled/short `ConverseStream`, verify CloudTrail, redaction, expiration refresh, and logout/cache cleanup. Record region, model ID, timestamp, commit, and pass/fail only—no credentials or response content. Budget cap and cleanup owner required.

**Azure live test deferred** until the Synaps Microsoft public client registration, consent, Azure OpenAI sandbox, and deployment exist. Then verify ARM discovery and one streamed inference with both scopes. **Vertex live test deferred** until the Synaps Google Desktop client, consent/test-user status, billed sandbox project, API, and model access exist. Then verify publisher discovery and one SSE inference. Deferred means not release-validated; fixtures cannot waive these gates.

Final sequential commands after focused tests:

```sh
CARGO_BUILD_JOBS=8 cargo fmt --all -- --check
CARGO_BUILD_JOBS=8 cargo test --workspace -- --test-threads=1
CARGO_BUILD_JOBS=8 cargo check --workspace
CARGO_BUILD_JOBS=8 cargo build --release
```

## 9. Definition of done

Provider-specific focused tests and zero-network E2E pass; existing OAuth/static providers regressions pass; credentials cannot cross broker interfaces; dynamic catalogs drive `/models`; AWS live gate passes; documentation states Azure/Vertex registration and live-test blockers honestly; security review approves host/path/scope/signing boundaries; workspace gates pass; and convergence holdout reaches **0.90** under the plan’s declared weighted axes.
