# H — Adversarial hunk-level re-audit: is the ENGINE half actually complete?

**Audited against:** `origin/dev` @ `816c143d` vs `origin/feat/context-continuation` @ `8bdabd4a`  
**Method:** `git diff origin/dev origin/feat/context-continuation -- <path>`, every `+` block classified, then deeper than symbols into changed function bodies  
**Prior claim:** "engine half is in, clean" (two reviews + symbol audit found 5 lost hunks, fixed)  
**Verdict:** **The engine half is ~92 % landed. 3 production hunks are LOST, ~42 test functions are missing (25 stream.rs, 14 mod.rs, 3 cmd/chat.rs+rpc.rs), and the budget exhaustion error message is silently degraded. The rest is either superseded by daemon architecture or deliberate design divergence.**

---

## §1  Priority files — hunk-level classification

### 1.1  `crates/agent-engine/src/runtime/stream.rs` (1005 upstream-adds, 547 dev-dels)

| Hunk / symbol | Lines (upstream) | Classification | Evidence |
|---|---|---|---|
| `activation_policy` fn removed | ~20 del | **superseded** | Dev has it at `stream.rs:28`; upstream inlined the logic. Dev's version supports `tools.activation_confirm = deny` (3-way policy), upstream collapsed to a 2-way `auto_approve_confirms` check. Dev version is strictly richer. |
| `StreamSession` field reorder (`context_window`, `continuation` moved later) | 4 | **deliberate** | Dev orders fields by category (context continuation grouped with memory_backend), upstream groups differently. Pure layout. |
| `StreamSession` session-identity fields removed (`session_id`, `cwd`, `env`, `env_stripped`, `env_warned`) | ~12 del | **superseded** | Dev @ `stream.rs:90-100` has these fields; they're the daemon session-identity system. Upstream never had them. Dev is strictly ahead. |
| `StreamSession.activation_confirm` field removed | 2 del | **superseded** | Dev @ `stream.rs:111` has this. Upstream simplified to just `auto_approve_confirms`. |
| `await_provider_call` fn (cancellation wrapper for provider IO) | 15 | **LOST** | Dev @ `stream.rs:1002-1020` calls `ApiMethods::call_api_stream_inner` raw. Upstream wraps it in a `tokio::select!` with pre-cancellation guard: if `cancel.is_cancelled()` before the call, return `Err(Canceled)` immediately instead of polling. Dev's path polls the provider even when already cancelled. |
| `await_tool_call` fn (cancellation wrapper for tool execution with started flag) | 22 | **LOST** | Dev @ `stream.rs:1299-1380` uses a raw `tokio::select!` on `tool.execute_rich` vs `cancel.cancelled()`, always setting `started=true` implicitly. Upstream's `await_tool_call` has an explicit `started` bool that is `false` when cancellation fires before the first poll — the `interrupted_started` ledger check at `stream.rs:1325` then knows the tool never ran. Dev unconditionally treats every cancelled tool as having started, which means: (a) the call-ledger records a false interrupted-started for unstarted tools, (b) the side-effect warning is emitted for tools that were never polled. |
| `validated_tool_output` fn (model-aware attachment validation in the stream loop) | 9 | **LOST** | Dev @ `stream.rs:1343` calls `o.into_parts()` raw. Upstream calls `validated_tool_output(&model, o)` which runs `attachments::validate_tool_blocks(model, blocks)` — rejecting unsupported-media images BEFORE they enter history. On dev, a non-Anthropic model receiving a tool output with image blocks gets raw base64 in history that the provider cannot render. The `validated_single_tool_output` fn in `mod.rs` handles the `run_single` path but NOT the streaming path. |
| `budget_meter.exhaustion_error(dimension)` vs `TurnError::budget(dimension)` | 1 | **LOST** (degraded) | Dev @ `stream.rs:511` emits `TurnError::budget(dimension)` — generic message. Upstream emits `budget_meter.exhaustion_error(dimension)` — enhanced message with elapsed/limit, `/budget status` instructions, history-retained notice. Both codebases have `exhaustion_error` in `budget.rs:249` identically. Dev simply doesn't call it from the streaming `finish_budget_exceeded!` macro. The `turn_budget_stream.rs` test confirms the enhanced message on upstream (`stream.rs:490`). |
| `session_id` param removed from `emit_before_tool_call`, `emit_after_tool_call`, `HookEvent.with_session` | ~8 del | **superseded** | Dev @ `mod.rs:66,174` passes `session_id` into hook events. This is the daemon multi-session hook-keying. Upstream has no concept of multi-session hooks. Dev is ahead. |
| `prepare_or_cancel!(tools.read())` guard | 1 | **deliberate** | Upstream adds a cancellation check before `tools.read().await`. Dev uses `tools.read().await` directly. Minor: if cancelled, the read still returns quickly (no IO). Low risk. |
| `hook_bus.emit` wrapped in `super::api::await_or_cancel` | ~5 | **deliberate** | Upstream wraps hook emissions in cancellation-aware calls. Dev does not — hooks run even on cancel. Different design choice: dev wants hooks to always fire (for logging/audit). |
| Forum-disable block in stream loop (DARK §7) | ~6 del | **superseded** | Both have this block. Dev @ `stream.rs:404-411`, upstream at the same logical position. Identical semantics. |
| `segment_has_provider_round` / `time_checkpoint` relocation | ~8 | **deliberate** | Dev declares `segment_has_provider_round` and `time_checkpoint` earlier (before session_tool_set construction). Upstream declares them later (after the activation authority block). Pure ordering change; no semantic difference. |
| Comment changes (dozens) | ~50 | **deliberate** | Upstream has different/fewer/reworded comments throughout. No semantic change. |
| `production_output = None` on tool error (F28 delta-lane fix) | 1 del | **deliberate** | Dev @ `stream.rs:1349` sets `production_output = None` on tool execution error to prevent the delta lane from winning over the error summary. Upstream removed this — different approach to the delta-lane/error priority. This is dev's F28 fix and should be kept. |
| `cap_history_image_bytes` fn | 36 | **superseded** | Both codebases have this function. Dev @ `stream.rs:1986`, upstream at a similar location. Identical semantics. |
| 25 test functions (see appendix) | ~700 | **test-only** | All 25 missing `#[test]`/`#[tokio::test]` fns are upstream unit tests for context continuation, cancellation, validation, and the budget integration. Listed in §3. |

