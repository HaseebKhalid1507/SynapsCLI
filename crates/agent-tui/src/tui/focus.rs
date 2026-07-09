#![allow(dead_code)] // UNWIRED foundation — remove when P7.3 wires ModalStack into App/input.rs.
//! Focus + modal-stack foundation for the TUI (P7.1 + P7.2).
//!
//! This module is the **unwired** foundation for the P7 FocusManager /
//! ModalStack migration described in
//! `docs/plans/2026-07-09-p7-focusmanager-modalstack-design.md`. It ships as a
//! self-contained set of data structures with unit tests and is deliberately
//! **not yet referenced** by any call site — wiring lands in P7.3+ (App owns
//! the stack, `input.rs` grows the routing shim). Nothing here mutates App
//! state; the stack is designed to be an *index over* the existing
//! `Option<…State>` fields on App, never a new owner (§6 behavior-preservation).
//!
//! Design references: §2 (`PaneId`), §3 (`ModalStack` + `PaneOutcome`),
//! §4 (`FocusManager`).

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// §2 — PaneId
// ---------------------------------------------------------------------------

/// Typed identity for every input-receiving surface. App-grade, not
/// framework-grade: a closed enum, no id-paths, no arenas (Yoru scoping).
///
/// `PaneId` is `Copy` and carries **no state**. Modal state stays exactly where
/// it is today (the `Option<…State>` fields on App, `app.rs:83-89`). The stack
/// is an *index over* open modals, not a new owner — this is the core of the
/// behavior-preservation strategy (§6).
///
/// # Deliberate NON-members (each with its reason, per §2)
/// - **Toast** — never focusable, purely passive (`draw.rs:1564`).
/// - **Gamba** — terminal handoff, gated upstream of the event loop
///   (`mod.rs:570`); the stack never sees events while it runs.
/// - **Sidecars / lightbox helpers** — render-only.
/// - **Models expanded lightbox** — internal view state of `ModelsModalState`
///   (`models/mod.rs:191`); its keys are already dispatched inside
///   `models::handle_event`. Promoting it would change behavior for zero
///   benefit. Revisit only if it ever needs to outlive the models modal.
/// - **Settings' other `ActiveEditor` variants** (Text/Picker/ApiKey/…) — stay
///   internal to the Settings pane handler; they are field editors, not panes.
///   Only `PluginCustom` is promoted (see `PluginEditor`), because it already
///   routes through the main loop as a distinct machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PaneId {
    /// Base pane — chat transcript + input box. Never on the stack;
    /// it is the implicit bottom (empty stack ⇒ Chat gets input).
    Chat,
    /// `/help find` lightbox (`app.rs:89`). Single search field; no Focus enum.
    HelpFind,
    /// `/model` / `/models` modal (`app.rs:87`). The expanded-models lightbox
    /// stays INTERNAL to Models (`models/mod.rs:191`).
    Models,
    /// `/plugins` marketplace (`app.rs:85`). Has `Focus { Left, Right }`.
    Plugins,
    /// `/settings` modal (`app.rs:83`). Has `Focus { Left, Right }`.
    Settings,
    /// `ActiveEditor::PluginCustom` promoted from `settings.edit_mode`
    /// special-case (`input.rs:104-118`) to a real stack level above Settings
    /// (`settings/mod.rs:196+`).
    PluginEditor,
    /// Secret / masked prompt queue (`mod.rs:170` queue → moves to App, §5).
    SecretPrompt,
}

// ---------------------------------------------------------------------------
// §3 — PaneOutcome
// ---------------------------------------------------------------------------

/// What a stack-routed pane handler tells the routing layer to do after
/// consuming an event. Pane handlers are thin adapters from today's per-modal
/// `InputOutcome` types to this shape (§3).
///
/// `InputAction` (`input.rs:10`) is kept **unchanged**; the async main-loop
/// dispatch (`mod.rs`) is out of P7's blast radius. The `Action` /
/// `PopThen` variants defer to that existing loop verbatim.
///
/// Note: `InputAction` is `pub(super)` in `super::input`; because `focus` is a
/// sibling submodule of `input` under `tui`, the path `super::input::InputAction`
/// is reachable here without any visibility change — no gating required.
pub(crate) enum PaneOutcome {
    /// Stay open, nothing for the loop.
    Consumed,
    /// Close me (routing clears my `Option` field + pops the stack).
    Pop,
    /// Defer to the async loop (PluginsOutcome, ModelsApply, …).
    Action(super::input::InputAction),
    /// Close AND defer (e.g. models Apply → `PopThen(ModelsApply(..))`).
    PopThen(super::input::InputAction),
}

