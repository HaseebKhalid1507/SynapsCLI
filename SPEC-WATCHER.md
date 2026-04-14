# Watcher — Autonomous Agent Supervisor for SynapsCLI

**Version:** 0.1.0
**Author:** Jawz (S157)
**Date:** 2026-04-13

---

## 1. Objective

**What:** A generalized autonomous agent system built into SynapsCLI. Users define agents via config files — each agent has an identity, a trigger mode (always-on, cron, file-watch, webhook, manual), resource limits, and a handoff protocol for session continuity. A supervisor daemon manages all agent lifecycles.

**Who:** Haseeb (dogfooding with Dexter, Shadow, file-watchers), and eventually any developer using SynapsCLI who wants persistent autonomous agents.

**Success looks like:**
- `watcher deploy dexter` starts an always-on trading agent that runs 24/7, reboots itself on token limits, and maintains state across sessions
- `watcher deploy watcher --trigger watch:~/inbox` starts an agent that wakes when files appear, processes them, then sleeps
- `watcher status` shows all running agents, uptime, session count, cost
- An agent can run unattended for days without human intervention
- Crash recovery is automatic — supervisor restarts within 30 seconds
- Zero token waste between sessions — handoff is lean, boot is fast

---

## 2. Architecture

```
┌──────────────────────────────────────────────────────────────┐
│  WATCHER SUPERVISOR (always running, single process)         │
│  Binary: watcher                                             │
│  - Reads agent configs from ~/.synaps-cli/watcher/           │
│  - Spawns/monitors/restarts agent worker processes            │
│  - Watches heartbeats, enforces resource limits               │
│  - Handles triggers (cron, file-watch, always-on)             │
│  - Exposes status via Unix socket                             │
│  - Managed by systemd                                         │
└────────────────┬─────────────────────────────────────────────┘
                 │ spawns (one per active agent)
                 ▼
┌──────────────────────────────────────────────────────────────┐
│  AGENT WORKER (ephemeral, dies & reborns)                     │
│  Binary: agent                                                │
│  - Uses synaps_cli::Runtime (library, no TUI)                 │
│  - Boots with: system prompt + handoff state + task/trigger    │
│  - Runs agentic loop until: token limit / time limit / idle   │
│  - Writes handoff.json on exit                                │
│  - Touches heartbeat file every 30s                           │
│  - Exits cleanly → supervisor decides: restart or sleep       │
└──────────────────────────────────────────────────────────────┘
```

### Binary Targets

```toml
[[bin]]
name = "agent"
path = "src/bin/agent.rs"

[[bin]]
name = "watcher"  
path = "src/watcher/mod.rs"
```

---

## 3. Agent Configuration

Each agent is a directory under `~/.synaps-cli/watcher/<name>/`:

```
~/.synaps-cli/watcher/
├── dexter/
│   ├── config.toml       # Agent configuration
│   ├── soul.md            # System prompt (identity)
│   ├── handoff.json       # State from last session (auto-managed)
│   ├── heartbeat          # Timestamp file (auto-managed)
│   └── logs/              # Session logs (auto-managed)
│       ├── session-001.json
│       └── current.log
├── watcher/
│   ├── config.toml
│   ├── soul.md
│   ├── handoff.json
│   └── logs/
└── shadow/
    ├── config.toml
    ├── soul.md
    ├── handoff.json
    └── logs/
```

### config.toml Schema

