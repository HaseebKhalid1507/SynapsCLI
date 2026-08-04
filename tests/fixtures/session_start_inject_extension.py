#!/usr/bin/env python3
"""on_session_start injection fixture (task #291, defect 1).

Subscribes to `on_session_start` and answers with

    {"action": "inject", "content": "..."}

which is exactly what the live `axel` extension does. Before the fix, the
runtime allowed only `"continue"` from this hook and discarded the result,
so the injected content never reached the model -- and the hook itself was
emitted at engine boot, before any extension had loaded, so no handler ran
at all.

Records every hook event it sees so the test can assert the hook actually
fired rather than merely that nothing crashed.
"""
import json
import os
import sys

LOG_PATH = os.environ.get("SYNAPS_SESSION_START_LOG") or os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    "session-start.jsonl",
)

INJECTED = "HANDOFF-SENTINEL: prior session ended mid-refactor."


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
        msg = read_message()
        if msg is None:
            break
        method = msg.get("method")
        if method == "initialize":
            write_message({
                "jsonrpc": "2.0",
                "id": msg["id"],
                "result": {"protocol_version": 1, "capabilities": {}},
            })
        elif method == "hook.handle":
            params = msg.get("params", {})
            append_log({"kind": params.get("kind"), "session_id": params.get("session_id")})
            if params.get("kind") == "on_session_start":
                write_message({
                    "jsonrpc": "2.0",
                    "id": msg["id"],
                    "result": {"action": "inject", "content": INJECTED},
                })
            else:
                write_message({
                    "jsonrpc": "2.0",
                    "id": msg["id"],
                    "result": {"action": "continue"},
                })
        elif method == "shutdown":
            write_message({"jsonrpc": "2.0", "id": msg["id"], "result": None})
            break
        else:
            write_message({
                "jsonrpc": "2.0",
                "id": msg.get("id"),
                "error": {"code": -32601, "message": "unknown method"},
            })


if __name__ == "__main__":
    main()
