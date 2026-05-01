#!/usr/bin/env python3
import json
import os
import sys

LOG_PATH = os.environ.get("SYNAPS_MESSAGE_COMPLETE_LOG")


def read_message():
    header = b""
    while not header.endswith(b"\r\n\r\n"):
        chunk = sys.stdin.buffer.read(1)
        if not chunk:
            return None
        header += chunk

    content_length = None
    for line in header.split(b"\r\n"):
        if line.lower().startswith(b"content-length:"):
            content_length = int(line.split(b":", 1)[1].strip())
            break

    if content_length is None:
        return None

    body = sys.stdin.buffer.read(content_length)
    return json.loads(body.decode("utf-8"))


def write_message(payload):
    body = json.dumps(payload).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("utf-8"))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()


def append_log(entry):
    if LOG_PATH:
        with open(LOG_PATH, "a", encoding="utf-8") as handle:
            handle.write(json.dumps(entry) + "\n")


def handle_initialize(message):
    write_message({
        "jsonrpc": "2.0",
        "id": message["id"],
        "result": {"protocol_version": 1, "capabilities": {}},
    })


def handle_hook(message):
    params = message.get("params", {})
    append_log({
        "kind": params.get("kind"),
        "message": params.get("message"),
        "data": params.get("data"),
    })
    write_message({"jsonrpc": "2.0", "id": message["id"], "result": {"action": "continue"}})


def main():
    while True:
        message = read_message()
        if message is None:
            break
        method = message.get("method")
        if method == "initialize":
            handle_initialize(message)
        elif method == "hook.handle":
            handle_hook(message)
        elif method == "shutdown":
            break
        else:
            write_message({
                "jsonrpc": "2.0",
                "id": message.get("id"),
                "error": {"code": -32601, "message": f"unknown method: {method}"},
            })


if __name__ == "__main__":
    main()
