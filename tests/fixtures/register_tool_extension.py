#!/usr/bin/env python3
import json
import os
import signal
import sys


MODE = os.environ.get("SYNAPS_REGISTER_TOOL_MODE", "valid")
PID_FILE = os.environ.get("SYNAPS_REGISTER_TOOL_PID_FILE")
if PID_FILE:
    with open(PID_FILE, "w", encoding="utf-8") as f:
        f.write(str(os.getpid()))


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


def tool_specs():
    valid = {
        "name": "echo",
        "description": "Echo text",
        "input_schema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    }
    if MODE == "empty_name":
        return [{**valid, "name": ""}]
    if MODE == "empty_description":
        return [{**valid, "description": ""}]
    if MODE == "duplicate_name":
        return [valid, valid]
    if MODE == "non_object_schema":
        return [{**valid, "input_schema": True}]
    return [valid]


while True:
    request = read_message()
    if request is None:
        break
    method = request.get("method")
    if method == "initialize":
        write_message(request, {
            "protocol_version": 1,
            "capabilities": {"tools": tool_specs()},
        })
    elif method == "tool.call":
        params = request.get("params") or {}
        if params.get("name") == "echo":
            write_message(request, {"content": f"echo: {params.get('input', {}).get('text', '')}"})
        else:
            write_message(request, error={"code": -32602, "message": "unknown tool"})
    elif method == "hook.handle":
        write_message(request, {"action": "continue"})
    elif method == "shutdown":
        write_message(request, None)
        break
    else:
        write_message(request, error={"code": -32601, "message": f"unknown method: {method}"})
