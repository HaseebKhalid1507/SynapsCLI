# Plan: dynamic exact-model reasoning modes — native Anthropic and xAI

Spec: `docs/specs/anthropic-xai-reasoning-modes-spec.md`. Branch:
`feat/provider-reasoning-modes` (dedicated worktree).

## Convergence declaration

- mode: informed
- threshold: 0.8
- weights:
  - correctness: 0.35
  - architecture/types: 0.25
  - security: 0.15
  - tests: 0.15
  - maintainability: 0.10
- max_fix_iterations: 2
- max_total_calls: 10

## Tasks and dependencies

| # | Task | Depends on |
|---|---|---|
| T1 | Spec + plan committed (this document) | — |
| T2 | xAI capability descriptors (`XaiReasoningCapability`, `xai_static_capability`) + `ReasoningSupport::XaiEffort`; RED tests first | T1 |
| T3 | Anthropic exact static descriptors + live-parse `thinking.supported=false → None` + capability-cache populate; RED first | T1 |
| T4 | Shared mutation-time validator (`catalog/validation.rs`): per-provider matrix, Codex unknown-id Max/Ultra gap fix, remove `unreachable!` in `validate_codex_level`; wire into `set_reasoning_level_checked`; provider default level applied in `set_model`; RED first | T2, T3 |
| T5 | xAI wire: pure `build_xai_body` with pre-network rejection; thread `ReasoningLevel` through `try_route` → `call_xai_responses_stream_inner`; Anthropic `RequestBody` effort from named level; golden gate stays green; RED first | T4 |
| T6 | Dynamic options: shared engine `thinking_options_for_model`; TUI delegates; TUI + headless harness tests (no credentials) | T4, T5 |
| T7 | Final verification: full crate test suites, `cargo check -p synaps`, release checks engine+TUI, `git diff --check`, rustfmt changed files only, commits + clean tree | T2–T6 |

## Checkpoints

- **C1 (after T1)**: docs committed; no production code touched.
- **C2 (after T3)**: `cargo test -p synaps-engine` catalog tests green; RED
  evidence for T2/T3 captured in commit messages.
- **C3 (after T5)**: engine tests green including golden byte-identity gate;
  pre-network rejection proven without credentials.
- **C4 (after T7)**: full matrix green; clean tree; commit ids recorded.

## Acceptance

- Every spec matrix cell has a test; RED shown before GREEN per slice.
- No substring-based capability inference; provider-qualified ids end-to-end.
- Rejection paths leave runtime state and persisted config unchanged (tested).
- Byte identity preserved where behavior unchanged (golden gate); intentional
  wire changes (xAI effort exactness) covered by new tests and documented.
- Broker/credential boundaries untouched; validation precedes credential use.

## Unattended harness

All tests run headless with no live credentials:

```bash
CARGO_BUILD_JOBS=8 cargo test -p synaps-core \
&& CARGO_BUILD_JOBS=8 cargo test -p synaps-engine \
&& CARGO_BUILD_JOBS=8 cargo test -p synaps-tui \
&& CARGO_BUILD_JOBS=8 cargo check -p synaps \
&& CARGO_BUILD_JOBS=8 cargo check --release -p synaps-engine \
&& CARGO_BUILD_JOBS=8 cargo check --release -p synaps-tui \
&& git diff --check
```

Only one heavy Cargo command runs at a time; `CARGO_BUILD_JOBS=8` throughout.
