# Performance / Scaling Audit — TUI & BIN
**Branch:** `dev` | **Date:** 2025-07-10 | **Scope:** O(total-data) bug class

---

## Summary Table

| # | File:line | What scales | Trigger | Severity | Fix direction |
|---|-----------|-------------|---------|----------|---------------|
| 1 | `crates/agent-tui/src/tui/mod.rs:1136` | Audit log file size (unbounded JSONL) | `/extensions audit` | **CRITICAL** | Tail-read N lines from end of file |
| 2 | `crates/agent-tui/src/tui/commands.rs:457` | # sessions × header-read cost | `/sessions` (sync, UI thread) | **CRITICAL** | spawn_blocking + cache |
| 3 | `crates/agent-core/src/core/session.rs:408` | # sessions (calls list_sessions twice) | `/resume <name>` | **HIGH** | Single list_sessions pass + direct load |
| 4 | `crates/agent-core/src/core/chain.rs:45` | # sessions (full header scan) | `/chain name <n>` (write path) | **HIGH** | Skip the name-collision guard, or use index |
| 5 | `crates/agent-tui/src/tui/draw.rs:467` | # messages × render cost | Every frame where cache is stale (resize, new msg, theme) | **HIGH** | Incremental / viewport-only re-render |
| 6 | `crates/agent-tui/src/tui/draw.rs:559–590` | # plugin commands | Every frame where input starts with `/` | **MEDIUM** | Cache command list; amortize alloc |
| 7 | `crates/agent-tui/src/tui/app.rs:330–384` | Input length (O(n) char scan) | Every keystroke (cursor ops, wrap calc) | **MEDIUM** | Maintain byte↔char index; width cache |
| 8 | `src/watcher/display.rs:198–226` | Log file size | `watcher logs --follow` | **MEDIUM** | Byte-offset seek, not full re-read |
| 9 | `crates/agent-tui/src/tui/highlight.rs:56–83` | Code block size × syntect cost | Every cache rebuild (stale line_cache) | **MEDIUM** | Per-block highlight cache keyed by (text, lang, width) |
| 10 | `src/cmd/chat.rs:147` | # sessions × header read | `synaps chat` → `/sessions` (headless, blocks stdin loop) | **MEDIUM** | Same as #2; async + bounded |

---

## Detailed Findings

---

### \#1 — CRITICAL: `/extensions audit` reads entire audit.jsonl into memory

**File:** `crates/agent-tui/src/tui/mod.rs:1136`
**Callsite:** `synaps_cli::extensions::audit::read_audit_entries()` →
**Impl:** `crates/agent-engine/src/extensions/audit.rs:122`

```rust
// audit.rs:122
let contents = match std::fs::read_to_string(&path) {
```

**What happens:**
- `read_to_string` slurps the entire `extensions/audit.jsonl` file, then collects every entry into a `Vec<ProviderAuditEntry>`.
- The file is append-only and unbounded. With daily use over months it can grow to MB+.
- The call is synchronous, on the Tokio event loop (inside the `InputAction::SlashCommand` arm of `mod.rs:615`).
- `tail` slicing (`mod.rs:1139`) happens *after* reading and deserializing all entries — allocation is `O(file_size)` regardless of the requested tail size.

**Scales with:** audit log file size (unbounded append-only log).
**Trigger:** `/extensions audit [N]` — user-invoked, but even `/extensions audit 10` reads the full file.
**Severity:** **CRITICAL** — large log → TUI freeze. Sync I/O on event loop.

**Fix:** Seek to the end of the file, walk backward for N newlines (the file is newline-delimited JSON), read only those bytes. For the "all entries" case, add a reasonable cap (e.g. 1 000 entries) and document it. Use `spawn_blocking` so the event loop is not stalled.

---

### \#2 — CRITICAL: `/sessions` calls `list_sessions()` synchronously on the UI thread

**File:** `crates/agent-tui/src/tui/commands.rs:457`
**Impl:** `crates/agent-core/src/core/session.rs:266`

```rust
// commands.rs:457
"sessions" => {
    match list_sessions() {
```

**What happens:**
- `list_sessions()` opens *every* session file, reads up to 256 KB of each (the header-extraction loop), parses the JSON metadata, and sorts the result.
- With 221 files that is 221 synchronous `File::open` + partial `Read` calls + `serde_json::from_str` parses, all on the Tokio async task (no `spawn_blocking`).
- The result is displayed but not cached — every invocation of `/sessions` repeats the full scan.

**Scales with:** # sessions on disk.
**Trigger:** `/sessions` — synchronous blocking I/O inside an async context.
**Severity:** **CRITICAL** — 221 files × syscall + JSON parse cost freezes the event loop; no redraw possible during the scan.

