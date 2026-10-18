# Lightweight long-horizon watcher agents

Long jobs (renders, training runs, sweeps) leave an agent with nothing to do
for hours. Polling with `trigger = "always"` and a short `cooldown_secs`
starts a full LLM session every few minutes just to learn "still running".
These config keys let the agent sleep cheaply and run under resource limits.
The limits are not a sandbox; see *Not a security boundary* below.

```toml
[agent]
name = "render-babysitter"
trigger = "always"            # or "watch"

[trigger]
# Shell gate run before every session spawn (sh -c, agent dir, 30s timeout).
# Exit 0 = wake; non-zero = stay asleep. No LLM session while asleep.
wake_if = "! vizjob status myjob | grep -q running"
wake_poll_secs = 120          # default 60, minimum 5

[limits]
memory_max = "1.5G"           # systemd MemoryMax — via `systemd-run --user --scope`
cpu_quota_pct = 200           # CPUQuota (needs the cpu controller delegated to user systemd)
nice = 10                     # 0..=19

[hooks]
# Completion notices are OFF by default.
notify_session = "e331"       # deliver to exactly one running session; dropped if not running
# notify_inbox = true         # legacy: shared inbox, taken by whichever session reads first
```

## Behaviour

- **wake_if** gates both initial start and every restart (`always`) and every
  file trigger (`watch`). The supervisor logs once when it starts sleeping and
  once when it wakes.
- **Hot reload.** `limits`, `hooks` and `wake_if` are re-read from
  `config.toml` before each spawn and on each wake poll. Changing the trigger
  *mode* still needs a redeploy.
- **Resource limits.** `systemd-run --scope` execs in place, so the agent PID
  and stop/kill behaviour stay the same. If `systemd-run --user` is
  unavailable, the agent runs without limits and a warning is logged. Only
  controllers delegated to the user manager take effect (`memory` is
  delegated almost everywhere; `cpu` often is not).
- **Notices never broadcast.** Headless watcher agents also never pick up
  `watcher-*.json` inbox files, so one agent can't steal another's
  completion notice.

## Not a security boundary

`limits` and `wake_if` protect against runaway resource use, not against the
agent itself. The agent runs as your user and can edit its own `config.toml`
with its own tools. Because these keys hot-reload, an edit takes effect on the
next spawn or wake poll: the agent can delete `memory_max`/`cpu_quota_pct`, or
add a `wake_if` command. `wake_if` runs through `sh -c` in the supervisor,
outside the `systemd-run` scope that applies the limits.
