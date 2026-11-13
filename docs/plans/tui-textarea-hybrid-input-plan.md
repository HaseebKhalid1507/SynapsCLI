# Input Editor → tui-textarea (Hybrid) — Scope & Plan

**Status:** SCOPED — not started
**Created:** 2026-08-03 (S289)
**Decision:** Option B — `TextArea` owns editing **state**; existing soft-wrap
rendering stays. Chosen over full swap because `tui-textarea` has **no soft-wrap**
(long-standing upstream issue) and the wrapped input box (up to 10 lines) is
load-bearing UX for a chat CLI.

---

## 1. Core insight (from recon)

agent-tui renders from a **`RenderModel` snapshot** (render-thread architecture),
not from `App`. `draw.rs` consumes `model.input: String` + `model.cursor_pos`.

Therefore the hybrid is clean: **`TextArea` is never rendered.** It lives in
`App` purely as the editing state machine. At snapshot time we derive
`(flat_text, flat_cursor_char_pos)` and feed the *unchanged* render pipeline
(wrap math, ghost hints, software cursor, height calc all stay).

```text
key events ─→ App.editor: TextArea<'static> ─→ snapshot (text, cursor)
                     │                              │
             editing semantics                render pipeline
             (lib-owned)                      (ours, UNCHANGED)
```

## 2. Current inventory (what exists today)

