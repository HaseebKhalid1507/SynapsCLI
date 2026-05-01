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

## Limits, security boundary, and current limitations

- Synaps enforces a maximum provider tool-loop iteration count to prevent infinite tool recursion. The current routing default is 8 provider turns.
- Tool output is truncated before it is returned to the provider. The current routing default is 30,000 bytes per tool result.
- Providers never execute tools directly. They can only request tools; Synaps mediates execution.
- Extensions must declare `providers.register` before provider metadata is accepted.
- Existing tool permissions and hook interception remain core-owned; providers do not bypass `before_tool_call` / `after_tool_call` hooks.
- Current Slice P routing uses a minimal tool execution context for provider-requested tools. Tool execution is mediated and works for stateless tools, but provider tool loops do not yet receive chat-session streaming deltas/events, secret prompts, subagent registry, shell session manager, event queue, or dynamic tool-registration channel. A follow-up should thread the active chat `ToolContext` factory and runtime-configured limits into provider routing.
- Extension authors should treat tool results as untrusted model context and avoid embedding secrets in provider logs.
