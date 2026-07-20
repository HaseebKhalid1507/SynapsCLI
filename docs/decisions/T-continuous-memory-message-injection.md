# Task B4 decision — memory recall wire injection as a separate synthetic message

Date: recorded with the task B4 implementation (spec §7.4, §5.3, §10.1–10.2).

## Decision

An accepted `MemoryContextContribution` enters the provider request as **its
own separate synthetic message object**:

```json
{"role": "user", "content": [{"type": "text", "text": "<rendered §10.2 block>"}]}
```

built by `memory_context::render_context_segment` and inserted into the turn's
`Vec<SharedMessage>` **immediately before the real new user message**, at the
single provider-agnostic entry point (`Runtime::run_stream_with_messages`,
before any per-provider wire translation forks). The memory text is:

- never merged into the user's own content array (spec §5.3.2 — memory is
  never presented as the user's current words);
- never merged into the system prompt (spec §5.3.3);
- wrapped in host-guaranteed lower-authority boundary lines (spec §5.3.4);
- neutralized before assembly: control characters and case-insensitive
  `<system` / `</system` / `<assistant` / `<user` (and other role/wrapper
  words) have their `<` replaced with the inert `‹`, so stored injection
  strings remain visibly quoted data (spec §5.3.5, §10.2).

Placing a user-role message directly after the previous assistant message
keeps Anthropic role-alternation valid while keeping memory in its own
message object.

## Alternative considered and rejected

**Synthesizing a fake `tool_use`/`tool_result` pair.** Anthropic requires
every `tool_result` block to reference the `tool_use_id` of a genuine
preceding `tool_use` (see `body_golden.rs` / `compaction.rs` / `stream.rs`).
Memory recall has no real tool call, so a fabricated pairing would forge
wire history, is fragile across providers (OpenAI/Gemini/broker translate
the same Vec), and one malformed pairing invalidates the whole request.
Rejected as too fragile; the separate-message form is valid on every
provider path without translation-layer special cases.

## Byte-identity-when-absent guarantee

When no validated contribution exists for the turn — memory off, no eligible
lease, budget floor unmet, timeout, transport error, malformed response, or
validator rejection — the messages Vec is **byte-identical** to pre-task-B4
behavior: no empty or no-op synthetic message is ever inserted, and the
disabled path performs zero `ExtensionLeaseCapability` calls. This is proven,
not asserted: unit tests serialize the Vec before/after
(`disabled_modes_make_zero_calls_and_leave_messages_byte_identical`,
`timeout_leaves_messages_byte_identical_and_retains_nothing`,
`runtime_recall_hook_is_byte_identical_when_memory_is_off`) and the frozen
`body_golden` fixture suite continues to pass unchanged.

Retry-exact semantics: one accepted contribution is retained per logical
request (keyed by a digest of the caller-supplied history) and reused by
retries — the provider is called at most once per logical request, and a
consumed one-shot never re-arms on retry.
