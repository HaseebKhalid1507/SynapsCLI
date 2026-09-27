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
| **RPC** | Inject + emit one `RpcEvent::Event` observability frame | Buffer + emit one frame; terminal flush injects | Owning session auto-turns (default on, opt-out) | Yes — drainer only, exactly once |
| **server** | Inject into the one shared owning conversation; no raw event frame | Buffer for the shared conversation; no raw event frame | Owning conversation auto-turns (default on, opt-out) | Yes — drainer only |

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
events.auto_turn = true    # default — owning session continues automatically
# events.auto_turn = false # opt-out: events injected, no model turn spawned
```

Default: **true** (RPC and server modes). When false:

* **RPC mode:** The event is injected into the owning session's `api_messages`
  as a `role=user` message. An `RpcEvent::Event` observability frame is still
  emitted by the drainer. No model turn is spawned automatically. The event is
  processed on the **next client-initiated `Prompt` or `FollowUp`** — the model
  will see the event in its conversation history and can respond to it then.
  No additional raw wire frame is emitted; the drainer already emitted one.

* **Server mode:** The event is injected into the owning conversation's
  `api_messages`. No raw `ServerMessage::Event` broadcast is sent to other
  connected clients (the broadcast was removed; the single owning conversation
  receives and holds the event). No model turn is spawned automatically.
  The event is processed when the next client `Message` arrives.

In both modes the event is **never silently dropped** when `auto_turn = false`:
it lands in `api_messages` so the model will see it on the next real turn.

When `auto_turn = true`, the server calls `run_injected_event_turn` (NOT `handle_user_message`)
which:
- Atomically acquires the streaming guard; drops the trigger if already streaming.
- Does **not** inject a sentinel `[event-reactor auto-turn]` user message.
- Runs the turn loop against the existing `api_messages` (event already there).
- Enforces the auto-turn cap (`events.auto_turn_cap`).

---

## `events.auto_turn_cap` (AUTO_TURN_CAP)

```toml
# ~/.config/synaps/config.toml
events.auto_turn_cap = 5          # default — park after 5 consecutive auto turns
# events.auto_turn_cap = 12       # allow longer autonomous chains
# events.auto_turn_cap = 0        # unlimited — "to infinity and beyond"
# events.auto_turn_cap = unlimited  # alias for 0 (also: inf, infinite)
```

```rust
pub const AUTO_TURN_CAP: u32 = 5;  // reactor.rs — the default only
```

Maximum consecutive event-triggered model turns without an intervening real
client user message. Default **5**. **`0` means unlimited** — the engine never
parks on its own (an agent that wants to keep going, keeps going). Unparseable
values print a warning and keep the default.

Why it is configurable: a subagent-completion event that lands after the cap has
tripped sits frozen until a human types something — e.g. finished work arriving
at 08:45 and nobody noticing until 13:07. Operators running long autonomous
sessions can raise or remove the cap.

All four reactor loops (TUI, `chat`, `rpc`, `server`) read the same value at
boot and gate through `reactor::auto_turn_cap_reached(consecutive, cap)` /
`claim_auto_turn_with_cap`. When a finite cap trips, the TUI/chat notice names
the cap and the config key to raise it.

When `consecutive_auto_turns` reaches a finite cap:

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
present, the server is idle, `auto_turn_enabled` is true, the configured cap
(`events.auto_turn_cap`) is unlimited or not yet reached,
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
