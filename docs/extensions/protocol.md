# Extension Protocol Specification

This document is the authoritative technical reference for authors building extensions for SynapsCLI.

For a user-facing guide on installing and configuring extensions, see [README.md](./README.md).

---

## Transport

Extensions communicate with the SynapsCLI runtime over **stdio**. The runtime spawns your extension as a subprocess using the `extension.command` and `extension.args` fields declared in `.synaps-plugin/plugin.json`. Your process's:

- **stdin** — receives messages from the runtime
- **stdout** — used to send responses back to the runtime
- **stderr** — captured by SynapsCLI and emitted to debug tracing with the extension id

Set `SYNAPS_EXTENSIONS_TRACE=1` (also accepts `true`, `yes`, or `on`) to emit one structured trace log for each extension hook call. Trace records include hook kind, extension id, action, duration, timeout state, health, and restart count. Trace mode intentionally does **not** log full hook params, tool inputs, or tool outputs by default.

The process is started with the plugin root as its current working directory. Relative file access from your extension should therefore be written relative to the plugin directory.

The protocol is **JSON-RPC 2.0** over a length-prefixed binary framing, identical in structure to the Language Server Protocol (LSP) transport. This is a deliberate choice — tooling that works with LSP servers works here too.

---

## Message Format

Every message (in both directions) is a frame consisting of a header block and a body.

```
Content-Length: <byte-length-of-body>\r\n
\r\n
<body>
```

- The header contains exactly one field: `Content-Length`, whose value is the byte length of the JSON body in UTF-8.
- The header is terminated by a blank line (`\r\n\r\n` in total — one `\r\n` after the last header field, one `\r\n` as the blank line separator).
- The body is a valid JSON-RPC 2.0 object.
- There is no trailing newline or delimiter after the body.

**Example frame (runtime → extension):**

```
Content-Length: 187\r\n
\r\n
{"jsonrpc":"2.0","id":"evt-001","method":"hook.handle","params":{"hook":"before_tool_call","tool":"bash","session_id":"sess-abc","input":{"command":"ls -la"},"timestamp":"2024-11-14T10:23:01Z"}}
```

**Example frame (extension → runtime):**

```
Content-Length: 22\r\n
\r\n
{"jsonrpc":"2.0","id":"evt-001","result":{"action":"continue"}}
```

Both directions use the same framing. Your extension must:

1. Read the `Content-Length` header line
2. Read and discard the blank line
3. Read exactly `Content-Length` bytes from stdin as the body
4. Parse the body as JSON
5. Write responses using the same framing on stdout

**Do not use `print()` with automatic newlines as your sole stdout mechanism** — the runtime reads by byte count, not by line. Use raw writes.

---

## Methods

The runtime calls methods on your extension. Your extension does not initiate calls — it only responds.

### `initialize`

Sent once immediately after the process starts, before any hooks are delivered. Extensions must respond with the protocol version they support.

**Request:**

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "initialize",
  "params": {
    "synaps_version": "0.1.0",
    "extension_protocol_version": 1,
    "plugin_id": "my-plugin",
    "plugin_root": "/path/to/my-plugin",
    "config": {}
  }
}
```

**Response:**

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "protocol_version": 1,
    "capabilities": {}
  }
}
```

If the response protocol version is unsupported, Synaps refuses to load the extension and reports the load failure.

Extensions that request `tools.register` may declare extension-provided tools in
`capabilities.tools`:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "protocol_version": 1,
    "capabilities": {
      "tools": [
        {
          "name": "echo",
          "description": "Echo text back to the model",
          "input_schema": {
            "type": "object",
            "properties": {
              "text": { "type": "string" }
            },
            "required": ["text"]
          }
        }
      ]
    }
  }
}
```

Registered tool runtime names are namespaced as `plugin-id:tool-name` to avoid
collisions. API-facing tool names are sanitized by the normal tool registry, for
example `policy-bundle:echo` becomes `policy-bundle_echo`.

---

### `tool.call`

Called when the model invokes an extension-provided tool declared during
`initialize`.

**Request:**

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "tool.call",
  "params": {
    "name": "echo",
    "input": { "text": "hello" }
  }
}
```

