# Boot / Extensions / Skills / Plugins / MCP — Perf & Scaling Audit

**Branch:** `dev`  **Scope:** boot critical path → first paint  **Method:** read-only static scan (no builds).

The bug-class we're hunting: *"O(total data) work for a small need"*, same shape as the `latest_session()` 76 MB-on-boot bug — but in the skills / extensions / tools / MCP loaders. The user reloads constantly with `jawz --continue`, so every ms in `boot()` is felt.

The boot critical path (`crates/agent-engine/src/engine/setup.rs::boot`) does, in order:
1. `apply_config` (already cached config)  → cheap
2. `resolve_system_prompt`                  → 1 file read
3. **`crate::skills::register(...)`** *(line 121)* — the 1.4 s suspect
4. `crate::mcp::setup_lazy_mcp(...)` *(line 124)* — 1 file read, lazy ✓
5. `resolve_or_create_session` *(line 129)* — already optimised via mtime+header
6. inbox watcher + socket listener spawn — async, non-blocking ✓
7. `ExtensionManager::new_with_tools` *(line 178)* — constructor only
8. Session start hook

Extension discovery itself is **off the critical path** (`extensions::loader::spawn_discover_and_load` is fired by the renderer, not by `boot()`), but it still serially blocks completion of "TUI ready with extensions" — the user sees the progress toast for the full duration.

---

## Ranked findings

### 1. 🔴 CRITICAL — `skills::register` does fully-synchronous, blocking `std::fs` walk on a tokio worker thread
**File:** `crates/agent-engine/src/skills/mod.rs:72`
calling → `crates/agent-engine/src/skills/loader.rs:92` `load_all`
→ `loader.rs:125` `walk_root` (recursive across **4 roots**: `.synaps-cli/plugins`, `.synaps-cli/skills`, `~/.synaps-cli/plugins`, `~/.synaps-cli/skills`)

- `loader.rs:46`  `std::fs::read_to_string(skill_md)` — per skill
- `loader.rs:56`  `canonicalize()` of skill dir — per skill
- `loader.rs:58`  `canonicalize()` of plugin root — **per skill (not cached)**
- `loader.rs:71`  `canonicalize()` of SKILL.md path — per skill
- `loader.rs:135` `read_to_string(marketplace.json)` + `serde_json::from_str` — per marketplace
- `loader.rs:160` `read_dir(root)` for plugin-subdir pass
- `loader.rs:165` **recursive walk** of any subdir that *also* has a marketplace.json
- `loader.rs:175` second `read_dir(loose_dir)` pass over the *same* root + `root/skills/`
- `loader.rs:202` `read_to_string(plugin.json)` — per plugin
- `loader.rs:211` `canonicalize(plugin_root)` — per plugin
- `loader.rs:227` `read_dir(skills_dir)` — per plugin

**Trigger:** every `boot()`, on the critical path, **before TUI first paint**. Same code path is reused by `/sessions`-style reloads via `reload_registry` (mod.rs:127).
**Why it's the prime 1.4 s suspect:** sync `std::fs` blocking the runtime's executor thread + per-skill canonicalize + multi-root walk + the "if subdir has marketplace.json recurse into it" path that double-walks marketplace clones. With ~10 plugins × ~3 skills each that's 30+ `read_to_string` + 60+ `canonicalize` + 8+ `read_dir`, all serial.
**Severity:** Critical — directly delays first paint.
**Fix direction:**
- Wrap `loader::load_all` in `tokio::task::spawn_blocking` so it stops parking the async runtime.
- Parallelize the per-plugin work with `rayon::par_iter` (CPU-light, IO-bound — bounded thread pool).
- Cache plugin-root canonicalization once per plugin and reuse for its skills (eliminate the per-skill `canonicalize` at `loader.rs:58`).
- Cache the discovery result with a checksum of `(root_path, root_mtime)`; skip re-walking if unchanged. Boot just consumes the cache; a background task validates it after first paint.
- Defer marketplace.json + plugin.json *body* parsing — store path + mtime, lazy-parse only when `/plugins` or a slash command actually needs the manifest.

