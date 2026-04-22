# tmux Native Enhancements — Design Document

**Branch:** `feat/tmux-native`
**Date:** 2025-01-27
**Status:** Approved

---

## Vision

Synaps launches inside a tmux session and becomes a tmux-native application. The agent can see itself, control panes, create visible shell sessions, and manage layouts — all through tmux's control mode protocol. The user gets a multiplexed workspace where they can watch the agent work in real-time, navigate with hotkeys or mouse, and customize the layout through natural language or settings.

---

## Entry Points

### CLI Flag
```bash
synaps --tmux 'my-project'     # explicit session name
synaps --tmux                   # auto-generated session name
synaps                          # if tmux=true in settings, auto-launches in tmux
```

### Auto-Naming
When no explicit session name is provided (either `synaps --tmux` with no name, or bare `synaps` with `tmux=true` in settings), Synaps auto-generates a session name:

1. **In a git repo:** `synaps-<repo-name>` (e.g. `synaps-SynapsCLI`)
2. **Not in a git repo:** `synaps-<directory-name>` (e.g. `synaps-my-project`)
3. **Collision (session already exists):** append `-<N>` (e.g. `synaps-SynapsCLI-2`)

Synaps **always knows its own session name** — it's stored in `TmuxController.session_name` and available to the agent via the system prompt context. This means the agent can reference its session in commands, and the user can reattach from another terminal with `tmux attach -t <name>` while Synaps is running.

### Settings (system-wide or per-project)
```
# ~/.synaps-cli/config (system-wide)
tmux = true                           # bare `synaps` launches in tmux mode
tmux_session_name = default           # optional: fixed name override

# .synaps/config (project-level, overrides system)
tmux = true
tmux_session_name = my-project        # optional: fixed name for this project
```

### Config Hierarchy
```
CLI flags  →  overrides  →  Project config (.synaps/config)  →  overrides  →  System config (~/.synaps-cli/config)
```

### Slash Command / Settings Page
Users can toggle tmux mode from the TUI settings page or via `/settings tmux on`.

---

## Architecture

### Control Mode (`tmux -CC`)

Synaps communicates with tmux exclusively through **control mode** — a persistent stdin/stdout text protocol. No shell-outs to the `tmux` binary for operations.

```
┌─────────────────────────────────────────────────────────┐
│                    Synaps Process                        │
│                                                         │
│  ┌──────────┐   ┌──────────────┐   ┌────────────────┐  │
│  │ Runtime   │──▶│ TmuxController│──▶│ Control Mode   │  │
│  │          │   │              │   │ stdin/stdout    │  │
│  │ToolReg   │   │ • state map  │   │ pipe to tmux   │  │
│  │  +tmux   │   │ • event loop │   │ server         │  │
│  │  tools   │   │ • cmd queue  │   │                │  │
│  └──────────┘   └──────────────┘   └────────────────┘  │
│                        │                                 │
│              ┌─────────┴──────────┐                      │
│              │  TmuxState         │                      │
│              │  • sessions []     │                      │
│              │  • windows []      │                      │
│              │  • panes []        │                      │
│              │  • layouts {}      │                      │
│              │  • hotkeys {}      │                      │
│              └────────────────────┘                      │
└─────────────────────────────────────────────────────────┘
         │ control mode protocol
         ▼
┌─────────────────────────────────────────────────────────┐
│                   tmux server                            │
│                                                         │
│  Session: "my-project"                                  │
│  ┌─────────────────────────┬────────────────────────┐   │
│  │ Window 0: chat          │ Window 1: shell         │   │
│  │ ┌─────────┬───────────┐ │ (fullscreen shell)     │   │
│  │ │ Synaps  │ Agent     │ │                        │   │
│  │ │ TUI     │ Shell #1  │ │                        │   │
│  │ │ (chat)  │ $ cargo   │ │                        │   │
│  │ │         │   build   │ │                        │   │
│  │ │         ├───────────┤ │                        │   │
│  │ │         │ Agent     │ │                        │   │
│  │ │         │ Shell #2  │ │                        │   │
│  │ │         │ $ grep .. │ │                        │   │
│  │ └─────────┴───────────┘ │                        │   │
│  └─────────────────────────┴────────────────────────┘   │
│  [my-project] 0:chat*  1:shell  sa_1:running ⏱12s      │
└─────────────────────────────────────────────────────────┘
```

