# PLAN-SSE.md — Typed SSE Events Refactor

**Target:** `src/runtime/api.rs` :: `call_api_stream_inner` — replace `serde_json::Value` string-key parsing of Anthropic SSE events with typed serde enums.
**Branch:** `dev`. Baseline: `9ba29c3`, 919 lib tests green.
**Precedent:** `src/runtime/openai/wire.rs` (`RawChunk`/`RawDelta` — owned-String style; we improve on it with borrowed `Cow`).
**Invariant:** `accumulated_content: Vec<Value>` stays `Value` — it is the *outgoing* Anthropic message format fed back into history. Only the incoming parse path changes.
**Authored:** S206 (2026-06-11), zero (architect) on recon by chrollo. Verified against live code at `9ba29c3`.

---

## Verification discoveries (corrections to the original recon)

1. **Tail-flush spans L479–510** (recon said ~483–510 — trivial drift). Main match L292–475 and end-of-stream flush L512–534 exact.
2. **LATENT DOUBLE-EMIT BUG (pre-existing, found during verification):** the tail-flush at L485–506 pushes thinking/tool_use blocks but never resets `in_thinking`/`in_tool_use`, so the end-of-stream flush at L512–534 pushes the *same block again* when the final `content_block_stop` arrives without a trailing newline. Slice 2 fixes this by construction.
3. **`&'a str` fields are UNSOUND for this use.** `serde_json::from_str` into `&'a str` hard-fails at runtime on any JSON string containing an escape (`\n`, `\"`, `\uXXXX`) — and text deltas contain escapes constantly. **Use `Cow<'a, str>` + `#[serde(borrow)]`** — borrows on the escape-free fast path, allocates only when escaped. (The recon's "silent String fallback" risk was inverted: actual failure mode is a hard parse error, not a silent allocation.)
4. `api_sync.rs`, `stream.rs`, `subagent.rs` contain no SSE parsing — `api.rs` is the sole battlefield.

## 0. Verified site map (live code, commit 9ba29c3)

| Site | Lines | Role |
|---|---|---|
| Main event match | `api.rs:292–475` | 8 arms: content_block_start/delta/stop, message_start/delta/stop, wildcard |
| TTFT capture | `api.rs:286–290` | `first_event_seen` gate, before the match |
| Tail-flush (partial final line) | `api.rs:479–510` | Duplicated `content_block_stop` logic — **carries the double-emit bug** |
| End-of-stream accumulator flush | `api.rs:512–534` | Unconditional flush of partial thinking/tool/text |
| Mutable loop state declarations | `api.rs:237–256` | Enumerated in §2 |

---

## 1. Slice breakdown

### Slice 1 — `sse_types.rs`: typed wire model + unit tests (additive, zero integration)

**Files:** `src/runtime/sse_types.rs` (new, ~140 LOC types + ~150 LOC tests), `src/runtime/mod.rs` (+1 line `mod sse_types;`).
**Pure addition** — nothing references it yet. Mark module `#![allow(dead_code)]` with `// TODO(slice 3): remove`.

```rust
//! Typed wire model for Anthropic SSE events. Borrowing deserializer:
//! `Cow<'a, str>` borrows from the line buffer on the escape-free fast
//! path, allocates only when JSON escapes force it. NOT &'a str — serde
//! hard-errors on escaped strings for &str targets.

use serde::Deserialize;
use std::borrow::Cow;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum AnthropicEvent<'a> {
    ContentBlockStart {
        #[serde(borrow)]
        content_block: ContentBlock<'a>,
    },
    ContentBlockDelta {
        #[serde(borrow)]
        delta: Delta<'a>,
    },
    ContentBlockStop,
    MessageStart {
        #[serde(borrow)]
        message: MessageStartPayload<'a>,
    },
    MessageDelta {
        #[serde(borrow, default)]
        delta: Option<MessageDeltaInner<'a>>,
        #[serde(default)]
        usage: Option<UsagePayload>,
    },
    MessageStop,
    /// Unit variant required: serde's #[serde(other)] only supports unit
    /// variants under internal tagging. Payload discarded — matches the
    /// current `_ => {}`. Covers `ping`, `error`, and future event types.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ContentBlock<'a> {
    Text,      // initial `text` field ignored — current code never reads it
    Thinking,  // initial `thinking`/`signature` ignored — same
    ToolUse {
        // #[serde(default)]: current code does .as_str().unwrap_or("") —
        // a missing id/name must not kill the event. Cow: Default = Borrowed("").
        #[serde(borrow, default)]
        id: Cow<'a, str>,
        #[serde(borrow, default)]
        name: Cow<'a, str>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Delta<'a> {
    TextDelta     { #[serde(borrow)] text: Cow<'a, str> },
    ThinkingDelta { #[serde(borrow)] thinking: Cow<'a, str> },
    SignatureDelta{ #[serde(borrow)] signature: Cow<'a, str> },
    InputJsonDelta{ #[serde(borrow)] partial_json: Cow<'a, str> },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub(super) struct MessageStartPayload<'a> {
    #[serde(borrow, default)]
    pub id: Option<Cow<'a, str>>,
    #[serde(default)]
    pub usage: Option<UsagePayload>,
}

#[derive(Debug, Deserialize)]
pub(super) struct MessageDeltaInner<'a> {
    #[serde(borrow, default)]
    pub stop_reason: Option<Cow<'a, str>>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct UsagePayload {
    // #[serde(default)] mirrors .as_u64().unwrap_or(0)
    #[serde(default)] pub input_tokens: u64,
    #[serde(default)] pub output_tokens: u64,
    #[serde(default)] pub cache_read_input_tokens: u64,
    #[serde(default)] pub cache_creation_input_tokens: u64,
    #[serde(default)] pub cache_creation: Option<CacheCreationDetail>,
}

#[derive(Debug, Deserialize)]
pub(super) struct CacheCreationDetail {
    #[serde(default)] pub ephemeral_5m_input_tokens: Option<u64>,
    #[serde(default)] pub ephemeral_1h_input_tokens: Option<u64>,
}
```

**Design rationale:**
- `Cow<'a, str>`, not `&'a str` — see discovery #3. Tripwire test asserts `matches!(.., Cow::Borrowed(_))` on escape-free input so a dropped `#[serde(borrow)]` is caught.
- `Unknown` sub-type variants on `ContentBlock`/`Delta` too — current code has `_ => {}` at all three match levels; an unknown inner type must not fail the whole event.
- `#[serde(default)]` everywhere current code uses `unwrap_or` — typed parsing must not be stricter for fields tolerated missing today. (Whole-event drop on malformed known payload is an accepted tightening — §5 Q3.)
- No `deny_unknown_fields` — Anthropic adds fields without warning; extra fields (`index`, initial `content_block.text`, `stop_sequence`) ignored.

**Gate:** `cargo test --lib` 919 + ~14 new green; clippy clean. **LOC:** ~290 new.

### Slice 2 — `ParseState` + `process_event` consolidation, **still on `Value`**

**Files:** `src/runtime/api.rs` only.

1. Introduce `ParseState` (§2), module-private.
2. Extract match body (L292–475) into `fn process_event(event: &Value, state: &mut ParseState, ctx: &EventCtx)`.
3. Extract end-of-stream flush (L512–534) into `ParseState::finalize(&mut self)`.
4. **Delete the duplicated tail-flush logic** (L484–507) — tail path calls `process_event` like the main loop. The `content_block_stop`-only filter dies; all event types in a partial final line are handled (strictly more correct).
5. Move TTFT capture (L286–290) into `process_event` so main and tail paths are uniform.
6. **Test seam:** `fn process_data_line(data: &str, state: &mut ParseState, ctx: &EventCtx)` — parses then dispatches; both loop and tail call it. All slice-2 tests target this fn so they survive slice 3 unchanged.
7. `#[cfg(test)] mod tests` in `api.rs` with the §3 suite.

**This slice fixes the double-emit bug** — the `content_block_stop` arm clears flags so `finalize()` cannot re-push. Declare the behavior fix in the commit (or land as separate micro-commit first — §5 Q1).

**Gate:** full suite green; equivalence + bug-fix tests; live smoke (streamed turn with tool call). **LOC:** ~±350 moved, ~60 net new + ~180 tests.

### Slice 3 — Swap `Value` → `AnthropicEvent` inside the seam

**Files:** `src/runtime/api.rs`, `src/runtime/sse_types.rs` (remove `allow(dead_code)`).

1. `process_data_line` parses `from_str::<AnthropicEvent>` instead of `Value`.
2. `process_event(event: AnthropicEvent<'_>, ...)` — enum matches; all `event["..."]` indexing in the event path dies.
3. `Unknown` arm: `tracing::trace!` with truncated raw `data_part` (≤200 bytes) — the tag is discarded by `#[serde(other)]`, the raw line is the only forensics. Plumb raw `&str` from `process_data_line`.
4. Lifetimes (verified sound): main loop — `line` borrows `SseLineBuffer.buf`, no `extend()` between parse and synchronous `process_event` completion. Tail — `remaining: String` local outlives the parse-and-process block. Borrow checker enforces all of it.
5. Keep `Err(_) => continue` for malformed JSON.

**Gate:** slice-2 tests pass **unchanged** (the seam's whole point), + new typed tests; full suite green; live smoke (thinking + tool call + multi-byte output). **LOC:** ~150 changed api.rs, ~−10 sse_types.rs, ~60 tests.

---

## 2. `ParseState` — complete enumeration of mutable loop state

All 14 mutable bindings declared `api.rs:237–256`, verified against all read/write sites:

```rust
/// All mutable state for one SSE stream parse. Mutated exclusively through
/// process_event() + finalize() — single write path makes duplicate-site
/// drift structurally impossible.
struct ParseState {
    // ── Output accumulation (stays Value — outgoing message format) ──
    accumulated_content: Vec<Value>,          // L237
    current_text: String,                     // L238
    // ── Tool-use block accumulation ──
    current_tool_name: String,                // L248
    current_tool_id: String,                  // L249
    current_tool_input_json: String,          // L250
    in_tool_use: bool,                        // L251
    // ── Thinking block accumulation ──
    current_thinking: String,                 // L254
    current_thinking_signature: String,       // L255
    in_thinking: bool,                        // L256
    // ── Telemetry captures ──
    telem_msg_id: Option<String>,             // L241  (message_start)
    telem_ttft: Option<u64>,                  // L242  (first event)
    telem_stop_reason: Option<String>,        // L243  (message_delta)
    telem_usage: telemetry::UsageRecord,      // L244  (message_delta)
    first_event_seen: bool,                   // L245  (TTFT gate)
}

impl ParseState {
    fn new() -> Self;
    /// End-of-stream flush — from api.rs L512–534 verbatim. Idempotent:
    /// clears in_* and current_text so a second call is a no-op.
    fn finalize(&mut self);
}

/// Immutable per-stream context — not state.
struct EventCtx<'t> {
    tx: &'t mpsc::UnboundedSender<StreamEvent>,
    telemetry_level: TelemetryLevel,   // Copy
    request_start: std::time::Instant, // Copy — TTFT basis
}
```

**Deliberately excluded:** `line_buffer` (transport), `stream`/`cancel` (I/O), `telem_request_id`/`telem_ratelimit` (captured from headers pre-stream, L223–233), request-side vars used only in post-stream telemetry (L536–572). Do NOT smuggle `tx` into ParseState — read/write separation is the point.

---

## 3. Test list per slice

### Slice 1 — `sse_types::tests`
- `content_block_start_tool_use` — ToolUse id/name extracted; extra `index` ignored
- `content_block_start_tool_use_missing_id_name_defaults_empty` — mirrors `unwrap_or("")`
- `content_block_start_thinking_and_text` — variants match; initial payload ignored
- `content_block_start_unknown_block_type` — inner Unknown, event still parses
- `text_delta_borrows_when_escape_free` — `matches!(Cow::Borrowed(_))` **borrow tripwire**
- `text_delta_with_escapes_is_owned_and_correct` — `"line\\none \\u00e9"` → Owned, == "line\none é"
- `text_delta_multibyte_utf8` — raw multi-byte, borrowed, byte-identical
- `thinking_delta_signature_delta_input_json_delta` — each variant, right source field
- `delta_unknown_subtype` — Delta::Unknown, event parses
- `message_start_full` — id + nested usage incl. cache fields
- `message_delta_stop_reason_and_usage` — usage sibling of delta; ephemeral_{5m,1h} land
- `usage_missing_fields_default_zero` — `{}` → zeros, cache_creation None
- `unknown_event_type_is_unit_unknown` — fnord/ping/error → Unknown
- `content_block_stop_and_message_stop_parse` — tolerate extra fields

### Slice 2 — `api::tests` via `process_data_line` seam
- `text_deltas_accumulate_then_flush_on_block_stop`
- `second_text_block_start_flushes_prior_text` (L312–320 branch)
- `tool_use_full_lifecycle` (start → deltas → stop; parsed input; flags cleared)
- `tool_use_invalid_json_yields_parse_error_object` (`__parse_error` contract, L14–22)
- `tool_use_empty_input_yields_empty_object`
- `thinking_lifecycle_with_signature`
- `empty_thinking_block_never_emitted` (Anthropic-rejection guard, L362–373)
- `message_delta_captures_usage_stop_reason_telemetry` (incl. TTL breakdown + hit_pct)
- `message_start_captures_msg_id_and_usage`
- `all_zero_usage_emits_no_event` (gate at L420, L459)
- `ttft_set_once_on_first_event`
- `tail_path_then_finalize_no_double_emit` — **regression test for the discovered bug**
- `finalize_flushes_partial_{text,thinking,tool}` (L512–534 contract; empty-thinking suppressed)
- `finalize_is_idempotent`
- `done_marker_and_non_data_lines_skipped` (`[DONE]`, `: keepalive`, `event: foo`)

### Slice 3 — typed-path additions (slice-2 suite must pass UNCHANGED — that is the gate)
- `unknown_event_type_no_state_change` — state bit-identical, zero events
- `malformed_json_line_skipped` — truncated JSON, no panic
- `multibyte_utf8_text_delta_end_to_end` — raw + escaped (`\u2728 h\u00e9llo`) variants, byte-identical through full pipeline
- `event_with_unknown_delta_subtype_ignored_gracefully`
- `tail_partial_line_typed_parse` — borrow-from-owned-String path via take_remaining()-shaped input

Frozen safety net: `sse.rs` exhaustive re-chunking tests (transport framing, orthogonal).

---

## 4. Ordering rationale + rollback

Decompose along the **diagnostic axis** — never mix novel-risk (types), restructuring (extraction), and bug-fix in one commit:
- Slice 1: zero blast radius; front-loads the only novel risk (serde tagged-enum + Cow borrows) into unit-test territory.
- Slice 2: pure restructuring of proven logic + one declared bug fix; representation unchanged so regressions are attributable to extraction.
- Slice 3: representation swap behind a seam pinned by ~15 tests; failures are by elimination the typed parse.
- 2 and 3 NOT reorderable: consolidate the three duplicated sites first, then transmute the single organ.

| Slice | Rollback | Blast radius |
|---|---|---|
| 1 | `git revert` — additive | Zero |
| 2 | `git revert` — api.rs-local. Caveat: resurrects double-emit bug; cherry-pick the two `in_* = false` lines if fix must survive | One file, no API change |
| 3 | `git revert` — seam restores Value parsing behind identical interface; slice-2 tests re-validate | One file + dead-code re-allow |

Per slice: one commit, `cargo test --lib && cargo clippy -- -D warnings` green, live smoke for 2 and 3.

---

## 5. Open questions for the implementer

1. **Double-emit fix disclosure:** land inside slice 2 or as its own two-line micro-commit before it (bisectable)? Recommendation: separate micro-commit. Bugs deserve their own tombstones.
2. **TTFT from tail path:** moving `first_event_seen` into `process_event` means a degenerate stream whose only event is a partial final line now records TTFT (was None). Strictly more correct; confirm telemetry consumers don't care.
3. **Strictness tightening:** typed parse drops a whole event when a known type has malformed payload, where Value code partially processed it. Accepted (half-parsed is worse than dropped) — but it's a contract change.
4. **Unknown-event forensics:** trace vs debug level? And: Anthropic `error` events (currently swallowed by wildcard) deserve promotion to a real arm surfacing `SessionEvent::Notice` — out of scope, five-line follow-up, note in commit.
5. **EventCtx vs flat params:** either fine; ParseState must stay pure mutable state.
6. **`LlmEvent::Text(String)` alloc:** Cow fast path still pays `.to_string()` at `tx.send`. Changing LlmEvent to Cow/Arc<str> is cross-cutting — explicitly OUT OF SCOPE. Resist.
7. **Fixture style:** inline `r#"..."#` JSON (matches sse.rs style) over tests/fixtures/. Locality beats reuse at this scale.
