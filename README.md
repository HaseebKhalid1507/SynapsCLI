# SynapsCLI

A minimal, terminal-native AI agent runtime built in Rust. It connects to the Anthropic API, streams responses with extended thinking, executes tools in an autonomous loop, and presents everything through a polished TUI — or serves multiple clients over WebSocket.

## Features

- **Server/client architecture** — One server holds the runtime and session; multiple clients connect via WebSocket and share a persistent conversation
- **Streaming SSE** — Real-time token streaming with thinking block display
- **Tool use loop** — Autonomous multi-step tool execution with cancellation support
- **7 built-in tools** — bash, read, write, edit, grep, find, ls
- **TUI interface** — Full terminal UI with markdown rendering, syntax highlighting, and animations
- **CLI client** — Lightweight WebSocket client with ANSI-colored streaming output
- **Session persistence** — Auto-saved sessions with `--continue` to resume any conversation
- **Extended thinking** — Configurable thinking budgets (low/medium/high/xhigh or custom token count)
- **Cost tracking** — Per-model pricing with session totals
- **Prefix commands** — Type `/q` instead of `/quit`, unambiguous prefixes resolve automatically
- **Tab completion** — Tab-complete slash commands with longest common prefix matching

## Quick Start

### Prerequisites

- Rust toolchain (1.70+)
- An Anthropic API key

### Setup

```bash
# Clone and build
git clone <repo-url>
cd SynapsCLI
cargo build --release

# Set your API key
export ANTHROPIC_API_KEY="sk-ant-..."

# Or create a config
mkdir -p ~/.synaps-cli
echo "model = claude-opus-4-6" > ~/.synaps-cli/config
echo "thinking = medium" >> ~/.synaps-cli/config
```

### Run

```bash
# ── Server/Client mode (recommended) ────────────────────────

# Start the server (holds runtime + session, listens for WebSocket clients)
cargo run --bin server
cargo run --bin server -- --port 3145 --system ./prompts/agent.md
cargo run --bin server -- --continue                # resume latest session
cargo run --bin server -- --continue 20260410-1430  # resume specific session

# Connect a client (multiple clients can connect simultaneously)
cargo run --bin client
cargo run --bin client -- --url ws://localhost:3145/ws

# ── Standalone TUI mode ─────────────────────────────────────

# TUI mode (self-contained, no server needed)
cargo run --bin chatui
cargo run --bin chatui -- --system "You are a Rust expert."
cargo run --bin chatui -- -s ./prompts/coding.md
cargo run --bin chatui -- --continue

# ── Other modes ─────────────────────────────────────────────

# Plain streaming chat (no TUI)
cargo run --bin chat

# Single prompt (non-streaming)
cargo run --bin synaps-cli run "explain quicksort"
```

## Architecture

```
                    ┌──────────┐  ┌──────────┐  ┌──────────┐
                    │ client   │  │ client   │  │ future:  │
                    │ (CLI)    │  │ (CLI)    │  │ chatui   │
                    └────┬─────┘  └────┬─────┘  └────┬─────┘
                         │             │             │
                         └─────────────┴──────┬──────┘
                                              │ WebSocket
                                        ┌─────┴─────┐
                                        │  server    │
                                        │            │
                                        │  Runtime   │
                                        │  Session   │
                                        │  Tools     │
                                        │  Broadcast │
                                        └────────────┘

src/
├── lib.rs          # Module exports
├── runtime.rs      # Core runtime: API calls, SSE parsing, tool loop, auth
├── tools.rs        # Tool registry and 7 tool implementations
├── session.rs      # Session persistence: save, load, list, find
├── protocol.rs     # Shared client/server message types
├── server.rs       # WebSocket server (axum): broadcast, commands, tool exec
├── client.rs       # CLI WebSocket client with ANSI streaming
├── chatui.rs       # Standalone TUI binary (ratatui + crossterm)
├── chat.rs         # Plain text streaming chat binary
├── error.rs        # Error types
└── main.rs         # CLI binary with run subcommand
```

### Server (`server.rs`)

The server is a long-running daemon that owns the `Runtime` and session state. Clients connect via WebSocket and share one persistent conversation. All stream events are broadcast to every connected client.

```bash
# Start with defaults (port 3145, new session)
cargo run --bin server

# Custom port + system prompt + resume session
cargo run --bin server -- -p 8080 -s ./system.md --continue
```

