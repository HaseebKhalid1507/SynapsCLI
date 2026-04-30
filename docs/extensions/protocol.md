# Extension Protocol Specification

This document is the authoritative technical reference for authors building extensions for SynapsCLI.

For a user-facing guide on installing and configuring extensions, see [README.md](./README.md).

---

## Transport

Extensions communicate with the SynapsCLI runtime over **stdio**. The runtime spawns your extension as a subprocess using the `entry` command declared in `plugin.json`. Your process's:

- **stdin** — receives messages from the runtime
- **stdout** — used to send responses back to the runtime
- **stderr** — captured and written to SynapsCLI's debug log (visible with `AXEL_BRAIN=1`)

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
{"jsonrpc":"2.0","id":"evt-001","result":{"type":"continue"}}
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

Your response's `result` field must be one of three variants, identified by the `type` field.

### `continue`

Allow the event to proceed normally. Use this when your extension has no objection.

```json
{ "type": "continue" }
```

### `block`

Prevent the event from proceeding. Only valid on hooks marked **Can block?** in the hook table. On observation-only hooks, this is silently treated as `continue`.

```json
{
  "type": "block",
  "reason": "Command contains a destructive pattern (rm -rf on non-/tmp path)."
}
```

| Field    | Type   | Required | Description                                           |
|----------|--------|----------|-------------------------------------------------------|
| `reason` | string | yes      | Human-readable explanation surfaced to the user/logs  |

When a tool call is blocked, the LLM receives a synthetic tool result indicating the tool was not executed, along with the reason.

### `inject`

Prepend content to the system prompt for the current request. Only valid on hooks marked **Can inject?**. On hooks that don't support injection, `inject` is treated as `continue` and the content is discarded.

```json
{
  "type": "inject",
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
    tool_input = event.get("input") or {}
    command = tool_input.get("command", "")

    if "rm -rf" in command and "/tmp" not in command:
        return {
            "type": "block",
            "reason": f"Blocked destructive command outside /tmp: {command!r}"
        }

    return {"type": "continue"}


def main():
    while True:
        message = read_message()
        if message is None:
            break

        method = message.get("method")

        if method == "hook.handle":
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
    "entry": "python3 main.py",
    "hooks": [
      {
        "hook": "before_tool_call",
        "tool": "bash"
      }
    ],
    "permissions": [
      "tools.intercept"
    ]
  }
}
```

| Field                    | Type            | Required | Description                                                           |
|--------------------------|-----------------|----------|-----------------------------------------------------------------------|
| `extension.entry`        | string          | yes      | Shell command to launch the extension process                         |
| `extension.hooks`        | array           | yes      | One or more hook registrations                                        |
| `extension.hooks[].hook` | string          | yes      | Hook name (see Available Hooks table)                                 |
| `extension.hooks[].tool` | string          | no       | If set, narrows the hook to a specific tool name                      |
| `extension.permissions`  | array of string | no       | Permissions the extension declares it requires (empty = observe-only) |

The `entry` command is executed with the plugin directory as the working directory. Relative paths in `entry` resolve from there.

---

## Permissions Reference

| Permission           | What it unlocks                                                                            |
|----------------------|--------------------------------------------------------------------------------------------|
| `tools.intercept`    | Enables `block` results on `before_tool_call`. Without this, block is silently ignored.    |
| `privacy.llm_content`| Populates `message` and `output` fields. Without this, those fields are always `null`.     |
| `session.lifecycle`  | Enables receipt of `on_session_start` and `on_session_end` events.                         |
| `tools.register`     | Allows the extension to declare new tools the LLM can call (separate registration flow).   |
| `providers.register` | Allows the extension to register a new LLM backend provider.                               |
| `tools.override`     | Allows replacing a built-in tool's implementation with the extension's own handler.        |

Permissions are declared but not enforced cryptographically — they serve as an explicit contract between the extension author and the user installing it. SynapsCLI will display the requested permissions at install time and warn if an extension is attempting to use capabilities it hasn't declared.

---

## Error Handling

### Fail-Open Behavior

SynapsCLI is designed to degrade gracefully when extensions misbehave. If your extension:

- **Does not respond within 5 seconds** — the event proceeds as `continue`. Your extension is not killed; the next event will still be sent.
- **Sends a malformed response** — treated as `continue`. The error is logged to stderr capture.
- **Crashes (exits unexpectedly)** — the runtime marks your extension as failed for this session. All subsequent hook events for your registered hooks are skipped. The session continues normally.
- **Sends an error object** instead of a result — treated as `continue`. The error message is logged.

### Timeouts

| Scenario                      | Timeout | Behavior on expiry                       |
|-------------------------------|---------|------------------------------------------|
| `hook.handle` response        | 5s      | Treat as `continue`, log warning         |
| Extension startup (first msg) | 10s     | Extension marked failed, not retried     |
| `shutdown` grace period       | 2s      | `SIGKILL` sent to extension process      |

### On Crash

When an extension process exits without receiving `shutdown`:

1. The runtime logs the exit code and any buffered stderr output
2. The extension is marked inactive for the session
3. No attempt is made to restart it
4. Other extensions continue operating normally

Design your extension to be stateless where possible. If you maintain state, write it to disk promptly — do not rely on an orderly `shutdown` call.

### Stderr Logging

Anything your extension writes to stderr is captured by the runtime and written to its internal debug log. With `AXEL_BRAIN=1`, this output appears in the terminal prefixed with the extension name:

```
[rm-rf-guard] Checked command: "ls -la" — allowed
[rm-rf-guard] Checked command: "rm -rf /home/user/docs" — blocked
```

Use stderr liberally for debug output. It never reaches the user by default.
