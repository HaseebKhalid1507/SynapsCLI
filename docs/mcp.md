# MCP servers

Configured in `~/.synaps-cli/mcp.json` (profile variant honoured):

```json
{
  "mcpServers": {
    "fs":         { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] },
    "playwright": { "command": "npx", "args": ["@playwright/mcp"], "shared": true }
  }
}
```

| Field     | Default | Meaning |
|-----------|---------|---------|
| `command` | —       | Executable to spawn (stdio JSON-RPC). |
| `args`    | `[]`    | Arguments. |
| `env`     | `{}`    | Extra environment for the child. |
| `shared`  | `false` | Daemon mode: one child for **every** session in the process (see below). |

## Environment and `${VAR}` expansion

MCP children start with a cleared environment: only `PATH`, `HOME`, `LANG`,
`TERM`, `XDG_RUNTIME_DIR` and `TMPDIR` are passed through, so host secrets
such as `ANTHROPIC_API_KEY` never reach a server by accident. Anything else a
server needs goes in `env`, and an `env` value can forward a variable from the
Synaps process:

```json
"env": { "TOKEN_FILE": "${MY_TOKEN_FILE}", "BASE_URL": "${API_URL:-http://localhost:3000}/v1" }
```

- `${VAR}` expands to the variable's value. If `VAR` is unset it expands to
  an empty string, and a warning naming the variable (never a value) is
  logged.
- `${VAR:-fallback}` uses `fallback` when `VAR` is unset or empty.
- Anything else is literal: `$VAR`, a lone `$`, `${}` and an unterminated
  `${`. There is no escape syntax. Expansion is one level deep; a value that
  itself contains `${…}` is copied as is.

**Daemon mode:** expansion happens in the daemon process, so it uses the
daemon's environment: a copy of the shell that started it (with auto-spawn,
the first client that attached). It does not use the shell you attach from.
A variable you export in a later shell is unset as far as MCP servers are
concerned until you restart the daemon (`synaps daemon stop`). This applies
to every server, `shared` or not.

## Exact leases (progressive disclosure)

Under progressive tool disclosure MCP tools are *dormant* registry entries
built from the descriptor cache; nothing is spawned until a gate-authorized
call needs a server. `McpRuntimeManager` then holds one **lease** per
`(session, server)`: one child process per session per server, initialised
and `tools/list`ed once, every call re-validated against the pinned listing
(name + schema digest), terminated at session end (`McpSessionEndGuard`),
on revocation, or after `idle_max` (default 300 s).

## `shared: true` — one child per process (opt-in)

In daemon mode N sessions × M servers would mean N×M children. A server
marked `shared: true` is instead keyed under the process-wide lease key
`"*"`:

- **one child** serves every session in the daemon;
- **the server sees every session's calls and holds cross-session state**
  (roots, cwd, open browser tabs, …). Only opt in for servers that are
  stateless per call or whose state you *want* shared;
- `terminate_session` / `McpSessionEndGuard` **never** terminate it;
  `reap_idle` (idle past `idle_max`), explicit revocation
  (`revoke_server_lease` on fingerprint drift / listing mismatch — the lease
  is poisoned for everyone) and `terminate_all` (process shutdown) do;
- exactness is unchanged: sharing widens the *child*, never the *grant*.
  Every call still passes the per-session execution gate and the pinned
  digest check;
- `shared` is **excluded from the config fingerprint** — it changes the
  lease key, not the launched process — so flipping it keeps cached
  descriptors valid and simply starts a fresh lease under the other key.

Never the default. In single-session (in-process) mode `shared` only
changes the key string; behaviour is identical.

## Descriptor cache write-back

`~/.synaps-cli/mcp-descriptors.json` is the operator-local descriptor cache
that `setup_lazy_mcp` reads at boot to register dormant tools without
spawning anything. Since daemon-mode phase 2 it is also **written**: after a
lease's first successful `tools/list`, the listing is merged into the cache
under the server name, keyed by the config fingerprint
(`descriptors::record_server_listing`).

Effect: the next boot (daemon restart, or the next `synaps` process)
advertises the same tool schemas as the last run without connecting — the
first turn's tool block, and thus the provider prompt-cache prefix, is
stable across restarts.

