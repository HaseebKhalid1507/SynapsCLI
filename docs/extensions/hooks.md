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

The `tool` filter is supported only for `before_tool_call` and
`after_tool_call`. It matches either the API-safe tool name or the runtime tool
name.

## Hook catalog and action matrix

| Hook | Required permission | Tool filter? | Allowed result actions | Purpose |
|---|---|---:|---|---|
| `before_tool_call` | `tools.intercept` | yes | `continue`, `block` | Inspect/block a tool call before execution |
| `after_tool_call` | `tools.intercept` | yes | `continue` | Observe tool input/output after execution |
| `before_message` | `privacy.llm_content` | no | `continue`, `inject` | Inspect the user message and optionally inject context |
| `on_session_start` | `session.lifecycle` | no | `continue` | Observe session creation |
| `on_session_end` | `session.lifecycle` | no | `continue` | Observe session shutdown and transcript |

Unsupported result actions are ignored fail-open and logged as warnings.

## Result behavior

- `continue` lets SynapsCLI proceed.
- `block` prevents the hooked operation and surfaces the reason. It is accepted
  only on `before_tool_call`.
- `inject` accumulates text from all matching handlers and injects it into the
  model context. It is accepted only on `before_message`.

Process extensions cannot mutate event payloads in place. If mutation support is
added later it should use an explicit patch/result object rather than relying on
serialized event mutation.
