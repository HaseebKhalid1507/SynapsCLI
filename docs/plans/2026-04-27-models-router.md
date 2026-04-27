# Plan — Models Router (`/models`) + Settings Models Pane Refresh

**Date:** 2026-04-27
**Author:** plan draft (planning-and-task-breakdown skill)
**Status:** awaiting human review
**Convergence mode:** `none` (confirmed by human 2026-04-27) — UI/refactor
work, low blast radius, full human review at every checkpoint.

> **Worktree reminder.** No code is written until the plan is approved
> *and* a dedicated worktree exists. Setup at the bottom of this doc.

---

## 1. Goals

1. **Hotfix** the Codex Responses-API 400:
   `Invalid 'input[N].id': 'call_…'. Expected an ID that begins with 'fc'.`
2. **`/models` command** — a full-screen "models router" modal that lets
   the user browse every model from every registered provider, organized
   by provider, with a favorites toggle. Visual language inspired by
   the existing `cmd/login.rs` clack-style picker and opencode's model
   list.
3. **`/settings → Model` pane refresh** — surface favorites first, fold
   the existing `ModelPicker` into the new shared list widget so both
   places look and feel identical.

Non-goals (this plan):

- New provider integrations.
- Editing the static model registry from the UI.
- Per-model cost/context metadata beyond what the registry already
  exposes (`tier`).
- OAuth flows for new providers.

---

## 2. Current state (confirmed by reading the source)

| Area | File | Notes |
|---|---|---|
| Settings modal | `src/chatui/settings/{mod,draw,input,defs,schema}.rs` | Categories: Model, Providers, Agent, ToolLimits, Appearance, Plugins. `Model` row uses `EditorKind::ModelPicker` (flat list rendered inline at `input.rs:209-243`). |
| Provider registry | `src/runtime/openai/registry.rs` | 17 providers, each with `models: &[(id, label, tier)]`. Source of truth for the new router. |
| Login picker (design ref) | `src/cmd/login.rs` | Clack-style box drawing (`┌ │ ◇ └`), banner, search, `●/○` markers, `(recommended)` suffix, dim secondary text. |
| Slash dispatch | `src/chatui/commands.rs::CommandAction` + `src/chatui/mod.rs:540…` | Add `OpenModels` variant + handler. |
| Built-in command list | `src/skills/mod.rs:51` (`BUILTIN_COMMANDS`) | Must add `"models"`. |
| Config persistence | `src/core/config.rs::SynapsConfig` | KV file at `~/.synaps-cli/config`. Mirror the `disabled_plugins` pattern for `favorite_models`. |
| Codex bug site | `src/runtime/openai/stream.rs::codex_input_messages` (≈L259–288) and the decoder around L397–432 | `id` is hard-coded to `call.id` (a `call_…` value). Responses API requires `fc_…`. |

---

## 3. Codex 400 — root cause + fix

**Symptom**

```
400 Bad Request: Invalid 'input[4].id': 'call_nZYquCuGUh8Qs9H51dwHMDgs'.
Expected an ID that begins with 'fc'.
```

**Why**

The OpenAI Responses API (Codex backend) returns two distinct ids per
tool invocation:

- `id` — the *output item id*, prefix `fc_…`. Required when echoing the
  function_call back as an `input` item.
- `call_id` — the *function call id*, prefix `call_…`. Used to
  correlate `function_call_output` rows back to the call.

Today both decoder branches do `call_id.or_else(|| id)` and store the
result in a single `tool.id` string (`stream.rs:397-403`, `425-432`).
The `call_…` value wins, the `fc_…` is dropped, and on the next turn we
emit:

```rust
out.push(json!({
    "type": "function_call",
    "id": call.id,        // ← "call_xxx" — REJECTED
    "call_id": call.id,
    "name": call.function.name,
    "arguments": call.function.arguments,
}));
```

**Fix — two options**

| Option | Diff size | Pros | Cons |
|---|---|---|---|
| **A. Drop `id` from `codex_input_messages`** | ~1 line | Minimal blast radius; `id` is *optional* for input items per the Responses API spec — `call_id` alone is sufficient to correlate. | No round-trip fidelity; if a future Responses feature needs the original `fc_` id we'd have to re-thread it. |
| **B. Track `fc_` and `call_` separately end-to-end** | ~30–60 lines | Future-proof; matches the wire shape exactly. | Touches `ToolCall` (or a side-map keyed by `call_id`); more places to keep in sync; larger review. |

