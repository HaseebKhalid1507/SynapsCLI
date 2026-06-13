#!/usr/bin/env python3
"""Manual integration repro for TUI signal shutdown (needs a real debug build + PTY).
NOT a cargo test. Run: python3 tests/repro_signal_shutdown.py
Verifies SIGTERM / SIGHUP / SIGINT each cause clean exit (<2s) while the terminal
is ALIVE — the systemd-credibility case. Exit 0 = all pass, nonzero = a signal hung.
"""
import os, pty, time, signal, subprocess, struct, fcntl, termios, sys
BIN = os.path.expanduser("~/Projects/agent-runtime/target/debug/synaps")

def trial(name, signum):
    m, s = pty.openpty()
    fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    p = subprocess.Popen([BIN], stdin=s, stdout=s, stderr=s,
        preexec_fn=os.setsid, env={**os.environ, "TERM": "xterm-256color"})
    os.close(s); time.sleep(3)
    os.kill(p.pid, signum)
    exited = False
    for _ in range(10):           # up to 2.5s
        time.sleep(0.25)
        if p.poll() is not None:
            exited = True; break
    rc = p.returncode
    if not exited:
        os.kill(p.pid, signal.SIGKILL); p.wait()
    try: os.close(m)
    except OSError: pass
    status = f"EXIT rc={rc}" if exited else "HUNG (needed SIGKILL)"
    print(f"  {name:8} -> {status}")
    return exited and rc == 0   # clean exit only

ok = True
for name, num in [("SIGTERM", signal.SIGTERM), ("SIGHUP", signal.SIGHUP), ("SIGINT", signal.SIGINT)]:
    ok &= trial(name, num)
print("RESULT:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
