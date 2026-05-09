#!/usr/bin/env python3
"""Test fixture: a provider extension that streams slowly (sleeps between deltas).

Emits four `provider.stream.event` notifications with 0.2 s pauses between each
so that an `Abort` sent ~150 ms after the `Prompt` lands deterministically
arrives during the second sleep.

LSP framing: Content-Length: <n>\r\n\r\n<body>

The framing format and method names (`initialize`, `provider.complete`,
`provider.stream`, `shutdown`) are defined by the SynapsCLI extension protocol
in `src/extensions/process/` — keep this fixture in sync with that source of
truth if the extension protocol ever changes.
"""
import json
import sys
import time


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
                        "display_name": "Streaming Echo Provider (slow)",
                        "description": "Slow deterministic streaming test provider",
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
        # Sleep between each notification so abort can land mid-stream.
        time.sleep(0.2)
        write_frame({
            "jsonrpc": "2.0",
            "method": "provider.stream.event",
            "params": {"type": "text", "delta": "hello "}
        })
        time.sleep(0.2)
        write_frame({
            "jsonrpc": "2.0",
            "method": "provider.stream.event",
            "params": {"type": "text", "delta": "world"}
        })
        time.sleep(0.2)
        write_frame({
            "jsonrpc": "2.0",
            "method": "provider.stream.event",
            "params": {"type": "usage", "input_tokens": 4, "output_tokens": 2}
        })
        time.sleep(0.2)
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
        write_frame({
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {"code": -32601, "message": "unknown method"}
        })
