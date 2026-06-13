# Runtime perf-scan — bug class: O(total) work for O(slice) need

**Scope:** `crates/agent-engine/src/runtime/{mod,api,api_sync,sse,stream,compaction,telemetry,openai/*}`, `crates/agent-engine/src/events/`, `crates/agent-engine/src/sidecar/`. Branch `dev`, read-only audit, no build attempted.

**Premise:** the same shape that bit `latest_session()` / `list_sessions()` — touch all of the data when you only need a slice — also lives in the per-turn API path. The session has 827 messages; the loop pays for all of them on every turn, not just the new tail.

Severity legend: 🔴 critical (per-turn × full conversation) · 🟠 high · 🟡 medium · ⚪ low.

---

## 🔴 P1 — Full deep-clone of the entire conversation on every API turn

**Files / lines:**
- `runtime/api.rs:553` — `let mut cleaned_messages = messages.to_vec();` (streaming path)
- `runtime/api_sync.rs:78` — same pattern (sync path)
- `runtime/openai/mod.rs:184` — `messages: messages.to_vec()` for the extension provider path

**What scales:** O(N) `serde_json::Value::clone()` where N = total messages in the session. `Value::clone()` is a recursive walk; for an 827-message session whose content blocks include long tool results, this is a multi-MB deep clone per turn.

**Trigger:** every single LLM call inside the agentic loop (`stream::run_stream_internal` → `ApiMethods::call_api_stream_inner` → clone). Tool-using turns spin this loop many times per user prompt.

**Why it exists:** `annotate_cache_breakpoint` (helpers.rs:174) mutates only the *last* message; `sanitize_thinking_blocks` rewrites in place. The full clone is defensive — it preserves the cache-stable prefix of the caller's `messages: Vec<Value>` — but the cost is O(N) when only the tail needs touching.

**Fix direction:**
- Store messages as `Vec<Arc<Value>>` (or `Vec<Arc<MessageBlock>>`); pass `&[Arc<Value>]` through the API path. Then `to_vec()` clones N Arc pointers, not N JSON subtrees.
- Or: build the request body with `messages` borrowed via `Vec<&Value>` and clone only the last message (the only one that needs `cache_control` attached). One owned message, N−1 borrowed.
- Invariant: keep `messages` sanitized on insert (see P2) so the per-request "clean" pass becomes O(1) on the tail.

---

## 🔴 P2 — `sanitize_thinking_blocks` rescans the entire conversation every turn

**File / lines:** `runtime/helpers.rs:95-153`, called from `api.rs:556` and `api_sync.rs:81`.

**What scales:**
- Pass 1: `messages.iter_mut().enumerate()` over **all** messages, `content.retain()` over every block of every assistant message → O(Σ blocks).
- Pass 2: `messages.remove(i)` in a back-to-front loop → each remove is O(N) memmove on `Vec<Value>` (worst case O(N²) if multiple drops).
- Pass 3: adjacency merge loop with `messages.remove(i+1)` (O(N) memmove) **plus** `next["content"].clone()` and `messages[i]["content"].clone()` — full subtree deep clones of message content arrays.

**Trigger:** every API turn, on the freshly-cloned `cleaned_messages` from P1. The work is almost entirely wasted: a message that was sanitized last turn is still sanitized this turn. Only the most recently appended assistant message can introduce a new empty thinking block.

**Fix direction:** sanitize *once*, on append, inside `stream.rs:185` where the assistant response is pushed. Then the per-request pass collapses to a debug-assert or is removed entirely. If preserved for paranoia, scope it to `messages.last()`.

**Bonus:** the back-to-front `remove` + adjacency merge can be rewritten as a single forward-walk that builds a new `Vec` (single allocation, O(N)) instead of N memmoves + N content clones.

---

## 🟠 P3 — Full request body re-serialized on every retry attempt

**Files / lines:** `runtime/api.rs:644-665` (`req.json(&body).send().await` inside the retry loop, MAX_429_RETRIES = 8), `runtime/api_sync.rs:131-143` (same pattern).

**What scales:** `reqwest::RequestBuilder::json(&body)` calls `serde_json::to_vec(body)` per attempt. `body["messages"]` is the entire conversation as `Value`. For 827 messages this is a full JSON serialization of the whole conversation per retry.