### 1.2  `crates/agent-engine/src/runtime/mod.rs` (760 upstream-adds, 411 dev-dels)

| Hunk / symbol | Lines (upstream) | Classification | Evidence |
|---|---|---|---|
| `ReasoningClamp` struct relocation | 6 | **deliberate** | Both have the struct. Dev @ `mod.rs:219`, upstream moves it after `validated_single_tool_output`. Pure layout. |
| `validated_single_tool_output` fn | ~30 | **superseded** | Both have it. Dev @ `mod.rs:227`, upstream at a nearby line. Identical semantics. |
| `session_id` param on `emit_before/after_tool_call` | ~6 del | **superseded** | Dev daemon multi-session. See stream.rs above. |
| `activation_policy` re-export removed | 1 del | **superseded** | Dev @ `mod.rs:50` exports it. Upstream inlined it. Dev keeps the richer 3-way version. |
| `RuntimeParts`, `from_parts`, `build_host_http_client`, `fresh_host_parts`, `fresh_session_manager` | ~60 del | **superseded** | Dev @ `mod.rs:826-870` has these as part of the `EngineHost` / daemon architecture. Upstream has a monolithic `Runtime::new()` instead. Dev design is the replacement. |
| `session_id`, `cwd`, `env`, `env_stripped`, `env_warned` fields on `Runtime` + setters | ~30 del | **superseded** | Dev @ `mod.rs:495-509` has session-identity fields. Part of daemon Phase 2. Upstream never had these. |
| `activation_confirm` field + setter/getter | ~8 del | **superseded** | Dev @ `mod.rs:458-460,1525-1533`. Upstream has simplified 2-way logic. |
| `Drop` impl for Runtime (reaper cancellation) | 6 del | **superseded** | Dev @ `mod.rs` has the Drop impl. Upstream moved reaper management differently. |
| `memory_context_project_id` fn moved | 15 | **deliberate** | Both have the function. Dev defines it inside `impl Runtime` block at `mod.rs:2342`; upstream defines it as a standalone fn. Same logic. |
| `memory_tool_capability` fn | 15 | **superseded** | Both have it. Dev @ `mod.rs:2356`, upstream at a similar position. Identical. |
| `terminal_capture_history` — `capture_text` extraction | 10 | **deliberate** | Upstream uses `axel_context::capture_text(message)` (cleaner). Dev uses inline `get("content").and_then(as_str)` logic. Both extract the same data. Dev could adopt the helper but it's not LOST — it's a stylistic difference. |
| `memory_provider_id` fn relocation | 4 | **deliberate** | Both have the function. Dev @ `mod.rs:697`, upstream at a different line. |
| `memory_backend.from_config_with_cwd` vs `from_config` | 3 del | **superseded** | Dev has `from_config_with_cwd` for daemon per-session cwd. Upstream uses `from_config` (process cwd only). |
| `Runtime::new()` inline construction vs `from_parts` | ~80 | **deliberate** | Dev uses `from_parts(RuntimeParts)` pattern, upstream has a monolithic constructor. Different designs. |
| `apply_config` — `disable_tools` guard (`!config.disabled_tools.is_empty()`) | 1 | **deliberate** | Upstream removed the `disable_tools` bool flag in the `apply_config` path. Dev has an additional `disable_tools` parameter. Different apply_config signature. |
| `clear_memory_contribution` + retained_recall reset on memory disable | 5 | **superseded** | Upstream adds explicit cleanup when disabling memory. Dev's path may not clear these. This is a minor behavioral difference — the contribution and recall turn are process-local state. |
| `memory_history_preview` — Axel backend path | 25 | **superseded** | Both have this. Upstream's version uses `memory_backend.scope()`/`brain_path()`/`repository_identity()` with a more structured fallback. Dev uses `HistoryImportHostState::from_current_host()`. The upstream version is more correct for the Axel backend but this only matters when `memory.backend = axel`, which is DARK. |
| `memory_history_confirm` — exclusive backend guard | 7 | **superseded** | Both have the guard. Upstream's version is more explicit. Same behavior under legacy. |
| `compaction_terminal_capture` — safe-source check | 10 | **superseded** | Upstream adds `capture_source_safe` check before compaction capture. Dev doesn't have this guard. Under legacy backend this is inert (Axel capture is the only path that uses it). |
| `run_single` context-management guard | 3 | **superseded** | Upstream adds `if self.context_management_enabled() { return Err(…) }` before `run_single`. Dev doesn't because `run_single` is only used in tests/non-streaming paths where context management is never enabled. The guard is defense-in-depth. |
| `memory_backend` passed to `ApiOptions` | 2 × 3 | **deliberate** | Upstream passes `Some(self.memory_backend.clone())` where dev passes `None`. The `ApiOptions.memory_backend` field controls attachment base64 gating per provider. Under legacy backend this is a no-op. |
| `bounded_tool_results` for tool batch | 2 | **deliberate** | Upstream calls `attachments::bounded_tool_results()` before pushing tool results to messages. Dev pushes raw JSON. The bounding prevents oversized tool output from exceeding provider limits — but this is an attachment-era safety net. |
| `agent_core::core::memstat::log_turn_memory()` removed | 1 del | **superseded** | Dev @ the stream spawn closure calls jemalloc memstat logging after each turn. Upstream removed it. Dev has the jemalloc dep in agent-core; upstream doesn't. |
| `Clone` impl — field differences | ~10 | **superseded** | Dev's Clone includes `session_id`, `cwd`, `env`, `env_stripped`, `env_warned`, `activation_confirm`. Upstream's doesn't have those fields. Each clones what it has. |
| 14 test functions (see appendix) | ~450 | **test-only** | Missing tests for forum author, rich output validation, memory backend config, terminal capture. Listed in §3. |

### 1.3  `crates/agent-engine/src/tools/subagent/mod.rs` (298 upstream-adds, 72 dev-dels)

