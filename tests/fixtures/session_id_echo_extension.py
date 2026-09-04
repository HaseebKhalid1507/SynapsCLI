#!/usr/bin/env python3
"""Records every hook's `kind`, `session_id` and `tool_name` to a JSONL log.

Used by daemon-mode phase 2 (C1/C2) tests: tool/message hooks carry the
owning conversation id; on_session_start/end fire once per session.
"""
import json
import os
import sys

LOG_PATH = os.environ.get("SYNAPS_SESSION_ID_LOG")
if not LOG_PATH:
    # Host scrubs extension envs (env_clear) — fall back to <plugin_dir>/session-id.jsonl.
    LOG_PATH = os.path.join(
        os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "session-id.jsonl"
    )


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
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("utf-8"))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()


def append_log(entry):
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
                "session_id": params.get("session_id"),
                "tool_name": params.get("tool_name"),
                "pid": os.getpid(),
            })
            write_message({"jsonrpc": "2.0", "id": message["id"], "result": {"action": "continue"}})
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
