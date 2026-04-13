# SynapsCLI Agent Reference

This document is for AI agents operating within the SynapsCLI runtime system. If you are reading this, you are an agent that has been spawned by SynapsCLI and need to understand your operating environment.

## System Overview

SynapsCLI is an agent runtime implemented in Rust with three operational modes:

1. **Interactive Mode** (`chatui`) — Human-agent chat sessions with tool access
2. **Headless Worker Mode** (`synaps-agent`) — Autonomous agents running one-shot tasks
3. **Sentinel Mode** (`sentinel`) — Supervised autonomous agents with state persistence

You are operating in one of these environments with access to a standardized tool suite. Your responses are handled by an LLM (typically Claude) via the Anthropic API.

### Runtime Architecture

- **Tool Registry**: Dynamically managed set of available tools
- **Thread Isolation**: Subagents run in separate threads with isolated tool access  
- **MCP Integration**: External tools can be loaded from Model Context Protocol servers
- **Skills System**: Markdown-based behavioral guidelines loaded on demand

## Tool Reference

All agents have access to the following built-in tools:

### Core File Operations

#### `bash`
Execute shell commands with timeout control.

**Parameters:**
- `command` (required): The bash command to execute
- `timeout`: Timeout in seconds (default: 30, max: 300)

**Behavior:**
- Commands are executed in a bash shell with piped stdout/stderr
- Output is stripped of ANSI escape sequences
- Output truncated at 30KB with notification
- Automatic process cleanup on timeout
- Returns error on non-zero exit status

#### `read`
Read file contents with line numbers and pagination.

**Parameters:**
- `path` (required): Path to the file to read
- `offset`: Line number to start reading from (0-indexed, default: 0)
- `limit`: Maximum number of lines to read (default: 500)

**Behavior:**
- Path expansion: `~` resolves to home directory
- Returns numbered lines: `{line_number}\t{content}`
- Shows truncation notice if more lines available
- Error on file not found or permission denied

#### `write`
Create or overwrite files with atomic operations.

**Parameters:**
- `path` (required): Path to the file to write
- `content` (required): Content to write to the file

**Behavior:**
- Creates parent directories if needed
- Atomic write via temp file + rename
- Returns line count and byte count
- Overwrites existing files completely

#### `edit`
Make surgical edits by exact string replacement.

**Parameters:**
- `path` (required): Path to the file to edit
- `old_string` (required): The exact text to find and replace. Must match exactly once in the file.
- `new_string` (required): The replacement text

**Behavior:**
- String must match exactly once (error on 0 or >1 matches)
- Atomic operation via temp file
- Preserves file structure around the edit
- Include sufficient context to make match unique

#### `grep`
Search file contents using regex patterns.

**Parameters:**
- `pattern` (required): Regex pattern to search for
- `path`: File or directory to search in (default: current directory)
- `include`: Glob pattern to filter files (e.g. "*.rs", "*.py")
- `context`: Number of context lines to show before and after each match

**Behavior:**
- Recursive search with standard exclusions (.git, node_modules, target)
- 15-second timeout with error on timeout
- Output truncated at 50KB with notice
- Returns "No matches found." if no results

#### `find`
Find files by name using glob patterns.

**Parameters:**
- `pattern` (required): Glob pattern to match file names (e.g. "*.rs", "Cargo.*")
- `path`: Directory to search in (default: current directory)
- `type`: Filter by type: "f" for files, "d" for directories

**Behavior:**
- Recursive search with standard exclusions
- 10-second timeout
- Returns relative paths from search root

#### `ls`
List directory contents with details.

**Parameters:**
- `path`: Directory path to list (default: current directory)

**Behavior:**
- Uses `ls -lah` format (permissions, size, modification date)
- Returns "Directory is empty." for empty directories
- Error on invalid path or permission denied

### Agent Operations

#### `subagent`
Dispatch a specialized subagent to perform a focused task.

