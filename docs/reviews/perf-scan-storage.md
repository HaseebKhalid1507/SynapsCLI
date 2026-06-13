# Storage / Persistence Layer — Perf & Scaling Audit

Scope: `crates/agent-core/src/` (read-only review).
Scan date: dev branch.
Bug class under audit:

1. **Read-all-and-parse** — `read_dir` + `read_to_string` + `serde_json::from_str` on every file in a dir when only metadata / latest / count is needed.
2. **Full deserialize when a subset suffices** — `IgnoredAny` still tokenizes; not free.
3. **O(n)/O(n²) over growing collections** on a hot path (boot, every command, every message).
4. **Repeated full loads** — no caching, redone per call.
5. **Synchronous blocking I/O** on a frequent path.

Confirmed already-fixed:
- `core/session.rs:221 latest_session()` — fixed (mtime-based, no reads/parses).
- `core/session.rs:259 list_sessions()` — in-flight fix.

Disk reality that makes this matter: ~/.synaps-cli/sessions = **221 files / 76 MB JSON**; sessions only grow; a single multi-MB session is normal.

---

## Findings — ranked by severity

### F1 — CRITICAL — `find_session_by_name()` calls `list_sessions()`
**Location:** `core/session.rs:330–341`
```rust
pub fn find_session_by_name(name: &str) -> std::io::Result<Session> {
    let sessions = list_sessions()?;   // ← reads + serde-parses every session file (76 MB)
    for s in &sessions { if s.name.as_deref() == Some(name) { return Session::load(&s.id); } }
    ...
}
```
**What it scans/parses:** every `*.json` under sessions dir, serde-tokenizes whole file (the `IgnoredAny api_messages` field tokenizes the entire message array — it does NOT skip bytes; it parses+discards).
**Trigger:** `resolve_session()` (`session.rs:363`) → boot path for `--continue <name>` / `--session <name>` / any name-resolve; also TUI `commands.rs:509` (every arg parse that might be a session name) and engine `setup.rs:270`.
**Scales with:** total sessions × avg session bytes (currently 76 MB). Same shape as the original 11s boot bug.
**Severity:** **CRITICAL** — user-facing boot latency when query is a name.
**Fix direction:** Maintain a `name → id` index file (single small JSON updated on `set_name` / `clear_name` / compaction). Or scan filenames only + open the few candidates' header bytes. Or piggy-back on the upcoming sessions index used by `list_sessions()` fix — gate on a cached metadata table.

---

### F2 — CRITICAL — `Session::set_name()` calls `list_sessions()` for uniqueness check
**Location:** `core/session.rs:158–180` (line 163)
```rust
let sessions = list_sessions()?;       // ← 76 MB read+parse, every /name
for s in &sessions { if s.name.as_deref() == Some(name) && s.id != self.id { ... } }
```
**Trigger:** every `/name <foo>` command (or any path that assigns/renames).
**Scales with:** total session bytes on disk.
**Severity:** **CRITICAL** — user-perceived hang on a single short command. Identical class to the 11s boot bug, just on a different trigger.
**Fix direction:** Same `name → id` index. Lookup is O(1); collision check is one map probe.

---