| Hunk / symbol | Lines (upstream) | Classification | Evidence |
|---|---|---|---|
| `apply_subagent_runtime_policy` — credential source comment | 4 del | **superseded** | Dev has a longer comment about host-built workers sharing the process-wide broker. Upstream's shorter comment is equivalent. |
| `subagent_tools()` vs `configured_subagent_tools()` | 30 | **deliberate** | Dev's `subagent_tools()` at `mod.rs:132` does extension-merge only. Upstream's `configured_subagent_tools()` ALSO applies `config.disabled_tools` forum filtering. On dev, forum removal happens in `stream.rs:404-411` (the DARK block) for all sessions including workers. Upstream doubles the removal at the registry level too. Minor: an operator disabling `forum_post` in config on dev won't see it removed from the worker registry, but it will still fail at `require_forum` under legacy backend. |
| `compose_system_prompt` — no `forum` bool | 8 | **deliberate** | Upstream always appends FORUM_GUIDANCE. Dev gates it on `memory_backend_is_axel()`. Both designs work because the forum tools are hidden from the catalog under legacy anyway — appending guidance for hidden tools wastes ~150 tokens per worker spawn. Dev's design is more token-efficient. |
| `compose_system_prompt_with_preamble` fn | 10 | **superseded** | Both have this. Dev's version has the `forum: bool` parameter. Same structure. |
| `FORUM_GUIDANCE` const | 1 | **superseded** | Both have it. Identical text. Dev @ `mod.rs:147`, present since the phase 5 fix. |
| 18 test functions (see appendix) — `ExtensionProbe`, `Probe`, and 11 test fns | ~200 | **test-only** | Forum worker tests, extension probe fixture, `disabled_forum_policy_applies_after_bare_and_extension_construction`, etc. Listed in §3. |

### 1.4  `src/cmd/rpc.rs` (259 upstream-adds, 68 dev-dels)

| Hunk / symbol | Lines (upstream) | Classification | Evidence |
|---|---|---|---|
| `rpc_attachment_paths` fn + `load_rpc_user_content` fn | 30 | **superseded** | Dev @ `rpc.rs` uses `build_user_content` from `rpc_dispatch`. Upstream has local `rpc_attachment_paths` (validates absolute, no `..`) + `load_rpc_user_content` (uses `attachments::build_user_content`). Upstream's version is a Wall 1 attachment safety layer that dev's `#121` partially landed. The path validation and multi-image content building landed in `#121`'s `rpc.rs`. |
| `handle_prompt` — attachment flow with rejection, disclosure, TOCTOU recheck | 87 | **superseded** | Dev's `#121` (merged at `19b634c2`) landed the Wall 1 RPC attachment handling including context-head blocking, TOCTOU rechecks, and attachment disclosure events. Verified by the `context_head_tests` module. |
| `auto_turn_cap` field / `wake_action_with_cap` / `claim_auto_turn_with_cap` | ~20 del | **superseded** | Dev has `auto_turn_cap` configurable (daemon feature). Upstream hard-codes `AUTO_TURN_CAP = 5`. Dev is ahead. |
| `context_head.is_blocked` ordering changes | 4 | **deliberate** | Upstream checks `context_head.is_blocked` before `had_buffered` in `terminal_flush`. Dev checks after. Ordering difference — both are correct since both check before the auto-turn reservation. |
| `context_head_rejection_blocks_shutdown_save_and_auto_chain` test | ~30 | **superseded** | Dev's `#121` has `context_head_tests` module with this test at `rpc.rs:1442-1567`. |
| `persist_context_head` fn | 5 | **superseded** | Dev's `#121` has this function. |
| `handle_compact` — auto_turn reservation on compact | ~10 | **deliberate** | Upstream reserves `auto_turn_pending = true` before spawning the compact transition, releases on success or failure. Dev doesn't. Minor race: on dev a background event could trigger an auto-turn during compaction. Very unlikely in practice (compaction runs while idle). |
| `attachment_tests` module (3 tests) | ~80 | **test-only** | `rpc_attachment_paths_require_absolute_without_parent_components`, `rpc_loads_bytes_and_ignores_mime_and_name_hints`, `rpc_attachment_failure_does_not_append_or_reserve_a_turn`. These test the attachment path validation. |

### 1.5  `src/cmd/chat.rs` (198 upstream-adds, 555 dev-dels)

| Hunk / symbol | Lines (upstream) | Classification | Evidence |
|---|---|---|---|
| `append_user_submission` fn | 30 | **superseded** | Dev's `chat.rs` is the actor-based `SessionCommand::Submit` path. The inline loop is behind `feature = "legacy_inline"`. Upstream's `append_user_submission` is for the inline loop that dev replaces with the actor. |
| `list_pending_attachments` fn | 15 | **superseded** | Same — inline-loop attachment handling. Dev's actor path handles attachments in `session/actor.rs`. |
| `/attach`, `/attachments`, `/detach` commands | 30 | **superseded** | Dev's TUI/actor has these commands. The inline chat.rs path is legacy. |
| `ContextHeadCheckpoint` handling in turn loop | 20 | **superseded** | Dev's actor handles checkpoints. The inline loop is legacy. |
| `ResponseStart`, `ResponseReset` event handling | 5 | **superseded** | Dev's actor handles these. The inline loop is legacy. |
| `context_management_enabled()` guard on auto-compact | 3 | **superseded** | Dev's actor has this guard in `loop_arms.rs`. The inline loop is legacy. |
| `wake_action` / `claim_auto_turn` (non-cap versions) | 6 | **superseded** | Dev has the configurable-cap versions. |
| `attachment_tests` module | ~40 | **test-only** | `rejected_attachment_submission_retains_bytes_and_abort_context`. Tests the legacy inline loop only. |
| Entire `actor` module deletion (-555 lines) | 555 del | **superseded** | The `actor` module in upstream's diff is dev-only code that upstream never had. Dev's `chat.rs` is the thin-client actor-based path. |

### 1.6  `crates/agent-engine/src/engine/setup.rs` (137 upstream-adds, 220 dev-dels)

