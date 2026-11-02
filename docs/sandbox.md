# Per-session write sandbox (Linux, Landlock)

An opt-in, kernel-enforced write sandbox for the commands an agent runs
through the `bash` tool and the interactive `shell_*` (PTY) tools. A
sandboxed command can read and execute everything it could before, but it
can only create, change, rename or delete files under the paths its
session's policy allows. Everything else fails with `EACCES` (`EXDEV` for a
cross-boundary rename or hard link).

It is **accident containment, not a security boundary** against a hostile
agent; see [Limits](#limits).

It is off unless a session has a policy file. With no file, the spawn path
is exactly what it was without the sandbox.

> **Stage status.** The `bash` tool's commands (rollout step C3) and the
> PTY shell (`shell_start`, and everything later sent to that shell with
> `shell_send`; step C4) are sandboxed as described here. Not yet:
> subagents (the subagent stage), whose commands still run unsandboxed, as
> do a few internal tool contexts that have no conversation id (see
> [Limits](#limits)). A compaction into a linked successor carries the
> policy to the new id (see [Compaction](#compaction)). Opt-in, a file can
> also hold the session's own in-process tools to it (`tools = confined`,
> see [Confined tools](#confined-tools-tools--confined)): its file tools
> write only inside the write set, it starts no worker (subagent), and it
> runs no other tool that isn't on its list. The daemon answers a
> [`sandbox_status`](#the-sandbox_status-query) query with what it would
> apply to a session.

## Turning it on for a session

One file per session:

    <base>/sandbox/sessions/<session-id>.conf

`<base>` is the Synaps base dir (`$SYNAPS_BASE_DIR`, default
`~/.synaps-cli`). The file is read at **every** bash/PTY spawn of that
session, so writing it turns the sandbox on from the next command, and
deleting it turns it off from the next command. No reload or restart.

A PTY shell is one spawn: its sandbox is decided at `shell_start`, and
every command later sent to that shell runs inside the domain the shell
started with. A policy change (or the kill switch) reaches the session's
next `bash` command and next `shell_start`, not a shell that is already
running: end it (`shell_end`) and start a new one.

Session ids that can name a file: ASCII letters, digits, `-`, `_`, `.`, not
starting with `.`, at most 128 bytes.

### Kill switch

    <base>/sandbox/OFF

While anything exists at that path (a file, a directory, even a dangling
symlink), **every** sandbox is off, whatever the session files say. It is
checked first, at every spawn, so it also overrides a broken policy file.
The per-session files are left alone, so removing `OFF` restores every
sandbox as it was. The daemon logs `sandbox.off reason=global` once when the
switch appears and `sandbox.on reason=global-cleared` once when it goes.
The check, the recorded state and the log line happen under one lock, so
concurrent spawns log each change once, in the order they observed it.

### Compaction

A compaction into a linked successor (`compaction_policy =
LinkedSuccessor`: the TUI's `/compact` and `auto_compact`, or a client's
compact command on a session created that way) gives the conversation a
new journal id. Before the successor's journal is written, the
predecessor's policy file is carried to `<new-id>.conf`: the same bytes and
the same permission bits (the `rwx` bits only; setuid, setgid and sticky
are never copied), one link, read and validated like any policy file,
written to a temp name and renamed into place without replacing anything. A
successor never exists without its predecessor's policy:

- no file for the predecessor: none for the successor (unsandboxed stays
  unsandboxed);
- a file already at `<new-id>.conf`: kept as it is;
- the copy can't be made (an invalid or unreadable predecessor file, a full
  disk): the compaction fails ("sandbox policy could not be carried to the
  new session id: ...") and the session stays on its predecessor, still
  sandboxed by its own file. **It can't compact until that's fixed**. The
  error and the `sandbox.carry ... result=failed` line name the bad line of
  an invalid file but never quote it: `synaps sandbox check <file>` gives
  the reason;
- a step after the rename fails (the directory's fsync): the new file is
  removed again before the error is returned, if it is still the file the
  carry created (same device and inode), and the error says which ("the
  copy was removed" / "the copy was not removed: <why>");
- the compaction fails after the copy: the copy is removed the same way;
  anything else at that path is kept (`result=rollback-kept`).

The identity check is `lstat` then `unlink`, not one atomic step: it never
removes a file that was already replaced when it looks, but an unconfined
process that replaces the file between the two calls can still lose its
replacement. Policy files are written only by operators, who should not
edit a session's files while it compacts.

**Crash residue.** A daemon that dies mid-carry can leave two kinds of file
in `<base>/sandbox/sessions/`, neither of which a spawn ever reads as a
policy for a live session:

- before the rename: a hidden temp file `.<new-id>.conf.carry-<pid>-<ns>`.
  A spawn only reads `<id>.conf`, and an id can't start with `.`, so it is
  inert. Remove any `.*.conf.carry-*` file whose `<pid>` is not a running
  daemon;
- after the rename, before the successor's journal was saved: an orphan
  `<new-id>.conf` with no session `<new-id>` (no `<base>/sessions/<new-id>.json`).
  Nothing uses it; remove it when no journal of that id exists.

Both are different from a failed step above, which cleans up after itself
before it returns.

In-place compaction keeps the id and carries nothing.

**Name and edit policy files by the journal id** (`journal_id` in session
listings). It equals the session id until a linked compaction. After one,
the daemon still shows the old session id until its next reload, but the
policy in effect is `<journal_id>.conf`: editing `<old id>.conf` then has
no effect on the session. Successor files accumulate, one per linked
compaction; a cleanup can remove the files of ended sessions. `synaps
sandbox check` validates a file; it doesn't follow a compacted id to its
successor.

## File format

Flat `key = value` lines. Blank lines and lines starting with `#` are
ignored. There are no inline comments (`#` inside a value is literal) and no
expansion of `~` or variables: paths must be absolute. Unknown keys,
repeated single-valued keys and malformed lines are errors.

| key | value | repeatable | meaning |
|---|---|---|---|
| `mode` | `enforce` \| `off` | no, **required** | `enforce` sandboxes the session's commands; `off` records an explicit opt-out |
| `required` | `true` \| `false` (default `false`) | no | with `enforce`: refuse every spawn when Landlock is unavailable on this host, instead of running unsandboxed |
| `subagents` | `enforce` \| `off` (default `enforce`) | no | whether the session's subagents are sandboxed. `off` is only valid with `mode = off`: an enforcing session's subagents are always sandboxed |
| `lane` | name (`A-Z a-z 0-9 . _ -`, ≤ 64) | no | a label, used in hints (e.g. where new worktrees should go) |
| `contact` | one line, ≤ 200 characters, without prompt words (`password`) | no | the advice sentence appended to refusals and hints, e.g. `Ask your operator.` |
| `audit_log` | `off` \| `/var/log/audit/audit.log` (the default) | no | `off` drops this session's `src=audit` and `sandbox.domain_end` lines (see [The kernel's audit records](#the-kernels-audit-records-srcaudit)); the default spelled out changes nothing. The daemon reads only that log, so any other value is invalid |
| `write` | directory | yes | **work** directory: all write rights beneath it |
| `write_file` | regular file | yes | **work** file: write and truncate that file only (it can't be created, removed or renamed) |
| `infra` | directory | yes | **infrastructure** directory (caches, build output, tool state): all write rights beneath it |
| `infra_file` | regular file | yes | **infrastructure** file: write and truncate only |
| `git_common` | a repository's `.git` directory | yes | the git common-dir subset: `objects/`, `refs/`, `logs/`, `worktrees/` |
| `cargo_home` | a `CARGO_HOME` directory | yes | `registry/`, `git/` and the files `.package-cache`, `.package-cache-mutate`, `.global-cache` |
| `device` | a character device directly under `/dev` (`/dev/<name>`) | yes | **infrastructure** device node: write and truncate that node only (not what its driver does: see below) |
| `tools` | `open` \| `confined` (default `open`) | no | `confined` (only with `mode = enforce`) holds the session's in-process tools to this file too: see [Confined tools](#confined-tools-tools--confined). `open` is today's behavior |
| `tool` | a tool's exact runtime name (`A-Z a-z 0-9 _ . : -`, 1-128), e.g. `ext__figma__get_screenshot` | yes (at most 64) | only with `tools = confined`: lets the session run that extension or MCP tool too. A worker tool (`subagent`, `subagent_start`, `subagent_resume`) can't be named |

Work and infrastructure rules are applied the same way to the session's own
commands. The split matters for subagents: a subagent's write set is cut
from the parent's **work** rules by its `write_policy`, while it keeps the
**infrastructure** rules so it can still build and test.

`git_common` deliberately leaves the rest of the common dir read-only:
`config`, `hooks/`, `info/`, `packed-refs`, `index` and `rr-cache/`. A
writable hook or config would run the agent's code in the next unsandboxed
git command. Expected by-design denials: deleting any ref (`branch -D`,
`tag -d`, `update-ref -d`, `fetch --prune`, loose or packed: git locks
`packed-refs` for every ref deletion), writing tracking config (`git
worktree add -b x <remote-branch>`; use `--no-track` or a commit),
`rerere`, and the `pack-refs` step of `gc --auto` (a warning). A commit
(and `git worktree add`) also deletes a pseudo-ref, so it prints `error:
Unable to create '<git_common>/packed-refs.lock': Permission denied` and
still succeeds. That path gets its own class (`git_packed_refs`) and hint,
which says the exit code tells. A cherry-pick succeeds the same way but
can't delete `CHERRY_PICK_HEAD`, so the next cherry-pick or merge refuses
with "You have not concluded your cherry-pick"; remove the file that `git
rev-parse --git-path CHERRY_PICK_HEAD` names (for a linked worktree it is
in the common dir's `worktrees/<name>/`, which `git_common` allows).

`cargo_home` leaves `bin/`, `.crates.toml`, `.crates2.json` and the
directory itself read-only. cargo's last-use tracking database
(`.global-cache`) keeps its journal next to it, so cargo may warn
`failed to save last-use data`; builds still work.

`device` is for hardware a command opens for writing. The baseline grants
no device but `/dev/null`, `/dev/zero`, `/dev/tty`, `/dev/ptmx` and
`/dev/pts`, so any other device opened for writing gets `EACCES`. A GPU
job is the common case: CUDA, `nvidia-smi`, NVENC and the NVIDIA GL driver
open the NVIDIA nodes read-write, so a lane that runs them lists each node
it has:

    device = /dev/nvidiactl
    device = /dev/nvidia0
    device = /dev/nvidia-uvm
    device = /dev/nvidia-uvm-tools
    device = /dev/nvidia-modeset

One line per node; there is no glob. A node that doesn't exist (a second
GPU, a module not loaded yet) is skipped like any missing path. Reading a
device was never restricted, and neither are ioctls: the sandbox doesn't
handle Landlock's `IOCTL_DEV` right. Root-only devices (`/dev/mem`,
`/dev/kmem`, `/dev/port`) are kept out by their file permissions, not by
this key. Only a node directly in `/dev` can be granted: nested ones
(`/dev/dri/*`, `/dev/net/tun`, `/dev/input/*`) are refused.

**A `device` line confines writes to that node, not what its driver does
with them.** Grant a node only when a lane needs it, one node at a time, as
a deliberate decision by whoever owns the policy. What a granted node can
reach, when its permissions, the user's groups or the session's ACLs let
the user open it:

- `/dev/uinput` injects keyboard and mouse input into the desktop, so it
  can drive an application that isn't sandboxed into writing anything;
- `/dev/kmsg` writes lines into the kernel log;
- terminal nodes (`/dev/ttyN`, `/dev/console`) type into or disturb
  another terminal;
- GPU, KVM and vhost nodes (`/dev/nvidia*`, `/dev/kvm`, `/dev/vhost-*`)
  expose large driver interfaces;
- tun (`/dev/net/tun`, nested, so refused anyway) makes network
  interfaces, behind its own capability checks.

`/dev/fuse` alone doesn't get past the sandbox: mounting a FUSE file
system also needs mount rights or the setuid `fusermount` helper, and a
sandboxed command can do neither (Landlock refuses a mount, and no new
privileges means setuid doesn't apply). Since reads and ioctls aren't
restricted, some of a device's effects are possible through a read-only
open with or without this key. Device rules are infrastructure rules, so
subagents keep them too (once subagents are sandboxed).

### Example

    # worker session, lane "demo"
    mode = enforce
    lane = demo
    contact = Ask your operator; they own the sandbox policy files.
    write = /var/tmp/demo
    write = /home/me/src/project-wt
    infra = /home/me/.cache
    infra = /var/tmp/demo-target
    git_common = /home/me/src/project/.git
    cargo_home = /home/me/.cargo

### Always allowed (baseline)

| path | rights |
|---|---|
| `/tmp`, `/dev/shm` | all write rights |
| `/dev/null`, `/dev/zero`, `/dev/tty`, `/dev/ptmx` | write, truncate |
| `/dev/pts` | write, truncate beneath |
| `/proc` | write, truncate, and the create bit procfs needs for `> /dev/stdout` (procfs itself never lets a file be created or removed) |

A baseline path that doesn't exist on the host is skipped; any other error
opening, naming or statting one refuses the policy. The baseline
directories are granted whole, so they must not contain `$HOME` or the base
dir: with either under `/tmp`, `/dev/shm`, `/dev/pts` or `/proc` (by name,
or as the same directory under another name), **every** policy is refused.

## Validation (fail closed)

Validation checks the object that was actually opened, not the path string:
every path is opened as a handle (`O_PATH`, symlinks followed), the kernel's
name for that open object (`/proc/self/fd/<n>`) is checked, and the Landlock
rule is later built from that same handle. Swapping a checked path for a
symlink afterwards changes nothing. A path is refused if the opened object:

- is `/`;
- has fewer than 3 components (`/var/tmp` is refused; paths under `/tmp`
  and `/dev/shm` are exempt, they are writable anyway, and so is a
  `device` named `/dev/<name>`, for that key only);
- for `device`: isn't directly under `/dev`. By its text, split on `/` as
  written, it must be `/dev/` and one name; this is checked before anything
  is opened, so anything else is an error even when it doesn't exist: a
  nested path (`/dev/dri/card0`, `/dev/net/tun`), `.`, `..`, an empty
  component (`/dev//null`) or a trailing `/`. By the kernel's name for the
  opened object, its parent must be `/dev`: a symlink out of `/dev` (such
  as `/dev/fd`) or into a directory under it is refused;
- is `$HOME` or an **ancestor** of `$HOME`;
- is the Synaps base dir, under it, or an ancestor of it;
- is, under another name (such as a bind mount), the very directory (same
  device and inode) of `$HOME`, the base dir, one of their ancestors, or
  the base dir's `sandbox/` or `sandbox/sessions/`;
- is, is under, or is an ancestor of one of owner rule 27c's paths, whatever
  the base dir is (see [Owner rule 27c](#owner-rule-27c-jrs-paths)): the Jev
  state dir `$HOME/.synaps-cli/memory/jev`, the Jev guard's config
  `$HOME/.synaps-cli/plugins/jev/config` (so the whole Jev plugin dir, its
  ancestor, can't be granted either; its other files can), and the Synaps
  config `$HOME/.synaps-cli/config`, by their names (a root that doesn't
  exist yet by the name it would get), or, under another name, is the same
  object as one of them or as a directory above one;
- is a file with more than one hard link (`write_file`, `infra_file` and
  the `cargo_home` files), or a `device` node without exactly one (none
  means it was unlinked after it was named): a grant covers the inode, so
  all of its names;
- has the wrong type: `write`, `infra`, `git_common` and `cargo_home` need a
  directory; `write_file` and `infra_file` need a regular file; `device`
  needs a character device (not a block device, directory, FIFO or regular
  file); `git_common` needs `objects/`, `refs/` and `HEAD`.

The whole policy is refused if `$HOME` or the base dir can't be resolved
(canonicalised), if it or one of their ancestors can't be statted, or if
`$HOME` is `/`: validation never falls back to a weaker, name-only check.

Together these keep a sandboxed command from rewriting the policy that
governs it (or creating `OFF`), **within one trust assumption**: the
protected tree and the mount namespace aren't rearranged by unconfined
processes after validation. A bind mount placed deeper inside an allowed
directory, or a protected subtree moved into one, isn't chased.

A policy builds at most **256 rules**, the baseline and every expansion
included. `contact` may not contain the word `password` (prompt-like text
has no place in a refusal).

A path that **doesn't exist** is skipped (narrower is safe) and logged as
`sandbox.skip`; create it and the next spawn picks it up. Validation needs
`/proc` mounted; without it no path can be named (the baseline included)
and the file is refused.

The file itself is opened once, without following a symlink and without
blocking, and checked through that handle. A file that is a symlink, isn't a
regular file, has more than one hard link, can't be read (other than
"doesn't exist"), is larger than 64 KiB, isn't UTF-8, or fails any rule
above refuses **every** spawn of that session until it is fixed. The
command gets:

    sandbox policy invalid for this session (<file>:<line>: <reason>); commands can't run until it's fixed. <contact>

`<contact>` is the file's `contact` value (found even when another line is
broken), else `Ask the operator who manages this session's sandbox policy.`

## The validator

    synaps sandbox check <file>

Runs the same read, parse and validation code as a spawn and prints every
rule as it would be applied (class, file or directory, origin line,
canonical path, rights), the skipped paths, the running Landlock ABI and
the kill-switch state, and, for a `tools = confined` file only, a `tools
= confined: ...` line after the mode line with the `tool =` names. When
the kill switch is present, that is the
report's first line: no policy file is read at a spawn while it exists.
Exit 0 = valid, 1 = invalid (with the refusal text
the agent would see). It writes nothing. Write new policies as
`<id>.conf.new`, check them, then `mv` them into place, so a spawn never
reads a half-written file (`mv` keeps the single link; don't `ln` it). The
reported Landlock ABI is the usable one: an ABI below 3 shows as
unavailable, as it behaves at a spawn.

## Kernel support

Landlock ABI 3 or later is needed (Linux 6.2+): ABI 1 refuses every
cross-directory rename and ABI 2 can't stop truncation outside the set.
Rights added by later ABIs are never handled, so a newer kernel can't start
denying more. On ABI 7+ the domain is created with `LOG_NEW_EXEC_ON`, so the
kernel audits denials made after `exec`.

Without a usable Landlock (`no-landlock`, `disabled`, `abi-too-old`), an
enforcing session runs unsandboxed and this is logged, unless the file sets
`required = true`, in which case its spawns are refused. Any other failure
of the version query or of building the domain (an errno such as `EMFILE`
or `ENOMEM`, or a refused rule) refuses the spawn whatever `required`
says: an error is never a reason to run unsandboxed.

Off Linux (macOS) there is no Landlock (`unsupported-os`) and the sandbox
fails closed: every bash/PTY spawn of a session whose file says `mode =
enforce` is refused, whatever `required` says, with the `required` text
and lines (`sandbox.refused session=<id> reason=unavailable:unsupported-os`).
The file, the kill switch, `mode = off`, `tools = confined` and owner rule
27c work as on Linux; a session without a file is unchanged. `synaps
sandbox check` reports the file and always exits 1 there.

## Owner rule 27c: JR's paths

Three paths under `$HOME` are written only by JR's app, the Jev guard (in
the Jev extension process) and the daemon itself, never by an agent:

| Path | What it holds |
|---|---|
| `$HOME/.synaps-cli/memory/jev/` | JR's Allow/Deny answers (`access/answers/`), the guard's `access/requests/` and `access/used/`, and the `sandbox-mode` switch |
| `$HOME/.synaps-cli/plugins/jev/config` | the Jev guard's config (it holds the Jev API key) |
| `$HOME/.synaps-cli/config` | the whole Synaps config (the guard's legacy fallback) |

They are `$HOME`-based, not base-dir-based, so they are protected whatever
the daemon's base dir is. Two layers:

- **Policy validation** refuses a grant equal to, under, or an ancestor of
  one of them (see [Validation](#validation-fail-closed)), so no sandboxed
  bash or PTY command can write them.
- **In-process tools**: in a session whose policy enforces (`mode =
  enforce`, the kill switch absent, whatever the Landlock ABI; a policy file
  that exists but can't be used counts too, failing closed), `write`, `edit`
  and `multi_edit` refuse a target that is or is inside one of them, the
  base dir's `config`, or the base dir's `sandbox/` (the policy files and
  `OFF`). The target's parent is opened as a handle (`O_PATH`, symlinks
  followed); the kernel's name for it and the identity of it and of every
  directory above it (walked by `..`, so a bind mount of a protected
  directory, or of a directory above one, is seen by its identity; a bind
  mount of a directory below one isn't, see the limits) are checked, and so
  are the name the target has or would get and the object there now (a
  second hard link of a protected file, a symlink into a protected
  directory). A kernel name ending in ` (deleted)` (the kernel's mark for an
  unlinked directory, but also a legal live name) is used only when that
  exact path is the same object; otherwise the check fails closed. `write`
  and `edit` then write through that same handle (a new temp file, `O_EXCL
  | O_NOFOLLOW`, renamed into place), so a symlink swapped in after the
  check can't redirect the write. Missing parents are made one directory
  at a time from a checked handle: before the first one is made, the whole
  target as it would be named is checked (so a refusal makes no
  directory), and each new name is checked again before it is made; when a
  `.` or `..` among the missing names makes the whole name unknowable
  ahead, only those per-step checks apply, so a refusal can leave the
  directories made before the protected name. `multi_edit` checks its own
  verified parent handle the same way before it reads or plans the file.
  Every write to a target that isn't protected gives the result it gives
  without a sandbox (the same modes, parents, symlink handling and error
  texts), with two exceptions: the temp file is created `O_EXCL |
  O_NOFOLLOW`, so a temp name planted beforehand makes the write fail
  instead of being followed or truncated; and a target whose check can't
  run (no usable name for it) fails closed with "the target could not be
  checked against owner rule 27c". A refusal changes nothing (but see the
  `..` case above); the agent gets

      Refused by owner rule 27c: `<path>` is protected. It is written only by JR's app or by JR, never by an agent's write, edit or multi_edit. Nothing was changed.

  and the daemon log gets one `sandbox.protected session=<id> tool=<tool>`
  line, without the path or any content.

## Confined tools (`tools = confined`)

A sandbox file confines the commands its session runs (bash and the PTY
shell). The session's own tools run in the daemon, outside that domain:
without this key, `write`, `edit` and `multi_edit` refuse only owner rule
27c's paths, a subagent runs unsandboxed, and extension and MCP tools
aren't covered at all. `tools = confined` (with `mode = enforce`) closes
that for a session that needs it, for example a short-lived session that
should read everywhere but write only in its own scratch:

    mode = enforce
    tools = confined
    write = /var/tmp/branch-1234
    contact = Ask the Master's desk.

In that session, whatever the Landlock ABI (the checks need no kernel
support; only the kill switch lifts them):

- **The file tools** (`write`, `edit`, `multi_edit`) write a target only
  if the directory it is or would be written in is, or is below, one of
  the file's directory rules with every write right: `write`, `infra`,
  `git_common`'s four directories, `cargo_home`'s two, and the baseline's
  `/tmp` and `/dev/shm`. File rules (`write_file`, `infra_file`, `device`,
  `cargo_home`'s files), `/dev/pts` and `/proc` admit no file-tool write:
  the tools replace a file through a temp file and a rename in its
  directory, which Landlock refuses there too. The check runs on the opened
  directory, by identity, walking `..` (as Landlock's path-beneath rules
  do), after owner rule 27c's check, on the same handle the tool then
  writes through: a symlink, a `..` or a bind mount leads where it leads;
  a final symlink is replaced, not followed, as without a sandbox; a
  missing parent is checked before any directory is made, so a refusal
  makes nothing. A protected target keeps 27c's refusal and its
  `sandbox.protected` line.
- **Worker tools** (`subagent`, `subagent_start`, `subagent_resume`) are
  refused, and so is the daemon's own resume of the session's workers
  after a reload: subagents aren't sandboxed yet, so this is the only way
  to keep a confined session's work confined.
- **Every other tool** runs only if it is on the built-in list or named
  by a `tool =` line. The built-in list holds the read-only tools and the
  tools whose commands run in the session's Landlock domain: `read`,
  `grep`, `find`, `ls`, `bash`, `shell_start`, `shell_send`, `shell_end`,
  `write`, `edit`, `multi_edit` (checked as above), `wait`,
  `context_checkpoint`, `memory_search`, `memory_fetch`, `forum_read`. It
  is an allowlist: a tool added later is refused until a file names it.
  The name is the tool's exact runtime name (an extension or MCP tool's
  prefixed name), matched after the tool-activation gate resolved it and
  before any `before_tool_call` hook or the tool runs, on every dispatch
  path: the stream loop's single and parallel lanes, the non-streaming
  loop's, and an extension-hosted provider's own tool loop.

Each refusal is one tool error and one log line. The agent gets

    Refused by this session's sandbox (tools = confined): `<tool>` is not on its tool list (the built-in read-only and sandboxed tools, and the policy's `tool =` lines). Nothing was run. <contact>

(for a worker tool: "starts or resumes a worker, and a confined session
runs none"; for a file tool's target: "`<path>` is outside the directories
its policy lets `<tool>` write. Nothing was changed."), and the daemon log
gets `sandbox.tool_refused session=<id> tool=<name> class=<path|worker|not_allowed>`
(the name escaped as `\xNN` outside the `tool =` alphabet, cut at 128
bytes), without a path, the contact or any of the tool's input.

A file that says `tools = confined` but can't be used (invalid, or not
UTF-8; the line is found leniently, as the contact is) keeps the tools
confined with an empty write set and no `tool =` names: no file-tool
write, no worker, only the built-in tools (whose commands are refused
anyway, as for any invalid file). A file that isn't read at all (a
symlink, a second hard link, over the size cap) is invalid as before,
without that. The file is read at every tool call, like a spawn reads it:
a new or changed file applies from the next call.

### The `sandbox_status` query

An app that writes a session's policy file can ask the daemon, before it
starts work in that session, what the daemon will apply: the daemon reads
the file from its own base dir, so a file written where the daemon doesn't
look shows up as `none`. The query is a control frame (protocol 3, feature
`sandbox_status` in `Welcome.features`; a daemon without it answers
`Error "malformed frame: unknown variant ..."` and keeps the connection):

    {"type":"sandbox_status","id":1,"session":"<journal id>"}
    {"type":"sandbox_status_result","id":1,"session":"<journal id>","state":"enforce","kill_switch":false,"tools":"confined","policy_hash":"<16 hex>","rules":12,"skipped":0,"abi":6}

`state` is `none` (no file: unsandboxed), `off`, `enforce` or `invalid`
(every spawn refused), or `unavailable` (an enforcing file on an OS
without Landlock: every spawn refused); `tools` is `open` or `confined` (an `invalid` file
that says `confined` stays confined); `policy_hash` is the `sandbox.policy`
line's hash (`null` for `none` and `invalid`); `rules` and `skipped` count
as that line does; `abi` is the usable Landlock ABI (`null` when
unavailable). It comes from the same read and validation a spawn makes and
works for a session that has never spawned. It reads the file even while
the kill switch is present (`kill_switch: true`: nothing is enforced while
it is). It writes nothing, logs nothing and never returns a path, the
contact sentence or the file's text.

## Log lines

Fixed-shape lines at WARN (the three `sandbox.audit` counts at INFO) in the
daemon log, `<base>/synaps.log.<YYYY-MM-DD>`
(one file a day; `<base>/<profile>/` with a profile). Their tracing target
is `agent_engine::sandbox`, which the daemon's default filter keeps with
`RUST_LOG` unset. The log doesn't print targets: find the lines by their
`sandbox.` prefix.

    sandbox.policy session=<id> mode=<enforce|off> subagents=<enforce|off> abi=<n|unavailable:<reason>> rules=<n> skipped=<n> hash=<16 hex>[ tools=confined named=<n>]
    sandbox.skip session=<id> line=<n> key=<key> path=<path> why=missing
    sandbox.invalid session=<id> file=<file> line=<n> reason=<reason>
    sandbox.off session=<id> reason=explicit subagents=<enforce|off>
    sandbox.off reason=global
    sandbox.on reason=global-cleared
    sandbox.off session=<id> reason=removed
    sandbox.unavailable session=<id> reason=<no-landlock|disabled|abi-too-old>
    sandbox.refused session=<id> reason=unavailable:<reason>
    sandbox.refused session=<id> reason=landlock-error errno=<n>
    sandbox.refused session=<id> reason=build <error>
    sandbox.spawn session=<id> tool=<tool> tag=<tag> pid=<pid>
    sandbox.spawn_failed session=<id> tool=<tool> tag=<tag> error=<error>
    sandbox.sudo_refused session=<id> tool=<tool> tag=<tag>
    sandbox.denial_suspected session=<id> tool=<tool> tag=<tag> class=<class> path=<path|-> src=output
    sandbox.denial session=<id> tool=<tool> tag=<tag> op=<op> class=<class> path=<path|-> n=<n> src=audit
    sandbox.domain_end session=<id> tag=<tag> denials=<n> ptrace=<n>
    sandbox.audit unavailable reason=<reason>
    sandbox.audit available
    sandbox.audit unattributed domains=<n> records=<n>        (INFO)
    sandbox.audit dropped malformed=<n> long=<n>              (INFO)
    sandbox.audit suppressed lines=<n>                        (INFO)
    sandbox.carry session=<new id> from=<old id> result=<copied|kept|failed|rolled-back|rollback-kept> [reason=<reason>]
    sandbox.protected session=<id> tool=<write|edit|multi_edit>
    sandbox.tool_refused session=<id> tool=<name> class=<path|worker|not_allowed>

`sandbox.policy` ends with ` tools=confined named=<n>` (the count of
`tool =` lines) only for a `tools = confined` file; for every other file
the line is exactly as it was before the key existed.

A session's lines appear once per change of its state (policy seen,
changed, removed, refused), not once per spawn, in the order its spawns
observed the states (one session's spawns decide one at a time);
`sandbox.spawn` is one
line per sandboxed command (`tool=bash`) or PTY shell (`tool=shell_start`),
with the tag and child pid the audit reader joins on (see below).
`sandbox.denial_suspected` comes from the
command's own output: a `Permission denied` or `Invalid cross-device
link`, in any case (Go and other runtimes print `open <path>: permission
denied`), naming a path outside the rules that the daemon itself could
write. A message soft-wrapped over two lines counts in two shapes: its
words straddle the break (`…: permission` at the end of one line,
`denied.` at the start of the next), or the break falls after the path's
colon (a line ending in `/a/b:`, the next starting with `permission
denied`). In both, the words must end at a word end (`denied.`,
`denied (os error 13)`, the end of the line; not `deniedness`). The two
lines are then read as one. Two kinds of /proc path are skipped by their
text, never followed and never blamed: a magic link itself (a process's
or thread's `cwd`, `exe`, `root`, `fd/<n>`, `map_files/<x>`) and any path
with a `..` component that is under /proc, as written or once each `..`
drops the part before it (`/proc/<pid>/../<pid>/cwd`, `/../proc/...`).
Following one inspects another process, so the check holds back rather
than guess; this is a conservative suppression, not proof that the
refused open wasn't a write (see Limits).
A PTY shell's output is checked the same way, each part as it comes back:
`shell_start`'s first output, each `shell_send`'s, and the final output
`shell_end` reads; the lines carry `tool=shell_start` and that shell's tag,
and the hint is appended to that tool result.
Its `class` is `worktree_new` (a new worktree outside the write set),
`git_packed_refs` (a `git_common` dir's `packed-refs.lock`),
`synaps_state`, `home_dot:<name>`, `home_other`, `dev_proc`, `system`,
`other`, or `unknown` (a denial that names no path). A class holds only
`[a-z0-9_:.-]`: a `home_dot` name with any other byte (an upper-case
letter too) is `home_dot:unprintable`.
Its `path` is a quoted span or a bare word starting with `/`, read up to
the first whitespace, backtick or quote, so a quoted span of prose that
starts with `/` (a printed brief) gives its first word. In the line and in
the hint it prints as the audit lines print theirs (see below): as is when
plain visible ASCII, else quoted with every control, space or non-ASCII
byte as `\xNN`. It takes at most 1024 bytes: a longer path is cut at a
whole escape, quoted, and marked by `...` after the closing quote (an
uncut path never ends in `"...`).
`sandbox.sudo_refused` is a refused sudo: output that shows sudo's
no_new_privs refusal, or the sandboxed `sudo` function's own message, from
a command whose text contains the word `sudo` (`sudo x`, `cd d && sudo x`,
`/usr/bin/sudo x`; not `pseudo`, `visudo`, `sudoers`). This is a lexical
check of the command's text, not a record of what ran: see the limits
below. Output from a command without the word that only quotes those texts
(reading this file, the source, a log) logs nothing. For a PTY shell the
text checked is the shell's command for `shell_start` and the text sent for
`shell_send` (after its escapes, such as a literal `\n`, became the bytes
the shell got).

### The kernel's audit records (`src=audit`)

A second detector reads the kernel's own record of each refused write, so
it also catches what the output check can't see: a message a tool words
its own way, sends elsewhere or never prints. The two complement each
other: the output check sees a refusal as soon as the command prints it
and adds a hint to the tool result; the audit reader sees every refused
write of a spawn's domain, worded or not, about a second later, in the log
only.

Only the daemon has a reader: a CLI or test binary's enforcing spawns open
no audit log and start no thread. The daemon's first enforcing spawn opens
the audit log (`/var/log/audit/audit.log`, with `O_NOFOLLOW`; the daemon
needs read access, e.g. through the `adm` group) and moves to its end
before the child forks; that open is the only step a spawn waits for. A
thread then reads what is appended, once a second; one poll reads at most
1 MiB, stops after the read that passes 10,000 lines, and checks for a
stop request between its 64 KiB reads. A poll that stops at its bound
leaves the rest to the next one, a second later, so a burst is read over
several seconds. Only
Landlock's own records are read (type 1423, `LANDLOCK_ACCESS`, and 1424,
`LANDLOCK_DOMAIN`, in either spelling), and only their raw fields: SYSCALL,
CWD, PATH, PROCTITLE, EXECVE and USER records, and the interpreted fields
auditd appends after `\x1d`, are never read. A value auditd wrote as hex
(a path with a space, quote, control or non-ASCII byte) is decoded.

A record is read strictly or not at all: every field is `key=value` or
`key="value"` with the quote closed, a lowercase key, and visible ASCII
with no quote inside the value; at most 32 fields and 64 blockers, and no
key twice. A record that breaks any of these is dropped whole and counted
(`malformed`), never read in part. A refusal with no `fs.*` blocker (a
`ptrace` or `scope.*` one) is only counted, without decoding its path.

A domain is joined to its spawn by the 1424 `allocated` record the kernel
writes with the domain's first refusal: its exe must be the daemon's own
(taken at its start; a trailing ` (deleted)` is ignored on both sides, as
an install replaces the binary; if the daemon can't read its own exe, no
domain is joined and all are counted as `unattributed`), its comm a tag this daemon gave out, its
pid that spawn's child (bash) or the daemon itself (a PTY shell restricts
a daemon thread), and the domain no older than the spawn (less 50 ms: the
kernel stamps its records from a coarse clock, up to a tick behind). A
refusal read before its domain's record waits up to 10 s for it, and so
does a bash child's domain until the spawn's child pid is known.

Memory is bounded: at most 4096 spawns (one that ended stays joinable for
15 min, checked again when a record is joined), 4096 domains, 1024
domains waiting (for their domain record, or for their spawn's child pid;
the two share the cap) and 4096 fold windows. Past a cap the oldest goes
first: a forgotten domain's later records and a waiting domain given up
are counted as `unattributed`, and a fold window closed early logs its
count then. Every elapsed-time gate (the waits, the 15 min keep, the fold
minute, the 10 min reports and retries, the line budgets) runs on the
monotonic clock, so a wall-clock step changes none of them; only the join
compares wall-clock stamps, the kernel's with the spawn's.

Every field a line prints from a record or a session (`session`, `tag`,
`op`, `path`) prints as is when it is plain visible ASCII, else quoted
with `\"`, `\\` and `\xNN` (each other byte, losslessly), so one record
always gives one line with no control bytes and no extra fields. A
`class` is a fixed word of `[a-z0-9_:.-]`; one that would hold anything
else prints as `<prefix>:unprintable` (`home_dot:unprintable`).

- `sandbox.denial … src=audit` is one refused write of the spawn's domain.
  `op` is what the kernel refused: `write`, `truncate`, `create`, `mkdir`,
  `symlink`, `mknod`, `remove`, `rename/link` (a rename or link across
  dirs), `mount`, or several joined by `+` (`write+truncate`). For
  `create`, `mkdir`, `symlink`, `mknod`, `remove` and `rename/link` the
  kernel names the PARENT dir, not the name created or removed; for `write`
  and `truncate` it names the file. `path` is escaped as above, and is `-`
  when the record names none (class `unknown`). The first refusal of a
  (spawn, op, path) logs `n=1` at once; repeats within the next minute
  fold into one more line at the minute's end (or at the domain's end),
  with their count in `n`.
- Its `class` is the output check's (above), `by_design:<x>` for a refusal
  of the exact object the policy means to refuse, or `uncertain:<dir>` for
  a create that names only a dir where a by-design file is made, along
  with anything else:
  - `by_design:git_common`: an op that names a `git_common` dir's root as
    the parent (a create or remove there), or a write of a file directly
    in it: the root stays read-only;
  - `by_design:session_registry`: a write of the base dir's
    `run/.session-registry.lock` (the registry reads it then);
  - `by_design:less_history`: a write of `~/.local/state/lesshst` (less,
    through git's pager);
  - `uncertain:local_state`: a create in `~/.local/state` (less's history,
    or any other program's file there);
  - `uncertain:cargo_home`: a create in a `cargo_home` dir's root.
  All are logged like the rest; a `by_design` class tells a refusal the
  policy means apart from one that may call for a policy change, and an
  `uncertain` one is not exempt.
- A session's new `src=audit` lines are held to a budget: at most 60 a
  minute per session, and 600 a minute for all sessions together (each
  with up to one count line after it). A refusal over the budget gets no
  line and opens no fold window; it is counted, and
  `sandbox.audit suppressed lines=<n>` (INFO) gives the count.
- `sandbox.domain_end`: the domain ended. `denials` is the kernel's count
  for it; `ptrace` is how many of its records were not file writes (a
  `ptrace` or `scope.*` refusal, such as a /proc walk over other
  processes), which get no line of their own.
- `sandbox.audit unavailable reason=<reason>`: the log can't be read (an
  errno name such as `ENOENT` or `EACCES`, `not-a-file`, or
  `errno-<n>`). Spawns go on, sandboxed as before, with only the output
  check. A later enforcing spawn retries at most every 10 min; a failed
  retry logs nothing, one that works logs `sandbox.audit available`. The
  reader thread logs the same pair if reading fails later and recovers,
  or if the log's name leads to nothing, to a symlink or to something
  other than a regular file for more than 10 s (the file it had open is
  read to its end meanwhile).
- `sandbox.audit unattributed domains=<n> records=<m>` (INFO): records of
  domains that couldn't be joined (no matching spawn, a domain from before
  the reader started, a refusal whose domain record never came, every
  domain when the daemon's own exe is unknown). Another program's domains
  (another exe, such as a test binary's) are skipped and not counted.
- `sandbox.audit dropped malformed=<n> long=<m>` (INFO): Landlock records
  that couldn't be read, and lines over 64 KiB, skipped.

Each of the three counts is logged at most every 10 min, only when
nonzero, and counts since its last line.

#### Accounting

One refusal can show twice: as a `sandbox.denial_suspected … src=output`
hint, when the command printed it, and as a `sandbox.denial … src=audit`
line, from the kernel. Count refusals from the audit lines only: the sum
of `n` over `sandbox.denial` lines, plus `sandbox.audit suppressed
lines=<n>`, is the number of refused writes of the daemon's attributed
domains (`unattributed` records are counted apart). `by_design:*` lines
are refusals the policy means; leave them out of a count of refusals that
call for action. Hints stay hints: a `denial_suspected` line is the
output check's reading of text, so it may name a refusal the audit lines
also name, a read, or a quoted message, and is never added to the count.

`audit_log = off` in a session's policy drops that session's `src=audit`
and `sandbox.domain_end` lines; its domains are still known as the
daemon's, so they aren't counted as unattributed.

Tags are unique per daemon start: a start's tags begin at a block of
36^6 numbers picked by a random seed (the OS RNG, read once, mod 36^5),
so a reload (which keeps the pid) reuses an earlier image's block 1 time
in 36^5 (about 60 million). The residual: when that happens AND an old
image's domain survives the reload AND its pid is reused by the new
image's spawn with the same tag, the old domain's refusals are joined to
the new spawn. A reload's exec closes the log and ends the thread; the new image opens the
log again at its first enforcing spawn. The thread stops at daemon
shutdown without holding it up: at once when it is waiting, else after
its current read.

## Limits

- **Not an adversary boundary.** At ABI 8, Landlock doesn't restrict
  `chmod`/`chown`/`utime`/xattrs, `connect()` to Unix sockets, or signals.
  A sandboxed command can still ask a running daemon (or any other
  unsandboxed service it can reach over a socket) to do work outside its
  write set.
- **In-process tools** (`write`, `edit`, `multi_edit`) run in the daemon,
  not in a sandboxed child. In an enforcing session without `tools =
  confined` they refuse only owner rule 27c's paths (see [Owner rule
  27c](#owner-rule-27c-jrs-paths)); the rest of the write set isn't
  enforced on them, and the session can run any tool, worker tools and
  extension or MCP tools included. `tools = confined` holds them to the
  write set (see [Confined tools](#confined-tools-tools--confined)), but
  it is a daemon-side check, not a kernel one: a tool it allows (a named
  extension or MCP tool in its own process, `memory_search`) still runs
  outside the Landlock domain, with whatever access that tool's own code
  has; and the file tools' check trusts the identity walk, as 27c's does
  (an unconfined process rearranging mounts under an allowed directory
  isn't chased). Other in-process tools that write files take no path
  from the agent (the subagent logs, archive and records, the tool-output
  spill files, `watcher_exit`'s hand-off file, whose path the host sets).
- **Owner rule 27c's in-process check covers enforcing sessions only.**
  Unsandboxed sessions, `mode = off`, the kill switch, subagents (until the
  subagent stage binds them), JR's own shell, and extension, MCP and
  sidecar processes (the Jev extension included) aren't covered by it; the
  Jev guard is the policy layer for those. The rest of the Jev plugin dir
  (`$HOME/.synaps-cli/plugins/jev/` other than `config`) isn't protected by
  this rule: R&R's installs write it. Both layers trust that the protected
  trees and the mount namespace aren't rearranged by unconfined processes:
  a bind mount of a protected directory, or of a directory above one, is
  seen by its identity, but a bind mount of a directory *below* one, made
  by an unconfined process, isn't chased (walking `..` from inside it need
  not pass the protected directory).
- **Commands without a session binding run unsandboxed**: subagents (until
  the subagent stage binds them), and tool contexts that carry no
  conversation id (operator plugin commands). An extension-hosted
  provider's own tool loop runs its commands under the session's binding.
- **A client's resume** switches a daemon session to another journal; the
  policy in effect is then that journal's file. A linked compaction
  carries the policy (see [Compaction](#compaction)); a resume doesn't.
- **Extension, MCP and sidecar processes** are not sandboxed by this.
- **`/tmp` and `/dev/shm` are shared**: an `rm -rf /*` in a sandbox still
  empties what it may write there, and in any `infra`/`write` directory.
- **Remote execution**: work a command hands to another machine (ssh, a
  remote build service) runs there unconfined; only the local half (its
  local state, lock and result files) is confined.
- **`synaps send`** falls back to writing `<base>/inbox` when the daemon
  socket is unreachable. Under the sandbox that write is denied, so a failed
  send is an error, not a silent drop into the inbox. Reaching a live
  session works: the session lookup and the daemon liveness probe open
  their lock files read-only (flock needs no write access), so `synaps
  send`, `synaps daemon status` and tools built on them work in a sandbox.
  A sandboxed lookup prunes no stale session cards; the next unconfined
  one does.
- **The sudo check is lexical: it reads the command's text, not what
  ran.** A sudo the text doesn't spell out gets no `sandbox.sudo_refused`
  line and no hint (the kernel still refuses it, and its own message is in
  the output): a script or program that runs sudo inside it
  (`./deploy.sh`, `make install`); an alias, variable or function set in an
  earlier call (`s=sudo` then `$s x`); quote concatenation (`su"do" x`);
  a PTY command split over several sends; a PTY shell's sudo whose refusal
  comes back only with a later call's output, or with `shell_end`'s final
  output (nothing was sent with it). The other way, a command whose text
  contains the word but doesn't run sudo (`cat sudo.log`, `grep sudo
  notes.md`, a comment, a quoted argument, a branch that isn't taken)
  still logs a line when its output quotes a refusal.
- **The denial check reads the output's text.** A refused write logs no
  `sandbox.denial_suspected` line and gets no hint (the kernel still
  refuses it) when its message doesn't reach the output (sent to
  `/dev/null` or a file, or swallowed by a wrapper), when it names no path
  and no file tool, or when it's split some other way: a path broken over
  two lines, a denial that doesn't start the line after the path's colon,
  a blank line between the halves, or a PTY shell's message that comes back
  across two calls. A join reads two lines as one and consumes the second,
  so of two wrapped denials on consecutive lines (`P: permission`,
  `denied; open Q: permission`, `denied`) only the first is caught. A
  joined denial's words must end at a word end (`permission deniedness`
  joins nothing; a single line still counts on the words anywhere in it).
  The /proc skip is lexical and narrow. It covers the enumerated link
  paths themselves (`/proc/<pid>/cwd`, `exe`, `root`, `fd/<n>`,
  `map_files/<x>`, for `<pid>`, `self` or `thread-self`, also under
  `task/<tid>/`) and any path with a `..` component that is under /proc
  as written or after dropping each `..` with the part before it
  (`/proc/1/../1/cwd`, `/proc/1/cwd/../x`, `/tmp/../proc/1/cwd`). It is a
  conservative suppression of process inspection: a write through a
  skipped path that really was refused isn't blamed either. Not skipped,
  so still followed and blamed when the daemon could write where they
  lead: a path below a link without a `..` (`/proc/<pid>/cwd/<sub>`), an
  alias (`/dev/fd/<n>`, `/dev/stdin`, a symlink into /proc), and the other
  /proc paths (a writable proc control). The other way, output that only
  quotes such a message (reading a log, this file, a test) counts when the
  path it names is outside the rules and the daemon could write it; so
  does a refused READ of such a path (the check sees EACCES, not the
  access that was refused). A path is read only up to its first
  whitespace, backtick or quote, so a path with one of those in its name
  is named by its start (a quoted `'/a/b c'` blames `/a/b`, and is
  covered, or not, as `/a/b`); the audit line names the whole path.
- **The audit reader sees only what the kernel logs, from when it opens
  the log.** A domain whose first refusal came earlier (before the
  daemon's first enforcing spawn, or while the log was unreadable) brings
  no record of its own, so its later refusals are only counted
  (`unattributed`); so are a previous daemon's domains (spawns are known
  in memory only) and a domain whose first refusal comes more than 15 min
  after its spawn ended (a background child). A test binary's domains
  (another exe) are skipped, even with a matching tag. For a create or
  remove the record names the parent dir, so a create in `~/.local/state`
  or a `cargo_home` root is only `uncertain`: the record can't say which
  file it was. Rotation is followed by name (the old file is drained, then
  the new one read from its start; a reopen that finds the same file
  keeps its place) and a truncation by size: a log truncated and grown
  back past where the reader was before its next read (about a second,
  as with `copytruncate`) is missed up to its new end. Lines come about a
  second after the refusal, later in a burst past one poll's bound; a
  minute's repeats come at its end. Past the line budgets, refusals are
  only counted (`suppressed`). The reader's reads are plain blocking
  reads: a stop request, or the next poll, waits for one that a slow or
  hung filesystem holds up (the audit log is normally on a local disk). On
  a kernel without Landlock audit (ABI 6 or older) or without an audit
  daemon the reader finds no records. Past one of its memory caps the
  reader forgets its oldest state, so in a flood of refusals some are only
  counted.
- **File descriptors** opened before the sandbox applies keep their rights.
- **Bind mounts and moves (the trust assumption)**: validation recognises
  `$HOME`, the base dir, their ancestors and the base dir's `sandbox/` and
  `sandbox/sessions/` by identity under any name, and refuses hard-linked
  files. A bind mount of some other directory inside the base dir, mounted
  elsewhere, or anything mounted or moved into an allowed directory after
  validation, isn't chased. Setting up mounts needs root; keep them out of
  the paths a policy allows.

## Tests

Real-kernel tests have `_local_` in their names (`landlock_local_*`,
`policy_local_*`, `spawn_local_*`, `bash_sandbox_local_*`,
`compaction_local_*`, `audit_local_*`) and only run with `SYNAPS_TEST_LANDLOCK=1`; with it set they **fail** (never skip) if
Landlock is unusable, so a green run can't be hollow. Run them on a local
Linux kernel (6.2+), not under WSL or in a container whose kernel differs.
The audit reader's other tests run on scratch audit files they write and
confine nothing; the one `audit_local_*` test that confines a real child
and reads the real audit log also needs that log readable, and it SKIPS
(printing why) when it isn't, so a green run doesn't by itself show it
ran.