### Control Mode Protocol

tmux control mode streams notifications as text lines:

```
# Notifications (tmux → Synaps)
%begin <time> <num> <flags>
%end <time> <num> <flags>
%output %<pane_id> <data>
%window-add @<window_id>
%window-close @<window_id>
%session-changed $<session_id> <name>
%pane-mode-changed %<pane_id>
%layout-change @<window_id> <layout>
...

# Commands (Synaps → tmux)
split-window -h -P -F '#{pane_id}' -t %0
send-keys -t %5 'cargo build' Enter
capture-pane -t %5 -p
select-layout -t @0 main-vertical
```

### TmuxController (`src/tmux/controller.rs`)

Core struct that owns the control mode connection:

```rust
pub struct TmuxController {
    /// stdin writer to control mode
    writer: BufWriter<ChildStdin>,
    /// Parsed state of all tmux objects
    state: Arc<RwLock<TmuxState>>,
    /// Channel for incoming notifications
    event_rx: mpsc::UnboundedReceiver<TmuxEvent>,
    /// Pending command responses
    pending: HashMap<u64, oneshot::Sender<CommandResult>>,
    /// Session name
    session_name: String,
    /// Pane ID where Synaps TUI is running
    self_pane: String,
}
```

### TmuxState (`src/tmux/state.rs`)

```rust
pub struct TmuxState {
    pub session_id: String,
    pub windows: HashMap<String, TmuxWindow>,
    pub panes: HashMap<String, TmuxPane>,
    pub layout_preset: LayoutPreset,
    pub subagent_display: SubagentDisplay,
}

pub struct TmuxWindow {
    pub id: String,       // @0, @1, ...
    pub name: String,
    pub index: u32,
    pub panes: Vec<String>, // pane IDs
    pub layout: String,
}

pub struct TmuxPane {
    pub id: String,        // %0, %1, ...
    pub window_id: String,
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub active: bool,
    pub role: PaneRole,
}

pub enum PaneRole {
    SynapsTui,                         // the main chat pane
    AgentShell { session_id: String }, // a shell_start pane
    Subagent { handle_id: String },    // a subagent display
    User,                              // user-created pane
}

pub enum LayoutPreset {
    Split,       // TUI left, shells right (default)
    Fullscreen,  // TUI fullscreen, shells in separate windows
    Tiled,       // Equal grid of all panes
    Custom(String), // tmux layout string
}

pub enum SubagentDisplay {
    Window, // each subagent gets its own tmux window (tab)
    Pane,   // each subagent gets a pane in the current window
}
```

---

## New Tools

The agent gets explicit tmux tools. These are only registered when tmux mode is active.

### `tmux_split`
Create a new pane by splitting.

```json
{
  "name": "tmux_split",
  "parameters": {
    "direction": "horizontal | vertical",   // default: horizontal
    "size": "50%",                           // percentage or line count
    "target": "%0",                          // pane to split (default: auto)
    "command": "cargo build",                // optional command to run
    "title": "build",                        // pane title
    "focus": false                           // switch focus to new pane?
  }
}
// Returns: { "pane_id": "%5", "window_id": "@0" }
```

### `tmux_send`
Send keys/commands to any pane.

```json
{
  "name": "tmux_send",
  "parameters": {
    "pane_id": "%5",
    "keys": "cargo test\n",      // keys to send (literal)
    "literal": true               // true = no key name lookup
  }
}
// Returns: { "sent": true }
```

### `tmux_capture`
Read content from any visible pane.

```json
{
  "name": "tmux_capture",
  "parameters": {
    "pane_id": "%5",
    "start_line": -100,     // negative = history lines
    "end_line": -1,         // -1 = last visible line  
    "include_ansi": false   // strip ANSI escapes (default)
  }
}
// Returns: { "content": "$ cargo build\n   Compiling synaps v0.1.0\n..." }
```

