#!/usr/bin/env python3
"""Test fixture: provider extension that emits an unrelated notification mid-stream."""
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
                "capabilities": {
                    "providers": [{
                        "id": "stream-echo",
                        "display_name": "Streaming Unrelated-Method Provider",
                        "description": "Emits an unrelated notification mid-stream",
                        "models": [{
                            "id": "stream-echo-mini",
                            "display_name": "Stream Echo Mini",
                            "capabilities": {"streaming": True, "tool_use": False},
                            "context_window": 4096
                        }]
                    }]
                }
            }
        })
    elif method == "provider.stream":
        write_frame({
            "jsonrpc": "2.0",
            "method": "provider.stream.event",
            "params": {"type": "text", "delta": "first"}
        })
        # Unrelated notification — must be ignored by the router.
        write_frame({
            "jsonrpc": "2.0",
            "method": "unrelated.event",
            "params": {"foo": "bar"}
        })
        write_frame({
            "jsonrpc": "2.0",
            "method": "provider.stream.event",
            "params": {"type": "done"}
        })
        write_frame({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "content": [{"type": "text", "text": "first"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }
        })
    elif method == "shutdown":
        write_frame({"jsonrpc": "2.0", "id": req_id, "result": None})
        break
    else:
        write_frame({"jsonrpc": "2.0", "id": req_id, "error": {"code": -32601, "message": "unknown method"}})
