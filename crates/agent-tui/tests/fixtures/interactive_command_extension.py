#!/usr/bin/env python3
import json
import sys


def send(msg):
    body = json.dumps(msg)
    sys.stdout.write(f"Content-Length: {len(body.encode())}\r\n\r\n{body}")
    sys.stdout.flush()


def read_msg():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        text = line.decode().strip()
        if ":" in text:
            k, v = text.split(":", 1)
            headers[k.lower()] = v.strip()
    length = int(headers.get("content-length", "0"))
    if length <= 0:
        return None
    return json.loads(sys.stdin.buffer.read(length).decode())

while True:
    req = read_msg()
    if req is None:
        break
    method = req.get("method")
    rid = req.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":rid,"result":{"protocol_version":1,"capabilities":{"tools":[]}}})
    elif method == "info.get":
        send({"jsonrpc":"2.0","id":rid,"result":{"capabilities":{"commands":["demo"],"tasks":True,"command_output":True}}})
    elif method == "command.invoke":
        params = req.get("params") or {}
        request_id = params.get("request_id") or ""
        frames = [
            {"jsonrpc":"2.0","method":"command.output","params":{"request_id":request_id,"event":{"kind":"text","content":"hello from demo"}}},
            {"jsonrpc":"2.0","method":"task.start","params":{"id":"demo-task","label":"Demo task","kind":"generic"}},
            {"jsonrpc":"2.0","method":"task.update","params":{"id":"demo-task","current":1,"total":2,"message":"halfway"}},
            {"jsonrpc":"2.0","method":"command.output","params":{"request_id":request_id,"event":{"kind":"done"}}},
            {"jsonrpc":"2.0","method":"task.done","params":{"id":"demo-task"}},
        ]
        for frame in frames:
            send(frame)
        send({"jsonrpc":"2.0","id":rid,"result":{"ok":True}})
    elif method == "shutdown":
        send({"jsonrpc":"2.0","id":rid,"result":{}})
        break
    else:
        send({"jsonrpc":"2.0","id":rid,"error":{"code":-32601,"message":"method not found"}})
