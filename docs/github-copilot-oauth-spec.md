# GitHub Copilot OAuth for `synaps login`

Status: **research / design scaffold for lead review** — branch `feat/github-copilot-oauth`  
Worktree: `/home/jr/Projects/Maha-Media/.worktrees/SynapsCLI-github-copilot-oauth`  
Phase: **spec only** — no product-code changes in this phase.

Add GitHub Copilot as a first-class OAuth provider alongside Anthropic, OpenAI
Codex, and xAI. This is **not** a static API-key entry in the OpenAI-compatible
registry. Login, refresh, credential storage, broker policy, model routing, and
request headers all differ from existing providers.

This document separates:

1. **Verified** facts from GitHub official docs / RFCs / published allowlists.
2. **Community-observed** protocol details that are widely implemented but not
   published as a stable public API contract for third-party clients.
3. **Unknowns / open questions** that must be decided or re-verified before
   implementation is considered complete.
4. **Security / terms constraints** that reject insecure designs (especially
   raw long-lived token vending).

---

## ASSUMPTIONS I'M MAKING

1. The product goal is interactive `synaps login` + runtime model use under a
   Copilot subscription (Free/Pro/Business/Enterprise), not BYOK-to-other-LLMs.
2. We will follow the existing typed provider + credential-broker architecture
   (`OAuthProviderId`, `BrokerCredentialStrategy`, `LocalBroker` /
   `RemoteBroker`, atomic `auth.json` merge storage). See
   `docs/decisions/credential-broker-checkpoint-1.md` and
   `docs/grok-xai-oauth-spec.md`.
3. Device Authorization Grant (RFC 8628) is preferred over localhost callback
   because that is what official Copilot CLI documents for interactive login.
4. We will **not** ship a design that vends the long-lived GitHub user token
   (`ghu_` / `gho_` / fine-grained PAT) over the broker `/token` endpoint.
5. Community reverse-engineered endpoints (`copilot_internal`,
   `*.githubcopilot.com` chat/models) are **working protocol evidence**, not a
   GitHub-supported public product API for arbitrary third-party agents. Terms
   risk is called out explicitly below and needs lead decision.
6. Enterprise (GHE.com / GHES / Copilot Enterprise tenant routing) is
   out-of-scope for v1 unless verified end-to-end; design should leave a clean
   extension point.

→ Proceed with these unless the lead overrides them.

---

## Objective

**What:** First-class GitHub Copilot OAuth provider for SynapsCLI.

**Who:** Users with a GitHub Copilot plan who want to authenticate via
`synaps login` and route models as `github-copilot/<model-id>` (exact key TBD —
see Provider identity).

**Success looks like:**

- Interactive device-flow login works without a localhost callback port.
- Credentials persist under a single canonical provider key in `auth.json`.
- Refresh produces a short-lived Copilot **session** token without re-prompting
  the browser while the long-lived GitHub token remains valid.
- Runtime can list models and stream chat completions with required headers.
- Broker never exposes the long-lived GitHub token to remote peers.
- Anthropic / Codex / xAI login, refresh, routing, and broker suites remain green.

---

## Provider identity

Propose one canonical key throughout login, `auth.json`, token refresh, broker,
routing, and model IDs:

```text
github-copilot
```

| Surface | Value |
| --- | --- |
| Canonical storage / broker id | `github-copilot` |
| User-facing name | **GitHub Copilot** |
| CLI aliases (normalize only) | `copilot`, `github-copilot` (optional: `gh-copilot`) |
| Model route prefix | `github-copilot/<model-id>` |

Do **not** mix `copilot`, `github`, `gh`, and `github-copilot` as storage keys.
CLI aliases must normalize to the canonical id before they enter
`OAuthProviderId` / storage (same pattern as `claude` → `anthropic`).

---

## Evidence tiers

| Tier | Meaning |
| --- | --- |
| **V** | Verified from GitHub official documentation, RFC, or GitHub-published allowlist. |
| **C** | Community / multi-client observation (VS Code-era clients, open-source proxies). Useful for protocol reconstruction; **not** a public stable contract. |
| **U** | Unknown / conflicting / needs lead decision or live verification. |

---

## Verified protocol facts (V)

### V1. Interactive Copilot CLI auth is OAuth device flow

