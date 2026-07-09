# Build a SynapsCLI Extension

This is a from-zero tutorial. If you read only this page, you can produce a
SynapsCLI extension that loads and runs. It builds one complete, working example
step by step, then shows how to install and test it.

The finished extension is checked in at
[`examples/extensions/hello-ext/`](../../examples/extensions/hello-ext/) — you
can diff your work against it at any point.

Deeper references, when you need them:

- [`protocol.md`](./protocol.md) — the authoritative wire protocol.
- [`hooks.md`](./hooks.md) — every hook, its permission, and its allowed actions.
- [`permissions.md`](./permissions.md) — what each permission unlocks.
- [`contract.json`](./contract.json) — the machine-readable contract (CI-checked).

---

## What an extension is

An extension is a **separate process** that SynapsCLI spawns and talks to over
**stdin/stdout** using **JSON-RPC 2.0** with LSP-style `Content-Length` framing.
The runtime calls methods on you; you only ever respond. Any language that can
read stdin and write stdout works — this tutorial uses Python 3 with no
dependencies.

An extension can do two kinds of things:

- **Register tools** the model can call (needs `tools.register`).
- **Subscribe to hooks** — lifecycle events like "a tool is about to run" — and
  optionally block, modify, confirm, or inject (needs a hook-specific permission).

We will do exactly one of each: a `hello` tool and a `before_tool_call` hook.

> **Stability promise.** The protocol is versioned by a single integer,
> `extension_protocol_version` (currently `1`), defined in
> [`contract.json`](./contract.json). Per
> [`../STABILITY.md`](../STABILITY.md) §1: *for as long as that number stays `1`,
> a correctly-written v1 extension keeps loading across all SynapsCLI minor and
> patch releases — no hooks, permissions, methods, actions, or fields will be
> removed or have their meaning changed within a version.* New optional things
> may be added, so your extension must **tolerate unknown fields**. That is the
> contract this tutorial is written against.

---

## Step 0 — Prerequisites

- Python 3 on your `PATH` (`python3 --version`).
- A place to build. Create a working directory:

```bash
mkdir -p hello-ext/.synaps-plugin
cd hello-ext
```

Your finished layout will be:

```
hello-ext/
  .synaps-plugin/
    plugin.json     # the manifest — how SynapsCLI discovers and loads you
  main.py           # the extension process
```

The `.synaps-plugin/plugin.json` file is what marks a directory as a loadable
plugin. No manifest, no load.

---

## Step 1 — The manifest

The manifest declares metadata plus an `extension` object: the command to spawn,
the permissions you need, and the hooks you subscribe to. Create
`.synaps-plugin/plugin.json`:

```json
{
  "name": "hello-ext",
  "version": "0.1.0",
  "description": "Minimal reference extension: one tool, one hook.",
  "author": "Your Name",
  "license": "MIT",
  "extension": {
    "protocol_version": 1,
    "runtime": "process",
    "command": "python3",
    "args": ["main.py"],
    "permissions": ["tools.register", "tools.intercept"],
    "hooks": [
      { "hook": "before_tool_call", "tool": "bash" }
    ]
  }
}
```

What each `extension` field means:

| Field              | Value here            | Meaning |
|--------------------|-----------------------|---------|
| `protocol_version` | `1`                   | The only supported version today. A different value is rejected. |
| `runtime`          | `"process"`           | The only supported runtime in phase 1. |
| `command`          | `"python3"`           | Executable to launch. Bare names resolve via `PATH`; relative paths resolve from the plugin dir. |
| `args`             | `["main.py"]`         | Passed to `command`. `main.py` is resolved relative to the plugin dir (the process runs with the plugin root as its CWD). |
| `permissions`      | see below             | What the extension is allowed to do. |
| `hooks`            | see below             | Which events you subscribe to. |

**Permissions must match what you do**, or loading fails:

- `tools.register` — lets you register tools in your `initialize` response.
- `tools.intercept` — lets you subscribe to `before_tool_call` / `after_tool_call`.

The `{ "hook": "before_tool_call", "tool": "bash" }` subscription means "call me
before the `bash` tool runs, and only for `bash`." Drop the `"tool"` field to
receive the hook for *every* tool. The `tool` filter is valid only on
`before_tool_call` and `after_tool_call`.

Pick the narrowest permission set that works — SynapsCLI rejects unknown
permission strings and refuses any hook subscription whose required permission is
missing. The full mapping of hook → required permission → allowed actions is in
[`hooks.md`](./hooks.md).

---

## Step 2 — The framing helpers

Every message in both directions is a header block + JSON body:

```
Content-Length: <byte-length-of-body>\r\n
\r\n
<body>
```

You read by **byte count**, not by line, and you write raw bytes to
`sys.stdout.buffer` — never `print()` (its newlines corrupt the stream, and
stdout is reserved for framed responses; use `stderr` for logs). Start `main.py`
with these two helpers:

