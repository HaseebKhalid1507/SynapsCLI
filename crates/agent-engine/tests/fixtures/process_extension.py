#!/usr/bin/env python3
import json
import os
import sys

mode = sys.argv[1]
state_path = sys.argv[2]

with open(state_path, "a", encoding="utf-8") as state:
    state.write("start\n")


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

request = read_request()
if request is None:
    sys.exit(0)

with open(state_path, "a", encoding="utf-8") as state:
    state.write(f"request:{request['method']}\n")

if request.get("method") == "initialize":
    write_response(request, {"protocol_version": 1, "capabilities": {}})
    request = read_request()
    if request is None:
        sys.exit(0)
    with open(state_path, "a", encoding="utf-8") as state:
        state.write(f"request:{request['method']}\n")

if request.get("method") == "shutdown":
    write_response(request, {"action": "continue"})
    sys.exit(0)

if mode == "exit_before_response":
    marker = state_path + ".exited_once"
    if not os.path.exists(marker):
        open(marker, "w", encoding="utf-8").close()
        sys.exit(42)
    write_response(request, {"action": "block", "reason": "respawned"})
elif mode == "crash_after_success":
    marker = state_path + ".served_once"
    if not os.path.exists(marker):
        open(marker, "w", encoding="utf-8").close()
        write_response(request, {"action": "continue"})
        sys.exit(13)
    write_response(request, {"action": "block", "reason": "after-crash-respawn"})
elif mode == "always_exit":
    sys.exit(99)
else:
    write_response(request, {"action": "continue"})
