# Multi-account usage snapshots (`synaps status`)

Read-only quota/usage reporting for every connected OAuth account of the four
providers the multi-account broker manages: Claude (Anthropic), ChatGPT/Codex,
Kimi Code and Grok (xAI). Implements goal G3 (normalized snapshots) and the
status half of G4 in `docs/plans/multi-account-broker-goals.md`.

Code: `crates/agent-core/src/core/auth/usage.rs` (module `auth::usage`),
`src/cmd/status.rs`.

## Command

```
synaps status                                   # policy-selected Claude account (legacy layout)
synaps status --provider codex                  # policy-selected Codex account
synaps status --provider codex --account astra2 # exactly that slot; never falls back
synaps status --all                             # every stored account of every usage-capable provider
synaps status --all --provider claude           # every stored Claude account
synaps status --all --json                      # machine-readable UsageReport (schema below)
synaps status --memory [--json] [--pid N]       # unchanged process-memory report
```

Provider names: canonical ids (`anthropic`, `openai-codex`, `kimi-code`,
`xai-auth`) plus the aliases `synaps login` accepts (`claude`, `kimi-code`,
`kimicode`, `kimi-cli`) and the usage-only aliases `codex`, `chatgpt`, `grok`,
`grok-build`. `kimi` and `google` remain static API-key provider names and are
rejected here exactly as in `login`. Copilot and Gemini have no readable quota
endpoint and are rejected with "no usage adapter".

Rules:

- `--account` requires `--provider`; `--all` cannot be combined with `--account`.
- An explicit `--account` is validated against the label grammar before it
  reaches any URL or JSON, and an unknown slot is an error row — never a fallback.
- With no `--account`, the account comes from the active policy
  (`SYNAPS_ACCOUNT_<PROVIDER>` > `auth.account.<provider>` > `default`). A
  provider set to `auto` requires `--account` or `--all`.
- Per-account failures are reported inline and never discard successful
  accounts. Exit status is non-zero only when every requested account failed
  or the selection itself was invalid. Zero connected accounts prints a hint
  and exits 0.
- The status process never receives an access token. Snapshots come from the
  credential broker's typed `usage(&CredentialRef)` operation; the broker
  (local in-process, or the remote `synaps auth-broker`) resolves the
  credential behind its boundary and returns the secret-free snapshot.

## Snapshot schema (`schema_version: 1`)

```jsonc
{
  "schema_version": 1,
  "observed_at": 1758400000000,      // epoch ms the report was assembled
  "source": "local",                 // or "remote"
  "accounts": [
    { "provider": "openai-codex", "account": "default", "status": "ok",
      "snapshot": {
        "schema_version": 1,
        "provider": "openai-codex", "account": "default",
        "observed_at": 1758400000000,          // epoch ms the body was received — authoritative for staleness
        "plan": "pro",
        "limit_reached": false,                // provider flag; null = not reported
        "spend_control_reached": false,        // Codex spend_control.reached; true blocks capacity
        "windows": [
          { "id": "primary", "label": "7d", "scope": {"kind": "account"},
            "duration_secs": 604800,           // from limit_window_seconds — primary is NOT assumed to be 5h
            "used_percent": {"state": "valid", "percent": 37.0},
            "used": null, "limit": null,
            "reset_at": 1758500000000,         // provider-authoritative, epoch ms; null when not reported
            "reset_kind": "quota_window", "limit_reached": false }
        ],
        "model_availability": [
          { "model": "gpt-6-astra", "availability": "exhausted",      // available | exhausted | unknown
            "used_percent": {"state": "unknown", "reason": "missing"},
            "reset_at": 1767603600000,         // explicit available_at
            "credits_would_enable": true,      // billing hint only; never makes the model available
            "source": "model_usage" }          // model_usage | window | merged
        ],
        "credits": { "kind": "codex_credits", "has_credits": true, "unlimited": false,
                     "remaining": 12.5, "total": null, "unit": "credits", "renews_at": null },
        "banked_resets": { "available_count": 2, "credits": [], "inventory_error": null },
        "identity_prefix": "acct_123",
        "notes": ["null window skipped"]     // fixed vocabulary only; never raw keys or values
      } },
    { "provider": "openai-codex", "account": "astra2", "status": "error",
      "error": { "kind": "unauthorized", "message": "provider rejected the access token (HTTP 401)", "http_status": 401 } },
    { "provider": "xai-auth", "account": "*", "status": "error",       // "*" = the provider's account listing failed
      "error": { "kind": "account_listing", "message": "…" } }
  ]
}
```

`used_percent` is never a bare number. `Valid` requires a finite value in
`0..=100`; everything else is `{"state":"unknown","reason":…}` with reasons
`missing`, `null`, `not a number`, `out of range`, `limit is zero`. Unknown is
never treated as available capacity.

### Capacity helpers (for routing / keeper consumers)

| Helper | Meaning |
| --- | --- |
| `is_stale(now, max_age)` | observation older than `max_age` (or from the future) |
| `has_proven_capacity(now, max_age)` | fresh, `limit_reached != true`, `spend_control_reached != true`, ≥1 account window, every account window `Valid < 100` |
| `has_proven_capacity_for_model(model, now, max_age)` | the above **and**, if the provider mentioned the model, its entry is `available`. Astra-style gating rejects even at 10 % generic usage. Unmentioned models fall back to account capacity. |
| `model_availability_for(model)` | case-insensitive exact or delimiter-token match (`sonnet` matches `claude-sonnet-4-5`) |
| `earliest_reset_at()` | min over window resets and model `available_at` |