### 2. 🟠 HIGH — Extensions loaded **strictly serially** with spawn + `initialize` handshake awaited per plugin
**File:** `crates/agent-engine/src/extensions/manager.rs:906-1037`
```rust
for (plugin_name, plugin_dir) in plugin_dirs {
    ...
    match self.load_with_cwd(&plugin_name, &resolved, Some(plugin_dir.clone())).await {
```
→ `manager.rs:212` `ProcessExtension::spawn_with_cwd(...).await`
→ `manager.rs:216` `process.initialize(cwd, config).await` (JSON-RPC round-trip; `runtime/process.rs:1125`)

**Trigger:** every boot, in the `spawn_discover_and_load` background task (`extensions/loader.rs:36`). Off the *first-paint* critical path, but blocks the "extensions ready" toast and any tool calls that depend on extension tools.
**Cost:** 7 extensions × (fork + JSON-RPC handshake) ≈ 7× ~200 ms when it could be ~200 ms total.
**Severity:** High — user-visible loading spinner duration.
**Fix direction:** Replace the `for` with a `tokio::task::JoinSet` (or `futures::stream::FuturesUnordered`) with a sane concurrency cap (4–8). Each plugin's spawn+initialize is independent.

### 3. 🟠 HIGH — `plugins.json` re-read + re-parsed *per plugin* inside the discovery loop
**File:** `crates/agent-engine/src/extensions/manager.rs:911`
```rust
if let Some(message) = installed_plugin_setup_failure(&plugin_name) {
```
→ `manager.rs:27` `installed_plugin_setup_failure` → `skills/state.rs:116` `PluginsState::load_from` does `std::fs::read_to_string(plugins.json)` + `serde_json::from_str`.

**Trigger:** N times per extension discovery (once per plugin dir found). 7 plugins → 7 full reads + parses of the *same* file every boot.
**Severity:** High (classic "read-all-and-parse on a hot loop").
**Fix direction:** Hoist `PluginsState::load_from` out of the loop into a single call; pass a `&PluginsState` (or a precomputed `HashMap<name, SetupStatus>`) into the loop.

### 4. 🟡 MEDIUM — `config::load_config()` re-read inside the extension loop
**File:** `crates/agent-engine/src/extensions/manager.rs:905`
```rust
let disabled_plugins = crate::config::load_config().disabled_plugins;
```
`setup.rs:112` already loaded the config and held it in `EngineBoot`. The extension manager re-loads from disk because it doesn't have a handle to the cached value.
**Severity:** Medium — only one extra read, but it's a redundant blocking disk I/O during a hot loading phase.
**Fix direction:** Plumb the `&SynapsConfig` (or just `disabled_plugins`) through `ExtensionManager::new_with_tools`, or have `discover_and_load_with_progress` take a config snapshot parameter.

### 5. 🟡 MEDIUM — `ToolRegistry::register` rebuilds the full schema on **every** call (O(n²) over the boot session)
**File:** `crates/agent-engine/src/tools/registry.rs:260-264`
```rust
pub fn register(&mut self, tool: Arc<dyn Tool>) {
    let name = tool.name().to_string();
    self.tools.insert(name, tool);
    self.rebuild_schema();   // ← sort + sanitize ALL tools, every time
}
```
`rebuild_schema` (registry.rs:227) sorts and `sanitize_schema`s every tool. The doc-comment at registry.rs:105-107 *explicitly calls this out* for the constructor path but **the public `register()` still has the bug**.

Callers that hit it in a loop on boot:
- `crates/agent-engine/src/extensions/manager.rs:298-301` — per-tool inside the per-extension load (T tools across E extensions → T rebuilds)
- `crates/agent-engine/src/mcp/mod.rs:90` — per-tool in `connect_mcp_servers` (the eager path; lazy path is safe)
- `crates/agent-engine/src/skills/mod.rs:121`, `crates/agent-engine/src/mcp/mod.rs:135` — one-shot each (cheap)