```python
#!/usr/bin/env python3
import json
import sys


def read_message():
    """Read one Content-Length-framed JSON-RPC message, or None on EOF."""
    content_length = None
    while True:
        line = sys.stdin.buffer.readline()
        if line == b"":
            return None                       # stdin closed — runtime is gone
        if line in (b"\r\n", b"\n"):
            break                             # blank line ends the header block
        name, _, value = line.decode("ascii").partition(":")
        if name.strip().lower() == "content-length":
            content_length = int(value.strip())
    if content_length is None:
        return None
    return json.loads(sys.stdin.buffer.read(content_length))


def write_message(request, result=None, error=None):
    """Write one framed JSON-RPC response, echoing the request's id."""
    payload = {"jsonrpc": "2.0", "id": request.get("id")}
    if error is None:
        payload["result"] = result
    else:
        payload["error"] = error
    body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
    header = f"Content-Length: {len(body)}\r\n\r\n".encode("ascii")
    sys.stdout.buffer.write(header + body)
    sys.stdout.buffer.flush()
```

Two rules that keep you out of trouble:

1. Always echo back the **same `id`** the request sent. The runtime matches
   responses to requests by id.
2. Always `flush()` after writing.

---

## Step 3 — The handshake (`initialize`)

Immediately after spawning you, the runtime sends `initialize` **once**, before
any hooks. You must reply with the protocol version you speak. This is the
version negotiation from [`../STABILITY.md`](../STABILITY.md) §1 — reply with a
version the runtime doesn't support and it refuses to load you (fails closed).

The request looks like:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "initialize",
  "params": {
    "synaps_version": "0.1.0",
    "extension_protocol_version": 1,
    "plugin_id": "hello-ext",
    "plugin_root": "/path/to/hello-ext",
    "config": {}
  }
}
```

Because we declared `tools.register`, this is also where we advertise our tool —
in `result.capabilities.tools`. Add the tool spec and the handshake reply:

```python
HELLO_TOOL = {
    "name": "hello",
    "description": "Return a friendly greeting for the given name.",
    "input_schema": {
        "type": "object",
        "properties": {"name": {"type": "string"}},
        "required": ["name"],
    },
}


def on_initialize(request):
    write_message(request, {
        "protocol_version": 1,
        "capabilities": {"tools": [HELLO_TOOL]},
    })
```

A registered tool needs a non-empty `name`, a non-empty `description`, and an
object `input_schema`. Its runtime name is namespaced as `hello-ext:hello`, and
the model sees the sanitized `hello-ext_hello`. If you weren't registering a
tool, `capabilities` would just be `{}`.

---

## Step 4 — The tool (`tool.call`)

When the model calls your tool, the runtime sends `tool.call` with the tool name
and its input:

```json
{ "jsonrpc": "2.0", "id": 2, "method": "tool.call",
  "params": { "name": "hello", "input": { "name": "Ada" } } }
```

Return the output under `result.content` (a plain string result also works).
Add:

```python
def on_tool_call(request):
    params = request.get("params") or {}
    if params.get("name") == "hello":
        name = (params.get("input") or {}).get("name", "world")
        write_message(request, {"content": f"Hello, {name}! (from hello-ext)"})
    else:
        write_message(request, error={"code": -32602, "message": "unknown tool"})
```

---

## Step 5 — The hook (`hook.handle`)

When a hook you subscribed to fires, the runtime sends `hook.handle` with a
`HookEvent` in `params`. For `before_tool_call` the event carries `kind`,
`tool_name`, and `tool_input`. Fields that don't apply are `null`, never missing,
so you can read them without existence checks.

You respond with a **`HookResult`**, identified by its `action`. For
`before_tool_call` the allowed actions are `continue`, `block`, `confirm`, and
`modify` (see [`hooks.md`](./hooks.md)). We'll use `continue` (allow) and `block`
(reject with a reason):

```python
def on_hook(request):
    params = request.get("params") or {}
    if params.get("kind") != "before_tool_call":
        write_message(request, {"action": "continue"})
        return
    command = (params.get("tool_input") or {}).get("command", "")
    if "rm -rf" in command and "/tmp" not in command:
        write_message(request, {
            "action": "block",
            "reason": f"hello-ext blocked destructive command outside /tmp: {command!r}",
        })
    else:
        write_message(request, {"action": "continue"})
