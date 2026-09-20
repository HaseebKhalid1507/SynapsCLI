# Merge Ledger — #112 engine half

| phase | crate | passed | failed | ignored |
|-------|-------|--------|--------|---------|
| baseline | synaps-core | 628 | 0 | 8 |
| 0 | synaps-core | 628 | 0 | 8 |
| 1 | synaps-core | 731 | 0 | 8 |
| 2 | synaps-core | 731 | 0 | 8 |
| baseline | synaps-engine | _(not captured at a576c597)_ | — | — |
| 2 | synaps-engine | 1979 | 2† | 12 |
| post-rebase | synaps-core | 732 | 0 | 8 |
| post-rebase | synaps-engine | 1989 | 2† | 12 |
| 3 | synaps-core | 732 | 0 | 8 |
| 3 | synaps-engine | 1990 | 2† | 12 |
| 4 | synaps-core | 732 | 0 | 8 |
| 4 | synaps-engine | 2023 | 1† | 12 |
| 5 | synaps-core | 732 | 0 | 8 |
| 5 | synaps-engine | 2023 | 1† | 12 |

† Pre-existing failures:
  - `static_table_and_wire_shape_classifier_agree_for_known_models` (anthropic.rs, confirmed at a576c597)
  - `user_binding_is_not_a_forum_even_with_user_notes_opt_in` (order-dependent flake, passes in isolation)
  - `continuous_memory_headless_lifecycle_captures_restarts_recalls_disables_and_imports` (pre-existing at 088ac939, memory_context_e2e integration test, intermittent)