**Trigger:** every retry attempt. During a 429 storm with `MAX_429_RETRIES = 8`, the **same payload** is re-serialized up to 8 times — pure waste, the bytes are identical.

**Fix direction:** serialize once outside the retry loop into `bytes::Bytes` (or `Vec<u8>`), then `.body(bytes.clone())` + `.header("content-type", "application/json")` per attempt. `Bytes::clone()` is a refcount bump, not a copy.

---

## 🟠 P4 — `messages_to_oai` walks the conversation twice with content clones

**File / lines:** `runtime/openai/translate.rs:113-200` (called from openai routing per request after `messages.to_vec()` at `openai/mod.rs:184`).

**What scales:**
- First pass (lines 129-144): O(N) walk of all messages just to build `tool_use_id → tool_name` HashMap.
- Second pass (lines 146-…): O(N) walk to translate, with `s.clone()` on every user text (`:153`), `s.clone()` on tool-result strings (`:175`), `.collect::<Vec<_>>().join("")` per tool-result array (`:177-182`) — fresh allocations proportional to content size.

**Trigger:** every request when an OpenAI-compat provider is selected.

**Fix direction:**
- Fuse the two passes: build the id→name map lazily, or carry tool_name forward on the assistant message you just visited (alternation guarantees the matching tool_result is in the very next user message).
- Use `&str` / `Cow<str>` through `ChatMessage` instead of cloning every text fragment.
- For an Anthropic-only deployment this path is unreachable per request, but `openai::try_route` is called *unconditionally* at `api.rs:532` and `api_sync.rs:50` — verify it short-circuits cheaply before the conversation enters the loop.

---

## 🟠 P5 — Sync blocking I/O on the async hot path, per request

**Files / lines:**
- `runtime/helpers.rs:303-316` — `log_usage` does `std::fs::OpenOptions::open` + `writeln!` per request (called from `api.rs:361, 449`, `api_sync.rs:237, 426`).
- `runtime/telemetry.rs:275-307` — `write_record` does `std::fs::metadata` + `std::fs::rename` + `std::fs::OpenOptions::open` + `writeln!` per terminal SSE event (called from `api.rs:869`).

**What scales:** not the *count* of work, but the *placement*: these run inside `tokio::spawn`'d futures and block the worker thread for the duration of the syscall. With telemetry enabled and a slow disk, every turn parks a worker. Compounds with P3 if many requests run in parallel (subagents).

**Trigger:** per API request (log_usage); per SSE completion (write_record).

**Fix direction:** route both through a single `tokio::sync::mpsc` to a dedicated writer task (one open fd, no per-call open/close), or `tokio::task::spawn_blocking` for the write. Same fix solves both call sites.

---

## 🟡 P6 — `ToolRegistry::clone()` per loop iteration

**File / line:** `runtime/stream.rs:119` — `let tools_snapshot = tools.read().await.clone();`

**What scales:** size of the registered tool set. With dynamically-registered MCP tools (lines 469-474 add tools mid-session), this grows during the session. The snapshot is cloned every iteration of the agentic loop so the iteration sees a stable schema even if MCP tools register concurrently.

**Trigger:** every iteration of the tool loop.

**Fix direction:** wrap `ToolRegistry` in `Arc<ArcSwap<ToolRegistry>>` — `load()` is a single atomic, no clone. Or cache `tools_schema()` as `Arc<Vec<Value>>` on the registry and bump generation on register; the loop reads the Arc and only re-fetches when generation changes.

---

## 🟡 P7 — `serde_json::Value` allocation per SSE event (OpenAI path)

**File / lines:** `runtime/openai/stream.rs:392` — `serde_json::from_str::<Value>(payload)` per chunk; lines 401, 421 `delta.to_string()` per text/tool delta.

**What scales:** number of SSE events in the stream (scales with output length). Each parse allocates a fresh `Value` tree; each `delta.to_string()` allocates a fresh `String`.

**Trigger:** every SSE chunk on the OpenAI-compat streaming path.

**Fix direction:** mirror the Anthropic path (`api.rs:175` parses into the typed `AnthropicEvent<'a>` with borrowed `&str` via `serde(borrow)`). Define a typed `OaiEvent<'a>` enum, parse with zero allocation for the matched arms.

