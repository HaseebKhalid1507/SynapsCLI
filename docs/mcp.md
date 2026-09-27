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

- only the **exact-lease path** writes (a server the operator configured,
  started for an already gate-authorized call). The legacy server-wide
  `connect_mcp_server` gateway never writes — it is not exact-tool
  authorized and must not be a cache-poisoning bridge;
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
