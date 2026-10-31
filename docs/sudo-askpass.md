# sudo passwords in the bash tool

The bash tool can run `sudo`. When sudo needs a password, Synaps asks you in a
masked password box, shows you the command sudo is about to run, and hands the
password to sudo only. This page describes how that works and what it does
and does not protect against.

## What you see

```
sudo password required
sudo is asking for your password to run:
sudo -A apt install ripgrep
[sudo] password for you:

password: ••••••
```

The second line is sudo's own command line, read from the sudo process itself,
not from anything the command printed. Control characters and invisible
formatting characters (bidi overrides, zero-width) are shown escaped, and the
line is cut at 240 characters. Esc cancels: sudo gets no password and the whole
command is stopped ("Command canceled while waiting for password").

Nothing a command prints can open this box. A line like `[sudo] password for
x:` or `Password:` on stderr is just output.

## How it works (Linux)

The bash tool defines `sudo` as `command sudo -A "$@"` and sets
`SUDO_ASKPASS` for the command. With `-A`, sudo runs the `SUDO_ASKPASS`
program only when it really needs a password, passes its prompt as argv[1]
and reads the password from the program's stdout. sudo also uses
`SUDO_ASKPASS` for a plain `sudo` when no terminal is available (`man sudo`;
some versions also want `DISPLAY`), so a script the command runs can be asked
for too, through the same checks.

`SUDO_ASKPASS` names `synaps-sudo-askpass`, a symlink to the running Synaps
binary (`/proc/<pid>/exe`, so it stays the same build across an upgrade) in a
fresh `0700` directory under `$XDG_RUNTIME_DIR` (or the temp dir). `synaps`
starts in helper mode when argv[0] is that name, before it reads any config.
The directory also holds a Unix socket (`0600`) the bash tool call listens on
and a random token (`0600`). It is removed when the call ends.

1. **The helper** first checks that its parent is the system sudo, then that
   its stdout is a pipe, then that the directory and token are private to
   this user. Only then does it connect and send the token and sudo's prompt.
2. **The tool call** checks the request on its own, from the socket's peer
   credentials (it does not trust the helper): the peer is this user and runs
   the Synaps binary; the peer's parent is the system sudo; that sudo descends
   from the shell this tool call started (so a request cannot come from
   another tool call, another session or anything outside the command); and
   the token matches. A token is good for one request and replaced right
   away, so a retry after a wrong password gets a fresh one.
3. Only then is the prompt raised, through the same secret-prompt channel as
   every other prompt: the in-process TUI, or a `Prompt` event to the client
   attached to the daemon. The answer goes back over the socket to the
   helper, which writes it to sudo's pipe and exits. The command's stdin is
   empty; the password never goes there. If the command echoes it, it is
   replaced with `[redacted]` in the output, and every copy is zeroized.

"The system sudo" means: `/proc/<pid>/status` says the process is named
`sudo`, was started by this user (real uid) and runs set-uid root (effective
and saved uid 0). The kernel keeps those credentials; a process of the same
user cannot fake them. `/proc/<pid>/exe` would be a stronger check, but a
set-uid sudo is not dumpable, so its own user gets `EACCES` there (checked
against sudo 1.9.17). When it is readable (running as root) it must also be a
root-owned set-uid file.

With no way to ask (a subagent, any context without a prompt channel), the
helper refuses ("no interactive client to ask for the sudo password") and
sudo reports its usual error. `sudo -n` and `NOPASSWD` rules never reach the
helper.

## What it protects against

- Output that looks like a prompt. Before, the tool watched stderr for text
  containing "password" and wrote your answer to the command's stdin, so a
  command that printed a lookalike line (or a diagnostic that printed the
  tool's own command lines, which contained the old prompt string) raised a
  password box, and the answer went to whatever was reading stdin.
- Requests from outside the tool call: other sessions, other tool calls,
  background processes, anything that is not a set-uid sudo under this
  command, and any program other than the Synaps helper talking to the
  socket.
- Replays: a spent or guessed token is refused.

## Limits

- A command inside the tool call that runs `sudo -A` itself still gets a
  prompt. That is how sudo works, and it is why the prompt shows the command:
  judge it before you type.
- Code running as your user can ptrace the helper, or start its own sudo with
  its own `SUDO_ASKPASS` that wraps the Synaps helper and copies what it
  writes. That is your account, not a boundary this can draw. The helper
  refuses a stdout that is a file or a terminal, but cannot see who reads a
  pipe.
- Two sudo password requests racing in one command share one token; the
  second is refused and that sudo fails.
- Other password prompts (`mysql -p`, `psql`, and anything else that falls
  back to stdin without a terminal) are no longer answered: they get EOF.
  Use their non-interactive options (option files, `PGPASSFILE`,
  `--password-stdin` with a secret you provide) instead. SSH could reuse the
  helper through `SSH_ASKPASS` with `SSH_ASKPASS_REQUIRE=force`; that is not
  done yet.

## Platforms

- **Linux:** as above.
- **macOS and other Unix:** fails closed. The tool defines `sudo` as
  `sudo -n`: a sudo that needs a password fails with "a password is
  required" and never asks. A macOS port would need `proc_pidinfo` for the
  credentials and ancestry, and sudo's argv for the prompt, which
  `KERN_PROCARGS2` does not give to another effective uid; and it would
  ship untested, which is not a good place to be for this code.
- **Windows:** no sudo; unchanged.

## Testing it by hand

`scripts/smoke/sudo-askpass.sh` uses the real sudo, so it is run by hand,
never in CI. It checks that the helper refuses a shell parent and accepts the
real sudo (and then fails, since no tool call is listening, so nothing is
authenticated), and lists the steps to try in a live session. The automated
tests cover the rest with a fake sudo and a protocol stand-in for the helper
(`tools::sudo_askpass::testing`): only the set-uid check is faked; the
socket, token and ancestry checks are real.