**Recommendation:** ship Option A as the hotfix in **Task 0** so users
are unblocked, then evaluate B as a follow-up only if a Responses
feature actually needs the `fc_` id round-tripped (none in current
code).

---

## 4. UX design

### 4.1 `/models` modal — anatomy

```
  ███████ ██    ██ ███    ██  █████  ██████  ███████   <- (subtle, optional)
  ┌ Models
  │
  ◇ Switch model · 47 available · 12 ✓ favorites
  │
  │ Search:  ▎
  │ View:    [✦ All] [★ Favorites]   (Tab to toggle)
  │
  │ ▾ Anthropic                                      (5)
  │   ● claude-opus-4-7         ★         A+  default
  │   ○ claude-sonnet-4-6                 A
  │   ○ claude-haiku-4                    B+
  │ ▾ Groq                              ✅ key set  (4 · 3 ✓)
  │   ○ groq/llama-4-maverick   ★   ✅ 220ms  S
  │   ○ groq/llama-4-scout              ⏳         A
  │   ○ groq/llama-3.3-70b      ★   ✅ 310ms  S
  │ ▸ Cerebras                          ⬚ no key  (2)
  │ ▸ NVIDIA NIM                        ✅ key set  (9 · 1 ✓)
  │ …
  │
  └ ↑/↓ select • Enter use • f favorite • Tab toggle view •
    /  search • c collapse • Esc close
```

Design rules pulled from `cmd/login.rs`:

- `LOGIN_PICKER_PADDING` ("  "), the `┌ │ ◇ └` glyphs, dim ANSI for
  secondary text, `●/○` for selection — reused verbatim. Themed via
  `THEME.load()` instead of raw escapes (we're inside ratatui, not the
  pre-TUI shell).
- Recommended/default markers in the right margin, not inline with the
  name.
- Search box always visible; type to filter, Backspace to delete.
- Sections are collapsible (`▾`/`▸`) so a user with 17 keys doesn't
  drown.

### 4.2 Favorites

- Marker: `★` next to favorited model id; toggle key: `f`.
- View toggle: `Tab` flips `[✦ All]` / `[★ Favorites]`. Default: All
  if no favorites exist; Favorites otherwise.
- Persistence: `favorite_models = provider/model, provider/model, …`
  in `~/.synaps-cli/config`. Same parser pattern as `disabled_plugins`.
- A favorite that no longer resolves (provider removed from registry)
  is shown grayed-out under a "Stale" section with a Delete hint.

### 4.3 `/settings → Model` pane refresh

- Replace today's flat `Picker` for the `model` row with the same
  reusable widget that backs `/models` (collapsed by default, with the
  current model expanded and selected).
- Surface a **"★ Favorites (N)"** pseudo-section pinned to the top
  when the user has any.
- Keep the existing rows for `thinking`, `context_window`,
  `compaction_model` unchanged. `compaction_model` also benefits from
  the new picker — wire it through too (zero net new code).

### 4.4 Keybinds (consistent across both surfaces)

| Key | Action |
|---|---|
| `↑` `↓` `j` `k` | Move cursor |
| `Enter` | Apply (set active model) |
| `f` | Toggle favorite |
| `Tab` | Toggle All ⇄ Favorites view |
| `c` | Collapse/expand the section under the cursor |
| `/` | Focus the search input (already focused = no-op) |
| `Esc` | Close modal (Settings: close editor, then close modal) |
| `p` | Ping provider/model (mirrors current Providers pane) |

---

## 5. Architecture

```
src/chatui/
  models/                    NEW
    mod.rs                   ModelsModalState, public render/handle
    state.rs                 view mode, search, favorites set, collapsed sections
    draw.rs                  list rendering (shared with settings model row)
    input.rs                 key handling
    favorites.rs             load/save + comma-list parser; pure, unit-tested
  commands.rs                + CommandAction::OpenModels
  mod.rs                     dispatch arm
  settings/
    input.rs                 ModelPicker → reuse models::draw::ModelList
    defs.rs                  no schema changes; both rows still EditorKind::ModelPicker
src/skills/mod.rs            BUILTIN_COMMANDS += "models"
src/core/config.rs           parse `favorite_models = …`; SynapsConfig.favorite_models: Vec<String>
src/runtime/openai/stream.rs Task 0 hotfix
```

The favorites store lives in `synaps_cli::config` (not the chatui
crate) so `cmd/login.rs` and any future CLI subcommand can read it.

---

## 6. Tasks