**Note:** the Anthropic SSE path (`runtime/sse.rs`) is already well-built — cursor-based, no per-line allocations, periodic compaction at `pos > 4096`. Keep that. The leakage is on the OpenAI path only.

---

## 🟡 P8 — Registry re-reads every file per resolution

**File / lines:** `events/registry.rs:127-153` (`list_active_sessions_in`), called from `find_session_registration_in:164` then walked three times (`:167, :172, :177`).

**What scales:** number of `.json` files in `registry_dir()` — bounded to *live* sidecar/agent sessions, but it's the same bug class as `latest_session`: read+parse all when you only need a match. With many concurrent agents this becomes a per-resolve scan.

**Fix direction:** if `query` is an exact ID, try `read_to_string(dir/query.json)` first — O(1) hit covers the common case. Fall back to a directory scan only for name/prefix lookup.

---

## ⚪ Lower / verified-safe (worth a note, not a fix)

- **`stream.rs:126`** — `messages.iter().rev().find(|m| m["role"]=="user")` per turn. Worst-case O(N) but last user is typically at the tail → O(1) in practice. Leave it.
- **`stream.rs:84,168,206,485,496`** — `SessionEvent::MessageHistory(messages)` over an mpsc channel. Looks like full-history send-per-cycle, but each is on a *terminal* branch (done / error / cancel) — exactly once per stream completion, and it's a `Vec` *move*, not a clone. Fine.
- **`runtime/sse.rs`** — already optimal: cursor-based, zero-alloc line yield, periodic compaction. Reference implementation for P7.
- **`events/queue.rs`** — bounded `VecDeque` with capacity check on push. No unbounded growth. `std::sync::Mutex` held briefly; not across `.await`. Fine.
- **`api.rs` ParseState** — `current_text` / `current_thinking` / `current_tool_input_json` strings are `clear()`-ed on content-block boundaries. `accumulated_content` lives for the duration of one response. No unbounded growth.

---

## What gets worse as the session grows (827-msg case study)

| Path | Cost per turn @ N=10 | Cost per turn @ N=827 | Slope |
|------|----------------------|-----------------------|-------|
| P1 `messages.to_vec()` | 10 deep Value clones | 827 deep Value clones | linear in N |
| P2 `sanitize_thinking_blocks` | 10 iter + ~0 remove | 827 iter + up to N² memmoves | super-linear in N |
| P3 retry body serialization | small JSON × R | ~conversation-size JSON × R | linear in N × R |
| P4 `messages_to_oai` | 2 × 10 walk | 2 × 827 walk + per-msg clones | linear in N |

P1 and P2 compound: P1 produces the cloned vec that P2 then walks. Eliminating P1 (Arc-share) without fixing P2 keeps the walk; fixing P2 (sanitize-on-insert) without P1 keeps the clone. Both should land together for a clean O(1)-on-tail per turn.

## Recommended fix order

1. **P2 first** — sanitize-on-insert in `stream.rs:185` (assistant push) and `stream.rs:478` (tool-result push). Drop the per-request scan to a `debug_assert!` on the tail. Cheap, high impact, no API change.
2. **P1 next** — migrate `messages: Vec<Value>` → `Vec<Arc<Value>>` end-to-end (api, api_sync, openai, stream). Largest mechanical change but converts the per-turn O(N) clone into O(N) refcount bumps (~ns each).
3. **P3** — pre-serialize the body once per request, `.body(bytes.clone())` per retry. Localised, no type changes.
4. **P5** — single background writer task for both `log_usage` and `write_record`. Localised.
5. **P4 / P7** — OpenAI path tightening. Lower priority unless the user actually routes through an OpenAI-compat provider.
6. **P6 / P8** — opportunistic.

## Watch list (after fixes land)

- Once messages move to `Arc<Value>`, ensure `engine/stream.rs:122` (`*messages = history`) still works — the engine layer holds owned `Vec<Value>`. Either lift the Arc up to engine, or keep an `into_owned()` boundary at the channel.
- `runtime/stream.rs:469-474` registers MCP tools mid-loop with `tools.write().await` — confirms the tool registry can mutate during a single stream. P6's `ArcSwap` fix must handle that.
- If `extensions::audit::append_audit_entry` (called from `openai/mod.rs:163`) also does sync file I/O, P5's writer task should subsume it.
