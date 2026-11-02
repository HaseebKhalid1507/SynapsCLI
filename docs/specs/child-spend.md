# Child spend: a subagent's spend in its parent session's totals (P1 as built)

Status: P1 built, dark until P2 and P3 land; the three ship live together in one build. P2 adds the cap scopes and the dispatch checks, P3 the session latch and the driver stop tags. This spec covers P1 (delivery, per-attempt booking, persistence, wire, display) and is extended by P2/P3. The `child_spend` frame (§7.1) is P1j. Its frame half is built: the session sends one per booking, and on this line the daemon sends it to no socket connection. The per-connection `accepted_events` opt-in comes with P1's rebase onto the line that carries it, as an `OPTIONAL_EVENTS` entry with its handshake tests. Everything else here is built.

Decision: a subagent's spend counts in its parent's totals, and against the parent's cost caps when they are set. No new default caps: with no `max_cost_usd`, child spend only adds to the totals. It covers today's (v1) workers as well as the rollover (v2) ones.

## 1. What P1 changes, in one paragraph
Every paid request a subagent makes (at any depth, reactive or oneshot) is booked once, at the end of the attempt, with what the provider reported, into its root session's inclusive cost and token totals and into a separate child share. The booking is not a request of the session's own: last request, the context gauge, keepwarm and `api_call_count` don't move. The share is shown in status, the Conversation snapshot, the attach snapshot and the TUI's `/stats`, survives a save and restore (labelled "as of last save" after a restore), and is sent per request as an opt-in `child_spend` frame. In a host with a sink a oneshot no longer sends its aggregate Usage (it would count twice). A host without a sink is unchanged.

## 2. Objects (`runtime/child_spend.rs`)
- **SpendSink**: one per root session, created by the session actor. It holds the publication gate (`Open`, `Closing`, `Closed`, plus the count of live producers) and the sending half of an unbounded channel the actor reads.
  - Open admits runs and publishes. Closing refuses new runs and still publishes. Closed publishes nothing.
  - `close()` and `reopen()` (Closing back to Open; a Closed sink stays closed) take only the gate.
- **RunLease**: one per launch (a start, each resume, a oneshot); clones share the run.
  - Its identity is (handle, run nonce, agent, parent handle). The nonce is a fresh u64 per launch, so a resume is a new run. The agent label is bounded to 64 chars with control chars and line separators flattened.
  - `book(report)` publishes with a per-run sequence number 0, 1, 2… with no gaps: the gate check, the number and the enqueue happen in one critical section.
  - `seal()` cuts publication off while the worker may still run. It is idempotent and sends `RunSealed` after every report already published.
  - The last clone's drop sends `RunEnded`, unless the run was sealed. Each run decrements the producer count once and sends exactly one terminal.
  - `child(handle, agent)` is a lease for a worker this run delegates. It is refused under a sealed run ("parent run sealed: no new subagent work") and once the session is ending ("session ending: no new subagent work").
- **Locks**: the run's mutex, then the gate; closing takes only the gate. Nothing awaits, logs or does I/O under a lock. Late spend (a sealed run, a closed sink) and a gone receiver are logged after the locks are released. Poisoned locks are recovered, and Drop never panics.
- **SpendReport**: kind, the qualified model, tokens, and the price in integer nanodollars. It is priced with the session's own pricing table and the usage counters' nanodollar rounding. `SpendKind` is `Turn` or `Summary`, with the wire names "turn" and "summary".
- **SpendTokens**: input, output, cache_read and cache_creation, plus the optional 5m/1h TTL split. It uses the Anthropic split on every route, so input excludes cache reads and writes.
- **SpendTap**: a worker runtime's right to book. It holds the run lease. Only workers in a host with a sink get one; the root and every runtime in a host without a sink have none.
- **SpendParent**: where a launch takes its lease. `Session(sink)` is used for a worker the session's own turn delegates, and `Run(lease)` for one a worker delegates. It reaches tool calls through `ToolCapabilities.spend`.
- **ReportedUsage**: one report as a route parsed it. Each count is present only if the provider stated it, so an absent, null or non-integer count is not stated, and a TTL split alone is not a count.
- **AttemptSpend**: the per-attempt guard (§3.1).
- **RouteSpend**: what a route hands its transports. It is the tap (or none) plus the model. `RouteSpend::NONE` is inert.