### Task 0 — Codex `id` hotfix  *(scope: XS)*

**Description:** Remove the invalid `id` field from
`codex_input_messages` so the Responses API stops 400-ing.

**Acceptance criteria:**
- [ ] `codex_input_messages` no longer emits an `id` key on
  `function_call` items (only `type`, `call_id`, `name`, `arguments`).
- [ ] A unit test covers the JSON shape for a single tool call and
  asserts `id` is absent and `call_id` is present.
- [ ] A regression note added to `docs/open-provider-issues.md`.

**Verification:**
- [ ] `cargo test -p synaps-cli runtime::openai::stream` passes.
- [ ] Manual: `synaps-cli` against `openai-codex/<model>`, run a turn
  that requires ≥ 2 tool calls, confirm no 400.

**Dependencies:** none.
**Files:** `src/runtime/openai/stream.rs`, `docs/open-provider-issues.md`.

---

### Task 1 — `favorite_models` config field  *(S)*

**Description:** Persist a comma-separated list of `provider/model`
favorites. Mirror the `disabled_plugins` pattern exactly.

**Acceptance criteria:**
- [ ] `SynapsConfig.favorite_models: Vec<String>` defaults to `[]`.
- [ ] Parser handles `favorite_models = a/b, c/d` (whitespace tolerant).
- [ ] Helper: `config::add_favorite(id) / remove_favorite(id) /
  is_favorite(id) -> bool` that round-trips through the on-disk file
  using `write_config_value` semantics.
- [ ] Unit tests cover parse + round-trip.

**Verification:**
- [ ] `cargo test -p synaps-cli config::` passes.

**Dependencies:** none.
**Files:** `src/core/config.rs` (+ tests).

---

### Task 2 — `models` shared list widget (read-only render)  *(M)*

**Description:** New module `chatui/models/` exposing
`ModelsListState` + a `render(frame, area, &state, &snap)` function
that draws the grouped list (no input handling yet). Pull provider
data from `runtime::openai::registry::providers()` and Anthropic
models from `synaps_cli::models::KNOWN_MODELS`.

**Acceptance criteria:**
- [ ] Visual match to §4.1 anatomy (collapsed/expanded glyphs,
  favorite star, key/ping status from existing `RuntimeSnapshot`
  helpers).
- [ ] Pure render — no key handling, no mutation.
- [ ] Snapshot test (or text-based golden) over a fixed registry
  subset.

**Verification:**
- [ ] `cargo test -p synaps-cli chatui::models::` passes.
- [ ] `cargo build` succeeds.

**Dependencies:** Task 1 (favorites), so render can show ★.
**Files:** `src/chatui/models/{mod,state,draw}.rs`.

---

### Task 3 — `/models` modal: input, search, view toggle, favorite toggle  *(M)*

**Description:** Wire input handling, search box, `f`/`Tab`/`c`/`Enter`
keys, and modal open/close into the chatui event loop.

**Acceptance criteria:**
- [ ] `BUILTIN_COMMANDS += "models"` (`src/skills/mod.rs`).
- [ ] `CommandAction::OpenModels` variant + dispatch arm in
  `chatui/mod.rs` opens the modal.
- [ ] `Enter` calls `runtime.set_model(id)` and closes.
- [ ] `f` toggles favorite via Task 1 helpers; UI updates immediately.
- [ ] `Tab` flips view; persists nothing (transient).
- [ ] `Esc` closes modal cleanly.
- [ ] Help text in `/help` lists `/models`.

**Verification:**
- [ ] `cargo build` succeeds.
- [ ] Manual: open modal, search for "qwen", favorite one, toggle to
  Favorites view, confirm only that one shows.
- [ ] `cargo test` (existing suite green).

**Dependencies:** Tasks 1 + 2.
**Files:** `src/chatui/models/{input,mod}.rs`,
`src/chatui/commands.rs`, `src/chatui/mod.rs`,
`src/skills/mod.rs`.

---

### Checkpoint A — after Tasks 0–3

- [ ] Codex hotfix verified against a real Codex-OAuth account.
- [ ] `/models` opens, favorites round-trip to disk, switching model
  works.
- [ ] No regression in existing `/settings` flow.
- [ ] Human review before proceeding.

---

### Task 4 — `/settings → Model` row uses the shared widget  *(S)*

**Description:** Replace the inline picker built in
`settings/input.rs:209-243` with a call into the Task 2 widget. Same
behavior for `compaction_model`.