**Severity:** Medium today (~17 built-ins + a few extension tools), but scales quadratically the moment someone connects an MCP server with many tools, or a plugin registers many tools.
**Fix direction:** Add `register_many(impl IntoIterator<Item=Arc<dyn Tool>>)` that inserts all then calls `rebuild_schema()` once. Use it at extensions/manager.rs:299 and mcp/mod.rs:76.

### 6. 🟡 MEDIUM — `canonicalize()` called 2-3× per SKILL.md, ignoring per-plugin caching
**File:** `crates/agent-engine/src/skills/loader.rs:56,58,71`
For every skill: canonicalize `parent`, canonicalize `plugin_root` (recomputed per skill even though all skills under a plugin share it), canonicalize `source_path`. `canonicalize` is a syscall-per-component path-walk.
**Severity:** Medium — multiplier effect with #skills.
**Fix direction:** Compute `plugin_root.canonicalize()` once in `load_plugin` (loader.rs:211 already does it) and thread the result into `load_skill_file` instead of re-canonicalizing per skill.

### 7. 🟡 MEDIUM — Marketplace + plugin-subdir paths can double-walk the same tree
**File:** `crates/agent-engine/src/skills/loader.rs:152-170`
The marketplace pass loads each plugin via `entry.source` (`loader.rs:142`). The plugin-subdir pass then re-`read_dir`s the same root and, for any subdir that *also* contains a `marketplace.json`, **recursively re-`walk_root`s it** (loader.rs:164-165). The `seen` HashSet dedupes skills, but the disk walk + manifest re-reads still happen.
**Severity:** Medium when users `git clone` marketplaces into `plugins/`.
**Fix direction:** Track visited canonical roots and short-circuit; or split discovery into "enumerate manifest paths" + "parse" phases so the dedupe key (canonical root) is known before re-walking.

### 8. 🟢 LOW — `canonicalize()` per extension manifest arg
**File:** `crates/agent-engine/src/extensions/manager.rs:988-997`
Inside the per-extension loop, every arg gets `arg_path.canonicalize()` + `plugin_dir.canonicalize()` calls. `plugin_dir.canonicalize()` is recomputed for every arg of every plugin instead of once.
**Severity:** Low (few args per ext).
**Fix direction:** Hoist `plugin_dir.canonicalize()` above the arg loop.

### 9. 🟢 LOW — `skills::register` re-walks discovery roots from scratch on every `reload_registry`
**File:** `crates/agent-engine/src/skills/mod.rs:128`
`reload_registry` calls the same `loader::load_all` as boot. Any `/plugins` reload pays the full cost again. Same fix as Finding 1 (cache + invalidate on mtime).

---

## What's already good (don't break these)

- `latest_session()` uses mtime-only, no body parse — `agent-core/src/core/session.rs:221` ✓
- `list_sessions()` reads only the header via `read_session_header` — `agent-core/src/core/session.rs:266` ✓
- MCP uses lazy connect (`mcp/mod.rs:117 setup_lazy_mcp`) — no eager handshakes ✓
- Built-in `ToolRegistry::new` uses `from_tools` to avoid the O(n²) per-register rebuild ✓ (note: only the *constructor* — `register()` still has the issue)
- Extension discovery is off the first-paint critical path via `spawn_discover_and_load` ✓

---

## Suggested implementation order (cost × payoff)

1. **Finding 3** — hoist `plugins.json` load out of the loop. ~10 lines, eliminates N re-reads/parses.
2. **Finding 1 (partial)** — wrap `loader::load_all` in `spawn_blocking`. ~3 lines, immediately unblocks the async runtime.
3. **Finding 2** — `JoinSet` the per-extension load. Halves visible "extensions loading" toast time on multi-extension setups.
4. **Finding 5** — add `register_many`. Linearizes the tool-registry boot cost.
5. **Finding 1 (full)** — discovery cache keyed by `(root, mtime)`. Best long-term fix; the second boot in a row pays ~0 in `skills::register`.
6. Findings 4, 6, 7, 8, 9 — cleanup.
