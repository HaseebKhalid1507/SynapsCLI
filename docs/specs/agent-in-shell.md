# Agent In Shell — Technical Specification

**Branch:** `agent_in_shell`
**Status:** Draft
**Author:** Jawz (S169)

---

## 1. Objective

Add interactive PTY-based shell sessions to SynapsCLI so agents can drive SSH connections, REPLs, debuggers, installers, and any interactive terminal program — the same way a human would.

**Users:** All agents (Jawz + subagents).

**Success Criteria:**
- Agent can start a persistent shell session and interact with it across multiple tool calls
- Agent can drive an SSH session (password prompt → commands → exit)
- Agent can interact with Python/Node REPLs, GDB, and similar interactive tools
- Sessions auto-reap after configurable idle timeout
- No PTY file descriptor leaks under any failure mode
- Existing `bash` tool behavior is unchanged
- All configuration is runtime-adjustable via `~/.synaps-cli/config`

---

## 2. Commands

```bash
cargo build                    # Build
cargo test                     # All tests
cargo test --test shell_pty    # Integration tests only
cargo clippy                   # Lint
```

---

## 3. Tool Schemas

### `shell_start`

Starts a new interactive shell session with a PTY.

```json
{
  "name": "shell_start",
  "description": "Start a new interactive shell session with a PTY. Returns a session ID and the initial output. Use shell_send to interact and shell_end to close.",
  "input_schema": {
    "type": "object",
    "properties": {
      "command": {
        "type": "string",
        "description": "Command to run (default: user's default shell). Examples: 'bash', 'python3', 'ssh user@host'"
      },
      "working_directory": {
        "type": "string",
        "description": "Working directory for the session (default: current directory)"
      },
      "env": {
        "type": "object",
        "description": "Additional environment variables as key-value pairs",
        "additionalProperties": { "type": "string" }
      },
      "rows": {
        "type": "integer",
        "description": "Terminal rows (default: from config, fallback 24)"
      },
      "cols": {
        "type": "integer",
        "description": "Terminal columns (default: from config, fallback 80)"
      },
      "readiness_timeout_ms": {
        "type": "integer",
        "description": "Override output readiness timeout for this session (ms)"
      },
      "idle_timeout": {
        "type": "integer",
        "description": "Override idle timeout for this session (seconds)"
      }
    },
    "required": []
  }
}
```

**Returns:** JSON string:
```json
{
  "session_id": "shell_01",
  "output": "user@host:~$ ",
  "status": "active"
}
```

### `shell_send`

Send input to an active session and return the output.

```json
{
  "name": "shell_send",
  "description": "Send input to an active shell session. Returns the output produced after sending the input. The input is sent exactly as provided — include \\n for Enter.",
  "input_schema": {
    "type": "object",
    "properties": {
      "session_id": {
        "type": "string",
        "description": "Session ID from shell_start"
      },
      "input": {
        "type": "string",
        "description": "Text to send to the shell. Use \\n for Enter, \\x03 for Ctrl-C, \\x04 for Ctrl-D"
      },
      "timeout_ms": {
        "type": "integer",
        "description": "Override readiness timeout for this send (ms)"
      }
    },
    "required": ["session_id", "input"]
  }
}
```

**Returns:** JSON string:
```json
{
  "session_id": "shell_01",
  "output": "file1.rs\nfile2.rs\nuser@host:~$ ",
  "status": "active"
}
```

**Status values:** `"active"`, `"exited"` (process ended), `"timeout"` (readiness timeout hit, output may be partial).

### `shell_end`

Close a session and clean up resources.

```json
{
  "name": "shell_end",
  "description": "Close an interactive shell session and clean up resources. Returns the final output if any.",
  "input_schema": {
    "type": "object",
    "properties": {
      "session_id": {
        "type": "string",
        "description": "Session ID to close"
      }
    },
    "required": ["session_id"]
  }
}
```

**Returns:** JSON string:
```json
{
  "session_id": "shell_01",
  "output": "logout\n",
  "status": "closed"
}
```

---

## 4. Session Manager Architecture

### Shared State Model

The `SessionManager` holds all active sessions and is shared via `Arc` through `ToolContext`:

```rust
// Added to ToolContext:
pub struct ToolContext {
    // ... existing fields ...
    pub session_manager: Option<Arc<SessionManager>>,
}
```

### SessionManager

