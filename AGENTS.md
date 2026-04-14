# SynapsCLI Agent Reference

You are an AI agent running inside the SynapsCLI runtime. This document describes your operating environment, available tools, and protocols.

## System Overview

SynapsCLI has three operational modes. You are running in one of them:

1. **Interactive** (`chatui`) — Human-agent chat with TUI, streaming, and tool access
2. **Headless Worker** (`synaps-agent`) — Autonomous agent managed by the watcher
3. **Subagent** — Dispatched by another agent for a focused task

Your responses are powered by an LLM (typically Claude) via the Anthropic API. You have access to a standardized tool suite described below.

---

## Tool Reference

### `bash`

Execute shell commands.

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `command` | string | yes | — | The bash command to execute |
| `timeout` | integer | no | 30 | Timeout in seconds, max 300 |

**Behavior:**
- Runs via `bash -c` with piped stdout/stderr
- ANSI escape sequences stripped from output
- Output truncated at 30KB with `[output truncated at 30KB]` notice
- Process killed on timeout with `kill_on_drop`
- Combined stdout + stderr in output

---

### `read`

Read file contents with line numbers.

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `path` | string | yes | — | Path to file (`~` expands to home) |
| `offset` | integer | no | 0 | Line to start from (0-indexed) |
| `limit` | integer | no | 500 | Max lines to read |

**Behavior:**
- Returns numbered lines: `{line_number}\t{content}`
- Validates UTF-8 — binary files return a clear error with suggestion to use `bash` + `xxd`
- Shows `... (N more lines)` if truncated

---

### `write`

Create or overwrite files.

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `path` | string | yes | — | Path to file |
| `content` | string | yes | — | Content to write |

**Behavior:**
- Creates parent directories automatically
- Atomic write: writes to `.agent-tmp` then renames
- Returns line count and byte count
- Completely overwrites existing files

---

### `edit`

Surgical string replacement.

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `path` | string | yes | — | Path to file |
| `old_string` | string | yes | — | Exact text to find (must match exactly once) |
| `new_string` | string | yes | — | Replacement text |

**Behavior:**
- Fails if `old_string` matches 0 or >1 times
- Atomic write via temp file + rename
- Include enough surrounding context to make the match unique

---

### `grep`

Search file contents using regex.

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `pattern` | string | yes | — | Regex pattern |
| `path` | string | no | `.` | File or directory to search |
| `include` | string | no | — | Glob filter (e.g. `"*.rs"`) |
| `context` | integer | no | — | Context lines before/after match |

**Behavior:**
- Recursive search, excludes `.git`, `node_modules`, `target`
- 15-second timeout
- Output truncated at 50KB with notice
- Returns `No matches found.` on zero results

---

### `find`

Find files by glob pattern.

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `pattern` | string | yes | — | Glob pattern (e.g. `"*.rs"`) |
| `path` | string | no | `.` | Directory to search from |
| `type` | string | no | — | `"f"` for files, `"d"` for directories |

**Behavior:**
- Recursive search, excludes `.git`, `node_modules`, `target`
- 10-second timeout

---

### `ls`

List directory contents.

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `path` | string | no | `.` | Directory to list |

**Behavior:**
- Uses `ls -lah` format (permissions, size, date)
- Returns `Directory is empty.` for empty directories

---

### `subagent`

Dispatch a specialist agent for a focused task. **Not available to subagents** (no recursion).

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `task` | string | yes | — | Task prompt for the subagent |
| `agent` | string | no* | — | Agent name → loads `~/.synaps-cli/agents/{name}.md` |
| `system_prompt` | string | no* | — | Inline system prompt (mutually exclusive with `agent`) |
| `model` | string | no | `claude-sonnet-4-20250514` | Model override |
| `timeout` | integer | no | 300 | Timeout in seconds |

*Must provide either `agent` or `system_prompt`.

**Behavior:**
- Runs in isolated thread with its own tokio runtime
- Has core tools (bash, read, write, edit, grep, find, ls) — no subagent, no MCP
- Thread panics are caught and reported as errors
- Returns partial results on timeout
- Streams status updates to parent's TUI panel
- Logs complete session to `~/.synaps-cli/logs/subagents/`
- Response prefixed with `[subagent:{name}]`

---

### `mcp_connect`

Connect to an MCP server and load its tools.

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `server` | string | yes | — | Server name from `mcp.json` config |

**Behavior:**
- Spawns server process and establishes JSON-RPC connection
- Registers tools as `mcp__{server}__{tool}` — available for rest of session
- Prevents duplicate connections
- Returns list of newly available tools
- 30-second timeout on MCP requests

**Configuration** (`~/.synaps-cli/mcp.json`):
```json
{
  "mcpServers": {
    "server-name": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-name"],
      "env": { "API_KEY": "value" }
    }
  }
}
```

---

### `load_skill`

Load behavioral guidelines from a markdown file.

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `skill` | string | yes | — | Skill name (matches filename without `.md`) |

**Behavior:**
- Returns skill content to follow for rest of conversation
- Resolution order: project-local `.synaps-cli/skills/` → global `~/.synaps-cli/skills/`
- Skills use YAML frontmatter for metadata:
  ```markdown
  ---
  name: code-review
  description: Structured code review guidelines
  ---
  # Code Review
  (content here)
  ```

---

### `watcher_exit`