Official docs: *Authenticating GitHub Copilot CLI*
<https://docs.github.com/en/copilot/how-tos/copilot-cli/set-up-copilot-cli/authenticate-copilot-cli>

- Default interactive method is **OAuth device flow** (`copilot login` / `/login`).
- CLI shows a one-time user code and directs the user to
  `https://github.com/login/device`.
- After consent, CLI remembers login; token lifetime depends on how the token
  was created / org settings.
- Supported token types for Copilot CLI:

  | Token type | Prefix | Supported |
  | --- | --- | --- |
  | OAuth (device flow) | `gho_` | Yes |
  | Fine-grained PAT | `github_pat_` | Yes — personal account only, **Copilot Requests** permission |
  | GitHub App user-to-server | `ghu_` | Yes (via env) |
  | Classic PAT | `ghp_` | **No** |

- Credential priority in Copilot CLI:
  1. `COPILOT_GITHUB_TOKEN`
  2. `GH_TOKEN`
  3. `GITHUB_TOKEN`
  4. OAuth token from OS keychain (`copilot-cli`)
  5. `gh auth token` fallback

### V2. GitHub Device Authorization Grant endpoints

Official docs: *Authorizing OAuth apps* — Device flow
<https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#device-flow>

| Step | Method + URL | Notes |
| --- | --- | --- |
| Request codes | `POST https://github.com/login/device/code` | Params: `client_id` (required), `scope` (optional) |
| User verification | Browser: `https://github.com/login/device` | User enters `user_code` |
| Poll for token | `POST https://github.com/login/oauth/access_token` | `grant_type=urn:ietf:params:oauth:grant-type:device_code`, `client_id`, `device_code` |

Device-code response fields (official):

- `device_code` (40 chars)
- `user_code` (8 chars with hyphen)
- `verification_uri` (default `https://github.com/login/device`)
- `expires_in` (default **900** seconds)
- `interval` (minimum poll seconds; default **5**)

Poll error codes (official): `authorization_pending`, `slow_down` (+5s),
`expired_token` / token expired, `access_denied`, `incorrect_device_code`,
`incorrect_client_credentials`, `unsupported_grant_type`,
`device_flow_disabled`.

**Device flow does not require `client_secret`** (official).

Prefer `Accept: application/json` so responses are JSON rather than form-encoded.

RFC reference: OAuth 2.0 Device Authorization Grant
<https://datatracker.ietf.org/doc/html/rfc8628>

### V3. Web application OAuth (for contrast; not the preferred CLI path)

Official same page documents authorization-code flow:

- Authorize: `GET https://github.com/login/oauth/authorize`
- Exchange: `POST https://github.com/login/oauth/access_token`
- Requires `client_secret` for exchange (unlike device flow)
- PKCE S256 supported / strongly recommended; `plain` not supported

Synaps already has localhost-callback + PKCE primitives for Anthropic / Codex /
xAI. Device flow is still preferred for Copilot because official Copilot CLI
uses it and it avoids a free local port.

### V4. Official network allowlist names Copilot internal + API hosts

Official docs: *Copilot allowlist reference*
<https://docs.github.com/en/copilot/reference/copilot-allowlist-reference>

Relevant entries (non-exhaustive):

| URL / pattern | Purpose |
| --- | --- |
| `https://github.com/login/*` | Authentication |
| `https://api.github.com/user` | User management |
| `https://api.github.com/copilot_internal/*` | User management |
| `https://copilot-proxy.githubusercontent.com` | Suggestions API |
| `https://*.githubcopilot.com/*` | Suggestions API |
| `https://*.individual.githubcopilot.com` | Suggestions API |
| `https://*.business.githubcopilot.com` | Suggestions API |
| `https://*.enterprise.githubcopilot.com` | Suggestions API |

This **verifies the existence and official operational role** of
`api.github.com/copilot_internal/*` and `*.githubcopilot.com` for Copilot
clients. It does **not** by itself publish a public third-party REST contract
for chat completions.

### V5. Official Copilot SDK authentication surface

Official docs: *Authentication* (Copilot SDK)
<https://docs.github.com/en/copilot/how-tos/copilot-sdk/auth/authenticate>

