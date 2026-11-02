# Automatic context windows — experimental implementation

## Intent

Context pressure is a work-planning signal, not a cliff or a claim about model
intelligence at a particular token count. The operator's starting heuristics:

| Effective window | Begin preparation | Prefer rollover |
| --- | ---: | ---: |
| 200,000 | 140,000 | 180,000 or earlier hard reserve boundary |
| 1,000,000 | 250,000 | 400,000 |

Intermediate sizes interpolate; larger windows keep the 250k/400k bands.
These are tunable heuristics, not provider quality guarantees. Current usage is
the existing request-aware conservative estimate, not exact tokenizer counts
or a learned cost optimizer. Reserves include system/tools, output/thinking,
likely tool result growth, safety margin, and the pressure advisory.

At ~350k of a 1m window, finishing a spec is legitimate. Starting the plan's
large implementation belongs in a fresh window. `plan -> execute` or `new_task`
at/above preparation pressure requests rollover. Ongoing execution can continue
through preparation. At the rollover band, `plan`/`wrap_up` get at most two
additional provider rounds (configurable 0–8). Re-reporting phases, lower token
estimates, or mode toggles cannot replenish this allowance. Hard capacity wins.

Task phases are model reports, not an independent proof of task complexity.
`context_checkpoint` must be called alone; batched calls are rejected before
any tool in that batch executes. This does not authorize any filesystem,
network, deployment, or delegation action.

## Enablement

Default is `auto` on Unix hosts (Linux, macOS), where the context archive is
supported, and `off` elsewhere. On supported Unix hosts:

```text
/context auto
/context status
/context off
```

These commands change only the current runtime. To set it persistently
(`context_management.mode = off` opts out):

```text
context_management.mode = auto
# Optional; omitted/auto thresholds use the table above:
context_management.pressure_tokens = auto
context_management.rollover_tokens = auto
context_management.reserve_tokens = 16000
context_management.finish_rounds = 2
# Successor footprint after a rollover; auto = min(100000, 2/3 of pressure):
context_management.target_tokens = auto
# Rolling summary cap, inside the target; auto = min(15000, 15% of target), 0 = off:
context_management.summary_tokens = auto
# Bound on the one summary request per rollover (1–600 s):
context_management.summary_timeout_secs = 300
# Model that writes the rolling summary (see "Rolling summary"):
context_management.summary_model = auto
```

`target_tokens` is the size a rolled-over context aims for, measured over the
same footprint as the policy's usage (system prompt + tool schemas + retained
messages). An explicit value must be positive and below the effective pressure
threshold. When a provider has reported usage for a recent request, the target
is read in provider-reported tokens (what the status bar shows) and converted
with that request's ratios; otherwise it is applied to the conservative
estimate directly. The fixed part (system prompt + tool schemas) is taken as
counting about 1:1 in both units, so only the rest of the target is converted,
at the sample's messages-only ratio (estimate - fixed) / (provider - fixed). The
whole-request ratio blends the two and over-sizes the successor when the fixed
part is large (~10% over a 100k target with a 59k fixed part). When no
messages-only ratio exists (the
provider count does not exceed the fixed part) or it lies outside 0.5–2.0, the
whole target is converted at the whole-request ratio as before. The fixed
part is assumed to count about 1:1, and the sample's messages and the
successor's are assumed to have comparable messages-only ratios. With a fixed
part lower in provider units, comparable message ratios, and a successor
smaller than the sampled request, this approximation biases downward; other
shapes can overshoot.
A sample applies only to the model whose request
produced it (switching models falls back to the estimator until that model
reports usage), and only when the provider count is nonzero and the
estimate/actual ratio lies within 0.5–2.0. Any newly booked usage replaces the
sample; an out-of-band or zero count clears it and the conservative estimate
applies. Usage is input
plus cache read plus cache write; the transport reads an omitted cache field as
zero and cannot distinguish it from a reported zero (an understated count shows
up as a high ratio and is rejected by the band above). Transports that do not
report usage never calibrate.

The 2/3-of-pressure ceiling caps only *optional* retention. The mandatory set
(below) plus the continuation message can exceed it, and even the pressure or
rollover thresholds. That case is not handled by selection: hard admission
still refuses a request that does not fit, and a successor that cannot shrink
by more than 1,024 tokens is reported as an unproductive rollover and deferred
(item 3), so it does not loop.

User-facing pressure notices show only the estimated token count, not internal
phase names or task-planning instructions. `/context status` reports the
configured capacity, current window, and effective thresholds.

Auto mode alone is not a pressure signal. Model advisories are request-local and
sent only when the current state changes (preparation, bounded finish, or an
unproductive rollover), not on every tool round. The one exception is a due
rollover waiting on workers: that reminder rides every deferred round, because a
single notice scrolls out of attention while the wait goes on. UI notices stay
edge-triggered. Normal/disabled assessments and committed rollovers clear stale
advisory state. These notices never enter durable history; their bounded size is
included in admission reserves.

The model should continue normal work, reporting substantial task boundaries,
not writing routine progress logs. Checkpoint notes are optional, a few lines of
unfinished state and next action; no speculative summary/spec is required for
context management. After rollover, use retained messages and the working note
first. Search/fetch archived history only for a specific missing fact, not to
reconstruct completed work. These guidance changes do not relax hard capacity,
phase/finish allowances, worker gates, or the durable-head barrier. Conservative
token estimation and the configured thresholds/reserves are unchanged. On Codex,
the effective system prompt (including this guidance) is sent once, in
`instructions`, not also as a synthetic system message in `input`. This does not
change historical message translation or other providers' system placement.

`memory_context` controls continuous-memory consent, not context-window usage.
For a consent status read, agents should send `{"action":"status"}`. Providers
that require every schema property can instead send:

```json
{"action":"status","mode":null,"capture_tools":null,"expires_minutes":null}
```

Only these three optional fields accept null as omission. Non-null `mode` and
`capture_tools` apply only to enable proposals; non-null `expires_minutes` applies
only to enable/recall_once. Other supplied values are rejected without changing
consent. Model calls still cannot enable durable capture or confirm history
import; Axel's control-only tool capability also cannot grant one-shot recall
(use `/memory once`).

