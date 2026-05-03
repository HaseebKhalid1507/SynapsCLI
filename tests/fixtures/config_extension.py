#!/usr/bin/env python3
"""Config protocol fixture.

On initialize, this fixture optionally calls config.set, then config.get, then
config.subscribe. The initialize response contains the observed config value.
"""
import json
import os
import sys

_next_outbound_id = 2_000


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


def call_synaps(method, params):
    global _next_outbound_id
    rid = _next_outbound_id
    _next_outbound_id += 1
    write_message({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
    while True:
        msg = read_message()
        if msg is None:
            raise RuntimeError(f"transport closed while awaiting response to {method}")
        if msg.get("id") == rid and ("result" in msg or "error" in msg):
            return msg
        sys.stderr.write(f"unexpected frame while awaiting {method}: {msg!r}\n")
        sys.stderr.flush()


def run_initialize(request):
    key = os.environ.get("CONFIG_FIXTURE_KEY", "backend")
    value = os.environ.get("CONFIG_FIXTURE_VALUE", "cpu")
    do_set = os.environ.get("CONFIG_FIXTURE_SET", "1") != "0"

    if do_set:
        set_resp = call_synaps("config.set", {"key": key, "value": value})
        if "error" in set_resp:
            send_response(request, error={"code": -32000, "message": "config.set failed: " + set_resp["error"].get("message", "")})
            return False

    get_resp = call_synaps("config.get", {"key": key})
    if "error" in get_resp:
        send_response(request, error={"code": -32000, "message": "config.get failed: " + get_resp["error"].get("message", "")})
        return False

    sub_resp = call_synaps("config.subscribe", {"keys": [key]})
    if "error" in sub_resp:
        send_response(request, error={"code": -32000, "message": "config.subscribe failed: " + sub_resp["error"].get("message", "")})
        return False

    observed = (get_resp.get("result") or {}).get("value")
    send_response(request, {"protocol_version": 1, "capabilities": {"tools": [], "observed": observed}})
    return True


def main():
    while True:
        msg = read_message()
        if msg is None:
            break
        method = msg.get("method")
        if method == "initialize":
            ok = run_initialize(msg)
            if not ok:
                break
        elif method == "shutdown":
            send_response(msg, None)
            break
        elif method == "hook.handle":
            send_response(msg, {"action": "continue"})
        else:
            send_response(msg, error={"code": -32601, "message": "unknown method"})


if __name__ == "__main__":
    main()
