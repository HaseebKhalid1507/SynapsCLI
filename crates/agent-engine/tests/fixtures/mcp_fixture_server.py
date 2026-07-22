#!/usr/bin/env python3
"""Checked-in MCP stdio JSON-RPC fixture for Task 19 lease tests.

Proper JSON parsing per line (json.loads/json.dumps) — no field-order
assumptions, no sockets, no network. Behavior is driven by env:

  MCP_FIXTURE_SPY        append-only event log file (spawn/request/notify/eof)
  MCP_FIXTURE_TOOLS_JSON path to a JSON array of tools to advertise
  MCP_FIXTURE_MODE       ok (default) | huge (oversized init line) |
                         error (hostile provider error message on init)
"""
import json
import os
import sys

SPY = os.environ.get("MCP_FIXTURE_SPY")


def log(event):
    if SPY:
        with open(SPY, "a") as f:
            f.write(event + "\n")


def reply(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


log("spawn")
mode = os.environ.get("MCP_FIXTURE_MODE", "ok")
tools = []
tools_path = os.environ.get("MCP_FIXTURE_TOOLS_JSON")
if tools_path and os.path.exists(tools_path):
    with open(tools_path) as f:
        tools = json.load(f)

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    method = msg.get("method", "")
    rid = msg.get("id")
    if rid is None:
        log("notify:" + method)
        continue
    if method == "tools/call":
        log("request:tools/call:" + str(msg.get("params", {}).get("name")))
    else:
        log("request:" + method)
    if method == "initialize":
        if mode == "huge":
            reply({"jsonrpc": "2.0", "id": rid,
                   "result": {"pad": "X" * (2 * 1024 * 1024)}})
            continue
        if mode == "error":
            reply({"jsonrpc": "2.0", "id": rid,
                   "error": {"code": -32000,
                             "message": "HOSTILE_PROVIDER_MARKER " + ("s3cr3t" * 64)}})
            continue
        reply({"jsonrpc": "2.0", "id": rid,
               "result": {"protocolVersion": "2024-11-05"}})
    elif method == "tools/list":
        reply({"jsonrpc": "2.0", "id": rid, "result": {"tools": tools}})
    elif method == "tools/call":
        params = msg.get("params", {})
        reply({"jsonrpc": "2.0", "id": rid,
               "result": {"content": [{"type": "text",
                                       "text": "called:" + str(params.get("name"))}]}})
    else:
        reply({"jsonrpc": "2.0", "id": rid, "result": {}})

log("eof")
