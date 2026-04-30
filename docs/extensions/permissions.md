# Extension permissions

Permissions are declared in the plugin manifest under `extension.permissions`.
They are checked before hook subscriptions are installed. Unknown permission
strings are rejected so typos fail loudly.

## Permission flags

| Permission | Allows |
|---|---|
| `tools.intercept` | Subscribe to `before_tool_call` and `after_tool_call` |
| `tools.override` | Reserved for overriding built-in tools |
| `privacy.llm_content` | Subscribe to LLM/user-message content hooks |
| `session.lifecycle` | Subscribe to `on_session_start` and `on_session_end` |
| `tools.register` | Reserved for registering extension-provided tools |
| `providers.register` | Reserved for registering extension-provided model providers |

## Principle

Grant the narrowest permission set possible. For example, a bash policy
extension usually needs only:

```json
{
  "permissions": ["tools.intercept"],
  "hooks": [{ "hook": "before_tool_call", "tool": "bash" }]
}
```

A timestamp injector for model context needs:

```json
{
  "permissions": ["privacy.llm_content"],
  "hooks": [{ "hook": "before_message" }]
}
```
