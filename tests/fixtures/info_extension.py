#!/usr/bin/env python3
"""Info protocol fixture."""
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
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii") + body)
    sys.stdout.buffer.flush()


def send_response(request, result=None, error=None):
    payload = {"jsonrpc": "2.0", "id": request.get("id")}
    if error is None:
        payload["result"] = result
    else:
        payload["error"] = error
    write_message(payload)


def info_payload():
    return {
        "build": {
            "backend": "cpu",
            "features": ["local-stt"],
            "version": "0.1.0-test",
        },
        "capabilities": [
            {"kind": "voice", "name": "Local Whisper STT", "modes": ["stt"]},
            {"kind": "models", "name": "Whisper models"},
        ],
        "models": [
            {"id": "ggml-tiny.en.bin", "display_name": "Tiny English", "installed": True},
            {"id": "ggml-base.en.bin", "display_name": "Base English", "installed": False},
        ],
    }


def main():
    while True:
        msg = read_message()
        if msg is None:
            break
        method = msg.get("method")
        if method == "initialize":
            send_response(msg, {"protocol_version": 1, "capabilities": {"voice": {"name": "Local Whisper STT", "modes": ["stt"]}}})
        elif method == "info.get":
            if os.environ.get("INFO_FIXTURE_DISABLE", "0") == "1":
                send_response(msg, error={"code": -32601, "message": "method not found"})
            else:
                send_response(msg, info_payload())
        elif method == "shutdown":
            send_response(msg, None)
            break
        else:
            send_response(msg, error={"code": -32601, "message": "unknown method"})


if __name__ == "__main__":
    main()
