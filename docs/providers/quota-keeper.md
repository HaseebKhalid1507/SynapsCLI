# Quota keeper (`synaps quota-keeper`)

Watches the weekly quota window of every connected Claude / Codex / Kimi /
Grok account, records the provider-reported reset generation, measures the
idle delay between a reset and the next anchored window, and — **only for
Codex accounts you explicitly opt in** — sends one minimal, tool-free request
per verified reset so the next window starts as early as possible.

It is a *keeper*, not an agent: no project context, no tools, no MCP, no
shell, no agent loop, no purchases, no redemption of banked resets, no login.

## Why

First-use anchoring is documented for some Codex reset paths, but it is **not
a universal documented guarantee for every natural weekly rollover** (see
`docs/plans/multi-account-broker-goals.md`). On accounts that require first use
to anchor a new window, sitting idle can delay the next reset. The keeper
makes that possible delay visible and, on explicit request and fresh capacity
evidence, attempts to reduce it. Real behavior must be verified on your seats.

The keeper never assumes a "naturally resetting" timer: a window is only
called **active** when a *fresh* usage read shows a strictly later reset than
the generation being tracked. With no evidence the state is `unknown` or
`due`, never `active`.

## Safety model (read this before `--activate`)

| Property | Guarantee |
| --- | --- |
| Default mode | Read-only. Polls, classifies, alerts. No inference. |
| Opt-in | Per account, per invocation: `--activate openai-codex@<label>` (or `openai-codex` for the default slot). Only Codex. Requires an explicit `--model`. |
| Model | No default, no guessed bucket. Must be a catalog Codex id (`openai-codex/` prefix accepted). The request carries the **lowest reasoning effort the model supports**, authorized through the same execution-plan builder the runtime uses. |
| Request | One `POST https://chatgpt.com/backend-api/codex/responses` (pinned) with the bearer vended for exactly the tracked `CredentialRef` and the `chatgpt-account-id` derived from that same token. The full token-derived seat fingerprint must match the tracked seat immediately before sending; a re-login mismatch disables activation until restart. Body mirrors the production Codex builder: `store:false`, `stream:true`, one-word instructions and input, `text.verbosity: low`, **no `tools`**. Like production it omits `max_output_tokens` (the ChatGPT backend may reject it) — so there is **no guaranteed output-token ceiling**; cost is bounded by minimal effort, a one-word prompt, low verbosity, no tools and a hard request timeout (60 s). |
| Proven capacity gate | Activation is authorized only when the **latest** poll succeeded, is fresh (`--stale-after`), carries no overall `limit_reached`/spend-control assertion, and every window applicable to the activation model (5 h, weekly and model-scoped) shows headroom with the model not listed exhausted/unknown. A passed reset while the provider still asserts 100 % is `exhausted (reset passed)` — never due. |
| One attempt per generation | Exactly one. The attempt is persisted (`activation_pending`) **before** the request leaves the process. Timeouts, 5xx, stream cuts, drain-cap overruns and crashes between begin and finish are *ambiguous* and consume the attempt. Only typed pre-flight failures (token vend, missing account id, request build, TCP connect) and whitelisted pre-inference 4xx (400/401/403/404/413/415/422) are refunded, with backoff and a bound of 5 per generation. 429 is never refunded. |
| Verification | After the attempt the keeper re-reads usage. `active` requires a strictly later weekly reset. Otherwise the attempt stays pending for 10 minutes, then becomes `unverified` and is **never retried automatically**. |
| Re-arm | `--rearm openai-codex@<label>` (with the same account in `--activate`) allows exactly one more attempt for a generation that is `unverified`. Operator action, once per invocation. |
| Attribution | A verified new window is labelled `observed` or `keeper attempt N correlated, not proven causal`. Correlation is recorded; causation is never claimed. |
| Locks | State-dir singleton lock plus one lock per account in the canonical keeper directory (next to the resolved `auth.json`, shared by profiles that inherit it). A second keeper on the same host for the same account is refused regardless of `--state-dir`. Strong seat identities additionally have an alias-independent seat lock; duplicate aliases are tracked once read-only and rejected for activation when selected together. |
| Remote broker | With `auth.remote_endpoint` set the keeper is **poll-only**; `--activate` is refused because locks are host-local and two hosts could each spend an attempt. Run the activating keeper on the broker host. |
| Ledger retention | Reconciliation never deletes an attempt ledger. An account filtered out with `--account`/`--provider` keeps its spent generation for when it returns; a re-login with a different provider seat gets a new identity key and never inherits a ticket. Changing only the label for the same strong seat preserves its attempt ledger. |
| Secrets | State (`state.json`, 0600 in a 0700 dir, atomic writes) holds phases, generations, timestamps, HTTP status codes and error classes only. No tokens, no upstream bodies, no JWT fragments. `--show-state` is safe to paste. |
| Banked resets | Codex `rate_limit_reset_credits` inventory is reported/alerted (count, earliest expiry, "expiring soon" within 3 days). Nothing is ever redeemed. |