**Parameters:**
- `task` (required): The task/prompt to send to the subagent
- `agent`: Agent name — resolves to ~/.synaps-cli/agents/<name>.md. Mutually exclusive with system_prompt.
- `system_prompt`: Inline system prompt for the subagent. Use when you don't have a named agent file.
- `model`: Model override (default: claude-sonnet-4-20250514). Use claude-opus-4-6 for complex tasks.
- `timeout`: Timeout in seconds (default: 300)

**Behavior:**
- Spawns agent in separate thread with isolated runtime
- Has access to core tools (bash, read, write, edit, grep, find, ls) — no recursive subagents
- Returns partial results on timeout instead of error
- Streams status updates during execution
- Logs complete session to ~/.synaps-cli/logs/subagents/

**Agent Resolution:**
- Named agent: Loads `~/.synaps-cli/agents/{agent}.md`, strips YAML frontmatter
- Inline prompt: Uses provided system_prompt directly
- Must provide either `agent` OR `system_prompt`

### Gateway Tools

#### `mcp_connect`
Connect to an external MCP (Model Context Protocol) server and load its tools.

**Parameters:**
- `server` (required): Name of the MCP server to connect to. Available servers are listed in the tool description.

**Behavior:**
- Loads server configuration from ~/.synaps-cli/mcp.json
- Spawns server process and establishes JSON-RPC connection
- Dynamically registers all server tools with `mcp__{server}__{tool}` prefix
- Tools become available for remainder of session
- Returns list of newly available tools
- Prevents duplicate connections to same server

**Configuration Format** (mcp.json):
```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/files"],
      "env": {}
    }
  }
}
```

#### `load_skill`
Load behavioral guidelines from a skill file.

**Parameters:**
- `skill` (required): Name of the skill to load. Available skills listed in tool description.

**Behavior:**
- Loads skill from ~/.synaps-cli/skills/{skill}.md
- Returns skill content to be followed for remainder of conversation
- Skills include structured guidelines, checklists, best practices
- Project-local skills (.synaps-cli/skills/) override global ones

**Skill Format** (markdown with YAML frontmatter):
```markdown
---
name: rust
description: Rust programming best practices
---

# Rust Development Guidelines

- Use `cargo clippy` for linting
- Write tests for all public interfaces
- Handle errors with Result<T,E>
```

### Sentinel-Only Tools

#### `sentinel_exit`
Signal completion and provide handoff state for next session. **Only available to Sentinel agents.**

**Parameters:**
- `reason` (required): Why you're exiting
- `summary` (required): What you accomplished this session
- `pending`: Array of tasks still pending
- `context`: Structured data for next session

**Behavior:**
- Writes handoff.json to agent directory
- Triggers graceful shutdown of sentinel agent
- Handoff data is loaded in next boot message
- File size should be kept under 50KB

## Sentinel Agent Lifecycle

If you are a Sentinel agent, you operate in a supervised autonomous loop:

### Configuration

Your behavior is controlled by `config.toml` in your agent directory:

```toml
[agent]
name = "your-name"
model = "claude-sonnet-4-20250514"  # or claude-opus-4-6
thinking = "medium"  # low, medium, high
trigger = "manual"   # manual, always

[limits]
max_session_tokens = 100000     # default: 100K
max_session_duration_mins = 60  # default: 60 minutes  
max_session_cost_usd = 0.50     # default: $0.50
max_daily_cost_usd = 10.0       # default: $10
max_tool_calls = 200            # default: 200
cooldown_secs = 10              # delay between sessions
max_retries = 3                 # attempts before marking crashed

[boot]  
message = "..."  # custom boot template (optional)

[heartbeat]
interval_secs = 30              # heartbeat frequency
stale_threshold_secs = 120      # when considered stale
```

### Boot Phase

1. Your system prompt is loaded from `soul.md`
2. Handoff state injected from previous session (if exists)
3. Boot message sent with template variables:
   - `{timestamp}`: Current time
   - `{handoff}`: JSON handoff from last session
   - `{trigger_context}`: What triggered this session