| Piece | Where | Fate |
|---|---|---|
| `App.input: String`, `cursor_pos: usize` (char-based) | app.rs:34-35 | **replaced** by `editor: TextArea<'static>` + compat accessors |
| `cursor_byte_pos()`, `input_char_count()` | app.rs:374-383 | deleted (byte/char conversion is the lib's problem now) |
| `input_history: Vec<String>`, `history_index`, `history_up/down` | app.rs:41-42, 616-646 | **kept** (ours; textarea's history is undo, not prompt history) |
| Hand-rolled editing in `handle_key` — char insert, backspace, left/right, Ctrl-A/E/W/U, Home/End, Alt-arrows, Shift-Enter newline | input.rs:279-421 | **deleted**, forwarded to `editor.input(...)` |
| `delete_word_backward`, `jump_word_left/right` | input.rs:925-977 | deleted (lib provides) |
| `handle_tab_complete`, `tab_cycle` | input.rs:862+, app.rs:48 | kept; mutation rewritten via editor set-text helper |
| Paste path (dedupe, size cap, `input_before_paste`) | input.rs:116-140, app.rs:90,326 | kept; insertion becomes `editor.insert_str` |
| Sidecar transcription insert-at-cursor | sidecar.rs:249-265 | kept; rewritten via `editor.insert_str` |
| Modal routing (models/settings/plugins/help/secret) | input.rs (~⅔ of file) | untouched |
| Keybind registry check | input.rs:296-326 | untouched (runs before editor forwarding) |
| Wrap math `input_wrap_info` | view_model.rs:157 | **untouched** |
| Input rendering + ghost hint + software cursor + scroll | draw.rs:1360-1460, 852, 868 | **untouched** |
| Snapshot fields `input`, `cursor_pos` | render_model.rs, draw.rs:708-709 | untouched; fed from accessors |
| Testing harness `&self.app.input` | testing.rs:337 | accessor swap |
| Tests writing `app.input =` directly | sidecar.rs tests, input.rs tests | migrate to `app.set_input_text(...)` helper |

## 3. Design

### 3.1 State

```rust
// app.rs
pub(crate) editor: tui_textarea::TextArea<'static>,  // replaces input + cursor_pos

// compat accessors (used by snapshot, commands, sidecar, tests):
fn input_text(&self) -> String            // editor.lines().join("\n")
fn input_is_empty(&self) -> bool
fn cursor_char_pos(&self) -> usize        // flat char index from editor.cursor() (row,col)
fn set_input_text(&mut self, s: &str)     // rebuild editor, cursor→end
fn clear_input(&mut self)
fn insert_at_cursor(&mut self, s: &str)   // editor.insert_str
```

Flat-cursor mapping (for the snapshot): `sum(chars(lines[..row]) + 1) + col`.
One ~10-line helper + unit tests. This is the only new math.

### 3.2 Key routing in `handle_key` (the contract)

Intercepted BEFORE the editor (unchanged semantics):

| Key | Behavior |
|---|---|
| Ctrl-C | Quit |
| Esc (streaming) | Abort |
| Enter (non-empty, !streaming) | submit |
| Enter (non-empty, streaming) | queue/steer submit |
| Shift-Enter | newline → forward to editor as insert `\n` |
| Tab on `/…` | completion cycle (unchanged) |
| Shift-Up/Down | transcript scroll |
| Ctrl-O | toggle full output |
| plugin/user keybinds | registry first, as today |
| Up / Down | **see 3.3** |

Everything else editing-related → `editor.input(Event::Key(...))`:
chars, backspace/delete, ←/→, Home/End, Ctrl-A/E, Ctrl-W/Alt-Backspace,
Alt-←/→ (word jumps), Ctrl-U, plus **new for free**: Ctrl-K, proper
Delete-forward, multi-line ↑/↓ cursor movement, **undo/redo (Ctrl-Z / Ctrl-Y)**.

Note: tui-textarea's default Ctrl-U = delete-to-line-start, today's Ctrl-U =
clear whole buffer. Keep today's behavior (intercept, `clear_input`). Any other
semantic drift found in review gets the same treatment: **our semantics win.**

### 3.3 Up/Down policy (DECIDED 2026-08-03: edges)

- **Up** → history iff cursor on first line of buffer; else cursor up.
- **Down** → history-forward iff on last line; else cursor down.
- History navigation replaces buffer via `set_input_text` (as today).

Cheap to implement (`editor.cursor().0 == 0` / `== lines.len()-1`). If it feels
wrong in practice, trivial to revert to history-always.

### 3.4 What we explicitly do NOT adopt

- TextArea's rendering/widget path, block/cursor styles, its horizontal scroll.
- Its search, its selection (transcript selection already exists separately).
- Its line-number/placeholder features.

## 4. Dependency

**P0 gate RESOLVED (2026-08-03, S289).** Original `tui-textarea` is dormant
(0.7.0, Oct 2024; master still pins ratatui ^0.29) → **fails** against our
ratatui 0.30.2 / crossterm 0.29.

**Use `tui-textarea-2` = "0.12"** (maintained continuation fork,
github.com/srothgan/tui-textarea, ~90K downloads, updated 2026-07):
- depends on `ratatui-core ^0.1` — satisfied by ratatui 0.30.2's
  `ratatui-core 0.1.2` already in our lockfile; **no duplicate ratatui**
- `crossterm ^0.29` feature matches our pin exactly — our `KeyEvent`s feed
  `editor.input()` natively, no manual `Input` mapping layer
- features: `["crossterm"]`, no `search`/`serde`/`arbitrary`

(Runner-up: `ratatui-textarea` 0.9.2 — also ratatui-core-compatible but events
go through a `ratatui-crossterm` adapter. Fallback if `-2` disappoints.)
API drift vs. the original crate is possible (0.7 → 0.12); P1 verifies the
exact method surface used by §3.1 before anything else builds on it.

## 5. Phases

- **P1 — beachhead.** Add dep (compat-gated). `editor` field + accessors +
  flat-cursor helper + unit tests. `input`/`cursor_pos` fields still present,
  kept in sync one-way (editor → mirror) so nothing else changes yet. Green.
- **P2 — key routing.** `handle_key` editing branches → `editor.input()`.
  Delete `delete_word_backward`/`jump_word_*`. Up/Down policy per 3.3.
  Ctrl-U interception. Existing input.rs tests updated. Green.
- **P3 — mutators.** Paste path, tab-complete, history nav, sidecar insert,
  `/`-detection sites (draw.rs:604, commands) all through accessors. Delete
  mirror fields; snapshot reads accessors. `cursor_byte_pos` dies. Green.
- **P4 — polish + proof.** Testing-harness accessor; migrate direct
  `app.input =` in tests; manual QA checklist (below); update CHANGELOG.

Each phase is a separate commit, workspace-green (`cargo test`), TUI boots and
chats between phases. P1+P2 prove the concept; P3 is the risky sweep — if it
sours, revert to the P2 commit and reassess.

## 6. QA checklist (P4, by hand, in a real terminal)

- [ ] type/edit ASCII + emoji + CJK (wide chars) — cursor lands right at wraps
- [ ] long paragraph wraps to ≤10 lines, scrolls beyond, cursor tracks
- [ ] Shift-Enter multi-line; ↑/↓ moves within buffer; history at edges (3.3)
- [ ] paste multi-line, paste >cap (truncation toast), paste during streaming
- [ ] `/` ghost hint renders; Tab completion cycles; ambiguous `/` opens finder
- [ ] Ctrl-A/E/W/U, Alt-arrows, Home/End behave as before; Ctrl-Z undo works
- [ ] sidecar transcription inserts at cursor with spacing rules (tests too)
- [ ] streaming: Esc aborts, Enter queues, typing stays live
- [ ] modals (models/settings/plugins/help/secret) unaffected

## 7. Risks

| Risk | Mitigation |
|---|---|
| ratatui 0.30 compat of the crate | P0 gate (§4) — checked before any code |
| Semantic drift in default keybinds | Contract table (3.2) is law; our semantics win via interception |
| Flat-cursor mapping bugs (wide chars, newlines) | Property-ish unit tests vs `input_wrap_info` expectations |
| Hidden direct `app.input` writers | `rg '\.input\b'` sweep in P3; compiler does the rest once fields die |
| Up/Down policy feels wrong | Single function, feature-flag-free, easy revert |

## 8. Payoff

~250–300 lines of hand-rolled editing + cursor conversion deleted; unicode
correctness delegated to a maintained lib; undo/redo + multiline navigation
gained; **zero rendering/UX regression** because the render path never changes.

## 9. Estimate

P1: small · P2: medium · P3: medium, mechanical · P4: small.
One focused session for P1–P2, one for P3–P4. Good spike-dispatch shape:
one phase per dispatch, review between.
