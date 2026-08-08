# TUI Engine Refactor — Specification

> **Note:** This spec is historical. The refactor is complete as of v0.1.5.
> `chatui/` has been deleted — `tui/` is the sole frontend. The `SYNAPS_TUI`
> env var and legacy fallback described below no longer exist.

**Branch:** `feat/headless-engine` (merged)  
**Approach:** Copied `src/chatui/` → `src/tui/`, refactored the copy to use engine modules. `chatui/` was removed after migration.

---

## Strategy

```
src/chatui/  ← FROZEN. Battle-tested. Current default.
src/tui/     ← COPY. Incrementally refactored to use engine.
src/engine/  ← SHARED. Used by both tui/ and cmd/chat.rs.
```

When `tui/` reaches parity and stability, swap the default in `main.rs`:
```rust
// Before:  chatui::run(...)
// After:   tui::run(...)
```

One flag to switch back if anything breaks: `synaps --legacy` → chatui.

---

## Phase 1: Mechanical rename (no logic changes)

Replace all 80 occurrences of `crate::chatui` with `crate::tui` inside `src/tui/`:
```bash
find src/tui -name "*.rs" -exec sed -i 's/crate::chatui/crate::tui/g' {} +
```

Register `mod tui;` in `main.rs` (not lib.rs — it's a binary module like chatui).

Verify: `cargo check` passes, `src/tui/` compiles independently.

**Risk:** Zero — chatui untouched, tui not wired to anything yet.

---

## Phase 2: Wire tui to main.rs behind a flag

Add `--engine` flag or `SYNAPS_TUI=engine` env var:
```rust
// main.rs
None => {
    if std::env::var("SYNAPS_TUI").as_deref() == Ok("engine") {
        tui::run(...).await?;
    } else {
        chatui::run(...).await?;
    }
}
```

Both TUIs coexist. Test by running `SYNAPS_TUI=engine synaps`.

**Risk:** Zero — default is still chatui.

---

## Phase 3: Replace boot sequence

In `src/tui/mod.rs`, replace lines 46-215 (inline boot) with:
```rust
let boot = engine::setup::boot(EngineOpts { ... }).await?;
let mut runtime = boot.runtime;
// Unpack boot into App fields...
```

Map `EngineBoot` fields → `App` fields. The App struct stays the same,
just populated from engine boot instead of inline code.

**Lines saved:** ~150  
**Risk:** Low — same data, different source.

---

## Phase 4: Replace session management

Make `App` own a `ConversationState` from `engine::session`:
```rust
pub struct App {
    pub conv: ConversationState,  // replaces api_messages, total_input_tokens, etc.
    // ... keep all TUI-specific fields
}
```

Replace all `app.api_messages` → `app.conv.api_messages`, etc.
Replace `app.save_session()` → `app.conv.save()`.
Replace `app.add_usage(...)` → `app.conv.add_usage(...)`.

**Fields removed from App:**
- `api_messages` → `conv.api_messages`
- `total_input_tokens` → `conv.total_input_tokens`
- `total_output_tokens` → `conv.total_output_tokens`
- `total_cache_read_tokens` → `conv.total_cache_read_tokens`
- `total_cache_creation_tokens` → `conv.total_cache_creation_tokens`
- `session_cost` → `conv.session_cost`
- `abort_context` → `conv.abort_context`
- `queued_message` → `conv.queued_message`

**Lines saved:** ~50 (save_session, add_usage, clear logic)  
**Risk:** Medium — many files reference these fields. Mechanical but tedious.

---

## Phase 5: Replace stream handler

In `src/tui/stream_handler.rs`, call `engine::stream::process_stream_event()`
then translate `EngineStreamEvent` → App mutations:

```rust
pub(super) async fn handle_stream_event(event: StreamEvent, app: &mut App, runtime: &Runtime) -> StreamAction {
    let (engine_event, completion) = engine::stream::process_stream_event(
        event,
        &mut app.conv.api_messages,
        &mut app.subagents_engine,  // engine SubagentTracker
        &mut app.conv.queued_message,
        &mut app.conv.pending_events,
    );
    
    // Translate to TUI — update App display state
    match engine_event {
        EngineStreamEvent::Text(text) => app.append_or_update_text(&text),
        EngineStreamEvent::ToolStart { tool_id, tool_name } => app.on_tool_use_start(tool_id, tool_name),
        // ... etc
    }
    
    // Translate completion to StreamAction
    match completion {
        StreamCompletion::Done => StreamAction::Continue, // StreamAction::None equivalent
        StreamCompletion::AutoSendQueued(q) => StreamAction::AutoSendQueued(q),
        // ...
    }
}
```

**Lines saved:** ~60 (logic moved to engine, tui keeps display mapping)  
**Risk:** Medium — stream handling is timing-sensitive.

---

## Phase 6: Route engine commands first

In `src/tui/commands.rs`, add engine command routing before TUI commands:

```rust
pub(super) async fn handle_command(cmd, arg, app, runtime, ...) -> CommandAction {
    // Try engine-level command first
    if let Some(result) = engine::commands::handle_engine_command(cmd, arg, runtime) {
        return match result {
            CommandResult::Quit => CommandAction::Quit,
            CommandResult::ModelChanged { model } => {
                app.push_msg(ChatMessage::System(format!("model → {}", model)));
                CommandAction::None
            }
            // ... translate CommandResult → CommandAction
        };
    }
    
    // Fall through to TUI-specific commands
    match cmd { ... }
}
```

**Lines saved:** ~20 (model/thinking/quit logic shared)  
**Risk:** Low — engine handles simple commands, TUI handles the rest.

---

## What does NOT change in tui/

These stay as-is because they're purely TUI:
- `draw.rs` (1,123 lines) — ratatui rendering
- `input.rs` (667 lines) — keyboard/mouse handling  
- `render.rs` (633 lines) — message rendering
- `markdown.rs` (1,014 lines) — markdown → ratatui spans
- `theme/` (1,233 lines) — 18 themes
- `settings/` (2,016 lines) — settings modal
- `plugins/` (3,683 lines) — plugin browser modal
- `models/` (1,238 lines) — model picker modal
- `help_find.rs` (280 lines) — help lightbox
- `toast.rs` (196 lines) — toast notifications
- `gamba.rs` (106 lines) — 🎰

---

## Execution order

| Step | What | Risk | Lines saved |
|------|------|------|-------------|
| 1 | Mechanical rename `chatui` → `tui` | Zero | 0 |
| 2 | Wire to main.rs behind env flag | Zero | 0 |
| 3 | Replace boot with engine::setup | Low | ~150 |
| 4 | Replace session with engine::session | Medium | ~50 |
| 5 | Replace stream handler with engine::stream | Medium | ~60 |
| 6 | Route engine commands first | Low | ~20 |

**Total lines saved:** ~280 out of 19,529 (1.4%)  
**Total lines of shared code with headless:** ~750 (engine modules)  
**Real value:** Bug fixes in engine automatically fix both TUI and headless.

---

## Success criteria

- [ ] `SYNAPS_TUI=engine synaps` boots and works identically to `synaps`
- [ ] All slash commands work
- [ ] Extensions load and hooks fire
- [ ] Session save/load/resume works
- [ ] Compaction works
- [ ] Subagent panel works
- [ ] No regressions in `synaps` (still uses chatui)
- [ ] Can switch default from chatui → tui with one line change
