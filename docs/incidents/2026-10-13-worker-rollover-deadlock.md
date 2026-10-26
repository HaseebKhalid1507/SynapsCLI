# Incident 2026-10-13: context rollover deadlocked behind pending workers

**Status:** fixed in #221 (merge `c00d5d08`) and #222 (merge `877bad59`), released in 0.11.0. See [Fixes](#fixes).
**Affected:** `context_management = auto` sessions that run reactive workers
(`subagent_start`). Code references are to `dev` at `ae490b32` (0.10.2).

## Symptom

A long-running TUI session refused every new message before any API call:

```
✘ Config error: context hard limit reached with pending workers;
  collect/reconcile before continuing; history retained
  [provider_failed code=config_error correlation=turn-2183357-520]
```

Nothing the user typed got through, because every message is a new model
round and the round itself was refused. `/compact` got the user out: manual
compaction skips the worker check and started a new session (`879c`) on the
same runtime.

## Timeline (UTC)

| Time | Event |
|---|---|
| 07:35 | Last successful rollover (565 messages, 355K real tokens). Before that the session had rolled over 10 times, roughly hourly, between 264K and 355K real tokens. |
| ~15:56 | Context crosses the rollover line (about 300K real, 400K by Synaps's estimate). Rollover is due but deferred because workers are pending. |
| 15:37–21:24 | 37 workers dispatched (benchmark waiters, watch waiters, executors, reviewers). In 5.5 hours there were only about 10 minutes with no worker running, and never a model round with zero workers. |
| 15:56–21:31 | Context grows from 302K to 541K real tokens. Rollover is deferred on every round. |
| 21:30:20 | User presses Esc. The abort cancels **every** running worker, including `sa_229`, started at 20:44 by an earlier turn. Cancelled workers publish no completion wake. |
| 21:30:43–21:32:27 | About 7 more rounds succeed. The model sees `sa_231` is cancelled but never collects either worker. |
| 21:32:27 | Last request: 547K real tokens (about 711K estimated), the hard-stop line. |
| 21:32:55, 21:33:03 | The user's next two messages are refused before any API call. Deadlock. |
| 21:36 | `/compact` recovers the session as `879c`. |

## Root causes

It was not one bug. Five Synaps flaws lined up, and a usage pattern kept the
trap armed for hours.

### 1. Rollover starves while any worker is in flight (primary cause)

`runtime/stream.rs` defers a due rollover while `workers_pending` is true. That
flag is "any running handle in the subagent registry, or any id in
`OrchestrationRuntime::unreconciled_runtime_handles()`". The latter uses
`all_unreconciled_ids()`, which includes **running** workers. The completion
gate (`completion_gate`) deliberately ignores running workers, because
reactive workers are meant to outlive the turn that started them. The rollover
gate did not. With workers almost always alive, rollover never got a window
and the context ran about 240K tokens past the threshold.

### 2. Cancelled workers never leave the orchestration

The abort path cancels running workers (`session/actor.rs`, "Cancel all running
reactive subagents"). The finalizer then skips the completion wake for a
cancelled worker (`tools/subagent/finalize.rs`, "suppressing completion
wake"), which is correct for the UI. It does not update the orchestration
record. So the worker stays "unreconciled" until the model calls
`subagent_collect(reconciled=true)`, and the wake that would have prompted the
model to do that is the one being suppressed. A cancelled worker pins
rollover, model changes and the completion gate indefinitely.

### 3. The "collect your workers" advisory is sent once

`ContextAdvisory::WorkersPending` is edge-triggered like the other advisories
(`ContinuationState::update_advisory`). It is attached to one request,
request-only, and never persisted. The model saw it once, around 16:00. For the
next five hours of deferrals it never appeared again.

### 4. The hard stop has no way out

At hard capacity with workers pending, `stream.rs` returns the config error
above. Collecting the workers needs a model round, and that round is the one
being refused. There was also no user command to reconcile workers, so the
user had no path except `/compact`.

### 5. Esc cancels workers from earlier turns

The abort path cancels every running handle in the registry, not just the
workers the interrupted turn launched. Reactive workers started by earlier
turns are background work the user may be waiting on, and losing them to an
Esc aimed at the current turn came as a surprise. It also created the two
cancelled-but-uncollected workers that pinned the gate.

### 6. Usage pattern: workers as timers

The operator (an agent running in the session) used Opus workers as sleep
timers for external benchmark and watch processes, with timeouts up to
3,500 s. That kept a worker alive almost constantly, which is what let cause 1
starve rollover for hours. This is not a Synaps code defect, but it is how the
other five got exercised.

**Why this had not happened before:** it needs auto context mode (enabled on
this machine Oct 10), hours of near-continuous workers, and a cancel close to
the hard limit. Earlier rollovers fired whenever there was a gap with no
workers.

## Fixes

| # | Cause | Fix | Where |
|---|---|---|---|
| 1 | Rollover starves on running workers | Bounded grace, then roll over and carry the workers | #222 (`fix/rollover-pending-workers`) |
| 2 | Cancelled workers never retire | Retire cancelled workers in the orchestration at finalize | #221 (`fix/worker-cancel-scope`) |
| 3 | Advisory sent once | Repeat the WorkersPending reminder every deferred round | #222 (`fix/rollover-pending-workers`) |
| 4 | Hard stop has no way out | Hard capacity never waits on workers; `/workers reconcile` | #221, #222 |
| 5 | Esc kills earlier turns' workers | Turn-scoped cancel for user interrupts | #221 (`fix/worker-cancel-scope`) |
| 6 | Workers used as timers | Operator rule, no code change; fix 1 removes the lever | operator config |

### Fix 1: bounded grace, then carry the workers

**Options considered**

- *Ignore running workers in the rollover gate* (mirror `completion_gate`).
  Simple, but a rollover could archive the dispatch call while the worker is
  still running. The next window would then not know the worker exists, could
  re-dispatch it, and would not know what to do with its completion wake.
- *Cancel workers at rollover.* Destroys work the user is waiting on.
- *Wait a bounded number of rounds, then roll over and tell the next window
  what is still in flight.* Chosen.

**Design**

- `ContinuationState::worker_gate(workers_pending, hard_stop, time_checkpoint)`
  returns `Proceed`, `Defer` or `EndTurnForTime`. A due rollover with pending
  workers defers for up to `WORKER_ROLLOVER_GRACE_ROUNDS` (4) admitted rounds,
  then proceeds. Hard capacity never defers. A wall-clock time checkpoint with
  live workers still ends the turn, as before. The counter resets when a
  rollover commits.
- `subagent::pending_workers()` lists every worker that holds the gate:
  running handles, finished-but-uncollected handles, and ids the registry has
  already reaped but the orchestration still counts as unreconciled.
- `subagent::render_worker_manifest()` renders that list (up to 12 rows, then
  "and N more") into the successor envelope, inside the continuation frame:
  handle, agent, state, age, and the dispatch preview. Model-authored text is
  neutralised the same way as the working note. The manifest tells the next
  window not to re-dispatch these workers and to collect each one when its
  wake arrives.
- Workers are not cancelled or migrated. Their completion wakes go to the
  same event queue as before.

### Fix 2: retire cancelled workers

**Design**

- `OrchestrationRuntime::retire_cancelled(id)` moves a worker straight through
  terminal (Failed), collected and reconciled. A cancellation needs no result
  integration, so the cancel itself is the attestation.
- `finalize::retire_if_cancelled()` calls it once the finalizer has run and
  `cancel_requested` is set. `start` and `resume` call it after the worker
  thread exits, and again right after `mark_running`, so a cancel that races
  launch is not missed.
- Completed workers keep the collect-and-attest contract and are never
  auto-reconciled. The call is idempotent, and a later model
  `subagent_collect` still succeeds.

### Fix 3: repeat the reminder while rollover waits

- `ContextAdvisory::repeats_each_round()` is true only for `WorkersPending`.
  `ContextAdvisory::for_request(transition, current)` attaches the transition
  advisory as before, or the current one if it repeats. The reminder rides
  every deferred round, which is at most four rounds after fix 1.
- The text now tells the model to collect finished workers now and that
  rollover will proceed after a few rounds and list what is still pending.
- UI notices stay edge-triggered. Pressure, FinishBounded and Unproductive are
  unchanged.

### Fix 4: no dead end at the hard limit

- Engine: at hard capacity `worker_gate` returns `Proceed`. The rollover runs
  and carries the workers (fix 1) instead of returning the config error.
- User escape hatch: `/workers [status] | cancel <sa_N|all> | reconcile
  <sa_N|all>`, an engine command shared by TUI, chat and server.
  - `status` lists every tracked worker and flags the ones holding rollover,
    including reaped-but-unreconciled ids.
  - `cancel` stops running workers. After fix 5 this is how background workers
    are stopped.
  - `reconcile` is the user's attestation for **finished** workers the model
    never collected. It never touches a running worker. No inference and no
    history change.

### Fix 5: Esc stops only the interrupted turn's workers

- `SubagentRegistry::begin_turn()` bumps a generation at turn start. Each
  registration is stamped with the current generation.
  `cancel_running(WorkerCancelScope)` cancels running workers in scope, skips
  handles whose cancel is already in flight, and returns the ids it cancelled.
- A live user interrupt uses `Turn(generation)`. Quitting, session teardown,
  cost cap, host checkpoint or restart, and driver revocation keep `All`.
  Frontends that never call `begin_turn` share generation 0, so they keep the
  old cancel-all behaviour.
- After a turn-scoped cancel, a notice says how many earlier workers are still
  running and points at `/workers cancel all`.

### Fix 6: operator rule

Do not use workers as timers. Completion wakes already make reactive workers
event-driven. To wait for an external process, poll it from the shell at a
task boundary instead of parking a model worker on it. Recorded in the
operator's runbook; no Synaps change. Fix 1 means a long-lived worker can no
longer starve rollover, whoever starts it.

## Immediate workaround (before the fixes ship)

In an affected session, have the model run `subagent_collect` with
`reconciled=true` on each cancelled or finished worker before the context
reaches the hard limit. If the session is already refusing requests, `/compact`
recovers it, but compaction and rollover do not touch the worker registry: the
uncollected workers carry over to the compacted session and will block its
next rollover too.

## Verification

- Unit tests for every new function: grace length, hard-stop and time-checkpoint
  behaviour, grace reset on commit, reminder repetition, `pending_workers`
  coverage and ordering, manifest bounds and neutralisation, envelope framing,
  turn-scoped cancel, generation-0 fallback, cancelled-worker retirement and
  idempotence, `/workers` status, cancel and reconcile.
- Existing suites: `synaps-engine` lib, `session_actor*`, `subagent_host_reuse`,
  `worker_runtime_isolation`, `synaps --test session_actor_differential`.
- Not covered: there is no end-to-end harness that drives a stream into
  rollover with live workers. The stream wiring is thin and calls the tested
  functions; a harness would be the next thing to add.
