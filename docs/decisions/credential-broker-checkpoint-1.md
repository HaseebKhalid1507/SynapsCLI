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

xAI callback/login/runtime behavior, generalized callback outcomes, and runtime
route migration are Checkpoint 2 or later and are intentionally excluded.
