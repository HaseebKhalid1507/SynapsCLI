# agent-runtime

A minimal, terminal-native AI agent runtime built in Rust. It connects to the Anthropic API, streams responses with extended thinking, executes tools in an autonomous loop, and presents everything through a polished TUI.

## Features

- **Streaming SSE** — Real-time token streaming with thinking block display
- **Tool use loop** — Autonomous multi-step tool execution with cancellation support
- **7 built-in tools** — bash, read, write, edit, grep, find, ls
- **TUI interface** — Full terminal UI with markdown rendering, syntax highlighting, and animations
- **Session persistence** — Auto-saved sessions with `--continue` to resume any conversation
- **Extended thinking** — Configurable thinking budgets (low/medium/high/xhigh) with summarized display
- **Cost tracking** — Per-model pricing with session totals shown in the footer
- **Dual auth** — OAuth and API key authentication

## Quick Start

### Prerequisites

- Rust toolchain (1.70+)
- An Anthropic API key

### Setup

```bash
# Clone and build
git clone <repo-url>
cd agent-runtime
cargo build --release

# Set your API key
export ANTHROPIC_API_KEY="sk-ant-..."

# Or create a config
mkdir -p ~/.agent-runtime
echo "model = claude-sonnet-4-20250514" > ~/.agent-runtime/config
echo "thinking = medium" >> ~/.agent-runtime/config
```

### Run

```bash
# TUI mode (recommended)
cargo run --bin chatui

# Plain streaming chat
cargo run --bin chat

# Single prompt (non-streaming)
cargo run --bin agent-runtime run "explain quicksort"
```

## Architecture

```
src/
├── lib.rs        # Module exports
├── runtime.rs    # Core runtime: API calls, SSE parsing, tool loop, auth
├── tools.rs      # Tool registry and 7 tool implementations
├── session.rs    # Session persistence: save, load, list, find
├── chatui.rs     # TUI binary (ratatui + crossterm)
├── chat.rs       # Plain text streaming chat binary
├── error.rs      # Error types
└── main.rs       # CLI binary with run/chat subcommands
```

### Runtime (`runtime.rs`)

The `Runtime` struct is the core engine. It handles:

- **Authentication** — Reads OAuth tokens from `~/.pi/agent/auth.json`, falls back to `ANTHROPIC_API_KEY`
- **SSE streaming** — Parses `content_block_start`, `content_block_delta`, `content_block_stop` events, accumulating thinking blocks (with signatures), text, and tool use blocks
- **Tool loop** — When the model returns `tool_use` blocks, executes each tool, sends results back, and continues until the model responds with text only
- **Cancellation** — Accepts a `CancellationToken` that can abort between API calls, between tool executions, or mid-tool via `tokio::select!`

Public API:

```rust
let runtime = Runtime::new().await?;
runtime.set_model("claude-sonnet-4-20250514".to_string());
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

### Tools (`tools.rs`)

Seven tools are registered by default:

| Tool | Description |
|------|-------------|
| **bash** | Execute shell commands. Configurable timeout (default 30s, max 300s). Captures stdout + stderr. |
| **read** | Read file contents with line numbers. Supports `offset` and `limit` for partial reads. |
| **write** | Create or overwrite files. Atomic writes via temp file + rename. Auto-creates parent directories. |
| **edit** | Surgical find-and-replace. `old_string` must match exactly once. Atomic write. |
| **grep** | Regex search across files. Supports `include` glob filter, `context` lines. Excludes .git/node_modules/target. |
| **find** | Glob-based file search. Supports `type` filter (f/d). Excludes noise directories. |
| **ls** | Directory listing with permissions, size, and dates (`ls -lah`). |

All tools expand `~` to `$HOME`. Tool results are streamed back to the TUI as they complete.

### Sessions (`session.rs`)

Every conversation is automatically saved to `~/.agent-runtime/sessions/`. Session files are JSON containing the full API message history, model settings, token counts, and cost.

```bash
# Continue the most recent session
cargo run --bin chatui -- --continue