| Hunk / symbol | Lines (upstream) | Classification | Evidence |
|---|---|---|---|
| `EngineHost::boot_and_install` → inline `Runtime::new()` | ~80 del | **superseded** | Dev uses `EngineHost` pattern (`setup.rs:91-100` references `EngineHost`). Upstream constructs everything inline. Dev design is the daemon-mode replacement. |
| `resolve_session_and_prompt` extraction as `pub(crate)` fn | 20 del | **superseded** | Dev extracted this for `SessionActor::create` reuse. Upstream keeps it inline in `boot()`. |
| `spawn_session_background` extraction as `pub(crate)` fn | 15 del | **superseded** | Dev extracted for actor reuse. |
| `finish_session_setup` extraction with `IndexRecord` enum | 20 del | **superseded** | Dev extracted for unpark (B3) / reload (C3) skip. |
| `BackgroundTasks.hook_bus` field + cleanup | 10 del | **superseded** | Dev clears session injection on shutdown. Upstream doesn't have per-session injection. |
| `ContinueInfo.resolved_via = "compacted"` + `compaction_notice` | 8 del | **superseded** | Dev has F24 compaction chain following. Upstream doesn't. |
| `follow_compaction_chain` in session resolution | 6 del | **superseded** | Dev's `resolve_or_create_session` follows `compacted_into` links (F24). |
| `SessionBootResult` visibility (`pub(crate)` vs private) | 2 | **deliberate** | Dev needs `pub(crate)` for `SessionActor`. Upstream keeps it private. |
| `default_config_is_dark_no_checkpoint_tool_legacy_binding` test | 20 del | **superseded** | Dev's test at `setup.rs` bottom. This was a merge-time darkness guard. |
| MCP setup + extension manager inline construction | ~40 | **superseded** | Upstream builds MCP + extension manager in `boot()`. Dev has these on `EngineHost`. |

### 1.7  `crates/agent-engine/src/engine/reactor.rs` (10 upstream-adds, 188 dev-dels)

| Hunk / symbol | Lines (upstream) | Classification | Evidence |
|---|---|---|---|
| `AUTO_TURN_UNLIMITED`, `AUTO_TURN_CAP_CONFIG_KEY`, `auto_turn_cap_reached`, `describe_auto_turn_cap` | ~25 del | **superseded** | Dev has configurable auto-turn cap (daemon feature). `reactor.rs:15-48`. |
| `wake_action_with_cap`, `claim_auto_turn_with_cap`, `terminal_flush_seam_with_cap` | ~40 del | **superseded** | Dev's cap-parameterized versions. Upstream uses hardcoded `AUTO_TURN_CAP`. |
| `claim_auto_turn` body change (`saturating_add` → `+= 1`) | 1 | **deliberate** | Minor. `saturating_add` vs `+= 1`. Both are correct — the cap prevents overflow. |

### 1.8  `crates/agent-engine/src/extensions/hooks/mod.rs` (9 upstream-adds, 76 dev-dels)

| Hunk / symbol | Lines (upstream) | Classification | Evidence |
|---|---|---|---|
| `session_injection` → `HashMap<String, String>` vs `Option<String>` | ~30 del | **superseded** | Dev's `hooks/mod.rs:71` uses a `HashMap` keyed by session_id for daemon multi-session injection. Upstream uses `Option<String>`. Dev design is the daemon-aware replacement. |
| `set_session_injection_for`, `session_injection_for`, `clear_session_injection` | ~20 del | **superseded** | Dev's keyed injection API. Upstream has the single-valued API. |
| `session_id` param removed from `emit_after_tool_call` calls in tests | ~8 del | **superseded** | Test fixtures adjusted for the non-keyed API. |
| `keyed_injection_isolated_per_session`, `clear_removes_only_that_session`, `deprecated_shims_use_empty_key` tests | ~30 del | **superseded** | Dev's daemon multi-session tests. Upstream has no equivalent (single-session only). |
| `cwd`, `env`, `env_stripped`, `env_warned` removed from `ToolCapabilities` in test fixtures | ~8 del | **superseded** | Dev's session-identity fields in test fixtures. |

### 1.9  `crates/agent-engine/src/extensions/loader.rs` (8 upstream-adds, 71 dev-dels)

| Hunk / symbol | Lines (upstream) | Classification | Evidence |
|---|---|---|---|
| `emit_session_start` extraction as a standalone fn | ~15 del | **superseded** | Dev extracted `emit_session_start` for reuse by `SessionActor::create`. Upstream inlines it in the spawn task. |
| `emit_session_end` extraction as a standalone fn | ~25 del | **superseded** | Dev extracted `emit_session_end` for reuse by actor teardown. Upstream doesn't have actor teardown. |
| `EngineHost::extensions_loading_guard` readiness protocol | ~15 del | **superseded** | Dev's `EngineHost` tracks extension loading state for the daemon. Upstream has no host. |
| Idempotency comment ("Process-level and IDEMPOTENT") | ~5 del | **superseded** | Dev's daemon C2 idempotency guarantee. Upstream rediscovers on every call. |

### 1.10  `crates/agent-engine/src/tools/subagent/start.rs` (20 upstream-adds, 17 dev-dels)

| Hunk / symbol | Lines (upstream) | Classification | Evidence |
|---|---|---|---|
| Description text — reactive vs poll-based subagent contract | 8 | **deliberate** | Dev's description says subagents are REACTIVE (completion events wake you). Upstream says poll with `subagent_status`. Dev's daemon architecture supports reactive events; upstream doesn't have the event-push mechanism. Dev's text is the correct description for the daemon model. |
| `spawn_runtime()` vs `Runtime::new()` | 2 del | **superseded** | Dev uses `spawn_runtime()` (host-aware). Upstream uses raw `Runtime::new()`. |
| `apply_subagent_runtime_policy` comment | 4 del | **superseded** | Different comment text, same function call. |
| `compose_system_prompt(…, runtime.memory_backend_is_axel())` vs `compose_system_prompt(…)` | 1 | **deliberate** | Dev passes the `forum` bool. Upstream always includes forum guidance. See §1.3. |
| `apply_anthropic_worker_reasoning` + `set_tools(subagent_tools())` | 2 | **deliberate** | Upstream has `apply_anthropic_worker_reasoning` inline + explicit `set_tools`. Dev's `spawn_runtime()` handles tools, and reasoning is applied by `apply_subagent_runtime_policy`. Different factoring. |
| `cancel_requested` pre-check on shutdown_rx | 6 | **deliberate** | Upstream checks `state_a.read().unwrap().cancel_requested` before spawning the cancel watcher, to honor an already-revoked session-driver launch fence. Dev doesn't have the session-driver launch fence. |

