#!/usr/bin/env python3
"""Manual integration repro for TUI signal shutdown (needs a real debug build + PTY).
NOT a cargo test. Run: python3 tests/repro_signal_shutdown.py
Verifies SIGTERM / SIGHUP / SIGINT each cause clean exit (<2.5s) while the terminal
is ALIVE — the systemd-credibility case. Exit 0 = all pass, nonzero = a signal hung.

"Alive terminal" means the PTY master is being drained (a real terminal always
reads its output).  We drain in a background thread to mirror that.

RC semantics (FIX 3):
  - DRAINED interactive path → rc=0 (clean session save + hooks completed)
  - watchdog/forced path     → rc=1 (process was stuck; supervisors should alert)
  This test asserts rc=0 because the PTY is alive and drained — teardown should
  always complete cleanly within the save+hooks budget.
"""
import os, pty, time, signal, subprocess, struct, fcntl, termios, sys, threading, select as sel
BIN = os.path.expanduser("~/Projects/agent-runtime/target/debug/synaps")

def trial(name, signum):
    m, s = pty.openpty()
    fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    p = subprocess.Popen([BIN], stdin=s, stdout=s, stderr=s,
        preexec_fn=os.setsid, env={**os.environ, "TERM": "xterm-256color"})
    os.close(s)

    # Drain PTY master in a background thread — a live terminal always reads.
    stop = threading.Event()
    def reader():
        while not stop.is_set():
            try:
                r, _, _ = sel.select([m], [], [], 0.1)
                if r:
                    os.read(m, 65536)
            except OSError:
                break
    threading.Thread(target=reader, daemon=True).start()

    time.sleep(3)
    os.kill(p.pid, signum)
    exited = False
    for _ in range(10):           # up to 2.5s
        time.sleep(0.25)
        if p.poll() is not None:
            exited = True; break
    rc = p.returncode
    stop.set()
    if not exited:
        os.kill(p.pid, signal.SIGKILL); p.wait()
    try: os.close(m)
    except OSError: pass
    status = f"EXIT rc={rc}" if exited else "HUNG (needed SIGKILL)"
    print(f"  {name:8} -> {status}")
    # Drained interactive path: must exit cleanly with rc=0.
    # rc=1 would indicate the watchdog fired, meaning teardown was stuck —
    # that's a bug on the drained path and should fail this test.
    return exited and rc == 0   # clean exit only

ok = True
for name, num in [("SIGTERM", signal.SIGTERM), ("SIGHUP", signal.SIGHUP), ("SIGINT", signal.SIGINT)]:
    ok &= trial(name, num)
print("RESULT:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
