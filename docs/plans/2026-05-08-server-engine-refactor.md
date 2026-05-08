# Migrate `synaps server` to `engine/`

**Status:** In progress
**Branch:** `refactor/server-uses-engine`
**Date:** 2026-05-08

## Objective

Replace `src/cmd/server.rs`'s direct `Runtime::new()` boot path and inline
`StreamEvent` matching loop with calls to `engine::setup::boot()` +
`engine::stream::process_stream_event()` + `engine::commands::handle_engine_command()`
+ `engine::session::ConversationState`. The WebSocket protocol surface
(`ServerMessage` / `ClientMessage`) stays unchanged — wire-compatible refactor.

## Why

`engine/mod.rs` doc explicitly states:

> "Renderers (chatui TUI, headless chat) call into the engine for all
> non-visual operations."

The WebSocket server is a renderer. Currently it bypasses the engine and
silently misses every feature the engine adds:

| Feature | TUI / `chat` | `synaps server` (today) |
|---------|--------------|-------------------------|
| `Runtime::new()` boot | engine | inline |
| System prompt resolution | engine | inline (duplicated) |
| Session resolution / continue | engine | inline (duplicated) |
| Skills + plugin registry | engine ✅ | ❌ missing |
| MCP lazy loading | engine ✅ | ❌ missing |
| Inbox watcher | engine ✅ | ❌ missing |
| Per-session Unix socket | engine ✅ | ❌ missing |
| Session-start index record | engine ✅ | ❌ missing |
| Extension manager + hook bus | engine ✅ | ❌ missing |
| `on_session_start` hook fire | engine ✅ | ❌ missing |
| Subagent events | engine ✅ | ❌ silently dropped (TODO comment in server.rs:380) |
| Message-history capture | engine | inline (duplicated) |
| Token / cost accounting | engine | inline (duplicated) |
| Compaction support | engine | ❌ missing |

Net: ~150 LOC of stream-event matching duplicated, with 5+ feature regressions
behind it.

## Success criteria

1. `cargo build --release` exits 0 with **0 new warnings**
2. `cargo clippy -- -D warnings` clean
3. `cargo test` passes — no regressions
4. Smoke test: `synaps server --port 5050` boots, `/health` → `ok`, WS connect
   → send `{"type":"Message","content":"hi"}` → stream `Thinking` / `Text` /
   `Done` events back
5. `ServerMessage` / `ClientMessage` wire format **unchanged**
6. `on_session_start` hook fires (verify via `SYNAPS_EXTENSIONS_TRACE=1` log)
7. Extensions load — confirm via `loaded plugins` log line
8. server.rs LOC drops: 544 → ~300-350 (target ~40% reduction)

## Constraints

- **Wire format frozen.** `ServerMessage` and `ClientMessage` enums in
  `synaps_cli::protocol` keep their exact serde shapes.
- **No new deps.** Reuse existing `engine::` surface.
- **No protocol additions** beyond what's already mapped (subagent events
  may need a new `ServerMessage` variant; that is the only wire addition
  in scope).

## Plan — phased

### Phase 1: boot via `engine::setup::boot`
Replace lines 80-135 (Runtime construction + session resolution + ServerState
init) with the `EngineBoot` pattern. ServerState consumes the boot artifacts.
**Removes ~50 LOC. Unlocks 6 feature regressions.**

### Phase 2: stream events via `engine::stream::process_stream_event`
Replace the 100-LOC `match event { StreamEvent::Llm(...) ... }` block in
`handle_user_message` with `process_stream_event(...) -> EngineStreamEvent`,
and a small translator `engine_event_to_server_message()` mapping
`EngineStreamEvent → ServerMessage`. **Removes ~80 LOC. Wires subagent events.**

### Phase 3: slash commands via `engine::commands::handle_engine_command`
Replace `handle_command` with engine command dispatch. Map `CommandResult`
variants to `ServerMessage` broadcasts. **Removes ~50 LOC.**

### Phase 4: cleanup + verification
Remove dead helpers (`rebuild_history` if duplicated by engine), run
`cargo fmt`, `cargo clippy`, `cargo test`, smoke-test live server.

## Tasks

| # | Task | Size | Files | Verification |
|---|------|------|-------|--------------|
| 1 | **Boot phase** — wire `engine::setup::boot` into `server::run` | M | `src/cmd/server.rs` | `cargo build`, `synaps server --port 5050` boots, `/health` → ok |
| 2 | **Stream phase** — replace stream loop with engine processor + translator | M | `src/cmd/server.rs` | `cargo build`, smoke send a message, see streaming response |
| 3 | **Commands phase** — engine command dispatch | S | `src/cmd/server.rs` | `cargo build`, `Status` command returns expected JSON |
| 4 | **Cleanup + verify** — fmt, clippy, tests, docs | XS | `src/cmd/server.rs` | `cargo clippy -D warnings`, `cargo test` 0 fail |

## Risks + mitigations

| Risk | Mitigation |
|------|------------|
| `ServerState` ownership of `Runtime` (engine returns by value, current code wraps in `Mutex`) | Keep `Mutex<Runtime>` wrapping; just consume from `EngineBoot` instead of constructing |
| `display_history` rebuild for late-connecting clients | Server keeps `display_history` ownership; engine doesn't replace this. We sync display_history in the translator. |
| `on_session_start` hook now fires on server boot — behavior change | Desired. Document in commit. |
| `EngineBoot::background` (BackgroundTasks) lifetime — Aborts on drop | Stash in ServerState so it lives for server lifetime |
| `Subagent`/`Steering` events were silently dropped — now they go through `engine::stream` and we have to decide what to broadcast | Add minimal `ServerMessage::Subagent { id, status }` variant OR drop them at the translator (preserve current behavior). v1 = drop with TODO. |

## Out of scope (defer)

- New WebSocket protocol additions beyond subagent variant
- Multi-session per server (still 1 session per server instance — separate task)
- Compaction triggers from server (engine has it; server doesn't expose it yet)
- HTTP/SSE alternative transport
- Auth / TLS / multi-user

## Wire-format addition (the one new variant)

```rust
// In src/protocol.rs — additive, doesn't break existing clients
pub enum ServerMessage {
    // ... existing variants ...
    /// NEW: subagent lifecycle event.
    Subagent { id: u64, name: String, status: String, done: bool },
}
```

If we don't want to add this variant in v1, the translator silently drops
subagent events (same as today). Boss decides at phase 2 review.