// ---------------------------------------------------------------------------
// §3 — ModalStack
// ---------------------------------------------------------------------------

/// Ordered set of currently-open modal panes, bottom → top.
///
/// # Contracts (§3; each tested below)
/// 1. **Top-of-stack gets input** — routing dispatches on [`ModalStack::top`];
///    nothing below the top ever sees an event (hidden-widget rule). Occluded
///    panes keep their state but are input-dead.
/// 2. **Escape pops uniformly** — each pane maps its close action to
///    [`PaneOutcome::Pop`]; the routing layer performs the pop AND clears the
///    matching `Option` field. One Esc = one level.
/// 3. **Duplicate-push rejected** — `debug_assert!` + no-op in release
///    (matches today: opening `/settings` while settings is open is impossible).
///    Pushing [`PaneId::Chat`] is likewise rejected — Chat is the implicit
///    bottom, never stored.
/// 4. **Sync invariant** — `stack.contains(X) ⇔ corresponding app field
///    is_some()`. Enforced in P7.3 by `debug_assert_stack_sync` after every
///    `handle_event` / reconcile. (See the stub at the bottom of this module —
///    the App cross-check is wired once App owns the stack.)
/// 5. **Empty stack ⇒ Chat** — the base pane is not stored; [`ModalStack::top`]
///    returns [`PaneId::Chat`] for an empty stack.
#[derive(Debug, Default)]
pub(crate) struct ModalStack {
    /// Bottom → top. Draw order is bottom-up; input goes to the top.
    stack: Vec<PaneId>,
}

impl ModalStack {
    /// Create an empty stack (base `Chat` pane is implicit, never stored).
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a pane onto the top of the stack.
    ///
    /// Rejects [`PaneId::Chat`] (the implicit bottom) and duplicate pushes:
    /// both are `debug_assert!` failures in debug builds and silent no-ops in
    /// release (§3 contract 3).
    pub fn push(&mut self, id: PaneId) {
        debug_assert!(
            id != PaneId::Chat,
            "ModalStack::push(Chat) — Chat is the implicit bottom, never stored"
        );
        debug_assert!(
            !self.stack.contains(&id),
            "ModalStack::push({id:?}) — duplicate push (pane already open)"
        );
        if id == PaneId::Chat || self.stack.contains(&id) {
            return;
        }
        self.stack.push(id);
    }

    /// Pop the top pane, if any. Returns the removed pane.
    pub fn pop(&mut self) -> Option<PaneId> {
        self.stack.pop()
    }

    /// Remove a specific pane from anywhere in the stack (out-of-order close,
    /// e.g. a secret prompt resolved by cancel-all). No-op if not present.
    /// Panes above the removed one keep their relative order and shift down.
    pub fn remove(&mut self, id: PaneId) {
        if let Some(pos) = self.stack.iter().position(|&p| p == id) {
            self.stack.remove(pos);
        }
    }

    /// The pane that currently receives input. Returns [`PaneId::Chat`] when the
    /// stack is empty (§3 contract 5).
    pub fn top(&self) -> PaneId {
        self.stack.last().copied().unwrap_or(PaneId::Chat)
    }

    /// Whether `id` is currently open (anywhere in the stack).
    pub fn contains(&self, id: PaneId) -> bool {
        self.stack.contains(&id)
    }

    /// Iterate panes bottom → top — i.e. draw order (topmost paints last).
    pub fn iter_bottom_up(&self) -> impl Iterator<Item = PaneId> + '_ {
        self.stack.iter().copied()
    }

    /// Number of open modal panes (does not count the implicit `Chat` base).
    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    /// Whether no modal panes are open (input falls through to `Chat`).
    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }
}

// ---------------------------------------------------------------------------
// §4 — FocusManager
// ---------------------------------------------------------------------------