**Fix:**
1. Move into `tokio::task::spawn_blocking` so the event loop keeps ticking.
2. Cache the result in `App` with a dirty flag (invalidate on `/clear`, `/resume`, `/saveas`).
3. Optionally: maintain a small index file (ids + titles + costs) updated on session save — O(1) list with no per-file reads.

**Same issue in:** `src/cmd/chat.rs:147` (headless `/sessions` blocks the stdin `read_line` loop, same `list_sessions()` call).

---

### \#3 — HIGH: `resolve_session()` calls `list_sessions()` twice for named sessions

**File:** `crates/agent-core/src/core/session.rs:390–413`

```rust
// session.rs:390
pub fn resolve_session(query: &str) -> std::io::Result<Session> {
    if let Ok(ptr) = crate::core::chain::load_chain(query) { … }   // cheap
    if let Ok(s) = find_session_by_name(query) { … }               // calls list_sessions() #1
    find_session(query)                                              // does NOT call list_sessions
}

// find_session_by_name (session.rs:376)
pub fn find_session_by_name(name: &str) -> std::io::Result<Session> {
    let sessions = list_sessions()?;   // full header scan #1
    …
}
```

Additionally, when `/resume` is used after successful load (`mod.rs:507`):

```rust
// mod.rs:507–512
let via = if synaps_cli::chain::load_chain(arg).is_ok() {          // disk read
    …
} else if synaps_cli::session::find_session_by_name(arg).is_ok() { // calls list_sessions() #2
    …
```

**What happens:**
- For a named session, `list_sessions()` is called once in `find_session_by_name` (inside `resolve_session`), then again in `find_session_by_name` called from the "via" annotation block in `mod.rs:509`.
- Total: 2× full directory scan for a single `/resume <name>`.

**Scales with:** # sessions.
**Trigger:** `/resume <name>` where `name` is a named session (not a chain, not a partial ID).
**Severity:** **HIGH** — 2× the already-expensive list_sessions scan; sync on event loop.

**Fix:** Thread the resolution through — return the matched name/chain info from `resolve_session` so the caller doesn't need to re-scan. Cache the list between the two calls in the same stack frame.

---

### \#4 — HIGH: `save_chain()` scans all sessions to check for name collision

**File:** `crates/agent-core/src/core/chain.rs:45`

```rust
// chain.rs:45
pub fn save_chain(name: &str, head: &str) -> io::Result<()> {
    if let Ok(sessions) = crate::session::list_sessions() {   // full scan
        if sessions.iter().any(|s| s.name.as_deref() == Some(name)) {
            tracing::warn!(…);
        }
    }
```

