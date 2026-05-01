#!/usr/bin/env python3
"""Test fixture: a provider extension that supports `provider.stream`.

Emits four `provider.stream.event` notifications (two text deltas, a usage
event, and a done marker) before responding with the aggregated result.
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


def last_user_text(params):
    for msg in reversed(params.get("messages", [])):
        if msg.get("role") == "user":
            content = msg.get("content")
            if isinstance(content, str):
                return content
            if isinstance(content, list):
                for block in content:
                    if isinstance(block, dict) and block.get("type") == "text":
                        return block.get("text", "")
            return ""
    return ""


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
                        "display_name": "Streaming Echo Provider",
                        "description": "Deterministic streaming test provider",
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
    elif method == "provider.complete":
        text = last_user_text(req.get("params", {}))
        write_frame({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "content": [{"type": "text", "text": "complete:" + text}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }
        })
    elif method == "provider.stream":
        # Emit notifications in order before responding.
        write_frame({
            "jsonrpc": "2.0",
            "method": "provider.stream.event",
            "params": {"type": "text", "delta": "hello "}
        })
        write_frame({
            "jsonrpc": "2.0",
            "method": "provider.stream.event",
            "params": {"type": "text", "delta": "world"}
        })
        write_frame({
            "jsonrpc": "2.0",
            "method": "provider.stream.event",
            "params": {"type": "usage", "input_tokens": 4, "output_tokens": 2}
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
                "content": [{"type": "text", "text": "hello world"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 4, "output_tokens": 2}
            }
        })
    elif method == "shutdown":
        write_frame({"jsonrpc": "2.0", "id": req_id, "result": None})
        break
    else:
        write_frame({"jsonrpc": "2.0", "id": req_id, "error": {"code": -32601, "message": "unknown method"}})