For an isolated test profile, `/context auto 20000 120000` supplies lower
session-only thresholds. Do not use test thresholds as production defaults.
Invalid config disables the policy with a warning. Model tools cannot enable
it. Opt-in authorizes local archival of eligible content from the session;
this is independent of continuous-memory capture/recall consent.

`context_checkpoint(phase, note?)` is offered only to enabled streams. The tool
is stateless: a call records into the calling session's own state, which its
tool context carries; without one it fails and records nothing. The first
enabled turn registers it in the host's tool registry, which every session of
a host may share. A session with context management off withholds it through
its own session tool set (not offered, not activatable, refused if called) and
never removes it from that registry; likewise a session whose memory backend
is not Axel withholds the forum tools. Session conditions never mutate the
shared registry. Two paths are unscoped and not for session tool loops:
`Runtime::run_single` (the non-streaming legacy loop: no session tool set or
execution gate, so withheld tools are offered and run there, recording only
into the calling runtime's own state) and the extension-provider route's
fallback when no retained session set is passed (a fresh default core
without the session's withheld tools). (Skills are different: `load_skill`/`search_skills` read one
host-wide command registry built at boot, so skill discovery is host-wide,
not per session.) The host assesses before provider rounds and rolls over automatically at a safe
tool boundary. `memory_search` and `memory_fetch` must remain executable and
present in the actual schema; disabled or revoked retrieval prevents rollover.

## What changes at rollover

The **same persisted session and live environment** continue. The active
provider context advances to another window. It does not restart the process,
reset spend or turn limits, migrate workers or acquire new permissions. It
makes at most one summarizer request (see *Rolling summary* below). Headless chat's old automatic in-place compaction is suppressed in
this mode; manual `/compact` is unchanged. The TUI/chat streaming engine
supports this mode. `run_single` does not reject it: it checks only for an
unresolved head save (`durability_blocked`), and with context management on a
`context_checkpoint` call there records into its own runtime's state. It has no
session tool set or execution gate and never assesses or rolls over (see
above: it is unscoped and not for session tool loops).

1. If workers are running or finished-but-unreconciled, wait up to four admitted
   rounds (`WORKER_ROLLOVER_GRACE_ROUNDS`) for the model to collect them, then
   roll over anyway. At hard capacity, never wait. Workers are not migrated or
   cancelled; the successor envelope lists each one still tracked (handle,
   agent, state, age, the dispatch preview) and says not to re-dispatch it, so
   its completion wake is collected in the new window. Do not replay unknown
   tool effects. (An unbounded wait let a session with near-continuous workers
   grow ~240K tokens past the threshold until hard capacity refused every
   request, including the round that would have collected them.)
2. Prepare a smaller context under `target_tokens`. Always kept: the
   continuation message and working note, system/developer messages, the last
   assistant/result tail, the request that started the current turn (when a
   turn is admitted the host tags a new plain user request with a stable
   `_synaps_turn` id that persists with the session; interruption markers
   never start a turn, so a re-entry with only markers, pushed events or tool
   traffic since (e.g. cancel, save, resume, event wake) keeps the recorded
   start; like `_synaps_context`, the key is stripped from every provider
   request, including the compaction transport), and every user turn carrying an attachment (image,
   document, ...) because the archive excludes those payloads and could not
   return them. All of these keep their tool-call/result groups and are kept
   even if together they exceed the target (hard admission still applies). The
   session's first request is pinned if it fits ~30% of the target; a larger
   text-only one is replaced by an archive pointer and preview carried into
   later windows (one with an attachment is never stubbed). Remaining
   user-authored turns fill the budget newest-first: every typed request that
   fits first, then pushed `<event …>` messages fill whatever room remains up
   to the target. Retained messages keep source order and their complete
   tool-call/result groups; no tool result is synthesized. The continuation
   message lists what was not retained (count plus one line per recent item,
   bounded to ~2k tokens) and points to `memory_search(source=history)`; the
   archive still seals the full eligible history. When a rolling summary is
   on, its cap (`summary_tokens`) is reserved inside the target first, so the
   successor with its summary still lands at about the target. The cap never
   exceeds a fifth of the successor's message budget (target minus system and
   tool-schema overhead), so a small target still leaves room for turns.
3. If retained material cannot meaningfully shrink at a soft boundary, keep the
   current history and continue only if the full request still passes hard
   admission. Reassess after four admitted rounds or an 8,192-token footprint
   change; repeated phase reports do not force another attempt. No archive,
   window advance or budget reset occurs for this no-op. If the current request
   cannot fit safely, stop without clearing history. User constraints are not
   summarized away: dropped turns stay archived and indexed. See `context-rollover-recovery.md`.
4. Seal the old eligible source projection and working note to a private archive,
   with data/directory sync and retry deduplication. Verify the note before commit.
5. Request a frontend durable-head checkpoint. The frontend validates the logical
   session ID, preserves host metadata/accounting and saves a full snapshot in
   either persistence mode. Only a successful durable acknowledgement permits
   the runtime to publish the new window and dispatch another provider round.
   A private message marker identifies the window/archive for resume; it is
   removed before provider wire serialization.
6. Continue with the same turn meter, authority, tools and runtime state.

Archive retry deduplication is bound to the persisted session ID, while retrieval
remains project-scoped. Independent sessions therefore do not accidentally
share a retry/tombstone identity. Resume restores the saved window before the
first request. On a legacy binding the marker's archive is verified in the
store that session's own binding seals into (its base directory and project
scope; for a daemon session that is its client's working directory, not the
daemon's), and a miss restores nothing. Clear/session switches/manual
compaction discard the previous task note and phase without changing the host
opt-in setting. Hidden checkpoint notes and synthetic continuation envelopes
are excluded from source indexing.

The source projection excludes system/developer prompts, private reasoning,
restricted/sensitive content and binary blocks. Ordinary text and eligible tool
evidence are screened/redacted, not summarized. User-authored images remain in
active context; the archive does not claim to preserve every original byte.
Redaction is defense-in-depth, not a universal secret detector.