## 3. Booking: one guard per paid attempt
### 3.1 The guard
- One `AttemptSpend` per paid attempt: an answered send, or an opened stream. A request refused before any body (a rate limit, an HTTP error) is not a paid attempt and books nothing.
- `observe`:
  - a report that states at least one count replaces what the guard retains, as a whole; it is never merged with an earlier partial;
  - a report with no counts is ignored;
  - an explicit zero is a report and replaces, on every route but native Anthropic (the exception below).
- **The native Anthropic exception.** On the native route the values the route prices for the root (`record_priced`, and the turn's one Usage event) are authoritative, and the guard observes exactly those. Anthropic's `message_delta` usage is cumulative, and the route has always read a zero or omitted counter there as "not reported": it keeps message_start's count for that counter, and the delta's TTL split only when the delta states one. So on this route an explicit zero in message_delta does not replace message_start's count. A start that states only zeros, with no later count, is priced by neither side and books no report. Why: the child share must equal what the same route books for a root turn on the same bytes, and counting the start's paid tokens fails toward counting. Root accounting is unchanged.
- `settle` books what is retained, once. It books nothing when no report came: unknown spend is never booked as zero.
- A drop without settle does the same, synchronously (no await, no I/O). So a cancel, an error return or a dropped future books what was reported.
- Each route settles explicitly before any retry wait, failover, account switch or next dispatch. A later check (P2) therefore sees the charge.
- `observer()` gives a task that sees reports (the extension stream forwarder) an observe-only clone. Once the guard is settled, an observer's reports are ignored, so a late report can't re-arm the slot or be booked twice. An observer's drop books nothing.
- Without a tap the guard is inert. The root's path, and every host without a sink, sends, emits and returns exactly what it did before (the off gate, §9).

### 3.2 Per route
| Route | Paid attempt | What is observed | Settled |
|---|---|---|---|
| Native Anthropic | each send in the retry loop | message_start / message_delta usage, merged as the root merges it: the values record_priced books (the exception in §3.1) | at the ordinary end, at the residual on cancel or error, before a stream retry; Drop otherwise |
| Chat Completions | each attempt of the proxy stream | the reported usage, with the last report that has counts winning | on cancel, on a broker error, before a retry's wait (the cut-stream branch), at the end; Drop |
| Codex (direct) and broker Responses (Copilot, xAI, Muse) | an answered send on Codex; an opened stream on the broker | each report with counts as `push_usage` reads it; the guard goes into the decoder with the body | at the top of the cut-stream branch, as soon as the body is consumed (before an in-stream quota failover, an empty-answer or capacity retry, or the result); Drop |
| Gemini (Code Assist) | each opened stream (a 429 opens none) | each `usageMetadata`, in the result's Anthropic field names; a thinking-only report has no counts | at the result; Drop on cancel or stream error |
| Extension, streamed | the call | the forwarder's streamed snapshots (the last with counts), then the call's own final usage (counts win) | when the call returns, before the cancel check after the join; booking fences the forwarder; Drop on cancel |
| Extension, direct complete | the call | the result's usage | when the call returns, also when the cancel check after it fires; an error carries no usage |
| Extension, host tool loop | each interior `provider_complete` | its result's usage | right after it returns, before its tools run or the iteration limit ends the loop; the outer result is never booked |
| Rollover summary (warm, cold (screened), routed) | the summary | the settlement's measured usage | at its settlement (`book`, `book_warm`), one `summary` report with the summary's model and the warm reply's TTL split |

Cloud routes carry no guard. A worker always has tools, so the cloud route's text-only preflight refuses it before any request. A guard must be added before a tools-capable cloud route is enabled.

Native root booking. Anthropic's streaming reference (https://platform.claude.com/docs/en/build-with-claude/streaming) says a stream carries "one or more message_delta events" and that "the token counts shown in the usage field of the message_delta event are cumulative". The root therefore books a native request once, from its last counted message_delta: each counted delta replaces the one before it (telemetry, the trace and the child's observation still follow every delta), and the root's one record_priced, usage log and `Usage` event are sent at message_stop, or at the stream's end if no message_stop came (a cut or an error), from the last counted delta, else from message_start as before. Before this, every counted delta booked the root again, so a stream with two counted deltas was priced twice while the child kept one. A client sees no change mid-stream: message_delta comes after the last content block, so `Usage` still arrives at the end of the reply, one event per request instead of one per counted delta. A user cancel still sends it: the cancel ends the read loop, and the stream's end sends the one `Usage` before the call returns (test cancelled_sse_after_a_counted_delta_sends_the_deltas_usage_once). Limit: only a future dropped without being polled again (an abort) between its last counted delta and message_stop sends no root `Usage`, as a drop before the first delta already did; the child's guard still books it at Drop. The hard drops are a turn task's abort on drop, an aborted compaction, and an aborted keep-warm ping, which then bills its worst case (it was sent and no `Usage` was seen) instead of the measured value. The other routes are unchanged: Chat Completions and Responses send one `Usage` per usage report, and Gemini's root sends none (both are follow-ups).