### Work Phase

- You operate in standard LLM conversation loop
- All tools available per your runtime context
- Heartbeat automatically sent every 30s (no action needed)
- Limits checked after each conversation turn:
  - Token count (input + output)
  - Session duration
  - Tool call count  
  - Per-session cost
  - Daily cost budget

### Exit Phase

**Option 1: Voluntary Exit**
Call `sentinel_exit` with your handoff state when done.

**Option 2: Limit Reached**
System prompts you to write handoff state before forced shutdown:
```
You've reached your [limit type] limit. Please call sentinel_exit with your handoff state to transfer your work to your next session.
```

### Handoff State

Structure for state transfer between sessions:

```json
{
  "summary": "What I accomplished",
  "pending": ["Task 1", "Task 2"], 
  "context": {
    "key": "structured data",
    "progress": {"task_a": "50%"}
  }
}
```

### Statistics Tracking

Per-agent stats tracked automatically:
- Total sessions run
- Total tokens consumed
- Total cost incurred  
- Uptime across sessions
- Crash count and last error
- Daily usage (resets at midnight)

## Directory Structure

Your runtime operates within this file system structure:

```
~/.synaps-cli/
├── config                  # global settings (model, thinking, skills)
├── agents/                 # subagent definitions (.md files)
├── skills/                 # loadable skill files (.md with frontmatter)
├── mcp.json               # MCP server configurations
├── sessions/              # conversation history
└── sentinel/              # autonomous agents
    └── <name>/
        ├── config.toml    # agent configuration
        ├── soul.md        # agent system prompt
        ├── handoff.json   # state from last session
        ├── stats.json     # usage statistics
        ├── heartbeat      # heartbeat timestamp file
        └── logs/          # per-session logs (JSONL format)
            └── session-001.jsonl
```

## Subagent Protocol

If you are a subagent (spawned via `subagent` tool):

- You run in an isolated thread with your own tokio runtime
- You receive: system prompt + user task + core tool suite
- You have NO access to:
  - `subagent` tool (prevents recursion)
  - MCP servers
  - Sentinel-specific tools
- Your output streams back to parent agent
- You're terminated on timeout with partial results returned
- Parent receives your final response prefixed with `[subagent:{name}]`

## Best Practices

### Tool Usage
- Use `bash` for system operations, but respect timeout limits
- Use `edit` for surgical changes; `write` for complete rewrites
- Use `subagent` for delegation of focused subtasks
- Always call `sentinel_exit` when done (Sentinel agents)

### Error Handling  
- File operations may fail due to permissions or missing paths
- Shell commands may timeout or return non-zero exit codes
- MCP servers may be unavailable or crash
- Subagents may timeout and return partial work

### State Management
- Keep handoff.json under 50KB for reliable state transfer
- Use structured data in context field for complex state
- Write meaningful summaries for continuity across sessions

### Resource Limits
- Default session limit: 100K tokens, 60 minutes, $0.50, 200 tool calls
- Daily limit: $10 by default
- Heartbeat automatic every 30s — no action needed
- Graceful degradation on limit hits with handoff prompting

## Tool Schema Summary

| Tool | Required Params | Optional Params | Purpose |
|------|----------------|------------------|---------|
| bash | command | timeout | Execute shell commands |
| read | path | offset, limit | Read file contents |
| write | path, content | | Create/overwrite files |
| edit | path, old_string, new_string | | Make surgical edits |
| grep | pattern | path, include, context | Search file contents |
| find | pattern | path, type | Find files by name |
| ls | | path | List directory contents |
| subagent | task | agent, system_prompt, model, timeout | Dispatch specialist agent |
| mcp_connect | server | | Connect to MCP server |
| load_skill | skill | | Load behavioral guidelines |
| sentinel_exit* | reason, summary | pending, context | Exit with handoff state |

*Sentinel agents only

---

*You are now fully briefed on your operational environment. Utilize the available tools effectively and follow the protocols appropriate to your agent type.*