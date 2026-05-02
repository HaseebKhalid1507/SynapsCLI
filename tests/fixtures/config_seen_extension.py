#!/usr/bin/env python3
"""Config resolution fixture.

Records the initialize `config` object to config-seen.json in cwd. This preserves
legacy extension-manager tests that only need to assert resolved manifest config.
"""
import json
import sys


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


while True:
    request = read_message()
    if request is None:
        break
    method = request.get("method")
    if method == "initialize":
        config = (request.get("params") or {}).get("config") or {}
        with open("config-seen.json", "w", encoding="utf-8") as f:
            json.dump(config, f, sort_keys=True)
        write_message(request, {"protocol_version": 1, "capabilities": {}})
    elif method == "hook.handle":
        write_message(request, {"action": "continue"})
    elif method == "shutdown":
        write_message(request, None)
        break
    else:
        write_message(request, error={"code": -32601, "message": "unknown method"})
