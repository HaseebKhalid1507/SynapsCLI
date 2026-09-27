# A — Engine-half merge map + Wall 1 design

**Scope:** crates/agent-core, crates/agent-engine, sidecars/axel-memory-service, tests/, docs/, examples/extensions/autonomous, .cargo/config.toml, Cargo.* — everything in #112 EXCEPT crates/agent-tui and TUI-coupled src/cmd.

**Branches:**
- #112: `feat/context-continuation` @ `8bdabd4a` (jr-112 worktree)
- Target: `integration/daemon-soak-fixes` @ `f3c97caf` (soak-fixes worktree) = dev `a0b2b390` + PRs #115, #116
- Stale merge attempt: `integration/112-to-dev` (PR #114, documents Wall 1 + Wall 2)

---

## 1. Conflict file inventory — engine/core/src bucket

### Structural delta: files unique to each branch

| Only in soak-fixes | Only in upstream #112 |
|---|---|
| `agent-engine/src/daemon/` (6 files) | `agent-engine/src/attachments.rs` |
| `agent-engine/src/host.rs` (564 ln) | `agent-engine/src/extensions/session_driver.rs` (1887 ln) |
| `agent-engine/src/extensions/notify_router.rs` | `agent-engine/src/memory_backend/` (9 files) |
| `agent-engine/src/session/` (11 files: actor, transport, wire, etc.) | `agent-engine/src/runtime/attachments.rs` (1041 ln) |
| `agent-core/src/core/memstat.rs` | `agent-engine/src/runtime/axel_context.rs` (848 ln) |
| `agent-core/src/core/session_lock.rs` | `agent-engine/src/runtime/continuation.rs` (1406 ln) |
| — | `agent-engine/src/tools/context_checkpoint.rs` (39 ln) |
| — | `agent-engine/src/tools/forum.rs` (1022 ln) |
| — | `agent-core/src/core/context_archive.rs` |
| — | `agent-core/src/core/context_head.rs` (60 ln) |
| — | `agent-core/src/core/context_policy.rs` |
| — | `agent-core/src/core/session_save_order.rs` (57 ln) |
| — | `agent-core/src/memory/forum.rs`, `repository.rs` |

### Per-file conflict analysis

| # | File (crate relative) | Soak-fixes change from merge-base | upstream #112 change from merge-base | Conflict class | Hunk semantics |
|---|---|---|---|---|---|
| 1 | **agent-core/src/core/mod.rs** | +`memstat`, +`session_lock` modules | +`context_head`, +`context_policy`, +`session_save_order` modules, -`memstat` (moved?), `session_lock` → `session_save_order` | Keep-both mod declarations | **Mechanical.** Both add `pub mod` lines. Soak keeps `memstat` and `session_lock`; upstream replaces them. Resolution: keep all four soak modules + add upstream's three new ones. upstream's removal of `memstat`/`session_lock` is incorrect for soak-fixes (we still need them). FACT: soak-fixes:core/mod.rs:7–8 has `memstat`, `session_lock`; upstream:core/mod.rs:7–8 has `context_head`, `context_policy`, replaces `session_lock` with `session_save_order`. |
| 2 | **engine/setup.rs** (484 vs 515 ln) | Wholesale refactor to `EngineHost::boot_and_install` pattern: process-global singleton (profile, logging, HTTP client, registry, skills, MCP, extension manager built ONCE). Extracted `resolve_session_and_prompt()`, `spawn_session_background()`, `finish_session_setup()` as `pub(crate)` functions reused by `SessionActor::create`. Hook bus cleanup on drop. `SessionBootResult` is `pub(crate)`. | Inline boot: `config::set_profile`, `Runtime::new()`, inline MCP/skills/ext setup, inline background tasks. `SessionBootResult` is private. No `EngineHost`. | **Semantic — EngineHost refactor.** The deepest conflict. upstream's inline boot is incompatible with soak's per-session actor lifecycle. Resolution: take soak-fixes wholesale, then graft upstream's additions into the EngineHost path — specifically `reset_context_continuation()`, `memory_backend`, `bind_memory_backend`, `ContextManagementConfig`. |
| 3 | **extensions/manager.rs** (2835 vs 2834 ln) | +`DiscoveryState` enum (daemon C2: once-per-process walk record), `discovery` field on `ExtensionManager`, removed `PermissionSet` direct import, added `session_id` propagation via `with_session()` on hooks. | +`exclusive_memory` field (bool), +`eager_permissions` HashMap, +`bind_memory_backend()`, Axel conflict message, +`exclusive_memory` on `DeferredExtensionRecord`. No `DiscoveryState`. | **Semantic — keep-both.** Both add orthogonal fields to `ExtensionManager`. Resolution: keep soak's `DiscoveryState` + upstream's `exclusive_memory`/`eager_permissions`/`bind_memory_backend`. ~6 hunks, no overlap in struct layout. |
| 4 | **runtime/mod.rs** (6313 vs 5588 ln) | +`RuntimeParts` struct (shared host parts), `from_parts()` constructor, `build_host_http_client()`, `session_id`/`cwd` fields, `activation_confirm` field, `pub use stream::activation_policy`, `session_id` param on `emit_before/after_tool_call`. Removed `ToolOutput` (merged to separate path). | +`continuation` field (SharedContinuation), +`memory_backend`/`memory_backend_config`/`memory_backend_reconfigure_denied` fields, +`attachments` mod, +`axel_context` mod, +`continuation` mod, +`validated_single_tool_output()`, +`retain_single_tool_blocks()`, +`single_tool_result_content()`, +`terminal_capture_start/messages/dispatch_completed_terminal_capture`. No `RuntimeParts`, no `session_id`/`cwd` on Runtime. | **Semantic — heaviest merge.** Both deeply modify the Runtime struct and helper functions. Resolution strategy per region: (a) struct fields: keep both sets, (b) `emit_before/after_tool_call`: keep soak's `session_id` param + add it to upstream's call sites, (c) `RuntimeParts`/`from_parts`: keep soak, (d) upstream's new modules/functions: add as-is, (e) `activation_confirm`: keep soak. ~15 hunks, 5 semantic. |
| 5 | **runtime/stream.rs** (3591 vs 2527 ln) | +`session_id` param threading through stream function, +`activation_policy` module/function, +`select_tool_result_content` helper, `with_session()` on hook events. | +context continuation assessment loop (~300 lines around tool boundary), +checkpoint rejection logic (batched `context_checkpoint` → synthetic error results), +`bounded_tool_results` for attachments, +`ContextAdvisory` notifications, +`PreparedRollover`/`persist_head` integration, +ResponseStart/ResponseReset event handling. The empty-content guard at stream.rs:1026 is identical to soak's at stream.rs:722. | **Semantic — the F19 intersection zone.** See §2 below. |
| 6 | **tools/memory.rs** (1304 vs 715 ln) | Unchanged from dev (project-scope `host_scope()` with `ProjectScope::discover`). | Complete rewrite: `MemoryScope::Repository|User`, binding-based routing, user-scope opt-in, history source routing, Axel-exclusive host backend. | **Take upstream wholesale.** Soak's version is dev-stock; upstream's is the replacement. No conflict content to preserve. |
| 7 | **tools/subagent/mod.rs** (785 vs 518 ln) | +`spawn_runtime()` using `EngineHost::worker_runtime()`, +`legacy_fresh_runtime()` kill-switch, removed `apply_auth_config` from `apply_subagent_runtime_policy` (host-built worker already has creds). | +`memory_backend` param to `apply_subagent_runtime_policy`, +`apply_anthropic_worker_reasoning`, +`inherit_memory_backend()`, kept `apply_auth_config`. No `spawn_runtime`, no `EngineHost`. | **Semantic — spawn path.** Soak's `spawn_runtime` delegates to `EngineHost::worker_runtime()` which shares HTTP client/creds/token cache. upstream's inline `Runtime::new()` per spawn is the old pattern. Resolution: keep soak's `spawn_runtime()`, add upstream's `memory_backend` param + `apply_anthropic_worker_reasoning` + `inherit_memory_backend()` call into the host-built path. |
| 8 | **tools/subagent/oneshot.rs** (528 vs 518 ln) | Uses `super::spawn_runtime().await` (host-built), no explicit `set_tools` or `apply_auth_config`. | Uses `crate::Runtime::new().await` (fresh), explicit `set_tools`, explicit `apply_auth_config` via policy fn, +`memory_backend`, +`apply_anthropic_worker_reasoning`, +ResponseStart/ResponseReset events. | **Semantic — same spawn divergence.** Resolution: keep soak's `spawn_runtime()` call, add upstream's `memory_backend` threading + `apply_anthropic_worker_reasoning` + ResponseStart/Reset event handling. |
| 9 | **tools/subagent/resume.rs** (590 vs 572 ln) | Same `spawn_runtime()` pattern. | Same fresh-runtime pattern + `memory_backend` + `apply_anthropic_worker_reasoning`. | **Mechanical variant of #8.** |
| 10 | **tools/subagent/start.rs** (605 vs 587 ln) | Same `spawn_runtime()` pattern. | Same fresh-runtime + `memory_backend` + `apply_anthropic_worker_reasoning` + `ResponseStart/ResponseReset`. | **Mechanical variant of #8.** |
| 11 | **engine lib.rs** | +`daemon` mod, +`host` mod, +`session` mod, re-exports `EngineHost`, `HostOpts`, `HostParts`, `ClientTransport`, `Envelope`, `LocalTransport`, etc. Session fns re-exported from `agent_core::session`. | +`attachments` mod, +`memory_backend` mod. No `daemon`/`host`/`session` mods. Session fns re-exported from `session` (agent-core path). | **Keep-both.** Both add mods. Soak's re-exports from `agent_core::session` vs upstream's from `session` — soak is correct (upstream's relied on `pub use agent_core::session`). |
| 12 | **src/lib.rs** | +feature gate `legacy_inline`, daemon exports. | +`attachments` mod reference (for TUI). | **Mechanical.** Keep soak + add upstream's attachment mod. |
| 13 | **src/cmd/chat.rs** | Rewritten to SessionActor-based: `LocalTransport` → `link.submit()`, feature-gated `legacy_inline`. | Inline engine loop with `PendingAttachments`, `ContextHeadPersistence`, `context_head.is_blocked` guard. | **Take soak.** This is TUI-bucket (Brief says "EXCEPT TUI-coupled parts of src/cmd"). The attachment/context-head logic moves to the actor in Wall 1. |
| 14 | **src/cmd/rpc.rs** | +cap-aware auto-turn (`claim_auto_turn_with_cap`), +`context_head.is_blocked` guard from soak's F21 fix. | +`context_head.is_blocked` guard (upstream's version). | **Keep soak.** Both add the same guard; soak's version integrates with the actor. |

### PR #114 resolution validity

PR #114 (stale merge) resolved 29 engine hunks. upstream's subsequent commits changed the picture:
- `2db91388` (Sep 10): evict-oldest-history-at-seal — changes `continuation.rs` rollover/archive logic. Any #114 resolution of continuation.rs hunks is stale.
- `8bdabd4a` (Sep 12): stop pressure feedback loops — changes `continuation.rs` advisory logic and `stream.rs` context assessment. Any #114 resolution of stream.rs context-assessment hunks is stale.

**Verdict:** #114's engine-bucket resolutions are directionally correct (the EngineHost/spawn_runtime choices match) but every `stream.rs` and `continuation.rs` hunk must be re-resolved from scratch. The `runtime/mod.rs` struct-field hunks are still valid. Subagent spawn-path hunks are still valid. `tools/memory.rs` (take-upstream) is still valid.

---

## 2. runtime/stream.rs — F19 intersection analysis

**F19 bug (soak finding):** After a tool_result round where the model already emitted its text answer BEFORE the tool_use, the follow-up response has `content: []` (empty — the model said `end_turn` with nothing more to add). The empty-content guard at **soak:stream.rs:722** / **upstream:stream.rs:1026** treats this as a degenerate error, fires an `empty_response` TurnError, and returns `Ok(())` — dropping the entire turn from history (the earlier assistant text + tool_use + tool_result are never committed to `MessageHistory`).

**Both branches have the identical vulnerable code:**
```rust
// soak:stream.rs:722–736, upstream:stream.rs:1026–1040
if content.is_empty() {
    if !cancel.is_cancelled() {
        let _ = tx.send(StreamEvent::Session(SessionEvent::Error(
            agent_core::TurnError::provider(
                "model returned an empty response — ...",
                "empty_response", &turn_correlation_id,
            ),
        )));
    }
    let _ = tx.send(StreamEvent::Session(SessionEvent::MessageHistory(messages)));
    return Ok(());
}
```

**upstream's context changes nearby:** upstream adds a large block AFTER the empty-content guard and BEFORE the tool-use loop:
- **upstream:stream.rs:1057–1063**: Batched `context_checkpoint` rejection — if `tool_uses.len() > 1` and any is `context_checkpoint`, all get synthetic error results and `continue` (skip tool execution). This is ~15 lines inserted after `messages.push(assistant)` and before `assistant_text` extraction.
- **upstream:stream.rs:780–810** (earlier in the function): Context assessment/advisory loop, `PreparedRollover`, `persist_head` — runs BEFORE the provider request, not in the same region as F19.

**Verdict: F19 can be fixed in the same pass safely.** The fix is localized: change the early `return Ok(())` to commit the accumulated messages before returning (or treat a post-tool-result empty `end_turn` as a valid completion, not an error). upstream's context additions are BELOW the empty-content guard (they come after the assistant message is pushed to history). The fix does not interact with the context checkpoint rejection or the advisory loop. No interference.

**Recommended F19 fix site:** soak:stream.rs:722 / merged:stream.rs ~same region. The guard must distinguish "first response is empty" (genuinely degenerate) from "Nth round response is empty after tool_result" (valid end_turn). A `round > 0` or `has_tool_results_pending` flag suffices. The `MessageHistory` event already fires on the early return — the issue is that the actor treats the TurnError as "don't save." The fix belongs in stream.rs (don't fire the error on a valid post-tool-result empty end_turn) NOT in the actor save logic.

---

## 3. Wall 1 design — actor-owned durable context head

### Current state

**upstream's design** (context_head.rs:1–60, engine/session.rs:15–81): `ContextHeadReceipt` wraps a `oneshot::Sender<Result<(), String>>` behind `Arc<Mutex<Option<_>>>`. The runtime (continuation.rs:273–311, `persist_head()`) sends a `SessionEvent::ContextHeadCheckpoint { session_id, messages, receipt }` through the stream's `UnboundedSender<StreamEvent>` channel. The frontend (TUI event handler or chat inline loop) receives it, calls `conv.persist_context_head(session_id, candidate)` which does `session.save_durable()`, then `receipt.complete(Ok(()))`. The runtime blocks on `acknowledged.await` — no further inference until the ack arrives.

**Problem:** `ContextHeadReceipt` contains `Arc<Mutex<Option<oneshot::Sender>>>` — not serializable. Under soak-fixes, the stream runs inside `SessionActor` (engine-side), and events reach the TUI via `SessionEventWire` over `ClientTransport` (LocalTransport in-process, SocketTransport for daemon). `SessionEventWire` is serializable. The oneshot cannot cross this boundary.

**But:** The actor ALREADY owns the `Runtime`, the `ConversationState` (including `Session`), and the save logic (`actor.save()` at actor.rs:642–645). The checkpoint doesn't NEED to cross the wire — the actor can handle it entirely server-side.

### Design: actor-owned checkpoint persistence

#### Where in actor.rs

The `SessionActor` event loop (actor.rs, `run()` method in actor_cmds.rs) processes `StreamEvent` variants from the runtime's stream channel. Add a match arm for `SessionEvent::ContextHeadCheckpoint`:

```
// In the actor's stream-event dispatch (actor_cmds.rs, inside the
// stream event match):
SessionEvent::ContextHeadCheckpoint { session_id, messages, receipt } => {
    self.handle_context_head_checkpoint(session_id, messages, receipt).await;
}
```

New method on `SessionActor`:
```rust
async fn handle_context_head_checkpoint(
    &mut self,
    session_id: String,
    messages: Vec<SharedMessage>,
    receipt: ContextHeadReceipt,
) {
    // 1. Build candidate session from current conv state + new messages
    let mut candidate = self.conv.session.clone();
    candidate.api_messages = messages;

    // 2. Persist via the same save path (session_save_order ordering)
    let result = candidate.save_durable().await;

    // 3. On success: adopt the new head in conv
    if result.is_ok() {
        self.conv.api_messages = candidate.api_messages.clone();
        self.conv.session = candidate;
    }

    // 4. Complete the receipt — unblocks the runtime's persist_head() await
    receipt.complete(result);
}
```

**Key:** The receipt stays in-process (actor task → runtime stream task, both on the same tokio runtime). It never needs to cross the wire. The `SessionEventWire` enum does NOT need a `ContextHeadCheckpoint` variant — the actor intercepts it before wire serialization. The TUI sees a `SystemNotice("Context window N")` or similar, not the checkpoint itself.

#### Ordering vs journal save (session_save_order.rs)

upstream's `session_save_order.rs` (agent-core:core/session_save_order.rs:1–57) provides `acquire(dir, id) -> OwnedMutexGuard<()>` — a per-session async mutex keyed by `(PathBuf, String)`. This ensures that if a timed-out ordinary save's blocking writer is still running, a subsequent checkpoint save waits for it to finish before writing.

**Integration:** The actor's `handle_context_head_checkpoint` must use the same ordering primitive that `conv.save()` uses. In soak-fixes, `ConversationState::save()` (engine/session.rs:61–95) calls `session.save()` which ultimately goes through `Session::save_in_dir()`. The checkpoint save (`candidate.save_durable()`) must acquire the same `session_save_order` lock:

```rust
// Inside handle_context_head_checkpoint:
let dir = crate::config::resolve_write_path("sessions");
let _guard = agent_core::core::session_save_order::acquire(&dir, &session_id).await;
let result = candidate.save_durable().await;
// guard drops here, releasing the ordering lock
```

**FACT:** `session_save_order` is new in upstream's branch (doesn't exist in soak). It must be ported to agent-core as part of this merge. The soak `Session::save()` path must also acquire this lock to maintain ordering — otherwise an ordinary auto-save and a checkpoint save could race.

#### Ordering vs park (persist BEFORE Runtime drop)

Park sequence (actor.rs:759–800): `save()` → verify journal exists → `session_manager.shutdown_all()` → `conv.park_take()` → `runtime.park_take()` → drop both → purge arenas → release session lock.

**Checkpoint must happen BEFORE park.** If the runtime requests a checkpoint and then park fires:
1. The checkpoint receipt is waiting in the stream event channel.
2. Park calls `self.save()` first — this is the ordinary save, not the checkpoint.
3. Park then drops the runtime (which drops the stream channel sender).
4. The runtime's `persist_head()` await sees the channel close → returns error → `durability_blocked = true`.

**Design:** Park must drain pending checkpoint events before saving. Add to `park()`:
```rust
// Before self.save():
self.drain_pending_checkpoints().await;
```
This processes any buffered `ContextHeadCheckpoint` events, completing their receipts. The checkpoint save happens BEFORE the ordinary park save, which is correct — the checkpoint messages supersede the ordinary history.

**Alternative (simpler):** The runtime's `persist_head()` runs synchronously with the stream — it awaits the receipt before yielding control back to the stream loop. Park only fires when `streaming == false`. So there is no race: if streaming is active, park is deferred; once streaming ends, either the checkpoint completed or the stream returned an error. **No explicit drain needed if the stream always completes or errors before the actor's select! loop gets to the park timer.** INFERENCE: This should be verified by tracing the actor's select! priority — does the stream-completion arm fire before the park-deadline arm?

#### Ordering vs F10 journal lock

F10 (actor.rs:718–733): per-session flock acquired at actor create, released on park, reacquired on unpark. The flock is on `<sessions>/<id>.json` — the same file the checkpoint writes.

**Does the context-head file need the same lock?** No — the context-head checkpoint writes to the SAME session file (`<id>.json`), not a separate file. It goes through `save_durable()` which writes to the same path. The F10 flock already protects this file. The `session_save_order` async mutex provides in-process ordering; the F10 flock provides cross-process exclusion. Both cover the checkpoint path. **No additional lock needed.**

**Separate lock consideration:** If the context-head archive (under `context-archives/<scope-hash>/`) were a separate file, it would need its own lock. But upstream's archive is written by the runtime (continuation.rs:313+, `rollover()`) BEFORE the checkpoint event is sent — the archive write is complete before the actor sees the checkpoint. The archive and the session file are not written atomically together; the spec explicitly states "the archive is synced first; the session snapshot rename is the logical head commit" (context-continuation.md:159). The actor only does the session rename. **No additional lock.**

#### Daemon reload / rehydrate

On unpark (actor.rs, `unpark()` method): the session is loaded from disk, a fresh runtime is built via `EngineHost::foreground_runtime()`, and `finish_session_setup()` is called. The checkpoint's messages were saved to the session file — they load back correctly. The continuation state (`ContinuationState`) is on the `Runtime` and is re-initialized via `reset_context_continuation()` on the new runtime.

**Key:** `reset_context_continuation()` (called in setup.rs during session resolution) reads the saved session's api_messages and restores the continuation window/archive metadata from the synthetic continuation envelope marker (`_synaps_context` with `schema: "synaps-context-window/1"`). If the checkpoint saved successfully, the reloaded session has the new (shorter) message history, and the continuation state discovers the window number from the marker. INFERENCE: This should work as-is, but must be verified — does `reset_context_continuation` scan api_messages for the marker?

```rust
// From upstream's setup.rs (jr-112), in boot():
runtime.reset_context_continuation(&sb.session.id, &sb.api_messages);
```

In soak's `EngineHost` path, this call must be added to `resolve_session_and_prompt()` and also to the unpark/reload path.

#### LinkedSuccessor compaction id change

When a session is compacted, it may get a new id (compacted_into). The continuation state's `logical_id` tracks the session — if the id changes, the epoch check in `PreparedRollover::commit()` (continuation.rs:252–268) will reject stale checkpoints. This is correct behavior. After compaction, `reset_context_continuation()` must be called with the new id.

**In the actor:** After a successful compaction that changes the session id, the actor already calls `reacquire_session_lock(new_id)`. Add `runtime.reset_context_continuation(new_id, &conv.api_messages)` in the same path.

#### LocalTransport path — identical behavior

Under `LocalTransport` (in-process daemon or `synaps chat` via actor), the stream events flow through an `UnboundedSender` from the runtime stream task to the actor's select loop. The `ContextHeadCheckpoint` event travels this in-process channel. The actor handles it identically regardless of transport — the transport is only for CLIENT events (what the TUI sees), not for runtime→actor events. **No special LocalTransport handling needed.**

---

## 4. Memory backend / axel-memory-service under the daemon

### Architecture (FACT)

The sidecar is NOT a long-lived process. `process.rs:22–103` shows it spawns a NEW child process per operation via `tokio::process::Command`, with `env_clear()` and `kill_on_drop(true)`. Each call is: spawn → write request frame to stdin → read response frame from stdout → child exits. The sidecar binary (main.rs:3–53) takes `--brain <path> --project <scope>` as CLI args, reads ONE operation from stdin, executes it against the SQLite `.r8` database, writes the response to stdout, and exits.

**Consequence:** The sidecar is per-operation, not per-session or per-process. Under the daemon (one process, N sessions), every memory operation from any session spawns its own short-lived sidecar process. There is no shared sidecar state across sessions.

### MemoryBinding lifecycle (FACT)

`MemoryBinding` (memory_backend/mod.rs:37–45) wraps `Arc<Binding>` where `Binding` holds:
- `base: PathBuf` (base dir, typically `~/.synaps-cli`)
- `scope: Result<ProjectScope, String>` (repository-scoped or error)
- `author: Author` (fresh UUID per binding, `.child()` for workers)
- `operation_lock: Arc<tokio::sync::Mutex<()>>` (serializes operations)
- `repository: Option<RepositoryIdentity>` (git common-dir identity)
- `user_scope_enabled: bool`, `managed_parent: bool`
- `backend: Backend` (Legacy | Axel { executable, brain } | Unavailable)

`MemoryBinding::configured_current()` (mod.rs:71–102) reads the GLOBAL config and discovers the project scope from `std::env::current_dir()`. This is called at extension manager construction time.

### What breaks under daemon (one process, N sessions)

| Component | Issue | Severity |
|---|---|---|
| **`configured_current()` uses `std::env::current_dir()`** | Daemon has ONE process cwd. Sessions may serve different project directories (T1–T3 in session-identity plan). The binding captures the WRONG scope for non-cwd sessions. | 🔴 **Breaks.** The binding must be constructed with the session's cwd, not the process cwd. |
| **`operation_lock`** | One `Arc<Mutex>` per `MemoryBinding`. If sessions share a binding (e.g., same project), operations are serialized across sessions. If each session has its own binding (correct), the lock is per-session — fine. | 🟡 **Design-dependent.** Per-session bindings = correct isolation. |
| **Repository identity discovery** | `ProjectScope::discover_repository(cwd)` walks up from cwd looking for `.git`. Daemon process cwd may not be in any repo. | 🔴 **Breaks** if binding is constructed from process cwd. |
| **`forum_author()`** | `Author::fresh()` per binding. Each session gets its own author. Workers get `author.child()`. This is correct — no cross-session author leakage. | ✅ OK |
| **`exclusive_memory` in ExtensionManager** | `ExtensionManager::new()` (manager.rs:250–260) calls `MemoryBinding::configured_current().exclusive()` at construction. Under EngineHost, the extension manager is process-global. The `exclusive_memory` flag is set once from the global config. | 🟡 **Semantics question:** Is `exclusive_memory` per-process or per-session? If per-process (reasonable — it gates whether the axel-memory-manager plugin loads), it's fine. If it should vary per session, it breaks. INFERENCE: Per-process is correct — you either run Axel mode or legacy mode, not both in one daemon. |
| **User scope** | `for_user_notes()` (mod.rs:175–182) creates a user-wide scope binding. Under daemon, user-wide notes are shared across sessions (by design — they're USER-scoped). | ✅ OK |
| **Forum** | `forum.rs` uses the binding's project scope. Per-session bindings = per-project forums. | ✅ OK (if bindings are per-session) |

### Resolution (maps to session-identity plan T6/T7)

The `MemoryBinding` must be constructed per-session with the session's resolved cwd. In the `SessionActor::create()` path:

```rust
let memory_backend = MemoryBinding::from_config_with_cwd(
    &config.memory_backend,
    session_cwd.as_deref(), // from SessionConfig
);
```

This requires adding a `from_config_with_cwd` variant (or passing cwd into `from_config`). The current `configured_current()` remains valid for in-process single-session use (TUI, headless chat).

**Does it already key by session?** No — it keys by project scope, which is derived from cwd. Two sessions in the same project directory share the same scope (and thus the same brain file), which is CORRECT — they should see the same project memories. Two sessions in different directories get different scopes. The keying is by PROJECT, not by session. **This is correct semantics** but requires per-session cwd propagation under the daemon.

---

## 5. Attachments engine

### Wire compatibility (FACT)

**Soak-fixes** (session/types.rs:390): `SessionCommand::Submit { text: String, attachments: Vec<RpcAttachment> }` where `RpcAttachment` (agent-core/rpc_protocol.rs:60–73) is `{ path: String, name: Option<String>, mime: Option<String> }` — file paths only, the actor reads files.

**upstream #112**: `attachments.rs` (agent-engine/src/attachments.rs) provides `PendingAttachments`, `load_attachment()`, `build_user_content()`. These are TUI-side attachment loaders that read files and produce inline base64 content blocks. `runtime/attachments.rs` (1041 ln) is the offline preflight validator.

**Are they compatible?** They serve different layers:
- Soak's `RpcAttachment` is a wire protocol type — the CLIENT tells the actor "here's a file path."
- upstream's `PendingAttachments`/`load_attachment` is the CLIENT-SIDE loader that reads the file and produces content blocks.
- upstream's `runtime/attachments.rs` is the ENGINE-SIDE validator that checks content blocks against model capabilities.

Under the soak architecture, the flow would be:
1. TUI calls `load_attachment(path)` → gets content blocks (upstream's loader)
2. TUI sends `Submit { text, attachments: [RpcAttachment { path }] }` over the wire (soak's type)
3. Actor receives it, reads the file (or receives pre-loaded content), validates via `runtime/attachments.rs` (upstream's validator)

**Issue:** The soak wire type carries only file PATHS, not content. For daemon mode (SocketTransport), the actor would need to read the file from the path — but the file is on the CLIENT's filesystem, not necessarily accessible to the daemon process. For `LocalTransport` (in-process), it works because both share a filesystem.

**INFERENCE:** For engine-half-only merge, the runtime/attachments.rs validator and the content-block types can land without conflict. The TUI-side loader (`attachments.rs`) belongs to the TUI half. The wire type compatibility (how attachments cross the transport) is a separate concern that the TUI merge must address.

**Soak `SessionCommand::Submit.attachments` type vs upstream:** Soak already has `Vec<RpcAttachment>` which is upstream's `RpcAttachment` type (same struct in agent-core). They are identical — `path`, `name`, `mime`. **Compatible.**

---

## 6. Engine test compatibility

| Test file | What it tests | Assumes TUI plumbing? | Will run on merged tree? |
|---|---|---|---|
| `tests/context_rollover_recovery.rs` | Loopback SSE server, `Runtime::new()`, `run_stream_with_messages`, checkpoint tool, unproductive rollover | No — uses bare Runtime + stream, no TUI. Imports `synaps_cli::runtime::budget`, `synaps_cli::StreamEvent`. | ✅ **Yes** — needs `Runtime` with `continuation` field, which the engine merge provides. No actor or TUI dependency. |
| `tests/multimodal_attachments.rs` | Attachment JSON roundtrip, journal persistence, `attachments::build_user_content`, `validate_messages` | No — uses `Session`, `save_session_in_dir`, file I/O. Imports `synaps_cli::attachments`. | ✅ **Yes** — needs `attachments` module on the engine crate. |
| `tests/shared_memory_migration.rs` | Repository identity, scope discovery, migration preview, linking | No — uses `agent_engine::memory_backend::repository_migration`, `ProjectScope`, temp dirs. | ✅ **Yes** — pure memory-backend tests, no TUI/actor dependency. |
| `tests/autonomous_plugin.rs` | Plugin start/stop/poll/reply protocol, `session_driver` parsing | No — uses `ExtensionManager`, `HookBus`, `session_driver` module directly. | ✅ **Yes** — needs `session_driver` module in agent-engine. No TUI. |
| `sidecars/axel-memory-service/tests/*` (4 files) | Service protocol, forum, multi-project, process spawning | No — standalone sidecar tests. | ✅ **Yes** — independent crate, no engine/TUI coupling. |

**All engine tests should run unchanged on the merged tree.** None assume TUI plumbing. They use `Runtime::new()` directly (not `Engine::new()` or `SessionActor`), or test standalone modules.

---

## 7. Feature flags — can the engine half land dark?

### Current gating (FACT)

| Feature | Gate | Default | Where |
|---|---|---|---|
| Context continuation | `context_management.mode` config key | `off` | config.rs:225–228, continuation.rs:107 (`enabled()` checks `Auto`) |
| `/context auto\|off` command | TUI/headless command handler | Runtime-only (not persisted) | context-continuation.md:36–39 |
| `context_checkpoint` tool | Only registered when mode=Auto | Not in catalog when off | stream.rs context_enabled flag |
| Memory backend (Axel) | `memory.backend` config key | `legacy` | config.rs:1276–1279 |
| Session driver | `session.drive` extension permission | Requires explicit plugin install + permission grant | session-drivers.md:9–11 |
| Attachments | `/attach` command (TUI) | TUI-only entry point | multimodal-attachments.md:1 |
| Forum | `memory.forum_disabled_names` config | Enabled by default when Axel backend active | forum.rs |

### Can it land dark?

**Yes.** The engine half introduces:
1. **Context continuation** — gated by `context_management.mode = off` (default). No user sees it unless they explicitly enable it. The `context_checkpoint` tool is not registered. The continuation assessment loop in stream.rs is skipped (`context_enabled` flag). The `SharedContinuation` struct exists on `Runtime` but is inert.

2. **Memory backend** — gated by `memory.backend = legacy` (default). The `MemoryBinding` falls through to `Backend::Legacy`, and all operations go through the existing note store. The axel-memory-service sidecar is never spawned. Forum tools are not registered.

3. **Session driver** — engine-side code (session_driver.rs) is pure policy parsing. No TUI → no driver activation. The permission `session.drive` is never checked without a TUI host. The TUI half owns the state machine that actually runs the driver.

4. **Attachments runtime validator** — exists as a library. Called only from TUI attachment submission or stream tool-result processing. The `validate_messages` function is a no-op if no messages contain attachment blocks (which they won't without the TUI `/attach` command).

5. **New tools** (`context_checkpoint`, `memory_search source=history`, `memory_fetch`, `memory_forget` with ctx- IDs, forum tools) — only registered when their feature is enabled. `context_checkpoint` requires `context_management.mode = auto`. Forum tools require `memory.backend = axel`.

**No user-visible change until:**
- Config key `context_management.mode = auto` is set → enables context windows
- Config key `memory.backend = axel` is set → enables Axel memory
- Plugin with `session.drive` permission is installed and invoked → enables autonomous driver
- `/attach` command is available in TUI → enables attachments

---

## 8. Merge plan

### Phase 0: Cargo.toml alignment (S — 1h)

Soak-fixes uses `workspace.package` version `0.9.1` and `workspace.dependencies`. upstream uses inline `0.9.0` versions. Resolution: keep soak's workspace pattern, bump version if needed, add upstream's new dependencies.

**Files:** `Cargo.toml`, `crates/agent-core/Cargo.toml`, `crates/agent-engine/Cargo.toml`, `sidecars/axel-memory-service/Cargo.toml`
**Hunks:** All mechanical — version numbers and dependency declarations.
**Test:** `cargo check --workspace` on bella.

### Phase 1: agent-core additions (S — 2h)

Pure additions — no conflicts expected:
1. Add `pub mod context_head` to `core/mod.rs` (keep soak's `memstat`, `session_lock`)
2. Add `pub mod context_policy` to `core/mod.rs`
3. Add `pub mod session_save_order` to `core/mod.rs` (alongside soak's `session_lock`, not replacing it)
4. Add `pub mod forum` and `pub mod repository` to `memory/mod.rs`
5. Copy all new files: `context_head.rs`, `context_policy.rs`, `session_save_order.rs`, `context_archive.rs`, `forum.rs`, `repository.rs`
6. Add upstream's `ContextManagementConfig`, `ContextManagementMode`, `MemoryBackendConfig`, `MemoryBackendKind` to config.rs (these may partially exist — verify).

**Hunk note:** `core/mod.rs` is the "+1 trivial" conflict from COMMON.md. Both branches add mod declarations. Keep all of soak's + add upstream's. 

**Test:** `cargo test -p synaps-core` on bella. Expected: all existing tests pass + new unit tests in context_head, session_save_order.

### Phase 2: engine runtime/mod.rs (L — 4h)

The largest merge. Work region-by-region:

1. **Imports:** Add upstream's `pub mod attachments`, `mod axel_context`, `pub mod continuation`. Keep soak's `pub use stream::activation_policy`.
2. **Runtime struct fields:** Add upstream's `continuation: SharedContinuation`, `memory_backend: MemoryBinding`, `memory_backend_config`, `memory_backend_reconfigure_denied`. Keep soak's `session_id`, `cwd`, `activation_confirm`, `progressive_tool_disclosure`.
3. **`RuntimeParts` / `from_parts`:** Keep soak's. Add upstream's new fields to `RuntimeParts` or the `from_parts` initializer.
4. **`emit_before_tool_call` / `emit_after_tool_call`:** Keep soak's `session_id` parameter. All upstream call sites must pass `session_id` (typically `None` or threaded from `ToolCapabilities`).
5. **New functions:** Add upstream's `validated_single_tool_output`, `retain_single_tool_blocks`, `single_tool_result_content`, `terminal_capture_start/messages/dispatch_completed_terminal_capture` verbatim.
6. **`Runtime::new()`:** Keep soak's `from_parts` path. Initialize upstream's new fields with defaults.
7. **Accessor methods:** Add upstream's `reset_context_continuation`, `inherit_memory_backend`, `memory_backend_exclusive`, etc.

**Test:** `cargo check -p synaps-engine` on bella. Then `cargo test -p synaps-engine`.

### Phase 3: engine runtime/stream.rs (L — 5h)

The most complex merge. upstream adds ~1000 lines to a 2527-line file.

1. **Pre-request context assessment** (~upstream:780–810): Add the context assessment/advisory/rollover loop. This runs before `build_request`. No conflict with soak code — it's a new block in the request loop.
2. **Batched checkpoint rejection** (~upstream:1057–1063): Add after assistant message push, before tool loop.
3. **Tool execution:** Add upstream's `validated_single_tool_output`, `retain_single_tool_blocks`, `single_tool_result_content` to the tool result construction path. Keep soak's `session_id` parameter threading.
4. **`bounded_tool_results`:** Add upstream's attachment validation on tool results.
5. **F19 fix:** In the empty-content guard (~line 722/1026), add a `round > 0` check or equivalent to distinguish valid post-tool-result empty end_turn from degenerate first-response empty.
6. **Context-head checkpoint in stream result:** Add the `persist_head` / `PreparedRollover::commit` flow at the stream completion boundary.
7. **ResponseStart/ResponseReset events:** Add to the event emission points.

**Hunk note:** Every hunk must thread soak's `session_id` through upstream's new hook calls. upstream's `HookEvent::on_message_complete(...)` becomes `.with_session(session_id.as_deref())`.

**Test:** `cargo test -p synaps-engine -- stream` + the full `context_rollover_recovery` integration test. Expected: existing stream tests pass + new context tests pass.

### Phase 4: engine extensions/manager.rs + session_driver.rs (M — 2h)

1. **manager.rs:** Keep soak's `DiscoveryState`. Add upstream's `exclusive_memory`, `eager_permissions`, `bind_memory_backend()`, `DeferredExtensionRecord.exclusive_memory`. ~6 hunks, no overlap.
2. **session_driver.rs:** Copy upstream's file verbatim (1887 ln, pure policy — no TUI dependency). Add `pub mod session_driver` to `extensions/mod.rs`.

**Test:** `cargo test -p synaps-engine -- extension` + `autonomous_plugin` integration test.

### Phase 5: engine tools (M — 3h)

1. **tools/memory.rs:** Take upstream wholesale (1304 ln replaces 715 ln). The old `host_scope` helper is subsumed.
2. **tools/context_checkpoint.rs:** Copy verbatim (39 ln). Add to tools mod.
3. **tools/forum.rs:** Copy verbatim (1022 ln). Add to tools mod.
4. **tools/subagent/{mod,oneshot,resume,start}.rs:** Keep soak's `spawn_runtime()` / `EngineHost::worker_runtime()` pattern. Into the shared policy function `apply_subagent_runtime_policy`, add upstream's `memory_backend` param + `inherit_memory_backend()` call. Add `apply_anthropic_worker_reasoning`. In each spawn site, thread `memory_backend` from `ToolCapabilities`. Add ResponseStart/ResponseReset event handling to oneshot/start event loops.

**Test:** `cargo test -p synaps-engine -- subagent` + `cargo test -p synaps-engine -- memory`.

### Phase 6: memory_backend + sidecar (M — 2h)

1. Copy entire `memory_backend/` directory (9 files, ~3000 ln total). Add `pub mod memory_backend` to engine lib.rs.
2. Copy/update `sidecars/axel-memory-service/` (new crate). Update workspace Cargo.toml members.
3. Wire `memory_backend` into `Runtime` initialization (the `from_parts` path).

**Test:** `cargo test -p synaps-engine -- memory_backend` + `cargo test -p synaps-axel-memory-service`. Expected: `shared_memory_migration` integration test passes.

### Phase 7: attachments engine + new modules (S — 1h)

1. Copy `runtime/attachments.rs` (1041 ln), `runtime/axel_context.rs` (848 ln), `runtime/continuation.rs` (1406 ln).
2. Copy `attachments.rs` (TUI-side loader — but it's in agent-engine, not agent-tui; it can land).
3. These are already wired in Phase 2 (mod declarations) and Phase 3 (stream integration).

**Test:** `multimodal_attachments` integration test.

### Phase 8: engine/setup.rs integration (M — 3h)

The keystone — everything wired together:
1. Keep soak's `EngineHost::boot_and_install` pattern wholesale.
2. In `resolve_session_and_prompt()`: add `runtime.reset_context_continuation(&sb.session.id, &sb.api_messages)`.
3. In `finish_session_setup()`: add `MemoryBinding` construction, `ext_mgr.bind_memory_backend()`.
4. In `spawn_session_background()`: no changes needed (background tasks are transport-agnostic).
5. In `EngineHost::foreground_runtime()`: ensure `continuation`, `memory_backend` fields are initialized.
6. In `EngineHost::worker_runtime()`: ensure `memory_backend.fork_for_worker()` is called.

**Test:** Full `cargo test --workspace` on bella. Expected test count: soak-fixes baseline + upstream's new tests.

### Phase 9: Wall 1 implementation (M — 3h)

Implement the actor-owned checkpoint persistence design from §3:
1. Add `ContextHeadCheckpoint` match arm to actor's stream event dispatch.
2. Implement `handle_context_head_checkpoint` on `SessionActor`.
3. Integrate `session_save_order::acquire` into the checkpoint save path.
4. Add `reset_context_continuation` call to unpark/reload path.
5. Add `reset_context_continuation` call after compaction id change.
6. Wire `ContextHeadPersistence` state into `ConversationState` (from upstream's engine/session.rs).

**Test:** Targeted test: mock a `ContextHeadCheckpoint` event → verify save → verify receipt completion. Then full workspace tests.

### Phase 10: Integration verification (M — 2h)

Live verification on bella (daemon mode):
1. ✅ Start daemon, create session, verify normal operation
2. ✅ Enable `context_management.mode = auto`, verify context pressure notices
3. ✅ Trigger rollover (low thresholds), verify archive + head persistence
4. ✅ Park session, verify save completes before runtime drop
5. ✅ Unpark/reload, verify continuation state restored from disk
6. ✅ F10 journal lock: verify no flock conflict during checkpoint save
7. ✅ `memory.backend = legacy` (default): verify no Axel operations attempted
8. ✅ Extension loading: verify `exclusive_memory` flag propagates

---

## Risks (ranked)

| # | Risk | Impact | Likelihood | Mitigation |
|---|---|---|---|---|
| 1 | **stream.rs merge errors** — 1000+ added lines interleaved with soak's session_id threading; one wrong hunk boundary and the stream loop misorders tool results or drops events | Data loss / crash | High | Hunk-by-hunk review, test each tool-execution path individually. F19 fix adds a regression test. |
| 2 | **RuntimeParts / from_parts drift** — upstream's new Runtime fields not properly initialized via the EngineHost path → None/default values where live state is expected | Silent feature breakage | Medium | Exhaustive field audit: every field on Runtime struct must appear in RuntimeParts or from_parts. |
| 3 | **session_save_order not integrated into soak's save path** — ordinary saves and checkpoint saves race | Data corruption (head rollback) | Medium | The lock must be added to ALL save paths (conv.save, save_durable, checkpoint). Verify with concurrent-save test. |
| 4 | **MemoryBinding uses process cwd under daemon** — sessions get wrong project scope | Wrong project memories | High (daemon), None (TUI) | Per-session binding with cwd propagation. Can defer to session-identity plan T1–T3. |
| 5 | **Continuation state not reset on compaction/session-switch** — stale epoch blocks checkpoint acks | Inference blocked until restart | Low | Add reset_context_continuation calls to compaction and session-switch paths. |
| 6 | **serde_json feature mismatch** — soak needs `float_roundtrip`, upstream doesn't specify it | Pricing rounding differences | Low | Keep soak's `features = ["float_roundtrip"]`. |

---

## Open decisions for Haseeb

1. **F19 fix: same pass or separate?** The analysis shows no interference, but it's a pre-existing bug with its own soak finding. Ship in the merge pass (less risk of forgetting; it's 5 lines) or as a separate stacked PR for clean attribution?

2. **MemoryBinding cwd: defer to session-identity T1–T3?** The per-session cwd issue is known and tracked. For the engine merge, `configured_current()` (process cwd) is correct for single-session hosts (TUI, chat). Daemon correctness requires T1. Land the engine half with process-cwd semantics (matches upstream's design) and fix in T1?

3. **session_save_order: agent-core or agent-engine?** upstream puts it in agent-core. Soak's save paths are in agent-engine (ConversationState) and the actor. The lock primitive should live where both can reach it. agent-core seems correct (it's a low-level ordering primitive).

4. **Continuation state on EngineHost vs per-Runtime?** upstream's `SharedContinuation` is per-Runtime (Arc<Mutex<ContinuationState>>). Under EngineHost, runtimes are per-session (foreground) or per-worker (fork). Workers don't get continuation. The per-Runtime placement is correct. Confirm: should the host cache the continuation config so unpark restores it without re-reading disk config?

5. **Version bump?** Soak is `0.9.1`, upstream is `0.9.0`. The merge adds significant new capability. `0.10.0`? Or stay `0.9.1` since features are dark?

---

## Size estimates

| Phase | Work item | Size | Hours |
|---|---|---|---|
| 0 | Cargo.toml alignment | S | 1 |
| 1 | agent-core additions | S | 2 |
| 2 | runtime/mod.rs merge | L | 4 |
| 3 | runtime/stream.rs merge + F19 | L | 5 |
| 4 | extensions/manager.rs + session_driver.rs | M | 2 |
| 5 | tools (memory, checkpoint, forum, subagent) | M | 3 |
| 6 | memory_backend + sidecar | M | 2 |
| 7 | attachments engine + new modules | S | 1 |
| 8 | engine/setup.rs integration | M | 3 |
| 9 | Wall 1 (actor-owned checkpoint) | M | 3 |
| 10 | Integration verification (daemon live) | M | 2 |
| — | **Total** | — | **28h** |

Buffer for unexpected coupling: +30% → **~36h (4–5 days)**.

Critical path: Phase 2 → Phase 3 → Phase 8 → Phase 9. Phases 4–7 can be parallelized after Phase 2.
