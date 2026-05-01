# Extension Providers

Extension providers let plugins register model backends that Synaps routes through the core runtime. Providers are discovered during extension `initialize` and are addressed as:

```text
<plugin_id>:<provider_id>:<model_id>
```

## Registration metadata

An extension returns providers from `initialize`:

```json
{
  "protocol_version": 1,
  "capabilities": {
    "providers": [{
      "id": "local",
      "display_name": "Local Provider",
      "description": "Runs local models",
      "models": [{
        "id": "model-small",
        "display_name": "Model Small",
        "capabilities": { "streaming": false, "tool_use": true },
        "context_window": 32768
      }]
    }]
  }
}
```

`capabilities.tool_use: true` declares that a model may emit mediated tool calls. Synaps surfaces this in extension status/model UX as a `tool-use` badge.

## Completion request

Synaps calls `provider.complete` with Anthropic-shaped messages and the active tool schema:

```json
{
  "provider_id": "local",
  "model_id": "model-small",
  "model": "plugin:local:model-small",
  "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
  "system_prompt": null,
  "tools": [{"name": "read", "description": "...", "input_schema": {"type": "object"}}],
  "temperature": null,
  "max_tokens": null,
  "thinking_budget": 0
}
```

## Streaming completion request

When a model declares `capabilities.streaming = true` in its `initialize` capability metadata, Synaps may call `provider.stream` instead of `provider.complete`. The params shape is identical to `provider.complete`:

```json
{
  "provider_id": "local",
  "model_id": "model-small",
  "model": "plugin:local:model-small",
  "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
  "system_prompt": null,
  "tools": [],
  "temperature": null,
  "max_tokens": null,
  "thinking_budget": 0
}
```

While the request is in flight the extension emits zero or more `provider.stream.event` JSON-RPC notifications. Each notification's `params` is discriminated by a `"type"` field:

| `type`     | Shape |
| ---------- | ----- |
| `text`     | `{ "type": "text", "delta": "<chunk>" }` |
| `thinking` | `{ "type": "thinking", "delta": "<chunk>" }` |
| `tool_use` | `{ "type": "tool_use", "id": "...", "name": "...", "input": { ... } }` |
| `usage`    | `{ "type": "usage", "input_tokens": N, "output_tokens": N, ... }` |
| `error`    | `{ "type": "error", "code": "...", "message": "..." }` |
| `done`     | `{ "type": "done" }` |

The final JSON-RPC response carries the same shape as `provider.complete`:

```json
{
  "content": [{"type": "text", "text": "hello world"}],
  "stop_reason": "end_turn",
  "usage": {"input_tokens": 4, "output_tokens": 2}
}
```

### Capability declaration

A model opts in by setting `capabilities.streaming = true` in its `initialize` response. The UX surfaces this as a `[streaming]` badge in `/extensions` and the model picker.

### Routing rule

`try_route` calls `provider.stream` when **both** of the following hold:

1. The selected model declares `capabilities.streaming = true`.
2. The request is not already inside an active provider tool loop.

Otherwise `try_route` falls back to `provider.complete`.

### Example stream sequence

A fixture answering `"hi"` might emit:

```text
→ provider.stream                  (request)
← provider.stream.event { "type": "text", "delta": "hello " }
← provider.stream.event { "type": "text", "delta": "world" }
← provider.stream.event { "type": "usage", "input_tokens": 4, "output_tokens": 2 }
← provider.stream.event { "type": "done" }
← { "result": { "content": [...], "stop_reason": "end_turn", "usage": {...} } }
```

### Known limitations

- **Tool-use during streaming is currently ignored by the router.** A `tool_use` notification emitted mid-stream is logged at `warn` and dropped. Streaming + tool-use combinations should fall back to non-streaming `provider.complete` with the existing tool loop.
- **60-second hard timeout** on the `provider.stream` call (matches `provider.complete`). On timeout the call returns `Err` and the notification subscription is cleared.
- **Malformed `provider.stream.event` notifications are logged at `warn` and dropped**; they do not abort the in-flight call. Notifications whose method is not `provider.stream.event` are ignored entirely.
- If the consumer's event sink is dropped mid-stream, forwarding stops but the in-flight `provider.stream` request is still allowed to complete and return its final response.

## Tool-use response shape

A provider requests tools by returning `content` blocks with type `tool_use`:

```json
{
  "content": [{
    "type": "tool_use",
    "id": "call-1",
    "name": "read",
    "input": {"path": "README.md"}
  }],
  "stop_reason": "tool_use"
}
```

Requirements:

- `id` must be a non-empty string and is echoed in the tool result.
- `name` must be a non-empty tool name from the provided tool schema.
- `input` must be a JSON object. Missing `input` is treated as `{}`.

Malformed `tool_use` blocks fail the provider turn and are reported as extension provider errors.

## Tool result loop

When Synaps receives tool-use blocks it:

1. Appends the provider response as an assistant message.
2. Executes each requested tool through the core `ToolRegistry`.
3. Runs normal `before_tool_call` and `after_tool_call` extension hooks around execution.
4. Appends a user message containing `tool_result` blocks.
5. Calls `provider.complete` again until the provider returns no tool-use blocks.

Tool results are Anthropic-shaped content blocks:

```json
{
  "type": "tool_result",
  "tool_use_id": "call-1",
  "content": "file contents..."
}
```