**Response:**

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": { "content": "echo: hello" }
}
```

If `result` is a string, Synaps uses it as the tool output. If `result.content`
is a string, Synaps uses that. Otherwise Synaps serializes the JSON result as the
tool output. JSON-RPC errors are surfaced as normal tool execution failures.

---

### `hook.handle`

Called when a registered hook fires.

**Request:**

```json
{
  "jsonrpc": "2.0",
  "id": "<event-id>",
  "method": "hook.handle",
  "params": <HookEvent>
}
```

**Response:**

```json
{
  "jsonrpc": "2.0",
  "id": "<event-id>",
  "result": <HookResult>
}
```

You must respond with the same `id` that was sent in the request.

---

### `shutdown`

Sent when the session is ending. Your extension should flush state and exit cleanly. No response is required.

```json
{
  "jsonrpc": "2.0",
  "method": "shutdown"
}
```

After receiving `shutdown`, your process should exit within 2 seconds. The runtime will `SIGKILL` it after a grace period.

---

## Hook Matchers

Manifest hook subscriptions may include simple matcher conditions:

```json
{
  "hook": "before_tool_call",
  "tool": "bash",
  "match": {
    "input_contains": "rm -rf"
  }
}
```

Supported matcher keys are listed in `docs/extensions/contract.json` under `matchers`. Phase G supports `input_contains` and `input_equals` against `tool_input`. All specified matcher keys must match for the handler to be invoked.

---

## HookEvent Schema

The `params` field of a `hook.handle` request is a `HookEvent` object.

```json
{
  "hook": "before_tool_call",
  "tool": "bash",
  "session_id": "sess-abc123",
  "input": {
    "command": "rm -rf /tmp/scratch"
  },
  "output": null,
  "message": null,
  "role": null,
  "timestamp": "2024-11-14T10:23:01Z",
  "metadata": {
    "model": "claude-opus-4-5",
    "turn": 3
  }
}
```

| Field        | Type               | Present on hooks                              | Description                                               |
|--------------|--------------------|-----------------------------------------------|-----------------------------------------------------------|
| `hook`       | string             | all                                           | The hook name that fired                                  |
| `tool`       | string \| null     | `before_tool_call`, `after_tool_call`         | Name of the tool being called                             |
| `session_id` | string             | all                                           | Stable identifier for the current session                 |
| `input`      | object \| null     | `before_tool_call`                            | The raw input arguments passed to the tool                |
| `output`     | object \| null     | `after_tool_call`                             | The result returned by the tool                           |
| `message`    | string \| null     | `before_message`                              | The message content (null without `privacy.llm_content`)  |
| `role`       | string \| null     | `before_message`                              | `"user"` or `"assistant"`                                 |
| `timestamp`  | string (ISO 8601)  | all                                           | When the event was generated, in UTC                      |
| `metadata`   | object             | all                                           | Runtime context: active model, turn count, etc.           |

Fields that are not applicable to the current hook are always `null`, never omitted. You can safely access any field without a key-existence check.

---

## HookResult Variants

Your response's `result` field must be one of these variants, identified by the `action` field.

### `continue`

Allow the event to proceed normally. Use this when your extension has no objection.

```json
{ "action": "continue" }
```

### `block`

Prevent the event from proceeding. Only valid on hooks marked **Can block?** in the hook table. On observation-only hooks, this is silently treated as `continue`.

```json
{
  "action": "block",
  "reason": "Command contains a destructive pattern (rm -rf on non-/tmp path)."
}
```

| Field    | Type   | Required | Description                                           |
|----------|--------|----------|-------------------------------------------------------|
| `reason` | string | yes      | Human-readable explanation surfaced to the user/logs  |

When a tool call is blocked, the LLM receives a synthetic tool result indicating the tool was not executed, along with the reason.

### `confirm`

Ask SynapsCLI to get explicit user confirmation before proceeding. Only valid on `before_tool_call`; on other hooks, `confirm` is treated as `continue` and logged as an unsupported action. Interactive TUI streams prompt the user; headless/non-interactive call sites fail closed by blocking the tool call.

```json
{
  "action": "confirm",
  "message": "Run `deploy-prod` now?"
}
```

| Field     | Type   | Required | Description                                      |
|-----------|--------|----------|--------------------------------------------------|
| `message` | string | yes      | Human-readable confirmation prompt for the user  |

### `modify`

Replace the tool input before execution. Only valid on `before_tool_call`; on other hooks, `modify` is treated as `continue` and logged as an unsupported action. The first `modify`, `confirm`, or `block` result stops the handler chain. Trace logs record `action=modify` but do not log the replacement input.

```json
{
  "action": "modify",
  "input": { "command": "echo safe" }
}
```

| Field   | Type   | Required | Description                       |
|---------|--------|----------|-----------------------------------|
| `input` | object | yes      | Replacement tool input JSON value |

### `inject`

Prepend content to the system prompt for the current request. Only valid on hooks marked **Can inject?**. On hooks that don't support injection, `inject` is treated as `continue` and the content is discarded.

```json
{
  "action": "inject",
  "content": "Policy: Never execute commands that modify files outside /tmp without explicit confirmation."
}
```

| Field     | Type   | Required | Description                                                    |
|-----------|--------|----------|----------------------------------------------------------------|
| `content` | string | yes      | Markdown-formatted text to prepend to the system prompt        |

---

## Complete Example: Minimal Python Extension

This extension registers on `before_tool_call` for the `bash` tool and blocks any command containing `rm -rf` outside of `/tmp`.

```python
import sys
import json