```toml
[agent]
name = "dexter"
model = "claude-sonnet-4-20250514"
thinking = "medium"

# Trigger mode — determines when the agent runs
# "always"     — runs 24/7, restarts immediately on exit
# "cron"       — runs on schedule (cron expression)
# "watch"      — wakes when files change in watched directories
# "webhook"    — wakes on HTTP POST to watcher API
# "manual"     — only runs when explicitly started via CLI
trigger = "always"

# Trigger-specific config
[trigger]
# For cron: schedule = "0 */6 * * *"        # every 6 hours
# For watch: paths = ["~/inbox", "~/data"]
#            debounce_secs = 5               # wait for writes to settle
#            patterns = ["*.csv", "*.json"]  # optional glob filter
# For webhook: path = "/agents/dexter"      # POST endpoint path

# Session lifecycle
[limits]
max_session_tokens = 100000       # kill & restart after this many tokens
max_session_duration_mins = 60    # kill & restart after this long
max_session_cost_usd = 0.50      # kill & restart after this cost
max_daily_cost_usd = 10.00       # stop for the day after this
max_tool_calls = 200              # kill if agent loops on tools
cooldown_secs = 10                # delay between session restarts
max_retries = 3                   # consecutive crash restarts before giving up
idle_timeout_secs = 300           # for watch/webhook: sleep after idle

# Heartbeat
[heartbeat]
interval_secs = 30                # how often agent touches heartbeat file
stale_threshold_secs = 120        # supervisor kills after this long without heartbeat

# Boot message — injected as first user message each session
[boot]
# Template with variables: {handoff}, {trigger_context}, {timestamp}
message = """
You are waking up for a new session. Current time: {timestamp}

## State from your last session:
{handoff}

## What triggered this session:
{trigger_context}

Review your state, decide what to do, and get to work. When you've done
what you can or hit a natural stopping point, write your handoff state
and exit cleanly.
"""

# Handoff — what the agent writes for its next self
[handoff]
# The agent is instructed to write a JSON file with these fields
# before exiting. Supervisor reads it and injects into next boot.
path = "handoff.json"
max_size_kb = 50                  # prevent handoff bloat
```

---

## 4. Session Lifecycle

```
                    ┌──────────┐
                    │ TRIGGER  │ (cron fires, file changes, always-on restart, manual)
                    └────┬─────┘
                         ▼
                ┌────────────────┐
                │   BOOT PHASE   │
                │                │
                │ 1. Load soul.md (system prompt)
                │ 2. Load handoff.json (last state)
                │ 3. Build trigger context (what woke us)
                │ 4. Compose boot message from template
                │ 5. Create Runtime with tools
                │ 6. Send boot message → start agentic loop
                └────────┬───────┘
                         ▼
                ┌────────────────┐
                │  WORK PHASE    │◄──────────────────┐
                │                │                    │
                │ Agent thinks, calls tools,          │
                │ produces output.                    │
                │ Heartbeat touches file every 30s.   │
                │                │                    │
                │ Check limits after each turn:       │
                │  - tokens > max? → EXIT             │
                │  - time > max? → EXIT               │
                │  - cost > max? → EXIT               │
                │  - tool_calls > max? → EXIT         │
                │  - idle > timeout? → EXIT (watch)   │
                │  - agent says "done"? → EXIT        │
                └────────┬───────┘
                         ▼
                ┌────────────────┐
                │  EXIT PHASE    │
                │                │
                │ 1. Send "write your handoff" message
                │ 2. Agent writes handoff.json via tool
                │ 3. Save session log
                │ 4. Process exits cleanly (code 0)
                └────────┬───────┘
                         ▼
                ┌────────────────┐
                │  SUPERVISOR    │
                │  DECISION      │
                │                │
                │ Exit code 0 + always → restart (after cooldown)
                │ Exit code 0 + watch  → sleep (wait for trigger)
                │ Exit code 0 + cron   → sleep (wait for schedule)
                │ Exit code != 0       → crash recovery (retry)
                │ Daily cost exceeded  → stop until midnight
                └────────────────┘
```

### Agent-Initiated Exit

The agent can signal it's done by calling a special tool or writing a watcher marker:

```
Tool: watcher_exit
Parameters:
  reason: "completed daily analysis"
  handoff: { ... state for next session ... }
```

This tool writes handoff.json and returns a special exit signal that the worker loop catches.

---

## 5. Supervisor Responsibilities

### Process Management
- Spawn `agent` binary as child process with agent-specific config
- Monitor child PID — detect crashes (non-zero exit), clean exits, OOM kills
- Enforce max_retries — if agent crashes N times in a row, stop and alert
- Reset retry counter on successful session

### Heartbeat Monitoring
- Check each agent's heartbeat file mtime every 30 seconds
- If stale > threshold → SIGTERM → wait 10s → SIGKILL → restart
- Log heartbeat failures for debugging

### Trigger Management
- **always:** Restart immediately (after cooldown) on clean exit
- **cron:** Use tokio-cron-scheduler or simple timer loop
- **watch:** Use notify crate (inotify on Linux) to watch directories
- **webhook:** Lightweight HTTP server (axum, already a dependency) on a configured port
- **manual:** Only start via CLI command

