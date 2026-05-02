#!/usr/bin/env python3
import json
import os
import sys

MODE = os.environ.get("SYNAPS_PROVIDER_MODE", "valid")


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


def provider_specs():
    model = {
        "id": "llama-3-8b",
        "display_name": "Llama 3 8B",
        "capabilities": {
            "streaming": True,
            "tool_use": False,
            "vision": False,
            "reasoning": False,
        },
        "context_window": 8192,
    }
    valid = {
        "id": "local-llama",
        "display_name": "Local Llama",
        "description": "Local model provider",
        "models": [model],
        "config_schema": {"type": "object"},
    }
    if MODE == "empty_id":
        return [{**valid, "id": ""}]
    if MODE == "bad_id":
        return [{**valid, "id": "Local Llama"}]
    if MODE == "empty_display_name":
        return [{**valid, "display_name": ""}]
    if MODE == "empty_description":
        return [{**valid, "description": ""}]
    if MODE == "empty_models":
        return [{**valid, "models": []}]
    if MODE == "empty_model_id":
        return [{**valid, "models": [{**model, "id": ""}]}]
    if MODE == "duplicate_model_id":
        return [{**valid, "models": [model, model]}]
    if MODE == "bad_config_schema":
        return [{**valid, "config_schema": True}]
    return [valid]


while True:
    request = read_message()
    if request is None:
        break
    method = request.get("method")
    if method == "initialize":
        write_message(request, {
            "protocol_version": 1,
            "capabilities": {"providers": provider_specs()},
        })
    elif method == "hook.handle":
        write_message(request, {"action": "continue"})
    elif method == "shutdown":
        write_message(request, None)
        break
    else:
        write_message(request, error={"code": -32601, "message": f"unknown method: {method}"})
