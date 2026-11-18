# Write confinement (Linux)

SynapsCLI can confine what a session **writes**. It is off by default and
opt-in per owner. With it on, a session's shell commands and its file-writing
tools can write only to the session's workspace, a small baseline and the
extra folders a policy plugin names. A session that cannot be confined is
refused; it is never started unconfined.

This is a v1: it confines writes only. **Reads and the network are not
confined, and process control only in part** (see [Other processes and the
daemon](#other-processes-and-the-daemon)), and **macOS and Windows are not
confined at all** (`sandbox.confine = enforce` there refuses every session).

## Turning it on

Set both keys by hand in the config file (`~/.synaps-cli/config`, or
`$SYNAPS_BASE_DIR/config`):

```
sandbox.confine = enforce
sandbox.policy_provider = my-policy-plugin
```

| Key | Values | Meaning |
|---|---|---|
| `sandbox.confine` | `off` (default), `enforce` | with `enforce`, every session is confined or refused. Any other value is read as `enforce`; so is a config file that can't be read |
| `sandbox.policy_provider` | a plugin name | the one plugin the core asks for the session's extra write folders. Without it a session under `enforce` is refused (`no-policy`) |

Both keys are **owner-set**: `synaps config set` (and the settings
screen) refuses them, an agent can't write them, and an extension can't change
them. Edit the file yourself. The keys are read as a session starts, so a
running session keeps the policy it started with.

## What a session may write

* its **workspace** (the working folder, canonical), with the cuts below;
* the baseline: `/tmp`, `/dev/shm`, `/dev/null`, `/dev/zero`, `/dev/tty`,
  `/dev/ptmx`, `/dev/pts`, and `/proc` with limited rights (write an existing
  file such as `/proc/self/fd/1`; nothing can be created, removed or renamed
  there). A plugin cannot grant `/tmp` or `/dev/shm` themselves, only paths
  two or more components below them;
* the folders the policy plugin's `write` list adds, after the core has
  screened them.

Every process a session starts confined gets `TMPDIR` set to the session's
own temporary folder, `/tmp/synaps-session-XXXXXX` (a fresh name, mode 0700,
removed when the session ends). `/tmp` and `/dev/shm` themselves stay
writable, and are shared with your other programs and sessions.

Every shell command, whichever way it starts (`bash`, the `shell_*` tools,
the file-listing helper, and the MCP servers and extension processes the
engine launches for the session), runs under a **Landlock** domain applied in
the child before `exec`, so everything it starts inherits it. The in-process
tools (`write`, `edit`, `multi_edit`) run in the engine itself, which is not
confined, so they check the same policy on the **opened** directory (an
`O_PATH` walk that refuses symlinks, then the kernel's own answer for where
that handle is), not on the path string.

### Never writable (the protected set)

These are denied even when the workspace or the plugin's list contains them,
and no extension can change the set:

* the engine's **whole base directory** (`~/.synaps-cli`, or
  `$SYNAPS_BASE_DIR`): config, profiles, plugins and their required-guard
  folders, `sandbox/`, the registry and the socket folder;
* credential stores: `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.config/gcloud`,
  `~/.kube`, `~/.docker`, `~/.local/share/keyrings`, `~/.password-store`,
  `~/.netrc`, `~/.git-credentials`;
* user autostart: `~/.config/systemd`, `~/.local/share/systemd`,
  `~/.config/autostart`;
* whole trees the owner's own tools load code from: `~/.config`, `~/.local`,
  `~/.cargo`, `~/.rustup`, every absolute `$PATH` entry at session start, the
  values of `$XDG_CONFIG_HOME`, `$XDG_DATA_HOME` and `$XDG_STATE_HOME`
  (each taken from the engine's environment and from the session's own, the
  one its client sent, so a `$PATH` entry only your terminal has is
  protected too),
  `~/.bashrc.d`, `~/.oh-my-zsh`, `~/.vim`, `~/.vscode*`, `~/go/bin`,
  `~/.deno`, `~/.bun`, `~/.thunderbird`, `~/.claude`;
* the shell start-up files `~/.bashrc`, `~/.profile`, `~/.bash_profile`,
  `~/.zshrc`, `~/.zprofile`;
* the home directory itself as a *whole* (a plugin cannot grant `$HOME`, or
  any folder above the base directory), taken from the passwd entry; an
  environment `HOME` that differs (the engine's or the session's) is
  protected too, and so is `$XDG_RUNTIME_DIR`;
* inside a workspace: a project plugin folder `.synaps/plugins` that already
  exists (shell commands can't change it, and the write tools never write
  under it, whether it exists or not; under `enforce` a project's own plugins
  aren't loaded at all, see [Project plugins](#project-plugins)), and a git
  checkout's `.git/hooks` and `.git/config` (see below).

### What the core drops from a plugin's list

An entry is dropped, and listed with its reason in `synaps sandbox check`,
when it:

* **equals, lies under or lies above** a protected path that no grant can
  survive. An entry under a protected path, or equal to one, is dropped. An
  entry that is `$HOME`, or a folder above the base directory, is dropped,
  because Landlock grants everything beneath a rule. The reason is a sentence
  naming the protected path (`is inside the engine base directory (...)`,
  `contains the home directory (...)`);
* is **not its own canonical form** (the path as returned differs from its
  canonical spelling) or is a symlink: reason `a symlink`;
* holds a control, bidi or zero-width character, or a NUL: reason `bad
  characters`;
* is not absolute, is `/tmp` or `/dev/shm` themselves, names procfs, or is
  beyond the 256th entry.

An entry that merely **contains** a protected tree further down (a
`$PATH` folder inside it, say) is not dropped: it is split (next section).

Before any rule is built, every kept path is **opened without following a
link** (`O_PATH | O_NOFOLLOW` per component, then the kernel's report of
where the handle lands is compared with the canonical path) and the rule is
built on that opened handle. If the path was swapped for a link between the
screening and the rule, the entry is dropped with the reason `a symlink` and
is not writable.

### Protected paths inside a grant: splitting

Landlock only allows. It cannot carve a read-only hole out of a folder it has
granted. So when a protected path sits **inside** a grant (the workspace or a
plugin entry), the core never grants the whole tree while calling the inner
path protected. It grants the tree **per child**, descending only along the
way to each protected inner path, and leaves the protected ones out. This
covers, for example, a `$PATH` entry `<ws>/bin`, `<ws>/node_modules/.bin`,
an existing `<ws>/.synaps/plugins`, and `<ws>/.git/hooks` and
`<ws>/.git/config`. A workspace with none of these inside is granted whole,
so a command can create new files and folders at its top. Each
inner path left out is listed by `synaps sandbox check` (`left out of its
write grant`). A child that is a symlink is never granted.

If a root cannot be split (a folder that can't be listed, or one with more
than 2048 entries), the session is refused with the cause
`protected-inside-grant: <path>`.

**The cost, stated plainly.** A directory that lies on a split path (the
workspace itself when `<ws>/bin` is protected, or `<ws>/node_modules`) gets no
grant of its own, so a shell command can't create **new entries directly in
it** (`touch <ws>/new-file`, `mkdir <ws>/new-dir`); its existing entries and
everything beneath the granted children stay writable. A `write`, `edit` or
`multi_edit` call does create files there, because it checks the policy on the
opened folder and the domain's per-directory grants do not bind the engine
itself. `synaps sandbox check` prints each such directory. The git directory
has the same limit: only `objects/`, `refs/`, `logs/` (and, in a linked
worktree, that worktree's own admin folder) are granted, never `.git/`
itself, so a shell command can't create new entries directly in `.git/`.

### Git checkouts

* A **main checkout** (`.git` is a folder): the workspace is split around
  `.git`; only `.git/objects`, `.git/refs` and `.git/logs` are granted in it.
  `.git/hooks` and `.git/config` (and `HEAD`, `index`, `packed-refs`) are
  read-only, so a command can't plant a hook the owner's next unconfined git
  would run. **Git commands that write the index fail in a confined main
  checkout**: `git add`, `git commit`, `git checkout`, `git stash`, `git
  merge` and the like stop with `Unable to create '.git/index.lock'`, because
  `.git/index` and its lock sit in the read-only part of `.git`. Run them
  yourself, or work in a linked worktree.
* A **linked worktree** (`.git` is a file): the workspace is granted around the
  `.git` file, which stays read-only; the **common directory** it points at
  (resolved from the `gitdir:` line and the admin directory's `commondir`,
  checked against each other) gets the same three grants plus the worktree's
  own admin folder (`<common>/worktrees/<name>`). `<common>/hooks` and
  `<common>/config` stay read-only here too.
* **Any other `.git`**: the `write`, `edit` and `multi_edit` tools refuse a
  path with a `.git` component (any letter case) that is not a write root's
  own checkout as above: a `.git` in a folder that isn't a repo yet, a nested
  repo's, a submodule's `.git` file. Shell commands are not stopped there
  (see [Not covered](#not-covered)).

## Project plugins

With `sandbox.confine = enforce`, Synaps doesn't load plugins from a
project's `.synaps/plugins` folder: the same switch as
`SYNAPS_DISABLE_PROJECT_PLUGINS=1`, which enforce turns on. One log line
says so when such a folder exists, and `synaps sandbox check` prints
`project_plugins: not loaded (sandbox.confine = enforce)`. Plugins installed
in the base folder load as before. With confinement off, project plugins
load as they always have (that is not new). A plugin folder a sandboxed
command created in a project while `enforce` was on loads once you turn
`enforce` off.

## Refusals

A session that is not allowed to run unconfined is refused at start (and
again when a parked session wakes), with a cause:

| Cause | When |
|---|---|
| `abi-too-old` | the kernel has no Landlock, or an ABI older than 3 (kernel 6.2), or this isn't Linux |
| `no-policy` | `enforce` with no `sandbox.policy_provider` |
| `provider-missing` | the named plugin isn't loaded, doesn't declare the `sandbox_policy` hook, or doesn't hold `sandbox.policy` |
| `provider-timeout` (`timeout`) | no answer within 5 s |
| `provider-error` (`error`) | a JSON-RPC error |
| `provider-bad-answer` (`bad-answer`) | an answer that is not exactly `{"write": [...], "read": [...]}` or `{"refuse": "..."}` |
| `provider-refused` (`refused: <reason>`) | `{"refuse": "<reason>"}`; the reason has control and bidi characters stripped and is cut at 200 characters |
| `protected-inside-grant: <path>` | a protected path is inside a grant and the grant can't be split |

When the provider is the cause, the session start's message names it:
`sandbox policy provider '<id>' <cause>`, the cause being `missing`,
`timeout`, `error`, `bad-answer` or `refused: <reason>` (for example
`sandbox policy provider 'my-policy-plugin' timeout`). Otherwise it reads
`sandbox: this session cannot be confined, so it was refused (<cause>)`.

A runtime that never resolved a policy while `enforce` is on refuses every
command and write; it does not run loose.

## `synaps sandbox check`

```
synaps sandbox check [--session <id>] [--workspace <dir>] [--json]
```

Reads the config, asks the provider the same question a session start would
(`session_id` `check`, unless `--session`; workspace `--workspace` or the
current folder) and prints what the session would get. It writes nothing.
Exit status 0 when usable (or confinement is off), 1 when it would refuse.
Plain output is `key: value` lines; `--json` prints exactly one object with
the keys `confine` (`off`/`enforce`), `abi` (int or null), `provider` (string
or null), `provider_state` (`off`, `loaded`, `missing`, `timeout`, `error`,
`bad-answer`, `refused`), `refusal` (a cause word above, or null), `write`,
`read` (absolute paths), `dropped` (`{"path","reason"}` each).

The plain report also lists the paths left out of a grant and the
directories on a split path, says `read: not enforced (v1)`, and under
`enforce` ends with `project_plugins: not loaded (sandbox.confine =
enforce)`.

## Other processes and the daemon

* **With the sandbox on, the Synaps daemon refuses sandboxed clients.**
  Every process a session starts confined runs with `no_new_privs`: the
  confined start sets it, all it starts inherit it, and it can't be
  cleared. The daemon reads that flag of each connecting process (by the
  pid the kernel records at `connect`, pinned with a pidfd on kernel 6.5
  and later) and answers `Refused` after Hello, then closes. So a sandboxed
  command that connects to the daemon can't create, attach to or drive a
  session, change the daemon's settings, reload it or shut it down, even
  after `setsid`, a double fork or a move to another cgroup scope. The
  sandbox is on for the daemon when its own config or the config of any
  profile in the same Synaps folder says `sandbox.confine = enforce`; it
  reads those files again for each connection. With the sandbox on, a
  client of yours that itself runs with `no_new_privs` (a systemd unit with
  `NoNewPrivileges=`, some containers, a flatpak or bubblewrap terminal) is
  refused too. With it off in every config there, the daemon admits any
  client running as you, as without a sandbox.
* **Signals and abstract sockets.** On Landlock ABI 6 and later (kernel
  6.12), a sandboxed process can't signal a process outside its sandbox (the
  daemon, your shell) or connect to an abstract unix socket made outside it.
  On ABI 3 to 5 it can (`synaps sandbox check` prints the ABI).
* **Extensions.** An extension whose manifest has no `deferred` block runs
  in one process every session shares, which is not sandboxed, so a
  sandboxed session is refused its tools. A deferred extension's tools run
  per session, inside the session's sandbox, and a process started under one
  sandbox is never reused under another. Hook and policy-provider processes
  are the host's own and are not sandboxed.

## Policy plugins

See [extensions/hooks.md](extensions/hooks.md#sandbox_policy). A plugin
declares `hooks: [{"hook": "sandbox_policy"}]` and the permission
`sandbox.policy`, and answers `hook.handle` with `{"write": [...], "read":
[...]}`. It can only add; the protected set above is not its to change. The
daemon advertises `sandbox_policy_v1` in `Welcome.features`.

## Limits

* **Reads are not confined** (`read: not enforced (v1)`): a confined command
  can read anything the owner can, including credential stores.
* **The network** is not confined; process control only as described in
  [Other processes and the daemon](#other-processes-and-the-daemon).
* **macOS and Windows** have no Landlock; with `enforce` every session is
  refused there, and with confinement off nothing changes. macOS stays
  unconfined.
* **Subagent workers** are asked of the policy provider too (their own
  session id, `parent_session_id` as the engine records it), and what the
  provider answers is held to the folders the parent session may write, so a
  worker is never less confined than its parent. A worker the provider
  refuses, or cannot answer for, does not run. There are no worker-specific
  policies beyond that.
* **A linked worktree's own admin folder** (`<common>/worktrees/<name>`) is
  granted whole, because git writes its `HEAD`, `index` and locks there, and
  Landlock cannot leave a file read-only inside a granted folder. A command
  can therefore rewrite `commondir` or `config.worktree` in it; a later
  unconfined `git status` in that worktree would read them. The common
  folder's `hooks/` and `config` are protected; this one is not.
* **MCP discovery** (the engine's one-off start of an MCP server to list its
  tools, which belongs to no session) runs under the **baseline only**: no
  workspace, no provider folders. With `enforce` on a kernel that can't
  confine, it does not start the server.
* **Daemon re-exec** (the daemon starting a fresh copy of itself) is not a
  session command and is not confined.
* A session's policy is fixed when it starts; a changed provider answer takes
  effect for new sessions and when a parked session wakes.
* Landlock allows only; see **splitting** above for what that costs.

### Not covered

* Sandboxed commands can still ask other programs running as you to act for them: systemd-run --user, D-Bus services, a tmux server, ssh-agent. Through such a program they can also reach the Synaps daemon. The daemon refuses only a sandboxed command that connects to it directly.
* In a folder that isn't a git repo yet, a sandboxed command can create one with hooks or settings that run code; your next git command there runs them. Nested repos are the same.
* /tmp and /dev/shm are shared with your other programs and sessions.
* A bind mount of a protected folder, a file a provider grants in your home folder, and an extension command outside the Synaps folder aren't protected.
* Under enforce, a project's own plugins aren't loaded.
