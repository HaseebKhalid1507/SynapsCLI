#!/usr/bin/env python3
"""Test fixture: HOSTILE flooding streaming provider (CP-11 fix-2 B).

`provider.stream` emits `FLOOD_EVENTS` text-delta notifications of
`CHUNK` bytes each as fast as the pipe allows, then the final aggregated
result. Pass argv `forever` to stream without ever responding (for
cancellation coverage).
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


FLOOD_EVENTS = 2000
CHUNK = "x" * 4096
FOREVER = "forever" in sys.argv[1:]

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
                "capabilities": {
                    "providers": [{
                        "id": "flood",
                        "display_name": "Flooding Provider",
                        "description": "Hostile high-volume TextDelta fixture",
                        "models": [{
                            "id": "flood-mini",
                            "display_name": "Flood Mini",
                            "capabilities": {"streaming": True, "tool_use": False},
                            "context_window": 4096
                        }]
                    }]
                }
            }
        })
    elif method == "provider.stream":
        try:
            if FOREVER:
                while True:
                    write_frame({
                        "jsonrpc": "2.0",
                        "method": "provider.stream.event",
                        "params": {"type": "text", "delta": CHUNK},
                    })
            for _ in range(FLOOD_EVENTS):
                write_frame({
                    "jsonrpc": "2.0",
                    "method": "provider.stream.event",
                    "params": {"type": "text", "delta": CHUNK},
                })
        except BrokenPipeError:
            sys.exit(0)
        write_frame({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "content": [{"type": "text", "text": "flood-final"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": FLOOD_EVENTS}
            }
        })
    elif method == "shutdown":
        write_frame({"jsonrpc": "2.0", "id": req_id, "result": None})
        break
    else:
        write_frame({
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {"code": -32601, "message": "unknown method"},
        })
