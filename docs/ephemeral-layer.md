# Request-only ephemeral prompt layer (protocol 3)

`Welcome.features` advertises `ephemeral_layer_v1`. One unnamed layer belongs to
one session, in RAM only. It is not an ordinary system-prompt setting.

Create with `SessionConfig.ephemeral_layer_wait_ms` (0–15000, absent disables
waiting) to establish the barrier before automatic first requests. This
non-content policy survives host reload/crash recovery. It is not inherited by
forks or workers. Existing sessions can arm the barrier using the command below.

Commands use the existing session command envelope:

```json
{"cmd":"ephemeral_layer","id":1,"epoch":"host-issued-uuid","generation":1,"op":"replace","text":"request-only context"}
{"cmd":"ephemeral_layer","id":2,"epoch":"host-issued-uuid","generation":2,"op":"clear"}
{"cmd":"ephemeral_layer","id":3,"epoch":"host-issued-uuid","generation":3,"op":"arm","supply_wait_ms":15000}
{"cmd":"query","id":4,"query":{"query":"ephemeral_layer"}}
```

Only the input owner may mutate. Observers can query metadata. Query on attachment
and reconnection to obtain the current epoch. Generation is strictly increasing
within that epoch. Text is capped at 262144 UTF-8 bytes including caller framing;
empty replacement is refused (use clear), and oversize input is never truncated.

The existing `QueryResult` carries a value containing `kind: ephemeral_layer`,
`session_id`, `epoch`, `generation`, `sequence`, `in_flight`, `state`, `reason`, and
for mutations `outcome`. Neither text nor a content digest is returned.

- State: `needs_supply`, `ready`, `cleared`, `missing`.
- Outcome: `applied`, `stale_epoch`, `stale_generation`, `too_large`, `empty`,
  `busy`, `not_owner`, `invalid_wait`.
- Reason: `installed`, `cleared`, `reload`, `rollover`, `disconnect`, `timeout`,
  `new_session`, `request_settled`, `armed`.

Opt into `ephemeral_layer_changed` in `Hello.accepted_events` for change notices:
`{ev: ephemeral_layer_changed, status: {epoch, generation, sequence, in_flight,
state, reason}}`, inside the existing session envelope. Notices are coalesced
latest state, not a durable history. Query after lag/reconnect. A release notice
is not a reservation: another request may start before a client retries.

Every conversational provider operation acquires an admission fence, held across
its retries. Mutation during that operation returns `busy` without falsely
acknowledging installation. `request_settled` signals release on success, error or
cancellation. A replacement acknowledged before admission is used by that request;
a clear acknowledged before admission excludes the layer. Already dispatched work
cannot be recalled. Changes cannot retroactively affect an existing request.

Installing-client disconnect, rollover, new/resumed conversation, and host
restart discard text and rotate epoch. A logical successor retains only the wait
policy. The next conversational request waits for supply for the configured
bounded interval, then reports `missing` and continues without the layer. Clear
releases the wait immediately. A late replacement applies to subsequent requests.
A terminal takeover does not grant a mirror client mutation authority.

Content is composed only at final provider admission, after the stored system
prompt. The client supplies lower-priority framing. It is excluded from ordinary
prompt getters, inspection, views, session JSON/journals/snapshots, replay/reload
state, forks, runtime clones, workers, summaries and request-content capture.
Transport serialization necessarily carries the inbound command; Debug redacts
it. Model-output echo prevention is outside this feature's scope.

In v1, opted-in sessions do not use background cache-warming pings or warm-summary
prefix recording. Regular conversation prompt caching continues. Summary requests
use the ordinary prompt/history, never this layer. Explicit request-content trace
capture is disabled for a request carrying ephemeral text.

Verification: `session_actor_ephemeral::ephemeral_layer_no_disk_sentinel` exercises
real request inclusion, view/history exclusion, linked-successor summary exclusion,
fork isolation, clear, reload-record serialization, fresh-runtime restoration and
a recursive artifact scan. Small unit tests cover admission, stale generations,
bounds, cancellation and Runtime clone isolation. No real credentials or provider
requests are used by these tests.
