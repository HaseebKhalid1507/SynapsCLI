#!/usr/bin/env python3
import json
import sys

for line in sys.stdin:
    if not line.strip():
        continue
    msg = json.loads(line)
    typ = msg.get("type")
    if typ == "init":
        print(json.dumps({
            "type": "hello",
            "protocol_version": 2,
            "extension": "mock-sidecar",
            "capabilities": ["insert-text", "status"],
        }), flush=True)
        print(json.dumps({"type": "status", "state": "ready"}), flush=True)
    elif typ == "trigger":
        name = msg.get("name")
        if name == "press":
            print(json.dumps({"type": "status", "state": "active", "label": "Active"}), flush=True)
        elif name == "release":
            print(json.dumps({"type": "status", "state": "processing", "label": "Processing"}), flush=True)
            print(json.dumps({"type": "insert_text", "text": "hello from sidecar", "mode": "final"}), flush=True)
    elif typ == "shutdown":
        break
