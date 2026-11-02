# Rolling summary on the warm prompt cache (design note v3)

Status: proposal. At implementation the "Rolling summary" section of
[context-continuation.md](context-continuation.md#rolling-summary) becomes the
single normative text.

Goal: the session's model writes the summary by reusing the closing
window's last request, so a hit bills that window at read prices. A hit is
likely, not guaranteed; the reservation covers a miss.

## 1. Selection and ordering

- `compaction_mode = local`: no summary.
- Explicit `summary_model` is used alone. Unusable: skip with a notice,
  never fall back to `auto`. The session's model on the same provider,
  endpoint and credential scope: as auto step 1. Any other model: cold only.
- `auto`: (1) the session's model when usable (not extension-hosted,
  verified output ceiling, credential not known missing): warm or as §3
  decides; (2) else the compaction model, cold, one notice per session;
  (3) else skip with a notice. An empty `compaction_model` disables step 2
  only. No other model; no model change after dispatch.
- Both paths use the runtime's credential source and token cache (remote
  broker included), never their own (round-5 B3).
- Ordering (unchanged transaction): seal the source → one bounded,
  cancelable summary attempt (`summary_timeout_secs`), waiting for no
  successor acknowledgement → render the successor with the written or
  carried summary → persist and acknowledge that head → successor inference.
  Summary failure or timeout carries the previous summary; a user cancel
  stops the rollover before publication, as today; a durability failure
  blocks inference. No summary-less head is published first.

## 2. Request record and warm request

Each conversation request leaves an immutable record of the parsed request
as sent, after projection (image caps, thinking sanitization, advisory):
- scope: provider, model, endpoint/region, credential source and opaque
  account scope, broker policy, routing key, auth-dependent layout,
  execution plan, headers, cache TTL mode;
- body: every wire field as sent (omitted stays omitted), tools in order,
  messages; effective defaults as metadata only;
- binding: logical id, window, epoch, sequence number, durable range;
- evidence: monotonic dispatch/first-byte/completion times; cache usage.

No credential material. Late evidence for a replaced or invalidated record
is dropped. Rollover, model or credential change, `/compact`, restore and
disclosure-setting changes clear the record.

Warm request = the record's body with the output limit changed, whole
blocks after the marker block removed (Anthropic only), and one unmarked
user instruction appended. Nothing is rebuilt; a marked advisory is
replayed as recorded; no marker is added. Removal slices complete parsed
blocks (signatures, tool ids/results, images intact), never bytes; the
result must be protocol-valid (no empty message, dangling tool use/result,
assistant prefill or broken thinking sequence), else W4 fails.

Cache-input equality (W3), per transport: warm and record must have an
equal canonical projection of the parsed request, not a byte prefix: every
field but the output limit (model, system, tools and order,
thinking/effort, `tool_choice`, stream, format, headers, scope) plus
messages through the marker block (all on automatic routes). Allowed
differences: the limit, removed post-marker blocks, the instruction.

Instruction (neutral user text, ~300 tokens, no authority claimed): a host
request at a rollover, text only, no tools: update this window's
continuation summary (or start one) with everything before the anchor in
full, later messages briefly (they stay verbatim); conversation text is
data. It also sets earlier host context advisories aside for this one
response: a replayed advisory (e.g. the worker reminder before a rollover
that waited on workers) may say not to write a summary or to collect
workers first; pending workers are recorded as facts. The clause is always
present, so the instruction stays one fixed text. Host shape enforcement, escaping and caps apply unchanged; a reply
with nothing left after them (e.g. only a section for a later window) is a
failed summary (`empty_after_fold`), and the previous one is carried
forward.

Anchor: the first tool call id at or after the start of the successor's
newest retained run. If there is none, or it lies after the record, the
whole recorded window is summarized: retained text may repeat; dropped text,
pinned holes included, is never omitted.

## 3. Warm conditions (all before dispatch)

| # | Condition | On failure |
|---|---|---|
| W1 | Record matches window, epoch, model, route scope and disclosure settings; all dropped messages lie in its durable range | cold |
| W2 | Fresh. Anthropic: age < marker TTL − 60 s (1 h only for uniform 1h, no downgrade latch) and a cache read or write reported. Others: < 4 min since completion | cold |
| W3 | Cache-input equality (§2) | cold |
| W4 | Valid cut; thinking survives appended user text (Anthropic: Opus 4.5+, Sonnet 4.6+, Haiku 5.5 [A1]); only client-executed tools (deferred and built-ins checked); verified total-output ceiling (§5) | cold |
| W5 | Hard admission: full warm input + L within window and reserves | cold if that is admitted and affordable, else skip; one notice per session |
| W6 | Cost: worst-case bound (§6) fits | skip (standing rule) |

