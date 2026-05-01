# Extension permissions

Permissions are declared in the plugin manifest under `extension.permissions`.
SynapsCLI validates them before spawning the extension process or installing hook
subscriptions. Unknown permissions fail loudly; reserved permissions are known to
the contract but are rejected until their runtime support exists.

## Active permissions

These are the only permissions currently accepted in `extension.permissions`:

| Permission | Allows |
|---|---|
| `tools.intercept` | Subscribe to `before_tool_call` and `after_tool_call` |
| `privacy.llm_content` | Subscribe to `before_message` and `on_message_complete`; receive message content |
| `session.lifecycle` | Subscribe to `on_session_start` and `on_session_end` |
| `tools.register` | Register extension-provided tools during initialization |
| `providers.register` | Register extension-provided provider metadata during initialization; chat routing is not wired yet |

## Reserved permissions

These names are reserved for future protocol phases and must not be declared by
plugins today:

| Permission | Reserved for |
|---|---|
| `tools.override` | Replacing or wrapping built-in tool implementations |

If a manifest includes a reserved permission, SynapsCLI rejects the extension
with a validation error instead of silently granting future power.

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
