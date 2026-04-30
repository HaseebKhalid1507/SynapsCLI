#!/usr/bin/env python3
import json
import sys

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

def write_response(request, result=None, error=None):
    payload = {"jsonrpc": "2.0", "id": request.get("id")}
    if error is None:
        payload["result"] = result
    else:
        payload["error"] = error
    body = json.dumps(payload).encode("utf-8")
    sys.stdout.buffer.write(b"Content-Length: " + str(len(body)).encode("ascii") + b"\r\n\r\n" + body)
    sys.stdout.buffer.flush()

mode = sys.argv[1]
state_path = sys.argv[2]

while True:
    request = read_request()
    if request is None:
        break
    with open(state_path, "a", encoding="utf-8") as state:
        state.write(request.get("method", "") + "\n")
        if request.get("method") == "initialize":
            params = request.get("params") or {}
            state.write("plugin_id=" + str(params.get("plugin_id")) + "\n")
            state.write("protocol=" + str(params.get("extension_protocol_version")) + "\n")
            state.write("root=" + str(params.get("plugin_root")) + "\n")
    if request.get("method") == "initialize":
        if mode == "bad_protocol":
            write_response(request, {"protocol_version": 999, "capabilities": {}})
        elif mode == "error":
            write_response(request, error={"code": -32000, "message": "initialize failed"})
        else:
            write_response(request, {"protocol_version": 1, "capabilities": {"hooks": True}})
    elif request.get("method") == "hook.handle":
        write_response(request, {"action": "continue"})
    elif request.get("method") == "shutdown":
        write_response(request, None)
        break
    else:
        write_response(request, error={"code": -32601, "message": "unknown method"})
