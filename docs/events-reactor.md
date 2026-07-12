# Runtime Event Reactor — Semantics & Mode Policy

Central reference for how runtime events flow through the agent-runtime, from
ingestion to model injection, across all operating modes.

---

## Canonical Event Format

All events are formatted exactly once via `format_event_for_agent` in
`crates/agent-engine/src/events/format.rs`. Output is an XML-tagged string:

```
<event id="{uuid}" type="{content_type}" severity="{severity}" source="{source}"{channel?}>{text}</event>
```

This string is the **single canonical representation** used everywhere:
- Injected into `api_messages` as a `role=user` message (idle path)
- Stored in `pending_events` as a raw string (busy path)
- Carried in `DrainedEvent.formatted`
- Stored in `EventPayload.formatted` on wire frames

**Invariant**: the formatted string produced for a given `Event` is identical
regardless of which mode (chat, RPC, server) drains it.

---

## Mode Policy Table

| Mode | Idle (not streaming) | Busy (streaming active) | Auto-turn | Wire frame emitted? |
|------|---------------------|------------------------|-----------|---------------------|
| **chat** | Inject into `api_messages` + continue turn | Steer via `steer_tx`; buffer if channel dead | N/A (turn-based) | No |
| **RPC** | Inject into `api_messages` + emit `RpcEvent::Event` frame | Buffer in `pending_events` + emit `RpcEvent::Event` frame | **Never** (client decides) | Yes — drainer only, exactly once |
| **server** | Inject into `api_messages` + broadcast `ServerMessage::Event` | Buffer in `pending_events` + broadcast `ServerMessage::Event` | Gated on `events.auto_turn` (default **false**) | Yes — drainer only |

---

## Exactly-One-Frame / Drain-All Invariant

1. **Exactly one drainer per Runtime.** A single background task owns
   `EventQueue::notified()` and calls `drain_event_queue`. No other code
   may drain the queue.

2. **Drain-all on each wake.** `drain_event_queue` pops every item from
   the priority queue in a single call — it never returns after partial
   processing. This prevents starvation of lower-priority events.

3. **Exactly one wire frame per event.** In RPC and server modes the
   drainer emits one `Event` frame per `DrainedEvent` immediately after
   draining, regardless of whether the event was `Injected`, `Steered`,
   or `Buffered`. The `Done` flush path **does not** emit additional
   frames — it only injects the canonical formatted string into
   `api_messages` so the model sees the event on the next turn.

---

## Done Flush (RPC Buffered Path)

When a streaming turn completes (`SessionEvent::Done`) and `pending_events`
is non-empty:

1. Lock `RpcState` briefly.
2. `std::mem::take(&mut st.pending_events)` — drain the buffer.
3. For each `formatted` string: push `{"role":"user","content":formatted}`
   into `st.api_messages`.
4. Release lock.
5. **Do NOT emit another `RpcEvent::Event` frame.** The drainer already
   emitted the frame when the event was buffered (step 3 of the drainer
   loop). Emitting again would produce wire duplication with fake metadata
   (synthetic UUID, `source="buffered"`).

---

## `events.auto_turn`

```toml
# ~/.config/synaps/config.toml
events.auto_turn = false   # default — clients must send a follow-up
# events.auto_turn = true  # server triggers model turns on idle events
```

Default: **false**. When false, the server broadcasts `ServerMessage::Event`
to all connected clients but does not spawn a new model turn. Clients are
expected to send a follow-up `Message` if they want the model to react.

When true, the server calls `run_injected_event_turn` (NOT `handle_user_message`)
which:
- Atomically acquires the streaming guard; drops the trigger if already streaming.
- Does **not** inject a sentinel `[event-reactor auto-turn]` user message.
- Runs the turn loop against the existing `api_messages` (event already there).
- Enforces `AUTO_TURN_CAP`.

---

## AUTO_TURN_CAP

```rust
pub const AUTO_TURN_CAP: u32 = 5;  // reactor.rs
```

Maximum consecutive event-triggered model turns without an intervening real
client user message. When `consecutive_auto_turns` reaches the cap:

- `wake_action` returns `WakeAction::Forward` (not `RunTurn`).
- The server's `run_injected_event_turn` bails and resets the counter to 0.
- Resumes when a real client `Message` arrives (counter reset to 0).

This prevents runaway API spending from high-frequency event sources.

---

## EventDisposition

| Variant | When | Effect |
|---------|------|--------|
| `Injected` | Idle (not streaming) | `api_messages.push(role=user, content=formatted)` |
| `Steered` | Busy + live `steer_tx` | Sent via channel into active stream's steering path |
| `Buffered` | Busy + no live `steer_tx` | `pending_events.push(formatted)` |

`wake_action` only returns `RunTurn` when at least one `Injected` event is
present, the server is idle, `auto_turn_enabled` is true, `consecutive_auto_turns < AUTO_TURN_CAP`,
and the last `api_messages` entry is `role=user`.

---

## Follow-up Items (out of scope for C6)

- **RPC save lock** — `save_session` acquires a write lock on `RpcState`
  which can contend with the streaming task on `MessageHistory`. Should
  release the full lock before disk I/O (see `server.rs` pattern).
- **Shutdown timeout** — no graceful drain timeout in RPC mode; SIGINT
  may lose the final `save_session`. Mirror server's two-budget teardown.
- **Agent monitor leak** — `SubagentTracker` tasks created by `run_injected_event_turn`
  are never reaped if the function returns early at cap. Track and abort.
