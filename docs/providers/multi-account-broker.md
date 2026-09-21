# Multiple OAuth accounts in one Synaps broker

One credential broker can hold independent OAuth logins for Claude, ChatGPT/Codex, Kimi Code and Grok. Each account has a provider-scoped label. Existing unlabelled credentials remain `default`; no migration or token copying is necessary.

Related: [usage snapshots](multi-account-usage.md), [quota keeper](quota-keeper.md), [goal plan](../plans/multi-account-broker-goals.md).

## First login pass

Run these commands **on the credential broker host**, using the newly built binary. The feature build is `target/release/synaps`; it does not replace your installed binary or restart your running broker.

```bash
SYNAPS="$PWD/target/release/synaps"  # from the repository root

"$SYNAPS" login --provider openai-codex --account astra1
"$SYNAPS" login --provider openai-codex --account astra2
"$SYNAPS" login --provider anthropic --account claude1
"$SYNAPS" login --provider anthropic --account claude2

# Optional other subscriptions
"$SYNAPS" login --provider kimi-code --account kimi1
"$SYNAPS" login --provider xai-auth --account grok1

"$SYNAPS" auth list
"$SYNAPS" status --all
"$SYNAPS" status --all --json
```

Complete each browser/device flow as the intended subscription owner. Use separate browser profiles or sign out of the previous account before the next login. Do not paste access/refresh tokens into chat. `auth login` is an alias for `login`.

A named login writes directly to its named slot: it never temporarily overwrites `default`. A label is 1–32 lowercase ASCII letters/digits/`.`/`_`/`-`, beginning with a letter/digit. `default` denotes the original bare provider key; `auto` is reserved for selection, not a login label.