### 3.3 Stated limits
- Retries inside one extension RPC are the extension's own and are invisible. Whether an extension's streamed usage is cumulative is the extension's contract.
- A Gemini running count left by a cut or a cancel is booked as the lower bound it is.
- A summary that never reaches its settlement (cancelled, timed out, dropped) books nothing, now or later. Its reservation stays the turn's local charge, as today. At most one summary request is lost per cancelled summary.
- Usage without input and output counts is not a report. Explicit zeros book a zero-cost report, except on the native route (§3.1), where a zero counter keeps the start's.
- A summary's transports carry no tap (the settlement is its only publisher): the compaction and warm requests are built from default options, and the routed summary hands the router no spend.

## 4. Launches
- start, resume and oneshot take a lease from `ToolCapabilities.spend` after authorization, before launch. A resume is a new run of the same handle.
- A refused lease (the delegating run is sealed, or the session is ending) refuses the launch. It undoes the launch's authorization and reservation like any other pre-launch refusal: no owner entry, no handle, no slot.
- The worker runtime gets the lease as its spend tap, so its paid requests book under its own run, and its own launches take child leases under it (`parent_handle` = its handle).
- **Oneshot with a tap**: no aggregate Usage at the end, at any depth (it would count every request twice). The result, the other events and the subagent log are unchanged.
- **Oneshot without a tap** (a host without a sink, headless `synaps agent`): the aggregate Usage, exactly as before.
- The dropped-oneshot reaper seals the run before it ends a handle whose worker may still run: at the 30 s bound, or with no reaper thread to wait. A later report is logged as late spend and not booked. A worker that stopped ends its own run.
- **Launch paths without a lease source** (`spend: None`) get no tap and keep today's accounting: a oneshot's aggregate Usage goes wherever it went before, and reactive spend is uncounted. Listed as a follow-up. The paths are:
  - host tool calls inside an extension provider's tool loop (`openai/extension_route.rs`);
  - both tool paths of the legacy non-streaming loop (`Runtime::run_single`, inline and parallel);
  - a skill command's direct tool call (`skills/commands.rs`).

