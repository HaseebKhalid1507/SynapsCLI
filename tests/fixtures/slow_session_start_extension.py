#!/usr/bin/env python3
"""A slow `on_session_start` hook that injects session context.

Used by the daemon reload tests. The extension environment is cleared by the
host, so the knobs are files in the plugin directory (the parent of this
file's directory):

  init-ms             if present: milliseconds to sleep before answering
                      `initialize` (holds extension discovery open)
  slow-ms             if present: milliseconds to sleep in every
                      `on_session_start` before answering (absent = answer
                      at once)
  session-start.jsonl one line per handled `on_session_start`, written
                      after its answer has been sent:
                      {"session_id": ..., "start": <unix s>, "end": <unix s>}

The answer is `{"action": "inject", "content": SENTINEL}`, like the live
extensions that hand context to a new session. Requests are handled one at a
time, as a single extension process does.
"""
import json
import os
import sys
import time

PLUGIN_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
INIT_PATH = os.path.join(PLUGIN_DIR, "init-ms")
SLOW_PATH = os.path.join(PLUGIN_DIR, "slow-ms")
LOG_PATH = os.path.join(PLUGIN_DIR, "session-start.jsonl")

SENTINEL = "SLOW-START-SENTINEL: session-scoped extension context."


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


def knob_ms(path):
    try:
        with open(path, encoding="utf-8") as handle:
            return int(handle.read().strip() or "0")
    except (OSError, ValueError):
        return 0


def main():
    while True:
        msg = read_message()
        if msg is None:
            break
        method = msg.get("method")
        if method == "initialize":
            delay = knob_ms(INIT_PATH)
            if delay > 0:
                time.sleep(delay / 1000.0)
            write_message({
                "jsonrpc": "2.0",
                "id": msg["id"],
                "result": {"protocol_version": 1, "capabilities": {}},
            })
        elif method == "hook.handle":
            params = msg.get("params", {})
            if params.get("kind") == "on_session_start":
                start = time.time()
                delay = knob_ms(SLOW_PATH)
                if delay > 0:
                    time.sleep(delay / 1000.0)
                end = time.time()
                # Answer first: a logged hook has its answer on the wire.
                write_message({
                    "jsonrpc": "2.0",
                    "id": msg["id"],
                    "result": {"action": "inject", "content": SENTINEL},
                })
                with open(LOG_PATH, "a", encoding="utf-8") as handle:
                    handle.write(json.dumps({
                        "session_id": params.get("session_id"),
                        "start": start,
                        "end": end,
                    }) + "\n")
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