### 1.11  Remaining engine files (small diffs)

| File | upstream-adds | Classification | Notes |
|---|---|---|---|
| `subagent/resume.rs` | 14 | **superseded** | `spawn_runtime()` vs `Runtime::new()`, comment diff. |
| `subagent/oneshot.rs` | 9 | **superseded** | Same as resume.rs. |
| `subagent/collect.rs` | 4 | **deliberate** | Description text: "reactive" vs "poll" semantics. Same as start.rs. |
| `subagent/status.rs` | 1 | **deliberate** | Description text: "reactive" note removed. |
| `tools/discovery.rs` | 4 | **superseded** | `host_prompt_allowed` / `with_host_prompt` — dev has this for `activation_confirm = deny`. Upstream simplified. |
| `extensions/hooks/events.rs` | 4 | **superseded** | `hook_session_id_enabled()` env flag + `session_id` on hook events — dev daemon multi-session. |
| `tools/bash.rs` | 11 | **superseded** | `ctx.capabilities.cwd`/`env` plumbing. Dev daemon session-identity. Upstream uses process env/cwd. |
| `tools/ls.rs` | 4 | **superseded** | `resolve_path_in(…, ctx.capabilities.cwd)` → `expand_path(…)`. Session-identity cwd. |
| `tools/find.rs` | 3 | **superseded** | Same as ls.rs + `env_clear().envs()` for session env. |
| `tools/read.rs` | 3 | **superseded** | Same as ls.rs. |
| `tools/grep.rs` | 2 | **superseded** | Same as ls.rs + env. |
| `tools/write.rs` | 2 | **superseded** | Same as ls.rs. |
| `tools/edit.rs` | 2 | **superseded** | Same as ls.rs. |
| `tools/shell/start.rs` | 1 | **superseded** | `working_directory` fallback to `ctx.capabilities.cwd`. |
| `tools/mod.rs` | 2 | **superseded** | `spawn_runtime`/`legacy_fresh_runtime` re-export; `resolve_path_in` rename; `PromptKind` re-export; `cwd`/`env`/`env_stripped`/`env_warned` on `ToolCapabilities`. |
| `extensions/manager.rs` | 19 | **superseded** | `ProviderRegistry` import reorder; `shared` field on MCP lease config (daemon mode). |
| `events/registry.rs` | 11 | **superseded** | `REGISTRATION_KIND` const; `kind` field on `SessionRegistration` (daemon registry discrimination). |
| `mcp/lease.rs` | 8 | **superseded** | `shared: true` daemon-mode server field; comment about daemon shared leases. |
| `runtime/subagent.rs` | 2 | **superseded** | `serde` derives on `SubagentRow`/`SubagentOutcome` for `SessionEventWire` serialization (daemon). |
| `runtime/budget.rs` | 1 | **superseded** | Comment: `events.auto_turn_cap` configurable vs hardcoded. |
| `memory_backend/mod.rs` | 3 | **superseded** | `from_config_with_cwd` variant for daemon per-session cwd. |
| `lib.rs` | 3 | **superseded** | `daemon`, `host`, `session` module declarations; `EngineHost`/`HostParts`/`SessionCommand` re-exports. |

### 1.12  `crates/agent-core` files

| File | upstream-adds | Classification | Notes |
|---|---|---|---|
| `config.rs` | 8 | **superseded** | `ActivationConfirm` enum + parser; `events.auto_turn_cap` config; `write_comma_list` rename (`_locked` suffix). All daemon features. |
| `session.rs` | 6 | **superseded** | `Session.env`/`env_stripped` fields (session-identity); `follow_compaction_chain`/`ResolvedSession`/`MAX_COMPACTION_HOPS` (F24). Daemon features. |
| `session_journal.rs` | 5 | **superseded** | `env` field in `SessionMeta` serialization (session-identity). |
| `auth/mod.rs` | 1 | **superseded** | Minor (1-line diff). |
| `logging.rs` | 1 | **superseded** | Minor (1-line diff). |
| `Cargo.toml` | 1 | **superseded** | `tikv-jemalloc-ctl` dep for memstat. |

### 1.13  Binary-level / workspace files

| File | upstream-adds | Classification | Notes |
|---|---|---|---|
| `src/main.rs` | 9 | **superseded** | `--attach` / daemon auto-spawn subcommand; `MALLOC_CONF` formatting. |
| `src/cmd/server.rs` | 9 | **superseded** | `auto_turn_cap` plumbing; `wake_action_with_cap` → `wake_action`. |
| `src/lib.rs` | 1 | **superseded** | `EngineHost` re-export. |
| `Cargo.toml` | 5 | **superseded** | `legacy_inline` feature; workspace version references. |
| `Cargo.lock` | 4 | **superseded** | Lockfile drift. |
| `crates/agent-engine/Cargo.toml` | 3 | **superseded** | Workspace version; `serde_json` features. |

### 1.14  Documentation / assets

| File | upstream-adds | Classification | Notes |
|---|---|---|---|
| `AGENTS.md` | 36 | **SHOULD PORT** | Build-worker ceiling preference (8 jobs), Axel memory backend docs, context continuation `/context auto` docs. These are documentation-only additions that describe features already landed. Not LOST code, but missing docs. |
| `assets/help.json` | 21 | **SHOULD PORT** | `/budget` command help entry. The command exists on dev but the help.json entry is missing. |
| `docs/extensions/contract.json` | 58 | **deliberate** | Permission ordering change. Dev already has a different ordering from #120's landing. |
| `docs/sidecar-protocol.md` | 41 | **deliberate** | Session-driver sidecar docs. Dev will get these when the driver ports. |
| `docs/events-reactor.md` | 5 | **superseded** | `auto_turn_cap` docs. Dev has configurable cap. |
| `docs/extensions/README.md` | 1 | **SHOULD PORT** | "Drive a session" bullet. Session-driver documentation. |
| `docs/extensions/permissions.md` | 1 | **SHOULD PORT** | Session-driver permission docs. |
| `docs/tools.json` | 4 | **superseded** | Generated file. Will be regenerated. |

### 1.15  Test files