Codex and Claude identity is checked to reject connecting one provider seat twice under different labels (see [Duplicate-seat detection](#duplicate-seat-detection)). Where a provider exposes no stable identity, duplicate detection is limited (reported by the login flow); keep one refresh owner per actual account. Do not copy rotating refresh tokens from another CLI or host into multiple slots. Use separate legitimate subscriptions, within provider terms and organization policy.

`status` uses a compact provider-grouped layout: each account shows its stored
email (or `unknown`), plan when available, and one line per main quota window
with percentage **used** and reset countdown. Unknown usage is never shown as
zero. Account errors and limit/spend-control warnings remain visible. Quiet
auxiliary counters and parser diagnostics are available with `status --all
--verbose`; `--json` retains the full report and includes optional per-account
`identity` metadata. Selection is unchanged: use `--all` for all accounts.

If an older login has no stored email, run `synaps auth identify --force` on the
credential host to backfill provider identity, then rerun status. Not all
providers expose an email; these remain `unknown`. Status only reads broker
metadata and usage; it does not perform a login or expose tokens. Emails are
personal data—redact them before sharing status output.

`auth list` shows **OAuth access-token expiry**, not the quota reset. An expired but refreshable token is not an expired subscription. `status` supplies quota windows/resets; billing renewal and OAuth expiry are separate concepts. Grok's billing adapter is fixture-tested but its live schema is explicitly unverified.

## Duplicate-seat detection

Two slots that hold the same provider account would spend one quota twice and rotate one refresh token from two owners, so the broker keys every slot on the provider's own account id (stored as `accountId`, shown as an 8-character `ID` prefix by `auth list`).

| Provider | Identity source | When |
|---|---|---|
| ChatGPT/Codex | `chatgpt_account_id` claim in the access-token JWT (no network) | at login and on every refresh |
| Claude (Anthropic) | `account.uuid` from `GET /api/oauth/profile` (read-only, bearer + `anthropic-beta`) — the quota-bearing entity; `organization.uuid` is informational | at login (one request) and via `auth identify` |
| Kimi Code, Grok, Copilot, Gemini | none exposed | duplicates cannot be detected; keep one refresh owner per real account |

- **Login-time guard.** `synaps login` resolves the identity *before* writing, then scans sibling slots of the same provider inside the same locked write. A seat already connected under another label is refused (`Not stored: this … seat is already connected as account '<label>'`). If the Anthropic profile lookup fails (network, 5xx), the login still succeeds and warns that a duplicate cannot be detected — nothing is ever guessed.
- **Backfill for pre-existing slots.** Logins made before this check (or whose lookup failed) carry no `accountId`; `auth list` prints `note: N slot(s) have unverified identity`. Run, on the broker host:

  ```bash
  "$SYNAPS" auth identify --dry-run          # resolve + report, write nothing
  "$SYNAPS" auth identify                    # record accountId/identity on each slot
  "$SYNAPS" auth identify --provider anthropic --account claude1 --force   # re-verify one slot
  "$SYNAPS" auth identify --json             # rows + duplicate groups for scripts
  ```

  `identify` mints a fresh access token through the normal single-flight refresh path, asks the provider once per slot, and records only `accountId`/`identity` (never touching token fields). Slots that already carry an id are reported as `already` unless `--force`. It exits non-zero iff duplicates remain, printing `⚠ DUPLICATE SEAT: anthropic, anthropic@claude1 share one anthropic account (…)` with the exact `auth remove` command to run. It refuses to run against a remote credential source (the refresh token lives on the broker host only).
- **Listing.** `auth list` marks every member of a duplicate group with `DUPLICATE-SEAT` and repeats the flagged keys in a `WARNING:` line (`duplicate_identity_keys` in `--json`).
- **Keeper.** With activation opted in, the quota keeper dedupes aliases by seat fingerprint and fails closed on a slot whose seat is ambiguous or duplicated; it never activates one seat twice through two labels.

## Choose a seat

Named logins do not silently change the default selection. Select a seat explicitly:

```bash
"$SYNAPS" auth use --provider openai-codex --account astra1
"$SYNAPS" auth use --provider anthropic --account claude1

# Optional automatic capacity-based choice
"$SYNAPS" auth use --provider openai-codex --account auto
"$SYNAPS" auth use --provider anthropic --account auto

# Inspect exactly one seat, independently of the selection policy
"$SYNAPS" status --provider openai-codex --account astra2
```

Selection precedence is explicit per-request account → `SYNAPS_ACCOUNT_<PROVIDER>` environment variable → `auth.account.<provider>` config → `default`. Hyphens become underscores, e.g. `SYNAPS_ACCOUNT_OPENAI_CODEX=astra2`. An unknown explicit slot or invalid selector fails closed, never falling back to another seat.

`auth use` writes only the active profile's config, never moves credentials. Restart existing clients to reliably pick up a changed policy. With a policy of `auto`, use `status --all` or an explicit `--account` to inspect usage.

Auto uses fresh provider evidence (a short cache, currently 60 seconds), excludes exhausted/unknown/malformed/stale readings and cooldowns, and checks model-specific limits. For a requested Codex model, a generic quota window does **not** prove model entitlement: absent availability evidence is rejected (including free seats with no Astra entitlement). Claude's paid `extra_usage` counter is not included subscription capacity and does not block a healthy subscription when disabled/null.

The default strategy is **soonest reset**, with these priority tiers:

| Tier | Eligible subscriptions | Order |
|---|---|---|
| 1 | Fixed reset within 24 hours | Earliest reset first |
| 2 | Likely unanchored/idle window | Stable account order |
| 3 | Fixed reset farther away | Earliest reset first |
| 4 | Headroom known, reset unknown | Lowest utilization |

Tier 2 is a **heuristic**, not verification: usage ≤0.5% and reset within five minutes of `observation + window duration`. A freshly started window can look similar. It gives real work a chance to start an idle clock; it does not prove every natural Codex rollover needs activation. The chooser sends no activation probes.

Selection is sticky by default: keep a healthy seat within the same tier to reduce churn, unless another seat reaches a higher-priority tier (or has an earlier urgent deadline). Hitting any applicable window limit, including Claude's five-hour limit, disqualifies it. Cooldowns follow the provider seat, not its alias; duplicate aliases cannot bypass them. A new login into a label does not inherit the previous seat's cooldown.

```ini
# Defaults; apply only to providers selected as auto:
auth.auto.strategy = soonest_reset
auth.auto.urgent_horizon_hours = 24
auth.auto.sticky = true
# Optional per-provider override:
# auth.auto.strategy.openai-codex = lowest_utilization
```

Strategies: `soonest_reset`, `lowest_utilization`, `preference_order`. Horizon accepts 1–168 hours. Invalid values produce a config warning and use that key's documented default. Explicit account selection still takes precedence and never silently falls back.

Preview before selecting auto:

```bash
"$SYNAPS" auth plan --provider anthropic --model claude-fable-5-1
"$SYNAPS" auth plan --provider openai-codex --model gpt-6-astra
"$SYNAPS" auth plan --json
```

The preview shows rank, tier, reset, utilization, and rejection reason, and prints `would select: <provider>@<account>`. A `*` reset is likely unanchored. It calls the same ranking code as the broker, but a **new CLI process has no running session's stickiness/cooldown history**. It does not change selection, spend inference, or activate quotas; expired OAuth tokens may refresh normally. Without `--model`, it reports generic capacity, not a guarantee a seat can serve your chosen model. For remote credentials, run this command on the broker host.

It does not assume every 429 is quota exhaustion. Codex and Anthropic permit **at most one immediate account switch per request**, under Auto only, on recognized pre-output window exhaustion. No cross-account replay after partial streamed output/tool activity; generic throttles retain ordinary bounded backoff. Claude recognizes rejected unified/per-window rate-limit status headers, or a typed rate-limit error with a long reset hint; the live exhaustion header schema remains to be verified with an actual limited request. A recognized limit is reported to the broker even when replay is prohibited, so the next turn can choose another seat. If the next seat also fails or none is eligible, the request stops rather than touring accounts indefinitely.

A switch appears as `⚠ <account> exhausted until <time> — switching to <next>`. Local runtime broker adapters share cooldown and sticky state through the runtime's token-cache handle; rebuilding an adapter no longer forgets a limit. Restarting the runtime clears that in-memory state; fresh usage still gates capacity. No cross-provider model substitution or purchase of extra credits is performed.

## Keep Codex windows moving

Start read-only:

```bash
"$SYNAPS" quota-keeper --once --json
# Continuous polling, no inference:
"$SYNAPS" quota-keeper
```

Only after checking real usage/reset behavior, opt specific seats into activation:

```bash
# Example only: choose a model confirmed to use the quota bucket you want to anchor.
"$SYNAPS" quota-keeper \
  --activate openai-codex@astra1 \
  --activate openai-codex@astra2 \
  --model gpt-6-astra
```

This consumes some quota. A cheaper model must **not** be assumed to start an Astra-specific window. The keeper requires fresh headroom and known reset evidence; an account with no known reset, or one still reported exhausted after its reset, is not activated blindly. One ambiguous attempt consumes the generation's attempt allowance. Verification requires a later provider-reported weekly reset; `now + 7 days` is never invented.

First-use anchoring is documented for some reset paths, **not established as a universal rule for all natural rollovers**. Real behavior for your subscriptions is pending login/observation. The request has no project context, tools or agent loop. It has minimal reasoning/verbosity and a timeout but **no guaranteed output-token cap** on this endpoint. See the keeper runbook before enabling it.

Activation is local-only, on the broker host, with a canonical private ledger beside the resolved credential file. Remote clients may poll, not activate. A disabled read-only systemd user-unit example is at `deploy/synaps-quota-keeper.service`; nothing is installed/enabled automatically.

## Remote clients and storage

The existing machine-authenticated broker exposes account-aware token, usage, capability and proxy operations. Upgrade the broker host as well as clients before relying on named accounts/auto. An old broker that cannot confirm the requested named slot is rejected rather than silently using its default. Machine-token holders can see account labels/minimal identity metadata; keep that token private. Use TLS or a trusted private tunnel, not public plaintext HTTP.

Long-lived credentials remain in the broker host's private `auth.json`; only short-lived access tokens or sanitized usage leave the boundary. Additive keys are `openai-codex@astra1`, `anthropic@claude1`, etc. Refresh is serialized by resolved file plus slot, across processes, and persisted atomically without overwriting other slots. Identity-scoped caches cannot apply the old seat's capacity after a different account is logged into its label.

Login/removal operate on **local files** even when a remote source is configured. Perform those operations on the broker host. `auth list` and status can query a remote broker; `auth use` selects policy for the invoking client.

```bash
# Deliberate local removal (does not revoke the subscription/provider account)
"$SYNAPS" auth remove --provider openai-codex --account astra2 --yes
# Return to the legacy slot if it exists
"$SYNAPS" auth use --provider openai-codex --account default
```

Removing a selected slot leaves its selector failing closed until you choose another account. Keep at least one valid route before removal. Do not hand-edit or delete the keeper ledger to obtain another automatic attempt; use the explicitly documented recovery/rearm procedure.

## Known operational limits

- Cooldowns and short-lived capacity caches are broker-instance/runtime-local, not a shared distributed database. Fleet clients should use one broker authority rather than independent credential copies.
- A caller/process cancellation during an upstream rotating-token refresh can still require re-login if the provider rotated before persistence. This is inherited behavior, not a new guarantee of cancellation-safe refresh. Cross-process locks prevent simultaneous owners, not recovery of a lost provider response.
- A profile inheriting the base `auth.json` may read/refresh it but cannot add/remove slots by silently creating a partial profile copy. Perform account management in the owning/base profile; do not fork refresh tokens.
- Banked-reset count/expiry is alert-only and only as complete as provider-exposed data. The optional separate inventory fetch exists in the usage adapter; normal status/keeper polling does not force it. No redemption/purchase is performed.

## Validation boundary

Automated tests use synthetic credentials, fake clocks and loopback HTTP. No real login, refresh or activation is required to run them. Production logins, each provider's current usage schema, actual reset anchoring, and model/bucket correlation must be verified with the operator's accounts. The build is ready for that controlled login pass, not evidence that live activation has already succeeded.
