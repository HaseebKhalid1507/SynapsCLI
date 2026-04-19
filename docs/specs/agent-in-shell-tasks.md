# Agent In Shell — Implementation Plan

**Branch:** `agent_in_shell`
**Spec:** `docs/specs/agent-in-shell.md`

---

## Dependency Graph

```
portable-pty dependency (Cargo.toml)
    │
    ├── PTY abstraction (pty.rs)
    │       │
    │       ├── ShellConfig (config.rs)
    │       │       │
    │       │       └── SessionManager (session.rs)
    │       │               │
    │       │               ├── Readiness detection (readiness.rs)
    │       │               │       │
    │       │               │       ├── ShellStartTool (start.rs)
    │       │               │       ├── ShellSendTool (send.rs)
    │       │               │       └── ShellEndTool (end.rs)
    │       │               │
    │       │               └── Reaper task (session.rs)
    │       │
    │       └── Integration with ToolContext + Registry
    │
    └── Integration tests
```

---

## Task 1: Add `portable-pty` dependency and shell module skeleton

**Description:** Add the PTY crate to Cargo.toml and create the empty module tree with placeholder types.

**Acceptance criteria:**
- [ ] `portable-pty = "0.9"` in Cargo.toml dependencies
- [ ] `src/tools/shell/mod.rs` exists with submodule declarations
- [ ] All submodule files exist as stubs (compile but do nothing)
- [ ] `cargo build` succeeds with no new warnings

**Verification:**
- [ ] `cargo check` passes
- [ ] Module tree matches spec structure

**Dependencies:** None
**Files:** `Cargo.toml`, `src/tools/shell/{mod,session,start,send,end,pty,readiness,config}.rs`, `src/tools/mod.rs`
**Scope:** S

---

## Task 2: ShellConfig — config parsing and defaults

**Description:** Implement `ShellConfig` with defaults and parsing from `SynapsConfig`. Add `shell.*` config key recognition.

**Acceptance criteria:**
- [ ] `ShellConfig` struct with all fields from spec (max_sessions, idle_timeout, readiness_strategy, etc.)
- [ ] `Default` impl with sensible defaults
- [ ] Config parsing from `SynapsConfig` recognizes `shell.*` keys
- [ ] Unknown shell keys are preserved (not rejected)
- [ ] Unit tests for default values and parsing

**Verification:**
- [ ] `cargo test` — config tests pass
- [ ] Config with no `shell.*` keys → all defaults

**Dependencies:** Task 1
**Files:** `src/tools/shell/config.rs`, `src/core/config.rs`
**Scope:** S

---

## Task 3: PTY abstraction — spawn, read, write

**Description:** Build the PTY layer that wraps `portable-pty`. Spawn a command on a PTY, set up async reader, provide write access.

**Acceptance criteria:**
- [ ] `PtyHandle` struct wrapping master, writer, reader task, output channel
- [ ] `PtyHandle::spawn(command, cwd, env, rows, cols)` — spawns process on PTY
- [ ] Async reader task via `spawn_blocking` — pushes bytes to `mpsc` channel
- [ ] `PtyHandle::write(input)` — writes bytes to PTY
- [ ] `PtyHandle::resize(rows, cols)` — resizes PTY
- [ ] `PtyHandle::try_read_output(timeout)` — drains channel with timeout
- [ ] `Drop` impl kills child and cleans up
- [ ] Integration test: spawn `echo hello`, read output, verify

**Verification:**
- [ ] `cargo test` — PTY spawn/read/write test passes
- [ ] No FD leaks (child process dies on Drop)

**Dependencies:** Task 1
**Files:** `src/tools/shell/pty.rs`
**Scope:** M

---

## Task 4: Readiness detection strategies

**Description:** Implement the output readiness detection system — timeout-based, prompt-detection, and hybrid.

**Acceptance criteria:**
- [ ] `ReadinessStrategy` enum: Timeout, Prompt, Hybrid
- [ ] `ReadinessDetector` with configurable strategy
- [ ] `detect(accumulated_output, elapsed_silence) → ReadinessResult` (Ready/Waiting/MaxTimeout)
- [ ] Default prompt patterns from spec
- [ ] Unit tests for each strategy with mock output

**Verification:**
- [ ] `cargo test` — all readiness detection tests pass
- [ ] Timeout strategy returns after silence period
- [ ] Prompt strategy returns immediately on pattern match
- [ ] Hybrid tries prompt first, falls back to timeout

**Dependencies:** Task 2 (needs config for defaults)
**Files:** `src/tools/shell/readiness.rs`
**Scope:** M

---

## Checkpoint: After Tasks 1-4
- [ ] All tests pass: `cargo test`
- [ ] Build clean: `cargo clippy`
- [ ] Foundation is solid: PTY works, readiness detection works, config works
- [ ] **Review with Haseeb before proceeding to session management**

---

## Task 5: SessionManager — create, access, close sessions

**Description:** Build the session manager that holds active sessions and provides thread-safe access.

**Acceptance criteria:**
- [ ] `SessionManager::new(config)` — creates empty manager
- [ ] `SessionManager::create_session(opts)` — spawns PTY, stores session, returns (id, initial_output)
- [ ] `SessionManager::send_input(id, input, timeout)` — writes to PTY, waits for readiness, returns output
- [ ] `SessionManager::close_session(id)` — kills process, cleans up, returns final output
- [ ] Max session limit enforced (returns error if exceeded)
- [ ] Session ID generation: `shell_01`, `shell_02`, etc.
- [ ] `last_active` updated on every `send_input`
- [ ] Process exit detection (reader EOF → status = Exited)
- [ ] Integration test: full lifecycle (create → send → close)