Reset kinds are distinguished: `quota_window` (rolling/anchored quota) vs
`billing_renewal` (credit period). OAuth token expiry is not a usage concept and
never appears here.

## Adapters (all `GET`, pinned URL, bearer from the broker)

| Provider | Endpoint | Extra headers | Schema provenance |
| --- | --- | --- | --- |
| Anthropic | `https://api.anthropic.com/api/oauth/usage` | `anthropic-beta: oauth-2025-04-20` | shipped `synaps status` reader; every object with `utilization`/`resets_at` is a window, `seven_day_<model>` → model scope |
| Codex | `https://chatgpt.com/backend-api/wham/usage` (+ optional `…/wham/rate-limit-reset-credits`) | `chatgpt-account-id` derived from **the same** token's JWT claim | rate-limit fields verified against the official `codex` binary string table; `model_usage` from a prior live response |
| Kimi Code | `https://api.kimi.com/coding/v1/usages` | none (the official CLI's usage fetch sends bearer + Accept only; no device-identity headers, no device-id state touched) | official `kimi` CLI bundle `managed-usage.ts` |
| Grok | `https://cli-chat-proxy.grok.com/v1/billing?format=credits` | `x-xai-token-auth: xai-grok-cli` | **unverified live**; QuotaKit-documented shape |

Codex specifics:

- `primary_window` / `secondary_window` durations come from
  `limit_window_seconds`; the primary can be weekly and the secondary can be
  `null` (skipped with a note). `reset_at` (unix seconds) is authoritative; if
  absent, `observed_at + reset_after_seconds` is used and noted.
- The `chatgpt-account-id` header is derived from the bearer token itself. A
  token without the claim fails with `account_id_unavailable` before any
  request. A response whose `account_id` differs from the header fails with
  `identity_mismatch` and the snapshot is discarded (pairing is never guessed).
- `model_usage` (`{"<slug>": {"available", "available_at", "credits_would_enable"}}`)
  is parsed first as the authoritative per-model statement; model-scoped
  windows are then merged in without overwriting it (exhaustion from either
  source wins, explicit `available_at` kept).
- `spend_control.reached` (top-level or any nested limit) sets
  `spend_control_reached`; nested limits with `remaining_percent`/`resets_at`
  become a `spend_control` feature window.
- Banked resets: `rate_limit_reset_credits.available_count` from the usage
  body; with `include_inventory` the read-only inventory endpoint fills
  `credits[]`. Inventory failures are recorded in `inventory_error` — partial,
  never fatal. The sibling `/consume` endpoint is deliberately unreachable from
  this module; nothing here redeems, purchases or activates anything.

Grok specifics (unverified until a live login is available):

- Percent from `config.creditUsagePercent`, else
  `onDemandUsed.val / onDemandCap.val × 100`; reset from
  `config.currentPeriod.end` or `config.billingPeriodEnd`; duration from
  `billingPeriodMinutes`. Each field is also accepted at the root.
- Without a recognizable fraction the single `credits` window is explicitly
  `unknown`; an entirely unrecognized body yields no windows and the note
  `unrecognized billing schema`. The snapshot always carries the note
  `grok billing schema unverified against a live account`.

## Transport guarantees

- `UsageClient::new()` builds the HTTP client with `redirect::Policy::none()`
  (a per-request builder cannot override a client's redirect policy), 10 s
  connect / 15 s total timeouts, built-in roots. 3xx is `redirected` and never
  followed.
- Non-2xx bodies are dropped unread (`unauthorized` 401/403, `rate_limited`
  429 — not proof of quota exhaustion — `upstream_status` otherwise).
- Success bodies are streamed into a 256 KiB cap (`body_too_large`).
- `UsageError` `Display`/JSON never contain a token, a URL query or upstream
  body bytes; transport errors carry only a coarse class (`connect`,
  `request`, `body`).
- `endpoint_override` / `inventory_endpoint_override` are loopback-only test
  seams (`127.0.0.1`, `localhost`, `::1`); any other host is refused with
  `invalid_endpoint_override` before a request is built.

## Verification

- `cargo test -p synaps-core --lib auth::usage` — 34 tests: parser fixtures
  (valid / missing / null / malformed / percent out of range / reset as RFC 3339,
  unix seconds, unix ms and garbage), Codex weekly-primary + null-secondary,
  Astra gating fixture (generic 10 % but `gpt-6-astra` unavailable →
  `has_proven_capacity_for_model` false), spend-control blocking, merge
  semantics, inventory partial failure, header pairing, identity mismatch,
  status classification with hostile bodies, redirect refusal, body cap,
  timeout, loopback-only override.
- `cargo test --bin synaps cmd::status` — 9 tests over a fake backend:
  alias parsing, selection rules, `--all` enumeration with per-provider
  listing isolation, partial-error report round-trip, text rendering, exit
  semantics. No network, no `auth.json`.

Live validation of every adapter against real accounts is pending the user's
multi-account logins (G7); the Grok schema in particular is documented as
unverified above.
