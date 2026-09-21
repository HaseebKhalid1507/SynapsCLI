# Auto chooser: soonest-reset-first with automatic hop

Status: plan (2026-09-21). Branch `feat/multi-account-broker`.

## Problem

Subscription quota is perishable. Anthropic 7-day windows reset on a fixed
schedule; unused capacity at the reset instant is lost. Codex 7-day windows
are anchored by first use after a reset, so an idle seat's window never
starts and every idle day is a lost day. The operator runs several seats per
provider and burns roughly one seat per 24 h. Today `auth.account.<p> = auto`
picks the **lowest-utilization** seat, which is the opposite of what
maximizes throughput: it spreads load across seats whose deadlines are far
away while capacity on the seat that resets tomorrow expires unused. On
Anthropic a window-exhausted 429 is retried on the same seat instead of
hopping.

## Goal

When `auto` is selected for a provider, every vend chooses the seat whose
remaining capacity is about to expire, keeps using it until the provider
says it is rate-limited or out of usage, then hops to the next such seat —
automatically, deterministically, with the reasoning inspectable — while
never replaying partial work on another account and never overriding an
explicit account.

## Non-goals

* Changing the quota keeper (anchoring idle Codex seats by probe stays the
  keeper's job; the chooser only *prefers* idle seats so real work anchors
  them when there is demand).
* Cross-provider fallback (Anthropic ⇄ Codex). The chooser is per provider.
* Replaying a request on another account after any output or tool activity.
* Treating every 429 as exhaustion.

## Ranking rule (the whole design, in one place)

For each seat with a fresh reading (existing eligibility rules unchanged —
stale/unknown/exhausted/cooldown seats are never candidates):

* **budget window** = the applicable weekly window (`weekly_window`), else
  the applicable window with the longest reported duration.
* **anchored** = the budget window's reset is a fixed instant. A window is
  *unanchored* (Codex idle seat) when it reports ≈0 % used **and**
  `resets_at − observed_at ≈ duration` (tolerance 5 min). Anthropic windows
  at 0 % report non-full-length remaining time, so they classify as anchored.
* **urgent** = anchored **and** `reset − now ≤ urgent_horizon` (default 24 h).

Tiers, highest priority first:

| tier | who | order within tier | why |
|---|---|---|---|
| 1 | urgent anchored | soonest reset | capacity that will be lost soonest |
| 2 | unanchored | preference, then storage key | first use starts the 7-day clock; every idle day is lost |
| 3 | anchored, not urgent | soonest reset | earliest-deadline-first |
| 4 | eligible, no reset evidence | lowest utilization | headroom proven, deadline unknown |

Ties inside a tier: lowest utilization → operator preference → storage key.

**Stickiness** (avoid thrash and prompt-cache loss): when the currently
pinned seat is still eligible, switch only if a candidate sits in a strictly
higher tier, or both are in tier 1 and the candidate's reset is strictly
sooner. Exhaustion/cooldown of the current seat always causes a switch.

**Hop**: a provider-declared window exhaustion on the current seat puts it in
cooldown until its reported reset; the very next vend (mid-turn if no output
has started, otherwise at the next turn boundary) re-selects by the rule
above. Cooldown expiry (= reset) makes the seat a candidate again, at which
point it is usually unanchored (Codex) or has a fresh 7-day deadline
(Anthropic) and re-enters the ranking naturally.

## Goals and acceptance

### G1 — Policy: `Strategy::SoonestReset` (pure)

Owner: worker W1. Files: `crates/agent-core/src/core/auth/quota_policy.rs` only.

* `Strategy::SoonestReset` implementing the tier table; `SelectionRequest`
  gains `current: Option<&CredentialRef>`, `urgent_horizon_ms: u64`,
  `sticky: bool`; `Eligible` gains `budget_reset_ms: Option<u64>`,
  `anchored: Option<bool>`, `tier: u8`. `RejectReason::Outranked` carries
  `rank`. `Selection::Selected` carries `tier`/`budget_reset_ms` so callers
  can explain the pick.
* Pure helpers shared with the keeper/plan command: `budget_window(&[WindowLimit], model)`,
  `is_unanchored(&WindowLimit, observed_at_ms)`.
* Accept: synthetic-clock tests — soonest reset beats lower utilization;
  exhausted soonest seat is skipped to next soonest; 5 h-throttled seat is
  rejected with the 5 h reset and re-enters after it; unanchored sorts after
  urgent but before far anchored; stickiness holds within tier 3 and yields
  to tier 1; explicit account never falls back; determinism (same input →
  same output, independent of candidate order). Existing strategies'
  behavior and all existing tests unchanged.

### G2 — Configurable strategy, broker wiring, `auth plan` visibility