| File | upstream-adds | Classification | Notes |
|---|---|---|---|
| `tests/turn_budget_stream.rs` | 56 | **test-only** | `wall_clock_exhaustion_can_resume_retained_history_after_explicit_extension` — tests the enhanced budget error message + `/budget time` recovery. Requires the `exhaustion_error` fix in stream.rs to pass. |
| `crates/agent-engine/tests/mcp_lease_lifecycle.rs` | 14 | **superseded** | `shared: false` field, `cwd`/`env` fields removed. Daemon multi-session MCP. |
| `crates/agent-engine/tests/discovery_activation_tools.rs` | 8 | **superseded** | `cwd`/`env` fields; `activation_confirm` test (Prompt/Deny modes). |
| `crates/agent-engine/tests/deferred_host_context.rs` | 3 | **superseded** | `memory_backend` fixture change (explicit `legacy_current()`). |
| `crates/agent-engine/tests/tools_registry_iter.rs` | 1 | **superseded** | Tool count (28 vs 24). Dev already has the correct count. |
| `tests/phase3_activation.rs` | 5 | **superseded** | `cwd`/`env` fields; `shared` field. Daemon test fixtures. |
| `tests/extensions_e2e.rs` | 3 | **superseded** | `cwd`/`env` fields. Daemon test fixtures. |
| `tests/support/phase2/mod.rs` | 2 | **superseded** | `ANTHROPIC_MESSAGES_JSON` const (compaction fixture). Upstream version different. |
| `tests/phase5_disclosure.rs` | 1 | **superseded** | Actor path (`actor.rs`) vs TUI path (`dispatch.rs`). |
| `tests/phase5_compaction.rs` | 1 | **superseded** | Same as disclosure. |
| `tests/phase5_context_memory.rs` | 1 | **superseded** | Same as disclosure. |
| `tests/tools_export.rs` | 1 | **superseded** | Tool count. |
| `scripts/ignore-ratchet.sh` | 1 | **superseded** | Baseline count difference (2 vs 1 `#[ignore]`). |
| `docs/plans/…` | 1 | **superseded** | Plan doc minor diff. |

---

## §2  THE LOST LIST

### LOST-1: `await_provider_call` — pre-cancellation guard on provider IO

**File:** `crates/agent-engine/src/runtime/stream.rs`  
**Dev line:** ~1002 (the raw `ApiMethods::call_api_stream_inner(…).await` call)  
**Upstream line:** ~974 (`await_provider_call(&cancel, ApiMethods::call_api_stream_inner(…)).await`)  
**What breaks:** If `cancel.is_cancelled()` is already true when the stream loop reaches the provider call, dev polls the provider anyway — dispatching a real HTTP request that will be immediately abandoned. This wastes an API call (billed) and adds unnecessary latency to cancellation. Upstream's `await_provider_call` short-circuits with `Err(RuntimeError::Canceled)` before polling.  
**Severity:** Medium. Billed waste on fast cancellation. Observable when the user hits Ctrl-C during the budget/context assessment phase between tool rounds.  
**Test that catches it:** `provider_pre_cancellation_never_polls_ready_future` (upstream `stream.rs`, also listed as missing).  
**Fix:** Add `await_provider_call` as a free fn in `stream.rs` (15 lines). Wrap the `call_api_stream_inner` call.

### LOST-2: `await_tool_call` — started-flag for unstarted tool cancellation

**File:** `crates/agent-engine/src/runtime/stream.rs`  
**Dev line:** ~1299-1380 (the `tokio::select!` on `tool.execute_rich` vs `cancel.cancelled()`)  
**Upstream line:** ~1266-1337 (`await_tool_call(&cancel, tool.execute_rich(…)).await`)  
**What breaks:** Dev always treats a cancelled tool as "started" — `interrupted_started` ledger at `stream.rs:1372` fires for every cancelled tool regardless of whether it was polled. Upstream's `await_tool_call` returns `(None, false)` when cancellation wins before the first poll, so `started && CallLedger::interrupted_started(…)` only fires for tools that actually began executing. Consequence: dev over-reports interrupted side effects for non-idempotent tools that never ran.  
**Severity:** Low-Medium. The ledger warning is informational, but false positives train users to ignore real interrupted-write warnings.  
**Test that catches it:** `tool_pre_cancellation_never_polls_or_marks_ready_call_started` (upstream `stream.rs`).  
**Fix:** Add `await_tool_call` as a free fn (22 lines). Replace the `tokio::select!` block. Pipe the `started` bool through to the ledger check.

### LOST-3: `validated_tool_output` — model-aware attachment validation in the streaming path

**File:** `crates/agent-engine/src/runtime/stream.rs`  
**Dev line:** `stream.rs:1343` — `Ok(o) => o.into_parts()`  
**Upstream line:** `stream.rs:1303` — `Ok(o) => validated_tool_output(&model, o)`  
**What breaks:** On dev, a tool returning `ToolOutput::Blocks` with image content on a non-Anthropic model (e.g. Google Gemini) passes raw base64 image blocks into history unvalidated. The provider either errors cryptically or silently drops the content. Upstream validates with `attachments::validate_tool_blocks(model, blocks)` and replaces with `"Attachment not sent: {error}"` text. `validated_single_tool_output` in `mod.rs:227` covers the `run_single` path but NOT the streaming path.  
**Severity:** Low. Requires: (a) non-Anthropic model, (b) tool producing rich output with images, (c) streaming mode. Currently only `ReadTool` produces images and non-Anthropic providers are rare in practice.  
**Test that catches it:** `unsupported_or_malformed_media_becomes_explicit_text` (upstream `mod.rs`) covers the `run_single` path; a streaming variant is needed.  
**Fix:** Add `validated_tool_output` as a free fn in `stream.rs` (9 lines). Replace `o.into_parts()` with `validated_tool_output(&model, o)` at the two streaming tool-execution sites.

### LOST-4 (degraded): `budget_meter.exhaustion_error(dimension)` — enhanced budget failure message