### `tmux_layout`
Change the layout of the current window or set defaults.

```json
{
  "name": "tmux_layout",
  "parameters": {
    "preset": "split | fullscreen | tiled | custom",
    "custom_layout": null,       // tmux layout string (if preset=custom)
    "set_default": false,        // persist as default?
    "scope": "project | system"  // where to persist (if set_default=true)
  }
}
// Returns: { "layout": "split", "applied": true, "persisted": false }
```

### `tmux_window`
Create, switch, or close tmux windows (tabs).

```json
{
  "name": "tmux_window",
  "parameters": {
    "action": "create | switch | close | rename",
    "name": "tests",           // window name
    "target": "@1",            // target window (for switch/close/rename)
    "command": null             // command to run in new window
  }
}
// Returns: { "window_id": "@2", "action": "create" }
```

### `tmux_resize`
Resize a pane.

```json
{
  "name": "tmux_resize",
  "parameters": {
    "pane_id": "%5",
    "width": null,       // absolute or "+10"/"-10" relative
    "height": null,
    "zoom": false        // toggle zoom (pane fills entire window)
  }
}
// Returns: { "pane_id": "%5", "width": 80, "height": 24 }
```

---

## Startup Flow

```
synaps --tmux 'my-project'
        │
        ▼
┌─ Check tmux binary ─────────────────────────────────┐
│  which tmux                                          │
│  NOT FOUND?                                          │
│    → "Error: tmux not found."                        │
│    → "Install tmux from source? [y/N]"               │
│    → y: git clone, autogen.sh, configure, make       │
│    → n: exit                                         │
└──────────────────────────────────────────────────────┘
        │ found
        ▼
┌─ Create tmux session ───────────────────────────────┐
│  tmux new-session -d -s 'my-project' -x 200 -y 50   │
│  (detached, sized to current terminal)               │
└──────────────────────────────────────────────────────┘
        │
        ▼
┌─ Start control mode client ─────────────────────────┐
│  tmux -CC attach -t 'my-project'                     │
│  → persistent stdin/stdout connection                │
│  → TmuxController starts event reader thread         │
│  → Parse %begin/%end, %output, %layout-change, etc. │
└──────────────────────────────────────────────────────┘
        │
        ▼
┌─ Identify self-pane ────────────────────────────────┐
│  Query: display-message -p '#{pane_id}'              │
│  Store as self_pane in TmuxState                     │
└──────────────────────────────────────────────────────┘
        │
        ▼
┌─ Apply default layout ──────────────────────────────┐
│  Read config: tmux_default_layout                    │
│  Read config: tmux_subagent_display                  │
│  Apply initial layout (split by default)             │
└──────────────────────────────────────────────────────┘
        │
        ▼
┌─ Set up convenience hotkeys ────────────────────────┐
│  bind-key -T prefix F   → toggle fullscreen          │
│  bind-key -T prefix S   → cycle subagent display     │
│  bind-key -T prefix L   → cycle layout presets       │
│  bind-key -T prefix N   → next subagent pane/window  │
│  bind-key -T prefix H   → show hotkey help popup     │
│  set -g mouse on         → enable mouse support      │
└──────────────────────────────────────────────────────┘
        │
        ▼
┌─ Register tmux tools ───────────────────────────────┐
│  Add to ToolRegistry:                                │
│    tmux_split, tmux_send, tmux_capture,              │
│    tmux_layout, tmux_window, tmux_resize             │
└──────────────────────────────────────────────────────┘
        │
        ▼
┌─ Start Synaps TUI normally ─────────────────────────┐
│  chatui::run() in the main pane                      │
│  TmuxController runs as background task              │
└──────────────────────────────────────────────────────┘
```

---

## Shutdown Flow

```
User exits Synaps (Ctrl-C, /exit, etc.)
        │
        ▼
┌─ Cleanup ────────────────────────────────────────────┐
│  1. Cancel all subagents (existing behavior)          │
│  2. Close all shell sessions (existing behavior)      │
│  3. Kill tmux session: kill-session -t 'my-project'   │
│  4. Drop TmuxController (closes control mode pipe)    │
│  5. Exit process                                      │
└──────────────────────────────────────────────────────┘
```