/// A focusable slot within a pane: a small id + a visibility flag.
///
/// Hidden (`visible == false`) slots are skipped by ring traversal so that
/// conditionally-shown widgets never trap focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FocusSlot {
    /// Stable per-pane slot identity (e.g. Left ↦ 0, Right ↦ 1).
    id: SlotId,
    /// Whether this slot currently participates in traversal.
    visible: bool,
}

impl FocusSlot {
    /// A visible slot with the given id.
    pub fn new(id: u8) -> Self {
        Self {
            id: SlotId(id),
            visible: true,
        }
    }

    /// This slot's stable id value.
    pub fn id(&self) -> u8 {
        self.id.0
    }

    /// Whether this slot is currently visible / traversable.
    pub fn is_visible(&self) -> bool {
        self.visible
    }
}

/// A small `u8` newtype identifying a focusable slot within a single pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SlotId(pub(crate) u8);

/// Per-pane focus ring: the ordered focusable slots plus the current index.
///
/// Traversal ([`FocusRing::next`] / [`FocusRing::prev`]) wraps around the ends
/// and skips `visible == false` slots. If no slot is visible, the current index
/// is left untouched.
#[derive(Debug, Clone, Default)]
pub(crate) struct FocusRing {
    slots: Vec<FocusSlot>,
    current: usize,
}

impl FocusRing {
    /// Build a ring from a sequence of slots. `current` starts at 0.
    pub fn new(slots: Vec<FocusSlot>) -> Self {
        Self { slots, current: 0 }
    }

    /// Build a ring of `count` visible slots with ids `0..count`.
    pub fn of_len(count: u8) -> Self {
        Self::new((0..count).map(FocusSlot::new).collect())
    }

    /// The currently-focused slot, if the ring has any slots.
    /// Read by draw code for highlight styling.
    pub fn current(&self) -> Option<FocusSlot> {
        self.slots.get(self.current).copied()
    }

    /// Move focus to the next visible slot (Tab), wrapping.
    pub fn next(&mut self) {
        self.step(true);
    }

    /// Move focus to the previous visible slot (Shift-Tab / BackTab), wrapping.
    pub fn prev(&mut self) {
        self.step(false);
    }

    /// Set the visibility of the slot with id `id`. Returns `true` if a slot
    /// matched. If hiding the currently-focused slot, focus is advanced to the
    /// next visible slot so `current()` never points at a hidden widget.
    pub fn set_visible(&mut self, id: u8, visible: bool) -> bool {
        let mut matched = false;
        for slot in &mut self.slots {
            if slot.id.0 == id {
                slot.visible = visible;
                matched = true;
            }
        }
        if matched
            && !visible
            && self
                .slots
                .get(self.current)
                .is_some_and(|s| s.id.0 == id)
        {
            self.step(true);
        }
        matched
    }

    fn step(&mut self, forward: bool) {
        let n = self.slots.len();
        if n == 0 {
            return;
        }
        if !self.slots.iter().any(|s| s.visible) {
            return;
        }
        let mut idx = self.current;
        for _ in 0..n {
            idx = if forward {
                (idx + 1) % n
            } else {
                (idx + n - 1) % n
            };
            if self.slots[idx].visible {
                self.current = idx;
                return;
            }
        }
    }
}

/// Per-pane focus rings with occlusion-surviving persistence.
///
/// Focus state **survives occlusion**: push `Plugins` over `Settings`, pop back,
/// and Settings' Left/Right position is exactly where you left it — matching
/// today, where `Focus` lives inside the retained `SettingsState` (§4). Rings
/// are keyed by [`PaneId`] and are independent of the [`ModalStack`]; nothing
/// about push/pop mutates a ring.
///
/// Deliberately small: exactly two per-modal focus enums exist today
/// (`plugins/state.rs:26`, `settings/mod.rs:161`), both `{ Left, Right }`.
/// help_find / models / secret_prompt get single-slot rings registered purely
/// for uniformity. Explicitly NOT built: no global focus tree, no id-paths, no
/// focus events/observers, no inter-pane Tab (Tab never leaves the active pane).
#[derive(Debug, Default)]
pub(crate) struct FocusManager {
    rings: HashMap<PaneId, FocusRing>,
}

impl FocusManager {
    /// Empty manager — panes register their rings lazily.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) the focus ring for `pane`.
    pub fn register(&mut self, pane: PaneId, ring: FocusRing) {
        self.rings.insert(pane, ring);
    }