## 5. The session (`session/child_spend.rs`)
- The actor gives the sink to every runtime it builds (create, and the unpark rebuild). A select arm reads the sink with or without a turn. While a cancelled stream drains, reports queue and are booked after it.
- **Dedupe**: the actor expects the run's next number. Equal is booked. Lower is dropped with a WARN ("child spend already booked"). Higher is booked with an ERROR ("spend sequence gap") and resynced. Exactly-once is the producers' invariant (one lock over the number and the send); the dedupe only guards it.
- **Booking**: the report goes into the inclusive cost and token totals and into the child share (`child_cost`, `child_tokens`), never as a Usage event or a request of the session's own, and its `child_spend` frame is sent (§7; P1j), one per report. Reports are booked in passes: the select arm books the message it received and those queued behind it, at most `SPEND_BACKLOG_WARN` (4096) messages per pass, and fires again after the loop turns while any remain. Returning to the select gives other commands a chance between passes; it does not guarantee that a waiting command is served between any two passes (the select picks among ready arms at random). A settle and an end's last drain book everything (End, below). Once per pass that booked any, after its last report:
  1. one Conversation snapshot goes out. It carries the totals, so the pass's last snapshot says all that earlier ones would; the frames carry each request's detail;
  2. one save is queued (the ordinary coalesced save, never awaited). Building a save copies the history, so a save per report would cost a copy of the history per report.
  A report that arrives while the session is parked is logged and not booked; rule A keeps that from happening.
