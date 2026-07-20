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
           huge-stderr (floods stderr before serving normally) |
           recall-timeout (recall calls sleep past the host's 150ms
           spec-16.2 hard budget before answering) |
           recall-malformed (recall calls return a structurally invalid
           contribution shape) |
           recall-cross-project (recall calls return a valid-shaped
           contribution claiming a DIFFERENT project id, so the host's
           validate_contribution must reject it)
  argv[4]  optional path to a JSON array of providers to register at
           initialize (RegisteredProviderSpec-shaped objects)
  argv[5]  optional path to a JSON array of context providers
           (DeclaredExtensionContextProvider-shaped objects) declared in
           the initialize response as `context_providers` (task B6)

Recall calls (continuous-memory task B6) arrive on the REAL engine path as
`tool.call` frames naming the manifest-declared `memory_recall` tool; a
direct `context_provider.recall` RPC method is served identically. Every
recall call received logs a `recall:<count>` spy event so tests can assert
exact call counts.
"""
import json
import sys
import time

SPY = sys.argv[1]
TOOLS_PATH = sys.argv[2] if len(sys.argv) > 2 else None
MODE = sys.argv[3] if len(sys.argv) > 3 else "ok"
PROVIDERS_PATH = sys.argv[4] if len(sys.argv) > 4 else None
CONTEXT_PROVIDERS_PATH = sys.argv[5] if len(sys.argv) > 5 else None

RECALL_TOOL_NAME = "memory_recall"
recall_calls = 0


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


def recall_response(params):
    """One MemoryContextContributionWire-shaped recall result (task B6).

    MODE "ok" echoes the host-authored project_id back with 2-3 synthetic
    model_visible records carrying plausible rank reasons; the recall-*
    failure modes produce exactly one typed failure shape each.
    """
    request_wire = params.get("input", params) or {}
    project_id = str(request_wire.get("project_id", "project-unknown"))
    if MODE == "recall-timeout":
        # Sleep well past the host's 150ms spec-16.2 hard budget, then
        # answer normally: the host must already have failed open.
        time.sleep(1.0)
    if MODE == "recall-malformed":
        # Structurally invalid contribution: `records` is not an array and
        # `rendered` is not a string — the host wire parser must reject it.
        return {
            "schema": "contribution/1",
            "provider_id": "project-memory",
            "project_id": project_id,
            "records": "not-an-array",
            "rendered": 42,
        }
    if MODE == "recall-cross-project":
        # Valid SHAPE, wrong project: parses fine, then the host's
        # validate_contribution must reject the project mismatch.
        project_id = "project-cwd-0000000000000000"
    records = [
        {
            "memory_id": "mem-b6-0001",
            "source": "chat_history",
            "timestamp": 1752000000,
            "rank_reason": ["exact_topic"],
            "sensitivity": "model_visible",
            "retention": "standard",
            "content": "Decision: extension authority flows through "
                       "host-minted leases (B6-REC-ALPHA).",
            "truncated": False,
        },
        {
            "memory_id": "mem-b6-0002",
            "source": "user_stated",
            "timestamp": 1752000100,
            "rank_reason": ["recency"],
            "sensitivity": "model_visible",
            "retention": "standard",
            "content": "Preference: keep extension spawns exact and "
                       "lease-scoped (B6-REC-BETA).",
            "truncated": False,
        },
        {
            "memory_id": "mem-b6-0003",
            "source": "chat_history",
            "timestamp": 1752000200,
            "rank_reason": ["exact_topic", "recency"],
            "sensitivity": "model_visible",
            "retention": "standard",
            "content": "Unresolved: extend the recall harness to turn "
                       "capture (B6-REC-GAMMA).",
            "truncated": False,
        },
    ]
    rendered = "\n".join(
        "%d. %s — %s" % (i + 1, r["memory_id"], r["content"])
        for i, r in enumerate(records)
    )
    return {
        "schema": "contribution/1",
        "provider_id": "project-memory",
        "project_id": project_id,
        "records": records,
        "rendered": rendered,
        "accounting": {"candidates_considered": 7, "withheld": 1,
                       "truncated": 0},
    }


log("spawn")
tools = []
if TOOLS_PATH:
    with open(TOOLS_PATH, encoding="utf-8") as f:
        tools = json.load(f)
providers = []
if PROVIDERS_PATH:
    with open(PROVIDERS_PATH, encoding="utf-8") as f:
        providers = json.load(f)
context_providers = []
if CONTEXT_PROVIDERS_PATH:
    with open(CONTEXT_PROVIDERS_PATH, encoding="utf-8") as f:
        context_providers = json.load(f)

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
        called = str(request.get("params", {}).get("name"))
        if called == RECALL_TOOL_NAME:
            recall_calls += 1
            log("recall:%d" % recall_calls)
        else:
            log("call:" + called)
    elif method == "context_provider.recall":
        recall_calls += 1
        log("recall:%d" % recall_calls)
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
        capabilities = {"tools": tools, "providers": providers,
                        "capabilities": []}
        if context_providers:
            capabilities["context_providers"] = context_providers
        respond(request, {
            "protocol_version": 1,
            "capabilities": capabilities,
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
    elif method == "context_provider.recall":
        respond(request, recall_response(request.get("params", {})))
    elif method == "tool.call":
        name = request.get("params", {}).get("name")
        if name == RECALL_TOOL_NAME:
            respond(request, recall_response(request.get("params", {})))
        elif MODE == "hostile-error":
            respond_error(request, -32000,
                          "HOSTILE_EXTENSION_MARKER " + ("s3cr3t" * 64))
        else:
            respond(request, {"content": "called:" + str(name)})
    elif method == "shutdown":
        log("shutdown")
        respond(request, {"ok": True})
        sys.exit(0)
    else:
        respond(request, {})