    /// Register a simple ring of `count` visible slots for `pane`.
    pub fn register_slots(&mut self, pane: PaneId, count: u8) {
        self.register(pane, FocusRing::of_len(count));
    }

    /// Whether `pane` has a registered ring.
    pub fn is_registered(&self, pane: PaneId) -> bool {
        self.rings.contains_key(&pane)
    }

    /// Tab within `pane` — advance to the next visible slot (wraps). No-op if
    /// `pane` is unregistered.
    pub fn next(&mut self, pane: PaneId) {
        if let Some(ring) = self.rings.get_mut(&pane) {
            ring.next();
        }
    }

    /// Shift-Tab / BackTab within `pane` — previous visible slot (wraps).
    /// No-op if `pane` is unregistered.
    pub fn prev(&mut self, pane: PaneId) {
        if let Some(ring) = self.rings.get_mut(&pane) {
            ring.prev();
        }
    }

    /// The currently-focused slot in `pane`, read by draw code for highlight
    /// styling. `None` if `pane` is unregistered or its ring is empty.
    pub fn current(&self, pane: PaneId) -> Option<FocusSlot> {
        self.rings.get(&pane).and_then(FocusRing::current)
    }

    /// Toggle a slot's visibility within `pane` (hidden slots are skipped by
    /// traversal). Returns `true` if `pane` and the slot both matched.
    pub fn set_visible(&mut self, pane: PaneId, slot_id: u8, visible: bool) -> bool {
        self.rings
            .get_mut(&pane)
            .is_some_and(|ring| ring.set_visible(slot_id, visible))
    }
}

// ---------------------------------------------------------------------------
// §3 invariant 4 — sync tripwire (stub; App cross-check wired in P7.3)
// ---------------------------------------------------------------------------

