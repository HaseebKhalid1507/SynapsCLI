#!/usr/bin/env python3
"""
Test harness for Synaps Hub — simulates subagent dispatch events
without needing SynapsCLI running.

Run hub.py first, then run this script to simulate agents working.
"""

import json
import subprocess
import sys
import time
import threading

def send_rpc(proc, method, params, req_id):
    """Send a JSON-RPC message to the hub process."""
    request = {"jsonrpc": "2.0", "method": method, "params": params, "id": req_id}
    body = json.dumps(request)
    frame = f"Content-Length: {len(body)}\r\n\r\n{body}"
    proc.stdin.write(frame.encode())
    proc.stdin.flush()

def read_rpc(proc):
    """Read a JSON-RPC response."""
    header = b""
    while not header.endswith(b"\r\n"):
        header += proc.stdout.read(1)
    length = int(header.decode().split(":")[1].strip())
    proc.stdout.read(2)  # \r\n separator
    body = proc.stdout.read(length)
    return json.loads(body)

def dispatch_agent(proc, agent_name, task, req_id):
    """Simulate dispatching a subagent."""
    send_rpc(proc, "hook.handle", {
        "kind": "before_tool_call",
        "tool_name": "subagent",
        "tool_input": {"agent": agent_name, "task": task},
        "tool_output": None,
        "message": None,
        "session_id": None,
        "data": None,
    }, req_id)
    return read_rpc(proc)

def complete_agent(proc, agent_name, req_id):
    """Simulate a subagent completing."""
    send_rpc(proc, "hook.handle", {
        "kind": "after_tool_call",
        "tool_name": "subagent",
        "tool_input": {"agent": agent_name},
        "tool_output": "done",
        "message": None,
        "session_id": None,
        "data": None,
    }, req_id)
    return read_rpc(proc)

def main():
    print("Starting hub process...")
    proc = subprocess.Popen(
        [sys.executable, "hub.py"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=sys.stderr,
        cwd=str(__import__('pathlib').Path(__file__).parent),
    )
    
    time.sleep(1)  # Let web server start
    print("Hub started. Open http://localhost:3456 in your browser.\n")
    
    req_id = 1
    
    # Simulate a realistic session
    scenarios = [
        ("spike",    "Fixing the auth module in storage.rs"),
        ("chrollo",  "Analyzing the codebase architecture for review"),
        ("shady",    "Reviewing PR #10 for security issues"),
    ]
    
    print("Dispatching 3 agents...")
    for agent, task in scenarios:
        resp = dispatch_agent(proc, agent, task, req_id)
        print(f"  → {agent}: {task[:50]}... (resp: {resp['result']['action']})")
        req_id += 1
        time.sleep(0.5)
    
    time.sleep(3)
    
    # Dispatch more Chrollos (multitasking!)
    print("\nDispatching 2 more Chrollos (multitasking)...")
    dispatch_agent(proc, "chrollo", "Deep dive on velocirag search pipeline", req_id)
    req_id += 1
    time.sleep(0.3)
    dispatch_agent(proc, "chrollo", "Analyzing memkoshi memory patterns", req_id)
    req_id += 1
    
    time.sleep(4)
    
    # Complete some tasks
    print("\nSpike finished!")
    complete_agent(proc, "spike", req_id)
    req_id += 1
    
    time.sleep(2)
    
    print("Shady finished!")
    complete_agent(proc, "shady", req_id)
    req_id += 1
    
    time.sleep(2)
    
    # Dispatch Yoru
    print("\nDispatching Yoru...")
    dispatch_agent(proc, "yoru", "Optimizing search query performance", req_id)
    req_id += 1
    
    time.sleep(3)
    
    # Complete Chrollo tasks one by one
    print("\nChrollo finishing tasks...")
    complete_agent(proc, "chrollo", req_id)
    req_id += 1
    time.sleep(1)
    complete_agent(proc, "chrollo", req_id)
    req_id += 1
    time.sleep(1)
    complete_agent(proc, "chrollo", req_id)
    req_id += 1
    
    print("\n✓ Simulation complete. Dashboard still live at http://localhost:3456")
    print("Press Ctrl+C to stop.\n")
    
    try:
        proc.wait()
    except KeyboardInterrupt:
        proc.terminate()

if __name__ == "__main__":
    main()
