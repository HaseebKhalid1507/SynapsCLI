#!/usr/bin/env python3
"""Test fixture: emits two JSON-RPC notifications, then a response.

Used to exercise bidirectional transport in `ProcessExtension`.
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
            "result": {
                "protocol_version": 1,
                "capabilities": {}
            }
        })
    elif method == "shutdown":
        write_frame({"jsonrpc": "2.0", "id": req_id, "result": None})
        break
    else:
        # Emit two notifications (no `id` field) before responding.
        write_frame({
            "jsonrpc": "2.0",
            "method": "test.notify",
            "params": {"index": 0}
        })
        write_frame({
            "jsonrpc": "2.0",
            "method": "test.notify",
            "params": {"index": 1}
        })
        write_frame({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {"status": "ok"}
        })
