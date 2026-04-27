# Expanded Model Browser Implementation Plan

Convergence: none (approved by user via "yes" to recommendation).
Expand key: `e`.

## Dependency Graph

1. Pure data/parsing/fuzzy helpers
   - Provider model-list JSON parser
   - Fuzzy scoring and sorting
2. Expanded modal state/input
   - State machine for curated vs expanded browser
   - Keyboard handling and selection
3. Async provider model loading
   - Fetch `/models` for configured OpenAI-compatible providers
   - Wire loading result into app event loop
4. Rendering
   - Expanded lightbox loading/error/ready states
   - Footer help and provider expand hints
5. End-to-end verification
   - Manual OpenRouter smoke test

## Task 1: Checkpoint current models-router polish

**Description:** Commit existing completed changes before beginning expanded browser work.

**Acceptance criteria:**
- [ ] Working tree contains a commit for current local-model visibility and American spelling changes.
- [ ] No unrelated changes are mixed into expanded browser commits.

**Verification:**
- [ ] `cargo test --bin synaps chatui::models::tests -- --nocapture`

**Dependencies:** None
**Files likely touched:** git only
**Scope:** XS

## Task 2: Add provider model-list parser and fuzzy matcher

**Description:** Add pure helpers for parsing OpenAI-compatible `/models` responses and fuzzy filtering model IDs/names.

**Acceptance criteria:**
- [ ] Parses OpenRouter-style `{ "data": [{ "id": "...", "name": "..." }] }` JSON.
- [ ] Fuzzy matcher is case-insensitive and matches subsequences.
- [ ] Contiguous/earlier matches rank above weaker matches.

**Verification:**
- [ ] Unit tests for parser and fuzzy matcher pass.

**Dependencies:** Task 1
**Files likely touched:** `src/runtime/openai/registry.rs` or new module, `src/chatui/models/mod.rs`
**Scope:** S

## Task 3: Add expanded browser state and input transitions

**Description:** Extend the models modal with an expanded provider-browser mode entered by `e`.

**Acceptance criteria:**
- [ ] Pressing `e` on provider section or model row selects the provider for expansion.
- [ ] Expanded mode supports typing, backspace, up/down, Esc back to curated modal.
- [ ] Enter on an expanded model returns `Apply(provider/model-id)`.
- [ ] `f` toggles favorite for expanded model IDs.

**Verification:**
- [ ] Unit tests for state/input transitions pass.

**Dependencies:** Task 2
**Files likely touched:** `src/chatui/models/mod.rs`, `src/chatui/models/input.rs`
**Scope:** M

## Task 4: Wire async provider model loading

**Description:** Fetch all provider models when expanded mode opens and deliver results to modal state without blocking rendering.

**Acceptance criteria:**
- [ ] Opening expanded mode shows loading state immediately.
- [ ] Successful fetch populates expanded list.
- [ ] Failed fetch shows error and lets user Esc back.
- [ ] API key/OAuth token is not logged or rendered.

**Verification:**
- [ ] Tests for URL/key resolution or fetch parser pass.
- [ ] Manual smoke can load OpenRouter models with configured key.

**Dependencies:** Task 3
**Files likely touched:** `src/chatui/mod.rs`, `src/chatui/app.rs`, `src/runtime/openai/registry.rs` or new module
**Scope:** M

## Task 5: Render expanded lightbox

**Description:** Render the expanded provider model browser as a lightbox over the existing models modal.

**Acceptance criteria:**
- [ ] Title includes provider name and loaded/error/loading status.
- [ ] Search input is visible.
- [ ] Results show provider-prefixed runtime IDs plus optional display labels.
- [ ] Footer documents `Esc`, `Enter`, `f`, and search behavior.

**Verification:**
- [ ] Snapshot-ish render unit checks where practical, otherwise manual tmux smoke.
- [ ] `cargo build --release --bin synaps`

**Dependencies:** Task 4
**Files likely touched:** `src/chatui/models/mod.rs`
**Scope:** M

## Checkpoint after Task 5
- [ ] `cargo test --bin synaps chatui::models::tests -- --nocapture`
- [ ] `cargo build --release --bin synaps`
- [ ] Install and smoke in tmux: `/model`, `e`, type `qwen`, favorite, Enter apply, Esc back.
