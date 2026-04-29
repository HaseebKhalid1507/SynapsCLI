#!/usr/bin/env python3
"""
Time extension for SynapsCLI — injects current date/time into every message.

Speaks JSON-RPC 2.0 over stdio with Content-Length framing.
Subscribes to before_message hook and returns an Inject result
with the current timestamp.
"""

import json
import sys
from datetime import datetime

def read_message():
    """Read a Content-Length framed JSON-RPC message from stdin."""
    header = sys.stdin.readline()
    if not header:
        return None
    
    # Parse Content-Length
    if not header.startswith("Content-Length:"):
        return None
    
    length = int(header.split(":")[1].strip())
    sys.stdin.readline()  # blank line separator
    body = sys.stdin.read(length)
    return json.loads(body)

def write_message(msg):
    """Write a Content-Length framed JSON-RPC message to stdout."""
    body = json.dumps(msg)
    frame = f"Content-Length: {len(body)}\r\n\r\n{body}"
    sys.stdout.write(frame)
    sys.stdout.flush()

def handle_hook(request):
    """Handle a hook.handle call."""
    params = request.get("params", {})
    kind = params.get("kind", "")
    
    if kind == "before_message":
        now = datetime.now().strftime("%A, %B %d, %Y at %I:%M %p")
        return {
            "action": "inject",
            "content": f"[Current date and time: {now}]"
        }
    
    return {"action": "continue"}

def main():
    sys.stderr.write("[time-ext] Started\n")
    sys.stderr.flush()
    
    while True:
        try:
            request = read_message()
            if request is None:
                break
            
            method = request.get("method", "")
            req_id = request.get("id", 0)
            
            if method == "hook.handle":
                result = handle_hook(request)
            elif method == "shutdown":
                sys.stderr.write("[time-ext] Shutting down\n")
                sys.stderr.flush()
                write_message({"jsonrpc": "2.0", "result": None, "id": req_id})
                break
            else:
                result = {"action": "continue"}
            
            write_message({
                "jsonrpc": "2.0",
                "result": result,
                "id": req_id
            })
            
        except Exception as e:
            sys.stderr.write(f"[time-ext] Error: {e}\n")
            sys.stderr.flush()
            break

if __name__ == "__main__":
    main()
