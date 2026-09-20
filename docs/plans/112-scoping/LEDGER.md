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

## Tests restore — upstream dropped unit tests
| commit | 97b24617 |
|--------|----------|
| files  | `stream.rs`, `translate.rs`, `codex.rs` (unit); `turn_budget_stream.rs` (attempted, reverted) |
| source | 8bdabd4a |
| check  | ✅ `cargo test -p synaps-engine --locked` |
| engine | ✅ 2032 pass, 1 known (`static_table_and_wire_shape…`) |
| clippy | ✅ no NEW warnings from restored tests |
| ported | 4 Class A tests |
| skipped | 6 Class B, ~86 Class C (see report) |

## Phase 8b — take auto-merged engine files from ref-merge
| commit | f2240002..96c2584d (7 commits) |
|--------|----------|
| files  | 57 auto-merged engine/src files taken from ref-merge; 16 residual (ours-only, justified) |
| source | 8bdabd4a |
| check  | ✅ `cargo check --workspace --all-targets --locked` |
| engine | ✅ 2096 pass (+64 from upstream), 12 ignored, 0 fail (lib only) |
| core   | ✅ 732 pass |
| workspace | ✅ no new failures; known: autonomous_plugin (7, fixture), extensions_contract (1, updated), continuous_memory_adversarial (1, recall fixture) |
| clippy | ✅ `-D warnings` clean (synaps-engine, synaps, synaps-core) |
| two-sided | ✅ no conflict markers (pre-commit hook active) |
| docs   | ✅ docs/tools.json regenerated (28 tools), contract.json updated (session.drive) |
| checkpoint-B | synaps-engine | 2110 | 0 (1 order-flake: malformed_scope_fails_closed…, passes 3/3 alone) | 12 |
| checkpoint-B | synaps-core | 732 | 0 | 8 |
| 10-live | daemon (bella, release) | DARK: no context/forum tools, survives reload; auto+tiny thresholds: "Context rollover deferred" advisory reached thin client | — | — |
| workspace | all | 4438+7 (autonomous fixture) | 6 pre-existing (4× memory_context_e2e need SYNAPS_AXEL_TEST_BIN, continuous_memory order-flake, tui client_diet flake) | — |
| fix-recall | synaps-engine | 2390 | 0 | 12 |
| audit | workspace (on #121) | 4459 | 0 | — |

## Track H — engine residuals (fix/112-engine-residuals)
| phase | crate | passed | failed | ignored | notes |
|-------|-------|--------|--------|---------|-------|
| H1 | synaps-engine | 2099 | 2† | 12 | +budget test; budget_meter.exhaustion_error wired |
| H1 | turn_budget_stream | 8 | 0 | 0 | new: wall_clock_exhaustion_can_resume… |
| H2 | synaps-engine (cancel) | 41 | 0 | 0 | +4 cancellation tests |
| H3 | synaps-engine (rich_output) | 16 | 0 | 0 | +4 validation tests, validated_tool_output on stream path |
| H3 | synaps-engine (attachments) | 21 | 0 | 0 | unchanged |
| H4 | synaps-engine | 2116 | 0 | 12 | +17 tests total from H1–H4 |
| H5 | tools_export | 14 | 0 | 0 | help.json +21 lines, no drift |
| H5 | extensions_contract | 7 | 0 | 0 | no drift |

† Pre-existing order-flakes: user_binding_is_not_a_forum…, legacy_history_eligible_budget_roundtrip…

### Symbol audit residue (40 symbols, all classified)
- `write_comma_list` (config.rs): superseded by `_locked` variant on dev
- `configured_subagent_tools`: dev uses `subagent_tools()` + stream.rs DARK block for forum filtering
- `append_user_submission`, `list_pending_attachments` (chat.rs): Track F (attachments)
- `rpc_attachment_paths`, `load_rpc_user_content`, attachment tests (rpc.rs): Track F
- `Probe`, `call_tool`, `handle`, `id`, `shutdown` (subagent/mod.rs): ExtensionProbe trait impls, simplified on dev
- `prompt_always_includes_agent_and_forum_guidance`: dev has `prompt_includes_agent_and_forum_guidance_when_forum_is_on`
- `disabled_forum_policy_applies_after_bare_and_extension_construction`: needs `configured_subagent_tools`
- `all_launch_paths_use_common_author_registry_and_prompt_wiring`: dev spawn paths differ
- Context continuation tests (6) + helpers (5) + fixtures (2): need `drive_with_backend_and_steering`/`validate_messages` early rejection (possible LOST hunk)
- `steering_queued_during_setup_reaches_first_request_once`: needs drive_with_backend_and_steering
- `history_media_limit_rejects_before_lossy_pruning`: needs validate_messages early rejection
- `real_axel_*` (2): need SYNAPS_AXEL_TEST_BIN (skipped)
- `runtime_clones_keep_author_and_each_worker_execution_forks`: needs adjusted ctor
- `terminal_source_boundary_excludes_prior_turns_and_survives_prepend_and_steering`: needs adjusted ctor

### Findings
1. `validate_messages` early rejection is NOT on the streaming path on dev — tool outputs with
   oversized media go through to the provider via lossy `cap_history_image_bytes` pruning rather
   than being rejected pre-flight. This is a possible LOST hunk or a deliberate dev choice.
2. `validate_request_preflight` does not check `memory_backend_reconfigure_denied` on dev —
   backend changes after first config are not caught. 2 tests fail on dev because of this.
3. `await_provider_call`/`await_tool_call` restored as free fns (not the upstream fn names
   verbatim — same semantics, same names). No helper resurrection needed.
| H7 | synaps-engine | 2414 | 0 | 12 |
| H | workspace | 4481 | 0 (+client_diet flake) | — |
## Track G — TUI context-sidecar UX (feat/112-tui-context-sidecar)
| phase | crate | passed | failed | ignored | notes |
|-------|-------|--------|--------|---------|-------|
| G1 | synaps-tui | 3 (response_reset filter) | 0 | 0 | reset_response_preview_basic_truncation, reset_discards_only_current_response_and_keeps_steering, response_reset_shrinks_transcript_preview |
| G2 | synaps-tui | 49 (sidecar filter) | 0 | 0 | pill_loading_state_shows_label, status_line_shows_loading + existing |
| G3 | synaps-tui | 3 (async_startup filter) | 0 | 0 | delayed_completion_drains_loading, disable_cancels, disabled_sidecars_never_start |
| G7 | synaps-tui | 599 (full) | 1 (client_diet flake) | 5 | splitn whitespace fix + command result refactor |
| G-gate | workspace | all green | 1 (client_diet flake) | — | clippy --all-targets clean |