- **Queue (unbounded, approved)**: the sink's channel is unbounded on purpose. A producer publishes from Drop and under the gate lock, so it can neither block nor wait for room, and dropping an accepted report would lose spend that was billed. Nothing bounds the backlog. It is the arrival rate (at most one report per settled paid attempt per live run; live runs are at most the delegation tree's concurrency limit, `subagent.max_concurrent`, 16 by default, or a manifest's own) times the time the actor isn't draining (one long command, a cancel drain, a save). `SPEND_BACKLOG_WARN` (4096) is a warning and load-test threshold, not a bound: a pass or a settle that takes that many messages logs a WARN, and every report is still booked. The per-pass cap gives other commands a chance when reports arrive as fast as they are booked.
- **Rule A**: a session doesn't park, or end idle, while one of its workers holds a live run lease or reports are queued.
- **Status**: `child_cost`, `child_tokens` and `child_share_exact`, next to `cost`. `spend_reporting_workers` (the number of live leases) is present only while it is above 0, so a session without workers answers as before. A parked session's status has no cost or token totals today, so it shows no child share either; unchanged.
- **End** (every reason):
  1. admissions close first (Closing);
  2. after the turn is cancelled, the remaining workers are asked to stop (cancellation is requested; a worker's thread may still finish its attempt and report), and what they report is booked until none still reports or the deadline. The wait for producers shares the end's cancel budget; the deadline bounds that wait only. Every report already accepted is booked, also past it, since accepted paid spend is never dropped to meet a timer, so the end's booking time grows with the backlog. e6 measures it at the warning threshold: on a reload checkpoint, 4096 queued reports, 2000 saved messages and a real save took 26 ms and 102 ms in two runs, and 38 ms and 212 ms with the per-pass cap (on a loaded build machine), against 715 ms for the earlier code, which sent a snapshot and queued a save per report (the budget is 6 s);
  3. the sink closes, what is queued is booked, and the save runs.
  A report after that is logged as late spend.
- **Reload checkpoint**: the same steps, inside the coordinator's absolute per-session deadline less the save. `Checkpoint{Reload}` carries that deadline host-only, never on the wire. After its save the checkpoint reopens admissions, since the session continues in this process if the exec fails. A host checkpoint is unchanged: idle workers keep running.

## 6. Persistence and the child share
- The inclusive `session_cost` and token totals are journaled as before, and they contain child spend. No SessionMeta field is added, so old readers are unchanged.
- `child_cost` and `child_tokens` are saved with the .json snapshot only, like the keepwarm labels (SNAPSHOT_ONLY_FIELDS). A session with no share saves no child keys. A load clamps the share under the totals it is part of (non-finite or negative becomes 0).
- A restored conversation (a resumed session, an unpark) takes the share from the last saved snapshot, clamped, and never adds it back into the inclusive totals. Its cache buckets restart at 0, as the keepwarm ones do.
- `child_share_exact` is true for a conversation created in this process (and after a clear). It is false for the rest of a load that restored one.
- Journal mode (opt-in): the snapshot is republished only now and then, so a restored share is as of the last full snapshot, not the last save; the same as the keepwarm labels. In the default Json mode a graceful reload or park flushes the save first, so the restored share is usually right even when the flag is false.
- **Not retroactive**:
  - A session's cost rises only by bookings made from the build that has P.
  - Reactive worker spend from before that build was never booked anywhere and can't be recovered.
  - Oneshot spend from before it is already in the journaled `session_cost`, via the old aggregate on the Ok paths, and is not counted again.
  - The first snapshot after the upgrade (a restore) shows the last saved inclusive cost, `child_cost` 0 and `child_share_exact` false.

## 7. Wire (draft 2, frozen names; later changes are additive only)
### 7.1 The `child_spend` frame
One frame per booked report, after the session booked it. It is opt-in: the daemon sends it only on a connection whose Hello lists "child_spend" in `accepted_events`. The name is exact; no groups, no versions. Welcome echoes "child_spend" when the daemon supports it; an old daemon echoes nothing and sends no frame. The opt-in is decided by each connection's own Hello, so a reconnect or reattach decides afresh. Until the opt-in is on the line, the daemon sends the frame to no connection. The frame never enters the attach replay ring.
```json
{"type":"event","session_id":"20261018-091203-a1b2","seq":4182,"ts":"2026-10-18T14:12:03.512Z",
 "event":{"ev":"child_spend","handle":"sa_12","run":"9f3c2a71d04e6b85","spend_seq":3,
  "agent":"reviewer","parent_handle":null,"kind":"turn","model":"anthropic/claude-sonnet-4-6",
  "input_tokens":1200,"output_tokens":340,"cache_read_input_tokens":45000,
  "cache_creation_input_tokens":2100,"cache_creation_5m":2100,"cache_creation_1h":0,
  "cost_usd":0.031245,"cost_nanousd":31245000,"session_cost_usd":4.517032,"child_cost_usd":1.204118}}
```
| field | type | meaning |
|---|---|---|
| handle | string | The worker's handle as status, collect and the subagent rows show it ("sa_N"). A resume is a new handle. |
| run | string, 16 lowercase hex | The launch the report belongs to. A string, so JS never rounds it. |
| spend_seq | u64 | 0, 1, 2… per (handle, run), one step per booked report. Replays are covered by the envelope `seq`. |
| agent | string | The worker's agent label, bounded (≤ 64 chars, control chars flattened). |
| parent_handle | string or null | The worker that delegated it. null means the session's own turn did. |
| kind | string, required, open set | "turn": a conversation request of the worker, every attempt (retries, failover and an extension provider's interior tool-loop calls included). "summary": the worker's rollover summary. A client maps an unknown kind to "other". |
| model | string | The qualified model the request was priced at. |
| input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens | u64 | The same names and split as the session's own Usage frame. |
| cache_creation_5m, cache_creation_1h | u64 or null | The TTL split; null when the route reports none. |
| cost_usd | f64, USD | The price, for display. |
| cost_nanousd | u64 | The same price in integer nanodollars, exactly as booked. Sum this for a per-worker breakdown. |
| session_cost_usd | f64, USD | The session's inclusive total right after this booking (= status `cost`). |
| child_cost_usd | f64, USD | The session's child share right after this booking (= status `child_cost`). |

An old client never gets the frame. If one did, its decoder reads it as an unknown event (`#[serde(other)]`); the in-process converter shows unknown events as a notice. The TUI and the CLI attach client ignore the frame.

### 7.2 Snapshot fields (every client)
- Status: `"child_cost": f64`, `"child_tokens": {"input","output","cache_read","cache_creation"}`, `"child_share_exact": bool`, and `spend_reporting_workers` while above 0.
- The Conversation event's digest and the attach snapshot's conversation carry `child_cost`, `child_tokens` (the shape of `tokens`) and `child_share_exact`. Absent keys (an older daemon) read as no share; a client treats an absent `child_share_exact` as unknown.
- `cost` and `tokens` are inclusive and already contain the share, for every client.

### 7.3 What an opted-in client does with it
- Display only. Never add a frame's cost or tokens to the totals.
- Never count a frame as a request: last request, the context gauge and the call count don't move.
- A frame isn't ordered against the parent's Usage, Done or completion. It can arrive while the parent is idle, between turns, or after the worker's completion row.

### 7.4 Channel, order and lag
- Channel. The session publishes child_spend frames on its own optional broadcast channel (`SessionHandle::subscribe_optional`; capacity `SYNAPS_SESSION_OPTIONAL_EVENTS_CAP`, default 1024, at least 16), never on the main event channel. A booking pass can publish up to 4096 frames, and a settle drain more, so frames never share a ring with ordinary events: a reader that falls behind on one channel loses only that channel's envelopes. A connection without the opt-in never subscribes to the optional channel, so its ordinary events and its reload drain are unaffected by any volume of frames. A send with no subscriber leaves nothing in the ring and no queued work; the envelope is still built and routed.
- Seq. Both channels take `seq` from the session's one counter, so a seq is unique within the session and increases strictly on each channel. Together the two channels are gapless; one channel alone has gaps where the other's envelopes went (a connection without the opt-in sees gaps where frames went).
- Merge. A connection that gets both channels receives one stream, strictly increasing in seq. The forwarder never holds an envelope waiting for a later one. On receiving seq s from one channel, it first forwards, with try_recv and no waiting, every envelope of the other channel whose seq is below s, then s. An envelope it peeked above s is forwarded the same way: after the first channel's envelopes below it. This is complete because the actor publishes from one task, synchronously, so any envelope below s is already in its ring when s is received. Only a poll of the other channel made after s was received counts: the actor runs on another thread, so a poll made before can answer Empty just before both channels get envelopes (main s, then optional s+1, with the optional poll returning s+1 alone). So whenever one channel's envelope is the only one peeked, the forwarder polls the other channel once more before it forwards it (`daemon::merge`, both ways). A frame that is the last event before an idle stretch reaches the connection at once.
- Lag notice. If a connection's optional receiver lags, the forwarder sends that connection `{"ev":"child_spend_lagged","dropped":N}`. Only connections that accepted "child_spend" get it, and no new feature string is involved. `dropped` counts child_spend frames only; ordinary events are never in it.
- Where the notice sits. The forwarder makes it for its own connection, so it takes no actor seq. Its envelope carries seq u64::MAX, the daemon's marker for an envelope made by the connection (the main channel's lag notice uses it too). The forwarder delivers it immediately before the first child_spend frame that survived the gap; after a lag the ring is full, so a survivor always exists and the notice is never delayed. Two edges, both at the connection's floor (the Attached envelope's seq, see Opt-in below) or its end:
  - A gap that ends below the floor is not reported. The optional receiver subscribes before Attach and is not read during the attach wait, so it can lag on frames the attach snapshot already holds; when the first survivor is below the floor, every dropped frame is lower still, and the client lost nothing. A gap that straddles the floor is reported whole, so `dropped` is then an upper bound; it only marks the breakdown incomplete.
  - The notice goes out with its survivor, so a lag whose survivor has not gone out when the connection ends (the client leaves, or it is cut) is not reported; the reconnect marks the breakdown anyway (below).

  On the connection:
  - every envelope before the notice has a seq below the survivor's;
  - every envelope after it has a seq at or above the survivor's, so above every dropped frame;
  - every snapshot delivered after it has totals that include every dropped frame's booking.
  Each frame's booking pass publishes a snapshot on the main channel after the survivor. Its delivery is not guaranteed, though: the main channel can lag on its own (the connection then gets the main channel's lag notice), and a reconnect or a reload loses what was in flight. A snapshot can also arrive between the gap and the notice; its totals include the gap's bookings too.
