# SynapsCLI Dev Branch Review — S184

**Date:** 2026-05-02
**Reviewers:** Zero (Architecture), Shady (Code Quality), Silverhand (Security), Chrollo (API/UX)
**Scope:** 44 commits, ~5,152 LOC, `main..dev`

---

## 🔴 Must Fix

### 1. `disabled_plugins` doesn't disable plugin manifest commands/keybinds/help (HIGH)
- **Found by:** Silverhand, Chrollo, Zero
- **Location:** `src/skills/mod.rs:70-99`, `src/skills/registry.rs:87-95`
- `filter_disabled()` only filters `Vec<LoadedSkill>`. `Vec<Plugin>` passes unfiltered to `CommandRegistry`, meaning disabled plugins still register slash commands, keybinds, and help entries.
- Extensions were fixed (manager.rs:664-668) but the skill/plugin-manifest path was not.
- **Fix:** Filter plugins by `disabled_plugins` in `skills/mod.rs` before passing to `CommandRegistry`.

### 2. Codex autonomous policy sentinel is prompt-injectable (MEDIUM)
- **Found by:** Silverhand
- **Location:** `src/runtime/openai/stream.rs:272-280`
- `codex_instructions()` checks if `[Synaps autonomous harness policy]` exists in the system prompt to avoid double-injection. A malicious AGENTS.md or plugin can embed this string to skip the real policy entirely.
- **Fix:** Use a structured bool (`autonomous_policy_applied`) tracked outside the prompt, not string matching.

---

## 🟡 Should Fix

### 3. `agent_name` naming collision
- **Found by:** Chrollo
- Overloaded across 4 contexts: config label, SubagentHandle.agent_name, params["agent"], AgentEvent. Rename to `agent_label` or `assistant_label`.

### 4. `agent_name` rendered without terminal escape sanitization (LOW)
- **Found by:** Silverhand
- **Location:** `src/core/config.rs:286`, `src/chatui/render.rs:156`
- Config values go to terminal verbatim. Strip C0/C1 control chars on parse.

### 5. Plugin help entries can embed terminal escapes (LOW)
- **Found by:** Silverhand
- **Location:** `src/skills/registry.rs:87-91`
- Plugin-supplied title/summary/lines flow to terminal unsanitized.

### 6. `HelpRegistry` rebuilt per keystroke (PERF)
- **Found by:** Chrollo, Zero
- Built from scratch at 4 call sites. `builtin_entries()` re-parses JSON every time. Cache in `App`, invalidate on `ReloadPlugins`.

### 7. `HelpFindState::filtered_rows()` called 5-7x per keystroke (PERF)
- **Found by:** Shady
- O(n² log n) category sort with zero memoization. Cache by `(filter, recently_opened)`.

### 8. `agent_name` not wired through `/settings` (BUG)
- **Found by:** Zero, Chrollo
- Read once in `App::new()`. `/set agent_name foo` does nothing until restart.

---

## 🟢 What's Good

### tool_id plumb-through (BEST CHANGE ON BRANCH)
- `LlmEvent::ToolUseStart/Delta/Use/Result` all carry `tool_id`
- Fixes parallel tool call misrouting (Codex `parallel_tool_calls: true`)
- Per-tool elapsed timers, correct delta routing, `output_item.done` finalize
- Excellent regression tests describing the bug, not the implementation

### help.rs module extraction
- Pure data + render layer, no I/O, no UI deps
- Tested in isolation (528 LOC tests)
- Protected namespace enforcement, ranked search, plugin extensibility

### viewport.rs
- Self-contained edge-scrub primitive
- Well-documented (explains the tmux scroll-region bug)
- Pure functions where possible

### disabled_plugins fix for extensions
- Correctly touches three layers: config.rs, skills/config.rs, extensions/manager.rs
- Single source of truth

---

## Architectural Notes

### God-struct: `chatui/app.rs`
- 1,317 LOC, 112 methods
- State + behavior + persistence + parsing all in one struct
- Recommendation: Extract `ToolBlockTracker`, cache `HelpRegistry`, break into sub-structs

### God-function: `chatui/mod.rs::run()`
- 1,491 lines in one function
- Every new overlay requires edits at 3 locations
- Recommendation: Extract `EventLoop` struct with `handle_input_action`, `handle_streaming_input_action`, `handle_command_action`

### `help.rs` render roundtrip waste
- Structured `HelpEntry` → stringify → `ChatMessage::System` → `wrap_text` re-wraps
- Should hand `HelpEntry` directly to renderer

### `CODEX_AUTONOMOUS_LOOP_POLICY` in wrong layer
- Provider-specific behavior modifier lives in `stream.rs` (wire layer)
- Should be a `Runtime`-level system-prompt fragment registry
