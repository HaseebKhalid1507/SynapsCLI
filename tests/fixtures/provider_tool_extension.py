#!/usr/bin/env python3
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
    sys.stdout.buffer.write(b"Content-Length: " + str(len(body)).encode("ascii") + b"\r\n\r\n" + body)
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
                        "id": "tools",
                        "display_name": "Tool Provider",
                        "description": "Requests a tool then returns final text",
                        "models": [{
                            "id": "tool-small",
                            "display_name": "Tool Small",
                            "capabilities": {"streaming": False, "tool_use": True},
                            "context_window": 4096
                        }]
                    }]
                }
            }
        })
    elif method == "provider.complete":
        params = req.get("params", {})
        saw_result = False
        result_text = ""
        for msg in params.get("messages", []):
            content = msg.get("content")
            if isinstance(content, list):
                for block in content:
                    if isinstance(block, dict) and block.get("type") == "tool_result":
                        saw_result = True
                        result_text = block.get("content", "")
        if saw_result:
            write_frame({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "content": [{"type": "text", "text": "final:" + result_text}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 2, "output_tokens": 2}
                }
            })
        else:
            write_frame({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "content": [{
                        "type": "tool_use",
                        "id": "tool-call-1",
                        "name": "echo_test",
                        "input": {"message": "from-provider"}
                    }],
                    "stop_reason": "tool_use",
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                }
            })
    elif method == "shutdown":
        write_frame({"jsonrpc": "2.0", "id": req_id, "result": None})
        break
    else:
        write_frame({"jsonrpc": "2.0", "id": req_id, "error": {"code": -32601, "message": "unknown method"}})