Cancellation before head publication preserves active history. An already-running
atomic archive write can finish after cancellation, leaving an unreferenced
eligible segment. Archive failures prevent replacement. Once head publication is
requested, cancellation is not raced against its atomic save: the candidate may
already be on disk. A missing/error acknowledgement stops further inference and
blocks ordinary saves/automatic turns until explicit recovery; it never writes
old history back over an ambiguously committed head. TUI recovery re-reads after
any detached writer and durably republishes the loaded head before clearing
latches. Cross-session recovery refuses while deferred work remains, rather than
automatically delivering the previous session's work into another conversation.
Unsupported consumers fail
closed. Host reset epochs prevent late acknowledgements from mutating a reloaded
conversation, including a reload with the same logical session ID.

The archive is synced first; the session snapshot rename is the logical head
commit, followed by directory fsync before acknowledgement. This is an ordered
publication protocol, not a general multi-file transaction: a crash before head
commit can leave an orphan archive and the older session head. After successful
acknowledgement, the new head is durable subject to filesystem/device guarantees.
Exactly-once external effects across a process crash are not promised.

Snapshots now use storage-only `_journal_generation` metadata and matching v2
journals. Stale journals cannot replay into a shortened or same-length replacement
head. Legacy unmarked snapshots/v1 journals remain readable and migrate on save.
Older binaries can read the snapshot but cannot replay v2 deltas; fold the journal
through a JSON-mode save with this implementation before downgrading. Prefix
validation costs O(history) CPU while append writes remain delta-sized. Async
session saves are ordered in-process through blocking-worker completion, even if
the awaiting frontend is dropped; concurrent processes writing the same session
are not supported. Ordinary bound snapshot rotation syncs before discarding old
journal deltas. No existing sessions are proactively migrated.

Known responsiveness limit: TUI checkpoint persistence currently awaits in the
event handler; RPC holds its state lock while saving. A stalled filesystem can
therefore delay cancellation/UI handling. A timeout must never clear the durability
block or permit inference; moving save ownership out of those control paths is
follow-up work.

## Rolling summary