**File:** `crates/agent-engine/src/runtime/stream.rs`  
**Dev line:** `stream.rs:511` — `agent_core::TurnError::budget(dimension)`  
**Upstream line:** `stream.rs:490` — `budget_meter.exhaustion_error(dimension)`  
**What breaks:** Budget exhaustion error message is the bare `TurnError::budget()` (e.g. "turn budget exhausted (wall_clock)") instead of the enhanced message with elapsed/limit seconds, `/budget status` instructions, "History retained" notice, and `/budget time` recovery instructions. The enhanced method exists at `budget.rs:249` — it's just not called from the streaming macro.  
**Severity:** Low. User experience degradation only — the turn still stops correctly.  
**Test that catches it:** `wall_clock_exhaustion_can_resume_retained_history_after_explicit_extension` in `tests/turn_budget_stream.rs` (also missing) asserts the enhanced message contains "/ limit 0s", "History retained", "/budget status".  
**Fix:** One-line change: replace `agent_core::TurnError::budget(dimension)` with `budget_meter.exhaustion_error(dimension)` in the `finish_budget_exceeded!` macro.

---

## §3  Portable test list

### stream.rs (25 tests)

| Test name | Kind | Port complexity |
|---|---|---|
| `await_provider_call` tests (3): `provider_cancellation_bounds_and_drops_uncooperative_pending_future`, `provider_cancellation_preserves_cooperative_partial_cleanup`, `provider_pre_cancellation_never_polls_ready_future` | unit | Verbatim (fn-level, no Runtime ctor) |
| `await_tool_call` tests (1): `tool_pre_cancellation_never_polls_or_marks_ready_call_started` | unit | Verbatim |
| `DropFlag` type + `drop` impl | fixture | Verbatim (test-local type) |
| `validated_tool_output` fn + validation test | unit | Verbatim |
| `drive_with_*` helpers (5): `drive_with_backend`, `drive_with_backend_and_steering`, `drive_with_budget`, `drive_with_context`, `drive_with_head_result` | fixture | Ctor adjustment needed: `StreamSession` has `session_id`/`cwd`/`env`/`env_stripped`/`env_warned`/`activation_confirm` on dev |
| Context continuation tests (6): `automatic_rollover_between_tool_rounds_preserves_evidence_and_pairs`, `automatic_rollover_save_failure_stops_before_second_provider_request`, `context_auto_time_boundary_at_low_pressure_requires_durable_successor`, `context_pressure_advisory_is_once_per_episode_not_per_request`, `normal_and_disabled_context_do_not_send_stale_pressure`, `steering_queued_during_setup_reaches_first_request_once` | integration | Ctor adjustment + `ContextEvidenceTool`/`SlowTimeBoundaryTool` fixtures |
| Rich output test (1): `history_media_limit_rejects_before_lossy_pruning` | integration | Ctor adjustment |
| Axel/production tests (2, `#[ignore]`): `real_axel_production_stream_captures_actual_final_turn`, `real_axel_rollover_has_one_history_authority_and_resumes` | integration | Ctor adjustment; require Axel service |

### mod.rs (14 tests)

| Test name | Kind | Port complexity |
|---|---|---|
| `runtime_clones_keep_author_and_each_worker_execution_forks` | unit | Verbatim (`new_headless`) |
| `rich_output` helper + `unchanged_summary_retains_rich_array_without_text_truncation` + `rewritten_or_truncated_summary_drops_rich_blocks` + `unsupported_or_malformed_media_becomes_explicit_text` + `plain_text_and_errors_keep_legacy_truncation` + `MODEL` const | unit (5 tests) | Verbatim |
| `terminal_source_boundary_excludes_prior_turns_and_survives_prepend_and_steering` | unit | Verbatim |
| `real_axel_terminal_worker_consumes_final_assistant_once` (`#[ignore]`) | integration | Requires Axel |
| `memory_backend_apply_config_revokes_preexisting_legacy_lease` + `memory_backend_exclusive_rejects_extension_recall_capture_and_history` + `memory_backend_first_config_is_immutable_and_changes_require_restart` + `memory_backend_path_changes_also_require_restart` + `axel_invalid_config_cannot_grant_consent_or_fallback` | unit/async | Verbatim (`new_headless` + `memory_runtime_with_providers`) |

### subagent/mod.rs (18 symbols, ~11 tests)

| Test name | Kind | Port complexity |
|---|---|---|
| `ExtensionProbe` + `Probe` fixture types | fixture | Verbatim |
| `prompt_always_includes_agent_and_forum_guidance` | unit | Adjust: `compose_system_prompt(…, true)` vs `compose_system_prompt(…)` |
| `disabled_forum_policy_applies_after_bare_and_extension_construction` | unit | Adjust: dev's `subagent_tools()` vs upstream's `configured_subagent_tools()` |
| `common_worker_policy_forks_author_without_mutating_parent` | unit | Verbatim |
| `all_launch_paths_use_common_author_registry_and_prompt_wiring` | unit | Verbatim |
| `fable_5_1_worker_uses_xhigh_exactly` | unit | Verbatim |
| `memory_backend_worker_inheritance_has_no_legacy_extension_fallback` | async | Adjust ctor |
| `Tool` trait impls (`name`/`description`/`parameters`/`execute`/`extension_id`/`id`/`handle`/`shutdown`/`call_tool`) on `ExtensionProbe` and `Probe` | fixture | Verbatim |

### turn_budget_stream.rs (1 test)

| Test name | Kind | Port complexity |
|---|---|---|
| `wall_clock_exhaustion_can_resume_retained_history_after_explicit_extension` | integration | Requires LOST-4 fix first. Uses `drive_runtime_turn` + `handle_engine_command("budget", "time 4h")`. |

---

## §4  Phase proposal: engine residuals

| Step | Work | Hours |
|---|---|---|
| LOST-1: `await_provider_call` | Add fn + wrap call site | 0.5 |
| LOST-2: `await_tool_call` | Add fn + replace select block + pipe started | 1.0 |
| LOST-3: `validated_tool_output` | Add fn + 2 call-site changes | 0.5 |
| LOST-4: budget exhaustion error | 1-line change | 0.1 |
| Port cancellation unit tests (4) | Verbatim copy | 0.5 |
| Port `validated_tool_output` + rich output tests (5) | Verbatim + ctor adjust | 0.5 |
| Port budget resume test (1) | After LOST-4 | 0.3 |
| Port memory-backend config tests (5) | Verbatim | 0.5 |
| Port forum/author tests (7) | Ctor adjust for `compose_system_prompt` bool | 1.0 |
| Port context continuation tests (6) | Ctor adjust for extra StreamSession fields | 2.0 |
| Port `AGENTS.md` + `help.json` + docs | Copy + review | 0.5 |
| Build gate (bella) | `cargo check/test/clippy` | 0.5 |
| **Total** | | **~8 h** |