### Resource Enforcement
- Track cumulative daily cost per agent (persist across restarts)
- Reset daily counters at midnight
- Pause agent if daily limit exceeded

### Status API
- Unix socket at `/tmp/watcher.sock`
- JSON protocol for status queries
- Used by `watcher status` CLI command

---

## 6. CLI Interface

```bash
# Deploy an agent (starts it according to its trigger mode)
watcher deploy <name>

# Stop a running agent
watcher stop <name>

# Restart an agent
watcher restart <name>

# Show status of all agents
watcher status
# Output:
# AGENT      TRIGGER   STATUS     SESSION   UPTIME     COST TODAY
# dexter     always    running    #47       2h 15m     $1.23
# watcher    watch     sleeping   #12       —          $0.00
# shadow     cron      sleeping   #8        —          $0.45
# monitor    manual    stopped    —         —          $0.00

# Show detailed status of one agent
watcher status <name>
# Output:
# Agent: dexter
# Trigger: always
# Status: running (session #47)
# Model: claude-sonnet-4-20250514
# Uptime: 2h 15m (current session: 14m)
# Tokens: 45,230 / 100,000
# Cost: $0.18 (session) / $1.23 (today) / $10.00 (limit)
# Tool calls: 23
# Last heartbeat: 12s ago
# Handoff size: 2.1 KB
# Crashes today: 0

# View agent logs (tail -f style)
watcher logs <name> [--follow]

# List configured agents (deployed or not)
watcher list

# Create a new agent from template
watcher init <name>

# Run supervisor daemon (foreground, for systemd)
watcher run

# One-shot: run an agent once, don't supervise
watcher once <name>
```

---

## 7. Agent Worker Binary (`agent`)

### Core Loop (pseudocode)

```rust
fn main() {
    let config = load_config(args.agent_name);
    let soul = load_soul(args.agent_name);
    let handoff = load_handoff(args.agent_name);
    let trigger_context = args.trigger_context; // passed by supervisor
    
    // Build boot message
    let boot_msg = config.boot.message
        .replace("{handoff}", &handoff)
        .replace("{trigger_context}", &trigger_context)
        .replace("{timestamp}", &now());
    
    // Create runtime
    let runtime = Runtime::new().await;
    runtime.set_model(&config.agent.model);
    runtime.set_system_prompt(&soul);
    
    // Register tools (including watcher_exit)
    let tools = default_tools();  // bash, read, write, edit, grep, find, ls
    tools.register(WatcherExitTool::new());
    tools.register(HeartbeatTool::new(&config));
    
    // Start heartbeat thread
    let hb = spawn_heartbeat(config.heartbeat.interval_secs);
    
    // Send boot message and run
    let mut messages = vec![user_message(&boot_msg)];
    let mut total_tokens = 0;
    let mut total_cost = 0.0;
    let mut tool_calls = 0;
    let start = Instant::now();
    
    loop {
        let stream = runtime.run_stream_with_messages(messages.clone(), cancel.clone());
        
        // Process stream events
        for event in stream {
            match event {
                StreamEvent::ToolUse { .. } => tool_calls += 1,
                StreamEvent::Usage { input_tokens, output_tokens, .. } => {
                    total_tokens += input_tokens + output_tokens;
                    total_cost += calculate_cost(input_tokens, output_tokens, &config.agent.model);
                }
                StreamEvent::MessageHistory(history) => {
                    messages = history;
                }
                // ... handle other events
            }
        }
        
        // Check limits
        if total_tokens > config.limits.max_session_tokens { break; }
        if start.elapsed() > config.limits.max_session_duration { break; }
        if total_cost > config.limits.max_session_cost { break; }
        if tool_calls > config.limits.max_tool_calls { break; }
        if watcher_exit_requested { break; }
        
        // Touch heartbeat
        hb.touch();
    }
    
    // Exit phase: ask agent to write handoff
    if !watcher_exit_requested {
        let exit_msg = "You're being shut down (resource limit reached). \
                        Write your handoff state to handoff.json — include: \
                        what you accomplished, what's pending, any context \
                        your next session needs.";
        messages.push(user_message(exit_msg));
        runtime.run_stream_with_messages(messages, cancel).await;
    }
    
    // Save session log
    save_session_log(&messages, &config);
    
    // Clean exit
    std::process::exit(0);
}
```

