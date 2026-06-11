# SynapsCLI Technical Review — Performance, Bugs, Architecture, Polish

**Date:** 2026-06-11 (S205)
**Reviewers:** Chrollo (performance), Shady (bugs), Spike (polish/UX), Yoru (architecture)
**Codebase:** ~75K LOC, 236 .rs files, single crate, 13MB release binary

---

## Executive Summary

The runtime is architecturally sound — channel-driven, properly async, good panic hygiene in production code (the 1007-unwrap stat was a head-fake; most live in `#[cfg(test)]`). The core problems are:

1. **The TUI burns a full CPU core during streaming** — caused by a multiplication of allocation patterns at 60fps (per-frame viewport clone, per-char String allocs, per-event SSE copies, per-frame chrome formatting). Fixes compose and are independent.
2. **Child process cleanup only runs on the happy path** — PTY sessions, MCP servers, and extension processes all risk orphaning on cancellation/crash. Every `Child` needs a Drop guard.
3. **Error messages are raw technical dumps** — API errors, network failures, config typos all surface as developer-facing strings with no user guidance.
4. **The crate should split** — 75K LOC in one crate tanks incremental compile. Three cuts along natural fault lines.

**Priority matrix: fix the render loop → kill orphan processes → parse error messages → split the crate → instrument everything else.**

---

## 🔥 PERFORMANCE (Chrollo — 15 findings)

### HIGH IMPACT

**P1. Full-frame redraw with deep-cloned viewport — `tui/draw.rs:564`**
Every frame at 60fps: `visible.clone()` deep-copies the entire `Vec<Line>` (each Line owns Vec<Span>, each Span owns a String). No dirty-region tracking — entire UI rebuilt from scratch per frame.
→ **Fix:** Borrow data into `Text<'a>`, add a `dirty` flag, cap repaints at ~30fps during streaming via `Instant`-based gate. Ratatui diffs terminal writes, but the allocation cost is yours.