```rust
pub struct SessionManager {
    sessions: Mutex<HashMap<String, ShellSession>>,
    config: ShellConfig,
    next_id: AtomicU32,
}

impl SessionManager {
    pub fn new(config: ShellConfig) -> Self;
    pub fn create_session(&self, opts: SessionOpts) -> Result<(String, String)>;
    pub fn send_input(&self, id: &str, input: &str, timeout_ms: Option<u64>) -> Result<SendResult>;
    pub fn close_session(&self, id: &str) -> Result<String>;
    pub fn reap_idle(&self) -> Vec<String>;  // returns reaped session IDs
    pub fn shutdown_all(&self);              // runtime shutdown
    pub fn active_count(&self) -> usize;
    pub fn list_sessions(&self) -> Vec<SessionInfo>;
}
```

### ShellSession

```rust
pub struct ShellSession {
    id: String,
    pty_master: Box<dyn MasterPty + Send>,   // portable-pty master
    writer: Box<dyn Write + Send>,            // write end (input to child)
    reader: Option<JoinHandle<()>>,           // async reader task
    output_rx: mpsc::UnboundedReceiver<Vec<u8>>,  // raw PTY output
    child: Box<dyn Child + Send>,             // child process handle
    created_at: Instant,
    last_active: Instant,
    idle_timeout: Duration,
    readiness_timeout: Duration,
    status: SessionStatus,
    accumulated_output: String,              // unread output buffer
    rows: u16,
    cols: u16,
}

enum SessionStatus {
    Active,
    Exited(i32),   // exit code
    Closed,
}
```

### Session ID Generation

Format: `shell_XX` where XX is zero-padded atomic counter. Simple, readable, no UUIDs.

```rust
let id = format!("shell_{:02}", self.next_id.fetch_add(1, Ordering::Relaxed));
```

### Concurrency Model

- `SessionManager` uses `Mutex<HashMap<...>>` — lock to get session, operate, release
- Each `ShellSession` is accessed exclusively (remove from map, operate, reinsert) to avoid holding the lock during I/O
- PTY reader runs in a background `tokio::spawn` task, pushes bytes into an `mpsc` channel
- `shell_send` drains the channel with timeout to collect output

---

## 5. PTY Integration

### Crate: `portable-pty` (v0.9.0)

**Why:**
- Cross-platform (Linux, macOS, Windows)
- Mature, maintained, used by Wezterm (same author)
- Clean API: `PtySystem::default().openpty(PtySize)` → `(master, slave)`
- `CommandBuilder` for process spawning on the slave side
- Resize support via `master.resize(PtySize)`

**Why not alternatives:**
- `pty-process`: simpler but less mature, limited resize support
- `tokio-pty-process`: abandoned (last update 2019)

### Spawn Flow

```rust
let pty_system = portable_pty::native_pty_system();
let pair = pty_system.openpty(PtySize {
    rows, cols,
    pixel_width: 0, pixel_height: 0,
})?;

let mut cmd = CommandBuilder::new(&command);
cmd.cwd(&working_directory);
for (k, v) in &env {
    cmd.env(k, v);
}
// Set TERM so programs know they have a real terminal
cmd.env("TERM", "xterm-256color");

let child = pair.slave.spawn_command(cmd)?;
drop(pair.slave); // close slave side — child owns it now

let writer = pair.master.take_writer()?;
let mut reader = pair.master.try_clone_reader()?;
```

### Async Reader Task

`portable-pty` reader is blocking, so we run it on a blocking thread:

```rust
let (output_tx, output_rx) = mpsc::unbounded_channel::<Vec<u8>>();

let reader_handle = tokio::task::spawn_blocking(move || {
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,  // EOF — process exited
            Ok(n) => {
                let _ = output_tx.send(buf[..n].to_vec());
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
});
```

### Writing Input

```rust
writer.write_all(input.as_bytes())?;
writer.flush()?;
```

### Signal Handling

- **Ctrl-C**: Agent sends `"\x03"` as input text
- **Ctrl-D**: Agent sends `"\x04"` as input text
- **Ctrl-Z**: Agent sends `"\x1a"` as input text
- **Window resize**: `pair.master.resize(PtySize { rows, cols, ... })?`

No special signal API needed — the PTY translates control characters to signals automatically.

---

## 6. Output Readiness Detection

### The Problem

After `shell_send` writes input, how long do we wait for output before returning? Too short = partial output. Too long = agent waits forever.

### Strategy: Configurable Hybrid