def read_message():
    """Read one JSON-RPC frame from stdin."""
    header = b""
    while not header.endswith(b"\r\n\r\n"):
        byte = sys.stdin.buffer.read(1)
        if not byte:
            return None
        header += byte

    content_length = None
    for line in header.split(b"\r\n"):
        if line.lower().startswith(b"content-length:"):
            content_length = int(line.split(b":")[1].strip())
            break

    if content_length is None:
        return None

    body = sys.stdin.buffer.read(content_length)
    return json.loads(body)


def write_message(payload: dict):
    """Write one JSON-RPC frame to stdout."""
    body = json.dumps(payload).encode("utf-8")
    header = f"Content-Length: {len(body)}\r\n\r\n".encode("utf-8")
    sys.stdout.buffer.write(header + body)
    sys.stdout.buffer.flush()


def handle_hook(event: dict) -> dict:
    tool_input = event.get("tool_input") or {}
    command = tool_input.get("command", "")

    if "rm -rf" in command and "/tmp" not in command:
        return {
            "action": "block",
            "reason": f"Blocked destructive command outside /tmp: {command!r}"
        }

    return {"action": "continue"}


def main():
    while True:
        message = read_message()
        if message is None:
            break

        method = message.get("method")

        if method == "initialize":
            write_message({
                "jsonrpc": "2.0",
                "id": message["id"],
                "result": {"protocol_version": 1, "capabilities": {}}
            })

        elif method == "hook.handle":
            result = handle_hook(message["params"])
            write_message({
                "jsonrpc": "2.0",
                "id": message["id"],
                "result": result
            })

        elif method == "shutdown":
            break


if __name__ == "__main__":
    main()
