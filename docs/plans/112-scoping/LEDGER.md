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
| 3-followup | synaps-engine | 2023 | 1† | 12 |
| 6 | synaps-axel-memory-service | — | — | — |
| 7 | synaps-engine | 2024 | 1† | 12 |
| 8 | synaps-engine | 2024 | 1† | 12 |
| 8 | synaps-core | 732 | 0 | 8 |
| 8 | workspace | — | 6‡ | — |

‡ Workspace failures (all pre-existing or env-dependent):
  - `static_table_and_wire_shape_classifier_agree_for_known_models` (known — new model entry)
  - `continuous_memory_headless_lifecycle…` (known AXEL fixture dep)
  - `recall_once_consumes…` / `recall_each_prompt…` ×3 (AXEL fixture dep, memory_context_e2e)
  - `stored_system_close_and_forged_tool_call_json…` (AXEL fixture dep, continuous_memory_adversarial)
  - `contract_json_matches_rust_hook_and_permission_catalogs` (drift check, pre-existing)
  - `export_pretty_matches_committed_docs_tools_json` / `drift_check…` (drift, pre-existing)
  - `e_opens_expanded_provider_browser` (TUI flake)

Phase 6 note: synaps-axel-memory-service is a standalone crate with own [workspace].
cargo check on bella failed with Permission denied on ~/.cargo/git/db for the
axel/axel-memkoshi git deps (private repo). DARK: memory.backend=legacy default
never spawns it. Sidecar binary located via config.executable or
<current_exe_dir>/synaps-axel-memory-service.

## Phase 9 — Wall 1: actor-owned context-head checkpoint persistence
| commit | da74f413 |
|--------|----------|
| files  | `crates/agent-engine/src/engine/session.rs`, `crates/agent-engine/src/session/actor.rs` |
| source | 8bdabd4a |
| check  | ✅ `cargo check --workspace --all-targets --locked` |
| engine | ✅ 2024 pass, 1 known (`static_table_and_wire_shape…`) |
| core   | ✅ 732+ pass |
| workspace | ✅ no new failures (known: autonomous_plugin, extensions_contract, shared_memory_migration, sidecar_manager_protocol, tools_export, memory_context_e2e, continuous_memory_adversarial, TUI lib) |
| clippy | ✅ `-D warnings` clean (synaps-engine, synaps) |
| two-sided | ✅ no conflict markers |