/// Debug-only stack/app-field sync tripwire (§3 contract 4).
///
/// Cross-checks the [`ModalStack`] against `App`. The invariant is
/// `stack.contains(X) ⇔ X's backing App state is present`; it is **extended
/// per-modal as each pane is migrated (P7.4+)**.
///
/// In **P7.3** no modal is migrated yet — the stack is wired but permanently
/// empty (every open modal is still routed by the legacy `input.rs` chain and
/// never pushes). So the P7.3 invariant is simply: **the stack is empty**.
/// Each migration step adds one `⇔` clause here (e.g.
/// `stack.contains(HelpFind) == app.help_find.is_some()`), and the tripwire
/// fails the harness loudly if any open/close site is missed.
///
/// Always also checks the stack's internal invariants (no `Chat`, no
/// duplicates) via [`debug_assert_stack_internal`].
#[cfg(debug_assertions)]
pub(crate) fn debug_assert_stack_sync(app: &super::app::App) {
    // P7.4: HelpFind membership on the stack must exactly mirror its backing
    // App field (§3 contract 4).
    debug_assert_eq!(
        app.modal_stack.contains(PaneId::HelpFind),
        app.help_find.is_some(),
        "stack sync (P7.4): modal_stack.contains(HelpFind)={} but help_find.is_some()={} \
         — a push/pop site was missed",
        app.modal_stack.contains(PaneId::HelpFind),
        app.help_find.is_some()
    );

    // P7.5: Models membership on the stack must exactly mirror `app.models`.
    debug_assert_eq!(
        app.modal_stack.contains(PaneId::Models),
        app.models.is_some(),
        "stack sync (P7.5): modal_stack.contains(Models)={} but models.is_some()={} \
         — a push/pop site was missed",
        app.modal_stack.contains(PaneId::Models),
        app.models.is_some()
    );

    // Plugins membership on the stack must exactly mirror `app.plugins`.
    // As of P7.7 settings is ALSO a stack member, so marketplace-from-settings
    // is a real two-deep stack [Settings, Plugins]; this clause cross-checks the
    // Plugins level, the Settings clause below checks the Settings level.
    debug_assert_eq!(
        app.modal_stack.contains(PaneId::Plugins),
        app.plugins.is_some(),
        "stack sync (P7.6): modal_stack.contains(Plugins)={} but plugins.is_some()={} \
         — a push/pop site was missed",
        app.modal_stack.contains(PaneId::Plugins),
        app.plugins.is_some()
    );

    // P7.7: Settings membership on the stack must exactly mirror `app.settings`.
    // Settings is now a TRUE stack member; marketplace-from-settings therefore
    // becomes a real two-deep stack [Settings, Plugins], and the nested
    // PluginCustom editor a [.., Settings, PluginEditor] two-deep stack.
    debug_assert_eq!(
        app.modal_stack.contains(PaneId::Settings),
        app.settings.is_some(),
        "stack sync (P7.7): modal_stack.contains(Settings)={} but settings.is_some()={} \
         — a push/pop site was missed",
        app.modal_stack.contains(PaneId::Settings),
        app.settings.is_some()
    );

    // P7.7: PluginEditor is the `ActiveEditor::PluginCustom` editor promoted to
    // a real stack level above Settings (§2). Its membership must mirror
    // edit_mode == Some(PluginCustom); it can only be active while Settings is
    // open. Pushed at PluginEditorOpen's Ok branch; popped at the Esc path
    // (route_settings) and both commit paths (ConfigWrite / InvokeCommand).
    let plugin_editor_active = matches!(
        app.settings.as_ref().map(|st| &st.edit_mode),
        Some(Some(super::settings::ActiveEditor::PluginCustom { .. }))
    );
    debug_assert_eq!(
        app.modal_stack.contains(PaneId::PluginEditor),
        plugin_editor_active,
        "stack sync (P7.7): modal_stack.contains(PluginEditor)={} but PluginCustom \
         edit_mode active={} — a push/pop site was missed",
        app.modal_stack.contains(PaneId::PluginEditor),
        plugin_editor_active
    );

    // Every other pane is still chain-routed (unmigrated) ⇒ it must NEVER be on
    // the stack yet. HelpFind, Models, Plugins, Settings and PluginEditor are
    // the only permitted members; the stack is therefore some ordering of a
    // subset of {HelpFind, Models, Plugins, Settings, PluginEditor}. Only
    // SecretPrompt remains chain/inline-routed (folded in at P7.8).
    for pane in app.modal_stack.iter_bottom_up() {
        debug_assert!(
            matches!(
                pane,
                PaneId::HelpFind
                    | PaneId::Models
                    | PaneId::Plugins
                    | PaneId::Settings
                    | PaneId::PluginEditor
            ),
            "stack sync (P7.7): unmigrated pane {pane:?} found on the ModalStack \
             — only HelpFind, Models, Plugins, Settings and PluginEditor are \
             stack-routed so far"
        );
    }

    debug_assert_stack_internal(&app.modal_stack);
}

