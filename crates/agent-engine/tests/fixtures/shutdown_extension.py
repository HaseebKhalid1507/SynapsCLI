#!/usr/bin/env python3
"""Extension fixture for shutdown timing.

argv[1] = mode:
  exit    reply to `shutdown`, then exit at once (what the protocol asks for)
  linger  reply to `shutdown`, then keep running until killed
  quit    exit on `shutdown` without a reply
"""
import json
import sys
import time

mode = sys.argv[1]


def read_request():
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
        raise RuntimeError("missing content-length")
    return json.loads(sys.stdin.buffer.read(content_length))


def write_response(request, result):
    body = json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}).encode("utf-8")
    sys.stdout.buffer.write(b"Content-Length: " + str(len(body)).encode("ascii") + b"\r\n\r\n" + body)
    sys.stdout.buffer.flush()


while True:
    request = read_request()
    if request is None:
        sys.exit(0)
    if "id" not in request:
        continue
    method = request.get("method")
    if method == "initialize":
        write_response(request, {"protocol_version": 1, "capabilities": {}})
    elif method == "shutdown":
        if mode == "quit":
            sys.exit(0)
        write_response(request, {})
        if mode == "exit":
            sys.exit(0)
        while True:
            time.sleep(3600)
    else:
        write_response(request, {"action": "continue"})
