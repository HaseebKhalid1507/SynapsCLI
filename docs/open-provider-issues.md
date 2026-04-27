# Open Provider Issues

Tracked from S176 code review (8-agent review × 2 rounds). These require design decisions or significant implementation work.

---

## 1. `/compact` broken on OpenAI models 🔴

**Problem:** `/compact` uses `call_api_simple` which has no `try_route` — always goes to Anthropic. If a user is on `groq/llama-3.3-70b` and compacts, it either:
- (a) Uses `compaction_model` (defaults to `claude-sonnet-4-6`) → hits Anthropic, which fails if no Anthropic key
- (b) User sets `compaction_model = groq/llama-3.3-70b` → `call_api_simple` sends it to Anthropic → 400

**Fix:** Route `call_api_simple` through `try_route`, same as the streaming path. Or better — make compaction use the streaming path with a collected result.

**Files:** `src/runtime/api.rs` (`call_api_simple`), `src/runtime/mod.rs` (compact logic)

---

## 2. Cost display lies for non-Claude models 🟠

**Problem:** Status bar shows `$0.0087` computed from a Claude-only pricing table (opus/sonnet/haiku). Non-Claude models fall into `_ => (3.0, 15.0)` default — silently shows wrong dollar amounts using Sonnet pricing for Groq/Cerebras/etc. Cache hit rate (`↺`) is also Anthropic-specific.

**Options:**
- A) Show tokens only (no `$`) when on non-Anthropic model
- B) Add pricing per model to the registry
- C) Show `$—` to indicate unknown cost

**Files:** `src/chatui/draw.rs:584-609`, `src/chatui/app.rs:277-292`

---

## 3. No retry on OpenAI stream path 🟠

**Problem:** Anthropic path has exponential backoff on 429/500/502/503/529 (1s/2s/4s). OpenAI path has zero retry — any transient error kills the turn immediately. Mid-stream TCP hiccup = entire generation lost.

**Fix:** Port the retry envelope from `api.rs:180-230` to `stream.rs`. Retryable: 429, 500, 502, 503. Non-retryable: 401, 403, 404, 400.

Also: add idle-timeout on the stream loop — if no bytes for 30s, cancel (prevents 5-minute hang on TCP half-open).

**Files:** `src/runtime/openai/stream.rs`

---

## 4. `context_window_for_model` wrong for non-Claude 🟠

**Problem:** Returns 200k for everything non-Claude. Llama 3.3 = 128k, Gemini = 1M+, GPT-4o = 128k. The context-usage bar in the footer shows wrong ratios. `max_tokens_for_model` also wrong (128k for opus pattern, 64k otherwise).

**Fix:** Add context window + max output to the provider registry's model metadata:
```rust
pub models: &'static [(&'static str, &'static str, &'static str, u64)], // (id, label, tier, ctx_window)
```
Have `context_window_for_model` and `max_tokens_for_model` consult the registry first, fall back to Claude defaults only for `claude-*` models.

**Files:** `src/core/models.rs`, `src/runtime/openai/registry.rs`

---

## 5. Codex Responses API: `function_call.id` must start with `fc_` ✅ FIXED 2026-04-27

**Symptom:** `400 Bad Request: Invalid 'input[N].id': 'call_…'. Expected
an ID that begins with 'fc'.` after the second turn of any tool-using
conversation routed through `openai-codex`.

**Cause:** `codex_input_messages` echoed `call.id` (a `call_…` value) in
both the `id` and `call_id` fields. The Responses API requires `id` to
be the original `fc_…` output-item id, not the call correlation id.

**Hotfix (Option A, this commit):** Emit `id` only when the stored value
already starts with `fc`; otherwise omit the field. `call_id` alone is
sufficient to correlate `function_call_output` rows. Covered by 3 unit
tests in `codex_input_messages_tests`.

**Follow-up (Option B, deferred):** Track the `fc_` *output-item id* and
the `call_` *correlation id* as separate fields end-to-end (decoder →
`ToolCall` → re-emit). Only worth the diff if a future Responses
feature actually consumes the `fc_` id round-trip. Not needed today.

**Files:** `src/runtime/openai/stream.rs`