Session dies with Synaps. Clean slate every time.

---

## Default Layout: Split View

```
┌────────────────────────────────┬─────────────────────────┐
│                                │                         │
│    Synaps TUI                  │  Shell Pane %1          │
│    (chat interface)            │  $ cargo build          │
│    pane %0                     │  Compiling...           │
│                                │                         │
│                                ├─────────────────────────┤
│                                │                         │
│    > user types here           │  Shell Pane %2          │
│    _ cursor                    │  $ grep -r "foo" src/   │
│                                │                         │
├────────────────────────────────┴─────────────────────────┤
│ [my-project] 0:chat*  C-b F:fullscreen  C-b L:layout    │
└──────────────────────────────────────────────────────────┘
```

### Fullscreen Mode
```
┌──────────────────────────────────────────────────────────┐
│                                                          │
│              Synaps TUI (full window)                    │
│              pane %0                                     │
│                                                          │
│                                                          │
│    > user types here                                     │
│    _ cursor                                              │
│                                                          │
├──────────────────────────────────────────────────────────┤
│ [my-project] 0:chat*  1:shell  2:tests  sa_1 ⏱12s      │
└──────────────────────────────────────────────────────────┘
```

Shell panes move to separate windows (tabs), navigable via status bar or `C-b n/p`.

---

## Subagent Display Modes

### Mode: `window` (subagent_display = "window")

Each subagent gets its own tmux window:

```
Status bar:
[my-project] 0:chat*  1:shell  2:sa_1:running  3:sa_2:done
```

Window content shows a mini Synaps-like stream:
```
┌──────────────────────────────────────────────────────────┐
│ Subagent sa_1 | model: claude-sonnet-4-6 | ⏱ 12.3s     │
│ Task: "Analyze the test coverage gaps"                   │
│──────────────────────────────────────────────────────────│
│ 🤔 Thinking: Let me look at the test files first...     │
│                                                          │
│ 🔧 Tool: bash("find tests/ -name '*.rs' | wc -l")      │
│ → 23                                                     │
│                                                          │
│ 🔧 Tool: read("src/tools/mod.rs")                       │
│ → [142 lines]                                            │
│                                                          │
│ 💬 There are 23 test files covering...                   │
│                                                          │
├──────────────────────────────────────────────────────────┤
│ [my-project] 0:chat  1:shell  2:sa_1*                    │
└──────────────────────────────────────────────────────────┘
```

### Mode: `pane` (subagent_display = "pane")

Each subagent gets a pane in the current window:

```
┌──────────────────────┬──────────────────┬────────────────┐
│                      │ Shell %1         │ sa_1 ⏱12s     │
│  Synaps TUI          │ $ cargo build    │ 🤔 Analyzing  │
│  (chat)              │ Compiling...     │ 🔧 bash(...)  │
│  %0                  │                  │ → 23 files    │
│                      ├──────────────────┤                │
│                      │ Shell %2         │                │
│  > user types here   │ $ grep ...       │ 💬 Found 3... │
├──────────────────────┴──────────────────┴────────────────┤
│ [my-project] 0:chat*  sa_1:running ⏱12s                  │
└──────────────────────────────────────────────────────────┘
```

---

## Convenience Hotkeys

All under the standard tmux prefix (`C-b` by default):

| Key | Action | Description |
|-----|--------|-------------|
| `C-b F` | Toggle fullscreen | Switch between split and fullscreen for Synaps TUI |
| `C-b S` | Cycle subagent display | Toggle between window/pane modes |
| `C-b L` | Cycle layout presets | Split → Fullscreen → Tiled → Split |
| `C-b N` | Next subagent | Focus next subagent pane/window |
| `C-b P` | Previous subagent | Focus previous subagent pane/window |
| `C-b H` | Hotkey help | Show popup with all Synaps hotkeys |
| `C-b Z` | Zoom pane | Standard tmux zoom (toggle) |
| `C-b arrows` | Navigate panes | Standard tmux pane navigation |
| Mouse click | Select pane | Standard tmux mouse support |
| Mouse drag | Resize pane | Standard tmux mouse support |
| Mouse wheel | Scroll | Scroll history in any pane |