Unknown tools, blocked tools, and execution failures are returned to the provider as `tool_result` blocks with `is_error: true` when applicable. The provider should recover or return a final user-visible error.

## Configuration and diagnostics

Provider extensions declare configuration in two places:

- The plugin manifest's `extensions[].config` array — describes non-secret config entries the extension expects (`key`, `description`, `required`, `default`, `secret_env`).
- Per-provider `config_schema` returned from `initialize` — currently a JSON object with optional `required: [string]` listing keys the provider needs at runtime.

Synaps resolves config values at extension load time using this precedence (see `src/extensions/manager.rs::resolve_config`):

1. Process env override `SYNAPS_EXTENSION_<EXTENSION_ID>_<KEY>` (uppercased, dashes → underscores).
2. The entry's `secret_env` env var, if declared.
3. Persisted config key `extension.<extension-id>.<key>`.
4. The entry's `default` value.
5. Otherwise: missing — load fails if `required: true`.

### Inspecting config

The chatui surfaces config diagnostics without leaking values:

- `/extensions status` — appends `⚠ missing required config: …` and `⚠ provider <pid> missing required config: …` lines for any loaded extension whose declared or provider-required keys aren't satisfied.
- `/extensions config` — lists every loaded extension's manifest config: each entry shows `key [required] — source: <label>, has_value: <bool>` plus its description if set. Source labels are `env override (NAME)`, `secret env (NAME)`, `config key (extension.<id>.<key>)`, `default`, or `missing`.
- `/extensions config <id>` — same as above for a single extension.

Provider-required keys that have no matching manifest entry are reported as `⚠ provider <pid> requires config '<key>' (no manifest entry)` so authors can correct the manifest.

### Redaction rules

Synaps **never displays the resolved value** of any config entry through `/extensions` UX, regardless of whether the source is plain config, default, secret env, or env override. The diagnostics surface only references the source identifier (env var name, config key) — not the value. The internal helper `extensions::config::redact_secret_value` is reserved for future log/error contexts where a partial value must appear; it never returns the full input.

### Authoring guidance

- Mark API keys, tokens, and any other credential as `secret_env` so authors can surface a clear env-var hint without committing values.
- Use `required: true` only for entries the extension cannot start without; defaults are preferred wherever sensible.
- Provider `config_schema.required` should reference keys that are also declared in the manifest's `config` array; otherwise diagnostics will warn about a missing manifest entry.

## Trust controls and audit log

Provider routing is gated by a per-provider trust toggle. Trust state lives at
`$SYNAPS_BASE_DIR/extensions/trust.json` and is **enabled-by-default** —
absence of an entry means trusted. Disabling a provider blocks routing
**before** any IPC starts; there is no fallback to built-in providers.

### chatui commands

- `/extensions trust` or `/extensions trust list` — show every registered provider with its enabled/disabled state and reason.
- `/extensions trust disable <runtime_id> [reason]` — record a disable decision.
- `/extensions trust enable <runtime_id>` — re-enable a previously disabled provider.

### Audit log

Every routing attempt appends one JSON line to
`$SYNAPS_BASE_DIR/extensions/audit.jsonl` with:

- `timestamp` (RFC3339 UTC)
- `plugin_id`, `provider_id`, `model_id`
- `tools_exposed` (bool) — whether tool schemas were sent to the provider
- `tools_requested` (u32) — number of provider-requested tool calls
- `streamed` (bool) — whether the call used `provider.stream`
- `outcome` — `ok` | `blocked` | `error`
- `error_class` (optional) — opaque label like `trust_disabled`, `provider_error`, `canceled`

Audit entries never contain prompts, tool inputs, tool outputs, or
config values. Inspect with `/extensions audit [N]` (last N entries).

### Tool-use warning

Providers that declare `tool_use: true` on any model log a warning at load
time so authors and users can review them. Disable them with `/extensions
trust disable` if untrusted.

### Security review checklist for `providers.register`

- Is the plugin source trusted (audited code, signed checksum)?
- Does the provider declare network destinations (currently informational)?
- Does it declare tool-use? If yes, verify which tools it can request through Synaps mediation.
- Does it require config keys with `secret_env`? Confirm secrets are exported via env, not committed.
- Run `/extensions config <id>` to confirm config sources before invoking.
- Use `/extensions audit` to inspect routing history.

## Limits, security boundary, and current limitations

- Synaps enforces a maximum provider tool-loop iteration count to prevent infinite tool recursion. The current routing default is 8 provider turns.
- Tool output is truncated before it is returned to the provider. The current routing default is 30,000 bytes per tool result.
- Providers never execute tools directly. They can only request tools; Synaps mediates execution.
- Extensions must declare `providers.register` before provider metadata is accepted.
- Existing tool permissions and hook interception remain core-owned; providers do not bypass `before_tool_call` / `after_tool_call` hooks.
- Current Slice P routing uses a minimal tool execution context for provider-requested tools. Tool execution is mediated and works for stateless tools, but provider tool loops do not yet receive chat-session streaming deltas/events, secret prompts, subagent registry, shell session manager, event queue, or dynamic tool-registration channel. A follow-up should thread the active chat `ToolContext` factory and runtime-configured limits into provider routing.
- Extension authors should treat tool results as untrusted model context and avoid embedding secrets in provider logs.