# Continue a specific session (partial ID match)
cargo run --bin chatui -- --continue 20260410-1430
```

Session functions:

- `Session::new()` — Create with current model/thinking/system prompt
- `Session::save()` / `Session::load()` — Persist to / read from disk
- `list_sessions()` — All sessions sorted by last updated
- `latest_session()` — Most recently active session
- `find_session("partial_id")` — Fuzzy match by ID substring

### TUI (`chatui.rs`)

The terminal interface built with ratatui, crossterm, tachyonfx, and syntect.

**Layout:**

```
┌─ agent-runtime │ streaming ──────────────────────────┐
│                                                       │
│  ● You                                        14:32   │
│    show me the project structure                      │
│                                                       │
│  … thinking                                           │
│    analyzing the directory layout...                  │
│                                                       │
│  ● Agent                                      14:32   │
│    Here's the structure:                              │
│    ── rust ──                                         │
│    │ fn main() { ... }                                │
│                                                       │
│  ❯ bash                                               │
│    command │ ls -la src/                               │
│    │ total 48                                         │
│    │ -rw-r--r-- 1 user 1234 lib.rs                    │
│                                                       │
├───────────────────────────────────────────────────────┤
│ ❯ type a message                                      │
├───────────────────────────────────────────────────────┤
│ ctrl+c quit  esc abort    $0.0312 1.2kin 3.4kout  ... │
└───────────────────────────────────────────────────────┘
```

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
| `Up` / `Down` | Input history |
| `Shift+Up` / `Shift+Down` | Scroll messages |
| `Mouse wheel` | Scroll messages (3 lines per tick) |
| `Tab` | Autocomplete slash commands |

**Slash commands:**

| Command | Description |
|---------|-------------|
| `/clear` | Reset conversation |
| `/model [name]` | Show or set model |
| `/system <prompt\|show\|save>` | Manage system prompt |
| `/thinking [low\|medium\|high\|xhigh]` | Set thinking budget |
| `/sessions` | List saved sessions |
| `/resume <id>` | Switch to a different session |
| `/help` | Show available commands |
| `/quit` | Exit |

**Rendering:**

- Markdown: headings, bold, italic, inline code, blockquotes, ordered/unordered lists
- Code blocks: syntax highlighted via syntect (base16-ocean.dark theme)
- Tool calls: per-tool icons (❯ bash, ▷ read, ◁ write, Δ edit, ⌕ grep, ⌂ find, ≡ ls)
- Tool results: smart truncation, error results in red
- Animations: fade-in on boot (300ms), dissolve on exit (800ms) via tachyonfx

## Configuration

### Config file

`~/.agent-runtime/config` — key=value format:

```
model = claude-opus-4-6
thinking = xhigh
```

**Thinking levels:**

| Level | Budget tokens |
|-------|--------------|
| `low` | 2,048 |
| `medium` | 4,096 |
| `high` | 16,384 |
| `xhigh` | 32,768 |

### System prompt

`~/.agent-runtime/system.md` — loaded on startup. Can also be set at runtime with `/system`.

### Authentication

The runtime checks for credentials in order:

1. **OAuth** — `~/.pi/agent/auth.json` (if present with valid access token)
2. **API key** — `ANTHROPIC_API_KEY` environment variable

## Cost Tracking

Session cost is calculated per API call using current Anthropic pricing:

| Model | Input (per MTok) | Output (per MTok) |
|-------|-----------------|-------------------|
| Opus | $15.00 | $75.00 |
| Sonnet | $3.00 | $15.00 |
| Haiku | $0.80 | $4.00 |

The running total is displayed in the footer and persisted with each session.

## Dependencies

| Crate | Purpose |
|-------|---------|
| tokio | Async runtime |
| reqwest | HTTP client with streaming |
| serde / serde_json | Serialization |
| clap | CLI argument parsing |
| ratatui | Terminal UI framework |
| crossterm | Terminal backend + input events |
| tachyonfx | Terminal animations |
| syntect | Syntax highlighting |
| chrono | Timestamps |
| uuid | Session ID generation |
| tokio-util | CancellationToken |
| thiserror | Error derive macros |

## License

MIT
