#!/usr/bin/env python3
"""
Synaps Hub — Live agent dashboard extension.

Runs a web dashboard showing subagent activity in real-time.
Hooks into before/after_tool_call:subagent to track agent states.

Usage: spawned by SynapsCLI extension system, serves on :3456
"""

import asyncio
import os
import json
import sys
import threading
import time
from datetime import datetime
from typing import Dict, List, Optional

try:
    from aiohttp import web
    HAS_AIOHTTP = True
except ImportError:
    HAS_AIOHTTP = False

# ── State ─────────────────────────────────────────────────────────────

class AgentTask:
    def __init__(self, task_id: str, description: str):
        self.task_id = task_id
        self.description = description
        self.started = time.time()
        self.status = "working"  # working, done, failed

class AgentState:
    def __init__(self, name: str):
        self.name = name
        self.tasks: Dict[str, AgentTask] = {}
        self.completed_count = 0
        self.last_active: Optional[float] = None

    @property
    def status(self):
        active = [t for t in self.tasks.values() if t.status == "working"]
        if len(active) > 1:
            return "multitasking"
        elif len(active) == 1:
            return "working"
        return "idle"

    @property
    def active_tasks(self):
        return [t for t in self.tasks.values() if t.status == "working"]

    def to_dict(self):
        return {
            "name": self.name,
            "status": self.status,
            "active_tasks": [
                {
                    "id": t.task_id,
                    "description": t.description[:80],
                    "elapsed": round(time.time() - t.started, 1),
                }
                for t in self.active_tasks
            ],
            "active_count": len(self.active_tasks),
            "completed": self.completed_count,
            "last_active": self.last_active,
        }

class HubState:
    def __init__(self):
        self.agents: Dict[str, AgentState] = {}
        self.total_dispatched = 0
        self.total_completed = 0
        self.session_start = time.time()
        self.websockets: List[web.WebSocketResponse] = []
        self._lock = threading.Lock()

    def get_or_create(self, name: str) -> AgentState:
        if name not in self.agents:
            self.agents[name] = AgentState(name)
        return self.agents[name]

    def dispatch(self, agent_name: str, task_id: str, description: str):
        with self._lock:
            agent = self.get_or_create(agent_name)
            agent.tasks[task_id] = AgentTask(task_id, description)
            agent.last_active = time.time()
            self.total_dispatched += 1

    def complete(self, agent_name: str, task_id: str):
        with self._lock:
            agent = self.get_or_create(agent_name)
            if task_id in agent.tasks:
                agent.tasks[task_id].status = "done"
                del agent.tasks[task_id]
                agent.completed_count += 1
                agent.last_active = time.time()
                self.total_completed += 1

    def snapshot(self):
        with self._lock:
            agents = []
            for name in sorted(self.agents.keys()):
                agents.append(self.agents[name].to_dict())
            
            active = sum(1 for a in self.agents.values() if a.status != "idle")
            return {
                "agents": agents,
                "stats": {
                    "active": active,
                    "total": len(self.agents),
                    "dispatched": self.total_dispatched,
                    "completed": self.total_completed,
                    "uptime": round(time.time() - self.session_start, 1),
                },
                "timestamp": datetime.now().isoformat(),
            }

state = HubState()

# ── JSON-RPC stdin reader ─────────────────────────────────────────────

def read_jsonrpc():
    """Read Content-Length framed JSON-RPC from stdin (blocking)."""
    header = sys.stdin.readline()
    if not header:
        return None
    if not header.startswith("Content-Length:"):
        return None
    length = int(header.split(":")[1].strip())
    sys.stdin.readline()  # blank separator
    body = sys.stdin.read(length)
    return json.loads(body)

def write_jsonrpc(msg):
    """Write Content-Length framed JSON-RPC to stdout."""
    body = json.dumps(msg)
    sys.stdout.write(f"Content-Length: {len(body)}\r\n\r\n{body}")
    sys.stdout.flush()

