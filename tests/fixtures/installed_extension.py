#!/usr/bin/env python3
import json
import os
import sys

seen_path = os.path.join(os.getcwd(), "hook-seen.json")


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


def write_response(request, result):
    body = json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}).encode("utf-8")
    sys.stdout.buffer.write(b"Content-Length: " + str(len(body)).encode("ascii") + b"\r\n\r\n" + body)
    sys.stdout.buffer.flush()

while True:
    request = read_request()
    if request is None:
        break
    if request.get("method") == "initialize":
        write_response(request, {"protocol_version": 1, "capabilities": {}})
    elif request.get("method") == "hook.handle":
        with open(seen_path, "w", encoding="utf-8") as seen:
            json.dump(request.get("params"), seen)
        write_response(request, {"action": "block", "reason": "installed hook fired"})
    elif request.get("method") == "shutdown":
        write_response(request, {"action": "continue"})
        break
    else:
        write_response(request, {"action": "continue"})
