#!/usr/bin/env python3
"""
hello-ext — the minimal reference SynapsCLI extension.

It does exactly two things, one of each kind the protocol supports:

  1. Registers ONE tool  — `hello`, callable by the model.
  2. Subscribes to ONE hook — `before_tool_call` on `bash`, which blocks
     `rm -rf` unless the command clearly targets /tmp.

Wire protocol: JSON-RPC 2.0 over stdio with LSP-style Content-Length framing.
The runtime calls us; we only respond. See docs/extensions/protocol.md.
"""

import json
import sys


# ── Framing ───────────────────────────────────────────────────────────
# Reads are byte-exact against Content-Length. Never rely on print()/newlines:
# the runtime reads by byte count, and stdout is reserved for framed responses.

def read_message():
    """Read one Content-Length-framed JSON-RPC message from stdin, or None on EOF."""
    content_length = None
    while True:
        line = sys.stdin.buffer.readline()
        if line == b"":
            return None  # stdin closed — runtime is gone
        if line in (b"\r\n", b"\n"):
            break  # blank line ends the header block
        name, _, value = line.decode("ascii").partition(":")
        if name.strip().lower() == "content-length":
            content_length = int(value.strip())
    if content_length is None:
        return None
    return json.loads(sys.stdin.buffer.read(content_length))


def write_message(request, result=None, error=None):
    """Write one framed JSON-RPC response, echoing the request id."""
    payload = {"jsonrpc": "2.0", "id": request.get("id")}
    if error is None:
        payload["result"] = result
    else:
        payload["error"] = error
    body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
    header = f"Content-Length: {len(body)}\r\n\r\n".encode("ascii")
    sys.stdout.buffer.write(header + body)
    sys.stdout.buffer.flush()


# ── The one tool we register ──────────────────────────────────────────

HELLO_TOOL = {
    "name": "hello",
    "description": "Return a friendly greeting for the given name.",
    "input_schema": {
        "type": "object",
        "properties": {"name": {"type": "string"}},
        "required": ["name"],
    },
}


def call_hello(tool_input):
    name = (tool_input or {}).get("name", "world")
    return {"content": f"Hello, {name}! 👋 (from hello-ext)"}


# ── The one hook we handle ────────────────────────────────────────────

def handle_hook(params):
    """before_tool_call on bash: block destructive `rm -rf` outside /tmp."""
    if params.get("kind") != "before_tool_call":
        return {"action": "continue"}
    command = (params.get("tool_input") or {}).get("command", "")
    if "rm -rf" in command and "/tmp" not in command:
        return {
            "action": "block",
            "reason": f"hello-ext blocked destructive command outside /tmp: {command!r}",
        }
    return {"action": "continue"}


# ── Dispatch loop ─────────────────────────────────────────────────────

def main():
    sys.stderr.write("[hello-ext] started\n")
    sys.stderr.flush()
    while True:
        request = read_message()
        if request is None:
            break
        method = request.get("method")

        if method == "initialize":
            # Handshake: assert the protocol version we speak, and declare our tool.
            write_message(request, {
                "protocol_version": 1,
                "capabilities": {"tools": [HELLO_TOOL]},
            })

        elif method == "tool.call":
            params = request.get("params") or {}
            if params.get("name") == "hello":
                write_message(request, call_hello(params.get("input")))
            else:
                write_message(request, error={"code": -32602, "message": "unknown tool"})

        elif method == "hook.handle":
            write_message(request, handle_hook(request.get("params") or {}))

        elif method == "shutdown":
            write_message(request, None)
            break

        else:
            write_message(request, error={"code": -32601, "message": f"unknown method: {method}"})


if __name__ == "__main__":
    main()