### Heartbeat

Simple file touch in a background thread:

```rust
fn spawn_heartbeat(interval_secs: u64) -> HeartbeatHandle {
    let path = watcher_dir.join("heartbeat");
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(interval_secs)).await;
            let _ = std::fs::write(&path, now_unix().to_string());
        }
    })
}
```

### Special Tools

**`watcher_exit`** — Agent calls this when it decides it's done:
```json
{
    "name": "watcher_exit",
    "description": "Signal that you've completed your work and are ready to shut down. Write your handoff state for your next session.",
    "parameters": {
        "reason": "string — why you're exiting",
        "handoff": "object — state for your next session (what you did, what's pending, context)"
    }
}
```

When called: writes handoff.json, sets exit flag, tool returns "shutdown acknowledged."

---

## 8. File & Directory Structure

```
~/.synaps-cli/watcher/
├── watcher.toml              # Global supervisor config (port, log level, etc.)
├── <agent-name>/
│   ├── config.toml            # Agent configuration
│   ├── soul.md                # System prompt / identity
│   ├── handoff.json           # State from last session (auto-managed)
│   ├── heartbeat              # Unix timestamp (auto-managed)
│   ├── stats.json             # Cumulative stats (sessions, cost, uptime)
│   └── logs/
│       ├── session-NNN.jsonl  # Session logs (messages + events)
│       └── supervisor.log     # Supervisor log for this agent
```

### Global Config (`watcher.toml`)

```toml
[supervisor]
socket = "/tmp/watcher.sock"     # Status API socket
log_level = "info"                 # debug, info, warn, error
max_concurrent_agents = 5          # prevent resource exhaustion

[webhook]
enabled = false
bind = "127.0.0.1:7700"           # webhook listener address

[defaults]
model = "claude-sonnet-4-20250514"
thinking = "medium"
max_session_tokens = 100000
max_session_duration_mins = 60
cooldown_secs = 10
```

---

## 9. Trigger Modes — Detail

### always
- Agent runs continuously. On clean exit → cooldown → restart.
- On crash → retry with backoff (10s, 30s, 60s, 5m).
- On daily cost limit → pause until midnight UTC.

