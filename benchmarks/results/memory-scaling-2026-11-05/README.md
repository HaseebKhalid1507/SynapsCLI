# Memory scaling, 1 to 1000 sessions (2026-11-05)

synaps 0.11.1 (`/usr/bin/synaps`, the same binary for every file), Intel Core
Ultra 9 275HX, 30.7 GB, CachyOS, Linux 7.3.0-4-cachyos. Idle `--attach --new
--keep-warm` sessions, no extensions, isolated base dir. Summary and method:
[BENCHMARKS.md](../../../BENCHMARKS.md#scaling-to-1000-sessions--synaps-0111).

| file | sessions (reps) | where | launch wait | RLIMIT_NOFILE |
|---|---|---|---|---|
| `host-1-10-20.json` | 1 (3), 10 (3), 20 (1) | host | 4 s (default) | 1024 |
| `host-10-200.json` | 10 (1), 200 (1) | host | 0.1 s | 65536 |
| `host-500.json` | 500 (1) | host | 0.1 s | 65536 |
| `container-10.json` | 10 (1) | container | 0.1 s | 65536 |
| `container-1000.json` | 1000 (1) | container | 0.1 s | 65536 |

- **Partial files.** `host-1-10-20.json` was stopped after the first 20-session
  repetition, and `host-10-200.json` after its 200-session repetition (a 500
  repetition in the same run was cut off by a wrapper timeout and re-run on its
  own as `host-500.json`). Their `summary` blocks are incomplete: use `runs`.
- **Launch wait.** These runs predate the `--launch-wait` flag. The 0.1 s runs
  wrapped `launch()` to cap its wait at 0.1 s, which is what `--launch-wait 0.1`
  now does, so their `args` don't record it. 10 sessions read 72.6 MB at 4 s and
  71.0 MB at 0.1 s.
- **RLIMIT_NOFILE.** Raised to 65536 by hand for 200+ (the harness now raises it
  itself). At 1024 the daemon refuses new sessions at about 170 (see
  BENCHMARKS.md).
- **Container (1000).** `ubuntu:24.04` with python3, the host binary bind-mounted
  read-only (it needs glibc 2.39, which 24.04 has), `--user 2000:2000 --ulimit
  nofile=65536:65536 --memory 8g --pids-limit 20000`, no privileges, same
  kernel. A separate uid keeps the run's ~1000 inotify instances off the desktop
  user's `fs.inotify.max_user_instances` (1024). Parity: 10 sessions read
  73.0 MB in the container. Peak during the 1000 run, sampled from inside:
  daemon 4,547 fds, 1,034 threads, 512 inotify instances.