Each successor also carries a rolling summary of the history earlier rollovers
dropped, so a new window reads like a fresh second window with a short record
of roughly the last ten. After the archive is sealed and before the head is
published, the host makes one request to the summary model
through the `/compact` transport (preflight, host keys stripped) with every
transport retry disabled: exactly one upstream attempt, including on HTTP 429,
5xx and connection failures. The single-attempt policy is passed through
routing, so it also skips the Codex persistent retry floor and Gemini's
reset-honoring 429 loop. The request never follows an HTTP redirect (a
redirect would replay it): routed requests use a broker whose client refuses
redirects, and the Anthropic leg a redirect-free client. A 3xx is an answered
request. This fails closed: when no redirect-free client can be built, the
summary is skipped with a one-time notice before anything is reserved or
sent, and a bounded request is never sent on another client. The request
uses the session's credential source and token cache. With a remote
credential source, local credentials are never consulted. With a remote
credential broker, a summary route that
would be proxied through the broker's `/proxy` operation is skipped before
dispatch with a one-time notice ("the remote credential broker can't yet
guarantee a single attempt"): the daemon dispatches proxied requests on an
ordinary broker that follows redirects, so a single upstream attempt cannot
be guaranteed there. Nothing is reserved or sent. Anthropic under a remote
broker is not proxied inference: the broker only vends the credential and
this process sends one direct Messages request on its redirect-free client,
so that route stays allowed. Under `auto`, a session model whose route
would be proxied falls back to the compaction model when that is usable,
and otherwise the summary is skipped with the proxy reason. A typed
bounded-attempt policy that the daemon acknowledges is follow-up work.
`/compact` is unchanged.

The summary model is `context_management.summary_model` (default `auto`). A
model is usable when it is not extension-hosted, its route has an enforceable
output ceiling (table below), and its provider's credential is not known to
be missing (Anthropic login or `ANTHROPIC_API_KEY`, the provider's OAuth
login, or its static key; with a remote credential broker the host cannot
tell and assumes it is present), and, with a remote credential broker, its
route is not proxied (above). `auto` uses the session's own model when it is
usable, with no notice. When that model is native Anthropic, the request is
the warm one when the warm path admits it
([rolling-summary-warm-cache.md](rolling-summary-warm-cache.md)), otherwise
the screened request above; an explicit `summary_model` naming the session's
model behaves the same. A summary on native Anthropic, the fallback
included, uses the Anthropic account already bound to the session and never
selects one (`no_bound_seat` below). When the session's model is not usable
(for example ChatGPT-Codex, which has no enforceable ceiling), `auto` falls
back to the compaction model (`compaction_model`, default
`claude-sonnet-4-6`), always as the screened request, with a one-time notice
naming the compaction model and why the session's model was not used
("Rolling summary model: <model> (the session model has no enforceable
output ceiling)", or "Attempted rolling summary with <model> (…)" when that
rollover's summary was not produced).
When the compaction model is not usable either, is empty, or is the
session's model, the summary is skipped with a notice giving the reason of
the last model tried. It never picks any other model or vendor. An empty
`compaction_model` removes only the fallback; it does not turn `auto` off.
An explicit model is used alone, with or without a compaction model: when it
is not usable the summary is skipped with a notice saying why. A
`summary_model` value that is neither `auto` nor a single model id turns the
summary off with a notice; it never falls back to `auto`. The summary is
billed at the rates of the model that served it. A ChatGPT-Codex session (no
enforceable ceiling) whose compaction model is also unusable, for example the
default Anthropic model without an Anthropic login, gets plain rollovers unless an
explicit `summary_model` is usable. Notices are said once per session
binding per reason and summary model (so a later model change is told again);
they can reappear after a reload. Model ids in notices have control and
whitespace characters removed and are cut at 80 characters.

The request carries a real output ceiling equal to
the summary cap, in the field the route's endpoint documents. A route with no
ceiling known to be accepted and enforced is never used as a summarizer: the
rollover is a plain one that carries the previous summary forward, and a
one-time notice says why. Coverage of the routes this router reaches:

Under a remote credential broker every route except Anthropic Messages is
proxied, so it is skipped with a notice (follow-up: a typed bounded policy
acknowledged by the daemon); the table describes local credentials.

| Route | Summary | Output ceiling sent | Live-verified |
|---|---|---|---|
| Anthropic Messages (`claude-*`, `anthropic/*`) | attempted (also under a remote broker: vended credential, direct request) | `max_tokens`, which counts thinking: thinking disabled where the model documents it (`between_tools` on Sonnet 5.5), omitted where it is off unless requested (before 4.5), adaptive at effort low with `max_tokens` = 2 × cap where it can't be off (Opus 5.5, Fable/Mythos 5.x); other Anthropic models are not used for the summary. Over OAuth the first system block is the transport identity, as on turns | no |
| xAI (`xai-auth/*`), Responses | attempted | `max_output_tokens` | no |
| GitHub Copilot, OpenAI/Azure-vendor models (Responses) | attempted | `max_output_tokens`, proxied under the Copilot identity | no |
| GitHub Copilot, Anthropic/Google-vendor models (Chat) | attempted | `max_tokens` (catalog vendor decides; unlisted ids do not route) | no |
| Gemini Code Assist (`google-gemini/*`) | attempted | `generationConfig.maxOutputTokens` (includes thinking) | no |
| Groq, Cerebras (Chat) | attempted | `max_completion_tokens` | no |
| Gemini API OpenAI-compatible (`google/*`), OpenRouter, Kimi platform and Kimi Code, Together, Fireworks, DeepInfra, NVIDIA, Mistral Codestral, Perplexity, SambaNova, Hugging Face (Chat) | attempted | `max_tokens` | no |
| `local/*` (the user's own OpenAI-compatible server) | attempted | `max_tokens`, or `max_completion_tokens` when the endpoint is an official OpenAI/Azure host | no |
| Any Chat route whose endpoint is the official OpenAI host (`api.openai.com`, regional `*.api.openai.com`) or an Azure OpenAI host (`*.openai.azure.com`) over HTTPS | attempted | `max_completion_tokens` for every model, without guessing model families | no |
| Other static OpenAI-compatible providers (Hyperbolic, Scaleway, SiliconFlow, Chutes, OVHcloud, InferX) | skipped, notice | no documented ceiling verified for them here | no |
| ChatGPT-Codex (`openai-codex/*`) | skipped, notice | none: the official Codex client sends no output limit to that backend and nothing available shows it accepting `max_output_tokens`; a bounded single-attempt request to it is also refused before sending | no |
| Extension-hosted providers | skipped, notice | none: the extension owns its transport, so a single attempt, a ceiling and usage reporting cannot be enforced | no |

Native `openai/*`, `azure-openai/*`, `google-vertex/*` and Muse routes are not
reachable through this summary router; they are unsupported for summaries, not
covered. "Attempted" means the request is sent with that field. "Live-verified"
says whether a live smoke test against the provider confirmed that it accepts
the field, enforces it on all billed output (including reasoning), makes one
attempt, and reports usage matching the host's booking; every route above is
unverified ("no") until such a test is recorded here.

The request holds the previous summary (from the
current continuation metadata) and a text projection of the messages this
successor does not retain. The projection is the archive's own screened,
redacted projection, rendered under `compaction_exclude`, with thinking always
excluded and no attachment or binary payloads. All bounds are in bytes after
escaping: 8,000 per entry, 32,000 for the file-operations record (paths
listed newest-touched first, so the cut drops the oldest), 96,000 for
the previous summary (its newest end kept), and a 400,000-byte ceiling on the
whole request input; the newest dropped messages are kept and every cut is
marked. A request over the ceiling is not sent.

This request is a narrow exception to "no provider request before the
durable-head acknowledgement". It is sent only after the source archive is
sealed, it carries no tools and cannot act, and it never continues the
conversation: the successor's first inference still waits for the durable-head
acknowledgement, and a missing or failed acknowledgement still blocks it.

The reply is Markdown organized by window: the window just closed in the most
detail, older windows progressively shorter, windows more than ten back folded
into one short "Earlier windows" paragraph. It records state, decisions, open
items, identifiers (IDs, paths, commits), who asked for what and what is
pending, and leaves out chit-chat and repeated status. The host enforces the
shape: it drops sections headed with a window after the one being closed
(window numbers are untrusted; arithmetic on them is checked), folds windows
more than ten back (about 400 characters each, the paragraph bounded at 4,000),
compresses the oldest detailed windows while the summary is over
`summary_tokens` (conservative-estimator tokens), and cuts hard only as a last
resort. The cut's marker is shortened or omitted when it does not fit, so the
result always fits the cap, even a cap of zero. The summary is escaped like the working note. It enters
the continuation message as untrusted historical data, and it is stored in the
continuation metadata (`summary.text`) for the next rollover. Each window's
full detail stays in that window's archive. The summary text itself is
persisted with the durable head, not inside the archive record: the archive
excludes derived continuation content, and its note field is capped at 8 KiB.

Failure is never fatal and is never retried in a loop. The one exception is
a refusal (stop reason `refusal`) of a sent warm request: one screened request
follows through the usual cold path and summary model, with its own
admission, reservation and booking, but only while at least 90 s of the
summary timeout are left, and it gets what is left as its own limit. A
refusal tends to repeat on the same content, so the warm request is never
retried, and no other failure gets a second request. A reply that stopped
with a refusal is never used as the summary, even with text before the
refusal. With no remote summarizer
(`compaction_mode = local`), nothing dropped, a projection or provider error,
an empty reply, a timeout (`summary_timeout_secs`, default 300 s), or a cancel,
the rollover proceeds as before and carries the previous summary forward
unchanged, alongside the dropped index. A cancel still stops the rollover
before head publication, with history retained. `summary_tokens = 0` disables
the summary; nothing is requested or carried. The timeout default is 300 s
because a warm summary at xhigh or max effort may write up to cap + 32,000
output tokens, and about 110 tokens/s was observed live; 120 s, the earlier
default, ended long ones early.

The summary request is billed like any other call. Usage is taken from the
reply of every route that reports it (the Anthropic response, the last usage
report of Chat Completions and Responses streams, Gemini's `usageMetadata`
with thinking tokens counted as output), separately from whether the reply
text was usable, so an empty reply is still billed. A failed reply that
reported its final usage is charged that usage too: a Responses
`response.incomplete` (for example at the output ceiling), `response.failed`
or a completion without text, and a Gemini stream error after the report sent
with its finish reason. The previous summary is carried forward. A running
Gemini count before the finish reason is not a final report. Usage counts as reported
only when it carries numeric input and output counters; explicit zeros are a
report, while an absent, `null` or empty usage object, or a partial one
without an output count, is not (missing output is not free output). It is
booked into the turn's cost and token totals, priced at the summary model's
rates, and so counts against `max_cost_usd`. It is also emitted as a session
usage event tagged with that model. It does not replace the conversation's
latest context footprint or request count, which bound and calibrate the
conversation's own requests.

Before sending, the host prices the request's upper bound: the conservative
input estimate plus the output ceiling it actually sends, at the model's
rates. If that exceeds what remains of the turn's cost budget (measured spend
plus held reservations), the host skips the summary, performs a plain
rollover that carries the previous summary forward, and says why once.
Otherwise it reserves that amount before sending, then settles it by the
request's typed failure stage and route:
- reported usage: the measured cost is booked and the reservation released;
- not dispatched: nothing reached the provider (preflight or routing refusal,
  local body or reasoning validation, Gemini setup, a broker refusal such as
  a missing credential or policy denial, or a connection that could not be
  established on the only attempt with no redirect followed). The broker
  reports a failed connection as a typed `ConnectFailed` only on a client
  that refuses redirects (after a followed redirect an earlier hop already
  received the request), and routed transports keep that type rather than
  flattening it to text. Which broker errors are pre-send is an explicit list.
  The reservation is released;
- rejected before generation: only on the official, non-overridden Anthropic
  host, only on the only attempt, and only for a parsed
  `invalid_request_error` (400), `authentication_error` (401) or
  `permission_error` (403). The reservation is released;
- unknown: everything else after the request was sent — any HTTP error from an
  overridden Anthropic endpoint, a router, a proxy, Copilot or another
  broker-routed or OpenAI-compatible provider; 409, 413 and 429 on every
  route; 408 and 5xx; any 3xx; a timeout, cancel or disconnect after sending;
  a reply without usable usage (including a failed reply without a final
  report, or with only a partial one). The reservation stays held for the rest of the turn
  as a conservative charge, so repeated failures use up the allowance.
Reservations count against `max_cost_usd` but are kept apart from measured
spend and token totals. The request's wall-clock time
falls in the closing window's segment, before the successor's fresh allowance
starts.

## Observability

Three `info` events under the tracing target `synaps::context`, with flat
`key=value` fields (no nesting, no quoting of codes). They carry metadata only:
a session label, window numbers, the archive id, counts, token numbers and
fixed reason codes — never message content, titles, prompts, file paths, tool
arguments or model names. Token numbers are conservative estimates unless
named `provider_*`; a `provider_*` field or `calibration_ratio` is absent when
no provider count is known.

**Enabling.** The events are written to the daemon's log file
(`synaps.log` in the active config directory) by default: the standard
logging setup enables `synaps::context=info`. `RUST_LOG` can override that
target (`RUST_LOG=synaps::context=off` silences it;
`RUST_LOG=synaps::context=info` enables it under a custom setup). Bare
global levels such as `RUST_LOG=warn` or `RUST_LOG=off` do not override
this built-in target default; to suppress context telemetry use
`RUST_LOG=synaps::context=off` or a covering directive such as
`RUST_LOG=synaps=off`. The standard file formatter hides targets, so a line is identified by its
message (`context rollover`, `context window settled`, `context rollover
deferred`); the example lines below use a formatter that shows the target.
Interest must exist when a rollover is prepared: the successor's sizes are
measured, and its settle comparison armed, only then. Enabling the target
mid-window does not retroactively arm a comparison, and disabling it can
drop a pending event. The file writer is non-blocking and lossy under
backlog. These events are diagnostics, not an audit trail: a missing line
proves nothing.

**Session label.** A logical session id is supplied by whoever binds the
session, so it is logged only when it is *generated-shaped*: it has a shape
the runtime itself generates — `YYYYMMDD-HHMMSS-xxxx` (four lowercase hex
digits), a canonical lowercase UUID, or `ephemeral-<uuid>` — and is then
logged as `session=<id>`. The shape is checked, not the provenance: a caller
can bind an id of that shape, which is then logged as-is (the shapes admit
only digits, lowercase hex and fixed separators, so no free text, paths or
control characters). Any other id — including any non-ASCII id — is logged
as `session_ref=<12 hex>` instead: the first 48 bits of an HMAC-SHA256 of
the id under a random key made once per process. This is process-local
pseudonymous correlation, not anonymity: it correlates events of one session
within one process, is not stable across restarts, and does not expose the
raw id or allow offline guessing without the key; 48 bits are not
collision-proof across very many distinct ids, and code in the same process
that can log chosen ids could test guesses.

- `context rollover`, once per committed rollover (after the durable head is
  acknowledged and the commit's binding check passes):
  - `session`/`session_ref`, `window_from`, `window_to`, `archive`;
  - `trigger`: `hard_capacity|wall_clock|phase_boundary|rollover_band|rollover_pending|other`;
  - `footprint_tokens`: the pre-rollover assessment (system + tool schemas +
    messages);
  - `provider_context_tokens`: latest provider input + cache read + cache
    write (this turn's last booked request, else the session's sample for
    this model);
  - `calibration_ratio`: estimate/provider of the sample used, if any;
  - `target_tokens`: the configured successor footprint, in provider units
    when calibrated (the one number here that is not an estimate);
  - `target_history_tokens`: the estimator budget for the successor's
    messages: the target minus overhead, converted to estimator units at the
    sample's messages-only ratio (or the whole target at `calibration_ratio`
    when that does not apply, see `target_tokens` above), with overhead plus
    messages capped at 2/3 of the pressure threshold; it includes the room for the
    continuation envelope (archive pointer, dropped index, note) and the
    rolling summary, not only retained messages;
  - `history_budget_tokens`: the safe history allowance — the context window
    minus the larger of the request reserves (output, thinking, tool growth,
    safety margin, advisory room) and the configured `reserve_tokens`, and
    minus overhead; a successor at or above it is refused (`admission`);
  - `history_target_tokens`: what the successor was sized to:
    `target_history_tokens` clamped below `history_budget_tokens`, less room
    for the rolling summary's second copy (admission's estimate also counts
    the copy in the continuation message's metadata) and 512 tokens of
    slack for a pinned stub (equal to `target_history_tokens` when the
    budget exceeds it by that room, as in every 1M window). The admission
    check counts the metadata copy too;
  - `overhead_tokens`: the assessment minus the estimate of the original
    messages (saturating). This is mainly system prompt and tool schemas but
    can include request-only transformations; it is not measured separately;
  - `successor_tokens`: `overhead_tokens` + the estimate of the rendered
    successor messages, envelope included;
  - `retained_messages`, `dropped_messages`: original messages kept / not
    kept (the new envelope message is not counted);
  - `must_keep_tokens`: the mandatory set (framing, protocol tail, turn
    start, attachment turns) before optional fill;
  - `workers_carried`: the pending workers the successor's manifest lists
    (the same union as the deferral's `workers`; `0` when none). Nonzero
    after the worker grace ran out or at hard capacity;
  - `continuation_tokens`: the whole continuation message as sent (envelope
    text, summary block, metadata); `summary_tokens` is the summary text
    alone, so it is part of `continuation_tokens`;
  - `summary`: `written|carried_forward|skipped`; `summary_reason`:
    `ok|disabled|nothing_dropped|no_summarizer|extension_hosted|no_enforceable_ceiling|missing_credentials|invalid_summary_model|remote_proxy|no_bounded_transport|no_bound_seat|not_admitted|unaffordable|run_deadline|failed`
    (`no_bound_seat`: the summary would go to native Anthropic but the
    session has no Anthropic account bound on its credential source that
    the selection still names; a background request never selects one, so
    nothing is reserved, vended or sent and the previous summary is
    carried; `run_deadline`: a worker run's deadline had passed at the
    seal, so no summary request was started; a deadline that passes later
    ends the attempt as a timeout or failure, `failed`);
  - the summary attempt (design: `rolling-summary-warm-cache.md` §9):
    `summary_attempted_path` (`warm|cold|none`; `warm` means a warm
    request was sent — its body published or a response received — not
    that it hit the cache; a warm attempt that timed out or was canceled
    before that reports `none` with terminal `timeout`/`canceled` and no
    id: the reporter withdraws the unpublished body, so no body byte can
    leave afterwards; `cold` means the screened request was started,
    also as the one attempt after a refused warm request, when
    `summary_cold_reason=warm_refusal` says so and the line's id and
    `summary_warm_settled_microusd` belong to that warm request while its
    other attempt fields describe the screened one); `summary_warm_miss`, why
    the warm path was not used when it was considered (`no_record`,
    `scope`, `stale`, `no_cache_evidence`, `thinking`, `limit`,
    `admission`, `prefix_policy`, `superseded`, …); `summary_terminal`
    (`written|empty|empty_after_fold|length|tool_call|refusal|missing_terminal|other_stop|error|timeout|canceled|not_sent|discarded`;
    `discarded` = answered after the session changed, text not used;
    `empty_after_fold` = the reply had text but nothing was left after
    the host's shape enforcement, e.g. its only section was for a later
    window: a failed summary, the previous one is carried forward);
    `summary_limit_tokens` (the request's output ceiling);
    `summary_cache_age_s` (the warm record's cache age at the decision);
    `summary_reserved_microusd`, `summary_settled_microusd` (worst case
    held, measured cost booked; rounded up); `summary_duration_ms`
    (always, sent or not; after a refusal, both attempts);
    `summary_cold_reason` (`warm_refusal`, only when the screened request
    followed a sent warm request); `summary_warm_settled_microusd` (that
    warm request's measured cost, only then and only when measured); `summary_error_status`, only with terminal
    `error` on a request that was sent: the HTTP status number, or
    `envelope` (a 200 whose body is an error), `decode` (a 200 that did
    not decode), `stream` (a streamed 200 that failed), `network` (no
    response); absent when the transport does not report it (routed
    providers); never provider text (the `rollover summary request
    failed` log line adds the failure's static kind as `error_type`: an
    in-stream error frame's vetted type or `unrecognized_error`,
    `transport`, `idle_timeout`, `empty_response`, `header_timeout`, or an
    HTTP error body's vetted type); `summary_id`, 64 random bits as 16 hex digits,
    present only for a warm request that was sent; that request's own
    telemetry record carries it as `rollover_summary`. Provider
    counters appear only when the provider reported them, never
    zero-filled: `provider_summary_input_tokens` (uncached),
    `…_output_tokens`, `…_cache_read_tokens`, `…_cache_write_tokens`
    (`_5m`/`_1h` where reported); `summary_cache_read_pct` =
    floor(100 · read / (input + read + write)), only when all three were
    reported and their sum is positive. The screened (cold) request's
    normalized usage fills missing cache counts with zero, so it reports
    input and output only and no read share.
- `context window settled`, once per window: the first provider usage booked
  after the rollover, on any attempt of any request — including an attempt
  that then failed or was retried; within one request with retries, the
  first attempt that reported usage counts. `session`/`session_ref`,
  `window`, `provider_context_tokens` (that attempt's input + cache read +
  cache write), `request_tokens` (the host's assessment of that request,
  taken before any request-only context advisory is appended),
  `successor_tokens` and `target_tokens` (from the rollover),
  `rounds_since_rollover` (provider calls in the window that returned or
  reported usage, this one included; a call canceled before sending or
  failed without usage is not counted and does not settle). If the model
  changes before the window settles, the window is not settled at all: the
  pending comparison is dropped, since another model's tokenizer and limits
  do not measure this rollover. A session reset likewise drops it.
- `context rollover deferred`, when a wanted rollover did not happen:
  `session`/`session_ref`, `window`, `reason`, `workers`, `footprint_tokens`,
  `provider_context_tokens`, with `reason` one of `workers_pending`,
  `unproductive_no_shrink` (including the cooldown rounds that follow),
  `retrieval_unavailable` or `head_not_acknowledged`. `workers` is the number of distinct pending
  workers: the union of the orchestration's unreconciled handles (running,
  or finished and not yet reconciled) and the registry's running handles
  (0 for other reasons). Logged once per reason per window, and again only
  when the footprint reaches a further absolute multiple of 50,000 tokens
  (a first event at 449,999 can repeat at 450,000).
- `context rollover refused: successor over the hard budget` (`reason=admission`)
  or `context rollover refused: archive commit failed`
  (`reason=archive_failure`), when a needed rollover could not be prepared
  and the turn ends with the error (history retained): the deferral's
  fields, under the same rate limit.

The token sizes of the successor (`successor_tokens`, `continuation_tokens`,
`summary_tokens`) cost one extra estimate pass over the successor; it is
skipped when no subscriber has `synaps::context` enabled at `info` while the
rollover is prepared, and then no settle comparison is armed either. If
interest appears after preparation (for example while the frontend is
acknowledging the head), the `context rollover` event is still logged, but
with `measured=false` and without those three fields — never with zeros. A
measured event carries no `measured` field.

Example lines (values illustrative):

```text
INFO synaps::context: context rollover session=20260916-101112-a3f9 window_from=3 window_to=4 archive=9f2c… trigger=rollover_band footprint_tokens=402311 provider_context_tokens=398020 calibration_ratio=1.01 target_tokens=100000 target_history_tokens=74210 history_budget_tokens=930000 history_target_tokens=74210 overhead_tokens=26800 successor_tokens=96400 retained_messages=41 dropped_messages=388 must_keep_tokens=12040 workers_carried=0 continuation_tokens=16900 summary=written summary_reason=ok summary_tokens=14200
INFO synaps::context: context window settled session=20260916-101112-a3f9 window=4 provider_context_tokens=97500 request_tokens=98100 successor_tokens=96400 target_tokens=100000 rounds_since_rollover=1
INFO synaps::context: context rollover deferred session_ref=4be0c19a7d23 window=4 reason=workers_pending workers=2 footprint_tokens=410000 provider_context_tokens=405000
```

To check that sessions settle near the target, pair each `context rollover`
with the next `context window settled` of the same session label and window
and compare `provider_context_tokens` with `target_tokens` (and with
`successor_tokens` to see the estimate's error). A rollover with no settled
event had its model changed, its session reset, no provider-reported usage
since, was not measured (`measured=false`), or its event was lost: the
target was disabled when it would have been logged, or the lossy writer
dropped the line. A provider footprint in the rollover band of a managed session
is either followed by a `context rollover` or covered by an earlier `context
rollover deferred` for that session and window (deferrals are rate-limited,
not repeated per request); one with neither most likely came from a request
outside context management, such as a worker or a session with the mode off.

## Short retrieval tools

```text
memory_search(source="history", query="failed approach", limit=8)
memory_fetch(ids=["ctx-<exact-returned-id>"], start=0, limit=8)
```

`source=notes` (default) retains the existing note-store semantics. History
results contain opaque IDs and bounded snippets. Fetch returns source indices;
large messages support `offset_bytes` within the message at `start`, with a
continuation hint. Output is bounded to about 24 KiB. Existing note IDs remain
unchanged. Fetch/search failures are not replaced with made-up history.

`memory_forget(id="ctx-...")` explicitly deletes that archive projection and
retains a tombstone. Normal rollover never invokes forget. This does NOT erase
other sessions, active context, derived notes, OS backups, or copies under other
IDs. Retry of the same sealed projection is denied after forgetting; a different
projection may still contain duplicated material. This is exact-projection
suppression, not content-wide erasure.

Archives live under the private `context-archives/<scope-hash>/` directory,
separate from ordinary session-retention cleanup. Limits: 128 slots including
tombstones, 16 MiB eligible input/serialized segment, 256 MiB per project,
4,096 source messages, 65,536 visited input nodes and depth 32. Excluded message
and media payloads and ignored metadata do not consume eligible byte budget;
their opaque descendants are not traversed. Admitted tool arguments/results
still count, and genuinely oversized eligible evidence fails without truncation.
See [eligible-input budgeting](archive-eligible-input-budget.md).
Storage exhaustion stops rollover; no automatic source eviction or tombstoning.
The index is currently a bounded scan, not a large-corpus search engine.
Non-Unix archive operations fail explicitly; enabling via the command is refused.

Concurrency: operations on one project's archive (rollover seals, parallel
history tool calls, subagents in the same process) wait for each other
in-process (not in FIFO order) and fail as busy if not admitted within 30
seconds; a seal and its stored note are one operation. Another process holding
the archive lock still fails closed without waiting for it ("archive busy";
history retained). The lock is released explicitly, so a child process forked
concurrently by another thread cannot keep it held. A caller canceled before
its operation is admitted reads and writes no records (opening the store may
already have happened); an admitted operation completes.

## Axel integration: one selected memory authority

**Shared-repository update:** the host now uses one shared brain with durable Git
repository scopes. Worktrees share repository memory; migration of existing path
scopes is explicit. Context-head durability and capture consent remain independent.
See [shared Axel repositories](shared-axel-repositories.md).


The unified host service now covers explicit notes, eligible source archives,
consented capture/recall, prior-session import, atomic legacy migration and memory
retention. See [unified Axel backend](axel-host-backend.md). In Axel mode rollover
projects source in memory and persists it directly in the same `.r8` database;
short history tools never fall back to the legacy archive. The durable frontend
head acknowledgement still gates further inference. Independent plugin/MCP memory
routes are disabled, rather than remain a second writable memory authority.

Migration is implemented but never automatic: metadata preview, exact digest and
explicit source/target scope precede one bounded transaction. Existing source
notes/archives and the installed plugin are untouched until the operator elects
cutover. Absolute expiry, restrictive policy and deletion evidence are preserved.
No real existing memories were migrated by implementation/testing.

## Evidence and remaining limits

Tests cover anchors, Plan->Execute at 350k, bounded extensions/hard limits,
config defaults/validation, private atomic archives, redaction, retry/tombstones,
large-message pagination, mixed tool pairs and images, cancellation, and an
actual loopback stream that rolls over between tool rounds. The off path
retains existing provider request behavior.

Live test in tmux `8:0.1`, isolated synthetic profile, extensions disabled:
Astra called plan then execute; the UI announced context window 2; history
search/fetch recovered `ROLLOVER_EVIDENCE_COBALT` and checksum `blue-27`.
Lower thresholds exercised the real control flow without sending a 350k-token
live request. No cost/quality improvement claim is inferred from that smoke.

Remaining work includes richer thread-level derivation graphs,
multi-process active-head coordination, actual-token/cost calibration,
large-corpus indexing, and portable archive support. The prototype intentionally
retains every real user turn; instruction-heavy histories may stop instead of
shrinking. Keep it opt-in until these limits have been reviewed.

### Validation record

- `cargo test --workspace --no-fail-fast -- --test-threads=1 --skip ui_catalog_fetch_github_copilot_returns_prefixed_chat_models`: **3,866 passed, 21 ignored, 1 filtered**, 118 targets. The filtered test is the previously reproduced ambient Copilot-account catalog test.
- `cargo clippy --workspace --all-targets`: completed with existing warnings; no new warning in the added modules.
- `cargo build --release --locked --offline --bin synaps`: succeeded.
- Release binary was started in test pane `8:0.1` with the isolated synthetic profile; after restart, context status restored window 2 and a new history search recovered the same source ID. No global config or installed binary was replaced.

**Reserve caveat:** the table is a soft policy, not a promise of dispatch at
those counts. Hard capacity comes first whenever the next round's reserves
leave less than the band: the 15% safety margin, the next tool result (10k
at the default 30 KB cap), the advisory, and the next round's expected
output. The output reserve is the thinking budget (16,384 for adaptive/off)
plus 16,384 of response, capped at the model's `max_tokens`
(`EXPECTED_RESPONSE_TOKENS`); it used to be the whole `max_tokens` (128k
for Opus), which put hard capacity at ~31.5k on a 200k Opus window. That
ceiling is not room a request needs: Claude 4.5 and later accept input +
`max_tokens` over the window and stop at the window
(`model_context_window_exceeded`,
<https://platform.claude.com/docs/en/build-with-claude/context-windows#context-window-overflow-behavior>);
for the earlier models that reject it, the request builder clamps
`max_tokens` to the room the input leaves. Hard capacity is now ~126.7k on
200k and ~806.7k on 1m (high or adaptive thinking), so on 200k it still
precedes the 180k rollover band (the margin and tool result alone take
40.5k) and the 140k preparation band; on 1m both bands govern. The
350k/1m policy scenario is tested without changing those safeguards.

### Post-review hardening

- Shared boot, `/resume`, clear/new session and manual compaction reset
  continuation bookkeeping at the appropriate lifecycle boundary. Status is
  correct before the first model request, without displaying internal phases.
- Progressive-disclosure catalog rebuilds preserve the opted-in retrieval
  surface; removed/disabled tools are not restored.
- Archive sealing uses the persisted session ID for deduplication, while
  read/search stay project-scoped. Forgetting one session's identical source
  projection does not tombstone an independent session's record.
- Neither synthetic continuation envelopes nor `context_checkpoint` note
  arguments are indexed as original source evidence on later rollovers.
- History-tool scope and base-directory bindings are captured before moving
  work onto a blocking thread.
- Final release pane check (before inference): `window 2 | capacity 1000000 |
  pressure 250000 | rollover 400000`; the earlier synthetic test overrides
  have been removed from the test profile. Installed/global Synaps is unchanged.

### Resumed durability follow-up

The resume experiment located `CONTEXT-ROLLOVER-RESUME-C12D603E`, fetched its exact
returned checkpoint ID, and verified clean branch `feat/context-continuation` at
`c12d603e`. The existing test pane still showed window 2 with 1m capacity and
250k/400k thresholds; it was inspected without sending a prompt or restarting it.

The follow-up adds the archive-before-durable-head barrier described above,
generation-safe journal recovery, frontend save/auto-turn failure latches and
runtime reset-epoch checks. Synthetic tests cover acknowledgement ordering,
rejected/unhandled checkpoints, blocked retries, stale journal crash states,
post-publication errors, session identity and detached-writer ordering. The real
loopback stream test verifies no second provider request before successful head
persistence and no second request at all after a failed save. This is not a new
large live-context or cost/quality benchmark.

The subsequent full backend integration resolves the independent notes/history/
capture routing gaps; see [unified Axel backend](axel-host-backend.md). The earlier
[source audit](../reviews/context-resume-axel-boundary.md) is historical, not the
current implementation status. No global
configuration, installed binary, plugin, external checkout or stored memories
were migrated by this follow-up.

Follow-up validation (synthetic/offline except the previously established ambient
catalog exclusion):

- Workspace: **3,898 passed, 21 ignored, 1 filtered**, 118 targets, using
  `cargo test --workspace --offline --no-fail-fast -- --test-threads=1 --skip ui_catalog_fetch_github_copilot_returns_prefixed_chat_models`.
- `cargo clippy --workspace --all-targets --offline`: succeeded with existing
  warnings, no warning in the new durability modules.
- `cargo build --release --locked --offline --bin synaps`: succeeded.
- Tool-schema drift, LOC ratchet, ignore ratchet, and `git diff --check`: passed.
- Workspace formatting check still reports pre-existing differences in untouched
  files; changed Rust files were formatted without unrelated churn.
- Logs: `/tmp/context-resume-workspace.log`, `/tmp/context-resume-clippy.log`,
  `/tmp/context-resume-release.log` and their `.exit` files. The release artifact
  was rebuilt locally; the running test pane and installed binary were not replaced.


### Wall-clock successor semantics (2026-09-06)

A successfully archived, durably acknowledged successor now starts a fresh
wall-clock segment; it no longer carries the original window's elapsed time.
This resets elapsed time only, not permissions, cumulative tool/cost/usage limits,
worker dispatch limits or autonomous grant deadlines. `/context auto` also makes
elapsed-time exhaustion request a successor even below context pressure. A
low-pressure time successor need not shrink the history, but must fit the request
budget and pass the same archive, tool-admission, worker and durable-head barriers.
Zero allowance and inference-free successor loops stop. Cancellation remains
checked before the next request; failed saves never renew time. A normal completed
answer still ends the stream; this is not an implicit autonomous plugin loop.