### cron
- Supervisor runs a scheduler. When cron fires → spawn agent with trigger_context = "scheduled run at {time}".
- If agent is already running when cron fires → skip (don't queue).
- Format: standard cron (e.g., `"0 */6 * * *"` = every 6 hours).

### watch
- Supervisor watches directories using `notify` crate.
- On file change → debounce → spawn agent with trigger_context = "file changed: {paths}".
- Pass changed file paths to agent so it knows what to process.
- Agent exits when done → goes to sleep → wakes on next file change.
- `idle_timeout_secs` — if agent has no tool calls for this long, exit.

### webhook
- Supervisor runs HTTP endpoint (axum).
- POST to `/agents/<name>` → spawn agent with trigger_context = request body.
- Returns 202 Accepted immediately (async execution).
- GET to `/agents/<name>/status` → current status.

### manual
- Only starts via `watcher once <name>` or `watcher deploy <name>`.
- `deploy` with manual trigger = start once, don't restart on exit.

---

## 10. Observability

### Stats File (`stats.json`)

Persisted per-agent, updated by supervisor after each session:

```json
{
    "total_sessions": 47,
    "total_tokens": 4523000,
    "total_cost_usd": 12.45,
    "total_uptime_secs": 86400,
    "total_tool_calls": 1230,
    "crashes": 3,
    "last_crash": "2026-04-12T03:14:00Z",
    "last_crash_reason": "heartbeat timeout",
    "today": {
        "date": "2026-04-13",
        "sessions": 5,
        "cost_usd": 1.23,
        "tokens": 452300
    }
}
```

### Logging

- Supervisor logs to stderr (captured by systemd journal)
- Per-agent logs in `logs/supervisor.log` (rotation: keep last 10)
- Session message history in `logs/session-NNN.jsonl`
- Log format: `[2026-04-13T14:30:00Z] [INFO] [dexter] session #47 started (trigger: always)`

---

## 11. Implementation Plan

### Phase 1: Agent Worker (~200 LOC)
- `src/bin/agent.rs` — headless binary using `Runtime`
- Boot with system prompt + initial message
- Run agentic loop with token/time/cost limit checking
- Heartbeat file touch in background
- `watcher_exit` tool
- Handoff write on exit
- **Test:** Run manually, verify it boots, works, writes handoff, exits

### Phase 2: Supervisor Core (~400 LOC)
- `src/watcher/mod.rs` — daemon binary
- Load agent configs from `~/.synaps-cli/watcher/`
- Spawn/monitor agent worker processes
- Heartbeat monitoring + kill/restart
- Trigger: `always` mode only (simplest)
- CLI: `watcher run`, `watcher deploy`, `watcher stop`, `watcher status`
- **Test:** Deploy an always-on agent, verify restart cycle

### Phase 3: Trigger Modes (~300 LOC)
- Cron scheduler (tokio timer or cron crate)
- File watcher (notify crate)
- Manual trigger
- **Test:** File watcher agent that processes inbox

### Phase 4: Resource Management (~200 LOC)
- Daily cost tracking + enforcement
- Stats persistence
- Concurrent agent limits
- Crash retry with backoff
- **Test:** Verify daily limit pauses agent

### Phase 5: Polish (~200 LOC)
- Webhook trigger (axum endpoint, already a dependency)
- `watcher init` scaffolding
- `watcher logs --follow`
- Status socket API
- systemd unit file
- **Test:** Full integration — multiple agents, mixed triggers

### Total: ~1,300 LOC across 2 new binaries

---

## 12. Dependencies

**Already in Cargo.toml (reused):**
- `tokio` — async runtime
- `serde` / `serde_json` — serialization
- `chrono` — timestamps
- `axum` — webhook HTTP server
- `tracing` — logging
- `synaps_cli` — Runtime, tools, auth, session

**New dependencies:**
- `toml` — parse config.toml (~lightweight)
- `notify` — file system watching (inotify on Linux)
- `cron` or `tokio-cron-scheduler` — cron expressions

---

## 13. Commands

```bash
# Build
cargo build --release --bin agent --bin watcher

# Test
cargo test

# Run supervisor (foreground, for dev)
watcher run

# Deploy
watcher deploy dexter
watcher deploy watcher
watcher status
watcher logs dexter --follow
watcher stop dexter
```

---

## 14. Project Structure

```
src/
├── agent.rs              # Headless autonomous agent worker
├── watcher.rs           # Supervisor daemon
├── watcher/
│   ├── config.rs         # Config parsing (TOML)
│   ├── supervisor.rs     # Process management, heartbeat, restart logic
│   ├── triggers.rs       # Always, cron, watch, webhook, manual
│   └── status.rs         # Unix socket status API + CLI output
├── chatui/               # (existing TUI)
├── runtime.rs            # (existing Runtime — shared by agent + chatui)
├── tools.rs              # (existing tools — shared by agent + chatui)
└── ...
```

---

## 15. Testing Strategy

- **Unit tests:** Config parsing, trigger logic, handoff read/write, limit checking
- **Integration tests:** Spawn agent process, verify heartbeat, verify handoff cycle
- **Manual smoke test:** Deploy dexter (always-on), watch it reboot 3 times, check logs
- **Chaos test:** Kill agent mid-stream, verify supervisor restarts it

---

## 16. Boundaries

### Always Do
- Write handoff before exit (even on forced shutdown — best effort)
- Log every session start/stop/crash with timestamps
- Enforce resource limits — never let an agent run unbounded
- Touch heartbeat — if agent hangs, supervisor must be able to kill it

### Ask First
- Adding new trigger modes beyond the five specified
- Changing the config.toml schema after v1
- Adding inter-agent communication (agents talking to each other)

### Never Do
- Let agents modify their own config.toml or soul.md
- Skip heartbeat monitoring (that's how you get runaways)
- Run agent as root or with elevated privileges
- Store API keys in agent config files (use existing auth system)

---

## 17. Open Questions

1. **Agent-to-agent communication?** Should agents be able to message each other, or is that a v2 feature? (Recommendation: v2)
2. **Web dashboard?** Live view of all agents? (Recommendation: v2, CLI-first)
3. **Remote agents?** Run agents on different machines? (Recommendation: v2, single-host first)
4. **Existing Shadow migration?** Port current Python Shadow to Watcher, or run both? (Recommendation: migrate after Watcher is proven)