- What a client does. Totals and the per-worker breakdown recover differently.
  - Totals. A snapshot's `cost`, `tokens` and `child_cost` are authoritative and include every booking before it. After child_spend_lagged or a reconnect, the client takes the totals from the latest snapshot it gets. If it lost ordinary events too (a main-channel lag notice, a reconnect, a reload), it waits for the next snapshot it receives or reattaches; the attach snapshot carries the totals.
  - Per-worker breakdown. A snapshot carries the session's aggregate child share only, not the split by handle, run or request, so it cannot restore what a lost frame said. A frame lost to lag, a reconnect or a reload is lost to the breakdown for good. On child_spend_lagged or a reconnect, the client either keeps the breakdown and marks it incomplete, or resets it and labels it as a new observation window that starts there. A snapshot does not clear that mark or that label.
- Not durable. Frames are display notifications, not receipts. A frame lost to lag, a reconnect or a daemon reload is not sent again; the totals are in the snapshots.
- Opt-in. A connection that lists "child_spend" in `Hello.accepted_events` (echoed in `Welcome.accepted_events`) subscribes to the optional channel before it sends Attach, and its forwarder merges both channels (`daemon::merge`). Frames whose seq is below its Attached envelope's are dropped: they were booked before the attach snapshot, whose totals include them. A connection without the opt-in never subscribes. The main pump still never forwards a ChildSpend that reached the main channel (a guard), and `skip` (the connection's event filter) applies to main envelopes only. A reconnect is a new opt-in: no frame is sent again.

