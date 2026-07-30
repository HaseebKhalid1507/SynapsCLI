# Headless Engine — Technical Specification

**Branch:** `feat/headless-engine`  
**Goal:** Extract chatui's business logic into a shared engine, enabling a headless mode (`synaps chat`) with full feature parity minus the TUI.

---

## Problem

SynapsCLI has 4 modes for running conversations:

| Mode | LOC | Features | Problem |
|------|-----|----------|---------|
| `chatui` (TUI) | 19,529 | Everything | Requires a terminal with TUI support |
| `daemon` | 425 | Events, compaction, sockets | No stdin interaction, event-driven only |
| `agent` | 557 | Handoff, watcher integration | Watcher-managed only, no standalone use |
| `chat` | 119 | Streaming + tools | Missing: MCP, extensions, skills, session, config, compaction, commands |
| `run` | 32 | Single-shot | No conversation, no tools |

There's no headless mode with full features. This blocks:
- **Harbor integration** — benchmark framework needs headless + tools + MCP
- **Scripting/piping** — `cat task.md | synaps chat` should have full power
- **Remote/SSH use** — not every terminal supports ratatui
- **CI/automation** — agents in GitHub Actions, cron jobs, Docker

---

## Architecture

### Current: monolith
```
chatui/mod.rs (2,176 lines)
├── Setup (config, MCP, extensions, skills, session, sockets)
├── Event loop (terminal events + LLM stream + extensions + sidecars)
├── Command processing (slash commands → actions)
├── Stream handling (tool routing, subagent tracking, steering)
└── TUI rendering (ratatui frames, themes, animations)
```

Everything is interleaved in one massive `tokio::select!` loop.

### Target: engine + renderers
```
src/
├── engine/
│   ├── mod.rs          # Engine struct — owns runtime, session, extensions
│   ├── setup.rs        # Boot sequence (config, MCP, extensions, skills, sockets)
│   ├── commands.rs     # Command router (parse + execute, no TUI types)
│   ├── session.rs      # Session persistence (save, load, resume, clear)
│   ├── stream.rs       # Stream event processor (tool routing, subagent tracking)
│   ├── compaction.rs   # Auto-compaction logic (moved from daemon.rs)
│   └── events.rs       # Event bus integration (inbox + socket)
│
├── chatui/             # TUI renderer (existing, refactored to use Engine)
│   ├── mod.rs          # TUI event loop — delegates to Engine
│   ├── app.rs          # TUI-specific state (scroll, cursor, selection)
│   ├── draw.rs         # Rendering
│   ├── input.rs        # Keyboard/mouse handling
│   └── ...             # themes, markdown, modals, etc.
│
├── cmd/
│   ├── chat.rs         # Headless renderer — uses Engine with stdin/stdout
│   └── ...
```

### The Engine struct

```rust
pub struct Engine {
    pub runtime: Runtime,
    pub session: Session,
    pub messages: Vec<Value>,
    pub config: SynapsConfig,
    pub command_registry: CommandRegistry,
    pub keybind_registry: KeybindRegistry,
    pub ext_manager: Arc<Mutex<ExtensionManager>>,
    pub event_queue: EventQueue,
    pub subagents: Vec<SubagentState>,
    pub session_cost: f64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
}

impl Engine {
    /// Full boot sequence: config → MCP → extensions → skills → session → sockets
    pub async fn new(opts: EngineOpts) -> Result<Self>;
    
    /// Process a user message through the full pipeline
    pub async fn send_message(&mut self, input: &str) -> StreamHandle;
    
    /// Process a slash command, returns an action
    pub fn handle_command(&mut self, input: &str) -> CommandResult;
    
    /// Run compaction if needed
    pub async fn maybe_compact(&mut self) -> Result<()>;
    
    /// Save session to disk
    pub fn save_session(&self) -> Result<()>;
    
    /// Shutdown: save session, fire hooks, cleanup
    pub async fn shutdown(&mut self) -> Result<()>;
}
```

---

## Migration Plan

### Phase 1: Extract setup (low risk)
Move the boot sequence from chatui/mod.rs lines 50-180 into `engine/setup.rs`:
- Config loading
- System prompt resolution
- Skill/plugin registration
- MCP lazy loading
- Extension manager creation
- Session socket + inbox watcher
- Session registry

chatui/mod.rs calls `Engine::new()` instead of doing this inline.

### Phase 2: Extract commands (medium risk)
Split commands.rs into:
- `engine/commands.rs` — command parsing + execution logic (returns `CommandResult` enum)
- `chatui/commands.rs` — TUI-specific rendering of command results (modals, toasts)

The engine processes `/model claude-opus-4-7` and returns `CommandResult::ModelChanged("claude-opus-4-7")`.
The TUI renderer decides whether to show a toast or open a modal.
The headless renderer prints "Model changed to claude-opus-4-7".

