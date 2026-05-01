#!/usr/bin/env python3
"""Memory protocol fixture.

On `initialize`, this fixture:
  1) Sends a `memory.append` request to Synaps and awaits the response.
  2) Sends a `memory.query` request and awaits the response.
  3) Verifies the appended record is present in the query result.

If any step fails, the fixture replies to `initialize` with a JSON-RPC
error so the integration test can assert on the message.

Behavior is parameterized via env vars (with sensible defaults):
  MEMORY_FIXTURE_NAMESPACE   - namespace string to use (default: extension id)
  MEMORY_FIXTURE_CONTENT     - content to append (default: "hello memory")
  MEMORY_FIXTURE_TAG         - tag to apply (default: "@test")
"""
import json
import os
import sys

_next_outbound_id = 1_000


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
    """Send a JSON-RPC request from extension → Synaps and await the response.

    While waiting for the matching response id, any other inbound request
    from Synaps is parked (we only expect responses here, so this is a
    pure response-await loop).
    """
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
        # Unexpected: log to stderr and continue.
        sys.stderr.write(f"unexpected frame while awaiting {method}: {msg!r}\n")
        sys.stderr.flush()


def run_initialize(request):
    plugin_id = (request.get("params") or {}).get("plugin_id") or "memory-test-ext"
    namespace = os.environ.get("MEMORY_FIXTURE_NAMESPACE", plugin_id)
    content = os.environ.get("MEMORY_FIXTURE_CONTENT", "hello memory")
    tag = os.environ.get("MEMORY_FIXTURE_TAG", "@test")

    append_resp = call_synaps(
        "memory.append",
        {"namespace": namespace, "content": content, "tags": [tag]},
    )
    if "error" in append_resp:
        send_response(
            request,
            error={
                "code": -32000,
                "message": "memory.append failed: " + append_resp["error"].get("message", ""),
            },
        )
        return False

    query_resp = call_synaps("memory.query", {"namespace": namespace})
    if "error" in query_resp:
        send_response(
            request,
            error={
                "code": -32000,
                "message": "memory.query failed: " + query_resp["error"].get("message", ""),
            },
        )
        return False

    records = (query_resp.get("result") or {}).get("records") or []
    matched = any(
        r.get("content") == content and r.get("namespace") == namespace
        for r in records
    )
    if not matched:
        send_response(
            request,
            error={
                "code": -32000,
                "message": f"appended record not found in query: {records!r}",
            },
        )
        return False

    send_response(request, {"protocol_version": 1, "capabilities": {}})
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
                # After replying with an init error, exit cleanly.
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
