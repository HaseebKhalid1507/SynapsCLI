#!/usr/bin/env python3
import json
import os
import sys

LOG_PATH = os.environ.get("SYNAPS_COMPACTION_LOG")


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
    return json.loads(sys.stdin.buffer.read(content_length).decode("utf-8"))


def write_message(payload):
    body = json.dumps(payload).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("utf-8") + body)
    sys.stdout.buffer.flush()


def append_log(entry):
    if LOG_PATH:
        with open(LOG_PATH, "a", encoding="utf-8") as handle:
            handle.write(json.dumps(entry) + "\n")


def main():
    while True:
        message = read_message()
        if message is None:
            break
        method = message.get("method")
        if method == "initialize":
            write_message({
                "jsonrpc": "2.0",
                "id": message["id"],
                "result": {"protocol_version": 1, "capabilities": {}},
            })
        elif method == "hook.handle":
            params = message.get("params", {})
            append_log({
                "kind": params.get("kind"),
                "message": params.get("message"),
                "session_id": params.get("session_id"),
                "data": params.get("data"),
            })
            action = {"action": "block", "reason": "ignored"} if params.get("kind") == "on_compaction" else {"action": "continue"}
            write_message({"jsonrpc": "2.0", "id": message["id"], "result": action})
        elif method == "shutdown":
            break


if __name__ == "__main__":
    main()
