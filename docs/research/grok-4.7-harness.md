# Grok 4.7 — local harness addition (2026-09-21)

## First-party research via web-tools

Direct live fetches (search excerpts were stale and still described 4.6 as latest):

- https://docs.x.ai/developers/models — exact ID `grok-4.7`, 500k-token context,
  advertised base input/output prices $2/$6 per million tokens, May 2026
  knowledge cutoff. Page says last updated September 21, 2026.
- https://docs.x.ai/developers/model-capabilities/text/reasoning — supports
  `low`, `medium`, `high`, `xhigh`; default `high`; reasoning cannot be disabled.
  Responses wire uses `reasoning: {effort: ...}`.
- Both pages document that Grok 4.7 always returns `reasoning.encrypted_content`
  on Responses. The reasoning page explicitly says clients that ignore the field
  continue working through server-side rehydration; response storage remains
  controlled by `store`. No new ciphertext persistence or storage-policy change
  is required for this catalog addition. Synaps does not claim client-side
  encrypted-reasoning replay support here.

## Implementation and scope

- Selectable qualified ID: `xai-auth/grok-4.7` (existing xAI OAuth broker,
  `https://api.x.ai/v1/responses`, existing tool and streaming adapter).
- Catalog records the documented 500,000 context capacity. Runtime's existing
  conservative 200,000 default/context override policy is intentionally unchanged.
- Shared exact-ID capability drives validation, thinking picker, default effort,
  and request-body construction. `off`, `max`, `ultra`, `ultracode` reject.
- No guessed `grok-4.7-latest` alias, maximum output, vision capability, or
  subscription entitlement. Existing text-only route remains text-only.
- Pricing defaults unchanged: base public API pricing is not a verified OAuth
  subscription cost schedule or a complete long-context price table.
- No active model/account/profile/worker authorization changes. No live inference
  or paid probes. Actual OAuth access depends on the provider account/rollout.
- Older models are retained unchanged. Current live docs also mention `xhigh`
  for 4.6; updating its previously pinned capability is outside this narrow addition.

## Use

In a newly built Synaps session: `/model xai-auth/grok-4.7`, then `/thinking high`
(or `low`, `medium`, `xhigh`, `adaptive` for the provider default).

Offline regressions cover catalog visibility, OAuth/Responses routing, reasoning
picker/validation, request shape and unsupported-mode rejection. Concurrent
broker, chooser, status and Jev work is not reset, stashed, reformatted or staged.