- Interactive apps use GitHub OAuth device flow + stored credentials.
- Apps may pass a user access token (`gho_`, `ghu_`, fine-grained `github_pat_`)
  into the SDK; classic `ghp_` is not supported.
- Env vars: `COPILOT_GITHUB_TOKEN` > `GH_TOKEN` > `GITHUB_TOKEN`.
- BYOK bypasses GitHub Copilot auth entirely (out of scope for this provider).

### V6. Terms / product-policy pointers (high level)

- Additional Products terms point Copilot Business/Enterprise users at
  [GitHub Copilot Product Specific Terms](https://github.com/customer-terms/github-copilot-product-specific-terms)
  and other users at Section J (AI Features) of the GitHub Terms of Service:
  <https://docs.github.com/en/site-policy/github-terms/github-terms-for-additional-products-and-features>
- Section J (AI Features) governs Inputs/Outputs and AI feature use under the
  general ToS:
  <https://docs.github.com/en/site-policy/github-terms/github-terms-of-service#j-ai-features-training-and-your-data>

GitHub’s public docs **do not** publish a “build your own OpenAI-compatible
proxy against `api.githubcopilot.com`” guide for arbitrary third-party agents.
See **Terms / policy risk** below.

---

## Community-observed protocol (C) — reconstruct carefully

These details appear consistently across independent open-source clients and
docs (e.g. agent-zero `github_copilot.py`, llm-liberty GitHub Copilot notes,
copilot-to-api guides, VS Code-era device-flow examples). Treat as
**implementation evidence**, not GitHub-supported API stability guarantees.

### C1. Public client ID used by Copilot clients

```text
Iv1.b507a08c87ecfe98
```

- Widely described as the **public** GitHub App / OAuth client id used by
  official Copilot editor clients (VS Code lineage).
- Not a client secret. Safe to embed in a native public client the same way
  Anthropic / Codex / xAI public client ids are embedded today.
- **Provenance:** community consensus + device-flow examples labeled as VS Code
  client id. **Not** found as a first-class documented constant on
  docs.github.com during this research pass.

**Lead decision needed:** confirm whether Synaps may reuse this public client
id (common practice) or must register its own GitHub App/OAuth app with device
flow enabled. Own-app registration changes scopes, consent screen branding, and
possibly access to Copilot session exchange.

### C2. Device-flow scope

Common request:

```text
scope=read:user
```

Variants observed: `user:email`, empty scope. Official device-flow docs allow
optional scope; Copilot-specific required scope is **not** published on the
official Copilot CLI auth page.

**Recommendation for v1:** start with `read:user` (least privilege among common
working values); verify live that session-token exchange succeeds. Do not
request `repo` or other broad scopes without a product reason.

### C3. Two-token model (critical)

Copilot runtime auth is a **two-step** credential chain:

```text
1) Device flow  →  long-lived GitHub user token   (ghu_… or gho_…)
2) Session mint →  short-lived Copilot token      (often tid=…;exp=…;proxy-ep=…)
```

| Role | What it is | Lifetime (observed) | Storage mapping |
| --- | --- | --- | --- |
| Long-lived GitHub token | Result of device poll `access_token` | Long / until revoked | Map to `OAuthCredentials.refresh` |
| Copilot session token | Result of internal token exchange | ~25–30 minutes | Map to `OAuthCredentials.access` + `expires` |

There is **no** standard `grant_type=refresh_token` POST for step 2 in the
community protocol. Session mint/refresh is:

```http
GET https://api.github.com/copilot_internal/v2/token
Authorization: Bearer <github_user_token>
Accept: application/json
```

Some clients also send editor/integration headers on this request (see C5).

Response (observed shape):

```json
{
  "token": "tid=…;exp=…;sku=…;proxy-ep=…;…",
  "expires_at": 1710000000
}
```

Notes:

- `expires_at` may be unix **seconds** (convert to ms for Synaps storage).
- Some payloads may include `expires_in` instead; handle both.
- Apply Synaps skew (2–5 minutes) when writing `OAuthCredentials.expires`,
  consistent with Anthropic / Codex / xAI.

**Important:** community clients often misuse OAuth vocabulary and call the
GitHub user token a “refresh_token”. In Synaps storage that mapping is fine
(`refresh` field holds the long-lived secret), but **broker vending must treat
it as a refresh credential, never as a vended access token**.

### C4. Inference base URL

Observed defaults:

| Plan / routing | Base URL |
| --- | --- |
| Individual (common default) | `https://api.individual.githubcopilot.com` |
| Legacy / generic | `https://api.githubcopilot.com` |
| Enterprise tenant | `https://api.<tenant>.githubcopilot.com` derived from session token `proxy-ep=proxy.<tenant>.githubcopilot.com` by rewriting `proxy.` → `api.` |

OpenAI-compatible paths observed:

- `GET  {base}/models`
- `POST {base}/chat/completions` (SSE when `stream: true`)
- Some clients also expose `/responses` (less consistent)

Enterprise tokens may **require** `stream: true` (non-streaming `400`) —
observed, not officially documented for third parties.

### C5. Required / conventional request headers

Community clients consistently send some subset of:

```http
Authorization: Bearer <copilot_session_token>
Content-Type: application/json
Accept: application/json   # or text/event-stream when streaming
User-Agent: GitHubCopilotChat/0.35.0
Editor-Version: vscode/1.107.0
Editor-Plugin-Version: copilot-chat/0.35.0
Copilot-Integration-Id: vscode-chat
```

Observed failure modes:

- Missing / wrong `Copilot-Integration-Id` → `403 token not authorized for this integration`.
- Missing editor headers → some preview models rejected.

**U / risk:** spoofing VS Code editor headers may be necessary for the
community protocol to work, but is ethically/ToS-sensitive. Lead must decide
whether Synaps:

1. Uses neutral self-identifying headers (may break),
2. Uses the conventional VS Code integration id (works in community clients),
3. Or only supports official SDK/CLI-mediated access (different architecture).

### C6. Model discovery and policy enablement

- `GET {base}/models` returns an OpenAI-like list (`data[].id` or similar).
- Some clients POST `{base}/models/{id}/policy` with `{"state":"enabled"}` to
  unlock models for the account. This is **not** officially documented for
  third parties; treat as optional / best-effort, never as a silent destructive
  action without user intent.

Do **not** hard-code an entire community model catalog as “supported” without
end-to-end verification against the selected wire path (same rule as xAI spec).

### C7. Auth header variants on session mint

Observed:

- `Authorization: Bearer <github_token>`
- `Authorization: token <github_token>` (older GitHub style)

Prefer `Bearer` first; fall back only if live verification requires it.

---

## Unknowns and open questions (U)

| ID | Question | Why it matters |
| --- | --- | --- |
| U1 | May Synaps reuse client id `Iv1.b507a08c87ecfe98`? | Own app vs shared app; consent UX; possible future revocation of third-party reuse |
| U2 | Exact device-flow `scope` required for Copilot session mint | Too little → exchange fails; too much → over-privilege |
| U3 | Is `GET /copilot_internal/v2/token` a permitted third-party integration surface? | ToS / account risk; supportability |
| U4 | Canonical integration headers for a non-VS-Code client | 403s vs ToS/spoofing concerns |
| U5 | Stable public base URL for individual vs business vs enterprise | Routing bugs; hard-coding wrong host |
| U6 | Whether non-streaming chat is allowed for all plan types | Wire path and tests |
| U7 | Whether `/responses` is first-class on Copilot hosts | Wire protocol choice in `ResolvedRoute` |
| U8 | GHE.com / GHES / data-residency host parameterization | Enterprise users |
| U9 | Whether fine-grained PAT import (`github_pat_` + Copilot Requests) is in scope for v1 | Alternate login path; still two-step session mint? |
| U10 | Product-Specific Terms text for Business/Enterprise (fetch failed during research) | Legal go/no-go for org customers |
| U11 | Does GitHub rotate/invalidate the long-lived device token, and can it be refreshed without full re-login? | Storage + UX for expiry |
| U12 | Rate limits specific to session mint and chat beyond generic GitHub API limits | Backoff design |

**Live verification checklist (implementation phase, not this phase):**

1. Device code request with chosen client id + scope.
2. Poll to GitHub user token; record token prefix (`gho_` vs `ghu_`).
3. Session mint against `copilot_internal/v2/token`; capture full JSON shape
   (redact secrets in logs).
4. `GET /models` and streaming `POST /chat/completions` with candidate headers.
5. Confirm expiry skew and re-mint path without browser.
6. Confirm failure modes: no subscription, org policy, revoked auth.

---

## Relationship to existing Synaps OAuth

### Shared primitives (reuse)

From `crates/agent-core/src/core/auth/`:

- `generate_state` (if needed for correlation of local device attempts)
- `open_browser` (open `verification_uri`; pre-fill is browser/OS dependent)
- `OAuthCredentials` + `save_provider_auth` / `load_provider_auth`
- atomic merge storage (preserve other providers)
- `CredentialSource::{Local, Remote}`, `TokenCache`, broker contract
- expiry skew pattern (`now + ttl - margin`)
- typed `OAuthProviderId` / `OAuthProviderDescriptor` /
  `BrokerCredentialStrategy` registry

### Provider-specific differences (do **not** copy 1:1)

| Concern | Anthropic / Codex / xAI | GitHub Copilot |
| --- | --- | --- |
| Grant type | Auth code + PKCE (+ localhost callback) | **Device code** (RFC 8628) |
| Callback server | Required | **None** |
| Client secret | Not used (public native clients) | Not used for device flow (V) |
| Refresh | Standard `grant_type=refresh_token` POST (or OIDC discovery for xAI) | **Non-standard GET** session mint with long-lived GitHub token |
| Access token meaning | Provider API bearer | **Short-lived Copilot session token** |
| Refresh token meaning | OAuth refresh | **Long-lived GitHub user token** (must never leave broker) |
| API base | Fixed per provider | **Token-dependent** (`proxy-ep` / plan host) |
| Extra headers | Minimal / provider-specific | Integration + editor headers (C) |
| Account id | Codex extracts ChatGPT account id | Optional GitHub login / enterprise domain later; not required for v1 |

### Mapping onto Checkpoint 1 broker policy

From `docs/decisions/credential-broker-checkpoint-1.md`:

- OAuth access-token vending is permitted **only** for short-lived access
  tokens + expiry.
- Refresh credentials never cross the broker boundary.
- Static keys are never vended; proxy/sign instead.
- Fail closed; no remote raw secret disclosure.

**Required interpretation for Copilot:**

| Secret | Broker treatment |
| --- | --- |
| Long-lived GitHub token (`ghu_`/`gho_`/`github_pat_`) | **Refresh-class.** Never return from `GET /token`. Never log. |
| Copilot session token (`tid=…`) | **Access-class.** May be vended with `expires` **only if** runtime can complete requests with that bearer alone **and** base URL / headers are not secret. |
| Editor/integration header set | Non-secret config; pin broker-side if proxying. |

**Preferred security posture (recommended):**

1. **Default:** `BrokerCredentialStrategy::OAuthAccessToken` vends **only** the
   short-lived session token + expiry after broker-side mint/refresh.
2. **Runtime path:** either
   - (A) local runtime uses vended session token + pinned headers/base URL, or
   - (B) broker `proxy` / `proxy_stream` for Copilot hosts (stronger: long-lived
     token never leaves broker host even as a mistaken access token).
3. **Reject:** any design that returns the GitHub user token from `/token`,
   embeds it in remote capability payloads, or treats Copilot as a static API
   key provider with raw key vending / same-host compatibility mode.

If session tokens are bound to integration id / host in a way that makes bare
token vending unsafe or insufficient, escalate to **broker-side proxy only**
and do not advertise remote token vending for this provider.

---

## Implementation map (design only — not authorized to code yet)

### 1. Auth provider module

Add `crates/agent-core/src/core/auth/github_copilot.rs` (name TBD) with:

1. **Device start:** `POST /login/device/code` with public client id + scope.
2. **User prompt:** print `user_code`, open `verification_uri`, instruct paste.
3. **Poll loop:** honor `interval`, handle `authorization_pending` / `slow_down`
   / expiry / denial; never busy-loop.
4. **Session mint:** `GET /copilot_internal/v2/token` with GitHub user token.
5. **Persist:**

   ```json
   {
     "type": "oauth",
     "refresh": "<github_user_token>",
     "access": "<copilot_session_token>",
     "expires": 1710000000000
   }
   ```

   Optional future metadata (only if storage type is extended carefully):
   `enterprise_domain`, validated `base_url`. Prefer deriving base URL from the
   fresh session token at use time over trusting stale stored hosts.

6. **Refresh path:** re-mint session token from stored GitHub user token; do
   **not** require browser if GitHub token still valid. On `401/403` from mint,
   surface re-login.

Re-export via `providers.rs` + `provider.rs` dispatch.

### 2. Typed registry changes

Extend:

```rust
// conceptual — not applied in this phase
OAuthProviderId::GitHubCopilot  // as_str() == "github-copilot"
ProviderBehavior::GitHubCopilot
BrokerCredentialStrategy::OAuthAccessToken // or proxy-only decision
```

Update `parse_cli_provider`, descriptors, login/refresh match arms, and tests
that freeze the registry.

### 3. Login UI

`src/cmd/login.rs` already builds the OAuth list from
`auth::provider::registry()`. Adding a descriptor should surface the provider;
still add explicit tests for key/alias/`oauth_storage_key` behavior.

Help text in `src/main.rs` examples should include
`synaps login github-copilot` once implemented.

### 4. Credential broker

- Allow `github-copilot` only under a strategy that cannot vend the GitHub user
  token.
- `ensure_fresh_provider_token` must understand session re-mint (GET internal),
  not standard refresh_token POST.
- Remote `/token` response remains `{ access_token, expires }` only — where
  `access_token` is the **session** token.
- If choosing proxy path: pin allowed hosts to
  `api.github.com` (mint only, broker-internal) and allowlisted
  `*.githubcopilot.com` chat/models paths; reject absolute caller URLs.

### 5. Runtime routing

Extend `resolve_route` (see `crates/agent-engine/src/runtime/openai/mod.rs`)
with something like:

```text
github-copilot/<model-id>
```

Design constraints:

- Auth: `AuthPolicy::OAuthAccessToken(GitHubCopilot)` and/or broker proxy.
- Wire: start with `OpenAiChatCompletions` unless live tests prove Responses is
  required.
- Endpoint: derived from latest session token / known individual default; do
  not silently send enterprise tokens to the individual host.
- Attach required headers on the Copilot path only; do not leak them into
  unrelated providers.

### 6. Optional PAT import (follow-up)

Official Copilot CLI supports fine-grained PAT with **Copilot Requests**.
Treat as a separate defensive import path:

- accept only `github_pat_` (reject `ghp_`)
- never log token material
- still run session mint before first inference
- store under the same canonical provider key

---

## Security constraints (normative for implementation)

### Always do

- Use device flow over HTTPS only; validate hosts before sending codes/tokens
  (`github.com` / configured enterprise host allowlist).
- Persist credentials with existing atomic merge + mode-600 auth file behavior.
- Store long-lived GitHub token only in `refresh` (or equivalent broker-owned
  field); vend only short-lived session tokens.
- Apply expiry skew; single-flight refresh like other providers.
- Redact tokens, device codes, and authorization headers from logs and error
  strings.
- Shut down any temporary resources; device flow has no callback server, but
  poll tasks must be cancel-safe.
- Fail closed on unknown providers, bad proxy paths, and missing credentials.

### Ask first

- Registering a Synaps-owned GitHub App instead of the public Copilot client id.
- Supporting GHE.com / GHES hosts.
- Enabling model policy endpoints (`/models/{id}/policy`).
- Spoofing VS Code integration headers vs negotiating official integration.
- Shipping remote broker proxy for Copilot hosts.
- Importing tokens from `gh auth token` or Copilot CLI keychain.

### Never do

- **Never vend the long-lived GitHub user token** via broker `/token`,
  capabilities, logs, telemetry, or UI.
- **Never** treat Copilot as `SameHostStaticKeyCompatibility` raw-key mode.
- **Never** accept classic PATs (`ghp_`) for this provider.
- **Never** commit client secrets, user tokens, device codes, or session
  tokens.
- **Never** send device codes or tokens to non-allowlisted hosts.
- **Never** log full callback/query/device responses.
- **Never** claim GitHub “officially supports” third-party OpenAI-compatible
  reuse of `copilot_internal` without a docs citation that says so.

---

## Terms / policy risk (lead decision required)

### What is solid

- Users may authenticate to GitHub / Copilot via documented OAuth device flow
  and supported token types (V1, V5).
- Official clients and firewall allowlists use `copilot_internal` and
  `*.githubcopilot.com` (V4).
- GitHub publishes a Copilot SDK that accepts user tokens for building
  Copilot-powered apps (V5).

### What is not solid

- There is **no** first-class public docs page that says “third-party agents
  may call `api.github.com/copilot_internal/v2/token` and
  `api.individual.githubcopilot.com/chat/completions` as a general
  OpenAI-compatible provider.”
- Community discussion (e.g. GitHub org community thread #178117) asserts that
  using the internal Copilot endpoint as a generic model provider outside
  official clients can violate Copilot terms / license. That is **community
  guidance**, not a substitute for counsel, but it is a real product risk.
- Open-source “copilot-to-api” proxies themselves warn of unofficial status.

### Product options for lead

| Option | Description | Risk posture |
| --- | --- | --- |
| **A. Full community protocol** | Device flow + internal session mint + direct `*.githubcopilot.com` chat | Highest capability; highest ToS/support risk |
| **B. Auth-only + official SDK path** | Login/store GitHub token; inference only through supported SDK/CLI mechanisms | Lower protocol-risk; larger eng integration |
| **C. Documented PAT/SDK only** | No device-flow client-id reuse; user supplies supported token; still constrained inference path | Medium |
| **D. Defer shipping** | Keep research; do not enable by default until legal/product sign-off | Safest |

This research branch may continue design for **Option A with hard security
boundaries**, but **shipping should be gated** on an explicit lead decision
about ToS acceptance and client-id provenance (U1, U3, U4, U10).

---

## Required tests (when implementation is authorized)

### Auth / device flow

- device start request hits allowlisted host with expected client id + scope
- poll respects interval; `slow_down` increases wait
- `authorization_pending` continues; `access_denied` / expiry fail cleanly
- device flow never starts a localhost callback server
- session mint uses GitHub user token; stores session token as `access`
- refresh re-mints session token without browser when GitHub token valid
- refresh failure on revoked GitHub token asks user to re-login
- rejects HTTP / non-allowlisted hosts for device and mint endpoints

### Storage / isolation

- saving Copilot preserves Anthropic / Codex / xAI entries (and vice versa)
- credential JSON never writes classic PAT acceptance path
- no token material in Display/Debug of public error types

### Broker

- `/token?provider=github-copilot` returns **session** token only (or is
  disabled if proxy-only decision wins)
- response structurally cannot include refresh/GitHub user token
- remote peer cannot obtain long-lived token via `/proxy`, `/capabilities`, or
  error bodies
- static-key providers still denied on `/token`

### Runtime

- `github-copilot/<verified-model>` routes without API-key config entry
- request includes required headers and Bearer session token
- base URL selection does not send tokens to attacker-controlled hosts
- Anthropic / Codex / xAI regression suites remain green

### Harness

- headless simulation of device flow (fake HTTPS servers for device code,
  poll, mint, models, chat) with **no real secrets**
- clock-skew / expiry re-mint test

---

## Commands (workspace)

```bash
# research branch only right now
git -C /home/jr/Projects/Maha-Media/.worktrees/SynapsCLI-github-copilot-oauth status

# later implementation gates (do not claim pass without running)
cargo test -p synaps-core --auth
cargo test -p synaps --lib auth_broker
cargo test --workspace
cargo clippy --all-targets -- --deny warnings
```

---

## Project structure touchpoints (future)

| Area | Path |
| --- | --- |
| OAuth providers | `crates/agent-core/src/core/auth/` |
| Typed registry | `crates/agent-core/src/core/auth/provider.rs` |
| Broker | `crates/agent-core/src/core/auth/broker.rs`, `src/cmd/auth_broker.rs` |
| Login UI | `src/cmd/login.rs` |
| Runtime route | `crates/agent-engine/src/runtime/openai/mod.rs` |
| Specs | `docs/github-copilot-oauth-spec.md` (this file) |
| Prior art | `docs/grok-xai-oauth-spec.md`, `docs/decisions/credential-broker-checkpoint-1.md` |

---

## Code style / testing strategy (when coding begins)

- Follow existing provider modules (`openai_codex.rs`, `xai.rs`): small pure
  URL builders + explicit token validation + unit tests for request shape.
- Prefer table-driven host-allowlist tests.
- No real network in unit tests; use `httpmock` / local hyper servers if already
  established in crate tests.
- Integration tests must not require a live Copilot subscription by default;
  gate any ignored live test behind an explicit env flag.

---

## Boundaries summary

| Tier | Items |
| --- | --- |
| **Always** | Device-flow login design; two-token model; broker keeps GitHub user token private; short-lived session token only at boundary; host allowlists; no classic PAT; redaction; registry typing; tests listed above |
| **Ask first** | Own GitHub App registration; enterprise hosts; header spoofing policy; model policy enablement; remote proxy; PAT import; legal sign-off to ship Option A |
| **Never** | Raw long-lived token vending; `ghp_` support; committing secrets; silent ToS claims; implementing on primary checkout outside this worktree |

---

## Source index

### Official

1. Authenticating GitHub Copilot CLI —  
   <https://docs.github.com/en/copilot/how-tos/copilot-cli/set-up-copilot-cli/authenticate-copilot-cli>
2. Troubleshooting Copilot CLI authentication —  
   <https://docs.github.com/en/copilot/how-tos/copilot-cli/set-up-copilot-cli/troubleshoot-copilot-cli-auth>
3. Authorizing OAuth apps (device flow + web flow) —  
   <https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps>
4. Scopes for OAuth apps —  
   <https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/scopes-for-oauth-apps>
5. Copilot allowlist reference —  
   <https://docs.github.com/en/copilot/reference/copilot-allowlist-reference>
6. Copilot SDK authentication —  
   <https://docs.github.com/en/copilot/how-tos/copilot-sdk/auth/authenticate>
7. GitHub Terms for Additional Products and Features (Copilot section) —  
   <https://docs.github.com/en/site-policy/github-terms/github-terms-for-additional-products-and-features>
8. GitHub Terms of Service §J AI Features —  
   <https://docs.github.com/en/site-policy/github-terms/github-terms-of-service#j-ai-features-training-and-your-data>
9. RFC 8628 Device Authorization Grant —  
   <https://datatracker.ietf.org/doc/html/rfc8628>

### Community / secondary (protocol reconstruction only)

10. agent-zero GitHub Copilot OAuth provider —  
    <https://github.com/agent0ai/agent-zero> (`plugins/_oauth/helpers/providers/github_copilot.py`)
11. llm-liberty GitHub Copilot notes —  
    <https://github.com/BodhiSearch/llm-liberty/blob/main/docs/github-copilot.md>
12. copilot-to-api README / guides —  
    <https://github.com/Alorse/copilot-to-api>
13. Device flow example citing VS Code client id —  
    <https://github.com/estruyf/github-copilot-usage-tauri/blob/main/device_flow_example.md>
14. Community discussion on `copilot_internal` third-party use —  
    <https://github.com/orgs/community/discussions/178117>

### In-repo architecture references

15. `docs/grok-xai-oauth-spec.md` — sibling OAuth provider spec pattern  
16. `docs/decisions/credential-broker-checkpoint-1.md` — broker vending policy  
17. `crates/agent-core/src/core/auth/provider.rs` — typed OAuth registry  
18. `crates/agent-core/src/core/auth/broker.rs` — access-token vs proxy boundary  
19. `crates/agent-engine/src/runtime/openai/mod.rs` — `resolve_route` / `AuthPolicy`  
20. `src/cmd/login.rs` — login provider list from registry  
21. `src/cmd/auth_broker.rs` — remote `/token` allowlist via registry strategy  

---

## Lead review checklist

- [ ] Accept or replace canonical id `github-copilot`
- [ ] Decide client-id strategy (reuse `Iv1.b507a08c87ecfe98` vs own app) — **U1**
- [ ] Decide ship posture Option A/B/C/D given ToS risk — **U3/U10**
- [ ] Decide header identity policy — **U4**
- [ ] Decide broker mode: session-token vend vs proxy-only
- [ ] Confirm v1 non-goals: enterprise hosts, PAT import, model policy posts
- [ ] Authorize implementation phase only after the above

---

## Change log

| Date | Change |
| --- | --- |
| 2026-07-12 | Initial research/spec scaffold on `feat/github-copilot-oauth`; no product code changes |