Safety properties:

- only two paths write: the **exact-lease path** (a server the operator
  configured, started for an already gate-authorized call), and **boot
  discovery** for a configured server with no fingerprint-matching entry
  (below);
- entries pass the same sanitisation as a load (name bounds, object
  schemas, ≤ 64 KiB per schema, ≤ 256 tools per server, descriptions
  clamped);
- exclusive `fs4` lock on `mcp-descriptors.json.lock` around
  read-merge-write; `0600` file in a `0700` dir; atomic rename. Two daemons
  / profiles / an in-process TUI writing concurrently serialise on the lock;
- an unreadable cache (corrupt, oversize, wrong version) is replaced rather
  than propagated — the reader would have refused it anyway;
- a write-back failure is logged and never surfaces: the lease is already
  live.

**This is on by default in every mode, including the single-session TUI /
plain `synaps chat` (no daemon).** Before: exact mode with no cache
meant "MCP tools are not discoverable this run", every run. After: run 1
writes the cache, run 2 registers dormant tools at boot — the tool list,
system prompt and provider prompt-cache prefix differ between the first and
second run for every MCP user. That is the feature working, but it is the
one default-path behaviour change of daemon-mode phase 2; it is listed in
`docs/daemon-mode.md` under "What changes on the default path".

Kill-switch: `SYNAPS_MCP_CACHE_WRITEBACK=0` (also `false`/`off`) — never
write. The cache remains a supported *input*.

## One MCP path in every mode (legacy gateway removed)

MCP tools are always descriptor-backed registry entries executed through
per-session leases, whatever `progressive_tool_disclosure` says:

- **Progressive on:** they stay dormant until `activate_tools` (or are sent
  deferred on Anthropic, see `request-lifecycle-progressive-disclosure.md`).
- **Progressive off:** they are part of every session's core, sent from the
  first request. The tools list never changes mid-session.

The old `connect_mcp_server` gateway is gone. It registered a server's tools
mid-session, which rewrote the prompt cache, and in the daemon it shared one
connection and one tool registry across every session. The daemon's
"legacy MCP conflict" refusal went with it; `--allow-legacy-mcp` and
`SYNAPS_DAEMON_ALLOW_LEGACY_MCP` are accepted and ignored.

### Boot discovery for new servers

A configured server with no fingerprint-matching descriptor-cache entry (a new
server, or a changed command/args/env) is started **once**: initialize,
`tools/list`, recorded through `record_server_listing` (same lock and
sanitisation as the lease write-back), then stopped. Missing servers are
discovered **in parallel** (at most 8 at once), each bounded to 20 s, so the
worst case is about one timeout, not 20 s per server. A timeout cannot leak a
process because children are `kill_on_drop`. `SYNAPS_MCP_CACHE_WRITEBACK=0`
disables discovery (and the failure records below).

**Failures are negative-cached.** A discovery that errors or times out is
recorded in the same cache file (`discovery_failures`, keyed by server name:
config fingerprint, last attempt time, attempt count) through the same locked,
sanitising write path (`record_discovery_failure`). The server then
contributes no tools and is **not started again** until its backoff expires:
1 h after the first failure, doubling per consecutive failure, capped at
24 h. Boot logs `MCP discovery for '<name>' failed recently; retrying after
<time>` when it skips one. Editing the server's command/args/env (a new
fingerprint) retries immediately; a successful listing removes the entry. The
field is optional and omitted when empty, so the cache format version is
unchanged and older binaries ignore it.

**Where discovery runs.** In-process modes (TUI without daemon, `chat`,
`agent`, `server`) discover during boot (`McpDiscovery::Inline`). The daemon
boots with **cached descriptors only** (`McpDiscovery::Background`): it
signals ready without waiting for any MCP server, and discovers uncached
servers in a background task once it is accepting. When that task finishes,
only the newly discovered servers' tools are added to the shared registry
(one atomic batch; a rejected batch is logged and adds nothing). With
progressive disclosure on they are dormant (searchable/activatable). With it
**off** they are core tools: every live session sends them from its next
request, which costs one prompt-cache rewrite per live session, once — the
same effect as an extension registering late. Sessions started after the
daemon restarts get them from the cache at boot.