## 8. Display (TUI)
- Both TUI hosts are clients of a session actor, so the TUI takes the share from the Conversation snapshots it already applies; there is no TUI-side receiver.
- `/stats` labels the share inside the totals, the way it labels keepwarm: `incl. subagents  read … / write … / input … / output …` and `incl. subagents $X`, with "(as of last save)" when the share isn't exact. Nothing is shown without child spend.
- The footer is unchanged. Child spend arrives as no Usage event, so the TUI's request count, last request and context gauge stay the foreground's own. The 5m/1h cache-write lines stay the TUI's own Usage accumulation, since the snapshot carries no TTL split for the share (a P1 limit; no wire field for it).

## 9. Unchanged
- The off gate (`spend_gate*`, `spend_gate_actor`, the oneshot gate) pins every route without a tap and a session actor with no subagent byte for byte, against fixtures written at 5f49b0a7. The fixtures cover requests, events, client-wire lines, results, status, Full and Digest attach, and the saved snapshot and journal. The new additive snapshot keys are the only normalization.
- Hosts without a sink: no sink, no tap, no booking, the oneshot aggregate kept. Exactly one accounting path per host.
- No cap behaviour changes in P1. P2/P3 add them, and all three go live together.

## 10. Visible changes with no cap set (P1)
- Session `cost` and `tokens` now include reactive workers' spend (never counted before), and Gemini and extension workers' spend (whose routes send no Usage).
- In a sink host, a timed-out or dropped oneshot books the request that was in flight when it ended (its start counts at least). The old aggregate never counted that request.
- The status, Conversation and attach snapshots gain the child-share keys (additive). Status gains `spend_reporting_workers` while a worker runs.
- A session doesn't park, or end idle, while its worker still runs (rule A).
- An end, and a reload checkpoint, stop idle workers before the save, so their last attempts are booked and saved. Before, they were dropped with the registry or killed by the exec.
- `/stats` shows the "incl. subagents" lines.
- Opted-in clients get `child_spend` frames.

