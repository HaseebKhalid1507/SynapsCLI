# T199.1 — `App` View-Model Boundary Audit & Extraction Spec

**Status:** Spec (doc-only). This document IS the T199.2 work order and the P7 coordination memo.
**Date:** 2026-07-09
**Author:** Zero (Architect)
**Scope:** Read-only analysis of `crates/agent-tui/src/tui/app.rs` and `crates/agent-tui/src/tui/draw.rs`. No code changed.
**Ground truth for "render input":** `draw.rs::build_render_model` (`draw.rs:480-684`), whose final assembly clones live at `draw.rs:609-684`.

---

## 0. Executive read

The `App` struct is not "~100 fields" — the roadmap estimate was loose. The struct at `app.rs:17-129` carries **58 `pub(crate)` fields**. `build_render_model` proves that only a minority are render inputs; the rest are loop bookkeeping, async channel endpoints, or modal states already claimed by P7.

The renderer already declares its own success criterion: `render_frame` (`draw.rs:697+`) states the invariant *"this function takes NO `&App` and accesses NO `App` field."* The seam is therefore already half-built — `RenderModel` is the snapshot. What remains for T199.2 is to stop `build_render_model` from taking `&mut App` and instead feed it a narrow `&ViewInputs`. This audit defines exactly which fields cross that seam.

### Classification counts (the 5-line breakdown)

| Classification | Count | Disposition |
|---|---:|---|
| **render-input** | 22 | 21 move behind ViewModel; 1 (`transcript`) already extracted (P9) |
| **loop-state** | 23 | stay on `App` |
| **channels** | 8 | stay on `App` |
| **modal-state** | 5 | **claimed-by-P7** (ModalStack) — NOT T199's to move |
| **Total** | **58** | — |

Disposition totals: **moves-behind-viewmodel = 21**, **stays = 32** (23 loop + 8 channels + 1 already-extracted transcript), **claimed-by-P7 = 5**.

---

## 1. What `build_render_model` actually consumes