```

When you `block`, the tool does not run and the model gets a synthetic result
carrying your `reason`. Returning an action a hook doesn't support (e.g. `block`
on an observe-only hook) is ignored fail-open and logged — it won't crash
anything.

---

## Step 6 — The dispatch loop and `shutdown`

Tie it together with a loop that reads messages and routes by `method`. Handle
`shutdown` by replying and exiting; also stop when `read_message()` returns
`None` (the runtime closed stdin).

```python
def main():
    sys.stderr.write("[hello-ext] started\n")
    sys.stderr.flush()
    while True:
        request = read_message()
        if request is None:
            break
        method = request.get("method")
        if method == "initialize":
            on_initialize(request)
        elif method == "tool.call":
            on_tool_call(request)
        elif method == "hook.handle":
            on_hook(request)
        elif method == "shutdown":
            write_message(request, None)
            break
        else:
            write_message(request, error={"code": -32601, "message": f"unknown method: {method}"})


if __name__ == "__main__":
    main()
```

That's the whole extension. The complete, verbatim `main.py` is at
[`examples/extensions/hello-ext/main.py`](../../examples/extensions/hello-ext/main.py).

> **Design for crashes.** SynapsCLI fails open: if you don't respond within 5s,
> send garbage, or die, the event proceeds as `continue` and the runtime may
> restart you (up to three times before marking you failed). Keep state on disk
> if you need it — don't rely on an orderly `shutdown`.

---

## Step 7 — Install and load

SynapsCLI scans two locations on startup and loads any subdirectory containing a
`.synaps-plugin/plugin.json`:

- User plugins: `~/.synaps-cli/plugins/`
- Project-local plugins: `./.synaps/plugins/` (overrides a same-named user plugin)

Install by copying (or symlinking) your directory into one of them:

```bash
cp -r hello-ext ~/.synaps-cli/plugins/hello-ext
```

**Two naming things people trip on:**
- The two roots use *intentionally different* paths — `~/.synaps-cli/plugins/` for
  user-wide plugins, `./.synaps/plugins/` for project-local ones. The `-cli` on the user
  path is **not a typo**.
- The **install directory name is the plugin-id** (`hello-ext` above). SynapsCLI derives
  the id from the directory, not from the manifest `name` field. Keep them matching by
  convention (`install-dir == manifest.name == plugin-id`) so tool namespacing like
  `hello-ext:hello` stays predictable.

Start SynapsCLI normally and it loads on session start. To confirm the extension
is the cause of some behavior, start with everything off:

```bash
synaps --no-extensions
```

To watch what your extension is doing, enable per-hook trace logging:

```bash
SYNAPS_EXTENSIONS_TRACE=1 synaps
```

Trace records include the hook kind, extension id, action, duration, and health
— but never your tool inputs/outputs or hook params.

---

## Step 8 — Test it without the runtime

You don't need SynapsCLI to prove your extension speaks the protocol. Drive it
over stdio from a script: send `initialize`, `tool.call`, `hook.handle`, and
`shutdown`, and check the responses. A ready-made harness — whose source you can
read and adapt — lives beside the example at
`examples/extensions/hello-ext/test_hello.py`:

```bash
cd examples/extensions/hello-ext
python3 test_hello.py
```

Expected output:

```
✓ initialize: protocol 1, tool 'hello' registered
✓ tool.call hello → Hello, Ada! ...
✓ hook.handle: 'ls -la' → continue
✓ hook.handle: 'rm -rf /home/me' → block (...)
✓ shutdown: process exited cleanly
```

If all four checks pass, the extension implements the protocol correctly and will
load in SynapsCLI. (This mirrors how the runtime's own `extensions_e2e`
integration tests spawn an extension and exercise the handshake, tool
registration, and hook dispatch.)

---

## Checklist / common mistakes

- [ ] `.synaps-plugin/plugin.json` exists and has an `extension` object.
- [ ] `initialize` response returns `"protocol_version": 1`. A mismatch → refused.
- [ ] Every permission you use is declared: `tools.register` to register tools,
      `tools.intercept` for `before_tool_call` / `after_tool_call`.
- [ ] You respond with the **same `id`** you received.
- [ ] You write raw framed bytes to `sys.stdout.buffer` and `flush()` — never
      `print()`. Logs go to `stderr`.
- [ ] Read by `Content-Length` byte count, not `readline()` on the body.
- [ ] Return only actions the hook allows (see [`hooks.md`](./hooks.md)); others
      are ignored fail-open.
- [ ] Handle `shutdown` and exit; also exit when `read_message()` returns `None`.

---

## Where to go next

- Rewrite tool output after execution (compression/redaction) with the
  `after_tool_call` → `replace` action — needs `tools.transform_output` in
  addition to `tools.intercept`. See [`hooks.md`](./hooks.md).
- Inject context into the model's system prompt with `before_message` → `inject`
  — needs `privacy.llm_content`. See
  [`examples/extensions/time-ext.py`](../../examples/extensions/time-ext.py) for a
  complete injector.
- Add configuration (endpoints, secrets) via `extension.config` — see the
  "Extension config" section of [`protocol.md`](./protocol.md).
- Read the compatibility guarantees you're building on in
  [`../STABILITY.md`](../STABILITY.md).
