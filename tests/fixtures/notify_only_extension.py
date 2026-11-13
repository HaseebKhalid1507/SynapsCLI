#!/usr/bin/env python3
"""Subscriber for the notify-only hooks (on_context_archived, on_memory_forgotten).

    notify_only_extension.py <mode> <log.jsonl>

Every hook.handle is appended to the log first (its params, plus this
process's pid), then answered according to <mode>:

    record  {"action": "continue"}
    block   {"action": "block"}: not allowed on these hooks, so ignored
    error   a JSON-RPC error
    exit    no answer: the process exits mid-call
    hang    no answer, ever: the process stays alive until it is killed

The host scrubs an extension's environment, so the log path is an argument.
"""
import json
import os
import sys
import time

MODE, LOG_PATH = sys.argv[1], sys.argv[2]


def read_message():
    header = b""
    while not header.endswith(b"\r\n\r\n"):
        chunk = sys.stdin.buffer.read(1)
        if not chunk:
            return None
        header += chunk
    length = None
    for line in header.split(b"\r\n"):
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":", 1)[1].strip())
    if length is None:
        return None
    return json.loads(sys.stdin.buffer.read(length).decode("utf-8"))


def write_message(payload):
    body = json.dumps(payload).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("utf-8") + body)
    sys.stdout.buffer.flush()


def main():
    while True:
        message = read_message()
        if message is None:
            break
        method, request_id = message.get("method"), message.get("id")
        if method == "initialize":
            write_message({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {"protocol_version": 1, "capabilities": {}},
            })
        elif method == "hook.handle":
            with open(LOG_PATH, "a", encoding="utf-8") as log:
                log.write(json.dumps({"pid": os.getpid(), "params": message.get("params")}) + "\n")
            if MODE == "exit":
                os._exit(3)
            if MODE == "hang":
                while True:
                    time.sleep(3600)
            if MODE == "error":
                write_message({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "error": {"code": -32000, "message": "notify fixture: handler failed"},
                })
                continue
            action = {"action": "block", "reason": "ignored"} if MODE == "block" else {"action": "continue"}
            write_message({"jsonrpc": "2.0", "id": request_id, "result": action})
        elif method == "shutdown":
            break
        else:
            write_message({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32601, "message": f"unknown method: {method}"},
            })


if __name__ == "__main__":
    main()
