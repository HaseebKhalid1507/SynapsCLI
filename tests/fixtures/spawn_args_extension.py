#!/usr/bin/env python3
"""sidecar.spawn_args RPC fixture.

Mimics a Phase-7-aware plugin: implements `initialize`, `info.get`, and
`sidecar.spawn_args`. The behavior is gated by env vars so the same
fixture can stand in for several test cases.
"""
import json
import os
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


def write_message(payload):
    body = json.dumps(payload).encode("utf-8")
    sys.stdout.buffer.write(
        f"Content-Length: {len(body)}\r\n\r\n".encode("ascii") + body
    )
    sys.stdout.buffer.flush()


def send_response(request, result=None, error=None):
    payload = {"jsonrpc": "2.0", "id": request.get("id")}
    if error is None:
        payload["result"] = result
    else:
        payload["error"] = error
    write_message(payload)


def main():
    while True:
        msg = read_message()
        if msg is None:
            break
        method = msg.get("method")
        if method == "initialize":
            send_response(msg, {"protocol_version": 1, "capabilities": {}})
        elif method == "info.get":
            send_response(msg, error={"code": -32601, "message": "method not found"})
        elif method == "sidecar.spawn_args":
            mode = os.environ.get("SPAWN_ARGS_MODE", "ok")
            if mode == "missing":
                send_response(msg, error={"code": -32601, "message": "method not found"})
            elif mode == "invalid":
                # Returns garbage so core's serde decoding fails.
                send_response(msg, {"args": "not-a-list"})
            elif mode == "minimal":
                send_response(msg, {})
            elif mode == "language_only":
                send_response(msg, {"language": "fr"})
            else:
                send_response(
                    msg,
                    {
                        "args": [
                            "--model-path",
                            "/plugin/owned/model.bin",
                            "--language",
                            "en",
                        ],
                        "language": "en",
                    },
                )
        elif method == "shutdown":
            send_response(msg, None)
            break
        else:
            send_response(msg, error={"code": -32601, "message": "unknown method"})


if __name__ == "__main__":
    main()
