#!/usr/bin/env python3
"""Manual integration repro for dead-PTY exit (needs a real debug build).
NOT a cargo test. Run: python3 tests/repro_dead_pty.py

Closes the PTY master (simulates terminal window close) and verifies the
process exits promptly on its own — no SIGKILL needed.

RC semantics (FIX 3):
  - Dead-PTY path → rc may be non-zero (process exits via draw() error
    detection or watchdog, not a clean session).  We treat any prompt exit
    (within 4s) as PASS regardless of rc — the important property is that
    the process doesn't survive indefinitely after PTY close.
  - rc=0 on the dead-PTY path would mean the loop hit an Err(draw) and fell
    through to the full teardown cleanly — also acceptable.
  - HUNG (needed SIGKILL) = FAIL.
"""
import os, pty, time, signal, subprocess, struct, fcntl, termios, sys
BIN = os.path.expanduser("~/Projects/agent-runtime/target/debug/synaps")
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
proc = subprocess.Popen([BIN], stdin=slave, stdout=slave, stderr=slave,
    preexec_fn=os.setsid, env={**os.environ, "TERM":"xterm-256color"})
os.close(slave)
pid=proc.pid
print(f"[repro] pid={pid}, booting 3s...")
time.sleep(3)

def cpu(pid):
    def j():
        with open(f"/proc/{pid}/stat") as f: p=f.read().split()
        return int(p[13])+int(p[14])
    try: a=j(); time.sleep(0.5); b=j()
    except FileNotFoundError: return None
    return (b-a)/os.sysconf("SC_CLK_TCK")/0.5*100

print(f"[repro] booted: alive={proc.poll() is None}, cpu={cpu(pid)}%")
print("[repro] >>> closing PTY master (terminal window closes) <<<")
os.close(master)

# After PTY close the process should exit on its own within ~4s
# (draw() returns EIO → loop breaks → teardown runs → exit).
# Any rc is acceptable here; HUNG = FAIL.
for t in range(8):
    time.sleep(0.5)
    if proc.poll() is not None:
        rc = proc.returncode
        print(f"[repro] ✅ EXITED {0.5*(t+1):.1f}s after PTY close, rc={rc}")
        # rc=0: draw error path fell through to clean teardown (ideal)
        # rc≠0: watchdog fired or save-timeout path (acceptable for dead PTY)
        print(f"[repro] rc={rc} is {'CLEAN (0)' if rc == 0 else 'NON-ZERO (watchdog/forced) — acceptable for dead-PTY path'}")
        sys.exit(0)   # exited promptly = PASS regardless of rc
    print(f"[repro] +{0.5*(t+1):.1f}s ALIVE cpu={cpu(pid)}%")

print("[repro] 🔴 survived 4s — sending SIGTERM")
os.kill(pid, signal.SIGTERM)
for t in range(4):
    time.sleep(0.5)
    if proc.poll() is not None:
        print(f"[repro] SIGTERM worked after {0.5*(t+1):.1f}s rc={proc.returncode}"); sys.exit(1)
    print(f"[repro] post-SIGTERM +{0.5*(t+1):.1f}s ALIVE cpu={cpu(pid)}%")
print("[repro] 🔴🔴 SIGTERM IGNORED — only SIGKILL works (matches bug report)")
os.kill(pid, signal.SIGKILL); proc.wait(); sys.exit(2)