### F3 — CRITICAL — `save_chain()` calls `list_sessions()` for a soft warning
**Location:** `core/chain.rs:41–62` (lines 45–52)
```rust
if let Ok(sessions) = crate::session::list_sessions() {
    if sessions.iter().any(|s| s.name.as_deref() == Some(name)) {
        tracing::warn!("chain name '{}' also used by a session ...", name);
    }
}
```
**Trigger:** every `/chain save`. Also called from compaction flow (`tui/mod.rs:455`) indirectly via auto-advance paths that re-save chain pointers.
**Scales with:** total session bytes — same 76 MB read+parse, just to emit one log line.
**Severity:** **CRITICAL** — the cost is **entirely overhead for a non-critical warning**. Worst cost-to-value ratio in the file.
**Fix direction:** Use the same `name → id` session-name index. If no index: defer to a background task; or drop the warning (it's advisory).

---

### F4 — CRITICAL — `resolve_session()` fallthrough hits `find_session_by_name`
**Location:** `core/session.rs:345–368`
```rust
if let Ok(ptr) = chain::load_chain(query) { ... }   // chain hit: cheap
if let Ok(s) = find_session_by_name(query) { ... }  // ← F1: 76 MB
find_session(query)                                  // see F5
```
**Trigger:** boot with any non-chain identifier (`--continue <name>`, `--session <partial-id>`), and any TUI command that calls `resolve_session`.
**Scales with:** total session bytes — F1 transitively.
**Severity:** **CRITICAL** — same boot-latency surface as the original 11s bug, just gated on the input being a name rather than `--continue`.
**Fix direction:** Reorder so the cheap partial-ID exact path (see F5 first branch) runs before the name path; back name lookup with an index (F1's fix).

---

### F5 — HIGH — `Session::save()` rewrites the entire session JSON every turn
**Location:** `core/session.rs:124–133`
```rust
pub async fn save(&self) -> std::io::Result<()> {
    ...
    let json = serde_json::to_string(self).map_err(...)?;   // ← serializes ALL api_messages
    tokio::fs::write(&tmp, &json).await?;
    tokio::fs::rename(&tmp, &path).await
}
```
**What it does:** serializes the whole `Session` (including the full `api_messages: Vec<Value>` history) and writes it atomically (tmp + rename) **every save**.
**Trigger:** every assistant turn / tool result / message append. Hottest write in the system.
**Scales with:** session length. A 5 MB session = serialize 5 MB + write 5 MB on every message. By turn N, cumulative I/O is O(N²) bytes per session — this is *why* sessions reach multi-MB and *why* the F1–F4 reads are so painful (they hit files this method bloated).
**Severity:** **HIGH** — silent quadratic; per-message latency creep; root cause of the 76 MB disk footprint that makes F1–F4 critical.
**Fix direction:** Split storage: small header (`<id>.json` with metadata + cost + name + counters) and append-only message log (`<id>.messages.jsonl`). Save = append last message + small header rewrite. Reads for listing already only want the header; reads for replay stream the log. This single change collapses F1–F4 *and* fixes the quadratic.

---

### F6 — HIGH — `find_session()` does directory scan on partial-ID lookups
**Location:** `core/session.rs:188–218`
```rust
let exact = dir.join(format!("{}.json", partial_id));
if exact.exists() { return Session::load(partial_id); }      // cheap path ✓
for entry in std::fs::read_dir(&dir)? {                       // O(n) filenames
    if id.contains(partial_id) { matches.push(id.to_string()); }
}
```
**What it does:** read_dir only — *no* JSON parse. So the cost is filename enumeration, not byte parse.
**Trigger:** `resolve_session()` fallthrough on partial ID; same hot paths as F4.
**Scales with:** session **count** (cheap: ~221 dir entries), not bytes.
**Severity:** **HIGH** *only because it sits on the boot path*; in absolute terms cheap. The substring (`contains`) match is also semantically weak — most callers want prefix.
**Fix direction:** Use prefix match (`starts_with`) for O(log n) on a sorted index; or accept current cost since it's filenames-only. Note: this is what F4's fast path *should* try before falling into F1.

---

### F7 — HIGH — `memory::query_in()` scans whole namespace file every query
**Location:** `memory/store.rs:141–193`
```rust
let reader = BufReader::new(f);
for line in reader.lines() {                        // every record, every query
    let rec: MemoryRecord = serde_json::from_str(&line)?;
    // filter (substring, tag prefix, since/until)
    out.push(rec);
}
out.sort_by_key(|b| std::cmp::Reverse(b.timestamp_ms));   // sort AFTER full read
out.truncate(limit);                                       // discards most of work
```
**Trigger:** every `memory.query` RPC call from plugins / agent.
**Scales with:** records in namespace. Reads from the **start** of the file and only sorts/truncates at the end, so `limit=50` over a 100k-record namespace still parses 100k records.
**Severity:** **HIGH** — unbounded growth, hot RPC. Currently survives because namespaces are young; will degrade silently as memory usage grows.
**Fix direction:** Read file backwards (mmap + reverse-line iterator, or `BufReader::seek_relative` from end). Stop when `out.len() == limit` AND time-window/tag filters are exhausted. Optional: maintain a per-namespace tail index (offsets of last N records).

---

### F8 — MEDIUM — `session_index::read_recent()` slurps whole jsonl
**Location:** `core/session_index.rs:69–115`
```rust
let contents = std::fs::read_to_string(path)?;     // reads ENTIRE file
for line in contents.lines().rev().take(limit) {   // .lines() needs full string
    ...
}
```
**Trigger:** anywhere recent-activity is requested (status panel / boot context).
**Scales with:** total lifetime sessions × 2 (start + end records). Unbounded; currently small (~hundreds of bytes per record) so latency is in the ms range, but the growth shape is the same class as F7.
**Severity:** **MEDIUM** — will not bite at 221 sessions, will at 50k.
**Fix direction:** Reverse-stream: open file, seek to end, read backwards in chunks until `limit` newlines found. Or maintain a rotating index file (cap last 10k records, roll older into a yearly archive).

---

### F9 — MEDIUM — `list_chains()` + `find_chain_by_head` / `find_all_chains_by_head` re-read all chain files
**Location:** `core/chain.rs:70–105`
```rust
pub fn list_chains() -> io::Result<Vec<NamedChain>> {
    for entry in std::fs::read_dir(&dir)? {
        let content = std::fs::read_to_string(&path)?;
        let ptr: ChainPointer = serde_json::from_str(&content)?;
        ...
    }
}
pub fn find_chain_by_head(...)        { list_chains()?.into_iter().find(...) }
pub fn find_all_chains_by_head(...)   { list_chains()?.into_iter().filter(...) }
```
**Trigger:** `find_all_chains_by_head` called on compaction (`tui/mod.rs:455`, `:814`) and on `/chain list`.
**Scales with:** number of named chains. Chain files are tiny (<100 bytes) and user-named → bounded in practice (dozens).
**Severity:** **MEDIUM** — fine today; the function is correct, just unindexed. The repeated `list_chains()` call inside both `find_chain_by_head` and `find_all_chains_by_head` is wasteful when a head→chain reverse index would be O(1).
**Fix direction:** Build an in-memory `BTreeMap<head_session_id, Vec<chain_name>>` lazy-loaded once per process; invalidate on `save_chain` / `delete_chain`.

---

### F10 — MEDIUM — `load_config()` is re-parsed on every call, no caching
**Location:** `core/config.rs:431–561`; consumers throughout TUI/engine.
- `core/config.rs:627 add_favorite_model` → `load_config()`
- `core/config.rs:637 remove_favorite_model` → `load_config()`
- `core/config.rs:644 is_favorite_model` → `load_config()` (called **per model** when rendering the model picker — see `tui/models/mod.rs:196` building a `BTreeSet` from `load_config().favorite_models` and `:212`, `:238` re-loading per refresh)
- `engine/setup.rs:112` once at boot (acceptable)
- `extensions/manager.rs:905` for disabled plugins
- `tui/app.rs:228` `agent_name`
- `tui/input.rs:176`, `tui/settings/mod.rs:108` …

**What it does:** opens config file, `read_to_string`, line-parses the whole thing into a `SynapsConfig`. File is small (~KB) so per-call cost is sub-ms — but the file is read **synchronously** on every call, sometimes multiple times per UI frame.
**Trigger:** model picker open, every favorite-toggle, every settings draw, every `is_favorite_model` check.
**Scales with:** call frequency (not data). Constant-time bad, not super-linear bad.
**Severity:** **MEDIUM** — sloppy but small. Worth fixing because `is_favorite_model(id)` called inside a `.iter().any(...)` loop is a textbook N×re-read foot-gun waiting to happen.
**Fix direction:** Cache `SynapsConfig` behind `OnceLock<RwLock<…>>` with explicit invalidation on writes (`write_comma_list`, `write_config_value`). The file already publishes `PROVIDER_KEYS` and `IDENTITY` via `OnceLock` — extend the same pattern to the whole struct.

---

### F11 — LOW (note) — `Session::load()` is synchronous on an async caller chain
**Location:** `core/session.rs:135–140`
```rust
pub fn load(id: &str) -> std::io::Result<Self> {
    let content = std::fs::read_to_string(path)?;    // sync blocking I/O
    serde_json::from_str(&content).map_err(...)
}
```
**Trigger:** boot (`latest_session`, `resolve_session`), `/sessions` open, compaction flow (loads parent after save).
**Scales with:** session file size (multi-MB; see F5).
**Severity:** **LOW** at boot (one-off), but it blocks the tokio runtime when called from the async TUI loop (e.g. `tui/mod.rs:470`). Pairs with F5 — the sync `serde_json::from_str` over a 5 MB string is the heavy part, not the I/O.
**Fix direction:** `tokio::task::spawn_blocking` for the parse, or migrate to `tokio::fs` + an async-friendly streaming parser. Becomes a real concern once F5 is fixed and sessions stop being giant.

---

## Cross-cutting observation

Three of the four CRITICAL findings (**F1, F2, F3**) are all the **same underlying gap**: there is no session-name → session-id index. Every consumer that needs name resolution re-derives it by reading and parsing every session file on disk. One index file (or one cached map) collapses F1, F2, F3, and F4's fallthrough path simultaneously.

The HIGH finding **F5** is the *cause* of why the CRITICALs hurt so much: `Session::save` rewriting the full message history every turn is what drives the 76 MB disk footprint. Splitting header from message-log neutralizes the per-byte cost of the read-all-and-parse bugs *and* fixes the quadratic write growth in one move.

## Recommended fix order

1. **F1 + F2 + F3 + F4** — introduce `sessions/index.json` (id, name, mtime, model, cost, message_count). Rebuild on first miss. Every read-all-sessions path becomes an index lookup. (Coordinate with the in-flight `list_sessions()` fix — same index.)
2. **F5** — split session storage: header JSON + append-only `.messages.jsonl`. Removes the quadratic; shrinks disk footprint by ~10×.
3. **F7** — reverse-stream `memory::query_in`. Before memory namespaces grow.
4. **F8** — reverse-stream `session_index::read_recent`. Before the jsonl grows.
5. **F9** — cache the chain index in-process.
6. **F10** — cache `SynapsConfig` behind `OnceLock`.
7. **F11** — `spawn_blocking` the session parse (or drop it once F5 lands).

## Out of scope but flagged

- `crates/agent-engine/src/events/registry.rs:127 list_active_sessions_in` — read-all-and-parse over the session-registration directory, plus a side-effect `remove_file` for stale entries. Same bug class; outside the requested scope but worth a follow-up audit.