_task_counter = 0

def handle_hook(params):
    """Handle a hook event from SynapsCLI."""
    global _task_counter
    kind = params.get("kind", "")
    tool_name = params.get("tool_name", "")
    tool_input = params.get("tool_input", {})
    tool_output = params.get("tool_output", "")

    if kind == "before_tool_call" and tool_name in ("subagent", "subagent_start"):
        # Extract agent name and task
        if isinstance(tool_input, dict):
            agent_name = tool_input.get("agent", tool_input.get("agent_name", "unknown"))
            task_desc = tool_input.get("task", "")[:120]
        else:
            agent_name = "unknown"
            task_desc = str(tool_input)[:120]

        _task_counter += 1
        task_id = f"task_{_task_counter}"
        state.dispatch(agent_name, task_id, task_desc)
        broadcast_state()
        return {"action": "continue"}

    elif kind == "after_tool_call" and tool_name in ("subagent", "subagent_start", "subagent_collect"):
        # Try to figure out which agent completed
        if isinstance(tool_input, dict):
            agent_name = tool_input.get("agent", tool_input.get("agent_name", ""))
        else:
            agent_name = ""

        # For inline subagents (no agent name), use "unknown"
        if not agent_name:
            agent_name = "unknown"

        if agent_name in state.agents:
            # Complete the oldest active task for this agent
            agent = state.agents[agent_name]
            active = [t for t in agent.tasks.values() if t.status == "working"]
            if active:
                oldest = min(active, key=lambda t: t.started)
                state.complete(agent_name, oldest.task_id)
                broadcast_state()
        return {"action": "continue"}

    return {"action": "continue"}

def stdin_loop():
    """Blocking stdin reader loop (runs in a thread)."""
    while True:
        try:
            request = read_jsonrpc()
            if request is None:
                break

            method = request.get("method", "")
            req_id = request.get("id", 0)

            if method == "hook.handle":
                result = handle_hook(request.get("params", {}))
            elif method == "shutdown":
                write_jsonrpc({"jsonrpc": "2.0", "result": None, "id": req_id})
                break
            else:
                result = {"action": "continue"}

            write_jsonrpc({"jsonrpc": "2.0", "result": result, "id": req_id})

        except Exception as e:
            sys.stderr.write(f"[hub] stdin error: {e}\n")
            sys.stderr.flush()
            break

    # Parent process (synaps) exited — kill the whole process
    sys.stderr.write("[hub] stdin closed, exiting\n")
    sys.stderr.flush()
    os._exit(0)

# ── WebSocket broadcast ───────────────────────────────────────────────

_loop = None

def broadcast_state():
    """Push state to all connected WebSocket clients."""
    if _loop is None:
        return
    snapshot = state.snapshot()
    msg = json.dumps(snapshot)
    # Schedule the broadcast on the async loop
    asyncio.run_coroutine_threadsafe(_broadcast(msg), _loop)

async def _broadcast(msg):
    dead = []
    for ws in state.websockets:
        try:
            await ws.send_str(msg)
        except Exception:
            dead.append(ws)
    for ws in dead:
        state.websockets.remove(ws)

# ── Web server ────────────────────────────────────────────────────────

async def handle_index(request):
    return web.Response(text=DASHBOARD_HTML, content_type="text/html")

async def handle_ws(request):
    ws = web.WebSocketResponse()
    await ws.prepare(request)
    state.websockets.append(ws)

    # Send initial state
    await ws.send_str(json.dumps(state.snapshot()))

    async for msg in ws:
        pass  # We don't expect messages from the client

    state.websockets.remove(ws)
    return ws

async def handle_api(request):
    return web.json_response(state.snapshot())

async def tick_loop():
    """Periodic state broadcast for elapsed time updates."""
    while True:
        await asyncio.sleep(1)
        if state.websockets:
            msg = json.dumps(state.snapshot())
            await _broadcast(msg)

