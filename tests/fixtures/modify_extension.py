#!/usr/bin/env python3
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


def main():
    saw_modified_after = False
    while True:
        request = read_message()
        if request is None:
            break
        method = request.get("method")
        if method == "initialize":
            write_message(request, {"protocol_version": 1, "capabilities": {}})
        elif method == "hook.handle":
            event = request.get("params") or {}
            tool_input = event.get("tool_input") or {}
            command = tool_input.get("command", "")
            if event.get("kind") == "before_tool_call" and "rm -rf" in command:
                write_message(request, {"action": "modify", "input": {"command": "printf modified"}})
            elif event.get("kind") == "after_tool_call" and command == "printf modified":
                saw_modified_after = True
                write_message(request, {"action": "continue"})
            else:
                write_message(request, {"action": "continue"})
        elif method == "shutdown":
            write_message(request, {"saw_modified_after": saw_modified_after})
            break
        else:
            write_message(request, error={"code": -32601, "message": f"unknown method: {method}"})


if __name__ == "__main__":
    main()