---

## §5  Appendix: `scripts/merge/symbol-audit.py` output

```
== .rs files upstream has that we don't (engine/core/src):

== symbols (fn/const/type) upstream defines that our copy of the same file lacks:
  crates/agent-core/src/core/config.rs (1)
      fn:write_comma_list
  crates/agent-engine/src/runtime/mod.rs (14)
      const:MODEL
      fn:axel_invalid_config_cannot_grant_consent_or_fallback
      fn:memory_backend_apply_config_revokes_preexisting_legacy_lease
      fn:memory_backend_exclusive_rejects_extension_recall_capture_and_history
      fn:memory_backend_first_config_is_immutable_and_changes_require_restart
      fn:memory_backend_path_changes_also_require_restart
      fn:plain_text_and_errors_keep_legacy_truncation
      fn:real_axel_terminal_worker_consumes_final_assistant_once
      fn:rewritten_or_truncated_summary_drops_rich_blocks
      fn:rich_output
      fn:runtime_clones_keep_author_and_each_worker_execution_forks
      fn:terminal_source_boundary_excludes_prior_turns_and_survives_prepend_and_steering
      fn:unchanged_summary_retains_rich_array_without_text_truncation
      fn:unsupported_or_malformed_media_becomes_explicit_text
  crates/agent-engine/src/runtime/stream.rs (25)
      fn:automatic_rollover_between_tool_rounds_preserves_evidence_and_pairs
      fn:automatic_rollover_save_failure_stops_before_second_provider_request
      fn:await_provider_call
      fn:await_tool_call
      fn:context_auto_time_boundary_at_low_pressure_requires_durable_successor
      fn:context_pressure_advisory_is_once_per_episode_not_per_request
      fn:drive_with_backend
      fn:drive_with_backend_and_steering
      fn:drive_with_budget
      fn:drive_with_context
      fn:drive_with_head_result
      fn:drop
      fn:history_media_limit_rejects_before_lossy_pruning
      fn:normal_and_disabled_context_do_not_send_stale_pressure
      fn:provider_cancellation_bounds_and_drops_uncooperative_pending_future
      fn:provider_cancellation_preserves_cooperative_partial_cleanup
      fn:provider_pre_cancellation_never_polls_ready_future
      fn:real_axel_production_stream_captures_actual_final_turn
      fn:real_axel_rollover_has_one_history_authority_and_resumes
      fn:steering_queued_during_setup_reaches_first_request_once
      fn:tool_pre_cancellation_never_polls_or_marks_ready_call_started
      fn:validated_tool_output
      type:ContextEvidenceTool
      type:DropFlag
      type:SlowTimeBoundaryTool
  crates/agent-engine/src/tools/subagent/mod.rs (18)
      fn:all_launch_paths_use_common_author_registry_and_prompt_wiring
      fn:call_tool
      fn:common_worker_policy_forks_author_without_mutating_parent
      fn:configured_subagent_tools
      fn:description
      fn:disabled_forum_policy_applies_after_bare_and_extension_construction
      fn:execute
      fn:extension_id
      fn:fable_5_1_worker_uses_xhigh_exactly
      fn:handle
      fn:id
      fn:memory_backend_worker_inheritance_has_no_legacy_extension_fallback
      fn:name
      fn:parameters
      fn:prompt_always_includes_agent_and_forum_guidance
      fn:shutdown
      type:ExtensionProbe
      type:Probe
  src/cmd/chat.rs (3)
      fn:append_user_submission
      fn:list_pending_attachments
      fn:rejected_attachment_submission_retains_bytes_and_abort_context
  src/cmd/rpc.rs (8)
      fn:attachment
      fn:context_head_rejection_blocks_shutdown_save_and_auto_chain
      fn:load_rpc_user_content
      fn:persist_context_head
      fn:rpc_attachment_failure_does_not_append_or_reserve_a_turn
      fn:rpc_attachment_paths
      fn:rpc_attachment_paths_require_absolute_without_parent_components
      fn:rpc_loads_bytes_and_ignores_mime_and_name_hints

TOTAL missing symbols: 69
```

### Deeper than symbols — body-level changes the audit misses

The symbol audit catches missing fn/const/type definitions. It does NOT catch:

1. **Changed function bodies** — `finish_budget_exceeded!` macro at `stream.rs:508` calls `TurnError::budget(dimension)` instead of `budget_meter.exhaustion_error(dimension)`. Same fn signature, different call inside. (LOST-4)

2. **Inlined logic that replaced a named fn** — `activation_policy` exists on both, but upstream's `stream.rs` inlines the 2-way check without calling the fn. Dev's fn is richer (3-way). Not lost — dev is ahead.

3. **Wrapped call patterns** — the symbol audit sees `call_api_stream_inner` on both and says "present". It doesn't see that upstream wraps the call in `await_provider_call`. (LOST-1)

4. **Tool execution restructuring** — the `tokio::select!` vs `await_tool_call` wrapper. Both contain `tool.execute_rich`, but the cancellation semantics differ. (LOST-2)

5. **`o.into_parts()` vs `validated_tool_output(&model, o)`** — same output type, different validation. The audit sees `into_parts` on both and says "present". (LOST-3)

These five body-level gaps represent all the LOST production code. Everything else flagged by the 69-symbol count is either test-only (42 fns), superseded by daemon architecture, or deliberately different.

---

## §6  Summary verdict

| Category | Count | % of 69 symbols |
|---|---|---|
| **LOST (production)** | 4 (3 fns + 1 call-site) | 5.8 % |
| **test-only** | ~42 fns | 60.9 % |
| **superseded** (daemon arch / dev-ahead) | ~20 | 29.0 % |
| **deliberate** (different design choice) | ~3 | 4.3 % |

The engine half is *functionally* landed. The 4 LOST items are all in `stream.rs` and amount to ~50 lines of production code. The test gap is large (~42 functions) but structurally expected: the symbol audit's PROCEDURE.md lesson already noted that "~86 Class C" tests were skipped. The production behaviour those tests guard is partially covered by existing tests, but the cancellation and validation gaps (LOST-1 through LOST-3) have zero test coverage on dev.