**Acceptance criteria:**
- [ ] Removing the inline `Vec<String>` model list from `input.rs`
  leaves no orphan helpers (`fmt_latency` either moves with it or
  stays where used).
- [ ] Settings modal still applies the picked model via
  `apply_setting_dispatch("model", …)`.
- [ ] Favorites pinned section appears at top when non-empty.

**Verification:**
- [ ] `cargo test` green.
- [ ] Manual: `/settings → Model → Enter`, pick model, value reflects.

**Dependencies:** Tasks 1–3.
**Files:** `src/chatui/settings/input.rs`,
`src/chatui/settings/draw.rs` (Picker render path may simplify).

---

### Task 5 — Visual polish pass (login-picker parity)  *(S)*

**Description:** Apply the clack-style chrome to the `/models` modal:
banner trim, `┌ │ ◇ └` framing inside the modal block, dim
secondary text via theme, `●/○` selection markers.

**Acceptance criteria:**
- [ ] Side-by-side screenshot diff vs login picker shows shared
  visual language (paddings, glyphs, dim modifiers).
- [ ] Footer hint string mirrors login style: `↑/↓ to select • …`
  with `•` separators.
- [ ] All colors via `THEME.load()`; no raw ANSI in chatui paths.

**Verification:**
- [ ] `cargo build` succeeds.
- [ ] Manual screenshot or `script(1)` capture committed under
  `docs/reviews/2026-04-27-models-router.md`.

**Dependencies:** Tasks 2–4.
**Files:** `src/chatui/models/draw.rs`.

---

### Checkpoint B — after Tasks 4–5

- [ ] Both surfaces (settings + `/models`) feel like the same product.
- [ ] Demo recorded; human signs off.
- [ ] PR ready.

---

## 7. Dependency graph

```
Task 0 ──── (independent hotfix, ship first)

Task 1 (config)
  └── Task 2 (widget render)
        └── Task 3 (/models modal input)
              └── Task 4 (settings reuses widget)
                    └── Task 5 (polish)
```

Tasks 0 and 1 can be implemented in parallel (different files, no
shared state). Tasks 2 → 5 are strictly sequential.

---

## 8. Risks & open questions

| Risk | Mitigation |
|---|---|
| Option A drops `id` and a future Responses feature wants it | Filed as TODO in `docs/open-provider-issues.md`; revisit when adding tool-call cancellation or parallel-call features. |
| `favorite_models` list grows unbounded | Soft cap rendered count to 200; warn in `/models` footer if exceeded. |
| Anthropic models don't have `provider/` prefix in current registry | Treat them as `claude/<id>` *for favorites only* (not for runtime resolution). Document in `favorites.rs`. |
| ratatui-driven modal can't reuse `cmd/login.rs` raw-ANSI helpers verbatim | Translate the visual language into ratatui Spans during Task 2; keep glyphs and spacings constant. |

**Resolved (2026-04-27):**

1. **Hide unconfigured providers** from `/models` entirely. Users
   discover providers via `/login`. The Settings → Providers pane is
   the catalog. `/models` only lists what is actually usable right now.
2. Picking a favorite does *not* flip auth path — `runtime.set_model`
   handles routing as today.
3. **Convergence mode `none` confirmed.** Single-agent loop. Hotfix
   ships through normal human review.

Anthropic favorites are stored as `claude/<id>` (e.g.
`claude/claude-opus-4-7`) so the favorites list is uniformly
`provider/model`. Resolution back to bare ids for `runtime.set_model`
happens in `favorites.rs` — strip the `claude/` prefix before applying.

---

## 9. Worktree setup (run when plan is approved)

```bash
cd /home/jr/Projects/Maha-Media/SynapsCLI
git fetch origin --prune
# Base branch: feat/login-enhancements — origin/main does not yet contain
# the Codex Responses API code, so Task 0 cannot apply against main.
git worktree add -b feat/models-router \
  ../.worktrees/SynapsCLI-models-router \
  origin/feat/login-enhancements
cd ../.worktrees/SynapsCLI-models-router
```

All implementation happens in the worktree. The primary checkout stays
clean on the integration branch.

---

## 10. Verification before any task starts

- [x] Every task has acceptance criteria.
- [x] Every task has a verification step.
- [x] Dependencies identified and ordered.
- [x] No task touches more than ~5 files.
- [x] Checkpoints between phases.
- [ ] Human reviewed and approved this plan.
- [x] Convergence mode declared (`none`, pending human confirmation).
- [ ] Worktree created and active.