**Features:**
- **Broadcast architecture** — When any client sends a message, all connected clients see the thinking, text, tool calls, and results in real time
- **Shared session** — One conversation persisted to disk, accessible from any client
- **Slash commands** — `/model`, `/thinking`, `/clear`, `/system` — all sent from any client, affect the shared state
- **Health endpoint** — `GET /health` returns `ok`
- **Concurrent safety** — Multiple clients can connect/disconnect freely; the server handles locking

### Client (`client.rs`)

A lightweight CLI client that connects to the server via WebSocket. Renders streaming responses with ANSI colors.

```bash
cargo run --bin client                               # connect to localhost:3145
cargo run --bin client -- --url ws://remote:3145/ws   # connect to remote server
```

**On connect:**
- Requests conversation history (displays a summary of past messages)
- Requests server status (model, thinking level, tokens, cost, client count)

**Commands:**
| Command | Description |
|---------|-------------|
| `/quit`, `/exit`, `/q` | Disconnect |
| `/cancel`, `/c` | Cancel current streaming response |
| `/status`, `/s` | Show server status |
| `/history`, `/h` | Show conversation history |
| `/model [name]` | Show or set model (server-side) |
| `/thinking [level]` | Set thinking budget (server-side) |
| `/clear` | Clear session (server-side) |
| `/system <prompt>` | Set system prompt (server-side) |

### Protocol (`protocol.rs`)

Shared message types for client↔server communication over WebSocket:

```rust
// Client → Server
ClientMessage::Message { content }        // Send a user message
ClientMessage::Command { name, args }     // Execute a slash command
ClientMessage::Cancel                     // Cancel streaming
ClientMessage::Status                     // Request server status
ClientMessage::History                    // Request conversation history

// Server → Client
ServerMessage::Thinking { content }       // Thinking tokens (streamed)
ServerMessage::Text { content }           // Text tokens (streamed)
ServerMessage::ToolUse { tool_name, .. }  // Tool invocation
ServerMessage::ToolResult { result, .. }  // Tool execution result
ServerMessage::Usage { .. }              // Token counts
ServerMessage::Done                       // Streaming complete
ServerMessage::Error { message }          // Error
ServerMessage::System { message }         // Info/command response
ServerMessage::StatusResponse { .. }      // Server state
ServerMessage::HistoryResponse { .. }     // Conversation history
```

All messages are JSON-serialized. The server→client messages map directly to the internal `StreamEvent` enum.

### Runtime (`runtime.rs`)

The `Runtime` struct is the core engine. It handles:

- **Authentication** — Reads `ANTHROPIC_API_KEY` from the environment (or OAuth from `~/.pi/agent/auth.json`)
- **SSE streaming** — Parses `content_block_start`, `content_block_delta`, `content_block_stop` events, accumulating thinking blocks (with signatures), text, and tool use blocks
- **Tool loop** — When the model returns `tool_use` blocks, executes each tool, sends results back, and continues until the model responds with text only. Updated message history is sent after each iteration so the UI stays in sync.
- **Cancellation** — Accepts a `CancellationToken` that can abort between API calls, between tool executions, or mid-tool via `tokio::select!`. Partial message history is preserved on cancellation.

Public API:

```rust
let runtime = Runtime::new().await?;
runtime.set_model("claude-opus-4-6".to_string());
runtime.set_thinking_budget(16384);
runtime.set_system_prompt("You are a helpful agent.".to_string());

// Streaming with cancellation
let cancel = CancellationToken::new();
let mut stream = runtime.run_stream_with_messages(messages, cancel.clone());
while let Some(event) = stream.next().await {
    match event {
        StreamEvent::Text(t) => print!("{}", t),
        StreamEvent::Thinking(t) => { /* thinking tokens */ },
        StreamEvent::ToolUse { tool_name, tool_id, input } => { /* tool called */ },
        StreamEvent::ToolResult { tool_id, result } => { /* tool finished */ },
        StreamEvent::MessageHistory(msgs) => { /* updated conversation */ },
        StreamEvent::Usage { input_tokens, output_tokens } => { /* token counts */ },
        StreamEvent::Done => break,
        StreamEvent::Error(e) => eprintln!("{}", e),
    }
}

// Non-streaming (blocks until complete, runs tool loop internally)
let response = runtime.run_single("list files in src/").await?;
```

#### SSE Event Handling

The streaming parser processes these SSE event types:

| Event Type | Description |
|------------|-------------|
| `content_block_start` | Marks beginning of a thinking, text, or tool_use block |
| `content_block_delta` | Incremental content: `text_delta`, `thinking_delta`, `signature_delta`, `input_json_delta` |
| `content_block_stop` | Finalizes the block, flushes accumulated content |
| `message_start` | Message envelope with initial usage data |
| `message_delta` | Token usage updates |
| `message_stop` | Final message marker |

Thinking blocks accumulate both the thinking text and a `signature` field across separate deltas. Both are stored and echoed back in tool loop continuations.

API requests use `max_tokens: 128000` and `thinking.display: "summarized"`.

### Tools (`tools.rs`)

Seven tools are registered by default:

| Tool | Description |
|------|-------------|
| **bash** | Execute shell commands via `/bin/bash -c`. Configurable `timeout` (default 30s, max 300s). Captures stdout + stderr. |
| **read** | Read file contents with line numbers. Supports `offset` (starting line, 0-indexed) and `limit` (max lines). Reports remaining lines when truncated. |
| **write** | Create or overwrite files. Atomic writes via temp file (`.agent-tmp`) + rename. Auto-creates parent directories. Reports line and byte counts. |
| **edit** | Surgical find-and-replace. `old_string` must match exactly once (errors on zero or multiple matches). Atomic write. |
| **grep** | Regex search via `grep -rn`. Supports `include` glob filter (e.g. `*.rs`), `context` lines. Excludes `.git`/`node_modules`/`target`. 15s timeout, 50KB output cap. |
| **find** | Glob-based file search via `find`. Supports `type` filter (`f` for files, `d` for directories). Excludes noise directories. 10s timeout. |
| **ls** | Directory listing via `ls -lah`. Shows permissions, size, and dates. |

All tools expand `~` to `$HOME`. Tool results are streamed back as they complete.

<details>
<summary>Full parameter reference</summary>

**bash**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `command` | string | yes | Shell command to execute |
| `timeout` | integer | no | Timeout in seconds (default: 30, max: 300) |

**read**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | File path to read |
| `offset` | integer | no | Line number to start from (0-indexed) |
| `limit` | integer | no | Maximum number of lines to read |

**write**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | File path to write |
| `content` | string | yes | File content |

**edit**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | File path to edit |
| `old_string` | string | yes | Exact text to find (must match once) |
| `new_string` | string | yes | Replacement text |

**grep**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `pattern` | string | yes | Regex pattern |
| `path` | string | no | Search directory (default: `.`) |
| `include` | string | no | Glob filter (e.g. `*.rs`) |
| `context` | integer | no | Context lines before and after matches |

**find**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `pattern` | string | yes | Glob pattern to match |
| `path` | string | no | Search directory (default: `.`) |
| `type` | string | no | `f` for files, `d` for directories |

**ls**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | no | Directory to list (default: `.`) |

</details>

### Sessions (`session.rs`)

Every conversation is automatically saved to `~/.synaps-cli/sessions/`. Session files are JSON containing the full API message history, model settings, token counts, and cost. Empty sessions (no user messages) are not saved.

Session ID format: `YYYYMMDD-HHMMSS-XXXX` (4-character UUID suffix).

Sessions are automatically titled from the first 80 characters of the first user message.

```bash
# Continue the most recent session (no ID needed)
cargo run --bin chatui -- --continue
cargo run --bin server -- --continue

# Continue a specific session (partial ID match)
cargo run --bin chatui -- --continue 20260410-1430
cargo run --bin server -- --continue 20260410-1430
```

Session functions:

- `Session::new()` — Create with current model/thinking/system prompt
- `Session::save()` / `Session::load()` — Persist to / read from disk
- `Session::auto_title()` — Set title from first user message
- `list_sessions()` — All sessions sorted by last updated
- `latest_session()` — Most recently active session
- `find_session("partial_id")` — Exact match first, then substring match. Errors on ambiguous matches.

### TUI (`chatui.rs`)

Standalone terminal interface built with ratatui, crossterm, tachyonfx, and syntect. Owns its own `Runtime` — no server needed.

**Performance:** Syntax highlighting sets are loaded once via `LazyLock`. Rendered lines are cached and only rebuilt when messages change (not on every keypress or scroll).

**Keyboard shortcuts:**

