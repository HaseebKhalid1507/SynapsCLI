# Models Router: Expanded Provider Model Browser

## Assumptions
1. The existing `/model` and `/models` modal remains the primary curated quick-switcher.
2. The new "expand models" view is a secondary lightbox inside that modal, used when the curated list is not enough.
3. Initially, dynamic API loading should target OpenAI-compatible `/models` endpoints for configured providers, with OpenRouter as the first-class use case.
4. Only providers the user is actually configured/logged into should be expandable.
5. The expanded browser should support fuzzy matching locally after loading the provider model list; it should not call the provider API on every keystroke.
6. No new dependencies unless approved; implement a small deterministic fuzzy scorer in-tree.

## Objective
Build a full-screen lightbox that lets users browse all available models from a provider API, type to fuzzy-filter model names/IDs, and apply or favorite a selected model.

Success looks like:
- From the models modal, user presses an expand key on a provider section or model row.
- A lightbox opens titled with the provider name, loads available models from the provider API, and shows a loading/error/ready state.
- Typing filters the loaded models with fuzzy matching.
- Enter applies `provider/model-id`; `f` toggles favorite; Esc returns to the curated models modal.

## Commands
Build/test commands:
```bash
cd ~/Projects/Maha-Media/.worktrees/SynapsCLI-models-router
cargo test --bin synaps chatui::models::tests -- --nocapture
cargo test --lib runtime::openai -- --nocapture
cargo build --release --bin synaps
```

Manual smoke:
```bash
synaps
# open /model
# navigate to OpenRouter section/model
# press expand key
# type qwen
# favorite/apply a result
```

## Project Structure
Likely files:
- `src/chatui/models/mod.rs` — modal state, sections, expanded state, render logic, fuzzy matcher tests.
- `src/chatui/models/input.rs` — key handling for entering/exiting expanded view and applying/favoriting expanded models.
- `src/chatui/mod.rs` or runtime event loop files — async model loading trigger/result handling if needed.
- `src/runtime/openai/registry.rs` or new `src/runtime/openai/models.rs` — provider model-list API client.
- `docs/plans/2026-04-27-models-router.md` — update task list/behavior notes.

## Code Style
Keep state transitions explicit and testable:
```rust
match state.mode {
    ModelsMode::Curated => handle_curated_key(state, key),
    ModelsMode::Expanded { .. } => handle_expanded_key(state, key),
}
```
Prefer pure helpers for fuzzy scoring and filtering so tests do not require network access.

## Testing Strategy
- Unit-test fuzzy matching: subsequence matches, case-insensitive scoring, better contiguous matches rank higher.
- Unit-test expanded state transitions: expand provider, type query, backspace, escape, select.
- Unit-test API parser with static JSON matching OpenRouter `/models` shape.
- Avoid live API tests in automated suite; manual smoke covers real credentials.

## Boundaries
Always do:
- Keep `/model <name>` direct-set behavior.
- Keep curated quick list available.
- Only fetch expanded models for configured/logged-in providers.
- Do not call network per keystroke.
- Run targeted tests before claiming completion.

Ask first:
- Adding a fuzzy matching crate or async helper dependency.
- Caching provider model lists to disk.
- Supporting non-OpenAI-compatible provider-specific model APIs.

Never do:
- Log API keys or OAuth tokens.
- Show unauthenticated provider models as selectable.
- Block TUI rendering while a network request is in flight.

## Risks / Convergence
This touches network I/O, auth-gated provider visibility, and TUI state but not secret storage semantics. Suggested convergence mode: `none` unless you want a higher-cost design review loop.
