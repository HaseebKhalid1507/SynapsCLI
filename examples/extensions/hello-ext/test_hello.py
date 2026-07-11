#!/usr/bin/env python3
"""
Standalone test harness for hello-ext — drives main.py over stdio the same way
SynapsCLI does, without needing the runtime. Run:  python3 test_hello.py
"""

import json
import subprocess
import sys
from pathlib import Path


def send(proc, method, params, req_id):
    req = {"jsonrpc": "2.0", "method": method, "id": req_id}
    if params is not None:
        req["params"] = params
    body = json.dumps(req).encode("utf-8")
    proc.stdin.write(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii") + body)
    proc.stdin.flush()


def recv(proc):
    length = None
    while True:
        line = proc.stdout.readline()
        if line in (b"\r\n", b"\n"):
            break
        name, _, value = line.decode("ascii").partition(":")
        if name.strip().lower() == "content-length":
            length = int(value.strip())
    return json.loads(proc.stdout.read(length))


def main():
    proc = subprocess.Popen(
        [sys.executable, "main.py"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=sys.stderr,
        cwd=str(Path(__file__).parent),
    )

    # 1. Handshake
    send(proc, "initialize", {"synaps_version": "test", "extension_protocol_version": 1}, 1)
    init = recv(proc)
    assert init["result"]["protocol_version"] == 1, init
    assert init["result"]["capabilities"]["tools"][0]["name"] == "hello", init
    print("✓ initialize: protocol 1, tool 'hello' registered")

    # 2. Tool call
    send(proc, "tool.call", {"name": "hello", "input": {"name": "Ada"}}, 2)
    out = recv(proc)
    assert "Hello, Ada!" in out["result"]["content"], out
    print(f"✓ tool.call hello → {out['result']['content']}")

    # 3. Hook: safe command continues
    send(proc, "hook.handle", {"kind": "before_tool_call", "tool_name": "bash",
                               "tool_input": {"command": "ls -la"}}, 3)
    r = recv(proc)
    assert r["result"]["action"] == "continue", r
    print("✓ hook.handle: 'ls -la' → continue")

    # 4. Hook: destructive command blocks
    send(proc, "hook.handle", {"kind": "before_tool_call", "tool_name": "bash",
                               "tool_input": {"command": "rm -rf /home/me"}}, 4)
    r = recv(proc)
    assert r["result"]["action"] == "block", r
    print(f"✓ hook.handle: 'rm -rf /home/me' → block ({r['result']['reason']})")

    # 5. Shutdown
    send(proc, "shutdown", None, 5)
    proc.wait(timeout=2)
    print("✓ shutdown: process exited cleanly")
    print("\nAll checks passed.")


if __name__ == "__main__":
    main()