/// Internal stack-only invariants (no `Chat`, no duplicates), independent of
/// `App`. Split out so it can be exercised by unit tests that build a bare
/// [`ModalStack`] without an `App`.
#[cfg(debug_assertions)]
pub(crate) fn debug_assert_stack_internal(stack: &ModalStack) {
    debug_assert!(
        !stack.contains(PaneId::Chat),
        "stack sync: Chat must never be stored on the ModalStack"
    );
    let mut seen: Vec<PaneId> = Vec::with_capacity(stack.depth());
    for pane in stack.iter_bottom_up() {
        debug_assert!(
            !seen.contains(&pane),
            "stack sync: duplicate {pane:?} on the ModalStack"
        );
        seen.push(pane);
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- ModalStack (§3) ---------------------------------------------------

    #[test]
    fn push_pop_preserves_lifo_order() {
        let mut s = ModalStack::new();
        s.push(PaneId::Settings);
        s.push(PaneId::Plugins);
        s.push(PaneId::HelpFind);
        assert_eq!(s.depth(), 3);
        assert_eq!(s.top(), PaneId::HelpFind);

        // bottom -> top draw order
        let order: Vec<PaneId> = s.iter_bottom_up().collect();
        assert_eq!(
            order,
            vec![PaneId::Settings, PaneId::Plugins, PaneId::HelpFind]
        );

        assert_eq!(s.pop(), Some(PaneId::HelpFind));
        assert_eq!(s.pop(), Some(PaneId::Plugins));
        assert_eq!(s.top(), PaneId::Settings);
        assert_eq!(s.pop(), Some(PaneId::Settings));
        assert_eq!(s.pop(), None);
    }

    #[test]
    fn top_is_chat_when_empty() {
        let s = ModalStack::new();
        assert!(s.is_empty());
        assert_eq!(s.depth(), 0);
        assert_eq!(s.top(), PaneId::Chat);
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn duplicate_push_is_noop_release() {
        // In release builds the debug_assert is compiled out; the contains-guard
        // makes a duplicate push a true silent no-op (§3 contract 3).
        let mut s = ModalStack::new();
        s.push(PaneId::Models);
        s.push(PaneId::Models);
        assert_eq!(s.depth(), 1);
        assert_eq!(s.top(), PaneId::Models);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn duplicate_push_panics_in_debug() {
        // In debug builds the duplicate-push contract is enforced loudly by the
        // debug_assert! tripwire — verify it actually fires.
        let result = std::panic::catch_unwind(|| {
            let mut s = ModalStack::new();
            s.push(PaneId::Models);
            s.push(PaneId::Models);
        });
        assert!(result.is_err(), "duplicate push must panic in debug builds");
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn push_chat_rejected_release() {
        let mut s = ModalStack::new();
        s.push(PaneId::Chat);
        assert!(s.is_empty());
        assert_eq!(s.top(), PaneId::Chat);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn push_chat_panics_in_debug() {
        let result = std::panic::catch_unwind(|| {
            let mut s = ModalStack::new();
            s.push(PaneId::Chat);
        });
        assert!(result.is_err(), "pushing Chat must panic in debug builds");
    }

    #[test]
    fn remove_mid_stack_keeps_order() {
        let mut s = ModalStack::new();
        s.push(PaneId::Settings);
        s.push(PaneId::Plugins);
        s.push(PaneId::HelpFind);

        // Remove the middle pane.
        s.remove(PaneId::Plugins);
        assert_eq!(s.depth(), 2);
        assert!(!s.contains(PaneId::Plugins));
        let order: Vec<PaneId> = s.iter_bottom_up().collect();
        assert_eq!(order, vec![PaneId::Settings, PaneId::HelpFind]);
        assert_eq!(s.top(), PaneId::HelpFind);

        // Removing an absent pane is a no-op.
        s.remove(PaneId::Models);
        assert_eq!(s.depth(), 2);
    }

    #[test]
    fn contains_reflects_membership() {
        let mut s = ModalStack::new();
        assert!(!s.contains(PaneId::Settings));
        s.push(PaneId::Settings);
        assert!(s.contains(PaneId::Settings));
        s.remove(PaneId::Settings);
        assert!(!s.contains(PaneId::Settings));
    }

    #[test]
    fn stack_sync_tripwire_passes_for_valid_stack() {
        let mut s = ModalStack::new();
        s.push(PaneId::Settings);
        s.push(PaneId::Plugins);
        // Should not panic — no Chat, no duplicates. (P7.3 split the App-facing
        // `debug_assert_stack_sync(&App)` from this bare-stack internal check.)
        debug_assert_stack_internal(&s);
    }

    // --- FocusManager / FocusRing (§4) ------------------------------------

    #[test]
    fn wrap_traversal_forward_and_back() {
        let mut fm = FocusManager::new();
        fm.register_slots(PaneId::Plugins, 2); // Left=0, Right=1

        assert_eq!(fm.current(PaneId::Plugins).map(|s| s.id()), Some(0));
        fm.next(PaneId::Plugins);
        assert_eq!(fm.current(PaneId::Plugins).map(|s| s.id()), Some(1));
        // wrap forward: 1 -> 0
        fm.next(PaneId::Plugins);
        assert_eq!(fm.current(PaneId::Plugins).map(|s| s.id()), Some(0));
        // wrap backward: 0 -> 1
        fm.prev(PaneId::Plugins);
        assert_eq!(fm.current(PaneId::Plugins).map(|s| s.id()), Some(1));
        fm.prev(PaneId::Plugins);
        assert_eq!(fm.current(PaneId::Plugins).map(|s| s.id()), Some(0));
    }

    #[test]
    fn hidden_slot_is_skipped_by_traversal() {
        let mut fm = FocusManager::new();
        // three slots: 0,1,2 — hide the middle one.
        fm.register(
            PaneId::Settings,
            FocusRing::new(vec![
                FocusSlot::new(0),
                FocusSlot::new(1),
                FocusSlot::new(2),
            ]),
        );
        assert!(fm.set_visible(PaneId::Settings, 1, false));

        // 0 -> (skip 1) -> 2
        assert_eq!(fm.current(PaneId::Settings).map(|s| s.id()), Some(0));
        fm.next(PaneId::Settings);
        assert_eq!(fm.current(PaneId::Settings).map(|s| s.id()), Some(2));
        // 2 -> wrap -> 0 (still skipping 1)
        fm.next(PaneId::Settings);
        assert_eq!(fm.current(PaneId::Settings).map(|s| s.id()), Some(0));
        // backward from 0 -> wrap -> 2 (skip 1)
        fm.prev(PaneId::Settings);
        assert_eq!(fm.current(PaneId::Settings).map(|s| s.id()), Some(2));
    }

    #[test]
    fn hiding_current_slot_advances_focus_off_it() {
        let mut fm = FocusManager::new();
        fm.register_slots(PaneId::Settings, 2); // 0,1 ; current=0
        assert!(fm.set_visible(PaneId::Settings, 0, false));
        // current was 0 (now hidden) -> advanced to 1
        assert_eq!(fm.current(PaneId::Settings).map(|s| s.id()), Some(1));
    }

    #[test]
    fn all_hidden_leaves_current_untouched() {
        let mut fm = FocusManager::new();
        fm.register_slots(PaneId::Models, 2);
        fm.set_visible(PaneId::Models, 0, false);
        // hiding slot 0 advanced to 1; now hide 1 as well
        fm.set_visible(PaneId::Models, 1, false);
        let before = fm.current(PaneId::Models).map(|s| s.id());
        fm.next(PaneId::Models);
        assert_eq!(fm.current(PaneId::Models).map(|s| s.id()), before);
    }

    #[test]
    fn per_pane_focus_persists_across_stack_push_pop() {
        // FocusManager rings are independent of the ModalStack: occlusion
        // (push Plugins over Settings) must not disturb Settings' focus.
        let mut fm = FocusManager::new();
        fm.register_slots(PaneId::Settings, 2);
        fm.register_slots(PaneId::Plugins, 2);

        // Move Settings focus to slot 1.
        fm.next(PaneId::Settings);
        assert_eq!(fm.current(PaneId::Settings).map(|s| s.id()), Some(1));

        // Simulate occlusion via the stack.
        let mut stack = ModalStack::new();
        stack.push(PaneId::Settings);
        stack.push(PaneId::Plugins);
        // Fiddle with the occluding pane's focus.
        fm.next(PaneId::Plugins);
        assert_eq!(fm.current(PaneId::Plugins).map(|s| s.id()), Some(1));

        // Pop back to Settings.
        assert_eq!(stack.pop(), Some(PaneId::Plugins));
        assert_eq!(stack.top(), PaneId::Settings);

        // Settings' focus is exactly where we left it.
        assert_eq!(fm.current(PaneId::Settings).map(|s| s.id()), Some(1));
    }

    #[test]
    fn unregistered_pane_is_inert() {
        let mut fm = FocusManager::new();
        assert!(!fm.is_registered(PaneId::HelpFind));
        assert_eq!(fm.current(PaneId::HelpFind), None);
        // next/prev on an unregistered pane are no-ops (must not panic).
        fm.next(PaneId::HelpFind);
        fm.prev(PaneId::HelpFind);
        assert_eq!(fm.current(PaneId::HelpFind), None);
    }

    #[test]
    fn single_slot_ring_traversal_is_stable() {
        // help_find / models / secret_prompt shape: one slot, registered for
        // uniformity. Tab/BackTab must stay put.
        let mut fm = FocusManager::new();
        fm.register_slots(PaneId::HelpFind, 1);
        assert_eq!(fm.current(PaneId::HelpFind).map(|s| s.id()), Some(0));
        fm.next(PaneId::HelpFind);
        assert_eq!(fm.current(PaneId::HelpFind).map(|s| s.id()), Some(0));
        fm.prev(PaneId::HelpFind);
        assert_eq!(fm.current(PaneId::HelpFind).map(|s| s.id()), Some(0));
    }
}