```rust
pub enum ReadinessStrategy {
    /// Wait for N ms of output silence
    Timeout { silence_ms: u64 },
    /// Wait until output matches a prompt regex
    Prompt { patterns: Vec<Regex> },
    /// Prompt detection with timeout fallback
    Hybrid { patterns: Vec<Regex>, silence_ms: u64 },
}
```

### Default Prompt Patterns

```rust
vec![
    r"[$#%>»] $",           // common shell prompts
    r"[$#%>»]\s*$",         // with trailing whitespace
    r"\(gdb\)\s*$",         // GDB
    r">>>\s*$",             // Python REPL
    r"\.\.\.\s*$",          // Python continuation
    r"In \[\d+\]:\s*$",    // IPython/Jupyter
    r"irb.*>\s*$",          // Ruby IRB
    r">\s*$",               // Node.js REPL
    r"mysql>\s*$",          // MySQL
    r"postgres[=#]>\s*$",   // PostgreSQL
    r"Password:\s*$",       // Password prompts
    r"\[Y/n\]\s*$",         // Confirmation prompts
    r"\(yes/no.*\)\?\s*$",  // SSH host key prompt
]
```

### Readiness Algorithm

```
1. Write input to PTY
2. Start silence timer (readiness_timeout_ms)
3. Loop:
   a. Drain output_rx with 50ms poll interval
   b. If new output received:
      - Append to accumulated buffer
      - If strategy is Prompt/Hybrid: check if buffer ends with prompt pattern
        - If match → return immediately
      - Reset silence timer
   c. If silence timer expired → return accumulated buffer with status "timeout"
   d. If total wait exceeds max_readiness_timeout (10s default) → return with "timeout"
4. Strip ANSI escape sequences from output before returning
```

### ANSI Handling

PTY output contains ANSI escape sequences (colors, cursor movement, etc.). Strip them before returning to the model:

```rust
// Use the strip-ansi-escapes crate or existing strip_ansi() utility
let clean = strip_ansi(&raw_output);
```

---

## 7. Session Lifecycle

```
 shell_start()           shell_send()            shell_end()
     │                       │                       │
     ▼                       ▼                       ▼
 ┌────────┐  input/output  ┌────────┐   close     ┌────────┐
 │Creating │──────────────►│ Active │────────────►│ Closed │
 └────────┘               └────┬───┘             └────────┘
                               │
                    idle timeout/crash
                               │
                               ▼
                          ┌────────┐
                          │ Reaped │
                          └────────┘
```

### Idle Reaping

A background task runs every 30 seconds:

```rust
tokio::spawn(async move {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    loop {
        interval.tick().await;
        let reaped = session_manager.reap_idle();
        for id in &reaped {
            tracing::info!("Reaped idle shell session: {}", id);
        }
    }
});
```

### Cleanup Guarantees

- **On `shell_end`**: Kill child process (SIGHUP via PTY close), drop PTY master, cancel reader task
- **On idle reap**: Same as shell_end, log warning
- **On runtime shutdown**: `shutdown_all()` — iterate all sessions, close each
- **On SessionManager drop**: `Drop` impl calls `shutdown_all()`
- **On panic**: `kill_on_drop` equivalent via Drop

### Process Exit Detection

The reader task detects EOF (read returns 0 bytes). Set `status = SessionStatus::Exited(code)`. Next `shell_send` call returns `status: "exited"` with any remaining buffered output.

---

## 8. Configuration

In `~/.synaps-cli/config`:

```
# Shell session settings
shell.max_sessions = 5
shell.idle_timeout = 600
shell.readiness_strategy = "hybrid"
shell.readiness_timeout_ms = 300
shell.max_readiness_timeout_ms = 10000
shell.default_rows = 24
shell.default_cols = 80
```

```rust
pub struct ShellConfig {
    pub max_sessions: usize,           // default: 5
    pub idle_timeout: Duration,        // default: 600s (10 min)
    pub readiness_strategy: ReadinessStrategy,  // default: Hybrid
    pub readiness_timeout_ms: u64,     // default: 300
    pub max_readiness_timeout_ms: u64, // default: 10000
    pub default_rows: u16,             // default: 24
    pub default_cols: u16,             // default: 80
}
```

### Config Parsing

Add to `SynapsConfig` in `src/core/config.rs`:

```rust
pub struct SynapsConfig {
    // ... existing fields ...
    pub shell: ShellConfig,
}
```

Parse `shell.*` keys during config load. Unknown keys are preserved (existing behavior).

---

## 9. Security Considerations