## 11. Tests (by part)
- p1–p13 (`runtime::child_spend_tests`), the sink and lease:
  - numbering without gaps;
  - seal and close, also racing bookings (p7 can pass with nothing accepted; p7b books each run's first report before the close and requires it, and every booked report, to arrive exactly once);
  - refusals under a sealed parent or an ending session;
  - poisoned locks;
  - a gone receiver;
  - run identity and pricing;
  - the producer count.
- q1–q5 (`runtime::child_spend_tests`), the actor-side dedupe: in order, a lower number, a higher number, a resume and another handle, a run's end. z1: reopen.
- The off gate (§9: `spend_gate_tests`, `spend_gate_{codex,responses,gemini,extension}_tests`, `tests/spend_gate_actor.rs`, the oneshot gate), with its mutations red.
- a1–a10 (`runtime::child_spend_tests`), the `AttemptSpend` guard:
  - inert without a tap;
  - the last report with counts booked once, as a whole;
  - count-less ignored;
  - explicit zero;
  - settle once, and the drop after it books nothing;
  - a drop books what it retained;
  - observers never settle;
  - a summary guard's kind and model;
  - a usage object states only its counts;
  - a sealed run publishes nothing.
  a1 (inert) and a10 (sealed) are negative controls: they pass for a guard that never publishes, and the positive cases are a2–a9's.
- The route matrix, per route (`spend_route_tests`, `spend_route_native_tests`, `spend_route_responses_tests`, `spend_route_gemini_tests`, `spend_route_extension_tests`), red with each seam removed:
  - an answer books one report and changes no event and no result;
  - retries and failover book before the next wait;
  - two counted terminals book the last once;
  - a count-less report leaves the earlier one;
  - an explicit zero books zero (native is the exception: `spend_route_native_tests` runs each case to its end with the root's usage counters and asserts that the child books exactly the root's priced value: an explicit all-zero final delta books the start's 82,200 nanodollars, an omitted input and a partial final report keep the start's counts for what they omit, and a count-less stream books nothing on either side);
  - terminal failures, cancels and dropped futures book what was reported;
  - a refused request books nothing;
  - extension streamed, direct and tool-loop cases.
- Summaries (`runtime::rolling_summary_spend_tests`):
  - the cold (screened), routed and warm summaries each book one `summary` report at the settlement;
  - an empty reply that reported usage books it once;
  - a cancelled summary books nothing;
  - usage that isn't usable books nothing, while explicit zeros book zero;
  - a warm summary without full usage books nothing.
- Launches: sp1–sp6, rs1–rs3, s1–s4:
  - a sink-host oneshot matches the gate minus the aggregate;
  - the booked totals equal the old aggregate;
  - nested parent_handle;
  - refused leases leave no slot, owner or handle;
  - the reaper seals.
- Session actor:
  - c1–c7: booking while idle and during a turn, rule A park and end, oneshot through the real tool;
  - e1–e5, z1: end and reload checkpoint settlement and deadlines;
  - c8: a queued backlog under the per-pass cap is booked in one pass, with one Conversation event and one queued save; c9: a backlog of 12 passes' worth is booked in passes of the cap, one Conversation each, a status query that arrives after the first pass is answered between passes (it fails only if the select picks the spend arm over the waiting query 11 times running: about (4/16)^11 = (1/4)^11, under 3 in 10 million, a conditional scheduling estimate, not a bound, that assumes those two of the outer select's 16 arms (the command arm first, the spend arm fifth, on the dev line) are the only ready ones and that the select's random start is uniform), and every report is booked exactly once (count, sum and sum of squares of their indexes; each run's sequence continues); e6: a reload checkpoint with 4096 queued reports (the warning threshold), a 2000-message history and a real save finishes inside half its budget (the measured time is printed);
  - h1–h3: the share in status, events and attach, and across a park/unpark and a save;
  - j1–j3 (P1j): one frame per booking, sent with it and before the pass's snapshot, with the totals right after it (a grandchild's summary carries its run, parent, kind and model; a refused report sends nothing); a pass of several reports sends a frame each, then one snapshot with the last frame's totals; not replayed on attach. They read the optional channel and check each frame's envelope session id and its seq against the main stream's;
  - j4: a duplicate report (a seq the session already booked for that run) sends no frame and books nothing, and the run's next report still books;
  - j5: with 2 × 4096 + 7 reports queued, a paused plain receiver keeps every ordinary event, in seq order, through the reload drain too (§7.4).
- Persistence (core `session_journal`, `engine::session`): the share in both save modes, clamp, old reader, snapshot-only.
- Wire (`session::wire::child_share_tests`): the digest carries the share both ways, and an older digest reads as no share and not exact.
- Wire (P1j): the frozen key set, the draft-2 example, round trip, the old decoder, the exact name. `daemon_handshake`: before the opt-in, the session sends the frame and no socket connection gets it. At the rebase: the opt-in name, the daemon's fanout to mixed clients, and a fresh decision per connection.
- TUI: a Conversation replaces the share; `/stats` labels.