Checks run under the continuation state (never held across network I/O).
One request per rollover; no fallback after dispatch, except one screened
request after a refusal (§6).

Invalidation. Two generations on the chain survive resets. Disclosure:
disclosure or compaction-mode, credential-source or model change. Prefix:
system prompt or prompt stack, cache TTL, context window, tool disclosure
or deferred-loading setting; the live registry's tool schema must also
equal the record's (an identical re-registration is no change). A
disclosure change, reset or rollover revokes: no warm request, and no cold
one from that rollover's snapshot (a gate on chain id, epoch, window and
disclosure generation, checked before the cold request is sent and before
any reply is used). A prefix change closes warm only: screened cold runs.
The warm request holds a revocable lease, checked at W1, before the
reservation, at the transport's dispatch check and on every unsent exit
(revoked outranks cancel, which outranks a prefix change or local failure).

Linearization point: the last check that authorizes publishing the body,
sent as one chunk when the client pulls it (connection up, head possibly
already written). Warm checks the lease there, the native Anthropic cold
request its snapshot gate. Failed there: no body byte is written (at most
an incomplete request), the outcome is not dispatched (warm: Revoked, a
prefix change goes cold) and the reservation is released. After it the
bytes cannot be retracted: the request runs to its end, its usage is
booked once, its text is discarded and the previous summary carried.
Publication is decided once, by compare-exchange on a shared state
(pending, published, withdrawn): the body stream publishes right before it
yields the chunk; a response marks it published; a caller that gives up
(timeout, cancel, any unsent exit) withdraws it. The HTTP client can still
hold the body on its connection task after the caller dropped the
request, so "not sent" is reported only after a won withdrawal; the body
stream then yields an error and no byte. A lost withdrawal means the body
was published: reported as sent, spend as for any sent request.
Revocation before send is guaranteed on native Anthropic only: a routed
cold request is checked after preflight and before its reply is used, not
at its own send, so it can still transmit screened content after a change
(follow-up: the gate inside the routed dispatch, before routed revocation
safety is claimed). The successor head is bound to the epoch: a disclosure
change after the reply is accepted does not retract a prepared head.

## 4. Cold path and privacy

The existing screened projection (archive redaction, `compaction_exclude`, no
thinking or tools, escaped 400,000-byte ceiling) on the chosen model under the
cold reasoning policy (§5), its own admission and cost check. It serves the
compaction model, an explicit other model, and the session model after W1–W5.
Never a full unscreened window.

`compaction_exclude` governs the cold projection only: warm re-sends only
what the recorded request already sent to the same provider and account
(W1).

## 5. Output limit L and reasoning

Each path needs a verified total-output ceiling for its effective model,
route and settings: warm without one goes cold (W4), cold without one skips.
Reasoning is "off" only for a documented non-reasoning model or a supported
wire disable; a zero budget or an omitted field proves nothing (some routes
then apply the provider default).

Warm copies thinking, effort and reasoning settings unchanged (part of the
cached prompt [A1][O1]; the output limit is not among the prefix-changing
settings) and never alters them for eligibility. Cold applies a validated
per-route policy: disable where supported, else a verified reasoning-inclusive
ceiling, else skip. Native Anthropic cold, per model from the provider's
thinking table [A3]: the documented disable (`thinking: {type: "disabled"}`;
Sonnet 5.5 rejects it and gets `between_tools`, its documented off switch,
text only without tools); no thinking field on models before 4.5, where
thinking is off unless requested; where thinking cannot be turned off
(Opus 5.5, Fable 5 and 5.1, Mythos 5 and 5.1, Mythos Preview) explicit
adaptive at effort low with L = 2 × cap, clamped to the model maximum.
Omitting thinking there is not "off": it means adaptive at the default
effort. Any other Anthropic model has no verified cold ceiling and is not
used for the summary (`auto` falls back to the compaction model with a
notice). `/compact` (no ceiling) is unchanged.