```

**Key points:**

- Reading is byte-exact based on `Content-Length`. Do not use `readline()` alone.
- Writing uses `sys.stdout.buffer` (raw bytes), not `print()`.
- `initialize` is handled before hooks and returns the supported protocol version.
- `shutdown` is handled gracefully — the loop exits and the process terminates naturally.
- No threads, no async — a simple synchronous loop is sufficient for most extensions.

---

## Manifest Reference

Full `plugin.json` with all supported extension fields:

```json
{
  "name": "rm-rf-guard",
  "version": "1.0.0",
  "description": "Blocks destructive shell commands outside of /tmp.",
  "author": "Your Name <you@example.com>",
  "license": "MIT",
  "extension": {
    "protocol_version": 1,
    "runtime": "process",
    "command": "python3",
    "args": ["main.py"],
    "permissions": ["tools.intercept"],
    "hooks": [
      {
        "hook": "before_tool_call",
        "tool": "bash"
      }
    ]
  }
}
```

| Field                         | Type            | Required | Description                                                           |
|-------------------------------|-----------------|----------|-----------------------------------------------------------------------|
| `extension.protocol_version`  | integer         | no       | Protocol version; defaults to `1`, future versions are rejected       |
| `extension.runtime`           | string          | yes      | Runtime type; phase 1 supports `process` only                         |
| `extension.command`           | string          | yes      | Executable or plugin-relative script path to launch                   |
| `extension.args`              | array           | no       | Arguments passed to `command`                                         |
| `extension.hooks`             | array           | yes      | One or more hook registrations                                        |
| `extension.hooks[].hook`      | string          | yes      | Hook name (see Available Hooks table)                                 |
| `extension.hooks[].tool`      | string          | no       | If set, narrows the hook to a specific tool name                      |
| `extension.permissions`       | array of string | no       | Active permissions the extension declares it requires                 |

Relative command paths and local argument paths are resolved from the plugin directory when safe. Bare commands such as `python3` and `node` are resolved through `PATH`.

---

## Permissions Reference

| Permission           | What it unlocks                                                                            |
|----------------------|--------------------------------------------------------------------------------------------|
| `tools.intercept`    | Allows subscription to `before_tool_call` and `after_tool_call`.                           |
| `privacy.llm_content`| Allows subscription to message-content hooks such as `before_message`.                     |
| `session.lifecycle`  | Enables receipt of `on_session_start` and `on_session_end` events.                         |
| `tools.register`    | Register extension-provided tools during initialization.                                    |

Reserved future permissions are rejected if declared today: `providers.register` and `tools.override`.

Permissions are enforced before hook subscriptions are installed. Unknown permission strings are rejected, reserved permission strings are rejected, and a hook subscription without its required permission fails manifest loading.

---

## Error Handling

### Fail-Open Behavior

SynapsCLI is designed to degrade gracefully when extensions misbehave. If your extension:

- **Does not respond within 5 seconds** — the event proceeds as `continue`.
- **Sends a malformed response** — treated as `continue`. The error is logged through SynapsCLI tracing.
- **Crashes or closes stdout** — the runtime restarts the process and retries the request. If retry also fails, hook delivery fails open for that call.
- **Sends an error object** instead of a result — treated as `continue`. The error message is logged.

After three restart attempts, the extension health becomes `Failed`; subsequent hook calls continue fail-open.

### Timeouts

| Scenario               | Timeout | Behavior on expiry                       |
|------------------------|---------|------------------------------------------|
| `hook.handle` response | 5s      | Treat as `continue`, log warning         |
| `shutdown` grace period| 500ms request timeout, then kill after a short grace period | Best-effort shutdown, then child kill |

### On Crash

When an extension process exits without receiving `shutdown`:

1. The runtime logs the transport error through tracing
2. The runtime respawns the process and retries the in-flight request
3. If retry fails, the triggering event proceeds as `continue`
4. After three restart attempts, the extension is marked `Failed`
5. Other extensions continue operating normally

Design your extension to be stateless where possible. If you maintain state, write it to disk promptly — do not rely on an orderly `shutdown` call.

### Stderr Logging

Child stderr is captured and forwarded to SynapsCLI debug tracing with the extension id. Use stderr for human/debug diagnostics only; stdout is reserved for framed JSON-RPC responses.

```
[rm-rf-guard] Checked command: "ls -la" — allowed
[rm-rf-guard] Checked command: "rm -rf /home/user/docs" — blocked
```

Do not write protocol data to stderr; stdout is reserved for framed JSON-RPC responses.
