# Prompt cache: hit rate since v0.8.0

Real usage, not a synthetic benchmark. These numbers cover every Anthropic
request Synaps made on the maintainer's two machines from v0.8.0 (Aug 4, 2026)
to Oct 25, 2026: main sessions, subagents and watchers, during daily
development of Synaps itself. That's 82 days, 79 of them active, through the
end of the 0.10.2 cycle.

- **Charts:** [`charts/`](charts/), rebuilt from the numbers with
  `python3 charts.py` (needs matplotlib).
- **Numbers:** [`stats.json`](stats.json), with every aggregate below by
  release, month, week, day and role.
- **Raw logs:** not published. They are private usage logs. See
  [Method](#method) for how they were turned into `stats.json`.

## Headline numbers

| | |
|---|---|
| Prompt tokens served from cache | **97.9%** (token-weighted). The median request reads **99.4%**. |
| Anthropic requests | **127,453**: 43.7k main sessions, 81.9k subagents, 1.8k unattributed |
| Tokens read from cache | **27.35 billion** |
| Tokens written to cache | 585M (2.1% of prompt tokens) |
| Uncached input | 5.4M (0.02%) |
| Cache rebuilds (≥20k tokens written) | 10 per 1,000 requests |
| API list-price equivalent | **$14.3k** with the cache vs **$137.7k** without it: **89.6% cheaper** |
| Every release | 97.1% – 98.4% |

The dollar figures are list-price equivalents, there for comparison. The
traffic ran on a subscription, so this is not money spent.

![weekly hit rate](charts/1-weekly-hit-rate.png)

## By release

| release (by tag time) | dates | requests | hit rate | median | p10 | re-written | reads | saved |
|---|---|---|---|---|---|---|---|---|
| 0.8.0 | Aug 4 – Aug 20 | 9,114 | 97.3% | 99.2% | 89.5% | 2.7% | 2.2B | 84% |
| 0.9.0 | Aug 20 – Sep 7 | 11,392 | 97.1% | 99.0% | 91.4% | 2.8% | 2.0B | 89% |
| 0.9.1 | Sep 7 – Sep 27 | 17,827 | 97.9% | 99.5% | 95.4% | 2.1% | 4.1B | 88% |
| 0.10.0 | Sep 27 – Sep 30 | 7,391 | 97.9% | 99.4% | 92.0% | 2.1% | 2.2B | 91% |
| 0.10.1 | Sep 30 – Oct 12 | 15,739 | **98.4%** | 99.3% | 92.3% | 1.6% | 3.7B | 91% |
| 0.10.2 | Oct 12 – Oct 25 | 65,990 | 97.9% | 99.4% | 93.9% | 2.0% | 13.2B | 91% |

Most traffic came from development builds, which run ahead of the latest tag.
Read each row as "while vX was the newest release". "Median" and "p10" are
per-request hit rates.

## By month, with cumulative reads

| month | requests | hit rate | reads | writes | **cumulative reads** | with cache | without | saved | subagent share of reads |
|---|---|---|---|---|---|---|---|---|---|
| Aug (from 4th) | 11,333 | 96.9% | 2.93B | 92M | 2.9B | $2.7k | $16.3k | 84% | ≥5% |
| Sep | 34,706 | 97.9% | 7.70B | 162M | 10.6B | $4.8k | $46.0k | 90% | 68% |
| Oct (to 25th) | 81,414 | 98.0% | 16.72B | 332M | **27.3B** | $6.8k | $75.4k | 91% | 61% |

"≥" marks August, when per-session telemetry was off on one machine, so its
subagent traffic can't be separated (see [Method](#method)).

![cumulative reads](charts/2-cumulative-cache-reads.png)

**Monthly requests went up about 7× from August to October, and the hit rate
held.** October alone read 16.7B
tokens, more than August and September combined. The week of Oct 12 had 54.6k
requests, five times the previous busiest week (11.0k), at 97.9%. Subagents
now account for about 60% of all cache reads. Their hit rate (98.0%) is
slightly higher than that of main sessions (97.8%).

![monthly savings](charts/4-monthly-savings.png)

## What keeps it there

![cache writes](charts/3-cache-writes.png)

- **Per-turn context never touches the cached prefix.** Extension context
  (memory recall, the current time, task pins) goes on the newest user
  message, the uncached tail, and the cache marker sits on the last durable
  block ([#81](https://github.com/HaseebKhalid1507/SynapsCLI/pull/81),
  v0.8.0). Anthropic's cache prefix is ordered tools → system → messages, so
  anything that changes every turn has to live after it.
- **`cache_ttl = 1h` on one machine since Aug 7.** It's a config choice; the
  default is `5m`. With it, a pause of up to an hour between turns doesn't cost
  a rebuild.
- **Subagents stay on the 5m TTL**
  ([#57](https://github.com/HaseebKhalid1507/SynapsCLI/pull/57)). See below
  for why that's right.
- **Keep-warm** (`cache_keepwarm`,
  [#205](https://github.com/HaseebKhalid1507/SynapsCLI/pull/205)) shipped in
  0.10.2. It's opt-in, and it was on for part of October.
- **0.11.0 fixes two leaks found in this data:** a daemon reload or park
  dropped the session's activated tools and re-wrote the prefix
  ([#294](https://github.com/HaseebKhalid1507/SynapsCLI/pull/294)), and
  cache totals reset on reload or resume
  ([#295](https://github.com/HaseebKhalid1507/SynapsCLI/pull/295)).

## Where the remaining ~2% goes (since Aug 7)

Since Aug 7, Synaps has written 566M cache tokens. 355M of that came from full
rebuilds (≥20k tokens written, reading less than was written). The table
assigns each rebuild to the conversation's previous turn, matched by prompt
size, since prompts only grow within a conversation. Treat the split as
approximate.

| cause (proxy) | main sessions | subagents |
|---|---|---|
| idle 5 min – 1 h, then resumed | 38M | **91M**: subagents use the 5m TTL |
| active session, prefix changed (reload, tool/schema change, rollover) | 59M | 55M |
| idle > 1 h (1h TTL expired) | 38M | 18M |
| new session / resume / rollover with no predecessor | 41M | 12M |

The largest bucket is subagents going idle for 5 minutes to an hour, usually
while waiting on long builds. **A 1h TTL for subagents would not pay off.**
The 1h write premium on every subagent write would cost about $1,149 to avoid
about $482 of rewrites, so the 5m policy stays.

## Method

**Two sources, merged per request:**
- `~/.cache/synaps/api-log.jsonl` (written when `telemetry = basic` or
  `full`): model, the 5m/1h write split and the request id. Before 0.11.0 it
  covered **main sessions only**, because subagent runtimes didn't inherit the
  telemetry level. It covers the whole window on machine A, and runs from
  Sep 20 on machine B.
- The daily `~/.synaps-cli/synaps.log.*` files: a `Token Usage:` DEBUG line
  for **every** Anthropic response, subagents included. These lines carry no
  model. Machine A stopped running Synaps on Oct 15.

**How they were merged:**
- About 98% of api-log rows match a log line (same input/read/write, within
  120 s). Matched lines are labelled **main**. Unmatched log lines inside a
  machine's api-log window are labelled **subagent**; outside it (machine B
  before Sep 20) they are **unattributed**. API rows with no log line are kept
  as main.

**Metrics and pricing:**
- **Hit rate** = cache read ÷ (uncached input + cache read + cache write),
  weighted by tokens.
- **Prices** are the list prices in
  [`crates/agent-core/src/pricing.rs`](../../crates/agent-core/src/pricing.rs):
  read 0.1×, 5m write 1.25×, 1h write 2× of the input price.
- **Subagent cost is an estimate.** Subagent log lines have no model. Each
  one gets a model from that machine-day's `Starting API request model=`
  counts minus the main-session requests, and is priced with 5m writes.

**Exclusions:**
- **Anthropic only.** Codex/OpenAI traffic doesn't produce this usage line.
- Keep-warm pings (by design they only read the cache) and failed requests
  with empty prompts.

## Measuring your own

Since 0.11.0, subagent requests are written to `api-log.jsonl` too
([#303](https://github.com/HaseebKhalid1507/SynapsCLI/pull/303)), so the
telemetry log alone covers every Anthropic request, without the DEBUG-log
matching above. Turn it on with `telemetry = basic` in your config. Every row
of `~/.cache/synaps/api-log.jsonl` carries the uncached input, cache read and
cache write counts that the hit rate above is computed from.
