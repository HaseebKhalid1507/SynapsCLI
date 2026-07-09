# P7 BUILD DESIGN — FocusManager + ModalStack (code-verified)
*Author: principal-architect pass, verified against dev HEAD of ~/Projects/agent-runtime (crates/agent-tui). All line numbers checked this session. No repo files modified, no builds run.*

**Context:** Wave 3 / P7 of `roadmap-overnight-plan.md`; original at `synaps-tui-modernization-plan.md` (P7 entry). Zero's original plan missed `secret_prompts` because it is routed inline in the event loop, outside `input.rs`. This doc's inventory (§1) is grep-complete across the tui crate specifically to prevent a recurrence.

---

## 1. Complete modal/overlay inventory (the anti-"missed-secret_prompts" artifact)

Grep basis: every `Option<*State>` on App, every input-consuming site in `input.rs` + `mod.rs` event arm, every draw call after the base frame in `draw.rs`.

| # | Modal / overlay | State field (file:line) | Input-routing site | Draw site | Focus model |
|---|---|---|---|---|---|
| 1 | **help_find** (`/help find` lightbox) | `app.rs:89` `help_find: Option<synaps_cli::help::HelpFindState>` | `input.rs:54-66` (chain arm #1, highest priority) | `draw.rs:1576-1578` (drawn LAST = topmost) | single search field; no Focus enum |
| 2 | **models** (`/model`, `/models`) | `app.rs:87` `models: Option<ModelsModalState>` | `input.rs:68-86` (chain arm #2); async results via `mod.rs:304` (`model_list_rx` arm) | `draw.rs:1570-1572` | internal cursor/view state; **internal sub-lightbox** `expanded: Option<ExpandedModelsState>` (`models/mod.rs:191`, rendered `models/mod.rs:706-711`) handled entirely inside `models::handle_event` — NOT a separate routing entity |
| 3 | **plugins** (`/plugins` marketplace) | `app.rs:85` `plugins: Option<PluginsModalState>` | `input.rs:88-101` (chain arm #3) → most outcomes deferred to async loop via `InputAction::PluginsOutcome` (`input.rs:29-30`, handled `mod.rs` ~1766+) | `draw.rs:1573-1575` | `plugins/state.rs:26` `enum Focus { Left, Right }` + `RightMode` (List/Detail/AddMarketplaceEditor, `plugins/state.rs:31+`) |
| 4 | **settings** (`/settings`) | `app.rs:83` `settings: Option<SettingsState>` | `input.rs:103-208` (chain arm #4, 105 lines): PluginCustom key-forward :104-118, paste-into-editors :120-134, 12 `InputOutcome` variants incl. synchronous config writes :143-175, theme preview/revert :189-197 | `draw.rs:1567-1569` (drawn FIRST = bottom modal) | `settings/mod.rs:161` `enum Focus { Left, Right }` + `edit_mode: Option<ActiveEditor>` (`settings/mod.rs:210`; 6 variants at :167 — Text/Picker/CustomModel/ApiKey/PluginText/PluginCustom) |
| 5 | **plugin custom editor** (nested inside settings) | lives inside `SettingsState::edit_mode = Some(ActiveEditor::PluginCustom{..})` (`settings/mod.rs:196+`), populated async at `mod.rs:1654-1683` | `input.rs:104-118` — intercepts BEFORE normal settings handling; Esc clears edit_mode, other keys → `InputAction::PluginEditorKey` executed at `mod.rs:1685+` (async extension round-trip) | inside `settings::render` | modal-within-modal; de-facto second stack level today |
| 6 | **secret_prompts** (THE MISSED ONE) | **not on App** — local `let mut secret_prompts = SecretPromptQueue::new()` in `run()` at `mod.rs:170`; channel wiring `mod.rs:167-169` (`SecretPromptHandle` → unbounded mpsc → `Arc<Mutex<rx>>`); queue polled at `mod.rs:423` inside the 16ms tick arm (arm gated at `mod.rs:411` incl. `secret_prompts.is_active()`) | **inline in the event arm, `mod.rs:573-591`** — checked BEFORE `input::handle_event` is ever called; Enter=submit, Esc=cancel, Backspace, Char, Paste; then `continue` (total bypass of input.rs) | `draw.rs:1517-1561` — **BEFORE toasts and modals** (draws UNDER them); also blanks the transcript at `draw.rs:931-934`; snapshot `render_model.rs:90,132-134` | single masked buffer; queue type `agent-engine/src/tools/secret_prompt.rs` — each prompt carries a `oneshot::Sender<Option<String>>`; `submit()`/`cancel()` auto-activate the next queued prompt |
| 7 | **toasts** | `app.rs` `toasts: ToastProvider` (toast.rs, timed) | **none** — passive, no input, auto-expiring | `draw.rs:1564` `render_toasts_from_snap` (above secret prompt, below modals) | n/a — never focusable |
| 8 | **gamba** (casino child process) | `app.rs:80-81` `gamba_child: Option<Child>` | **not routed** — the entire crossterm event arm is gated off: `mod.rs:570` `maybe_event = event_reader.next(), if app.gamba_child.is_none()`; child owns the terminal | frame skipped entirely: `build_render_model` returns `None` when gamba active (`draw.rs:487-488`) | out of scope — a terminal handoff, not a modal. **Explicit P7 exclusion**; the `mod.rs:570` gate stays. |
| 9 | **sidecars** | `app.rs` `sidecars: HashMap<String, SidecarUiState>` | none in `input.rs` (zero grep hits) — render-only status pills | base frame | not a modal |
| 10 | **lightbox.rs** | — | — | geometry helpers only (`LIGHTBOX_EDGE_INSET`, safe-area math) | not a modal; shared layout util |

**Chain priority (input) vs draw z-order — verified consistent for the four chain modals:** input priority help_find > models > plugins > settings (`input.rs:54,68,88,103`) is exactly the reverse of draw order settings → models → plugins → help_find (`draw.rs:1567-1578`), i.e. z-topmost gets input. The existing settings→marketplace flow (`InputOutcome::OpenPluginsMarketplace` → `input.rs:198-200` → `mod.rs:1825-1834` `PluginsModalState::new_from_settings`, `plugins/state.rs:226`) is an **implicit two-deep stack** encoded by chain ordering: plugins arm sits above settings arm, plugins draws above settings, closing plugins (sets `None`) falls back to settings. The ModalStack makes this explicit.

**⚠ Latent inconsistency found (decide at Gate 2):** secret_prompts intercepts input BEFORE all modals (`mod.rs:573`) but draws UNDER them (`draw.rs:1517` vs modals at :1566+). If a tool fires a secret prompt while e.g. settings is open (possible: settings stays open during streaming), the user types into an occluded password box. The stack migration naturally fixes this (top of stack draws last); the fix is a deliberate, flagged behavior change — see §5/§6.

**Harness gap found:** `testing.rs:158-174` `event()` dispatches straight into `input::handle_event` — the harness can NEVER exercise today's secret-prompt routing because that routing lives only in `run()`. `testing.rs:77` holds a `SecretPromptQueue` for rendering only (`:189,:229`). Folding secret_prompts into `input.rs` routing is what makes it testable at all.

---

## 2. `PaneId` design

New module `src/tui/focus.rs`:

```rust
/// Typed identity for every input-receiving surface. App-grade, not
/// framework-grade: a closed enum, no id-paths, no arenas (Yoru scoping).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PaneId {
    /// Base pane — chat transcript + input box. Never on the stack;
    /// it is the implicit bottom (empty stack ⇒ Chat gets input).
    Chat,
    HelpFind,      // app.rs:89
    Models,        // app.rs:87 (expanded-models lightbox stays INTERNAL to Models)
    Plugins,       // app.rs:85
    Settings,      // app.rs:83
    /// ActiveEditor::PluginCustom promoted from settings.edit_mode special-case
    /// (input.rs:104-118) to a real stack level above Settings.
    PluginEditor,  // settings/mod.rs:196+
    SecretPrompt,  // mod.rs:170 queue → moves to App (§5)
}
```

Deliberate NON-members, each with a reason recorded in a doc comment:
- **Toast** — never focusable, purely passive (`draw.rs:1564`).
- **Gamba** — terminal handoff, gated upstream of the event loop (`mod.rs:570`); the stack never sees events while it runs.
- **Sidecars / lightbox helpers** — render-only.
- **Models expanded lightbox** — internal view state of `ModelsModalState` (`models/mod.rs:191`); its keys are already dispatched inside `models::handle_event`. Promoting it would change behavior for zero benefit. Revisit only if it ever needs to outlive the models modal.
- **Settings' other ActiveEditor variants** (Text/Picker/ApiKey/…) — stay internal to the Settings pane handler; they are field editors, not panes. Only `PluginCustom` is promoted, because it already routes through the main loop as a distinct machine.

`PaneId` is `Copy` and carries **no state**. Modal state stays exactly where it is today (the `Option<…State>` fields on App, `app.rs:83-89`). The stack is an *index over* open modals, not a new owner — this is the core of the behavior-preservation strategy (§6).

---

## 3. `ModalStack` design

Same module `src/tui/focus.rs`:

```rust
#[derive(Default)]
pub(crate) struct ModalStack {
    stack: Vec<PaneId>,          // bottom → top
}

impl ModalStack {
    pub fn push(&mut self, id: PaneId) { /* rejects PaneId::Chat and duplicates (debug_assert + no-op) */ }
    pub fn pop(&mut self) -> Option<PaneId> { self.stack.pop() }
    pub fn remove(&mut self, id: PaneId) { /* out-of-order close, e.g. secret prompt resolved by cancel-all */ }
    pub fn top(&self) -> PaneId { self.stack.last().copied().unwrap_or(PaneId::Chat) }
    pub fn contains(&self, id: PaneId) -> bool { … }
    pub fn iter_bottom_up(&self) -> impl Iterator<Item = PaneId> + '_ { … }  // draw order
    pub fn is_empty(&self) -> bool { … }
}
```

Contracts (doc-commented on the type, tested in unit tests):
1. **Top-of-stack gets input** — `input.rs::handle_event` dispatches on `stack.top()`; nothing below the top ever sees an event (hidden-widget rule). Occluded panes keep their state but are input-dead.
2. **Escape pops uniformly** — each pane handler maps its close action to a `Pop` outcome; the routing layer performs the pop AND clears the matching `Option` field. One Esc = one level (settings→marketplace→Esc lands back in settings, exactly as today's chain fallthrough does).
3. **Duplicate-push rejected** — matches today: opening `/settings` while settings is open is impossible (the settings arm swallows the slash input first). `debug_assert!` + no-op in release.
4. **Sync invariant:** `stack.contains(X) ⇔ corresponding app field is_some()` (for SecretPrompt: `⇔ app.secret_prompts.is_active()`). Enforced by a single `debug_assert_stack_sync(app)` called after every `handle_event` and every reconcile (§5). This is the tripwire that catches any open/close site we missed.
5. **Empty stack ⇒ Chat** — the base pane is not stored; `top()` returns `Chat` for an empty stack and routing falls through to today's `input.rs:210-212` key/mouse/paste handling.

Pane handler shape (the thing that replaces each chain arm):

```rust
pub(crate) enum PaneOutcome {
    Consumed,                 // stay open, nothing for the loop
    Pop,                      // close me (routing clears my Option field + pops)
    Action(InputAction),      // defer to the async loop (PluginsOutcome, ModelsApply, …)
    PopThen(InputAction),     // close AND defer (e.g. models Apply)
}
```

The existing `InputAction` enum (`input.rs:10-41`) is **kept unchanged** — the async main-loop dispatch (`mod.rs:606-1841`, 16 variants) is out of P7's blast radius. Pane handlers are thin adapters from today's per-modal `InputOutcome` types to `PaneOutcome`.

---

## 4. `FocusManager` design

Scope check from real code: exactly TWO per-modal focus enums exist — `plugins/state.rs:26` and `settings/mod.rs:161`, both literally `{ Left, Right }`. help_find and models have a single implicit focusable; secret prompt has one masked field. So the FocusManager is deliberately small:

```rust
pub(crate) struct FocusManager {
    /// Per-pane focus ring: pane → (focusables, current index).
    /// Focus state SURVIVES occlusion: push Plugins over Settings, pop back,
    /// and Settings' Left/Right position is where you left it — this matches
    /// today, where Focus lives inside the retained SettingsState.
    rings: HashMap<PaneId, FocusRing>,
}
pub(crate) struct FocusRing { slots: Vec<FocusSlot>, current: usize }
// FocusSlot: a small id (u8 newtype) + a `visible: bool` so hidden widgets
// are skipped by next()/prev() (traversal wraps).
```

- `next(pane)` / `prev(pane)` — Tab / Shift-Tab (BackTab) traversal within the ACTIVE pane only; wraps; skips `visible == false` slots.
- `current(pane) -> FocusSlot` — read by draw code for highlight styling.

**Migration mapping (per modal):**
- **plugins** (`plugins/state.rs:26`): `Focus::Left` ↦ slot 0, `Focus::Right` ↦ slot 1. Mechanically: `PluginsModalState` keeps a `focus()` accessor whose backing store becomes the FocusManager ring; `plugins/input.rs` match arms on `Focus` are untouched in shape. Today's left/right arrow semantics that *also* move focus are preserved by the pane handler calling `next/prev` where the old code assigned `state.focus`.
- **settings** (`settings/mod.rs:161`): identical two-slot mapping. `edit_mode: Some(…)` (`settings/mod.rs:210`) suppresses traversal exactly as today (editor swallows Tab); implemented by the pane handler checking `edit_mode` before consulting the ring — behavior-preserving, no new rule.
- **help_find / models / secret_prompt**: single-slot rings (registered for uniformity so the P7.9 "synthetic modal" extensibility test has one shape to follow).

Explicitly NOT built (report-aligned, "no architecture astronautics"): no global focus tree, no id-paths, no focus events/observers, no inter-pane Tab (Tab never leaves the active pane today — verified: no chain arm forwards Tab across modals).


---

## 5. secret_prompts decision: FOLD IT INTO THE MODALSTACK (recommended, with a reconcile mechanism)

**Decision: fold in.** Leaving it inline is a permanent routing bypass — the exact structural hole that made Zero miss it. Three code-grounded reasons:

1. **It already IS a modal by every behavioral criterion:** it swallows all input (`mod.rs:573-591` then `continue`), blanks the transcript (`draw.rs:931-934`), draws an overlay (`draw.rs:1517-1561`), and has Esc-to-dismiss (cancel). It only *lives* outside `input.rs` because its state was a `run()` local instead of an App field.
2. **The harness cannot test it today.** `testing.rs:158-174 event()` calls `input::handle_event` directly; the `mod.rs:573` interception is unreachable headless. `testing.rs:77` carries a `SecretPromptQueue` for *render* snapshots only. Folding in = the Enter/Esc/Backspace/paste flows become harness-testable for the first time.
3. **The "exception" alternative rots.** Any future pane added above the stack would silently sit UNDER the secret prompt's hardcoded pre-check — invisible priority, undocumented, unrepeatable.

**The async/channel problem, precisely:** the queue is not user-opened. Tools deep in the engine call `SecretPromptHandle::prompt()` (`agent-engine/src/tools/secret_prompt.rs:18-27`), which sends a `SecretPromptRequest` (carrying a `oneshot::Sender<Option<String>>`) over the mpsc created at `mod.rs:167`. The queue drains that channel via `poll_requests` at `mod.rs:423` (inside the 16ms tick arm, which stays alive during prompts because `mod.rs:411`'s gate includes `secret_prompts.is_active()` — and prompts only fire mid-stream, when `app.streaming` keeps the tick alive anyway). `submit()`/`cancel()` fire the oneshot AND auto-activate the next queued prompt (`secret_prompt.rs` `activate_next`). So activation and deactivation both happen OUTSIDE any input event — a push/pop cannot be tied to a keypress alone.

**Mechanism (exact):**
1. **Move the queue onto App:** `app.secret_prompts: SecretPromptQueue` replaces the `run()` local at `mod.rs:170`. Channel wiring (`mod.rs:167-169`) is untouched; `poll_requests` call site at `mod.rs:423` becomes `app.secret_prompts.poll_requests(&secret_prompt_rx)`. `build_render_model`'s `&SecretPromptQueue` parameter (`draw.rs:484`) is dropped in favor of reading the App field; `testing.rs:77/116/189/229` lose their parallel copy of the queue (harness now shares production state — a correctness win by construction).
2. **Reconcile, don't event-couple:** a single function, called (a) immediately after `poll_requests` at `mod.rs:423`, and (b) after every `submit()`/`cancel()` in the pane handler:
   ```rust
   fn reconcile_secret_prompt(app: &mut App) {
       let active = app.secret_prompts.is_active();
       let on_stack = app.modal_stack.contains(PaneId::SecretPrompt);
       match (active, on_stack) {
           (true, false) => app.modal_stack.push(PaneId::SecretPrompt),
           (false, true) => app.modal_stack.remove(PaneId::SecretPrompt),
           _ => {}
       }
   }
   ```
   Queue chaining (submit → `activate_next` promotes the next pending prompt) is handled for free: the pane stays on the stack while `is_active()` remains true across consecutive prompts — identical to today's behavior where the `mod.rs:573` check re-fires per event.
3. **Pane handler:** `PaneId::SecretPrompt` handler in `input.rs` reproduces `mod.rs:573-589` verbatim — Key(Enter)→submit, Key(Esc)→cancel, Key(Backspace)→backspace, Key(Char(c))→push_char, Paste(text)→push_char per char, everything else swallowed (`PaneOutcome::Consumed`). The inline block at `mod.rs:573-591` is deleted; `app.request_redraw()` (`mod.rs:590`) is preserved by the existing `request_immediate_redraw` on the input path (`mod.rs:595-…`).
4. **Priority preservation:** today secret_prompts pre-empts ALL modals (checked before `input::handle_event`). The reconcile pushes it on TOP of whatever is open, so `stack.top()` reproduces the pre-emption exactly. If a modal is opened *while* a prompt is active — impossible via keys (prompt swallows them) and no async path opens modals unprompted (verified: all four open sites `mod.rs:677/680/686/700` + `mod.rs:1829` are InputAction/CommandAction-driven, i.e. key-originated) — the invariant still holds because `reconcile` runs after `poll_requests` every tick.
5. **The z-order side effect (flag for Gate 2):** stack-driven draw (P7.8) will paint SecretPrompt LAST (topmost) whenever it coexists with a modal, whereas today `draw.rs:1517` paints it under `draw.rs:1567+`. Input-wise nothing changes (prompt already wins). Draw-wise this is a bug fix — the box you're typing a password into becomes visible. It is the ONE deliberate pixel-level divergence in all of P7; it cannot occur in any existing harness snapshot (no scenario opens a modal mid-prompt), so the P4 suite stays byte-identical, but I mark it for explicit human sign-off in §8.

---

## 6. Behavior-preservation SHIM strategy (the risk)

**Principle: the stack is an index, not an owner.** Modal state stays in the existing `Option<…State>` fields (`app.rs:83-89`) for the entire migration. Push/pop only ever accompanies the existing `= Some(…)` / `= None` assignments. Nothing about state lifetime, draw order, or the `InputAction` async dispatch changes until the final step. That is what makes each intermediate state byte-identical under the P4 harness.

**The shim (lands in P7.3):** at the top of `handle_event` (`input.rs:44`), BEFORE the chain:

```rust
// P7 SHIM — stack-routed panes dispatch here; unmigrated panes fall through
// to the legacy if-let chain below. A pane is EITHER stack-routed OR
// chain-routed, never both. Delete this comment block in P7.8.
match app.modal_stack.top() {
    PaneId::Chat => { /* fall through: chain, then base handling at input.rs:209+ */ }
    migrated_pane => return route_pane(migrated_pane, event, app, …),
}
```

**Iron rules for every migration step (P7.4–P7.8):**
1. One modal per commit. The commit moves, atomically: (a) all open sites push, (b) all close sites pop, (c) the chain arm is DELETED. Never leave a modal both pushed and chain-matched — the chain arm would shadow or double-handle.
2. `debug_assert_stack_sync(app)` (§3 invariant 4) runs after every `handle_event` in debug/test builds — any missed open/close site fails the harness loudly instead of misrouting silently.
3. Full harness suite (`harness_scenarios` 24 scenarios + `harness_smoke`) green with UNCHANGED snapshots after every step. Draw code is untouched until P7.8, so any frame diff before P7.8 is by definition a routing regression.

**Why the chain and the stack can coexist safely — the ordering proof (from code):**
Modal coexistence is limited. Verified open sites: models `mod.rs:677`, settings `mod.rs:680`, plugins `mod.rs:686` + `mod.rs:1829` (marketplace-from-settings), help_find `mod.rs:700` + `input.rs:555`. Since every open flows through a slash command or a settings outcome, and open modals swallow slash input, the ONLY coexisting pairs are:
- **settings + plugins** (marketplace, `InputOutcome::OpenPluginsMarketplace` → `input.rs:198-200` → `mod.rs:1825-1834`)
- **settings + plugin editor** (`ActiveEditor::PluginCustom`, `input.rs:104-118`)
- **secret prompt + anything** (async, pre-empts all)

Migration order help_find → models → plugins → settings → secret_prompt is chosen so that at every intermediate state, the coexisting pair is handled correctly:

| After step | Stack-routed | Chain-routed | Coexistence check |
|---|---|---|---|
| P7.3 | none (stack permanently empty) | all four + inline secret block | trivially identical — the shim's `PaneId::Chat` arm is the only path taken |
| P7.4 | help_find | models, plugins, settings | help_find never coexists with anything — safe |
| P7.5 | help_find, models | plugins, settings | models never coexists — safe |
| P7.6 | + plugins | settings | settings(chain) + plugins(stack): plugins pushed ⇒ `top()==Plugins` wins, exactly matching today's chain order (plugins arm `input.rs:88` above settings arm `input.rs:103`). Plugins pops ⇒ stack empty ⇒ chain routes settings. Identical fallback. |
| P7.7 | + settings, PluginEditor | none (chain empty) | settings pushes Plugins (marketplace) or PluginEditor above itself — explicit two-deep stack replicating chain order / edit_mode precedence |
| P7.8 | + secret_prompt; chain + `mod.rs:573-591` block DELETED; draw order stack-driven | — | single routing point achieved |

**Chain-arm deletion is what keeps this honest:** after P7.6, `grep "app.plugins.as_mut()" input.rs` must return nothing; the P7.8 exit criterion is grep-zero for all four (`help_find|models|plugins|settings`) chain patterns plus the `secret_prompts.is_active()` check gone from `mod.rs`.

**Draw-order shim:** `draw.rs:1567-1578` keeps its hardcoded order until P7.8, reading the same Option fields it reads today — pixel-identical through P7.7. P7.8 replaces it with iteration over `model.modal_order` (a `Vec<PaneId>` snapshot of the stack added to `RenderModel`), dispatching to the same four render fns. For every reachable state the stack order equals the hardcoded order (settings below plugins below…, proof: the only multi-modal states are the pairs above, and push order matches paint order) — EXCEPT the secret-prompt-over-modal case flagged in §5.5.

**Rollback story:** each step is a small, revertable commit on the P7 branch; because the chain still exists until P7.8, reverting step N restores routing for that modal with zero interaction with steps <N.

---

## 7. Refined sub-task sequence P7.1 → P7.9 (all verified against code; S/M each)

Sequencing constraint from the collision map: P7.3–P7.8 all touch `input.rs`/`app.rs` and run STRICTLY SEQUENTIALLY (Chain 1). P7.1 ∥ P7.2 are parallel-safe (new files).

**P7.1 — `PaneId` + `ModalStack` (new module, unwired) — S**
- Files: `src/tui/focus.rs` (new), `src/tui/mod.rs` (one-line `mod focus;` next to `mod lightbox;` at `mod.rs:15`).
- Content: §2 enum (7 variants incl. `SecretPrompt`, `PluginEditor`), §3 stack + `PaneOutcome`, unit tests: push/pop order, `top()==Chat` when empty, duplicate-push no-op, `remove()` mid-stack.
- Verify: `cargo test -p synaps-tui focus::`

**P7.2 — `FocusManager` (same module, unwired) — S**
- Files: `src/tui/focus.rs`.
- Content: §4 rings; tests: wrap traversal, hidden-slot skip, per-pane persistence across push/pop.
- Verify: `cargo test -p synaps-tui focus::manager`

**P7.3 — Wire stack into App + shim in `handle_event` — M** ⛔ GATE 1 before proceeding
- Files: `src/tui/app.rs` (add `modal_stack: ModalStack` + init in `App::new` near `app.rs:189`), `src/tui/input.rs` (shim at :44-53, per §6), `src/tui/focus.rs` (`debug_assert_stack_sync`).
- Stack is wired but permanently empty — zero behavior change by construction.
- Verify: `cargo test -p synaps-tui --test harness_scenarios && cargo test -p synaps-tui --test harness_smoke` (all 24 scenarios, unchanged snapshots).

**P7.4 — Migrate help_find — S**
- Files: `input.rs` (delete arm :54-66, add pane handler), `mod.rs:700-704` + `input.rs:555-…` (push at both open sites), `app.rs`.
- Verify: `cargo test -p synaps-tui --test harness_scenarios scenario_08` + full suite.

**P7.5 — Migrate models — S**
- Files: `input.rs` (delete arm :68-86; `Close`→`Pop`, `Apply`→`PopThen(ModelsApply)`, `ExpandProvider`→`Action(…)`), `mod.rs:677` (push). Note the async expanded-list arm `mod.rs:304` mutates `app.models` directly — no stack interaction needed (modal already open); leave untouched.
- Verify: `cargo test -p synaps-tui --test harness_scenarios scenario_06` + full suite.

**P7.6 — Migrate plugins + Focus{Left,Right} → FocusManager — M**
- Files: `input.rs` (delete arm :88-101), `mod.rs:686` and `mod.rs:1825-1834` (push; marketplace-from-settings becomes the first real two-deep push), `plugins/state.rs:26` (Focus backed by ring), `plugins/input.rs`, `focus.rs`.
- `InputOutcome::Close` from marketplace pops back to settings — assert stack depth 2→1 in a new test.
- Verify: `cargo test -p synaps-tui --test harness_scenarios scenario_07 && cargo test -p synaps-tui plugins::` + full suite.

**P7.7 — Migrate settings (hardest) — M**
- Files: `input.rs` (delete the 105-line arm :103-208 — the 12 InputOutcome match moves verbatim into the pane handler, synchronous config writes at :143-175 stay exactly where they execute), `mod.rs:680` (push), `mod.rs:1654-1683` (PluginEditorOpen success path additionally pushes `PaneId::PluginEditor`), `settings/mod.rs:161` Focus → ring, `settings/input.rs`.
- `ActiveEditor::PluginCustom` Esc (`input.rs:106-108`) becomes pop of PluginEditor (clears `edit_mode`, settings stays open — identical to today).
- Paste-into-editor (`input.rs:120-134`) moves into the pane handler unchanged.
- Verify: `cargo test -p synaps-tui --test harness_scenarios scenario_05 && cargo test -p synaps-tui settings::` + full suite.

**P7.8 — secret_prompts fold-in + stack-driven draw + delete chain — M** ⛔ GATE 2 before merge
- Files: `mod.rs` (delete :573-591 inline block; queue → `app.secret_prompts`; reconcile after `poll_requests` at :423; drop the `&secret_prompts` args at :242/:749/:1466/:1921/:1947), `app.rs`, `input.rs` (SecretPrompt pane handler; delete shim comment — chain is now empty), `draw.rs` (`:484` signature, `:1567-1578` → stack-order iteration), `render_model.rs` (`modal_order: Vec<PaneId>`), `testing.rs` (:77/:116/:189/:229 use App's queue).
- Exit criteria: `grep -c "if let Some(state) = app\.\(help_find\|models\|plugins\|settings\)" src/tui/input.rs` == 0; `grep -c "secret_prompts.is_active()" src/tui/mod.rs` == 0 (except tick-gate at :411, which reads the App field).
- Verify: full `harness_scenarios` + `harness_smoke`, snapshots unchanged; NEW secret-prompt scenarios (Enter/Esc/Backspace/paste) — now possible for the first time.

**P7.9 — P7 harness suite — S**
- Files: `tests/harness_focus.rs` (new), `src/tui/testing.rs` (read-only accessors: `modal_stack_depth()`, `top_pane()`, `activate_secret_prompt()` test hook).
- ≥6 scenarios: settings→marketplace depth-2 + one-Esc-one-level unwind; settings→PluginEditor nesting; Tab/BackTab traversal in plugins and settings; input-does-NOT-reach-occluded-pane; secret-prompt-over-modal (the §5.5 divergence, snapshot-blessed at Gate 2); synthetic-modal-without-touching-input.rs extensibility proof.
- Verify: `cargo test -p synaps-tui --test harness_focus`

---

## 8. Human-review GATES (Haseeb)

- **GATE 1 — after P7.3, before P7.4 (BLOCKING):** review THIS document, specifically §6 (shim rules, migration order proof) and §5 (fold-in decision). P7.3's diff is the shim made real; nothing downstream is cheap to redo if the shim shape is wrong. Also confirm the two scope calls: gamba stays excluded (`mod.rs:570` gate untouched) and models' expanded lightbox stays internal.
- **GATE 2 — after P7.8, before merge / before P12 starts (BLOCKING):** whole-branch review at the point of maximum regression surface (`mod.rs` + `draw.rs` + `render_model.rs` in one task). Explicit sign-offs required: (a) the §5.5 z-order divergence (secret prompt now paints ABOVE modals — intended bug fix, needs a blessed snapshot); (b) chain deletion grep-proofs; (c) the `testing.rs` queue unification.
- **Soft checkpoint — after P7.6:** first two-deep stack in production paths (marketplace-from-settings). Async review of the depth-2 test output is enough; not blocking.

---

## 9. Autonomous-safety call (honest, per sub-task)

| Task | Call | Rationale |
|---|---|---|
| P7.1 | **SAFE-TO-BUILD-AUTONOMOUSLY** | New file + 1-line mod decl; unwired; unit-tested in isolation. |
| P7.2 | **SAFE-TO-BUILD-AUTONOMOUSLY** | Same — pure data structures, zero call sites. |
| P7.3 | **NEEDS-REVIEW-FIRST** (build on branch OK, GATE 1 before P7.4) | Touches the input-routing hotspot of the runtime Haseeb boots daily. The diff itself is provably inert (stack always empty) so *building* it autonomously is fine; *proceeding past it* without review is not — every later task inherits its shape. |
| P7.4 | SAFE-TO-BUILD-AUTONOMOUSLY *after Gate 1* | Simplest modal, no coexistence, scenario_08 pins it, revert-in-isolation. |
| P7.5 | SAFE-TO-BUILD-AUTONOMOUSLY *after Gate 1* | Same shape as P7.4; async arm at `mod.rs:304` untouched. |
| P7.6 | SAFE-TO-BUILD-AUTONOMOUSLY *after Gate 1*, soft checkpoint after | First depth-2 case, but fully pinned by scenario_07 + the new depth test; chain fallback still live for settings. |
| P7.7 | **NEEDS-REVIEW-FIRST** (of the diff, before landing on branch) | 105-line arm with synchronous config WRITES (`input.rs:143-175` — real file-system side effects on the user's config) plus theme mutation (:189-197). A misrouted outcome here corrupts config, not just pixels. Harness can't fully cover the write paths. |
| P7.8 | **NEEDS-REVIEW-FIRST** (GATE 2) | mod.rs + draw.rs + render_model.rs + testing.rs in one pass; deletes the safety net (chain); contains the one deliberate behavior divergence (§5.5). Maximum regression surface by design. |
| P7.9 | SAFE-TO-BUILD-AUTONOMOUSLY | Tests only; merges with the branch after Gate 2. |

---

## Summary

- **Modal/overlay inventory: 10 entities found** — 4 chain-routed modals (help_find, models, plugins, settings), 1 nested modal (plugin custom editor), 1 loop-inline modal (**secret_prompts** — the one Zero missed), plus toasts / gamba / sidecars / lightbox-helpers classified out with reasons. Two NEW findings beyond the plan: the harness cannot test secret_prompts at all today, and secret_prompts has an input-vs-draw z-order inconsistency.
- **secret_prompts: FOLDS INTO the ModalStack** via queue-on-App + tick-time reconcile (§5) — no exception carved out; the async oneshot plumbing is untouched.
- **7 PaneIds**, stack-as-index (state stays in `app.rs:83-89` Options), chain coexists with stack until P7.8 with a per-step coexistence proof (§6).
- **9 sub-tasks** (P7.1–P7.9), all S/M, strictly sequential P7.3→P7.8 on the input.rs/app.rs spine.
- **Gates: GATE 1 after P7.3** (shim design — this doc) and **GATE 2 after P7.8** (whole branch, incl. the z-order divergence sign-off); P7.7 additionally needs diff review because it moves config-writing code.

*— End of P7 build design. No repo files modified; no builds run.*