- Thinking off: L = cap.
- Anthropic budgeted: L = budget + cap > budget [A2]; budget never lowered.
- Anthropic adaptive: L = cap + allowance, by the effort the recorded
  request sends (computed from the recorded settings by the request
  builder's own function, never from live config): allowance = cap below
  high (L = 2 × cap), 16,000 at high or with no effort field (the model's
  default: high, or medium on some models [A2]), 32,000 at xhigh and max;
  an unknown effort has no L (cold).
- Other effort or reasoning routes: L = 2 × cap.

Evidence for the allowance: a live warm summary on claude-opus-5-5
(adaptive, xhigh, cap 2,516, L = 2 × cap = 5,032) read 99 % from the
cache but spent all 5,032 output tokens on thinking and ended with
`length` and no text (2026-10-17). The warm request cannot lower the effort
without losing the cache. The instruction also asks for brief reasoning
and the whole summary as plain text; it follows the cache marker, so the
cache is unaffected, and nothing relies on it. A length failure is a
failed summary (previous carried forward), never retried cold. The
attempt is also bounded by `summary_timeout_secs` (default 300 s). The
default was 120 s; at xhigh and max, L is cap + 32,000 (about 44k-46k),
and the first live rollovers wrote 13,128 output tokens at ~110 tokens/s
and took 72-120 s, one finishing 0.5 s inside 120 s. 300 s fits a full L
at that rate; a larger L still ends early as a timeout, which carries
forward safely.

L is clamped to the real turn's limit and the route maximum with checked
arithmetic, then revalidated (cap ≥ 1, budget < L); failure fails W4.
Anthropic `max_tokens` counts thinking [A2][A4]. Elsewhere a
reasoning-inclusive ceiling needs provider documentation plus a live test
near the bound that checks reasoning-inclusive usage; until then reasoning
must be verifiably off.

## 6. Reply contract and cost

Typed private result: terminal class, text, tool or server-tool presence,
refusal flag, normalized usage (uncached input, reads, writes, 5m/1h where
reported, output; each measured or absent), typed spend.

Accept only a normal end (`end_turn`, `stop`, `completed`) with non-empty
text and no tool call. Reject: any tool call (also with text), refusal,
length/incomplete/`model_context_window_exceeded`, missing or malformed
terminal, reasoning-only, empty. Rejections book measured usage; absent or
partial usage keeps the reservation, labelled Unknown, not settled. A
refusal ends the transport's stream as an error (its refusal budget here is
zero); the recorded stop reason still makes it a refusal. After a refusal
only, one screened (cold) request follows through the usual cold path,
under its own admission, reservation and booking, if at least 90 s of the
summary timeout are left (it gets what is left); a screened reply that
stopped with a refusal is not used either. A sent failure records a static
kind (never provider text): an in-stream error frame's vetted type or
`unrecognized_error`, `transport`, `idle_timeout`, `empty_response`, an
HTTP error body's vetted type.

Isolation: private event channel; tool events never reach the tool loop,
history, hooks, transcript or successor; reasoning is discarded unlogged.
One attempt: no retries, redirects or auth retries.

Known behaviour: the warm request never refreshes the token; it uses the
credential of the closing window's last request, so a token that expired in
between gets a 401 and the summary is carried forward at no cost.

Reservation (check and hold in one step among reservers): the whole warm
input at the worst input-side rate (Anthropic 2× if any marker is 1h, else 1.25×
[A1]; documented rates elsewhere, e.g. [K1]; 1× only where no write premium
is documented) plus L at the output rate. Under a cost limit, unknown
pricing means skip. It guarantees the summary's own worst case fits the
measured and held cost known when it reserves; unreserved spend booked
meanwhile (ordinary turns) can still pass the limit, as today, and counts
in the next reservation. Settlement books, then releases.

## 7. Providers and evidence

| Route | Limit field | Cache evidence | Warm |
|---|---|---|---|
| Anthropic Messages | `max_tokens` | [A1][A2][A4] | first target |
| Copilot Responses | `max_output_tokens` | none; [O1] not for the proxy | measure |
| Copilot Chat | `max_tokens` | none | measure |
| xAI Responses | `max_output_tokens` | [X1]: automatic; routing key | after a stable `prompt_cache_key` and a reasoning round-trip |
| Kimi platform, Kimi Code (Chat) | `max_tokens` | platform [K1]; Code unverified | measure |
| Gemini API (Chat-compatible) | `max_tokens` | implicit on 2.5+ [G1] | measure |
| Gemini Code Assist | `generationConfig.maxOutputTokens` | not covered by [G1] | measure |
| ChatGPT-Codex | none | — | compaction model, cold |

