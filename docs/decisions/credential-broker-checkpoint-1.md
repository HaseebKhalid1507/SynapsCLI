# Checkpoint 1 credential architecture decision

Status: **accepted and frozen for Checkpoint 1**

## Boundary

The credential broker is the mandatory credential owner/resolver for local and
remote runtimes. Runtime-facing credential requests identify a typed provider
and contain no auth-file path, provider-key config map, or environment lookup.
Storage/config/env discovery belongs behind the broker boundary.

## Vending policy

* OAuth access-token vending is permitted. Responses contain only the access
  token and expiry. Refresh tokens never cross the broker boundary.
* Remote raw static API-key vending is forbidden.
* Static-key providers require broker-side request proxying or signing so the
  key remains broker-owned.
* A same-host raw-key compatibility mode may exist only when explicitly marked,
  authenticated, audited, denied to every remote peer by default, and carrying
  a tracked removal plan. It is secret disclosure, not a safe steady state.
  No provider opts into this mode at Checkpoint 1.

`BrokerCredentialStrategy` makes this policy explicit on every descriptor.
Registry construction is the validation gate; descriptors without a strategy
cannot be represented, duplicate IDs/behaviors and missing cross-references are
rejected.

## Identity and compatibility

Canonical OAuth IDs are typed. Checkpoint 1 registers `anthropic` and
`openai-codex`. `claude` is accepted only by CLI parsing and immediately becomes
`OAuthProviderId::Anthropic`; it is never a storage or internal dispatch ID.
Existing public login/storage functions remain as compatibility adapters while
new login and refresh dispatch use the registry.

Auth storage remains an open JSON object and merge-updates one provider entry,
which preserves unknown provider entries. Codex account ID extraction remains
inside the Codex provider implementation and does not enter common credential
metadata.

## Deferred

xAI callback/login/runtime behavior and generalized callback outcomes are
Checkpoint 2 or later and are intentionally excluded.

## Checkpoint 1 completion (third pass): the broker boundary is live

The mandatory broker path is now implemented, not just declared:

* **Typed protocol** (`agent_core::auth::broker`): `CredentialBroker` exposes
  `access_token` (OAuth, token+expiry only — `AccessToken` has no refresh
  field), `proxy` / `proxy_stream` (typed, credential-free `ProxyRequest`
  executed broker-side for static-key providers, covering streaming chat,
  non-streaming ping, and `/models` catalog), and `capabilities`
  (configured-ness booleans, never key material).
* **In-process local broker**: `LocalBroker` is the process-wide default
  (`global_broker()`), so normal local use requires no separately launched
  daemon. It is the only module that reads `auth.json`, `provider.<key>`
  config, or credential environment variables.
* **Authenticated remote transport**: `synaps auth-broker` adds machine-auth
  `POST /proxy` (SSE-capable) and `GET /capabilities` beside `GET /token`;
  `/token` refuses static-key providers, `/proxy` structurally refuses OAuth
  providers and absolute/escaping paths. Loopback/TLS bind policy unchanged.
* **Broker-owned static keys**: persisted as `{"type":"api_key","key":…}`
  entries in the broker credential store with the same atomic merge writer;
  legacy login-config/env keys are discovered and migrated behind the
  boundary. Proxy destinations are pinned from the broker's own
  `static_providers` table; raw keys are never vended, locally or remotely.
* **Runtime/TUI migration**: `ProviderConfig` has no key field; routing,
  streaming, ping, catalog, settings/status UI, and model pickers consume
  broker routing data and non-secret capability queries only. Local endpoint
  URL (`provider.local.url` / `LOCAL_ENDPOINT`) remains non-secret
  configuration.
* **Anthropic unification**: Anthropic refresh flows through the same
  per-provider single-flight gate + atomic merge persistence as Codex
  (`ensure_fresh_provider_token`); `ensure_fresh_token` is a compatibility
  wrapper. Runtime auth state never holds a refresh token in either source
  mode.
