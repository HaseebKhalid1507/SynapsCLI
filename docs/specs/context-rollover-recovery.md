# Unproductive context rollover recovery

## Incident

A wall-clock rollover may preserve nearly the whole active head. A subsequent
small `context_checkpoint(new_task)` at soft pressure requested another rollover.
The candidate saved fewer than 1,024 estimated tokens, producing the same fatal
config error as an over-capacity request. The autonomous driver correctly refused
to retry a config error, unnecessarily ending otherwise safe work.

## Contract

- Separate a typed **unproductive rollover** from actual inability to fit.
- At a soft boundary, if the current full request passes the existing hard
  admission check, continue unchanged after an unproductive preparation. Do not
  archive, publish a head, advance the window or reset elapsed/cumulative budgets.
- Rate-limit further soft rollover attempts: retry after four admitted rounds or
  a change of at least 8,192 estimated input tokens. Check hard capacity before
  this cooldown on every round. Phase reports and mode toggles do not replenish
  bounded-finish allowances or the cooldown. A real committed rollover resets it.
- Hard-capacity, cancellation, missing retrieval, archive failures, unresolved
  durable heads and time-boundary save requirements remain blocking. No error
  string matching/fallback-model workaround; only the typed no-reduction result
  is recoverable.
- Candidate budgets include all request overhead and the greater of configured
  minimum reserve and computed next-round reserves (including advisory overhead).
- Retain human turns, attachments, constraints and protocol-complete tool tails
  verbatim. No text-based detection/deletion of historical autonomous prompts.
  Host prompt provenance is a separate storage contract and is not needed to
  recover this failure safely; legacy unmarked messages stay preserved.

## Verification

Offline tests: policy cooldown/phase repeats/mode toggles/hard caps/reset; typed
no-op leaves history, note, window and archives untouched; oversized candidate
fails with precise diagnostics; real runtime with pressure, repeated checkpoint
tools and pinned history continues without head publication and still honors
provider-round limits. A shrinking successor still archives/acknowledges before
inference. Cargo jobs <=8, test threads 1. No installation, live inference,
configuration changes, history migration or live memory mutation.