**P2. SSE parse loop: double-copy + untyped JSON per event — `runtime/api.rs:270-283`**
Per SSE line: `to_vec()` (copy #1) → `from_utf8_lossy().to_string()` (copy #2) → `serde_json::from_str::<Value>()` (full DOM allocation). At per-token delta granularity.
→ **Fix:** Parse in place with `str::from_utf8()`, define typed structs for known event shapes with `#[serde(borrow)]`, keep `Value` fallback only for unknowns.

**P3. Full message history cloned twice per turn — `tui/mod.rs:268,352` + `api.rs:93`**
Every stream launch: `app.api_messages.clone()` (deep copy of all messages), then `messages.to_vec()` in api.rs for cleaning. Megabytes for long sessions.
→ **Fix:** `Arc<Vec<Value>>` or `im::Vector<Arc<Value>>`. Cleaning pass should use `Cow<Value>` per element.

**P4. Per-character String allocations in render loops — `tui/draw.rs:679,963`**
`ch.to_string()` per character per frame (art spans), `cell.symbol().to_string()` per cell (selection overlay). Thousands of 1-char heap allocations per frame.
→ **Fix:** Group consecutive same-styled chars. Selection overlay: mutate cell style in place, don't extract symbol.

**P5. Clone-to-borrow workaround — `tui/mod.rs:72`**
`rebuild_display_messages(&app.api_messages.clone(), &mut app)` — entire history cloned just to satisfy the borrow checker.
→ **Fix:** `let msgs = std::mem::take(&mut app.api_messages); rebuild(...); app.api_messages = msgs;`

### MEDIUM IMPACT

**P6.** Tool I/O cloned 3-4× per call (stream.rs:242-317) → Arc<Value>/Arc<str>
**P7.** Tool registry snapshot deep-cloned per turn (stream.rs:119) → Arc behind the RwLock
**P8.** Per-delta String clones of tool_id/name in SSE loop → Arc<str>
**P9.** Status bar/chrome rebuilt with format! every frame including constants (draw.rs:490) → cache on value change
**P10.** Subagent panel + RuntimeSnapshot rebuilt per frame (draw.rs:1130) → event-driven rebuild

### LOW IMPACT (P11-P15): Event fan-out clones, final-buffer SSE flush, tool_uses.push clone, capability/context Arc clones (actually cheap), session-chain walk allocations (cold path).

### Where the CPU actually goes during streaming
Every SSE delta (P2) → triggers full-frame rebuild (P1) → deep-clones viewport → reformats all chrome (P9) → re-allocates per-char/per-cell strings (P4) — at 60Hz. **Throttle + dirty-flag the render loop is the biggest single win.**

---

## 🐛 BUGS (Shady — 8 findings)

### HIGH SEVERITY

**B1. Extension process manager: orphan risk — `extensions/runtime/process.rs` (2446 lines)**
The file is too large to audit (that's itself a bug). If the parent task is cancelled mid-await between spawn and registering the child for cleanup → zombie/orphan. Every `Child` needs `kill_on_drop(true)` at minimum. **Split this file.**

**B2. PTY sessions on timeout — `tools/shell/`**
Timeout fires, future dropped, but PTY child process keeps running. Session map may hold the master fd forever. Child becomes a zombie.
→ **Fix:** On timeout, explicitly kill() + wait(). Wrap sessions in a Drop guard that sends SIGKILL and reaps.

**B3. MCP child processes on crash path — `mcp/`**
Same pattern as B2. If runtime panics or MCP client is dropped without explicit shutdown, child only dies if `kill_on_drop` is set. Verify with `kill -9` on the runtime.
→ **Fix:** `kill_on_drop(true)` + best-effort graceful shutdown with hard-kill fallback.

### MEDIUM SEVERITY

**B4.** Channel sends with discarded results in event fan-out — `let _ = tx.send(...)` for load-bearing events = silent data loss
**B5.** Streaming path error swallowing — `.ok()` on sends in SSE loop; dead consumer means wasted tokens/money
**B6.** Lock ordering unverified between runtime and subagent locks — potential deadlock under concurrent spawn + config reload
**B7.** Watcher debounce/restart race — two rapid writes to watched config; loaded state matches write #1

### GOOD NEWS

**B8. Production panic surface is genuinely small.** After filtering `#[cfg(test)]`, remaining unwraps are defensible (Mutex poison, static regex, infallible serialization). **8/10 panic hygiene.** The 1007-unwrap stat was a head-fake from test code.

---

## ✨ POLISH / UX (Spike — 13 findings)

### HIGH USER IMPACT

**U1. Raw API error JSON dumped into chat — `runtime/api.rs:203`**
529 overload → user sees `{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}`. Retry toast injected as fake assistant text, polluting history.
→ **Fix:** Parse error.type, map to human text (529→"Anthropic overloaded, wait a minute", 401→"Run synaps login", 413→"suggest /compact"). Retry as SessionEvent, not assistant text.

**U2. Raw reqwest errors bubble to user — `core/error.rs:5-6`**
Network down → `"API error: error sending request for url..."`. No "check your connection", no retry suggestion.
→ **Fix:** `Network` error variant inspecting `e.is_connect()`/`e.is_timeout()`, with actionable message.

**U3. Config typos silently swallowed — `core/config.rs:359-442`**
`modle = claude-opus-4-7` → silently ignored, default model used. `thinking = hgih` → thinking silently off. No warnings. Half the file validates, half doesn't.
→ **Fix:** Collect warnings for unknown keys (levenshtein did-you-mean), unparseable values. Surface once at startup.

**U4. First run: silent boot with no credentials, no guidance**
Fresh install → TUI opens, user types, gets error. No first-run banner, no scaffolding.
→ **Fix:** Detect `auth_type == "none"` at boot, render a first-run banner with credential instructions.

**U12. User input lost on stream error — `engine/stream.rs:197-208`**
529 after retries → error → user's carefully-written prompt popped from history and gone forever.
→ **Fix:** Stash the popped user message, pre-fill input box, or offer `/retry`. Losing user input is the cardinal TUI sin.

### MEDIUM USER IMPACT

**U5.** OAuth refresh failure says bare `login` not `synaps login` — wrong command
**U6.** `synaps status` ignores profiles, doesn't refresh token — fails on expired OAuth
**U7.** Extension crash: user sees stale tools, restart-limit message goes to model not user
**U8.** MCP spawn failure shows command name not server name — "npx" vs "github"
**U9.** `--help` quality: no arg descriptions, watcher subcommands invisible to clap
**U10.** No shell completions, no man page, no self-update mechanism
**U11.** No range validation on config numbers — `bash_timeout = 0` makes every command fail
**U13.** Dead `EngineOpts.no_extensions` field, error enum too coarse (7 variants for whole runtime)

---

## 🏗️ ARCHITECTURE (Yoru — 6 findings)

### HIGH BANG-FOR-BUCK

**A1. Instrument first, rewrite second — run these before committing to anything:**
```bash
cargo machete          # unused deps
cargo tree -d          # duplicate versions (syn, serde, hashbrown)
cargo bloat --release --crates  # binary size breakdown
cargo build --timings  # what's slow
cargo llvm-cov --html  # what's untested
```
**Effort: XS | Impact: Unblocks everything.**

**A2. Feature-gate TUI — `[features] tui = ["dep:ratatui", "dep:syntect"]`**
Headless/server mode doesn't need rendering or syntax highlighting. Syntect pulls onig (C regex, slow build). Trim syntect to ~15 languages instead of 150.
→ Estimated binary savings: 2-3MB. Build time: significant.

**A3. 3-crate workspace split (not 10)**

| Crate | Contents |
|---|---|
| `agent-core` | types, config, errors, serde models |
| `agent-providers` | reqwest, SSE, provider engine |
| `agent-tui` | ratatui + syntect |
| `agent-runtime` (bin) | glue |

Three cuts. Hot edit loop recompiles ~25K LOC instead of 75K. **40-60% faster incremental builds.** Effort: M (3-5 days).

### MEDIUM BANG-FOR-BUCK

**A4. Async architecture: shared-state actor soup**
158 channels AND 98 Arc<Mutex> = paying both costs, getting neither guarantee. Migrate one subsystem (session/conversation state) from `Arc<Mutex>` → single-owning task with mpsc. Measure with tokio-console before committing.

**A5. SSE chunk-boundary fuzz tests**
A proptest harness that re-chunks known-good SSE streams at random offsets: ~50 lines, catches an entire bug class. If your tests only feed whole well-formed events, **coverage is theater.**

**A6. Split runtime/mod.rs (962 lines)**
Mechanical split into lifecycle/dispatch/state/shutdown. Effort: XS, do regardless.

### QUICK WINS
- Dev profile: `split-debuginfo = "unpacked"`, `debug = "line-tables-only"`, mold linker → 20-30% faster incremental link
- reqwest features: verify `default-features = false` + only `rustls-tls`, `stream`, `json`
- tokio features: trim `full` to actual usage list

---

## 🎯 Master Priority Matrix

| # | Fix | Category | Effort | Impact |
|---|-----|----------|--------|--------|
| 1 | Throttle render loop + dirty flag (P1) | Perf | S | 🔴 Kills the core burn |
| 2 | Kill orphan children: PTY, MCP, extensions (B1-3) | Bug | M | 🔴 Process leaks |
| 3 | Zero-copy SSE parsing (P2) | Perf | M | 🔴 Per-token alloc churn |
| 4 | Parse API errors → human messages (U1-2) | Polish | S | 🔴 Every user hits this |
| 5 | Don't lose user input on stream error (U12) | Polish | S | 🔴 Data loss |
| 6 | Arc message history + tool I/O (P3,6,7) | Perf | M | 🟠 O(conversation) copies |
| 7 | Config warnings + did-you-mean (U3) | Polish | M | 🟠 Silent misconfig |
| 8 | First-run banner (U4) | Polish | S | 🟠 First impression |
| 9 | Instrument: machete, tree -d, bloat, timings, llvm-cov (A1) | Arch | XS | 🟠 Unblocks decisions |
| 10 | Feature-gate TUI + trim syntect (A2) | Arch | S | 🟠 Build time + binary |
| 11 | 3-crate workspace split (A3) | Arch | M | 🟠 Dev velocity |
| 12 | Fix per-char/per-frame allocs (P4,9) | Perf | S | 🟡 Render polish |
| 13 | SSE fuzz tests (A5) | Arch | S | 🟡 Coverage theater → real |
| 14 | Split process.rs + mod.rs (B1, A6) | Arch | S | 🟡 Reviewability |
| 15 | Shell completions, help text (U9-10) | Polish | S | 🟡 Distribution polish |