async def start_server():
    global _loop
    _loop = asyncio.get_event_loop()

    app = web.Application()
    app.router.add_get("/", handle_index)
    app.router.add_get("/ws", handle_ws)
    app.router.add_get("/api/state", handle_api)

    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, "127.0.0.1", 3456)
    await site.start()

    sys.stderr.write("[hub] Dashboard running at http://localhost:3456\n")
    sys.stderr.flush()

    # Start tick loop for elapsed time updates
    asyncio.create_task(tick_loop())

    # Block forever (stdin thread handles shutdown)
    while True:
        await asyncio.sleep(3600)

# ── Dashboard HTML ────────────────────────────────────────────────────

DASHBOARD_HTML = """<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Synaps Hub</title>
<style>
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body {
    background: #0a0a0f;
    color: #e0e0e0;
    font-family: 'JetBrains Mono', 'Fira Code', monospace;
    min-height: 100vh;
  }
  .header {
    text-align: center;
    padding: 24px;
    border-bottom: 1px solid #1a1a2e;
  }
  .header h1 {
    font-size: 28px;
    background: linear-gradient(135deg, #00d4ff, #7b2fef);
    -webkit-background-clip: text;
    -webkit-text-fill-color: transparent;
    letter-spacing: 4px;
  }
  .stats {
    display: flex;
    justify-content: center;
    gap: 32px;
    margin-top: 12px;
    font-size: 13px;
    color: #666;
  }
  .stats .val { color: #00d4ff; font-weight: bold; }
  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(280px, 1fr));
    gap: 16px;
    padding: 24px;
    max-width: 1200px;
    margin: 0 auto;
  }
  .agent-card {
    background: #12121f;
    border: 1px solid #1e1e35;
    border-radius: 12px;
    padding: 20px;
    transition: all 0.3s ease;
    position: relative;
    overflow: hidden;
  }
  .agent-card.working {
    border-color: #00d4ff44;
    box-shadow: 0 0 20px rgba(0, 212, 255, 0.08);
  }
  .agent-card.multitasking {
    border-color: #ffa50066;
    box-shadow: 0 0 20px rgba(255, 165, 0, 0.12);
  }
  .agent-card.idle {
    opacity: 0.5;
  }
  .agent-name {
    font-size: 18px;
    font-weight: bold;
    margin-bottom: 8px;
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .status-dot {
    width: 10px;
    height: 10px;
    border-radius: 50%;
    display: inline-block;
  }
  .status-dot.working { background: #00d4ff; animation: pulse 1.5s infinite; }
  .status-dot.multitasking { background: #ffa500; animation: pulse 0.8s infinite; }
  .status-dot.idle { background: #333; }
  @keyframes pulse {
    0%, 100% { opacity: 1; transform: scale(1); }
    50% { opacity: 0.5; transform: scale(0.8); }
  }
  .status-label {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 2px;
    margin-bottom: 12px;
  }
  .status-label.working { color: #00d4ff; }
  .status-label.multitasking { color: #ffa500; }
  .status-label.idle { color: #444; }
  .task {
    background: #0a0a14;
    border-left: 3px solid #00d4ff;
    padding: 8px 12px;
    margin-bottom: 6px;
    border-radius: 0 6px 6px 0;
    font-size: 12px;
  }
  .task.multi { border-left-color: #ffa500; }
  .task-desc {
    color: #aaa;
    line-height: 1.4;
    word-break: break-word;
  }
  .task-time {
    color: #555;
    font-size: 11px;
    margin-top: 4px;
  }
  .completed {
    font-size: 11px;
    color: #333;
    margin-top: 8px;
  }
  .multi-badge {
    background: #ffa500;
    color: #000;
    font-size: 10px;
    font-weight: bold;
    padding: 2px 6px;
    border-radius: 4px;
    margin-left: auto;
  }
  .empty-state {
    text-align: center;
    color: #333;
    padding: 60px;
    font-size: 16px;
  }
  .empty-state .hint {
    font-size: 12px;
    margin-top: 8px;
    color: #222;
  }
</style>
</head>
<body>
<div class="header">
  <h1>⚡ SYNAPS HUB</h1>
  <div class="stats">
    <span>Active: <span class="val" id="stat-active">0</span>/<span id="stat-total">0</span></span>
    <span>Dispatched: <span class="val" id="stat-dispatched">0</span></span>
    <span>Completed: <span class="val" id="stat-completed">0</span></span>
    <span>Uptime: <span class="val" id="stat-uptime">0s</span></span>
  </div>
</div>
<div class="grid" id="grid"></div>
<div class="empty-state" id="empty">
  Waiting for agents...<br>
  <span class="hint">Dispatch subagents in SynapsCLI to see them here</span>
</div>

<script>
const ws = new WebSocket(`ws://${location.host}/ws`);
const grid = document.getElementById('grid');
const empty = document.getElementById('empty');

function formatTime(secs) {
  if (secs < 60) return Math.round(secs) + 's';
  if (secs < 3600) return Math.round(secs / 60) + 'm ' + Math.round(secs % 60) + 's';
  return Math.round(secs / 3600) + 'h ' + Math.round((secs % 3600) / 60) + 'm';
}

function render(data) {
  // Stats
  document.getElementById('stat-active').textContent = data.stats.active;
  document.getElementById('stat-total').textContent = data.stats.total;
  document.getElementById('stat-dispatched').textContent = data.stats.dispatched;
  document.getElementById('stat-completed').textContent = data.stats.completed;
  document.getElementById('stat-uptime').textContent = formatTime(data.stats.uptime);

  if (data.agents.length === 0) {
    grid.innerHTML = '';
    empty.style.display = 'block';
    return;
  }
  empty.style.display = 'none';

  grid.innerHTML = data.agents.map(agent => {
    const isMulti = agent.status === 'multitasking';
    const taskClass = isMulti ? 'task multi' : 'task';
    const tasks = agent.active_tasks.map(t => `
      <div class="${taskClass}">
        <div class="task-desc">${escHtml(t.description)}</div>
        <div class="task-time">${formatTime(t.elapsed)}</div>
      </div>
    `).join('');

    const multiBadge = isMulti 
      ? `<span class="multi-badge">×${agent.active_count}</span>` 
      : '';

    return `
      <div class="agent-card ${agent.status}">
        <div class="agent-name">
          <span class="status-dot ${agent.status}"></span>
          ${escHtml(agent.name)}
          ${multiBadge}
        </div>
        <div class="status-label ${agent.status}">${agent.status}</div>
        ${tasks}
        ${agent.completed > 0 ? `<div class="completed">${agent.completed} completed</div>` : ''}
      </div>
    `;
  }).join('');
}

function escHtml(s) {
  const d = document.createElement('div');
  d.textContent = s;
  return d.innerHTML;
}

ws.onmessage = (e) => render(JSON.parse(e.data));
ws.onclose = () => {
  empty.innerHTML = 'Connection lost. Refresh to reconnect.';
  empty.style.display = 'block';
};
</script>
</body>
</html>
"""

# ── Main ──────────────────────────────────────────────────────────────

def main():
    if not HAS_AIOHTTP:
        sys.stderr.write("[hub] ERROR: aiohttp not installed. Run: pip install aiohttp\n")
        sys.stderr.flush()
        # Fall back to just responding to hooks without web UI
        stdin_loop()
        return

    # Start stdin reader in a background thread
    stdin_thread = threading.Thread(target=stdin_loop, daemon=True)
    stdin_thread.start()

    # Run the web server on the main thread
    try:
        asyncio.run(start_server())
    except KeyboardInterrupt:
        pass

if __name__ == "__main__":
    main()