| Key | Action |
|-----|--------|
| `Enter` | Send message |
| `Esc` | Abort streaming (cancels tool execution too) |
| `Ctrl+C` | Quit (with dissolve animation) |
| `Ctrl+A` / `Home` | Move cursor to start |
| `Ctrl+E` / `End` | Move cursor to end |
| `Ctrl+W` | Delete word backward |
| `Ctrl+U` | Delete to start of line |
| `Alt+Left` / `Alt+Right` | Move cursor by word |
| `Alt+Backspace` | Delete word backward |
| `Up` / `Down` | Input history navigation |
| `Shift+Up` / `Shift+Down` | Scroll message history |
| `Mouse wheel` | Scroll messages (3 lines per tick) |
| `Tab` | Autocomplete slash commands (longest common prefix) |

**Slash commands:**

Commands support unambiguous prefix matching — `/q` resolves to `/quit`, `/s` is ambiguous (sessions/system) so it won't resolve, but `/se` resolves to `/sessions`.

| Command | Description |
|---------|-------------|
| `/clear` | Save current session and start a new one |
| `/model [name]` | Show or set model |
| `/system <prompt\|show\|save>` | Manage system prompt |
| `/thinking [low\|medium\|high\|xhigh]` | Set thinking budget |
| `/sessions` | List saved sessions (up to 20, with active marker) |
| `/resume <id>` | Save current session and switch to another |
| `/help` | Show available commands |
| `/quit` | Exit |
| `/exit` | Exit (alias for `/quit`) |

**Rendering:**

- Markdown: headings, bold, italic, inline code, blockquotes, ordered/unordered lists
- Code blocks: syntax highlighted via syntect (base16-ocean.dark theme)
- Tool calls: per-tool icons (❯ bash, ▸ read, ◂ write, Δ edit, ⌕ grep, ○ find, ≡ ls)
- Tool results: compact "└─ ok (N lines)" summary with indented output, errors in red
- Thinking: dimmed with │ left border, truncated to 6 lines
- Animations: fade-in on boot (300ms, QuadOut), dissolve on exit (800ms, QuadIn) via tachyonfx

**Theme:**

Dark base (RGB 12,14,18) with teal accents (RGB 80,200,160). User messages have a subtle background highlight. The color palette is designed for high contrast on dark terminals.

## Configuration

### Config file

`~/.synaps-cli/config` — key=value format, supports `#` comments:

```
# Model selection
model = claude-opus-4-6

# Thinking budget (named level or raw token count)
thinking = xhigh
# thinking = 8192
```

**Thinking levels:**

| Level | Budget tokens |
|-------|--------------|
| `low` | 2,048 |
| `medium` | 4,096 |
| `high` | 16,384 |
| `xhigh` | 32,768 |
| `<number>` | Custom token count |

### System prompt

Loaded in priority order:

1. `--system` / `-s` CLI flag (string or file path — auto-detected)
2. `~/.synaps-cli/system.md` file
3. Built-in default

Can also be changed at runtime with `/system` (chatui) or `/system` command (client → server).

Default (when no flag or file exists):
> You are a helpful AI agent running in a terminal. You have access to bash, read, and write tools. Be concise and direct. Use tools when the user asks you to interact with the filesystem or run commands.

### Authentication

Set `ANTHROPIC_API_KEY` in your environment. The runtime sends it via the `x-api-key` header with `anthropic-version: 2023-06-01`.

Also supports OAuth tokens from `~/.pi/agent/auth.json` (auto-detected).

## Cost Tracking

Session cost is calculated per API call using current Anthropic pricing:

| Model | Input (per MTok) | Output (per MTok) |
|-------|-----------------|-------------------|
| Opus | $15.00 | $75.00 |
| Sonnet | $3.00 | $15.00 |
| Haiku | $0.80 | $4.00 |

Model matching is substring-based (e.g. any model ID containing "opus" uses Opus pricing). Unknown models default to Sonnet pricing.

The running total is displayed in the TUI footer, client status response, and persisted with each session.

## Dependencies

| Crate | Purpose |
|-------|---------|
| tokio | Async runtime |
| reqwest | HTTP client with streaming |
| serde / serde_json | Serialization |
| clap | CLI argument parsing |
| axum | HTTP/WebSocket server |
| tokio-tungstenite | WebSocket client |
| tower / tower-http | Server middleware |
| ratatui | Terminal UI framework |
| crossterm | Terminal backend + input events |
| tachyonfx | Terminal animations (fade-in, dissolve) |
| syntect | Syntax highlighting for code blocks |
| chrono | Timestamps |
| uuid | Session ID generation |
| tokio-util | CancellationToken for streaming abort |
| futures-util | Stream combinators for WebSocket |
| thiserror | Error derive macros |

## License

MIT
