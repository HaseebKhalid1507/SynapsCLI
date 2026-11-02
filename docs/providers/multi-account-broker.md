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

Auto uses fresh provider evidence (for an Anthropic account a selection is pinned to, its reading is reused for up to 10 minutes while it shows room or shows the account's paid extra usage off, and, with extra usage on or not reported, for 60 seconds once a window of the request is at 90% or more; for another provider's pinned account, and for any account a selection is about to switch to, a 60-second cache; any other account is read again after 10-12 minutes, with the clocks spread over the accounts so the reads don't come in one burst, or sooner once a reset it reported has passed), excludes exhausted/unknown/malformed/stale readings and cooldowns, and checks model-specific limits. A selection reads each account at most once, and judges a reading's age when it decides: the account it is about to choose is read again if its reading, taken before the selection began, is over 60 seconds old by then (another account's read may have been slow); the Anthropic account the selection is pinned to keeps its own bound instead (10 minutes while it shows room or its extra usage off, 60 seconds near its limit with extra usage on or not reported). It keeps to the accounts listed when it began; an account added meanwhile counts from the next selection. The spread separates the accounts' due times, not the reads within one selection: a first selection, or one after every account's clock has run out, still reads every account, one after the other. Before a selection reports no capacity, it reads again, once, each account it rejected on a reading over 60 seconds old that names no reset still ahead for a window it is used up on (no reset, or one that had already passed when it was read); never an account it may not use, a cooling one, or one whose failed read is held. An account used up until a reset still ahead is not read again then: it can't have room before that reset, and it is read again once the reset has passed. For a requested Codex model, a generic quota window does **not** prove model entitlement: absent availability evidence is rejected (including free seats with no Astra entitlement). Claude's paid `extra_usage` counter is not included subscription capacity and does not block a healthy subscription when disabled/null.

Anthropic usage reads are throttled further. A usage-endpoint 429 on one account pauses every Anthropic usage read of the process until the 429's hold ends (2 minutes at first, longer on repeats, up to 10; a longer `Retry-After` wins), including a read that was waiting for its token refresh; meanwhile selection uses the last readings. The daemon saves each account's last good reading and any active pause to `usage-last-good.json` beside `auth.json` (mode 0600; numbers, times, slot keys and an account fingerprint only: no token, account id or credential digest), so a restart or reload keeps them: a saved reading counts as a cached one at its original age, and the first turns after a reload send no usage read for one still inside its account's age. Only one process writes the file: the daemon that holds its lease (an exclusive lock on `usage-last-good.lock` beside it, taken at the daemon's start and freed at its shutdown or exec; a child forked without exec shares it). It writes what it holds: a reading it drops (a reported limit, a refused login) is gone from the file at once. A good reading that lands after the account's reading was dropped while it was being read is discarded, not used, saved or cached, and a selection that already holds a copy of the dropped reading (one it read, reused or offered to grace) stops using it: that account counts as failed for the rest of the selection: a re-read still queued for it when the drop is seen is dropped unsent, and one already going out can complete but can't make the account eligible in that selection. Other processes (another profile's daemon on the same `auth.json`, `synaps status`, a standalone TUI) only load it. A reading is saved only for a login with a provider account id, and is used only while the slot holds that account. Limits: a CLI's own 429 does not pause a running daemon; pauses and drops reach the file only from the lease owner; a daemon that starts while another profile's daemon holds the lease only loads the file until its next start, and another profile's daemon that starts inside a reload's exec gap can take the lease; after a rollback to a build without the file and an upgrade within 30 minutes, a reading the older build dropped can be reused once (a message-path 429 still cools the account); removing an account does not drop its reading: the entry is pruned at the writer's next selection, and until then the reading can be written back (by a direct usage read at any time, or by a first read in flight across a prune, whose result can then stay in the file until a later selection), and a re-add of the same account before then can be served it within the normal age limits (it stays bound to that account, and never undoes a refusal, a reported limit or a pause). Other providers keep a 60-second pinned cache, no pause and no saved readings.

When an account's usage read fails transiently (a usage 429, a 5xx, a timeout, a transport failure), automatic selection judges it on its last good reading of the account in the slot, for up to 30 minutes. For an Anthropic account whose last reading shows its paid extra usage switched off (`extra_usage.is_enabled: false`), and whose read failed with a usage 429 (or was held back by the pause), that reading counts at any age: a window that fills up then answers the message request with a limit, which cools the account and moves the request on, never with a bill. Extra usage on, or not reported, and every other failure keep the 30 minutes; an account with no reading stays unselectable. On such a reading, a window that was used up and whose reset has passed since has started again (room of unknown use: choosing anew, the account ranks after every account with a known use, whatever the strategy's order; the sticky rule still judges an account the selection is pinned to as it would any graced reading, so this alone never moves off it); a window used up until a reset still ahead keeps the account out until that reset, and when every account is out that way the selection fails as `all_cooling_down` with the earliest reset, not `usage_read_failed`; a window used up with no reset, or with one that had already passed when it was read, keeps the account out as a failed read. These rules act on the reading's windows: a used-up model-scoped window of the requested model (say a spent Sonnet week for a Sonnet request) first fails the reading's model availability, which keeps the account out as a failed read, neither restarted nor cooling down. Limit: an account whose extra usage was switched on after its last good reading, while every usage read keeps failing with a 429, can be billed for extra usage once a window fills, until a read succeeds.

`synaps status` with the local broker uses the saved readings too, and never writes the file. An Anthropic account whose saved reading is at most as old as the age the broker itself would reuse it for (10 minutes; 60 seconds once a window is at 90% or more, unless that reading shows extra usage off) is shown from that reading, with its age (`read 4m ago`), and no usage request is sent for it, unless a usage 429 holds the reads back (below). Any other account is read, but not while its refresh is held (the row shows today's error) or a usage 429 holds the reads back (the saved pause, or the account's own held 429). When the read gets a usage 429, or is held back that way (also for an account whose saved reading is recent enough to show: the row doesn't turn on the reading's age), the row shows the account's last good reading (only one bound to the account now in the slot), its age and `! usage read rate-limited, retry in Ns`, with the time taken from the pause, not the server's `Retry-After: 0`; without a last good reading it shows `usage read rate-limited (HTTP 429)` and the retry time. `--json` marks that error with the kind `usage_rate_limited` and the optional fields `retry_at_ms` and `last_good` (absent otherwise, so other rows keep today's bytes); the row still counts as an error and the exit code is unchanged. Other errors keep today's text. Limits: status shows only what the lease-owning daemon saved, so the readings of a daemon that only loads the file aren't shown; only Anthropic readings are saved, so other providers are read on every run; a remote broker keeps today's text; a saved reading is as fresh as the daemon's last save; and a reading the daemon dropped (a reported limit, a refused login) before the run loaded the file is gone, so a 429 on that account shows the plain line; status loads the file once per run, so a drop or a pause the daemon saves after that is not seen by that run.

The default strategy is **soonest reset**, with these priority tiers:

| Tier | Eligible subscriptions | Order |
|---|---|---|
| 1 | Fixed reset within 24 hours | Earliest reset first |
| 2 | Likely unanchored/idle window | Stable account order |
| 3 | Fixed reset farther away | Earliest reset first |
| 4 | Headroom known, reset unknown | Lowest utilization |

Tier 2 is a **heuristic**, not verification: usage ≤0.5% and reset within five minutes of `observation + window duration`. A freshly started window can look similar. It gives real work a chance to start an idle clock; it does not prove every natural Codex rollover needs activation. The chooser sends no activation probes.

Selection is sticky by default: keep a healthy seat within the same tier to reduce churn, unless another seat reaches a higher-priority tier (or has an earlier urgent deadline). Hitting any applicable window limit, including Claude's five-hour limit, disqualifies it. Cooldowns follow the provider seat, not its alias; duplicate aliases cannot bypass them. The cooldown of a login with a provider account id stays with that id: a new login into the label for another account id does not inherit it. A login without an account id keeps its cooldown across token refreshes, under its label; synaps can't tell that login gaining an id from a new login by another account, so any new login into that label inherits the cooldown too (it errs toward not using the account; `synaps auth state --clear` drops it). An account whose cooldown is active is not read while it lasts (no usage request, no token refresh): a selection that finds no account reports it as cooling down, and it is read again once the cooldown ends or `synaps auth state --clear` drops it. A turn that is already on an account that starts cooling down (another session met its limit) selects again before its next request instead of sending it to that account.

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

The preview shows rank, tier, reset, utilization, and rejection reason, and prints `would select: <provider>@<account>`. A `*` reset is likely unanchored. It calls the same ranking code as the broker, but a **new CLI process has no running session's stickiness/cooldown history**, and it is one ranking pass: a turn may still read again the account it would choose, or the accounts rejected on readings over 60 seconds old, so the account a turn finally uses can differ from the preview. It does not change selection, spend inference, or activate quotas; expired OAuth tokens may refresh normally. Without `--model`, it reports generic capacity, not a guarantee a seat can serve your chosen model. For remote credentials, run this command on the broker host.

It does not assume every 429 is quota exhaustion. Account switches happen under Auto only, on recognized pre-output window exhaustion. Codex permits **at most one immediate account switch per request**. Anthropic switches to **each account at most once per request** (a backstop of 16 switches stops a runaway router), and each lookup is told every account the request already used. No cross-account replay after partial streamed output/tool activity; generic throttles retain ordinary bounded backoff. Claude recognizes rejected unified/per-window rate-limit status headers, or a typed rate-limit error with a long reset hint; the live exhaustion header schema remains to be verified with an actual limited request. A recognized limit is reported to the broker even when replay is prohibited, so the next turn can choose another seat. When no account the request hasn't used has proven capacity, the request stops and says so; no text claims that another account will serve. A failed token refresh of the automatically chosen account, of any kind, does not end the selection either: that account counts as failed (with its kind) for the rest of the selection, and another account with proven capacity serves; `synaps auth state` shows the failed refresh. A refused refresh (`refresh_rejected`, `refresh_action_required`) is also held: while the stored login is unchanged, neither the chosen account's token vend nor a usage read asks the token endpoint again, and the account fails with the held kind (the text still names its login command or remedy). The hold ends when the stored login changes (a new login, or a refresh by another process), on `synaps auth state --clear`, on a daemon restart, or after 15 minutes; then one refresh is tried, and a new refusal holds it again. Turns that need the same account at the same time make one decision: the hold is checked and recorded under that account's refresh lock, so one refresh is sent and every turn waiting for it takes the held refusal (and selects another account), and the hold is for the login that was actually refused. Only the token endpoint's answer is held, never a failure to read or save the local credential store. The bound keeps a provider that wrongly refuses during an outage from leaving every account unusable until a login or a restart. Retryable refresh failures are never held.

A switch appears as `⚠ <account> is at a usage limit until <time> — switching to <next>`. A long wait on a 429 without a usage-limit header reads `⚠ <account> is rate-limited until <time> (HTTP 429, no usage-limit header) — …`, and a usage limit without a reset time reads `(reset time not given; not used again before <time>)`. Local runtime broker adapters share cooldown and sticky state through the runtime's token-cache handle; rebuilding an adapter no longer forgets a limit. Restarting the runtime clears that in-memory state; fresh usage still gates capacity. No cross-provider model substitution or purchase of extra credits is performed.

Claude's Fable model has a weekly limit of its own, inside the shared five-hour and weekly windows. The usage reading reports it in a `limits` list (`{"kind": "weekly_scoped", "scope": {"model": {"display_name": "Fable"}}, "percent": …, "resets_at": …}`), which synaps reads as a `seven_day_fable` window scoped to Fable, as a top-level `seven_day_opus` is scoped to Opus: a spent one keeps the account from Fable requests until its reset, and from no other model's (`synaps status` shows it as `7d · fable`, `EXHAUSTED` and `! fable: unavailable`). Any other single-word model name in such an entry is read the same way; a top-level window of the same id wins; the entries that repeat the shared windows (`session`, `weekly_all`) are skipped; every other entry is left out with the note `unrecognized limit entries ignored`. A `percent` is read as `utilization` is, as a percentage: 1 is 1 % (not a whole fraction), and 100 to 1000 is spent. A 429 on a message request is a limit of Fable's requests only when all of these hold, each of these headers sent once and readable (visible ASCII; a repeated or unreadable one is unknown, never absent): `anthropic-ratelimit-unified-7d_oi-status` is `rejected`; the `5h` and `7d` statuses are each exactly `allowed` or `allowed_warning` (any case); `anthropic-ratelimit-unified-5h-utilization` and `anthropic-ratelimit-unified-7d-utilization` are both sent and read from 0 to below 1.0 (a shared window can be spent behind an `allowed` status while extra usage pays for it; a missing one is unknown, not room); and `anthropic-ratelimit-unified-representative-claim` is absent or names the `7d_oi` bucket (`seven_day_oi` or `7d_oi`, any case). It lasts until the `7d_oi` reset. Anything short of that is left to today's rules (they decide: a limit of the whole account if the unified status is `rejected`; those rules read a repeated header's first value, as before). With any other claim, or an unreadable or repeated one, and the rest met, the log gets an INFO line, `7d_oi rejected but claim not recognized; rule 0 not applied; today's rules decide`, naming the claim only as a plain word (`[a-z0-9_]`, at most 32 bytes, lowercased; else `other`) and no other header value; it is admitted at most once a minute per process, on a monotonic clock of the limiter's own (never the response's time), and each line carries `suppressed=<n>`, how many were left out before it (a thread that stalls between its admission and its write can write its line late, so two lines can rarely land close together; lines never outnumber admissions, so a storm of such 429s can't flood the log). No rule reads `anthropic-ratelimit-unified-overage-status`: every one of the 219 429s captured from 10-11 to 10-20 had it `rejected` (extra usage off or out of credits on every account), so it can't tell a Fable-only limit apart. On a Fable request the local broker then cools the account for Fable's requests only: other models keep it, on its reading, which is kept (the cooldown wins over any room that reading shows for Fable), and the request moves to another account for Fable. On a request for another model such a 429 can't be Fable's limit, so it cools the whole account, as any recognized limit does. Anything short of every condition is classified by today's rules, as above. A turn already on an account that starts cooling for Fable selects again only if it is a Fable turn. Limits: the `limits` shape, the `7d_oi` header as Fable's own limit when rejected (every captured 429 so far had it allowed), and the claim values on such a 429 are taken from other clients and from captures, not from a documented schema, and are unverified; each unknown falls back to the behaviour above for any other limit. A selection without a model counts a spent Fable limit and a Fable cooldown as blocking, as it does any model's window (fail closed). Those selections are: the CLI-compatible automatic token vend, the daemon's automatic token route when the client names no model, `/auth reload` in a session whose model isn't an Anthropic one, and the legacy usage call under `auto`. Turns, and `/auth reload` in a session on an Anthropic model, pass the model. The quota keeper vends each account explicitly, by its key, so it doesn't go through the selection. `synaps auth state` shows a whole-account cooldown only, not one of Fable's requests (`--clear` ends both). A remote broker keeps no Fable cooldown, so there a Fable limit cools the whole account, as before. If the provider ever reported `percent` as a fraction (0 to 1), a spent Fable limit would read as 1 % used; the message-path 429 above is then what stops it.

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

## Inspect and clear auth state

`synaps auth state` shows what this machine's running daemon remembers about each OAuth account:

- its cooldown: until when, and a fixed reason kind (`anthropic_quota_exhausted`, `codex_quota_exhausted`, `client_report` or `other`; never the reporter's own text);
- its last usage read: ok, or the failure kind with the HTTP status and `Retry-After` when the endpoint sent them (delta-seconds only: an HTTP-date `Retry-After` is ignored, and a timeout has no status);
- its last token refresh exchange, with the HTTP status when the token endpoint answered with an error;
- a held refused refresh: the kind (`refresh_rejected` or `refresh_action_required`), when it was refused, and "held until the stored login changes or <time>";
- the last automatic selection per provider and model: the account chosen, or the failure kind and each account's part in it.

It reads memory only: no usage request, no token refresh, no write. It never shows tokens, emails, account ids or identity hashes.

```bash
"$SYNAPS" auth state --provider claude
"$SYNAPS" auth state --provider claude --account claude5 --json
# Forget in-memory state; credentials are never touched
"$SYNAPS" auth state --provider claude --account claude5 --clear
"$SYNAPS" auth state --provider claude --clear
```

- **NOT LIVE.** With no running daemon, or with one that predates `auth state` (the command names it), the report is built by the command itself, which has served no request: it lists the stored accounts with nothing recorded, under `auth state (NOT LIVE: …)`. Only a report from the daemon is live.
- **`--account LABEL`** shows that account, and only the selections that chose it or list it as failed.
- **`--clear`** first drops, in the running daemon, the matched accounts' cooldowns, cached usage readings and their read gates, last usage results, held refused refreshes, and stickiness (an account a provider and model currently keep). With `--account`, the selection history stays; only a clear without `--account` drops it, for the provider (or, without `--provider`, for every provider). The last token refresh record and everything on disk (credentials, refresh locks) are never touched. The daemon logs one INFO `auth_state_cleared` line. Without a daemon that answers `auth state`, nothing is cleared and the command fails.
- **Remote brokers.** Where auth uses a remote credential broker (`auth.remote_endpoint`), `auth state` refuses: the state lives in the `synaps auth-broker` process on the broker host, which has no `auth state` endpoint.

A failed automatic selection also logs one `auth_selection_failed` WARN line, with the kind and each account's `label:kind[:status][:cooling]`, in the log of the process that selected (for a remote broker, the broker host's).

## Remote clients and storage

The existing machine-authenticated broker exposes account-aware token, usage, capability and proxy operations. Upgrade the broker host as well as clients before relying on named accounts/auto. An old broker that cannot confirm the requested named slot is rejected rather than silently using its default. Machine-token holders can see account labels/minimal identity metadata; keep that token private. Use TLS or a trusted private tunnel, not public plaintext HTTP.

The broker always requires a machine token. A loopback bind (`--bind 127.0.0.1:8181`, the default) with no `--machine-token-file` / `SYNAPS_BROKER_TOKEN` generates one into `<base_dir>/auth-broker.token` (default `~/.synaps-cli/auth-broker.token`, mode 0600, reused across restarts); `--insecure-no-auth` is ignored on loopback, with a warning. The broker refuses a token file that is a symlink, not a regular file, or owned by another user. If the file was readable by group or other, the broker treats the old token as leaked and writes a NEW one (and logs that it rotated it); a deliberately group-shared (0640) file therefore stops working for the group, so share a token with `--machine-token-file` instead. A client whose `auth.remote_endpoint` / `SYNAPS_AUTH_ENDPOINT` is a loopback URL and that has no `auth.machine_token` / `SYNAPS_MACHINE_TOKEN` reads that file, and re-reads it once after a 401 (the file is never read, or sent, when an explicit token is configured, even for a loopback endpoint such as an `ssh -L` tunnel), so a same-user local setup needs no extra configuration and a running client picks up a new token without a restart. A client running as another user, with another `SYNAPS_BASE_DIR`, through an SSH port-forward, or older than this change must set the token explicitly; the broker's denial log and the client's 401 error both say so. A loopback broker also rejects any `Host` whose hostname is not `localhost`, `127.0.0.1`, `[::1]` or the bound IP (DNS rebinding). The port is not checked, so `ssh -L 9000:127.0.0.1:8181` works. Delete the token file and restart the broker to rotate it.

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
