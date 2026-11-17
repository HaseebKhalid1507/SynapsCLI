#!/usr/bin/env python3
"""A `sandbox_policy` provider for the `synaps sandbox check` contract test.

`mode.json`, one folder above this script, says what it answers:
  {"mode": "ok" | "refuse" | "garbage" | "error" | "hang",
   "write": [...], "read": [...], "reason": "...", "seen": "<file>"}
Every `hook.handle` request's params are written to the `seen` file first, so
the test can read the envelope the host sent.
"""
import json
import os
import sys
import time

here = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(os.path.dirname(here), "mode.json"), encoding="utf-8") as f:
    mode = json.load(f)


def read_request():
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
        raise RuntimeError("missing content-length")
    return json.loads(sys.stdin.buffer.read(content_length))


def send(body):
    data = json.dumps(body).encode("utf-8")
    sys.stdout.buffer.write(b"Content-Length: " + str(len(data)).encode("ascii") + b"\r\n\r\n" + data)
    sys.stdout.buffer.flush()


def respond(request, result):
    send({"jsonrpc": "2.0", "id": request["id"], "result": result})


while True:
    request = read_request()
    if request is None:
        break
    method = request.get("method")
    if method == "initialize":
        respond(request, {"protocol_version": 1, "capabilities": {}})
    elif method == "hook.handle":
        with open(mode["seen"], "w", encoding="utf-8") as seen:
            json.dump(request.get("params"), seen)
        kind = mode["mode"]
        if kind == "ok":
            respond(request, {"write": mode.get("write", []), "read": mode.get("read", [])})
        elif kind == "refuse":
            respond(request, {"refuse": mode.get("reason", "no")})
        elif kind == "garbage":
            respond(request, {"write": ["relative/path"]})
        elif kind == "error":
            send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32000, "message": "boom"}})
        elif kind == "hang":
            time.sleep(120)
    elif method == "shutdown":
        respond(request, {"action": "continue"})
        break
    else:
        respond(request, {"action": "continue"})