## Commands

```bash
# Read-only, one pass, human output
synaps quota-keeper --once

# Read-only daemon (5-minute base poll; polls right after a reported reset + 30 s)
synaps quota-keeper

# Machine-readable (one JSON object per event and a `pass` summary)
synaps quota-keeper --once --json

# Only some accounts / providers
synaps quota-keeper --once --provider openai-codex --account openai-codex@astra2

# Opt ONE Codex account into activation (explicit model required)
# Confirm this model anchors the intended bucket; mini may not anchor Astra.
synaps quota-keeper --activate openai-codex@astra2 --model gpt-5.4-mini

# One extra attempt for a generation that stayed unverified (operator decision)
synaps quota-keeper --once --activate openai-codex@astra2 --model gpt-5.4-mini --rearm openai-codex@astra2

# Inspect persisted state (secret-free)
synaps quota-keeper --show-state
```

Options: `--poll-interval <secs>` (default 300, clamped 60..3600),
`--max-backoff <secs>` (default 14400), `--stale-after <secs>` (default 1800),
`--state-dir <path>` (read-only runs only; refused with `--activate`).
SIGTERM / Ctrl-C ends the loop gracefully after persisting state.

## Phases

| Phase | Meaning | Activation? |
| --- | --- | --- |
| `unknown (…)` | No usable weekly-window evidence (never polled, missing reset time, malformed, unsupported, contradictory). | no |
| `auth error (…)` | Provider rejected the credential; re-login required. Polled at max backoff. | no |
| `exhausted until T` | Live window, no headroom (or overall limit asserted). | no |
| `exhausted; reset T passed but provider still asserts limit` | The reported reset passed and fresh usage still says 100 % / `limit_reached`. Alert. | **no** |
| `active (N%, resets T; observed)` | Live window with headroom, seen without a keeper attempt. | no |
| `active (…; keeper attempt N correlated, not proven causal)` | New window verified after a keeper attempt. | no |
| `due: reset T passed, no new window anchored` | Reset passed; fresh usage shows headroom but no newer reset. Alert `window idle since reset`. | only if opted in |
| `activation pending verification` | Attempt persisted/sent; waiting for fresh usage evidence. | no |
| `unverified: N attempt(s) … not retrying` | Verification window elapsed without a new reset. Alert. | only via `--rearm` |

Each row also shows `last idle delay` (new anchor − previous reset; exact when
the provider reports the window duration, otherwise an upper bound from the
observation time), banked resets, the last error class and the next action time.

## Weekly window detection

The weekly window is identified by its provider-reported **duration**
(6–8 days), never by its name: real Codex accounts can have a weekly
`primary` and no `secondary`. A window with no duration is never treated as
weekly. Model-scoped windows (Anthropic `seven_day_sonnet`, Codex per-model
rows) and the 5 h window all gate activation for the chosen model; feature
windows (code review) do not.

## Running as a service

`deploy/synaps-quota-keeper.service` is a **disabled** systemd *user* unit
example (read-only as shipped). Copy it to `~/.config/systemd/user/`, start it,
watch `journalctl --user -u synaps-quota-keeper -f` in read-only mode first,
and only then add `--activate … --model …` through a drop-in. Enabling at login
(`systemctl --user enable`) is your explicit step. Run one keeper per
`auth.json`.

## Recovery

* **Corrupt `state.json`** — the keeper refuses to run (fail closed). Move the
  file aside to reset; you lose idle-delay history and the attempt ledger, so
  the current generation may be attempted once more.
* **Killed mid-attempt** — on restart the pending attempt is recorded as
  ambiguous and counted. Nothing is re-sent for that generation.
* **Provider keeps asserting the limit after the reset** — nothing to do
  automatically; the keeper alerts. Check the account in the provider UI.

## Live validation status

Everything above is covered by deterministic tests (synthetic clock, mock
broker, fake usage snapshots, loopback HTTP for the activation request). Live
behaviour against real Codex seats — in particular what `wham/usage` reports
between a reset and the first request — is **pending the operator's real
multi-account login**; nothing here is marked verified against production.