Evidence trail through `draw.rs:480-684`, section by section (§ markers are the source's own comments):

| § | Location | App fields read |
|---|---|---|
| §1 gamba gate | `draw.rs:487-489` | `gamba_child` (read as a suppression gate — *not* copied into `RenderModel`) |
| §2 layout | `draw.rs:493-511` | `subagents`, `input` (via `input_wrap_info`), `active_tasks` |
| §3 transcript window | `draw.rs:520-532` | `transcript`, `spinner_frame`, `streaming`, `agent_name` (last three via `RenderCtx`) |
| §7 subagent snaps | `draw.rs:535-547` | `subagents` |
| §8 sidecar pills | `draw.rs:550-573` | `sidecars` |
| §9 ghost hint | `draw.rs:576-601` | `input` |
| §10 toasts | `draw.rs:605` | `toasts` |
| §11 modals | `draw.rs:608-635` | `settings` + `model_health`, `plugins`, `models`, `help_find` (+ write-back) |
| §14 assemble | `draw.rs:653-683` | `status_text`, `streaming`, `spinner_frame`, `logo_build_t`, `logo_dismiss_t`, `subagents`, `active_tasks`, `input`, `cursor_pos`, `session_cost`, `total_input_tokens`, `total_output_tokens`, `total_cache_read_tokens`, `total_cache_creation_tokens`, `total_cache_write_1h`, `last_turn_context`, `last_turn_context_window`, `toasts`, `settings`, `plugins`, `models`, `help_find` |

**Two sharp findings from the ground truth:**

1. **`total_cache_write_5m` (`app.rs:42`) is NOT a render input.** Only the `_1h` bucket is cloned at `draw.rs:678`. The 5m bucket is cost/accounting state only. It stays.
2. **`gamba_child` is read but never rendered.** It is a full-screen suppression gate (`draw.rs:487`), not render data. It belongs to the modal/full-screen-takeover family (§3 below), not to `ViewInputs`.

---

## 2. Complete field-by-field table

All lines are `crates/agent-tui/src/tui/app.rs`. Disposition legend: **VM** = moves-behind-viewmodel (enters the narrow `ViewInputs` read by `build_render_model`); **STAY** = remains owned by `App` for the loop; **P7** = claimed by P7 ModalStack, out of T199 scope.

| # | Field | Line | Classification | Disposition | Evidence / rationale |
|---:|---|---:|---|---|---|
| 1 | `transcript` | 18 | render-input | STAY (already extracted, P9) | Consumed via `visible_window` `draw.rs:531`; already behind `TranscriptStore`. Feeds VM through `RenderCtx`, not a raw field copy. |
| 2 | `input` | 19 | render-input | VM | `draw.rs:501,576-601,671` (wrap math, ghost hint, `.clone()`). Edit buffer owned by input.rs; VM takes a snapshot. |
| 3 | `cursor_pos` | 22 | render-input | VM | `draw.rs:672`. Copy. |
| 4 | `api_messages` | 23 | loop-state | STAY | Conversation payload sent to API; never rendered. |
| 5 | `streaming` | 24 | render-input (dual-use) | VM | `RenderCtx` `draw.rs:525` + `draw.rs:661`. Also loop control — mirrored into VM, retained for loop. |
| 6 | `input_history` | 25 | loop-state | STAY | history_up/down. |
| 7 | `history_index` | 26 | loop-state | STAY | input navigation cursor. |
| 8 | `input_stash` | 27 | loop-state | STAY | draft stash during history browse. |
| 9 | `tab_cycle` | 32 | loop-state | STAY | tab-completion cycle; input.rs only. |
| 10 | `input_tokens` | 33 | loop-state | STAY | per-turn accounting; not in `RenderModel`. |
| 11 | `output_tokens` | 34 | loop-state | STAY | per-turn accounting; not rendered. |
| 12 | `total_input_tokens` | 35 | render-input | VM | `draw.rs:674`. |
| 13 | `total_output_tokens` | 36 | render-input | VM | `draw.rs:675`. |
| 14 | `total_cache_read_tokens` | 37 | render-input | VM | `draw.rs:676`. |
| 15 | `total_cache_creation_tokens` | 38 | render-input | VM | `draw.rs:677`. |
| 16 | `total_cache_write_5m` | 42 | loop-state | STAY | **Not rendered** — only `_1h` is cloned. Cost accounting. |
| 17 | `total_cache_write_1h` | 43 | render-input | VM | `draw.rs:678`. |
| 18 | `last_turn_context` | 49 | render-input | VM | `draw.rs:679` (context-usage bar). |
| 19 | `last_turn_context_window` | 54 | render-input | VM | `draw.rs:680` (bar denominator). |
| 20 | `api_call_count` | 55 | loop-state | STAY | accounting; not rendered. |
| 21 | `session_cost` | 56 | render-input | VM | `draw.rs:673`. |
| 22 | `session` | 57 | loop-state | STAY | persistence handle; `save_session`. |
| 23 | `agent_name` | 58 | render-input | VM | `RenderCtx` `draw.rs:526`. Effectively const per session — pass by value. |
| 24 | `needs_redraw` | 59 | loop-state | STAY | repaint scheduling flag. |
| 25 | `force_redraw` | 63 | loop-state | STAY | throttle-bypass flag; loop-owned. |
| 26 | `logo_dismiss_t` | 64 | render-input | VM | `draw.rs:665`. Animation clock. |
| 27 | `logo_build_t` | 65 | render-input | VM | `draw.rs:664`. Animation clock. |
| 28 | `subagents` | 67 | render-input | VM | `draw.rs:493,535,663`. |
| 29 | `abort_context` | 70 | loop-state | STAY | injected into next user msg. |
| 30 | `queued_message` | 72 | loop-state | STAY | auto-send buffer. |
| 31 | `input_before_paste` | 74 | loop-state | STAY | paste bookkeeping. |
| 32 | `pasted_char_count` | 75 | loop-state | STAY | paste bookkeeping. |
| 33 | `spinner_frame` | 77 | render-input (dual-use) | VM | `RenderCtx` `draw.rs:524` + `draw.rs:660`. Counter advanced by loop, read by render. |
| 34 | `status_text` | 79 | render-input | VM | `draw.rs:659`. |
| 35 | `gamba_child` | 81 | modal-state (full-screen gate) | **P7** | `draw.rs:487` suppression gate. Not render data; a full-screen external-process takeover. See §3. |
| 36 | `settings` | 83 | modal-state | **P7** | `draw.rs:608-616`. See §3. |
| 37 | `plugins` | 85 | modal-state | **P7** | `draw.rs:617`. See §3. |
| 38 | `models` | 87 | modal-state | **P7** | `draw.rs:618`. See §3. |
| 39 | `help_find` | 89 | modal-state | **P7** | `draw.rs:619-638` (+ write-back). See §3 & §4. |
| 40 | `compact_task` | 91 | loop-state | STAY | async JoinHandle polled in loop. |
| 41 | `pending_events` | 93 | loop-state | STAY | buffered during streaming; merged post-stream. |
| 42 | `model_health` | 95 | render-input (modal-scoped) | VM (coupled to P7 settings) | `draw.rs:611` — cloned only to build the settings `RuntimeSnapshot`. Populated by `ping_rx`. See §3.1. |
| 43 | `ping_print` | 97 | loop-state | STAY | `/ping` output flag. |
| 44 | `ping_pending` | 98 | loop-state | STAY | in-flight ping counter. |
| 45 | `ping_tx` | 100 | channels | STAY | mpsc sender. |
| 46 | `ping_rx` | 101 | channels | STAY | mpsc receiver. |
| 47 | `model_list_tx` | 103 | channels | STAY | mpsc sender. |
| 48 | `model_list_rx` | 104 | channels | STAY | mpsc receiver. |
| 49 | `suppress_paste_until` | 109 | loop-state | STAY | right-click paste TTL guard. |
| 50 | `sidecars` | 114 | render-input | VM | `draw.rs:550-573`. |
| 51 | `active_tasks` | 117 | render-input | VM | `draw.rs:503,666` (`Arc` — refcount bump, not deep clone). |
| 52 | `toasts` | 119 | render-input | VM | `draw.rs:605,681`. |
| 53 | `extension_loader_rx` | 121 | channels | STAY | mpsc receiver. |
| 54 | `extension_loader_tx` | 122 | channels | STAY | mpsc sender. |
| 55 | `extension_loader_running` | 123 | loop-state | STAY | loader progress flag. |
| 56 | `widget_rx` | 125 | channels | STAY | mpsc receiver. |
| 57 | `widget_tx` | 126 | channels | STAY | mpsc sender. |
| 58 | `keybinds` | 128 | loop-state | STAY | `Arc<RwLock<..>>` registry handle; hot-swapped by /settings. |

---

## 3. P7 overlap resolution — modal state is NOT T199's

**The rule: the modal `Option<State>` fields at `app.rs:81-89` migrate under P7's `ModalStack`. T199 does not touch their ownership.** They appear in `build_render_model` §11 (`draw.rs:608-638`) only because the render must project the *currently-open* modal into the snapshot. Once P7.8 lands the stack, `build_render_model` reads "the top modal" from the stack, not five discrete `Option`s off `App`.

**The five fields P7 claims (name them explicitly):**

1. `settings: Option<SettingsState>` — `app.rs:83`
2. `plugins: Option<PluginsModalState>` — `app.rs:85`
3. `models: Option<ModelsModalState>` — `app.rs:87`
4. `help_find: Option<HelpFindState>` — `app.rs:89`
5. `gamba_child: Option<Child>` — `app.rs:81` — *borderline.* It is a full-screen exclusive takeover (render-suppression gate at `draw.rs:487`), semantically a modal even though it holds a process handle rather than a UI state struct. **P7 owns the disposition call.** T199 explicitly does **not** fold it into `ViewInputs`; it is never rendered, only gated on. If P7 decides the `ModalStack` only holds UI-state variants, `gamba_child` stays as a standalone loop gate — but either way it is **out of T199 scope**.

**Coordination contract for T199.2:** the four UI modal states are read-only from T199's perspective. `build_render_model` will, post-P7.8, obtain the active modal projection through the stack API rather than cloning `app.settings`/`app.plugins`/`app.models`/`app.help_find` individually (`draw.rs:608-638`). T199.2 must depend on P7.8 (already declared in the roadmap: *"Dependencies: T199.1, P7.8, P12.4"*) so it does not re-plumb modal cloning that P7 is about to delete.

### 3.1 `model_health` is the one coupled edge case

`model_health` (`app.rs:95`) is a render input **only in service of the settings modal** — it is cloned at `draw.rs:611` to build `RuntimeSnapshot::from_runtime_with_health`. It is populated by the `ping_rx` channel (loop-state feed). Disposition: it enters `ViewInputs` (VM) **but its lifetime is bound to the settings modal being open.** T199.2 should carry it in `ViewInputs` as an ordinary render input, but be aware that once P7 owns the settings modal, the natural home for the `model_health` → `RuntimeSnapshot` projection may migrate into the modal's own render path. **Do not block T199.2 on this** — pass `model_health` through `ViewInputs` now; note it in the P7 handoff for later consolidation.

---

## 4. The `help_find` visible-height write-back — must be resolved, not carried

`build_render_model` today takes `&mut App` for exactly one structural reason beyond convenience: the `help_find` visible-height write-back at **`draw.rs:619-638`**. The render mirrors terminal geometry and calls `hf.set_visible_height(...)` on the *authoritative* `App` state before snapshotting, because the render thread would otherwise mutate a throwaway clone and desync the modal's scroll window on first open at a non-default size.

This is the single mutation that blocks converting `&mut App` → `&ViewInputs`. **T199.2 must resolve it explicitly (per the roadmap's own instruction).** Two acceptable resolutions:

- **(A) Returned patch (preferred):** `build_render_model` computes the geometry as today but returns the `visible_height` as part of its result (or a small `RenderPatch`), and the caller applies it to `App`/the modal store. The builder becomes non-mutating.
- **(B) Move into the modal store:** the geometry mirror moves into the P7 modal owner, computed from `term_size` at the point the modal is pushed/resized, eliminating the render-time write entirely.

Because P7 is taking `help_find` anyway, **(B) is the strategically correct home** — but it creates a hard ordering dependency on P7.8. **(A) is the safe fallback** if P7.8 slips: it lets T199.2 achieve the non-mutating-builder acceptance criterion independently. T199.2 should implement (A) as the immediate move and file a follow-up for (B) under P7.

---

## 5. Extraction order for T199.2 (the work order)

Execute in waves of increasing coupling. Each wave keeps `render_frame`'s no-`App` invariant intact and must produce **byte-identical harness snapshots** (roadmap acceptance).

**Wave 0 — define the seam (no behavior change).**
Introduce `ViewInputs` (a borrowing struct) OR `ViewModel` (an owning snapshot). Recommendation: a **borrowing `&ViewInputs<'_>`** assembled at the call site, because most fields are already cloned inside `build_render_model` — the goal is to narrow the *type*, not add a second copy. `build_render_model` signature changes from `app: &mut App` toward `inputs: &ViewInputs<'_>` (plus the write-back resolution from §4).

**Wave 1 — pure `Copy` scalars (zero-risk).**
`streaming`, `spinner_frame`, `cursor_pos`, `session_cost`, `total_input_tokens`, `total_output_tokens`, `total_cache_read_tokens`, `total_cache_creation_tokens`, `total_cache_write_1h`, `last_turn_context`, `last_turn_context_window`, `logo_build_t`, `logo_dismiss_t`. (13 fields — all `Copy`, no clone semantics to preserve.)

**Wave 2 — owned clones already performed by the builder.**
`input`, `status_text`, `agent_name`, `subagents`, `sidecars`, `toasts`, `active_tasks` (`Arc` clone). (7 fields.) These already `.clone()` at `draw.rs:659-681`; moving them behind `ViewInputs` is a pure relocation of the borrow.

**Wave 3 — transcript projection (verify P9 seam).**
`transcript` is already extracted; confirm `RenderCtx` (`spinner_frame`, `streaming`, `agent_name`) flows through `ViewInputs` cleanly. No new extraction — a consistency pass.

**Wave 4 — resolve the `&mut` write-back (§4).**
Implement resolution (A): make `build_render_model` non-mutating by returning the `help_find` visible-height as a patch. This is the field that lets the signature finally drop `&mut`.

**Wave 5 — P7-gated (do NOT start before P7.8).**
`settings` (+ `model_health` snapshot projection), `plugins`, `models`, `help_find`, `gamba_child` gate. These reach `ViewInputs` via the P7 `ModalStack` top-of-stack API, not as five `Option` clones. Blocked on P7.8.

**Acceptance gate (from roadmap T199.2):** after Waves 1-4, `build_render_model` no longer takes `&mut App` except as resolved in §4; the `App` `pub(crate)` field count is unchanged (fields aren't deleted — they're read through a narrow seam), but the *coupling surface* the builder touches drops from ~30 field accesses to a single `&ViewInputs`. The measurable win is the type signature and the proven no-`App` render path, not raw field deletion. Field deletion, if any, lands in T199.3's reconciliation once P7/P12 settle.

---

## 6. Handoffs

- **→ T199.2:** §5 is your ordered task list. Start Wave 0-4 immediately; Wave 5 waits on P7.8. Resolve §4 via resolution (A).
- **→ P7 (P7.3+):** §3 names the five fields you own (`app.rs:81-89`). Decide `gamba_child`'s membership in `ModalStack`. Own the §4 resolution (B) as the long-term home for the `help_find` geometry write-back, and the eventual home of the `model_health`→`RuntimeSnapshot` projection (§3.1).
- **→ T199.3:** reconcile this table against post-P7/P12 reality; document residual `App` fields (the 23 loop-state + 8 channels that consciously stay) with rationale, and close T199.

*The architecture was always sound. It was merely undocumented. Now the seam is drawn — the builders may proceed without asking a single question.*
