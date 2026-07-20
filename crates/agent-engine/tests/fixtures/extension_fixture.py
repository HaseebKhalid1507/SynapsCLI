#!/usr/bin/env python3
"""Checked-in extension JSON-RPC fixture for Task 20 lease tests.

Speaks the real extension protocol: Content-Length framed JSON-RPC 2.0
over stdio, proper JSON parsing (json.loads/json.dumps) — no field-order
assumptions, no sockets, no network. The extension host clears the child
environment, so ALL behavior is driven by argv:

  argv[1]  spy log path (append-only event log: spawn / request:<method>
           / call:<tool> / hook:<kind> / provider:<model> / sidecar /
           shutdown / eof)
  argv[2]  path to a JSON array of tools to register at initialize
           (objects with name/description/input_schema)
  argv[3]  mode: ok (default) | hostile-error (tool.call returns a
           JSON-RPC error carrying marker content that must be withheld) |
           huge-stderr (floods stderr before serving normally)
  argv[4]  optional path to a JSON array of providers to register at
           initialize (RegisteredProviderSpec-shaped objects)
"""
import json
import sys

SPY = sys.argv[1]
TOOLS_PATH = sys.argv[2] if len(sys.argv) > 2 else None
MODE = sys.argv[3] if len(sys.argv) > 3 else "ok"
PROVIDERS_PATH = sys.argv[4] if len(sys.argv) > 4 else None


def log(event):
    with open(SPY, "a", encoding="utf-8") as f:
        f.write(event + "\n")


def read_request():
    content_length = None
    while True:
        line = sys.stdin.buffer.readline()
        if line == b"":
            return None
        if line in (b"\r\n", b"\n"):
            break
        name, _, value = line.decode("ascii", "replace").partition(":")
        if name.lower() == "content-length":
            content_length = int(value.strip())
    if content_length is None:
        raise RuntimeError("missing content-length")
    return json.loads(sys.stdin.buffer.read(content_length))


def write_frame(payload):
    body = json.dumps(payload).encode("utf-8")
    sys.stdout.buffer.write(
        b"Content-Length: " + str(len(body)).encode("ascii") + b"\r\n\r\n" + body
    )
    sys.stdout.buffer.flush()


def respond(request, result):
    write_frame({"jsonrpc": "2.0", "id": request["id"], "result": result})


def respond_error(request, code, message):
    write_frame(
        {"jsonrpc": "2.0", "id": request["id"],
         "error": {"code": code, "message": message}}
    )


log("spawn")
tools = []
if TOOLS_PATH:
    with open(TOOLS_PATH, encoding="utf-8") as f:
        tools = json.load(f)
providers = []
if PROVIDERS_PATH:
    with open(PROVIDERS_PATH, encoding="utf-8") as f:
        providers = json.load(f)

if MODE == "huge-stderr":
    # One enormous newline-free blob: proves the host's bounded stderr
    # forwarding, then serve normally.
    sys.stderr.write("S" * (1024 * 1024))
    sys.stderr.flush()

while True:
    request = read_request()
    if request is None:
        log("eof")
        sys.exit(0)
    method = request.get("method", "")
    if method == "tool.call":
        log("call:" + str(request.get("params", {}).get("name")))
    elif method == "hook.handle":
        log("hook:" + str(request.get("params", {}).get("kind")))
    elif method == "provider.complete":
        log("provider:" + str(request.get("params", {}).get("model_id")))
    else:
        log("request:" + method)
    if method == "initialize":
        # Persist the initialize params next to the spy log so tests can
        # assert on the host-resolved config (e.g. host_context values)
        # without changing the spy event stream existing tests assert on.
        try:
            with open(SPY + ".init.json", "w", encoding="utf-8") as f:
                json.dump(request.get("params", {}), f)
        except OSError:
            pass
        respond(request, {
            "protocol_version": 1,
            "capabilities": {"tools": tools, "providers": providers,
                             "capabilities": []},
        })
    elif method == "hook.handle":
        respond(request, {"action": "continue"})
    elif method == "provider.complete":
        respond(request, {
            "content": [{"type": "text", "text": "provider-reply"}],
            "stop_reason": "end_turn",
        })
    elif method == "sidecar.spawn_args":
        log("sidecar")
        respond(request, {"args": ["--fixture-sidecar"]})
    elif method == "tool.call":
        if MODE == "hostile-error":
            respond_error(request, -32000,
                          "HOSTILE_EXTENSION_MARKER " + ("s3cr3t" * 64))
        else:
            name = request.get("params", {}).get("name")
            respond(request, {"content": "called:" + str(name)})
    elif method == "shutdown":
        log("shutdown")
        respond(request, {"ok": True})
        sys.exit(0)
    else:
        respond(request, {})