Signal work completion and provide handoff state. **Only available to watcher agents.**

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `reason` | string | yes | — | Why you're exiting |
| `summary` | string | yes | — | What you accomplished |
| `pending` | array[string] | no | `[]` | Tasks still pending |
| `context` | object | no | `{}` | Structured data for next session |

**Behavior:**
- Writes `handoff.json` to agent directory
- Triggers graceful shutdown
- Handoff data injected into next session's boot message

---

## Watcher Agent Lifecycle

If you are a watcher agent (`synaps-agent`), you operate in a supervised loop.

### Boot

1. System prompt loaded from `soul.md` in your agent directory
2. Handoff state injected from previous session's `handoff.json` (if exists)
3. Boot message sent with template variables:
   - `{timestamp}` — current time
   - `{handoff}` — JSON from last session
   - `{trigger_context}` — what triggered this session

### Work

Standard conversation loop with full tool access. The supervisor automatically:
- Sends heartbeat every 30s (configurable)
- Checks limits after each turn:
  - Token count (default: 100K)
  - Session duration (default: 60 min)
  - Per-session cost (default: $0.50)
  - Daily cost budget (default: $10.00)
  - Tool call count (default: 200)

### Exit

**Voluntary:** Call `watcher_exit` with handoff state when done.

**Forced:** When a limit is hit, you're prompted to write handoff state before shutdown:
```
You've reached your [limit] limit. Please call watcher_exit with
your handoff state to transfer your work to your next session.
```

### Configuration

`~/.synaps-cli/watcher/{name}/config.toml`:

```toml
[agent]
name = "scout"
model = "claude-sonnet-4-20250514"   # default
thinking = "medium"                   # low | medium | high
trigger = "manual"                    # manual | always | watch

[trigger]                             # watch mode only
paths = ["./src"]                     # directories to monitor
patterns = ["*.rs", "*.toml"]         # file filters
debounce_secs = 3                     # settle time before triggering

[limits]
max_session_tokens = 100000
max_session_duration_mins = 60
max_session_cost_usd = 0.50
max_daily_cost_usd = 10.0
max_tool_calls = 200
cooldown_secs = 10                    # delay between sessions
max_retries = 3                       # crash retries before giving up

[boot]
message = "..."                       # custom boot template (optional)

[heartbeat]
interval_secs = 30
stale_threshold_secs = 120            # when considered stale
```

### Trigger Modes

| Mode | Behavior |
|------|----------|
| `manual` | Only runs when explicitly deployed |
| `always` | Restarts automatically after each session (with cooldown) |
| `watch` | Triggers on file changes in configured paths/patterns |

### Statistics

Per-agent stats tracked in `stats.json`:
- Total sessions, tokens, cost, uptime
- Crash count and last crash error
- Daily usage (resets at midnight)

---

## Subagent Protocol

If you were spawned via the `subagent` tool:

- You run in an **isolated thread** with your own tokio runtime
- You have: `bash`, `read`, `write`, `edit`, `grep`, `find`, `ls`
- You do **NOT** have: `subagent`, `mcp_connect`, `load_skill`, `watcher_exit`
- Your output streams back to the parent agent in real-time
- On timeout, partial results are returned (not an error)
- Your session is logged to `~/.synaps-cli/logs/subagents/`

---

## Directory Structure

```
~/.synaps-cli/
├── config                  # Global settings (model, thinking, theme)
├── auth.json               # OAuth tokens (file-locked, permissions 600)
├── system.md               # Default system prompt
├── mcp.json                # MCP server configurations
├── agents/                 # Subagent definitions
│   └── {name}.md           # Markdown with optional YAML frontmatter
├── skills/                 # Global behavioral guidelines
│   └── {skill}.md          # Markdown with YAML frontmatter
├── sessions/               # Conversation history (JSON)
├── logs/
│   └── subagents/          # Per-subagent session logs
└── watcher/                # Autonomous agents
    └── {name}/
        ├── config.toml     # Agent configuration
        ├── soul.md         # Agent system prompt
        ├── handoff.json    # State from last session
        ├── stats.json      # Cumulative usage statistics
        ├── heartbeat       # Timestamp file
        └── logs/           # Per-session JSONL logs
```

---

## Best Practices

**Tool usage:**
- Use `edit` for surgical changes, `write` for complete rewrites
- Use `bash` for system operations — respect the timeout
- Use `subagent` to delegate focused subtasks
- Always call `watcher_exit` when done (watcher agents)

**Error handling:**
- File operations may fail (permissions, missing paths)
- Shell commands may timeout or return non-zero exit
- MCP servers may be unavailable
- Subagents may timeout and return partial work

**State management (watcher agents):**
- Keep `handoff.json` concise for reliable state transfer
- Use structured data in the `context` field
- Write meaningful summaries for continuity across sessions

---

## Tool Schema Summary

| Tool | Required | Optional | Purpose |
|------|----------|----------|---------|
| `bash` | command | timeout | Shell execution |
| `read` | path | offset, limit | File reading |
| `write` | path, content | — | File creation |
| `edit` | path, old_string, new_string | — | Surgical editing |
| `grep` | pattern | path, include, context | Regex search |
| `find` | pattern | path, type | File discovery |
| `ls` | — | path | Directory listing |
| `subagent` | task | agent, system_prompt, model, timeout | Agent dispatch |
| `mcp_connect` | server | — | MCP server connection |
| `load_skill` | skill | — | Behavioral guidelines |
| `watcher_exit`* | reason, summary | pending, context | Session handoff |

*Watcher agents only