Mouse is enabled by default (`set -g mouse on`).

---

## Configuration Options

```
# tmux mode
tmux = true | false                          # enable tmux mode (default: false)
tmux_session_name = "my-project"             # session name (default: auto-generated)
tmux_default_layout = "split"                # split | fullscreen | tiled (default: split)
tmux_subagent_display = "pane"               # window | pane (default: pane)
tmux_mouse = true                            # enable mouse (default: true)

# Layout customization (future)
tmux_split_ratio = 60                        # TUI pane width percentage (default: 60)
tmux_shell_position = "right"                # right | bottom (default: right)
```

All settings available at:
- System level: `~/.synaps-cli/config`
- Project level: `.synaps/config`
- Runtime: `/settings` TUI page or agent prompt

---

## Module Structure

```
src/
├── tmux/
│   ├── mod.rs              # public API, feature detection
│   ├── controller.rs       # TmuxController - control mode connection
│   ├── state.rs            # TmuxState, TmuxWindow, TmuxPane, enums
│   ├── protocol.rs         # Control mode parser (notifications + responses)
│   ├── layout.rs           # Layout presets, apply/cycle logic
│   ├── hotkeys.rs          # Convenience keybinding setup
│   └── install.rs          # tmux-not-found installer flow
├── tools/
│   ├── tmux_split.rs       # tmux_split tool
│   ├── tmux_send.rs        # tmux_send tool
│   ├── tmux_capture.rs     # tmux_capture tool
│   ├── tmux_layout.rs      # tmux_layout tool
│   ├── tmux_window.rs      # tmux_window tool
│   └── tmux_resize.rs      # tmux_resize tool
```

---

## tmux Not Found — Install Flow

When `synaps --tmux` is invoked but `tmux` is not in PATH:

```
┌──────────────────────────────────────────────────────────┐
│  Error: tmux is not installed.                           │
│                                                          │
│  tmux mode requires tmux to be available in your PATH.   │
│                                                          │
│  Would you like to install tmux from source? [y/N]       │
│                                                          │
│  This will run:                                          │
│    git clone https://github.com/tmux/tmux.git            │
│    cd tmux                                               │
│    sh autogen.sh                                         │
│    ./configure && make && sudo make install               │
└──────────────────────────────────────────────────────────┘
```

On `y`: execute the build, verify `which tmux`, then restart in tmux mode.
On `n` or Enter: exit with error code.

Build dependencies note: `autogen.sh` requires `automake`, `configure` requires `libevent-dev` and `ncurses-dev`. The installer should check for these and report if missing.

---

## What Stays The Same

- **portable-pty** remains for non-tmux mode. Zero changes to existing shell session behavior when tmux mode is off.
- **Existing shell tools** (`shell_start`, `shell_send`, `shell_end`) remain unchanged in both modes. In tmux mode, they still create invisible PTY sessions — they're for background work. The new tmux tools are for visible panes.
- **Subagent spawning** mechanism stays the same (OS thread isolation). The display mode only affects whether a visible pane/window is created to show the subagent's output.
- **TUI** (ratatui) runs as-is inside its tmux pane.

---

## Open Questions (for implementation phase)

1. **Subagent pane rendering** — The subagent output stream (thinking, tool use, text) needs a renderer for the tmux pane. Options: (a) pipe raw StreamEvents through a simple formatter, (b) run a mini headless Synaps in each subagent pane, (c) use `display-message -I` to write formatted text to empty panes.

2. **Control mode reconnection** — If the control mode pipe breaks mid-session, should we attempt reconnection or fail hard?

3. **Terminal size coordination** — When the user resizes their terminal, tmux handles pane resizing. The ratatui TUI already handles resize events. Need to verify these compose cleanly.

4. **Status bar format** — Exact format string for the tmux status bar showing session info, subagent status, and hotkey hints.
