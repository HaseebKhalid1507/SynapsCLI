# Extension hooks

Hooks are compiled into SynapsCLI. Extensions subscribe to them from the
`extension.hooks` array in `.synaps-plugin/plugin.json`.

## Manifest syntax

All tool calls:

```json
{ "hook": "before_tool_call" }
```

Only one tool:

```json
{ "hook": "before_tool_call", "tool": "bash" }
```

The `tool` filter is supported for `before_tool_call` and `after_tool_call`.
It matches either the API-safe tool name or the runtime tool name.

## Hook catalog

| Hook | Required permission | Purpose |
|---|---|---|
| `before_tool_call` | `tools.intercept` | Inspect/block a tool call before execution |
| `after_tool_call` | `tools.intercept` | Observe tool input/output after execution |
| `before_message` | `privacy.llm_content` | Inspect the user message and optionally inject context |
| `on_session_start` | `session.lifecycle` | Observe session creation |
| `on_session_end` | `session.lifecycle` | Observe session shutdown and transcript |

## Result behavior

- `continue` lets SynapsCLI proceed.
- `block` prevents the hooked operation and surfaces the reason.
- `inject` accumulates text from all matching handlers and injects it into the
  model context where supported.

Process extensions cannot mutate event payloads in place. If mutation support is
added later it should use an explicit patch/result object rather than relying on
serialized event mutation.