### Session Isolation
- Each session has a unique ID — agents can only interact with sessions they started
- Subagents get their own `SessionManager` (via fresh `ToolContext`) — they cannot access parent sessions
- Session IDs are not guessable (atomic counter per-runtime, not global)

### Resource Limits
- `max_sessions` prevents PTY exhaustion (system limit is ~4096 PTYs)
- `idle_timeout` prevents zombie sessions
- `max_readiness_timeout_ms` prevents infinite waits
- Output buffer is capped at `max_tool_output` (existing 30KB limit)

### Logging
- Session creation: log command, session_id, working directory
- Session input: log session_id + input length (NOT input content — may contain passwords)
- Session close: log session_id, duration, exit code
- Idle reap: log session_id with warning

### What We Don't Restrict
- No command blocklist — the agent already has `bash` with full access
- No network restrictions — same trust model as existing tools
- No filesystem sandboxing — same as existing tools

---

## 10. Project Structure

```
src/tools/
├── shell/
│   ├── mod.rs          # Module declarations, re-exports
│   ├── session.rs      # ShellSession struct, SessionManager
│   ├── start.rs        # ShellStartTool
│   ├── send.rs         # ShellSendTool
│   ├── end.rs          # ShellEndTool
│   ├── pty.rs          # PTY abstraction (spawn, read, write, resize)
│   ├── readiness.rs    # Output readiness detection strategies
│   └── config.rs       # ShellConfig parsing
├── mod.rs              # Add shell module declaration + tool registration
├── registry.rs         # Register shell tools
└── bash.rs             # UNCHANGED
```

---

## 11. Testing Strategy

### Unit Tests (`src/tools/shell/` inline)
- Session ID generation
- Config parsing and defaults
- Readiness detection with mock output streams
- Session lifecycle state transitions
- Output buffer management and truncation
- ANSI stripping

### Integration Tests (`tests/shell_pty.rs`)
- Start session → send `echo hello` → verify output → end session
- Start session with `python3` → send `1+1\n` → verify `2` → send `exit()\n`
- Idle timeout reaping
- Max session limit enforcement
- Concurrent sessions (start 3, interact with each, close all)
- Process exit detection (start `cat`, send Ctrl-D, verify `exited` status)
- Ctrl-C handling (start `sleep 999`, send `\x03`, verify interrupted)
- Working directory and env vars
- Session not found error
- Double-close handling (idempotent)

### What We Don't Test
- Full ncurses rendering (too complex, not worth it)
- SSH with real remote hosts (requires infrastructure)
- Windows PTY (Linux-only CI for now)

---

## 12. Boundaries

### Always Do
- Validate session_id on every `shell_send`/`shell_end` call
- Respect configured timeouts
- Clean up PTY resources on Drop
- Strip ANSI from output before returning to model
- Log session lifecycle events
- Cap output buffer at max_tool_output

### Ask First
- Adding dependencies beyond `portable-pty`
- Changing `ToolContext` struct (affects all tools)
- Modifying `ToolRegistry` constructors
- Adding config keys to `SynapsConfig`

### Never Do
- Block the tokio runtime with synchronous PTY reads (use `spawn_blocking`)
- Leak PTY file descriptors
- Log input content (may contain passwords)
- Allow shell sessions to spawn shell sessions (no recursion)
- Break existing `bash` tool behavior

---

## 13. Risks & Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| PTY FD leak on crash | System runs out of PTYs | Drop impl + reaper task + RAII wrapper |
| Blocking tokio runtime | All tools freeze | PTY reader on `spawn_blocking`, never sync read on async thread |
| Readiness detection wrong | Agent gets partial/no output | Configurable strategies, generous defaults, `timeout` status |
| Agent forgets to close session | Resource leak | Idle timeout reaper, runtime shutdown cleanup |
| `portable-pty` upstream issues | Build breaks | Pin version, vendor if needed |
| Output floods memory | OOM | Cap at `max_tool_output`, truncate early |
| Race between send and reaper | Session disappears mid-use | Lock ordering, reaper skips sessions with recent `last_active` |

---

## 14. Dependencies

### New
- `portable-pty = "0.9"` — PTY allocation and management
- `strip-ansi-escapes = "0.2"` — ANSI escape stripping (or reuse existing `strip_ansi`)
- `regex = "1"` — prompt pattern matching (already in Cargo.toml? check)

### Existing (already in Cargo.toml)
- `tokio` — async runtime, `spawn_blocking`, channels, timers
- `serde_json` — tool params/returns
- `tracing` — logging
