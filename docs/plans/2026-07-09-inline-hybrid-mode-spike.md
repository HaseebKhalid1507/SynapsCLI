# SynapsCLI Inline/Hybrid Rendering Mode — Design Spike

**Date:** 2026-07-13
**Author:** Chrollo (subagent, deep analysis & reconnaissance)
**Scope:** P20 — research only, no code, no repo touch
**Inputs verified against:** `~/Projects/agent-runtime/crates/agent-tui/src/tui/` @ dev, field-report-s235.md (exec), tui-field-report-DEEP-synaps-strategy.md (Yoru), synaps-tui-modernization-plan.md (Zero)

---

## Finalization note (2026-07-09, ported into repo — P20.1)

This spike (authored by Chrollo against an earlier tree) is finalized and ported here per
P20's "Done" criteria. Line refs in the body are re-verified against **`dev` @ `2ca6ae2`**
below; the body prose may be ±1–2 lines off in spots due to post-draft code drift, but the
architectural conclusions hold.

**Re-verified 2026-07-09:**
- `EnterAlternateScreen` — `lifecycle.rs:29` (single setup-step call).
- `LeaveAlternateScreen` — `lifecycle.rs:68` (in `emergency_teardown_terminal()`).
- `RenderModel` struct — `render_model.rs` (ends `:96`); **no alt-screen assumptions** — confirmed mode-agnostic.
- Edge-scrub call — `draw.rs:720` (`scrub_crossterm_terminal_edges`, moved from the draft's :797).

**Verdict stands:** the alt-screen commitment is localized to `setup_terminal()`,
`emergency_teardown_terminal()`, and the edge-scrub. `RenderModel` + render thread + input
routing are mode-agnostic. **1.0 does not preclude inline mode in 1.x**, gated on P9
(TranscriptStore — already landed on dev) so the line cache is separable from full history.

**T211 pointer:** the 1.0 milestone task (T211) should reference this doc as the recorded
rationale for deferring inline/hybrid rendering to 1.x (implementation deferred; the "1.0
doesn't preclude it" assertion is verified in §7 below).

---

## 1. What "Inline Mode" Would Mean for SynapsCLI

A precise definition first, because the term is overloaded.

**Inline mode** does not mean "fullscreen but smaller." It does not mean "alt-screen with fewer rows." It means:

> The application renders on the **primary screen buffer** — the same buffer the user's shell prompt, `ls` output, and prior commands live in. Completed content scrolls upward into native terminal scrollback and becomes part of the permanent session transcript. The user's Cmd-F, native scroll, terminal selection, and copy operate on this content exactly as they do on any other terminal output. The application owns only a **bounded live region** at the bottom of the visible viewport — a fixed-height window containing the input box and the currently-streaming response — which it redraws in place without disturbing scrollback above it.

For SynapsCLI specifically, this would mean:

- **User messages** and **completed assistant responses** (including rendered markdown, code blocks, tool results) are written to the primary screen and scroll into native terminal history. They become native scrollback — searchable, selectable, copyable by the terminal itself.
- **The current streaming response** and **the input box** occupy a bounded region (say, the bottom N rows of the visible terminal) that the application redraws. During streaming, this region grows as content arrives, and when the response completes, the entire response is "committed" — written to the primary screen as permanent scrollback — and the live region resets to just the input box.
- **Modals** (settings, plugins, models, help) either (a) switch to alt-screen temporarily for their duration, or (b) render as inline overlays if their scope is small enough.

The key invariant: **the terminal, not the application, owns scrollback.** The application never needs to track scroll position for committed history. The terminal handles scroll physics, selection, copy, Cmd-F, and reflow on resize (within its own capabilities). The application only manages what is currently live.

This is the model Ink's `<Static>` component implements, and it is the model the field report identifies as the #5 ranked demand — specifically for agent CLIs, where sessions generate long transcripts that users want to search and reference.

---

## 2. Ink's `<Static>` Pattern — The Actual Mechanic

Ink (the React-for-terminals library) separates rendering into two regions:

### The `<Static>` component

`<Static>` accepts an `items` array and a render function. Each item in the array is rendered **once** — when it first appears — and the rendered output is written to stdout as permanent terminal output. It scrolls upward as new content arrives. Ink never redraws it.

```jsx
<Static items={completedMessages}>
  {(message) => <Text key={message.id}>{message.content}</Text>}
</Static>
```

The mechanic:
1. On each render cycle, Ink compares the current `items` array against the previous one.
2. New items (appended to the end) are rendered and written to stdout via raw terminal output — `process.stdout.write()`. They become part of native scrollback.
3. These items are then **excluded from Ink's managed viewport**. Ink does not track them, does not redraw them, does not diff them.

### The live region

Everything rendered *outside* `<Static>` lives in Ink's managed region — a bounded area at the bottom of the viewport that Ink redraws on each cycle using cursor-up + erase-line sequences. This is the "bounded live region" — it contains whatever is currently active (input, streaming output, spinners, progress bars).

### The seam between them

The critical contract: **items in `<Static>` must be append-only.** You cannot modify or remove a committed static item. Ink renders it once and forgets it. If you need to update an item, it must stay in the live region until it's finalized.

This maps directly to a chat transcript:
- Completed messages → `<Static>` (committed, append-only, native scrollback)
- Currently streaming response + input box → live region (bounded, redrawn)

### Ink's `<Static>` failure mode

Ink's renderer has a well-documented failure when the live region exceeds the terminal height: issue [#359](https://github.com/vadimdemedes/ink/issues/359), open since 2020 — content taller than the viewport "flickers badly on updates," structurally unfixable because Ink's `eraseLines` can only erase what is still on screen. This is why Claude Code ultimately shipped `CLAUDE_CODE_NO_FLICKER=1` — the alt-screen buffer — after a year of custom renderer work. The `<Static>` *model* is correct; Ink's *implementation* of the live-region redraw is the fragile part.

The lesson for us: the `<Static>` split (committed history vs. bounded live region) is the right architectural primitive. The implementation of the live-region redraw — specifically, ensuring it never exceeds what the terminal can erase-and-repaint without flicker — is where the engineering difficulty lives.

---

## 3. The tui2 Retirement — The Receipt

Codex CLI's `tui2` is the era's sharpest negative result for viewport ownership, and the receipt is specific.

### What tui2 did

tui2 rearchitected Codex CLI's viewport so the **application, not the terminal, owned scrollback, selection, and copy.** The in-memory transcript was the single source of truth: a flat `Vec<Arc<dyn HistoryCell>>` where each cell stored width-agnostic content and wrapped at render time, addressed by stable cell index.

### What it won

- **Resize-rewrap:** correct. Content stored width-agnostic, wrapped at render time.
- **Copy fidelity:** excellent. Selection lived in content-relative coordinates, copy reconstructed off-screen ranges, joined soft-wrapped prose, emitted *markdown source*.

### Why it was retired

The retirement commit ([#9640](https://github.com/openai/codex/commit/a489b64cb5)) states: *"making that experience feel fully native across the environment matrix (terminal emulator, OS, input modality, multiplexer, font/theme, alt-screen behavior) creates a combinatorial explosion of edge cases."*

Issue [#8344](https://github.com/openai/codex/issues/8344) — "Don't mess with the native TUI" — is the user verdict: *"Terminal is king because anything works anywhere. Don't break scrolling, copy/paste, for crying out loud."*

### The compatibility matrix explosion — specifics

The 13,734-scroll-event study across 8 terminals quantified the cost of owning scroll:

| Terminal | Raw events per physical wheel notch |
|----------|-------------------------------------|
| Apple Terminal | 3 |
| Warp | 9 |
| WezTerm | 1 |
| iTerm2 | 3 |
| kitty | 3 |
| Alacritty | 3 |
| VS Code terminal | variable |
| tmux (passthrough) | varies by underlying terminal |

Central finding: **timing cannot distinguish a mouse wheel from a trackpad.** The wheel-vs-trackpad heuristic is best-effort at best.

Owning the viewport means reimplementing:
1. **Per-terminal scroll normalization tables** — events-per-tick varies 1× to 9×
2. **Stream grouping** — batching rapid events into logical scroll actions
3. **Wheel-vs-trackpad heuristic** — fundamentally unsolvable without user input
4. **User override configs for input semantics** — mandatory because the heuristic fails
5. **Multiplexer detection matrix** — tmux/screen change the raw event semantics
6. **Selection geometry** — mapping screen coordinates to content coordinates across scroll
7. **Copy fidelity** — reconstructing source text from rendered content

Each of these is a dedicated engineering effort with a per-terminal-per-OS-per-multiplexer testing surface. tui2 built all seven. Then retired them.

---

## 4. Where SynapsCLI's Current Architecture Sits on the Spectrum

I verified the render pipeline against the actual code. The question: does anything assume alt-screen, or is the assumption localized?

### Files verified

- `lifecycle.rs` — terminal setup and teardown
- `render_model.rs` — the per-frame snapshot
- `render_thread.rs` — the dedicated render thread
- `draw.rs` — `build_render_model()` + `render_frame()`
- `viewport.rs` — edge-scrub hack
- `input.rs` — event routing / focus chain

### `EnterAlternateScreen` — localized, not pervasive

**`lifecycle.rs:28`** — `EnterAlternateScreen` is executed in `setup_terminal()` as one of four setup steps (raw mode, alt-screen, mouse capture, bracketed paste). The reverse (`LeaveAlternateScreen`) is in `emergency_teardown_terminal()` at line 67.

This is the **only place** in the core render pipeline where alt-screen is entered. It is a single call in a setup function, not a structural assumption. Replacing it with a no-op (or a conditional) would not cascade.

**`gamba.rs:38,61`** — The casino subprocess handoff also enters/leaves alt-screen, but this is a separate concern (terminal ownership transfer to a child process).

### `RenderModel` — mode-agnostic

The `RenderModel` struct (`render_model.rs:18-96`) contains **zero alt-screen assumptions.** It is a bag of data:
- `lines: Arc<[Line]>` — pre-rendered visible window
- `scroll_back: u16` — offset
- Modal state snapshots (settings, plugins, models, help_find)
- Input box state, footer stats, toasts, subagent snapshots

Nothing in `RenderModel` says "I am being drawn on an alternate screen." It is a complete, owned, `Send`-safe snapshot — proven by the invariant comment at line 7: *"If `render_frame` compiles without an `&App` parameter, the snapshot is proven complete."*

A second render mode that consumed a `RenderModel` and drew it inline instead of fullscreen would work without changing the snapshot.

### `render_frame()` — assumes full-screen layout, but structurally

`render_frame()` (`draw.rs:783+`) calls `terminal.draw(|frame| { ... })` inside a ratatui `Terminal::draw` closure. The closure lays out the entire frame area: header, body (messages), subagent panel, input box, footer. This is a full-screen layout — it assumes `frame.area()` is the entire terminal.

However, this is **structural, not contractual.** The function takes a `&mut Terminal<CrosstermBackend<Stdout>>` and a `&RenderModel`. An inline-mode renderer would be a different function — say `render_inline_frame()` — that consumed the same `RenderModel` but only drew the live region, and wrote committed content directly to stdout. The existing `render_frame()` would continue to serve alt-screen mode and modal overlays unchanged.

### Edge-scrub — alt-screen-specific artifact

`viewport.rs:22-115` — The edge-scrub hack (`scrub_crossterm_terminal_edges`) physically blanks terminal edge columns before each draw because "some terminals/tmux combinations can leave stale glyphs." This is specifically an alt-screen artifact — in inline mode, the application doesn't redraw the full screen, so edge-column residue from ratatui's diff is not a concern.

The edge-scrub also writes directly to the terminal backend *outside* of ratatui's `draw()` call (`viewport.rs:104-110`), which is the kind of raw-escape-sequence emission that would conflict with an inline renderer's cursor management. In inline mode, the edge-scrub should be disabled entirely.

### Scroll state — `scroll_back: u16` is App-side

`draw.rs:574-608` — Scroll bookkeeping runs on the main side in `build_render_model()`. `scroll_back` is a `u16` offset into the flat line cache. In inline mode, committed messages wouldn't exist in the line cache at all (they'd be in terminal scrollback), so `scroll_back` would only apply to the live region. This field would need reinterpretation, not removal.

### The focus/input chain — mode-independent

`input.rs:44+` — The if-let modal chain routes keyboard events. None of this cares about the screen buffer. Focus routing is orthogonal to rendering mode.

### Verdict on the architecture

| Component | Alt-screen assumption? | Change needed for inline mode |
|-----------|----------------------|-------------------------------|
| `setup_terminal()` | **Yes** — `EnterAlternateScreen` | Conditional: skip alt-screen in inline mode |
| `emergency_teardown_terminal()` | **Yes** — `LeaveAlternateScreen` | Conditional: skip in inline mode |
| `RenderModel` | **No** | None — already mode-agnostic |
| `render_frame()` | Implicit (full-screen layout) | New function for inline rendering; this one unchanged |
| Edge-scrub (`viewport.rs`) | **Yes** | Disable in inline mode |
| `build_render_model()` | Soft (scroll/line-cache assumes full history) | Restructure: committed messages excluded from cache |
| `render_thread` loop | **No** | Mode-selection before `render_frame()` dispatch |
| Input routing | **No** | None |
| `RenderCmd` enum | **No** | May add mode-switch commands |

**The alt-screen assumption is localized to three points:** `setup_terminal()`, `emergency_teardown_terminal()`, and the edge-scrub. The render pipeline's core — `RenderModel`, the render thread, input routing — is mode-agnostic. This is good architecture, whether or not it was intentional.

---

## 5. Hybrid Mode Spec — The Middle Path

The hybrid is not a compromise. It is a distinct design that takes the best of both modes by using each for what it does well.

### The three rendering modes in hybrid

#### Mode A: Primary Screen — Committed Transcript

**What it owns:** Completed user messages and completed assistant responses (including rendered markdown, code blocks, tool call results).

**How it works:** When a streaming response completes, the entire response is written to stdout as formatted terminal output. It scrolls upward into native scrollback. The application forgets it — no line cache, no scroll tracking, no selection management. The terminal handles all of that natively.

**Implementation:** A `commit_message()` function that takes the final rendered content and writes it to the backend's stdout using raw `crossterm::execute!(Print(...))` sequences — not through ratatui's `Terminal::draw()`. This is analogous to Ink's `<Static>` render path.

**What we gain:** Native scroll physics, native Cmd-F, native selection and copy, native resize reflow (terminal-dependent), zero memory for history rendering.

**What we lose:** Control over committed content appearance after commit. No retroactive style updates. No app-managed copy fidelity (content-relative selection, soft/hard break reconstruction). Terminal-quality reflow, not app-quality reflow.

#### Mode B: Bounded Live Region — Active Session

**What it owns:** The input box, the currently streaming response, status indicators, spinners, toasts, the subagent panel, the active-task progress bar.

**How it works:** A fixed-height region at the bottom of the visible terminal. The application redraws this region using cursor-positioning sequences (move to top of live region, clear to end of screen, redraw). The height grows as streaming content arrives, up to a configurable maximum (e.g., terminal height minus some padding). When the response completes, the live region's content is committed (Mode A) and the region shrinks back to the input box.

**Implementation:** A `render_live_region()` function that uses `MoveTo` + `Clear(ClearType::FromCursorDown)` to redraw only the bottom section. This could still use ratatui internally — create a `Terminal` with a constrained viewport — or use raw crossterm sequences for the simple case.

**Critical constraint:** The live region must **never exceed the terminal height.** If it does, the erase-and-redraw cycle will corrupt scrollback above. This is exactly Ink's [#359](https://github.com/vadimdemedes/ink/issues/359) failure mode. The mitigation: when streaming content exceeds the live region's height budget, begin committing the *top* of the current response to scrollback mid-stream, keeping the live region bounded.

#### Mode C: Alt-Screen — Modals Only

**What it owns:** Settings modal, plugins modal, models modal, help/keybinds modal, secret prompt modal, lightbox.

**How it works:** When a modal opens, the application enters `EnterAlternateScreen`, draws the modal using the existing `render_frame()` code (which already handles modal rendering), and when the modal closes, returns to `LeaveAlternateScreen`. The primary screen's scrollback is preserved — alt-screen is a separate buffer.

**Implementation:** The existing modal rendering code in `render_frame()` is already well-suited. The transition would be: `EnterAlternateScreen` → render modal with existing code → `LeaveAlternateScreen` on close. The `Pause`/`Resume` protocol in `RenderCmd` already demonstrates this pattern (terminal ownership transfer for the casino subprocess).

### What each mode owns — summary

```
┌─────────────────────────────────────────────────┐
│                  SCROLLBACK                      │
│  (terminal-owned, native)                        │
│                                                  │
│  ┌────────────────────────────────────────────┐  │
│  │ [committed] User: write a fibonacci fn     │  │
│  │ [committed] Asst: ```python                │  │
│  │               def fib(n):                  │  │
│  │                   ...                      │  │
│  │             ```                             │  │
│  │ [committed] User: now add memoization      │  │
│  └────────────────────────────────────────────┘  │
├─────────────────────────────────────────────────┤
│              LIVE REGION                         │
│  (app-owned, bounded, redrawn)                   │
│                                                  │
│  ┌────────────────────────────────────────────┐  │
│  │ ⟳ streaming... ▓▓▓░░░ 45%                 │  │
│  │                                            │  │
│  │ Here's the memoized version:               │  │
│  │ ```python                                  │  │
│  │ from functools import lru_cache            │  │
│  │ █ (cursor — still streaming)               │  │
│  │                                            │  │
│  │ ❯ _                                        │  │
│  └────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────┘

     Alt-screen only for modals:
     ┌──────────────────────────────┐
     │       ⚙ Settings             │
     │                              │
     │   Provider: [anthropic ▾]    │
     │   Model:    [sonnet-4  ▾]    │
     │   Theme:    [tokyo-night ▾]  │
     │                              │
     │        [Save]  [Cancel]      │
     └──────────────────────────────┘
```

---

## 6. The Costs We Don't Want to Inherit

The tui2 receipt itemizes six categories of cost from viewport ownership. Here's which ones the hybrid mode avoids and which it doesn't.

### Avoided costs ✅

| Cost | Why it's avoided in hybrid mode |
|------|-------------------------------|
| **Per-terminal scroll normalization tables** (AppleTerminal 3, Warp 9, WezTerm 1) | The terminal owns scrollback. Mouse wheel events in the committed-transcript region are handled by the terminal natively. The application never interprets scroll events for committed history. |
| **Wheel-vs-trackpad heuristic** | Same reason. Terminal-native scroll means we never need to distinguish input modalities for scrollback. |
| **User override configs for input semantics** | No app-owned scroll → no user-facing scroll-speed config → no override surface. |
| **Selection geometry for committed content** | Terminal-native selection applies to committed scrollback. No coordinate mapping needed. |
| **Full copy fidelity for committed content** | Terminal copy applies. Content is committed as rendered text — what you see is what you copy. (Trade-off: we lose tui2's content-relative copy-fidelity for committed messages. See below.) |

### Inherited costs ⚠️ (but scoped)

| Cost | Why it persists, and scope |
|------|--------------------------|
| **Scroll normalization for the live region** | If the live region is scrollable (e.g., streaming response exceeds the bounded height), the application must handle mouse wheel events within that region. But the scope is small: one bounded region, not the entire transcript. The current hardcoded 3-lines-per-event (`input.rs:254-263`) would apply only here. |
| **Multiplexer detection matrix** | tmux affects rendering in the live region too — escape sequence support, passthrough behavior, resize handling. But we already cope with tmux today (edge-scrub exists because of tmux). The incremental cost of inline mode under tmux is primarily: does the commit-to-scrollback write path work correctly when tmux is intercepting output? This needs testing, not a detection matrix. |

### New costs specific to hybrid mode ⚠️

| Cost | Description |
|------|-------------|
| **Mid-stream commit** | When streaming content exceeds the live region height, the top of the response must be committed to scrollback mid-stream. This requires a "partial commit" mechanism — writing the top N lines to stdout while continuing to stream into the remaining live region. Getting the cursor math right during this transition is the single hardest implementation problem in the hybrid design. |
| **Commit formatting** | Committed content must be written as formatted terminal output (ANSI escape sequences for colors, bold, etc.) that looks correct in terminal scrollback. Today, ratatui handles all formatting through its cell buffer. Committing to scrollback means we need a "serialize rendered content to ANSI output" path. ratatui's `Buffer` can be dumped to ANSI, but the integration path needs design. |
| **Live region height tracking** | The application must know exactly how many rows the live region occupies, and must track this correctly as content streams in. Miscounting by even one row causes either: (a) corruption of the scrollback line above the live region, or (b) a gap between committed content and the live region. |
| **Alt-screen transition for modals** | Entering/leaving alt-screen mid-session must preserve the live region state. The sequence: save live region state → enter alt-screen → render modal → leave alt-screen → restore live region. The `Pause`/`Resume` pattern in `render_thread.rs` is a precedent but handles a simpler case (full terminal handoff to a child process, not a scoped modal overlay). |

---

## 7. The 1.0-Doesn't-Preclude-1.x Assertion

Yoru's Bet 4: *"Don't promise inline mode in 1.0. Do promise hybrid mode in 1.x."*

The question is concrete: what must be TRUE about the 1.0 architecture to keep inline/hybrid mode viable in 1.x? And are any of the relevant structures 1.0-frozen?

### What must be true

**1. `RenderModel` must remain mode-agnostic.**

Current status: ✅ **Already true.** As verified in §4, `RenderModel` contains no alt-screen assumptions. It is a bag of data that a mode-specific renderer consumes. A `render_inline_frame()` function could consume the same `RenderModel` that `render_frame()` does today.

Risk of 1.0 freezing this: Low. `RenderModel` is `pub(crate)` — internal to agent-tui. It is not part of any external API surface. It can change freely in 1.x.

**2. `setup_terminal()` / `emergency_teardown_terminal()` must accept a mode parameter.**

Current status: ❌ **Not yet true.** These functions unconditionally enter/leave alt-screen. But they are two-line changes to conditionalize.

Risk of 1.0 freezing this: None. These are internal functions, not API surface.

**3. The render thread must support mode dispatch.**

Current status: 🟡 **Structurally ready.** The render thread loop (`render_thread.rs:387+`) calls `render_frame()` unconditionally. Adding a mode flag and dispatching to either `render_frame()` or `render_inline_frame()` is a small change to the loop. The `RenderCmd` enum already supports extensibility (adding a `SwitchMode` variant is trivial).

Risk of 1.0 freezing this: None. Internal.

**4. The line cache must support partial-history rendering (committed messages excluded).**

Current status: ❌ **Not true.** `build_render_model()` (`draw.rs:514-565`) builds the line cache from `app.messages[0..n]` — all messages. In inline mode, committed messages should be excluded (they're in terminal scrollback, not in the app's render path). This requires P9 (TranscriptStore extraction) to land first — specifically, the TranscriptStore needs a concept of "committed up to message K" so the line cache only covers messages K+1..n.

Risk of 1.0 freezing this: **This is the one dependency that matters.** If 1.0 ships without P9 (TranscriptStore extraction), then the line cache's tight coupling to the full message array makes inline mode a larger refactor. P9 is the gate.

**5. Committed-content serialization must be possible.**

Current status: 🟡 **Partially ready.** `render_message_lines()` produces `Vec<Line<'static>>` — ratatui `Line` objects with styled spans. These can be serialized to ANSI escape sequences, but there's no existing function that does this. Ratatui's `Buffer` can be rendered to a backend, but writing individual `Line` objects to raw stdout requires a new code path.

Risk of 1.0 freezing this: None. This is new code, not a change to existing API.

**6. The edge-scrub must be conditional (disabled in inline mode).**

Current status: ❌ **Not conditional.** `scrub_crossterm_terminal_edges` is called unconditionally in `render_frame()` at `draw.rs:797-800`.

Risk of 1.0 freezing this: None. Internal.

### Summary: what structures would need to change

| Structure | Change needed | Part of 1.0 API surface? | P-task dependency |
|-----------|--------------|--------------------------|-------------------|
| `setup_terminal()` | Add mode parameter | No (internal) | None |
| `emergency_teardown_terminal()` | Conditionalize alt-screen leave | No (internal) | None |
| Render thread loop | Mode dispatch | No (internal) | None |
| `RenderCmd` | Add `SwitchMode` variant | No (internal) | None |
| Line cache / message array | Committed-message exclusion | No (internal) | **P9 (TranscriptStore)** |
| Edge-scrub | Conditional disable | No (internal) | None |
| `render_frame()` | Unchanged — stays as alt-screen renderer | No (internal) | None |
| New `render_inline_frame()` | New code | N/A | P9 |
| New `commit_to_scrollback()` | New code | N/A | None |

### The 1.0 gate check

**None of the relevant structures are part of SynapsCLI's external API surface.** `RenderModel`, the render thread, lifecycle functions, and the draw pipeline are all `pub(crate)` internal implementation. A 1.0 release freezes the *user-facing* interface (commands, config format, extension protocol), not internal rendering architecture.

**The one structural dependency is P9 (TranscriptStore).** If TranscriptStore lands before or in 1.0, the line cache becomes separable from the full message history, and inline mode is a clean addition in 1.x. If P9 does not land, inline mode requires both the extraction and the mode implementation simultaneously — a larger, riskier change.

**Assertion: 1.0 does not preclude inline mode in 1.x**, provided:
1. P9 (TranscriptStore) lands before 1.0 or early in 1.x
2. No new code in the 1.0 cycle introduces alt-screen assumptions into `RenderModel` or the render thread protocol

Both are achievable without any special effort. The architecture is already clean on this axis.

---

## 8. Recommendation

Three options were on the table. Here is the call.

### Option A: Attempt hybrid in 0.5.x with careful scope
**Rejected.**

The hybrid mode is not a weekend project. Mid-stream commit, live-region height tracking, alt-screen modal transitions, and the commit-to-scrollback serialization path are each individually tractable but collectively represent ~2-3 weeks of focused work plus a testing surface that spans every terminal × multiplexer combination we support. Attempting this before the test harness (P4) and TranscriptStore (P9) land is building on sand. We would be doing exactly what tui2 did — building a viewport ownership system without the infrastructure to verify it works across terminals — and the tui2 receipt says that path ends in retirement.

### Option B: Defer indefinitely
**Rejected.**

The demand is real. The field report's #5 ranked demand — inline rendering — is specifically for our workload class: agent CLIs that generate long streaming transcripts. Claude Code spent a year on it. Gemini CLI has open issues requesting it. Every agent CLI user who wants to Cmd-F through their session history is asking for it. "Indefinitely" means "until a competitor ships it and we're asked why we don't." That's a reactive posture for a proactive product.

### ✅ Option C: Stay alt-screen for 1.0; preserve three architectural invariants; ship hybrid in 1.x

**This is the recommendation.**

**For 1.0**, maintain the current alt-screen-only mode. It works. It's tested (or will be, with P4). It avoids the tui2 cliff. Users get a correct, polished, stable interface.

**Preserve these three invariants through 1.0 development:**

1. **`RenderModel` stays mode-agnostic.** No alt-screen assumptions enter the snapshot struct. Any code review that adds terminal-mode-specific fields to `RenderModel` is a red flag.

2. **P9 (TranscriptStore) lands before or in 1.0.** The committed-message concept must exist in the data layer before the rendering layer can use it. TranscriptStore is independently justified (god-object decomposition, virtualization prep) — inline mode is a bonus reason.

3. **`lifecycle.rs` setup/teardown remain the sole owners of alt-screen entry/exit.** No new code should hardcode `EnterAlternateScreen` or `LeaveAlternateScreen` outside lifecycle (the gamba.rs precedent is acceptable as a subprocess handoff, not as a rendering mode assumption).

**For 1.x**, ship hybrid mode as a user-selectable option:

- Default: alt-screen (current behavior)
- Opt-in: `--inline` flag or `display.mode = "inline"` in config
- Hybrid semantics as specified in §5: committed transcript on primary screen, bounded live region, alt-screen for modals

**Sequencing for 1.x hybrid implementation:**

1. P4 (test harness) — must exist first
2. P9 (TranscriptStore) — must exist first
3. `commit_to_scrollback()` — serialize rendered `Line` content to ANSI stdout
4. `render_inline_frame()` — draw only the live region, using cursor-positioning
5. Render thread mode dispatch — `RenderCmd::SwitchMode`
6. Mid-stream commit — the hard part: partial commit when live region exceeds height budget
7. Alt-screen modal transitions — enter/leave alt-screen for modals within inline mode
8. Terminal compatibility testing — the surface that decides whether we ship or don't

**Estimated 1.x implementation cost:** ~3-4 weeks, assuming P4 and P9 are already landed. The riskiest item is #6 (mid-stream commit) — budget extra time there.

**Go/no-go criteria for shipping hybrid in 1.x:**
- P4 harness covers inline-mode-specific scenarios (commit, live-region redraw, modal transition)
- No terminal-specific workarounds required beyond what we already carry (edge-scrub is acceptable precedent; a scroll normalization table is not)
- User testing with at minimum: iTerm2, kitty, WezTerm, Alacritty, VS Code terminal, tmux
- Fallback to alt-screen is always one config change away

---

## Coda

The tui2 receipt is a warning about totality, not about ambition. tui2 failed because it tried to own *everything* — scrollback, selection, copy, scroll physics — and the terminal compatibility matrix punished that totality. The hybrid mode succeeds by owning only what the application must own (the live streaming region) and delegating everything else (committed history) to the terminal, which has had fifty years of practice at it.

SynapsCLI's architecture is, whether by design or happy accident, already structured to support this. The `RenderModel` snapshot is mode-agnostic. The render thread is a dispatch loop awaiting a second renderer. The line cache can be scoped once TranscriptStore exists. The three invariants above are cheap to maintain and expensive to violate.

The recommendation: don't build it now, don't promise it now, but don't close the door. Keep the architecture honest. When the demand crystallizes into a 1.x milestone, the path is clear.

---

*The field report says inline rendering is no longer optional for agent CLIs. The tui2 receipt says owning the viewport is a cliff. The hybrid mode is the narrow path between those two truths. It is walkable — but only if the ground is prepared before we step onto it.*