**Verification:**
- [ ] `cargo test` — session lifecycle tests pass
- [ ] Max sessions error works
- [ ] Double-close is idempotent (not an error)

**Dependencies:** Tasks 3, 4
**Files:** `src/tools/shell/session.rs`
**Scope:** L (break if needed)

---

## Task 6: Idle reaper task

**Description:** Background tokio task that periodically checks for and closes idle sessions.

**Acceptance criteria:**
- [ ] `SessionManager::start_reaper()` — spawns background task
- [ ] Reaper runs every 30 seconds
- [ ] Sessions past `idle_timeout` with no `send_input` activity are closed
- [ ] Reaper skips sessions with very recent activity (avoid race with active send)
- [ ] Reaper logs reaped session IDs via `tracing::warn`
- [ ] Test: create session, advance time, verify reaped

**Verification:**
- [ ] `cargo test` — reaper test passes
- [ ] No panic if SessionManager is dropped while reaper runs

**Dependencies:** Task 5
**Files:** `src/tools/shell/session.rs`
**Scope:** S

---

## Task 7: Shell tools — `shell_start`, `shell_send`, `shell_end`

**Description:** Implement the three Tool trait impls that delegate to SessionManager.

**Acceptance criteria:**
- [ ] `ShellStartTool` — parses params, calls `session_manager.create_session()`, returns JSON
- [ ] `ShellSendTool` — parses params, calls `session_manager.send_input()`, returns JSON
- [ ] `ShellEndTool` — parses params, calls `session_manager.close_session()`, returns JSON
- [ ] All tools handle missing/invalid session_id gracefully
- [ ] All tools handle missing SessionManager in ToolContext (return error, not panic)
- [ ] JSON return format matches spec
- [ ] Error messages are clear and actionable

**Verification:**
- [ ] `cargo test` — tool execute tests pass
- [ ] Tools return proper JSON structure
- [ ] Missing session_id → useful error

**Dependencies:** Task 5
**Files:** `src/tools/shell/{start,send,end}.rs`
**Scope:** M

---

## Task 8: Wire into ToolRegistry and ToolContext

**Description:** Register shell tools in ToolRegistry, add SessionManager to ToolContext, wire through runtime.

**Acceptance criteria:**
- [ ] `SessionManager` created during Runtime initialization
- [ ] `session_manager: Option<Arc<SessionManager>>` added to `ToolContext`
- [ ] `ToolContext` populated with `Some(session_manager)` in both stream and single modes
- [ ] Shell tools registered in `ToolRegistry::new()` and `without_subagent()`
- [ ] Reaper task started on Runtime creation
- [ ] Shutdown: `session_manager.shutdown_all()` on runtime drop
- [ ] `cargo build` — full build succeeds

**Verification:**
- [ ] `cargo test` — all existing tests still pass
- [ ] `cargo clippy` — no new warnings
- [ ] Shell tools appear in tool schema sent to API

**Dependencies:** Tasks 6, 7
**Files:** `src/tools/mod.rs`, `src/tools/registry.rs`, `src/runtime/mod.rs`, `src/tools/shell/mod.rs`
**Scope:** M

---

## Checkpoint: After Tasks 5-8
- [ ] Full build passes: `cargo build`
- [ ] All tests pass: `cargo test`
- [ ] Clippy clean: `cargo clippy`
- [ ] End-to-end: can manually test shell_start → shell_send → shell_end
- [ ] **Review with Haseeb before integration tests**

---

## Task 9: Integration tests

**Description:** Comprehensive integration tests with real PTY sessions.

**Acceptance criteria:**
- [ ] Test: bash session — start, `echo hello`, verify output, end
- [ ] Test: python REPL — start `python3`, send `1+1\n`, verify `2`, exit
- [ ] Test: Ctrl-C — start `sleep 999`, send `\x03`, verify interrupted
- [ ] Test: Ctrl-D / EOF — start `cat`, send `\x04`, verify exited
- [ ] Test: working directory — start in /tmp, `pwd`, verify
- [ ] Test: env vars — start with custom env, `echo $VAR`, verify
- [ ] Test: max sessions — exceed limit, verify error
- [ ] Test: session not found — send to invalid ID, verify error
- [ ] Test: double close — close twice, verify idempotent
- [ ] Test: process exit — run `exit 42`, next send returns exited status

**Verification:**
- [ ] `cargo test --test shell_pty` — all pass
- [ ] No flaky tests (timeouts are generous)

**Dependencies:** Task 8
**Files:** `tests/shell_pty.rs`
**Scope:** M

---

## Task 10: TUI streaming integration

**Description:** Shell tool output streams to TUI via `tx_delta` during `shell_send`.

**Acceptance criteria:**
- [ ] `shell_send` streams output chunks to `tx_delta` as they arrive (before readiness)
- [ ] `shell_start` streams initial output to `tx_delta`
- [ ] TUI shows shell output in real-time (same as bash tool)
- [ ] No TUI corruption from PTY escape sequences (stripped)

**Verification:**
- [ ] Manual test in chatui: start shell, send commands, see output stream live
- [ ] No ANSI garbage in tool result panel

**Dependencies:** Task 8
**Files:** `src/tools/shell/session.rs`, `src/tools/shell/{start,send}.rs`
**Scope:** S

---

## Final Checkpoint
- [ ] All tests pass: `cargo test`
- [ ] Clippy clean: `cargo clippy`
- [ ] No warnings: `cargo build`
- [ ] Manual chatui test: full interactive session works
- [ ] Spec compliance: all success criteria met
- [ ] Ready for PR to dev
