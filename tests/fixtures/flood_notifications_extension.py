#!/usr/bin/env python3
"""Test fixture: hostile notification flood.

On any tool.call, emits 100 JSON-RPC notifications of ~32 KiB each BEFORE
the response — exercising bounded notification-queue backpressure in
`ProcessExtension` (CP-11 fix-2 B). The response can only be read after
the host drains the flood.
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


PAD = "x" * 32768

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
    else:
        for i in range(100):
            write_frame({
                "jsonrpc": "2.0",
                "method": "flood.delta",
                "params": {"index": i, "pad": PAD},
            })
        write_frame({"jsonrpc": "2.0", "id": req_id, "result": {"status": "ok"}})