**What happens:**
- Every `/chain name <name>` command triggers a full `list_sessions()` scan just to emit a `tracing::warn!` about a potential name collision — a cosmetic advisory, not a hard guard.
- This is a write path that goes through the same O(#sessions) scan as the read path.

**Scales with:** # sessions.
**Trigger:** `/chain name <name>`.
**Severity:** **HIGH** — unnecessary full scan on a write command; blocks event loop.

**Fix:** Remove the guard or defer it. Chain names and session names occupy different namespaces; a session with the same name doesn't break anything — the resolver prefers chains (documented in `resolve_session`). If the warning must stay, make it async or move it behind a `spawn_blocking`.

---

### \#5 — HIGH: Per-frame full message re-render (`render_lines`) on any stale condition

**File:** `crates/agent-tui/src/tui/draw.rs:467`
**Called from:** `build_render_model` (called every frame when `needs_redraw` is set)

```rust
// draw.rs:466–469
let needs_rebuild = app.line_cache.as_ref().map_or(true, |(w, _)| *w != content_width);
if needs_rebuild {
    let lines = app.render_lines(content_width);   // O(messages)
```

**What `render_lines` does (render.rs:14–end):**
- Iterates every `ChatMessage` in `app.messages`.
- For each `ChatMessage::Text`, calls `render_markdown(text, m, width)` — full markdown parse + table layout + word-wrap for the entire message body every time.
- For each `ChatMessage::ToolResult` with read-tool output, calls `highlight_read_output` → syntect `HighlightLines::highlight_line` per line.
- For each `ChatMessage::ToolUse`, calls `highlight_tool_code` → syntect per-line.

**Cache invalidation is too broad:**
- `app.invalidate()` is called on: every `push_msg`, every `push_chunk`, every scroll event that changes `scroll_pinned`, every terminal resize (changes `content_width`), every theme change, every animation tick that touches messages (e.g. active ToolUseStart spinner).
- On resize: all `N` messages are re-rendered from scratch via syntect.
- During streaming: every new chunk calls `push_msg` / `invalidate`, triggering a full re-render of all prior messages plus the new delta.

**Scales with:** # messages in `app.messages` (display buffer, capped at 120 on resume but grows without bound during a session).

**Trigger:** Any cache invalidation — resize, new message, theme change, `show_full_output` toggle. During streaming: up to 60× per second.

**Severity:** **HIGH** — with a long session (hundreds of messages, large tool outputs) a single resize hangs the terminal visibly. During streaming at 60fps it burns CPU proportional to conversation length.

**Partial mitigation already in place:** `cap_resumed_display(120)` (app.rs:464) caps the display buffer at resume. But within a live session the buffer grows unboundedly, and the cap is bypassed once the session grows past 120 display messages naturally.

**Fix direction:** Viewport virtualization (#98 referenced in app.rs:463): only render the lines visible in the viewport ± a small overscan buffer. Per-message render cache keyed by `(message_index, content_width, theme_id)` so only the changed/new message is re-rendered. The `ToolUseStart` spinner should not invalidate the full cache — it only changes the last message's header line.

---

### \#6 — MEDIUM: Ghost-hint and tab-complete call `all_commands_with_skills()` every keystroke / frame

**Files:**
- `crates/agent-tui/src/tui/draw.rs:562` — ghost hint computed in `build_render_model` (every frame when input starts with `/`)
- `crates/agent-tui/src/tui/input.rs:411, 539, 584` — `all_commands_with_skills(registry)` called in keybind dispatch, submit, and tab-complete

```rust
// draw.rs:562
let commands = super::commands::all_commands_with_skills(registry);
```

**What `all_commands_with_skills` / `all_commands()` does (`registry.rs:409`):**
```rust
pub fn all_commands(&self) -> Vec<String> {
    let mut v: Vec<String> = self.builtins.iter().map(|s| s.to_string()).collect();
    v.extend(r.skills.keys().cloned());
    v.extend(r.plugin_commands.keys().cloned());
    v.extend(r.lifecycle_claims.keys().cloned());
    v.sort();
    v.dedup();
    v
}
```
- Acquires an `RwLock` read guard, clones all keys from 4 collections, allocates a `Vec<String>`, sorts, deduplicates — every time.
- Called from `build_render_model` on every frame where the input buffer starts with `/`, and on every keypress in `handle_key` → `process_submit`.

**Scales with:** # registered plugin commands.
**Trigger:** Every keystroke while typing a `/` command; every frame during that same input.
**Severity:** **MEDIUM** — small lock + sort + Vec alloc repeated 60× per second. Degrades with large plugin registries.

**Fix:** Cache the sorted command list in `App` (or in `CommandRegistry` behind a `OnceCell`/dirty flag); invalidate only when the registry is mutated (plugin load/reload). The registry is immutable after boot in normal use.

---

### \#7 — MEDIUM: `cursor_byte_pos()` and `input_char_count()` are O(input length) on every keystroke

**File:** `crates/agent-tui/src/tui/app.rs:330–341`

```rust
// app.rs:330
pub(crate) fn cursor_byte_pos(&self) -> usize {
    self.input.char_indices()
        .nth(self.cursor_pos)            // O(cursor_pos) char scan
        .map(|(i, _)| i)
        .unwrap_or(self.input.len())
}

pub(crate) fn input_char_count(&self) -> usize {
    self.input.chars().count()           // O(input_len) always
}
```

**`input_wrap_info` (app.rs:344)** iterates all chars of `self.input` to compute wrap layout — called from `build_render_model` (draw.rs:433) every frame.

**Scales with:** Length of text in the input box.
**Trigger:** Every keystroke (backspace, char insert, cursor movement) and every frame while input is non-empty.
**Severity:** **MEDIUM** — for typical input (<500 chars) this is fine. For large pastes (100 000 chars cap at `input.rs:224`) or multi-line documents typed into the box, all three become measurably slow.

**Fix:**
- Track `input_char_count` as a maintained field (`cursor_pos` is already maintained; length can be maintained alongside it).
- Replace `cursor_byte_pos()` with a maintained `cursor_byte_pos: usize` field that is updated atomically with `cursor_pos` on every mutation — eliminates the O(n) scan.
- `input_wrap_info`: cache the result keyed by `(input.len(), cursor_pos, content_width)`; invalidate only on input change.

---

### \#8 — MEDIUM: `watcher logs --follow` re-reads entire log file on every poll tick

**File:** `src/watcher/display.rs:198–226`

```rust
// display.rs:214
if let Ok(contents) = tokio::fs::read_to_string(&follow_path).await {
    let new_content = &contents[(last_size as usize)..];   // slice into full string
```

**What happens:**
- Every 500ms, the whole log file is read into memory via `read_to_string`.
- Only the bytes from `last_size` onward are printed, but the full allocation happens regardless.
- As the log grows (long agent session), memory allocation and I/O work grow linearly.

**Scales with:** Log file size.
**Trigger:** `watcher logs <name> --follow` (poll loop, not event-driven).
**Severity:** **MEDIUM** — for large/long-running agents the log can be MB+; 2× per second full-file alloc.

**Fix:** Open the file once, `seek(SeekFrom::Start(last_size))`, and read only the delta. Use `tokio::fs::File` + `AsyncSeekExt::seek`. Alternatively, use `notify` (already a crate dep) to watch for inotify `MODIFY` events instead of polling.

---

### \#9 — MEDIUM: Syntect highlight runs on every `render_lines` rebuild for unchanged code blocks

**File:** `crates/agent-tui/src/tui/highlight.rs:56–83` (`highlight_code_block`)
**Called from:** `render.rs` (inside `render_markdown`, via `markdown.rs:459`)
**Also:** `highlight_read_output` (highlight.rs:297), `highlight_tool_code` (highlight.rs:88)

```rust
// highlight.rs:64
let mut h = HighlightLines::new(syntax, theme);
for line in LinesWithEndings::from(code) {
    let ranges = h.highlight_line(line, ss).unwrap_or_default();   // syntect tokenize
```

**What happens:**
- Syntect's `highlight_line` is a full tokenizer pass. It is not cheap (regex-based PEG grammar).
- Every time `line_cache` is invalidated and `render_lines` is called, every code block in every message is re-highlighted from scratch — even messages from 100 turns ago.
- There is no per-block highlight cache; the only cache is the coarse `line_cache` (all messages at a given width).

**Scales with:** # code blocks × lines per block × # messages.
**Trigger:** Any `invalidate()` call that misses the `content_width` guard (resize, new message, show_full_output toggle).
**Severity:** **MEDIUM** — a session with many large code blocks (file writes, bash outputs) makes each `render_lines` call noticeably slow.

**Fix:** Cache per-block highlight output keyed by `(text_hash, lang, width)`. Since message content is immutable after creation, this cache can be a `HashMap` with no eviction other than session change. The `line_cache` is already the right structure; the missing piece is a sub-cache for expensive sub-operations within it.

---

### \#10 — MEDIUM: `/chain` walks full parent chain loading every session file synchronously

**File:** `crates/agent-tui/src/tui/mod.rs:778–792`

```rust
// mod.rs:779–791
let mut current_parent = app.session.parent_session.clone();
while let Some(ref parent_id) = current_parent {
    match synaps_cli::core::session::Session::load(parent_id) {   // full JSON parse
        Ok(parent) => {
            let msg_count = parent.api_messages.len();             // deserializes api_messages array
            …
            current_parent = parent.parent_session.clone();
        }
```

**What happens:**
- `Session::load` (session.rs:137) calls `std::fs::read_to_string` + full `serde_json::from_value` on the entire session file, including the `api_messages` array.
- For a compaction chain of depth `D`, this reads and fully deserializes `D` session files (each potentially multi-MB).
- The chain walk is synchronous on the event loop — no `spawn_blocking`.
- `parent.api_messages.len()` forces deserialization of the entire message array just to count it.

**Scales with:** Compaction chain depth × average session file size.
**Trigger:** `/chain` (bare) — user-visible chain display command.
**Severity:** **MEDIUM** — a session with 5 compaction generations (common with long projects) reads and deserializes 5 × multi-MB files synchronously.

**Fix:** 
1. Add `message_count: usize` to the session header (persisted on save) so `list_sessions` / `read_session_header` can return it without parsing `api_messages`.
2. Alternatively: use `read_session_header` (already exists) instead of `Session::load` for the chain walk — it reads only the header and returns the lightweight `SessionInfo` struct.
3. Move into `spawn_blocking`.

---

## Bug-class Map

| Bug class | Instances |
|-----------|-----------|
| Blocking I/O on UI/event-loop thread | #1, #2, #3, #4, #10 |
| O(total data) for a bounded need | #1 (audit), #2 (sessions), #8 (log follow) |
| Re-doing expensive work (no cache) | #5 (render), #6 (command list), #9 (syntect) |
| Per-frame/per-keystroke O(n) over growing collection | #5, #6, #7 |
| Loading everything when a slice suffices | #1, #8, #10 |

---

## Excluded / Already-Fixed

- `latest_session()` — **already fixed** (session.rs:221–255): uses file mtime, reads only one file. The comment documents the old O(total-data) bug and the fix.
- `list_sessions()` header extraction — **already partially fixed** (session.rs:313–351): `read_session_header` stops at `"api_messages"` key. Still O(#sessions) directory scans, and still synchronous.
- `cap_resumed_display(120)` — **already mitigated** (app.rs:464): caps display buffer at resume, preventing the worst-case `render_lines` cost on `--continue`. Does not address within-session growth.
