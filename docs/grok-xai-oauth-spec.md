# Grok (xAI) OAuth for `synaps login`

Status: **reviewed design scaffold** — branch `feat/grok-xai-oauth`

Add xAI/Grok as a third first-class OAuth provider alongside Anthropic and
OpenAI Codex. Protocol reference:
<https://github.com/BlockedPath/pi-xai-oauth> (the source linked from
<https://pi.dev/packages/pi-xai-oauth>).

This is not merely another API-key entry in the OpenAI-compatible registry.
Login, refresh, credential-source handling, model routing, and request format
must all understand the OAuth provider.

## Provider identity

Use one canonical key throughout login, `auth.json`, token refresh, broker,
routing, and model IDs:

```text
xai-auth
```

This matches the reference provider and avoids mixing `xai-grok`, `grok`, and
`xai-auth`. The user-facing name is **xAI (Grok)**.

## Confirmed OAuth metadata

The reference uses OpenID discovery rather than hard-coding authorization and
token endpoints:

```text
issuer:        https://auth.x.ai
discovery:     https://auth.x.ai/.well-known/openid-configuration
client_id:     b1a00492-073a-47ea-816f-4c329264a828
scope:         openid profile email offline_access grok-cli:access api:access
redirect:      http://127.0.0.1:56121/callback
PKCE method:   S256
API base:      https://api.x.ai/v1
CLI API base:  https://cli-chat-proxy.grok.com/v1
```

Implementation should fetch discovery at login/refresh, require HTTPS, and
allow only `x.ai` or `*.x.ai` endpoint hosts before sending codes or refresh
tokens. This prevents compromised discovery data from exfiltrating secrets.
The authorize request also includes a random OIDC `nonce` in addition to PKCE
and `state`.

## Relationship to existing Synaps OAuth

### Shared with Anthropic and OpenAI Codex

Reuse these established primitives:

- `generate_code_verifier`, `generate_code_challenge`, and `generate_state`
- `start_callback_server` (already binds `127.0.0.1` and supports `/callback`)
- `open_browser`
- strict `state` validation for HTTP and pasted callback URLs
- `OAuthCredentials` and generic `save_provider_auth` / `load_provider_auth`
- atomic merge-based storage, preserving all other provider entries
- `CredentialSource::{Local, Remote}` and the token cache/broker contract
- an expiry skew before the provider's true expiry

### Provider-specific differences

Do **not** copy either provider 1:1:

- Anthropic has compile-time endpoints/scopes and a `code#state` manual form.
- Codex has compile-time endpoints, Codex-only authorize parameters, and must
  extract a ChatGPT account ID from its JWT.
- xAI uses OIDC discovery, requires a `nonce`, has no Codex account-ID step,
  and may rotate or omit a refresh token on refresh. Preserve the previous
  refresh token when a successful refresh response omits a replacement.
- The reference accepts a bare manual authorization code for WSL fallback.
  Synaps' established CSRF invariant rejects manual input without `state`.
  Keep Synaps' stricter invariant unless xAI provides a separately verifiable
  out-of-band flow; do not label a bare pasted code “trusted.”
- xAI's callback implementation handles OAuth `error` and
  `error_description`; improve the shared callback result/error path or add a
  provider-specific wrapper so cancellation does not appear merely as a
  closed channel.

## Implementation map

### 1. Auth provider

Add `crates/agent-core/src/core/auth/xai.rs` with:

- validated OIDC discovery response
- authorize URL construction (PKCE, state, nonce)
- code exchange and refresh
- token-response validation (`access_token` required; refresh required on
  login, previous refresh retained during refresh)
- checked/saturating expiry arithmetic with a two-to-five-minute skew
- browser + localhost callback + strict manual callback behavior
- persistence under `xai-auth`

Re-export `login_xai` from `auth/mod.rs`. Add `"xai-auth"` to
`ensure_fresh_provider_token`, using the caller's shared `reqwest::Client`
where practical.

`OAuthCredentials.account_id` remains `None`. Storing the discovered token
endpoint is unnecessary if refresh safely repeats discovery; if it is stored,
extend the credential type with an optional validated field without breaking
existing auth files.

### 2. Login UI

In `src/cmd/login.rs`:

- add OAuth `LoginProvider { key: "xai-auth", name: "xAI (Grok)", ... }`
- route it to `auth::login_xai()`
- retain `oauth_storage_key()` as-is (`xai-auth` maps directly)
- add provider-list, case-insensitive lookup, and storage-key tests

Update CLI help examples in `src/main.rs`.

### 3. Credential broker

Add `xai-auth` to `src/cmd/auth_broker.rs::ALLOWED_PROVIDERS` and test both
allowed token vending and rejection behavior. The broker continues to return
only short-lived access tokens; refresh tokens never leave the broker host.

### 4. Runtime routing (not just registry catalog)

A normal `ProviderSpec` currently requires an API key before routing, so adding
xAI only to `registry::providers()` would make OAuth login unusable. Introduce
an OAuth-aware route for model IDs such as:

```text
xai-auth/grok-4.5
xai-auth/grok-4.3
```

The route must resolve the access token through `resolve_access_token(
"xai-auth", ...)`, analogous to Codex credential resolution, then send it as a
Bearer token to the correct xAI endpoint.

Do not blindly reuse Codex's `Provider::Codex` path: that path targets the
ChatGPT backend, requires a ChatGPT account ID, and uses Codex-specific wire
semantics. xAI needs its own route or an OAuth-capable OpenAI route.

The reference primarily uses the OpenAI **Responses API** at
`https://api.x.ai/v1/responses`; Synaps' generic OpenAI path currently uses
`/chat/completions`. Confirm supported wire behavior with xAI and add a
Responses path if required before advertising full compatibility. CLI-only
models (`grok-build`, Composer) use `https://cli-chat-proxy.grok.com/v1` and
must not be placed on the public API route without explicit model-aware
routing.

Start with models verified end-to-end against the selected wire path; do not
copy the reference's entire catalog based only on README claims.

### 5. Optional Grok CLI credential import

Treat `~/.grok/auth.json` reuse as a separate, defensive follow-up:

- parse without logging token material
- validate schema and expiry
- support the official key
  `https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828`
- copy into Synaps' atomic storage rather than writing Grok's file
- preserve the normal fresh-login fallback on malformed/expired data

## Required tests

- discovery rejects HTTP and non-`x.ai` hosts
- authorize URL includes exact client ID, scopes, redirect, PKCE, state, nonce
- manual callback requires both code and matching state
- callback cancellation/error shuts down the server
- token responses require access/refresh appropriately
- refresh preserves old refresh token when omitted and persists atomically
- xAI login preserves Anthropic and Codex credentials (and vice versa)
- credential-source local/remote paths use canonical `xai-auth`
- broker allowlist includes xAI without exposing refresh credentials
- `xai-auth/model` routes without an API-key config entry
- xAI route uses the expected URL, Bearer header, wire shape, and model
- Anthropic and Codex routing/login regression suites remain green

## Security constraints

- Never commit client secrets or tokens. The public native-app client ID is not
  a secret.
- Never log authorization codes, access tokens, refresh tokens, or callback
  query strings. Existing shared callback mismatch logging currently prints
  state values; avoid extending that behavior and preferably redact it.
- Validate discovery endpoints before token POSTs.
- Preserve PKCE and constant-time-quality random state/nonce generation.
- Shut down the callback listener before network exchange/persistence errors
  can leak its task or occupied port.
