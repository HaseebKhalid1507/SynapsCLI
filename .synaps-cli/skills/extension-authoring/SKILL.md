---
name: extension-authoring
description: Step-by-step guide to building, installing, and testing a SynapsCLI extension — manifest, handshake, tool, hook, install, and test.
---

# Build a SynapsCLI Extension

> **Source of truth:** `{baseDir}/../../../docs/extensions/tutorial.md`
> Read that file for the complete worked example. This skill is a structured
> summary + checklist you can keep in context while authoring.

---

## What you are building

A **separate process** SynapsCLI spawns and talks to over stdin/stdout using
**JSON-RPC 2.0** with `Content-Length` framing. The runtime calls methods on
you; you only respond. Any language works — Python 3 (no deps) is the tutorial
language.

Two capabilities:
- **Tool** — callable by the model (`tools.register` permission required).
- **Hook** — reacts to lifecycle events, optionally blocks/modifies them
  (`hook.*` permission required per hook type).

---

## Step 1 — Manifest (`plugin.json`)

Create `.synaps-plugin/plugin.json` in your extension root:

```json
{
  "name": "hello-ext",
  "version": "0.1.0",
  "description": "Hello-world extension",
  "extension": {
    "entry_point": "python3 main.py",
    "extension_protocol_version": 1,
    "permissions": [
      "tools.register",
      "hooks.before_tool_call"
    ]
  }
}
```

Key fields:
- `entry_point` — shell command to start your process (resolved from plugin root).
- `extension_protocol_version` — must be `1`.
- `permissions` — declare every capability you use; undeclared calls are rejected.

---

## Step 2 — Framing helpers

Every message is a `Content-Length: N\r\n\r\n` header followed by N bytes of
JSON. Write helpers once:

```python
import sys, json

def send(obj):
    body = json.dumps(obj).encode()
    sys.stdout.buffer.write(b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body)
    sys.stdout.buffer.flush()

def recv():
    header = b""
    while not header.endswith(b"\r\n\r\n"):
        header += sys.stdin.buffer.read(1)
    n = int(header.split(b"Content-Length:")[1].strip().split()[0])
    return json.loads(sys.stdin.buffer.read(n))
```

---

## Step 3 — Handshake (`initialize`)

The runtime sends `initialize` first. You must respond with your capabilities:

```python
def handle_initialize(req):
    send({
        "jsonrpc": "2.0", "id": req["id"],
        "result": {
            "name": "hello-ext",
            "version": "0.1.0",
            "protocol_version": 1,
            "capabilities": {
                "tools": [
                    {
                        "name": "hello",
                        "description": "Greet someone",
                        "input_schema": {
                            "type": "object",
                            "properties": {"name": {"type": "string"}},
                            "required": ["name"]
                        }
                    }
                ],
                "hooks": [{"name": "before_tool_call"}]
            }
        }
    })
```

Only declare tools/hooks whose permissions you listed in `plugin.json`.

---

## Step 4 — Tool handler (`tool.call`)

```python
def handle_tool_call(req):
    name_arg = req["params"]["input"].get("name", "world")
    send({
        "jsonrpc": "2.0", "id": req["id"],
        "result": {"output": f"Hello, {name_arg}!"}
    })
```

`params.name` — the tool name.
`params.input` — the JSON object matching your `input_schema`.
Return `result.output` (string shown to the model).

---

## Step 5 — Hook handler (`hook.handle`)

```python
def handle_hook(req):
    # Inspect req["params"]["hook"] and req["params"]["context"]
    # To allow: return result: {}
    # To block: return result: {"action": "block", "reason": "..."}
    send({"jsonrpc": "2.0", "id": req["id"], "result": {}})
```

Available actions (depending on hook type): `allow`, `block`, `modify`,
`confirm`, `inject`. See `docs/extensions/hooks.md` for per-hook details.

---

## Step 6 — Dispatch loop + shutdown

```python
def main():
    while True:
        req = recv()
        method = req.get("method", "")
        if method == "initialize":
            handle_initialize(req)
        elif method == "tool.call":
            handle_tool_call(req)
        elif method == "hook.handle":
            handle_hook(req)
        elif method == "shutdown":
            send({"jsonrpc": "2.0", "id": req["id"], "result": {}})
            break

if __name__ == "__main__":
    main()
```

Tolerate unknown methods — return `result: {}` or ignore them; never crash.

---

## Step 7 — Install and load

```bash
# Option A: copy into the user plugins dir
cp -r hello-ext ~/.synaps-cli/plugins/

# Option B: symlink (dev mode)
ln -s $(pwd)/hello-ext ~/.synaps-cli/plugins/hello-ext
```

Then inside SynapsCLI:
```
/extensions reload
/extensions list        # hello-ext should appear as "loaded"
```

Or start SynapsCLI fresh — plugins in `~/.synaps-cli/plugins/` auto-load.

---

## Step 8 — Test without the runtime

Drive your extension over pipes to verify framing and logic before connecting
to SynapsCLI:

```python
# test_hello_ext.py
import subprocess, json, threading

def framed(obj):
    body = json.dumps(obj).encode()
    return b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body

proc = subprocess.Popen(["python3", "main.py"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE)

proc.stdin.write(framed({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}))
proc.stdin.flush()
# ... read response, assert capabilities ...
```

See `docs/extensions/tutorial.md` § "Step 8 — Test it without the runtime" for
the full reference test.

---

## Checklist / common mistakes

- [ ] `plugin.json` present at `.synaps-plugin/plugin.json`
- [ ] `extension_protocol_version: 1` set
- [ ] Every permission you use is listed in `permissions`
- [ ] `initialize` response includes ALL tools and hooks you will use
- [ ] Framing uses `\r\n\r\n` (not `\n\n`)
- [ ] Flush stdout after every write
- [ ] Unknown methods tolerated (no crash)
- [ ] `shutdown` response sent before exit

---

## Reference docs

All paths relative to the repo root:

| File | Purpose |
|---|---|
| `docs/extensions/tutorial.md` | Full worked example (this skill summarises it) |
| `docs/extensions/protocol.md` | Authoritative wire protocol |
| `docs/extensions/hooks.md` | Every hook, permission, allowed actions |
| `docs/extensions/permissions.md` | What each permission unlocks |
| `docs/extensions/contract.json` | Machine-readable contract (CI-checked) |
| `examples/extensions/hello-ext/` | Finished reference implementation |