Owner: worker W2 (starts after G1's API lands). Files: `crates/agent-core/src/core/config.rs`
(auth section), `crates/agent-core/src/core/auth/broker.rs` (`select_auto`
and the pinned-seat memory it needs), `crates/agent-core/src/core/auth/account.rs`
(policy struct if needed), `src/cmd/auth_accounts.rs`, `src/main.rs`,
`docs/providers/multi-account-broker.md`.

* Config: `auth.auto.strategy = soonest_reset | lowest_utilization | preference_order`
  (default **`soonest_reset`**), per-provider override
  `auth.auto.strategy.<provider>`, `auth.auto.urgent_horizon_hours` (default 24,
  bounds 1..=168), `auth.auto.sticky` (default true). Invalid values fail
  closed to the default with a warning, never to a different seat rule.
* `select_auto` uses the configured strategy, passes the provider's last
  vended seat as `current`, and logs the pick with tier/reset/utilization
  (no tokens). Remote broker clients inherit the host's policy (no change).
* `synaps auth plan [--provider <id>] [--model <m>] [--json]`: read-only.
  Fetches the same fresh capacity the broker would, runs `select` with the
  configured strategy, prints the ranked table
  `RANK PROVIDER ACCOUNT TIER RESETS-IN USED% VERDICT` plus
  `would select: <seat> (tier n, resets in …)` or `no capacity (earliest reset …)`.
  Never mutates cooldowns or state.
* Accept: unit tests for config parsing/bounds; broker test with a fake
  usage server showing the strategy switch changes the pick; `auth plan`
  test on a seeded temp store (`SYNAPS_BASE_DIR`) with a loopback usage
  fake; docs section "Choose a seat" rewritten around the tier table.

### G3 — Anthropic: recognize window exhaustion and hop

Owner: worker W3 (independent of G1/G2; uses existing broker cooldown +
re-vend). Files: new `crates/agent-engine/src/runtime/anthropic_quota.rs`,
`crates/agent-engine/src/runtime/api.rs` (the 429 branch only),
`crates/agent-engine/src/runtime/auth.rs` (a re-pin-excluding-failed helper
if needed), `crates/agent-engine/src/runtime/mod.rs`.

* Pure classifier `classify_anthropic_429(status, headers, body_prefix) -> AnthropicQuotaEvidence | RateLimited`.
  Exhaustion is recognized ONLY from provider-declared signals, fail closed:
  primary `anthropic-ratelimit-unified-status: rejected` (+ `…-unified-reset`,
  `…-unified-5h-*`, `…-unified-7d-*` for the reset instant); secondary a
  `rate_limit_error` body **and** a reset/retry-after ≥ 300 s. Anything else
  is an ordinary 429 and keeps today's retry/backoff. Body is read into the
  existing bounded buffer for classification only and never logged.
* On first 429 of a turn, `tracing::warn!` the names+values of every
  `anthropic-ratelimit-*` and `retry-after` header (never the body) so the
  live schema can be confirmed from `debug.log`.
* Mirror `openai/account_routing.rs`: under `Auto`, pre-output, ≤ 1 failover
  per request → `report_cooldown(seat, until = reset)`, re-vend, require a
  DIFFERENT seat, rebuild the auth header, retry immediately with notice
  `⚠ <label> exhausted until <t> — switching to <label2>`. After output or
  under an explicit account: still report the cooldown (so the next turn
  hops), then existing behavior, with notice
  `⚠ <label> exhausted until <t>; next turn will use another account`.
* Accept: classifier tests (rejected header → exhausted with reset; plain
  429 with 5 s retry-after → rate-limited; body-only with long reset →
  exhausted; malformed → rate-limited); stream test with a scripted broker
  proving one hop pre-output, no hop post-output, no hop under explicit
  account, no loop when the broker returns the same seat. Codex path and
  its tests untouched.

### G4 — Docs, live validation, rollout (foreground)

* `docs/providers/multi-account-broker.md`: tier table, config keys,
  `auth plan`, what a hop looks like, the 429 verification note ("unified
  headers unverified against a live Anthropic exhaustion until observed").
* Live pass with the operator (read-only first):
  1. `synaps auth plan` for both providers; expected today: Anthropic →
     `claude4` (52 %, resets ~12:00 UTC, tier 1) then `default` (43 %,
     09-22 14:00, tier 3), `claude5`, `claude3`, `claude2` by reset; Codex →
     `codex3` first (0 %, sliding reset ⇒ unanchored, tier 2: using it
     starts its clock), then `default` (33 %, resets 09-27 13:24, tier 3),
     then `codex4` (2 %, 09-27 22:24); `codex5`/`codex6` rejected as
     exhausted; `codex2`/`codex7` (free, 30-day) rejected or last. The plan
     output must make that reasoning visible before anything is switched.
  2. `synaps auth use --provider anthropic --account auto` and the same for
     `openai-codex`; run a session; confirm the pinned seat matches the plan.
  3. When a seat exhausts: confirm cooldown + hop in `debug.log`, and that
     `auth plan` now ranks the next seat.
* Follow-ups (not in this plan): raise the non-current seats' usage cache
  above 60 s; keeper sliding-reset detection; capture `anthropic-ratelimit-unified-*`
  from 200 responses as free capacity evidence.

## Sequencing

W1 and W3 start in parallel (disjoint files, no API dependency). W2 starts
when W1 reports its public API. G4 is foreground work after W2/W3 land.
Each worker: `cargo fmt` on touched files only (tree is not fmt-clean),
`cargo build`, crate tests, `cargo clippy -p <crate> --all-targets -- -D warnings`.
No commits by workers; the foreground reviews and commits per goal.