### Phase 3: Extract stream handling (medium risk)
Move stream event processing from the chatui event loop into `engine/stream.rs`:
- Tool use routing (parallel tool_id tracking)
- Subagent start/update/done state management
- Message history capture
- Usage/cost accumulation
- Abort context capture

Returns `StreamEvent` variants that renderers can display however they want.

### Phase 4: Extract session management (low risk)
Move session save/load/resume/clear into `engine/session.rs`.
Currently scattered across chatui/mod.rs and commands.rs.

### Phase 5: Build headless renderer (the payoff)
Rewrite `cmd/chat.rs` to use Engine:

```rust
pub async fn run(opts: ChatOpts) -> Result<()> {
    let mut engine = Engine::new(EngineOpts {
        profile: opts.profile,
        system: opts.system,
        agent: opts.agent,
        continue_session: opts.continue_id,
        no_extensions: opts.no_extensions,
    }).await?;

    // Simple stdin loop
    loop {
        let input = read_line()?;
        if input.starts_with('/') {
            match engine.handle_command(&input) {
                CommandResult::Quit => break,
                CommandResult::Output(text) => println!("{}", text),
                CommandResult::ModelChanged(m) => eprintln!("Model: {}", m),
                // ... handle other results
            }
        } else {
            let stream = engine.send_message(&input).await;
            // Print streaming output to stdout
            while let Some(event) = stream.next().await {
                match event {
                    StreamEvent::Text(t) => print!("{}", t),
                    StreamEvent::ToolUse { name, .. } => eprintln!("⚙️  {}", name),
                    StreamEvent::Done => { println!(); break; }
                    // ...
                }
            }
            engine.maybe_compact().await?;
        }
    }
    engine.shutdown().await
}
```

### Phase 6: Refactor chatui to use Engine
Replace the inline logic in chatui/mod.rs with Engine calls.
chatui becomes a "TUI renderer" that owns an Engine and translates its outputs into ratatui widgets.

---

## What Each Mode Gets

| Feature | chatui (TUI) | chat (headless) | daemon | agent |
|---------|:---:|:---:|:---:|:---:|
| Full config loading | ✅ | ✅ | ✅ | ✅ |
| MCP servers | ✅ | ✅ | ❌ | ❌ |
| Extensions/plugins | ✅ | ✅ | ❌ | ❌ |
| Skills | ✅ | ✅ | ❌ | ❌ |
| Slash commands | ✅ | ✅ | ❌ | ❌ |
| Session persistence | ✅ | ✅ | ❌ | via handoff |
| Session resume | ✅ | ✅ | ❌ | ❌ |
| Compaction | ✅ | ✅ | ✅ | ✅ |
| Event bus | ✅ | ✅ | ✅ | ✅ |
| Subagent dispatch | ✅ | ✅ | ✅ | ✅ |
| Themes/animations | ✅ | ❌ | ❌ | ❌ |
| Mouse/selection | ✅ | ❌ | ❌ | ❌ |
| Modals/lightboxes | ✅ | ❌ | ❌ | ❌ |
| Markdown rendering | ✅ | ❌ | ❌ | ❌ |
| stdin interaction | ✅ | ✅ | ❌ | ❌ |

---

## Files Touched

| Phase | Files | Risk |
|-------|-------|------|
| 1 (setup) | New: `engine/mod.rs`, `engine/setup.rs`. Modified: `chatui/mod.rs` | Low |
| 2 (commands) | New: `engine/commands.rs`. Modified: `chatui/commands.rs` | Medium |
| 3 (stream) | New: `engine/stream.rs`. Modified: `chatui/mod.rs` | Medium |
| 4 (session) | New: `engine/session.rs`. Modified: `chatui/mod.rs`, `chatui/commands.rs` | Low |
| 5 (headless) | Rewrite: `cmd/chat.rs` | Low (new code) |
| 6 (refactor chatui) | Modified: `chatui/mod.rs` heavily | High |

---

## Out of Scope

- Daemon/agent modes are NOT refactored to use Engine (they have different lifecycles)
- No new TUI features in this branch
- Extensions/sidecars protocol unchanged
- MCP protocol unchanged

---

## Success Criteria

- [ ] `synaps chat` boots with full config, MCP, extensions, skills
- [ ] Slash commands work in headless mode (text output instead of modals)
- [ ] Session save/load/resume works in headless
- [ ] Compaction triggers automatically
- [ ] Subagent dispatch works and shows status inline
- [ ] Event bus (socket + inbox) works
- [ ] `chatui` still works exactly as before (no regression)
- [ ] Harbor integration works: pipe instruction in, get result out
- [ ] All existing tests pass
