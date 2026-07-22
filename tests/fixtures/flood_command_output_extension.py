#!/usr/bin/env python3
"""Test fixture: hostile command.invoke output flood.

Simulates a malicious or malfunctioning extension whose interactive
command floods `command.output` notifications (matching the caller's
`request_id`) while the `command.invoke` JSON-RPC response remains
pending — the CP-11 fix-3 adversarial scenario.

Commands (dispatched on the `command` field of `command.invoke` params):

- ``flood <count> <pad_bytes>``: emit `count` Text output events of
  exactly `pad_bytes` ASCII bytes each, then a `done` output event, then
  the JSON-RPC response ``{"status": "ok", "emitted": count,
  "payload_bytes": count * pad_bytes}``. Defaults: 640 x 65536 (40 MiB).
- ``flood_forever <pad_bytes>``: emit Text output events in an infinite
  loop and NEVER respond. Used to prove cancellation releases the
  producer chain. Defaults: 65536.
- ``small``: emit one event of each kind (text/system/table/task
  start+update+log+done/error/done) then respond ``{"status": "ok"}`` —
  proves bounded outputs are preserved verbatim.
"""
import json
import sys


def read_frame():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":", 1)[1].strip())
    if length is None:
        return None
    return json.loads(sys.stdin.buffer.read(length).decode("utf-8"))


def write_frame(payload):
    body = json.dumps(payload).encode("utf-8")
    sys.stdout.buffer.write(
        b"Content-Length: " + str(len(body)).encode("ascii") + b"\r\n\r\n" + body
    )
    sys.stdout.buffer.flush()


def output(request_id, event):
    write_frame({
        "jsonrpc": "2.0",
        "method": "command.output",
        "params": {"request_id": request_id, "event": event},
    })


def task(method, params):
    write_frame({"jsonrpc": "2.0", "method": method, "params": params})


while True:
    req = read_frame()
    if req is None:
        break
    method = req.get("method")
    req_id = req.get("id")
    if method == "initialize":
        write_frame({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {"protocol_version": 1, "capabilities": {}},
        })
    elif method == "shutdown":
        write_frame({"jsonrpc": "2.0", "id": req_id, "result": None})
        break
    elif method == "command.invoke":
        params = req.get("params") or {}
        command = params.get("command")
        args = params.get("args") or []
        request_id = params.get("request_id")
        if command == "flood":
            count = int(args[0]) if len(args) > 0 else 640
            pad_bytes = int(args[1]) if len(args) > 1 else 65536
            pad = "x" * pad_bytes
            for _ in range(count):
                output(request_id, {"kind": "text", "content": pad})
            output(request_id, {"kind": "done"})
            write_frame({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "status": "ok",
                    "emitted": count,
                    "payload_bytes": count * pad_bytes,
                },
            })
        elif command == "flood_forever":
            pad_bytes = int(args[0]) if len(args) > 0 else 65536
            pad = "y" * pad_bytes
            try:
                while True:
                    output(request_id, {"kind": "text", "content": pad})
            except (BrokenPipeError, OSError):
                sys.exit(0)
        elif command == "small":
            output(request_id, {"kind": "text", "content": "hello"})
            output(request_id, {"kind": "system", "content": "working"})
            task("task.start", {"id": "t1", "label": "Fetching", "kind": "download"})
            task("task.update", {"id": "t1", "current": 1, "total": 2})
            task("task.log", {"id": "t1", "line": "fetched shard 1"})
            task("task.done", {"id": "t1"})
            output(request_id, {
                "kind": "table",
                "headers": ["name", "value"],
                "rows": [["alpha", "1"], ["beta", "2"]],
            })
            output(request_id, {"kind": "error", "content": "one minor problem"})
            output(request_id, {"kind": "done"})
            write_frame({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {"status": "ok"},
            })
        else:
            write_frame({
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {"code": -32601, "message": "unknown command"},
            })
    else:
        write_frame({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {"status": "ignored"},
        })
