# Daemon mode (`synaps daemon` / `synaps attach`) — phase 2, package B

**Status:** on by default. The first `synaps --attach` / `synaps attach` starts the daemon itself
(jcode model: `setsid` + ready-fd, the daemon holds the flock; on Windows see [Windows](#windows)); once one is running, a plain
`synaps` **adopts** it, spawning the daemon first if needed (thin client, fresh session — `SYNAPS_DAEMON_ADOPT=0` opts out);
`SYNAPS_DAEMON=0` turns every daemon feature off. Protocol v3 (`crates/agent-engine/src/session/wire.rs`), daemon in `agent-engine::daemon`
(no `agent-tui` in its graph — `tests/daemon_no_tui_dep.rs`).

## Flags and kill-switches

| Env | Effect |
|---|---|
| `SYNAPS_DAEMON=0` (also `false`/`off`/`no`) | Disable daemon features: `synaps daemon` and `synaps attach` exit 3 with `daemon disabled by SYNAPS_DAEMON=0`; `--attach` prints `--attach ignored: …` and runs the normal in-process TUI. Unset or `1` = on (`=1` is a no-op kept for older scripts). |
| `SYNAPS_DAEMON_AUTOSPAWN=0` | Disable auto-spawn only: an attach with no live daemon exits 3 with `no daemon running — start it with \`synaps daemon --detach\``. Default: the first client spawns the daemon (`current_exe daemon --foreground`, same profile, inherits env), waits ≤ 15 s (`SPAWN_READY_TIMEOUT`: the daemon's ≤ 10 s extension-discovery wait plus margin) on the ready-fd, prints `starting daemon (pid N) — synaps daemon stop to end it` on stderr, then attaches. A **spawn lock** (`daemon.spawn.lock`) serialises concurrent first-clients: the loser waits, re-probes the flock, attaches to the winner's daemon. If the spawned daemon refuses/dies before it is ready (e.g. the legacy-MCP refusal, exit 3) `--attach` falls back to the in-process TUI with a `daemon unavailable: <reason> — running in-process` notice (exit 0); `synaps attach` prints the reason and exits 3. |
| `SYNAPS_DAEMON_IDLE_EXIT_SECS` | Auto-spawned daemons (from plain `synaps` or `--attach`) exit on their own after this long with **zero clients, no turn running and no pinned session** — default **10 s** (the clock starts at the last disconnect; parked/idle sessions do not hold it, a [pinned](#pinning-a-session-pin) one does); `0`/`never` keeps them up. The config key `daemon.idle_exit_secs` sets the same default (`0` = never); the env var wins when set. Parked sessions are journaled and `--continue` brings them back; nothing is lost, only the warm extensions. A daemon started by hand (`synaps daemon`) uses its own `--idle-exit`. |
| `SYNAPS_DAEMON_IDLE_END_GRACE_SECS` | A **zero-turn** session with no clients ends after this long (F18) — default **5 s**. It has nothing to keep warm, so it does not wait the full park grace. |
| `SYNAPS_DAEMON_PARKED_EVICT_SECS` | A **parked** session stays in the daemon's map (so `--attach <ID>`/the adopt banner find it) for this long, then is evicted (`Ended(Evicted)`). Default **1 h**; `never` keeps rows forever. Nothing is lost — the journal stays and `--continue <ID>` rebuilds it, exactly as after a daemon restart. A zero-turn session never parks: it ends (`Idle`) at the park deadline (F18). |
| `SYNAPS_DAEMON_ADOPT=0` | Disable **adopt**: by default a plain `synaps` (no subcommand, no `--attach`; only `--system`/`-s`, `--continue`, `--prompt-manifest`, `--profile` allowed) runs as the thin socket client with `--attach --new` semantics (fresh session; `--continue` still continues) against the daemon for that profile — **spawning it first if none is running**, exactly like `--attach` (jcode model). A failed spawn, or `SYNAPS_DAEMON_AUTOSPAWN=0` with no daemon, falls back to the ordinary in-process boot with a notice. `SYNAPS_DAEMON_ADOPT=0`, or any other flag (`--no-extensions`, `--help`, attach modifiers) → in-process, untouched. |
| `SYNAPS_DAEMON_ALLOW_LEGACY_MCP=1` | Deprecated, ignored. Legacy MCP was removed: every mode uses per-session MCP leases, so the daemon no longer refuses to start with `progressive_tool_disclosure=false` and MCP servers configured. (`--allow-legacy-mcp` is likewise accepted and ignored.) |
| `tools.activation_confirm = auto \| prompt \| deny` (config key, default `auto`) | Host policy for MODEL-INITIATED `activate_tools` under progressive disclosure. `auto`: granted without asking. `prompt`: the session raises a `Prompt{kind: Confirm}` ("Confirm tool activation" — the TUI shows a y/n dialog listing the exact ids; `synaps attach` prints `[confirm #id] …\n(y/n):`); only `y`/`yes` allows, anything else/Esc denies (fail-closed). `deny`: always refused, no prompt is raised (locked-down hosts). `server.auto_approve_confirms = true` / `--auto-approve-confirms` still grants regardless. The mode is logged once at boot (`tools.activation_confirm: …`). |
| `SYNAPS_RUNTIME_DIR` | Where the socket/lock/json/pid live (default `~/.synaps-cli/run`, 0700). |
| `process_scopes = auto \| off` (config key, default `auto`) / `SYNAPS_PROCESS_SCOPES` (env wins) | On Linux with a reachable systemd user manager, the spawned daemon, every bash tool command and every PTY shell start in their **own** transient scope (`systemd-run --user --scope`). systemd-oomd kills a whole cgroup at once; without this a runaway tool command shared the cgroup of the terminal that started the daemon, and an OOM kill took the command, the daemon and every session down together (S348). Separate scopes reduce shared-cgroup OOM exposure; they are not memory caps. Ancestor-cgroup or global kernel OOM can still kill the daemon and other sessions. Windows isolation is not implemented. Off, unavailable (macOS, containers, no user bus), or a session env without `XDG_RUNTIME_DIR`/`DBUS_SESSION_BUS_ADDRESS`: processes launch directly. Each launch probes with its effective environment and working directory (a three-second polling deadline after spawn, plus best-effort cleanup; offloaded for async bash and PTY startup). The probe never dispatches the requested command, but exact-environment execution is not a side-effect-free sandbox. Kernel-stalled spawn/reaping is not covered by an absolute wall-clock guarantee. A failed probe selects direct launch. Availability can race after a successful probe: any subsequent wrapper failure surfaces as an error, never an automatic direct retry, because the command may already have started. `daemon reload` re-execs in place and keeps the current cgroup; a daemon moves into its own scope at its next fresh start. Scopes show in `systemctl --user list-units --type=scope` as `synaps daemon` / `synaps tool: bash|shell`. |
| `tool_scope_slice = <name>.slice` / `daemon_scope_slice = <name>.slice` (config keys, default unset) | The systemd slice the scopes above go in: `tool_scope_slice` for bash tool commands and PTY shells, `daemon_scope_slice` for the daemon's own scope (separate, so tools can be capped without the daemon). Set, Synaps passes `--slice=<name>` to `systemd-run`; unset or empty (`tool_scope_slice =`), the command line is exactly as without the keys. **Synaps sets no limits** — the operator does, on the slice. Use a top-level name such as `synaps_tools.slice`: a `-` nests, and `CPUWeight` only counts against siblings. A slice that doesn't exist is created empty, not refused. Recipe, rules and checks: [Capping tool commands with a slice](#capping-tool-commands-with-a-slice). |
| `SYNAPS_SESSION_EVENTS_CAP` | Per-session broadcast capacity (default 1024). A slow client gets `SystemNotice("event stream lagged; n dropped")`. |
| `synaps daemon reload [--now] [--drain-secs N] [--exe PATH]` / `SYNAPS_DAEMON_RELOAD_DRAIN_SECS` (default 30) | Re-exec the daemon in place (C3). `--now` = drain 0. `--exe` overrides (and records) the binary. |
| `SYNAPS_TUI_ATTACH_RECONNECT_SECS` (default 60) | Total `SocketTransport::reconnect` budget after `Reloading`/EOF (backoff 100 ms ×2, cap 5 s). |
| `SYNAPS_DAEMON_RELOAD_STATE` / `SYNAPS_DAEMON_LOCK_FD` / `SYNAPS_DAEMON_SUBAGENT_ID_FLOOR` | Internal: handed to the new image by `reload`; all scrubbed at start. They are a reload only when the state file parses AND the `LOCK_FD` fd is open on this daemon's `daemon.lock` (same device and inode). Otherwise (e.g. an environment copied from a reloaded daemon's `/proc/<pid>/environ` into a stop + start) the daemon logs one WARN `stale reload hand-off in the environment (…); starting fresh` (also on stderr), removes them, never touches that fd, and starts fresh: it takes the flock itself and restores the crash/stop record. An fd that IS the lock with a missing or unparsable state file refuses to start, as before. On Windows `SYNAPS_DAEMON_HANDOFF_EVENT` and `SYNAPS_DAEMON_HANDOFF_PID` take the place of `LOCK_FD` (see [Windows](#windows)); `RELOAD_STATE` without the event refuses to start. |
| `daemon.crash_restore = on \| off` (config key, default `on`) | The crash record and the restore after a crash or a stop ([Restore after a crash](#restore-after-a-crash-cold-start)). `off`: no record is written, a leftover one is removed at start (a quiet start after a deliberate halt), and a fresh start sweeps the dead daemon's session cards as before; clients bring their sessions back one by one. Takes effect at the next fresh start. |
| `capacity_resume = on \| off` (config key, default `on`) | A session whose turn stopped because no Anthropic account had capacity, with a known reset within 8 days, resumes by itself once at that reset ([Capacity resume](#capacity-resume-capacity_resume)). Needs the crash record (`daemon.crash_restore = on`); independent of `daemon.reload_resume`. `off`: nothing is armed, and an armed resume is dropped at its time. |
| `daemon.umask = inherit \| <octal>` (config key, default `inherit`) | File-creation mask of the daemon process and so of everything it starts: bash tool commands, PTY shells, extensions, MCP servers. `inherit` keeps the mask the daemon was started with (a systemd user service typically starts it with `0002`, so tools write group-writable `0664`/`0775` files); an octal mode (`022`, `"0022"`; permission bits only, at most `0777`) replaces it. Set once per daemon start, in the daemon's start path (`run_foreground`, `daemon/umask.rs`): after the host has booted with the config, before the daemon starts any process (extensions, MCP servers, sessions). The current mask is read from `/proc/self/status` and the new one set with one `umask(2)` call, only when it differs (none for no key); without a readable `/proc` the mask is read by a swap through `0777` (two calls, during which a concurrently created file gets no permission bits), then changed if it differs. The environment is not changed. A changed value takes effect at the next `daemon reload` or restart, never while running. A malformed value warns and inherits. One log line per daemon start: `daemon: umask umask=0022 source=config base=0002` (`source=inherit` for no key, `inherit` or a malformed value; the malformed case is a WARN line with the parser's reason). Only the daemon: the TUI and one-shot commands keep their inherited mask, and files written through `private_fs` keep their explicit `0600`/`0700`. What can keep the started-with mask: the base/log directory, if this start creates it, and the first log file (the boot creates and opens them before the apply). Rollback: remove the line and `synaps daemon reload` (the reload state carries the started-with mask, so the new image returns to it). An older binary ignores the key ("unknown daemon key") and the state field, so a reload to one keeps the mask the newer image set until a full restart. Round trip: after a reload to an older binary and back, the started-with mask is lost (the older image records none, so the newer image takes the mask it inherited, the one the newer image had set, as its base). Removing the line restores the original mask only when every reload since the daemon started went through images that carry it; otherwise only a full daemon restart does. |
| `synaps daemon purge` / `scripts/memprof/purge.sh` | jemalloc purge in the daemon before an RssAnon sample (C2). `SYNAPS_MEMPROF_PURGE=1` makes the attach client purge on every idle immediately. |
| Thin-client flags (`SYNAPS_CLIENT_REEXEC=0`, `SYNAPS_CLIENT_ALLOC=default`, `SYNAPS_CLIENT_THP=1`, `SYNAPS_CLIENT_PURGE_IDLE_SECS`, `SYNAPS_CLIENT_HISTORY=full`, `SYNAPS_TUI_SCROLLBACK[_BYTES]`, `SYNAPS_TUI_SYNTECT=full`, `SYNAPS_TUI_SYNTECT_IDLE_SECS`, `SYNAPS_CLIENT_SIGNAL_THREAD=1`, `SYNAPS_ATTACH_TAIL_ITEMS`) | Phase 4 client diet — full table with defaults in [memory-budget.md](memory-budget.md#kill-switches-all-env-vars). Only read on the `--attach`/`attach` thin path; the in-process fallback (`SYNAPS_DAEMON=0`, or a failed auto-spawn) boots untouched (`tests/attach_no_daemon.rs`, `tests/daemon_autospawn.rs`). |
| `SYNAPS_DAEMON_READY_FD` / `SYNAPS_DAEMON_READY_EVENT` | Internal: write end of the ready pipe handed to a `--detach`ed child (Windows: the name of the ready event). Scrubbed from the env before accept. |
| `SYNAPS_DAEMON_PARK_GRACE_SECS` (default 60; `never` disables) | B3: seconds after the last detach (idle, no prompts, no compaction, not keep-warm, journal on disk) before a session is **Parked** — `Runtime` + `ConversationState` dropped, restored from the journal on the next attach/turn/`synaps send`. A session that never ran a turn has no journal and never parks. |
| **Pin**: `/pin [on\|off\|status]` / `synaps daemon pin <ID\|NAME> [--off]` / `--keep-warm` (attach flag) / `/keep-warm on\|off` / `SessionCommand::KeepWarm` | The session stays loaded: never parked, never idle-ended (F18), never evicted, and the daemon does not idle-exit while any session is pinned. Survives `daemon reload` and comes back when the daemon starts again (after a stop, crash or reboot). See [Pinning a session](#pinning-a-session-pin). Not `/keepwarm`, which pings the API to keep the prompt cache warm and costs money. |
| `synaps attach --observe` / `--takeover` | B1 attach modes: `--observe` never owns input (setters/`Submit` are `Refused`); `--takeover` steals ownership from the current owner (who is told via `InputOwnerChanged{Takeover}`); default `Mirror` owns input iff nobody does (with `client_input = shared`, every `Mirror` may type; see below). |
| `synaps --attach --as NAME` / `synaps attach --as NAME` | Names this client in the session (also `SYNAPS_CLIENT_NAME`): 1–32 letters, digits, `-` or `_`, starting with a letter or digit, checked by the client and again by the daemon (an invalid name is `Refused{Protocol}` at the handshake). A label, not a login: every client is the daemon's own user. Without one a client gets its kind (`tui`, `attach`, `rpc`, …); a name already taken in the session gets the lowest free `-2`, `-3` suffix (case-insensitive). The resolved name rides `ClientJoined{name, mode}` and `Attached.roster`, and shows in `/clients`, `synaps daemon sessions` (`clients=2 (tui*, dash(observe))`, `*` = owns input), `input owned by tui (#1)` refusals, approval notices and the TUI's "input taken over by" toast. Older daemons ignore the name; older clients ignore the new fields. |
| `/takeover` (TUI) / `SessionCommand::TakeInput` | The in-place `--takeover`: an attached read-only client takes input without re-attaching (no re-mirror), mid-turn too. Same `InputOwnerChanged{Takeover}` (`Attach` if nobody owned input); refused from an `--observe` client; a no-op from the owner. A read-only TUI's attach line and its "refused: input owned by …" errors name it. |
| `client_input = exclusive` (default) / `shared` (config, `/settings` → Daemon) | Who may type in a session with several clients attached. By default (`exclusive`, the rule before `client_input`) only the input owner drives: the others are read-only until they `--takeover` or `/takeover`, and their input is `Refused` (`input owned by client #<id> (<name>)`). Every client is the daemon's own user, so with `shared` every drive client (not `--observe`) may send input; the one that did last is the *active* client: `InputOwnerChanged{Input}` moves the marker (older clients, without `ClientCapabilities::owner_reason_input`, read it as `Takeover`), it may choose "Allow all" (answering makes a client active), and it gets input back first after a daemon reload. When the active client leaves, input goes to whoever typed most recently. The daemon reads it from config when it starts, for every session it creates or restores (`synaps daemon reload` picks up a change; `SessionConfig.client_input` overrides it per session). The snapshot carries it (`Attached.client_input`; absent from an older daemon, which means `exclusive`), and so does the listing (`synaps daemon sessions` says `active=` instead of `owner=`). An older client (no `owner_reason_input` capability) on a shared session knows only the owner rule, so the daemon presents it as owning input and never sends it `InputOwnerChanged`: it can type all along, and would otherwise announce "input is owned by …" on attach and "input taken over" whenever another client typed (every open TUI is an older client right after a `daemon reload` onto a new build). The TUI header shows `N clients` when another client is attached; a TUI that may not drive (`--observe`, or not the owner under `exclusive`) sends nothing on Enter or Esc and says why in a toast, keeping the text. |
| `synaps --attach --no-confirm-answers` | TUI only, any attach mode: this client never answers a `Confirm` prompt (tool-call and tool-activation approvals). It never opens the approval pane for one, so no key typed into it can approve; it lists the waiting confirms in a toast until another client's answer resolves them (`PromptResolved`). `Secret` prompts are unchanged. Client-side only: no wire or daemon change, so it works against any daemon. For a TUI embedded where keystrokes may not come from the person approving (a terminal pane in another app). An `--observe` attach still can answer prompts without this flag: `Answer` is not an input command. |
| `SYNAPS_SESSION_COMPACT_INLINE=1` | One-release kill-switch: run `/compact` and auto-compaction inline on the actor task (the #107 body — `Attach`/`Cancel` wait behind it) instead of the spawned job. Deleted in phase 4. |
| `synaps daemon reload` / `stop` / `purge` / `close` / `pin` / `rename` / `reload-extension` from **any** same-uid client | Not a privilege boundary (0600 socket): a stray `synaps daemon reload` from any shell checkpoints every session and closes everyone's PTYs, `daemon close --attached` ends one session whoever owns its input (without `--attached` it refuses a session that has clients attached), `daemon pin` pins one session whoever owns its input (host authority, like `stop`), `daemon rename` names one session whoever owns its input (metadata only), and `daemon reload-extension` restarts an extension every session shares. `Checkpoint` over the wire is owner-only. |

## Capping tool commands with a slice

By default the user manager puts each transient scope in its default slice
(usually `app.slice`, next to the desktop's apps). To cap what agent tool
commands can take, give them a slice of their own and set the limits on it.
**Synaps sets no limits itself.**

**1. Define the slice** — `~/.config/systemd/user/synaps_tools.slice`:

```ini
[Unit]
Description=Synaps tool commands

[Slice]
# Share of CPU under contention, against the slice's siblings (systemd's
# defaults: app.slice and session.slice 100, background.slice 30).
CPUWeight=20
# Reclaim/throttle above this, OOM-kill inside the slice above that.
MemoryHigh=8G
MemoryMax=12G
TasksMax=2048
```

**2. Point Synaps at it** — `~/.synaps-cli/config`:

```
tool_scope_slice = synaps_tools.slice
```

**3. Load it:** `systemctl --user daemon-reload`.

**When it applies.** The config is read at every spawn, so tool commands go in
the slice from the **next** bash command or PTY shell, with no restart.
`daemon_scope_slice` applies when the daemon **starts fresh**: `synaps daemon
reload` re-execs in the same scope, so a change needs `synaps daemon stop` and a
new start.

**Pick a flat name.** In a slice name a `-` means nesting:
`synaps-tools.slice` sits under `synaps.slice`, and `agent-tools.slice` under
`agent.slice`. `CPUWeight` only weighs a cgroup against its **siblings**, so on
a nested slice that is the only child of its parent it does nothing (the parent,
at the default 100, is what competes with `app.slice`). Put `CPUWeight` on a
top-level slice: a name without `-`, such as `synaps_tools.slice`.
`MemoryHigh`/`MemoryMax`/`TasksMax` are hierarchical and hold wherever the
slice sits (an ancestor's limits apply too).

**`io` and `cpuset` are usually not yours.** The user manager typically gets
only the `cpu`, `memory` and `pids` controllers (`cat
/sys/fs/cgroup/user.slice/user-$UID.slice/user@$UID.service/cgroup.subtree_control`).
Without `io` and `cpuset` there, `IOWeight=`, `IOReadBandwidthMax=` and
`AllowedCPUs=` on a user slice are silently ignored, unless `Delegate=` on
`user@.service` is extended (a system-level change).

**A typo is not an error.** systemd creates a slice that doesn't exist on the
fly, with no limits, so a misspelt name runs tool commands **uncapped** and the
launch still succeeds. Check while a command runs:

```
systemctl --user status synaps_tools.slice         # Loaded: the unit file; Memory: … (high …, max …); the scopes in it
systemd-cgls --user-unit synaps_tools.slice        # the scopes (synaps tool: bash|shell) in it
systemctl --user show -p MemoryHigh,MemoryMax,CPUWeight,TasksMax synaps_tools.slice
```

Synaps helps once: the first time a launch in a process uses a slice, a
background thread (off the command's path) asks the manager for the slice's
`MemoryHigh`, `MemoryMax`, `MemorySwapMax`, `CPUWeight`, `CPUQuota` and
`TasksMax`; if none is set it logs one WARN naming the slice ("no unit file
defines it: a typo, or a new unit not yet loaded with `systemctl --user
daemon-reload`?"). It is best-effort: a lookup that fails or times out says
nothing.

**Name rules.** A value must end in `.slice`, use only `[A-Za-z0-9:_.-]`, not
start with `-`, not be `-.slice`, contain no `--`, not end its name in `-`, and
be at most 255 bytes. Anything else is a config warning and is ignored: the key
keeps the value of an earlier valid line, else stays unset. Write the bare name
without quotes — the config file is not TOML (`tool_scope_slice =
"synaps_tools.slice"` warns and is ignored). An empty value (`tool_scope_slice
=`) unsets the key, without a warning, undoing any earlier line.

**If the probe fails.** Each launch first probes with the same `--slice`. If
the probe fails — the user manager can't be reached, or it refuses the slice
(e.g. a slice it can't create) — the command starts without a scope, as with
scopes unavailable, and one WARN per slice name says so: `scope probe failed
(user manager unreachable, or slice synaps_tools.slice refused)`.

## Kill guard on tool children (Linux glibc; macOS Seatbelt)

Every command the `bash` tool runs and every `shell_start` PTY shell starts with a small `LD_PRELOAD` shim
(`crates/agent-engine/shim/killguard.c`, module `killguard`) that makes `kill(-1, sig)` and `killpg(1, sig)` fail with
`EPERM`. Every other `kill`/`killpg` is the raw `kill` syscall, exactly what glibc's own functions do (`killpg` of a
negative group: `EINVAL`). The shim needs only `syscall` and `__errno_location` from `libc.so.6`, at their oldest
versions (`GLIBC_2.2.5` on x86_64, `GLIBC_2.17` on aarch64; no `dlsym`, no constructor), so it loads into any glibc
process. It exists because a test whose daemon failed to start called `kill(-1, SIGKILL)` (10-27) and ended every
process of the user: the desktop session and every Synaps session. It sets no `no_new_privs` and installs no seccomp
filter (both break setuid `sudo` and snaps); nothing else about a child changes. Same guard with the write sandbox
([sandbox.md](sandbox.md)) on or off, with or without a process scope. macOS has a Seatbelt stand-in (below). Other
targets: no guard, no change; that includes musl and every other non-glibc Linux build (the
`x86_64-unknown-linux-musl` release artifact): nothing is built, installed or added there.

- Install: at first use in a process, the shim (built by `crates/agent-engine/build.rs`, embedded in the binary) is
  written to `<base>/lib/synaps-killguard-<first 12 hex of its sha256>.so` (`<base>` = `SYNAPS_BASE_DIR`, else
  `~/.synaps-cli`): dir `0700`, file `0644`, temp file + fsync + rename in the same dir. A file with the right hash is
  reused, one with the wrong bytes rewritten. Older versions are never deleted (a running child may still map one).
- Self-check: right after the install, `true` (`/usr/bin/true`, else `/bin/true`, else NixOS's) runs once with
  `LD_PRELOAD` set to the shim alone (stdin null, 2 s). It must exit 0 with nothing on stderr; otherwise the process
  doesn't use the shim (fail-open below, `reason=self_check: … exit code …`, at most ~120 chars of the stderr).
- Env: the shim path is appended to `LD_PRELOAD` (a value already there is kept, ours added once), and
  `verify_asan_link_order=0` to `ASAN_OPTIONS` (set when absent, `:`-appended otherwise, unchanged when the last
  `verify_asan_link_order` there is already `=0`): ASan aborts when a library is preloaded ahead of its runtime.
- Fail-open: when the shim can't be written or found, fails the self-check, or its path is relative or has a space or
  `:` (the `LD_PRELOAD` separators), children start without it, env unchanged, and the process logs one WARN:
  `kill guard shim unavailable; tool children run without it` with `reason=…`. A tool never fails because of the
  guard; only the first tool start in a process waits, for the self-check (about a millisecond, 2 s at most). A shim
  file deleted while the daemon runs: children run without it until a restart (one WARN).
- One command without it: `env -u LD_PRELOAD <command>`. There is no kill switch.
- `ldd` and `env` in a tool child show the shim (and `ASAN_OPTIONS`); use `env -u LD_PRELOAD` for bundlers that copy
  what `ldd` lists (AppImage, linuxdeploy) and to inspect a program's own dependencies.
- glibc drops `LD_PRELOAD` at a setuid or file-capability exec, but snapd restores it for snap apps (`SNAP_SAVED_*`):
  a strict snap (e.g. firefox via `xdg-open`) may log `cannot be preloaded … ignored` when AppArmor denies the file.
- 32-bit and qemu-user programs print `wrong ELF class` (or a wrong-machine line) once per exec; they run unguarded.
- The env can leak past tool children (`systemctl --user import-environment`, `dbus-update-activation-environment`, a
  tmux/screen server first started from a tool): harmless, since shim files are never deleted.
- Not covered (accepted gaps): Go and statically linked binaries (no libc `kill` to interpose); a raw
  `syscall(SYS_kill, -1, …)`; descendants started with a cleared or allowlisted env (the pria-apps worker env and the
  start-on-login launcher drop `LD_PRELOAD` on purpose); a command that sets `LD_PRELOAD` itself; setuid and
  file-capability programs (glibc secure-exec ignores the preload; they run normally, unguarded); processes Synaps
  doesn't start as a tool: MCP servers, extensions, the sidecar, memory backends, plugin slash commands and setup
  scripts, the watcher, the daemon itself, and the `ls`/`find`/`grep` tools (fixed programs, no agent command).

macOS (module `seatbelt`): SIP strips `DYLD_*` at `/bin/bash` and every `/usr/bin` program, so no shim. Instead the
same children (bash tool commands and `shell_start` shells, nothing else) start as `/usr/bin/sandbox-exec -p
<profile> <program> <args…>`, the program first resolved by the child's `PATH`. The profile is `(version 1)(allow
default)(deny signal)(allow signal (target same-sandbox))`: everything is allowed except a signal to a process
outside the child's own sandbox, so `kill(-1, …)` reaches nothing outside it. The kernel enforces it: Go, static and
raw-syscall programs are covered too. sandbox-exec execs in place (same pid and process group) and never prompts.

- Probe and fail-open: the first tool start in a process runs `sandbox-exec -p <profile> /usr/bin/true` once (stdin
  null, 2 s at most); it must exit 0 with an empty stderr. No off switch, as on Linux. When it fails, children
  start unwrapped and the process logs one WARN: `kill guard seatbelt unavailable; tool children run without it`
  with `reason=…`.
- What it can't see: `open`, `osascript` and `launchctl` hand work to launchd or another app, which starts it
  outside the sandbox, unguarded. A tool child can't signal (or `kill -0`) any process outside its own sandbox: a
  later tool call can't signal what an earlier call left running (each call is its own sandbox; confirmed on the
  Mac, 10-30), and a pidfile liveness check sees `EPERM`. A `shell_start` shell is one sandbox, so jobs inside it are
  fine. Setuid programs can't run under any Seatbelt profile (even `(allow default)` alone): `/bin/ps` and
  `/usr/bin/top` are setuid root on macOS, so in a tool command they fail with "Operation not permitted" (exit 126);
  use `pgrep -fl` and `lsof` instead. Seatbelt doesn't nest: when Synaps itself runs
  under sandbox-exec, the probe fails and children run unguarded (the WARN); a tool command that runs sandbox-exec
  itself fails inside the guard. `scripts/seatbelt-proof.sh` checks the guard on a Mac (don't run it on Linux).

## CLI

```
synaps daemon [--foreground|--detach] [--socket PATH] [--idle-exit SECS] [--profile P]
synaps daemon status [--json]        # daemon.json + flock probe + Ping → state/pid/uptime/sessions (exit 1 if not answering)
synaps daemon stop [--force]         # Shutdown{force} over the socket, wait for the flock (47 s; --force 10 s); --force escalates SIGTERM → SIGKILL (+5 s); Windows: taskkill /T /F (10 s)
synaps daemon sessions [--json]
synaps daemon purge                  # Purge frame → memstat::purge_arenas() in the daemon; reply Pong (bench hygiene, C2)
synaps daemon reload [--now] [--drain-secs N] [--exe PATH] [--force] [--json]   # re-exec in place, same pid (C3; §Reload below; Windows: new pid, §Windows)
synaps daemon close <ID|NAME> [--force] [--attached] [--json]   # end one session without attaching (§Closing a session); exit 0/1/2/3
synaps daemon pin <ID|NAME> [--off] [--json]       # pin/unpin one session without attaching (§Pinning a session); exit 0/1/2/3
synaps daemon rename <ID|NAME> <NEW_NAME> | --clear [--force] [--json]   # name/unname one session without attaching (§Renaming a session); exit 0/1/2/3
synaps daemon reload-extension <NAME> [--json]     # restart one extension in place, sessions untouched (§Reloading one extension); exit 0/1/2/3
synaps skills status [--json]        # SkillsStatus → the last skill discovery pass (§Skills below); never starts a daemon
synaps skills reload [--json]        # SkillsReload → walk the skill roots again with the skill keys read fresh
synaps attach [ID] [--create] [--continue NAME_OR_ID] [-s PROMPT] [--observe|--takeover] [--keep-warm] [--as NAME]
synaps --attach [ID] [--observe|--takeover] [--keep-warm] [--no-confirm-answers] [--as NAME]   # thin TUI client over the socket (phase 4); starts the daemon if none is running
```

### How to use

```
synaps --attach --new
```

That is all: no env, no `daemon --detach` first. `synaps daemon status` says
`not running (auto-starts on first --attach)` until then; `synaps daemon stop` ends it.

`--detach` forks `current_exe daemon --foreground` under `setsid` (through `systemd-run --user --scope` when
`process_scopes` applies, which execs in place: same pid), stdout → null, stderr → pipe,
and waits ≤ 15 s (`SPAWN_READY_TIMEOUT`, strictly above the daemon's 10 s extension-discovery wait; MCP discovery runs after ready) for `R` on an anonymous ready pipe (EOF before `R` = child died → error with its
stderr tail). Measured on bella: ready in ~75 ms. The readiness pipe is allocated
only after scope probing, and failure diagnostics are nonblocking and byte-bounded.
A kernel-stalled kill/reap is not subject to an absolute five-second return guarantee.

Test-support builds (`testing`) suppress real scope probing by default, including
ordinary bash/PTY tests. Synthetic launcher tests do not use the host manager.
Real systemd smoke tests require explicit `SYNAPS_TEST_REAL_SCOPES=1`; this does not
override an operator's `SYNAPS_PROCESS_SCOPES=off`.

`synaps attach` is a thin line client: stdin lines → `Submit` (or `Steer` while streaming);
`/abort` → `Cancel`; `/detach`, `/quit` or **Ctrl-C → `Detach` + `Bye` — the turn keeps running**;
`/close [force]` → engine command `close` (ends the session; see [Closing a session](#closing-a-session-close));
`/model NAME`; `/cmd NAME [ARG]` (engine command); `/save`; `/new`; `/sessions` (status query);
`/pin on|off` (alias `/keep-warm on|off`). Typed events render as `[aborted…]`, `[session cleared → id]`, `[compacting: …]`,
`[compacted N messages]`, `[refused cmd: reason]`, `[input owner → …]`, `[session Parked]`.
Prompts render as `[prompt #id] title: prompt > `; `Secret` prompts turn terminal echo off.
With no ID: attaches to the single live session, creates one if none, lists if several.

## Skills (feature `skills_v1`)

The daemon walks five skill roots at boot, in this order. A skill (plugin
and name) met again later in the walk is dropped and reported as shadowed:

1. `project_plugins`: `.synaps-cli/plugins` (relative to the daemon's cwd)
2. `project_skills`: `.synaps-cli/skills`
3. `extra`: the config key `skills.extra_root` (default unset)
4. `home_plugins`: `<base>/plugins`
5. `home_skills`: `<base>/skills`

`skills.extra_root = /absolute/path` adds the third root. Write the path
bare: spaces are kept, `~` is not expanded, and a quoted or relative path is
a config warning (the key keeps an earlier valid value, else stays unset).
The root is resolved (realpath) once per walk, so a `current` symlink
swapped mid-walk cannot mix two versions. `disabled_plugins` and
`disabled_skills` apply to it as to every root. Its plugins are full plugins
(commands, keybinds, sidecars, settings). One pass keeps at most 2048 skills
and 4 MiB of skill metadata; what is past a cap is counted as `skipped`.

- `synaps skills status [--json]` / `SkillsStatus{id}` (also accepted on an
  attached connection): the report of the last pass, boot or reload. No
  disk walk.
- `synaps skills reload [--json]` / `SkillsReload{id}` (an unattached
  connection only; an attached one gets `Error` "control frames are not
  accepted on an attached connection"): reads `skills.extra_root`,
  `disabled_plugins` and `disabled_skills` fresh from the config file,
  walks the roots again, swaps the new set into the command registry and
  publishes its report with the next generation. Reloads run one at a time,
  in order. The rest of the daemon's config snapshot is kept: extensions,
  hooks, MCP servers and keybinds still need `synaps daemon reload`. A walk
  that fails answers a session-less `Error`, and the last report stays.
  A `load_skill` whose source changed or went away since the pass (the old
  version was deleted) fails with "… reload skills and retry".

Both answer `SkillsResult{id, report}`: `generation` (1 = boot),
`at_unix_ms`, `roots[]` `{kind, configured, resolved, plugins, skills}`
(paths as text; `null` = not set, or absent / not a directory; counts are
before the disable filters), `skills_total`/`skills_cap`,
`metadata_bytes`/`metadata_cap`, `skipped`, `disabled`, `shadowed[]`
`{plugin, name, kept_root_kind, dropped_root_kind}` (the first 64) and
`shadowed_total`. The CLI asks a running daemon and never starts one. Exit
codes: 0 report; 1 no daemon, I/O error, or no answer in time (10 s status,
60 s reload); 3 refused (an `Error` answer, or a daemon whose `Welcome` does
not list `skills_v1`). `--json` prints the report on one line, or
`{"error": "no_daemon"|"io"|"timeout"|"refused"|"daemon_predates", "message": …}`.

## Detach vs abort (F27)

Over a socket transport (daemon session), **Ctrl+C / `/quit` detaches the client
but leaves the turn running** in the daemon. This is the correct default for
"close the lid" workflows, but a footgun when the user meant to stop a
destructive command.

To avoid accidental headless turns:

- **First Ctrl+C (or `/quit`) while streaming** shows a notice:
  `turn still running in the daemon — press Esc to abort it, or Ctrl+C again
  within 3 s to detach (session keeps running)`.
- **Second Ctrl+C within 3 seconds** detaches as before (the turn continues in
  the daemon).
- **Esc while streaming** aborts the turn (sends `Cancel` to the actor) — same
  as in-process.
- **Ctrl+C while idle** (not streaming) detaches immediately — no double-press
  needed.
- **In-process** (`TransportMode::Local`, i.e. `SYNAPS_DAEMON=0` or no daemon)
  is unchanged: quit ends the session.

The line client (`synaps attach`) follows the same double-press rule. The notice
is printed to stderr so scripts piping stdout are not affected.

## Closing a session (`/close`)

Detaching leaves the session loaded in the daemon. It parks after the grace
(`SYNAPS_DAEMON_PARK_GRACE_SECS`) and leaves the map an hour after that
(`SYNAPS_DAEMON_PARKED_EVICT_SECS`), but an event queued for it (`synaps send`,
an extension's inbox event, a worker's wake) unparks it and starts a turn. A
session you are done with should be **closed**:

- `/close` (TUI and `synaps attach`) is the engine command `close`. The actor
  replies on its `QueryResult`, then ends exactly as the in-process quit does
  (`End{ClientQuit}`: save, `on_session_end`, `Ended` to every client). It
  leaves `synaps daemon sessions`; each attached TUI exits and prints
  `synaps: session <id> was closed; resume it with: synaps --attach --continue <id>`.
- It is an input command: only the input owner may close (others get `Refused`).
- It refuses while closing would stop work in flight: a running turn or
  compaction, a running worker (workers die with the session's runtime), or an
  armed `/auto` run. The reply names them. `/close force` closes anyway: it
  cancels the running workers first and notes each in the conversation
  (`subagent_cancelled`), so whoever continues the session knows no wake is coming.
- A **parked** session closes without being rebuilt.
- In-process, `/close` is `/quit` with the same checks.
- **`synaps daemon close <ID|NAME> [--force] [--attached] [--json]`** does the same from any
  shell, without attaching: no snapshot, no ownership change, a parked session is
  never rebuilt. The target is an id, a journal id, a name or a unique prefix
  (at least 8 characters, `MIN_PREFIX_LEN`) of an id or journal id, resolved against the daemon's listing (a name two
  sessions share, or a prefix of several, is an error that lists them). A
  session with clients attached is refused (it says how many and who owns
  input) unless `--attached` is given: closing it exits their TUIs and drops
  unsent input. `--force` only overrides work in flight, not attached
  clients; an attached session that is mid-turn needs both. The
  daemon sends the session `close` (`force` with `--force`) with host authority
  under a fresh reserved id per request (from `CLOSE_QUERY_ID_BASE`), waits for that reply and for the actor to
  end, and answers `Closed`; the attached TUIs exit as above. Exit codes: 0
  closed; 3 refused (work in flight: the reply names it, `--force` closes
  anyway; clients attached: `--attached`; or the daemon is reloading); 1 no such session, ambiguous, prefix too short, no daemon,
  timeout; 2 the daemon predates `daemon close` (`synaps daemon reload`) or a
  protocol mismatch. `--json` prints `{ok, session_id, outcome, text}`. A
  session whose journal another process holds (`locked_by`) is only removed
  from this daemon's list; it keeps running there.

## Pinning a session (`/pin`)

A detached session parks after the grace and, when the daemon has no clients and
nothing running, the daemon idle-exits and ends it. A session you want kept
loaded (a long-lived worker host, an inbox other tools write to) is **pinned**:

- It is never parked, never idle-ended (F18) and never evicted (only parked
  sessions are), and the daemon does not idle-exit (`--idle-exit`,
  `daemon.idle_exit_secs`, the auto-spawn default) while any session is pinned.
- `/pin` (or `/pin status`) in the TUI asks the session whether it is pinned;
  `/pin on` / `/pin off` pin and unpin it (input owner only). `/keep-warm on|off`
  is the same command; `synaps --attach --keep-warm` / `synaps attach --keep-warm`
  pin at attach. In-process (no daemon) `/pin` only says it applies to a daemon
  session. **`/keepwarm`** (no dash) is unrelated: it keeps the *prompt cache*
  warm by pinging the API, which costs money.
- **`synaps daemon pin <ID|NAME> [--off] [--json]`** does it from any shell,
  without attaching. The target resolves like `daemon close`'s. The daemon sends
  the session `KeepWarm{on}` with host authority and answers `Pinned{on}` once
  the session applied it (≤ 2 s, ≤ 10 s when pinning a parked session, which
  wakes it). Exit codes: 0 done; 3 refused (daemon reloading, or the session is
  live in another process: an in-process session has no pin, so quit it there
  and load it into the daemon with `synaps --attach --continue <id>` first); 1
  no such session, ambiguous, prefix too short, no daemon, timeout; 2 the daemon predates `daemon pin` (`synaps daemon reload`)
  or a protocol mismatch. `--json` prints `{ok, session_id, outcome, pinned, text}`.
- Pinning a **parked** session wakes it (a pinned session is live by
  definition); unpinning a parked one leaves it parked.
- `synaps daemon sessions` marks a pinned session with `pinned` after its
  lifecycle; `--json` has `"pinned": true|false` (`SessionMeta.pinned`).
- The pin survives `daemon reload`, and a pinned session **comes back when the
  daemon starts again** (see below). `/close` and `daemon close` end it on
  purpose: then it is unpinned and does not come back.
- A pinned session keeps the prompt cache warm (`/keepwarm`, when it is on)
  even with no TUI ever attached, e.g. one restored after a restart and fed by
  `synaps send`. Unpinned, only a session a typing client attached to keeps warm.

## Renaming a session (`daemon rename`)

`/saveas <name>` names the session (input owner only; an observer is told it is
read-only). **`synaps daemon rename <ID|NAME> <NEW_NAME> | --clear [--force]
[--json]`** does it from any shell, without attaching: renaming is metadata,
not input, so it needs no input ownership, and since the control connection
never attaches, presence (clients, input owner, which client claims the shared
inbox) does not change. Meant for tools such as an extension that auto-names
unnamed sessions.

**Rename never silently replaces a name.** A session that already has a name
is refused (`named`, exit 3) unless `--force`; `--clear` on a named session
needs `--force` too. The CLI sends the name it saw in the listing as the
frame's `expect`, which the session checks atomically, so a name the user sets
(`/saveas`) between the listing and the rename still wins, even with
`--force`. An auto-namer should never pass `--force`. **Raw socket clients:
send `"expect": null`** — an absent `expect` means no precondition and
replaces any name.

- The target resolves like `daemon close`'s (id, id prefix, journal id, or the
  current name). The daemon sends the session the host-only engine command
  `rename` (JSON args; a plain-text `/rename foo` typed in-process is refused:
  use `/saveas`). The session applies `/saveas`'s rules: `validate_name`
  (`[a-z0-9-]{1,40}`), unique across the sessions directory, no clash with a
  chain name. A process-wide lock, taken by `daemon rename` and by
  `/saveas` alike, covers the check through the save, so two sessions of
  this daemon can never take the same name at once. Waiting for it is
  bounded (3 s, then `busy`). It does not cover other processes: a journal
  lock is per session, so an in-process TUI's `/saveas` or the session
  picker can still race a daemon rename for a name.
- **A live session** saves durably (`save_durable`), then puts the file
  mtimes of `<id>.json` (and `<id>.journal`) back: a rename is not activity,
  so `synaps --continue` (newest mtime) and the `/sessions` order do not
  change. Then the listing, the `run/<sid>.json` card and a pinned session's
  pin-book entry (verbatim: a cleared name stops resolving) are updated, and
  every attached client, observers included, gets a `Conversation` with the
  new name. A failed save is reported (`failed`) and rolled back. A session
  created with `persist: false` gets the name in memory only (the reply says
  `in_memory_only`); a live session whose journal lock this daemon does not
  hold is refused (`locked`).
- **A parked session is renamed on disk and not woken**: under its journal
  lock (`session_catalog::rename_past_session`, the same primitive the
  session picker uses), with a durable save that leaves `updated_at` and the
  mtime alone. `expect` is checked against the name in the file, under that
  lock (another process may have renamed it while it was parked); a mismatch
  is `named` and the listing picks up the file's name. If another process
  holds the journal (an in-process `--continue`), it is `locked`. On unpark
  the name comes from the file.
- Answers: `renamed{session_id, name, in_memory_only?}`, or
  `rename_failed{session_id, reason, message, taken_by?, current_name?}` with
  `reason` one of `invalid`, `taken` (`taken_by` is the holder's journal id,
  e.g. `20261028-045932-eb8e`), `chain_conflict`, `named` (`current_name`),
  `locked`, `busy` (queue full, daemon reloading, another rename saving: retry), `unknown` (no answer
  in time: it may still apply, **re-list before retrying**), `failed`.
- Exit codes: 0 renamed or cleared; 3 refused or not applied right now
  (`named` without `--force`, `locked`, `busy`, `unknown`); 1 invalid name,
  name taken, chain conflict, failed, no such session, ambiguous, prefix too
  short, no daemon; 2 the daemon predates `daemon rename` (`synaps daemon
  reload`), a protocol mismatch, or a usage error (a blank `NEW_NAME` is one:
  use `--clear`). `--json` prints `{ok, session_id, outcome, name, taken_by,
  text}`; `name` is the name afterwards, or the current name for `named`.
- An attached client cannot send `rename` (neither the control frame nor the
  engine command): it has `/saveas`.

## Reloading one extension (`daemon reload-extension`)

Extensions are daemon-wide: one process each, shared by every session. A new
version of one (code or manifest) used to need `synaps daemon reload`, which
checkpoints every session and cancels the turns in flight.
**`synaps daemon reload-extension <NAME> [--json]`** restarts just that one:

- `NAME` is the plugin's directory name, as `/extensions` lists it. The daemon
  finds it the way discovery does (`~/.synaps-cli/plugins/`, then the daemon's
  `./.synaps/plugins/`, the later one winning), reads and validates its
  manifest, then stops the running instance and starts the new one. A plugin
  installed after the daemon started (or one that failed then) is loaded.
- Every session gets the new version from its next request: its tools are
  re-registered in the shared registry (a tool the new version dropped is
  removed, not left calling a stopped process), its hooks re-subscribed, and a
  deferred extension's session leases revoked (the next use starts it). No
  session is checkpointed and no turn is cancelled; only a call into the old
  process that is in flight when it stops fails, as it would if it crashed.
- Anything that can be checked first is: a name discovery would never load
  (a path, `foo.disabled`/`.bak`/…), a plugin that is not installed, disabled
  (`disabled_plugins`), whose setup failed, without an `extension` block, or
  whose manifest does not parse or validate is refused **with the running
  instance untouched**. Only a new version that fails to *start* leaves the
  extension unloaded; the error says so, and running the command again after
  the fix loads it.
- Exit codes: 0 reloaded (or loaded); 3 refused (the daemon is reloading); 1
  not reloaded (any reason above), no daemon, timeout (60 s); 2 the daemon
  predates `reload-extension` (`synaps daemon reload` once) or a protocol
  mismatch. `--json` prints `{ok, extension, outcome, unloaded, report, text}`;
  `report` is `{extension, was_loaded, deferred, tools, removed_tools,
  plugin_dir}`.
- Not reloaded with it: slash commands a plugin declares outside its
  `extension` block come from the skills registry (`/plugins reload`).

### Pinned sessions come back after a restart

The daemon keeps the set of pinned journals in the **pin book**,
`<config dir>/daemon-pins.json` (`~/.synaps-cli/[<profile>/]`; 0600; one entry per
pin: `journal_id`, `name`, `cwd`, `pinned_at`, the `/keepwarm` choice, and a
restore `state`/`reason`/`attempts`). It is written when the pin changes (TUI
`/pin`, `daemon pin`, `p` in `/sessions`, `--keep-warm`), when a pinned session
moves to a new journal (`/clear`, a successor compaction) or is renamed, and
when a pinned session is ended on purpose. A daemon stop, SIGTERM, idle exit,
reload or crash leaves it alone.

When the daemon starts (not on `daemon reload`, which hands its sessions over
already), it continues every entry that is not live, one at a time, in the
background after it is ready (`synaps --attach --continue <id>`'s create, in the
saved cwd), pins it, and restores its `/keepwarm` choice. Keep-warm pings
nothing until the first real turn. The restore holds the daemon's idle exit.

- **Journal held by another process** (an in-process TUI): never stolen. The
  entry is marked `waiting: held by pid N, tui` (one 2 s re-check); the next
  daemon start tries again, `synaps daemon pin <id>` tries now.
- **Missing journal / create fails**: `failed: <reason>`, kept until cleared.
- **Crash loop**: an attempt marker is written before each restore; an entry
  whose restore the daemon did not live through twice is marked failed and not
  retried (`synaps daemon pin <id>` retries it). The marker names whichever
  entry was in flight, so an unrelated crash during a restore counts against it.
- `synaps daemon sessions` lists pins that are not live (`<id>  pinned (not live)
  <state>`), and `/sessions` shows them under **PINNED · NOT LIVE** (⚑; `p`
  clears one, `enter` continues it). `--json` lists live sessions only.
- `synaps daemon pin <ID|NAME> --off` clears an entry that is not live, even
  with no daemon running. In `/sessions` → Past, a pinned journal shows ⚑; `p`
  pins a Past row (continues it into the daemon, pinned, without switching to
  it) or unpins a pinned one.
- `synaps daemon stop` prints `N pinned sessions will come back on next start: …`.
- `SYNAPS_DAEMON_RESTORE_PINS=0` starts a daemon without restoring (the pin book
  is kept for the next start).
- Not restored: `/system` overrides, `/settings` knobs changed in the session
  (thinking, model override beyond the journal's, auto-approve), `/memory`
  choice, running workers. The conversation, model, name and env come from the
  journal.
- At boot: `deploy/synaps-daemon.service` (systemd user unit, with
  `loginctl enable-linger`) starts the daemon, and with it every pin.

## Files (under `registry_dir()`, one set per profile: `daemon-<P>.*`)

| File | Mode | Purpose |
|---|---|---|
| `daemon.sock` | 0600 | UDS listener (symlinks refused on cleanup; on Windows a named pipe whose name is derived from this path, nothing on disk — see [Windows](#windows)). Every accepted connection must come from the daemon's own uid (`SO_PEERCRED`/`getpeereid`); any other peer, root included, or one whose credentials can't be read, is closed unread with a `daemon: refused connection` warning. |
| `daemon.lock` | 0600 | **flock = liveness oracle.** Alive iff someone holds it. Nobody unlinks on ECONNREFUSED alone; `reap_stale` unlinks sock/json/pid only when the lock is free. A second daemon on the same paths is refused. |
| `daemon.spawn.lock` | 0600 | Spawn lock: held by the one client auto-spawning the daemon (`daemon::ensure_running`). Never a liveness signal. |
| `daemon.json` | 0600 | `{pid, protocol_version, daemon_version, profile, started_at, socket, exe, generation}` — never credentials. The daemon keeps the record in memory: when a Ping finds the file missing or naming another pid (an older `synaps` that lists sessions unlinks run-dir `*.json` files it can't parse as session cards), the lock holder writes it back (one INFO line `daemon.json rewritten` per 10 min). Every write of `daemon.json` and `daemon.pid` goes through that file's own `<name>.tmp`, created exclusively and without following a symlink, mode 0600, then renamed into place. One metadata lock serializes the start write, the heal, the reload's write and its restore, and the shutdown cleanup: a heal never writes back an older record than the one held, and nothing writes the files after the cleanup. `daemon status`, `daemon reload`, `daemon stop` and `daemon start --detach` Ping before they read it. |
| `daemon.pid` | 0600 | pid |
| `daemon.live.json` | 0600 | The **crash record** (see [Restore after a crash](#restore-after-a-crash-cold-start)): the writer's `{pid, start_ticks, boot_id}` and every session's reload record plus `turn_in_flight` and `running_workers`. Written through `<name>.tmp` + `fsync` + rename; a clean stop writes it one last time from its checkpoints (marked as a stop) and keeps it. After two starts that died restoring it, moved aside as `daemon.live.json.quarantined-<UTC time>`; a record whose writer still runs is moved aside as `daemon.live.json.skipped-<UTC time>`. Never credentials. |

## Per-session event endpoint ownership

Session registry cards still use `<session-id>.json`, but active event sockets use
short generation-specific `s-<digest>.sock` names (Windows pipes derive their name
from that path). Clients must read the card's `socket_path`, not construct a path
from the session ID; `socket_path_for_session` remains a legacy helper. Registration
JSON remains backward-readable.

Updated writers coordinate bind/publication, rename, sweeping and removal through
a permanent `.session-registry.lock` in the runtime directory. **Never delete this
lock file to resolve contention:** replacing its inode defeats advisory locking.
Acquisition is bounded to 100 ms. Endpoints bind synchronously before publication;
cleanup uses the captured runtime directory and socket identity, so an old listener
cannot delete or rename its replacement's generation. Failure to publish rolls
back only the newly bound endpoint. Ordinary teardown also cleans up on Drop.

On Unix, cleanup still removes its captured socket if the card lock is busy; the
next successful scan removes a generated-path card whose endpoint is missing, even
when the PID remains alive. Windows has no filesystem socket, so card cleanup under
contention remains best-effort until a retry or own-PID reload sweep. Legacy endpoint
aborts remove only their captured inode. Regular files, symlinks and unrelated card
or socket paths are not cleanup targets.

The protocol coordinates updated binaries, not older binaries that ignore the lock
or hostile same-UID filesystem mutation. Enabled daemon auto-start callers also
serialize cleanup and liveness probing under the spawn lock, held through readiness;
a cleanup lock held briefly by another first client is not proof of a ready daemon.
Warm enabled attaches therefore also require acquiring the writable spawn lock.
Disabled auto-start behavior is unchanged. Generation collisions fail binding without
replacing the old endpoint. Long runtime-directory prefixes can still exceed Unix
socket limits; use a short `SYNAPS_RUNTIME_DIR`. Existing journal locks and compaction
registration-following behavior are outside this change.

### Event requests and receipts

One event per connection, at most 256 KiB (`MAX_PAYLOAD`, the JSON plus an
optional trailing newline). The client sends the event's JSON and ends the request
in one of two ways: it closes its write side (Unix half-close), or it ends the JSON
with a newline. The newline form exists for clients that cannot half-close, such
as a named-pipe client on Windows. A buffer that ends with a newline completes the
request only if it parses as a whole event, so a newline inside a pretty-printed
event never ends it early. The client has 5 s (`READ_TIMEOUT`) to deliver the whole
event.

The listener answers with exactly one line of compact JSON:

| Line | Meaning |
|---|---|
| `{"receipt":"ack","event_id":"<id>","peer_verified":false}` | The event has been queued in the session's event queue. `peer_verified` is the listener's verdict on the sender (see below). |
| `{"receipt":"nack","reason":"oversized","detail":"…"}` | More than 256 KiB; nothing queued. |
| `{"receipt":"nack","reason":"invalid","detail":"…"}` | Not an event, or unreadable; nothing queued. |
| `{"receipt":"nack","reason":"queue_full","detail":"…"}` | The queue is at capacity; nothing queued. |
| `{"receipt":"nack","reason":"timeout","detail":"…"}` | No complete event within 5 s; nothing queued. |
| `{"receipt":"nack","reason":"foreign_uid","detail":"…"}` | The client runs as another user (root included); its request was not read and nothing was queued. |

- An ack is written only after the event has been pushed into the queue. It means
  queued, not acted on: it says nothing about whether or when a turn runs.
- `detail` is at most 200 characters. Readers must accept reasons they don't know.
- A connection that closes without sending gets no reply; one left open without
  sending gets a `timeout` nack.
- The reply is best effort. Clients that don't read it are unaffected: a client
  that has already closed makes the write fail, and the error is ignored. After
  writing, the listener waits up to 2 s for the client to close, then drops the
  connection.

Cards advertise the protocol as `"receipts": 1`. A card without the field (written
by a build from before receipts) reads as 0, and its endpoint closes without a
reply. A daemon reload re-registers every successfully restored event endpoint
(failed or journal-locked placeholders are excluded), so after reloading into a
build with receipts each of those cards has the flag. Rolling back to an older build
leaves cards without it, and a client that needs a receipt can see that before it
connects.

### Who sent an event

The listener gives each event that arrives on a connection its own verdict on the
process that connected, as the event's `peer`. Only the listener writes it: whatever
`peer` a client puts in its JSON is replaced, and events that arrive any other way (the
inbox, watchers, internal notices) carry none.

```json
"peer": {"verified": true, "session_id": "…", "name": "…", "via": "in_process|resolver|process"}
"peer": {"verified": false, "session_id": null, "name": null, "via": "none", "reason": "…"}
```

`verified` means that the listener matched the connecting process to a session, and
nothing more. It is not authentication: it does not vouch for the content and grants no
authority. `name` is that session card's name when the event arrived; sessions can be
renamed, so it attests nothing, and anything that checks who sent an event uses
`session_id` only. The ack carries the same verdict as `"peer_verified": true|false`.
The event and the ack keep only this verdict: the uid, pid and start times the listener
read to reach it are not stored or passed on.

The model sees the verdict in the event's own tag, after `source` and `channel`:

```
<event id="…" type="message" severity="medium" source="…" from_session="…" from_name="…" verified="true">…</event>
<event id="…" type="message" severity="medium" source="…" verified="false">…</event>
```

`from_name` is left out when the card has no name. An event without a `peer` renders as
before, with none of these attributes. `source` stays whatever the sender claimed. The
values get the same sanitising as every attribute, and text in the body that reads as an
event tag is neutralised (see [Events reactor](events-reactor.md)), so the only
`verified` attribute in a tag is the listener's own.

#### What the listener reads

The connecting process is the one that called `connect`, with the credentials it had at
that moment, as the kernel reports them (SO_PEERCRED on Linux, getpeereid and
LOCAL_PEERPID on macOS). So:

- a process that changes its uid after connecting is judged by the uid it connected
  with, at the foreign-uid gate too;
- a process that execs another program keeps its pid, its pidfd and its start time, so
  the program it execs counts as the connecting process;
- a socket that is inherited across `fork` or passed to another process (SCM_RIGHTS)
  still names the process that connected it. The listener can't tell which process
  writes the bytes.

For each connection, in this order:

1. **Other users first.** A process running as another user, root included, is refused
   with the nack `foreign_uid` before anything else about it is read: no `/proc` read,
   no card scan, and its request is never parsed.
2. **Pinning the process** (Linux 6.5 and later). At accept the listener takes a pidfd
   for the connecting process (SO_PEERPIDFD). A pidfd refers to that process for as
   long as it is held, whatever later gets its pid. The listener then reads the
   process's start time and its parents (at most 16) from `/proc`, twice. After both
   reads the pidfd must still show the process alive at that pid, so what was read
   belongs to it and not to a later process with the same pid. A parent that exits
   during the reads changes its child's parent (the child is reparented), so the two
   reads differ.
3. **Cards.** The listener reads the live session cards in the registry. A card holds
   its process's pid and start time (`start_ticks`, from `/proc/self/stat`, written at
   registration; a reload re-registers every successfully restored event endpoint, so
   their cards have it too). A card matches a process only when both the pid and the
   start time match, so a stale card whose pid now belongs to another process matches
   nothing. A card without a start time (written by an older build) never verifies
   anyone: `legacy_card`.
4. **The verdict.** The first rule that answers decides:
   1. The connecting process is the listener's own: verified `in_process` only if the
      event's `sender` names a session whose card this process serves. No sender:
      `no_claim`. A sender that is no session served here: `unknown_sender`.
   2. A resolver, if one is installed (`events::set_peer_resolver`; none is installed
      by default), is asked about a pinned process that isn't the listener's. It names
      a session (verified `resolver`, provided that session's card is live and its
      start time is the one read now for the card's pid, else `no_card`), refuses with
      a reason (final: the rules below don't run), or passes.
   3. The connecting process is the process of exactly one live card: verified
      `process`. A process serving several sessions (a daemon) can't say which one
      sent: `shared_host`. This rule and the next match by process, so they need every
      card: after a card scan that didn't see them all (see Bounds) the verdict is
      unverified `scan_incomplete`, since a card it never saw could share the process.
   4. Otherwise unverified: `tool_child` (one of its parents is a card's process) or
      `no_card`.

Missing evidence fails closed. When the process can't be pinned in time, no rule above
runs and the verdict is unverified with one of these reasons:

| Reason | When |
|---|---|
| `peer_gone` | The process had exited when the pidfd was taken, or by the time its `/proc` reads were checked. |
| `unreadable` | Its credentials, its pidfd's state or its `/proc` entry couldn't be read, or its pid isn't visible in the listener's pid namespace. |
| `evidence_stale` | The two reads of the process and its parents differ. |
| `no_pidfd` | The kernel gives no pidfd for connections (Linux before 6.5). |
| `unsupported` | The platform can't pin the process (macOS, Windows). |
| `timeout` | The identity step didn't finish within its deadline (below). |
| `busy` | Too many identity steps were running already. |

#### Bounds

Pinning, the card scan and the resolver form one identity step. It runs off the
listener's own tasks, on a blocking thread, at most 4 at once per process, with one
deadline of 500 ms counted from accept; the request is read meanwhile. A step that
can't start reads as `busy` and one past its deadline as `timeout`. Either way the event
is still queued and acked: identity is an attribute of the event, and only a foreign uid
is refused. A step that stalls keeps one of the 4 places until its read returns, so a
stuck `/proc` read can make later connections `busy`, but never holds them up.

The card scan opens at most 256 card files, in directory order. When there are more,
or a card file can't be read, the scan is incomplete: rules 1 and 2 still decide (they
find a card by its session id), and rules 3 and 4 give `scan_incomplete`. Only regular
files of at most 64 KiB are read. A larger card, a symlink or any other kind of file is
no card: it is skipped, never trusted, and leaves the scan complete.

#### Trust

Cards are input from this user: any process running as this user can write one, naming
any pid and start time, and so make a process of its choosing read as a session. So
`verified` holds against other users only. It says which session's process connected,
provided nothing else running as this user lies.

#### Who reads as what

| Sender | `peer` |
|---|---|
| Code in the receiving process that sets `sender` to a session it serves | verified `in_process` |
| A standalone session's own process, sending to another process | verified `process` |
| A daemon, sending from one of its sessions to another process | `shared_host` |
| A shell command a session runs (`synaps send` from a tool), and anything it starts | `tool_child`; verified only through a resolver |
| A subagent | not verified by the rules here: the commands it runs are tool children, and it has no way to send from inside the process. A resolver could name a session for one, so whether a subagent is ever verified depends on the resolver the host installs (one that refuses gives `subagent_call`) |
| Any other process of this user (scripts, timers, a terminal) | `no_card`, or `peer_gone` if it had already exited |
| A process running as another user, root included | refused: nack `foreign_uid`, nothing queued, its request never read |
| Any process matched by process alone, while the registry holds more than 256 card files or one can't be read | `scan_incomplete` |
| Any process, on Linux before 6.5 | `no_pidfd` |
| Any process, on macOS; a Windows named-pipe client | `unsupported` |

Readers must accept `via` and `reason` values they don't know, and treat a `peer` that
says `verified` without a `session_id` as unverified. The other reasons a resolver can
give: `left_the_call`, `no_ledger_record`, `ledger_truncated`, `ledger_full`,
`ambiguous_caller`, `call_ended`, `not_a_bash_call`, `subagent_call`, and
`evidence_stale` (which the listener also gives).

Limits:

- Start times are counted in clock ticks since boot (normally 10 ms each). A process
  matches a card it doesn't own only if it has the card's pid and started in the same
  tick as the card's process: that process would have to register, exit and have its pid
  reused all within one tick.
- A parent's identity only ever labels a process `tool_child` (and is passed to a
  resolver); it never verifies on its own. A parent that exits during the reads shows up
  as `evidence_stale`, because its child's parent changes between them; what remains is
  the same-tick case above.
- The verdict holds for the moment of the reads. A process that exits after its reads
  were checked keeps its verdict.
- Cards and the listener are assumed to share a pid namespace: a card's pid is looked up
  in the listener's.
- The deadline is real time. On a heavily loaded machine an honest sender can read as
  `timeout` or `busy`.
- macOS has no pidfds, so no connection there is pinned or verified. Windows doesn't
  report the client.

### Strict send (`synaps send --strict`)

```
synaps send --strict --session <exact id or name> [--source S] [--severity low|medium|high|critical]
            [--channel C] [--content-type T] [--json] (MESSAGE | --body-file <path|->)
```

A strict send delivers one event to exactly one live session and says what happened.
It never writes the inbox, never broadcasts and never matches a prefix. Plain
`synaps send` is unchanged: it still resolves a unique prefix, falls back to the inbox
and prints "Event sent" once the bytes are written.

**Which session.** The target is a session id or a session name, matched exactly (case
matters). The run dir is read without its lock, and nothing there is pruned, created or
written. A card counts when its id or name is the target and its owner pid is alive;
cards with a dead owner are only counted in the message. Exactly one card must count,
and then:

- its socket must be directly in the run dir (the canonical path; the card's path is
  compared as written), and on Unix it must be a socket. A socket that is missing is left
  to the connect, so a session that is restarting reads as `connect_failed`.
- its card must advertise receipts (`"receipts": 1`). A session on an older build gets
  nothing: `receiver_too_old`.

**What "sent" means.** Sent means the session acked this event's id: the event is in
that session's event queue. It does not mean a turn ran, or will. After the whole event
is written, anything other than an ack or a nack for it (the connection closes, no
answer within 5 s, an unreadable answer, an ack for another id) is uncertain: the event
may or may not be queued. A daemon reload shows up as `connect_failed` (not sent) while
the sessions re-register. Retrying is the caller's choice; a strict send never retries.

| Result | Exit | Why codes |
|---|---|---|
| `sent` | 0 | (none) |
| `usage` | 2 | (none; `detail` says what was wrong) |
| `not_sent` | 3 | `no_live_session`, `ambiguous`, `registry_unreadable`, `socket_outside_run_dir`, `not_a_socket`, `receiver_too_old`, `oversized`, `connect_failed`, `write_failed`, `nack_invalid`, `nack_oversized`, `nack_queue_full`, `nack_timeout`, `nack_foreign_uid`, `nack_unknown` |
| `uncertain` | 5 | `no_receipt`, `receipt_timeout`, `receipt_mismatch` |

Exit 4 is reserved and never used: a strict send writes no inbox. `nack_unknown` is a
nack whose reason this build doesn't know.

**Usage errors (exit 2).** `--json` or `--body-file` without `--strict`; `--strict`
without `--session`, with an empty one, or with `--broadcast`; both MESSAGE and
`--body-file`, or neither; a severity other than the four (any case); a source that is
not 1 to 64 characters of `A-Z a-z 0-9 . _ : @ -`; a body that is empty or only
whitespace; a body file that can't be read or isn't UTF-8.

**Size.** A session takes at most 256 KiB per event, counting the event's JSON and its
newline. A strict send checks the encoded size before it connects (`oversized`), and
`--body-file` reads at most 256 KiB + 1 bytes from the file, or from stdin for `-`.

**Output.** stderr gets one line. A sent event prints
`Event sent to session <id> via socket. Receipt <event_id>.`; anything else names the why
code and a short detail. With `--json`, stdout gets the result as one line, keys in this
order:

```json
{"result":"sent","target":"alpha","session_id":"s-1","event_id":"…","why":null,"detail":null,"peer_verified":false}
```

`target` is what the caller asked for; `session_id` is the session it resolved to, or
null. `event_id` is null when nothing was queued. `peer_verified` comes from the ack
(null without one); it is the receiving session's verdict on the sender (see above) and
authenticates nothing.

**Limits.** The timeouts are fixed: connect 2 s, write 5 s, receipt 5 s (and 1 s to
read a nack after a write that failed). A card or socket swapped between the checks and
the connect is not caught; the run dir is the user's own (mode 0700). On Windows there
is no pid or socket-type check: opening the pipe is the liveness test.

## Per-session journal lock (F10)

Each session with an active `Runtime` holds an exclusive advisory `flock` on
`<sessions_dir>/<session_id>.lock` (0600). This prevents two runtimes (e.g.
a daemon actor and an in-process `synaps --continue X` with `SYNAPS_DAEMON=0`)
from writing to the same journal — the second writer would silently erase the
first's turns (last-writer-wins).

| File | Scope | Purpose |
|---|---|---|
| `<sessions_dir>/<id>.lock` | 0600, per session | **flock = journal ownership.** Body: `<pid>\n<kind>\n` (`kind` = `daemon` or `tui`). Acquired on `SessionActor::create`; released on `Park` (no `Runtime`); re-acquired on `unpark`. Compaction (`LinkedSuccessor`) and `NewSession` re-acquire on the new id. |

**Behaviour:**
- `--continue X` when the lock is held → error: `session X is live in another process (pid N, kind) — use synaps --attach X, or synaps daemon sessions`.
- Fresh sessions: best-effort lock (warn on failure, e.g. read-only fs).
- Stale locks from dead processes never block (`flock` is released on fd close / process death).
- `unpark` failure (lock held by another process) → error surfaced to the attaching client: `cannot unpark session X: journal locked by another process`.
- **Windows:** `fs4` maps to `LockFileEx` — same semantics, no feature-gate needed.

## Protocol summary (line-JSON over UDS or a Windows named pipe, `DAEMON_MAX_FRAME_BYTES` = 64 MiB, same framing as `synaps rpc`)

```
C: {"type":"hello", protocol_version, client:{kind,terminal,instance}, cwd, client_version}   ← MUST be first
D: {"type":"welcome", protocol_version, daemon_version, pid, profile, sessions:[SessionMeta], progressive_tool_disclosure, generation, daemon_build}
   | {"type":"refused", reason:{reason:"version",daemon_version,min,max}|"protocol"|…, message}   → daemon closes
C: ping | sessions | shutdown{force}          → D: pong{pid,uptime_s,sessions} | session_list{sessions} | bye   (no session allocated)
C: close{session_id, force, attached}         → D: closed{session_id, text} | refused{busy|unknown_session} (→ daemon closes) | error
C: pin{session_id, on}                        → D: pinned{session_id, on} | refused{busy|unknown_session} (→ daemon closes) | error
C: rename{session_id, name?, expect?}         → D: renamed{session_id, name, in_memory_only?} | rename_failed{session_id, reason, message, taken_by?, current_name?} | refused{unknown_session} (→ daemon closes)
C: reload_extension{extension}                → D: extension_reloaded{report} | extension_reload_failed{extension, message, unloaded} | refused{busy} (→ daemon closes)
C: {"type":"attach", attach:"existing", session_id, mode} | {"type":"attach", attach:"create", config:SessionConfig, mode}
D: {"type":"attached", client, meta, view, conversation, streaming, replay:[WireEnvelope], pending_prompts, clients} | error
C: {"type":"cmd", session_id, cmd:SessionCommand} …      → D: {"type":"event", session_id, seq, ts, event:WireSessionEvent} …
C: bye | socket close = Detach (turn keeps running)
```

- Version policy: exact match on `protocol_version` (min == max == 3) → `Refused{Version}` **loudly**
  (client exit 2). Different *binary* (version, or `daemon_build`: the commit it was built from) → allowed;
  the TUI leaves a transcript notice on attach and after every reload reconnect (an attached TUI keeps
  its own binary across `daemon reload`), and sends slash commands it does not handle to the session.
  `WireSessionEvent` has `#[serde(other)] Unknown` so additive variants from a newer daemon are tolerated.
- `WireSessionEvent` mirrors `SessionEventWire` variant-for-variant (`StreamEvent`/`TurnError`/
  `ExtensionLoaderEvent` included; round-trip test covers every variant), with **one lossy variant**:
  `Conversation` goes over the wire as a `ConversationDigest` `{messages_len, messages_hash (FNV-1a),
  tokens, cost, abort_context (legacy, always null), queued_message, pending_events_len, consecutive_auto_turns}` — never the
  messages. Full `api_messages` travel only in `Attached` and `QueryResult{Messages}`. `SocketTransport`
  keeps a local mirror (seeded by `Attached`, updated by `Stream(MessageHistory)`), fills the digest in
  when the hash matches, and on a miss (compaction, abort repair, flushed events) issues one
  `Query{Messages}` under the reserved id `DIGEST_RESYNC_QUERY_ID` (= 2^63) and re-emits the
  `Conversation` with the fetched history. Per-event wire cost is O(1) in history size; the O(history)
  cost is paid once per attach and once per tool round (`MessageHistory`, which the engine emits anyway).
- **Phase 4 (thin client) — `HistoryMode::Digest`** (`hello.client.history = "digest"`, `tail_items`,
  the `synaps --attach` default; `SYNAPS_CLIENT_HISTORY=full` restores the mirror above — the local
  `api_messages` copy, `MessageHistory` forwarding and `Query{Messages}` resync — and nothing else of
  the client diet): the
  daemon's `Attached` carries `conversation.api_messages = []` + `conversation.messages_len` + a
  `display_tail {items:[{kind:user|thinking|text|tool_use,…}], omitted, times?}` projected by
  `session::display::display_tail` — the SAME filter the in-process TUI applies to its history, so Local
  and Socket render byte-identical transcripts (golden test, `differential.sh` S pane). `times` holds each
  item's message time (RFC 3339 UTC, parallel to `items`), omitted when no item has one (legacy history,
  an older daemon). The conn
  forwarder drops `Stream(MessageHistory)` for Digest clients (they key on the `Conversation` digest that
  follows; `messages_len` is always filled); the mid-turn `replay` ring is filtered the same way. The
  client keeps NO mirror, never re-serialises history for `matches()`, never issues `Query{Messages}`;
  after compaction/resume it issues one `Query{DisplayTail{items}}` (ordinary id) and rebuilds from the
  answer. `/resync` is the manual path. Per-event AND per-attach wire cost is O(tail), not O(history).
  Digest mode is the TUI's; `synaps attach`/`send` stay Full. Frames are read into one reused buffer per
  connection (shrunk to 64 KiB after any > 1 MiB frame). `serde_json` `float_roundtrip` is on: `cost`
  crosses the wire bit-exact (the footer's `$0.0002` used to read `$0.0001` on the socket).
- **Frame cap: 64 MiB both directions**, distinct from rpc's 1 MiB (an `Attached` for a long session is
  several MiB). Enforced symmetrically: `encode_line` refuses to build an oversize frame (the daemon sends
  `Error{"daemon could not encode a frame: …"}` and closes), both readers are `take()`-bounded and reply
  `Error{"frame exceeds 64 MiB limit"}` + close. Tested: a > 1 MiB history attaches
  (`attach_to_session_with_history_over_1mib`), a 2 MiB `ping` is answered, a 64 MiB + 10 B frame is refused.
- Query ids ≥ `RESERVED_QUERY_ID_BASE` (2^63) belong to the transport/daemon (`DIGEST_RESYNC_QUERY_ID`,
  `IDLE_PROBE_QUERY_ID`, `CHECKPOINT_QUERY_ID`, `CLOSE_QUERY_ID_BASE`+n); `SocketTransport` never surfaces them
  as `QueryResult`.
- `close` was added without a protocol bump: a daemon older than it answers the unknown frame with
  `Error{"malformed frame: unknown variant `close`…"}` and keeps the connection open, and
  `synaps daemon close` reports "daemon … predates `daemon close`; run `synaps daemon reload`" (exit 2).
  A bump would refuse every older TUI that reconnects after a reload. `closed` only answers `close`, so
  older clients never see it. On an attached connection `close` is refused like `reload`/`purge`
  (`Error{"control frames are not accepted on an attached connection"}`); that client has `/close`.
- `pin` (`on` required) follows the same rule: an older daemon answers "unknown variant `pin`" and
  `synaps daemon pin` exits 2; `pinned` only answers `pin`; refused on an attached connection (that
  client has `/pin`). `SessionMeta.pinned` is `#[serde(default)]`: an older daemon omits it (→ false).
- `rename` (`name` optional, absent = clear; `expect` optional: absent = no precondition, `null` = only
  if unnamed, `"x"` = only if named `x`) follows the same rule: an older daemon answers "unknown
  variant `rename`" at once (no hang) and `synaps daemon rename` reports "daemon … predates `daemon
  rename`; run `synaps daemon reload`" (exit 2); `renamed`/`rename_failed` only answer it; refused on
  an attached connection (that client has `/saveas`). Its replies use `RENAME_QUERY_ID_BASE`+n.
- `reload_extension` follows it too: an older daemon answers "unknown variant `reload_extension`" and
  `synaps daemon reload-extension` exits 2; its two replies only answer it; refused on an attached
  connection.
- First frame not `Hello` / malformed → `Refused{Protocol}`; oversize frame → `Error` + close;
  `Cmd` before `Attach` → `Error`, connection stays open.
- **Client command whitelist** (`conn.rs::client_may_send`): an attached client may send `Submit`,
  `Steer`, `Cancel`, `Answer`, `Set`, `Compact`, `NewSession`, `Save`, `Query`, `EngineCommand`, and
  `Detach` **for its own client id only**. `Detach{other}`, `Attach`, `Resync`, `End`, `HostEvent` →
  `Error{"refused: …"}` and nothing reaches the actor. A client ends its session with `/close` (an
  engine command, owner-only), `synaps daemon close` ends one from a control connection, and
  `synaps daemon stop` ends them all. This is a footgun guard on a same-uid 0600 socket, not an auth boundary
  (`client_commands_are_whitelisted`).
- `Attach::Create` fills `cwd` from `Hello.cwd`; it must be absolute and exist.

## Security posture

- Socket 0600 in a 0700 dir; same trust domain as every existing per-session socket. On Windows an
  owner-only named pipe ([Windows](#windows)).
- **No secret ever crosses daemon → client.** `Welcome`/`Attached` carry summaries only (grep-tested for
  token/secret/key names). Client → daemon `Answer.value` (e.g. a sudo password) is forwarded to the actor
  and nowhere else: `ClientFrame`'s `Debug` redacts `Answer`/`Submit`/`Steer` bodies, and
  `tests/daemon_no_tui_dep.rs::daemon_never_traces_answer_bodies` greps the daemon + socket transport
  sources for any `tracing::` line mentioning `answer`/`value`.
- A bash tool command raises a sudo password prompt only through sudo's askpass hook, verified as a
  set-uid sudo inside that tool call ([sudo-askpass.md](sudo-askpass.md)), never from its output.
- Backpressure, never silent drops: bounded writer queues both ways (`CMD_CHAN_CAP` = 256);
  `send` returns `TransportError::Backpressure`; the daemon replies `Error{"backpressure…"}`.
- **The daemon trusts its uid.** Anyone who can open the socket can `shutdown`, `Attach::Create` with any
  `SessionConfig` (`prompt_manifest`, `auto_approve_confirms`, `persist:false`), and `Cancel`/`Compact`
  a session someone else is attached to. That is the same trust as the shell that started the daemon.

## Windows

The daemon runs natively on Windows 10/11. The protocol, sessions, pins and
reload-state are the same; the operating-system pieces differ:

- **Endpoint.** `daemon.sock` is a named pipe, `\\.\pipe\synaps-daemon-<hash>`,
  the hash taken from the path's canonical, lower-cased form
  (`crates/agent-engine/src/ipc.rs`). Nothing is created on disk at the path.
  Its instances get a protected DACL granting this user and SYSTEM only, and
  refuse remote clients. A client refuses a pipe whose server process runs
  as another user, so no other account can squat the name while no daemon
  serves it. This is what the 0600 socket, the 0700 dir and the peer-uid
  check do on Unix; the file modes under Files above are Unix-only.
  Per-session event endpoints (`synaps send --session`) are pipes too,
  `\\.\pipe\synaps-session-<hash>` of the card's `socket_path`, with the
  same owner-only DACL and remote refusal, and `synaps send` refuses one
  served by another user.
- **Spawn.** `--detach` and auto-spawn start `daemon --foreground` with a
  hidden console (so tool commands never pop a window), in its own process
  group (Ctrl+C in the terminal never reaches it), outside the caller's job
  when the job allows it, and with only the handles it needs. Readiness is
  a named event (`SYNAPS_DAEMON_READY_EVENT`) instead of the ready pipe; the
  child exiting first plays the part of EOF before `R`. There are no
  systemd scopes (`process_scopes` is Linux-only).
- **Reload.** There is no `execv`, and a byte-range lock belongs to the
  process that took it. After the checkpoint the old image starts the new
  one, which reads the reload-state, queues an exclusive lock request on
  `daemon.lock` behind the old image and signals it
  (`SYNAPS_DAEMON_HANDOFF_EVENT`). The old image exits, and its release
  grants the lock straight to the new one, so the liveness check never
  reads "free" in between. The new image waits for the old process to be
  gone, then binds and rehydrates as on Unix. **The pid changes.** If the
  new image exits first, or does not signal within 15 s, the old one keeps
  serving, as after a failed exec. `--exe` is canonicalised but not checked
  for owner or mode.
- **Stop.** There is no SIGTERM: the `Shutdown` frame is the graceful path,
  and the daemon stops on Ctrl+C in a foreground console. `daemon stop
  --force` runs `taskkill /PID <pid> /T /F` (the daemon and its children:
  MCP servers, shells) when the daemon is still alive after 10 s, then waits
  up to 5 s more.
- **Locks.** `daemon.lock` and the per-session journal locks are
  `LockFileEx` locks with the same semantics.

## Lifecycle

- Extension discovery runs **once** per daemon before accept (bounded 10 s) — sidecars are per daemon.
- Daemon SIGTERM/SIGINT/`Shutdown` → first the stop's checkpoints for the crash record (see "Restore
  after a crash"), then `End{HostShutdown}` to every session **concurrently** under ONE
  `SESSION_END_TIMEOUT` budget, then `ExtensionManager::shutdown_all`, then unlink files.
  Attached clients see `Ended` before `Bye`. **Worst case of a clean stop, request to exit: 37 s**
  (`DAEMON_STOP_WORST_CASE_SECS`): a crash restore still running gets 2 s to stop (then it is aborted;
  see "Restore after a crash"), the checkpoints 18 s (concurrent; quiesce grace 2 + cancel 3 +
  workers 10 + save 2 + 1), the session ends 12 s (concurrent; cancel 3 + save 2 + hooks 5 +
  observability flush 2), the extension shutdown 5 s. Not time-bounded, normally milliseconds: the
  record's one fsync'd write and the log flush. `--force`: no checkpoints, 2 s + 0.5 s + 5 s. A
  supervisor's stop timeout must exceed 37 s (a systemd unit's `TimeoutStopSec=60` does); `synaps
  daemon stop` waits 47 s. A SIGKILL during the checkpoints leaves the last periodic record, and the next start
  treats the stop as a crash; a SIGKILL after the record's write still restores it as a stop.
- `--idle-exit SECS`: exit after SECS of **zero connections, no pinned session and no session running a
  turn** (default: never). A pin is read from the handle (`SessionHandle::pinned`), no probe; tested:
  `a_pin_holds_the_daemons_idle_exit_and_unpinning_releases_it`. Sessions never end on their own (no `Parked` yet), so the monitor probes each client-less
  session with `Query{Status}` (reserved id `IDLE_PROBE_QUERY_ID`) and treats `streaming:true`, a pending
  prompt, or **no answer within 2 s** (actor inside `compact()`/preflight, queue full) as busy — the
  daemon never exits under a running turn and never aborts one. Idle client-less sessions are ended
  through the normal `End{HostShutdown}` path (saved to `sessions/<id>.json`; `synaps attach --continue`
  brings them back). Tested: `idle_exit_counts_clientless_idle_sessions_and_never_a_running_turn`.
- **Extension notification router** (`extensions::notify_router`) is spawned by `run_foreground` after
  discovery: a sidecar's `widget.*` frame goes to the session named by `params.session_id` (dropped with
  a `debug!` if that session is not live), or to **every** live session when the frame carries none
  (daemon-global — the pre-phase-3 behaviour). Plugin contract: `docs/extensions/session-id.md`;
  `heartbeat`/`jawz-widget` stay last-writer-wins until their own repos adopt it.
- **Compaction is inline in the actor**: `Attach`/`Detach`/`Cancel` wait behind a running `compact()`;
  `SocketTransport::attach` gives up after `ATTACH_TIMEOUT` (5 s) with "attach timed out" — retry.
  Spawned compaction is day 2.
- **An interrupted turn is kept as real history — every cancel path, every host.** Esc/`Cancel`,
  `End{ClientQuit}` (chat's stdin EOF / `/quit`, the in-process TUI's quit — never `synaps attach`, which
  only detaches), `daemon stop`, a reload `Checkpoint` and the session cost cap all run `cancel_turn()`:
  the token is cancelled, pending prompts are answered `None`, and the still-live stream is drained
  (bounded by `budgets::CANCEL_DRAIN_TIMEOUT`, 1 s) so the engine's cancel-path history — the partial
  assistant message (text, signed thinking, completed tool calls), every completed tool round,
  delivered steering, a labelled canceled `tool_result` for any unfinished call — plus the final
  `Usage` and any in-flight context-head checkpoint reach the actor. That history is adopted verbatim
  and ONE interruption marker is appended (`engine::interrupt`: `[Request interrupted by user]`, or
  the cost-cap / restart / host / driver variant). Nothing already sent is edited, so the provider's
  cached prefix survives and the next request extends it. This replaces the old `abort_context` recap,
  which re-described the model's own output inside the next user message and which current models
  refuse as a prompt injection. Sessions saved with a recap are migrated on load (recap dropped, marker
  appended). Documented in `synaps chat /help` too.
  - A turn the engine had already FINISHED when the cancel reached it (Esc a moment too late) is not
    marked: the engine records a normal end (`TurnCompletion`), the complete answer is kept as is and
    clients get `Done`, not `Aborted`.
  - A driver turn cut by the driver's revocation (its token is a child of the driver's) ends the same
    way, with `[Request interrupted: session driver revoked]`: on its `Done`, its stream's end, or after
    the same 1 s budget — never through the normal post-turn path (queued auto-send, auto-compaction).
  - Markers are matched EXACTLY (`is_interruption_marker`): user text that merely starts like one is
    the user's.
  - A canceled `tool_result` names the cause the same way (`CancelCause`, noted by the host before it
    cancels): "Canceled by user" only for the user, else e.g. "Canceled (Synaps restarted)",
    "Canceled (session cost cap reached)", "Canceled (session driver revoked)".
- **The session on disk follows a running turn.** The engine publishes the conversation at every
  round boundary — the prompt before the first request, each completed tool round (every
  `tool_use` paired with its `tool_result`), rollover heads — and the actor saves each one. Saves run
  on the actor's background writer (`session::persister`), never inside the turn machine: latest wins
  per session (if rounds come faster than the disk, only the newest history is written), an identical
  snapshot is not queued twice, and every write is ordered with the others (`session_save_order`). The
  actor waits for the writer (bounded by `SAVE_TIMEOUT`) only where the disk must be current: at
  the end of every turn, before it announces `Idle` (so `Idle` still means "saved", as in 0.10.0),
  before parking (a failed save keeps the session live), a durable context-head checkpoint,
  compaction, `/resume`, `Checkpoint` and session end. A crash / `kill -9` / power loss mid-turn loses at most the
  round in flight, and what is on disk is always a valid history to resume from.
  Cost (`json` mode rewrites and fsyncs the whole file; measured on btrfs/NVMe with
  `save_cost_bench` in `agent-core` — set `SYNAPS_SAVE_BENCH_DIR` to a real disk, NOT tmpfs, whose
  fsync is free), release build, 3 runs: ~0.6–0.9 ms median for a 200 KB session, ~2.2–2.4 ms for
  2.3 MB, ~6–7.5 ms for 10 MB, with a p95 of 5–17 ms (2.3 MB) and 9–21 ms (10 MB) and occasional
  20–30 ms spikes (filesystem commits) — paid by the background writer, not the turn.
  `session_persistence = journal` appends only the new messages, but at these sizes it is no cheaper
  (each save re-hashes the saved prefix to validate it).
  Attach replays carry no per-round `MessageHistory` or `Conversation` (the snapshot already holds the
  latest history), and a round checkpoint drops the finished rounds' display events from the replay
  ring, so a client attaching mid-turn sees each round once. Every other attached client learns the
  prompt that started a user turn (`TurnStarted.user_text`; the submitter drew its own card), each
  turn's reply opens a new text block, and a notice landing mid-reply no longer splits the reply.
- **A turn cut off by a crash is recovered on the next load — by the session lock holder only.** While
  a turn runs, the actor keeps a small draft `sessions/<id>.turn` (`agent_core::core::session_draft`):
  the text of the response in flight plus the history length it continues from — written at turn
  start, at most once per 1 Hz turn tick while text streams, and removed when the turn ends, only
  AFTER the history that ends the turn is saved (same background writer; a removal waits for a good
  save). 0600, confined, atomic, bounded on read; O(partial text) in either persistence mode. Only the
  holder of the session lock writes a draft or recovers one: daemon create/unpark, `--continue` in the
  in-process TUI, and `/resume` (which takes the new session's lock BEFORE changing anything and
  refuses a session locked by another process or another session of the daemon). A draft found by the
  holder means the previous holder died mid-turn: the partial text is appended as a real assistant
  message (text only, like a cancel) followed by `[Request interrupted: Synaps stopped unexpectedly]`,
  saved, then the draft is removed (if that save fails, the draft stays for the next holder). A leftover
  draft of a turn that actually concluded (history already ends with the model's final reply or an
  interruption marker) is just removed; a stale one (its round already committed) contributes no text.
  Recovery only ever appends, so the cached prefix is untouched. rpc and `synaps server` save every
  round but keep no draft; they DO take the session lock (`setup::lock_session`): continuing a
  session another process has live (a TUI, the daemon, another rpc), or one already compacted into a
  successor, is refused with the reason named, instead of running a second writer on the same
  history — and, as lock holders, they recover a draft their predecessor left. Legacy chat takes no
  lock. Deleting a session (and retention) removes its draft.
- Refuse-to-start (exit 3): flag unset; another daemon holds the lock.
- Daemon lost (exit 4, `EXIT_DAEMON_LOST`): the daemon was killed/crashed, the client could not reconnect
  within `SYNAPS_TUI_ATTACH_RECONNECT_SECS` (default 60). Stderr prints `synaps: lost the daemon (pid N)
  and could not reconnect within M s — session <id>; resume with synaps --attach <id> / --continue <id>`.
  Both TUI (`--attach`) and line client (`synaps attach`) exit this way; the in-process TUI is unaffected.
  During the reconnect window the client shows `daemon connection lost — reconnecting (Ns left)…` and
  clears streaming/compacting state. A successful reconnect prints a notice and resumes normally.

## Stdio, last words and supervision

### A daemon's stderr never depends on its starter

An auto-spawned or `--detach`ed daemon used to get a **pipe** for stderr, read by the spawning client
only until that client exited. The daemon's next stderr write then failed with `EPIPE`; `eprintln!`
turns a failed write into a panic, and a release build (`panic = "abort"`) ended the daemon at once
with no log line. A `daemon reload` inherits fd 2, so the next image died at its first stderr write (its
boot-time config warnings). Every `load_config()` also printed the config-family diagnostics
(`shell.*`, `events.*`, `turn_budget.*`, `memory.*`) to stderr, so one config line an older binary
rejects (e.g. `turn_budget.event.*`) killed it on the next session save, auth selection or timer.

Now (`daemon/stdio.rs`, `core/stdio.rs`):

- The spawner gives the child an **append-only file**, `<base>[/<profile>]/logs/daemon-stderr.log`
  (created `0600`, never truncated, not rotated), and reads a failure before the ready byte from what
  the child appended (last 64 KiB) instead of a pipe tail. If the file cannot be opened it falls back to
  the old pipe.
- Once ready (after the `R` byte), a daemon whose stderr is still a **pipe, a terminal or closed** moves
  it onto that file (`dup2`; a terminal gets one line saying so), and such a stdout onto `/dev/null`. A
  regular file, `/dev/null` or a socket (a supervisor's journal) is left alone. This covers daemons
  spawned by older clients, one started by hand in a terminal, and every reload's next image (which
  inherits the moved fd). One `daemon: detached stdio from its starter` log line says what moved.
- Every stderr write in Synaps' own code that the daemon can reach is non-panicking
  (`agent_core::eprintln_lossy!`): config
  warnings and family diagnostics, the logging-init failure, the `auth.json` reset notice and the
  daemon's own exit reason. The config sinks also log a `warn` line (`config diagnostic`, the
  `server.*`/`bridge.*` value warnings), so they reach `synaps.log`. `load_config()` runs on hot paths,
  so each distinct config diagnostic is reported once per process (at most 1024 remembered); a changed
  line gives a new message, which is reported, and a reload's next image reports them again.
- **Limit:** a dependency still prints with `eprintln!`/`println!` on its own error paths:
  `tracing-appender` 0.2.5 (a failed flush of the log file, a failed creation of the next log file; a
  shutdown signal that times out prints to stdout, which is `/dev/null` once detached). With stderr on
  the file,
  those writes fail only when that file cannot be written either (a full disk, another I/O error), and
  under `panic = "abort"` such a failure still ends the daemon.

### Last words: a panic is in `synaps.log`

The panic hook (`src/panic_hook.rs`, installed first thing in `main`) writes **one** `ERROR` line through
tracing before the default hook: target `synaps_cli::panic` (the file format hides targets; grep for
`PANIC`), with `thread=`, `location=`, `payload=` (Debug-quoted, at most 4 KiB) and `backtrace=` (at most
40 frames on one line, joined by ` | `, the panic machinery's own frames dropped, captured even without
`RUST_BACKTRACE`). When the panic aborts (`panic = "abort"`), it then drains the log writer (`try_lock`
only: it never waits for a lock the panicking thread may hold), then runs the default hook (stderr, now
the file above). A panic from the logging stack itself (the `tracing-appender` writer thread, or a
location inside a `tracing*` crate) gets no tracing line, so the hook never re-enters a broken logger.

A release binary is stripped (`strip = true`), so its backtrace frames read `<unknown>`; thread,
location and payload are still exact. A build with `CARGO_PROFILE_RELEASE_STRIP=debuginfo` keeps symbol
names.

### `panic = "unwind"` builds (dev installs)

The release profile aborts on any panic, in any thread or task. Built with
`CARGO_PROFILE_RELEASE_PANIC=unwind`, a panic ends only its thread or tokio task: the daemon keeps
serving. What then happens:

- **A session actor panics:** `EngineHost::create_session` catches it (`session actor panicked`, ERROR),
  removes the session from the map, and its attached clients get `Bye` and exit: the session is gone,
  nothing hangs. Its journal keeps what was saved before the panic.
- **A connection task panics:** that client's connection drops (EOF). The connection count is given
  back on unwind (`daemon/listener.rs`), so idle-exit and the reload announce wait do not wait for it.
- **Any other spawned task panics:** that task ends; whoever awaits its handle sees a `JoinError`.
- The hook never shuts the log writer down under unwind (the old hook did, so a dev daemon lost every
  log line after its first contained panic); `main` flushes when a panic unwinds out of it.
- Known and not changed: a `std::sync::Mutex` poisoned by a panic makes later `.lock().unwrap()` calls
  panic too (one panic can take down every task that uses that lock), and the TUI's own hook restores
  the terminal on any panic, even one only a task dies of.

### Running under systemd (`contrib/systemd/synaps-daemon.service`)

An optional user unit supervises the default profile's daemon: `synaps daemon --foreground` (no idle
exit), `Restart=on-failure` with `RestartSec=1` (a crash or a failed start restarts; a clean stop stays
stopped), at most 5 starts a minute (a crash loop then leaves the unit `failed`: fix the cause, then
`systemctl --user reset-failed synaps-daemon` and start it again), `KillMode=mixed` (SIGTERM to the daemon for its
graceful shutdown, then SIGKILL to whatever it left in the unit; tool commands run in their own scopes
and are untouched), stderr **appended** to
`%h/.synaps-cli/logs/daemon-stderr.log` (the file a detached daemon uses too), environment from
`%h/.synaps-cli/daemon.env` if it exists. Install and use:

```sh
mkdir -p ~/.config/systemd/user ~/.synaps-cli/logs   # logs/ must exist: see below
cp contrib/systemd/synaps-daemon.service ~/.config/systemd/user/
(umask 077; echo "PATH=$PATH" > ~/.synaps-cli/daemon.env)   # your shell's PATH, see below
systemctl --user daemon-reload
systemctl --user start synaps-daemon     # and `enable` to start it at login
systemctl --user stop synaps-daemon      # stop for good (or `synaps daemon stop`)
systemctl --user show -p LoadState --value synaps-daemon.service   # `loaded` = installed
```

- **The installer writes `daemon.env` and creates `logs/`.** A user manager's `PATH` is minimal (no
  `~/.cargo/bin`, no `~/.local/bin`), and the tools sessions run need the shell's: `daemon.env` holds
  `PATH` plus anything else the daemon previously had from the shell that started it, one `KEY=VALUE`
  per line (no shell syntax; its values override the unit's `Environment=`). It is optional. The
  `logs/` directory is not: with it missing, `StandardError=append:` cannot open the file and every
  start fails with status `209/STDERR`.

- **Coexisting with auto-spawn:** a client finds the running daemon by its flock and socket and spawns
  none. `synaps daemon reload` re-execs in place (same pid), which systemd follows. If a client
  auto-spawned a daemon while the unit was down (e.g. inside the 1 s restart gap), the unit's starts
  fail (that daemon holds the profile's lock) until the start limit stops them;
  `synaps daemon stop`, `systemctl --user reset-failed synaps-daemon`, then
  `systemctl --user start synaps-daemon` hands the profile back to the unit. With the unit, set
  `daemon.idle_exit_secs = 0` too, so a daemon a client spawns in that gap does not idle-exit either.
- `synaps daemon stop --force` escalates to SIGTERM, then SIGKILL; a SIGKILL under the unit counts as a
  crash and restarts it. Use `systemctl --user stop synaps-daemon` to stop it for good.
- **Editing the live config:** a line goes into the config a running daemon reads only if the
  **running** binary accepts it with no warning. Lines only a newer binary knows go in between
  `synaps daemon stop` (or `systemctl --user stop`) and the new binary's start, after a dated backup of
  the config. Whoever operates the install owns those edits.

## Autonomous driver (in the session actor)

The autonomous session driver (`session.drive` permission; see
[session-drivers.md](extensions/session-drivers.md)) runs **inside the session
actor**, not in any client process. That changes its lifecycle relative to the
old "local TUI only" world:

- **The grant survives detach.** Headless runs are the feature: a client can arm
  a driver with `/auto …`, disconnect, and the actor keeps driving. The **safety
  floor** is the S1 zero-client revoke — when the *last* client detaches, the
  actor revokes the grant (`no clients attached`) and fail-closed-denies any
  pending host-confirmation prompt (`None`). So a run persists across detach only
  while at least one client remains attached; it never spends fully unwatched.
- **Park is blocked while armed.** `can_park()` requires no active driver
  (`self.driver.is_none()`), so an armed session stays **Live** — the idle
  monitor and park deadline will not put a driving session to sleep underneath a
  running or delayed driver turn.
- **Reload revokes the driver.** `Checkpoint{Reload}` revokes any armed grant
  (`daemon reloaded`) before saving, and the client is notified. Grants are not
  part of the `SessionReloadRecord` and are never rehydrated — consistent with
  "process restart must never restore an active run automatically."
- **Remote steering.** `synaps send` is a dumb pipe: it delivers an `Event` to
  the session's per-session event queue, **not** a `SessionCommand`. The actor's
  `on_queue_wake` path owns the routing: while a driver turn is streaming the
  event is **`Steered`** into the live stream (steer a driven session from
  another terminal); idle-and-armed injects it without a competing turn
  (`WakeAction::RunTurn` inhibited); idle-and-unarmed runs a normal Submit-like
  turn. Pair `synaps attach --observe` (watch the stream) with `synaps send`
  from a second terminal for remote observe-and-steer.
- **Follow-up (not yet done):** `SessionQuery::Status` does not expose a
  `driver_armed` flag (its payload is `{session, model, tokens, cost, streaming,
  auto_turns, attached, pending_prompts}`), and `synaps send` does not speak
  `SessionCommand`. Armed-state routing is entirely the actor's job today, so no
  client needs to branch on it. Exposing `driver_armed` would be a one-line
  addition if explicit client-side steer/submit branching is ever wanted.

## Event delivery

Two transports carry an external `Event` into a session; they differ in who
receives it.

- **Per-session socket (targeted).** `synaps send --session <id|name|prefix>`,
  the only-session case of a bare `synaps send`, `--broadcast`, agent-comms
  and watcher `notify_session` all write to the socket on the session's
  registry card. It always reaches that session, and wakes it even when
  parked (unpark, then an `EventAuto` turn as under Remote steering above).
- **Shared inbox (untargeted).** `~/.synaps-cli/inbox/*.json`. A daemon
  session takes inbox files only while a client owns its input (a `Mirror`
  or `Takeover` attach; `Observe` never owns input, so a keep-warm observer
  does not count). Detached and parked sessions leave the files on disk, and
  a session rehydrated by a reload takes none until a client reattaches. Each
  file is claimed by rename first, so exactly one session takes it. An
  in-process session (`synaps chat` without the daemon) takes inbox files
  while it runs. A headless `synaps agent` never takes them: nobody is at
  it, and it would race the session someone is at for the one copy. Reach
  an agent through its socket (`synaps send --session <agent>`).
- **`synaps send` and the inbox.** A `--session` that matches nothing is an
  error (exit 1, nothing written) that lists the running sessions; `--inbox`
  opts in to leaving it in the shared inbox instead. With no flag and zero or
  several sessions running, `synaps send` writes to the inbox and says so on
  stderr. An event addressed to a session (`--session`, `--broadcast`)
  never falls back to the inbox: a socket that still refuses after 3 tries
  (150 ms, then 500 ms apart; the registration is re-read between tries; 5 s
  cap per try) is an error naming the session, exit 1. `--inbox` opts back
  in for `--session`. Only the no-flag form, which names no session, still
  drops to the inbox when its one session refuses.

## Reload (`synaps daemon reload`, phase 3 C3)

Sequence (`daemon/reload.rs`, PLAN-phase3 §2.8), all on the requesting control connection:

1. **Version gate** before anything is disturbed: `<exe> daemon --print-version` (hidden flag, 5 s) must
   succeed, speak protocol ≥ ours, and be **newer or equal** by semver (`RefuseReason::ReloadRefused{why}`
   otherwise; equal allows same-version rebuilds). `exe` = the recorded exe (`daemon.json.exe`, taken from the image's in-memory record; argv[0] canonicalised at
   first start — not `/proc/self/exe`, which reads "(deleted)" after an in-place rebuild) or `--exe`.
2. **Drain**: `Attach::Create` → `Refused{Busy}`, `Submit`/`SubmitPrepared`/`Compact` → `Error("daemon
   reloading; retry in a moment")`; wait ≤ `--drain-secs` for every session to be idle; then
   `Checkpoint{Reload}` every session concurrently (≤ `CHECKPOINT_BUDGET` each): stop every running
   reactive worker and wait ≤ `WORKER_RELOAD_STOP_TIMEOUT_SECS` (10) for its finalizer (§Workers below),
   cancel (partial history kept + restart marker), answer prompts `None`, save, close PTYs, notice.
   Workers do not hold the drain: an idle parent waiting on workers counts as idle.
   - **The cap is hard.** Each drain pass asks every live session at once (its `Status` probe), all
     under the drain's one deadline (start + `--drain-secs`), and passes repeat every 200 ms. A probe
     with no answer by the deadline counts as busy, and the sleep between passes stops at the deadline,
     so the drain never runs past `--drain-secs`. It ends early once every session is idle. One INFO
     line ends it: `daemon: reload drain ended` with `elapsed_ms`, `cap_ms`, `sessions`, `idle`, `busy`,
     `writers` and `cap_used`.
   - **Worker records** (`daemon/worker_check.rs`; the full rule is in
     `docs/specs/subagent-context-rollover.md` §9). A worker run whose record is unresolved refuses the
     reload before anything is quiesced. A run whose record write is in flight counts as busy in the
     drain. After the drain, before anything is stopped, writes still in flight get 5 s to land; one
     still not durable refuses the reload (`ReloadRefused{why}`, the CLI exits 3, `why` starts
     `worker records are not durable:`). The quiesce lifts, and a turn the drain paused at a round
     boundary resumes in place. A reload's worst case before its checkpoints is the drain cap + 5 s.
     `--force` (`Reload{force: true}`) goes ahead without those records: the requester's
     `Bye{Reloading}` carries them as `waived: [{worker, session?, why}]`. The CLI prints one
     `reload forced: …` line on stderr, or a `waived` field with `--json`. With worker rollover off, no
     run is in the table, so a reload never waits or refuses here.
   - **Quiesce grace.** A turn still running at its checkpoint gets `QUIESCE_GRACE` (2 s) to end at a
     round boundary before the cancel. A drain that used its whole cap skips the grace: the quiesce
     flag went up before the drain, so the turn has already had the whole cap, and it is cancelled at
     once. Stream events already in hand are still handled first. An early drain keeps the grace.
     `--now` keeps it too, since with no drain the grace is a turn's only chance to end at a round
     boundary.
3. **reload-state** `daemon[-P].reload.json` (0600): generation, per-session `{id, journal_id, config
   (continue_session = journal_id, cwd, model), keep_warm, lifecycle, workers, tool_activations}`, and
   `next_subagent_id` (the `sa_N` counter).
4. **Announce**: every other connection gets `Event(Reloading{generation, retry_after_ms: 500})` +
   `Bye{Reloading}` (forwarders drain the checkpoint's events first); the requester gets `Bye{Reloading}`.
5. **Exec**: `daemon.json` rewritten (`generation+1`, same pid), sidecars stopped (≤ 5 s), the flock fd made
   inheritable (`SYNAPS_DAEMON_LOCK_FD`), `execv(exe, original argv)`. The listener is CLOEXEC (asserted in
   `listener::bind`) — closed at exec; the new image rebinds. **Exec failure**: the record from before the
   attempt is put back whole, in memory and in `daemon.json` (exe and started_at too, so the next default
   reload uses the old exe), the reload-state file removed, `reloading` cleared, the reload-announce token re-armed (new connections are served again),
   extension discovery re-spawned (the sidecars were stopped for the exec — bounded 10 s like boot), old
   image keeps serving (sessions checkpointed but alive; the requester already got `Bye{Reloading}` and
   reconnects to the same generation). From its checkpoint to the exec a session is fenced: no
   queued event, park, keep-warm ping or driver tick starts work, and commands that would are refused
   (`daemon reloading; retry in a moment`); a failed exec lifts the fence (`ReloadAborted`). Work that
   also waits for the session's `on_session_start` starts once both have cleared (the hook itself may run while fenced). `--exe` is validated **before** the probe: canonical path, regular
   file, executable, owned by our uid or root, not group/world-writable; then `--print-version` must parse
   and pass the gate. A failed `daemon.json` write at the exec or at the restore logs one WARN line. Tested:
   `exe_validation_refuses_and_exec_failure_keeps_serving`,
   `a_failed_reload_exec_leaves_the_previous_record_in_memory_and_on_disk`.
   The reload state also records the mask the daemon was first started with (`base_umask`; an exec
   inherits the mask the old image set): the new image applies `daemon.umask` from the config as it is
   now, so a changed value takes effect here and a removed line returns to the started-with mask.
6. **New image**: first of all (before the inherited-child snapshot, the host boot and the sidecars,
   so before it starts any process) `run_foreground` sets close-on-exec back on the `SYNAPS_DAEMON_LOCK_FD`
   fd if it is open on `daemon.lock` (`fcntl(F_SETFD)` only: same fd, same flock), so no child of the
   new image inherits the lock; one that outlived the daemon would hold the flock and every later fresh
   start would refuse. (The old image's window between making the fd inheritable and the `execv` is not
   closed: a process another of its threads starts in those microseconds inherits the fd.) Then
   `Daemon::start` sees `SYNAPS_DAEMON_RELOAD_STATE`, checks the hand-off first (the
   state file parses and `SYNAPS_DAEMON_LOCK_FD` is open on `daemon.lock`; anything else is a stale
   hand-off and the start is fresh, see the env table) → `DaemonLock::adopt(fd)` (same open file
   description = same flock, no gap), no `reap_stale`, rehydrates every recorded session
   (`create_session(continue_session = journal_id)`) **before** accepting; `reload_aliases` maps old→new
   ids for one generation (they differ only after a LinkedSuccessor compaction). Before the first session
   the handle counter is raised to `next_subagent_id` (and past every carried id); each continued
   session with tool activations gets the host-only `RestoreToolActivations` first, and each session
   with carried workers gets the host-only `AdoptWorkers` (both before any `Park`). The restore does not wait
   for `on_session_start`: each session runs it in the background, one hook at a time per host (a
   session restored Parked once it is unparked), so the restore does not add the hooks' serial
   latency (this does not bound the restore's other work); a session's first request still waits
   for its hook.
   The exec keeps the pid, so the old image's child processes (a tool command killed in the drain, a PTY
   shell, an MCP server) become the new image's children, which it holds no handle on. The new image lists
   them before it starts any process of its own and reaps exactly those pids as they exit
   (`daemon/reap.rs`: `waitpid(pid, WNOHANG)` per pid, on SIGCHLD and a 60 s tick; never `waitpid(-1)`,
   so the image's own children keep their exit statuses). One log line when there were any:
   `daemon: inherited N children, M already exited and reaped`. Linux only; it relies on the daemon not
   being a child subreaper.
   **Resume**: before `AdoptWorkers`, each session that is not Parked gets the host-only
   `ResumeAfterReload{generation}`. A session whose turn the reload cut (its checkpoint record says
   `turn_cut`, and nothing answered it since; a turn the drain's quiesce stopped at a round boundary gets
   the restart marker too and counts as cut) gets ONE user message, "resume: the daemon reloaded mid-turn (HH:MM UTC); your
   last turn was cut, so check your last tool call before trusting it, then continue.", and a new turn
   starts. Idle sessions get nothing; the same generation never resumes a session twice. A record with
   no `turn_cut` (an older image did the reload, or the checkpoint did not reply) resumes nothing: the
   restart marker carries no time, so an earlier reload's unanswered marker can't be told from this
   one's. A cut turn that something re-prompted after the cut (a user message past the cut turn's own
   tail, an event-shaped one included: an operator's sweep or a fleet message) is not resumed again.
   The checkpoint records that tail as `cut_tail` (how many messages followed the marker when it saved
   the session: events that arrived during the turn and were flushed in at the cut, a host note), so
   the turn's own events never count; a record without `cut_tail` (an older image) has no such check.
   Off switch:
   `daemon.reload_resume = off` in the config file (default on). Log lines: target `agent_engine::reload`.
7. **Clients**: `SocketTransport::next_event` returns `None` with `last_error = Reloading{generation}`;
   `reconnect(mode)` backs off, sends `Hello{reconnect_of}` and `Attach::Existing{id, Takeover iff
   was_owner else mode}` — two reconnecting mirrors cannot both take over.

**Not preserved** (stated once): in-flight turns (checkpointed = cancelled; partial history + restart
marker kept; the new image then resumes them, step 6), pending prompts (`None`), PTY/background shells (closed, announced before exec),
`turn_replay`, input ownership (re-established by reconnect order + `was_owner`), running reactive
workers (stopped resumably, see below).
**Preserved** — each session's `Checkpoint{Reload}` reply carries a `SessionReloadRecord` that the new
image rehydrates from: the journal and session id (same journal continued; `reload_aliases` only after a
LinkedSuccessor compaction), the `SessionConfig` as created (cwd, `--system`, prompt manifest,
compaction policy, auto-compact, persist), the keep-warm pin, the lifecycle (**a Parked session comes
back Parked** — rehydrate creates it then sends the host-only `Park`), the non-persisted runtime knobs
(`settings_replay`: `/context`, compaction model, API retries, subagent/bash timeouts, tool-output cap,
worker grants, **`/system`**), the **current** model/thinking (a `/model` change mid-session
survives; the create-time `--model` override is not re-applied), and the session's **exact tool
activations** (`activate_tools` grants under progressive disclosure; see
[Tool activations across a reload](#tool-activations-across-a-reload-or-a-park)). A session that never ran a turn has no
journal (`save` skips an empty conversation): it is recreated fresh under a new id and aliased.
A session whose journal lock is held by another process (e.g. an in-process `synaps --continue X`) is
NOT recreated — it is registered as a **Parked placeholder** under the same id with `locked_by` set
in its metadata. Attach is refused with a message naming the lock holder's pid and kind (F23).
`daemon sessions` shows it as Parked with the `locked_by` marker.

Tested against a **real** `synaps daemon --foreground` process (`tests/daemon_reload.rs`): same pid before
and after, `generation` 1→2, flock held throughout, conversation identical after reconnect, client is
owner again, second turn works; older `--exe` refused with the daemon still serving; a turn in flight is
checkpointed and its partial history + restart marker come back from the journal; `/model` + `/context` + `/system` +
keep-warm + a Parked session survive (`reload_preserves_model_keep_warm_settings_and_parked`).
F23 lock-held reload tested in `tests/daemon_reload_lock_held.rs`.

The session's prompt-cache read/write totals survive too, so the footer's `%↺` and `/stats` keep the
whole session's hit rate instead of restarting at the first request after the reload (usually the one
miss). They live in the session's `.json` snapshot only (`Session::total_cache_read_tokens` /
`total_cache_creation_tokens`), never in the journal: 0.10.1 rejects unknown journal fields. In
journal mode they are as of the last snapshot. A session saved before they persisted resumes them at 0.

### Tool activations across a reload or a park

Under progressive disclosure with deferred loading, an activated tool whose `tool_reference` has left
the history (context rollover, compaction) is part of the **non-deferred** tools array, which is the
start of the cached prompt prefix. A reload or an unpark builds a new runtime, so a session that lost
its activations sent a shorter tools array and rewrote its whole prompt cache on the next request
(request trace, 2026-10-24 05:37: 19 → 14 tools, cache read 85,679 → 5,784, write 70,268), and the
model lost tools it was using mid-session.

So the activations go with the conversation. `Checkpoint{Reload}` records them in
`SessionReloadRecord::tool_activations` as **pins only** — tool id, schema digest, trust provenance —
never as grants. Park stashes them on the actor; a reload of a Parked session records the stash. The
new runtime gets them (`RestoreToolActivations` after a reload; directly on unpark) and restores
them at its **first turn start**, against that moment's catalog
(`SessionToolSet::restore_carried`): each one must still be cataloged with the same schema digest,
the same trust provenance and a source that agrees with it — the pins `session_tools_schema` and the
execution gate check. A drifted, removed or unparsable entry is dropped fail-closed and logged
(`carried activation not restored`), never re-granted. A keep-warm ping projects them the same way.

**Policy** (`tools.activation_confirm` of the restoring runtime): `deny` drops every carried
activation — that runtime refuses model-initiated activation and lists no activatable tools in the
system prompt. `auto` and `prompt` restore without a new prompt: the grant was already approved in
this conversation, and restoring continues it; it is not a new grant.

Not carried: a checkpoint that did not confirm in budget, a session recreated fresh (its journal was
not continuable — a new conversation inherits no grants), a lock-held placeholder, and `--continue`
from the journal (activations are not journaled).

### Workers across a reload

Reactive workers (`subagent_start`/`subagent_resume`) are threads of the daemon process: the exec
destroys them, and nothing that only lives in memory survives (their in-flight tool's process-group
kill is a `Drop`, which an exec never runs). So `Checkpoint{Reload}`:

- **stops** every running worker (`SubagentRegistry::stop_for_reload`, flagged *stopped by reload*, not
  a user cancel) before the turn is cancelled, and waits ≤ 10 s for each finalizer to settle: its
  in-flight tool is dropped (killing that command's process group before the exec instead of leaving
  it running in the worktree; a job the command detached with `setsid` is its own and lives on) and
  its resume archive is written (`subagent-history/<pid>-<uuid>/sa_N.json`, `status: cancelled`).
  The stop is decided under the lock the worker takes to record its completion, so a worker that
  finished first keeps its completion; one still finishing is awaited too, so its wake reaches the
  queue the reload carries;
- records every registry entry in `SessionReloadRecord.workers` (`CarriedWorker`): stopped workers,
  finished workers whose completion wake never reached the parent (still queued, which the exec
  discards, or not yet pushed when the wait ended), and reaped ids with an archive (≤ 1024, newest
  first).

The new image adopts them (`AdoptWorkers`): each old id resolves to its archive again
(`subagent_status` answers from the archive with `stopped_by_reload`; `subagent_collect` and
`subagent_resume` work by the old id), and the parent is woken once per stopped worker ("stopped by a
daemon reload, not by the user; `subagent_resume` continues it") or lost wake (the normal completion).
The model decides whether to resume. **Exactly once**: `CarriedWorker.wake` is set only by the reload
that stopped the worker or lost the wake; the *stopped by reload* label rides along for status, but a
later reload repeats the wake only if the parent has not received it yet (still queued), never after —
a repeated "resume it" would start a duplicate worker. Handle ids continue from the old counter
(`next_subagent_id`, raised again by `SYNAPS_DAEMON_SUBAGENT_ID_FLOOR`, read at the `execv` itself),
so `sa_1` never names two workers. A failed exec re-adopts the same records in the old image (its
parents are still waiting).
A worker that does not stop within 10 s (a tool that ignores cancellation) is still announced, but
only its previous archive (if any) survives.

Tested against a real daemon (`tests/daemon_reload_workers.rs`): a worker mid-`bash` is stopped, its
shell and the background `sleep` it waited on are gone after the reload while its `setsid` job lives,
its archive is `cancelled` with history, the parent is woken once, `subagent_status` of the old id
reports `stopped_by_reload`, the next worker is `sa_2`, and a second reload neither repeats the wake
nor loses or doubles the second worker's completion.

## Restore after a crash (cold start)

A daemon that dies without a reload hand-off (a crash, an abort, `kill -9`) or is stopped used to come
back empty: the next start swept the dead daemon's session cards as stale, and clients re-created their
sessions one at a time. Now the daemon keeps a **crash record**, `daemon[-P].live.json` in the run dir
(`daemon/crash_record.rs`), and a fresh start restores from it right after it starts listening.

- **What it holds**: the writer's process identity (`pid`, start time from `/proc/<pid>/stat`, boot
  id), and per session the record a reload checkpoint carries (journal, config as created, keep-warm,
  lifecycle, `settings_replay`, `/keepwarm`, current model) plus `turn_in_flight` and the handle ids of
  the workers that were running. No secrets: the same fields as the reload state. Sessions that don't
  persist, ending ones and lock-held placeholders are left out.
- **When it is written**: each actor publishes its entry into its handle at create, at every turn start
  and end, on park/unpark, after a settings change and when its set of running workers changes; the
  daemon's writer rewrites the whole record (temp file, `fsync`, rename, 0600): **at once after a turn
  start**, otherwise at most once a second. A missing file is written back within 30 s.
- **A clean stop** (`synaps daemon stop`, SIGTERM/SIGINT, the idle exit) keeps the record: before its
  sessions end, the daemon checkpoints every live session as a reload does (a running turn is cut at
  once, its partial reply and the `Restart` marker kept; its workers stopped resumably and carried) and
  writes the record one last time from those checkpoints, marked as a stop, each session with the
  checkpoint's `turn_cut`. The next start restores every session, Live or Parked as recorded, and a
  turn THIS stop cut gets ONE message and a new turn: "resume: the daemon was stopped and started again
  mid-turn (HH:MM UTC); your last turn was cut, so check your last tool call before trusting it, then
  continue." The stop's own record decides (as after a reload); a marker in the history alone never
  does. `synaps daemon stop --force` (no checkpoints) records the sessions from their last published
  entries without `turn_cut`: they come back, nothing is resumed. A session whose checkpoint does not
  confirm in time is recorded the same way. The carried workers wake their parent at the restore as
  after a reload. The checkpoints make a stop longer: at most 37 s from the request to the exit (see
  Lifecycle), so a supervisor's stop timeout must exceed that.
- **A quiet start after a deliberate halt** (no sessions restored, no resume): before the start, put
  this line in the config file (`~/.synaps-cli/config`, or the profile's own config):

  ```
  daemon.crash_restore = off
  ```

  then start the daemon. That start removes the record and restores nothing. Delete the line (or set
  `daemon.crash_restore = on`) afterwards; the next fresh start after that writes a new record.
- **A reload** keeps precedence: the new image restores from the reload state, never from the record.
  Just before the exec, the old image writes the record from the reload state (marked with the reload's
  generation) and stops writing, so a new image that dies before its first write still leaves a record:
  the next start restores those sessions and resumes the reload's cut turns as the reload would have
  (`daemon.reload_resume` applies).
- **At a fresh start** (no reload state, `daemon.crash_restore` on), before it listens, the daemon reads
  the record and touches no session yet. If the record's writer still runs (anything that can't be
  read counts as running), the record is moved aside as `daemon.live.json.skipped-<UTC time>`, with a
  WARN, and nothing is restored; a second daemon on the same paths is refused by the flock, as before.
  If two earlier starts died while restoring it, it is moved aside as
  `daemon.live.json.quarantined-<UTC time>`, with a WARN, and nothing is restored. Otherwise the start
  counts this attempt in the record (if that write fails: a WARN, and it restores anyway) and puts the
  recorded sessions in its **restore set**, leaving out:
  - a session with no history on disk (no `<id>.json` and no `<id>.journal`): a WARN `crash restore:
    session=<id> has no history on disk; skipped`. It is never created empty under its old id;
  - a Parked session past its eviction age (`daemon.parked_evict_secs`, counted from when it parked,
    which the record carries): it stays on disk, `--continue` brings it back.

  Then the daemon sweeps stale cards (the dead daemon's included), signals ready and listens, and only
  THEN restores: every session in the set is rehydrated from its journal through the reload's
  per-session path, **up to 16 at a time** (half the host's cores, at least 2), back Live (a Parked one back Parked, its eviction timer going on
  from its recorded park time) and registered under the new daemon's pid. (A stop's writer released the
  daemon lock as its last step, so it serves nothing even if its process has not exited yet; the
  restore waits up to 3 s for that exit.) A restore (a crash or stop restore, or a reload's
  rehydrate) loads each session by the journal id the daemon recorded, directly: no chain or name
  look-up, so a session merely named like that id never takes its place. A journal no longer on disk
  is looked up as a `--continue <id>` would be. The sessions' builds run in parallel, except their
  endpoint starts (the session registry scan and the socket bind), which take turns in the daemon. An autospawn's 15 s ready wait and a client's 2 s handshake
  never wait on the restore.
- **While it restores**, a session is in exactly one of three states:
  - **pending**: listed from the record (`daemon sessions`, the `SessionList` frame) with its recorded
    lifecycle (`live`, or `parked`) and `restoring: true` (the only new field; absent on every other
    entry). An `Attach::Existing`, or an `Attach::Create` whose `--continue` names it (by its id, its
    journal id, or a name only one recorded session carries), waits for it up to 30 s, then gets
    `Error { session_id: Some(id), message: "session <id> is still restoring" }`. The restore goes on.
  - **restored**: listed from its actor, no `restoring` key; an attach finds it Live (no create). A
    listing reads the restore set before the live sessions and shows each session once: from its actor
    if it has one (matched by id or by journal id), else from the record, so a session that comes back
    while a listing is made is never missing from it.
  - **given up**: not listed. Each rehydrate is capped at 20 s, counted from the moment its create holds
    its session's create lock (the host reserves each session for one create at a time; creates of
    different sessions build at once). Time queued behind another create of the same session, a
    client's waiting on its hooks, does not count (one edge does: a build that resolves a session its
    reservation does not hold, because the disk changed after the reservation, and waits for that
    session's create, waits inside the cap; a restore resolves its exact journal id both times, so it
    does not meet it in practice); a client create waits for extension discovery before
    it takes that lock. The wait for that lock is capped too, at 600 s, so a create that hangs holding
    the lock can't keep the restore, and with it the idle exit,
    reloads and the gates, waiting for ever: the reason is `its create waited 600 s for the create lock;
    it stays on disk`. A session past either cap, one whose journal
    is locked by another process (no placeholder here) or one that fails to load is given up with a
    WARN `crash restore: session=<id> given up: <reason>`, in the same step it leaves the list. If the
    restore task itself panics in a build that unwinds (`panic = "unwind"`: dev and test builds, and a
    release build made with the release profile's `panic` set to `unwind`, as the live `-uw` builds
    are), every session it had not
    finished is given up the same way, with the reason `the restore failed; it stays on disk`, and the
    restore ends (reloads, the idle exit and the gates go on as after the summary line). The upstream
    release profile sets `panic = "abort"` (Cargo.toml `[profile.release]`), so a build made with it
    aborts on a panic and that guard never runs: the daemon dies, and the next start counts it as a
    start that died restoring
    (two of those quarantine the record). A
    waiting or later `Attach::Existing` gets `Error { session_id: Some(id), message: "session <id>
    could not be restored: <reason>" }`. `Attach::Existing` only attaches to a live session and never
    loads one, so a client that reconnects by id keeps getting this error until something loads the
    session. It stays on disk: `Attach::Create` with `--continue` loads it
    (by id, journal id, name or chain, or as the latest), and the daemon then applies the one resume
    decision the restore owed it (same record,
    same rules: once per crash or stop, nothing if a user message already follows the marker, none
    for a recorded-parked session, the crash loop cap), re-registers its carried workers, and logs
    INFO `crash restore: session=<id> loaded on attach, resume queued|not needed|unknown` (`unknown`:
    no answer from the session within 10 s). The entry is matched by the session that loaded, never
    by the client's words. A session the cap gave up after its create had already finished is live:
    the restore applies the same decision to it at once, with the same INFO line (or the first
    `Attach::Create` or `Attach::Existing` that reached it before that did), so a stop or the crash
    record, which take a live session from its actor, never drop the resume it is owed. A second
    load does nothing more. Until it is loaded its
    record entry is kept unchanged, so a later start tries it again; once it is live, the crash record
    and a stop's record take it from its actor.

  Synaps's own client (`SocketTransport::attach`) waits up to 55 s for an attach naming a session
  listed `restoring` (the 30 s gate, then a parked session's unpark), so it gets the gate's own error
  rather than its usual 5 s "attach timed out".

  `synaps daemon reload` is refused until the restore is done ("a crash restore is still running;
  reload once it is done"); the idle exit waits for it too. A stop during the restore keeps the
  sessions it had not finished, and the given-up ones, in the stop's record, unchanged (with the
  record they came from, so their one resume still comes at the next start); a given-up session that
  is live by then is recorded from its actor.

  Limits:
  - a stop gives the restore 2 s to finish what it started, then aborts it; a session it aborts
    between its load and its resume is restored again (and resumed once) from its carried entry at the
    next start;
  - a reload after a session was given up and before a client loaded it drops that session's pending
    resume (the reload's hand-off has no entry for it); the session stays on disk and loads with
    `--continue`, without the resume;
  - a reload's hand-off record names no session name, creation time or park time, so a crash restore
    from a hand-off (the reloaded image died before its first record write) lists those sessions with
    no name and the record's time, and gives a parked one a fresh eviction window (it is never skipped
    as past eviction);
  - a crash in the short window between a turn's start and the record write that marks it in flight
    (the draft is written at the turn's start; that record write follows at once, ahead of the
    debounce) leaves a record that says the session was idle: the next start folds the draft in and
    appends the crash marker, but sends no resume (`resumed=no`), so the turn waits for the next user
    message. The other way round is harmless: a turn's end reaches the record up to 1 s later, and a
    record that still says in flight for a turn that ended resumes nothing, because its draft is gone
    or its history already ends in the reply (a leftover draft is then just removed).
- **Crash resume**: a restored, non-parked session whose record says `turn_in_flight` gets ONE user
  message and a new turn: "resume: the daemon restarted after a crash mid-turn (HH:MM UTC); your last
  turn was cut, so check your last tool call before trusting it, then continue." HH:MM is the dead
  daemon's last record write. It is sent only when all of these hold: this load folded the cut turn's
  draft in and that draft was written by the dead daemon (the draft names its writer), the history's
  latest interruption marker is the crash marker that load wrote with nothing answering it, and no user
  message follows that marker (a resume or nudge another sender already delivered). At most once per
  crash (the dead writer's identity); a restore from the same record finds the draft consumed and sends
  nothing. Idle, parked and excluded sessions get nothing. The daemon is the only sender of this line.
  The stop's resume follows the same rule against a user message after its marker, not counting the
  cut turn's own tail (`cut_tail`, as for a reload). A stop record without `cut_tail` (an older
  image wrote it) has no such check, as for a reload: a turn re-prompted after the stop is resumed once
  more if that re-prompt has no reply yet, and not if it has one (the history no longer ends at the
  marker).
- **Crash loop cap**: when a session's cut turn was resumed by a crash or stop restore and the next
  death cuts it again within 30 min, the restore brings it back WITHOUT a resume and logs a WARN
  `crash restore: crash loop suspected: session=<id> not resumed`. The record keeps that mark (the
  resume's time) until a turn of the session ends with its reply; a cancelled or failed turn keeps
  it. So a death that cuts a later, unrelated turn is resumed as usual.
- **Workers** running at the crash died with it (their archives are written at finalize, so none
  exists). A cut turn's resume line names them; a session that was idle while its workers ran gets one
  note naming them in place of their completion wakes ("resume: the daemon restarted after a crash
  (HH:MM UTC) while your subagents (sa_3) were running; ..."). The note is a user message that starts a
  turn, so the parent wakes; it is not sent again when the history already holds it. New handles start
  past every recorded id. A worker a stop carried is adopted once; adopting it again wakes no one.
- **Log** (target `agent_engine::reload`):
  - at the start: `crash restore: restoring N sessions after the <crash|stop|handoff>, once the daemon
    listens`;
  - one summary line as soon as the last session is back or given up (it does not wait for the
    resumes): `crash restore: restored N sessions, resume queued M, given up G, skipped K, took T ms`,
    with fields `how` (`crash`, `stop` or `handoff`), `writer`, `restored`, `parked`, `resume_queued`
    (sessions asked to resume a cut turn or note lost workers), `given_up`, `skipped`, `took_ms` and
    `interrupted` (a stop came first). `resume queued` counts the sessions the restore ASKED to
    resume; each actor decides after that and may still send nothing (the turn was re-prompted, the
    crash loop cap, no draft from the dead writer), so it is not the number resumed: the per-session
    lines below are. Until this line a start counts as one that died restoring;
  - one line per restored session once its decision is known (bounded at 10 s, then `unknown`):
    `crash restore: session=<id> restored, resumed=yes|no|unknown`, fields `session`, `resumed`,
    `how`. Another sender skips a session with `resumed=yes`. `unknown` means no answer within 10 s,
    not `no`. A restore a stop interrupted (`interrupted=true`) writes NO per-session lines: each
    session it restored before the stop was still asked, and its actor still decides; only the lines
    are missing, so a sweep must not read a missing line as `no` (the actor's `crash resume: queued
    ...` or `reload resume: queued ...` line below shows a resume that was sent). In `loaded on
    attach, resume queued`, `queued` is the actor's answer (it sent the resume), not the summary's
    "asked";
  - per session also `daemon: session rehydrated after crash restore` (or `stop restore`) and
    `crash resume: queued ...` / `reload resume: queued ...` for each one resumed. A stop logs `crash
    record: written for the stop` with the session count, `carried` and `turns_cut`.
- **Off switch**: `daemon.crash_restore = off` in the config file (default on). Under off nothing
  writes a record (a reload writes no hand-off) and any start, fresh or after a reload, removes a
  leftover one.

### Capacity resume (`capacity_resume`)

A turn that ends because no Anthropic account has capacity, with a limited account due back at a
known time within 8 days (a reset-based stop of a local `auto` selection), arms ONE automatic resume
for that time plus 0-60 s of jitter. The stop is kept as `capacity_stop` on the session's entry
(`resume_at`, `stopped_at`, `kind`, `history_len`, a 64-hex digest of the last message, `attempt`
0..=2; no account, email, id or seat hash), so it rides the reload state, the stop record and the
crash record; it needs the crash record (no Recorder: nothing is armed, one INFO
`phase=suppressed why=no_crash_record`).
- **Fire.** At its time the actor unparks the session if needed, checks that the history still ends
  where it stopped and that nothing is busy (a turn, a compaction, a driver, queued commands or
  events; a pending `on_session_start` is not busy: the resumed turn holds its request until the
  hook ends, as a queue wake does), then CONSUMES the stop: it writes the entry without it and waits up to 5 s for the writer's ACK of
  that write. Only then does it start the `resume:` turn. No ACK: nothing is dispatched, one WARN
  `class=consume_unacked`, and it tries again 10 min later. A stop/reload seal, a fence, Esc or a
  message that arrives during the wait wins; work queued behind the stopped turn is sent in place of
  the resume. AT MOST ONCE: a crash after the ACK and before the turn starts loses that resume.
- **Parked and evicted.** An armed parked session is not evicted (`phase=eviction_held`), and the
  restore keeps a parked entry past its eviction age only for a stop that validates.
- **Restore.** `rehydrate_one` sends the host-only `ArmCapacityResume` after the resume decision
  and `AdoptWorkers`, before the Park. The actor takes it once, only before any turn, Esc, `/new`,
  End or checkpoint, and only if it validates (a closed set of kinds, the digest, attempt, times and
  the horizon; an invalid one is dropped with a fixed class). It is not sent to a session a client
  loaded first (`why=client_loaded`) or to one recreated fresh; a session the restore gives up, a
  lock-held placeholder included, drops it (`why=given_up`), so `loaded on attach` never arms one.
- **Cancel.** Any turn start, an idle Esc, `/new` and End clear it; for Esc, `/new` and End the
  writer's ACK of the clear is awaited (up to 5 s) before the Esc notice. Two accepted limits: a
  clear whose write fails (`class=clear_unacked`, retried at the next write) followed by a crash
  before that write lets the next start resume once; and an Esc during a reload's fence clears it
  here while the hand-off already carries it, so the new image may resume once.
- **Chain.** A resumed turn that stops again re-arms only for a strictly later reset, and at most
  three resumes are dispatched in a row (`why=loop_cap` / `not_later`).
- Logs: target `agent_engine::session::capacity_resume`, `event=capacity_resume` with `phase=`
  armed / not_armed / cleared / deferred / eviction_held / refingerprinted / consumed / superseded /
  dispatched / rearmed / dropped and a fixed `why=` or `class=`; never the loaded text or digest.
- **A damaged record.** A recorded `capacity_stop` that does not decode (a wrong type, a missing
  field, a number out of range such as `1e400`, nesting past the JSON reader's 128 levels) drops
  only itself, with one WARN `class=malformed`; the rest of its entry and the other sessions of the
  crash record or reload state restore as usual. The resume's raw text is set aside before any
  typed decoding (the crash record's entry reads it outside its flattened session), so this holds
  for a resume that is valid JSON. Limits: invalid JSON syntax inside the resume still makes the
  whole record unusable (and running out of memory while reading it is not isolated: an allocation
  failure aborts the process), and so does the same damage in any OTHER field, as before this
  feature (one line naming a fixed class and where, never the stored text).
- **Off switch**: `capacity_resume = false` (top-level config key, default on; independent of
  `daemon.reload_resume`). Read live: an armed stop is dropped at its time.
- **Tested against real daemons** (`tests/daemon_capacity_resume.rs`: `kill -9`, a stop and start,
  a real reload, parked past its eviction age, a given-up restore). That binary starts daemons only
  when it is given `--real-daemons`; run it on a host set aside for daemon tests, one at a time:
  `cargo test -p synaps-engine --test daemon_capacity_resume -- --real-daemons`. An ordinary
  `cargo test` and CI skip it (one skip line, no daemon started); `--list` prints its cases and an
  unknown flag is an error, as is a flag that takes a value given none or one that looks like a
  flag (`--format --bogus-flag`); every flag is checked before anything starts. Its daemon mode
  refuses to start without the marker its opted-in parent passes. Each daemon runs in a process
  group of its own, killed whole and reaped when its case ends or is stopped; a case passes only
  once those groups are empty and its egress trap's backlog is drained with no connection in it.

## cwd caveats (risk §6.1)

Tools, shell and the memory tool honour the session `cwd` (`Runtime.cwd`). Still process-wide in the
daemon today: memory project scope, `host_project_root`, project-local plugin discovery, extension
process cwd. Start the daemon in the project you care about until day 2's `memory_project_scope(cwd)`.

## Session identity — environment snapshot (wave 1, T1–T3)

**Principle:** a daemon session's process identity (env, cwd) comes from the
*creating client*, not the daemon. The daemon's own env is used by exactly one
thing: the daemon itself.

### How it works

1. **Client captures env at Hello.** `Hello::new()` calls `capture_client_env()`
   which snapshots `std::env::vars()`, sorts by key, and strips:
   - Client-only prefixes: `SYNAPS_CLIENT_*`, `SYNAPS_TUI_*`, `SYNAPS_DAEMON_*`, `SYNAPS_MEM_TRACE*`
   - Secret **names** (case-insensitive, `config::is_secret_key`): `*_KEY` (so `*_API_KEY`),
     `*_TOKEN`, `*_CREDENTIALS`, `*_PAT`, `*_DSN`, `*SECRET*`, `*PASSWORD*`, `*PASSWD*`.
     Suffix-anchored where it says so: `SSH_KEY_PATH` and `API_TOKEN_TTL_SECS` are kept.
   - Secret **values**, whatever the key is called (`config::is_secret_env_value`): any URL
     with userinfo, anywhere in the value — `scheme://user:pass@host` or `scheme://key@host`
     (`DATABASE_URL=postgres://u:p@h`, an authenticated `HTTPS_PROXY`, a Sentry DSN, a JDBC
     option string, a list of URLs). A username-only URL (`ssh://git@host`) counts too.
   - Stripped secrets are recorded **by name** in `env_stripped`; their values go nowhere.
     Consequence: the session's tools do not see them. A command that fails while
     referencing one gets the notice below; an authenticated `HTTPS_PROXY` is used
     implicitly by curl/git, so it fails without one. For a workflow that needs a secret
     in the agent's shell, run in-process (`SYNAPS_DAEMON=0`).
2. **Daemon copies `hello.env` → `config.env` on `Attach::Create` only.**
   Attaching to an existing session never changes its env (creator's env is final).
3. **`SessionConfig.env` → `Runtime.env` → `ToolCapabilities.env`** — same pipe as cwd.
4. **Tools apply `env_clear().envs(session_env)`** when `env` is `Some`:
   `bash.rs`, `find.rs`, `grep.rs`, `ls.rs`. No daemon env var leaks through.
5. **In-process hosts pass `env: None`** — inherit the process env, byte-identical to before.

### `SYNAPS_*` split (stub — T11 will formalize)

### Multimodal attachments

Attachment file paths are resolved on the **client** side, following the same
principle as `--system` path resolution (T4): the daemon's filesystem is not
the client's. The client loads bytes via `load_attachment` (with `O_NOFOLLOW`
and regular-file checks), builds canonical content blocks via
`PendingAttachments::build_content`, and ships them as `Vec<serde_json::Value>`
inside `SessionCommand::Submit`. The actor validates structurally and per-model
before appending. Rejection emits `SystemNotice`; the message is not pushed.
See `docs/multimodal.md` for full details.

| Category | Prefixes | Read by | In session env? |
|---|---|---|---|
| Client-only | `SYNAPS_CLIENT_*`, `SYNAPS_TUI_*` | Thin TUI client | No (stripped) |
| Daemon-only | `SYNAPS_DAEMON_*` | Daemon boot | No (stripped) |
| Debug | `SYNAPS_MEM_TRACE*` | Profiling harness | No (stripped) |
| Session | everything else (`SYNAPS_ANTHROPIC_BASE_URL`, etc.) | Runtime/tools | Yes |

### What is NOT yet covered (wave 2+)

- **Extension protocol: per-call env/cwd** (T6) — sidecars still get the daemon's env.
- **Provider switches read by the transport** — e.g. `SYNAPS_CODEX_WEB_SEARCH`
  is read from the daemon's env, not the session env. See
  [providers/codex-astra.md](providers/codex-astra.md#hosted-web-search-synaps_codex_web_search).
- **`SO_PEERCRED` uid check** (T11) — no auth boundary on the daemon socket yet. (The
  per-session event endpoints do refuse other users; see [Who sent an event](#who-sent-an-event).)

### Path-like arg resolution — `--system` by content (T4, F26)

Thin clients resolve path-like `--system` and `--prompt-manifest` arguments
against the **client's** cwd before sending `Hello`, so
`cd /tmp/proj && synaps --system ./prompt.md` works identically over adopt
and in-process.

**Heuristic** (`session::client_args::looks_path_like`): a value is path-like
when it starts with `/`, `./`, `../`, or `~`, **or** ends with `.md`/`.txt`
(case-insensitive) — AND contains no newlines or runs of double-spaces (prose
guard).  Path-like + readable → replaced with the file's **contents**.
Path-like + missing → non-zero exit with `--system <val>: no such file
(resolved <abs>)`.  Non-path-like → passed through as literal prompt text.

`--prompt-manifest` is canonicalized against client cwd; missing → error.

> **Design note:** a cleaner signal would be `--system @file` (like curl),
> but the launcher `~/Jawz/tools/jawz` already passes `--system /tmp/jawz-identity.md`
> unchanged, so the heuristic must accept bare paths.

### Journal env persistence and secret notices (T5)

`SessionConfig.env` and `SessionConfig.env_stripped` are persisted in the
session journal (as part of the `Session` struct, BEFORE `api_messages` so
`read_session_header` sees them). Both artifacts carry them — the `.json`
snapshot a fresh daemon reads and the journal meta tail — and a fresh client
env on `--continue` is written back so the next restart sees the latest.

**Rehydration precedence** (`--continue`/unpark after daemon restart):
1. An explicit `Hello.env` from the continuing client wins (the user is
   sitting in a shell *now*).
2. If the client sent `env: None` (in-process/legacy), the journal's
   persisted env fills in.
3. The daemon's own process env is **never** used for a daemon-hosted session.

**Belt-and-braces:** the journal-side write runs every env pair through
`is_secret_env` — the name denylist **or** a credential-carrying value — so a
secret is dropped even if a future client forgets to strip. The `env_stripped`
list records **names only**.

**Loud notices:** when a bash command exits non-zero AND the script text
references a name from `env_stripped` as a whole identifier (`$NAME`,
`${NAME:?}`, `$env:NAME`, `NAME=`… — `GH_TOKEN_FILE` does not match
`GH_TOKEN`), a system notice is appended to the tool output, once per name
per session:
```
note: $GH_TOKEN was stripped from the session env as a secret; reattach from a shell that has it (synaps --system … from that shell) — the daemon never receives credentials.
```

## What changes on the default (in-process) path — read before merging

Honest list of behaviour changes on this branch on the plain in-process path (`synaps`, `synaps chat` — no `--attach`):

1. **`synaps chat` runs on `SessionActor`** (`LocalTransport`, in-process). The differential test
   (`tests/session_actor_differential.rs`) compares the actor against a *frozen re-derivation* of the
   inline engine halves (not a verbatim copy; it has no abort/prompt/save) on **three scenarios only**:
   plain turn, provider error repairing history, idle auto-turn to cap. **Tool loop, steer-mid-stream,
   queue-while-busy, cancel (history + marker), secret-prompt round-trip are asserted by actor unit
   tests, not by the differential.** `tests/chat_stdin.rs` is unchanged and green but never runs a turn through a
   stub. The kill-switch `SYNAPS_CHAT_INLINE=1` only exists in a `--features legacy_inline` build.
   Cancel diverges from the frozen oracle by design: the oracle folds an `ABORT CONTEXT` recap into the
   next user message; the actor keeps the interrupted turn as history plus an interruption marker
   (`cancel_keeps_real_history_where_the_oracle_folded_a_recap` pins both sides).
2. **MCP descriptor cache write-back is ON by default, in-process too** (`docs/mcp.md`): after the first
   `tools/list` on an exact lease the listing is written to `~/.synaps-cli/mcp-descriptors.json`, so the
   *next* boot registers dormant MCP tools it did not know before → tool list, system prompt and the
   provider prompt-cache prefix differ between run 1 and run 2 for every MCP user. Intended (the cache
   finally has a writer), but it is a default-path change. `SYNAPS_MCP_CACHE_WRITEBACK=0` restores the
   old read-only behaviour.
3. Hook events carry `session_id` (additive; `SYNAPS_HOOK_SESSION_ID=0`).
4. **`synaps chat` renders typed events**: `/compact` prints `compacting...`, the disclosure line and
   `[compacted → ~N tokens]` (one each — the actor emits `CompactionStarted/Applied/Failed/Cancelled`,
   never a "compacting..." notice); `/abort`-equivalents render `Aborted{context_saved}` (now: partial
   work kept), `/clear` renders `Cleared{session_id}`. Quit mid-turn keeps the partial turn in history
   (§Lifecycle above).

## Memory acceptance — `DAEMON=1 SYNAPS_DAEMON=1 scripts/memprof/bench-sessions.sh BIN 1 2 3`

bella, 2026-09-04, release build @ B6, **real `SessionActor` sessions** (A merged), 2 sidecars
(synaps-chronos, munder-hive-god), settle 8 s, median of 3:

| N | PSS total (daemon tree ∪ clients) | daemon tree PSS | daemon process PSS | marginal PSS | procs beyond daemon / session | daemon procs | attach ready |
|---|---|---|---|---|---|---|---|
| 1 | 108.1 MB | 85.0 MB | 31.5 MB | – | 1.00 | 3 | 47 ms |
| 2 | 128.1 MB | 82.2 MB | ~28 MB | 19.9 MB | 1.00 | 3 | 46 ms |
| 3 | 145.6 MB | 81.8 MB | 27.5 MB | 17.6 MB | 1.00 | 3 | 46 ms |

Reading:
- **procs/session == 1** (the attach client) and **daemon procs constant (3)** across N: sidecars spawn
  once per daemon, not per session. Gates pass.
- **Daemon-side marginal cost per idle session ≈ 0 MB** (daemon tree PSS is flat/slightly down 85 → 82 MB
  as text pages get shared with more clients).
- **Marginal PSS as measured (17.6–19.9 MB) is above the 15 MB gate**, and it is entirely the
  `synaps attach` client process (PSS ≈ 21.5 MB, RssAnon ≈ 19.3 MB each, 10 threads): it is the full
  22 MB `synaps` binary with jemalloc + a tokio runtime. The plan lists the attach client RSS as
  informational ("≪ 40 MB"); the ≤ 15 MB gate was written for actor/session allocations in the daemon,
  which are within noise here. Cutting the client below 15 MB is a client-diet task (single-thread runtime,
  no jemalloc arenas, lazy statics), not a daemon one.
- Compare in-process (Phase 1 gates): PSS(N=3) ≤ 160 MB with 3 procs/session → daemon mode at 145.6 MB
  with 1 proc/session, and every additional session costs a 20 MB thin client instead of a full engine.

Raw runs: `/tmp/memprof-synaps-b-daemon-N<N>-r<i>.txt` on bella.

### Phase 4 — the thin client after the diet (bella, THP=`[always]`, release @ c8a7e030, median of 3)

The 19.3 MB / 10-thread client above is the **before**. `DAEMON=1 CLIENT=1 BOUNDED=1 SETTLE=12 REPEAT=3
FIXTURE_MSGS_MB=0 scripts/memprof/bench-sessions.sh BIN 1 2 3` plus `scripts/memprof/client-ladder.sh`:

| Gate | before (741b6b60) | after | |
|---|---|---|---|
| G1 client RssAnon idle, empty session, post-purge | 19.35 MB (18.4 MB huge pages) | **2.27 MB** (N=1/2/3: 2.27 / 2.26 / 2.27) | ≤ 10 ✓ |
| G2 client RssAnon idle, 20 MB history | 78.5 MB | **2.38 MB**, `bounded_delta` 0.11 MB | ≤ 12 ✓ |
| G3 client threads at idle | 8 | **3** | ≤ 4 ✓ |
| G4 `first_frame` | attach_ms 47 | **7–10 ms** (attach_ms 46–48; 79 ms / 114 on the 20 MB fixture) | ≤ 100 ✓ |
| G5 all-in marginal N=2→3 (daemon anon + client) | 17.6 MB | **3.18 MB** (0.91 + 2.27) | ≤ 10 ✓ |
| G7 retention pre→post purge | — | 0.00 MB | ≤ 0.5 ✓ |
| G11 daemon side | — | unchanged within noise; daemon procs 3/3 | ✓ |

Where it went: the re-exec with `PR_SET_THP_DISABLE` (−17 MB of huge pages on a 0.86 MB heap),
no history mirror (−76 MB on the 20 MB fixture), jemalloc background threads + `signal-listener`
off (−5 threads), lazy reqwest (`http` stage gone).

**Honest coding-session number:** the 2.3 MB is with no code rendered. One rendered code block
compiles that grammar's regexes → **≈ 13.7 MB** idle until the syntect idle eviction (default 120 s),
**≈ 4.9 MB** after; each further language ≈ +8–10 MB until then. The curated grammar dump saves
dump bytes, not heap. On `[madvise]`/`[never]` kernels the re-exec is skipped (nothing is
huge-mapped before `main`); the remaining knobs are applied in-process.

The socket differential (`SYNAPS_TUI_E2E_SOCKET=1 scripts/tui-e2e/differential.sh`) compares
**content order**, not layout: its normaliser drops blank rows, `█` cursor rows and the
`│Extensions` line before diffing L against S.

### Fix round: daemon-tree RssAnon column (§9 of the phase-2 review)

`bench-sessions.sh` DAEMON=1 now prints `daemon_anon` (Σ RssAnon over the daemon tree) and
`anon_marginal` (= daemon-side RssAnon cost of one idle session — the number the ≤ 15 MB gate was
written for; PSS "≈ 0 marginal" above was a sharing artefact). bella, 2026-09-04, release @ fix round,
REPEAT=1, real actors, same 2 sidecars. `before` = multi-thread attach client, `after` = current-thread
attach client (commit "attach client diet"):

| binary | settle | N | PSS total | daemon tree PSS | daemon_anon (tree) | daemon proc RssAnon | **anon_marginal / session** | attach client RssAnon | client threads |
|---|---|---|---|---|---|---|---|---|---|
| before | 8 s | 1 | 111.1 MB | 85.8 MB | 36.4 MB | 23.6 MB | – | 21.4 MB | 10 |
| before | 8 s | 3 | 128.9 MB | 82.7 MB | 34.9 MB | 22.0 MB | **−0.7 MB** | 19.1 / 19.1 / 1.7 MB | 10 |
| before | 15 s | 1 | 82.3 MB | 61.6 MB | 18.5 MB | 7.4 MB | – | 1.7 MB | 10 |
| before | 15 s | 3 | 70.3 MB | 61.9 MB | 20.6 MB | 7.4 MB | **+1.0 MB** | 1.7 / 1.7 / 1.8 MB | 10 |
| after | 8 s | 1 | 98.8 MB | 83.9 MB | 34.0 MB | ~21 MB | – | ~11 MB | 4 |
| after | 8 s | 3 | 77.8 MB | 66.6 MB | 18.6 MB | 5.9 MB | **−7.7 MB** | 1.4 / 1.4 / 1.4 MB | 4 |
| after | 15 s | 1 | 91.8 MB | 78.2 MB | 33.0 MB | 20.6 MB | – | 11.1 MB | 4 |
| after | 15 s | 3 | 116.2 MB | 77.4 MB | 33.5 MB | 20.6 MB | **+0.25 MB** | 11.1 / 11.1 / 13.1 MB | 4 |

Reading, honestly:
- **Daemon-side RssAnon per idle session is ≈ 0–1 MB** (−0.7, +1.0, −7.7, +0.25 MB across the four
  pairs). A `SessionActor` + `Runtime` + `ConversationState` with no turn is small; the registries are
  `Arc`-shared. The ≤ 15 MB daemon-side gate passes with a wide margin. This is the number the gate was
  about; it was simply unmeasured before.
- **Everything else in this table is jemalloc purge timing, not code.** The daemon process swings
  7 → 23 MB and the *same* attach binary reads 1.7 MB or 19 MB depending on whether jemalloc's background
  thread (sleeps up to 10 s when idle) has purged the startup garbage before the sample. An 8 s or 15 s
  settle lands on either side of that window at random. `bench-turns.sh`-style double settle (> 20 s) or
  a `mallctl("arena.<i>.purge")` before sampling is the day-2 fix for the *script*.
- **Client diet**: the current-thread runtime for `synaps attach` cuts threads 10 → 4 and removes the
  worker pool; its RssAnon effect cannot be resolved under the purge noise above (both binaries bottom
  out at 1.4–1.8 MB once purged, i.e. the post-purge client is already tiny — the 21 MB PSS headline is
  mostly shared text of the 22 MB binary). Remaining day-2 diet items: skip `EngineHost`/config/skills
  statics on the attach path (nothing in `main.rs` boots them before dispatch today — verified), and
  `narenas:1,background_thread:false` for the client via a per-command `MALLOC_CONF` (needs the static
  conf to move to a runtime `mallctl`).
- procs/session == 1.00 and daemon procs == 3 held in all eight runs.

Raw runs: `/tmp/memprof-synaps-fb-{before,after}-daemon-N<N>-r1.txt` on bella (the 15 s runs
overwrote the 8 s ones for the same N; the 8 s values above are transcribed from the earlier console).

## Not landed today (day 2/3)

`--attach` driving the TUI over `SocketTransport` (needs A4); `Resync` after
`Lagged` via `turn_replay`; delta coalescing; `--tcp`; HTTP+SSE front-end; `prompt_over_wire` and
`detach_without_abort_over_socket` integration tests (need a secret-prompting tool fixture + `Endless`
script through the actor — the paths are exercised by the actor unit tests and the socket tests above).