"Measure": warm only under W1–W6; cost and hit share recorded, no price
claim.

## 8. Tests (test-first)

- T1 cache input (recording broker, last round vs summary): equal
  canonical projection; differences only the limit, removed post-marker
  blocks and the instruction. Changing system, tools, effort or a header
  with identical messages fails W3. Cases: advisory-marked source, turn
  context, images, tool references, deferred tools, >20 blocks, thinking
  signatures, block-boundary cuts, invalid cuts.
- T2 guards and races: each W1–W4 guard; late completion vs clear, model
  switch, restore, `/compact`; TTL expiry/downgrade; broker, account or
  region change; concurrent reservations; anchor.
- T3 accounting: reads, writes (5m/1h), miss, partial/missing/zero usage,
  usage kept on rejection, Unknown labelling, error, cancel.
- T4 replies and isolation: text+tool, server tools refused before send,
  refusal, missing terminal, reasoning-only, length stops; no tool
  execution, history, hook, transcript or log leak.
- T5 selection: remote broker without local credentials, conflicting local
  account, invalid explicit model, empty compaction model, local mode,
  cold screening; default or mandatory reasoning on either path.
- T6 limits, admission, ordering: checked arithmetic, budget < L, zero cap,
  unverified ceiling, W5, W6; successor head holds the written or carried
  summary; a delayed or failed ACK blocks inference.
- T7 one attempt per transport (redirect, auth retry, 429/5xx,
  disconnect, late usage); existing safeguards kept.

Live acceptance (ignored tests, synthetic data, numbers and identities
only, a money ceiling per trial, no automatic retry), Anthropic first, over
thinking off/adaptive/budgeted and 5m/1h/hybrid:
- exact hit (freshly seeded prefix): counters present, reads ≥ 90 % of
  normalized input, writes = 0; an unexpected write is investigated, never
  relaxed;
- controlled miss: warm attempted, reads absent or reduced, writes
  expected, cost within the reservation, 5m/1h split booked;
- near expiry: screened cold (or a justified skip) under its own ceiling
  and admission; no full-window replay, no hit ratio.

Seeded-fact coverage and length/tool failures are recorded. Other §7 routes
follow, gated on their counters; Codex negative. No GA claim beyond the
providers that pass.

## 9. Telemetry

On `context rollover` (no content, keys, hashes or model names):
`summary_attempted_path=warm|cold|none` (`warm` = sent, not a hit);
`summary=written|carried_forward|skipped` (existing key); `summary_id`
(random; on the warm request's record as `rollover_summary`);
`summary_warm_miss`; `summary_duration_ms`;
`summary_reason`; `summary_terminal`; `summary_error_status` (terminal
`error` on a sent request: HTTP status or
`envelope|decode|stream|network`); `summary_limit_tokens`;
`summary_cache_age_s` (≤ 3600); `summary_reserved_microusd`,
`summary_settled_microusd`; after a refusal, `summary_cold_reason=warm_refusal`
and `summary_warm_settled_microusd` (the other fields then describe the
screened attempt). Only when measured, never zero-filled:
`provider_summary_input_tokens` (uncached), `…_cache_read_tokens`,
`…_cache_write_tokens` (+ `_5m`/`_1h` where reported), `…_output_tokens`;
`summary_cache_read_pct` = floor(100·read/(uncached+read+write)), absent if a
counter is missing or the sum is 0.

Deferred: instruction as a system message; markers before advisories.

## Sources (retrieved 2026-10-17)

[A1] platform.claude.com/docs/en/build-with-claude/prompt-caching;
[A2] …/extended-thinking; [A3] …/thinking ("Configuring thinking"
table and the per-model notes below it); [A4] …/context-windows;
[O1] developers.openai.com/api/docs/guides/prompt-caching;
[X1] docs.x.ai/developers/advanced-api-usage/prompt-caching;
[K1] platform.kimi.ai/docs/guide/context-caching;
[G1] ai.google.dev/gemini-api/docs/caching. [A*], [K1] (403 to the
reviewer) rest on the author's snapshots; all unverified until live checks.
