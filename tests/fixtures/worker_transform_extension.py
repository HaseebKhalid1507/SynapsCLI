#!/usr/bin/env python3
"""Test fixture: an after_tool_call output transform shared by parallel
subagent workers (tests/worker_transform_e2e.rs).

    after_tool_call  ->  {"action": "replace", "output": "[crushed] <tool_output>"}
                         (so each caller can tell its own replacement apart)
    before_tool_call ->  {"action": "block"}  (a worker must never send one)

A `tool_output` of exactly `__crash__` makes the process exit without
answering: a crash in the middle of a worker's hook call.

argv[1] is a spy log: one `spawn` line per process start and one
`hook:<kind>` line per hook.handle, so the test can count (re)spawns and
see which hook kinds arrived.
"""
import json
import sys

SPY = sys.argv[1]


def log(event):
    with open(SPY, "a", encoding="utf-8") as f:
        f.write(event + "\n")


def read_message():
    content_length = None
    while True:
        line = sys.stdin.buffer.readline()
        if line == b"":
            return None
        if line in (b"\r\n", b"\n"):
            break
        name, _, value = line.decode("ascii").partition(":")
        if name.lower() == "content-length":
            content_length = int(value.strip())
    if content_length is None:
        raise RuntimeError("missing Content-Length")
    return json.loads(sys.stdin.buffer.read(content_length))


def write_message(request, result=None, error=None):
    payload = {"jsonrpc": "2.0", "id": request.get("id")}
    if error is None:
        payload["result"] = result
    else:
        payload["error"] = error
    body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii") + body)
    sys.stdout.buffer.flush()


def main():
    log("spawn")
    while True:
        request = read_message()
        if request is None:
            break
        method = request.get("method")
        if method == "initialize":
            write_message(request, {"protocol_version": 1, "capabilities": {}})
        elif method == "hook.handle":
            event = request.get("params") or {}
            kind = event.get("kind")
            log(f"hook:{kind}")
            if kind == "after_tool_call":
                out = event.get("tool_output") or ""
                if out == "__crash__":
                    sys.exit(1)
                write_message(request, {"action": "replace", "output": f"[crushed] {out}"})
            elif kind == "before_tool_call":
                write_message(request, {"action": "block", "reason": "a worker must never ask"})
            else:
                write_message(request, {"action": "continue"})
        elif method == "shutdown":
            write_message(request, {})
            break
        else:
            write_message(request, error={"code": -32601, "message": f"unknown method: {method}"})


if __name__ == "__main__":
    main()
